# Memory-only tool results (v1)

`Welcome.features` advertises `memory_only_results_v1`.

A memory-only tool result is never written anywhere. The session's records keep a
fixed placeholder instead. The raw result lives only in the live session's RAM
and goes only into the live provider request. After a restart, reload or
rollover the model sees the placeholder and must call the tool again (a new,
normally gated call). Nothing is replayed automatically.

This is a generic capability. It covers extension tools and MCP tools.
Built-in tools and plugin-declared tools are never memory-only in v1.

## Marking

A result is memory-only when EITHER of these says so.

1. The tool marks the result itself with `_meta["synaps/memory_only"]` set to
   JSON `true` (not the string `"true"`).
   - Extension tools: the `tool.call` result object, for example
     `{"content": "...", "_meta": {"synaps/memory_only": true}}`. This is the
     first-class path: a tool that always serves protected content should set
     it on every result.
   - MCP tools: the `CallToolResult` `_meta` (also for an `isError` result).
   - An extension JSON-RPC error response has no `_meta`; only a glob (below)
     marks errors. A marked call's error text is memory-only too.
2. The host or app names the tool with a glob.
   - Per session: `SessionConfig.memory_only_tools` (JSON array of globs), set
     at create or added to on a live session (add-only, see below). It is not
     content, so it is kept in the reload and crash records and copied to
     forks. Workers (subagents) read their caller's session list at each check,
     so they share it, including later additions.
   - Per install: config key `memory_only_tools = foo:*, ext__kag__*`.
   - Both lists apply. `*` matches any run of characters (including `:`), `?`
     exactly one character, everything else is literal, and the glob must match
     the whole name.

### Which names a glob matches

A glob is tested against three names of the tool. Any match marks it.

1. Its OWN name, exactly as the extension or MCP server registered it.
2. Its runtime name: `<extension_id>:<tool>` for an extension tool,
   `ext__<server>__<tool>` for an MCP tool.
3. Its API wire name, the name the model sees.

| Tool | Own name | Runtime name | Wire name |
|---|---|---|---|
| extension `foo`, tool `bar` | `bar` | `foo:bar` | `foo_bar` |
| extension `pria-desktop`, tool `pria_vault_get` | `pria_vault_get` | `pria-desktop:pria_vault_get` | `pria-desktop_pria_vault_get` |
| extension `jev`, tool `jev_decide` | `jev_decide` | `jev:jev_decide` | `jev_jev_decide` |
| MCP server `kag`, tool `search` | `search` | `ext__kag__search` | `ext__kag__search` |

The wire name is the runtime name with every character other than `A-Z a-z 0-9 _ -`
turned into `_` (so `:` becomes `_`), cut to 128 characters, with a `_2`, `_3`
suffix only if two tools would collide.

So the globs `pria_vault_*` and `pria_memory_*` mark an extension tool
registered as `pria_vault_get` by its own name, whatever the extension's id.
They match neither its runtime name `pria-desktop:pria_vault_get` nor its wire
name `pria-desktop_pria_vault_get`.

An own-name glob marks a tool of that name from ANY extension or MCP server.
That errs toward marking more, never less. To pin one extension, use the
runtime form, e.g. `pria-desktop:pria_vault_*`. For extension `foo` tool `bar`,
each of `bar`, `foo:*`, `foo_*`, `foo:bar` and `foo_bar` matches; `foo_*` also
matches the wire names of an extension called, say, `foo_x`. A built-in tool is
never marked, even when its name matches.

### Setting the globs on a live session

An existing session does not have to be re-created. The session's input owner
can add globs at any time, also while a turn is running:

`{"cmd":"set","id":1,"setting":{"setting":"memory_only_tools","globs":["pria_vault_*","pria_memory_*"]}}`

The reply is a `setting_changed` event with `setting: "memory_only_tools"`.

- Like every `set`, it is owner-only: an observer's is refused. It is also
  refused after a reload checkpoint.
- It is ADD-ONLY. Send the full list you want. A list holding every glob
  already set (an identical resend, or a superset) succeeds and adds the new
  ones. A list that leaves out any glob already set is refused: `ok: false`,
  the message names the left-out globs, nothing changes, and the `memory_only`
  query counts it in `set_refused`. Removing a glob takes a new session.
- The config-key list is never changed by a `set`.
- Every reply, ok or refused, echoes the effective list (config-key globs
  first, then the session's) in `memory_only_tools`.
- It marks every check after it. A `set` that lands mid-turn applies to the
  later calls of that same turn, and to workers, including ones already
  running: every check reads the session's one shared list.
- It never unmarks. A result already held stays memory-only until its
  lifetime ends.
- It cannot reach back. A result that came back before the glob was set was an
  ordinary result and is already in the session's records.
- A restored session (reload or resume) keeps its list, so resending the same
  list succeeds.

The replies (`view` elided):

- Accepted, first time:
  `{"ev":"setting_changed","applied":{"id":1,"setting":"memory_only_tools","ok":true,"message":null,"view":{...},"clamp":null,"memory_only_tools":["pria_vault_*","pria_memory_*"]}}`
- An identical resend gets the same reply, `ok: true`, with its own `id`.
- Refused, after `pria_extra_*` was added and a list without it was sent:
  `{"ev":"setting_changed","applied":{"id":5,"setting":"memory_only_tools","ok":false,"message":"memory-only tools are add-only: this list drops `pria_extra_*`; nothing changed. Removing a glob takes a new session.","view":{...},"clamp":null,"memory_only_tools":["pria_vault_*","pria_memory_*","pria_extra_*"]}}`

## What is kept instead

- Placeholder (in the history, and what the model sees once the raw is gone):
  `[memory-only result of {tool} (call {call_id}) is not kept in this session's records. Call the tool again if you need it.]`
- One result over 262144 bytes is refused whole, never truncated; nothing is kept:
  `[memory-only result of {tool} (call {call_id}) refused: {n} bytes is over the 262144-byte limit for one result. Nothing was kept. Ask for less and call again.]`
  It is an ordinary tool-result text (the engine's tool errors carry no `is_error`).
- A `max_tool_output` smaller than the placeholder itself would truncate the
  placeholder in the history, and a truncated placeholder is never swapped
  back. Then nothing is kept and the result says why (no WARN; the reason
  comes first, so a truncated copy still says it):
  `[memory-only result withheld: max_tool_output ({n} bytes) is too small for the placeholder of {tool} (call {call_id}). Nothing was kept. Raise max_tool_output and call again.]`
- `{tool}` is the wire name. No content, digest or preview is ever kept.
- Session cap: 1048576 live raw bytes. A new result that does not fit pushes
  out the OLDEST live results (they become their placeholder); the session keeps
  working and can fetch them again.
- A kept result is exempt from `max_tool_output` truncation: the provider gets
  it whole, or the model gets the refusal or the withheld notice.

## Lifetime

The raw text ends at: process restart; daemon reload; park/unpark; rollover and
`/compact` (the successor and the summary request see placeholders); a new or
resumed conversation; fork (the child starts empty) and worker spawn; eviction;
and when its tool_use_id is no longer in the history (rewind/edit).

## Where the raw text goes

Only into the live provider request, swapped into the request copy at the last
step. Such a request has request-content trace capture off and is not recorded
for the warm rolling summary. Context-pressure estimates count the raw bytes.
Background cache warming is unavailable while any raw result is live (the status
says `memory-only tool results are live: background cache warming is unavailable`).

Everything else is built from the history, so it holds only the placeholder: the
session file and turn drafts, the session journal, rollover archives and Axel
history, the rolling-summary request, subagent archives and collect output, the
reload record and crash record, logs at every level, Debug output, prompt
inspection, views and snapshots. `after_tool_call` hooks get the placeholder, and
a hook `Replace` is ignored for these results.

## Wire

- Raw text never reaches a client. A `tool_result` stream event for a kept or
  refused result carries the placeholder and `memory_only: true` (omitted when
  false), e.g. `{"kind":"llm","llm":"tool_result","tool_id":"toolu_1","result":"[memory-only result of ...]","memory_only":true}`.
  Messages, MessageHistory, Attached and DisplayTail carry the placeholder.
- `{"cmd":"query","id":1,"query":{"query":"memory_only"}}` answers (live or
  parked) `{kind: "memory_only", session_id, live, live_bytes, per_result_cap,
  session_cap, evicted, refused, set_refused}`. Counts only. `set_refused`
  counts the refused live `set`s of the list; `refused` counts results over
  the per-result limit.

## NAMED LIMIT: this covers tool RESULTS only

Memory-only applies to what a tool RETURNS, nothing else. These are saved as
usual:

- The model's own reply text. If the model quotes or summarises a memory-only
  result in its answer, that answer is kept in the session's records.
- Anything an agent writes to a file, or passes to another tool as arguments,
  is handled like any other tool call: the arguments are kept, and the other
  tool's result is ordinary unless that tool is memory-only too.
- A tool an extension runs itself through `tool.invoke` (outside a
  conversation) is outside this feature: that extension and the hooks on that
  path get the result as is.
- What the provider or the tool's own server retains is outside Synaps.

## Known limits

- A glob that names only built-in or plugin-declared tools marks nothing: only
  extension and MCP tools can be memory-only in v1.
- MCP: only the `CallToolResult` `_meta` marks. A JSON-RPC error response, and
  `_meta` on a content item or on `structuredContent`, do not; a glob does.
- `tool.invoke` (an extension calling a tool itself, the named limit above)
  takes the plain result path, which drops a `_meta` mark; nothing on that path
  is memory-only.
- Every compaction drops every live raw result when it STARTS: `/compact` and
  auto-compaction alike (both run the same compaction, which clears the held
  results first). So a compaction that fails or is cancelled has still dropped
  them. That is fail-safe: the model sees the placeholders and calls again.

## Proof

`crates/agent-engine/tests/session_actor_memory_only.rs::memory_only_no_disk_sentinel`
runs a real session actor with persistence on, a real extension process
returning one `_meta`-marked and one glob-marked sentinel, and a mock provider.
The sentinels are in the live provider requests and in nothing else: no event,
Messages/View/PromptInspection answer, summary request, fork-child request,
reload record or restored-session request, no log line at TRACE, and zero bytes
of any file under the temp home.

The sentinel test runs with `context_management.mode = off`. Rollover, the warm
summary and rollover archives are covered by the shared code path (the
history, which holds only the placeholder), not by this test.

`crates/agent-engine/tests/session_actor_memory_only_live.rs::memory_only_tools_set_on_a_live_session_mark_the_next_calls`
starts a session with no globs. A real extension `pria-desktop` returns an
unmarked sentinel from `pria_vault_get`. The test then sets `pria_vault_*` and
`pria_memory_*` live, and checks:

- the next call is memory-only;
- add-only: an identical resend and a superset succeed; a list dropping one
  glob, and an empty list, are refused, name the dropped globs, change
  nothing and are counted in `set_refused`; every reply echoes the list;
- after the refusals the held result is still live and a new call still marks;
- a reload record carries the list, and the restored session accepts the same
  list again;
- no file under the temp home holds the sentinel.

`crates/agent-engine/tests/session_actor_memory_only_midturn.rs::memory_only_set_mid_turn_marks_the_same_turn_and_workers`
sets the IK1 globs while a turn is in flight (the provider stub holds the
turn's next request) and checks that each of these is memory-only, with the
raw only in its live request and zero sentinel bytes under the temp home:

- the turn's next `pria_vault_get` call;
- the call of a worker started after the `set`;
- the call of a worker already running when the `set` landed.

`runtime::memory_only::tests` pin the add-only list
(`memory_only_session_globs_are_add_only`), the shared list read at each
check, by a worker too (`memory_only_session_globs_are_shared_and_read_at_each_check`),
and an MCP tool matched by its bare server tool name: `pria_vault_*` marks
`pria_vault_get` (runtime and wire name `ext__pria__pria_vault_get`) and not
`other_get` (`memory_only_policy_marks_an_mcp_tool_by_its_server_tool_name`).

`tools::extension::tests::memory_only_globs_match_an_extension_tool_by_its_own_name`
registers extension `pria-desktop` tool `pria_vault_get` and checks:

- its wire name is `pria-desktop_pria_vault_get` and its runtime name is
  `pria-desktop:pria_vault_get`;
- exactly these two globs mark it, as session globs and as config globs;
- `other:pria_vault_*` does not mark it;
- a built-in named `pria_vault_get` is not marked.
