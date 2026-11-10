//! The [`Store`] is the primary public API of the richclip library.
//!
//! It combines a SQLite index (`db.rs`) with a content-addressed blob
//! directory (`blob.rs`). Database changes use one transaction per mutation.
//! Blob writes and reference-checked cleanup run under SQLite's writer lock so
//! independent store processes cannot race file creation against deletion.

mod blob;
mod db;

// Re-export so callers can hash bytes without depending on blake3 directly.
pub use blob::blob_hash;

use crate::error::Result;
use crate::model::{Format, ItemWithFormats};
use crate::paths::default_data_dir;
use rusqlite::{Connection, TransactionBehavior};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use time::OffsetDateTime;
use uuid::Uuid;

/// The committed result of adding an item, including history rows replaced by
/// this add and the final MIME list visible to consumers.
#[derive(Debug, Clone)]
pub struct AddItemResult {
    pub id: Uuid,
    pub created_at: OffsetDateTime,
    pub formats: Vec<String>,
    pub removed_ids: Vec<Uuid>,
    /// A post-commit blob cleanup failure does not undo the add. Callers that
    /// can report warnings should surface this message.
    pub blob_cleanup_error: Option<String>,
}

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
        self.add_item_with_result(formats).map(|result| result.id)
    }

    /// Add an item and return the final stored formats and older items removed
    /// because they had identical clipboard content.
    ///
    /// This keeps the established [`Store::add_item`] return type available to
    /// library callers while letting event-producing callers preserve delete
    /// side effects and publish the actual stored MIME list.
    pub fn add_item_with_result(&mut self, formats: &[(String, Vec<u8>)]) -> Result<AddItemResult> {
        // The primary key makes the last supplied value for each MIME the
        // effective persisted value. Keep that same view for comparison and
        // insertion.
        let mut effective = BTreeMap::<String, Vec<u8>>::new();
        for (mime, bytes) in formats {
            effective.insert(mime.clone(), bytes.clone());
        }

        let clipboard_content: BTreeMap<String, String> = effective
            .iter()
            .filter(|(mime, _)| !is_internal_mime(mime))
            .map(|(mime, bytes)| (mime.clone(), blob::hash_bytes(bytes)))
            .collect();

        // IMMEDIATE reserves the SQLite writer before checking history or
        // touching blob files. Independent Store instances therefore see
        // earlier commits and cannot race blob cleanup against a new write.
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;

        let matching = if clipboard_content.is_empty() {
            Vec::new()
        } else {
            db::list_items(&tx, None, None)?
                .into_iter()
                .filter(|item| {
                    let existing: BTreeMap<String, String> = item
                        .formats
                        .iter()
                        .filter(|format| !is_internal_mime(&format.mime))
                        .map(|format| (format.mime.clone(), format.blob_hash.clone()))
                        .collect();
                    !existing.is_empty() && existing == clipboard_content
                })
                .collect::<Vec<_>>()
        };

        let inherited: Vec<Format> = matching
            .first()
            .map(|item| {
                item.formats
                    .iter()
                    .filter(|format| {
                        is_internal_mime(&format.mime) && !effective.contains_key(&format.mime)
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();

        let now = OffsetDateTime::now_utc();
        let id = Uuid::now_v7();
        let item = crate::model::Item {
            id,
            created_at: now,
            updated_at: now,
        };

        let mut orphan_candidates = BTreeSet::new();
        db::insert_item(&tx, &item)?;
        for (mime, bytes) in &effective {
            let hash = blob::write_blob(&self.blob_root, bytes)?;
            db::upsert_format(
                &tx,
                &Format {
                    item_id: id,
                    mime: mime.clone(),
                    blob_hash: hash,
                    size: bytes.len() as u64,
                    captured_at: now,
                },
            )?;
        }
        for mut format in inherited {
            format.item_id = id;
            db::upsert_format(&tx, &format)?;
        }

        let removed_ids: Vec<Uuid> = matching.iter().map(|item| item.item.id).collect();
        for old_id in &removed_ids {
            orphan_candidates.extend(db::delete_formats_for_item(&tx, *old_id)?);
            db::delete_item_row(&tx, *old_id)?;
        }

        let mut final_formats = db::get_formats(&tx, id)?;
        final_formats.sort_by(|a, b| a.mime.cmp(&b.mime));
        let mime_list = final_formats
            .into_iter()
            .map(|format| format.mime)
            .collect();

        tx.commit()?;
        let blob_cleanup_error = self
            .cleanup_unreferenced_blobs(orphan_candidates)
            .err()
            .map(|error| error.to_string());

        Ok(AddItemResult {
            id,
            created_at: now,
            formats: mime_list,
            removed_ids,
            blob_cleanup_error,
        })
    }

    /// Add or replace one format on an existing item.
    ///
    /// Bumps `updated_at`. Returns `Err(NotFound)` if the item does not exist.
    pub fn set_format(&mut self, id: Uuid, mime: &str, bytes: &[u8]) -> Result<()> {
        let now = OffsetDateTime::now_utc();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let hash = blob::write_blob(&self.blob_root, bytes)?;

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
        if let Some(old) = old_hash
            && old != hash
            && !db::blob_hash_has_references(&tx, &old)?
        {
            tx.commit()?;
            self.cleanup_unreferenced_blobs([old])?;
            return Ok(());
        }

        tx.commit()?;
        Ok(())
    }

    /// Remove one format from an item; inline-clean its blob if now
    /// unreferenced. Bumps `updated_at`. Returns `Err(NotFound)` if the
    /// item or the mime are missing.
    pub fn remove_format(&mut self, id: Uuid, mime: &str) -> Result<()> {
        let now = OffsetDateTime::now_utc();

        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Verify item exists first so we get a clear NotFound.
        db::require_item(&tx, id)?;
        let old_hash = db::delete_format_row(&tx, id, mime)?;
        db::bump_updated_at(&tx, id, now)?;
        tx.commit()?;
        self.cleanup_unreferenced_blobs([old_hash])?;
        Ok(())
    }

    /// Delete an item and all of its formats; inline-clean unreferenced blobs.
    ///
    /// Returns `Err(NotFound)` if the item does not exist.
    pub fn delete_item(&mut self, id: Uuid) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        db::require_item(&tx, id)?;
        let hashes = db::delete_formats_for_item(&tx, id)?;
        db::delete_item_row(&tx, id)?;
        tx.commit()?;
        self.cleanup_unreferenced_blobs(hashes)
    }

    /// Delete all items whose `created_at` is before `cutoff`. Inline-cleans
    /// unreferenced blobs. Returns the number of items deleted.
    pub fn delete_older_than(&mut self, cutoff: OffsetDateTime) -> Result<u64> {
        let cutoff_ms = db::to_epoch_ms(cutoff);

        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (ids, hashes) = db::delete_items_older_than(&tx, cutoff_ms)?;
        let count = ids.len() as u64;
        tx.commit()?;
        self.cleanup_unreferenced_blobs(hashes)?;
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

    /// Delete blobs that remain unreferenced while holding SQLite's writer
    /// lock. All blob creation also happens under this lock, so another
    /// process cannot add a reference between the check and unlink.
    fn cleanup_unreferenced_blobs(
        &mut self,
        hashes: impl IntoIterator<Item = String>,
    ) -> Result<()> {
        let hashes: BTreeSet<String> = hashes.into_iter().collect();
        if hashes.is_empty() {
            return Ok(());
        }

        let blob_root = self.blob_root.clone();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for hash in hashes {
            if !db::blob_hash_has_references(&tx, &hash)? {
                blob::delete_blob(&blob_root, &hash)?;
            }
        }
        tx.commit()?;
        Ok(())
    }
}

fn is_internal_mime(mime: &str) -> bool {
    mime.starts_with("application/x-richclip-")
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
    fn adding_identical_content_keeps_the_new_id() {
        let (mut store, _dir) = temp_store();
        let first = store
            .add_item(&[("text/plain".into(), b"A".to_vec())])
            .unwrap();
        let second = store
            .add_item(&[("text/plain".into(), b"A".to_vec())])
            .unwrap();

        assert_ne!(first, second);
        let items = store.list_items(None, None).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].item.id, second);
    }

    #[test]
    fn adding_a_b_a_keeps_newest_a_then_b() {
        let (mut store, _dir) = temp_store();
        let _first_a = store
            .add_item(&[("text/plain".into(), b"A".to_vec())])
            .unwrap();
        let b = store
            .add_item(&[("text/plain".into(), b"B".to_vec())])
            .unwrap();
        let newest_a = store
            .add_item(&[("text/plain".into(), b"A".to_vec())])
            .unwrap();

        let items = store.list_items(None, None).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].item.id, newest_a);
        assert_eq!(items[1].item.id, b);
    }

    #[test]
    fn matching_uses_unordered_complete_mime_and_byte_sets() {
        let (mut store, _dir) = temp_store();
        let first = store
            .add_item(&[
                ("text/plain".into(), b"plain".to_vec()),
                ("text/html".into(), b"<b>plain</b>".to_vec()),
            ])
            .unwrap();
        let reordered = store
            .add_item(&[
                ("text/html".into(), b"<b>plain</b>".to_vec()),
                ("text/plain".into(), b"plain".to_vec()),
            ])
            .unwrap();
        assert!(
            !store
                .list_items(None, None)
                .unwrap()
                .iter()
                .any(|i| i.item.id == first)
        );
        assert!(store.get_item(reordered).is_ok());

        store
            .add_item(&[("text/plain".into(), b"plain".to_vec())])
            .unwrap();
        store
            .add_item(&[
                ("text/plain".into(), b"plain".to_vec()),
                ("text/html".into(), b"<i>plain</i>".to_vec()),
            ])
            .unwrap();
        store
            .add_item(&[("text/uri-list".into(), b"plain".to_vec())])
            .unwrap();

        assert_eq!(store.list_items(None, None).unwrap().len(), 4);
    }

    #[test]
    fn repeated_mime_input_uses_last_value_for_matching_and_storage() {
        let (mut store, _dir) = temp_store();
        let first = store
            .add_item(&[
                ("text/plain".into(), b"ignored first value".to_vec()),
                ("text/plain".into(), b"effective last value".to_vec()),
            ])
            .unwrap();
        let replacement = store
            .add_item(&[("text/plain".into(), b"effective last value".to_vec())])
            .unwrap();

        assert_ne!(first, replacement);
        assert_eq!(store.list_items(None, None).unwrap().len(), 1);
        assert_eq!(store.formats(replacement).unwrap().len(), 1);
        assert_eq!(
            store.decode(replacement, "text/plain").unwrap(),
            b"effective last value"
        );
    }

    #[test]
    fn enrichment_is_inherited_from_newest_legacy_match_and_reported() {
        let (mut store, _dir) = temp_store();
        let older_a = store
            .add_item(&[("text/plain".into(), b"A".to_vec())])
            .unwrap();
        let b = store
            .add_item(&[("text/plain".into(), b"B".to_vec())])
            .unwrap();
        let newer_legacy_a = store
            .add_item(&[
                ("text/plain".into(), b"C".to_vec()),
                (
                    "application/x-richclip-label".into(),
                    b"newest label".to_vec(),
                ),
                ("application/x-richclip-ocr".into(), b"newest OCR".to_vec()),
            ])
            .unwrap();
        store
            .set_format(newer_legacy_a, "text/plain", b"A")
            .unwrap();
        store
            .set_format(older_a, "application/x-richclip-label", b"older label")
            .unwrap();

        let added = store
            .add_item_with_result(&[("text/plain".into(), b"A".to_vec())])
            .unwrap();

        assert_eq!(added.removed_ids, vec![newer_legacy_a, older_a]);
        assert!(matches!(
            store.set_format(
                newer_legacy_a,
                "application/x-richclip-label",
                b"late label"
            ),
            Err(Error::NotFound)
        ));
        assert!(
            added
                .formats
                .contains(&"application/x-richclip-label".into())
        );
        assert!(added.formats.contains(&"application/x-richclip-ocr".into()));
        assert_eq!(
            store
                .decode(added.id, "application/x-richclip-label")
                .unwrap(),
            b"newest label"
        );
        assert_eq!(
            store
                .decode(added.id, "application/x-richclip-ocr")
                .unwrap(),
            b"newest OCR"
        );
        let items = store.list_items(None, None).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].item.id, added.id);
        assert_eq!(items[1].item.id, b);
    }

    #[test]
    fn incoming_enrichment_wins_and_empty_content_items_do_not_collapse() {
        let (mut store, _dir) = temp_store();
        let old = store
            .add_item(&[
                ("text/plain".into(), b"A".to_vec()),
                (
                    "application/x-richclip-label".into(),
                    b"stored label".to_vec(),
                ),
                ("application/x-richclip-ocr".into(), b"stored OCR".to_vec()),
            ])
            .unwrap();
        let added = store
            .add_item_with_result(&[
                ("text/plain".into(), b"A".to_vec()),
                (
                    "application/x-richclip-label".into(),
                    b"incoming label".to_vec(),
                ),
            ])
            .unwrap();
        assert_eq!(added.removed_ids, vec![old]);
        assert_eq!(
            store
                .decode(added.id, "application/x-richclip-label")
                .unwrap(),
            b"incoming label"
        );
        assert_eq!(
            store
                .decode(added.id, "application/x-richclip-ocr")
                .unwrap(),
            b"stored OCR"
        );

        let meta_one = store
            .add_item(&[("application/x-richclip-label".into(), b"metadata".to_vec())])
            .unwrap();
        let meta_two = store
            .add_item(&[("application/x-richclip-label".into(), b"metadata".to_vec())])
            .unwrap();
        assert_ne!(meta_one, meta_two);
        assert!(store.get_item(meta_one).is_ok());
        assert!(store.get_item(meta_two).is_ok());
    }

    #[test]
    fn matching_ignores_lifecycle_metadata_but_preserves_mime_spelling() {
        let (mut store, _dir) = temp_store();
        let old = store
            .add_item(&[("text/plain;charset=utf-8".into(), b"A".to_vec())])
            .unwrap();
        store
            .set_format(old, "application/x-richclip-extra", b"metadata")
            .unwrap();
        let new_same_mime = store
            .add_item(&[("text/plain;charset=utf-8".into(), b"A".to_vec())])
            .unwrap();
        assert!(matches!(store.get_item(old), Err(Error::NotFound)));
        assert!(
            store
                .formats(new_same_mime)
                .unwrap()
                .iter()
                .any(|f| f.mime == "application/x-richclip-extra")
        );

        store
            .add_item(&[("text/plain".into(), b"A".to_vec())])
            .unwrap();
        assert_eq!(store.list_items(None, None).unwrap().len(), 2);
    }

    #[test]
    fn add_failure_rolls_back_insertion_and_all_pruning() {
        let (mut store, _dir) = temp_store();
        let first = store
            .add_item(&[("text/plain".into(), b"A".to_vec())])
            .unwrap();
        let second = store
            .add_item(&[("text/plain".into(), b"B".to_vec())])
            .unwrap();
        store.set_format(second, "text/plain", b"A").unwrap();
        store
            .conn
            .execute_batch(
                "CREATE TRIGGER fail_history_delete BEFORE DELETE ON items
                 BEGIN SELECT RAISE(ABORT, 'forced delete failure'); END;",
            )
            .unwrap();

        assert!(
            store
                .add_item(&[("text/plain".into(), b"A".to_vec())])
                .is_err()
        );
        assert_eq!(store.list_items(None, None).unwrap().len(), 2);
        assert_eq!(store.decode(first, "text/plain").unwrap(), b"A");
        assert_eq!(store.decode(second, "text/plain").unwrap(), b"A");
    }

    #[test]
    fn independent_concurrent_adds_replace_each_other_and_keep_blob() {
        use std::sync::{Arc, Barrier};

        let dir = tempfile::tempdir().unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let store_a = Store::open(dir.path()).unwrap();
        let store_b = Store::open(dir.path()).unwrap();
        let mut workers = Vec::new();
        for mut store in [store_a, store_b] {
            let barrier = barrier.clone();
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                store
                    .add_item(&[("text/plain".into(), b"same bytes".to_vec())])
                    .unwrap()
            }));
        }
        barrier.wait();
        let ids: Vec<Uuid> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();

        let store = Store::open(dir.path()).unwrap();
        let items = store.list_items(None, None).unwrap();
        assert_eq!(items.len(), 1);
        assert!(ids.contains(&items[0].item.id));
        assert_eq!(
            store.decode(items[0].item.id, "text/plain").unwrap(),
            b"same bytes"
        );
    }

    #[test]
    fn blob_cleanup_racing_an_independent_add_keeps_surviving_reference() {
        use std::sync::{Arc, Barrier};

        let dir = tempfile::tempdir().unwrap();
        let mut setup = Store::open(dir.path()).unwrap();
        let old_id = setup
            .add_item(&[("text/plain".into(), b"shared bytes".to_vec())])
            .unwrap();
        drop(setup);

        let delete_store = Store::open(dir.path()).unwrap();
        let add_store = Store::open(dir.path()).unwrap();

        let barrier = Arc::new(Barrier::new(3));
        let delete_barrier = barrier.clone();
        let deleter = std::thread::spawn(move || {
            let mut store = delete_store;
            delete_barrier.wait();
            store.delete_item(old_id).unwrap();
        });
        let add_barrier = barrier.clone();
        let adder = std::thread::spawn(move || {
            let mut store = add_store;
            add_barrier.wait();
            store
                .add_item(&[("text/html".into(), b"shared bytes".to_vec())])
                .unwrap()
        });
        barrier.wait();
        deleter.join().unwrap();
        let new_id = adder.join().unwrap();

        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.decode(new_id, "text/html").unwrap(), b"shared bytes");
    }

    #[test]
    fn formats_returns_all_mimes() {
        let (mut store, _dir) = temp_store();

        let id = store
            .add_item(&[
                ("image/png".to_string(), b"PNG_DATA".to_vec()),
                (
                    "application/x-richclip-label".to_string(),
                    b"a label".to_vec(),
                ),
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

        store
            .set_format(id, "text/html", b"<em>hello</em>")
            .unwrap();

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
            .add_item(&[("text/html".to_string(), shared.to_vec())])
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
        assert!(
            !path.exists(),
            "blob must be removed when last reference gone"
        );
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
            store
                .conn
                .execute(
                    "UPDATE items SET created_at = ?1, updated_at = ?1 WHERE id = ?2",
                    rusqlite::params![old_ms, id.hyphenated().to_string()],
                )
                .unwrap();
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
