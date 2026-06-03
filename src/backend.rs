//! Phase-2 seam: the [`ClipboardBackend`] trait and minimal placeholder types.
//!
//! Nothing here is wired up in Phase 1. The trait gives a stable boundary for
//! the Wayland capture/restore implementation that will land in Phase 2.

#![allow(dead_code)]

use crate::model::ItemWithFormats;

/// A sink that receives captured clipboard data from the backend.
///
/// Phase 2 will fill this out; for now it is a minimal placeholder so the
/// trait compiles.
pub struct CaptureSink {
    _private: (),
}

impl CaptureSink {
    pub fn new() -> Self {
        CaptureSink { _private: () }
    }
}

impl Default for CaptureSink {
    fn default() -> Self {
        Self::new()
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
/// event loop. Phase 2 will provide `WlrDataControlBackend: ClipboardBackend`.
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
