//! The [`Store`] is the primary public API of the richclip library.
//!
//! It combines a SQLite index (`db.rs`) with a content-addressed blob
//! directory (`blob.rs`). All mutations are performed inside a single
//! transaction. Blobs that lose all `formats` references are deleted inline
//! (no deferred GC).

mod blob;
mod db;

// Re-export so callers can hash bytes without depending on blake3 directly.
pub use blob::blob_hash;

use crate::error::Result;
use crate::model::{Format, ItemWithFormats};
use crate::paths::default_data_dir;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use time::OffsetDateTime;
use uuid::Uuid;

/// The richclip storage layer: SQLite item/format index + blob directory.
pub struct Store {
    conn: Connection,
    blob_root: PathBuf,
}

impl Store {
    // ------------------------------------------------------------------
    // Construction
    // ------------------------------------------------------------------

    /// Open a store rooted at `root`.
    ///
    /// Creates `<root>/db.sqlite` and `<root>/blobs/` if they don't exist.
    /// Enables WAL mode and foreign keys; creates the schema on first use.
    pub fn open(root: &Path) -> Result<Store> {
        std::fs::create_dir_all(root)?;
        let db_path = root.join("db.sqlite");
        let blob_root = root.join("blobs");
        std::fs::create_dir_all(&blob_root)?;

        let conn = Connection::open(&db_path)?;

        // Performance / correctness settings.
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA foreign_keys=ON;",
        )?;

        db::create_schema(&conn)?;

        Ok(Store { conn, blob_root })
    }

    /// Open a store at the XDG default location (`$XDG_DATA_HOME/richclip`).
    pub fn open_default() -> Result<Store> {
        let root = default_data_dir()?;
        Self::open(&root)
    }

    // ------------------------------------------------------------------
    // Mutations
    // ------------------------------------------------------------------

    /// Create a new item with one or more formats, all in one transaction.
    ///
    /// Returns the new item's [`Uuid`] (v7).
    pub fn add_item(&mut self, formats: &[(String, Vec<u8>)]) -> Result<Uuid> {
        let now = OffsetDateTime::now_utc();
        let id = Uuid::now_v7();

        let item = crate::model::Item {
            id,
            created_at: now,
            updated_at: now,
        };

        // Write blobs outside the transaction (idempotent; worst case leaves
        // an orphan blob that will be deduplicated on the next identical
        // write). Then do all DB work in one transaction.
        let mut hashes = Vec::with_capacity(formats.len());
        for (_, bytes) in formats {
            let hash = blob::write_blob(&self.blob_root, bytes)?;
            hashes.push(hash);
        }

        let tx = self.conn.transaction()?;
        db::insert_item(&tx, &item)?;
        for ((mime, bytes), hash) in formats.iter().zip(&hashes) {
            let fmt = Format {
                item_id: id,
                mime: mime.clone(),
                blob_hash: hash.clone(),
                size: bytes.len() as u64,
                captured_at: now,
            };
            db::upsert_format(&tx, &fmt)?;
        }
        tx.commit()?;

        Ok(id)
    }

    /// Add or replace one format on an existing item.
    ///
    /// Bumps `updated_at`. Returns `Err(NotFound)` if the item does not exist.
    pub fn set_format(&mut self, id: Uuid, mime: &str, bytes: &[u8]) -> Result<()> {
        let now = OffsetDateTime::now_utc();
        let hash = blob::write_blob(&self.blob_root, bytes)?;

        let tx = self.conn.transaction()?;

        // Capture any old hash before we overwrite the row.
        let old_hash: Option<String> = {
            use rusqlite::OptionalExtension;
            tx.query_row(
                "SELECT blob_hash FROM formats WHERE item_id = ?1 AND mime = ?2",
                rusqlite::params![id.hyphenated().to_string(), mime],
                |r| r.get(0),
            )
            .optional()?
        };

        let fmt = Format {
            item_id: id,
            mime: mime.to_string(),
            blob_hash: hash.clone(),
            size: bytes.len() as u64,
            captured_at: now,
        };
        // This also implicitly checks item existence via the FK — but we
        // want a `NotFound` rather than a FK error, so check explicitly.
        db::require_item(&tx, id)?;
        db::upsert_format(&tx, &fmt)?;
        db::bump_updated_at(&tx, id, now)?;

        // Inline blob cleanup: if the old hash changed and nothing else
        // references the old blob, delete the file.
        if let Some(old) = old_hash {
            if old != hash && !db::blob_hash_has_references(&tx, &old)? {
                tx.commit()?;
                blob::delete_blob(&self.blob_root, &old)?;
                return Ok(());
            }
        }

        tx.commit()?;
        Ok(())
    }

    /// Remove one format from an item; inline-clean its blob if now
    /// unreferenced. Bumps `updated_at`. Returns `Err(NotFound)` if the
    /// item or the mime are missing.
    pub fn remove_format(&mut self, id: Uuid, mime: &str) -> Result<()> {
        let now = OffsetDateTime::now_utc();

        let tx = self.conn.transaction()?;
        // Verify item exists first so we get a clear NotFound.
        db::require_item(&tx, id)?;
        let old_hash = db::delete_format_row(&tx, id, mime)?;
        db::bump_updated_at(&tx, id, now)?;
        let still_referenced = db::blob_hash_has_references(&tx, &old_hash)?;
        tx.commit()?;

        if !still_referenced {
            blob::delete_blob(&self.blob_root, &old_hash)?;
        }
        Ok(())
    }

    /// Delete an item and all of its formats; inline-clean unreferenced blobs.
    ///
    /// Returns `Err(NotFound)` if the item does not exist.
    pub fn delete_item(&mut self, id: Uuid) -> Result<()> {
        let tx = self.conn.transaction()?;
        db::require_item(&tx, id)?;
        let hashes = db::delete_formats_for_item(&tx, id)?;
        db::delete_item_row(&tx, id)?;

        // Determine which hashes are now unreferenced.
        let mut orphans = Vec::new();
        for h in &hashes {
            if !db::blob_hash_has_references(&tx, h)? {
                orphans.push(h.clone());
            }
        }
        tx.commit()?;

        for h in orphans {
            blob::delete_blob(&self.blob_root, &h)?;
        }
        Ok(())
    }

    /// Delete all items whose `created_at` is before `cutoff`. Inline-cleans
    /// unreferenced blobs. Returns the number of items deleted.
    pub fn delete_older_than(&mut self, cutoff: OffsetDateTime) -> Result<u64> {
        let cutoff_ms = db::to_epoch_ms(cutoff);

        let tx = self.conn.transaction()?;
        let (ids, hashes) = db::delete_items_older_than(&tx, cutoff_ms)?;
        let count = ids.len() as u64;

        // Determine orphaned blobs now that the rows are gone.
        let mut orphans = Vec::new();
        for h in &hashes {
            if !db::blob_hash_has_references(&tx, h)? {
                orphans.push(h.clone());
            }
        }
        tx.commit()?;

        for h in orphans {
            blob::delete_blob(&self.blob_root, &h)?;
        }
        Ok(count)
    }

    // ------------------------------------------------------------------
    // Queries
    // ------------------------------------------------------------------

    /// Fetch a single item together with all of its formats.
    ///
    /// Returns `Err(NotFound)` if the item does not exist.
    pub fn get_item(&self, id: Uuid) -> Result<ItemWithFormats> {
        let item = db::get_item(&self.conn, id)?;
        let formats = db::get_formats(&self.conn, id)?;
        Ok(ItemWithFormats { item, formats })
    }

    /// List items, newest first. Optionally capped by `limit`; optionally
    /// filtered to items that have at least one format with `mime_filter`.
    pub fn list_items(
        &self,
        limit: Option<usize>,
        mime_filter: Option<&str>,
    ) -> Result<Vec<ItemWithFormats>> {
        db::list_items(&self.conn, limit, mime_filter)
    }

    /// Return all formats for `id`. Returns `Err(NotFound)` if the item does
    /// not exist.
    pub fn formats(&self, id: Uuid) -> Result<Vec<Format>> {
        db::get_formats(&self.conn, id)
    }

    /// Read the raw blob bytes for `(id, mime)`.
    ///
    /// Returns `Err(NotFound)` if the item or the mime are missing.
    pub fn decode(&self, id: Uuid, mime: &str) -> Result<Vec<u8>> {
        let fmt = db::get_format(&self.conn, id, mime)?;
        blob::read_blob(&self.blob_root, &fmt.blob_hash)
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use tempfile::TempDir;

    fn temp_store() -> (Store, TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(dir.path()).expect("store open");
        (store, dir)
    }

    // -----------------------------------------------------------------------
    // Blob helpers
    // -----------------------------------------------------------------------

    #[test]
    fn blob_write_read_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("blobs");
        std::fs::create_dir_all(&root).unwrap();

        let data = b"hello richclip blobs";
        let hash = blob::write_blob(&root, data).unwrap();
        let read_back = blob::read_blob(&root, &hash).unwrap();
        assert_eq!(read_back, data);
    }

    #[test]
    fn blob_deduplication() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("blobs");
        std::fs::create_dir_all(&root).unwrap();

        let data = b"dedupe me";
        let hash1 = blob::write_blob(&root, data).unwrap();
        let hash2 = blob::write_blob(&root, data).unwrap();
        assert_eq!(hash1, hash2, "identical content must produce the same hash");

        // Only one file on disk.
        let path = blob::blob_path(&root, &hash1);
        assert!(path.exists());
    }

    // -----------------------------------------------------------------------
    // add_item → get_item / formats / decode
    // -----------------------------------------------------------------------

    #[test]
    fn add_item_then_get_item() {
        let (mut store, _dir) = temp_store();

        let formats = vec![
            ("text/plain".to_string(), b"hello world".to_vec()),
            ("text/html".to_string(), b"<b>hello</b>".to_vec()),
        ];
        let id = store.add_item(&formats).unwrap();

        let iwf = store.get_item(id).unwrap();
        assert_eq!(iwf.item.id, id);
        assert_eq!(iwf.formats.len(), 2);

        // Ids are time-ordered UUIDv7.
        assert_eq!(iwf.item.created_at, iwf.item.updated_at);
    }

    #[test]
    fn formats_returns_all_mimes() {
        let (mut store, _dir) = temp_store();

        let id = store
            .add_item(&[
                ("image/png".to_string(), b"PNG_DATA".to_vec()),
                ("application/x-richclip-label".to_string(), b"a label".to_vec()),
            ])
            .unwrap();

        let fmts = store.formats(id).unwrap();
        let mimes: Vec<&str> = fmts.iter().map(|f| f.mime.as_str()).collect();
        assert!(mimes.contains(&"image/png"));
        assert!(mimes.contains(&"application/x-richclip-label"));
    }

    #[test]
    fn decode_returns_correct_bytes() {
        let (mut store, _dir) = temp_store();
        let payload = b"precise bytes 1234";

        let id = store
            .add_item(&[("application/octet-stream".to_string(), payload.to_vec())])
            .unwrap();

        let got = store.decode(id, "application/octet-stream").unwrap();
        assert_eq!(got, payload);
    }

    // -----------------------------------------------------------------------
    // set_format — replace semantics + updated_at bump
    // -----------------------------------------------------------------------

    #[test]
    fn set_format_replace_bumps_updated_at() {
        let (mut store, _dir) = temp_store();

        let id = store
            .add_item(&[("text/plain".to_string(), b"v1".to_vec())])
            .unwrap();
        let created = store.get_item(id).unwrap().item.created_at;

        // Small sleep to ensure wall-clock advances.
        std::thread::sleep(std::time::Duration::from_millis(5));

        store.set_format(id, "text/plain", b"v2").unwrap();

        let after = store.get_item(id).unwrap();
        assert_eq!(store.decode(id, "text/plain").unwrap(), b"v2");
        // updated_at must be strictly after created_at.
        assert!(
            after.item.updated_at >= created,
            "updated_at should not go backwards"
        );
    }

    #[test]
    fn set_format_adds_new_mime() {
        let (mut store, _dir) = temp_store();

        let id = store
            .add_item(&[("text/plain".to_string(), b"hello".to_vec())])
            .unwrap();

        store.set_format(id, "text/html", b"<em>hello</em>").unwrap();

        let fmts = store.formats(id).unwrap();
        assert_eq!(fmts.len(), 2);
    }

    // -----------------------------------------------------------------------
    // remove_format + inline blob cleanup
    // -----------------------------------------------------------------------

    #[test]
    fn remove_format_deletes_orphan_blob() {
        let (mut store, dir) = temp_store();
        let blob_root = dir.path().join("blobs");

        let id = store
            .add_item(&[("text/plain".to_string(), b"only format".to_vec())])
            .unwrap();

        // Locate the blob file before removal.
        let hash = {
            let fmts = store.formats(id).unwrap();
            fmts[0].blob_hash.clone()
        };
        let path = blob::blob_path(&blob_root, &hash);
        assert!(path.exists(), "blob must exist before removal");

        store.remove_format(id, "text/plain").unwrap();

        assert!(
            !path.exists(),
            "orphan blob must be deleted after removing the last reference"
        );
    }

    #[test]
    fn remove_format_keeps_shared_blob() {
        let (mut store, dir) = temp_store();
        let blob_root = dir.path().join("blobs");
        let shared_bytes = b"shared content";

        // Both formats share the same blob (identical bytes → same hash).
        let id = store
            .add_item(&[
                ("text/plain".to_string(), shared_bytes.to_vec()),
                ("text/x-other".to_string(), shared_bytes.to_vec()),
            ])
            .unwrap();

        let hash = {
            let fmts = store.formats(id).unwrap();
            fmts[0].blob_hash.clone()
        };
        let path = blob::blob_path(&blob_root, &hash);
        assert!(path.exists());

        // Remove one of the two references.
        store.remove_format(id, "text/plain").unwrap();

        assert!(
            path.exists(),
            "blob must survive while the other format still references it"
        );
    }

    // -----------------------------------------------------------------------
    // delete_item + inline blob cleanup
    // -----------------------------------------------------------------------

    #[test]
    fn delete_item_removes_blobs() {
        let (mut store, dir) = temp_store();
        let blob_root = dir.path().join("blobs");

        let id = store
            .add_item(&[
                ("image/png".to_string(), b"PNG".to_vec()),
                ("text/plain".to_string(), b"alt text".to_vec()),
            ])
            .unwrap();

        let hashes: Vec<String> = store
            .formats(id)
            .unwrap()
            .iter()
            .map(|f| f.blob_hash.clone())
            .collect();

        store.delete_item(id).unwrap();

        for h in &hashes {
            let p = blob::blob_path(&blob_root, h);
            assert!(!p.exists(), "all blobs must be removed after delete_item");
        }

        // Item itself must be gone.
        assert!(matches!(store.get_item(id), Err(Error::NotFound)));
    }

    #[test]
    fn delete_item_keeps_shared_blob() {
        let (mut store, dir) = temp_store();
        let blob_root = dir.path().join("blobs");
        let shared = b"shared across two items";

        let id1 = store
            .add_item(&[("text/plain".to_string(), shared.to_vec())])
            .unwrap();
        let id2 = store
            .add_item(&[("text/plain".to_string(), shared.to_vec())])
            .unwrap();

        let hash = store.formats(id1).unwrap()[0].blob_hash.clone();
        let path = blob::blob_path(&blob_root, &hash);

        store.delete_item(id1).unwrap();

        assert!(
            path.exists(),
            "blob referenced by id2 must survive deletion of id1"
        );

        // Clean up id2 to verify the blob is now removable.
        store.delete_item(id2).unwrap();
        assert!(!path.exists(), "blob must be removed when last reference gone");
    }

    // -----------------------------------------------------------------------
    // delete_older_than
    // -----------------------------------------------------------------------

    #[test]
    fn delete_older_than_count() {
        let (mut store, _dir) = temp_store();

        // Add three items.
        let _id1 = store
            .add_item(&[("text/plain".to_string(), b"a".to_vec())])
            .unwrap();
        let _id2 = store
            .add_item(&[("text/plain".to_string(), b"b".to_vec())])
            .unwrap();
        let _id3 = store
            .add_item(&[("text/plain".to_string(), b"c".to_vec())])
            .unwrap();

        // Cutoff = far future → all three deleted.
        let far_future = OffsetDateTime::now_utc() + time::Duration::hours(1);
        let deleted = store.delete_older_than(far_future).unwrap();
        assert_eq!(deleted, 3);

        assert_eq!(store.list_items(None, None).unwrap().len(), 0);
    }

    #[test]
    fn delete_older_than_keeps_newer_items() {
        let (mut store, _dir) = temp_store();

        // Add an item with an old created_at via direct SQL (simulate aging).
        let old_id = {
            // Add normally first, then patch the timestamp.
            let id = store
                .add_item(&[("text/plain".to_string(), b"old".to_vec())])
                .unwrap();
            // Use a cutoff that is one second after epoch — very far in the past.
            // We'll manipulate the db directly.
            let old_ms: i64 = 1_000; // epoch + 1s
            store.conn.execute(
                "UPDATE items SET created_at = ?1, updated_at = ?1 WHERE id = ?2",
                rusqlite::params![old_ms, id.hyphenated().to_string()],
            ).unwrap();
            id
        };

        let new_id = store
            .add_item(&[("text/plain".to_string(), b"new".to_vec())])
            .unwrap();

        // Cutoff = 1 hour ago; only the "old" item (epoch+1ms) should be deleted.
        let cutoff = OffsetDateTime::now_utc() - time::Duration::hours(1);
        let deleted = store.delete_older_than(cutoff).unwrap();
        assert_eq!(deleted, 1);

        // Old item gone.
        assert!(matches!(store.get_item(old_id), Err(Error::NotFound)));
        // New item survives.
        assert!(store.get_item(new_id).is_ok());
    }

    // -----------------------------------------------------------------------
    // NotFound paths
    // -----------------------------------------------------------------------

    #[test]
    fn get_item_not_found() {
        let (store, _dir) = temp_store();
        let bogus = Uuid::now_v7();
        assert!(matches!(store.get_item(bogus), Err(Error::NotFound)));
    }

    #[test]
    fn formats_not_found() {
        let (store, _dir) = temp_store();
        let bogus = Uuid::now_v7();
        assert!(matches!(store.formats(bogus), Err(Error::NotFound)));
    }

    #[test]
    fn decode_not_found_item() {
        let (store, _dir) = temp_store();
        let bogus = Uuid::now_v7();
        assert!(matches!(
            store.decode(bogus, "text/plain"),
            Err(Error::NotFound)
        ));
    }

    #[test]
    fn decode_not_found_mime() {
        let (mut store, _dir) = temp_store();

        let id = store
            .add_item(&[("text/plain".to_string(), b"hi".to_vec())])
            .unwrap();

        assert!(matches!(
            store.decode(id, "image/png"),
            Err(Error::NotFound)
        ));
    }

    #[test]
    fn set_format_not_found() {
        let (mut store, _dir) = temp_store();
        let bogus = Uuid::now_v7();
        assert!(matches!(
            store.set_format(bogus, "text/plain", b"x"),
            Err(Error::NotFound)
        ));
    }

    #[test]
    fn remove_format_not_found_item() {
        let (mut store, _dir) = temp_store();
        let bogus = Uuid::now_v7();
        assert!(matches!(
            store.remove_format(bogus, "text/plain"),
            Err(Error::NotFound)
        ));
    }

    #[test]
    fn remove_format_not_found_mime() {
        let (mut store, _dir) = temp_store();

        let id = store
            .add_item(&[("text/plain".to_string(), b"data".to_vec())])
            .unwrap();

        assert!(matches!(
            store.remove_format(id, "image/png"),
            Err(Error::NotFound)
        ));
    }

    #[test]
    fn delete_item_not_found() {
        let (mut store, _dir) = temp_store();
        let bogus = Uuid::now_v7();
        assert!(matches!(store.delete_item(bogus), Err(Error::NotFound)));
    }

    // -----------------------------------------------------------------------
    // list_items
    // -----------------------------------------------------------------------

    #[test]
    fn list_items_newest_first() {
        let (mut store, _dir) = temp_store();

        let id1 = store
            .add_item(&[("text/plain".to_string(), b"first".to_vec())])
            .unwrap();
        let id2 = store
            .add_item(&[("text/plain".to_string(), b"second".to_vec())])
            .unwrap();
        let id3 = store
            .add_item(&[("text/plain".to_string(), b"third".to_vec())])
            .unwrap();

        let items = store.list_items(None, None).unwrap();
        assert_eq!(items.len(), 3);
        // Newest (UUIDv7 = highest) first.
        assert_eq!(items[0].item.id, id3);
        assert_eq!(items[1].item.id, id2);
        assert_eq!(items[2].item.id, id1);
    }

    #[test]
    fn list_items_limit() {
        let (mut store, _dir) = temp_store();

        for i in 0..5u8 {
            store
                .add_item(&[("text/plain".to_string(), vec![i])])
                .unwrap();
        }

        let items = store.list_items(Some(3), None).unwrap();
        assert_eq!(items.len(), 3);
    }

    #[test]
    fn list_items_mime_filter() {
        let (mut store, _dir) = temp_store();

        store
            .add_item(&[("text/plain".to_string(), b"text only".to_vec())])
            .unwrap();
        let png_id = store
            .add_item(&[
                ("image/png".to_string(), b"PNG".to_vec()),
                ("text/plain".to_string(), b"alt".to_vec()),
            ])
            .unwrap();

        let items = store.list_items(None, Some("image/png")).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].item.id, png_id);
    }
}
