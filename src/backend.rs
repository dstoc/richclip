//! Phase-2 seam: the [`ClipboardBackend`] trait and capture sink types.
//!
//! [`CaptureSink`] is a callback-based sink that the Wayland capture loop
//! pushes completed clipboard items into. Keeping tokio out of this root lib
//! means the core storage crate remains runtime-agnostic.

#![allow(dead_code)]

use crate::model::ItemWithFormats;

/// One captured MIME representation of a clipboard item.
pub struct CapturedFormat {
    pub mime: String,
    pub bytes: Vec<u8>,
}

/// A captured clipboard selection: all accepted formats grouped together.
pub struct CapturedItem {
    pub formats: Vec<CapturedFormat>,
}

/// Sink the capture loop pushes completed items into.
///
/// The handler runs on the (blocking) Wayland thread, so it must be `Send`.
/// Construct with [`CaptureSink::new`], then pass to
/// [`ClipboardBackend::run_capture_loop`].
pub struct CaptureSink {
    handler: Box<dyn FnMut(CapturedItem) + Send>,
}

impl CaptureSink {
    /// Create a sink backed by `handler`. The handler is called once per
    /// completed clipboard selection, on whatever thread the capture loop runs.
    pub fn new(handler: impl FnMut(CapturedItem) + Send + 'static) -> Self {
        Self { handler: Box::new(handler) }
    }

    /// Deliver a captured item to the handler.
    pub fn push(&mut self, item: CapturedItem) {
        (self.handler)(item)
    }
}

/// The data needed by the backend to restore (re-advertise) an item to the
/// Wayland compositor.
pub struct RestorableItem {
    /// The item and formats to advertise.
    pub inner: ItemWithFormats,
    /// Raw bytes for each format, indexed parallel to `inner.formats`.
    pub blobs: Vec<Vec<u8>>,
}

/// Trait that Wayland (or any future) backend implements.
///
/// Both methods are async; the concrete implementation will drive a Wayland
/// event loop. Phase 2 provides `WlrDataControlBackend: ClipboardBackend`.
pub trait ClipboardBackend {
    /// Run the capture loop, delivering new items to `sink` as they arrive.
    ///
    /// This future runs until an error occurs or the compositor disconnects.
    fn run_capture_loop(
        &mut self,
        sink: CaptureSink,
    ) -> impl Future<Output = anyhow::Result<()>> + Send;

    /// Advertise `item` as the current clipboard selection and serve format
    /// blobs to requesting clients.
    fn restore_item(
        &mut self,
        item: RestorableItem,
    ) -> impl Future<Output = anyhow::Result<()>> + Send;
}

// Re-export Future so the trait bound above resolves without requiring
// callers to import it.
use std::future::Future;
