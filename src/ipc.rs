//! Newline-delimited JSON IPC types for richclipd↔richclip communication.
//!
//! No socket code lives here — only the serde shapes that both sides agree on.
//! Phase 2 will add the actual Unix socket transport.

#![allow(dead_code)]

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

    /// Add a new item from one or more (mime, base64-encoded bytes) pairs.
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
    /// `(mime, base64-encoded-bytes)` pairs.
    pub formats: Vec<(String, String)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateItemParams {
    pub id: Uuid,
    /// Formats to set: `(mime, base64-encoded-bytes)`.
    pub set_formats: Vec<(String, String)>,
    /// MIME types to remove.
    pub remove_mimes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchEventsParams {
    /// If set, only surface events of this type.
    pub event_filter: Option<String>,
    /// If set, only surface events for items that have this MIME type.
    pub mime_filter: Option<String>,
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
