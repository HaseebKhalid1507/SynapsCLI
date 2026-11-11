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

Unsupported result actions are ignored fail-open and logged as warnings.

## Result behavior

- `continue` lets SynapsCLI proceed.
- `block` prevents the hooked operation and surfaces the reason. It is accepted
  only on `before_tool_call`.
- `confirm` asks the runtime to get explicit user confirmation before proceeding.
  It is accepted only on `before_tool_call`. Interactive TUI streams prompt the
  user; headless/non-interactive call sites fail closed by blocking the tool call.
- `inject` accumulates text from all matching handlers and injects it into the
  model context. It is accepted on `before_message` (attached to the newest
  user message as a trailing ephemeral text block, positioned after the
  conversational cache marker — never the system prompt) and on
  `on_session_start` (appended to the system prompt once, session-stable).
  See `protocol.md` §`inject` for the placement contract.
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
history. It requires `privacy.llm_content` and is observe-only. The `message`
field contains concatenated assistant text blocks when present; tool-use blocks
are not serialized into `message`. Summary metadata is available in `data`,
including `content_block_count` and `has_tool_use`.

`on_compaction` fires after manual conversation compaction creates a replacement
session. It requires `privacy.llm_content` and is observe-only. The `message`
field contains the compaction summary. The `session_id` field is the new session
id, and `data` includes `old_session_id`, `new_session_id`, `message_count`, and
`source`.

`block`, `confirm`, and `modify` all stop the `before_tool_call` handler chain in
registration order. Put high-priority security policy extensions earlier in plugin
load order, and avoid granting trust to plugins that can modify tool inputs unless
you accept this ordering behavior.

## Notify-only hooks

`on_context_archived` tells you that something has already happened. It is
sent after the fact, and Synaps does not wait for you:

- **Nothing you do changes or delays it.** The host hands the event to a
  background task and carries on at once. A handler that is slow, hangs,
  crashes, answers with an error, or is not there at all leaves the rollover
  exactly as it would have been, and no slower.
- **Your answer is discarded.** Return `{"action": "continue"}`. Anything
  else is ignored.
- **Best effort.** An event is sent once and not retried. It is lost when
  your process is down and cannot be started within the call, when the call
  takes longer than 5 s, when 64 notify-only events are already being
  delivered (a handler that is slow while events keep coming does not build
  a queue in the host; the extra events are dropped and logged), and when
  the host exits first.
- **Unordered, and sometimes twice.** Events are delivered independently, so
  two of them can arrive in either order. If your process dies mid-call it is
  restarted and sent the event again. Make the handler idempotent.
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
fresh window. This hook fires each time such a file has been written.
Requires `privacy.llm_content`: the event carries no conversation text, but
its `path` leads to it.

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
| `data.message_count` | Messages stored in the archive. |
| `data.source_message_count` | Messages in the window before filtering; system messages, reasoning and restricted content are not archived. |
| `data.window` | Number of the window that starts after this archive. |
| `data.part`, `data.parts` | Which archive of the window this is, counting from 1, and how many there are. A window is one archive today, so both are `1`. A later version may seal an oversized window as several archives and then sends one event per part, with the same `window`, in any order. |
| `data.first_source_index` | Index in the window of the first message this archive covers. `0` today. |
| `data.reduced_messages` | Messages stored with an oversized payload left out. `0` today. |

What it means, and what it does not:

- **"This archive file now exists."** It is sent once the file is written and
  synced. It does not promise the chat went on in the new window: saving the
  new window can still fail or be cancelled, and a later attempt seals the
  window again.
- **The same `archive_id` can be announced more than once.** Sealing the same
  window again finds the archive already written, writes nothing, and
  announces it again. Key your work on `archive_id`.
- **An archive can exist that was never announced:** the rollover was
  cancelled just after the file was written, the event was dropped, or no
  extension was listening. List the store when you start.
- **Not sent when the seal fails.** The chat keeps its history and no event
  goes out.
- **Plain-file memory backend only.** With `memory.backend = axel` there is
  no archive file and no event.
- **Not for subagent workers**, whose windows are saved beside their own
  transcripts, and not for `/compact`, which is `on_compaction`.
- **Treat the file as read-only.** It is written once and never changed,
  except that forgetting the archive replaces its content with a tombstone.

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
  the tools run. That covers tools run by an extension-provided model's own
  tool loop, a `tool.call` with no chat behind it, and a pinned chat the
  daemon restores after its folder was deleted.

Rules:

- **Always present.** A string, or `null` when the folder is not valid
  UTF-8 or cannot be read. Never omitted. `SYNAPS_HOOK_SESSION_ID=0` does not
  touch it, and workers have it although their `session_id` is `null`.
- **As given, not canonical.** The path is the client's spelling: symlinks
  are not resolved and it is not walked up to a repository root. On macOS
  `/tmp/x` and `/private/tmp/x` are one folder; on Windows case and `\` vs
  `/` vary. If you key state on `cwd`, canonicalise it yourself, and find the
  repository root yourself if that is what you mean by "project".
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
  row) runs no hooks at all for workers, as before.
