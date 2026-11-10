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
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{Mutex, broadcast};
use uuid::Uuid;

use crate::filter::{CaptureConfig, accepted_mimes};
use richclip::ipc::{
    AddItemParams, Request, Response, UpdateItemParams, WatchEvent, WatchEventsParams,
};
use richclip::{AddItemResult, Store};

// ---------------------------------------------------------------------------
// Shared daemon state
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
pub struct RestoreSuppression(Arc<std::sync::Mutex<Option<RestoreMarker>>>);

struct RestoreMarker {
    token: Uuid,
    fingerprint: HashSet<(String, String)>,
    active: bool,
}

impl RestoreSuppression {
    fn arm(&self, token: Uuid, fingerprint: HashSet<(String, String)>) {
        if let Ok(mut marker) = self.0.lock() {
            *marker = Some(RestoreMarker {
                token,
                fingerprint,
                active: false,
            });
        }
    }

    fn activate(&self, token: Uuid) {
        if let Ok(mut marker) = self.0.lock()
            && let Some(marker) = marker.as_mut()
            && marker.token == token
        {
            marker.active = true;
        }
    }

    /// Consume the restore echo once. A different capture clears an active
    /// marker, so a later genuine copy of the restored content reaches Store.
    pub fn observe_capture(&self, fingerprint: &HashSet<(String, String)>) -> bool {
        let Ok(mut marker) = self.0.lock() else {
            return false;
        };
        let Some(current) = marker.as_ref() else {
            return false;
        };

        if &current.fingerprint == fingerprint {
            *marker = None;
            true
        } else {
            if current.active {
                *marker = None;
            }
            false
        }
    }

    fn clear(&self, token: Uuid) {
        if let Ok(mut marker) = self.0.lock()
            && marker
                .as_ref()
                .is_some_and(|current| current.token == token)
        {
            *marker = None;
        }
    }
}

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
    /// The capture consumer checks captures against the active restore's
    /// fingerprint. It consumes one matching echo, clears on the next different
    /// capture or when that restore fails or is cancelled, and leaves an
    /// awaiting marker in place while the selection request is being flushed.
    pub suppression: RestoreSuppression,
}

impl DaemonState {
    pub fn new(store: Store, watch_capacity: usize, cache_dir: PathBuf) -> Self {
        let (watch_tx, _) = broadcast::channel(watch_capacity);
        Self {
            store: Arc::new(Mutex::new(store)),
            watch_tx,
            cache_dir,
            suppression: RestoreSuppression::default(),
        }
    }
}

/// Apply the normal daemon side effects for a committed add or replacement.
/// Deleted events precede the added event, whose MIME list reflects inherited
/// enrichment and other formats present after the commit.
pub fn publish_added_item(
    store: &Store,
    cache_dir: &Path,
    watch_tx: &broadcast::Sender<WatchEvent>,
    added: AddItemResult,
) {
    if let Some(error) = &added.blob_cleanup_error {
        tracing::warn!(id = %added.id, "blob cleanup after add failed: {error}");
    }

    for id in added.removed_ids {
        let _ = std::fs::remove_file(richclip::paths::thumb_path(cache_dir, id));
        let _ = watch_tx.send(WatchEvent::ItemDeleted { id });
    }

    if let Err(e) = richclip::thumbnail::generate_item_thumbnail(store, cache_dir, added.id) {
        tracing::warn!(id = %added.id, "thumbnail generation failed: {e}");
    }

    let _ = watch_tx.send(WatchEvent::ItemAdded {
        id: added.id,
        created_at: added.created_at,
        formats: added.formats,
    });
}

/// Fingerprint the formats the capture path would retain from a restore offer.
/// This uses capture's existing MIME allowlist, parameter stripping, casing,
/// and first-base-type-wins behavior.
pub fn restore_capture_fingerprint(formats: &[(String, Vec<u8>)]) -> HashSet<(String, String)> {
    let offered: Vec<String> = formats.iter().map(|(mime, _)| mime.clone()).collect();
    accepted_mimes(&CaptureConfig::default(), &offered)
        .into_iter()
        .filter_map(|accepted| {
            let bytes = formats
                .iter()
                .find(|(mime, _)| *mime == accepted.offered)
                .map(|(_, bytes)| bytes)?;
            Some((accepted.store_as, richclip::blob_hash(bytes)))
        })
        .collect()
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
    match store.add_item_with_result(&p.formats) {
        Ok(added) => {
            let id = added.id;
            publish_added_item(&store, &state.cache_dir, &state.watch_tx, added);
            drop(store);
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
    let fingerprint = restore_capture_fingerprint(&formats);

    // ── 4. Spawn restore thread ───────────────────────────────────────────────
    // The restore runs in a dedicated thread so it doesn't block the async
    // runtime.  We return `ok` immediately — the caller can paste straight away.
    // RUNTIME-UNVERIFIED: run_restore connects to Wayland and blocks until
    // the source is cancelled (another selection replaces ours).
    let token = Uuid::now_v7();
    let suppression = state.suppression.clone();
    std::thread::spawn(move || {
        let arm = suppression.clone();
        let activate = suppression.clone();
        let result = crate::restore::run_restore_with_callbacks(
            formats,
            move || arm.arm(token, fingerprint),
            move || activate.activate(token),
        );
        suppression.clear(token);
        if let Err(e) = result {
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fingerprint(mime: &str, bytes: &[u8]) -> HashSet<(String, String)> {
        HashSet::from([(mime.to_string(), richclip::blob_hash(bytes))])
    }

    #[test]
    fn restore_fingerprint_uses_capture_mime_normalization() {
        let actual = restore_capture_fingerprint(&[
            ("TEXT/PLAIN;charset=utf-8".into(), b"A".to_vec()),
            ("application/x-richclip-label".into(), b"label".to_vec()),
        ]);
        assert_eq!(actual, fingerprint("text/plain", b"A"));
    }

    #[test]
    fn restore_echo_is_suppressed_once() {
        let suppression = RestoreSuppression::default();
        let token = Uuid::now_v7();
        let restored = fingerprint("text/plain", b"A");
        suppression.arm(token, restored.clone());
        suppression.activate(token);

        assert!(suppression.observe_capture(&restored));
        assert!(!suppression.observe_capture(&restored));
    }

    #[test]
    fn different_capture_clears_marker_before_a_later_copy() {
        let suppression = RestoreSuppression::default();
        let token = Uuid::now_v7();
        let restored = fingerprint("text/plain", b"A");
        let other = fingerprint("text/plain", b"B");
        suppression.arm(token, restored.clone());
        suppression.activate(token);

        assert!(!suppression.observe_capture(&other));
        assert!(!suppression.observe_capture(&restored));
    }

    #[test]
    fn failed_or_cancelled_restore_clears_only_its_marker() {
        let suppression = RestoreSuppression::default();
        let old_token = Uuid::now_v7();
        let new_token = Uuid::now_v7();
        let old = fingerprint("text/plain", b"A");
        let new = fingerprint("text/plain", b"B");

        suppression.arm(old_token, old);
        suppression.clear(old_token); // failed before selection
        suppression.arm(new_token, new.clone());
        suppression.clear(old_token); // prior restore exits after replacement

        assert!(suppression.observe_capture(&new));
        let cancelled_token = Uuid::now_v7();
        suppression.arm(cancelled_token, new.clone());
        suppression.activate(cancelled_token);
        suppression.clear(cancelled_token); // cancelled restore
        assert!(!suppression.observe_capture(&new));
    }

    #[test]
    fn replacement_publishes_deleted_and_final_added_events_and_thumbnails() {
        const BMP: &[u8] = &[
            0x42, 0x4d, 0x3a, 0, 0, 0, 0, 0, 0, 0, 0x36, 0, 0, 0, 0x28, 0, 0, 0, 1, 0, 0, 0, 1, 0,
            0, 0, 1, 0, 24, 0, 0, 0, 0, 0, 4, 0, 0, 0, 0x13, 0x0b, 0, 0, 0x13, 0x0b, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0xff, 0,
        ];

        let data_dir = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();
        let mut store = Store::open(data_dir.path()).unwrap();
        let old_id = store
            .add_item(&[("image/bmp".into(), BMP.to_vec())])
            .unwrap();
        store
            .set_format(old_id, "application/x-richclip-label", b"old label")
            .unwrap();

        // Simulate a second legacy duplicate made by later update operations.
        let legacy_id = store
            .add_item(&[("text/plain".into(), b"temporary".to_vec())])
            .unwrap();
        store.set_format(legacy_id, "image/bmp", BMP).unwrap();
        store.remove_format(legacy_id, "text/plain").unwrap();
        store
            .set_format(legacy_id, "application/x-richclip-label", b"new label")
            .unwrap();

        let old_ids = [legacy_id, old_id];
        let old_thumbs: Vec<_> = old_ids
            .iter()
            .map(|id| richclip::paths::thumb_path(cache_dir.path(), *id))
            .collect();
        for path in &old_thumbs {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"old thumbnail").unwrap();
        }

        let added = store
            .add_item_with_result(&[("image/bmp".into(), BMP.to_vec())])
            .unwrap();
        let added_id = added.id;
        let (watch_tx, _) = broadcast::channel(4);
        let mut events = watch_tx.subscribe();
        publish_added_item(&store, cache_dir.path(), &watch_tx, added);

        assert!(old_thumbs.iter().all(|path| !path.exists()));
        assert!(richclip::paths::thumb_path(cache_dir.path(), added_id).exists());
        for expected_id in old_ids {
            assert!(matches!(
                events.try_recv().unwrap(),
                WatchEvent::ItemDeleted { id } if id == expected_id
            ));
        }
        match events.try_recv().unwrap() {
            WatchEvent::ItemAdded { id, formats, .. } => {
                assert_eq!(id, added_id);
                assert!(
                    formats
                        .iter()
                        .any(|mime| mime == "application/x-richclip-label")
                );
            }
            other => panic!("expected item-added after item-deleted, got {other:?}"),
        }
    }
}
