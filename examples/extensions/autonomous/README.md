# Autonomous — external session-driver plugin

A separately installable **Python 3.8+ standard-library** plugin. It owns the
entire autonomous loop policy: prompts, successful-turn counts, duration limits,
exact ordered favorites, retries, failover and cooldown. There are no provider
clients, subprocess tools, network requests, dependencies or setup scripts.

**First release: local interactive TUI only**, on a Synaps build implementing
`session.drive` and the session-driver API. Headless chat, RPC, server and
subagents do not gain driver authority. Older hosts must not activate a run
from printed output. This example does not install or bundle a Synaps binary.

Loading/initializing the eager extension does **not** start inference. It has
one interactive command, `/auto`, no model-callable tools, no hooks, and only
the `session.drive` permission. Only the host can authorize a proposal from an
explicit local user command; the plugin cannot authenticate that user itself.

## Files and optional manual placement

```text
autonomous/
  .synaps-plugin/plugin.json
  main.py
  tests/test_autonomous.py
  README.md
  prefs.json                    # created only by favorites set/reset
```

No installation is needed to run the tests below. To install later, explicitly
copy this directory (including the hidden manifest) as
`~/.synaps-cli/plugins/autonomous/`, using the normal plugin discovery workflow.
The installed directory must be owned by you and not writable by other users:
mode `0700` or `0755`, **not `0775`**. Use a real directory, not a symlink. Review
the plugin before granting `session.drive`. Python is the interpreter, not a
package to install; no `pip`, binary download or install script is involved.

## Commands

```text
/auto start -- Inspect the repository and finish the authorized task
/auto start --turns 5 -- Inspect and improve the tests
/auto start --for 30m -- Continue the authorized maintenance work
/auto start --turns 20 --for 2h -- Finish the requested implementation
/auto start --context off --turns 5 -- Inspect without automatic context continuation
/auto start --for 30m --context auto --turns 10 -- Continue the authorized work
/auto status
/auto stop
/auto favorites
/auto favorites set provider/model ultra another-provider/model max
/auto favorites reset
```

- `--` is mandatory before the goal. Everything after it is goal text, including
  things that look like flags. The TUI splits on whitespace; this is **not shell
  parsing**, quotes/escapes are not interpreted and whitespace is normalized.
- `--context auto|off`: defaults to **`auto` on every explicit start**, even if
  this session previously used `off`. `off` explicitly disables automatic context
  continuation for the foreground session. See the disclosure below.
- Flags may appear in any order **before `--`**, once each. Unknown options,
  missing values, repeated flags (even with identical values), `--context=off`,
  and context values other than exact lowercase `auto` or `off` are rejected.
  `-- new prompt goal --context off` is all goal text and still defaults to auto.
- No limits means **infinite by default**. It can continue incurring charges
  until cancelled; finishing the task in prose does not revoke the host grant.
  Prefer explicit limits for unattended use. Completion prompts prohibit
  inventing more work or repeating completed external actions.
- `--turns N`: positive decimal integer `1..1000000`, no leading zeros. Counts
  **successful foreground turns including the initial turn**. Provider errors
  and zero-send selection rejections do not count. Intermediate tool results
  are not foreground-turn completions.
- `--for duration`: positive integer followed by lowercase `m`, `h` or `d`
  (minutes/hours/days), at most 365 days. Examples: `30m`, `2h`, `7d`.
  Fractions, seconds, compound units, repeated flags, `--turns=N`, and unknown
  options are rejected. Either of the two limits ends a bounded run.
- Limits are **never inferred from prose**: `-- work for 30 minutes` still has
  no duration limit. Use `--for 30m -- work` instead.
- Goals must be nonblank valid UTF-8, at most 12 KiB. Generated prompts remain
  below the host's 16 KiB limit. Unsafe control characters are rejected.
- `status` and favorites operations never start a run. Favorites changes affect
  only a subsequent explicit start, never the current pinned allowlist.

## Foreground session context (0.1.3)

Every structured Start sends the selected `context_mode`, including the default
`"auto"`; it is **not inferred from the goal or a prompt instruction**. Only after
accepting the explicit user's grant does the host apply the same runtime-only
validation and setting path as `/context auto` or `/context off`. Unsupported or
invalid context setup fails closed before inference; it is not a retryable
provider/model-selection error. A rejected grant must not change context mode.

**Auto authorizes local archival of eligible session content** for automatic
context continuation. The archive excludes system/developer prompts, private
reasoning, restricted/sensitive content and binary blocks. Eligible ordinary text
and tool evidence are screened/redacted; redaction is not a universal secret
detector or a promise to preserve every original byte. Normal context durability,
retrieval, media, permission and safety gates remain in force. `off` disables this
automatic continuation; it does not delete existing archives or bypass capacity
and history validation.

The selected **setting remains in the current runtime after stop**, just
like explicit `/context`; stopping does not restore the previous mode. Use
`/context off` or `/context auto` explicitly to change it afterward (a submitted
command still revokes an armed driver). This is not a global config change or a
plugin preference, and it does **not** enable or change memory recall/capture
consent. The current `/context` mode is runtime only: it is not saved to disk or
restored from a saved session across restart. Resume and restart never re-arm a run.

This change covers **`/auto` foreground context only**. Actual delegated worker
context defaulting is out of scope: workers use a different durability consumer.
It does not enable worker context continuation or grant subagents driver authority.
The Start notice discloses the selected mode, archival exclusions and that the
setting remains in the current runtime after stop, alongside the existing
spend/cross-provider history warning.

## Drafts, steering and stopping

**Draft edits never revoke the run or submit the draft.** Typing, pasting text,
input-history navigation and clearing/editing the input leave automation armed;
automatic proposals do not consume or send that unfinished text. Merely typing
a slash command is still a draft, not a control operation.

**Submitting normal text steers the same armed run.** The host sends it to the
owned active stream when possible. If the stream is ending, its steering channel
has closed, or no stream is active (including delays/cooldown), the host retains
accepted text in a bounded FIFO for the next authorized proposal turn. Each
submission becomes a **separate history user message**, not part of the plugin's
prompt and not a replacement run. Full history validation and the existing
permissions still apply; the queue cannot launch work after revocation or a
limit. If the queue is full, the host retains the rejected input and tells you,
rather than silently overwriting earlier steering. Steering does not renew the
parent cancellation scope or deadline, change exact model/effort authorization,
or reset successful-turn/retry policy counts. Accepted steering clears the host's
feedback comparison so prior output is not mistaken for repetition of the new
direction; it does not itself count as a completed foreground turn.

**Staged attachments are not text steering and never auto-send.** If attachments
are present, the driver pauses while idle without consuming them. To submit
attachments, **press Escape to stop automation first**, then submit through the
normal attachment flow; the host tells you this and retains the staged input.
Staging attachments does not itself authorize sending their bytes to a provider.

Use **Escape to revoke even while idle/delayed**, or `/auto stop` (including
while streaming where supported by the TUI adapter). Ctrl-C cancellation and
quit also stop automation. **Submitted commands are control operations and all
revoke the host grant, including `/auto status`**. Model changes, session
replacement and plugin unload/reload/death also revoke it; lifecycle and safety
gates still stop work. The plugin's bookkeeping may still hold its old immutable
model list. A new explicit start creates a fresh run, not a resume. Revocation
cancels the foreground and its reactive workers, and disables automatic event
wakes until you submit new work or explicitly start another run. Cancellation
cannot undo external effects already performed. Events steered into the owned
active turn and successful same-session context-pressure checkpoints do not
themselves stop the loop.

The host owns live cancellation and the grant. It need not notify the plugin on
Escape or other host-side revocation. Accordingly `/auto status` labels its data
as **plugin run bookkeeping**, not proof of a currently armed host; submitting
that command revokes the host grant too. The plugin never sends anything
unsolicited and cannot continue without host polls.

## Exact favorites and retry policy

The default order is exactly:

| Order | Qualified model | Effort |
| --- | --- | --- |
| 1 | `openai-codex/gpt-6-astra` | `ultra` |
| 2 | `anthropic/claude-fable-5-1` | `xhigh` |
| 3 | `kimi-code/k3` | `max` |
| 4 | `x-ai/grok-4.6` | `high` |

There are no spelling aliases, capability probes, automatic effort reductions
or substitutions. **`ultra`, `ultracode`, `max` and `xhigh` are distinct.** Valid
canonical efforts: `off`, `adaptive`, `low`, `medium`, `high`, `xhigh`, `max`,
`ultra`, `ultracode`. For example, `none`, `med`, `x-high` and `Ultra` are rejected.

`favorites set` takes 1..16 complete qualified-model/effort pairs and replaces
the whole list. Models must be unique even if efforts differ; identifiers are
case-preserved ASCII, at most 256 bytes. Use exact `provider/model` names;
additional slash-separated model segments are allowed. Provider segments allow
letters, digits, `.`, `_`, `-`; model segments additionally allow `:`. Each
segment starts with a letter/digit. The host still validates actual availability
and exact reasoning support. Unsupported exact choices are visibly skipped
through `selection_rejected`, never silently downgraded.

Availability caveat for the current host catalog: `anthropic/claude-fable-5-1`
has no static model row, and `x-ai/grok-4.6` is not the host's native `xai-auth`
route. These requested defaults are preserved verbatim, not rewritten to a
different model/provider. Without exact supported catalog/route evidence they
are visibly rejected and skipped; the four-entry preference list is not a
claim that all four choices are usable today.

| Settled outcome | Plugin decision |
| --- | --- |
| Success | Increment successful-turn count and enforce limits first; otherwise continue after 1 second, subject to completed-turn feedback below. |
| First/second consecutive `repeated`/`empty` successful turn | Send fixed course-correction guidance on the same favorite after 1 second; limits still win. |
| Three consecutive `repeated`/`empty` successful turns on one favorite | Immediately select the next exact favorite; reset retries and feedback streak. No additional provider retry first. |
| Auth/quota provider error | Immediately advance to the next exact favorite, with a 1-second submission delay. |
| Transient/rate-limit provider error | Up to **3 retries** on that favorite at **2, 4, 8 seconds**; the next failure advances. Success or advancing resets the retry count. |
| Selection rejected | Skip that exact model/effort without counting a turn. |
| Entire remaining list exhausted (failures or repeated/empty feedback) | **5-minute cooldown**, then first favorite; retry count and feedback streak reset. |
| Blocked, unknown provider error, invalid callback | Stop visibly; require human attention/new explicit authorization. |

Host/provider internal transport retries are separate from these finite
foreground-attempt retries. All emitted delays are `1000..300000` ms. The
plugin does not sleep: the TUI schedules proposals and remains cancellable.
Every fallback notice names **both the previous and newly selected exact
model/effort**, including wraparound to the first favorite after cooldown.

### Completed-turn repetition feedback (version 1)

Plugin **0.1.5** retains the integer `feedback_version:1` opt-in introduced in
0.1.1 on its structured `start` reply. A supporting host exports only one of four feedback enum labels:
`unknown`, `changed`, `repeated`, or `empty`. **No transcript, output text or
hashes are sent to the plugin.** This host-side loop detector is a **bounded
exact-repetition heuristic at completed foreground turns only**. For turns
with tools, tool fingerprints ignore accompanying assistant prose: changing
that prose does not hide repeated tool outputs. Nonterminal tool errors can
contribute to detection only through repeated outputs at successful completed
turns; they are not treated as provider failures. **Blocked tool/policy actions
never retry**: gates still stop the run, irrespective of repetition feedback.

The plugin counts consecutive successful turns labeled `repeated` or `empty`
on the same active exact model/effort; mixtures count toward the same threshold
of **3**. The first and second send fixed course-correction guidance on the
same favorite after 1 second, without resetting the streak. The third immediately
advances selection, without an additional retry
on that favorite (the normal 1-second submission delay still applies, or
5-minute cooldown at the end of the list). Successful-turn count increases as
usual, including empty/repeated turns, and turn/duration limits take precedence
over recovery. `changed`, `unknown`, or **missing feedback** resets the streak.
Provider failures, selection rejection, advancing favorites, and a fresh
explicit start/restart also reset it. A successful turn resets provider retries
as before. The host's initial sample may be `unknown`; the threshold counts
feedback labels, not an assumption that the first sample was already a repeat.

This is **not semantic progress proof**, and provides **no in-flight supervision
or interruption**. It cannot detect every stalled approach, an infinite tool
loop inside one foreground turn, or task completion from prose. Unknown or
bounded-detector overflow feedback does not trigger a switch: the host exports
`unknown` (not a fifth `overflow` label), which resets the streak. The plugin
never parses arbitrary prose or raw errors to invent feedback. Existing host
cancellation, permission and deadline enforcement remain essential.

Duration uses `time.monotonic()`, checked on polls, duplicate responses and
status. A proposal is stopped rather than queued if its delay would reach or
pass the deadline. Start emits `max_duration_ms` only when `--for` is present;
the host must enforce its **own monotonic deadline**, including while delayed
or streaming. Plugin polling alone cannot interrupt an in-flight turn.

Every initial, continuation and recovery prompt explicitly **honors the latest
human steering in the conversation**, including separate history user messages.
The original goal is still included so it survives initial zero-send selection
rejection, but is labeled **historical context, not an override of later human
instructions**. Repeating it in an automated proposal is not a new request to
restore superseded work. The plugin does not receive steering text in callbacks
or rewrite its stored original goal; the model follows the human conversation.
Inspect retained work; **do not replay completed external actions**, including
partially executed work before a provider failure. Do not expand authorization.
All normal permission, confirmation, media, context and safety gates still
apply. Fallback/recovery continuations say feedback suggests repetition/stalling
or empty output, the provider attempt failed, or the exact selection was
rejected; they tell the model to inspect retained results and try a
**materially different approach**, never replay side effects or bypass gates.
This fixed guidance also accompanies provider retries and cooldown recovery;
it contains no raw error content and does not override the latest human steering.
Normal successful continuations keep the nominal bounded prompt. Human-needed
steps must stop work and ask the user. Since callbacks contain no transcript,
the plugin cannot infer a human-needed request or task completion from assistant
prose; the host's blocked/permission boundaries and human cancellation remain
essential.

**Spend/privacy:** a run may incur ongoing charges and share retained
conversation history across the approved favorite providers. The initial
notice discloses this; the host must display its grant disclosure too. No
plugin retry can bypass permission/context/history validation.

## Redundant confirmation requests (0.1.5)

Every start, continuation and recovery prompt explicitly identifies itself as
**automated continuation, not new human input or approval**. If actual human
instructions already clearly authorize the next step, the model is told to act
rather than require a magic phrase such as “Approved” or “Authorized”. An
assistant's own request to reconfirm does not create a new approval requirement.
Latest human steering still takes precedence over the historical original goal.

On the first `repeated` or `empty` completed-turn feedback, the plugin sends a
fixed correction asking the model to re-check actual human instructions. The
second does the same; the existing third-label failover and full-list cooldown
are unchanged. Repetition is **not** evidence that consent exists or a gate is
redundant. No transcript text, approval keywords or additional feedback fields
are collected or classified. `changed`, `unknown` and missing feedback restore
the ordinary continuation prompt. Limits and duplicate-decision safety still
apply before correction, and typed blocked outcomes remain terminal.

The plugin **never sends a fabricated human “I approve”**. Explicit human review
checkpoints, unclear scope, new permissions, missing decisions and credentials
still require real human input. The correction says to name the concrete blocker
and wait, not retry the blocked action. Switching models confers no approval.

This is a prompt-level mitigation, **not guaranteed semantic loop detection or
an automatic prose-level pause**. Different wording can evade exact-repetition
feedback. A completed answer saying “please approve” or “finished” still counts
as a successful turn: it does not itself revoke the host grant or stop charges.
Use Escape, `/auto stop`, or explicit turn/duration limits to stop automation;
the normal host permission gates remain enforced independently.

## Preferences and privacy

Only `prefs.json` adjacent to the installed `main.py` is persisted **by the plugin**,
and only by explicit `favorites set/reset`. The host's context mode is runtime
only, like explicit `/context`; it is not saved in preferences, on disk or in a
saved session for restoration across restart. The preferences format is:

```json
{"version":1,"favorites":[{"model":"provider/model","effort":"ultra"}]}
```

The schema is exact: unknown fields, duplicate JSON keys, invalid efforts,
duplicate models, non-integer versions, and files over 8 KiB are rejected.
Initialization reads preferences but creates no directories/files. Missing
preferences means the exact defaults above; **unsafe/corrupt preferences do
not silently fall back** and block start until explicitly fixed/reset.

No goal, run id, counters, deadline, poll cache, active grant or credentials are
written. Process restart/reinitialization forgets the run; old polls return
`Stop`, even if favorites survive. The plugin does not read API credentials.

Storage deliberately ignores `HOME`, XDG variables, `initialize.config`, and
`initialize.plugin_root`: it is tied to the actual installed script directory,
not the caller's working directory or an ambient user/project config path.
The host scrubs most environment variables (HOME currently survives; XDG config
variables need not), so storage does not depend on them.

On POSIX, operations pin a directory descriptor, refuse symlinks in the entire
path, require an owned directory without group/world write permission, and
require any existing prefs file to be an owned, single-link regular file with
mode **0600**. Nonblocking/no-follow opens avoid FIFOs/symlink targets. Writes
use an exclusive mode-0600 temporary file in the same directory, fsync, atomic
rename and directory fsync. Unsafe paths are never repaired/overwritten
silently. Symlink targets are never followed; the final entry is rechecked
before replacement. This protects against other users, not hostile code
already running as the same OS user. No cross-process lock: concurrent
explicit preference updates are atomic, with last-writer-wins semantics.
Unsupported no-follow platforms fail closed for preferences. Install/copy to
a safe writable directory instead of weakening file protections.

## Wire contract and bounds

JSON-RPC 2.0 over binary stdio, existing **Content-Length** framing. `stdout`
contains only framed messages; no prompt/shell/tmux injection. Methods:

- `initialize`: protocol 1, empty tools/providers, no driver reply.
- `command.invoke`: `{command:"auto", args:[...], request_id:"..."}`.
  Structured `result.session_driver` returns `start`, `status`, or `stop`.
  Favorites additionally emits bounded `command.output` table/done display
  notifications correlated by `request_id`; display text conveys no authority.
- `command.invoke` with reserved `command:"__session_driver__"`, `args:[JSON]`:
  returns structured `next` or `stop`, with no display notifications.
- `hook.handle`: inert `continue`; `tool.call`: method-not-found, no tools.
  `info.get`: passive command inventory. `shutdown`: forget state and exit.
  Incoming JSON-RPC notifications cannot mutate state or start a run.

`context_mode` is an **optional strict Start-only field in the host schema**:
omission leaves the existing session context setting unchanged for other/legacy
drivers. When present it must be the string `"auto"` or `"off"`; null, nonstrings,
other values and duplicate fields are invalid. This plugin **always sends it**,
including the default `"auto"`. It adds no field to initialize, Next or polls;
`context_mode` in a poll is rejected as an unknown field.

Poll payload (the existing six required string fields plus optional feedback;
no transcript, secrets, hashes or raw errors):

```json
{"run_id":"start-run-id","decision_id":"decision-1","outcome":"success","error_kind":"none","model":"openai-codex/gpt-6-astra","effort":"ultra","feedback":"repeated"}
```

`feedback` is an **optional string** restricted to exactly `unknown`, `changed`,
`repeated`, `empty`; null, nonstrings, arbitrary text, extra fields, missing
required fields, and duplicate JSON keys are rejected safely. On any nonsuccess
outcome it must be **`unknown` or omitted**; other labels stop the run before
accounting/retry. Six-field legacy polls without feedback remain accepted and
behave as before (no feedback fallback, and any existing feedback streak resets).
The `feedback_version:1` opt-in is on **Start only**, not initialize, Next or the
poll. A supporting host sends feedback only for an opted-in grant. **Update the
host and plugin together**: a strict older host that does not support
`context_mode` rejects Start as an unknown field, even if it already supports
`feedback_version`. This is intentional fail-closed compatibility, not a silent
fallback to starting without the selected context mode. Older hosts lacking
`feedback_version` reject that field too. Six-field legacy-poll compatibility
never claims those hosts accept the new Start; the plugin does not drop either
field and retry or use printed output to activate a run.

`run_id` must match the current run. `decision_id` is 1..128 ASCII
letters/digits/`-`/`_`; production uses fresh UUIDs, tests may use `decision-1`.
Before **accounting any new outcome**, model **and** effort must equal the last
emitted selection. Changed payload under a cached decision id, a stale cached
decision after another decision, or a mismatched run/selection stops the run.
The fingerprint includes optional feedback's **presence and value**: even
omitted versus `unknown` under the same decision id is a changed payload.
The latest identical decision returns a copy of the cached result without
counting successful turns, feedback streaks or retries again (including a
failover, whose old selection is no longer current). A duplicate cannot bypass
stop, restart, cooldown delay validation, or a deadline.

The LRU retains at most **256 decisions** per process run and is not persisted.
This is a bounded transport-retry window, not an unlimited replay database:
the host must serialize polls and retry only the pending decision, not replay
older evicted identifiers. It must never submit a cached proposal twice.
`error_kind` accepts the coarse known kinds; `none`/empty/`unknown` on success
are no-error markers. Unknown provider errors stop, even though zero-send
`selection_rejected/unknown` still advances to the next exact choice.

Resource bounds: 64 KiB frame bodies/replies, 4 KiB total headers, 1 KiB header
lines, JSON nesting at most 32, 4 KiB poll JSON, 4096 command arguments/32 KiB
command text. Only integer JSON numbers are needed/accepted. Reject duplicate
Content-Length, duplicate JSON keys, nonfinite numbers, invalid UTF-8,
truncation and oversized/deep bodies. Malformed framing terminates the process
without unsafe resynchronization or echoing input. The host's bounded RPC
poll timeout remains responsible for stalled/incomplete transport peers.

## Offline verification

From the repository root, serial and with bytecode writes disabled:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover \
  -s examples/extensions/autonomous/tests -v
```

Tests use fake monotonic time plus real copied subprocesses/pipes. All
preferences, HOME overrides and script copies are temporary fixtures. They
cover exact defaults, context auto/off Start-only fields and disclosures,
mixed-order context/limit flags, literal goal separators, rejected invalid/duplicate/
missing values without state mutation, invalid limits/favorites, finite retries, exhaustion,
completed-turn feedback thresholds/mixes/resets, limit precedence, initial,
continuation and recovery prompts honoring latest human steering without weakening
authorization, deadlines, successful-turn accounting, immutable active selections,
duplicate feedback fingerprints, invalid optional feedback, legacy polls, real
wire opt-in/feedback, cooldown/restart, private/atomic persistence, notification
nonactivation, maximum goal/prompt/reply bounds, UTF-8 byte lengths,
fragmented/coalesced frames and malicious bounded input. They exercise the
plugin contract with synthetic host labels, not the host's fingerprint detector.
No inference, network, paid API, installation, Rust build/test,
or real user configuration access is involved.


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
