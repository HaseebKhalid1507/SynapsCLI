# B — session_driver → SessionActor port map

> **Deliverable B** for scope-112 — feat/context-continuation onto daemon-mode dev.
> READ-ONLY RECON. No code changes, no cargo, no git writes.
> Conventions: **FACT** = read in code, **INFERENCE** = judgment, **UNKNOWN** = needs experiment.
> All file paths relative to repo root unless prefixed with `jr-112/` or `soak-fixes/`.

---

## Q1 — Responsibility inventory (TUI `session_driver.rs`)

Source: `jr-112/crates/agent-tui/src/tui/session_driver.rs` (1811 lines).

| # | Item (pub(crate) unless noted) | Lines | Reads from App | Writes to App | Calls on Runtime / ExtensionManager | Timers / Tasks | StreamEvents observed | Summary |
|---|-------------------------------|-------|---------------|--------------|--------------------------------------|----------------|----------------------|---------|
| 1 | `struct SessionDriver` (Default) | 99–111 | — | — | — | — | — | State container: `generation`, `active: Option<Active>`, `pending: Option<Pending>`, `interrupted_owner`, `auto_wakes_blocked`. |
| 2 | `is_active()` | 114–116 | `active`, `pending` | — | — | — | — | True iff armed or waiting on a spawned task. Controls the TUI tick timer gate (`mod.rs:262`). |
| 3 | `owner()` | 118–123 | `active.grant.plugin_id`, `interrupted_owner` | — | — | — | — | Plugin-id of the armed grant or the most recent revoked grant, for stop/status command routing. |
| 4 | `invalidate()` (private) | 125–135 | `active` | sets `auto_wakes_blocked=true`, `interrupted_owner`, bumps `generation`, drops `active`+`pending` | — | Aborts pending task (via Drop of `Task<T>`) | — | Core revocation primitive. Dropping `Active` cancels its CancellationToken + workers. |
| 5 | `spawn()` (private) | 137–148 | — | sets `pending` | — | `tokio::spawn` → `Task<T>` | — | Fire one background task (command invoke, poll, prepare, start_stream). Only one pending at a time. |
| 6 | **`revoke(app, reason)`** | 151–177 | `active.steering` (drains), `input_text()` | Restores undelivered steering to input draft; pushes `ChatMessage::System("Session driver stopped: …")` | — | — | — | The single revocation entry point for all callers. Calls `invalidate()`. |
| 7 | **`submit_steering(app, input, tx)`** | 183–241 | `active.cancel`, `grant.expired()`, `grant.session_id`, `context_head`, `pending_attachments`, `steering` queue, `streaming` | Pushes steering to `active.steering` deque; resets `feedback` tracker; pushes System chat message; clears `input_before_paste`/`pasted_char_count` | — | — | — | Handles user text submission while armed. Bounded FIFO (16 msgs, 256 KiB). Sends to `steer_tx` if streaming. |
| 8 | `restore_submission()` (private) | 243–250 | `input_text()` | `set_input_text()` | — | — | — | Put rejected text back into the input draft. |
| 9 | **`steering_delivered(app, message)`** | 252–269 | `active.steering.front()` | Pops front of steering deque; appends provisional user message to `api_messages` | — | — | `StreamEvent::Agent(SteeringDelivered)` | Acknowledgement that the engine injected a steered message. |
| 10 | `history_with_steering()` (private) | 271–282 | `api_messages`, `active.steering` | — | — | — | — | Builds proposed history = acknowledged messages + pending steering queue. Used by prepare & commit. |
| 11 | `commit_submission()` (private) | 286–302 | `active.steering` (drains), `abort_context` | Drains steering into `api_messages` + ChatMessage::User cards; appends plugin prompt (+ abort context) | — | — | — | Atomic commit of an autonomous turn's user messages just before stream start. |
| 12 | **`auto_wakes_allowed(app)`** | 304–311 | `auto_wakes_blocked`, `active.cancel`, `active.grant.expired()` | — | — | — | — | Gate: `stream_handler.rs:240,371,550` use this to suppress auto-turns and event-triggered work after revocation. |
| 13 | **`user_takeover(app, runtime)`** | 314–322 | — | Clears `auto_wakes_blocked`, `interrupted_owner` | `runtime.subagent_registry().set_spawn_cancellation(None)` | — | — | User's explicit new submission re-enables automatic wakes. |
| 14 | **`observe_events(app, dispositions)`** | 326–343 | `streaming`, `active.awaiting_terminal` | Revokes if any disposition is not Steered/DisplayOnly | — | — | — | Reactor event classification gate. In-flight steered/display events are safe; buffered/injected/idle events revoke. |
| 15 | **`observe_checkpoint(app, session_id, succeeded)`** | 347–357 | `active.grant.session_id`, `app.session.id` | Revokes on failure or session mismatch | — | — | — | Context-head checkpoint continuation/revocation. |
| 16 | `notice()` (private) | 359–365 | — | `push_msg(ChatMessage::System(sanitized))` | — | — | — | Display a plugin notice. |
| 17 | **`start_command(app, manager, owner, command, arg)`** | 369–433 | `session.id` | Revokes existing; spawns command invoke task | `manager.try_read()` → `user_action_handler(owner)`, `session_driver_handler(owner)` (timeout selection) | `tokio::spawn` (invoke_command) | — | Entry point from dispatch.rs for any slash command from a driver-permissioned plugin. |
| 18 | `idle_conflict()` (private) | 437–463 | `queued_message`, `pending_events`, `compact_task`, `modal_stack.top()`, `secret_prompts`, `extension_loader_running`, `gamba_child`, `context_head` | — | `runtime.event_queue()`, `runtime.orchestration()` | — | — | Pre-flight: returns `Some(reason)` if the session is not idle enough for an autonomous turn. |
| 19 | `completion_blocked()` (private) | 465–480 | — | — | `runtime.orchestration().completion_gate()`, `runtime.subagent_registry().list_active()` | — | — | Checks for outstanding workers needing collection. |
| 20 | `live_generation()` (private) | 482–493 | — | — | `handler.lifecycle_snapshot()` | — | — | Reads the extension's process generation; rejects dead/unknown. |
| 21 | `same_lifecycle()` (private) | 495–500 | — | — | `handler.lifecycle_snapshot()` | — | — | Generation must match the pinned generation. |
| 22 | `same_handler()` (private) | 503–518 | — | — | `manager.try_read().session_driver_handler()`, `Arc::ptr_eq`, `same_lifecycle` | — | — | Validates the handler is the same object with the same generation. |
| 23 | `schedule()` (private) | 520–536 | — | `active.selection`, `active.proposal` | — | — | — | Set the next proposal + due time; checks delay vs deadline. |
| 24 | `arm()` (private) | 538–602 | `session.id` | Sets `active`, pushes auth notice, clears `interrupted_owner`/`auto_wakes_blocked` | `runtime.subagent_registry()` (set_spawn_cancellation), `Grant::apply_context_mode(runtime)` | `tokio::spawn` (deadline timer task) | — | Creates the `Active` struct from a validated Start reply. Applies context_mode. |
| 25 | **`observe_feedback(app, event)`** | 606–612 | `active.awaiting_terminal`, `grant.feedback_enabled()` | `active.feedback.observe(event)` | — | — | `StreamEvent::Llm(*)` | Feed LLM output into the feedback fingerprint tracker. |
| 26 | `enum Terminal` | 616–620 | — | — | — | — | — | Success / Failure(Outcome, kind) / Canceled. |
| 27 | **`capture_terminal(event, canceled)`** | 622–638 | — | — | — | — | `SessionEvent::Done`, `SessionEvent::Error`, EOF | Classify the terminal stream event into a Terminal variant. |
| 28 | **`observe_terminal(app, runtime, terminal)`** | 640–690 | `active.awaiting_terminal` | Sets `active.outcome`, revokes on Blocked/Canceled, captures feedback | `runtime.turn_budget().max_elapsed` (zero-budget guard), `runtime.event_queue()` | — | — | Post-turn settlement: exactly-once transition from awaiting_terminal to outcome or revocation. |
| 29 | **`tick(app, runtime, manager, secret_prompt, stream, cancel_token, steer_tx)`** | 695–1027 | Many (full turn machine) | Many (streaming state, api_messages, session model/thinking) | `runtime.run_stream_with_messages(...)`, `runtime.clone()`, `protocol::prepare()`, `protocol::poll()` | Spawns: Poll, Prepared, Started tasks | Indirectly via task completion | **THE MAIN DRIVER LOOP.** Runs every ~200ms while `is_active()`. Validates lifecycle, processes TaskResults (Command/Poll/Prepared/Started), schedules proposals, prepares submissions, starts streams. |
| 30 | `poll_request()` (private) | 1031–1048 | `active.grant.run_id`, `active.selection`, `active.completed_feedback` | — | — | — | — | Build the PollRequest struct for the `__session_driver__` callback. |
| 31 | `struct Active` | 51–74 | — | — | — | `_deadline_task: Option<Task<()>>` | — | Armed grant state. On Drop: cancels CancellationToken + workers. |
| 32 | `struct Pending` | 76–81 | — | — | — | `task: Task<TaskResult>` | — | One in-flight spawned task. |
| 33 | `struct Scheduled` | 46–49 | — | — | — | — | — | Proposal + due instant. |
| 34 | `enum TaskResult` | 83–97 | — | — | — | — | — | Command / Poll / Prepared / Started discriminant. |
| 35 | `struct Task<T>` | 39–44 | — | — | — | Abort-on-drop wrapper around JoinHandle | — | Ensures task is aborted on drop, not just detached. |
| 36 | `mod feedback` (feedback.rs, 345 ln) | — | — | — | — | — | `LlmEvent::Text`, `LlmEvent::ToolUse`, `LlmEvent::ToolResult`, `ResponseStart/Reset` | Bounded hash-fingerprint tracker. Reports changed/repeated/empty/unknown. |
| 37 | `mod steering_tests` (1040 ln) | — | — | — | — | — | — | Unit tests for steering queue, revocation, event observation, lifecycle. |

### Call-site summary (how the TUI integrates the driver)

| Call site | File:Line | What it does |
|-----------|-----------|-------------|
| `input.rs:88-93` | Esc / Ctrl-C while `is_active()` → `InputAction::Abort` | Hard stop; cancellation outranks all plugin keybinds |
| `mod.rs:262-273` | tokio::select `driver_timer.tick()` → `session_driver::tick(…)` | The tick loop; guarded by `is_active() \|\| streaming` |
| `mod.rs:348` | `session_driver::revoke(&mut app, "TUI shutting down")` | Cleanup on exit |
| `dispatch.rs:127,141` | All commands: `revoke(app, "explicit user action")` | Any slash command revokes |
| `dispatch.rs:465` | After normal user submit: `user_takeover(app, runtime)` | Re-enables auto wakes |
| `dispatch.rs:523-525` | Plugin command with `session.drive` permission → `start_command(…)` | Arms the driver |
| `dispatch.rs:1223-1227` | Plain text submit while armed → `submit_steering(…)` | Steering path |
| `dispatch.rs:1266` | User submit (non-armed) → `user_takeover(…)` | Clears blocked wakes |
| `dispatch.rs:1315-1319` | Streaming input (armed) → `submit_steering(…)` or revoke | Streaming steering |
| `dispatch.rs:1335` | Plugin command during streaming → `start_command(…)` | Plugin slash from streaming |
| `stream_handler.rs:94` | Context-head checkpoint → `observe_checkpoint(…)` | Checkpoint continuation |
| `stream_handler.rs:160` | `SteeringDelivered` → `steering_delivered(…)` | Ack steering delivery |
| `stream_handler.rs:240,371,550` | `auto_wakes_allowed(app)` guards | Auto-turn/event-wake gates |
| `stream_handler.rs:319` | Post-event-drain → `observe_events(…)` | Reactor classification |
| `stream_handler.rs:447-459` | Pre-terminal: `observe_feedback(…)`, `capture_terminal(…)`, `observe_terminal(…)` | Turn settlement |
| `stream_handler.rs:490` | After compaction Done → `user_takeover(…)` | Post-compaction |
| `stream_handler.rs:600` | EOF / unexpected end → `observe_terminal(…)` | Stream close |
| `app.rs:44,288` | `session_driver: SessionDriver` field + Default init | Struct member |
| `app.rs:575` | `session_driver::revoke(self, "session replaced")` in `apply_new_session()` | Session swap |

---

## Q2 — Command / Event mapping to actor-native design

### Dev's existing SessionCommand enum (`soak-fixes/…/session/types.rs:383-447`)

| Existing SessionCommand | Payload | Applicable? |
|------------------------|---------|-------------|
| `Submit { text, attachments }` | User text + media | ✅ Driver's `commit_submission` → Submit (text only, no attachments) |
| `Steer { text }` | Steering text | ✅ Driver's `submit_steering` → Steer (in-flight delivery) |
| `Cancel` | — | ✅ Esc/Ctrl-C → Cancel (revokes grant as side-effect) |
| `Set { id, setting }` | SessionSetting | Partially: model/effort changes during prepare use `try_set_model`/`set_reasoning_level_checked` — not the Set command path |
| `PluginCommand { id, plugin, name, arg }` | Plugin invocation | ✅ The `start_command` → interactive invoke could become PluginCommand but needs the `session_driver` reply parsing |
| `EngineCommand { id, name, arg }` | Engine slash | ❌ Driver doesn't use engine commands |
| `Checkpoint { reason }` | Reload reason | ❌ But reload → revoke interplay matters |
| `End { reason }` | Quit | ❌ But quit → revoke matters |
| All others | — | Not directly relevant |

### Dev's existing SessionEventWire enum (`soak-fixes/…/session/types.rs:634-721`)

| Existing Event | Applicable? |
|---------------|-------------|
| `Stream(StreamEvent)` | ✅ Actor already forwards all stream events; driver needs to observe them for feedback/terminal |
| `TurnStarted { turn_baseline, trigger, user_text }` | ✅ Driver turns use `TurnTrigger::PluginCommand` (new variant needed: `DriverAuto`) |
| `Conversation(snapshot)` | ✅ Steering commits change api_messages → Conversation |
| `Steered { text, delivered }` | ✅ Already exists; driver steering maps directly |
| `Idle` | ✅ Turn machine idle; driver can start its tick/poll cycle |
| `SystemNotice(String)` | ✅ Driver notices, auth messages, revocation messages |
| `Aborted { context_saved }` | ✅ Cancel path |
| `SubagentRows(…)` | ✅ Worker state for completion_blocked check |
| `SettingChanged(…)` | ✅ Model/effort change from driver prepare |
| `Lifecycle(…)` | ⚠️ Park/reload → driver revocation |
| `Reloading { generation, retry_after_ms }` | ⚠️ Reload → driver revocation |

### NEW commands needed

| New Command | Payload | Rationale |
|-------------|---------|-----------|
| `DriverStart { plugin: String, command: String, arg: String }` | Plugin id + interactive command args | Replaces `start_command()`. The actor runs the interactive invoke, parses the `session_driver` reply, creates the Grant, arms itself. The client simply requests it; the actor owns the lifecycle. FACT: today the TUI does `start_command` → spawns task → `arm()` all within the TUI process (`session_driver.rs:369-602`). |
| `DriverSteer { text: String }` | Human steering text | Could reuse `Steer`, but semantics differ: driver steering validates grant (expiry, session match, attachment check, queue bounds) before accepting. **INFERENCE**: better to use existing `Steer` and have the actor's driver state gate it — avoids a new command. The actor can differentiate based on whether `driver_active`. |
| `DriverStop` | — | Explicit revoke from client. Could also be a `Cancel` while armed; the actor decides. **INFERENCE**: `Cancel` already handles this — when the driver is armed, Cancel = revoke + cancel stream. No new command needed. |

### NEW events needed

| New Event | Payload | Rationale |
|-----------|---------|-----------|
| `DriverArmed { plugin_id, run_id, selection, notice, deadline_ms }` | Grant metadata | Clients need to know the driver is armed so they can render the status bar (model, plugin name, remaining time) and route Esc/Ctrl-C as driver-stop. FACT: today the TUI's `arm()` pushes local ChatMessage::System; this becomes a wire event. |
| `DriverNotice { text: String }` | Plugin notice | Covers all driver-specific notices (proposals, warnings, stop reasons). Could fold into `SystemNotice` but keeping them separate lets clients distinguish driver UI. **INFERENCE**: `SystemNotice` is sufficient if tagged with a prefix; this is a design decision for Haseeb. |
| `DriverRevoked { reason: String, steering_restored: Vec<String> }` | Revocation + undelivered steering | Client must restore steering to draft, update status bar. FACT: today `revoke()` writes directly to `App.input_text` and pushes a system message (`session_driver.rs:151-177`). |
| `DriverSteered { text: String, queued: bool }` | Steering accepted by driver | Distinct from the existing `Steered` (which is turn-level); this confirms the driver's bounded FIFO accepted it. **INFERENCE**: can be folded into existing `Steered` with a `driver: bool` flag — design decision. |
| `DriverTurnOutcome { outcome: Outcome, model: String, effort: String }` | After terminal | Optional: lets the client show "provider_error: auth → switching model". **INFERENCE**: `SystemNotice` may suffice. |

### Mapping: every TUI responsibility → actor home

| # (from Q1) | Responsibility | Home after port | Mechanism | Notes |
|-------------|---------------|----------------|-----------|-------|
| 1-5,31-35 | Driver state machine (`SessionDriver`, `Active`, `Pending`, `Scheduled`, `Task`, `invalidate`, `spawn`) | **SessionActor** field: `driver: Option<DriverState>` | Actor-local; no wire representation | The actor owns the grant, pending tasks, timers. `DriverState` ≈ `Active` minus `App` couplings. |
| 6 | `revoke()` | **SessionActor** method | Emits `DriverRevoked` event; drains steering queue; bumps generation | No direct App access; client renders the event |
| 7 | `submit_steering()` | **SessionActor** steer handler (guarded by `driver.is_some()`) | Client sends `Steer { text }` → actor validates grant → inserts into driver.steering FIFO → emits `Steered` | Bounds checking (16 msgs, 256 KiB) stays in actor |
| 8 | `restore_submission()` | **Client-side** (TUI renders `DriverRevoked.steering_restored`) | Client receives revoked steering, sets input draft | Pure presentation |
| 9 | `steering_delivered()` | **SessionActor** `on_stream_event(SteeringDelivered)` | Actor pops from driver.steering and appends provisional message to api_messages | Already an actor stream-event handler; extend it |
| 10 | `history_with_steering()` | **SessionActor** method | Actor has direct access to both api_messages and driver.steering | Trivial |
| 11 | `commit_submission()` | **SessionActor** method called before `start_turn()` | Actor drains steering + appends prompt | No client involvement |
| 12 | `auto_wakes_allowed()` | **SessionActor** internal guard | Actor checks `driver.auto_wakes_blocked` in its auto-turn path | Already the actor's event-drain / auto-turn code |
| 13 | `user_takeover()` | **SessionActor** method on `Submit` | Actor clears `auto_wakes_blocked`, `interrupted_owner`, spawn cancellation | Already called in `submit()` path |
| 14 | `observe_events()` | **SessionActor** event-drain path | Actor checks dispositions after `drain_event_queue()`; revokes if non-steered | `actor.rs:1251-1310` event drain already exists; extend with driver check |
| 15 | `observe_checkpoint()` | **SessionActor** checkpoint handler | Actor already handles context-head checkpoints; add driver validation | Extend existing path |
| 16 | `notice()` | **SessionActor** → `SystemNotice` event | Actor emits; client renders | Trivial |
| 17 | `start_command()` | **SessionActor** handler for `DriverStart` command | Actor: `manager.user_action_handler(owner)` → spawn invoke task → parse reply → `arm()` → emit `DriverArmed`. FACT: the ExtensionManager is already `Arc<RwLock<_>>` shared across sessions (`soak-fixes/…/session/actor.rs:325 host: Arc<EngineHost>`). |
| 18 | `idle_conflict()` | **SessionActor** method | Actor has direct access to `conv.queued_message`, `conv.pending_events`, `compact`, event_queue, orchestration | Most checks are already in actor; `modal_stack`, `gamba_child`, `secret_prompts` don't exist in the actor (those are client-side UI state). **INFERENCE**: actor-side idle checks are a subset; client-modal gates become "is_busy" in the actor (streaming, compacting, pending prompts). |
| 19 | `completion_blocked()` | **SessionActor** method | Actor has `runtime.orchestration()` and `runtime.subagent_registry()` | Trivial |
| 20-22 | `live_generation()`, `same_lifecycle()`, `same_handler()` | **SessionActor** — handler/generation pinning | Actor pins the handler Arc + generation on arm; validates at every tick + before accepting results | Identical logic, different location |
| 23 | `schedule()` | **SessionActor** internal | Actor sets `driver.proposal = Some(Scheduled { proposal, due })` | Trivial |
| 24 | `arm()` | **SessionActor** internal | Actor creates DriverState, applies context_mode on runtime directly, spawns deadline timer, emits `DriverArmed` | No need for client involvement in the arm decision |
| 25 | `observe_feedback()` | **SessionActor** `on_stream_event()` | Actor feeds LLM events into `driver.feedback.observe()` | Extend existing stream event handler |
| 26-28 | `Terminal`, `capture_terminal()`, `observe_terminal()` | **SessionActor** turn-end path | Actor classifies terminal in its Done/Error handler; sets `driver.outcome` or revokes | Extend existing `SessionEvent::Done`/`Error` handling |
| 29 | **`tick()`** — the main loop | **SessionActor** — new `driver_tick()` method called from the main `run()` select loop | Actor runs the tick logic: validate lifecycle → check conflicts → process pending tasks → schedule proposals → prepare → start_turn. No `App` involvement. FACT: the actor already has a `tokio::select!` loop with idle arms (`actor.rs:2212+`). Add a `driver_timer.tick()` arm. |
| 30 | `poll_request()` | **SessionActor** internal | Actor builds PollRequest from driver state | Trivial |
| 36 | Feedback tracker | **SessionActor** `driver.feedback: feedback::Tracker` | Module moves to engine crate | feedback.rs has no TUI dependencies (uses only `StreamEvent`, `LlmEvent` from synaps_cli); can be lifted verbatim. |

---

## Q3 — Semantics that CHANGE when actor-resident

### 3.1 Grant survives client detach

**FACT**: Under dev, `Detach` never touches `stream`/`cancel` (`actor.rs:15-16`). A detached session keeps streaming.
**CHANGE**: An armed driver grant + active autonomous stream continue headless when the client disconnects.
**Feature**: Lid-close / terminal-loss doesn't kill autonomous work.
**Risk**: F27 ("quit-while-streaming is silent") — turn keeps running but user doesn't know. With a *driver* running headless, it incurs ongoing charges indefinitely until the deadline or turn limit.
**Decision**:
- (a) On last-client-detach while driver armed: emit `SystemNotice` to any remaining mirrors; set a **detach grace timer** (e.g. 5 minutes) after which revoke if no client reattaches. OR
- (b) Revoke on last-client-detach (safe-default, lose the "lid-close" feature for now). OR
- (c) Keep running (full autonomous); `synaps daemon sessions` shows armed status. Require `--keep-warm` if detach grace is wanted.
**INFERENCE**: (c) is the correct design for headless/daemon-first. But must fix F27 first: client quit must print "driver still running in daemon — use `synaps attach` to reconnect or `synaps send --stop` to stop".

### 3.2 Escape/Ctrl-C: stop vs detach

**FACT**: In upstream's TUI, Esc/Ctrl-C while `is_active()` → `InputAction::Abort` → revoke grant + cancel stream (`input.rs:88-93`).
**FACT**: In the thin client, Ctrl-C = detach from daemon session. Turn keeps running.
**CHANGE**: Esc/Ctrl-C in the thin client with an armed driver must send `Cancel` to the actor (which revokes + cancels), NOT just detach.
**Decision**: The thin client must detect "driver armed" (from `DriverArmed` state tracking) and route Esc as `SessionCommand::Cancel` rather than local disconnect. Already precedented: the TUI sends Cancel for Esc-while-streaming.
**INFERENCE**: Straightforward. The thin client already sends `Cancel` for Esc-while-streaming.

### 3.3 Two clients, one session — who owns the grant?

**FACT**: Dev has input ownership: `input_owner: Option<ClientId>` (`types.rs:357`). Only the input owner can send `Submit`/`Cancel`/`Set`/etc. (`types.rs:269-288`).
**FACT**: The driver grant is session-scoped, not client-scoped. The `Active.grant` has `session_id` and `plugin_id`, not a `client_id`.
**CHANGE**: Grant belongs to the session (actor), not a client. The input owner controls it (can Cancel/revoke). A mirror/observer client sees `DriverArmed`/`DriverRevoked` events and driver `SystemNotice`s but cannot Cancel or steer.
**Decision**: `DriverStart` is an input command (only the owner can send it). `Cancel` while armed is already input-only. Steering (`Steer`) is already input-only. **Mirror clients see driver notices as read-only.** `InputOwnerChanged` with takeover: the new owner inherits driver-stop authority.
**INFERENCE**: No new ownership model needed; existing B1 ownership handles it.

### 3.4 Park while armed

**FACT**: Sessions park after 60s with no clients (`park_grace()`, `actor.rs:199-212`). Park drops `runtime` and `conv` (`Live.park_take()`).
**CHANGE**: Parking while the driver is armed would drop the runtime mid-autonomous-run.
**Decision**: **Never park while `driver.is_some()`**. Add `can_park()` check:
```rust
fn can_park(&self) -> bool {
    !self.streaming && self.driver.is_none() && /* existing checks */
}
```
**FACT**: `keep_warm` already exists (`SessionConfig.keep_warm`, `types.rs:94`). An armed driver implies keep_warm semantics automatically.
**INFERENCE**: Minimal — add the guard. On revoke (driver dropped), the park timer can arm normally.

### 3.5 Daemon reload while armed

**FACT**: upstream's spec: "Process restart must never restore an active run automatically" (`session-drivers.md:208`).
**FACT**: upstream's `Active.grant` and run state are in process memory only; no disk persistence. Plugin's `Run` state is also process-memory-only (`main.py:301`).
**FACT**: Daemon reload (`Checkpoint { reason: Reload }`) → cancel turn, abort compaction, save, close PTYs → exec self.
**CHANGE**: On `Checkpoint{Reload}`, the actor must **revoke the driver** before saving. The new process loads the saved session with no driver state. Plugin process is restarted by the extension manager; its `initialize()` explicitly says "initialization/restart never resumes a run" (`main.py:339`).
**FACT**: upstream's identity checks: `handler_generation`, `same_lifecycle()`, `same_handler()` all verify the exact Arc pointer + generation. After reload, the extension manager creates a new handler with a new generation → any stale check fails.
**Decision**: Add `revoke("daemon reload")` to the `Checkpoint` command handler. upstream's identity checks already protect against a stale handler surviving — but explicit revocation is cleaner and matches the spec.
**INFERENCE**: The `generation` checks DO hold for N sessions because each session pins its own `handler: Arc<dyn ExtensionHandler>` + `handler_generation: u64`. Even with a shared ExtensionManager, each session's driver independently validates its pinned handler. Reload creates new handlers → all sessions' grants invalidate on the next tick.

### 3.6 `synaps send` while armed

**FACT**: `synaps send` creates a session or attaches, sends `Submit`, waits for `Idle`, detaches.
**CHANGE**: If a driver is armed when `synaps send` arrives, the `Submit` would normally steer (since it's text during streaming). But if the driver is between turns (idle, waiting for delay), the `Submit` would be a normal submit — which calls `user_takeover()` and clears `auto_wakes_blocked`.
**Decision**: Two options:
- (a) `Submit` while driver is armed + idle → treat as steering (queue into FIFO). This requires the actor's submit path to check `driver.is_some()`.
- (b) `Submit` while driver armed → revoke driver, process as normal user submit.
**INFERENCE**: (a) matches upstream's spec ("Submitting ordinary text while armed steers the same run" `session-drivers.md:72`). The actor's submit handler should check `driver.is_some()` and route to driver steering if armed. If not streaming, queue for next autonomous turn.

### 3.7 Headless `synaps chat`, RPC, server gain the driver

**FACT**: upstream's spec says "local TUI sessions only" (`session-drivers.md:10`).
**CHANGE**: Once the driver lives in the actor, ALL session types get it for free: headless chat, daemon attach, `synaps send`, RPC.
**Decision**: **Enable it.** The spec's "local TUI sessions only" was an implementation constraint, not a security boundary. The driver's safety comes from: (1) explicit user command invocation, (2) permission gating, (3) fail-closed identity checks, (4) Grant lifetime model. All of these work in the actor. The only change: update `session-drivers.md` to remove the "local TUI sessions only" clause.
**INFERENCE**: No reason NOT to enable it. The autonomous plugin already has no TUI-specific code — it's pure RPC. Headless chat could use `/auto start -- <goal>` directly.

---

## Q4 — upstream's fail-closed invariants: enforcement after port

Source: `jr-112/docs/extensions/session-drivers.md` "Host constraints" (lines 211-237) + tests in `jr-112/crates/agent-tui/src/tui/session_driver.rs` (lines 1050-1811).

| Constraint (from spec) | Enforced by (upstream) | Enforced by (after port) | Test disposition |
|------------------------|------------------|--------------------------|-----------------|
| **One outstanding callback/proposal; 5s timeout** | `spawn()` has `debug_assert!(pending.is_none())` (line 142); `POLL_TIMEOUT=5s` (engine `session_driver.rs:33`) | Actor's `driver_tick()` serializes tasks identically; poll timeout unchanged (engine crate unchanged) | Engine tests stay verbatim; TUI spawn tests → actor integration tests |
| **Replies bounded 64 KiB, prompts 16 KiB, notices 2 KiB, models ≤16** | `validate_reply()` (engine `session_driver.rs:169-222`) | **Unchanged** — engine crate's validation is host-agnostic | Engine tests stay verbatim (`parser_rejects_unknown_fields_types_and_noncanonical_efforts`, `parser_enforces_all_byte_count_and_time_bounds`) |
| **Checked model/effort mutation + history validation before commit** | `prepare()` (engine `session_driver.rs:775-856`), `validate_prepared()` (line 860-873) | Actor calls same `prepare()`/`validate_prepared()` — it has `&mut Runtime` directly | Engine tests stay verbatim; TUI `tick()` Prepared handling → actor test |
| **Identity/generation checks at dispatch, acceptance, tick, before stream** | `same_handler()`, `same_lifecycle()`, `live_generation()` (lines 482-518) | Actor pins `handler: Arc<dyn ExtensionHandler>` + `generation: u64` on arm; validates at every `driver_tick()` + before accepting task results | `same_handler_pointer_does_not_preserve_authority_after_restart_or_death` (line 1155) → actor test with mock handler. Differential harness suitable. |
| **Esc/Ctrl-C stop automation, including while waiting** | `input.rs:88-93` routes to `InputAction::Abort` → `revoke()` | TUI sends `Cancel` → actor revokes driver | `idle_escape_and_ctrl_c_outrank_registered_bindings` (line 1555) → TUI-only presentation test (verifies InputAction; stays in TUI crate) |
| **All dispatched commands revoke the grant** | `dispatch.rs:141` calls `revoke(app, "explicit user action")` | Actor's command dispatch: any non-driver command while armed → revoke first | New actor test: "command while armed revokes" |
| **Events steered into owned turn don't revoke; idle/buffered events do** | `observe_events()` (lines 326-343) | Actor's event-drain path: check dispositions, revoke on non-steered | `owned_turn_steering_is_not_competing_work_but_buffered_or_idle_is` (line 1167) → actor differential test |
| **Same-session checkpoint retains grant; failure revokes** | `observe_checkpoint()` (lines 347-357) | Actor checkpoint handler | `same_session_checkpoint_receipt_retains_grant_failure_revokes` (line 1208) → actor test |
| **Revocation cancels workers and blocks wakes** | `revoke()` → `invalidate()` → Drop `Active` → cancels workers | Actor's revoke drops `DriverState` → identical Drop semantics | `revocation_cancels_workers_and_event_wakes_until_explicit_takeover` (line 1225) → actor test |
| **Terminal Done/Error/EOF observed once** | `awaiting_terminal` flag (lines 648-650) | Actor's `on_stream_event(Done/Error)` sets driver.outcome once | `only_final_done_is_success_not_tools_not_notices_not_eof` (line 1255) → actor test |
| **Time checkpoint needs opt-in and zero budget guard** | `observe_terminal()` (lines 665-676) | Actor checks `grant.time_checkpoints_enabled()` + `runtime.turn_budget().max_elapsed` | `time_checkpoint_needs_opt_in_and_is_observed_once_not_success` (line 1281) → actor test |
| **Typed errors never fail over** | `classify_turn_error()` (engine crate) | **Unchanged** — engine crate | Engine tests stay verbatim |
| **Drafts don't revoke; idle gates don't count them** | `idle_conflict()` doesn't check input text | Actor has no draft concept → automatically satisfied | `ordinary_gates_prevent_idle_poll_but_drafts_do_not_revoke` (line 1519) → not applicable to actor (actor never sees drafts) |
| **Dropping pending task aborts and invalidates** | `invalidate()` → `Task::Drop` → abort | Actor uses same pattern | `dropping_pending_task_aborts_and_invalidates_generation` (line 1536) → actor test |
| **Prompts are user messages, not slash commands** | `commit_submission()` appends as `{"role":"user","content":...}` | Actor's submit path identical | `slash_prefixed_driver_prompt_is_ordinary_user_content` (line 1798) → actor test |
| **Full timer integration (draft, steering, selection, poll)** | `timer_keeps_drafts_and_steering_across_pending_selection_and_poll` (line 1588) | Actor differential test: arm → steer → prepare (SelectionRejected) → poll → steer → revoke → check steering restored | **Differential harness** — this is the most comprehensive test. |

### Test migration summary

| Category | Count | Disposition |
|----------|-------|-------------|
| Engine crate tests (parse, validate, classify, grant) | ~10 tests in engine `session_driver.rs:923-1400` | **Verbatim** — no changes needed, they test the protocol crate |
| TUI lifecycle tests (revoke, observe, checkpoint, terminal) | ~12 tests in TUI `session_driver.rs:1050-1516` | **Port to actor tests** — replace `App` with `SessionActor`; use `SessionHandle` to send commands and capture events |
| TUI presentation tests (Esc/Ctrl-C routing, input restoration) | 2 tests (`idle_escape_and_ctrl_c`, part of `timer_keeps_drafts`) | **Stay in TUI** — they test `input.rs` routing and `App` state |
| TUI integration test (timer with real extension lifecycle) | 1 test (`timer_keeps_drafts_and_steering_across_pending_selection_and_poll`) | **Differential harness** (`tests/session_actor_differential.rs` pattern) |
| Feedback tracker tests | ~15 tests in `feedback.rs:346-885` | **Move with module** — feedback.rs moves to engine crate |
| Steering tests | 1040 lines in `steering_tests.rs` | **TBD** — need to read; likely split between actor (queue semantics) and TUI (input routing) |

---

## Q5 — `__session_driver__` callback session scoping

**FACT**: The `__session_driver__` callback runs via `ExtensionManager::invoke_command()` on the handler (`engine session_driver.rs:692-754`, called from TUI `session_driver.rs:991-995`). The handler is an `Arc<dyn ExtensionHandler>` — it is **extension-scoped, not session-scoped**. The same handler object is shared by all sessions that load the same extension.

**FACT**: Session scoping comes from:
1. The `run_id` in the `PollRequest` (`engine session_driver.rs:429-439`) — the plugin validates `run_id` matches its in-memory `Run.run_id` (`main.py:449`).
2. The `decision_id` — unique per decision, deduped by the plugin (`main.py:472-481`).
3. The `handler_generation` pinned at arm time — validated at every tick and before accepting results (`TUI session_driver.rs:722-728`).

**FACT**: Identity checks upstream does:
- `live_generation(handler)` — reads `handler.lifecycle_snapshot().generation` (line 482-493)
- `same_lifecycle(handler, generation)` — generation must match (line 495-500)
- `same_handler(manager, owner, handler, generation)` — Arc::ptr_eq + same_lifecycle (line 503-518)
- Session id match: `active.grant.session_id != app.session.id` (line 714)
- Grant expiry: `active.grant.expired()` (line 716)
- Cancel token: `active.cancel.is_cancelled()` (line 718)

**ANALYSIS for N sessions each with a grant**: The `ExtensionManager` is process-wide (behind `Arc<RwLock<_>>`). Each session pins its own handler Arc + generation. If two sessions arm grants from the same plugin:
- Each gets the same `Arc<dyn ExtensionHandler>` (same pointer)
- Each gets the same `generation` (same lifecycle)
- Each has a different `run_id` (plugin generates uuid per start)
- **But**: the plugin has only ONE `self.run` (`main.py:326`). The second session's `start` would replace the first session's run state in the plugin.

**RISK**: **The plugin is single-tenant by design.** upstream's spec says "local TUI sessions only" — one TUI, one session. With N sessions in a daemon, two concurrent grants would corrupt the plugin's `self.run` state. The host-side identity checks don't prevent this because the handler is the same object.

**DECISION NEEDED**: Either:
- (a) **One driver grant per plugin process-wide** — the actor checks a shared `Arc<AtomicBool>` or similar before arming. Second arm attempt → error "plugin already driving session X".
- (b) **One plugin instance per session** — the extension manager creates a fresh process per session. Heavy.
- (c) **Plugin protocol change** — add `session_id` to the poll frame so the plugin can multiplex. But spec says zero plugin changes.
**INFERENCE**: (a) is correct. The ExtensionManager or the EngineHost should maintain a `driver_lock: HashMap<String, SessionId>` — before arming, check if the plugin is already driving another session. This is a small addition.

---

## Q6 — Autonomous plugin contract

Source: `jr-112/examples/extensions/autonomous/main.py` (693 lines), `plugin.json`.

### Host fields the plugin requires

| Field | Where | Version |
|-------|-------|---------|
| `feedback_version: 1` | `main.py:411` — always sends in Start | v0.1.4+ |
| `context_mode` (auto\|off) | `main.py:411` — always sends in Start | v0.1.4+ |
| `time_checkpoint_version: 1` | `main.py:411` — always sends in Start | v0.1.4+ |
| `run_id` (uuid) | `main.py:392` | v0.1.0+ |
| `models` (favorites list) | `main.py:410` | v0.1.0+ |
| `delay_ms` (1000+) | `main.py:411` | v0.1.0+ |
| `max_duration_ms` (optional) | `main.py:413` | v0.1.0+ |

### Poll request fields consumed

| Field | `main.py` line |
|-------|---------------|
| `run_id` | 449 — must match `self.run.run_id` |
| `decision_id` | 452 — dedup cache key |
| `outcome` | 454 — success/provider_error/selection_rejected/time_checkpoint/blocked |
| `error_kind` | 456 — none/auth/quota/rate_limit/transient/wall_clock/unknown |
| `model`, `effort` | 482 — must match current `run.selection` |
| `feedback` (optional) | 462 — unknown/changed/repeated/empty |

### Does the port change the plugin contract?

**NO.** The plugin communicates exclusively through:
1. `command.invoke` for `/auto start|stop|status|favorites` → `session_driver` structured reply
2. `command.invoke` for `__session_driver__` → `PollRequest` JSON arg → `next|stop` reply

The actor uses the exact same `ExtensionHandler::invoke_command()` method with the same JSON contract. The `PollRequest` struct and `Reply` enum live in the engine crate and are unchanged. The plugin process never sees who called it or whether it's a TUI or an actor.

**GOAL: Zero plugin changes. ✅ ACHIEVABLE.**

---

## Q7 — PORT PLAN

### New types

| Type | Location | Size | Description |
|------|----------|------|-------------|
| `DriverState` | `agent-engine/src/session/driver.rs` (new file) | M | Analogue of `Active` — Grant, handler, generation, cancel, workers, feedback tracker, steering deque, proposal, outcome. No App references. |
| `DriverArmed` event variant | `types.rs` SessionEventWire | S | `{ plugin_id, run_id, models, selection, deadline, notice }` |
| `DriverRevoked` event variant | `types.rs` SessionEventWire | S | `{ reason, undelivered_steering: Vec<String> }` |
| `DriverStart` command variant | `types.rs` SessionCommand | S | `{ plugin: String, command: String, arg: String }` |
| `TurnTrigger::DriverAuto` | `types.rs` TurnTrigger | S | Distinguish driver-initiated turns from event/user/plugin |
| `WireDriverArmed`, `WireDriverRevoked` | `wire.rs` WireSessionEvent | S | Wire mirror of new events |
| Driver lock (process-wide) | `EngineHost` or shared state | S | `Arc<Mutex<HashMap<String, SessionId>>>` — one grant per plugin |

### Actor changes

| Step | File | Change | Size | Hours |
|------|------|--------|------|-------|
| A1 | `session/driver.rs` (NEW) | Create `DriverState` struct, `driver_revoke()`, `driver_arm()`, `driver_idle_conflict()`, `driver_tick()` methods. Port logic from TUI `session_driver.rs` lines 99-1048, replacing `App` access with direct `SessionActor` field access. | **L** | 8-12 |
| A2 | `session/actor.rs` | Add `driver: Option<DriverState>` field to `SessionActor`. Add `driver_timer` arm to `run()` select loop. Call `driver_revoke()` on Cancel, End, Checkpoint, NewSession. Call `driver_tick()` from timer arm. | **M** | 3-4 |
| A3 | `session/actor.rs` `on_stream_event()` | Extend Done/Error handlers: call `driver.observe_terminal()`, `driver.observe_feedback()`. Extend `SteeringDelivered`: call driver steering acknowledgement. | **M** | 2-3 |
| A4 | `session/actor.rs` `submit()` | When `driver.is_some()` and not streaming: route to driver steering queue instead of normal submit. When streaming: existing steer path, but also update driver feedback. | **S** | 1-2 |
| A5 | `session/actor.rs` event drain | After `drain_event_queue()`: call `driver.observe_events(dispositions)`. | **S** | 1 |
| A6 | `session/actor.rs` park logic | Add `driver.is_none()` to `can_park()` guard. On park attempt while armed: skip (keep_warm semantics). | **S** | 0.5 |
| A7 | `session/actor.rs` checkpoint | Add `driver_revoke("daemon reload")` to `Checkpoint{Reload}` handler. | **S** | 0.5 |
| A8 | `session/types.rs` | Add `DriverStart` to `SessionCommand`, `DriverArmed`/`DriverRevoked` to `SessionEventWire`, `DriverAuto` to `TurnTrigger`. Add to `is_input_command()`, `command_name()`, Debug impls. | **M** | 2 |
| A9 | `session/wire.rs` | Add wire mirrors for new events. | **S** | 1 |
| A10 | `extensions/session_driver.rs` | Move `feedback.rs` from TUI crate into engine crate (or a shared location). No logic changes. | **S** | 1 |
| A11 | Engine-host driver lock | Add process-wide `driver_lock: Arc<Mutex<HashMap<String, SessionId>>>` to `EngineHost`. Check on arm; release on revoke. | **S** | 1 |

### TUI changes

| Step | File | Change | Size | Hours |
|------|------|--------|------|-------|
| T1 | `tui/session_driver.rs` | **DELETE** (or reduce to a thin client-side state struct tracking armed/revoked for UI rendering). Replace 1811 lines with ~100 lines: `DriverUiState { armed: bool, plugin_id, selection, deadline }`, updated from `DriverArmed`/`DriverRevoked` events. | **M** | 3-4 |
| T2 | `tui/input.rs` | Esc/Ctrl-C while `driver_ui.armed` → send `SessionCommand::Cancel` (not `InputAction::Abort` — that was the local-only path). | **S** | 1 |
| T3 | `tui/dispatch.rs` | Remove all `session_driver::*` calls. Plugin command with `session.drive` → send `SessionCommand::DriverStart`. User submit → send `SessionCommand::Submit` (actor handles steering routing). Explicit command → actor already revokes. | **M** | 2-3 |
| T4 | `tui/stream_handler.rs` | Remove all `session_driver::*` calls. Driver feedback/terminal/checkpoint observation is now actor-side. Keep `DriverArmed`/`DriverRevoked`/`DriverSteered` event handlers for UI rendering. | **M** | 2-3 |
| T5 | `tui/mod.rs` | Remove `session_driver::tick()` arm from select loop. Remove shutdown revoke. Add event handlers for new driver events. | **S** | 1 |
| T6 | `tui/app.rs` | Remove `session_driver: SessionDriver` field. Add `driver_ui: DriverUiState`. | **S** | 0.5 |

### Attach-line client changes

| Step | File | Change | Size | Hours |
|------|------|--------|------|-------|
| C1 | `cmd/attach.rs` (or thin client) | Handle `DriverArmed`/`DriverRevoked` events: display notices, track armed state. Route Esc while armed as `Cancel` not disconnect. | **S** | 1-2 |
| C2 | `cmd/send.rs` | No special handling needed — `Submit` while driver armed is handled actor-side (routes to steering). | **S** | 0.5 |

### Tests

| Step | File | Change | Size | Hours |
|------|------|--------|------|-------|
| X1 | `tests/session_driver_actor.rs` (NEW) | Port ~12 TUI lifecycle tests to actor tests. Use `SessionHandle::send()` + capture `Envelope` events. Mock `ExtensionHandler`. | **L** | 6-8 |
| X2 | `tests/session_actor_differential.rs` | Add differential harness for the full timer integration test (arm → steer → prepare → poll → revoke). | **M** | 3-4 |
| X3 | Engine crate tests | Verify existing tests pass unchanged. feedback.rs tests move with module. | **S** | 1 |
| X4 | TUI tests | Remove/update TUI driver tests. Keep input routing tests. | **S** | 1 |

### Docs

| Step | File | Change | Size | Hours |
|------|------|--------|------|-------|
| D1 | `docs/extensions/session-drivers.md` | Remove "local TUI sessions only" clause. Add "driver lives in the session actor; all client types support it. One concurrent grant per plugin process-wide." | **S** | 0.5 |
| D2 | `docs/daemon-mode.md` | Add driver-in-actor section: grant survives detach, park blocked while armed, reload revokes. | **S** | 0.5 |

### Order of execution

```
Phase 1: Foundation (days 1-2)
  A8  → new types (commands/events/trigger)
  A9  → wire mirrors
  A10 → move feedback.rs
  A11 → driver lock

Phase 2: Actor core (days 2-4) ← RISKIEST
  A1  → DriverState + driver.rs (the big port)
  A2  → actor integration (field + select loop)
  A3  → stream event handling
  A4  → submit routing
  A5  → event drain
  A6  → park guard
  A7  → checkpoint revoke
  X3  → verify engine tests

Phase 3: TUI thinning (days 4-5)
  T1  → gut session_driver.rs
  T2  → input routing
  T3  → dispatch changes
  T4  → stream_handler changes
  T5  → mod.rs changes
  T6  → app.rs changes

Phase 4: Tests + clients (days 5-6)
  X1  → actor lifecycle tests
  X2  → differential harness
  X4  → TUI test updates
  C1  → attach client
  C2  → send client

Phase 5: Docs (day 6)
  D1, D2
```

---

## Risks (ranked)

| # | Risk | Severity | Likelihood | Mitigation |
|---|------|----------|-----------|------------|
| 1 | **A1 (DriverState port) is large and touches the turn machine** — `tick()` alone is 330 lines of state-machine logic with 4 TaskResult arms, each touching runtime/streaming/app state. The actor's equivalents are similar but not identical (no `App`, different streaming lifecycle). | High | High | **De-risk first**: write the actor's `driver_tick()` arm by arm, with unit tests per TaskResult variant, before wiring into the select loop. Use the differential harness (X2) early. |
| 2 | **Plugin single-tenancy** — two daemon sessions arming the same plugin will corrupt state. Not caught by any existing test. | High | Medium | Implement A11 (driver lock) FIRST, before any actor arm logic. |
| 3 | **submit() steering routing** — the actor's `submit()` must now differentiate "user submit while driver idle" (should steer) from "user submit, no driver" (normal). Getting this wrong either loses steering or revokes unexpectedly. | Medium | Medium | Explicit tests: submit-while-armed-idle → steering; submit-while-armed-streaming → steer; submit-no-driver → normal. |
| 4 | **Detach-while-armed (3.1)** — charging the user's API key headlessly without visible UI is a footgun. | Medium | High (by design) | Address F27 first: print warning on detach-while-streaming/armed. Add `synaps daemon sessions` status to show armed grants. |
| 5 | **feedback.rs crate move** — it imports from `synaps_cli::{LlmEvent, StreamEvent}`. These types exist in the engine crate too but may differ. | Low | Medium | Verify type identity; feedback.rs only uses enum matching, no private fields. |
| 6 | **Wire protocol version bump** — new SessionEventWire variants require updating `PROTOCOL_VERSION` in wire.rs. | Low | Certain | Protocol is exact-match today; bump to v3 alongside the additive variants. |

---

## Open decisions for Haseeb

| # | Decision | Options | Recommendation |
|---|----------|---------|----------------|
| 1 | **Detach-while-armed policy** | (a) Grace timer; (b) Revoke on detach; (c) Keep running | (c) Keep running + fix F27 warning. Daemon-first means headless is the feature. |
| 2 | **Driver notices: new event or SystemNotice?** | Separate `DriverNotice` vs tagged `SystemNotice` | SystemNotice with "🔄 " prefix — simpler, no wire change. |
| 3 | **Plugin single-tenancy enforcement** | (a) Process-wide lock; (b) Per-session plugin instance; (c) Protocol change | (a) Process-wide lock — minimal, correct, zero plugin changes. |
| 4 | **`synaps send` + armed driver** | (a) Route to steering; (b) Revoke + submit | (a) Route to steering — matches spec. |
| 5 | **Enable driver for all session types?** | Yes / No (TUI-only initially) | Yes — the constraint was implementation, not security. |
| 6 | **Phase this into the 112 merge or do it after?** | (a) Include in 112 merge; (b) Separate PR after 112 lands | (b) Separate PR — 112 is already 50k lines. Port the driver after the base merge stabilizes. Avoids compounding merge risk. |

---

## Size estimate

| Phase | Hours | Days (solo) |
|-------|-------|-------------|
| Phase 1: Foundation | 5 | 0.5-1 |
| Phase 2: Actor core | 16-22 | 2-3 |
| Phase 3: TUI thinning | 9-12 | 1-1.5 |
| Phase 4: Tests + clients | 11-15 | 1.5-2 |
| Phase 5: Docs | 1 | 0.25 |
| **Total** | **42-55** | **5.5-7.5** |

**Critical path**: A1 (DriverState port, ~10h) → A2-A5 (actor integration, ~8h) → T1-T4 (TUI thinning, ~9h). The riskiest step is A1: the `tick()` port. De-risk by writing it first with synthetic tests before wiring into the real actor loop.
