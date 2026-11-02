# Client tools (v1)

`Welcome.features` advertises `client_tools_v1`.

A client tool is a tool that an attached app registers on one live session.
The daemon offers it to that session's model. When the model calls it, the
daemon sends the call to the app that registered it. The app runs the call
and sends back the result. The daemon gives that result to the model.

The registration lives only in the daemon's RAM. It belongs to one link, one
session, one input owner and one daemon process (pid and generation). It is
never written to disk, never copied, never replayed and never logged.

This is a generic capability. Pria's vault tools are only the running example.

Contract: `contract: 1` (the frozen r2 design, IK1 seam B'). Code:
`session/client_tools.rs` (wire shapes, parsing, merging),
`session/actor_client_tools.rs` (commands and lifecycle) and
`runtime/client_tools.rs` (the per-session overlay, the handler, the pending
calls).

## Before you register

1. Check that `Welcome.features` holds BOTH `client_tools_v1` and
   `memory_only_results_v1`. `client_tools_v1` is always listed. The operator
   kill switch (below) shows up only as a `feature_off` refusal.
2. If the results are protected content, mark the tools memory-only first. Send
   `set memory_only_tools [...]` on THIS link and wait for
   `setting_changed ok:true` (see [memory-only-results.md](memory-only-results.md),
   "Setting the globs"). The glob matches the tool's OWN name, for example
   `pria_vault_*` matches runtime `pria-desktop:pria_vault_get`. It does not
   match the wire name `pria-desktop_pria_vault_get`.
3. Send `register_client_tools` with `require_memory_only: true`.

Send all of this again on every new link. `Hello.reconnect_of` restores input
ownership only. It does not restore a registration.

## Who may register

- Only the session's input owner (attached, not Observe).
- Only a root session that has a conversation id. A fork child is refused
  `not_root_session`, and so is a worker or subagent session (one with a
  delegation parent). A session with no conversation id is refused
  `no_conversation`.
- Workers and subagents have no link, so they cannot register. They never see a
  client tool either.

The daemon cannot tell which session the app calls "main". It gives evidence
instead. The ACK echoes `client` (your `Attached.client`), `conversation_id`,
`forked_from`, `daemon_pid` and `daemon_generation`. Check them against your
own record of the session you created.

## `register_client_tools`

The command rides the usual envelope
`{"type":"cmd","session_id":S,"cmd":{...}}`.

```json
{"cmd":"register_client_tools","id":7,"contract":1,"daemon_generation":40,
 "extension_id":"pria-desktop","require_memory_only":true,
 "tools":[{"name":"pria_vault_get","description":"Fetch one granted vault item.",
           "input_schema":{"type":"object","properties":{"item":{"type":"string"}},"required":["item"]},
           "result_family":"pria.vault.v1","outcomes":["ok","refused_no_grant","refused_expired"]}]}
```

| Field | Rule |
|---|---|
| `id` | u64, echoed in the ACK. Without it the command does not parse: the link gets the usual bad-command error, not an ACK. |
| `contract` | must be `1`, else `contract_unsupported` (missing too). |
| `daemon_generation` | must equal `Welcome.generation`, else `wrong_generation` (missing too). |
| `extension_id` | `^[a-z][a-z0-9-]{0,31}$`. It must not be a loaded extension's id (`name_collision`). It is fixed for the registration's life (`extension_mismatch`). |
| `require_memory_only` | bool, default `false`. It is fixed for the registration's life: a resend with another value is `def_changed`. When `true`, every tool must be marked memory-only NOW, else `not_marked`. |
| `tools` | 1 to 32 tools in total (`limit`). |
| `tools[].name` | `^[a-z][a-z0-9_]{0,63}$`, unique. The runtime name is `<extension_id>:<name>`; the wire name (what the model and the `ToolUse` event see) is `<extension_id>_<name>`. Names are never cut or suffixed. A name equal to any tool the session already has is `name_collision`. A client tool never shadows another tool. |
| `tools[].description` | 1 to 4096 UTF-8 bytes; no control characters except LF and TAB. |
| `tools[].input_schema` | a JSON object with `"type":"object"`, at most 16384 bytes serialized (`limit`). It is passed to the provider as is. |
| `tools[].result_family` | `^[a-z][a-z0-9_.]{0,47}$`. |
| `tools[].outcomes` | 1 to 32 unique codes `^[a-z][a-z0-9_]{0,47}$`. The prefix `synaps_` is reserved for the daemon. |

Any other rule broken is `invalid`. Unknown keys are ignored. The whole
command must be at most 262144 bytes serialized (`limit`).

### Add-only

Send the full list every time. The command is all or nothing; a refusal changes
nothing.

- Nothing registered: `applied`, with a new `registration_id` (`cr_` + 32
  lowercase hex, random).
- The same list again: `unchanged`, same id. Use this to resync.
- A superset: `applied`, same id. New tools are offered from the next provider
  request, also mid-turn.
- A registered tool with any field changed: `def_changed`.
- A registered tool left out: `omitted`.

### Refusal codes, in the order they are checked

`not_owner`, `feature_off`, `busy` (a reload is in progress),
`contract_unsupported`, `wrong_generation`, `no_conversation`,
`not_root_session` (a fork child or a worker), then the field checks
(`invalid`, `limit`), `busy` again if the extension manager is busy loading,
`name_collision`, then the merge (`extension_mismatch`, `def_changed`,
`omitted`), then `not_marked`. The first failure answers. Collisions are
checked again on every register.

A command sent by the host (not by a client) gets no reply.

### The ACK

The ACK goes to the sender only.

```json
{"ev":"client_tools","id":7,"outcome":"applied","code":null,"message":null,"contract":1,
 "registration_id":"cr_5f0c...","client":17,"daemon_pid":4242,"daemon_generation":40,
 "conversation_id":"20261027-...","forked_from":null,"extension_id":"pria-desktop",
 "require_memory_only":true,
 "tools":[{"name":"pria_vault_get","runtime_name":"pria-desktop:pria_vault_get",
           "wire_name":"pria-desktop_pria_vault_get","description":"...","input_schema":{...},
           "result_family":"pria.vault.v1","outcomes":["ok","refused_no_grant","refused_expired"],
           "memory_only_marked":true}],
 "pending":0,"limits":{"max_tools":32,"max_pending":8,"timeout_s":120,"max_content_bytes":262144}}
```

- `outcome` is `applied`, `unchanged`, `refused` or `dropped`.
- `code` is `null` on success, a refusal code on `refused`, `owner_change`,
  `new_conversation` or `reload` on `dropped`, and `rollover` on an unsolicited
  `unchanged`. `message` is a short human text.
- Every reply echoes the registration in force now. After a refusal that is
  the one still in force, or `registration_id: null, tools: []`. A `not_owner`
  reply echoes no registration (and `client` is the sender).
- `forked_from` is the session's fork parent, as an object, or `null`.
- Unsolicited notices use the same event with `id: null`.
- A NEW `registration_id` means the old authority ended. Drop everything bound
  to the old id.

## Who sees the tools

The tools are an overlay on that one session's root runtime. They are merged
only into the root stream's tool list, and appended after every other tool.
They are always offered (never deferred, never found by `activate_tools`).

They are never in the shared tool registry, `SessionConfig`, a fork seed, a
reload or crash record, or worker inheritance. So workers, subagents, forks,
the standalone CLI, MCP and an extension provider's own model loop never see
them.

At dispatch the daemon checks again, before it sends any frame. The call must
come from the foreground root (no delegation parent), with a conversation id,
a tool_use id, a non-empty turn id, and the registration that offered the tool
still in force. Otherwise the call ends with the gate text (below) and no frame
is sent.

Client calls pass the same sandbox gate and hooks as any tool.

### Hooks

Extension hooks (`before_tool_call`, `after_tool_call`) see a memory-only
client call by its NAME only (decision 18(a)). A hook runs in a plugin
process that may send what it sees off the box, so the args and the result
of a memory-only call never reach it.

- `before_tool_call` gets the tool's names (`tool_name`, `tool_runtime_name`)
  and the call's ids (`tool_call_id`, `session_id`), with `"tool_input": null`
  and `"input_withheld": true`.
- `after_tool_call` gets the same, plus `"tool_output": null` and
  `"output_withheld": true`.
- A `Block` still refuses the call: no frame is sent and the model sees the
  block reason.
- A `Modify` (before) or `Replace` (after) is ignored, with a debug line: the
  hook never saw what it would rewrite. The installer gets the model's args
  unchanged.
- A `Confirm` never shows a prompt for such a call. It stands only on the
  session's auto-approve or an earlier "Allow all from" latch; otherwise the
  call is blocked.
- The same applies to any tool a `memory_only_tools` glob marks, wherever a
  streamed turn dispatches it.
- A client tool that is not memory-only (`require_memory_only: false` and no
  `memory_only_tools` glob marks it) is hooked like any tool: the hooks see
  its args and result.

`input_withheld` and `output_withheld` are left out of the event when false,
so the hook event of every other tool call is unchanged.

## `tool_invoke` (daemon to the installer)

```json
{"ev":"tool_invoke","invocation_id":"ci_9a41...","registration_id":"cr_5f0c...",
 "daemon_pid":4242,"daemon_generation":40,"conversation_id":"20261027-...",
 "turn_correlation_id":"...","tool_use_id":"toolu_01...","tool":"pria_vault_get",
 "wire_name":"pria-desktop_pria_vault_get","args":{"item":"..."},"deadline_ms":120000}
```

- Only the client that registered (the installer) gets it. It is never in the
  replay ring, so attach and `Resync` never deliver it again.
- `invocation_id` is `ci_` + 32 lowercase hex, random, never reused in one
  daemon process. It is valid only for this daemon process, registration,
  session and turn, and only until the call ends.
- `turn_correlation_id` is the turn's id, always a non-empty string. A call
  with no turn id gets no frame; it ends at once with `synaps_not_root`.
- `args` is the model's tool_use input, unchanged. As compact JSON it must be
  at most 1048576 bytes. Larger arguments get no frame; the call ends at once
  with `synaps_args_too_large`.
- `deadline_ms` is the call's timeout (120000).
- At most 8 calls pend per session. A 9th gets no frame and ends at once with
  `synaps_busy`. Calls may overlap.
- Delivery is at most once. An invoke lost to broadcast lag is never sent
  again; that call ends by timeout.

Before acting, the app checks: the envelope session is the registered one;
`registration_id` is its current ACK's; pid and generation are its Welcome's;
the tool is in its registration; `invocation_id` is new. Then it consumes the
id once. It may also wait for the matching `ToolUse` event (tool_id =
`tool_use_id`, tool_name = `wire_name`, input = `args`), but lag can drop that
event.

## `tool_return` (installer to the daemon)

```json
{"cmd":"tool_return","id":8,"invocation_id":"ci_9a41...","outcome":"ok",
 "content":"...text...","_meta":{"synaps/memory_only":true}}
```

- `content` is a string, 0 to 262144 UTF-8 bytes AFTER JSON decoding. It is
  kept whole or refused; it is never cut. Text only in v1. The serialized line
  is bounded only by the daemon's 64 MiB frame limit, so heavily escaped
  content at the edge fits.
- `outcome` must be one of this tool's registered `outcomes`. A `synaps_` code
  is refused `unknown_outcome`. There is no `is_error`; it is ignored if sent.
- `_meta` is an object of at most 4096 bytes serialized (`null` means absent).
  Only `synaps/memory_only` (JSON `true`) is honoured. Other keys, and unknown
  top-level keys, are ignored.

The checks run in this order. The first failure answers.

1. `invalid`: the shape is wrong.
2. `not_installer`: the sender did not install the registration in force.
3. `not_pending`: the id is unknown, already answered, cancelled or timed out
   (one code for all of them).
4. `unknown_outcome`: the outcome is not one of this tool's codes.
5. `too_large`: `content` or `_meta` is over its limit.

`invalid`, `unknown_outcome` and `too_large` consume nothing. The call still
pends, the payload is dropped at once and never reaches the model, and the app
may send a corrected return before the deadline. Only an accepted return
consumes the id.

```json
{"ev":"tool_return_ack","id":8,"invocation_id":"ci_9a41...","outcome":"accepted","code":null}
```

The ack goes to the sender only. `outcome` is `accepted` or `refused`.
`invocation_id` is echoed once the fields parsed (else `null`). `accepted`
means the result was handed to the waiting call; it does not mean the model saw
it (see "Cancel wins"). A return sent by the host gets no ack.

## `tool_cancel` (daemon to the installer)

```json
{"ev":"tool_cancel","invocation_id":"ci_9a41...","registration_id":"cr_5f0c...","reason":"turn_cancel"}
```

`reason` is `turn_cancel`, `timeout`, `reload`, `owner_change` or
`new_conversation`. A turn cancel's `tool_cancel` reaches the installer before
the turn's `Idle`. Detach sends nothing (the link is gone): treat link loss as
the cancel of every call you hold.

## Cancel wins

Each call ends exactly once. Under one lock a call moves
Pending, then Accepted (a return was accepted), then Taken (the result was
handed to the model's stream), or it moves to Ended with a daemon code. Turn
cancel, timeout, detach, owner change, reload, `/new` and `/resume` all try to
end a Pending or Accepted call. The handler checks the call again when it takes
the result. So an accepted result whose authority ended before it was taken
never reaches the model: the model sees the end text instead. A Taken result
is final.

An `accepted` ack can still be followed by a `tool_cancel`. The cancel and the
stamp are final.

Nothing is retried or re-sent, ever. Reconnecting, resyncing or registering
again re-sends nothing; the model decides whether to call again.

### What the model sees

Every text starts with `Tool execution failed: ` (the usual tool error
prefix). `{wire}` is the wire name and `{call}` the tool_use id.

| End | Stamp `outcome` | Model sees |
|---|---|---|
| accepted return | the app's code | `content` (raw if memory-only; history keeps the placeholder) |
| turn cancel | `synaps_cancelled` | the turn's cancel phrase, e.g. `Canceled by user` |
| reload | `synaps_reload` | the turn's cancel phrase |
| timeout | `synaps_timeout` | `client tool {wire} timed out after 120 s (call {call}); nothing came back. Call it again if you still need it.` |
| detach | `synaps_detached` | `client tool {wire} ended: its client disconnected (call {call}). Call it again if you still need it.` |
| owner change | `synaps_owner_changed` | `client tool {wire} ended: the session's input owner changed (call {call}).` |
| `/new`, `/resume` | `synaps_new_conversation` | nothing: the turn is cancelled and the conversation replaced |
| 9th pending call | `synaps_busy` | `client tool {wire} refused: 8 client tool calls are already in flight in this session. Wait for one to finish, then call again.` |
| registration gone | `synaps_unregistered` | `client tool {wire} is no longer registered in this session (call {call}).` |
| arguments too large | `synaps_args_too_large` | `client tool {wire} refused: its arguments are over 1048576 bytes (call {call}). Send smaller arguments.` |
| gate refusal (a worker, no conversation id or no turn id) | `synaps_not_root` | `client tool {wire} runs only in the session's own conversation, not in a worker or without a conversation id.` |

The timeout text shows the timeout in force (120 s outside tests). A
glob-marked tool's error text is memory-only too, so history keeps the
placeholder and the stamp keeps the code.

## The result stamp

A client tool's result carries a stamp. The daemon builds it from the pending
call's registration and the validated return code, never from the text.

```json
"client_tool":{"registration_id":"cr_5f0c...","invocation_id":"ci_9a41...",
               "extension_id":"pria-desktop","tool":"pria_vault_get",
               "family":"pria.vault.v1","outcome":"ok","memory_only":true}
```

- `invocation_id` is `null` when no invoke was sent (`synaps_busy`,
  `synaps_unregistered`, `synaps_not_root`, `synaps_args_too_large`).
- `memory_only` is the typed result of the memory-only take for this result.
- A call cut off by a turn cancel (the stream drops it) gets no stamp. A call
  refused by a hook or the sandbox gets no stamp.
- Other tools never get a stamp.

Where clients see it:

- The live `tool_result` event gains `client_tool` (absent when there is
  none). Every `tool_result` also carries the typed `memory_only` flag.
- `Attached` gains `client_tools: {tool_use_id: stamp}` for the tool_use ids
  in the session's history. It is absent when empty. Digest attaches get it
  too.
- The `messages` query answer gains the same map as a sibling of `value`:
  `{"ev":"query_result","id":4,"value":[...],"client_tools":{"toolu_01...":{...}}}`.
  It is absent when empty and on every other query.

Where it is kept: in the session file, next to the messages (key
`client_tool_stamps`), never inside `api_messages`. So no provider, summary or
keep-warm request carries a stamp. In journal mode it is in the snapshot only.

- Reload and crash restore keep the stamps.
- Compaction and rollover keep only the ids still in the new history.
- A fork child copies the stamps of the messages it copies (history, not
  authority).
- A stamp whose tool_use id left the history is pruned at the end of the turn
  and at compaction, and is never returned to a client.
- A stamp never restores the right to call a tool. It holds the two ids and the
  codes only: no definition, no arguments, no content.

Display (app side): show your own card only if `registration_id` is one this
daemon gave YOU in an ACK. `extension_id` or `family` alone never authorize a
card. `synaps_*` outcomes keep the general card. No client can write a stamp.

## Memory-only and `max_tool_output`

A client tool has Extension origin, so the memory-only rules in
[memory-only-results.md](memory-only-results.md) apply as they are: the globs
match the own name, `_meta.synaps/memory_only: true` marks a result, the raw
text goes only into the live provider request, history and clients get the
placeholder.

`require_memory_only: true` makes the glob a second guarantee that the daemon
checks at register time. Globs are add-only, so the mark cannot lapse while the
tools are registered.

Sizes: `content` is at most 262144 bytes, so the memory-only oversize text never
fires for a client tool. The session's 1048576 live raw bytes still apply: a
5th maximum-size result evicts the oldest raw to its placeholder.

`max_tool_output` never cuts or withholds a MARKED client tool result. The raw
text and its placeholder are both exempt. A 196608-byte result reaches the live
provider request whole even at `max_tool_output = 1`, while history and clients
get only the placeholder, the typed `memory_only` flag and the stamp. An
UNMARKED client tool result is truncated like any extension result.

## Lifecycle

| Event | Registration | Pending calls |
|---|---|---|
| installer detach, EOF or Bye | dropped | end at once, `synaps_detached`; no frame |
| owner change (Takeover, owner passes) | dropped; `client_tools{dropped, owner_change}` to the old installer if still attached | end at once, `synaps_owner_changed`; `tool_cancel{owner_change}` |
| observer joins or leaves | unchanged | unchanged |
| reload | dropped at the checkpoint (even if the exec then fails); `client_tools{dropped, reload}` to the installer if still attached, so after an aborted or refused reload the app registers again | turn cancelled; `tool_cancel{reload}` best effort before `Reloading` |
| daemon crash and restore | gone | gone; the new pid or generation forces a new register |
| `/new`, `/resume` | dropped; `client_tools{dropped, new_conversation}` | turn cancelled; `tool_cancel{new_conversation}` |
| fork | the parent keeps it; the child has none and may not register | the parent's are unchanged |
| park | not dropped by the park itself (a park needs no attached client, so a detach has usually dropped it already); an unpark hands the overlay back to the runtime | none (a park needs no turn) |
| compaction or rollover | kept; `client_tools{unchanged, rollover}` with the new `conversation_id` when the id changed | none can pend |
| turn cancel, timeout | unchanged | `tool_cancel{turn_cancel}` / `tool_cancel{timeout}` |

Apply a rollover notice's `conversation_id` before you act on the next invoke.

## Nothing is kept or logged

- The registration, the pending calls, the invoke's `args` copy and the
  return's `content` live in RAM only. They are not in `SessionConfig`,
  settings replay, reload or crash records, the fork seed or worker
  inheritance.
- `client_tools`, `tool_invoke`, `tool_cancel` and `tool_return_ack` are
  never replayed (like `Prompt`, `QueryResult` and `Forked`).
- The Debug output of the two commands and four events leaves out `args`,
  `content`, `description` and `input_schema`. None of them is traced at any
  level.
- An honest limit: `args` ARE the model's tool_use input. Like any tool call
  it stays in the history and reaches every attached client in the `ToolUse`
  event. A memory-only result is a placeholder in history, but the model's own
  reply, files and later tool arguments are kept as usual.

## Limits

| Limit | Value |
|---|---|
| tools per registration | 32 |
| pending calls per session | 8 |
| timeout per call | 120 s |
| `args` (compact JSON) | 1048576 bytes |
| `content` (decoded) | 262144 bytes |
| `_meta` (serialized) | 4096 bytes |
| `description` | 4096 bytes |
| `input_schema` (serialized) | 16384 bytes |
| outcomes per tool | 32 |
| `register_client_tools` (serialized) | 262144 bytes |
| any frame | 64 MiB |

## Turning it off

Config key `client_tools = off` (default `on`). The daemon reads it at each
register, so a change applies to the next `register_client_tools`, which is
refused `feature_off`. A registration already in force is kept. The key is not
shown in the TUI settings.
