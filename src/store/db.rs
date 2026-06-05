//! SQLite-backed item/format index and timestamp helpers.

use crate::error::{Error, Result};
use crate::model::{Format, Item, ItemWithFormats};
use rusqlite::{Connection, OptionalExtension, params};
use time::OffsetDateTime;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Timestamp helpers
// ---------------------------------------------------------------------------

/// Convert an `OffsetDateTime` to epoch **milliseconds** for storage.
pub fn to_epoch_ms(dt: OffsetDateTime) -> i64 {
    let nanos = dt.unix_timestamp_nanos();
    // Integer division: nanos / 1_000_000 gives milliseconds.
    (nanos / 1_000_000) as i64
}

/// Reconstruct an `OffsetDateTime` (UTC) from epoch **milliseconds**.
pub fn from_epoch_ms(ms: i64) -> OffsetDateTime {
    // from_unix_timestamp_nanos accepts i128 nanoseconds.
    let nanos = (ms as i128) * 1_000_000;
    OffsetDateTime::from_unix_timestamp_nanos(nanos).unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

// ---------------------------------------------------------------------------
// Schema creation / migration
// ---------------------------------------------------------------------------

/// Create the schema if it does not already exist.
///
/// The schema is intentionally minimal: only `items` and `formats`.
pub fn create_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS items (
            id         TEXT    PRIMARY KEY,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS formats (
            item_id    TEXT    NOT NULL,
            mime       TEXT    NOT NULL,
            blob_hash  TEXT    NOT NULL,
            size       INTEGER NOT NULL,
            captured_at INTEGER NOT NULL,
            PRIMARY KEY (item_id, mime),
            FOREIGN KEY (item_id) REFERENCES items(id)
        );
        ",
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Row mapping helpers
// ---------------------------------------------------------------------------

fn row_to_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<Item> {
    let id_str: String = row.get(0)?;
    let created_ms: i64 = row.get(1)?;
    let updated_ms: i64 = row.get(2)?;
    Ok(Item {
        id: Uuid::parse_str(&id_str).unwrap_or(Uuid::nil()),
        created_at: from_epoch_ms(created_ms),
        updated_at: from_epoch_ms(updated_ms),
    })
}

fn row_to_format(row: &rusqlite::Row<'_>) -> rusqlite::Result<Format> {
    let item_id_str: String = row.get(0)?;
    let mime: String = row.get(1)?;
    let blob_hash: String = row.get(2)?;
    let size: i64 = row.get(3)?;
    let captured_ms: i64 = row.get(4)?;
    Ok(Format {
        item_id: Uuid::parse_str(&item_id_str).unwrap_or(Uuid::nil()),
        mime,
        blob_hash,
        size: size as u64,
        captured_at: from_epoch_ms(captured_ms),
    })
}

// ---------------------------------------------------------------------------
// Queries
// ---------------------------------------------------------------------------

/// Insert a new `Item` row.
pub fn insert_item(conn: &Connection, item: &Item) -> Result<()> {
    conn.execute(
        "INSERT INTO items (id, created_at, updated_at) VALUES (?1, ?2, ?3)",
        params![
            item.id.hyphenated().to_string(),
            to_epoch_ms(item.created_at),
            to_epoch_ms(item.updated_at),
        ],
    )?;
    Ok(())
}

/// Upsert a `Format` row (insert or replace on conflict).
pub fn upsert_format(conn: &Connection, fmt: &Format) -> Result<()> {
    conn.execute(
        "INSERT INTO formats (item_id, mime, blob_hash, size, captured_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(item_id, mime) DO UPDATE SET
           blob_hash  = excluded.blob_hash,
           size       = excluded.size,
           captured_at = excluded.captured_at",
        params![
            fmt.item_id.hyphenated().to_string(),
            fmt.mime,
            fmt.blob_hash,
            fmt.size as i64,
            to_epoch_ms(fmt.captured_at),
        ],
    )?;
    Ok(())
}

/// Bump `updated_at` for an item.
pub fn bump_updated_at(conn: &Connection, id: Uuid, now: OffsetDateTime) -> Result<()> {
    let changed = conn.execute(
        "UPDATE items SET updated_at = ?1 WHERE id = ?2",
        params![to_epoch_ms(now), id.hyphenated().to_string()],
    )?;
    if changed == 0 {
        return Err(Error::NotFound);
    }
    Ok(())
}

/// Check whether an item exists; return `Err(NotFound)` if not.
pub fn require_item(conn: &Connection, id: Uuid) -> Result<()> {
    let exists: bool = conn
        .query_row(
            "SELECT 1 FROM items WHERE id = ?1",
            params![id.hyphenated().to_string()],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);
    if exists { Ok(()) } else { Err(Error::NotFound) }
}

/// Fetch a single item by id.
pub fn get_item(conn: &Connection, id: Uuid) -> Result<Item> {
    conn.query_row(
        "SELECT id, created_at, updated_at FROM items WHERE id = ?1",
        params![id.hyphenated().to_string()],
        row_to_item,
    )
    .optional()?
    .ok_or(Error::NotFound)
}

/// Fetch all formats for an item; returns `Err(NotFound)` if the item itself
/// is missing.
pub fn get_formats(conn: &Connection, id: Uuid) -> Result<Vec<Format>> {
    require_item(conn, id)?;
    let mut stmt = conn.prepare(
        "SELECT item_id, mime, blob_hash, size, captured_at
         FROM formats WHERE item_id = ?1",
    )?;
    let formats = stmt
        .query_map(params![id.hyphenated().to_string()], row_to_format)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(formats)
}

/// Fetch a single format row for `(id, mime)`.
pub fn get_format(conn: &Connection, id: Uuid, mime: &str) -> Result<Format> {
    conn.query_row(
        "SELECT item_id, mime, blob_hash, size, captured_at
         FROM formats WHERE item_id = ?1 AND mime = ?2",
        params![id.hyphenated().to_string(), mime],
        row_to_format,
    )
    .optional()?
    .ok_or(Error::NotFound)
}

/// List items newest-first (`ORDER BY id DESC`), with optional limit and MIME
/// filter. MIME filter keeps items that have at least one format with that MIME.
pub fn list_items(
    conn: &Connection,
    limit: Option<usize>,
    mime_filter: Option<&str>,
) -> Result<Vec<ItemWithFormats>> {
    // Gather item ids first, then batch-fetch formats.
    let ids: Vec<String> = if let Some(mime) = mime_filter {
        let mut stmt = conn.prepare(
            "SELECT DISTINCT i.id
             FROM items i
             JOIN formats f ON f.item_id = i.id
             WHERE f.mime = ?1
             ORDER BY i.id DESC",
        )?;
        let rows = stmt.query_map(params![mime], |r| r.get::<_, String>(0))?;
        let mut v: Vec<String> = rows.collect::<rusqlite::Result<_>>()?;
        if let Some(l) = limit {
            v.truncate(l);
        }
        v
    } else {
        let sql = match limit {
            Some(l) => format!("SELECT id FROM items ORDER BY id DESC LIMIT {l}"),
            None => "SELECT id FROM items ORDER BY id DESC".to_string(),
        };
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };

    let mut result = Vec::with_capacity(ids.len());
    for id_str in &ids {
        let id = Uuid::parse_str(id_str).map_err(Error::InvalidId)?;
        let item = get_item(conn, id)?;
        let formats = {
            let mut stmt = conn.prepare(
                "SELECT item_id, mime, blob_hash, size, captured_at
                 FROM formats WHERE item_id = ?1",
            )?;
            stmt.query_map(params![id_str], row_to_format)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        result.push(ItemWithFormats { item, formats });
    }
    Ok(result)
}

/// Delete a single item row (formats must already be removed).
pub fn delete_item_row(conn: &Connection, id: Uuid) -> Result<()> {
    let changed = conn.execute(
        "DELETE FROM items WHERE id = ?1",
        params![id.hyphenated().to_string()],
    )?;
    if changed == 0 {
        return Err(Error::NotFound);
    }
    Ok(())
}

/// Delete a single format row, returning the old blob_hash (or `NotFound`).
pub fn delete_format_row(conn: &Connection, id: Uuid, mime: &str) -> Result<String> {
    // Check the format exists and capture its hash before deletion.
    let old_hash: Option<String> = conn
        .query_row(
            "SELECT blob_hash FROM formats WHERE item_id = ?1 AND mime = ?2",
            params![id.hyphenated().to_string(), mime],
            |r| r.get(0),
        )
        .optional()?;
    let hash = old_hash.ok_or(Error::NotFound)?;
    conn.execute(
        "DELETE FROM formats WHERE item_id = ?1 AND mime = ?2",
        params![id.hyphenated().to_string(), mime],
    )?;
    Ok(hash)
}

/// Delete all format rows for an item; returns each distinct blob_hash that
/// was referenced.
pub fn delete_formats_for_item(conn: &Connection, id: Uuid) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT DISTINCT blob_hash FROM formats WHERE item_id = ?1")?;
    let hashes: Vec<String> = stmt
        .query_map(params![id.hyphenated().to_string()], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    conn.execute(
        "DELETE FROM formats WHERE item_id = ?1",
        params![id.hyphenated().to_string()],
    )?;
    Ok(hashes)
}

/// Delete items with `created_at < cutoff_ms` along with their format rows.
/// Returns the ids and blob_hashes involved.
pub fn delete_items_older_than(
    conn: &Connection,
    cutoff_ms: i64,
) -> Result<(Vec<String>, Vec<String>)> {
    // Collect ids to delete.
    let mut stmt = conn.prepare("SELECT id FROM items WHERE created_at < ?1")?;
    let ids: Vec<String> = stmt
        .query_map(params![cutoff_ms], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;

    if ids.is_empty() {
        return Ok((vec![], vec![]));
    }

    // Collect blob hashes for those items.
    let mut hashes: Vec<String> = Vec::new();
    for id_str in &ids {
        let mut h_stmt =
            conn.prepare("SELECT DISTINCT blob_hash FROM formats WHERE item_id = ?1")?;
        let hs: Vec<String> = h_stmt
            .query_map(params![id_str], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        hashes.extend(hs);
    }
    hashes.sort();
    hashes.dedup();

    // Delete format rows then item rows.
    conn.execute(
        "DELETE FROM formats WHERE item_id IN (SELECT id FROM items WHERE created_at < ?1)",
        params![cutoff_ms],
    )?;
    conn.execute(
        "DELETE FROM items WHERE created_at < ?1",
        params![cutoff_ms],
    )?;

    Ok((ids, hashes))
}

/// Check whether any `formats` row still references `hash`.
pub fn blob_hash_has_references(conn: &Connection, hash: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM formats WHERE blob_hash = ?1",
        params![hash],
        |r| r.get(0),
    )?;
    Ok(count > 0)
}
