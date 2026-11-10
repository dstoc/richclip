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

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex as StdMutex};

use anyhow::Context;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{Mutex, broadcast};
use uuid::Uuid;

use crate::filter::{CaptureConfig, accepted_mimes};
use richclip::backend::{CaptureOrderingBarrier, CapturedItem};
use richclip::ipc::{
    AddItemParams, Request, Response, UpdateItemParams, WatchEvent, WatchEventsParams,
};
use richclip::{AddItemResult, Store};

// ---------------------------------------------------------------------------
// Shared daemon state
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
pub struct RestoreSuppression(Arc<std::sync::Mutex<SuppressionState>>);

/// Coordinates restore setup with the capture connection's pending event
/// queue. A restore marker is armed only after this barrier has dispatched
/// selection notifications that were already pending before the restore.
#[derive(Clone)]
pub struct CaptureOrdering(Arc<(StdMutex<CaptureOrderingState>, Condvar)>);

enum CaptureOrderingState {
    Starting,
    Ready(CaptureOrderingBarrier),
    Unavailable,
}

impl Default for CaptureOrdering {
    fn default() -> Self {
        Self(Arc::new((
            StdMutex::new(CaptureOrderingState::Unavailable),
            Condvar::new(),
        )))
    }
}

impl CaptureOrdering {
    fn starting() -> Self {
        Self(Arc::new((
            StdMutex::new(CaptureOrderingState::Starting),
            Condvar::new(),
        )))
    }

    /// Install the barrier provided by the initialized capture backend.
    pub fn install(&self, barrier: CaptureOrderingBarrier) {
        let (state, ready) = &*self.0;
        if let Ok(mut state) = state.lock() {
            *state = CaptureOrderingState::Ready(barrier);
            ready.notify_all();
        }
    }

    /// Mark capture unavailable so restore setup does not wait forever when
    /// the compositor or capture protocol is unavailable.
    pub fn mark_unavailable(&self) {
        let (state, ready) = &*self.0;
        if let Ok(mut state) = state.lock() {
            *state = CaptureOrderingState::Unavailable;
            ready.notify_all();
        }
    }

    /// Wait until capture setup is resolved, then dispatch the pending-event
    /// barrier if the capture backend is running.
    pub fn synchronize(&self) -> Result<(), String> {
        let barrier = {
            let (state, ready) = &*self.0;
            let mut state = state
                .lock()
                .map_err(|_| "capture ordering state was poisoned".to_string())?;
            while matches!(*state, CaptureOrderingState::Starting) {
                state = ready
                    .wait(state)
                    .map_err(|_| "capture ordering state was poisoned".to_string())?;
            }
            match &*state {
                CaptureOrderingState::Ready(barrier) => Some(barrier.clone()),
                CaptureOrderingState::Unavailable => None,
                CaptureOrderingState::Starting => unreachable!(),
            }
        };

        if let Some(barrier) = barrier {
            barrier()
        } else {
            Ok(())
        }
    }
}

#[derive(Default)]
struct SuppressionState {
    markers: VecDeque<RestoreMarker>,
    latest_selection_id: u64,
}

struct RestoreMarker {
    token: Uuid,
    fingerprint: HashSet<(String, String)>,
    active: bool,
    after_selection_id: u64,
}

/// Ordered messages shared by the capture backend and restore threads.
///
/// Keeping restore lifecycle updates in this queue makes their order relative
/// to captured and skipped selections visible to the delayed consumer. The
/// capture start message also lets it distinguish a delayed drain from a
/// selection that began after a restore was armed.
pub enum CaptureMessage {
    SelectionStarted {
        selection_id: u64,
    },
    Item(CapturedItem),
    SelectionSkipped,
    OrderedItem {
        selection_id: u64,
        item: CapturedItem,
    },
    OrderedSelectionSkipped {
        selection_id: u64,
    },
    Restore(RestoreSuppressionEvent),
}

/// Restore lifecycle changes applied by the capture consumer in queue order.
pub enum RestoreSuppressionEvent {
    Arm {
        token: Uuid,
        fingerprint: HashSet<(String, String)>,
    },
    Activate {
        token: Uuid,
    },
    Clear {
        token: Uuid,
    },
}

impl CaptureMessage {
    /// Apply state-only suppression updates for this ordered message.
    pub fn apply_suppression_updates(&self, suppression: &RestoreSuppression) {
        match self {
            Self::Restore(event) => suppression.apply_event(event),
            Self::SelectionStarted { selection_id } => suppression.selection_started(*selection_id),
            Self::OrderedSelectionSkipped { selection_id } => {
                suppression.observe_skipped_selection_for(*selection_id)
            }
            Self::SelectionSkipped => suppression.observe_skipped_selection(),
            Self::Item(_) | Self::OrderedItem { .. } => {}
        }
    }
}

impl RestoreSuppression {
    fn arm(&self, token: Uuid, fingerprint: HashSet<(String, String)>) {
        if let Ok(mut state) = self.0.lock() {
            let after_selection_id = state.latest_selection_id;
            state.markers.push_back(RestoreMarker {
                token,
                fingerprint,
                active: false,
                after_selection_id,
            });
        }
    }

    fn activate(&self, token: Uuid) {
        if let Ok(mut state) = self.0.lock()
            && let Some(marker) = state
                .markers
                .iter_mut()
                .find(|marker| marker.token == token)
        {
            marker.active = true;
        }
    }

    fn selection_started(&self, selection_id: u64) {
        if let Ok(mut state) = self.0.lock() {
            state.latest_selection_id = state.latest_selection_id.max(selection_id);
        }
    }

    fn apply_event(&self, event: &RestoreSuppressionEvent) {
        match event {
            RestoreSuppressionEvent::Arm { token, fingerprint } => {
                self.arm(*token, fingerprint.clone());
            }
            RestoreSuppressionEvent::Activate { token } => self.activate(*token),
            RestoreSuppressionEvent::Clear { token } => self.clear(*token),
        }
    }

    /// Consume one matching restore echo. Keep separate markers for repeated
    /// restores because their captures may already be queued when the consumer
    /// reaches them. A different capture clears markers armed before that
    /// selection started so a later genuine copy reaches Store.
    pub fn observe_capture_for(
        &self,
        selection_id: u64,
        fingerprint: &HashSet<(String, String)>,
    ) -> bool {
        let Ok(mut state) = self.0.lock() else {
            return false;
        };

        let matching_token = state
            .markers
            .iter()
            .filter(|marker| selection_id > marker.after_selection_id)
            .find(|marker| &marker.fingerprint == fingerprint)
            .map(|marker| marker.token);
        if let Some(matching_token) = matching_token {
            let mut passed_match = false;
            state.markers.retain(|marker| {
                if selection_id <= marker.after_selection_id {
                    return true;
                }
                if marker.token == matching_token {
                    passed_match = true;
                    false
                } else {
                    passed_match || !marker.active
                }
            });
            true
        } else {
            // A selection that started before a restore must not cancel its
            // marker, even if its blocking drain completes afterward.
            state
                .markers
                .retain(|marker| selection_id <= marker.after_selection_id);
            false
        }
    }

    /// Observe a capture that does not carry a selection-start ID.
    ///
    /// This preserves the original public API for non-Wayland callers. Such a
    /// capture is treated as newer than every selection start already seen.
    pub fn observe_capture(&self, fingerprint: &HashSet<(String, String)>) -> bool {
        let selection_id = self.next_unstarted_selection_id();
        self.observe_capture_for(selection_id, fingerprint)
    }

    fn clear(&self, token: Uuid) {
        if let Ok(mut state) = self.0.lock() {
            // A restore that never activated cannot have produced an echo and
            // should not leave state behind after a failed setup. An activated
            // marker remains until its echo or a subsequent different capture
            // is observed; the echo may already be queued when its source is
            // cancelled by a newer selection.
            state
                .markers
                .retain(|marker| marker.token != token || marker.active);
        }
    }

    /// Retire markers for selections that began before this skipped selection.
    pub fn observe_skipped_selection(&self) {
        let selection_id = self.next_unstarted_selection_id();
        self.observe_skipped_selection_for(selection_id);
    }

    fn observe_skipped_selection_for(&self, selection_id: u64) {
        if let Ok(mut state) = self.0.lock() {
            state
                .markers
                .retain(|marker| selection_id <= marker.after_selection_id);
        }
    }

    fn next_unstarted_selection_id(&self) -> u64 {
        self.0
            .lock()
            .map(|state| state.latest_selection_id.saturating_add(1))
            .unwrap_or(0)
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
    /// The capture consumer checks captures against restore fingerprints in
    /// selection order. It consumes one matching echo per restore and clears
    /// older markers on a different capture or skipped selection. Markers
    /// remain pending after an active source is cancelled because its echo may
    /// already be queued.
    pub suppression: RestoreSuppression,

    /// Capture-connection barrier used to order restore markers after pending
    /// selection notifications.
    pub capture_ordering: CaptureOrdering,

    /// Serializes the short restore setup window through selection activation.
    restore_selection_gate: Arc<tokio::sync::Semaphore>,

    /// Ordered channel shared by capture observations and restore lifecycle
    /// updates. The daemon wires its consumer before accepting IPC requests.
    capture_tx: Option<tokio::sync::mpsc::UnboundedSender<CaptureMessage>>,
}

impl DaemonState {
    pub fn new(store: Store, watch_capacity: usize, cache_dir: PathBuf) -> Self {
        let (watch_tx, _) = broadcast::channel(watch_capacity);
        Self {
            store: Arc::new(Mutex::new(store)),
            watch_tx,
            cache_dir,
            suppression: RestoreSuppression::default(),
            capture_ordering: CaptureOrdering::default(),
            restore_selection_gate: Arc::new(tokio::sync::Semaphore::new(1)),
            capture_tx: None,
        }
    }

    /// Connect the ordered capture consumer channel used for restore updates.
    pub fn with_capture_events(
        mut self,
        capture_tx: tokio::sync::mpsc::UnboundedSender<CaptureMessage>,
    ) -> Self {
        self.capture_tx = Some(capture_tx);
        self.capture_ordering = CaptureOrdering::starting();
        self
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
    let config = CaptureConfig::default();
    let mut total_bytes = 0;
    let mut fingerprint = HashSet::new();

    for accepted in accepted_mimes(&config, &offered) {
        let Some(bytes) = formats
            .iter()
            .find(|(mime, _)| *mime == accepted.offered)
            .map(|(_, bytes)| bytes)
        else {
            continue;
        };

        // capture.rs stops accepting formats at the first size-limit failure.
        if !crate::filter::within_size_limits(&config, total_bytes, bytes.len()) {
            break;
        }
        total_bytes = total_bytes.saturating_add(bytes.len());
        fingerprint.insert((accepted.store_as, richclip::blob_hash(bytes)));
    }

    fingerprint
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

    let Some(capture_tx) = state.capture_tx.clone() else {
        return Response::err("restore capture event queue is not configured", "error");
    };

    let selection_permit = match state.restore_selection_gate.clone().acquire_owned().await {
        Ok(permit) => permit,
        Err(_) => return Response::err("restore selection gate is closed", "error"),
    };

    // ── 3. Build suppression fingerprint ─────────────────────────────────────
    let fingerprint = restore_capture_fingerprint(&formats);
    let capture_ordering = state.capture_ordering.clone();

    // ── 4. Spawn restore thread ───────────────────────────────────────────────
    // The restore runs in a dedicated thread so it doesn't block the async
    // runtime.  We return `ok` immediately — the caller can paste straight away.
    // RUNTIME-UNVERIFIED: run_restore connects to Wayland and blocks until
    // the source is cancelled (another selection replaces ours).
    let token = Uuid::now_v7();
    let arm_tx = capture_tx.clone();
    let activate_tx = capture_tx.clone();
    std::thread::spawn(move || {
        let mut selection_permit = Some(selection_permit);
        let result = crate::restore::run_restore_with_callbacks(
            formats,
            move || match capture_ordering.synchronize() {
                Ok(()) => {
                    let _ = arm_tx.send(CaptureMessage::Restore(RestoreSuppressionEvent::Arm {
                        token,
                        fingerprint,
                    }));
                }
                Err(error) => {
                    tracing::warn!("could not order restore marker with capture events: {error}");
                }
            },
            move || {
                let _ =
                    activate_tx.send(CaptureMessage::Restore(RestoreSuppressionEvent::Activate {
                        token,
                    }));
                drop(selection_permit.take());
            },
        );
        let _ = capture_tx.send(CaptureMessage::Restore(RestoreSuppressionEvent::Clear {
            token,
        }));
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

    fn observe_selection(
        suppression: &RestoreSuppression,
        selection_id: u64,
        fingerprint: &HashSet<(String, String)>,
    ) -> bool {
        suppression.selection_started(selection_id);
        suppression.observe_capture_for(selection_id, fingerprint)
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

        assert!(observe_selection(&suppression, 1, &restored));
        assert!(!observe_selection(&suppression, 2, &restored));
    }

    #[test]
    fn repeated_restores_preserve_each_queued_echo() {
        let suppression = RestoreSuppression::default();
        let restored = fingerprint("text/plain", b"A");
        let first_token = Uuid::now_v7();
        let second_token = Uuid::now_v7();

        suppression.arm(first_token, restored.clone());
        suppression.activate(first_token);
        suppression.arm(second_token, restored.clone());
        suppression.activate(second_token);
        suppression.clear(first_token); // first source is superseded

        // The capture consumer can be behind both completed captures.
        assert!(observe_selection(&suppression, 1, &restored));
        assert!(observe_selection(&suppression, 2, &restored));
        assert!(!observe_selection(&suppression, 3, &restored));
    }

    #[test]
    fn different_capture_clears_marker_before_a_later_copy() {
        let suppression = RestoreSuppression::default();
        let token = Uuid::now_v7();
        let restored = fingerprint("text/plain", b"A");
        let other = fingerprint("text/plain", b"B");
        suppression.arm(token, restored.clone());
        suppression.activate(token);

        assert!(!observe_selection(&suppression, 1, &other));
        assert!(!observe_selection(&suppression, 2, &restored));
    }

    #[test]
    fn failed_and_cancelled_restores_do_not_leave_stale_markers() {
        let suppression = RestoreSuppression::default();
        let restored = fingerprint("text/plain", b"A");
        let other = fingerprint("text/plain", b"B");

        let failed_token = Uuid::now_v7();
        suppression.arm(failed_token, restored.clone());
        suppression.clear(failed_token); // failed before selection was activated
        assert!(!observe_selection(&suppression, 1, &restored));

        let cancelled_token = Uuid::now_v7();
        suppression.arm(cancelled_token, restored.clone());
        suppression.activate(cancelled_token);
        suppression.clear(cancelled_token); // its queued echo may still arrive
        assert!(!observe_selection(&suppression, 2, &other)); // a new selection retires it
        assert!(!observe_selection(&suppression, 3, &restored)); // genuine later copy
    }

    #[test]
    fn skipped_selection_retires_cancelled_restore_marker() {
        let suppression = RestoreSuppression::default();
        let restored = fingerprint("text/plain", b"A");
        let token = Uuid::now_v7();
        suppression.arm(token, restored.clone());
        suppression.activate(token);
        suppression.clear(token);

        suppression.selection_started(1);
        suppression.observe_skipped_selection_for(1); // e.g. clipboard cleared/unsupported
        assert!(!observe_selection(&suppression, 2, &restored));
    }

    fn selection_started(selection_id: u64) -> CaptureMessage {
        CaptureMessage::SelectionStarted { selection_id }
    }

    fn captured_item(selection_id: u64, mime: &str, bytes: &[u8]) -> CaptureMessage {
        CaptureMessage::OrderedItem {
            selection_id,
            item: CapturedItem {
                formats: vec![richclip::backend::CapturedFormat {
                    mime: mime.into(),
                    bytes: bytes.to_vec(),
                }],
            },
        }
    }

    fn apply_queued_message(
        suppression: &RestoreSuppression,
        message: CaptureMessage,
    ) -> Option<bool> {
        message.apply_suppression_updates(suppression);
        match message {
            CaptureMessage::OrderedItem { selection_id, item } => {
                let formats: Vec<(String, Vec<u8>)> = item
                    .formats
                    .into_iter()
                    .map(|format| (format.mime, format.bytes))
                    .collect();
                let fingerprint = restore_capture_fingerprint(&formats);
                Some(suppression.observe_capture_for(selection_id, &fingerprint))
            }
            CaptureMessage::Item(item) => {
                let formats: Vec<(String, Vec<u8>)> = item
                    .formats
                    .into_iter()
                    .map(|format| (format.mime, format.bytes))
                    .collect();
                let fingerprint = restore_capture_fingerprint(&formats);
                Some(suppression.observe_capture(&fingerprint))
            }
            CaptureMessage::SelectionStarted { .. }
            | CaptureMessage::OrderedSelectionSkipped { .. }
            | CaptureMessage::SelectionSkipped
            | CaptureMessage::Restore(_) => None,
        }
    }

    fn check_delayed_consumer(intervening_capture: bool) {
        let restored = fingerprint("text/plain", b"A");
        let first = Uuid::now_v7();
        let second = Uuid::now_v7();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        // Queue restore A and its echo without consuming either message.
        tx.send(CaptureMessage::Restore(RestoreSuppressionEvent::Arm {
            token: first,
            fingerprint: restored.clone(),
        }))
        .unwrap();
        tx.send(CaptureMessage::Restore(RestoreSuppressionEvent::Activate {
            token: first,
        }))
        .unwrap();
        tx.send(selection_started(1)).unwrap();
        tx.send(captured_item(1, "text/plain", b"A")).unwrap();
        tx.send(CaptureMessage::Restore(RestoreSuppressionEvent::Clear {
            token: first,
        }))
        .unwrap();

        // Queue a different copy or a skipped selection after the first
        // restore, then queue a second restore before the consumer catches up.
        tx.send(selection_started(2)).unwrap();
        if intervening_capture {
            tx.send(captured_item(2, "text/plain", b"B")).unwrap();
        } else {
            tx.send(CaptureMessage::OrderedSelectionSkipped { selection_id: 2 })
                .unwrap();
        }
        tx.send(CaptureMessage::Restore(RestoreSuppressionEvent::Arm {
            token: second,
            fingerprint: restored,
        }))
        .unwrap();
        tx.send(CaptureMessage::Restore(RestoreSuppressionEvent::Activate {
            token: second,
        }))
        .unwrap();
        tx.send(selection_started(3)).unwrap();
        tx.send(captured_item(3, "text/plain", b"A")).unwrap();
        // A later genuine copy must still be accepted.
        tx.send(selection_started(4)).unwrap();
        tx.send(captured_item(4, "text/plain", b"A")).unwrap();

        let suppression = RestoreSuppression::default();
        let outcomes: Vec<_> = std::iter::from_fn(|| {
            rx.try_recv()
                .ok()
                .map(|message| apply_queued_message(&suppression, message))
        })
        .collect();
        assert_eq!(
            outcomes,
            vec![
                None,
                None,
                None,
                Some(true),
                None,
                None,
                if intervening_capture {
                    Some(false)
                } else {
                    None
                },
                None,
                None,
                None,
                Some(true),
                None,
                Some(false),
            ],
            "an older queued selection must not retire a later restore echo marker"
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn review_queued_copy_between_restores_preserves_new_echo() {
        check_delayed_consumer(true);
    }

    #[test]
    fn review_queued_clear_between_restores_preserves_new_echo() {
        check_delayed_consumer(false);
    }

    #[test]
    fn older_selection_cannot_revive_pending_restore_marker() {
        let suppression = RestoreSuppression::default();
        let token = Uuid::now_v7();
        let restored = fingerprint("text/plain", b"A");

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tx.send(CaptureMessage::Restore(RestoreSuppressionEvent::Arm {
            token,
            fingerprint: restored,
        }))
        .unwrap();
        tx.send(selection_started(1)).unwrap();
        tx.send(captured_item(1, "text/plain", b"B")).unwrap();
        tx.send(CaptureMessage::Restore(RestoreSuppressionEvent::Activate {
            token,
        }))
        .unwrap();
        tx.send(selection_started(2)).unwrap();
        tx.send(captured_item(2, "text/plain", b"A")).unwrap();

        assert_eq!(
            apply_queued_message(&suppression, rx.try_recv().unwrap()),
            None
        );
        assert_eq!(
            apply_queued_message(&suppression, rx.try_recv().unwrap()),
            None
        );
        assert_eq!(
            apply_queued_message(&suppression, rx.try_recv().unwrap()),
            Some(false)
        );
        assert_eq!(
            apply_queued_message(&suppression, rx.try_recv().unwrap()),
            None
        );
        assert_eq!(
            apply_queued_message(&suppression, rx.try_recv().unwrap()),
            None
        );
        assert_eq!(
            apply_queued_message(&suppression, rx.try_recv().unwrap()),
            Some(false)
        );
    }

    #[test]
    fn restore_fingerprint_stops_at_per_format_size_limit() {
        let config = CaptureConfig::default();
        let formats = vec![
            ("image/jpeg".into(), b"small image".to_vec()),
            ("image/png".into(), vec![0; config.max_format_bytes + 1]),
        ];

        assert_eq!(
            restore_capture_fingerprint(&formats),
            fingerprint("image/jpeg", b"small image")
        );
    }

    #[test]
    fn restore_fingerprint_stops_at_total_size_limit() {
        let config = CaptureConfig::default();
        let jpeg = vec![0x11; config.max_format_bytes];
        let png = vec![0x22; config.max_format_bytes];
        let webp = vec![0x33; 1];
        let formats = vec![
            ("image/jpeg".into(), jpeg.clone()),
            ("image/png".into(), png.clone()),
            ("image/webp".into(), webp),
        ];

        assert_eq!(
            restore_capture_fingerprint(&formats),
            HashSet::from([
                ("image/jpeg".into(), richclip::blob_hash(&jpeg)),
                ("image/png".into(), richclip::blob_hash(&png)),
            ])
        );
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
