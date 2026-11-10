//! Phase-2 seam: the [`ClipboardBackend`] trait and capture sink types.
//!
//! [`CaptureSink`] is a callback-based sink that the Wayland capture loop
//! pushes completed clipboard items into. Keeping tokio out of this root lib
//! means the core storage crate remains runtime-agnostic.

#![allow(dead_code)]

use crate::model::ItemWithFormats;

/// A synchronous barrier that waits until the backend has dispatched all
/// selection events already pending on its capture connection.
pub type CaptureOrderingBarrier =
    std::sync::Arc<dyn Fn() -> Result<(), String> + Send + Sync + 'static>;

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
    handler: Option<Box<dyn FnMut(CapturedItem) + Send>>,
    selection_skipped_handler: Option<Box<dyn FnMut() + Send>>,
    selection_started_handler: Option<Box<dyn FnMut(u64) + Send>>,
    ordered_handler: Option<Box<dyn FnMut(u64, CapturedItem) + Send>>,
    ordered_selection_skipped_handler: Option<Box<dyn FnMut(u64) + Send>>,
    ordering_barrier_handler: Option<Box<dyn FnOnce(CaptureOrderingBarrier) + Send>>,
    next_selection_id: u64,
}

impl CaptureSink {
    /// Create a sink backed by `handler`. The handler is called once per
    /// completed clipboard selection, on whatever thread the capture loop runs.
    pub fn new(handler: impl FnMut(CapturedItem) + Send + 'static) -> Self {
        Self {
            handler: Some(Box::new(handler)),
            selection_skipped_handler: None,
            selection_started_handler: None,
            ordered_handler: None,
            ordered_selection_skipped_handler: None,
            ordering_barrier_handler: None,
            next_selection_id: 0,
        }
    }

    /// Create a sink that reports when each selection starts and associates its
    /// eventual item or skipped-selection notification with that start.
    ///
    /// This is useful when a backend can block while reading clipboard data:
    /// consumers can preserve selection order even when delivery happens later.
    pub fn new_ordered(
        started_handler: impl FnMut(u64) + Send + 'static,
        item_handler: impl FnMut(u64, CapturedItem) + Send + 'static,
        selection_skipped_handler: impl FnMut(u64) + Send + 'static,
    ) -> Self {
        Self {
            handler: None,
            selection_skipped_handler: None,
            selection_started_handler: Some(Box::new(started_handler)),
            ordered_handler: Some(Box::new(item_handler)),
            ordered_selection_skipped_handler: Some(Box::new(selection_skipped_handler)),
            ordering_barrier_handler: None,
            next_selection_id: 0,
        }
    }

    /// Arrange to receive an ordering barrier after the backend has connected
    /// and before it begins dispatching selection events.
    ///
    /// A caller can use the barrier to order external lifecycle events, such
    /// as restore markers, after selection notifications already pending in
    /// the backend's event queue.
    pub fn with_ordering_barrier_handler(
        mut self,
        handler: impl FnOnce(CaptureOrderingBarrier) + Send + 'static,
    ) -> Self {
        self.ordering_barrier_handler = Some(Box::new(handler));
        self
    }

    /// Notify the caller that the backend is ready and provide its ordering
    /// barrier, if one was requested.
    pub fn capture_initialized(&mut self, barrier: CaptureOrderingBarrier) {
        if let Some(handler) = self.ordering_barrier_handler.take() {
            handler(barrier);
        }
    }

    /// Register a callback for clipboard selections that the backend does not
    /// deliver as items, such as an empty, unsupported, or filtered selection.
    ///
    /// Restore echo suppression uses this notification to retire pending
    /// markers when the clipboard changes to content that cannot be captured.
    pub fn with_selection_skipped_handler(
        mut self,
        handler: impl FnMut() + Send + 'static,
    ) -> Self {
        self.selection_skipped_handler = Some(Box::new(handler));
        self
    }

    /// Deliver a captured item to the handler.
    pub fn push(&mut self, item: CapturedItem) {
        if let Some(handler) = self.handler.as_mut() {
            handler(item);
        } else {
            panic!("ordered capture sink requires push_for_selection");
        }
    }

    /// Mark the start of a backend selection event and return its local ID.
    pub fn selection_started(&mut self) -> u64 {
        self.next_selection_id = self.next_selection_id.wrapping_add(1);
        let selection_id = self.next_selection_id;
        if let Some(handler) = self.selection_started_handler.as_mut() {
            handler(selection_id);
        }
        selection_id
    }

    /// Deliver an item with the ID returned by [`Self::selection_started`].
    pub fn push_for_selection(&mut self, selection_id: u64, item: CapturedItem) {
        if let Some(handler) = self.ordered_handler.as_mut() {
            handler(selection_id, item);
        } else {
            self.push(item);
        }
    }

    /// Notify the handler that the current clipboard selection will not be
    /// delivered as a captured item.
    pub fn selection_skipped(&mut self) {
        if let Some(handler) = self.selection_skipped_handler.as_mut() {
            handler();
        } else if self.ordered_selection_skipped_handler.is_some() {
            panic!("ordered capture sink requires selection_skipped_for");
        }
    }

    /// Notify the handler that the selection with this ID will not be
    /// delivered as an item.
    pub fn selection_skipped_for(&mut self, selection_id: u64) {
        if let Some(handler) = self.ordered_selection_skipped_handler.as_mut() {
            handler(selection_id);
        } else {
            self.selection_skipped();
        }
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
