# Proposal: contrib image labeller

## Motivation

The repo already treats async enrichment as an external concern: `richclipd`
captures and restores clipboard items, while labels and OCR are expected to flow
through `watch -> decode -> update`. That contract is documented and the picker
already consumes `application/x-richclip-label` as the preferred display label.
What is missing is a first-party contrib worker that performs image labelling in
a way that matches the existing model and MIME conventions.

## Problem statement

- The documented example in [README.md](/home/user/workspace/richclip-agent/README.md:93)
  is only a shell sketch; there is no maintained implementation.
- Display labels are derived from `application/x-richclip-label` first, then
  from a `text/plain` snippet, in
  [src/bin/richclip.rs](/home/user/workspace/richclip-agent/src/bin/richclip.rs:444).
  So a labeller must write plain UTF-8 label text to that MIME if it wants the
  rest of the system to pick it up automatically.
- Restore intentionally excludes `application/x-richclip-*` formats in
  [richclip-wayland/src/daemon.rs](/home/user/workspace/richclip-agent/richclip-wayland/src/daemon.rs:245),
  which is exactly what we want for generated labels, but there is no contrib
  worker that formalizes this behavior.
- `../ut` already has a good model for a small OpenAI-compatible HTTP client:
  structured config, endpoint/model/api-key handling, prompt text, timeout, and
  request-body extension points. Reusing that shape is safer than inventing a
  second ad hoc configuration style.

## Proposal

Ship a first-party contrib binary, `richclip-labeld`, that watches for new image
items, sends the chosen image bytes to a configurable vision-capable
OpenAI-compatible chat-completions endpoint, and writes the returned label back
to the item as `application/x-richclip-label`.

This remains outside `richclip` and `richclipd` so model orchestration does not
become a daemon responsibility.

### Placement

Add a new crate under the workspace, for example:

```text
contrib/richclip-labeld/
```

This should build a standalone binary named `richclip-labeld`. It may depend on
the `richclip` crate for IPC types and path helpers, but it should not require
changes to the storage model.

The binary belongs in `contrib` conceptually even if it is a workspace member,
because it is optional and provider-specific.

### Runtime behavior

The worker loop is:

```text
watch item-added image events
-> resolve concrete source MIME for the item
-> decode bytes
-> send image to model with a label prompt
-> update item with application/x-richclip-label
```

Detailed semantics:

- Subscribe to `item-added` only, not `item-updated`, so self-generated updates
  do not loop.
- Watch image items using the proposed image-aware watch filter; until that
  lands, the binary can subscribe broadly and inspect `formats` itself.
- Choose the source image MIME using the same priority as thumbnails:
  `image/png`, `image/webp`, `image/jpeg`, `image/bmp`.
- Skip the item if none of those formats are present.
- Skip the item by default if `application/x-richclip-label` already exists.
  Add `--overwrite` to replace existing labels when explicitly requested.
- Write plain UTF-8 label text to `application/x-richclip-label`.
- Optionally write structured metadata to a separate MIME such as
  `application/x-richclip-label+json`.

The worker must never rewrite original clipboard image bytes and must never
promote the label to `text/plain` automatically.

### Model client

Follow the configuration shape already used in `../ut`:

```toml
[model]
url = "http://127.0.0.1:11434/v1"
model = "your-vision-model"
timeout_seconds = 60
api_key_env = "OPENAI_API_KEY"

[prompt]
label = "Return one short clipboard label for this image..."
```

The exact config path can be independent from `ut`, but the fields should match:

- `url`
- `model`
- `timeout_seconds`
- `api_key`
- `api_key_env`
- `extra_body`

That gives the same provider flexibility without coupling the projects.

### Request/response contract

Use an OpenAI-compatible chat-completions request with multimodal message
content. Unlike `ut`, which sends `input_audio`, the image labeller sends an
image content part plus a text instruction. The worker should treat the first
returned text as the label, trim it, and reject empty output.

Prompt behavior should be intentionally narrow:

- one short label
- factual, not conversational
- no quotes
- no trailing punctuation requirement
- optimized for picker/search display, not caption generation

### Failure behavior

Failure must be best-effort and local to the item:

- watch stream disconnect: log and reconnect with backoff
- decode failure: log and skip item
- HTTP/model failure: log and skip item
- empty/invalid model output: log and skip item
- update failure: log and continue

The worker should not terminate the whole process for per-item failures.

### Documentation

Document in `contrib/README.md`:

- install/build command
- required environment/config
- example systemd user service or shell launch line
- explanation that the label is stored as `application/x-richclip-label`
- optional metadata MIME if enabled

Update the root README labelling section to point at the contrib worker instead
of only the shell sketch.

## Non-goals

- No model execution inside `richclipd`.
- No OCR extraction or structured search indexing in this proposal.
- No automatic `text/plain` promotion of generated labels.
- No provider-specific SDK integration; use plain HTTP against an
  OpenAI-compatible endpoint.
- No batching or historical backfill in the first version.

## Verification

- Start `richclip-labeld`, add an image item, and confirm the item gains
  `application/x-richclip-label`.
- `richclip list --json` shows the generated label as the derived `label` field.
- Restoring the item does not advertise the generated label as a paste format.
- When the model returns an empty label, the item remains unchanged and the
  worker logs the failure.
- When an item already has `application/x-richclip-label`, the worker leaves it
  unchanged unless `--overwrite` is set.
- Killing the network or model endpoint causes retries/reconnect behavior, not a
  process crash.

## Success criteria

- [ ] A first-party contrib binary exists for automatic image labelling.
- [ ] It stores plain text labels under `application/x-richclip-label`.
- [ ] It never changes advertised restore formats.
- [ ] Config matches the existing OpenAI-compatible shape already proven in `ut`.
- [ ] Per-item failures are non-fatal and observable in logs.

## Suggested implementation shape

```text
contrib/richclip-labeld/Cargo.toml
contrib/richclip-labeld/src/main.rs
contrib/richclip-labeld/src/config.rs
contrib/richclip-labeld/src/client.rs
contrib/README.md
README.md
```

Suggested module split:

- `config.rs`: file/env config parsing and validation
- `client.rs`: OpenAI-compatible HTTP request builder and response parsing
- `main.rs`: watch loop, MIME selection, decode/update orchestration

### Sequencing

Implement in this order:

1. image-aware watch filters
2. contrib labeller binary using those filters
3. optional metadata MIME and reconnect/backoff polish

This keeps the worker simpler and avoids teaching it to re-implement image-event
matching that properly belongs to the core watch interface.

## Future expansion

- Add OCR as a second contrib worker or a second output MIME.
- Add a backfill mode that scans existing unlabelled image items.
- Add prompt variants for different label styles once there is a second real
  caller.
