use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use richclip::backend::CaptureSink;
use richclip_wayland::{CaptureConfig, capture::run_capture, daemon::CaptureOrdering};

#[test]
fn activation_barrier_must_keep_serving_restore_data_requests() {
    // Minimal Wayland wire fixture, not a live compositor. It forwards the
    // capture receive FD to the real restore source after activation begins.
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("wayland-review");
    let mut server = Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/review_activation_server.py"
        ))
        .arg(&socket)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut output = BufReader::new(server.stdout.take().unwrap());
    let mut ready = String::new();
    output.read_line(&mut ready).unwrap();
    assert_eq!(ready.trim(), "ready");
    // This integration-test binary contains one test.
    unsafe {
        std::env::remove_var("WAYLAND_SOCKET");
        std::env::set_var("WAYLAND_DISPLAY", &socket);
    }

    let ordering = CaptureOrdering::default();
    let install_ordering = ordering.clone();
    let (ready_tx, ready_rx) = mpsc::channel();
    let (item_tx, item_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let sink = CaptureSink::new(move |item| {
            let _ = item_tx.send(item);
        })
        .with_ordering_barrier_handler(move |barrier| {
            install_ordering.install(barrier);
            ready_tx.send(()).unwrap();
        });
        let _ = run_capture(CaptureConfig::default(), sink);
    });
    ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();

    let before_ordering = ordering.clone();
    std::thread::spawn(move || {
        let _ = richclip_wayland::restore::run_restore_with_callbacks(
            vec![("text/plain".into(), b"restored A".to_vec())],
            move || before_ordering.synchronize().unwrap(),
            move || {
                let _waiter = ordering
                    .synchronize_in_background(|result| result.unwrap())
                    .unwrap();
            },
        );
    });

    let mut forwarded = String::new();
    output.read_line(&mut forwarded).unwrap();
    assert_eq!(forwarded.trim(), "send forwarded during activation barrier");
    let result = item_rx.recv_timeout(Duration::from_secs(2));
    // Stop the fixture before reporting failure. Worker threads are detached;
    // a deadlock cannot prevent this regression-test process from exiting.
    server.kill().unwrap();
    server.wait().unwrap();
    assert!(
        result.is_ok(),
        "capture could not finish the restore echo while activation waited: {:?}",
        result.err()
    );
}
