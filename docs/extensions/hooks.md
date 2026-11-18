# Extension hooks

Hooks are compiled into SynapsCLI. Extensions subscribe to them from the
`extension.hooks` array in `.synaps-plugin/plugin.json`.

## Manifest syntax

All tool calls:

```json
{ "hook": "before_tool_call" }
```

Only one tool:

```json
{ "hook": "before_tool_call", "tool": "bash" }
```

The `tool` filter is supported only for `before_tool_call` and
`after_tool_call`. It matches either the API-safe tool name or the runtime tool
name.

A `before_tool_call` hook may also say what a call does when its extension
gives no answer (see [Required guards](#required-guards)):

```json
{ "hook": "before_tool_call", "on_failure": "block" }
```

`on_failure` is `"continue"` (the default, and what every hook did before the
field existed: a crashed, timed-out or malformed handler is skipped and the call
proceeds) or `"block"` (the call is refused, naming the extension and the
cause). It is accepted only on `before_tool_call`; any other hook with
`on_failure: "block"`, or a value other than those two, fails manifest
validation.

A filter matches one tool name exactly, with one exception: a
`before_tool_call` filter for `edit` or `write` also matches `multi_edit`,
which changes files the same way, so a guard written for those tools still
blocks or confirms it. It sees `multi_edit`'s own input (`edits[].path`
per file); see [multi-edit.md](../multi-edit.md#hooks). `after_tool_call`
filters stay exact.

## Hook catalog and action matrix

| Hook | Required permission | Tool filter? | Allowed result actions | Purpose |
|---|---|---:|---|---|
| `before_tool_call` | `tools.intercept` | yes | `continue`, `block`, `confirm`, `modify` | Inspect/block/confirm/modify a tool call before execution |
| `after_tool_call` | `tools.intercept` | yes | `continue`, `replace` | Observe or rewrite tool output after execution |
| `before_message` | `privacy.llm_content` | no | `continue`, `inject` | Inspect the user message and optionally inject context |
| `on_message_complete` | `privacy.llm_content` | no | `continue` | Observe completed assistant responses |
| `on_compaction` | `privacy.llm_content` | no | `continue` | Observe completed conversation compaction summaries |
| `on_session_start` | `session.lifecycle` | no | `continue`, `inject` | Observe session creation and optionally inject session-stable context |
| `on_session_end` | `session.lifecycle` | no | `continue` | Observe session shutdown and transcript |
| `on_context_archived` | `privacy.llm_content` | no | `continue` | [Notify-only](#notify-only-hooks): a context window was sealed into an archive file |
| `on_memory_forgotten` | `session.lifecycle` | no | `continue` | [Notify-only](#notify-only-hooks): `memory_forget` forgot an archive or a note |
| `sandbox_policy` | `sandbox.policy` | no | none: the [answer](#sandbox_policy) is the result | Request/answer, asked once when a session starts under write confinement: which extra directories the session may write |

Unsupported result actions are ignored fail-open and logged as warnings.

## Result behavior

- `continue` lets SynapsCLI proceed.
- `block` prevents the hooked operation and surfaces the reason. It is accepted
  only on `before_tool_call`.
- `confirm` asks the runtime to get explicit user confirmation before proceeding.
  It is accepted only on `before_tool_call`. Interactive TUI streams prompt the
  user; headless/non-interactive call sites fail closed by blocking the tool call.
- `inject` accumulates context from all matching handlers, kept apart per
  extension. It is accepted on `before_message` and on `on_session_start`
  (appended to the system prompt once, session-stable). On `before_message`
  content is **saved** by default: written once into the stored
  conversation on the user message the hook ran for, fenced with the
  extension id, and ordinary history from then on. `"forget": true` (or a
  `forget` entry in `parts`) shows it at the tail of every request of the
  turn instead, never stored, and saves a marker `[<extension id> context
  was shown here for one turn and not kept]`. A saved part over 16 KiB is
  handled as `forget`. A message is hooked once, ever (also when no
  handler was registered then): a resend or resumed turn does not run the
  hook again (forget content is not shown again; the marker stays). Only the newest user message is hooked (the last of
  several steers; a rollover or the preamble nudge does not re-hook).
  Framing look-alikes in content are escaped with a `\` before their `[`.
  Hosts with this behaviour list `inject.forget` in the `initialize`
  params' `features`. See `protocol.md` §`inject` for the placement
  contract, the `_synaps_hooked` count that tells saved blocks from user
  text in transcripts, and a migration note.
- `modify` replaces the tool input before execution. It is accepted only on
  `before_tool_call`. Only `block` stops the chain: later handlers see the
  modified input. See `protocol.md` §Handler chain order.
- `replace` substitutes the tool **output** after execution, before it enters
  conversation history. It is accepted only on `after_tool_call`, and requires
  the **`tools.transform_output`** permission **in addition to** `tools.intercept`
  (subscribing/observing needs `tools.intercept`; rewriting the output needs the
  second key — a Replace from an extension lacking `tools.transform_output` is
  ignored and the original output is preserved). Return
  `{"action": "replace", "output": "<string>"}`. The first handler to return
  `replace` wins and stops the chain (later handlers are skipped). Use it for
  compression, redaction, or summarization. Notes:
  - The `tool_output` delivered in the event is a **size-limited preview** for
    large outputs — up to 256 KB (`MAX_HOOK_OUTPUT = 256 * 1024`, matching
    `max_tool_buffer`) plus a `…[truncated, N total bytes]` marker when the
    output exceeds that cap. A transform decides on this preview, not the full bytes.
  - A malformed `replace` (missing/invalid `output`) or a crashed/timed-out
    handler is fail-safe: the **original output is preserved unchanged**.
  - `replace` does **not** clear the `is_error` flag — replacing the content of
    a failed tool call leaves it marked as an error.
  - Replacement output larger than 1 MiB is clamped.

`on_message_complete` fires after an assistant response is added to session
history (an empty reply after tool calls is not added, and still fires). It
requires `privacy.llm_content` and is observe-only. The `message` field contains concatenated assistant text blocks when present; tool-use blocks
are not serialized into `message`. Summary metadata is available in `data`,
including `content_block_count` and `has_tool_use`. It fires once per assistant
message. `has_tool_use: false` marks a message that asked for no tools; its
`message` may be empty. It is not a reliable "turn done" signal: a turn can have
more than one such message (a steering message, or the Muse nudge, continues the
turn after one), and a turn that stops after a tool round (cancelled, out of a
turn budget, or on an error) ends on an event with `has_tool_use: true`.

`on_compaction` fires after manual conversation compaction creates a replacement
session. It requires `privacy.llm_content` and is observe-only. The `message`
field contains the compaction summary. The `session_id` field is the new session
id, and `data` includes `old_session_id`, `new_session_id`, `message_count`, and
`source`.

`block`, `confirm`, and `modify` all stop the `before_tool_call` handler chain in
registration order. Put high-priority security policy extensions earlier in plugin
load order, and avoid granting trust to plugins that can modify tool inputs unless
you accept this ordering behavior.

## Required guards

A guard is an extension whose `before_tool_call` hook blocks calls it does not
like. Left alone, a guard that is missing, switched off, crashed or silent
leaves every call unguarded: the chain skips a handler that is not there.
`required_guards` in the config file turns that around:

```
required_guards = example-guard, other-guard
```

The value is a comma list of plugin ids. It is the owner's to set, by editing
the config file: no product path writes it (the config writer and the
`/settings` dialog both refuse the key) and it is not a `/settings` row,
because a list the product could rewrite would let the thing it guards
against switch the guard off. That is all the engine does, though: it does not
write-protect the config file, the guard's folder or `synaps daemon reload`,
so until tool calls are confined the guard must refuse those itself (see
[Not covered](#not-covered)). Absent or empty, nothing changes: no gate, as
before the key existed, and a daemon says so in its log at every start
(`daemon: no required guards configured ...`, a warning) as well as in
`synaps daemon status`. The last `required_guards` line in a file wins. With a
profile active, the list is the default config file's and the profile's
together (the default file's names first, once each): a profile's config can
add a guard and never removes one the default file names, and the first
setting the product writes under a profile, which creates that profile's file
with that one key, does not drop the default file's list. The list is read
when the engine starts (a daemon picks up a change at its next reload or
restart) and only grows within a running process.

With guards required:

- **A guard that is not loaded refuses every tool call.** The refusal names
  the guard and the cause: no extension host in this process, extensions
  started off, discovery not finished, no folder of that name, a
  `disabled_plugins` entry, a folder whose name discovery skips
  (`example-guard.disabled`, `.bak`), a folder under a project's
  `.synaps/plugins/` that would override the base one (a required guard loads
  only from the base directory's `plugins/`), a bad manifest, a failed start,
  a manifest that subscribes no `before_tool_call`, an unload, or an exited
  process. A required guard always starts when extensions load, even where
  other extensions start on first use, so there is always a process to check.
- **The check is made twice,** before the handler chain and again after it,
  so a guard unloaded or reloaded while a call was in the chain does not let
  that call through on a stale answer.
- **A guard that is loaded but gives no usable answer blocks the call.** No
  answer within the 5 s hook timeout, a JSON-RPC error, a failed call, or an
  answer that is not a result `before_tool_call` accepts: for a required
  guard the call is refused, naming the guard and the cause, whatever the
  manifest's `on_failure` says. A guard that is not required keeps its
  manifest's `on_failure` (default `"continue"`).
- **Guards run last.** In a `before_tool_call` chain the required guards run
  after every other extension's hook (in registration order among
  themselves; everything else keeps its order), so a `modify` from another
  extension reaches a guard before it answers, and the guard vets the input
  the tool would run. The other hooks are asked first, so they see a call
  that a guard then refuses. A required guard's own `modify` after another
  required guard has answered blocks the call: that guard would never see the
  input it ran.
- **A guard subscribes with no filter.** A required guard's
  `before_tool_call` subscription should have no `tool` or `match` filter.
  The load check counts any `before_tool_call` subscription as loaded, a
  filtered one included, and a call the filter does not match never reaches
  the guard, so a filtered guard is loaded and silent for every other tool.
- **Subagent workers are covered** (see [Subagent workers](#subagent-workers)).
- **Restoring the guard clears it.** Put the folder back, fix the manifest or
  remove the `disabled_plugins` entry, then `synaps daemon reload-extension
  <id>` or reload the daemon.

`synaps daemon status` shows whether the daemon requires any guard and, for
each, whether it is loaded (see [daemon-mode.md](../daemon-mode.md)).

### Not covered

What required guards do not do:

- unconfined, an agent can edit or remove a guard's files; only confining its tool calls (a sandbox) makes them unwritable.
  The same goes for the config file and
  for `synaps daemon reload`: the engine write-protects none of them, so a
  guard has to refuse a call that edits its own folder or the config file, or
  that reloads the daemon, itself.
- An agent rewriting a guard into an allow-all guard that still loads.
- A subagent started where the calling chat's scope is not set (the stream
  sets it around each tool call it runs; one test pins that scope, none pins
  that every stream lane sets it) falls back to the same chat's hook bus with
  auto-approve off: stricter, and still guarded.
- The check made before the chain has no timeout. A guard whose state cannot
  be read makes the call hang; it never lets it through.
- `server.auto_approve_confirms` is the owner's setting and predates required
  guards. When it is on, every `confirm` any extension answers is approved
  unasked, a guard's included.
- Commands the owner types (slash commands and shell commands) are not tool
  calls and do not pass through `before_tool_call`; nor do plugin slash
  commands that an extension tool backs.
- Windows: the daemon reload there starts a new process (see the Windows
  section of [daemon-mode.md](../daemon-mode.md)); this was built and tested
  on Linux only.
- On a case-insensitive file system a folder that differs from a guard's id
  only in case is not read as that guard's folder when a refusal names its
  cause, so the cause text may say the guard's folder is absent. The call is
  refused all the same.

## Notify-only hooks

`on_context_archived` and `on_memory_forgotten` tell you that something has
already happened. They are sent after the fact, and Synaps does not wait for
you:

- **Nothing you do changes or delays it.** The host hands the event to a
  background task and carries on at once. A handler that is slow, hangs,
  crashes, answers with an error, or is not there at all leaves the rollover
  or the forget exactly as it would have been, and no slower.
- **Your answer is discarded.** Return `{"action": "continue"}`. Anything
  else is ignored.
- **Best effort.** An event is sent once and not retried. It is lost when
  your process is down and cannot be started within the call, when the call
  takes longer than 5 s, when 64 notify-only deliveries are already in
  flight (a handler that is slow while events keep coming does not build
  a queue in the host; the extra events are dropped and logged), and when
  the host exits first. The events for the parts of one window (below) are
  one delivery however many they are, so a window in many parts cannot use
  up the 64 by itself.
- **Unordered, and sometimes twice.** Events are delivered independently, so
  two of them can arrive in either order. The one exception is the parts of
  one window, which are sent one after another, first part first. If your
  process dies mid-call it is restarted and sent the event again. Make the
  handler idempotent.
- **One timeout ends the delivery for you.** When your handler does not
  answer an event within 5 s, the remaining events of that delivery (the
  later parts of that rollover's window) are not sent to you; the host logs
  how many. Other extensions still get all of them. Nothing sends them
  later, so you must be able to catch up by listing the store.
- **Answer at once and do the work afterwards.** Calls to one extension are
  made one at a time, so while your handler sits on a notify-only event your
  other hooks (a `before_tool_call`, say) wait behind it, each for up to its
  own 5 s. Put the event on your own queue and reply.
- **The files are the truth; the hook is a hint.** If your index must be
  complete, read the store when you start and treat events as the fast path.
- **Not sent for subagent workers.**

**Compatibility.** These hook kinds are additive within protocol version 1:
an extension that does not name them is unaffected. An extension that does
name one fails to load on a Synaps that predates it (`Unknown hook kind`), so
a plugin that subscribes needs a new enough host.

### `on_context_archived`

When a chat gets too long, automatic context management (`/context auto`)
seals the older messages into an archive file and the chat continues in a
fresh window. This hook fires for each archive file of a window, once the
chat has moved to the new window: every file of the old window is written
and the new window, which names them, is saved. Requires
`privacy.llm_content`: the event carries no conversation text, but its
`path` leads to it.

A window is normally one archive and one event (`part` 1 of `parts` 1). A
window over what one archive may hold (4,096 messages, 16 MiB, 65,536 visited
nodes, or a tool input nested too deep) is sealed as several archives,
"parts", and you get one event per part: see [A window in several
parts](#a-window-in-several-parts).

```json
{
  "kind": "on_context_archived",
  "tool_name": null,
  "tool_runtime_name": null,
  "tool_input": null,
  "tool_output": null,
  "message": null,
  "session_id": "20260501-120000-abcd",
  "cwd": "/home/u/project",
  "transcript": null,
  "data": {
    "archive_id": "9f2c1a7e04b35d6688c1f0a2b7d94e13",
    "path": "/home/u/.synaps-cli/context-archives/<64 hex>/s000001/9f2c1a7e04b35d6688c1f0a2b7d94e13.json",
    "project_key": "p0123456789abcdef",
    "message_count": 212,
    "source_message_count": 240,
    "window": 4,
    "part": 1,
    "parts": 1,
    "first_source_index": 0,
    "reduced_messages": 0
  }
}
```

| Field | Meaning |
|---|---|
| `session_id` | The chat the window belonged to. Always set; `SYNAPS_HOOK_SESSION_ID=0` does not apply. |
| `cwd` | That chat's working folder (see below). |
| `data.archive_id` | 32 lowercase hex characters. The model sees it as `ctx-<archive_id>` in `memory_search` and `memory_fetch`. |
| `data.path` | Absolute path of the file that holds the archive, or `null` if it could not be located or is not valid UTF-8. |
| `data.project_key` | The project the archive belongs to (`p` and 16 hex characters). |
| `data.message_count` | Messages stored in this archive. |
| `data.source_message_count` | Messages of the window this archive covers, before filtering; system messages, reasoning and restricted content are not archived. For a window in one archive that is the whole window. |
| `data.window` | Number of the window that starts after this archive. The same on every part of one window. |
| `data.part`, `data.parts` | Which archive of the window this is, counting from 1, and how many there are. Both are `1` for a window that fits one archive. |
| `data.first_source_index` | Index in the window of the first message this archive covers; it covers `source_message_count` messages from there. `0` for the first part. |
| `data.reduced_messages` | Messages in this archive that are stored with an oversized payload left out. `0` unless a single message was too large for one archive by itself. |

#### A window in several parts

For a window sealed as several archives the host sends one event per part,
all after the chat has moved to the new window, as one delivery: first part
first, each handed to your handler once it has answered the one before. If
it does not answer one within 5 s, the parts after it are not sent to you
(see above). Each extension gets that sequence on its own, so
another extension that is slow, stuck or failing does not delay yours.
Every event has the same `session_id`, `cwd`, `project_key`, `window` and
`parts`, and its own `archive_id`, `path`, counts, `part` and
`first_source_index`.

- **The parts are consecutive and cover the window.** Part `n + 1` starts
  where part `n` ends: its `first_source_index` is the previous part's
  `first_source_index + source_message_count`. Inside an archive file each
  message's `source_index` counts from that part's first message, so the
  message's index in the window is `first_source_index + source_index`.
- **The archive files do not say they belong together.** Each part is an
  ordinary archive, and nothing in the file names the others or its place in
  the window. These events, and the chat's saved session (below), are the
  only record of how a window was cut. If you need to put a window back
  together later, keep `part`, `parts` and `first_source_index` when you get
  them.
- **A missing part is visible.** Delivery is still best effort, so check that
  you have `parts` events for a `(session_id, window)` if it matters.
- **Two parts can be the same archive.** Parts with identical content (the
  same messages repeated) have the same `archive_id` and `path`; you get one
  event per part, with different `part` and `first_source_index`.
- **A reduced message is marked in the file.** `reduced_messages` counts
  messages that were too large for an archive by themselves and are stored
  with the oversized payloads left out. In the file such a message carries
  the key `_archive_reduced`, and each omission is replaced by a marker that
  says how large it was. Everything else in the window is stored exactly.
- **A window is announced whole or not at all.** If sealing stops between
  two parts (a full disk, a store that stays locked, a cancelled turn) no
  event is sent. The parts already written stay on disk as ordinary
  archives that nothing refers to. The rollover that does complete
  announces every part it recorded; a part whose content is already on disk
  is found there and not written again.

The chat's saved session holds the same list. The first message of the new
window carries host metadata, `_synaps_context`, in which `archive` is the
id of the last part and, for a window in several parts or with a reduced
message, `archives` lists every part in order as
`{"id", "first", "count", "messages"}` (plus `"reduced"` when it is not 0):
the archive id, `first_source_index`, `source_message_count` and
`message_count` of the events above. That list is replaced when the chat
next rolls over.

What it means, and what it does not:

- **"The chat has left this window, and this file holds it."** It is sent
  once every archive file of the window is written and synced, the new
  window that names them is saved, and the chat has gone on in it. A
  rollover that is given up before that (the seal failed, the turn was
  cancelled, the new window could not be saved) sends nothing, so you are
  not told about a window and then told again by the attempt that follows.
- **Key on `archive_id`, not on `(session_id, window)`.** While a chat
  stays open, a `(session_id, window)` is announced by the one rollover
  that completed. Across a resume it is not unique: a chat resumed in
  another project, or while the archive its saved window names could not
  be read, goes on at window 1, and its next rollover announces window 2
  again, for other archives.
- **The same `archive_id` can still arrive more than once:** two parts with
  identical content are one archive, an event is sent again if your process
  died while handling it, and a session file put back from a backup can
  roll the same window over again. Key your work on `archive_id`.
- **An archive can exist that was never announced:** the rollover that wrote
  it was given up before the new window was saved (cancelled, the save
  failed, sealing stopped between two parts); the host exited after saving
  the new window and before the events went out, in which case the chat's
  saved session names the archive all the same and nothing sends the events
  later; the event was dropped; or no extension was listening. List the
  store when you start.
- **Not sent when the seal fails.** The chat keeps its history and no event
  goes out.
- **Not for subagent workers**, whose windows are saved beside their own
  transcripts, and not for `/compact`, which is `on_compaction`.
- **Treat the file as read-only.** It is written once and never changed,
  except that forgetting the archive replaces its content with a tombstone.

### `on_memory_forgotten`

Fires after the `memory_forget` tool has forgotten something: an archived
context window (the model calls it `ctx-<id>`) or a note. Requires
`session.lifecycle`; the event carries ids only, no content. Use it to drop
the thing from your own index.

```json
{
  "kind": "on_memory_forgotten",
  "tool_name": null,
  "tool_runtime_name": null,
  "tool_input": null,
  "tool_output": null,
  "message": null,
  "session_id": "20260501-120000-abcd",
  "cwd": "/home/u/project",
  "transcript": null,
  "data": {
    "id": "9f2c1a7e04b35d6688c1f0a2b7d94e13",
    "kind": "archive",
    "project_key": "p0123456789abcdef",
    "scope": "repository"
  }
}
```

| Field | Meaning |
|---|---|
| `session_id` | The chat whose tool call did the forgetting; `null` when the call had no chat behind it. `SYNAPS_HOOK_SESSION_ID=0` does not apply. |
| `cwd` | The folder that tool call ran in: the chat's working folder (see below). |
| `data.kind` | `"archive"` or `"note"`. |
| `data.id` | For an archive, the bare 32 hex characters: the same string as `archive_id` in `on_context_archived`, without the `ctx-` the model writes. For a note, its id exactly as stored. |
| `data.project_key` | The project the archive or note belonged to. |
| `data.scope` | Always `"repository"`. (It could be `"user"` for a user-wide note while Synaps had an Axel memory backend.) |

What it means, and what it does not:

- **Sent only when the forget happened.** The tombstone is written before
  the event goes out. A forget
  that fails sends nothing: an id that does not exist, one that belongs to
  another project, a note already forgotten, a store error.
- **No path.** It describes what the tool did and carries no path.
- **An archive can be announced twice.** Forgetting an archive that is
  already forgotten succeeds again, and is announced again. Key your work on
  `id`.
- **Not every forget is announced.** No event is sent when the forget was
  made by a subagent worker or by the `synaps retention` command, which runs
  without extensions. And like every notify-only event it can be dropped.
- **The tombstone is the record.** Whether or not you were told, the forget
  is on disk: the archive's file now holds `"body": {"state": "tombstone"}`
  instead of its messages, and a forgotten note is a
  `{"tombstone": "<id>", ...}` line in the project's notes file. An index
  that must be exact checks those when it starts.
- **Removal is not announced.** When the retention sweep later deletes a
  tombstoned note for good, no event is sent.

## The capture screen

A capture or memory extension is sent chat text, tool output and memory
metadata by hooks. Core screens that text **before the bus hands it to any
extension**, so a credential that reached a tool result or a message is not
stored by a plugin. The screen is part of core: a manifest, a config entry or a
hook result cannot turn it off or thin it out.

It applies to `after_tool_call`, `before_message`, `on_message_complete`,
`on_compaction`, `on_session_end`, `on_context_archived` and
`on_memory_forgotten`. It does not apply to `before_tool_call` (a guard has to
judge the real call; the payload is never withheld) or `on_session_start` (no
text). It runs for every delivery route of those hooks: the blocking chain, the
concurrent observers, the notify-only deliveries and the subagent worker view.

- **What is withheld.** Text holding a known credential shape (provider key
  prefixes, private-key blocks, JWTs, credential-named values, credentials in
  URLs), an opaque high-entropy token, or one of the extra prefixes in
  `capture_scrub_prefixes`; and serialized thinking, signature or raw tool
  blocks.
- **Dropped, not masked.** A message or text block that needed anything removed
  is left out whole. Your handler never sees a marker standing in for the secret,
  and a transcript (`on_session_end`, `on_compaction`) arrives with only the
  visible text of each message: no `thinking`, `tool_use` or `tool_result`
  blocks, no injected recall or event text.
- **Inline reasoning is cut out.** `<think>…</think>` and
  `<thinking>…</thinking>` spans (any case) are removed from every message,
  reply, summary and tool output, including a subagent's last reply returned as
  a tool result. An opening tag that is never closed hides the rest of the text,
  later blocks of the same message included; a closing tag with no opening one
  hides what came before it.
- **Compaction summaries are not delivered as a `before_message`; event
  messages are, screened, and are kept out of capture transcripts.** A
  `before_message` whose text is a compaction summary (`<context-summary>`) is
  not delivered. One whose text is an agent event (`<event …>…</event>`: a
  `synaps send` event, a subagent's completion notice) is delivered as an
  event, never as the owner's input: `message` is the text the host wrote, its
  `<event …>` header and `source` intact, after the same screen as the user's
  words (a credential withholds it whole, inline reasoning is cut out), and
  `data` is `{"message_kind": "event", "verified": false}` (the host vouches for
  no sender). What you inject on it is saved or shown, and neutralized, as on
  any other message. A transcript leaves both kinds of message out.
- **Capture transcripts exclude extension-supplied context.** The blocks a
  `before_message` hook added to a user message (saved extension context and
  the markers left for turn-only context, told apart from the user's words by
  the host's `_synaps_hooked` count) are left out of every transcript message,
  and so is that count: a message arrives as its `role` and the user's own
  text. A message with none of the user's text left is not delivered. Text the
  user typed that only looks like a saved block is the user's and is screened
  like any other text.
- **The compaction summary needs thinking excluded.** A summary is model text
  written from the conversation; by default the summarizer is also given the
  model's thinking, and the summary can restate it. `on_compaction` therefore
  arrives without its summary text (`message` is absent; the ids, count and
  `source` still arrive) unless the summary's input held no thinking. To receive
  it, add `thinking` to `compaction_exclude` in config. The summary is then sent
  when the conversation also has no inline `<think>` text and any earlier
  summary folded into it was written with thinking excluded too. The payload's
  `data.thinking_excluded` says which case applies.
- **Escapes and the size cap.** Text is also screened with every backslash
  escape (`\n`, `\t`, `\uXXXX`, …) read as a space, so a key printed right after
  an escape in a tool's JSON is still found by its prefix. A tool output over the
  256 KiB hook cap is screened across the cut as well: a key the cut splits is
  withheld, not half delivered.
- **Fail closed.** A payload that cannot be screened (no screen on the bus,
  over the 8 MiB, node or one-second budget, nested too deep, a malformed
  archive id) is not delivered at all. The host logs the hook, the extension and
  the reason, never the content.
- **`capture_scrub_prefixes`** (config, comma separated, default empty) adds
  prefixes (3 to 64 printable ASCII bytes with no space, at most 64 of them) for
  credentials of your own organisation. It only adds: no value removes a rule of
  the screen. Entries that do not fit are ignored with one warning at startup
  that gives their count, never their values.
- **Announced.** A daemon with the screen working lists `capture_scrub_v1` in
  `Welcome.features`; `synaps daemon status --json` prints
  `"capture_scrub": true`. A client that must not send text to a daemon without
  the screen checks that feature first.

The names and ids the host sets on a payload (`tool_name`,
`tool_runtime_name`, `session_id`, `cwd`) are not screened.

What the screening of `after_tool_call` does and does not give you:

- `before_tool_call` is a decision hook and is sent the tool's **input
  unscreened**. It needs the same `tools.intercept` permission as
  `after_tool_call`, so screening the input of `after_tool_call` protects nothing
  against a plugin that subscribes to both.
- An output that holds a secret is withheld from `after_tool_call` whole, so a
  transform (`tools.transform_output`) never sees it and cannot redact it: that
  raw output goes into the chat history unchanged.

What the screen does not cover: it sees text the host puts in a hook payload,
and protects against extensions that use only what hooks send them.
It does not see what an extension reads from disk (session files included),
fetches or computes itself, the arguments of a `tool.call` request, the
`provider.*`, `command.invoke` and `hook.handle` requests an extension answers,
or what an extension sends to the host with `memory.append`, `memory.query` or
`config.*`. A secret that matches no shape and no entropy rule is not detected,
nor is one disguised: split across fields, blocks or messages, or written with
zero-width or lookalike characters. Reasoning in shapes core does not produce
today (structured or double-escaped tags, for example) is not recognised. The
archive file an `on_context_archived` notice points to is not screened: the
notice carries its path, and an extension that opens it reads the archived
conversation as written. Screening a large payload can take up to about a
second, during which the delivering task waits.

## `sandbox_policy`

`sandbox_policy` is not an observation: it is a question the host asks one
plugin, once, when a session starts under write confinement (the owner set
`sandbox.confine = enforce` in the config file, see
[../sandbox.md](../sandbox.md)). It needs the `sandbox.policy` permission.
The host asks only the plugin the owner named in `sandbox.policy_provider`,
and only if that plugin declares this hook **and** holds the permission;
any other plugin that names the hook is never asked. If the named plugin is
not loaded, lacks either key, fails, times out (5 s) or answers anything
else than the shape below, the session is **refused**: it never starts
unconfined. The refusal reads `sandbox policy provider '<id>' <cause>`, the
cause being `missing`, `timeout`, `error`, `bad-answer` or
`refused: <reason>`.

The request is the ordinary `hook.handle` envelope (the same fields as any
other hook) with `kind` set to `sandbox_policy` and three fields beside it:

```json
{
  "kind": "sandbox_policy",
  "tool_name": null,
  "tool_runtime_name": null,
  "tool_input": null,
  "tool_output": null,
  "message": null,
  "session_id": "20260501-120000-abcd",
  "cwd": "/home/u/project",
  "transcript": null,
  "data": null,
  "workspace": "/home/u/project",
  "base_dir": "/home/u/.synaps-cli",
  "parent_session_id": null
}
```

`workspace` is the session's working folder, absolute and canonical.
`base_dir` is the engine's base directory (config, plugins, sessions) as the
engine itself resolves it, absolute and canonical; it is always sent, so a
provider never has to work it out from its own environment.
`parent_session_id` is the engine's own record of the session this one came
from (a compaction or fork successor), `null` for a top-level session. It
comes from the engine's session record only: nothing an agent, a worker, a
tool argument or the configuration says can set it.

The **result is the answer** (not a `HookResult`):

```json
{ "write": ["/home/u/scratch"], "read": [] }
```

or a refusal:

```json
{ "refuse": "no policy for this project" }
```

- `write` lists extra directories the session may write, each absolute and
  already canonical (no `..`, no symlink on the way, no NUL, control, bidi
  or zero-width character). The answer can only add: the workspace and a
  small baseline (`/tmp`, `/dev/null`, ...) are always writable, and the
  host's own protected set (the engine's base directory, credential stores,
  autostart folders, shell start-up files, the folders on `$PATH`, `$HOME`)
  never is. The host **drops** every entry that is equal to, under or above
  a protected path, or that is a symlink, and reports each drop with its
  reason (`synaps sandbox check` lists them). An entry that is swapped for a
  link between validation and use is dropped too.
- `read` is accepted and shown by `synaps sandbox check`; v1 does not
  enforce it (`read: not enforced (v1)`).
- `refuse` makes the session refuse with the cause `refused: <reason>`; the
  reason is stripped of control and bidi characters and cut at 200
  characters. A JSON-RPC error answer is the cause `error`, a timeout
  `timeout`, and a garbage answer `bad-answer`.

The host announces that it can ask this question as the `Welcome` feature
`sandbox_policy_v1` (Linux builds).

## `cwd`: the chat's working folder

Every hook event carries `cwd`, and so does every `tool.call` request
(`params.cwd`). It is the folder the chat runs in: the one its tools resolve
relative paths against and start commands in. Use it to scope what your
extension does by project. In daemon mode one extension process serves every
chat, so your own process folder says nothing about which project a call is
for; `cwd` does.

| Where | `cwd` |
|---|---|
| `before_message`, `on_message_complete`, `before_tool_call`, `after_tool_call` | the folder of the chat that fired the hook |
| `on_session_start`, `on_session_end`, `on_compaction` | the folder of that session |
| `on_context_archived` | the folder of the chat whose window was archived |
| `on_memory_forgotten` | the folder of the chat whose `memory_forget` call it was |
| `tool.call` `params.cwd` | the folder of the chat (or worker) that called your tool |

Which folder that is:

- **A chat hosted by the daemon** (`synaps daemon`, the TUI attached to it):
  the folder the client was started in, or the one it asked for; not the
  daemon's own folder.
- **An in-process host** (`synaps chat`, `synaps rpc`, the TUI without a
  daemon): the folder Synaps was started in.
- **A subagent worker:** the folder its parent chat runs in. A worker resumed
  with `subagent_resume` keeps the folder it was first started in, while that
  folder exists.
- **A resumed chat:** `--continue` of a chat that is not running takes the
  folder of the client that resumed it, which is where its tools now run and
  can differ from the folder the chat was created in. Attaching to a chat
  that is still running, or waking a parked one, does not change its folder.
- **No folder of its own:** the host process's folder, which is also where
  the tools run. Two cases: a pinned chat the daemon restores, and a chat
  the daemon carries through `synaps daemon reload`, after its folder was
  deleted. It reports the daemon's folder.

The calls a chat causes outside its own turn loop carry its folder too:

- **Tools run by an extension-provided model.** A model your extension
  serves (`plugin:provider:model`) has its tool calls run by the host in a
  loop of their own. They run in the chat's folder, and the
  `before_tool_call` / `after_tool_call` events and `tool.call` requests
  they cause carry the chat's `cwd` and `session_id`.
- **A plugin slash command.** An extension tool run by `/plugin:command`
  gets the `cwd` and `session_id` of the chat the command was typed in.
- **The host's own calls to a memory extension** (capture and recall for a
  chat with memory on) carry that chat's `cwd`. Their `session_id` is
  `null`: the host makes them, not the chat's model.

Rules:

- **Always present.** The key is never omitted. The value is a string, or
  `null` in two cases: the folder's path is not valid UTF-8; or the folder
  to report is the host process's own (an in-process host, or the pinned
  chat above) and the process can no longer read it, for instance because
  it was deleted. `SYNAPS_HOOK_SESSION_ID=0` does not touch it, and workers
  have it although their `session_id` is `null`.
- **As given: Synaps does not canonicalise it.** The value is the folder as
  the client gave it. That is normally the client's own working directory
  as the operating system reports it, in which the OS has already resolved
  symbolic links; a client that names a folder itself can spell it any way.
  Synaps passes it on unchanged and does not walk it up to a repository
  root. So one folder can arrive under two spellings: on macOS `/tmp/x` and
  `/private/tmp/x`; on Windows case and `\` vs `/` vary. If you key state on
  `cwd`, canonicalise it yourself, and find the repository root yourself if
  that is what you mean by "project".
- **Additive.** Hosts before this field omit the key. Treat a missing key
  like `null`. An extension that ignores `cwd` is unaffected.

`cwd` is not the same thing as the `host_context: "project_root"` config
value: that one is resolved once per extension process from the host's own
folder, so under the daemon it is the daemon's folder for every chat.

## Subagent workers

A subagent worker's tool calls run only one kind of hook: `after_tool_call`
for extensions that hold **`tools.transform_output`**, so output transforms
(compression, redaction) see worker tool results the way they see the main
session's. Nothing else fires for a worker's tool calls: no `before_tool_call`
(so a worker never raises a confirm prompt and an extension can't block or
modify its input), no observe-only `after_tool_call` (`tools.intercept`
without `tools.transform_output`), no message or session hooks, no
notify-only hooks.

- **Same rules as the main session:** the handler chain order, the first
  `replace` wins, the two-key permission gate, the 5 s timeout, the 1 MiB
  clamp, and fail-safe pass-through (a timed-out, crashed or malformed handler
  leaves the original output). The order inside the worker's tool loop is the
  main session's too: your `replace`, then tool output virtualisation (the
  spill file holds your output), then read dedupe.
- **`session_id` is `null`.** A worker has no conversation id (see
  `session-id.md`). Treat `null` as "a worker or no session".
- **`cwd` is set.** It is the folder the worker runs in: its parent chat's
  (see "`cwd`: the chat's working folder" above).
- **One process for everyone.** The main session and all its workers call the
  same extension process; calls are serialized per extension and matched by
  JSON-RPC id, so parallel workers each get the reply to their own call. A
  call that waits behind a slow one still has its own 5 s budget, then passes
  through.
- **Workers never start or restart your process.** While it is down
  (crashed, or deferred and not yet started by the main session), a worker's
  tool result passes through untransformed; the main session's next call
  starts it as before. A call in flight when a reload stops the old instance
  passes through; a reloaded or newly loaded extension applies to running
  workers from their next tool call.
- **Opt-out:** `subagent_transform_hooks = off` (default on; a /settings
  row) runs no hooks at all for workers, as before, unless a guard is
  required (below).

### With required guards

While the config's [`required_guards`](#required-guards) is not empty, a
worker's `before_tool_call` is no longer skipped: it runs the chain of the chat
that started the worker.

- The caller's `before_tool_call` handlers run for each of the worker's tool
  calls, on the caller's runtime, so a guard's process is started and
  restarted there and not on the worker's short-lived one. The result,
  including a `block`, is the caller's.
- The event carries the caller's `session_id` (the worker has none of its
  own) and a `subagent_id` (`sa_N`, the worker's id as its caller sees it);
  `subagent_id` is absent on events from the main session. A guard can tell a
  worker's call from its caller's, and key its state on either.
- A `confirm` is answered the way the caller's own would be: its prompt, its
  auto-approve setting, and its "Allow all" latch.
- The required-guard gate applies on the worker's bus too, so a guard that is
  not loaded refuses the worker's calls as it refuses the main session's.
- `subagent_transform_hooks = off` does not get a worker out of this: it
  turns off the worker's output transforms, never its guards.
- A worker's own subagents run the same chain: the route is to the original
  caller, not to the (empty) hook list of an intermediate worker.
- Output transforms (`after_tool_call`) are unchanged.

With nothing required, workers behave as described above.
