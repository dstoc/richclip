# Proposal: thumbnail generation for image items

## Motivation

`docs/richclip.md` reserves a cache location for per-item thumbnails:

```text
$XDG_CACHE_HOME/richclip/thumbs/<item-id>.webp
```

and the "Picker integration" section anticipates "a richer picker can show
thumbnails". Nothing generates them yet. A GUI/dmenu picker that wants a visual
preview for image items currently has only the full-size `image/png` blob, which
is wasteful to decode and downscale on every list render.

## Problem statement

- Image selections are stored as a full-resolution `image/png` (or `image/jpeg`,
  `image/webp`, `image/bmp`) blob under the content-addressed blob store. There
  is no small preview artifact.
- `src/paths.rs` exposes `default_data_dir()` and `default_socket_path()` only —
  there is **no cache-dir helper**, so nothing can resolve the `thumbs/` path the
  design already names.
- `list --json` (`ListItemEntry` in `src/bin/richclip.rs`) carries
  `id, created_at, label, formats[]` but no thumbnail path, so a picker has no
  way to discover a preview even if one existed.
- The daemon's capture consumer in `richclip-wayland/src/bin/richclipd.rs`
  already runs once per captured item (it calls `store.add_item` then broadcasts
  `ItemAdded`) — the obvious generation hook — but does nothing image-aware.

## Proposal

Generate a small preview for any item that has an `image/*` format, store it in
the cache directory, and surface its path through `list --json`. Generation is
**best-effort and never blocks capture or a CLI mutation**.

### Output format: PNG, not WebP

The design text says `.webp`. Reliable WebP *encoding* in Rust currently means
the `webp` crate (libwebp-sys, a C dependency needing a system/vendored
libwebp); the pure-Rust `image` crate is decode-only for WebP in recent
versions. To stay consistent with this project's "pure-Rust, builds offline"
posture (cf. `rusqlite` is `bundled`, and the toolchain pin in
`Cargo.toml`), thumbnails are encoded as **PNG**:

```text
$XDG_CACHE_HOME/richclip/thumbs/<item-id>.png
```

PNG thumbnails at 256px are small enough for a picker and need no C toolchain.
Switching to WebP later is a localized change behind the same path helper (see
Future expansion).

### Cache path helpers (`src/paths.rs`)

```rust
/// `$XDG_CACHE_HOME/richclip` (falls back to ~/.cache/richclip).
pub fn default_cache_dir() -> Result<PathBuf>;

/// `<cache>/thumbs/<item-id>.png`
pub fn thumb_path(cache_dir: &Path, id: Uuid) -> PathBuf;
```

`default_cache_dir()` uses the existing `directories::ProjectDirs::cache_dir()`,
mirroring `default_data_dir()`.

### Pure thumbnailing function (new lib module `src/thumbnail.rs`)

Keep the image work pure and unit-testable, independent of the store/daemon:

```rust
/// Decode `image_bytes`, downscale so the longest edge is <= `max_edge`
/// (preserving aspect ratio; never upscale), and re-encode as PNG.
pub fn make_thumbnail(image_bytes: &[u8], max_edge: u32) -> Result<Vec<u8>>;
```

Implemented with the `image` crate (`image::load_from_memory` →
`imageops::thumbnail`/`resize` → `ImageFormat::Png`). Default `max_edge = 256`.

### Store-aware helper

```rust
/// Pick an item's best image format, generate a thumbnail, write it to
/// `thumb_path`, and return that path. Returns Ok(None) if the item has no
/// image format. Errors are returned, not panicked.
pub fn generate_item_thumbnail(store: &Store, cache_dir: &Path, id: Uuid)
    -> Result<Option<PathBuf>>;
```

"Best image format" = first of `image/png, image/webp, image/jpeg, image/bmp`
present on the item (decode coverage of the `image` crate). The internal
`application/x-richclip-*` namespace is ignored.

### Generation hook points

1. **Daemon capture** — in the `richclipd.rs` capture consumer, after a
   successful `add_item`, call `generate_item_thumbnail` on a blocking task
   (`spawn_blocking`); on error, `tracing::warn!` and continue. The
   `ItemAdded` broadcast is unaffected.
2. **Manual add** — `richclip add` (Phase 1 path, no daemon) generates the
   thumbnail inline after creating the item, best-effort (a warning to stderr on
   failure, still exit 0).
3. **Backfill / on-demand** — a new CLI command:
   ```sh
   richclip thumbnail <id>     # (re)generate; prints the thumb path, or nothing if not an image
   ```

### Surfacing in `list --json`

Add an optional field to `ListItemEntry`, populated only when the file exists:

```json
{
  "id": "0190a1b2-...",
  "created_at": "...",
  "label": "screenshot of rust panic",
  "thumbnail": "/home/u/.cache/richclip/thumbs/0190a1b2-....png",
  "formats": [{"mime": "image/png", "size": 184221}]
}
```

`thumbnail` is `null`/absent for non-image items or when generation never ran.
`list` does **not** generate thumbnails on read — it only reports existing ones.

### Cleanup

On `delete <id>` / `delete_item`, best-effort remove `thumb_path(id)`. Thumbnails
are disposable cache, so a missed deletion is harmless (and `delete --older-than`
may leave orphans — acceptable; a future `richclip thumbnail --prune` can sweep).

## Non-goals

- No WebP/AVIF encoding, no animated or video previews.
- No full-size image re-caching or format conversion of stored blobs — original
  bytes are never modified.
- No thumbnail generation for non-image MIME types (no text rendering, no PDF
  rasterization).
- `list` will not synchronously generate thumbnails; generation stays on the
  capture/add/backfill paths so listing is never slow or side-effecting.
- No eviction policy beyond delete-time cleanup in this proposal.

## Dependencies

Adds the `image` crate (default features trimmed to the needed codecs: `png`,
`jpeg`, `webp`, `bmp`) to the `richclip` crate, ideally behind a default-on
`thumbnails` cargo feature so the dependency can be compiled out. Adding it must
go through the gated workflow (`run-with-network -- cargo fetch`) and pick a
version that builds on the pinned toolchain (rustc 1.93.1).

## Verification

- Capture (or `add`) an image item → `thumbs/<id>.png` exists, decodes to a PNG
  whose longest edge is ≤ 256px and ≤ the original's edge.
- A text-only item produces no thumb file and `list --json` shows
  `thumbnail: null`.
- `richclip thumbnail <id>` on an image item prints a path and (re)creates the
  file; on a text item it exits 0 and prints nothing.
- Feeding `make_thumbnail` corrupt/non-image bytes returns `Err`, and the daemon
  logs a warning and keeps serving (no panic, item still stored) — assert via a
  unit test for `make_thumbnail` and a daemon-path test with a bogus image blob.
- `delete <id>` removes the corresponding thumb file when present.

## Success criteria

- [ ] `paths::default_cache_dir()` and `paths::thumb_path()` exist and respect
      `XDG_CACHE_HOME`.
- [ ] `thumbnail::make_thumbnail` is pure and unit-tested (size + format).
- [ ] Image items get a `thumbs/<id>.png` via daemon capture and via `add`.
- [ ] `richclip thumbnail <id>` regenerates on demand.
- [ ] `list --json` includes `thumbnail` (path or null) without generating.
- [ ] Generation failure never aborts capture/add (best-effort, warns only).

## Suggested implementation shape

```text
src/paths.rs        + default_cache_dir(), thumb_path()
src/thumbnail.rs    + make_thumbnail() (pure), generate_item_thumbnail(store,…)
src/lib.rs          pub mod thumbnail;  (behind `thumbnails` feature)
src/bin/richclip.rs + `thumbnail` subcommand; call generate on `add`; add
                      `thumbnail` to ListItemEntry
richclip-wayland/src/bin/richclipd.rs
                    capture consumer: spawn_blocking generate_item_thumbnail,
                    warn on error
```

## Future expansion

- Optional WebP output behind a `webp` feature using the `webp` crate, switching
  only the encoder and the path extension.
- `richclip thumbnail --prune` to drop thumbs with no backing item.
- Configurable `max_edge` via the daemon config / an env var.
