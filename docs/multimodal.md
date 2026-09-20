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

For `/trace next content`, binary media is structurally redacted. OpenAI requests
containing text-document attachments explicitly withhold the content capture
bundle (reported by trace status), because lowering to ordinary input text loses
attachment provenance. Metadata tracing continues normally.
