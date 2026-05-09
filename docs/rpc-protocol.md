# synaps rpc — Wire Protocol Reference

> See also: `synaps-bridge.SPEC.md §4` for the canonical specification.

## Overview

`synaps rpc` is a headless line-JSON IPC server.  The bridge daemon spawns one
child process per Slack thread and communicates with it over the child's
`stdin` / `stdout` pipes.

**One process = one synaps session.**  The child reuses the same `Runtime`,
`Session`, and streaming engine that `synaps server` uses, so tool execution,
MCP, skills, and session persistence all work identically.

---

## §1 — Framing

| Property       | Value                                              |
|----------------|----------------------------------------------------|
| Encoding       | UTF-8                                              |
| Format         | Line-delimited JSON (LDJSON / NDJSON)              |
| Frame boundary | One JSON object per line, terminated by `\n` (0x0A)|
| Envelope       | None — no `Content-Length` or binary header        |
| Max frame size | **1 MiB** (1 048 576 bytes) inbound                |

- The parent writes [`RpcCommand`] frames to the child's **stdin**.
- The child writes [`RpcEvent`] frames to the child's **stdout**.
- **stdout is reserved for protocol frames only** — no diagnostics or logging
  ever appear there.  All `tracing::*` output goes to the log file / stderr.

---

## §2 — Version and Handshake

The current protocol version is **`1`** (`RPC_PROTOCOL_VERSION`).

Immediately after startup, before accepting any commands, the child emits a
`ready` event on stdout:

```json
{"type":"ready","session_id":"<uuid>","model":"<model-id>","protocol_version":1}
```

The parent **must** verify `protocol_version == 1` before proceeding.  If the
version does not match, the parent should kill the child and report an error.

---

## §3 — Commands (parent → child)

All commands are JSON objects with a `"type"` discriminant.  All except
`shutdown` carry an `"id"` string that the child echoes in the matching
`response` event.

| `type`                 | Extra fields                                      | Description                              |
|------------------------|---------------------------------------------------|------------------------------------------|
| `prompt`               | `id`, `message`, `attachments?`                   | Send a new user message                  |
| `follow_up`            | `id`, `message`                                   | Continue the conversation                |
| `compact`              | `id`                                              | Summarise and compress history in-context|
| `new_session`          | `id`                                              | Discard history, start a fresh session   |
| `get_messages`         | `id`                                              | Return the full message history          |
| `set_model`            | `id`, `model`                                     | Switch the active model                  |
| `get_available_models` | `id`                                              | List all provider models                 |
| `abort`                | `id`                                              | Cancel an in-flight stream               |
| `get_session_stats`    | `id`                                              | Token usage, cost, message count         |
| `get_state`            | `id`                                              | Streaming flag, model, session id        |
| `shutdown`             | *(none)*                                          | Save session and exit 0                  |

### `prompt` — `attachments` field

`attachments` is an optional array of objects:
```json
{"path": "/tmp/file.txt", "name": "file.txt", "mime": "text/plain"}
```
`name` and `mime` are optional.  In v0 the child does **not** read file bytes —
it prepends a textual note to the user message:
`[user attached files: /tmp/file.txt, ...]`.  Full binary attachment support is
planned for Task 10.

---

## §4 — Events (child → parent)

| `type`           | Key fields                                        | Description                              |
|------------------|---------------------------------------------------|------------------------------------------|
| `ready`          | `session_id`, `model`, `protocol_version`         | Startup handshake (first frame emitted)  |
| `message_update` | `event` (see §4.5)                                | Streaming delta from the assistant       |
| `subagent_start` | `subagent_id`, `agent_name`, `task_preview`       | A subagent was spawned                   |
| `subagent_update`| `subagent_id`, `agent_name`, `status`             | Intermediate subagent status             |
| `subagent_done`  | `subagent_id`, `agent_name`, `result_preview`, `duration_secs` | Subagent finished        |
| `agent_end`      | `usage` (see §4.6)                                | Turn complete; final token-usage summary |
| `response`       | `id`, `command`, *(flattened body fields)*        | Reply to a specific command              |
| `error`          | `id?`, `message`                                  | Protocol or runtime error                |

### §4.5 — `message_update` event types

The `event` field is a tagged union:

| `event.type`           | Extra fields                  | Description                    |
|------------------------|-------------------------------|--------------------------------|
| `text_delta`           | `delta`                       | Incremental assistant text     |
| `thinking_delta`       | `delta`                       | Incremental thinking fragment  |
| `toolcall_start`       | `tool_id`, `tool_name`        | Tool call started streaming    |
| `toolcall_input_delta` | `tool_id`, `delta`            | JSON fragment of tool input    |
| `toolcall_input`       | `tool_id`, `input`            | Final complete tool input      |
| `toolcall_result`      | `tool_id`, `result`           | Tool execution result          |

Events intentionally **not** forwarded: `ToolResultDelta` (no wire variant),
`SteeringDelivered` (internal hook signal).

### §4.6 — `usage` object (inside `agent_end`)

```json
{
  "input_tokens": 1234,
  "output_tokens": 567,
  "cache_read_input_tokens": 0,
  "cache_creation_input_tokens": 0,
  "model": "claude-opus-4-5"
}
```

---

## §5 — Error handling

| Situation                        | Child behaviour                                              |
|----------------------------------|--------------------------------------------------------------|
| Inbound frame > 1 MiB            | Emit `error` with `id: null`; **stay alive**                 |
| Malformed JSON                   | Emit `error` with `id: null`; **stay alive**                 |
| Runtime error during `prompt`    | Emit `error` with `id: <prompt-id>`, then `agent_end`, then `response {ok: false}` |
| `prompt` while one is in flight  | Emit `error` with the new command's `id`; **do not cancel** the running stream |
| stdin EOF                        | Save session and exit 0 (same as `shutdown`)                 |

---

## §6 — Concurrency model

Commands run **sequentially**.  A `prompt` or `follow_up` command spawns a
background streaming task; the reader loop continues to accept commands while
the stream runs.  The **only** commands that should be sent while a prompt is in
flight are:

- `abort` — cancels the stream via a `CancellationToken`
- `get_state` — non-mutating snapshot
- `get_session_stats` — non-mutating snapshot

Any other command (including a second `prompt`) while a stream is in flight will
receive an `error` response and be ignored.

---

## §7 — CLI flags

```
synaps rpc [OPTIONS]

Options:
  --continue <SESSION_ID>   Resume an existing session by ID, name, or prefix
  --system <PROMPT_OR_FILE> System prompt string or path to a .md file
  --model <MODEL_ID>        Override the active model for this session
  --profile <PROFILE>       Configuration profile to load
```
