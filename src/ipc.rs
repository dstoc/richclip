//! Newline-delimited JSON IPC types for richclipd↔richclip communication.
//!
//! No socket code lives here — only the serde shapes that both sides agree on.
//! The `client` submodule provides a minimal synchronous IPC client using
//! `std::os::unix::net::UnixStream` (no tokio required in the CLI).

use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Request
// ---------------------------------------------------------------------------

/// Every command sent from the CLI to the daemon is one of these variants.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum Request {
    /// List items, optionally filtered/limited.
    ListItems(ListItemsParams),

    /// Retrieve one item by id.
    GetItem { id: Uuid },

    /// Add a new item from one or more (mime, bytes) pairs.
    AddItem(AddItemParams),

    /// Update (set or remove) formats on an existing item.
    UpdateItem(UpdateItemParams),

    /// Delete an item.
    DeleteItem { id: Uuid },

    /// Ask the daemon to become clipboard owner and serve this item.
    RestoreItem { id: Uuid },

    /// Subscribe to live change events (streaming response).
    WatchEvents(WatchEventsParams),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListItemsParams {
    pub limit: Option<usize>,
    pub mime_filter: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddItemParams {
    /// `(mime, bytes)` pairs. serde_json encodes `Vec<u8>` as a JSON byte array.
    pub formats: Vec<(String, Vec<u8>)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateItemParams {
    pub id: Uuid,
    /// Formats to set: `(mime, bytes)`.
    pub set_formats: Vec<(String, Vec<u8>)>,
    /// MIME types to remove.
    pub remove_mimes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchEventsParams {
    /// If set, only surface events of this type (e.g. "item-added").
    pub event_filter: Option<String>,
    /// If set, only surface events for items that have any of these MIME types.
    pub mime_filters: Vec<String>,
}

// ---------------------------------------------------------------------------
// Response
// ---------------------------------------------------------------------------

/// A simple success/failure envelope returned for non-streaming commands.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Arbitrary JSON payload (list, item, new id, …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl Response {
    pub fn ok(data: impl Into<Option<serde_json::Value>>) -> Self {
        Response {
            ok: true,
            error: None,
            code: None,
            data: data.into(),
        }
    }

    pub fn err(message: impl Into<String>, code: impl Into<String>) -> Self {
        Response {
            ok: false,
            error: Some(message.into()),
            code: Some(code.into()),
            data: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Watch events (streaming)
// ---------------------------------------------------------------------------

/// One event emitted by the daemon on the watch stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum WatchEvent {
    /// A new item was captured or added.
    ItemAdded {
        id: Uuid,
        #[serde(with = "time::serde::rfc3339")]
        created_at: time::OffsetDateTime,
        /// MIME types present on the item at creation time.
        formats: Vec<String>,
    },

    /// An existing item's formats were changed (set or removed).
    ItemUpdated {
        id: Uuid,
        /// MIME types that changed (added/replaced/removed).
        changed: Vec<String>,
    },

    /// An item was deleted.
    ItemDeleted { id: Uuid },
}

// ---------------------------------------------------------------------------
// Synchronous IPC client (std sockets, no tokio)
// ---------------------------------------------------------------------------

/// Minimal synchronous IPC client for the CLI.
///
/// Uses `std::os::unix::net::UnixStream` — no tokio dependency required in the
/// CLI binary. All framing is newline-delimited JSON (one request line, one
/// response line for mutations; for watch the server streams event lines until
/// the client disconnects).
pub mod client {
    use super::{Request, Response, WatchEvent};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::path::Path;
    use std::time::Duration;

    /// Attempt to connect to the daemon socket. Returns `None` if the socket
    /// does not exist or connection fails (daemon is not running).
    pub fn try_connect(path: &Path) -> Option<UnixStream> {
        let stream = UnixStream::connect(path).ok()?;
        // Short timeout so a stale/hung daemon doesn't block the CLI.
        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
        Some(stream)
    }

    /// Send one request and read one response line.
    ///
    /// Writes `request` serialised as a single JSON line terminated with `\n`,
    /// then reads one JSON line from the server and deserialises it as a
    /// [`Response`].
    pub fn send_request(stream: &mut UnixStream, request: &Request) -> std::io::Result<Response> {
        let mut line = serde_json::to_string(request)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        line.push('\n');
        stream.write_all(line.as_bytes())?;
        stream.flush()?;

        let mut reader = BufReader::new(stream.try_clone()?);
        let mut resp_line = String::new();
        reader.read_line(&mut resp_line)?;
        serde_json::from_str(resp_line.trim())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    /// Send a `WatchEvents` request and return an iterator that yields
    /// `WatchEvent` values, one per line, until the connection closes or an
    /// error occurs.
    ///
    /// The returned iterator borrows the stream; the caller should hold the
    /// stream alive as long as events are needed and drop it to stop watching.
    pub fn watch_events(stream: UnixStream, request: &Request) -> std::io::Result<WatchIter> {
        let mut write_half = stream.try_clone()?;
        let mut line = serde_json::to_string(request)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        line.push('\n');
        write_half.write_all(line.as_bytes())?;
        write_half.flush()?;

        // No read timeout for the watch stream — it blocks indefinitely.
        let _ = stream.set_read_timeout(None);
        Ok(WatchIter {
            reader: BufReader::new(stream),
        })
    }

    /// Iterator over `WatchEvent` values received from the daemon.
    pub struct WatchIter {
        reader: BufReader<UnixStream>,
    }

    impl Iterator for WatchIter {
        type Item = WatchEvent;

        fn next(&mut self) -> Option<Self::Item> {
            loop {
                let mut line = String::new();
                match self.reader.read_line(&mut line) {
                    Ok(0) => return None, // EOF — daemon closed the connection
                    Err(_) => return None,
                    Ok(_) => {}
                }
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                match serde_json::from_str::<WatchEvent>(trimmed) {
                    Ok(ev) => return Some(ev),
                    Err(_) => continue, // skip unparseable lines
                }
            }
        }
    }
}
