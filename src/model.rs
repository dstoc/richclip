use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// A clipboard history item. Pure identity and lifecycle — all content lives
/// in [`Format`] rows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    pub id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

/// One MIME representation within a clipboard item. Points to a
/// content-addressed blob via [`blob_hash`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Format {
    pub item_id: Uuid,
    pub mime: String,
    pub blob_hash: String,
    pub size: u64,
    #[serde(with = "time::serde::rfc3339")]
    pub captured_at: OffsetDateTime,
}

/// An [`Item`] together with all of its [`Format`] rows, as returned by
/// queries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemWithFormats {
    pub item: Item,
    pub formats: Vec<Format>,
}
