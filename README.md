# richclip

A Wayland-native clipboard history manager whose unit of history is a **grouped
item**: one stable id holding every MIME representation of a selection (captured
*and* generated) — editable and scriptable from a CLI-first interface.

Where `cliphist` stores one blob per entry and CopyQ wraps everything in a GUI,
richclip keeps the grouped multi-MIME model *and* the Unix-pipeline
scriptability, with faithful Wayland multi-MIME restore and a `watch → update`
loop for async enrichment (labels, OCR). See [`docs/richclip.md`](docs/richclip.md)
for the full design and [`docs/justification.md`](docs/justification.md) for why
it's a separate tool.

## How it works

```text
richclipd   Wayland data-control client: captures clipboard offers into grouped
            items, restores items as the active selection, emits events.
richclip    CLI: add/list/formats/decode/update/delete/inspect/watch/restore/
            thumbnail. Reads go straight to SQLite; mutations route through the
            daemon when it's running (single writer), else direct.
store       SQLite index (items + formats) + content-addressed blob dir
            (blake3, deduped) + PNG thumbnail cache.
```

An `Item` is pure identity (`id`, timestamps); all content lives in `Format`
rows, one per MIME type, each pointing at a content-addressed blob. Generated
data (labels, OCR, search text) is just another format under the
`application/x-richclip-*` namespace — stored but never advertised on restore.

### Crates

```text
richclip/            lib (model, store, blobs, IPC types) + `richclip` CLI
richclip-wayland/    wlr-data-control capture/restore backend + `richclipd`
```

## Build & install

Requires a recent stable Rust toolchain.

```sh
cargo build --release --workspace        # builds both `richclip` and `richclipd`

# or install to ~/.cargo/bin:
cargo install --path .                    # richclip (CLI)
cargo install --path richclip-wayland     # richclipd (daemon)
```

Run the daemon in your Wayland session (Sway/wlroots; needs the
`zwlr_data_control` protocol):

```sh
richclipd                                 # foreground; or run as a systemd user service
```

## Usage

```sh
# Add an item (data enters via stdin `-` or a file `@path`; --set-mime repeatable)
echo "hello"        | richclip add --set-mime text/plain=-
richclip add --set-mime image/png=@shot.png        # -> prints new id

# List (human by default; newest first)
richclip list
richclip list --json --limit 50 --mime image/png

# Inspect / read
richclip formats <id>
richclip decode  <id> image/png > out.png
richclip inspect <id>

# Edit an item (one verb for everything; generated data -> custom MIME)
echo "rust panic screenshot" | richclip update <id> --set-mime application/x-richclip-label=-
richclip update <id> --set-mime text/html=@frag.html
richclip update <id> --remove-mime text/html

# Delete
richclip delete <id>
richclip delete <id> --mime text/html
richclip delete --older-than 30d

# Live events (needs richclipd) — the async-labeller hook
richclip watch --json --event item-added --image

# Become the clipboard owner again (needs richclipd)
richclip restore <id>

# Thumbnails (image items): (re)generate; prints the cache path
richclip thumbnail <id>
```

### Async labelling

Use the maintained contrib worker in [`contrib/richclip-labeld`](contrib/richclip-labeld/)
for image labelling. It watches `item-added` image events, reads the preferred
stored image bytes locally, sends them to an OpenAI-compatible vision endpoint,
and writes the result back as `application/x-richclip-label`.

See [`contrib/README.md`](contrib/README.md) for build/install, config, and
service examples.

`watch` + `update` is still the underlying enrichment loop if you want to build
your own worker:

```sh
richclip watch --json --event item-added --image |
while read -r ev; do
  id=$(jq -r .id <<<"$ev")
  mime=$(jq -r '.formats[] | select(startswith("image/"))' <<<"$ev" | head -n1)
  [ -n "$mime" ] || continue
  richclip decode "$id" "$mime" > /tmp/clip.png
  label-image /tmp/clip.png | richclip update "$id" --set-mime application/x-richclip-label=-
done
```

A label stored under `application/x-richclip-label` enriches search/display
without changing paste behaviour. Promote it to `text/plain` only if you want it
advertised on restore.

### Picker

`richclip` stays UI-agnostic; see [`contrib/`](contrib/) for a fuzzel picker
(`richclip-fuzzel`) that lists items (with thumbnails as icons) and restores the
chosen one.

## Output & errors

- `list`/`inspect`/`add`/`watch` support `--json` with stable shapes; `decode`
  writes raw bytes to stdout.
- Exit codes: `0` ok, `2` no such item/MIME, `1` other. With `--json`, errors are
  `{"error":"...","code":"not_found"}` on stderr.

## Configuration

| Env var | Purpose | Default |
|---|---|---|
| `RICHCLIP_DATA_DIR` | SQLite DB + blobs | `$XDG_DATA_HOME/richclip` |
| `RICHCLIP_CACHE_DIR` | thumbnail cache | `$XDG_CACHE_HOME/richclip` |
| `RICHCLIP_SOCKET` | daemon IPC socket | `$XDG_RUNTIME_DIR/richclip.sock` |
| `RICHCLIP_RETENTION_DAYS` | auto-prune window | `30` |
| `RUST_LOG` | daemon log filter | `info` |

The CLI also takes a global `--data-dir <path>` (overrides `RICHCLIP_DATA_DIR`).

Default capture allowlist: `text/plain`, `text/html`, `text/uri-list`,
`image/{png,jpeg,webp,bmp}`, `application/json`, `application/rtf`. Offers
carrying a password-manager hint (e.g. `x-kde-passwordManagerHint`) are skipped
entirely and never stored.

## Status

Implemented: storage + full CLI, Wayland capture, daemon + IPC + `watch`,
restore, PNG thumbnails, fuzzel picker. See [`docs/richclip.md`](docs/richclip.md)
and [`docs/proposals/`](docs/proposals/) for design and follow-ups.

## Development

```sh
cargo test --offline --workspace      # lib unit + CLI/IPC integration + filter/thumbnail tests
```

Tests are hermetic (set `RICHCLIP_DATA_DIR`/`RICHCLIP_CACHE_DIR`/`RICHCLIP_SOCKET`
to temp dirs); the IPC test runs `richclipd` headless. Wayland capture/restore
require a live compositor and are validated manually.
