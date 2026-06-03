//! Restore provider: become the Wayland clipboard owner and serve blobs.
//!
//! Each restore operation creates its own Wayland [`Connection`] and event loop
//! on a **dedicated `std::thread`**. This keeps restore completely independent
//! of the long-lived capture connection — no shared connection, no locking.
//!
//! # Protocol flow (RUNTIME-UNVERIFIED)
//!
//! The following aspects require a live wlroots compositor to validate:
//!
//! - `Connection::connect_to_env()` reaching `$WAYLAND_DISPLAY`.
//! - `registry_queue_init` enumerating compositor globals.
//! - `globals.bind::<ZwlrDataControlManagerV1>` (requires the protocol to be
//!   advertised; compositor must support `zwlr_data_control_manager_v1`).
//! - `globals.bind::<WlSeat>` returning the primary seat.
//! - `manager.create_data_source` / `source.offer(mime)` registering MIMEs.
//! - `device.set_selection(Some(&source))` making us the clipboard owner.
//! - The compositor routing a `Send { mime_type, fd }` event to our source.
//! - `write_all` into the fd correctly delivering bytes to the requesting client.
//! - The `Cancelled` event being fired when a newer selection supersedes ours,
//!   and the dispatch loop exiting cleanly afterwards.
//!
//! # Self-capture suppression
//!
//! After `set_selection` the compositor will deliver a `Selection` event to the
//! capture device (because the selection changed). Suppression of that echo is
//! handled **in the daemon's capture consumer**, not here — see `daemon.rs` and
//! `richclipd.rs`.

use std::collections::HashMap;
use std::io::Write;
use std::os::fd::OwnedFd;

use wayland_client::{
    Connection, Dispatch, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_registry, wl_seat::WlSeat},
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};

// ─── State ────────────────────────────────────────────────────────────────────

/// Mutable state for the restore event loop.
///
/// Stored on the restore thread; `Dispatch` handlers mutate it directly.
struct State {
    /// MIME → raw blob bytes; populated before the loop starts and read in
    /// `Send` event handlers.
    blobs: HashMap<String, Vec<u8>>,

    /// The data source we own; kept alive so the compositor does not immediately
    /// cancel it, and destroyed when we receive `Cancelled`.
    source: ZwlrDataControlSourceV1,

    /// Set to `true` by the `Cancelled` handler; the dispatch loop checks this
    /// flag after every dispatch call and exits when it is `true`.
    done: bool,
}

// ─── Dispatch<WlRegistry, GlobalListContents> ─────────────────────────────────

impl Dispatch<wl_registry::WlRegistry, wayland_client::globals::GlobalListContents> for State {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &wayland_client::globals::GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // Dynamic global add/remove after initial enumeration — not handled.
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
        // No events defined for this interface.
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
        // We only use the seat object to create the data device; ignore events.
    }
}

// ─── Dispatch<ZwlrDataControlDeviceV1, ()> ───────────────────────────────────

impl Dispatch<ZwlrDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _device: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_device_v1::Event::Finished => {
                // The compositor has invalidated this data device (seat removed
                // or similar).  Treat it as cancellation so the thread exits.
                tracing::info!("restore: data device finished; exiting restore loop");
                state.done = true;
            }
            _ => {
                // Selection / PrimarySelection / DataOffer events are not
                // relevant to a source-only restore provider.
            }
        }
    }
}

// ─── Dispatch<ZwlrDataControlSourceV1, ()> ───────────────────────────────────

impl Dispatch<ZwlrDataControlSourceV1, ()> for State {
    fn event(
        state: &mut Self,
        _source: &ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_source_v1::Event::Send { mime_type, fd } => {
                // A client is requesting our blob for `mime_type`.
                //
                // RUNTIME-UNVERIFIED: The compositor fires this event when any
                // client calls `receive(mime_type, fd)` on the corresponding
                // offer.  We must write all bytes then close the fd, signalling
                // EOF to the receiver.
                //
                // Writing may block if the receiving client is slow to read
                // (the pipe buffer fills up and write() blocks).  To avoid
                // stalling the dispatch loop — which would prevent us from
                // handling *other* events (including `Cancelled`) — we spawn a
                // short-lived thread to do the write.  The fd is transferred
                // into the thread via `OwnedFd`; no additional locking needed
                // because `blobs` is immutable after the loop starts and bytes
                // are cloned per send.
                let bytes = state.blobs.get(&mime_type).cloned();
                std::thread::spawn(move || {
                    write_blob_to_fd(fd, bytes.as_deref(), &mime_type);
                });
            }

            zwlr_data_control_source_v1::Event::Cancelled => {
                // The compositor has replaced our selection with a newer one
                // (another app copied something, or a second `richclip restore`
                // superseded us).  Destroy the source and signal the loop to exit.
                //
                // RUNTIME-UNVERIFIED: This event is expected to fire reliably
                // whenever selection ownership is transferred away from us.
                tracing::debug!("restore: source cancelled; releasing clipboard ownership");
                state.source.destroy();
                state.done = true;
            }

            _ => {
                // Forward-compatibility: ignore unknown events.
            }
        }
    }
}

// ─── Write helper (runs in a per-send thread) ─────────────────────────────────

/// Write `bytes` into `fd` and close it.
///
/// Runs on a dedicated thread so the dispatch loop is never stalled by a slow
/// paste target.
///
/// RUNTIME-UNVERIFIED: Requires a live compositor; `fd` is an anonymous pipe
/// write-end provided by `zwlr_data_control_source_v1::Event::Send`.
fn write_blob_to_fd(fd: OwnedFd, bytes: Option<&[u8]>, mime_type: &str) {
    // Convert OwnedFd → File so we get Write + automatic drop/close.
    let mut file = std::fs::File::from(fd);
    match bytes {
        Some(b) => {
            if let Err(e) = file.write_all(b) {
                tracing::warn!("restore: failed to write {mime_type} blob to fd: {e}");
            }
        }
        None => {
            // Advertised MIME was requested but we have no bytes for it.
            // Close the fd immediately (EOF) — the receiver will see an empty
            // payload.  This should not happen in practice because we only
            // advertise MIMEs present in `blobs`.
            tracing::warn!(
                "restore: Send event for unknown mime {mime_type}; sending empty payload"
            );
        }
    }
    // `file` is dropped here, closing the fd and delivering EOF to the reader.
}

// ─── Public entry point ───────────────────────────────────────────────────────

/// Become the Wayland clipboard owner for the given `formats`, serving blobs
/// lazily until our selection is superseded (i.e. until `Cancelled` fires).
///
/// **Blocking** — intended to be called from a `std::thread::spawn` closure.
/// Returns `Ok(())` when the source is cancelled (normal exit) or `Err` on a
/// Wayland / I/O error.
///
/// `formats` must not contain `application/x-richclip-*` MIMEs; that filtering
/// is the caller's responsibility (see `daemon.rs` `handle_restore_item`).
///
/// # RUNTIME-UNVERIFIED
///
/// This entire function is unverified without a live compositor.  See module
/// doc comment for the full list of unverified aspects.
pub fn run_restore(formats: Vec<(String, Vec<u8>)>) -> anyhow::Result<()> {
    // ── 1. Connect ────────────────────────────────────────────────────────────
    // RUNTIME-UNVERIFIED: Requires $WAYLAND_DISPLAY to be set and reachable.
    let conn = Connection::connect_to_env()?;

    // One round-trip to enumerate globals and create the event queue.
    let (globals, mut event_queue) = registry_queue_init::<State>(&conn)?;
    let qh = event_queue.handle();

    // ── 2. Bind globals ───────────────────────────────────────────────────────
    // RUNTIME-UNVERIFIED: compositor must advertise zwlr_data_control_manager_v1.
    let manager: ZwlrDataControlManagerV1 = globals
        .bind(&qh, 1..=2, ())
        .map_err(|e| anyhow::anyhow!("zwlr_data_control_manager_v1 not available: {e}"))?;

    // RUNTIME-UNVERIFIED: compositor must advertise wl_seat.
    let seat: WlSeat = globals
        .bind(&qh, 1..=8, ())
        .map_err(|e| anyhow::anyhow!("wl_seat not available: {e}"))?;

    // ── 3. Create data device + source ────────────────────────────────────────
    // The device is only needed to call `set_selection`; we don't need to
    // receive selection-changed events here.
    // RUNTIME-UNVERIFIED: get_data_device + create_data_source requests sent.
    let device = manager.get_data_device(&seat, &qh, ());
    let source = manager.create_data_source(&qh, ());

    // ── 4. Advertise MIME types ───────────────────────────────────────────────
    // RUNTIME-UNVERIFIED: Each `offer()` call registers a MIME type with the
    // compositor.  Requesting clients will see these via `ZwlrDataControlOffer`.
    for (mime, _) in &formats {
        source.offer(mime.clone());
    }

    // ── 5. Become the clipboard owner ─────────────────────────────────────────
    // RUNTIME-UNVERIFIED: set_selection transfers ownership to our source.
    // The compositor will deliver a `Cancelled` event on the *previous* owner's
    // source (if any) and a `Selection` event to all data-control listeners.
    device.set_selection(Some(&source));

    // Flush all queued requests to the compositor before entering the loop.
    event_queue.flush()?;

    // ── 6. Build initial state ────────────────────────────────────────────────
    let blobs: HashMap<String, Vec<u8>> = formats.into_iter().collect();
    let mut state = State {
        blobs,
        source,
        done: false,
    };

    // ── 7. Dispatch loop ──────────────────────────────────────────────────────
    // Block until events arrive, dispatch them, then check `done`.
    // We exit when `done` is set by `Cancelled` (or `Finished`).
    while !state.done {
        event_queue.blocking_dispatch(&mut state)?;
    }

    tracing::debug!("restore: event loop exited cleanly");
    Ok(())
}
