//! Integration tests for the richclipd IPC server.
//!
//! These tests start `richclipd` as a child process with
//! `RICHCLIP_DATA_DIR` and `RICHCLIP_SOCKET` set to temp paths, and with
//! `WAYLAND_DISPLAY` unset so the capture loop errors out (expected) but the
//! daemon continues serving IPC.
//!
//! The tests then drive the daemon via the `richclip` CLI, verifying:
//! - `add` routes through the socket and returns an id.
//! - `list --json` (direct DB read) shows the added item.
//! - `update` routes through the socket.
//! - `watch --json` streams events; an `add` triggers an `item-added` event.
//! - `delete` routes through the socket; subsequent `list --json` is empty.
//! - A second `richclipd` instance exits non-zero (single-instance guard).

use assert_cmd::cargo::CommandCargoExt;
use std::io::{BufRead, BufReader};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Guard that kills the daemon child at test end.
// ---------------------------------------------------------------------------

struct DaemonGuard(Child);

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

// ---------------------------------------------------------------------------
// Helper: start richclipd and wait for its socket to appear.
// ---------------------------------------------------------------------------

fn start_daemon(data_dir: &TempDir, socket: &std::path::Path) -> DaemonGuard {
    let mut cmd = Command::cargo_bin("richclipd").expect("richclipd binary not found");
    cmd.env("RICHCLIP_DATA_DIR", data_dir.path())
        .env("RICHCLIP_SOCKET", socket)
        // Unset WAYLAND_DISPLAY so capture fails gracefully.
        .env_remove("WAYLAND_DISPLAY")
        .env("RUST_LOG", "warn") // keep test output quiet
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let child = cmd.spawn().expect("failed to spawn richclipd");
    let guard = DaemonGuard(child);

    // Poll until the socket file appears (up to 10 seconds).
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if socket.exists() {
            // Also try a quick connect to confirm it's actually listening.
            if std::os::unix::net::UnixStream::connect(socket).is_ok() {
                return guard;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!(
        "richclipd socket {:?} did not appear within 10 seconds",
        socket
    );
}

// ---------------------------------------------------------------------------
// Helper: run `richclip <args>` with the test's data dir and socket.
// ---------------------------------------------------------------------------

fn richclip(data_dir: &TempDir, socket: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::cargo_bin("richclip")
        .expect("richclip binary not found")
        .env("RICHCLIP_DATA_DIR", data_dir.path())
        .env("RICHCLIP_SOCKET", socket)
        .env_remove("WAYLAND_DISPLAY")
        .args(args)
        .output()
        .expect("failed to run richclip")
}

// Helper that also provides stdin.
fn richclip_stdin(
    data_dir: &TempDir,
    socket: &std::path::Path,
    args: &[&str],
    stdin_data: &[u8],
) -> std::process::Output {
    use std::io::Write;
    let mut child = Command::cargo_bin("richclip")
        .expect("richclip binary not found")
        .env("RICHCLIP_DATA_DIR", data_dir.path())
        .env("RICHCLIP_SOCKET", socket)
        .env_remove("WAYLAND_DISPLAY")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn richclip");

    child.stdin.as_mut().unwrap().write_all(stdin_data).unwrap();
    drop(child.stdin.take());

    child.wait_with_output().expect("richclip did not finish")
}

fn spawn_watch(data_dir: &TempDir, socket: &std::path::Path, args: &[&str]) -> Child {
    Command::cargo_bin("richclip")
        .expect("richclip binary not found")
        .env("RICHCLIP_DATA_DIR", data_dir.path())
        .env("RICHCLIP_SOCKET", socket)
        .env_remove("WAYLAND_DISPLAY")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn richclip watch")
}

fn read_watch_event(stdout: ChildStdout, timeout: Duration) -> Option<serde_json::Value> {
    let (tx, rx) = mpsc::channel();

    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();

        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    let _ = tx.send(None);
                    break;
                }
                Ok(_) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let event =
                        serde_json::from_str(trimmed).expect("watch output must be valid JSON");
                    let _ = tx.send(Some(event));
                    break;
                }
                Err(_) => {
                    let _ = tx.send(None);
                    break;
                }
            }
        }
    });

    rx.recv_timeout(timeout).ok().flatten()
}

fn add_item(data_dir: &TempDir, socket: &std::path::Path, mime: &str, body: &[u8]) -> String {
    let set_arg = format!("{mime}=-");
    let add_out = richclip_stdin(data_dir, socket, &["add", "--set-mime", &set_arg], body);
    assert!(
        add_out.status.success(),
        "add failed: {:?}",
        String::from_utf8_lossy(&add_out.stderr)
    );
    String::from_utf8(add_out.stdout)
        .expect("add output must be valid UTF-8")
        .trim()
        .to_string()
}

fn update_item(data_dir: &TempDir, socket: &std::path::Path, id: &str, mime: &str, body: &[u8]) {
    let set_arg = format!("{mime}=-");
    let update_out = richclip_stdin(
        data_dir,
        socket,
        &["update", id, "--set-mime", &set_arg],
        body,
    );
    assert!(
        update_out.status.success(),
        "update failed: {:?}",
        String::from_utf8_lossy(&update_out.stderr)
    );
}

// ---------------------------------------------------------------------------
// Main integration test
// ---------------------------------------------------------------------------

#[test]
fn test_ipc_full_flow() {
    let data_dir = TempDir::new().unwrap();
    let socket_dir = TempDir::new().unwrap();
    let socket = socket_dir.path().join("richclipd-test.sock");

    // ── Start daemon ─────────────────────────────────────────────────────────
    let _daemon = start_daemon(&data_dir, &socket);

    // ── add routes through socket, returns a UUID ─────────────────────────────
    let add_out = richclip_stdin(
        &data_dir,
        &socket,
        &["add", "--set-mime", "text/plain=-"],
        b"hello from ipc test",
    );
    assert!(
        add_out.status.success(),
        "add failed: {:?}",
        String::from_utf8_lossy(&add_out.stderr)
    );
    let id = String::from_utf8(add_out.stdout)
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(id.len(), 36, "expected UUID-length id, got {:?}", id);

    // ── list --json (direct DB read) shows the item ──────────────────────────
    let list_out = richclip(&data_dir, &socket, &["list", "--json"]);
    assert!(list_out.status.success(), "list failed");
    let items: Vec<serde_json::Value> =
        serde_json::from_slice(&list_out.stdout).expect("list --json must be valid JSON");
    assert_eq!(items.len(), 1, "expected 1 item after add");
    assert_eq!(items[0]["id"].as_str().unwrap(), id);

    // ── update routes through socket ─────────────────────────────────────────
    let update_out = richclip_stdin(
        &data_dir,
        &socket,
        &[
            "update",
            &id,
            "--set-mime",
            "application/x-richclip-label=-",
        ],
        b"test label",
    );
    assert!(
        update_out.status.success(),
        "update failed: {:?}",
        String::from_utf8_lossy(&update_out.stderr)
    );

    // ── watch + add: verify item-added event arrives ──────────────────────────
    {
        // Spawn `richclip watch --json` as a background process.
        let mut watch_child = Command::cargo_bin("richclip")
            .expect("richclip binary not found")
            .env("RICHCLIP_DATA_DIR", data_dir.path())
            .env("RICHCLIP_SOCKET", &socket)
            .env_remove("WAYLAND_DISPLAY")
            .args(["watch", "--json", "--event", "item-added"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn richclip watch");

        // Give the watch stream a moment to subscribe.
        std::thread::sleep(Duration::from_millis(200));

        // Add a second item — this should trigger an item-added event.
        let add2_out = richclip_stdin(
            &data_dir,
            &socket,
            &["add", "--set-mime", "text/plain=-"],
            b"second item",
        );
        assert!(add2_out.status.success(), "second add failed");
        let id2 = String::from_utf8(add2_out.stdout)
            .unwrap()
            .trim()
            .to_string();

        // Read one event line from the watch process stdout (with timeout).
        let stdout = watch_child.stdout.take().unwrap();
        let mut reader = BufReader::new(stdout);
        let mut event_line = String::new();

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut got_event = false;
        while Instant::now() < deadline {
            event_line.clear();
            match reader.read_line(&mut event_line) {
                Ok(0) => break,
                Ok(_) => {
                    let trimmed = event_line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let v: serde_json::Value =
                        serde_json::from_str(trimmed).expect("watch output must be valid JSON");
                    assert_eq!(
                        v["event"].as_str().unwrap(),
                        "item-added",
                        "expected item-added event"
                    );
                    assert_eq!(
                        v["id"].as_str().unwrap(),
                        id2,
                        "event id must match the added item"
                    );
                    got_event = true;
                    break;
                }
                Err(_) => break,
            }
        }

        // Kill the watch process.
        let _ = watch_child.kill();
        let _ = watch_child.wait();

        assert!(got_event, "did not receive item-added event within timeout");
    }

    // ── delete routes through socket; list becomes empty ─────────────────────
    // First delete the id2 (second item).
    {
        let list2_out = richclip(&data_dir, &socket, &["list", "--json"]);
        let all: Vec<serde_json::Value> = serde_json::from_slice(&list2_out.stdout).unwrap();
        for item in all {
            let del_id = item["id"].as_str().unwrap();
            let del_out = richclip(&data_dir, &socket, &["delete", del_id]);
            assert!(
                del_out.status.success(),
                "delete {} failed: {:?}",
                del_id,
                String::from_utf8_lossy(&del_out.stderr)
            );
        }
    }

    let final_list = richclip(&data_dir, &socket, &["list", "--json"]);
    assert!(final_list.status.success());
    let remaining: Vec<serde_json::Value> = serde_json::from_slice(&final_list.stdout).unwrap();
    assert!(
        remaining.is_empty(),
        "list must be empty after deleting all items"
    );

    // ── single-instance: second richclipd on same socket exits non-zero ───────
    {
        let mut second = Command::cargo_bin("richclipd")
            .expect("richclipd binary not found")
            .env("RICHCLIP_DATA_DIR", data_dir.path())
            .env("RICHCLIP_SOCKET", &socket)
            .env_remove("WAYLAND_DISPLAY")
            .env("RUST_LOG", "error")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn second richclipd");

        // Give it a moment to check the socket and exit.
        std::thread::sleep(Duration::from_millis(500));

        let status = second.wait().expect("second richclipd did not exit");
        assert!(
            !status.success(),
            "second richclipd must exit non-zero (single-instance guard)"
        );
    }
}

#[test]
fn test_watch_repeated_mime_filters_match_png_and_jpeg_but_not_text_only_items() {
    let data_dir = TempDir::new().unwrap();
    let socket_dir = TempDir::new().unwrap();
    let socket = socket_dir.path().join("richclipd-test.sock");

    let _daemon = start_daemon(&data_dir, &socket);
    let watch_args = [
        "watch",
        "--json",
        "--event",
        "item-added",
        "--mime",
        "image/png",
        "--mime",
        "image/jpeg",
    ];

    {
        let mut watch_child = spawn_watch(&data_dir, &socket, &watch_args);
        std::thread::sleep(Duration::from_millis(200));

        let id = add_item(&data_dir, &socket, "image/png", b"png payload");
        let event = read_watch_event(watch_child.stdout.take().unwrap(), Duration::from_secs(5))
            .expect("expected item-added event for image/png");

        assert_eq!(event["event"].as_str().unwrap(), "item-added");
        assert_eq!(event["id"].as_str().unwrap(), id);
        assert_eq!(event["formats"], serde_json::json!(["image/png"]));

        let _ = watch_child.kill();
        let _ = watch_child.wait();
    }

    {
        let mut watch_child = spawn_watch(&data_dir, &socket, &watch_args);
        std::thread::sleep(Duration::from_millis(200));

        let id = add_item(&data_dir, &socket, "image/jpeg", b"jpeg payload");
        let event = read_watch_event(watch_child.stdout.take().unwrap(), Duration::from_secs(5))
            .expect("expected item-added event for image/jpeg");

        assert_eq!(event["event"].as_str().unwrap(), "item-added");
        assert_eq!(event["id"].as_str().unwrap(), id);
        assert_eq!(event["formats"], serde_json::json!(["image/jpeg"]));

        let _ = watch_child.kill();
        let _ = watch_child.wait();
    }

    {
        let mut watch_child = spawn_watch(&data_dir, &socket, &watch_args);
        std::thread::sleep(Duration::from_millis(200));

        let _id = add_item(&data_dir, &socket, "text/plain", b"text payload");
        let event = read_watch_event(
            watch_child.stdout.take().unwrap(),
            Duration::from_millis(750),
        );
        assert!(
            event.is_none(),
            "text-only item should not match repeated MIME filters"
        );

        let _ = watch_child.kill();
        let _ = watch_child.wait();
    }
}

#[test]
fn test_watch_image_flag_matches_supported_image_formats_and_not_text_only_items() {
    let data_dir = TempDir::new().unwrap();
    let socket_dir = TempDir::new().unwrap();
    let socket = socket_dir.path().join("richclipd-test.sock");

    let _daemon = start_daemon(&data_dir, &socket);

    for (mime, payload) in [
        ("image/webp", b"webp payload" as &[u8]),
        ("image/bmp", b"bmp payload" as &[u8]),
    ] {
        let mut watch_child = spawn_watch(
            &data_dir,
            &socket,
            &["watch", "--json", "--event", "item-added", "--image"],
        );
        std::thread::sleep(Duration::from_millis(200));

        let id = add_item(&data_dir, &socket, mime, payload);
        let event = read_watch_event(watch_child.stdout.take().unwrap(), Duration::from_secs(5))
            .expect("expected item-added event for image item");

        assert_eq!(event["event"].as_str().unwrap(), "item-added");
        assert_eq!(event["id"].as_str().unwrap(), id);
        assert_eq!(event["formats"], serde_json::json!([mime]));

        let _ = watch_child.kill();
        let _ = watch_child.wait();
    }

    {
        let mut watch_child = spawn_watch(
            &data_dir,
            &socket,
            &["watch", "--json", "--event", "item-added", "--image"],
        );
        std::thread::sleep(Duration::from_millis(200));

        let _id = add_item(&data_dir, &socket, "text/plain", b"text payload");
        let event = read_watch_event(
            watch_child.stdout.take().unwrap(),
            Duration::from_millis(750),
        );
        assert!(event.is_none(), "text-only item should not match --image");

        let _ = watch_child.kill();
        let _ = watch_child.wait();
    }
}

#[test]
fn test_watch_single_mime_png_still_behaves_the_same() {
    let data_dir = TempDir::new().unwrap();
    let socket_dir = TempDir::new().unwrap();
    let socket = socket_dir.path().join("richclipd-test.sock");

    let _daemon = start_daemon(&data_dir, &socket);

    {
        let mut watch_child = spawn_watch(
            &data_dir,
            &socket,
            &[
                "watch",
                "--json",
                "--event",
                "item-added",
                "--mime",
                "image/png",
            ],
        );
        std::thread::sleep(Duration::from_millis(200));

        let id = add_item(&data_dir, &socket, "image/png", b"png payload");
        let event = read_watch_event(watch_child.stdout.take().unwrap(), Duration::from_secs(5))
            .expect("expected item-added event for image/png");

        assert_eq!(event["event"].as_str().unwrap(), "item-added");
        assert_eq!(event["id"].as_str().unwrap(), id);
        assert_eq!(event["formats"], serde_json::json!(["image/png"]));

        let _ = watch_child.kill();
        let _ = watch_child.wait();
    }

    {
        let mut watch_child = spawn_watch(
            &data_dir,
            &socket,
            &[
                "watch",
                "--json",
                "--event",
                "item-added",
                "--mime",
                "image/png",
            ],
        );
        std::thread::sleep(Duration::from_millis(200));

        let _id = add_item(&data_dir, &socket, "image/jpeg", b"jpeg payload");
        let event = read_watch_event(
            watch_child.stdout.take().unwrap(),
            Duration::from_millis(750),
        );
        assert!(
            event.is_none(),
            "single --mime image/png should not match image/jpeg"
        );

        let _ = watch_child.kill();
        let _ = watch_child.wait();
    }
}

#[test]
fn test_watch_item_updated_filters_when_requested_mime_changes() {
    let data_dir = TempDir::new().unwrap();
    let socket_dir = TempDir::new().unwrap();
    let socket = socket_dir.path().join("richclipd-test.sock");

    let _daemon = start_daemon(&data_dir, &socket);
    let id = add_item(&data_dir, &socket, "text/plain", b"text payload");

    let mut watch_child = spawn_watch(
        &data_dir,
        &socket,
        &[
            "watch",
            "--json",
            "--event",
            "item-updated",
            "--mime",
            "image/jpeg",
        ],
    );
    std::thread::sleep(Duration::from_millis(200));

    update_item(&data_dir, &socket, &id, "image/jpeg", b"jpeg payload");
    let event = read_watch_event(watch_child.stdout.take().unwrap(), Duration::from_secs(5))
        .expect("expected item-updated event for image/jpeg");

    assert_eq!(event["event"].as_str().unwrap(), "item-updated");
    assert_eq!(event["id"].as_str().unwrap(), id);
    assert_eq!(event["changed"], serde_json::json!(["image/jpeg"]));

    let _ = watch_child.kill();
    let _ = watch_child.wait();
}
