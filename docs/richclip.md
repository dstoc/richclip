# Proposal: rich Wayland clipboard history manager in Rust

Build a clipboard history system that stores each clipboard selection as a **grouped item** containing all offered MIME representations, plus generated enrichment formats (labels, OCR, search text, thumbnails) used for search and picker display.

The key difference from `cliphist` is:

```text
cliphist entry:
  one blob

new entry:
  stable item id
  many MIME blobs (captured + generated)
  thumbnail/cache data
  updateable
```

Wayland supports this model: a clipboard offer advertises multiple MIME types, and receivers can request specific types over file descriptors; the protocol explicitly allows receive requests for different MIME types. `wl-copy` is not enough for faithful restore because its man page says it currently cannot copy data in multiple MIME types at the same time. ([wayland.freedesktop.org][1]) ([man.archlinux.org][2])

---

## Goals

The system should support:

```text
capture clipboard item
  enumerate MIME types
  read selected MIME types
  group them under one item id

restore item
  advertise all stored MIME types
  lazily serve the requested format

modify item
  add MIME type
  remove MIME type
  replace MIME type
  edit labels/enrichment formats

query item
  list/search history
  inspect formats
  feed picker UI

watch item changes
  let an external labeller react to new entries
  update entries after async processing
```

Target environment: **Sway/wlroots first**, using `wlr-data-control`. The `zwlr_data_control_offer_v1.receive` API is the practical capture path for clipboard-manager-style clients on wlroots compositors. ([wayland.app][3])

---

## Architecture

```text
richclipd
  Wayland data-control client
  captures clipboard offers
  owns/restores clipboard selections
  writes item records and blobs
  emits events over unix socket

richclip
  CLI frontend
  add/list/update/delete/watch/restore/decode
  reads DB directly (list/formats/inspect/decode)
  routes mutations through the daemon when it is running

store
  sqlite item/format index DB
  content-addressed blob directory
  thumbnail/cache directory
```

Suggested paths:

```text
$XDG_RUNTIME_DIR/richclip.sock
$XDG_DATA_HOME/richclip/db.sqlite
$XDG_DATA_HOME/richclip/blobs/aa/bbcc...
$XDG_CACHE_HOME/richclip/thumbs/<item-id>.webp
```

---

## Data model

Core concept: an `Item` is a grouped clipboard selection. A `Format` is one MIME representation within that item.

```rust
struct Item {
    id: Uuid,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
}

struct Format {
    item_id: Uuid,
    mime: String,
    blob_hash: String,
    size: u64,
    captured_at: OffsetDateTime,
}
```

`Item` is pure identity and lifecycle — nothing else. All content lives in
`Format` rows. That includes generated enrichment: a label, OCR text, or any
searchable string is just another format under a custom MIME type
(`application/x-richclip-label`, `application/x-richclip-ocr`, ...). There is
no separate label/title/search column and no key/value metadata table.

`id` is a **UUIDv7** (time-ordered), so `ORDER BY id DESC` yields newest-first
history with no separate index. Timestamps are stored as **epoch
milliseconds** (`INTEGER`).

There is no role enum and no per-format "offer" flag. The MIME type itself
decides what gets advertised on restore:

```text
standard MIME types (text/plain, image/png, ...)
  real clipboard data, captured from an offer
  advertised on restore

custom MIME types (application/x-richclip-*)
  generated/derived/internal data we attach ourselves
  stored but never advertised on restore
```

So restore advertises every format whose MIME is not in the
`application/x-richclip-*` namespace.

SQLite shape:

```sql
CREATE TABLE items (
  id TEXT PRIMARY KEY,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);

CREATE TABLE formats (
  item_id TEXT NOT NULL,
  mime TEXT NOT NULL,
  blob_hash TEXT NOT NULL,
  size INTEGER NOT NULL,
  captured_at INTEGER NOT NULL,
  PRIMARY KEY (item_id, mime),
  FOREIGN KEY (item_id) REFERENCES items(id)
);
```

Content-addressed blobs:

```text
blob_hash = blake3(bytes)
```

Identical bytes dedupe to one blob, while each `(item_id, mime)` row keeps the
MIME grouping.

Deletion is immediate and complete: removing an item (or a single format)
deletes its `formats` rows and then deletes any blob no longer referenced by
another format. There is no soft-delete tombstone and no deferred GC pass —
cleanup happens inline with the delete. A blob is removable once no `formats`
row points at its `blob_hash`.

---

## CLI proposal

Use one binary:

```sh
richclip <command>
```

### `list`

Human display by default, JSON for scripts.

```sh
richclip list
richclip list --json
richclip list --limit 50
richclip list --mime image/png
```

Example human output:

```text
0190a1b2-...  2m ago   image/png, text/plain   screenshot of rust panic
0190a1a0-...  8m ago   text/html, text/plain   copied web fragment
```

JSON should be stable and script-friendly:

```json
{
  "id": "0190a1b2-...",
  "created_at": "2026-06-03T12:34:56+10:00",
  "label": "screenshot of rust panic",
  "formats": [
    {"mime": "image/png", "size": 184221},
    {"mime": "application/x-richclip-label", "size": 24}
  ]
}
```

`label` is a **derived convenience field**, not stored on the item: `list`
computes it by decoding the `application/x-richclip-label` format (falling back
to a snippet of `text/plain`). The same string drives the human list's label
column. There is no label column on the item itself.

### `add`

Create a new item from one or more formats. This is how data enters the store
without Wayland capture (manual import, scripts, Phase 1 testing).

```sh
richclip add --set-mime image/png=@foo.png
# -> 0190a1b2-...

richclip add --json --set-mime text/plain=@note.txt
# -> {"id":"0190a1b2-..."}

cat foo.png | richclip add --set-mime image/png=-
```

`add` prints the new item id to stdout (plain id by default, `{"id":...}` with
`--json`) so scripts can capture it and follow up with `update`. It accepts the
same `--set-mime` forms as `update`.

### `update`

This is the main editing API for existing items.

```sh
richclip update <id> --set-mime image/png=@new.png
richclip update <id> --remove-mime text/html
richclip update <id> --set-mime application/x-richclip-label=@label.txt
echo "screenshot of rust panic" | richclip update <id> --set-mime application/x-richclip-label=-
```

MIME update semantics:

```text
--set-mime MIME=@path
  add or replace that MIME blob

--set-mime MIME=-
  read blob from stdin

--remove-mime MIME
  remove that MIME from the item
```

Everything is a format, so `add`/`update` share one editing verb. Whether a
format is advertised on restore is decided by its MIME type: store
generated/support data (labels, OCR, search text) under a custom MIME type
(e.g. `application/x-richclip-label`) and it stays out of paste targets.

Each `add`/`update` invocation is applied in a single SQLite transaction (write
blob, upsert format rows, bump `updated_at`), so a concurrent labeller and user
edit can't leave an item half-written.

Important design choice: **generated representations should use custom MIME
types**. Only add `text/plain` or other standard MIME types when you actually
want paste targets to see that representation.

### `delete`

Deletion is immediate and removes unreferenced blobs inline (see Data model).

```sh
richclip delete <id>
richclip delete <id> --mime text/html
richclip delete --older-than 30d
```

MIME deletion is available through both `delete --mime` and
`update --remove-mime`; document `update --remove-mime` as the preferred form.

**Retention.** The daemon enforces a default retention window and prunes
automatically — old items are deleted (with their blobs) without a manual
sweep. `--older-than` is the same operation exposed manually.

```text
default: keep items for 30d   (configurable)
auto-prune runs in richclipd  (e.g. on capture and on a periodic timer)
delete --older-than <dur>      manual one-off prune
```

### `watch`

For an external labeller.

```sh
richclip watch
richclip watch --json
richclip watch --event item-added
richclip watch --event item-updated
richclip watch --mime image/png
```

Example event:

```json
{
  "event": "item-added",
  "id": "0190a1b2-...",
  "created_at": "2026-06-03T12:34:56+10:00",
  "formats": ["image/png", "text/html", "text/plain"]
}
```

A labeller could do:

```sh
richclip watch --json --event item-added --mime image/png |
while read -r event; do
  id="$(jq -r .id <<<"$event")"
  richclip decode "$id" image/png > /tmp/clip.png
  label="$(label-image /tmp/clip.png)"
  printf '%s' "$label" | richclip update "$id" --set-mime application/x-richclip-label=-
done
```

### Additional commands worth adding

Even if the basic API is `list`, `update`, `delete`, `watch`, I’d add these because they make the system usable:

```sh
richclip restore <id>
richclip decode <id> <mime>
richclip formats <id>
richclip inspect <id> --json
```

`restore` is the command that asks the daemon to become clipboard owner and advertise that item’s MIME types.

```sh
richclip restore <id>
```

`decode` is the equivalent of `cliphist decode`, but MIME-aware:

```sh
richclip decode <id> image/png > image.png
richclip decode <id> text/html
```

`formats` lists the MIME types on an item; `inspect` is the full record:

```sh
richclip formats 0190a1b2-...
# image/png                      184221
# application/x-richclip-label       24

richclip inspect 0190a1b2-... --json
# { "id": "0190a1b2-...", "created_at": "...", "updated_at": "...",
#   "formats": [ {"mime": "image/png", "size": 184221, "blob_hash": "..."} ] }
```

**Error contract.** Commands exit `0` on success, non-zero on failure
(`2` = no such item/MIME, `1` = other errors). With `--json`, errors are
emitted as `{"error":"...","code":"not_found"}` on stderr so scripts can branch
without parsing human text.

---

## Daemon behavior

### Capture flow

```text
selection changes
  if we own the current selection (from a restore), ignore it
  get data offer
  collect advertised MIME types
  if offer looks sensitive, skip entirely (store nothing)
  filter MIME types
  receive each accepted MIME type (drain fds promptly)
  store blobs
  create item row
  emit item-added event
```

Two correctness notes up front:

- **No self-capture.** The daemon both captures and restores, so a `restore`
  makes it the selection owner. It must ignore selection-change events for a
  selection it owns, or it would re-capture (and loop on) what it just restored.
- **Drain fds promptly.** With `wlr-data-control`, each `receive(mime, fd)`
  must be read while the offer is still alive; read the accepted types
  concurrently and don't block, or the offer closes and data is lost.

**Sensitive content.** Password managers and similar tools mark secret
selections (e.g. a `x-kde-passwordManagerHint` value of `secret`, or
`org.freedesktop.SecretService`-style hints). When such a hint is present, skip
the selection entirely — store nothing. This is checked before the allowlist.

Filtering matters. Some apps may advertise large, redundant, or awkward types. Start with a default allowlist:

```text
text/plain
text/html
text/uri-list
image/png
image/jpeg
image/webp
image/bmp
application/json
application/rtf
```

And a denylist / size limit:

```text
max item size
max single format size
deny application/octet-stream by default?
deny huge portal/file-transfer formats initially?
```

### Restore flow

```text
richclip restore <id>
  daemon loads item formats whose MIME is not application/x-richclip-*
  creates Wayland data source
  offers each MIME type
  sets source as current selection (capture ignores this while we own it)
  stays alive
  on send(mime, fd):
    streams blob bytes into fd
```

This lazy serving model is important: the compositor does not simply hold all clipboard bytes for you; the selection owner needs to provide the requested format when another client asks for it.

### Lifecycle

A single `richclipd` process does both capture and restore. It is the only
writer of clipboard ownership, so capture/restore state lives in one place (this
is what makes the no-self-capture rule enforceable). Run it as a systemd user
service; enforce a single instance via the unix socket path
(`$XDG_RUNTIME_DIR/richclip.sock`).

**Single writer.** When the daemon is running it is the only process that
writes the DB: the CLI routes `add`/`update`/`delete`/`restore` through the
socket, so every mutation emits a `watch` event and there's no write contention.
Reads (`list`/`formats`/`inspect`/`decode`) go straight to SQLite — run it in
WAL mode so readers never block the daemon's writes. In Phase 1 there is no
daemon, so the CLI opens the DB and writes directly; the same core functions
back both paths.

---

## Labelling model

A separate labeller should not mutate original bytes unless explicitly asked.

For an image item:

```text
image/png
  original captured bytes
  advertised on restore

application/x-richclip-label
  generated label text
  custom MIME, so never advertised on restore

application/x-richclip-ocr
  generated OCR text, searchable
  custom MIME, so never advertised on restore
```

So the labeller can enrich the item without changing paste behavior — every
addition is just another format:

```sh
printf '%s' "$label" | richclip update "$id" --set-mime application/x-richclip-label=-
richclip update "$id" --set-mime application/x-richclip-ocr=@ocr.txt
```

If a labeller wants to record structured detail (model, confidence), it can
put a JSON blob under its own custom MIME type, e.g.
`application/x-richclip-label+json`.

If you later want selecting an image to paste the generated label instead,
promote that text to a standard MIME type so it gets advertised on restore —
store it as `text/plain` rather than (or in addition to) the custom type:

```sh
printf '%s' "$label" | richclip update "$id" --set-mime text/plain=-
```

---

## Picker integration

Keep the picker separate. The CLI should make fuzzel integration trivial:

```sh
id="$(
  richclip list --json |
  jq -r '.[] | "\(.id)\t\(.label // "")\t\(.formats | map(.mime) | join(","))"' |
  fuzzel --dmenu |
  cut -f1
)"

[ -n "$id" ] && richclip restore "$id"
```

Later, a richer picker can show thumbnails from:

```text
$XDG_CACHE_HOME/richclip/thumbs/<item-id>.webp
```

But the core system should not depend on fuzzel, rofi, or a GUI.

---

## Rust implementation shape

Start with a **single package**: one library plus two binaries. Cargo lets one
package expose a lib and multiple `[[bin]]` targets, so the binaries stay thin
and the logic is testable without spawning a process — no workspace ceremony
yet.

```text
richclip/
  src/lib.rs        data model, DB, blob store, IPC types  (the lib)
  src/bin/richclip.rs    CLI binary
  src/bin/richclipd.rs   daemon binary
```

Phase 1 (storage + CLI) needs nothing more. Keep capture/restore behind a
trait in the lib so there's a seam to split along later.

**Grow to a second package at Phase 2**, when Wayland lands. Extract
`richclip-wayland` as its own crate so the heavy, platform-specific
`wayland-client` + protocol-codegen dependency stays out of the lib and the CLI
— Phase-1 builds and tests then never pull the Wayland toolchain.

```text
richclip/            lib + richclip + richclipd binaries
richclip-wayland/    data-control capture/restore (added at Phase 2)
```

Don't pre-split `richclip-core`/`richclip-ipc` into packages — they're just
modules in the lib until a real dependency or boundary justifies the seam.

Core dependencies I’d expect:

```text
clap        CLI
rusqlite    SQLite
blake3      content hashes
uuid        ids (v7, time-ordered)
serde       JSON events/API
tokio       daemon, IPC, labeller-friendly async
tracing     logs
```

Wayland side: use `wayland-client` plus generated protocol bindings for `wlr-data-control`; hide it behind your own small trait so you can later support `ext-data-control` without touching the DB/CLI.

```rust
trait ClipboardBackend {
    async fn run_capture_loop(&mut self, sink: CaptureSink) -> anyhow::Result<()>;
    async fn restore_item(&mut self, item: RestorableItem) -> anyhow::Result<()>;
}
```

---

## IPC API

The CLI should talk to the daemon for operations that affect live clipboard ownership or event streams.

```text
ListItems
GetItem
AddItem
UpdateItem
DeleteItem
RestoreItem
WatchEvents
```

Use newline-delimited JSON initially. Easy to debug, fine for local IPC.

```json
{"cmd":"restore-item","id":"0190a1b2-..."}
```

Daemon response:

```json
{"ok":true}
```

Watch stream:

```json
{"event":"item-added","id":"0190a1b2-..."}
{"event":"item-updated","id":"0190a1b2-...","changed":["application/x-richclip-label"]}
```

---

## MVP phases

### Phase 1: storage + CLI

Implement:

```text
add
list
formats
decode
update
delete
```

Use manual import at first:

```sh
richclip add --set-mime image/png=@foo.png
```

This validates DB/blob/add/update semantics without Wayland complexity.

### Phase 2: capture daemon

Implement:

```text
richclipd capture
richclip watch
```

Capture multi-MIME offers into grouped items.

### Phase 3: restore provider

Implement:

```text
richclip restore <id>
```

Advertise all stored MIME types and lazily serve requested blobs.

### Phase 4: labeller

Implement external process first:

```text
richclip watch -> label image -> richclip update
```

Keep labelling out of the daemon initially. The daemon should be clipboard plumbing, not model orchestration.

### Phase 5: picker polish

Add:

```text
thumbnail generation
fuzzel formatter
```

---

## Open design decisions

The main ones I’d settle early:

```text
What should the default retention window be?
  Some default (e.g. 30d) with auto-prune. The exact value/units are open.
  Deletion itself is settled: immediate, blobs cleaned inline.

Should duplicate clipboard selections create new history rows?
  Probably configurable:
    dedupe by comparing captured formats (set of (mime, blob_hash)) by default
    bump updated_at maybe
    optional keep duplicates

Should the daemon capture all MIME types?
  No. Use allowlist + size limits first (and skip sensitive offers).

Should primary selection be supported?
  Later. Keep CLIPBOARD first.
```

The resulting system is basically: **CopyQ’s item model, cliphist’s scriptability, and Wayland-native multi-MIME restore**, with a clean `watch → update` loop for async labelling.

[1]: https://wayland.freedesktop.org/docs/html/apa.html?utm_source=chatgpt.com "Appendix A. Wayland Protocol Specification"
[2]: https://man.archlinux.org/man/wl-copy.1.en?utm_source=chatgpt.com "wl-copy(1) - Arch manual pages"
[3]: https://wayland.app/protocols/wlr-data-control-unstable-v1?utm_source=chatgpt.com "wlr data control protocol - Wayland Explorer"
