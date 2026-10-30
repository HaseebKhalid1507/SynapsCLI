# `session_id` on hook events

Since daemon-mode phase 2 (C1), **every** hook event carries the owning
conversation id in the `session_id` field — not only the session-lifecycle
hooks.

| Hook                  | `session_id` before | `session_id` now                          |
|-----------------------|---------------------|-------------------------------------------|
| `before_tool_call`    | `null`              | `"<conversation id>"` (foreground session) |
| `after_tool_call`     | `null`              | `"<conversation id>"`                      |
| `before_message`      | `null`              | `"<conversation id>"`                      |
| `on_message_complete` | `null`              | `"<conversation id>"`                      |
| `on_compaction`       | new session id      | unchanged                                  |
| `on_session_start`    | session id          | unchanged                                  |
| `on_session_end`      | session id          | unchanged                                  |
| `tool.call` request   | absent              | `params.session_id`: the conversation that invoked the extension's tool (`null` for workers) |

The field already existed as `Option<String>` in the JSON schema
(`docs/extensions/contract.json`), so the change is **additive**: an
extension that ignores `session_id` sees the same shape it always did.

## When it is still `null`

- **Workers / subagents.** A worker runtime has no conversation id
  (`Runtime.session_id == None`), so its tool hooks still emit `null` —
  exactly as before. The only hook a worker runs is the `after_tool_call`
  output transform (extensions with `tools.transform_output`; see
  `hooks.md`, "Subagent workers"), and it arrives with `session_id: null`;
  `SYNAPS_HOOK_SESSION_ID=0` changes nothing for it. Plugins must not assume
  every tool call belongs to a session.
- **Extension-originated `tool.invoke`.** A tool invoked by a sidecar via the
  extension protocol runs outside a conversation turn; `null`.
- **`tool.call` outside a conversation.** The extension-provider tool route,
  skill commands and host-originated lease calls (memory capture/recall)
  send `params.session_id: null`.
- **Kill-switch `SYNAPS_HOOK_SESSION_ID=0`** (also `false`/`off`): forces
  `null` on the four tool/message hooks and on `tool.call` `params.session_id`. Use it if a plugin misbehaves on the
  new value. Session-lifecycle hooks are unaffected.

## Why: one sidecar, many sessions

In daemon mode one extension process serves every session in the daemon.
The reverse direction is routed too: `synaps daemon` runs the
notification router (`extensions::notify_router`), which delivers a
sidecar's `widget.*` frame to the session named by `params.session_id`
when the frame carries one, and to **every** live session when it does
not (daemon-global; the pre-phase-3 behaviour). See "Plugin contract" below.
"Last tool call"-style state keyed on nothing is now keyed on the wrong thing:
two sessions interleave their hooks on the same stdin. Key per-session state
on `params.session_id`, and treat `null` as "no session (worker)".

The in-tree helper is `HookEvent::with_session(Option<&str>)`; the runtime
emitters `emit_before_tool_call` / `emit_after_tool_call` take a trailing
`session_id: Option<&str>`.

## Plugin contract (daemon-mode phase 3, C2)

The runtime side is done: hooks carry `session_id`; the router keys
`widget.*` frames on `params.session_id`. A plugin that wants correct
behaviour with more than one live session must:

1. **Key its state on `session_id`** — from hook events and, for the
   extension's own tools, from `tool.call` `params.session_id` (the caller). One entry per session, created on
   the first hook that names the session, removed on `on_session_end`
   for that id. Treat `session_id == null` as "no session (worker /
   extension-originated tool call)" — never fold it into a session entry.
2. **Address outbound notifications.** Put `"session_id": "<id>"` in the
   `params` of every `widget.upsert` / `widget.dismiss` (and any future
   session-scoped notification). A frame without it is delivered to every
   session — acceptable only for genuinely global UI.
3. **Address injected events.** `synaps send --session <id>` (never
   `--broadcast`) for anything that is a reply to one session's activity.
   A parked session (phase-3 B3) is woken by an injected event, so a
   periodic global beat wakes every parked session; key beats per session
   and respect the park grace (`SYNAPS_DAEMON_PARK_GRACE_SECS`).

Concretely, for the two in-tree plugins that have state today:

| Plugin | Today | Contract |
|---|---|---|
| `heartbeat` (`~/.synaps-cli/plugins/heartbeat/main.py:76-105,150-183`) | one global `STATE["session_id"]`, captured from any hook; `fire_beat(--session\|--broadcast)` | `STATE: dict[session_id → {last_activity, beats, …}]`; capture per hook; `fire_beat --session <id>` per entry; delete the entry on `on_session_end`; `idle_threshold_sec ≥` park grace |
| `jawz-widget` (`~/.synaps-cli/plugins/jawz-widget/main.py:110-126`) | one global `st.mode` | `modes: dict[session_id → mode]`; every `widget.upsert` carries `params.session_id` (the widget then shows in the session that produced it, not in every client) |

Those plugin edits live in their own repos (`~/.synaps-cli/plugins`) and are
a follow-up PR there; nothing in this repo depends on them landing.

## Hosts without `tool.call` `params.session_id`

Builds before this field (0.10.2 and earlier) send `tool.call` with only
`name` and `input`. A plugin that has to support them can find out which
session called its tool from `before_tool_call`, which already carries
`session_id`:

1. Subscribe to `before_tool_call`. For your own tools only (match on
   `tool_runtime_name` `"<plugin-id>:<tool>"`), return `modify` with the
   input plus a single-use random token, for example
   `{"_caller": "<token>"}`. Remember `token → session_id` in memory.
2. In `tool.call`, pop the token from `input` and redeem it. An unknown or
   reused token means "no caller": treat it like `null`, never as a session.
   The hook overwrites any token the model supplies, and tokens are
   unguessable, so the model cannot pose as another session.

Prefer `params.session_id` whenever the key is present. The token pattern
has costs the field does not:

- It needs `tools.intercept`, a permission that lets the plugin block or
  rewrite every tool call it subscribes to.
- On `before_tool_call`, only `block` stops the handler chain. A `modify` rewrites the input and the chain continues: every later handler sees the modified input, a later `block` still wins, and a later `confirm` still prompts (approving runs the modified input). A `confirm` is recorded and the chain continues: a later `block` still wins, confirms from several extensions are merged into one prompt that names all of them, and a `modify` that arrives after a `confirm` is ignored and logged (the user confirms the input as it was when they were asked). Later handlers therefore run, with any side effects, before the user answers a `confirm`, and they are not told whether the user approved.
  So if an earlier extension blocks the call, or raises a `confirm` before
  your `modify`, your token is never applied. Extensions after you see the
  stamped input, token included.

Do not match a `tool.call` to the preceding `before_tool_call` by tool name
and input. Two sessions can make identical calls at the same time, and the
hooks and calls can arrive interleaved.

## In-tree plugin audit (2026-09, jade 16 / bella 2)

Plugins with hook handlers and what the new field means for them:

| Plugin (host)            | Hooks                                                   | Finding |
|--------------------------|---------------------------------------------------------|---------|
| `heartbeat` (jade)       | on_session_start, on_message_complete, on_session_end   | Captures `session_id` from **any** hook into a single global `STATE["session_id"]` ("defensive"). Single session: identical (same id). Daemon mode: last-writer-wins → beats target whichever session most recently completed a message. Needs per-session state on day 2; not broken. |
| `munder-hive-god` (bella)| before/after_tool_call, on_message_complete, session start/end | `payload.session_id = payload.session_id \|\| SESSION_ID` — it already prefers the hook's id and falls back to a registry-recovered one (comment notes "HookEvent.session_id is always null"). Now the hook value wins; correct per-session in daemon mode. Comment at `bridge.cjs:45` is stale. |
| `synaps-tasks` (jade)    | before_message, on_session_start, on_session_end        | Reads `session_id` only in `on_session_end` (velocity record). Unaffected. |
| `jawz-widget` (jade)     | before_message, on_message_complete, before/after_tool_call | Switches on `kind` only; keeps a global `mode` (thinking/coding/done). Ignores `session_id`; in daemon mode the widget reflects the union of sessions. Cosmetic. |
| `axel` (jade, Rust)      | before_message, on_session_start, on_session_end        | No `session_id` handling in hook path. Unaffected. |
| `chronos` (jade+bella)   | before_message                                          | Stateless inject. Unaffected. |
| `d20` (jade)             | before_message                                          | Stateless. Unaffected. |
| `crush` (jade, Rust)     | after_tool_call                                         | Stateless transform. Unaffected. |
| `jimmy-provider` (jade)  | on_session_start                                        | Already receives the id. Unaffected. |
| others (finlens, hubspot-tools, misfire, pria-dev-plugin, sample-sidecar, tmux-tools, weather-lens, web-tools) | none | No hook handlers. |

No plugin breaks on the additive field. Two (`heartbeat`, `jawz-widget`) hold
process-global "current session" state that becomes last-writer-wins under a
shared daemon sidecar — the day-2 audit item.
