# External session drivers

An optional `session.drive` extension permission lets a **user-invoked interactive
plugin command** propose continued foreground work. The autonomous reference
plugin is in [`examples/extensions/autonomous`](../../examples/extensions/autonomous/).
Its prompts, favorite models, retries and loop limits are Python plugin code, not
compiled into Synaps.

## Scope and authorization

The first implementation supports **local TUI sessions only**. Headless chat,
stdio RPC, WebSocket server and subagents do not activate drivers. Existing
extensions remain protocol v1; this is an additive permission and response
contract. Older hosts reject the unknown permission rather than silently enabling
unsupported automation.

Merely installing the plugin does not run the model. The host accepts a start
proposal only from the successful response to an explicit interactive command of
a loaded, permissioned extension. Hook injections, tool calls, `command.output`
text, sidecar frames and spontaneous notifications cannot authorize a driver.

Starting authorizes automatic paid requests and sending this session's retained
history to the selected providers. The ordered exact model/effort pairs in the
start proposal become the run's immutable allowlist; later responses cannot add
providers or change effort. Choose favorites accordingly. Ordinary tool approval,
model capability, context durability, attachment and subagent-completion gates
still apply. Model/effort changes are session-only, not new global defaults.

## Foreground context selection

Reference plugin **0.1.4** accepts
`/auto start [--turns N] [--for Nm|Nh|Nd] [--context auto|off] -- <goal>`.
The context default is **auto on every explicit start**; `--context off` opts out
of automatic context continuation for that foreground session. Flags can be mixed
in any order before the mandatory `--`, once each. Invalid context values,
duplicate flags (including identical ones), missing values and `--context=off`
are rejected. Everything after `--` is literal goal text, not a context setting.
The plugin always sends the selected value in structured Start, never via prompt
text or display output.

After accepting the explicit user's grant, the host applies `context_mode` using
the **same runtime-only validation and setting path as `/context auto` or
`/context off`**, before inference. A rejected grant does not change context mode;
failed context validation stops visibly, rather than falling back to a provider
retry or silently ignoring the setting. Ordinary context/history, retrieval and
durability gates still apply.

**Auto authorizes local archival of eligible session content**. Archives exclude
system/developer prompts, private reasoning, restricted/sensitive content and
binary blocks. Eligible text/tool evidence is screened/redacted; redaction is not
a universal secret detector. Off disables automatic context continuation, not
existing archives or normal capacity/history validation. The host and plugin
notices disclose the selected mode and local eligible archival/exclusions as well
as ongoing spend and cross-provider retained-history disclosure.

Like explicit `/context`, the **setting remains in the current runtime after
stop**; revocation does not restore the old mode. The current `/context` mode is
runtime only: it is not saved to disk or restored from a saved session across
restart. No global config or memory recall/capture consent changes. Resume and
restart never restore a driver grant. This patch covers **`/auto` foreground context only**;
actual delegated worker context defaulting is not changed, since workers use a
different durability consumer. Subagents do not gain driver authority.

## Drafts, submitted steering and control operations

Draft edits are local input only: **typing, paste, input-history navigation and
clearing/editing a draft never revoke the grant or send draft text**. Automatic
proposals neither consume the draft nor wait for an empty text input. Typing a
slash command is not dispatching it.

Submitting ordinary text while armed **steers the same run**, rather than
cancelling it or starting a competing foreground turn. The host delivers the
text to the owned active stream when possible. If the stream is ending, the
channel is closed, or there is no active stream, accepted submissions remain in
a **bounded FIFO**. The next authorized proposal turn includes them in order as
**separate history user messages**, distinct from the automated plugin prompt.
Full proposed-history validation precedes committing those messages. A full
queue rejects additional steering visibly and retains that input, not by
silently replacing earlier submissions. This queue cannot authorize work on its
own or escape a revoked/expired grant.

Accepted steering clears the host's completed-turn feedback comparison; it is
not itself a completed turn and does not change plugin success/retry counts.
A policy callback already dispatched before the steering submission still settles
exactly once: its selected favorite, delay, and genuine limit/stop decision are
not rolled back. Steering remains queued for its next authorized turn (or is
restored as draft if the run stops). New feedback comparisons start fresh;
steering never resets deadlines or bypasses a provider cooldown.
The same parent cancellation scope, monotonic deadline and exact model/effort
allowlist remain in force. Initial, continuation and recovery prompts must honor
**the latest human steering in the conversation**: an original goal repeated by
the plugin is historical context, never an override of later human instructions.
The reference plugin 0.1.3 retains the distinction introduced in 0.1.2 in every prompt, while
preserving normal authorization and safety gates. Steering text is delivered in
host history, not exported in plugin polls.

Staged attachments **never auto-send** as steering or as part of an automatic
proposal. While idle with attachments present, the driver pauses without
consuming them. Attachment submission while armed is refused with input retained
and a notice to **press Escape before submitting attachments** through the normal
user flow. Staging is not authorization to send; existing history attachments
remain subject to the ordinary media/preflight gates.

Submitted commands remain host control operations, not steering. **All commands,
including status, revoke** before dispatch; an explicit authorized start can
create a fresh grant. Escape, Ctrl-C cancellation, stop and quit remain stopping
operations even while idle/delayed. Lifecycle loss and safety boundaries still
stop; drafts or steering cannot revive an expired, cancelled or invalid grant.

## Structured result

An interactive `command.invoke` result may contain `session_driver`:

```json
{
  "session_driver": {
    "action": "start",
    "run_id": "fresh-id",
    "models": [{"model": "anthropic/claude-fable-5-1", "effort": "xhigh"}],
    "prompt": "Continue the user's requested work within its existing authorization.",
    "delay_ms": 1000,
    "max_duration_ms": 1800000,
    "feedback_version": 1,
    "context_mode": "auto",
    "notice": "Proposing a 30 minute foreground run with context auto. May incur ongoing charges and send retained history across providers. Auto authorizes local archival of eligible session content, excluding system/developer prompts, private reasoning, restricted/sensitive content and binary blocks. The setting remains in the current runtime after stop; it is not saved across restart. No global config or memory recall/capture consent change."
  }
}
```

The first selected model is `models[0]`. `max_duration_ms` is optional; omitted
means no elapsed-time deadline. The host enforces a supplied deadline even while
waiting or streaming. A deadline cancels work; it is not a promise all external
operations can be rolled back.

`context_mode` is an **optional strict Start-only field** in the host schema.
Omission leaves the existing runtime context setting unchanged for other/legacy
drivers. When present, it must be exactly the string `"auto"` or `"off"`; null,
nonstrings, other values and duplicate fields are invalid. Plugin **0.1.3 always
sends the selected value**, including the default `"auto"`. It adds no field to
initialize, Next or polls. **Update host and plugin together**: strict older hosts
reject the unknown Start field, even if they support `feedback_version`. This is
intentional fail-closed compatibility; the plugin never drops the field and
retries or activates through printed output. Legacy poll compatibility does not
imply that an old host accepts the new Start.

`stop` and `status` responses have only `action` and optional `notice`. Status does
not confer authority. A response without `session_driver` retains its ordinary
command behavior.

While armed, the host makes at most one asynchronous decision request at a settled
foreground-turn boundary using the existing `command.invoke` transport. Its
command is `__session_driver__`; `args` contains **one JSON-encoded string**:

```json
{
  "run_id": "fresh-id",
  "decision_id": "unique-host-decision-id",
  "outcome": "success",
  "error_kind": "none",
  "model": "anthropic/claude-fable-5-1",
  "effort": "xhigh"
}
```

An optional start `feedback_version: 1` opts that grant into a seventh poll field,
`feedback`, with only `unknown`, `changed`, `repeated` or `empty`. Absent opt-in,
legacy polls retain exactly their original fields. Other versions are rejected.
The reference plugin v0.1.4 requires a host accepting these Start fields,
`feedback_version`, `context_mode` and `time_checkpoint_version`; update both host and plugin.

Feedback describes a **completed foreground turn**, not semantic task progress.
A bounded host-local tracker compares text-only output, or finalized tool calls
and results while ignoring accompanying prose, against four prior successful
turn fingerprints under the same exact model/effort. Tool IDs and thinking are
ignored. Interrupted attempts never enter that ring. Empty output is reported
separately; overflow yields `unknown`. The host sends no fingerprint or body.
Non-success polls carry `unknown`. Plugin policy chooses whether/when to switch;
the reference plugin switches after three consecutive repeated/empty signals,
without overriding the successful-turn or duration limits.

This does **not** interrupt an in-flight tool loop or prove that changed output
made useful progress. Tool/policy/non-time-budget/cancellation boundaries still stop;
feedback cannot convert them into retryable errors. No transcript, tool results,
raw provider errors or credentials are included.
Outcomes are `success`, `provider_error`, `selection_rejected`, `time_checkpoint` (opted-in grants only) and `blocked`.
Cancellation revokes the grant instead of asking the plugin whether to continue.
Only known provider failures may be retried; local policy/session/tool/non-time-budget
failures and ambiguous side effects stop continuation.

A continuation response is:

```json
{
  "session_driver": {
    "action": "next",
    "run_id": "fresh-id",
    "selection": {"model": "anthropic/claude-fable-5-1", "effort": "xhigh"},
    "prompt": "Continue the remaining authorized work, honoring the latest human steering in the conversation. Treat the original goal as historical context; preserve completed work and all safety gates.",
    "delay_ms": 1000,
    "notice": "Continuing with the same model."
  }
}
```

A plugin may instead return `stop`. It must deduplicate `decision_id`: the existing
extension transport can retry a request after failure. Process restart must never
restore an active run automatically. The reference plugin retains run state only
in process memory.

## Host constraints

- One outstanding callback/proposal; callbacks have a five-second timeout and run
  outside the TUI event handler. Delays are 1–300 seconds.
- Replies are bounded to 64 KiB, prompts to 16 KiB, notices to 2 KiB and favorites
  to 16 unique qualified model identities. Invalid/unknown fields fail closed.
- Checked model/effort mutation and complete proposed-history validation precede
  committing a new user message. Unsupported effort is not silently downgraded.
- Run/session/extension identity and live process generation are checked at
  command dispatch, reply acceptance, every active timer tick and before stream
  setup. Unload/reload, detected process/transport loss, cancellation, control
  commands, competing work and session replacement invalidate pending work;
  draft edits and accepted steering do not. An observed-live check cannot make
  process exit atomic with a subsequent request.
- Escape/Ctrl-C stop automation, including while waiting. All dispatched commands
  (including status), stop and quit revoke the grant. Revocation
  cancels foreground work and reactive workers, fences late worker registration,
  and disables event-triggered inference until an explicit user submission or
  new start. Workers still require ordinary collection/reconciliation; stopping
  is not an implicit reconciliation or rollback of external effects.
- Events successfully steered into an owned in-flight turn do not cause another
  turn or revoke authority. Buffered/idle competing events stop the driver.
  Successful same-session durable pressure checkpoints retain the current grant;
  failure or a changed session revokes it. Nothing restores a grant on resume.
- Prompts are submitted as text user messages, never parsed as slash commands or
  interpreted as arbitrary host controls.

Extension permissions are API gates, **not an operating-system sandbox**. Install
only trusted local plugins. No paid provider smoke test is implied by offline
protocol and state-machine tests.


## Automatic wall-clock continuation (0.1.4)

The external plugin proposes Start-only `time_checkpoint_version: 1`. This
explicitly opts its grant into `outcome: "time_checkpoint"` with
`error_kind: "wall_clock"`. Older grants that omit the field retain blocked-on-time
behavior. Unsupported versions/types or contradictory callback metadata fail
closed. Update host and plugin together; an older strict host rejects this Start.

Only typed host wall-clock exhaustion qualifies, never error-message text,
cost/tool-call/provider-round limits, storage failures, policy denials, cancellation
or ambiguous side effects. After history repair the normal same-grant lifecycle,
worker, history, permission, media and deadline gates still run before polling
or sending another request. Zero elapsed allowance stops instead of busy-looping.

The plugin submits a new turn on the **same exact model/effort**, after the normal
one-second delay, using retained history and the latest steering. This is not
provider fallback or success/repetition feedback. Successful-turn counts remain
unchanged (`--turns` still counts successful turns); retries/repetition streak
reset. Duplicate callbacks are idempotent. `--for`, Escape and revocation remain
unchanged. No run is restored or started on load/restart.

This also works with `/auto start --context off -- ...`. Separately, `/context auto`
starts a fresh wall-clock segment after **every acknowledged durable successor**.
Time exhaustion can force that same archive/head barrier at low context pressure;
no provider request runs on an unacknowledged head. Other cumulative resource/cost
limits remain unchanged. A context-enabled stream that reaches a normal final
answer still ends; `/context auto` is not the independent infinite `/auto` loop.
