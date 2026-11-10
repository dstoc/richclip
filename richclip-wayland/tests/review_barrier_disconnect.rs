use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc;
use std::time::Duration;

use richclip::backend::CaptureSink;
use richclip_wayland::{CaptureConfig, capture::run_capture, daemon::CaptureOrdering};

fn word(bytes: &[u8]) -> u32 {
    u32::from_ne_bytes(bytes[..4].try_into().unwrap())
}

fn send_event(stream: &mut UnixStream, object: u32, opcode: u16, body: &[u8]) {
    stream.write_all(&object.to_ne_bytes()).unwrap();
    stream
        .write_all(&(((body.len() as u32 + 8) << 16) | opcode as u32).to_ne_bytes())
        .unwrap();
    stream.write_all(body).unwrap();
}

fn global(stream: &mut UnixStream, registry: u32, name: u32, interface: &str, version: u32) {
    let mut body = name.to_ne_bytes().to_vec();
    let mut string = interface.as_bytes().to_vec();
    string.push(0);
    body.extend((string.len() as u32).to_ne_bytes());
    string.resize(string.len().next_multiple_of(4), 0);
    body.extend(string);
    body.extend(version.to_ne_bytes());
    send_event(stream, registry, 0, &body);
}

#[test]
fn capture_disconnect_releases_restore_already_waiting_for_barrier() {
    // This is a wire-protocol fixture, not a live compositor. Only registry
    // enumeration and wl_display.sync are needed to exercise run_capture.
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("wayland-review");
    let listener = UnixListener::bind(&socket).unwrap();
    // This integration-test binary has exactly one test. No other thread
    // accesses these environment variables until setup is complete.
    unsafe {
        std::env::remove_var("WAYLAND_SOCKET");
        std::env::set_var("WAYLAND_DISPLAY", &socket);
    }
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut syncs = 0;
        loop {
            let mut header = [0u8; 8];
            stream.read_exact(&mut header).unwrap();
            let object = word(&header);
            let size_opcode = word(&header[4..]);
            let opcode = size_opcode & 0xffff;
            let mut body = vec![0; (size_opcode >> 16) as usize - 8];
            stream.read_exact(&mut body).unwrap();
            if object == 1 && opcode == 1 {
                let registry = word(&body);
                global(&mut stream, registry, 1, "zwlr_data_control_manager_v1", 2);
                global(&mut stream, registry, 2, "wl_seat", 8);
            } else if object == 1 && opcode == 0 {
                syncs += 1;
                if syncs == 2 {
                    // The restore has sent its sync request successfully.
                    // Disconnect without delivering that callback.
                    break;
                }
                let callback = word(&body);
                send_event(&mut stream, callback, 0, &0u32.to_ne_bytes());
                send_event(&mut stream, 1, 1, &callback.to_ne_bytes());
            }
        }
    });

    let ordering = CaptureOrdering::default();
    let capture_ordering = ordering.clone();
    let (ready_tx, ready_rx) = mpsc::channel();
    let (capture_done_tx, capture_done_rx) = mpsc::channel();
    let capture = std::thread::spawn(move || {
        let install_ordering = capture_ordering.clone();
        let sink = CaptureSink::new(|_| {}).with_ordering_barrier_handler(move |barrier| {
            install_ordering.install(barrier);
            ready_tx.send(()).unwrap();
        });
        let result = run_capture(CaptureConfig::default(), sink);
        // The daemon does this when its capture task returns.
        capture_ordering.mark_unavailable();
        capture_done_tx.send(result.is_err()).unwrap();
    });
    ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();

    let (restore_done_tx, restore_done_rx) = mpsc::channel();
    let restore = std::thread::spawn(move || {
        let result = ordering.synchronize();
        restore_done_tx.send(result).unwrap();
    });
    assert!(
        capture_done_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
    );
    capture.join().unwrap();
    server.join().unwrap();
    let result = restore_done_rx.recv_timeout(Duration::from_secs(2));
    assert!(
        result.is_ok(),
        "restore barrier stayed blocked after capture exited: {result:?}"
    );
    restore.join().unwrap();
}
