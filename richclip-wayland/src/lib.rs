//! Wayland (wlr-data-control) clipboard backend for richclip.
//!
//! Provides [`WlrDataControlBackend`] which implements
//! [`richclip::backend::ClipboardBackend`]. The capture loop runs on a
//! dedicated blocking thread via `tokio::task::spawn_blocking` so the async
//! runtime is never blocked.

pub mod capture;
pub mod daemon;
pub mod filter;
pub mod restore;

pub use filter::CaptureConfig;

use std::future::Future;

use richclip::backend::{CaptureSink, ClipboardBackend, RestorableItem};

/// wlr-data-control capture backend.
///
/// Construct with [`WlrDataControlBackend::new`] (uses default
/// [`CaptureConfig`]) or [`WlrDataControlBackend::with_config`].
#[derive(Default)]
pub struct WlrDataControlBackend {
    config: CaptureConfig,
}

impl WlrDataControlBackend {
    /// Create a backend with the default [`CaptureConfig`].
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self::default())
    }

    /// Create a backend with a custom [`CaptureConfig`].
    pub fn with_config(config: CaptureConfig) -> Self {
        Self { config }
    }
}

impl ClipboardBackend for WlrDataControlBackend {
    fn run_capture_loop(
        &mut self,
        sink: CaptureSink,
    ) -> impl Future<Output = anyhow::Result<()>> + Send {
        // Clone config so it can cross into the spawn_blocking closure
        // (CaptureConfig: Clone). CaptureSink is Send + 'static because its
        // handler is `Box<dyn FnMut(CapturedItem) + Send>`.
        let config = self.config.clone();
        async move {
            tokio::task::spawn_blocking(move || capture::run_capture(config, sink))
                .await
                // JoinError (task panicked) → convert to anyhow::Error.
                .map_err(|e| anyhow::anyhow!("capture task panicked: {e}"))?
        }
    }

    async fn restore_item(&mut self, _item: RestorableItem) -> anyhow::Result<()> {
        unimplemented!("restore lands in P2-M4")
    }
}
