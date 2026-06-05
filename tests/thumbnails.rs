//! Integration tests for thumbnail generation.
//!
//! Each test gets its own isolated temp directories for both data and cache,
//! set via `RICHCLIP_DATA_DIR` and `RICHCLIP_CACHE_DIR` env vars.
//! No daemon is started — all operations go through the direct-DB path.

use assert_cmd::Command;
use image::{DynamicImage, ImageBuffer, ImageFormat, Rgba};
use std::io::Cursor;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Return a `richclip` Command with both data and cache dirs isolated.
/// Both TempDirs must be kept alive for the test duration.
fn cmd(data_dir: &TempDir, cache_dir: &TempDir) -> Command {
    let mut c = Command::cargo_bin("richclip").expect("richclip binary not found");
    c.env("RICHCLIP_DATA_DIR", data_dir.path());
    c.env("RICHCLIP_CACHE_DIR", cache_dir.path());
    c
}

/// Build a small solid-colour RGBA PNG in memory (200×150).
fn make_real_png() -> Vec<u8> {
    let buf: ImageBuffer<Rgba<u8>, _> =
        ImageBuffer::from_fn(200, 150, |_, _| Rgba([100u8, 149, 237, 255]));
    let img = DynamicImage::ImageRgba8(buf);
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, ImageFormat::Png)
        .expect("encode should succeed");
    out.into_inner()
}

/// Add an item with a real PNG format; return the trimmed UUID string.
fn add_png_item(data_dir: &TempDir, cache_dir: &TempDir, png_bytes: &[u8]) -> String {
    // Write the PNG to a temp file so we can pass it as @path.
    let png_file = data_dir.path().join("test.png");
    std::fs::write(&png_file, png_bytes).unwrap();
    let src_arg = format!("image/png=@{}", png_file.display());

    let out = cmd(data_dir, cache_dir)
        .args(["add", "--set-mime", &src_arg])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8(out).unwrap().trim().to_string()
}

/// Return `<cache_dir>/thumbs/<id>.png` path (mirrors `paths::thumb_path`).
fn thumb_path(cache_dir: &TempDir, id: &str) -> std::path::PathBuf {
    cache_dir.path().join("thumbs").join(format!("{id}.png"))
}

// ---------------------------------------------------------------------------
// Test 1: add image/png → thumb file exists and is a valid PNG ≤ 256px
// ---------------------------------------------------------------------------

#[test]
fn test_add_image_generates_thumbnail() {
    let data_dir = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();
    let png = make_real_png();

    let id = add_png_item(&data_dir, &cache_dir, &png);
    let thumb = thumb_path(&cache_dir, &id);

    assert!(
        thumb.exists(),
        "thumbnail file must exist after add: {thumb:?}"
    );

    // Verify it is a valid PNG.
    let thumb_bytes = std::fs::read(&thumb).expect("should be able to read thumb");
    let img = image::load_from_memory_with_format(&thumb_bytes, ImageFormat::Png)
        .expect("thumbnail must be a valid PNG");

    // Longest edge must be ≤ 256.
    assert!(
        img.width() <= 256 && img.height() <= 256,
        "thumbnail dimensions {}×{} exceed max_edge 256",
        img.width(),
        img.height()
    );

    // Source was 200×150 — original fits within 256 so should NOT be downscaled.
    assert_eq!(
        (img.width(), img.height()),
        (200, 150),
        "small source must not be upscaled"
    );
}

// ---------------------------------------------------------------------------
// Test 2: list --json includes thumbnail path when file exists
// ---------------------------------------------------------------------------

#[test]
fn test_list_json_thumbnail_field_image() {
    let data_dir = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();
    let png = make_real_png();

    let id = add_png_item(&data_dir, &cache_dir, &png);
    let expected_thumb = thumb_path(&cache_dir, &id);

    let list_out = cmd(&data_dir, &cache_dir)
        .args(["list", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let items: Vec<serde_json::Value> =
        serde_json::from_slice(&list_out).expect("list --json must be valid JSON");

    assert_eq!(items.len(), 1);
    let item = &items[0];

    // The thumbnail field must be the path string.
    let thumb_val = item["thumbnail"]
        .as_str()
        .expect("thumbnail must be a string for image items");
    assert_eq!(
        thumb_val,
        expected_thumb.to_string_lossy().as_ref(),
        "thumbnail path in JSON must match the expected path"
    );
}

// ---------------------------------------------------------------------------
// Test 3: text-only item → no thumb file and list --json thumbnail is null
// ---------------------------------------------------------------------------

#[test]
fn test_text_item_no_thumbnail() {
    let data_dir = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();

    // Add text-only item.
    let out = cmd(&data_dir, &cache_dir)
        .args(["add", "--set-mime", "text/plain=-"])
        .write_stdin("just some text")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let id = String::from_utf8(out).unwrap().trim().to_string();

    // No thumb file should exist.
    let thumb = thumb_path(&cache_dir, &id);
    assert!(
        !thumb.exists(),
        "no thumbnail must be created for text-only items"
    );

    // list --json thumbnail must be null.
    let list_out = cmd(&data_dir, &cache_dir)
        .args(["list", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let items: Vec<serde_json::Value> =
        serde_json::from_slice(&list_out).expect("list --json must be valid JSON");

    assert_eq!(items.len(), 1);
    let item = &items[0];
    assert!(
        item["thumbnail"].is_null(),
        "thumbnail must be null for text-only items, got {:?}",
        item["thumbnail"]
    );
}

// ---------------------------------------------------------------------------
// Test 4: `richclip thumbnail <image-id>` prints the path; file exists
// ---------------------------------------------------------------------------

#[test]
fn test_thumbnail_subcommand_image() {
    let data_dir = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();
    let png = make_real_png();

    // First add (which generates the thumb), then delete the thumb file so
    // we can test that `thumbnail` subcommand (re)generates it.
    let id = add_png_item(&data_dir, &cache_dir, &png);
    let thumb = thumb_path(&cache_dir, &id);

    // Remove the auto-generated file so the subcommand must regenerate it.
    if thumb.exists() {
        std::fs::remove_file(&thumb).unwrap();
    }
    assert!(
        !thumb.exists(),
        "thumb must be removed before subcommand test"
    );

    // Run `richclip thumbnail <id>` — must print the path.
    let out = cmd(&data_dir, &cache_dir)
        .args(["thumbnail", &id])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let printed = String::from_utf8(out).unwrap().trim().to_string();
    assert!(
        !printed.is_empty(),
        "thumbnail subcommand must print a path for an image item"
    );
    assert_eq!(
        printed,
        thumb.to_string_lossy().as_ref(),
        "printed path must match expected thumb path"
    );

    // File must now exist.
    assert!(
        thumb.exists(),
        "thumb file must exist after thumbnail subcommand"
    );

    // Verify it is a valid PNG.
    let bytes = std::fs::read(&thumb).unwrap();
    image::load_from_memory_with_format(&bytes, ImageFormat::Png)
        .expect("regenerated thumbnail must be a valid PNG");
}

// ---------------------------------------------------------------------------
// Test 5: `richclip thumbnail <text-id>` → prints nothing, exit 0
// ---------------------------------------------------------------------------

#[test]
fn test_thumbnail_subcommand_text_item() {
    let data_dir = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();

    let out = cmd(&data_dir, &cache_dir)
        .args(["add", "--set-mime", "text/plain=-"])
        .write_stdin("text content")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let id = String::from_utf8(out).unwrap().trim().to_string();

    // `richclip thumbnail <text-id>` — exit 0, no output.
    let thumb_out = cmd(&data_dir, &cache_dir)
        .args(["thumbnail", &id])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    assert!(
        thumb_out.is_empty(),
        "thumbnail subcommand must produce no output for text-only items"
    );
}

// ---------------------------------------------------------------------------
// Test 6: `richclip delete <image-id>` removes the thumb file
// ---------------------------------------------------------------------------

#[test]
fn test_delete_removes_thumbnail() {
    let data_dir = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();
    let png = make_real_png();

    let id = add_png_item(&data_dir, &cache_dir, &png);
    let thumb = thumb_path(&cache_dir, &id);

    // The thumbnail must exist before deleting.
    assert!(
        thumb.exists(),
        "thumbnail must exist before delete; got {thumb:?}"
    );

    // Delete the item.
    cmd(&data_dir, &cache_dir)
        .args(["delete", &id])
        .assert()
        .success();

    // Thumbnail must be removed.
    assert!(
        !thumb.exists(),
        "thumbnail must be removed after delete, but {thumb:?} still exists"
    );
}

// ---------------------------------------------------------------------------
// Test 7: large image is downscaled in the thumbnail
// ---------------------------------------------------------------------------

#[test]
fn test_large_image_thumbnail_is_downscaled() {
    let data_dir = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();

    // Create a 512×400 PNG (larger than max_edge=256 on both dimensions).
    let buf: ImageBuffer<Rgba<u8>, _> =
        ImageBuffer::from_fn(512, 400, |_, _| Rgba([200u8, 50, 100, 255]));
    let img = DynamicImage::ImageRgba8(buf);
    let mut png_out = Cursor::new(Vec::new());
    img.write_to(&mut png_out, ImageFormat::Png).unwrap();
    let png = png_out.into_inner();

    let id = add_png_item(&data_dir, &cache_dir, &png);
    let thumb = thumb_path(&cache_dir, &id);

    assert!(thumb.exists(), "thumbnail must exist after add");

    let bytes = std::fs::read(&thumb).unwrap();
    let decoded = image::load_from_memory_with_format(&bytes, ImageFormat::Png)
        .expect("thumbnail must be a valid PNG");

    assert!(
        decoded.width() <= 256 && decoded.height() <= 256,
        "thumbnail dimensions {}×{} must not exceed 256",
        decoded.width(),
        decoded.height()
    );
    // At least one edge should be exactly 256.
    assert!(
        decoded.width() == 256 || decoded.height() == 256,
        "at least one edge must equal 256 for a downscaled image, got {}×{}",
        decoded.width(),
        decoded.height()
    );
}
