//! Content-addressed blob store backed by the filesystem.
//!
//! Blobs are stored at `<root>/blobs/<aa>/<rest>` where `aa` is the first two
//! hex characters of the `blake3` hash of the content. This two-level fanout
//! keeps directory sizes reasonable.

use crate::error::{Error, Result};
use std::path::{Path, PathBuf};

/// Compute the `blake3` hex digest of `bytes`.
pub fn hash_bytes(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// Public re-export of [`hash_bytes`] for use outside the `store` module.
///
/// This exists so `richclip-wayland` (which cannot add `blake3` as a direct
/// dependency) can compute blob hashes without duplicating the logic.
pub fn blob_hash(bytes: &[u8]) -> String {
    hash_bytes(bytes)
}

/// Derive the filesystem path for a blob given the blob root and hash.
pub fn blob_path(root: &Path, hash: &str) -> PathBuf {
    // hash is a 64-char hex string; use the first two chars as the bucket dir.
    let (prefix, rest) = hash.split_at(2);
    root.join(prefix).join(rest)
}

/// Write `bytes` to the blob store idempotently (deduplication by hash).
///
/// Returns the hex hash of `bytes`.
pub fn write_blob(root: &Path, bytes: &[u8]) -> Result<String> {
    let hash = hash_bytes(bytes);
    let path = blob_path(root, &hash);
    if path.exists() {
        // Already stored — nothing to do.
        return Ok(hash);
    }
    // Ensure the bucket directory exists.
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, bytes)?;
    Ok(hash)
}

/// Read the blob identified by `hash` from the store.
pub fn read_blob(root: &Path, hash: &str) -> Result<Vec<u8>> {
    let path = blob_path(root, hash);
    std::fs::read(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::NotFound
        } else {
            Error::Io(e)
        }
    })
}

/// Delete the blob file for `hash`, if it exists on disk.
///
/// This is called only after confirming (within a transaction) that no
/// remaining `formats` row references this hash.
pub fn delete_blob(root: &Path, hash: &str) -> Result<()> {
    let path = blob_path(root, hash);
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(Error::Io(e)),
    }
    // Best-effort: remove the bucket directory if now empty.
    if let Some(dir) = path.parent() {
        let _ = std::fs::remove_dir(dir); // fails silently if not empty
    }
    Ok(())
}
