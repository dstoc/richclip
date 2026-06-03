# richclip contrib

Optional integrations that build on the `richclip` CLI. These are deliberately
kept out of the core binaries so the daemon and CLI stay UI-agnostic — they only
use the documented `richclip list --json` and `richclip restore <id>` interface.

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
