//! Daemon IPC server: accept loop, connection handler, broadcast state.
//!
//! This module owns the shared state that the IPC handlers and the capture
//! consumer task both need:
//!
//! - `Arc<Mutex<Store>>` — single writer for all DB mutations.
//! - `broadcast::Sender<WatchEvent>` — fan-out to all active `watch` streams.
//!
//! The connection handler reads one newline-delimited JSON request, dispatches
//! it, and (for non-Watch requests) writes one JSON response line.  `Watch`
//! connections are long-lived: events are streamed until the client disconnects
//! (detected by a write error).

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{Mutex, broadcast};
use uuid::Uuid;

use richclip::Store;
use richclip::ipc::{
    AddItemParams, Request, Response, UpdateItemParams, WatchEvent, WatchEventsParams,
};

// ---------------------------------------------------------------------------
// Shared daemon state
// ---------------------------------------------------------------------------

/// Self-capture suppression marker: the set of `(mime, blob_hash)` fingerprint
/// pairs for the most-recently restored item, shared across daemon tasks.
/// `None` means no restore is in flight.
type SuppressionMarker = Arc<std::sync::Mutex<Option<HashSet<(String, String)>>>>;

/// Shared state threaded through the daemon's tasks.
#[derive(Clone)]
pub struct DaemonState {
    pub store: Arc<Mutex<Store>>,
    pub watch_tx: broadcast::Sender<WatchEvent>,

    /// Directory where per-item thumbnail files are written.
    ///
    /// Resolved at startup from `RICHCLIP_CACHE_DIR` or
    /// `richclip::paths::default_cache_dir()`.
    pub cache_dir: PathBuf,

    /// Self-capture suppression marker.
    ///
    /// When the daemon performs a `restore`, it records the set of
    /// `(mime, blob_hash)` pairs for the restored item here.  The capture
    /// consumer (in `richclipd.rs`) checks each freshly captured item against
    /// this set: if they match, the captured item is the daemon's own restored
    /// selection echoing back, and it is silently discarded instead of being
    /// stored as a new history entry.
    ///
    /// `None` means no restore is in flight (or the last captured item didn't
    /// match).  `Some(set)` holds the fingerprint of the most-recently restored
    /// item.
    pub suppression: SuppressionMarker,
}

impl DaemonState {
    pub fn new(store: Store, watch_capacity: usize, cache_dir: PathBuf) -> Self {
        let (watch_tx, _) = broadcast::channel(watch_capacity);
        Self {
            store: Arc::new(Mutex::new(store)),
            watch_tx,
            cache_dir,
            suppression: Arc::new(std::sync::Mutex::new(None)),
        }
    }
}

// ---------------------------------------------------------------------------
// Connection handler
// ---------------------------------------------------------------------------

/// Handle a single IPC connection.
///
/// Reads one request line, dispatches to the right handler, and writes the
/// response (or streams events for `WatchEvents`).  Errors from handlers are
/// mapped to `Response::err` and written back; panics are prevented by
/// catching all errors at this level.
pub async fn handle_conn(stream: UnixStream, state: DaemonState) {
    if let Err(e) = handle_conn_inner(stream, state).await {
        tracing::debug!("IPC connection closed: {e}");
    }
}

async fn handle_conn_inner(stream: UnixStream, state: DaemonState) -> anyhow::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    let mut request_line = String::new();
    let n = reader.read_line(&mut request_line).await?;
    if n == 0 {
        return Ok(()); // client disconnected before sending anything
    }

    let request: Request =
        serde_json::from_str(request_line.trim()).context("failed to parse request")?;

    match request {
        Request::AddItem(p) => {
            let resp = handle_add_item(p, &state).await;
            write_response(&mut write_half, &resp).await?;
        }
        Request::UpdateItem(p) => {
            let resp = handle_update_item(p, &state).await;
            write_response(&mut write_half, &resp).await?;
        }
        Request::DeleteItem { id } => {
            let resp = handle_delete_item(id, &state).await;
            write_response(&mut write_half, &resp).await?;
        }
        Request::ListItems(p) => {
            let resp = handle_list_items(p, &state).await;
            write_response(&mut write_half, &resp).await?;
        }
        Request::GetItem { id } => {
            let resp = handle_get_item(id, &state).await;
            write_response(&mut write_half, &resp).await?;
        }
        Request::RestoreItem { id } => {
            let resp = handle_restore_item(id, &state).await;
            write_response(&mut write_half, &resp).await?;
        }
        Request::WatchEvents(p) => {
            handle_watch_events(p, &mut write_half, &state).await;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Request handlers
// ---------------------------------------------------------------------------

async fn handle_add_item(p: AddItemParams, state: &DaemonState) -> Response {
    let mut store = state.store.lock().await;
    match store.add_item(&p.formats) {
        Ok(id) => {
            // Gather mimes for the broadcast event.
            let mimes: Vec<String> = p.formats.iter().map(|(m, _)| m.clone()).collect();
            let created_at = {
                // Re-read to get the actual timestamp stored.
                match store.get_item(id) {
                    Ok(iwf) => iwf.item.created_at,
                    Err(_) => time::OffsetDateTime::now_utc(),
                }
            };
            drop(store); // release lock before broadcasting
            let _ = state.watch_tx.send(WatchEvent::ItemAdded {
                id,
                created_at,
                formats: mimes,
            });
            Response::ok(Some(serde_json::json!({ "id": id })))
        }
        Err(e) => Response::err(e.to_string(), e.json_code()),
    }
}

async fn handle_update_item(p: UpdateItemParams, state: &DaemonState) -> Response {
    let mut store = state.store.lock().await;

    let mut changed: Vec<String> = Vec::new();

    // Apply removes first.
    for mime in &p.remove_mimes {
        match store.remove_format(p.id, mime) {
            Ok(()) => changed.push(mime.clone()),
            Err(e) => return Response::err(e.to_string(), e.json_code()),
        }
    }

    // Then apply sets.
    for (mime, bytes) in &p.set_formats {
        match store.set_format(p.id, mime, bytes) {
            Ok(()) => changed.push(mime.clone()),
            Err(e) => return Response::err(e.to_string(), e.json_code()),
        }
    }

    drop(store);

    if !changed.is_empty() {
        let _ = state
            .watch_tx
            .send(WatchEvent::ItemUpdated { id: p.id, changed });
    }

    Response::ok(None)
}

async fn handle_delete_item(id: Uuid, state: &DaemonState) -> Response {
    let mut store = state.store.lock().await;
    match store.delete_item(id) {
        Ok(()) => {
            drop(store);
            let _ = state.watch_tx.send(WatchEvent::ItemDeleted { id });
            // Best-effort thumbnail cleanup.
            let thumb = richclip::paths::thumb_path(&state.cache_dir, id);
            let _ = std::fs::remove_file(&thumb);
            Response::ok(None)
        }
        Err(e) => Response::err(e.to_string(), e.json_code()),
    }
}

async fn handle_list_items(p: richclip::ipc::ListItemsParams, state: &DaemonState) -> Response {
    let store = state.store.lock().await;
    match store.list_items(p.limit, p.mime_filter.as_deref()) {
        Ok(items) => match serde_json::to_value(items) {
            Ok(v) => Response::ok(Some(v)),
            Err(e) => Response::err(e.to_string(), "error"),
        },
        Err(e) => Response::err(e.to_string(), e.json_code()),
    }
}

async fn handle_get_item(id: Uuid, state: &DaemonState) -> Response {
    let store = state.store.lock().await;
    match store.get_item(id) {
        Ok(iwf) => match serde_json::to_value(iwf) {
            Ok(v) => Response::ok(Some(v)),
            Err(e) => Response::err(e.to_string(), "error"),
        },
        Err(e) => Response::err(e.to_string(), e.json_code()),
    }
}

async fn handle_restore_item(id: Uuid, state: &DaemonState) -> Response {
    // ── 1. Look up the item ───────────────────────────────────────────────────
    let store = state.store.lock().await;
    let iwf = match store.get_item(id) {
        Ok(v) => v,
        Err(e) => return Response::err(e.to_string(), e.json_code()),
    };

    // ── 2. Collect advertisable formats (exclude application/x-richclip-*) ────
    let mut formats: Vec<(String, Vec<u8>)> = Vec::new();
    for fmt in &iwf.formats {
        if fmt.mime.starts_with("application/x-richclip-") {
            continue;
        }
        match store.decode(id, &fmt.mime) {
            Ok(bytes) => formats.push((fmt.mime.clone(), bytes)),
            Err(e) => {
                tracing::warn!(
                    "restore: failed to decode {}/{}: {e}; skipping format",
                    id,
                    fmt.mime
                );
            }
        }
    }
    drop(store); // release the lock before spawning

    if formats.is_empty() {
        return Response::err("item has no advertisable formats", "error");
    }

    // ── 3. Build suppression fingerprint ─────────────────────────────────────
    // Record the set of (mime, blob_hash) pairs for the formats we are about
    // to advertise.  The capture consumer checks incoming clipboard captures
    // against this set and discards exact matches (self-restore echoes).
    let fingerprint: HashSet<(String, String)> = formats
        .iter()
        .map(|(mime, bytes)| (mime.clone(), richclip::blob_hash(bytes)))
        .collect();

    // Store the fingerprint; replace any previous one.
    if let Ok(mut guard) = state.suppression.lock() {
        *guard = Some(fingerprint);
    }

    // ── 4. Spawn restore thread ───────────────────────────────────────────────
    // The restore runs in a dedicated thread so it doesn't block the async
    // runtime.  We return `ok` immediately — the caller can paste straight away.
    // RUNTIME-UNVERIFIED: run_restore connects to Wayland and blocks until
    // the source is cancelled (another selection replaces ours).
    std::thread::spawn(move || {
        if let Err(e) = crate::restore::run_restore(formats) {
            tracing::warn!("restore thread exited with error: {e}");
        }
    });

    Response::ok(None)
}

// ---------------------------------------------------------------------------
// Watch stream handler
// ---------------------------------------------------------------------------

/// Subscribe to the broadcast channel and stream matching events to the client.
///
/// Runs until the client disconnects (write error) or the broadcast sender is
/// dropped.  Applies `event_filter` (match on the event tag) and
/// `mime_filters` (pass if the event carries any requested mime).
async fn handle_watch_events(
    params: WatchEventsParams,
    write_half: &mut tokio::net::unix::OwnedWriteHalf,
    state: &DaemonState,
) {
    let mut rx = state.watch_tx.subscribe();

    loop {
        let event = match rx.recv().await {
            Ok(ev) => ev,
            Err(broadcast::error::RecvError::Closed) => break,
            Err(broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!("watch client lagged, {n} events dropped");
                continue;
            }
        };

        // Apply event_filter.
        if let Some(ref filter) = params.event_filter {
            let tag = event_tag(&event);
            if tag != filter.as_str() {
                continue;
            }
        }

        // Apply mime_filters with OR semantics across exact MIME matches.
        if !params.mime_filters.is_empty()
            && !params
                .mime_filters
                .iter()
                .any(|mime| event_matches_mime(&event, mime))
        {
            continue;
        }

        // Serialize and send.
        let mut line = match serde_json::to_string(&event) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("failed to serialize watch event: {e}");
                continue;
            }
        };
        line.push('\n');

        if write_half.write_all(line.as_bytes()).await.is_err() {
            break; // client disconnected
        }
    }
}

/// Return the serde kebab-case tag for a `WatchEvent`.
fn event_tag(ev: &WatchEvent) -> &'static str {
    match ev {
        WatchEvent::ItemAdded { .. } => "item-added",
        WatchEvent::ItemUpdated { .. } => "item-updated",
        WatchEvent::ItemDeleted { .. } => "item-deleted",
    }
}

/// Returns true if the event passes a single exact MIME filter.
///
/// - `ItemAdded`: pass if the `formats` list contains `mime`.
/// - `ItemUpdated`: pass if the `changed` list contains `mime`.
/// - `ItemDeleted`: always pass (no format information available post-delete).
fn event_matches_mime(ev: &WatchEvent, mime: &str) -> bool {
    match ev {
        WatchEvent::ItemAdded { formats, .. } => formats.iter().any(|m| m == mime),
        WatchEvent::ItemUpdated { changed, .. } => changed.iter().any(|m| m == mime),
        WatchEvent::ItemDeleted { .. } => true,
    }
}

// ---------------------------------------------------------------------------
// Response serialization helper
// ---------------------------------------------------------------------------

async fn write_response(
    write_half: &mut tokio::net::unix::OwnedWriteHalf,
    resp: &Response,
) -> anyhow::Result<()> {
    let mut line = serde_json::to_string(resp)?;
    line.push('\n');
    write_half.write_all(line.as_bytes()).await?;
    Ok(())
}
