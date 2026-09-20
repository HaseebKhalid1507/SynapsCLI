# E — driver→actor port spec (Wall 2)

> **Wall 2 executable spec** for #112 `feat/context-continuation` onto daemon-mode dev.
> Derived from code; every claim cites `path:line` on the build worktree
> (`112-build/`) or `git show origin/feat/context-continuation:<path>` for JR's
> TUI half. **FACT** = read in code. **INFERENCE** = judgment from code evidence.
> Conventions follow A/B/D.

---

## §1 — What the TUI loop actually does

Source: `git show origin/feat/context-continuation:crates/agent-tui/src/tui/session_driver.rs` (1 811 lines).

Cross-reference key: **B-row** = B-driver-port.md Q1 row number. **Engine?** =
line in `crates/agent-engine/src/extensions/session_driver.rs` where that policy
already lives. **Actor/IO** = glue that must move into the actor.
**Render** = stays in TUI as render-only.

| # | Responsibility | TUI lines | Pure policy in engine `session_driver.rs`? | Must move to actor | Stays in TUI (render-only) | B-row |
|---|---------------|-----------|-------------------------------------------|-------------------|--------------------------|-------|
| 1 | `SessionDriver` state container | 99–111 | — | **Yes**: becomes `DriverState` field on `SessionActor` | — | 1 |
| 2 | `is_active()` | 114–116 | — | **Yes**: actor checks `driver.is_some()` | Client tracks `armed: bool` from `DriverArmed`/`DriverRevoked` | 2 |
| 3 | `owner()` | 118–123 | — | **Yes**: actor field `driver.plugin_id` | — | 3 |
| 4 | `invalidate()` | 125–135 | — | **Yes**: actor `driver_revoke()` drops `DriverState` | — | 4 |
| 5 | `spawn()` | 137–148 | — | **Yes**: actor spawns with `tokio::spawn` + `Task<T>` abort-on-drop | — | 5 |
| 6 | `revoke(app, reason)` | 151–177 | — | **Yes**: actor `driver_revoke()` + emit `DriverRevoked { reason, undelivered }` | Client receives `DriverRevoked`, restores steering to input draft | 6 |
| 7 | `submit_steering(app, input, tx)` | 183–241 | — | **Yes**: actor routes `Submit`/`Steer` while `driver.is_some()` to steering FIFO | Client sends `Steer`/`Submit`; renders `Steered` event | 7 |
| 8 | `restore_submission()` | 243–250 | — | — | **Render**: client restores from `DriverRevoked.undelivered` | 8 |
| 9 | `steering_delivered(app, message)` | 252–269 | — | **Yes**: actor pops front of steering deque on `SteeringDelivered` | Client renders ack | 9 |
| 10 | `history_with_steering()` | 271–282 | — | **Yes**: actor builds proposed history for `prepare()` | — | 10 |
| 11 | `commit_submission()` | 286–302 | — | **Yes**: actor drains steering into `api_messages` before turn start | — | 11 |
| 12 | `auto_wakes_allowed(app)` | 304–311 | — | **Yes**: actor gate in `on_queue_wake()` | — | 12 |
| 13 | `user_takeover(app, runtime)` | 314–322 | — | **Yes**: actor clears `auto_wakes_blocked` on explicit `Submit` | — | 13 |
| 14 | `observe_events(app, dispositions)` | 326–343 | — | **Yes**: actor event-drain path checks dispositions | — | 14 |
| 15 | `observe_checkpoint(app, session_id, ok)` | 347–357 | — | **Yes**: actor checkpoint handler revokes on failure/mismatch | — | 15 |
| 16 | `notice()` | 359–365 | — | — | **Render**: `SystemNotice` | 16 |
| 17 | `start_command(app, manager, …)` | 369–433 | `parse_reply()` :230, `Grant::from_start()` :291, `validate_reply()` :169 | **Yes**: becomes `DriverStart` command handler on actor; invoke_command + arm | Client sends `DriverStart` | 17 |
| 18 | `idle_conflict()` | 437–463 | — | **Yes**: actor checks `compact`, `queued_message`, `pending_events`, `context_head.is_blocked` | — | 18 |
| 19 | `completion_blocked()` | 465–480 | — | **Yes**: actor checks `runtime.orchestration().completion_gate()` + subagent list | — | 19 |
| 20 | `live_generation()` | 482–493 | — | **Yes**: actor checks `handler.lifecycle_snapshot()` | — | 20 |
| 21 | `same_lifecycle()` | 495–500 | — | **Yes**: generation == pinned generation | — | 21 |
| 22 | `same_handler()` | 503–518 | — | **Yes**: `Arc::ptr_eq` + generation | — | 22 |
| 23 | `schedule()` | 520–536 | `Grant::check_delay()` :368 | **Yes**: actor sets `driver.proposal` + due time | — | 23 |
| 24 | `arm()` | 538–602 | `Grant::from_start()` :291, `Grant::apply_context_mode()` :345 | **Yes**: actor `driver_arm()` creates `DriverState`, emits `DriverArmed` | Client renders armed status bar | 24 |
| 25 | `observe_feedback(app, event)` | 606–612 | — | **Yes**: actor feeds `LlmEvent` into `feedback::Tracker` | — | 25 |
| 26 | `enum Terminal` | 616–620 | `classify_turn_error()` :452, `classify_error()` :537 | **Yes**: enum lives in actor's `driver.rs` | — | 26 |
| 27 | `capture_terminal(event, canceled)` | 622–638 | classification in engine :452-675 | **Yes**: actor maps Done/Error/EOF to Terminal | — | 27 |
| 28 | `observe_terminal(app, runtime, terminal)` | 640–690 | `Grant::time_checkpoints_enabled()` :355, `turn_budget().max_elapsed` via runtime | **Yes**: actor sets `driver.outcome`, revokes on Blocked/Canceled | — | 28 |
| 29 | **`tick()`** (main driver loop) | 695–1027 | `poll()` :695, `prepare()` :775, `validate_prepared()` :860 | **Yes**: becomes actor `driver_tick()` in 200 ms select arm | — | 29 |
| 30 | `poll_request()` | 1031–1048 | `PollRequest` struct :430 | **Yes**: actor builds PollRequest | — | 30 |
| 31 | `struct Active` | 51–74 | `Grant` :275 | **Yes**: becomes `DriverState` (minus `App` refs) | — | 31 |
| 32 | `struct Pending` | 76–81 | — | **Yes**: actor `driver_pending: Option<Task<TaskResult>>` | — | 32 |
| 33 | `struct Scheduled` | 46–49 | — | **Yes**: `driver.proposal` | — | 33 |
| 34 | `enum TaskResult` | 83–97 | — | **Yes**: same enum in actor's `driver.rs` | — | 34 |
| 35 | `struct Task<T>` | 39–44 | — | **Yes**: abort-on-drop wrapper | — | 35 |
| 36 | `mod feedback` (345 ln) | feedback.rs | — (TUI crate only today) | **Yes**: moves to engine crate | — | 36 |
| 37 | `mod steering_tests` (1 040 ln) | steering_tests.rs | — | Tests → actor tests | — | 37 |

### B cross-check

B's 37 rows are comprehensive. **No row missed or wrong.** Two minor precision notes:

- B row 18 lists `gamba_child` and `modal_stack.top()` as idle_conflict checks (TUI :437-463). **FACT**: `gamba_child` and `modal_stack` are TUI-only rendering concepts; the actor has no equivalent. The actor's `idle_conflict` should check `compact.is_some()` (`actor.rs` compact field), `queued_message.is_some()` (conv field), `!pending_events.is_empty()` (conv field), `context_head.is_blocked()` (conv field), and `ext_ready.is_some()` (`actor.rs:371`). `secret_prompts` equivalent is `!pending_prompts.is_empty()` (`actor.rs:348`).
- B row 7 mentions 256 KiB steering bound. **FACT**: The TUI code at :193-202 checks `total + input.len() > 256 * 1024` on the steering deque. The actor must replicate this bound.

---

## §2 — Actor surface

### New `SessionCommand` variants

Current enum at `session/types.rs:460-539`.

```rust
// session/types.rs — addition
SessionCommand::DriverStart {
    plugin: String,    // extension id (e.g. "autonomous")
    command: String,   // interactive command name (e.g. "auto")
    arg: String,       // everything after the command ("start -- do X")
}
```

**No `DriverStop` needed.** INFERENCE: `Cancel` (`types.rs:473`) already handles
this — when the driver is armed, the actor's Cancel handler calls `driver_revoke()`
then cancels any active stream. This matches the TUI where `Esc` → `revoke()` then
Cancel.

**No `DriverSteer` needed.** INFERENCE: Reuse `Submit`/`Steer`. The actor's
submit path (`actor.rs:1078-1105`) already routes `Submit`-while-streaming to
`steer()` at :1088. When the driver is armed and idle (not streaming), `Submit`
should route to the steering FIFO instead of starting a normal turn. The actor
differentiates via `self.driver.is_some()`.

### New `SessionEventWire` variants

Current enum at `session/types.rs:710-808`.

```rust
// session/types.rs — additions
SessionEventWire::DriverArmed {
    plugin_id: String,
    run_id: String,
    models: Vec<session_driver::Selection>,
    selection: session_driver::Selection,
    deadline_ms: Option<u64>,      // remaining ms, not absolute Instant
    notice: String,
}

SessionEventWire::DriverRevoked {
    reason: String,
    undelivered_steering: Vec<String>,  // client restores to input draft
}

SessionEventWire::DriverTurnOutcome {
    outcome: session_driver::Outcome,
    selection: session_driver::Selection,
    feedback: Option<String>,
}
```

**INFERENCE**: `DriverNotice` is NOT needed as a separate variant.
SYNTHESIS decision 2 (implicit): plugin notices route through
`SystemNotice(String)` (`types.rs:741`) — simpler, no wire change. The TUI
can prefix driver notices with "🔄 " for visual distinction based on
`driver_ui.armed`.

### `TurnTrigger` addition

Current enum at `session/types.rs:815-821`.

```rust
TurnTrigger::DriverAuto,  // distinguishes driver-initiated turns
```

### Wire mirrors

`session/wire.rs` — add `WireDriverArmed`, `WireDriverRevoked`,
`WireDriverTurnOutcome` in the same pattern as existing wire events.
Protocol version bump to v3 (additive variants; old clients skip unknown).

### State machine on `SessionActor`

New file: `session/driver.rs`.

```rust
pub(crate) struct DriverState {
    pub grant: Grant,                       // engine session_driver.rs:275
    pub handler: Arc<dyn ExtensionHandler>, // pinned at arm
    pub handler_generation: u64,            // pinned at arm
    pub cancel: CancellationToken,
    pub workers: Arc<Mutex<SubagentRegistry>>,
    pub worker_epoch: u64,
    pub deadline_task: Option<Task<()>>,
    pub proposal: Option<Scheduled>,
    pub selection: Selection,
    pub awaiting_terminal: bool,
    pub outcome: Option<(Outcome, String)>,
    pub feedback: feedback::Tracker,
    pub completed_feedback: &'static str,
    pub steering: VecDeque<String>,
    pub auto_wakes_blocked: bool,
    pub cost_at_arm: f64,                   // session_cost snapshot at arm
    // pending task lives as a separate field on SessionActor
    // (not inside DriverState) for borrow-checker ergonomics
}

impl Drop for DriverState {
    fn drop(&mut self) {
        self.cancel.cancel();
        // cancel workers at the pinned epoch
        cancel_workers(&self.workers, self.worker_epoch);
    }
}
```

**Fields on `SessionActor`** (additions to `actor.rs:316`):
```rust
pub(crate) driver: Option<DriverState>,
pub(crate) driver_pending: Option<DriverPending>,
pub(crate) driver_timer: Option<tokio::time::Interval>,
pub(crate) driver_suspend_deadline: Option<tokio::time::Instant>,
```

### When `driver` is reset

| Event | Behaviour | Evidence |
|-------|-----------|---------|
| **`DriverStart`** | If `driver.is_some()`, revoke existing first; then invoke command, parse reply, arm | TUI :369-433 (`start_command` calls `revoke` then `arm`) |
| **`Cancel`** | `driver_revoke("canceled")`; then normal cancel logic | TUI input.rs:88-93 → `revoke()` |
| **Any non-driver command** | `driver_revoke("explicit user action")` | TUI dispatch.rs:141 |
| **`NewSession`** | `driver_revoke("session replaced")` | TUI app.rs:575 |
| **`End`** | `driver_revoke("session ending")` | TUI mod.rs:348 |
| **`Checkpoint{Reload}`** | `driver_revoke("daemon reload")` BEFORE save | JR spec session-drivers.md:206-208, D-security S10 |
| **Park** | Never happens while armed — `can_park()` returns false (see §3) | actor.rs:763-776 + new guard |
| **Detach (last client)** | Suspend-and-timeout — see §3 S1 gate | SYNTHESIS decision 4 |
| **Grant expired** | `driver_revoke("grant deadline reached")` in `driver_tick()` | TUI :716 |
| **Handler death/reload** | `driver_revoke("handler lifecycle changed")` in `driver_tick()` | TUI :720-728 |
| **Idle conflict** | `driver_revoke("queued work or lifecycle change")` | TUI :736-746 |

### `SessionReloadRecord` and reload

**FACT**: `SessionReloadRecord` (`types.rs:525-535`) carries `config`,
`keep_warm`, `lifecycle`, `settings_replay`, `model`, `thinking_level`. It
does NOT carry any driver state.

**Decision (derived from D-security S10 + JR spec :206-208)**: Driver state is
**never persisted in the reload record**. `checkpoint()` (`actor.rs:1978-2010`)
explicitly revokes the driver (`driver_revoke("daemon reload")`) before saving.
The new process starts clean — no driver, no grant. Plugin's `initialize()`
resets `self.run = None` (`main.py:339`). The handler-generation check
(`process.rs:636-665`) would also catch any stale reference, but explicit
revocation is cleaner.

**Compaction**: The driver is revoked before compaction starts
(`idle_conflict` catches `compact.is_some()`). On `CompactionApplied`, the
session_id changes → the grant's `session_id` mismatches → revocation on next
tick. No special handling needed.

---

## §3 — The three gates, as code

### Gate 1: S1 — zero-client prompt (suspend + notify + timeout → revoke)

**Current behaviour (FACT)**

- `actor.rs:1631-1645`: `on_prompt_request` pushes `(PromptRequest, oneshot::Sender)` onto `pending_prompts`, emits `Prompt(pr)` over broadcast. If `self.attached.is_empty()`, no client is subscribed — the `Prompt` envelope is silently dropped.
- `actor.rs:763-768`: `can_park()` requires `self.pending_prompts.is_empty()`. A session with a pending prompt and zero clients stays **Live, streaming, blocked** — the oneshot `response_rx` in `tools/secret_prompt.rs:20-31` never resolves.
- `actor.rs:1044`: `start_turn()` passes `self.config.auto_approve_confirms` — if `true`, prompts never fire but tools are auto-approved headlessly (see S9).

**SYNTHESIS decision 4**: suspend + notify + timeout → revoke. Never auto-approve under a driver.

**Mechanism**

1. **On last-client detach** (`actor.rs:1857-1878`), if `self.driver.is_some()`:
   - Emit `SystemNotice("driver suspended: no clients attached")`.
   - Arm `driver_suspend_deadline = Some(Instant::now() + T)`.
   - `T` = config value `driver.zero_client_timeout_secs` (default: 120, sourced from `SynapsConfig`).
   - While suspended: `driver_tick()` short-circuits (no polls, no prepares, no stream starts).

2. **On client re-attach**: clear `driver_suspend_deadline`. Emit `SystemNotice("driver resumed: client attached")`. Replay any buffered `Prompt` events to the attaching client. Normal `driver_tick()` resumes.

3. **On timeout expiry** (checked in the run loop's select): `driver_revoke("no client within timeout")`. Answer all pending prompts with `None` (deny — `tools/discovery.rs:194` treats `None` as `Unauthorized`; fail-closed). Normal park/end lifecycle follows.

4. **Independent prompt gate**: Even without the suspend timer, if the driver is armed and a `SecretPromptRequest` arrives while `attached.is_empty()`, auto-answer with `None` immediately. Don't let the stream block.

**Tests**

| Test | Proves |
|------|--------|
| `driver_armed_last_detach_suspends_tick` | Detach → no poll/prepare fires → re-attach → tick resumes |
| `driver_armed_zero_client_timeout_revokes` | Detach → sleep(T+1) → grant revoked, `DriverRevoked` emitted |
| `prompt_with_zero_clients_auto_denied` | Driver turn → tool fires Confirm → `attached.is_empty()` → prompt answered `None`, tool gets `Unauthorized` |
| `reattach_before_timeout_clears_deadline` | Detach → sleep(T/2) → re-attach → sleep(T) → grant still alive |

### Gate 2: S3 — spend caps (per-run USD + daemon ceiling)

**Current behaviour (FACT)**

- `runtime/budget.rs:20-26`: `TurnBudget` has `max_cost_usd: Option<f64>`. This is a **per-turn** budget checked by the stream loop's `TurnBudgetMeter` (`budget.rs:171-216`).
- `session/actor.rs:1303-1326`: `StreamEvent::Session(SessionEvent::Usage{…})` → `self.conv.add_usage(…)` updates `conv.session_cost`. **FACT**: `session_cost` is tracked but **never gated** — no ceiling enforced.
- `engine/commands.rs:307-319`: `/budget` displays `max_cost_usd` but it's `None` by default for all roles (`budget.rs:58,70,81`).
- `extensions/session_driver.rs:196-199`: `max_duration_ms` bounded ≤365 days. No cost field in `Reply::Start` or `PollRequest` (:430-440).

**SYNTHESIS decision 5**: per-run USD from `DriverStart` + per-daemon ceiling in config.

**Mechanism**

1. **Per-run cap** declared in `DriverStart`:
   - Extend `Grant` (`session_driver.rs:275`) with `max_run_cost_usd: Option<f64>`.
   - Sourced from plugin's `Reply::Start` (new optional field `max_cost_usd`).
   - Host enforces `min(plugin_proposed, host_config.driver.max_run_cost_usd)`.
   - `DriverState.cost_at_arm` captures `conv.session_cost` at arm time.
   - **Enforcement point**: after every `Usage` event in `on_stream_event` (`actor.rs:1303-1326`), when `driver.is_some()`:
     ```rust
     let run_spend = self.conv.session_cost - self.driver.cost_at_arm;
     if self.driver.grant.max_run_cost_usd
         .is_some_and(|cap| run_spend >= cap) {
         self.driver_revoke("run cost cap reached");
         // hard stop — do NOT poll the plugin
     }
     ```

2. **Per-daemon ceiling** in config:
   - New `SynapsConfig` field: `driver.max_daemon_cost_usd: Option<f64>`.
   - `EngineHost` maintains `driver_aggregate_cost: Arc<AtomicU64>` (f64 bits → u64 via `f64::to_bits`/`from_bits`).
   - On every `Usage` event while driver is armed: atomically add delta to aggregate.
   - Before arming: check `aggregate + estimated_min_turn_cost < ceiling`.
   - On breach: revoke with `"daemon cost ceiling reached"`.

3. **Cost in PollRequest** (should-have):
   - Add `run_cost_so_far: Option<f64>` to `PollRequest` (`session_driver.rs:430`) — plugin can make informed stop decisions. Optional field, backward-compatible (serde `skip_serializing_if`).

**Tests**

| Test | Proves |
|------|--------|
| `driver_run_cost_cap_revokes_at_boundary` | Arm with `max_run_cost_usd=0.50` → simulate Usage events summing to $0.51 → `DriverRevoked("run cost cap reached")` |
| `driver_daemon_ceiling_prevents_arm` | Set daemon ceiling $1.00, aggregate already $0.95 → `DriverStart` refused |
| `cost_at_arm_captures_session_baseline` | Session already has $2.00 cost → arm → run spends $0.30 → cap of $0.50 not breached (delta = $0.30) |

### Gate 3a: S2/S5 — single-tenancy + detach

**Current behaviour (FACT)**

- `host.rs:51`: `ext_manager: Arc<RwLock<ExtensionManager>>` — **one** instance shared by ALL sessions.
- `manager.rs:1354-1370`: `session_driver_handler()` returns `handler.clone()` (same `Arc<dyn ExtensionHandler>`) to every caller.
- `main.py:326`: Plugin has single `self.run` — second session's `start` overwrites first session's run state.
- `actor.rs:763-776`: `can_park()` requires `attached.is_empty() && !streaming && compact.is_none() && pending_prompts.is_empty() && !keep_warm`. **No driver check today.**
- `session/types.rs:161`: `SessionConfig.keep_warm: bool` — `true` prevents park.

**SYNTHESIS decision 3**: one driver grant per plugin daemon-wide, zero plugin changes.

**Mechanism — single-tenancy lock**

1. Add to `EngineHost` (`host.rs:46`):
   ```rust
   driver_grants: std::sync::Mutex<HashMap<String, SessionId>>,
   ```
2. Before arming in `DriverStart` handler:
   ```rust
   let mut grants = host.driver_grants.lock().unwrap();
   if let Some(existing) = grants.get(&plugin_id) {
       if *existing != self.id {
           emit(SystemNotice(format!(
               "plugin '{}' already driving session {}", plugin_id, existing
           )));
           return; // refuse
       }
   }
   grants.insert(plugin_id.clone(), self.id.clone());
   ```
3. On `driver_revoke()` / `End` / session drop:
   ```rust
   host.driver_grants.lock().unwrap().remove(&driver.grant.plugin_id);
   ```

**Mechanism — detach with live driver**

**Decision (derived from SYNTHESIS + S5 + JR spec)**: Driver **keeps the session warm** during the suspend window. Park is blocked.

- Add to `can_park()` (`actor.rs:763`): `&& self.driver.is_none()`.
- **INFERENCE**: This is equivalent to implicit `keep_warm` while armed. When
  the driver is revoked (or the suspend timeout fires from S1), the park timer
  can arm normally. JR's `keep_warm` field on `SessionConfig` (:161) already
  exists for explicit pinning; the driver adds a dynamic equivalent via the
  `can_park()` guard.

**Justification (S5)**: The entire value proposition of daemon-mode drivers is
"close the lid, work continues." Revoking on detach would make the daemon driver
identical to the TUI driver. The suspend-and-timeout gate (S1 above) is the
safety net — the session stays warm but paused, and revokes after T seconds if
nobody returns.

**Tests**

| Test | Proves |
|------|--------|
| `single_tenancy_second_session_refused` | Session A arms autonomous → Session B sends `DriverStart` for same plugin → refused with notice |
| `single_tenancy_released_on_revoke` | Session A arms → revokes → Session B arms → succeeds |
| `can_park_false_while_driver_armed` | Arm driver → all other park conditions met → `can_park()` returns false |
| `driver_revoke_allows_park` | Arm → revoke → detach → `can_park()` returns true → parks normally |

### Gate 3b: S9 — `auto_approve_confirms` override

**Current behaviour (FACT)**

- `session/types.rs:139`: `pub auto_approve_confirms: bool` on `SessionConfig`, default `false`.
- `actor.rs:1044`: passed directly to `run_stream_with_messages`. When `true`, `runtime/stream.rs:28-41` returns `(ModelConfirmed, false)` — ALL tool activations auto-approved.
- JR spec `session-drivers.md:25-26`: "Ordinary tool approval [...] gates still apply."

**SYNTHESIS (implicit from D-security S9)**: driver-armed sessions MUST NOT auto-approve.

**Mechanism**

In `start_turn()` (`actor.rs:1023`), when `self.driver.is_some()`:
```rust
let auto_approve = if self.driver.is_some() {
    false  // S9: never auto-approve under a driver
} else {
    self.config.auto_approve_confirms
};
// ... pass auto_approve to run_stream_with_messages
```

**FACT**: This is a 3-line change at a single call site (`actor.rs:1044`).

**Tests**

| Test | Proves |
|------|--------|
| `driver_armed_overrides_auto_approve_to_false` | Config has `auto_approve_confirms: true` → arm driver → `start_turn` passes `false` → tool fires `Confirm` prompt → prompt emitted, not auto-approved |

---

## §4 — Phases

Flag: `session.drive` permission (`permissions.rs:30,93,114`). Nothing changes
for sessions without a driver — all new code is behind `if self.driver.is_some()`
guards. The permission is the existing gate; no new feature flag needed.

| Phase | Name | Files | Size | Hours | Test | Dark / flag story |
|-------|------|-------|------|-------|------|------------------|
| P0 | **Types + wire** | `session/types.rs` (add `DriverStart`, `DriverArmed`, `DriverRevoked`, `DriverTurnOutcome`, `DriverAuto`), `session/wire.rs` (wire mirrors, protocol v3 bump) | S | 2–3 | Existing serde round-trip tests extended; `cargo test -p agent-engine` green | Additive variants; old clients skip unknown events |
| P1 | **Move `feedback.rs`** | Copy `tui/session_driver/feedback.rs` → `engine/extensions/feedback.rs`; re-export; update imports | S | 1–2 | feedback.rs unit tests (15) pass in new location | No behaviour change |
| P2 | **Single-tenancy lock** | `host.rs` (add `driver_grants: Mutex<HashMap<String,SessionId>>`), expose `claim_driver`/`release_driver` methods | S | 1–2 | `single_tenancy_second_session_refused`, `released_on_revoke` | Inert until P3 arms it |
| P3 | **`DriverState` + arm/revoke** | New `session/driver.rs`: `DriverState`, `driver_arm()`, `driver_revoke()`, `DriverPending`, `TaskResult`. `actor.rs`: add `driver` field, `DriverStart` handler (invoke → parse → arm), Cancel/NewSession/End/Checkpoint → revoke hooks | M | 5–7 | `driver_start_arms_and_emits`, `cancel_revokes_driver`, `checkpoint_reload_revokes`, `new_session_revokes` | Behind `session.drive` permission; no plugin has it unless explicitly granted |
| P4 | **`driver_tick()` core** | `session/driver.rs`: port `tick()` (TUI :695-1027). Actor `run()` select loop adds 200 ms timer arm gated on `driver.is_some()`. State machine: validate lifecycle → process TaskResult (Command/Poll/Prepared/Started) → schedule → prepare → start stream. Implement arm by arm with unit test per TaskResult variant | **L** | 8–12 | Per-variant: `tick_processes_poll_result`, `tick_processes_prepared`, `tick_starts_stream`, `tick_revokes_on_lifecycle_death`, `tick_revokes_on_idle_conflict`. Differential harness against TUI tests | **Riskiest step.** 330-line state machine. Actor has no `App`; different streaming lifecycle requires careful translation |
| P5 | **Stream event hooks** | `actor.rs` `on_stream_event`: extend Done/Error → `driver.observe_terminal()`; SteeringDelivered → driver steering ack; LlmEvent → `driver.feedback.observe()` | M | 2–3 | `driver_observe_terminal_done_success`, `driver_observe_terminal_error_revokes`, `steering_delivered_pops_queue` | Guards: `if let Some(driver) = &mut self.driver` |
| P6 | **Submit routing** | `actor.rs` `submit()`: `driver.is_some() && !streaming` → push to steering FIFO (16 msgs / 256 KiB). Streaming → existing steer path, also update driver. `on_queue_wake()`: inhibit `WakeAction::RunTurn` while `driver.is_some()` (S4 fix) | M | 2–3 | `submit_while_armed_idle_queues_steering`, `submit_while_armed_streaming_steers`, `event_wake_inhibited_while_armed` | Behind driver-armed guard |
| P7 | **Security gates** | S1 zero-client (suspend timer + prompt auto-deny); S3 cost caps (Grant field, daemon aggregate, Usage enforcement); S9 auto_approve override; `can_park()` driver guard | M | 4–6 | All 12 tests from §3 | Config-gated (`driver.zero_client_timeout_secs`, `driver.max_run_cost_usd`, `driver.max_daemon_cost_usd`) |
| P8 | **TUI thinning** | `tui/session_driver.rs`: 1 811 → ~100 ln `DriverUiState`. `tui/input.rs`: Esc while armed → `Cancel`. `tui/dispatch.rs`: plugin cmd → `DriverStart`. Remove all `session_driver::*` calls. `stream_handler.rs`: remove driver callbacks, add `DriverArmed`/`DriverRevoked`/`DriverTurnOutcome` handlers. `mod.rs`: remove tick arm. `app.rs`: field swap | M | 4–5 | TUI input routing tests stay; driver lifecycle tests already ported in P4. `cargo test -p agent-tui` green | TUI becomes thin render client |
| P9 | **Attach/send clients** | `cmd/attach.rs`: handle DriverArmed/DriverRevoked (display, state tracking). Esc while armed → Cancel. `cmd/send.rs`: Submit while armed → actor handles routing | S | 1–2 | Manual: `synaps attach` while armed shows status; `synaps send` steers | Additive event handling |
| P10 | **Docs** | `session-drivers.md`: remove "local TUI sessions only"; add daemon semantics. `daemon-mode.md`: driver-in-actor section | S | 0.5 | Review | — |

### Critical path

```
P0 (types) → P1 (feedback) → P2 (lock) → P3 (arm/revoke) → P4 (tick) → P5 (stream hooks)
                                                  │                           │
                                                  ├→ P6 (submit routing) ─────┘
                                                  └→ P7 (security gates) → P8 (TUI) → P9 → P10
```

**P4 is the single riskiest phase.** De-risk by writing `driver_tick()` arm by arm
with synthetic tests before wiring into the real select loop.

### `feedback.rs` disposition

Moves from TUI crate to engine crate (P1). The module's 15 unit tests move with
it. Its only external dependency is `{LlmEvent, StreamEvent}` which are
`agent-core` types re-exported by the engine. No logic changes. The TUI stops
importing it; the actor's `driver.rs` imports from the engine crate directly.

---

## §5 — What we need from JR

1. **`feedback.rs` crate placement**: It imports `synaps_cli::{LlmEvent, StreamEvent}` — types originating in `agent-core`. Moving it to `engine/extensions/feedback.rs` should be a pure re-path with no functional change. **Confirm no TUI-only types are used internally** (INFERENCE: none found, but JR may have an intent for crate boundaries).

2. **`PollRequest` extension with `session_id`**: D-security S2 says the poll must carry `session_id` for multi-session safety. Single-tenancy (SYNTHESIS decision 3) prevents concurrent runs, so `session_id` is belt-and-suspenders. **Should `session_id` land in the poll frame now (additive, backward-compatible) or wait for multi-tenancy B-phase?** Determines whether the plugin contract changes.

3. **`Reply::Start` cost field**: SYNTHESIS decision 5 says per-run USD from `DriverStart`. Current `Reply::Start` (`session_driver.rs:68-91`) has no cost field. **Should `max_cost_usd` be plugin-proposed (new optional field) or host-imposed from config only?** If plugin-proposed, the reference plugin needs a `--cost` flag.

Everything else: derived — see the cited sections.

---

## §6 — Risks + estimate

### Revised estimate

| Phase | Hours (low) | Hours (high) | Notes |
|-------|-------------|--------------|-------|
| P0 Types + wire | 2 | 3 | Mechanical |
| P1 feedback.rs | 1 | 2 | Import fix; tests must pass |
| P2 Single-tenancy | 1 | 2 | Small; Mutex + HashMap |
| P3 Arm/revoke | 5 | 7 | Moderate; invoke_command + Grant creation |
| P4 tick() port | 8 | 12 | **Riskiest**: 330-line state machine translation |
| P5 Stream hooks | 2 | 3 | Guarded additions to existing handlers |
| P6 Submit routing | 2 | 3 | Submit/steer differentiation + wake inhibit |
| P7 Security gates | 4 | 6 | Three mechanisms + config plumbing |
| P8 TUI thinning | 4 | 5 | Deletions + thin state struct |
| P9 Clients | 1 | 2 | Additive event handling |
| P10 Docs | 0.5 | 0.5 | Text |
| **Total** | **30.5** | **45.5** | |

**B estimated 42–55h.** This spec revises to **30–46h** because:

- B counted A11 (driver lock, 1h) separately; absorbed into P2.
- B's TUI thinning (9–12h) → 4–5h: deletions are straightforward once actor handles all logic.
- Security gates (S1/S3/S9) now have concrete mechanisms rather than open questions.
- The `feedback.rs` move (B: 1h) is likely zero-change.

**Buffer**: +20% for integration surprises, especially P4 where `App`-to-actor
translation may reveal implicit ordering. **Buffered range: 37–55h.**

### Risks (ranked)

| # | Risk | Severity | Likelihood | Mitigation |
|---|------|----------|-----------|------------|
| 1 | **P4 `tick()` port** — 330 lines of state-machine with 4 TaskResult arms, each touching Runtime/streaming/App. Actor equivalents similar but not identical (no `App`, different streaming lifecycle). Implicit ordering (e.g. `app.streaming = true` before `push_msg`) may not hold. | High | High | Write arm by arm with per-variant unit tests. Differential harness. De-risk before wiring into select loop. |
| 2 | **Submit routing ambiguity** — `submit()` must distinguish "user submit while driver idle" (→ steer) from "user submit, no driver" (→ normal). Wrong → lost steering or unexpected revocation. | Medium | Medium | Explicit tests per scenario; guard is `self.driver.is_some()`. |
| 3 | **`feedback.rs` import compatibility** — uses `synaps_cli::{LlmEvent, StreamEvent}`. Engine crate may alias them differently. | Low | Medium | Verify type identity at compile; feedback.rs only does enum matching, no private fields. |
| 4 | **Wire protocol version** — new `SessionEventWire` variants require `PROTOCOL_VERSION` bump. Old clients see unknown events. | Low | Certain | Protocol designed for forward compat (unknown variants skipped). Bump to v3. |
| 5 | **Timer interactions** — 200 ms driver tick vs park timer vs subagent tick vs event-queue drain. | Medium | Low | Driver tick is additive in select. Park blocked by `can_park()` guard. No competition. |
