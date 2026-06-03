//! richclipd — Wayland clipboard history daemon (Phase 2, WIP).
//!
//! This stub initialises tracing and exits immediately. The Wayland backend
//! and IPC socket will be wired up in subsequent Phase 2 milestones.

use tracing::info;
use tracing_subscriber::EnvFilter;

fn main() {
    // Initialise structured logging; respects `RUST_LOG` env var.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // Confirm linkage against the Wayland backend crate.
    let _backend = richclip_wayland::WlrDataControlBackend::new()
        .expect("failed to construct WlrDataControlBackend");

    info!(
        "richclipd: Wayland capture and restore are not yet implemented (Phase 2 in progress). \
         Use `richclip add/list/decode/update/delete` to interact with the store directly."
    );

    std::process::exit(0);
}
