//! richclipd — Wayland clipboard history daemon (Phase 2 stub).
//!
//! In Phase 1, this binary initialises tracing and exits immediately with a
//! message explaining that capture/restore are not yet implemented. Phase 2
//! will wire up the Wayland backend and the IPC socket.

use tracing::info;

fn main() {
    // Initialise structured logging; respects `RUST_LOG` env var.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    info!(
        "richclipd: Wayland capture and restore are not yet implemented (Phase 2). \
         Use `richclip add/list/decode/update/delete` to interact with the store directly."
    );

    std::process::exit(0);
}
