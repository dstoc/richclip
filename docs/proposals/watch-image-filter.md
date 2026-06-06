# Proposal: image-aware watch filters

## Motivation

`richclip` already positions `watch` as the external enrichment hook for
labellers and OCR workers. The documented example watches `item-added` events
for `image/png` and then runs `decode -> label -> update`. That works for one
exact MIME, but real clipboard image capture already accepts multiple image
formats (`image/png`, `image/jpeg`, `image/webp`, `image/bmp`), and contributors
should not need to spawn one watcher per format or subscribe to every event and
re-implement image detection themselves.

## Problem statement

- `WatchArgs` in [src/bin/richclip.rs](/home/user/workspace/richclip-agent/src/bin/richclip.rs:152)
  exposes a single `--mime <MIME>` flag.
- `WatchEventsParams` in [src/ipc.rs](/home/user/workspace/richclip-agent/src/ipc.rs:61)
  carries exactly one `mime_filter: Option<String>`.
- The daemon applies that filter as an exact string match via
  `event_matches_mime` in
  [richclip-wayland/src/daemon.rs](/home/user/workspace/richclip-agent/richclip-wayland/src/daemon.rs:362).
- The documented async labelling flow in
  [README.md](/home/user/workspace/richclip-agent/README.md:93) therefore only
  scales cleanly for `image/png`, even though thumbnail generation already has a
  canonical image MIME priority order in
  [src/thumbnail.rs](/home/user/workspace/richclip-agent/src/thumbnail.rs:57).

The missing piece is not general event routing; it is a narrow way to express
"watch items that contain any supported image format".

## Proposal

Extend `watch` and the underlying IPC to support a small set of multi-MIME
filters, with image watching as the first caller.

### CLI shape

Make `--mime` repeatable:

```sh
richclip watch --json --event item-added --mime image/png --mime image/jpeg
```

Add a convenience flag:

```sh
richclip watch --json --event item-added --image
```

`--image` expands to the same supported-image set used by thumbnail generation:

```text
image/png
image/webp
image/jpeg
image/bmp
```

This keeps the first use case terse while still exposing a generic repeated MIME
mechanism for other contrib tools.

### IPC shape

Change `WatchEventsParams` from a single `mime_filter` to plural filters:

```rust
pub struct WatchEventsParams {
    pub event_filter: Option<String>,
    pub mime_filters: Vec<String>,
}
```

Semantics:

- empty `mime_filters` means "no MIME filtering"
- non-empty `mime_filters` means "pass if any requested MIME matches the event"

`event_filter` stays unchanged.

### Matching semantics

Preserve exact MIME matching. Do not add prefix wildcards such as `image/*` in
this change.

That is narrower and safer than introducing MIME pattern syntax because:

- the repo already has a concrete supported-image set
- exact matching preserves the current mental model
- repeated filters plus `--image` solve the real caller without inventing a new
  pattern language

For `ItemAdded`, the event passes when `formats` contains any requested MIME.
For `ItemUpdated`, it passes when `changed` contains any requested MIME. For
`ItemDeleted`, it continues to pass regardless of MIME filters because the event
does not carry format detail today.

### Backward compatibility

This is a source-level change inside the repo, not a stable external protocol
guarantee. The CLI remains backward-compatible for users because existing
invocations with one `--mime` still behave the same.

JSON event payloads are unchanged. Only the request-side watch parameters and
CLI parsing change.

### Documentation

Update:

- `README.md` watch examples to prefer `--image` for image labellers
- `docs/richclip.md` labeller examples to show the same

The old exact-MIME example can remain as a lower-level illustration.

## Non-goals

- No daemon-side concept of "image item" beyond the fixed supported MIME list.
- No MIME globbing or prefix syntax such as `image/*`.
- No changes to `list --mime`, `delete --mime`, or other exact-MIME commands.
- No new watch event fields.

## Verification

- `richclip watch --json --event item-added --mime image/png --mime image/jpeg`
  receives events for PNG and JPEG items, but not text-only items.
- `richclip watch --json --event item-added --image` receives events for PNG,
  JPEG, WebP, and BMP items, but not text-only items.
- `richclip watch --json --event item-added --mime image/png` preserves current
  behavior.
- `item-updated` filtering still works when one of the requested MIME types is
  added or removed.
- Existing watch JSON output is byte-for-byte unchanged for the same event.

## Success criteria

- [ ] `watch` accepts repeated `--mime`.
- [ ] `watch` accepts `--image` as a shorthand for the supported image MIME set.
- [ ] Daemon MIME filtering uses OR semantics across requested MIME types.
- [ ] Existing single-`--mime` invocations continue to behave the same.
- [ ] Docs use the new image-aware watch path for labeller examples.

## Suggested implementation shape

```text
src/bin/richclip.rs              parse repeated `--mime` plus `--image`
src/ipc.rs                       pluralize watch MIME filters
richclip-wayland/src/daemon.rs   match any requested MIME
tests/                           CLI/IPC coverage for repeated filters and --image
README.md, docs/richclip.md      update examples
```

## Future expansion

- If a second real caller appears for MIME families outside images, revisit a
  pattern syntax then.
- If `ItemDeleted` ever gains pre-delete format summaries, MIME filtering can be
  made strict for delete events too.
