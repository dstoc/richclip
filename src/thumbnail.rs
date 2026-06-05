//! Pure thumbnail generation for image clipboard items.
//!
//! This module is intentionally free of filesystem and store access so that it
//! can be unit-tested in isolation (the pure `make_thumbnail` function).
//! The `generate_item_thumbnail` store-aware helper lives here too.

use crate::error::{Error, Result};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Default longest edge (pixels) for generated thumbnails.
pub const DEFAULT_MAX_EDGE: u32 = 256;

/// Decode `image_bytes` (PNG / JPEG / WebP / BMP), downscale so the longest
/// edge is `<= max_edge` while preserving the aspect ratio, and re-encode the
/// result as PNG bytes.
///
/// Images that are already small enough (both dimensions `<= max_edge`) are
/// returned unmodified in encoding only (re-encoded to PNG) without any resize
/// step, so this function **never upscales**.
///
/// Errors (undecodable input, encode failure) are returned as [`Error::Other`];
/// the function never panics on bad input.
pub fn make_thumbnail(image_bytes: &[u8], max_edge: u32) -> Result<Vec<u8>> {
    let img = image::load_from_memory(image_bytes)
        .map_err(|e| Error::Other(format!("thumbnail decode: {e}")))?;

    // Only resize when at least one dimension exceeds max_edge.
    let resized = if img.width() > max_edge || img.height() > max_edge {
        // `DynamicImage::thumbnail(w, h)` scales so the longest edge fits
        // within the given bounds while preserving the aspect ratio.  It uses
        // `resize_dimensions` internally with `fill = false`, which picks the
        // *minimum* of the two ratios — i.e., it can upscale when both ratios
        // are > 1.  We guard against that above, so by the time we reach here
        // at least one dimension is already > max_edge, guaranteeing a
        // downscale-only operation.
        img.thumbnail(max_edge, max_edge)
    } else {
        img
    };

    let mut buf = Cursor::new(Vec::new());
    resized
        .write_to(&mut buf, image::ImageFormat::Png)
        .map_err(|e| Error::Other(format!("thumbnail encode: {e}")))?;

    Ok(buf.into_inner())
}

// ---------------------------------------------------------------------------
// Image MIME priority for picking the best format to thumbnail
// ---------------------------------------------------------------------------

/// Best image format priority order for thumbnail generation.
/// Formats tried in order; first one present on the item wins.
const IMAGE_MIME_PRIORITY: [&str; 4] = ["image/png", "image/webp", "image/jpeg", "image/bmp"];

// ---------------------------------------------------------------------------
// Store-aware helper
// ---------------------------------------------------------------------------

/// Pick the best image format for `id`, generate a thumbnail with
/// [`make_thumbnail`], write it to `paths::thumb_path(cache_dir, id)`, and
/// return that path.
///
/// Returns `Ok(None)` if the item has no image format in
/// `IMAGE_MIME_PRIORITY`.  Errors from the store or filesystem are returned
/// as `Err`.  Creates `<cache_dir>/thumbs/` as needed.
pub fn generate_item_thumbnail(
    store: &crate::Store,
    cache_dir: &Path,
    id: Uuid,
) -> Result<Option<PathBuf>> {
    // 1. Fetch the item's formats and pick the best image mime.
    let formats = store.formats(id)?;
    let mime = IMAGE_MIME_PRIORITY
        .iter()
        .find(|&&m| formats.iter().any(|f| f.mime == m))
        .copied();

    let mime = match mime {
        Some(m) => m,
        None => return Ok(None), // no image format on this item
    };

    // 2. Decode and thumbnail.
    let bytes = store.decode(id, mime)?;
    let png = make_thumbnail(&bytes, DEFAULT_MAX_EDGE)?;

    // 3. Write thumbnail to cache dir.
    let path = crate::paths::thumb_path(cache_dir, id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, &png)?;

    Ok(Some(path))
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, ImageBuffer, ImageFormat, Rgba};
    use std::io::Cursor;

    /// Encode a `DynamicImage` to an in-memory buffer using the given format.
    fn encode_image(img: &DynamicImage, fmt: ImageFormat) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        img.write_to(&mut buf, fmt).expect("encode should succeed");
        buf.into_inner()
    }

    /// Decode PNG bytes and return the resulting `DynamicImage`.
    fn decode_png(bytes: &[u8]) -> DynamicImage {
        image::load_from_memory_with_format(bytes, ImageFormat::Png)
            .expect("result should be a valid PNG")
    }

    /// Build an RGBA image filled with a solid colour.
    fn solid_rgba(width: u32, height: u32, r: u8, g: u8, b: u8) -> DynamicImage {
        let buf: ImageBuffer<Rgba<u8>, _> =
            ImageBuffer::from_fn(width, height, |_, _| Rgba([r, g, b, 255]));
        DynamicImage::ImageRgba8(buf)
    }

    // -------------------------------------------------------------------------
    // Large image: 800×600 → should be downscaled, longest edge == 256.
    // -------------------------------------------------------------------------
    #[test]
    fn large_image_is_downscaled() {
        let src = solid_rgba(800, 600, 200, 100, 50);
        let src_png = encode_image(&src, ImageFormat::Png);

        let thumb_bytes = make_thumbnail(&src_png, 256).expect("should succeed");
        let thumb = decode_png(&thumb_bytes);

        let (tw, th) = (thumb.width(), thumb.height());
        // Longest edge must be <= 256.
        assert!(
            tw <= 256 && th <= 256,
            "dimensions {tw}x{th} exceed max_edge"
        );
        // At least one edge should be exactly 256 (it's the longest edge of the
        // result when we hit the bound).
        assert!(
            tw == 256 || th == 256,
            "expected longest edge == 256, got {tw}x{th}"
        );
        // Dimensions must be strictly smaller than the original.
        assert!(
            tw < 800 && th < 600,
            "thumbnail was not smaller than original"
        );
        // Aspect ratio preserved within rounding: original is 4:3.
        let orig_ratio = 800.0_f64 / 600.0;
        let thumb_ratio = tw as f64 / th as f64;
        assert!(
            (orig_ratio - thumb_ratio).abs() < 0.02,
            "aspect ratio drifted: orig={orig_ratio:.4} thumb={thumb_ratio:.4}"
        );
    }

    // -------------------------------------------------------------------------
    // Small image: 100×80 → must NOT be upscaled.
    // -------------------------------------------------------------------------
    #[test]
    fn small_image_is_not_upscaled() {
        let src = solid_rgba(100, 80, 0, 128, 255);
        let src_png = encode_image(&src, ImageFormat::Png);

        let thumb_bytes = make_thumbnail(&src_png, 256).expect("should succeed");
        let thumb = decode_png(&thumb_bytes);

        assert_eq!(
            (thumb.width(), thumb.height()),
            (100, 80),
            "small image should not be upscaled"
        );
    }

    // -------------------------------------------------------------------------
    // Non-square wide image: 400×100 → longest edge becomes 256, shorter
    // edge scales proportionally (~64).
    // -------------------------------------------------------------------------
    #[test]
    fn wide_image_scales_proportionally() {
        let src = solid_rgba(400, 100, 50, 200, 10);
        let src_png = encode_image(&src, ImageFormat::Png);

        let thumb_bytes = make_thumbnail(&src_png, 256).expect("should succeed");
        let thumb = decode_png(&thumb_bytes);

        let (tw, th) = (thumb.width(), thumb.height());
        // Longest edge is width — must now be 256.
        assert_eq!(tw, 256, "longest edge should be 256, got {tw}");
        // Shorter edge: 100 * (256/400) = 64.
        assert!((th as i64 - 64).abs() <= 1, "expected height ~64, got {th}");
    }

    // -------------------------------------------------------------------------
    // Garbage bytes → must return Err and not panic.
    // -------------------------------------------------------------------------
    #[test]
    fn garbage_bytes_return_err() {
        let result = make_thumbnail(b"not an image", 256);
        assert!(result.is_err(), "expected Err for non-image bytes");
        // Verify the error message mentions "thumbnail decode".
        if let Err(Error::Other(msg)) = result {
            assert!(
                msg.contains("thumbnail decode"),
                "unexpected error message: {msg}"
            );
        }
    }

    // -------------------------------------------------------------------------
    // JPEG input: confirm multi-format decode works.
    // -------------------------------------------------------------------------
    #[test]
    fn jpeg_input_is_accepted() {
        let src = solid_rgba(320, 240, 100, 150, 200);
        // DynamicImage::write_to with Jpeg requires an Rgb8 image (no alpha).
        let src_rgb = DynamicImage::ImageRgb8(src.to_rgb8());
        let src_jpeg = encode_image(&src_rgb, ImageFormat::Jpeg);

        let thumb_bytes = make_thumbnail(&src_jpeg, 256).expect("should succeed for JPEG input");
        let thumb = decode_png(&thumb_bytes);

        // 320 > 256, so longest edge must be <= 256.
        assert!(
            thumb.width() <= 256 && thumb.height() <= 256,
            "JPEG thumbnail dimensions {}x{} exceed max_edge",
            thumb.width(),
            thumb.height()
        );
    }
}
