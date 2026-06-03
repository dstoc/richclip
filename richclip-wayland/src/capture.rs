//! Blocking Wayland capture loop using the wlr-data-control protocol.
//!
//! # Runtime verification note
//!
//! This module cannot be exercised without a live Wayland compositor. The
//! following aspects are **not runtime-verified** in this environment and will
//! need validation by the project owner against a real Sway/wlroots session:
//!
//! - That `registry_queue_init` successfully enumerates the compositor globals.
//! - That `globals.bind::<ZwlrDataControlManagerV1, _, _>` succeeds (requires
//!   `zwlr_data_control_manager_v1` to be advertised by the compositor).
//! - That `globals.bind::<WlSeat, _, _>` returns the primary seat.
//! - The `data_offer` → `offer` → `selection` event sequence.
//! - The synchronous `receive` + `UnixStream::pair` drain, in particular that
//!   `conn.flush()` reliably delivers the fd to the compositor before we block
//!   reading, and that the compositor closes the write-end after writing.
//! - Behaviour when multiple MIME types are drained sequentially (head-of-line
//!   blocking; see comment in `drain_format`).
//! - Primary-selection events (not captured; see TODO below).
//! - Multi-seat scenarios (only the first seat is bound; see TODO below).

use std::collections::HashMap;
use std::io::Read;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;

use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_registry, wl_seat::WlSeat},
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
};

use richclip::backend::{CapturedFormat, CapturedItem, CaptureSink};

use crate::filter::{self, CaptureConfig};

// ─── State ────────────────────────────────────────────────────────────────────

/// All mutable state visible inside `Dispatch` event handlers.
///
/// Lives entirely on the Wayland thread; does not need to be `Send`.
struct State {
    config: CaptureConfig,
    sink: CaptureSink,

    /// Accumulated MIME types per offer object, keyed by `ObjectId`.
    /// Entries are inserted when `data_offer` introduces a new offer and
    /// populated by subsequent `ZwlrDataControlOfferV1::offer` events.
    offers: HashMap<wayland_client::backend::ObjectId, Vec<String>>,

    /// The connection handle; stored so `Dispatch` impls can call `flush()`.
    conn: Connection,

    // TODO (P2-M4 restore milestone): When the daemon owns the current
    // selection (after a `restore_item` call), record the source object ID
    // here so that `Selection` events for that offer can be short-circuited
    // without re-capturing what we just restored.
    //
    //   owned_source_id: Option<wayland_client::backend::ObjectId>,
    //
    // The check would look like:
    //   if Some(offer.id()) == state.owned_source_id { return; }
}

// ─── Dispatch<WlRegistry, GlobalListContents> ─────────────────────────────────

// `registry_queue_init` requires this impl on the `State` type.
impl Dispatch<wl_registry::WlRegistry, wayland_client::globals::GlobalListContents> for State {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &wayland_client::globals::GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // Dynamic global add/remove events after initial enumeration. We do
        // not handle hot-plug of the data-control manager or seats; restart
        // the daemon if the compositor's global list changes significantly.
    }
}

// ─── Dispatch<ZwlrDataControlManagerV1, ()> ──────────────────────────────────

impl Dispatch<ZwlrDataControlManagerV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrDataControlManagerV1,
        _event: <ZwlrDataControlManagerV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // The manager has no events defined in the protocol XML.
    }
}

// ─── Dispatch<WlSeat, ()> ────────────────────────────────────────────────────

impl Dispatch<WlSeat, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlSeat,
        _event: <WlSeat as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // We ignore wl_seat capability/name events; we only need the seat
        // object to pass to get_data_device.
        //
        // TODO (future): support multi-seat by subscribing to wl_seat globals
        // via WlRegistry instead of relying on `globals.bind()` for the first.
    }
}

// ─── Dispatch<ZwlrDataControlDeviceV1, ()> ───────────────────────────────────

impl Dispatch<ZwlrDataControlDeviceV1, ()> for State {
    // Declare that the `data_offer` event creates a `ZwlrDataControlOfferV1`
    // child object. The macro overrides `event_created_child()` so the queue
    // allocates the right ObjectData for the new proxy.
    wayland_client::event_created_child!(State, ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ()),
    ]);

    fn event(
        state: &mut Self,
        _device: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_device_v1::Event::DataOffer { id } => {
                // A new offer object has been created. Register an empty MIME
                // list; subsequent `ZwlrDataControlOfferV1::offer` events will
                // populate it before the matching `Selection` event arrives.
                state.offers.insert(id.id(), Vec::new());
            }

            zwlr_data_control_device_v1::Event::Selection { id } => {
                // `id` is `Option<ZwlrDataControlOfferV1>`; `None` means the
                // selection was cleared (clipboard is empty).
                let offer = match id {
                    Some(o) => o,
                    None => {
                        // Clipboard cleared; nothing to capture.
                        return;
                    }
                };

                // TODO (P2-M4): If `offer.id() == state.owned_source_id`, we
                // are the current clipboard owner (after a restore). Skip this
                // event so we do not re-capture our own restore.

                let mimes: Vec<String> = match state.offers.get(&offer.id()) {
                    Some(m) => m.clone(),
                    None => {
                        tracing::warn!(
                            "Selection event for unknown offer {:?}; skipping",
                            offer.id()
                        );
                        return;
                    }
                };

                // ── Sensitivity check (before allowlist) ──────────────────────
                if filter::is_sensitive(&mimes) {
                    tracing::debug!("Skipping sensitive clipboard offer (mime hints: {:?})", mimes);
                    // Clean up.
                    state.offers.remove(&offer.id());
                    return;
                }

                // ── Allowlist filter ─────────────────────────────────────────
                let accepted = filter::accepted_mimes(&state.config, &mimes);
                if accepted.is_empty() {
                    tracing::debug!("No accepted MIMEs in offer {:?}; skipping", offer.id());
                    state.offers.remove(&offer.id());
                    return;
                }

                // ── Drain each accepted format ────────────────────────────────
                //
                // NOTE: This performs sequential synchronous reads for each
                // MIME type. For MVP this is correct because we call
                // `offer.receive` + `conn.flush()` before blocking on a read,
                // which causes the compositor to write the data asynchronously
                // while we block reading from our end of the socket.
                //
                // A more robust implementation would issue all `receive`
                // requests first (recording all (read_fd, mime) pairs), flush
                // once, then drain all fds concurrently (e.g. with threads or
                // poll/epoll), to avoid head-of-line blocking when a slow
                // source writes one format at a time.
                let mut formats: Vec<CapturedFormat> = Vec::new();
                let mut total_bytes: usize = 0;

                for mime in &accepted {
                    match drain_format(&state.conn, &offer, mime, &state.config, total_bytes) {
                        Ok(Some(bytes)) => {
                            total_bytes = total_bytes.saturating_add(bytes.len());
                            formats.push(CapturedFormat { mime: mime.clone(), bytes });
                        }
                        Ok(None) => {
                            // Skipped due to size cap — also stop accepting more.
                            tracing::debug!(
                                "Format {mime} skipped (size cap); stopping format drain"
                            );
                            break;
                        }
                        Err(e) => {
                            // I/O failure reading this format; skip it and try
                            // the next one rather than aborting the whole item.
                            tracing::warn!("Failed to read format {mime}: {e}; skipping format");
                        }
                    }
                }

                // Clean up the offer entry regardless of whether we captured anything.
                state.offers.remove(&offer.id());

                if formats.is_empty() {
                    tracing::debug!("No formats captured after drain; discarding item");
                    return;
                }

                state.sink.push(CapturedItem { formats });
            }

            zwlr_data_control_device_v1::Event::PrimarySelection { .. } => {
                // Primary selection (middle-click paste buffer) — not in scope
                // for this milestone. Only CLIPBOARD is captured.
                //
                // TODO (future): optionally capture primary selection when
                // configured; the handling would mirror the `Selection` arm above.
            }

            zwlr_data_control_device_v1::Event::Finished => {
                // The compositor has invalidated this data device (e.g. the
                // seat was removed). The caller's `blocking_dispatch` will
                // return an error on the next call after the connection is
                // closed, which propagates up through `run_capture`.
                tracing::info!("Data device finished; capture loop will exit");
            }

            _ => {
                // Forward-compatibility: ignore unknown events.
            }
        }
    }
}

// ─── Dispatch<ZwlrDataControlOfferV1, ()> ────────────────────────────────────

impl Dispatch<ZwlrDataControlOfferV1, ()> for State {
    fn event(
        state: &mut Self,
        offer: &ZwlrDataControlOfferV1,
        event: zwlr_data_control_offer_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_offer_v1::Event::Offer { mime_type } => {
                // Accumulate the MIME type into this offer's list.
                if let Some(mimes) = state.offers.get_mut(&offer.id()) {
                    mimes.push(mime_type);
                }
                // If the offer is not in the map the `DataOffer` event hasn't
                // arrived yet, which should not happen per protocol ordering —
                // but we silently ignore it to be robust.
            }
            _ => {}
        }
    }
}

// ─── Receive helper ───────────────────────────────────────────────────────────

/// Request the compositor to send `mime` data for `offer` over a Unix socket
/// pair, then synchronously drain the read end to EOF.
///
/// Returns:
/// - `Ok(Some(bytes))` — the format was read within size limits.
/// - `Ok(None)` — the format was skipped because a size cap was exceeded.
/// - `Err(e)` — an I/O error occurred; the caller should skip this format.
///
/// # Correctness notes (RUNTIME-UNVERIFIED)
///
/// The `UnixStream::pair()` call creates an anonymous socket pair without the
/// overhead of a named pipe. We pass the write-end's `AsFd` borrow to
/// `offer.receive()`, which encodes the fd into a Wayland message and queues
/// it. `conn.flush()` ensures the message (and the fd) reach the compositor
/// before we close our copy of the write-end and block on `read_to_end`.
///
/// Once the compositor closes its copy of the write-end, `read_to_end` returns
/// EOF. We then enforce size caps and return the bytes.
///
/// If `max_format_bytes` is exceeded we read the entire content anyway (to
/// allow the compositor to finish cleanly) but discard the result. An
/// alternative is to close `read_end` early after reading `max_format_bytes`,
/// which signals the source to stop writing — this is safe but may confuse
/// some sources; we choose the simpler "read-all then discard" approach for MVP.
fn drain_format(
    conn: &Connection,
    offer: &ZwlrDataControlOfferV1,
    mime: &str,
    config: &CaptureConfig,
    current_total: usize,
) -> anyhow::Result<Option<Vec<u8>>> {
    let (mut read_end, write_end) = UnixStream::pair()?;

    // Send the receive request with the write-end fd.
    // RUNTIME-UNVERIFIED: The Wayland request is queued here; the compositor
    // will write the data asynchronously after we flush.
    offer.receive(mime.to_owned(), write_end.as_fd());

    // Flush ensures the fd-carrying message reaches the compositor now,
    // before we close our write-end copy below.
    conn.flush()?;

    // Close our copy of the write-end so we see EOF when the compositor
    // finishes writing and closes its copy.
    drop(write_end);

    // Block until the compositor writes all data and closes its write-end.
    let mut buf = Vec::new();
    read_end.read_to_end(&mut buf)?;

    // Enforce per-format size cap.
    if !filter::within_size_limits(config, current_total, buf.len()) {
        // Format too large; signal to the caller to stop.
        return Ok(None);
    }

    Ok(Some(buf))
}

// ─── Public entry point ───────────────────────────────────────────────────────

/// Connect to the Wayland compositor and run the clipboard capture loop.
///
/// This is a **blocking** function intended to be called from a dedicated
/// thread (e.g. via `tokio::task::spawn_blocking`). It returns only on error
/// or compositor disconnection.
///
/// # Errors
///
/// - `WAYLAND_DISPLAY` not set or the compositor cannot be reached.
/// - The compositor does not advertise `zwlr_data_control_manager_v1`
///   (i.e. not a wlroots compositor or the protocol is disabled).
/// - The event loop encounters a fatal Wayland protocol error.
pub fn run_capture(config: CaptureConfig, sink: CaptureSink) -> anyhow::Result<()> {
    // Connect to the compositor specified by WAYLAND_DISPLAY.
    // RUNTIME-UNVERIFIED: Requires a live Wayland socket.
    let conn = Connection::connect_to_env()?;

    // Enumerate globals and create the event queue.
    // `registry_queue_init` does one roundtrip to collect the global list.
    let (globals, mut event_queue) = registry_queue_init::<State>(&conn)?;
    let qh = event_queue.handle();

    // Bind the wlr-data-control manager (v1 or v2).
    // Panics at compile time (inside bind) if the requested max version
    // exceeds the crate's generated interface version — safe here since we
    // match the XML.
    let manager: ZwlrDataControlManagerV1 = globals
        .bind(&qh, 1..=2, ())
        .map_err(|e| anyhow::anyhow!("zwlr_data_control_manager_v1 not available: {e}"))?;

    // Bind the first available seat.
    //
    // TODO (future): support multi-seat by listening for wl_seat globals via
    // WlRegistry dispatch and creating a data device per seat. For MVP the
    // first seat covers the primary keyboard/pointer device.
    let seat: WlSeat = globals
        .bind(&qh, 1..=8, ())
        .map_err(|e| anyhow::anyhow!("wl_seat not available: {e}"))?;

    // Create the data device. The compositor will immediately send the current
    // clipboard state (a `Selection` event) upon binding.
    let _device = manager.get_data_device(&seat, &qh, ());

    let mut state = State {
        config,
        sink,
        offers: HashMap::new(),
        conn: conn.clone(),
    };

    // Flush the requests above before entering the dispatch loop.
    event_queue.flush()?;

    // Main event loop: block until events arrive, dispatch them, repeat.
    //
    // All capture logic executes synchronously inside the `Dispatch` impls
    // above. `blocking_dispatch` returns `Err` when the compositor closes
    // the connection, which propagates up as our return value.
    loop {
        event_queue.blocking_dispatch(&mut state)?;
    }
}
