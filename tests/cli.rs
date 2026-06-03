//! Integration tests for the `richclip` CLI.
//!
//! Each test gets its own isolated temp directory via `RICHCLIP_DATA_DIR` so
//! they are fully independent and can run in parallel.

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Return a Command bound to the `richclip` binary with a fresh data dir.
/// The TempDir is returned so the caller keeps it alive for the test lifetime.
fn cmd(dir: &TempDir) -> Command {
    let mut c = Command::cargo_bin("richclip").expect("richclip binary not found");
    c.env("RICHCLIP_DATA_DIR", dir.path());
    c
}

/// Add a single text/plain item via stdin; returns the trimmed id.
fn add_text_plain(dir: &TempDir, content: &str) -> String {
    let out = cmd(dir)
        .args(["add", "--set-mime", "text/plain=-"])
        .write_stdin(content.to_string())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8(out).unwrap().trim().to_string()
}

// ---------------------------------------------------------------------------
// Test 1: add (stdin) → list
// ---------------------------------------------------------------------------

#[test]
fn test_add_stdin_then_list() {
    let dir = TempDir::new().unwrap();
    let content = "hello richclip";

    // add returns a non-empty uuid-shaped id
    let id_out = cmd(&dir)
        .args(["add", "--set-mime", "text/plain=-"])
        .write_stdin(content)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let id = String::from_utf8(id_out).unwrap();
    let id = id.trim();
    assert!(!id.is_empty(), "id must be non-empty");
    // Basic UUID format check: 36 chars with hyphens
    assert_eq!(id.len(), 36, "id should be UUID length (36)");

    // list --json returns one item with that id and text/plain of right size
    let list_out = cmd(&dir)
        .args(["list", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let items: Vec<serde_json::Value> =
        serde_json::from_slice(&list_out).expect("list --json must be valid JSON");

    assert_eq!(items.len(), 1, "must be exactly one item");
    assert_eq!(items[0]["id"].as_str().unwrap(), id);

    let formats = items[0]["formats"].as_array().unwrap();
    let tp = formats
        .iter()
        .find(|f| f["mime"].as_str().unwrap() == "text/plain")
        .expect("must have text/plain format");
    assert_eq!(
        tp["size"].as_u64().unwrap(),
        content.len() as u64,
        "size must match content length"
    );
}

// ---------------------------------------------------------------------------
// Test 2: add (file) → decode roundtrip (binary bytes)
// ---------------------------------------------------------------------------

#[test]
fn test_add_file_decode_roundtrip() {
    let dir = TempDir::new().unwrap();

    // fake PNG-like binary content (includes non-UTF8 bytes)
    let binary_content: Vec<u8> = vec![
        0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n', // PNG magic
        0x00, 0x01, 0x02, 0xFF, 0xFE, 0x80, 0x7F, 0x00, // misc binary bytes
    ];

    let tmp_file = dir.path().join("fake.png");
    std::fs::write(&tmp_file, &binary_content).unwrap();

    let src_arg = format!("image/png=@{}", tmp_file.display());
    let id_out = cmd(&dir)
        .args(["add", "--set-mime", &src_arg])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let id = String::from_utf8(id_out).unwrap().trim().to_string();

    // decode returns exact bytes
    let decoded = cmd(&dir)
        .args(["decode", &id, "image/png"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    assert_eq!(decoded, binary_content, "decoded bytes must match file bytes exactly");
}

// ---------------------------------------------------------------------------
// Test 3: add --json returns parseable {"id": "<uuid>"}
// ---------------------------------------------------------------------------

#[test]
fn test_add_json_output() {
    let dir = TempDir::new().unwrap();

    let out = cmd(&dir)
        .args(["add", "--json", "--set-mime", "text/plain=-"])
        .write_stdin("test content")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let v: serde_json::Value = serde_json::from_slice(&out).expect("must be valid JSON");
    let id_str = v["id"].as_str().expect("must have 'id' string field");

    // Must parse as a UUID
    id_str
        .parse::<uuid::Uuid>()
        .expect("id must be a valid UUID");
}

// ---------------------------------------------------------------------------
// Test 4: formats lists all mimes with sizes
// ---------------------------------------------------------------------------

#[test]
fn test_formats_lists_all_mimes() {
    let dir = TempDir::new().unwrap();

    // Write temp file for the PNG format
    let png_bytes: Vec<u8> = vec![0x89, b'P', b'N', b'G', 0x00, 0x01, 0x02, 0x03];
    let png_file = dir.path().join("img.png");
    std::fs::write(&png_file, &png_bytes).unwrap();

    let png_arg = format!("image/png=@{}", png_file.display());
    let id_out = cmd(&dir)
        .args(["add", "--set-mime", "text/plain=-", "--set-mime", &png_arg])
        .write_stdin("alt text")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let id = String::from_utf8(id_out).unwrap().trim().to_string();

    // formats output: each line is "<mime padded to 40> <size right-justified to 10>"
    cmd(&dir)
        .args(["formats", &id])
        .assert()
        .success()
        .stdout(predicate::str::contains("text/plain"))
        .stdout(predicate::str::contains("image/png"));
}

// ---------------------------------------------------------------------------
// Test 5: update --set-mime replaces content
// ---------------------------------------------------------------------------

#[test]
fn test_update_set_mime_replace() {
    let dir = TempDir::new().unwrap();

    let id = add_text_plain(&dir, "v1");

    // update text/plain to "v2"
    cmd(&dir)
        .args(["update", &id, "--set-mime", "text/plain=-"])
        .write_stdin("v2")
        .assert()
        .success();

    // decode now returns "v2"
    let decoded = cmd(&dir)
        .args(["decode", &id, "text/plain"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_eq!(decoded, b"v2", "decode must return updated content");
}

// ---------------------------------------------------------------------------
// Test 6: update --remove-mime removes one format
// ---------------------------------------------------------------------------

#[test]
fn test_update_remove_mime() {
    let dir = TempDir::new().unwrap();

    // Add item with two formats
    let png_bytes: Vec<u8> = vec![0x89, b'P', b'N', b'G'];
    let png_file = dir.path().join("img.png");
    std::fs::write(&png_file, &png_bytes).unwrap();
    let png_arg = format!("image/png=@{}", png_file.display());

    let id_out = cmd(&dir)
        .args(["add", "--set-mime", "text/plain=-", "--set-mime", &png_arg])
        .write_stdin("has two formats")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let id = String::from_utf8(id_out).unwrap().trim().to_string();

    // Remove image/png
    cmd(&dir)
        .args(["update", &id, "--remove-mime", "image/png"])
        .assert()
        .success();

    // formats should only show text/plain
    cmd(&dir)
        .args(["formats", &id])
        .assert()
        .success()
        .stdout(predicate::str::contains("text/plain"))
        .stdout(predicate::str::contains("image/png").not());

    // decode of removed mime → exit 2
    cmd(&dir)
        .args(["decode", &id, "image/png"])
        .assert()
        .code(2);
}

// ---------------------------------------------------------------------------
// Test 7: blob dedup — two items with identical bytes share blob_hash
// ---------------------------------------------------------------------------

#[test]
fn test_blob_dedup() {
    let dir = TempDir::new().unwrap();

    let shared = "identical content for dedup test";

    let id1_out = cmd(&dir)
        .args(["add", "--set-mime", "text/plain=-"])
        .write_stdin(shared)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let id1 = String::from_utf8(id1_out).unwrap().trim().to_string();

    let id2_out = cmd(&dir)
        .args(["add", "--set-mime", "text/plain=-"])
        .write_stdin(shared)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let id2 = String::from_utf8(id2_out).unwrap().trim().to_string();

    // inspect both items and compare blob_hash
    let inspect1_out = cmd(&dir)
        .args(["inspect", &id1])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let inspect2_out = cmd(&dir)
        .args(["inspect", &id2])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let v1: serde_json::Value = serde_json::from_slice(&inspect1_out).unwrap();
    let v2: serde_json::Value = serde_json::from_slice(&inspect2_out).unwrap();

    let hash1 = v1["formats"][0]["blob_hash"].as_str().unwrap();
    let hash2 = v2["formats"][0]["blob_hash"].as_str().unwrap();

    assert_eq!(hash1, hash2, "identical content must share the same blob_hash");
    assert!(!hash1.is_empty(), "blob_hash must not be empty");
}

// ---------------------------------------------------------------------------
// Test 8: delete item → decode/inspect exit 2, list is empty
// ---------------------------------------------------------------------------

#[test]
fn test_delete_item_cleanup() {
    let dir = TempDir::new().unwrap();

    let id = add_text_plain(&dir, "to be deleted");

    // delete the item
    cmd(&dir)
        .args(["delete", &id])
        .assert()
        .success();

    // decode → exit 2
    cmd(&dir)
        .args(["decode", &id, "text/plain"])
        .assert()
        .code(2);

    // inspect → exit 2
    cmd(&dir)
        .args(["inspect", &id])
        .assert()
        .code(2);

    // list --json returns []
    let list_out = cmd(&dir)
        .args(["list", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let items: Vec<serde_json::Value> = serde_json::from_slice(&list_out).unwrap();
    assert!(items.is_empty(), "list must be empty after delete");
}

// ---------------------------------------------------------------------------
// Test 9: delete --mime removes only that one format
// ---------------------------------------------------------------------------

#[test]
fn test_delete_mime() {
    let dir = TempDir::new().unwrap();

    // Add item with two formats
    let png_bytes: Vec<u8> = vec![0x89, b'P', b'N', b'G', 0x00, 0x01];
    let png_file = dir.path().join("img.png");
    std::fs::write(&png_file, &png_bytes).unwrap();
    let png_arg = format!("image/png=@{}", png_file.display());

    let id_out = cmd(&dir)
        .args(["add", "--set-mime", "text/plain=-", "--set-mime", &png_arg])
        .write_stdin("two format item")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let id = String::from_utf8(id_out).unwrap().trim().to_string();

    // delete only image/png
    cmd(&dir)
        .args(["delete", &id, "--mime", "image/png"])
        .assert()
        .success();

    // text/plain must still be decodeable
    cmd(&dir)
        .args(["decode", &id, "text/plain"])
        .assert()
        .success()
        .stdout(predicate::eq("two format item".as_bytes()));

    // image/png must be gone → exit 2
    cmd(&dir)
        .args(["decode", &id, "image/png"])
        .assert()
        .code(2);
}

// ---------------------------------------------------------------------------
// Test 10: delete --older-than deletes items and list becomes empty
// ---------------------------------------------------------------------------

#[test]
fn test_delete_older_than() {
    let dir = TempDir::new().unwrap();

    let id1 = add_text_plain(&dir, "item one");
    let id2 = add_text_plain(&dir, "item two");
    let _ = (id1, id2); // suppress unused warnings

    // delete --older-than 0s: cutoff = now - 0 = now.
    // Items were created fractionally before "now", so created_at < cutoff.
    let delete_out = cmd(&dir)
        .args(["delete", "--older-than", "0s", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let v: serde_json::Value = serde_json::from_slice(&delete_out).unwrap();
    let deleted = v["deleted"].as_u64().unwrap();
    assert!(deleted >= 2, "must have deleted at least the 2 items we added, got {deleted}");

    // list must now be empty
    let list_out = cmd(&dir)
        .args(["list", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let items: Vec<serde_json::Value> = serde_json::from_slice(&list_out).unwrap();
    assert!(items.is_empty(), "list must be empty after delete --older-than");
}

// ---------------------------------------------------------------------------
// Test 11: derived label from application/x-richclip-label
// ---------------------------------------------------------------------------

#[test]
fn test_derived_label_richclip_label() {
    let dir = TempDir::new().unwrap();

    // Add item with text/plain and application/x-richclip-label
    let id_out = cmd(&dir)
        .args(["add", "--set-mime", "text/plain=-"])
        .write_stdin("background text")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let id = String::from_utf8(id_out).unwrap().trim().to_string();

    // Set the richclip label
    cmd(&dir)
        .args(["update", &id, "--set-mime", "application/x-richclip-label=-"])
        .write_stdin("screenshot of rust panic")
        .assert()
        .success();

    let list_out = cmd(&dir)
        .args(["list", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let items: Vec<serde_json::Value> = serde_json::from_slice(&list_out).unwrap();
    let label = items[0]["label"].as_str().expect("label must be a string");
    assert_eq!(label, "screenshot of rust panic");
}

// ---------------------------------------------------------------------------
// Test 12: label fallback — text/plain snippet used as label
// ---------------------------------------------------------------------------

#[test]
fn test_label_fallback_text_plain() {
    let dir = TempDir::new().unwrap();

    let id = add_text_plain(&dir, "some copied text");

    let list_out = cmd(&dir)
        .args(["list", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let items: Vec<serde_json::Value> = serde_json::from_slice(&list_out).unwrap();
    let item = items.iter().find(|i| i["id"].as_str().unwrap() == id).unwrap();
    let label = item["label"].as_str().expect("label must be non-null");
    assert!(!label.is_empty(), "label must be non-empty");
    // The label should be a snippet of the text/plain content
    assert!(
        "some copied text".starts_with(label) || label.starts_with("some copied text"),
        "label '{label}' must be a snippet of the text/plain content"
    );
}

// ---------------------------------------------------------------------------
// Test 13: error contract (human) — bogus uuid → exit 2, "not found" on stderr
// ---------------------------------------------------------------------------

#[test]
fn test_error_contract_human_not_found() {
    let dir = TempDir::new().unwrap();

    cmd(&dir)
        .args(["decode", "00000000-0000-0000-0000-000000000000", "text/plain"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("not found"));
}

// ---------------------------------------------------------------------------
// Test 14: error contract (json) — bogus uuid update --json → exit 2,
//          stderr is JSON with code == "not_found"
// ---------------------------------------------------------------------------

#[test]
fn test_error_contract_json_not_found() {
    let dir = TempDir::new().unwrap();

    let err_out = cmd(&dir)
        .args([
            "update",
            "00000000-0000-0000-0000-000000000000",
            "--set-mime",
            "text/plain=-",
            "--json",
        ])
        .write_stdin("some stdin")
        .assert()
        .code(2)
        .get_output()
        .stderr
        .clone();

    let v: serde_json::Value =
        serde_json::from_slice(&err_out).expect("--json error output must be valid JSON");
    assert_eq!(
        v["code"].as_str().unwrap(),
        "not_found",
        "code must be 'not_found'"
    );
    assert!(
        v["error"].as_str().is_some(),
        "must have an 'error' message field"
    );
}

// ---------------------------------------------------------------------------
// Test 15: inspect shape — all required fields present
// ---------------------------------------------------------------------------

#[test]
fn test_inspect_shape() {
    let dir = TempDir::new().unwrap();

    let id = add_text_plain(&dir, "inspect me");

    let inspect_out = cmd(&dir)
        .args(["inspect", &id])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let v: serde_json::Value =
        serde_json::from_slice(&inspect_out).expect("inspect must emit valid JSON");

    assert_eq!(v["id"].as_str().unwrap(), id, "id must match");
    assert!(
        v["created_at"].as_str().is_some(),
        "must have created_at string"
    );
    assert!(
        v["updated_at"].as_str().is_some(),
        "must have updated_at string"
    );

    let formats = v["formats"].as_array().expect("must have formats array");
    assert!(!formats.is_empty(), "formats must be non-empty");

    let fmt = &formats[0];
    assert!(fmt["mime"].as_str().is_some(), "format must have mime");
    assert!(fmt["size"].as_u64().is_some(), "format must have size");
    assert!(
        fmt["blob_hash"].as_str().is_some(),
        "format must have blob_hash"
    );
    assert!(
        !fmt["blob_hash"].as_str().unwrap().is_empty(),
        "blob_hash must not be empty"
    );
}

// ---------------------------------------------------------------------------
// Bonus: derive_label multibyte truncation does not panic
// ---------------------------------------------------------------------------

#[test]
fn test_derive_label_multibyte_no_panic() {
    let dir = TempDir::new().unwrap();

    // Build a string of 70 multi-byte characters (each '€' is 3 bytes in UTF-8)
    // This would previously panic on a byte-index truncation at position 60.
    let long_multibyte: String = "€".repeat(70);
    assert!(long_multibyte.len() > 60, "must be >60 bytes");
    assert_eq!(long_multibyte.chars().count(), 70, "must be 70 chars");

    let id = add_text_plain(&dir, &long_multibyte);

    // list --json must succeed without panic
    let list_out = cmd(&dir)
        .args(["list", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let items: Vec<serde_json::Value> = serde_json::from_slice(&list_out).unwrap();
    let item = items.iter().find(|i| i["id"].as_str().unwrap() == id).unwrap();
    let label = item["label"].as_str().expect("label must be non-null");

    // Must be exactly 60 chars (truncated at char boundary, not byte boundary)
    assert_eq!(
        label.chars().count(),
        60,
        "label must be truncated to exactly 60 characters"
    );
    // Must be valid UTF-8 (no panic means we got here; this is a sanity check)
    assert_eq!(label, "€".repeat(60));
}
