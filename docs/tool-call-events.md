# Tool-call events (`daemon.tool_call_events`)

A daemon-socket client can subscribe to one small, content-free record per
phase of every tool call the daemon runs: every session's own calls, its
subagents' calls, and the calls an extension makes through `tool.call`. A
client that used to tap the `before_tool_call` hook to learn what ran (a
gate, an audit pane, a "what is the agent doing" view) can read these
instead, without being on the tool path.

## Turning it on

Off by default. Set the config key and restart or reload the daemon:

```
daemon.tool_call_events = on
```

`on`/`true`/`1`/`yes` and `off`/`false`/`0`/`no` are accepted; anything else
warns and leaves it off. The Settings page has the same row (Daemon,
"Tool-call events"). The daemon reads the key when it starts (a
`synaps daemon reload` re-reads it); changing it does not affect a running
daemon. An in-process (non-daemon) session never records.

Off, or on with nobody subscribed, a tool call builds and hashes nothing.

## Subscribing

On a daemon connection (line-JSON over the UDS, see the protocol summary
in [daemon-mode.md](daemon-mode.md)), after `Hello`/`Welcome`:

```
→ {"type":"tool_calls"}
← {"type":"tool_calls_subscribed"}
← {"type":"tool_call","seq":1,"dropped":0,"record":{...}}
← {"type":"tool_call","seq":2,"dropped":0,"record":{...}}
```

- The connection is then a tool-call stream. It still answers
  `{"type":"ping"}` with `pong`; `{"type":"bye"}` ends it. Any other frame
  gets an `error` frame and the stream goes on.
- With the key off, the subscribe is answered with an `error` frame that
  names `daemon.tool_call_events`, and the connection stays usable.
- A daemon older than this feature answers `error` "malformed frame:
  unknown variant `tool_calls`, ..." and also keeps the connection usable.
- `tool_calls_subscribed` and `tool_call` frames go only to a connection
  that asked for them. Attach clients and older clients never see them.
- One subscription covers every session of the daemon. Filter by
  `session_id` (and `handle_id`) on the client.

### Reload

A `synaps daemon reload` ends the stream with a `bye` frame whose reason
is `reloading` (with the new image's `generation`), as for attach clients.
Reconnect and subscribe again on the new image. Records for calls that ran during the
gap are not replayed, and `seq` starts again at 1 on the new image.

## The `tool_call` frame

| Field | Meaning |
|---|---|
| `seq` | Daemon-wide, increasing within one daemon image. |
| `dropped` | How many records this subscriber missed since its previous `tool_call`, because it read too slowly (see [Fail-open](#fail-open)). |
| `record` | One record, below. |

## The record

```json
{"v":1,"phase":"called","fp":"664c8869d8c4b4dd","cwd":"/srv/project",
 "session_id":"8f0c…","tool":"bash","tool_runtime":"bash",
 "tool_id":"toolu_01AbC…","ts":"2026-10-24T18:00:00.123Z"}
```

Every record has:

| Field | Meaning |
|---|---|
| `v` | Record version, `1`. |
| `phase` | `called`, `decided` or `finished` (below). |
| `session_id` | The owning session's id (the `SessionMeta.id` attach clients see). A subagent's call carries its **parent's** session id, plus `handle_id`. `null` when there is none: an extension's own `tool.call`, or a subagent whose parent has no session. |
| `handle_id` | The subagent's handle (`sa_N`, as the children ledger reports it) when a subagent made the call. Absent for a session's own calls. |
| `tool` | The tool's API name: the name the model called. |
| `tool_runtime` | The tool's runtime name (for an extension or MCP tool, its full name). Same as `tool` for builtins. |
| `tool_id` | The provider's call id (`tool_use.id`): the id the attach stream's `tool_use` and the children ledger report for the same call. Empty when the dispatch site has none. |
| `ts` | When the phase happened: RFC 3339, milliseconds, UTC. |

### Phases

One call gives `called`, then `decided`, then `finished`, all with the same
`tool_id`. A call the runtime refuses before dispatch (a tool that is not
exposed or not activated) gives no record.

| Phase | Extra fields | When |
|---|---|---|
| `called` | `fp`, `cwd` (optional) | The model asked for the call; no extension has seen it yet. |
| `decided` | `decision`, `by`, `fp` | The `before_tool_call` hook chain has decided. |
| `finished` | `status`, `duration_ms` | The call ended. |

- **`fp`** is a fingerprint of the call, `FNV-1a-64(tool ‖ 0x00 ‖ canonical
  JSON of the input)` as 16 lowercase hex digits (canonical JSON: object
  keys sorted, no whitespace). It is the same algorithm and the same bytes
  as pria-gate's `call_fingerprint`, so a client can pair a record with a
  `tool_use` it already holds. `called.fp` is over the model's input;
  `decided.fp` is over the input the hook chain settled on, so the two
  differ exactly when an extension modified the input. It links records;
  it is not a secret and not a security boundary.
- **`cwd`** is the base directory the tool resolves relative paths against:
  the same value the call's `before_tool_call` hook event carries. It is
  omitted when unknown or not UTF-8. A subagent has no working directory of
  its own, so its calls carry the daemon process's directory. A folder
  named in the tool's own input (a `path` argument, a `cd` in a command) is
  NOT here: resolve it from the `tool_use` you already have. `cwd` is on
  `called` only.
- **`decision`**: `allow`, `confirm`, `block` or `modify`. **`by`** names
  the extensions that decided it (host-stamped ids): the blocker, the
  confirmers or the modifiers; empty for `allow`.
- **`status`**: `ok` or `error` (the tool ran; `error` when it returned an
  error), `blocked` (an extension blocked it, or a confirmation was denied)
  or `cancelled` (the turn was cancelled or the call was dropped before it
  ended). `duration_ms` runs from `called`.
- `finished` is published when the tool returns, before the
  `after_tool_call` hook runs.

### Confirm prompts

A `Confirm` prompt raised for a tool call carries the call's id in
`confirm_scope.tool_id`, the same value as the records' `tool_id`, so a
client can tie the prompt to the call it is about. An older client ignores
the field.

## No content

A record, a frame and their `Debug` lines never carry the tool's input, its
output or command text. The one path is `called.cwd`. Everything else is
names, ids, the fingerprint, the decision, the status and timings.

## Fail-open

Nothing on the tool path waits for a client. Records go into one in-memory
broadcast channel (4096 records) with a synchronous send. A subscriber
that reads too slowly skips ahead to the oldest record still held and
learns how many it missed from `dropped` on its next `tool_call`. A
subscriber that disconnects is simply gone. The tool call itself never
slows down, fails or blocks because of a subscriber.

## Limits

- No replay: a subscriber sees calls from the moment it subscribed, and a
  reload or reconnect leaves a gap.
- A call refused before dispatch (not exposed, not activated) has no
  record.
- A subagent's own subagent carries `session_id: null` and its own handle.
- Subagents started with the legacy `SYNAPS_SUBAGENT_FRESH_RUNTIME=1` path
  are not recorded.
- Same-uid only, like the rest of the daemon socket (0700 run dir, 0600
  socket): it is not a privilege boundary.
