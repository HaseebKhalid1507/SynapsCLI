# synaps rpc — Wire Protocol Reference

> See also: `synaps-bridge.SPEC.md §4` for the canonical specification.

## Overview

`synaps rpc` is a headless line-JSON IPC server.  The bridge daemon spawns one
child process per Slack thread and communicates with it over the child's
`stdin` / `stdout` pipes.

**One process = one synaps session.**

### Architecture: a client of a private session daemon

`synaps rpc` runs no engine of its own. At start it spawns a **private**
session daemon in a fresh run dir of its own,
`<run>/rpc/<pid>-<random>/` (0700; `<run>` is `$SYNAPS_RUNTIME_DIR`, else
`$SYNAPS_BASE_DIR/run`), and creates an **owned** session there: the
session's cwd and environment are this process's, and the secret env vars
a daemon normally strips are forwarded to the session's tools (in memory
only). It then translates each command below into a session command and
each session event back into the frames below. The turn loop, tools,
saves, context-head checkpoints, event auto-turns and turn budgets are the
session actor's — the same code a TUI or `synaps attach` session runs.

The private daemon gives each `synaps rpc` process the isolation the old
in-process engine had: it inherits this process's environment (provider
auth such as `SYNAPS_AUTH_ENDPOINT`/`SYNAPS_MACHINE_TOKEN`, extension
staging, `SYNAPS_BASE_DIR` and config), mount namespace and cgroup, and
nothing else connects to it: it never adopts, or is adopted by, the
profile's shared daemon that `synaps` / `synaps attach` use. It does not
restore pinned sessions. When the session ends the daemon exits (at most
10 s later, also when the rpc process was SIGKILLed), and the run dir is
removed; a run dir left behind by a killed process is swept by the next
`synaps rpc` once its daemon is gone. Sessions are saved under
`$SYNAPS_BASE_DIR` as before, so `--continue` works across processes.

Frames on stdout are queued in memory (bounded at 64 MiB), so a parent
that reads stdout slowly never stalls the daemon connection.

**The protocol is unchanged**: the same frame types and fields. A
golden-fixture differential test (`tests/rpc_differential.rs`) holds the
frames to the pre-daemon output; the differences it allows are listed in
the CHANGELOG (`get_session_stats.cost` is now the real session cost;
`get_messages` messages carry no internal `_synaps_*` keys; some
actor-owned message texts). What follows from the new architecture:

- The session is owned by the process: when it exits or is killed, the
  daemon cancels any running turn, saves, and ends the session.
- No daemon, no rpc: with `SYNAPS_DAEMON=0` or
  `SYNAPS_DAEMON_AUTOSPAWN=0`, or when the private daemon cannot start,
  the child writes one `error` frame (no `ready`), says why on stderr and
  exits **3**. There is no in-process fallback.
- Losing the daemon (crash, kill, or the daemon cutting this client off
  for falling behind its event stream, which would otherwise drop events
  and usage) answers everything in flight (see the error table) and exits
  **4**.
- A `synaps daemon reload` aimed at the private daemon
  (`SYNAPS_DAEMON_DIR=<its run dir>`) cancels an in-flight turn at its
  checkpoint: the turn ends with `error` + `agent_end` + `response {ok:
  false}`; the child re-binds to its session and carries on (stdin is
  buffered meanwhile).
- On exit the child keeps writing queued frames for as long as the parent
  can read its stdout (the pipe's read end is open), at most 60 s: keep
  reading to EOF after closing stdin or sending SIGTERM, or the cut turn's
  `agent_end` is lost. Prefer SIGTERM (with a few seconds' grace) to
  SIGKILL: SIGKILL writes nothing.
- Processes: the child (a single-threaded runtime) plus one detached
  `synaps daemon --foreground` (2 tokio workers unless
  `SYNAPS_WORKER_THREADS` says otherwise). Engine logs go to the daemon's
  log file; the daemon's stderr is copied to the child's stderr.

### Exit codes

| Code | Meaning |
|------|---------|
| 0    | stdin EOF or `shutdown` |
| 1    | startup refused: `--continue` of a session that is live elsewhere or does not resolve, bad `--system` file |
| 2    | daemon protocol version mismatch |
| 3    | no session daemon (disabled, autospawn off, spawn failed); also a `--profile` that cannot be applied |
| 4    | the session daemon was lost, or ended the session |
| 143  | SIGTERM (after writing the terminal frames, see the error table) |

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
  ever appear there.  Diagnostics go to **stderr** (the child's warnings,
  WARN and above, and whatever its private daemon prints there), written
  lossily: a parent that never reads stderr cannot stall the child.

> **v0 limitation:** the `prompt` command's `attachments` field carries file
> paths but the rpc child does NOT read file bytes — it prepends a textual
> note `[user attached files: "<path>", ...]` to the user message instead.
> Binary attachment support is planned for Task 10.

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
  "cache_creation_5m": 0,
  "cache_creation_1h": 0,
  "model": "claude-opus-4-5"
}
```

`cache_creation_5m` / `cache_creation_1h` split `cache_creation_input_tokens`
by cache-write TTL (5-minute vs 1-hour; see the `cache_ttl` config key). Both
are **optional** and **skipped entirely when unknown** — they appear only when
the API reported a `cache_creation` sub-object (Anthropic native path). When
absent, consumers must not assume a split; bill aggregate writes at the 5m
rate. When turns are accumulated, each field is `null`/absent only if it was
never reported; otherwise it is the sum of the reported values.

---

## §5 — Error handling

| Situation                        | Child behaviour                                              |
|----------------------------------|--------------------------------------------------------------|
| Inbound frame > 1 MiB            | Emit `error` with `id: null`; **stay alive**                 |
| Malformed JSON                   | Emit `error` with `id: null`; **stay alive**                 |
| Runtime error during `prompt`    | Emit `error` with `id: <prompt-id>`, then `agent_end`, then `response {ok: false}` |
| `prompt` while one is in flight  | Emit `error` with the new command's `id`; **do not cancel** the running stream |
| stdin EOF                        | End the session: everything already sent is answered in order (queries, `set_model`, …); an in-flight turn is cancelled and still gets `agent_end` (usage so far) + `response {ok: true, cancelled: true}`; exit 0 |
| `shutdown`                       | Let an in-flight turn finish (≤ 30 s; then as EOF), end the session, exit 0 |
| SIGTERM                          | As EOF, bounded: cancel the turn, wait ≤ 2 s for it to end, write its `agent_end` (usage so far) + `response`, answer the rest, exit 143 |
| No session daemon                | Emit `error` with `id: null` (no `ready`), exit 3            |
| Session daemon lost, or the session ended under it | Every request in flight gets a terminal frame: a turn gets `error {id}` + `agent_end` (usage so far) + `response {ok: false}`; a query, `set_model`, `new_session` or `compact` gets `error {id}`. Only when nothing was in flight: one `error` with `id: null`. Exit 4 |
| `compact` the session refuses (turn running, compaction in progress) | `error` with the compact's `id` |
| `compact` with no answer         | `error` with the compact's `id` after 30 s (not started) / 10 min (started); stdin is read again |
| `set_model` the session refuses  | `error` with the command's `id`                              |
| `--continue` of a live session   | Refused: reason on stderr, exit 1, no `ready`                |

---

## §6 — Concurrency model

Commands run **sequentially**.  A `prompt` or `follow_up` command spawns a
background streaming task; the reader loop continues to accept commands while
the stream runs.  The **only** commands that should be sent while a prompt is in
flight are:

- `abort` — cancels the turn (the session's `Cancel`)
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
