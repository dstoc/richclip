//! Wayland (wlr-data-control) clipboard backend for richclip. (Phase 2, WIP.)

use richclip::backend::{CaptureSink, ClipboardBackend, RestorableItem};

/// wlr-data-control backend. Capture/restore land in subsequent milestones.
#[derive(Default)]
pub struct WlrDataControlBackend {
    // connection/state fields added in the capture milestone
}

impl WlrDataControlBackend {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self::default())
    }
}

impl ClipboardBackend for WlrDataControlBackend {
    fn run_capture_loop(
        &mut self,
        _sink: CaptureSink,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send {
        async { unimplemented!("capture loop lands in P2-M2") }
    }

    fn restore_item(
        &mut self,
        _item: RestorableItem,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send {
        async { unimplemented!("restore lands in P2-M4") }
    }
}
