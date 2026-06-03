## Why not just use `cliphist`?

`cliphist` is excellent for the simple Unix-pipeline model:

```sh
wl-paste --watch cliphist store
cliphist list | fuzzel --dmenu | cliphist decode | wl-copy
```

That is also exactly its limitation. Its documented workflow is `list`, `decode`, `delete`, `wipe`, `compact`, and `store`; entries are treated as individual stored payloads, not as grouped clipboard offers. ([GitHub][1])

For this project, we specifically want:

```text
one history item
  text/plain
  text/html
  image/png
  generated label
  OCR/search text
  thumbnail
  stable editable ID
```

`cliphist` does not model that. With `cliphist`, if `wl-paste --type text` and `wl-paste --type image` both observe the same clipboard selection, they are stored as separate blobs. That makes later update flows awkward:

```text
replace image/png inside item X     # not a native operation
add generated text/plain to item X  # not a native operation
keep label as metadata for item X   # requires sidecar state
restore all MIME types together     # not supported through cliphist/wl-copy
```

So `cliphist + sidecar DB` is possible, but at that point the sidecar DB becomes the real system, and `cliphist` becomes an inconvenient blob store with mismatched IDs.

## Why not just use CopyQ?

CopyQ is much closer to the data model we want. Its docs explicitly say clipboard/items can provide multiple formats keyed by MIME type, and CopyQ advertises support for storing text, images, HTML, URLs, and custom MIME types side by side. ([CopyQ Documentation][2]) ([CopyQ][3])

The reason not to use it is not capability; it is **control and integration**.

CopyQ is a full clipboard manager with its own UI, daemon, tabs, commands, scripting model, database behavior, search behavior, and restore behavior. We want something more like:

```text
Wayland-native clipboard capture/restore plumbing
scriptable CLI-first history store
fuzzel-compatible list output
async labeller via watch/update
metadata-first enrichment
precise MIME mutation API
```

CopyQ may support many of these things indirectly through scripting, but then the implementation becomes “write a system inside CopyQ’s scripting/runtime model.” That is probably more friction than just implementing the small purpose-built core in Rust.

## Why not use `wl-copy`/`wl-paste` scripts?

Because they are intentionally one-stream tools. `wl-paste` can list MIME types and retrieve a selected MIME type, but each invocation gives you one representation. More importantly, `wl-copy` currently cannot offer multiple MIME types at the same time. ([Arch Manual Pages][4])

That means a faithful restore needs direct Wayland data-control behavior:

```text
restore item X
  advertise text/plain
  advertise text/html
  advertise image/png
  when target requests one, stream that blob
```

A script around `wl-copy` cannot do that today.

## Justification for building it

The custom Rust implementation is justified because the desired unit of abstraction is different:

```text
cliphist:
  history of blobs

CopyQ:
  full desktop clipboard manager

this project:
  programmable grouped MIME clipboard store
```

The feature that makes it worth building is not just “history.” It is **editable grouped clipboard items**:

```sh
richclip update <id> --set-mime text/plain=@label.txt
richclip update <id> --remove-mime text/html
richclip update <id> --set-label 'terminal screenshot showing rust panic'
richclip restore <id>
```

That is the core API neither `cliphist` nor shelling out to `wl-copy` gives us, and CopyQ gives it only inside a larger application model we do not really want.

So the decision is:

```text
Use cliphist if:
  one blob per entry is fine
  fuzzel pipeline is the whole goal

Use CopyQ if:
  a full GUI clipboard manager is acceptable
  its scripting/database model is acceptable

Build this if:
  grouped MIME items are the core data model
  labels/OCR/thumbnails are first-class metadata
  async labelling should be external and scriptable
  restore should faithfully re-offer multiple MIME types
  the UI should remain replaceable
```

[1]: https://github.com/sentriz/cliphist?utm_source=chatgpt.com "sentriz/cliphist - Clipboard history “manager” for Wayland"
[2]: https://copyq-docs.readthedocs.io/en/latest/scripting-api.html?utm_source=chatgpt.com "Scripting API - CopyQ documentation - Read the Docs"
[3]: https://copyq.net/?utm_source=chatgpt.com "CopyQ - Free Download for Windows, macOS, Linux"
[4]: https://man.archlinux.org/man/extra/wl-clipboard/wl-paste.1.en?utm_source=chatgpt.com "wl-paste(1) - Arch manual pages"
