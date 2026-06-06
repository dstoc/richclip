use assert_cmd::cargo::CommandCargoExt;
use richclip::Store;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use uuid::Uuid;

const LABEL_MIME: &str = "application/x-richclip-label";

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[derive(Debug, Clone)]
struct MockResponse {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

#[derive(Debug, Clone)]
struct RecordedRequest {
    path: String,
    authorization: Option<String>,
    body: Vec<u8>,
}

struct MockServer {
    addr: String,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    responses: Arc<Mutex<VecDeque<MockResponse>>>,
    shutdown: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl MockServer {
    fn start(responses: Vec<MockResponse>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        listener
            .set_nonblocking(true)
            .expect("set mock listener nonblocking");
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let responses = Arc::new(Mutex::new(VecDeque::from(responses)));
        let shutdown = Arc::new(AtomicBool::new(false));

        let thread_requests = Arc::clone(&requests);
        let thread_responses = Arc::clone(&responses);
        let thread_shutdown = Arc::clone(&shutdown);
        let thread = thread::spawn(move || {
            while !thread_shutdown.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        handle_connection(stream, &thread_requests, &thread_responses);
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(25));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            addr: format!("http://{addr}"),
            requests,
            responses,
            shutdown,
            thread: Some(thread),
        }
    }

    fn base_url(&self) -> &str {
        &self.addr
    }

    fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    fn recorded_requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().unwrap().clone()
    }

    fn pending_responses(&self) -> usize {
        self.responses.lock().unwrap().len()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr.trim_start_matches("http://"));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn handle_connection(
    stream: TcpStream,
    requests: &Arc<Mutex<Vec<RecordedRequest>>>,
    responses: &Arc<Mutex<VecDeque<MockResponse>>>,
) {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).ok().filter(|n| *n > 0).is_none() {
        return;
    }

    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok().filter(|n| *n > 0).is_none() {
            return;
        }
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }

    let content_length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0; content_length];
    if reader.read_exact(&mut body).is_err() {
        return;
    }

    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_string();
    requests.lock().unwrap().push(RecordedRequest {
        path,
        authorization: headers.get("authorization").cloned(),
        body,
    });

    let response = responses.lock().unwrap().pop_front().unwrap_or(MockResponse {
        status: 500,
        content_type: "text/plain",
        body: b"unexpected extra request".to_vec(),
    });

    let reason = match response.status {
        200 => "OK",
        500 => "Internal Server Error",
        _ => "OK",
    };
    let mut stream = reader.into_inner();
    let header = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.status,
        reason,
        response.content_type,
        response.body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(&response.body);
    let _ = stream.flush();
}

fn start_daemon(data_dir: &TempDir, socket: &Path) -> ChildGuard {
    let mut cmd = Command::cargo_bin("richclipd").expect("richclipd binary not found");
    cmd.env("RICHCLIP_DATA_DIR", data_dir.path())
        .env("RICHCLIP_SOCKET", socket)
        .env_remove("WAYLAND_DISPLAY")
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let child = cmd.spawn().expect("spawn richclipd");
    let guard = ChildGuard(child);
    wait_until(Duration::from_secs(10), || {
        socket.exists() && std::os::unix::net::UnixStream::connect(socket).is_ok()
    })
    .expect("richclipd socket did not become ready");
    guard
}

fn start_labeld(
    data_dir: &TempDir,
    socket: &Path,
    config_path: &Path,
    overwrite: bool,
    api_key: &str,
) -> ChildGuard {
    let mut cmd = Command::cargo_bin("richclip-labeld").expect("richclip-labeld binary not found");
    cmd.env("RICHCLIP_DATA_DIR", data_dir.path())
        .env("RICHCLIP_SOCKET", socket)
        .env("LABELD_TEST_API_KEY", api_key)
        .args(["--config", config_path.to_str().unwrap()]);
    if overwrite {
        cmd.arg("--overwrite");
    }
    cmd.stdout(Stdio::null()).stderr(Stdio::piped());

    let child = cmd.spawn().expect("spawn richclip-labeld");
    let guard = ChildGuard(child);
    thread::sleep(Duration::from_millis(250));
    guard
}

fn richclip(
    data_dir: &TempDir,
    socket: &Path,
    args: &[&str],
    stdin: Option<&[u8]>,
) -> std::process::Output {
    let mut cmd = Command::cargo_bin("richclip").expect("richclip binary not found");
    cmd.env("RICHCLIP_DATA_DIR", data_dir.path())
        .env("RICHCLIP_SOCKET", socket)
        .env_remove("WAYLAND_DISPLAY")
        .args(args);

    if stdin.is_some() {
        cmd.stdin(Stdio::piped());
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("spawn richclip");
    if let Some(stdin_data) = stdin {
        child
            .stdin
            .as_mut()
            .expect("stdin missing")
            .write_all(stdin_data)
            .expect("write richclip stdin");
    }
    child.wait_with_output().expect("wait richclip")
}

fn add_image_item(data_dir: &TempDir, socket: &Path, bytes: &[u8]) -> Uuid {
    let output = richclip(
        data_dir,
        socket,
        &["add", "--set-mime", "image/png=-"],
        Some(bytes),
    );
    assert!(
        output.status.success(),
        "add image failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output_id(&output)
}

fn add_image_with_existing_label(
    data_dir: &TempDir,
    socket: &Path,
    dir: &TempDir,
    image_bytes: &[u8],
    label: &str,
) -> Uuid {
    let image_path = dir.path().join(format!("{}.png", Uuid::now_v7()));
    let label_path = dir.path().join(format!("{}.txt", Uuid::now_v7()));
    std::fs::write(&image_path, image_bytes).unwrap();
    std::fs::write(&label_path, label).unwrap();

    let image_arg = format!("image/png=@{}", image_path.display());
    let label_arg = format!("{LABEL_MIME}=@{}", label_path.display());
    let output = richclip(
        data_dir,
        socket,
        &["add", "--set-mime", &image_arg, "--set-mime", &label_arg],
        None,
    );
    assert!(
        output.status.success(),
        "add image+label failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output_id(&output)
}

fn output_id(output: &std::process::Output) -> Uuid {
    String::from_utf8(output.stdout.clone())
        .expect("stdout utf-8")
        .trim()
        .parse()
        .expect("uuid output")
}

fn write_config(dir: &TempDir, base_url: &str) -> std::path::PathBuf {
    let config_path = dir.path().join("labeld.toml");
    let config = format!(
        r#"[model]
url = "{base_url}/v1"
model = "vision-test-model"
timeout_seconds = 5
api_key_env = "LABELD_TEST_API_KEY"
extra_body = {{ max_completion_tokens = 12 }}

[prompt]
label = "Return one short factual label for this clipboard image. Use plain text only."
"#
    );
    std::fs::write(&config_path, config).unwrap();
    config_path
}

fn read_label(data_dir: &TempDir, id: Uuid) -> Option<String> {
    let store = Store::open(data_dir.path()).expect("open store");
    let bytes = store.decode(id, LABEL_MIME).ok()?;
    Some(String::from_utf8(bytes).expect("label utf-8"))
}

fn wait_for_label(data_dir: &TempDir, id: Uuid, expected: &str) {
    wait_until(Duration::from_secs(10), || {
        read_label(data_dir, id)
            .map(|label| label.trim().to_string())
            .as_deref()
            == Some(expected)
    })
    .expect("label not written in time");
}

fn wait_for_no_label(data_dir: &TempDir, id: Uuid) {
    wait_until(Duration::from_secs(3), || read_label(data_dir, id).is_none())
        .expect("label unexpectedly present");
}

fn wait_for_request_count(server: &MockServer, expected: usize) {
    wait_until(Duration::from_secs(5), || server.request_count() == expected)
        .expect("mock server did not receive expected request count");
}

fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> Option<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition() {
            return Some(());
        }
        thread::sleep(Duration::from_millis(50));
    }
    None
}

fn list_item(data_dir: &TempDir, socket: &Path, id: Uuid) -> Value {
    let output = richclip(data_dir, socket, &["list", "--json"], None);
    assert!(
        output.status.success(),
        "list failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let items: Vec<Value> = serde_json::from_slice(&output.stdout).expect("list json");
    items.into_iter()
        .find(|item| item["id"].as_str() == Some(&id.to_string()))
        .expect("item missing from list")
}

#[test]
fn labels_images_and_skips_existing_empty_and_failed_items() {
    let data_dir = TempDir::new().unwrap();
    let socket_dir = TempDir::new().unwrap();
    let fixture_dir = TempDir::new().unwrap();
    let socket = socket_dir.path().join("richclipd-test.sock");
    let _daemon = start_daemon(&data_dir, &socket);

    let server = MockServer::start(vec![
        MockResponse {
            status: 200,
            content_type: "application/json",
            body: br#"{"choices":[{"message":{"content":"mountain lake"}}]}"#.to_vec(),
        },
        MockResponse {
            status: 200,
            content_type: "application/json",
            body: br#"{"choices":[{"message":{"content":"   "}}]}"#.to_vec(),
        },
        MockResponse {
            status: 500,
            content_type: "text/plain",
            body: b"upstream error".to_vec(),
        },
        MockResponse {
            status: 200,
            content_type: "application/json",
            body: br#"{"choices":[{"message":{"content":"forest path"}}]}"#.to_vec(),
        },
    ]);
    let config_path = write_config(&fixture_dir, server.base_url());
    let mut labeld = start_labeld(
        &data_dir,
        &socket,
        &config_path,
        false,
        "integration-secret",
    );

    let labelled_id = add_image_item(&data_dir, &socket, b"first image bytes");
    wait_for_label(&data_dir, labelled_id, "mountain lake");
    let listed = list_item(&data_dir, &socket, labelled_id);
    assert_eq!(listed["label"].as_str(), Some("mountain lake"));

    let skipped_id = add_image_with_existing_label(
        &data_dir,
        &socket,
        &fixture_dir,
        b"prelabelled image bytes",
        "keep me",
    );
    wait_until(Duration::from_secs(2), || {
        read_label(&data_dir, skipped_id)
            .map(|value| value.trim().to_string())
            .as_deref()
            == Some("keep me")
    })
    .expect("existing label should remain unchanged");
    wait_for_request_count(&server, 1);

    let empty_id = add_image_item(&data_dir, &socket, b"empty label bytes");
    wait_for_request_count(&server, 2);
    wait_for_no_label(&data_dir, empty_id);

    let failing_id = add_image_item(&data_dir, &socket, b"failing label bytes");
    wait_for_request_count(&server, 3);
    wait_for_no_label(&data_dir, failing_id);
    assert!(labeld.0.try_wait().unwrap().is_none(), "labeld exited after per-item failure");

    let post_failure_id = add_image_item(&data_dir, &socket, b"post failure bytes");
    wait_for_request_count(&server, 4);
    wait_for_label(&data_dir, post_failure_id, "forest path");
    assert!(labeld.0.try_wait().unwrap().is_none(), "labeld exited before subsequent item");

    let requests = server.recorded_requests();
    assert_eq!(requests.len(), 4, "prelabelled item should have been skipped");
    assert_eq!(requests[0].path, "/v1/chat/completions");
    assert_eq!(
        requests[0].authorization.as_deref(),
        Some("Bearer integration-secret")
    );

    let first_body: Value = serde_json::from_slice(&requests[0].body).expect("request json");
    assert_eq!(first_body["model"].as_str(), Some("vision-test-model"));
    assert_eq!(first_body["max_completion_tokens"].as_i64(), Some(12));
    assert_eq!(
        first_body["messages"][0]["role"].as_str(),
        Some("system")
    );
    assert_eq!(
        first_body["messages"][1]["content"][0]["type"].as_str(),
        Some("text")
    );
    assert_eq!(
        first_body["messages"][1]["content"][1]["type"].as_str(),
        Some("image_url")
    );
    let image_url = first_body["messages"][1]["content"][1]["image_url"]["url"]
        .as_str()
        .expect("image url");
    assert!(
        image_url.starts_with("data:image/png;base64,"),
        "expected data URL image part, got {image_url}"
    );
    assert_eq!(server.pending_responses(), 0, "unused mock responses remain");
}

#[test]
fn overwrite_replaces_existing_labels() {
    let data_dir = TempDir::new().unwrap();
    let socket_dir = TempDir::new().unwrap();
    let fixture_dir = TempDir::new().unwrap();
    let socket = socket_dir.path().join("richclipd-test.sock");
    let _daemon = start_daemon(&data_dir, &socket);

    let server = MockServer::start(vec![MockResponse {
        status: 200,
        content_type: "application/json",
        body: br#"{"choices":[{"message":{"content":"new label"}}]}"#.to_vec(),
    }]);
    let config_path = write_config(&fixture_dir, server.base_url());
    let _labeld = start_labeld(
        &data_dir,
        &socket,
        &config_path,
        true,
        "overwrite-secret",
    );

    let id = add_image_with_existing_label(
        &data_dir,
        &socket,
        &fixture_dir,
        b"overwrite image bytes",
        "old label",
    );
    wait_for_request_count(&server, 1);
    wait_for_label(&data_dir, id, "new label");

    let listed = list_item(&data_dir, &socket, id);
    assert_eq!(listed["label"].as_str(), Some("new label"));
}
