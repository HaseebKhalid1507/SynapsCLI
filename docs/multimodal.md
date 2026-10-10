# Local attachments

Synaps can send explicitly selected local images, PDFs, and UTF-8 text files in
TUI chat, `synaps chat`, and local stdio `synaps rpc` prompts. A path mentioned in
ordinary chat is **not** automatically attached. There is no URL fetching,
clipboard image decoding, remote file-ID support, or remote WebSocket filesystem
attachment loading.

## TUI and headless chat

```text
/attach ./screenshots/error.png
/attach "./notes/design review.txt"
/attachments
What explains this error? Use the attached notes as context.
```

- `/attach PATH` captures the file bytes immediately and stages them for the next
  **normal user input**. The entire remainder is one path, including spaces;
  matching surrounding single or double quotes are accepted. It is not a shell
  command: no globbing or command substitution is performed.
- `/attachments` (or `/attachments list`) lists pending filenames, detected types,
  and byte counts, without showing file contents.
- `/detach` (or `/detach clear`) discards all pending attachments. It does not
  remove previously submitted attachments from conversation history or sessions.
- Enter with an empty input sends an attachment-only message when files are
  staged. In `synaps chat`, send a blank line; EOF alone does not submit files.
- Slash commands do not send staged attachments. `/clear` discards them. TUI
  `/resume` and session-switch commands refuse while attachments are staged;
  submit or `/detach` first. TUI `/compact` likewise requires detaching or
  submitting first because it may establish a new session.

Attaching and submitting attached messages require idle chat. The TUI refuses
these actions while streaming or compacting instead of placing attachments in a
text-only steering/queued-message slot. Existing staged bytes are retained on
rejection. Listing and detaching are safe while busy. Headless chat reads commands
only at its idle prompt, after the current stream or compaction completes.

An unsupported model/transport, invalid file, or exceeded limit produces an
explicit error. A rejected submission does not append a partial user message;
previously staged attachments remain available to retry with a supported model or
to discard. Preflight checks the proposed **complete history**, including the new
message, before consuming staged bytes. Provider failures after a successful
append do not re-stage attachments: they are already in conversation history.

For piped input:

```sh
printf '%s\n' '/attach "./diagram.png"' 'Explain the diagram.' | synaps chat
# Attachment-only submission (the second line is blank):
printf '%s\n' '/attach "./diagram.png"' '' | synaps chat
```

A failed attachment load or submission in piped mode stops processing and returns
an error, rather than silently continuing with an unintended text-only request.
Unsent pending files are not saved on exit, and headless chat reports their count.

## Local stdio RPC

The existing `prompt.attachments` field now sends actual captured file content,
not a filename placeholder:

```json
{"type":"prompt","id":"p1","message":"Explain this diagram","attachments":[{"path":"/absolute/path/diagram.png"}]}
```

Every attachment path must be absolute and contain no `..` path components. All
paths are checked before loading, and all files must load and pass complete-history
preflight before any user message is appended. Errors use the existing correlated
`error` event and leave history unchanged. Optional `name` and `mime` hints are not
authoritative: the loader uses the selected file's basename and detected content.
An empty `message` is valid with attachments. `follow_up` remains text-only and
unchanged.

Successful attached prompts emit a correlated `response` with command
`attachments.disclosure` before provider dispatch. Its body contains `count` and a
privacy-disclosure `message`. Clients should display the disclosure and still wait
for the ordinary streaming/completion events. This local stdio mechanism must not
be exposed as arbitrary remote filesystem access by a bridge or WebSocket server.

## File and model support

The shared loader accepts regular files, verifies supported images, recognizes
PDF bytes, and treats other accepted files as UTF-8 text documents. A filename
extension does not override content detection. Directories, devices, unsupported
binary data, malformed/oversized media, and empty files are rejected. Current
loader limits are eight attachments per message, 15 MiB combined raw bytes,
3.5 MiB per image (PNG/JPEG/GIF/WebP, at most 8000 pixels per side), 10 MiB per PDF,
and 256 KiB per text document. Full-history and transport limits can be tighter,
including the broker proxy budget.

Availability depends on the exact model **and** transport. Supported Anthropic
models accept images, PDFs, and text documents. OpenAI-compatible and
Responses/Codex routes use their supported wire formats and advertised
capabilities; not every image-capable model accepts PDFs. Unknown or unsupported
routes fail closed rather than dropping data or turning base64 into prompt text.
Changing models or resuming history triggers runtime preflight again.

## Privacy and retention

**Attachment bytes are included in provider requests and stored in private
session storage after submission.** Subsequent turns may resend attachments still
in history. Only attach files you intend to disclose to that provider and retain
locally. The listing/attach confirmation and RPC disclosure make this explicit.

Staging captures a snapshot: changing or deleting the source file afterward does
not change the pending attachment. Submitted bytes are embedded in private
session snapshots/journals so a resume does not need the original file. Staged-but-unsent bytes
are not persisted. Detaching or starting a new conversation does not erase older
saved sessions or retract previous provider requests.

Attachments remain lower-authority user data. Canonical text documents retain
structured document sources; plain-text lowering happens only at the provider
boundary. Context archive/history-memory projections exclude image and document
source bytes, and tracing must remain metadata-only. Private session storage
itself intentionally contains submitted content; it is not a secret-redaction
mechanism.

### Current transport boundaries

`openai-codex/gpt-6-astra` has verified **text and image** inputs. Synaps sends
images as Responses `input_image`; UTF-8 files become labelled `input_text`.
The observed Astra catalog does **not** advertise PDF/file input, so PDFs are
rejected rather than guessed supported. Native known Anthropic models accept
PDF document blocks; compatible routes require exact advertised file capability.
This implements inline request attachments, not a provider Files API upload/delete
lifecycle. Native Gemini Code Assist, explicit cloud invokes and extension
providers currently reject attachments. Clipboard image decoding and WebSocket
binary uploads are not implemented; use `/attach` or local stdio RPC.

### Daemon mode

Under the daemon (`synaps --daemon`), attachment paths are resolved on the
**client** (TUI or `synaps chat`), not the daemon process. The client calls
`load_attachment` on its own filesystem, builds canonical content blocks, and
ships them as `Vec<serde_json::Value>` inside `SessionCommand::Submit`. The
daemon's `SessionActor` validates the blocks structurally
(`validate_attachment`) and against the session's model (`validate_history_for_request`)
before appending to history. A validation failure emits a `SystemNotice` and
does not push the message. Per-frame size is bounded by
`DAEMON_MAX_FRAME_BYTES` (64 MiB). Native Anthropic request attachment bytes
are bounded by the chunked replay policy below; other routes retain their
existing strict history and broker budgets. The RPC stdio path (`synaps rpc`) runs
in-process and loads files directly — no daemon filesystem boundary.

For `/trace next content`, binary media is structurally redacted. OpenAI requests
containing text-document attachments explicitly withhold the content capture
bundle (reported by trace status), because lowering to ordinary input text loses
attachment provenance. Metadata tracing continues normally.

## Model capability gating — a note on model ids

Attachments are accepted or refused per model by `validate_messages`. The static
modality table is keyed by **provider-prefixed** ids (`anthropic/claude-sonnet-4-6`,
`openai-codex/gpt-6-astra`). Live catalog evidence (the capability cache) extends
it at runtime. An unprefixed id with no cached evidence is treated as text-only
and image attachments are refused with `attachments rejected: Selected model
lacks exact image input capability metadata`. Profiles normally use prefixed ids.

### Anthropic request-local image sizing

On the native Anthropic streaming and non-streaming paths, images with either
encoded-raster side over 2000 pixels are resized proportionally from the **first**
request, regardless of image count. This prevents the former 20-to-21 image
transition from changing earlier image blocks. Saved originals, tool ingress,
and non-Anthropic routes are unchanged. Older saved originals can produce a
one-time different outgoing prefix after upgrading.

Projection uses deterministic Triangle filtering, JPEG quality 85 for JPEG input,
and RGBA PNG otherwise (preserving alpha). A stable adjacent text block states
original/sent dimensions: coordinates refer to sent pixels. Resizing uses the
encoded raster, deliberately does **not** apply EXIF orientation, and removes
metadata; export an upright static image first if orientation matters. Images
at or below 2000 pixels per side are sent unchanged. They retain preexisting
structural/header validation, not full decoding: a small image with corrupt pixel
data can still pass through for the provider to reject. Oversized GIF/WebP (including
static files) and animated PNG require static PNG/JPEG export rather than silently
losing animation.

Original attachment preflight runs first; projected output is revalidated against
the same budgets. Projection bounds input to 3.5 MiB, 8000 pixels per side and
16 million pixels for resizing, sets image 0.24's partial 128 MiB library
allocation guard, and bounds encoded output to 3.5 MiB. This guard is **not a
hard ceiling on JPEG decoder memory**: it is not propagated to JPEG internal
allocations, including progressive coefficient buffers. The 16 MP cap and single
heavy-work slot bound the workload, not total decoder memory to 128 MiB. Nor is
this a process-memory ceiling: decoded pixels, resize scratch/output, base64 and
encoder buffers also consume memory. Invalid/unsupported oversized images or budget
failures return actionable errors before sending, never silently fall back to
the original. One process-wide blocking worker slot limits heavy decode work,
including after cancellation. A shared SHA256-keyed FIFO memo holds only projected
base64 bytes and explanatory metadata, limited to 16 entries and 16 MiB; cache
hits and eviction do not alter projection results.

### Native Anthropic chunked image retention

Original admission validates canonical shape, role/nesting, exact capabilities,
per-item limits and the eight-attachment limit **per original user message**.
Before decoding base64 or collecting occurrence vectors, a bounded serialization
prepass caps original canonical media at 256 MiB and 4096 image occurrences
on all routes (in addition to the existing strict provider/broker budgets).
Only native Anthropic defers its aggregate 20 MiB budget into chronological replay;
compatible providers and broker proxy budgets retain their existing rules.

Replay uses unchanged original user-message batches (including all nested sibling
tool results), not image hashes. At each historical batch, either more than 100
retained images or more than 20 MiB of serialized original attachment blocks
triggers retirement of an oldest-image prefix. It retires at least 40 older images
when available, and continues toward 12 MiB total retained attachments, without
ever retiring images in that batch. Hard bounds must hold; the chunk/low-water
headroom is best-effort when the protected batch or documents prevent it.
The 12 MiB target does not trigger premature eviction: a retained total above
12 MiB but below 20 MiB stays intact unless count overflows. After overflow,
soft headroom yields to a protected batch above 12 MiB that still fits 20 MiB.
Documents are never evicted and accumulate chronologically. If documents plus
the protected batch cannot fit, admission explicitly fails—even if later text
has been appended. New tool batches reject all media-bearing siblings atomically
with explicit errors, preserving correlation IDs, order and text-only siblings.

The watermark is recomputed from every original batch on retries and reloads.
100 small one-image batches become 61 retained images at batch 101, hold the
same retired prefix through batch 140, then retire the next chunk at 141.
Identical images count as distinct occurrences. Omissions use a fixed label.
No session schema, persistent watermark or runtime retention state is introduced.
Original messages are untouched. Compaction, truncation and edits start a new
replay window: **global non-resurrection is not guaranteed** across those changes.

Both native API paths plan retirement on pristine original batches, then project/check
every original image and apply retirement in one blocking pass, followed by strict projected validation,
then thinking sanitization/adjacent-role merging, tool-reference lowering and
cache annotation. Strict checks therefore preserve original batch count semantics
without weakening the eight-attachment admission limit. The continuation wire copy
only strips message metadata and preserves batch boundaries. Accounting includes
serialized original attachment blocks (including documents), not the whole HTTP
request. Generated cache markers are added only to the outgoing copy, after
accounting; any attachment metadata already present in originals is conservatively
charged. Resized output is strictly revalidated for expansion; no second eviction
fallback occurs. The protected raw batch budget is deliberately conservative.

Retired originals still receive all existing structural/header/base64 validation.
They also receive the same projection checks as retained images: oversized codec,
animation and pixel-resource errors still reject the request. Retired projected
output is discarded, never persisted; small images retain structural-only checks. The separate resize memo is not retention
state: memo eviction cannot change which image occurrences are retired. Chunk
boundaries deliberately change the outgoing prefix; retries/reload of unchanged
history reproduce it, not universal prefix stability across all history changes.

Retention coverage includes shared projection regressions and source-order checks
for both native transports, plus streaming request tests in
`runtime::stream::rich_output_tests`. No retention-specific sync loopback parity
test is provided.
