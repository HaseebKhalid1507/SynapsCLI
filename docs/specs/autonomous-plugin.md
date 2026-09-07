# External autonomous session driver — implementation contract

## Goal / boundaries
Implement `examples/extensions/autonomous/` as a separately installed Python3 stdlib plugin. All loop prompts, run limits, retry/backoff/favorite policy live outside Rust. Core adds a generic explicitly user-authorized session-driver seam; no built-in auto command/policy/favorites. Preserve dirty tree. Builds/tests at most 8 compile workers aggregate; foreman owns Rust runs.

Current reference plugin: **0.1.5**. The frontend is **local TUI only**; headless chat, RPC, server and subagents remain unchanged and MUST NOT silently gain driver authority. Never auto-start at load, hook, model tool call, resume or restart. No live inference or installation without explicit further user request. User cancellation revokes the driver, including pending async callbacks. Existing permission/confirmation/context/media/orchestration gates remain unchanged.

## Additive protocol using existing command.invoke
New declared permission `session.drive` (permission metadata/contract updated). No new hook kind. Only explicit local interactive command response may arm a driver. Other command consumers ignore driver response; extension tools cannot arm it.

Top-level command result may contain `session_driver` object:
- `{ "action":"start", "run_id":"opaque-bounded-id", "models":[{"model":"qualified/provider-model","effort":"ultra"},...], "prompt":"initial goal", "delay_ms":1000, "context_mode":"auto", "notice":"..." }`
- `{ "action":"stop", "notice":"..." }`
- `{ "action":"status", "notice":"..." }`
`start` MUST validate permission, request came from explicit user interaction, nonempty run_id, exact qualified allowlist max16, nonempty bounded text prompt <=16KiB, known parseable effort, model uniqueness, delay 1000..300000ms. Initial selection is models[0]; Start has no separate `selection` field. Start may include `max_duration_ms`, integer `feedback_version:1` and optional strict `context_mode:"auto"|"off"` (see below). The reference plugin always sends its selected `context_mode`, including the default `"auto"`. Successful-turn accounting is plugin policy; the host also enforces the proposed duration deadline. Arming displays spend/cross-provider history disclosure; prompt goes through normal frontend preflight and submission. Model/effort changes are session-only and atomic (clone Runtime, checked try_set_model + checked set_reasoning_level + validate complete message history before commit). Never silently clamp effort or mutate config. Host model allowlist pinned to start response; a poll cannot expand it. Disallow replacing an active grant without explicit user command.

Host polls ONLY an armed plugin at a settled foreground-turn boundary, with no competing queued work, modal, or compaction. Draft text and the driver-owned steering FIFO are not competing work and do not invalidate callbacks/proposals. Staged attachments pause idle progress without being consumed. Asynchronous task, bounded 5s timeout, no manager locks across plugin IO. At most one call/pending prompt. Pending results invalidated by cancellation, dispatched control commands, session replacement or plugin unload/disable/reload/lifecycle loss, not draft edits or accepted normal-text steering. No callback on intermediate response/tool completion. Pin owning session and handler/generation; fail closed on lifecycle loss.

Poll invokes reserved command `__session_driver__`, args `[JSON-string]`, request_id fresh UUID, using existing invoke_command/collected frame contract. Request object:
`{"run_id":"...","decision_id":"unique-host-decision-id","outcome":"success|provider_error|selection_rejected|blocked","error_kind":"none|auth|quota|rate_limit|transient|unknown", "model":"...", "effort":"..."}`
Only coarse metadata, no message bodies/transcript/secrets. A success follows final Done/stream end, provider_error follows terminal error classified conservatively; unknown/local/permission/context errors => blocked. selection_rejected is a zero-send model/effort validation error only; invalid media/history => blocked. Cancellation revokes without callback.
Poll result `session_driver`:
- `{ "action":"next", "run_id":"...", "selection":{"model":"...","effort":"..."}, "prompt":"Continue...", "delay_ms":1000..300000, "notice":"..." }`
- `{ "action":"stop", "notice":"..." }`
Reject malformed/oversized output, bad run id, out-of-grant model/effort, plugin errors/timeouts => stop visibly. No asynchronous plugin-to-host commands; no injection via display text or shell/tmux.

## Plugin UX / policy
Interactive `/auto start [--turns N] [--for 30m|2h|Nd] [--context auto|off] -- <goal>` (both limits omitted => infinite; either limit ends run, turns counts successful foreground turns including initial; context defaults to **auto on every explicit start**). Flags can be mixed in any order before the mandatory `--`, once each. Reject invalid or non-lowercase context values, missing values, duplicate flags even with identical values, and `--context=off`. Everything after `--` is literal goal text: `-- new prompt goal --context off` still selects default auto. `/auto stop`, `/auto status`, `/auto favorites`, `/auto favorites set <qualified-model> <effort> ...`, `/auto favorites reset`. Parsing is deterministic; reject missing goal/bad/ambiguous flags, limits not guessed from prose. Explain limit flags in docs; optional natural-language model tool only if safely scoped later (not necessary first release). `status` and favorites configuration never start a run. No live grant persistence/resurrection. User config preferences can persist plugin-locally privately with atomic write, no secrets. Default favorites exactly:
1 openai-codex/gpt-6-astra ultra
2 anthropic/claude-fable-5-1 xhigh
3 kimi-code/k3 max
4 x-ai/grok-4.6 high
No silent spelling aliases or effort downgrades. Unsupported exact choice visibly skipped via selection_rejected. Account auth/quota => next favorite; transient/rate_limit => finite retries per favorite, exponential backoff; repeated errors => next. Exhausted list => bounded 5min cooldown then first favorite (deadline still enforced). Explicit duration uses plugin monotonic checks and a mandatory host monotonic deadline when `max_duration_ms` is supplied (see next section); no submission at/after deadline, including while delayed. Unknown/blocked stops. Error does not count success; no duplicate replay of original user actions after provider partially executed tools: continuation prompt asks inspect retained work and continue, not rerun initial task.

Every initial/continuation/recovery prompt MUST honor **the latest human steering in the conversation**, including human text supplied as separate history user messages. Keep the original goal to survive an initial zero-send selection rejection, but label it **historical context, not an override of later human instructions**. Its repetition by the plugin is not a fresh request to restore superseded work. The plugin does not receive human steering text in callbacks or rewrite `run.goal`. Provider retries, exact-selection fallback, repeated/empty-output recovery and cooldown retain this guidance plus the no-replay, existing-authorization, permission/confirmation/context/safety and human-needed-stop instructions. Recovery asks for a materially different approach to remaining authorized work, never a bypass of gates.

## Foreground context selection (0.1.3)
`context_mode` is an optional **Start-only** strict host field: when omitted by other/legacy drivers, leave the existing runtime setting unchanged; when present, accept only the strings `"auto"` and `"off"`. Reject null, nonstrings, other values and duplicate fields. Plugin 0.1.3 ALWAYS sends the selected value on Start, never by inference from a prompt or display text. No context field is added to initialize, Next or Poll; unknown fields there remain rejected.

Only after accepting the explicit user's grant, the host applies the same runtime-only validation and setting path as `/context auto` or `/context off`, before inference. Rejected grants must not change context mode. Unsupported or invalid context setup fails closed visibly, not as retryable provider/model-selection failure. Existing context durability, retrieval, history, media, permission and safety gates remain unchanged.

Auto authorizes **local archival of eligible session content** for automatic context continuation. Archives exclude system/developer prompts, private reasoning, restricted/sensitive content and binary blocks. Eligible ordinary text/tool evidence is screened/redacted; redaction is not a universal secret detector or a promise to preserve every original byte. Off disables automatic context continuation, not existing archives or normal capacity/history validation. Start notices disclose the selected mode, local eligible archival and exclusions, runtime-only behavior after stop, and ongoing spend/cross-provider retained-history exposure.

The **setting remains in the current runtime after stop**, like explicit `/context`; stopping does not restore the previous mode. Current `/context` mode is runtime only, not saved to disk or restored from a saved session across restart. No global config, plugin preference, or memory recall/capture consent changes. Resume and restart never re-arm an automation grant. This patch covers **`/auto` foreground context only**: actual delegated worker context defaulting is out of scope because workers use a different durability consumer. It grants no subagent driver authority.

Update host and plugin together. A strict older host rejects unknown Start `context_mode`, even if it supports `feedback_version`; older hosts lacking `feedback_version` reject that field too. Compatibility is intentionally fail closed: never drop either field and retry or activate via printed output. Accepting legacy six-field polls does not imply old hosts accept this Start.

## Deadline refinement
Start optional `max_duration_ms` bounded positive <=365d. Host enforces its own monotonic deadline even while sleeping/inflight (cancel turn if deadline expires). Poll next delay must not schedule past deadline. Generic deadline is a user-proposed grant limit, not loop policy. Grant may be unbounded if omitted. Frontend timer cancels/revokes at deadline. Plugin owns turn count and includes it in notices. User can stop promptly while idle or streaming (Escape/Ctrl-C; `/auto stop` where supported). Draft editing and submitted normal-text steering neither revoke nor renew the grant or deadline.

## Draft, steering and attachment UX (current contract)
- Typing, text paste, input-history navigation and clearing/editing a draft NEVER revoke a run or submit the draft. Automatic proposals must preserve draft text and may progress while it is nonempty. Merely typing a slash command is not dispatching one.
- Submitted normal text steers the **same armed run** through its owned stream. If the stream is ending/closed or no stream is active (including a pending callback, delay or cooldown), keep accepted human submissions in a **bounded FIFO** for the next authorized proposal turn. Include each as a **separate history user message**, in order, not merged into the plugin prompt. Validate the complete latest proposed history before commit. Queue overflow rejects additional input visibly and retains it rather than overwriting earlier steering; queued text cannot authorize a new run or escape revocation/limits.
- Accepted steering clears the host's feedback comparison. It does not change the parent cancellation scope, extend the deadline, modify the exact model/effort allowlist, reset successful-turn/provider-retry counts, or itself count as a completed turn. The plugin continues to account only settled outcomes; `unknown` feedback resets its repeat streak as before.
- Submitted commands remain **control operations**, not steering. All commands revoke before dispatch, **including status**. Escape/Ctrl-C, stop and quit remain stopping operations, including idle/delayed cancellation. Model changes, session replacement, lifecycle loss and safety gates still stop. An expired/cancelled/invalid grant cannot be revived by steering.
- Staged attachments NEVER auto-send with a proposal or as steering. Their presence pauses the driver while idle without consuming input/attachments. Attempted submission while armed retains the input and tells the user to **press Escape before submitting attachments** through the normal user flow. Staging alone grants no media-send authority; all existing attachment/history validation remains.

Historical note: early first-release coordination proposed revocation on ordinary input/typing and considered a status exception. The final pre-steering adapter revoked on typing and all commands. Those input rules are superseded by the current draft/steering distinction above; the all-command revocation rule (including status) remains. Headless support and optional Start `selection` were early possibilities, not current contracts.

## Completed-turn feedback (retained from 0.1.1)
Plugin 0.1.3 retains Start-only integer `feedback_version:1`. Only opted-in grants add optional poll string `feedback` in `unknown|changed|repeated|empty`; legacy polls omit it. No output text, hashes or human steering bodies go to the plugin. Non-success feedback is `unknown` or omitted. The bounded host detector compares completed foreground output, not semantic progress or in-flight tool behavior. Accepted steering clears that comparison; normal gates remain authoritative.

Three consecutive `repeated`/`empty` successful turns on one favorite advance immediately to the next exact choice (1s delay, or 5min cooldown on wrap), resetting retries and the repeat streak. Successful-turn/duration limits take precedence. `changed`, `unknown`, missing feedback, failures, rejection, selection advance and fresh start/restart reset the streak. Success resets provider retries. Duplicate decision fingerprints include optional feedback presence and value; the latest identical retry must not double-count or resubmit. This is not semantic completion detection or permission to retry blocked work.

## Verification
Offline Python tests (serial): wire framing, explicit start only, context default auto/explicit off and Start-only fields/disclosures, mixed-order context/limit flags, literal new-prompt goal separator, rejected invalid/duplicate/missing flags, initial/continuation/recovery prompt precedence for latest human steering with unchanged authorization/safety, limits/deadline, favorites validation/persistence, exact order/efforts, account failover, repeated transient retries, all-down cooldown, unsupported selection, unknown blocked, duplicate poll/report id handling, process restart no resurrection, invalid commands, feedback thresholds/resets and prompt/wire bounds. These use synthetic host labels, not actual TUI delivery or inference. Foreground-owned Rust verification covers generic grant parsing/bounds, exact allowlist and permission/lifecycle, no frontend state changes on validation error, cancel/deadline/stale responses, true terminal boundary, draft preservation/non-submission, active steering and bounded FIFO delivery races, feedback reset, attachment pause/refusal and all-command revocation. Production checks and full workspace tests with <=8 workers belong to the foreground; plugin worker runs Python only. No paid API calls. Docs accurately state frontend coverage.

## Agreed first-release scope and Rust API (implementation coordination)
First release covers **local TUI only**. Headless/RPC/server/subagents remain unchanged and fail closed/no driver activation. Docs must state this. No natural-language limit guessing. Plugin command is `/auto`; does not activate loop at install. Esc must revoke even while idle. Draft edits preserve the grant without sending; submitted normal text steers it under the current UX contract above. All submitted commands, including `/auto status`, and model apply revoke the previous grant before dispatch. Start from plugin printed output never works.

Engine worker owns new `extensions/session_driver.rs`, export in extensions/mod.rs, permission/contract, manager narrow grant getter. Required public APIs:
- `Selection { pub model:String, pub effort:String }` serde Deserialize/Serialize Clone PartialEq Eq.
- Base `Reply` serde action tagged enum (with the 0.1.3 Start extension specified immediately below): `Start {run_id:String, models:Vec<Selection>, prompt:String, delay_ms:u64, max_duration_ms:Option<u64>, feedback_version:Option<u8>, notice:String}`, `Next {run_id:String, selection:Selection, prompt:String, delay_ms:u64, notice:String}`, `Stop {notice:String}`, `Status {notice:String}`. Optional notice defaults empty; start max_duration optional; feedback_version absent or integer 1. Extend only Start with optional strict `context_mode` (`"auto"` or `"off"`, omission unchanged); no Next/Poll extension. The host applies it after grant acceptance using the runtime-only `/context` path described above.
- `parse_reply(&serde_json::Value) -> Result<Option<Reply>,String>` wrapper `session_driver` absent returns None; strict deny unknown fields and type/bounds validated (reply total <=64KiB, prompt <=16KiB, notice<=2KiB, identifier<=128 ASCII alnum/-_, models<=16, exact qualified model contains slash, max model/effort lengths; efforts ReasoningLevel::parse exact names only; delay1000..300000).
- `Grant::from_start(plugin_id:&str, session_id:&str, reply:Reply) -> Result<(Grant, Proposal),String>`. Grant stores immutable models/run/session/owner, monotonic deadline from max_duration_ms; `pub plugin_id:String`, `pub session_id:String`, `pub run_id:String`; methods `expired()->bool`, `deadline()->Option<std::time::Instant>`, `accept(reply:Reply)->Result<Option<Proposal>,String>` (Next only with exact run+model+effort allowlist, Stop => None; no start/status accepted during poll). `Proposal { pub selection:Selection, pub prompt:String, pub delay:Duration, pub notice:String }`. Initial proposal models[0]. Host deadline max365days.
- `Outcome` serde snake case enum Success, ProviderError, SelectionRejected, Blocked; `PollRequest` serializes `run_id`, `decision_id`, `outcome`, `error_kind`, `model`, `effort`, plus optional `feedback` only for opted-in grants (frontend populates unique decision_id UUID). `classify_error(&str)->(Outcome,String)` CONSERVATIVE known auth/quota/rate-limit/transient provider errors only; local/ambiguous -> Blocked; no raw error transmitted.
- `async fn poll(handler:Arc<dyn ExtensionHandler>, request:PollRequest)->Result<Reply,String>` timeout5s, invokes `__session_driver__` via existing bounded invoke-event collector; ignores display output; parses mandatory session_driver; no manager lock. Can rely on existing retry transport only because plugin dedups decision_id and never resurrects run state after restart.
- `async fn prepare(runtime:&Runtime, proposal:&Proposal, history:&[SharedMessage])->Result<Runtime,PrepareError>` clones Runtime, validates canonical qualified model no alias downgrade, try_set_model plus checked exact reasoning set; complete proposed history validate before return. `PrepareError { Selection(String), Blocked(String) }`. Original never mutated, global config untouched. It validates existing+new user message. Candidate runtime preserves host session; caller commits runtime plus session mirrors only after success.
- `ExtensionManager::session_driver_handler(&self,id:&str)->Result<Arc<dyn ExtensionHandler>,String>` requires loaded eager permission SessionDrive from validated manifest. Retain minimal permission map on load and revoke on unload, no secrets. Deferred drivers unsupported first release, clear error. Every timer checks current returned Arc::ptr_eq versus grant handler to invalidate unload/reload. Never hold mgr guard during RPC.
TUI worker owns `tui/session_driver.rs`, app.rs/mod.rs/dispatch.rs/commands.rs/stream_handler.rs edits. Owns frontend async task, delayed proposal, pending outcome state, generation invalidation (dropping JoinHandle must abort via Drop). Poll exactly once after terminal Error/Done tracked before handler consumes event; Error may drop stream. Call observe_terminal after processing event, never count Done twice. Only success on actual final Done with no preceding error, cancelled token => revoke; unexpected stream EOF => blocked. Competing queued work/events, control commands, explicit compaction/reload/history replacement revoke; draft edits and accepted human steering do not. Driver-owned steering stays in a bounded FIFO and joins the next authorized proposal as separate history user messages after complete validation; pending attachments pause idle progress and require Escape before explicit attachment submission. Driver prompt may not route via slash command text; directly normal prepared user message path, with same context guard and pending media discipline. Reuse submit after prepare carefully to avoid self-revocation; direct helper for auto-submit is okay. Initial explicit command result consumed by commands helper (can store offer in App as `(plugin_id,Reply)`), then next timer arm validates permission and schedules. Any rejections visible. `/auto stop` while streaming should revoke/cancel via owner plugin command routing, no hardcoded plugin name in core; generic interactive command to current owner while streaming can cancel+invoke. User Esc idle must map to revoke even if existing input behavior only emits abort while streaming.
Foreman owns docs/tests outside these paths and integration checks. Workers may edit only assigned scopes, no cargo builds/tests. Plugin worker runs Python unittest serially only.

## Final review refinements (current contract)
- Draft edits do not revoke or send; submitted normal text steers the same armed run. All dispatched commands, including `/auto status`, still revoke. No status exception.
- Already-steered/display-only events in the owned in-flight turn are not new work;
  queued events are classified by the reactor before any next driver action.
  Idle/buffered work revokes. Successful same-session durable context rollover is
  part of the current turn, not a replacement authorization; failure/replacement stops.
- Revocation/deadline cancel the foreground and its reactive workers. A registry
  cancellation fence handles late registration and persists until user takeover.
  Automatic event wakes are inhibited after stop until an explicit user submission
  or newly armed run; no raw event label can bypass it. Collection gates remain.
- Handler pointer identity alone is insufficient. Pin a live lifecycle generation
  before command dispatch and check it on acceptance/ticks/stream setup; detected
  process exit, transport death or in-place restart invalidates pending proposals.
  This is a point-in-time observation, not an atomic process-exit/request lease.
- Provider waits honor cancellation and preserve partial terminal accounting/history.
  Durable checkpoint writes remain noninterruptible consistency barriers; cancellation
  cannot roll back already-completed tool side effects or provider-side billing.
- Exact requested favorites remain unchanged. Fable 5.1 has no current static row;
  `x-ai` is not the host's native `xai-auth` route. Unsupported entries skip visibly.
  Finite transient retries are three at 2/4/8 seconds; all-down cooldown is 5 minutes.


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


## Redundant-confirmation recovery (plugin 0.1.5)

Plugin-only policy change; no Rust production change, new permission, host grant
or feedback protocol revision. Every emitted prompt identifies automated origin
and denies new human approval. Clearly authorized remaining work does not need an
assistant-invented confirmation phrase; latest actual human steering and explicit
review checkpoints control. Unclear scope, new permissions, missing decisions,
credentials and real gates still require human input. Never manufacture consent,
replay completed side effects or infer approval from a model switch.

After successful-turn/remaining-delay limits, first and second consecutive
`repeated`/`empty` feedback labels emit fixed re-evaluation guidance on the same
favorite, with the existing 1-second delay and without resetting the streak. The
third retains existing failover/full-wrap cooldown. Changed/unknown/missing
feedback restores ordinary continuation. Duplicate decisions remain idempotent;
blocked outcomes cannot reach correction. No response prose, approval classifier,
new fields or fake short human answers are introduced.

This does not guarantee model compliance or semantic approval detection. A
prose-only approval request/completion remains a successful turn, not a structured
driver stop, and may incur continued charges. Human stop controls, explicit run
limits and independently enforced host gates remain necessary.
