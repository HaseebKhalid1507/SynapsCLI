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

## Subagent workers

A subagent worker's tool calls run only one kind of hook: `after_tool_call`
for extensions that hold **`tools.transform_output`**, so output transforms
(compression, redaction) see worker tool results the way they see the main
session's. Nothing else fires for a worker's tool calls: no `before_tool_call`
(so a worker never raises a confirm prompt and an extension can't block or
modify its input), no observe-only `after_tool_call` (`tools.intercept`
without `tools.transform_output`), no message or session hooks.

- **Same rules as the main session:** the handler chain order, the first
  `replace` wins, the two-key permission gate, the 5 s timeout, the 1 MiB
  clamp, and fail-safe pass-through (a timed-out, crashed or malformed handler
  leaves the original output). The order inside the worker's tool loop is the
  main session's too: your `replace`, then tool output virtualisation (the
  spill file holds your output), then read dedupe.
- **`session_id` is `null`.** A worker has no conversation id (see
  `session-id.md`). Treat `null` as "a worker or no session".
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
