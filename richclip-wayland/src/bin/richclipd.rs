//! richclipd — Wayland clipboard history daemon.
//!
//! ## Architecture
//!
//! 1. **Single instance** via the Unix socket: if a daemon is already running
//!    on the socket path we log a message and exit non-zero. If the socket path
//!    exists but no daemon answers (stale file) we remove it and bind fresh.
//!
//! 2. **Capture task** (Wayland-optional): spawns the `WlrDataControlBackend`
//!    capture loop via `tokio::task::spawn_blocking`.  If no `WAYLAND_DISPLAY`
//!    is set the capture loop errors immediately; we log the error and continue
//!    serving IPC so the daemon is usable headless (required for integration
//!    tests and non-compositor environments).
//!
//! 3. **IPC accept loop**: newline-delimited JSON over a Unix domain socket.
//!    Each connection is handled in its own task; `WatchEvents` connections are
//!    long-lived streaming connections.
//!
//! 4. **Retention auto-prune**: runs on startup and then every hour (or
//!    `RICHCLIP_RETENTION_DAYS`-based interval).
//!
//! 5. **Graceful shutdown**: `Ctrl-C` removes the socket file and exits.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use tokio::net::UnixListener;
use tokio::sync::Mutex;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use richclip::Store;
use richclip::backend::{CaptureSink, CapturedItem};
use richclip::ipc::WatchEvent;
use richclip_wayland::{WlrDataControlBackend, daemon::DaemonState};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default retention window in days. Items older than this are pruned.
const DEFAULT_RETENTION_DAYS: u64 = 30;

/// Broadcast channel capacity for watch events.
const WATCH_CHANNEL_CAPACITY: usize = 256;

/// How long to wait between retention prune passes.
const PRUNE_INTERVAL_SECS: u64 = 3600; // 1 hour

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // ── 1. Tracing ────────────────────────────────────────────────────────────
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // ── 2. Resolve paths ─────────────────────────────────────────────────────
    let db_root = resolve_data_dir()?;
    let socket_path = resolve_socket_path()?;
    let cache_root = resolve_cache_dir()?;

    info!(?db_root, ?socket_path, ?cache_root, "richclipd starting");

    // ── 3. Single-instance check ──────────────────────────────────────────────
    if socket_path.exists() {
        // Try to connect. If a daemon answers, bail out.
        if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&socket_path) {
            use richclip::ipc::{ListItemsParams, Request};
            // Send a lightweight request to probe liveness.
            let probe = Request::ListItems(ListItemsParams {
                limit: Some(0),
                mime_filter: None,
            });
            let mut line = serde_json::to_string(&probe).unwrap();
            line.push('\n');
            use std::io::Write;
            if stream.write_all(line.as_bytes()).is_ok() {
                tracing::error!("richclipd is already running on {:?}; exiting", socket_path);
                std::process::exit(1);
            }
        }
        // Connection failed — stale socket file. Remove it.
        warn!(?socket_path, "removing stale socket file");
        let _ = std::fs::remove_file(&socket_path);
    }

    // ── 4. Open store ─────────────────────────────────────────────────────────
    let store = Store::open(&db_root).context("failed to open store")?;
    let state = DaemonState::new(store, WATCH_CHANNEL_CAPACITY, cache_root);

    // ── 5. Bind Unix socket ───────────────────────────────────────────────────
    // Ensure parent directory exists.
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent).context("failed to create socket parent dir")?;
    }
    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("failed to bind socket {:?}", socket_path))?;
    info!(?socket_path, "IPC socket bound");

    // ── 6. Spawn capture task (best-effort; skip if no compositor) ────────────
    {
        let state_cap = state.clone();
        let (cap_tx, mut cap_rx) = tokio::sync::mpsc::unbounded_channel::<CapturedItem>();

        // Spawn the Wayland capture loop in a blocking thread.
        tokio::spawn(async move {
            let sink = CaptureSink::new(move |item: CapturedItem| {
                let _ = cap_tx.send(item);
            });
            match WlrDataControlBackend::new() {
                Ok(mut backend) => {
                    use richclip::backend::ClipboardBackend;
                    if let Err(e) = backend.run_capture_loop(sink).await {
                        warn!("capture loop ended: {e}");
                    }
                }
                Err(e) => {
                    warn!("failed to create capture backend: {e}");
                }
            }
        });

        // Spawn consumer that converts CapturedItems into store writes + events.
        tokio::spawn(async move {
            while let Some(item) = cap_rx.recv().await {
                let formats: Vec<(String, Vec<u8>)> = item
                    .formats
                    .into_iter()
                    .map(|f| (f.mime, f.bytes))
                    .collect();

                // Self-capture suppression: if this capture exactly matches the
                // content we most recently restored, it's our own selection
                // echoing back through the capture device — drop it instead of
                // creating a duplicate history entry.
                let captured_fp: std::collections::HashSet<(String, String)> = formats
                    .iter()
                    .map(|(mime, bytes)| (mime.clone(), richclip::blob_hash(bytes)))
                    .collect();
                let is_self_restore = state_cap
                    .suppression
                    .lock()
                    .map(|g| g.as_ref() == Some(&captured_fp))
                    .unwrap_or(false);
                if is_self_restore {
                    tracing::debug!("ignoring self-restore echo");
                    continue;
                }

                let mut store = state_cap.store.lock().await;
                match store.add_item(&formats) {
                    Ok(id) => {
                        let mimes: Vec<String> = formats.iter().map(|(m, _)| m.clone()).collect();
                        let created_at = match store.get_item(id) {
                            Ok(iwf) => iwf.item.created_at,
                            Err(_) => time::OffsetDateTime::now_utc(),
                        };
                        // Best-effort thumbnail generation while holding the
                        // store lock (avoids a second lock/clone; capture is
                        // not high-frequency so the brief blocking is fine).
                        if let Err(e) = richclip::thumbnail::generate_item_thumbnail(
                            &store,
                            &state_cap.cache_dir,
                            id,
                        ) {
                            warn!(%id, "thumbnail generation failed: {e}");
                        }
                        drop(store);
                        let _ = state_cap.watch_tx.send(WatchEvent::ItemAdded {
                            id,
                            created_at,
                            formats: mimes,
                        });
                        info!(%id, "captured item stored");
                    }
                    Err(e) => {
                        warn!("failed to store captured item: {e}");
                    }
                }
            }
        });
    }

    // ── 7. Retention auto-prune ───────────────────────────────────────────────
    {
        let state_prune = state.clone();
        let retention_days = std::env::var("RICHCLIP_RETENTION_DAYS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(DEFAULT_RETENTION_DAYS);

        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(std::time::Duration::from_secs(PRUNE_INTERVAL_SECS));
            // First tick fires immediately.
            loop {
                interval.tick().await;
                prune_old_items(&state_prune.store, retention_days).await;
            }
        });
    }

    // ── 8. IPC accept loop + graceful shutdown ────────────────────────────────
    let socket_path_for_cleanup = socket_path.clone();

    tokio::select! {
        result = accept_loop(listener, state) => {
            if let Err(e) = result {
                tracing::error!("accept loop error: {e}");
            }
        }
        _ = tokio::signal::ctrl_c() => {
            info!("received Ctrl-C, shutting down");
        }
    }

    // Clean up socket file on exit.
    let _ = std::fs::remove_file(&socket_path_for_cleanup);
    info!("richclipd exited cleanly");

    // The Wayland capture loop runs on a `spawn_blocking` thread whose
    // `blocking_dispatch()` never returns while the compositor is connected.
    // Blocking tasks can't be cancelled, and dropping the tokio runtime blocks
    // until they finish — so returning here would hang in runtime shutdown.
    // Terminate explicitly now that cleanup is done.
    std::process::exit(0)
}

// ---------------------------------------------------------------------------
// Accept loop
// ---------------------------------------------------------------------------

async fn accept_loop(listener: UnixListener, state: DaemonState) -> anyhow::Result<()> {
    loop {
        let (stream, _addr) = listener.accept().await?;
        let state_clone = state.clone();
        tokio::spawn(async move {
            richclip_wayland::daemon::handle_conn(stream, state_clone).await;
        });
    }
}

// ---------------------------------------------------------------------------
// Retention prune helper
// ---------------------------------------------------------------------------

async fn prune_old_items(store: &Arc<Mutex<Store>>, retention_days: u64) {
    let cutoff = time::OffsetDateTime::now_utc() - time::Duration::days(retention_days as i64);
    let mut store = store.lock().await;
    match store.delete_older_than(cutoff) {
        Ok(0) => {}
        Ok(n) => info!(n, "pruned items older than {retention_days} days"),
        Err(e) => warn!("retention prune error: {e}"),
    }
}

// ---------------------------------------------------------------------------
// Path resolution
// ---------------------------------------------------------------------------

fn resolve_data_dir() -> anyhow::Result<PathBuf> {
    if let Ok(v) = std::env::var("RICHCLIP_DATA_DIR") {
        return Ok(PathBuf::from(v));
    }
    richclip::paths::default_data_dir().context("failed to determine data directory")
}

fn resolve_socket_path() -> anyhow::Result<PathBuf> {
    if let Ok(v) = std::env::var("RICHCLIP_SOCKET") {
        return Ok(PathBuf::from(v));
    }
    richclip::paths::default_socket_path().context("failed to determine socket path")
}

fn resolve_cache_dir() -> anyhow::Result<PathBuf> {
    if let Ok(v) = std::env::var("RICHCLIP_CACHE_DIR") {
        return Ok(PathBuf::from(v));
    }
    richclip::paths::default_cache_dir().context("failed to determine cache directory")
}
