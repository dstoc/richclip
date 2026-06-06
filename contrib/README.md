# richclip contrib

Optional integrations that build on the `richclip` CLI. These are deliberately
kept out of the core binaries so the daemon and CLI stay UI-agnostic — they only
use the documented `richclip list --json` and `richclip restore <id>` interface.

## `richclip-labeld`

A contrib image-labelling worker. It watches `item-added` image events from
`richclipd`, reads the stored source image from the local richclip data store,
sends it to a configured OpenAI-compatible vision model, and writes the label
back as `application/x-richclip-label`.

Build it:

```sh
cargo build --release -p richclip-labeld
install -Dm755 target/release/richclip-labeld ~/.local/bin/richclip-labeld
```

Configuration lives at `~/.config/richclip/labeld.toml` by default, or pass
`--config <path>`.

Example config:

```toml
[model]
url = "http://127.0.0.1:11434/v1"
model = "your-vision-model"
timeout_seconds = 60
api_key_env = "OPENAI_API_KEY"

[prompt]
label = "Return one short factual label for this clipboard image. Use plain text only, no quotes, no prefixes, and no trailing sentence punctuation."
```

Relevant runtime environment:

| Env var | Purpose |
|---|---|
| `OPENAI_API_KEY` | Optional bearer token when `api_key_env` points at it |
| `RICHCLIP_SOCKET` | Override the daemon socket path |
| `RICHCLIP_DATA_DIR` | Override the local richclip store used for image bytes |
| `XDG_CONFIG_HOME` | Changes the default `labeld.toml` location |

Launch it directly:

```sh
richclip-labeld
```

Replace existing labels instead of skipping them:

```sh
richclip-labeld --overwrite
```

Example `systemd --user` unit:

```ini
[Unit]
Description=richclip image labeller
After=graphical-session.target

[Service]
ExecStart=%h/.local/bin/richclip-labeld
Restart=always
RestartSec=2
Environment=OPENAI_API_KEY=replace-me

[Install]
WantedBy=default.target
```

Storage behavior:

- `richclip-labeld` only watches `item-added` events and only processes items
  that advertise one of `image/png`, `image/webp`, `image/jpeg`, or `image/bmp`.
- It chooses the concrete source MIME in that order, matching thumbnail
  generation priority.
- It stores plain UTF-8 label text in `application/x-richclip-label` only.
- It does not rewrite the original image bytes and does not promote labels to
  `text/plain`, so restore/paste behavior stays unchanged.

## `richclip-fuzzel`

A [fuzzel](https://codeberg.org/dnkl/fuzzel) picker: choose a clipboard history
item and restore it to the active clipboard.

```sh
install -Dm755 contrib/richclip-fuzzel ~/.local/bin/richclip-fuzzel
```

Run it (with `richclipd` already running):

```sh
richclip-fuzzel
```

Bind it to a key in Sway:

```
bindsym $mod+v exec richclip-fuzzel
```

How it works:

- reads `richclip list --json` (newest first),
- shows one fuzzel entry per item — the derived `label`, or the comma-joined MIME
  list when there's no label,
- attaches the item's `thumbnail` (a `thumbs/<id>.png` from thumbnail generation)
  as the fuzzel entry icon when present; entries are text-only otherwise,
- restores the chosen item **by index** (`fuzzel --dmenu --index`), so selection
  is correct even when two items share a label or a label is empty.

Requirements: `richclip`, a running `richclipd` (restore needs the daemon),
`fuzzel` with `--dmenu --index` support, and `jq`.

To target rofi/wofi/tofi instead, copy the script and replace the
`fuzzel --dmenu --index` command with the equivalent menu invocation that can
return a selected index.
