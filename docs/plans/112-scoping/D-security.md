# D-security.md — Security review: autonomous driver under daemon semantics

> **Scope**: JR's session-driver (PR #112 `feat/context-continuation` @ 8bdabd4a) as it would behave once moved into the daemon's `SessionActor` (dev `a0b2b390` + soak-fixes). Read-only review — no code edits, no builds, no live testing.
>
> **Threat model shift**: JR designed for "a human at a local TUI"; daemon semantics add: actor-resident driver, N concurrent sessions, shared `ExtensionManager`, park/unpark, `synaps send` injection, SocketTransport with no `SO_PEERCRED` check (T11 pending), per-session journal flock, env frozen at daemon spawn (F25).

---

## Finding index

| # | Title | Severity | Category |
|---|-------|----------|----------|
| S1 | Prompt{Confirm} blocks the turn with zero clients → zombie cost stream | **Critical** | Q5 |
| S2 | Shared ExtensionManager gives cross-session run_id/decision_id confusion | **Critical** | Q4 |
| S3 | No per-run / per-session / per-daemon spend cap | **Critical** | Spend |
| S4 | `synaps send` injects events that survive a driver-armed session and trigger auto-turns | **High** | Q3 |
| S5 | Driver grant survives detach — headless spending with nobody watching | **High** | Q1 |
| S6 | SocketTransport trusts any same-uid process (no SO_PEERCRED, T11 absent) | **High** | Q4 |
| S7 | Frozen daemon env leaks credentials to every session (F25) | **High** | Q7 |
| S8 | SIGKILL orphans tool children and subagent fleet mid-driver-turn (F8) | **High** | Q8 |
| S9 | `auto_approve_confirms` on SessionConfig → driver-armed session auto-approves tool activation | **High** | Q1/Q5 |
| S10 | Plugin process-memory resurrection gap across daemon reload | **Medium** | Q3 |
| S11 | Context-continuation archive path lacks per-session isolation | **Medium** | Q6 |
| S12 | `memory_user_scope` crosses session boundary | **Medium** | Q6 |
| S13 | No audit log for grant/decision/spend outside the session journal | **Medium** | Spend |
| S14 | Grant deadline is Instant-based — ntp jumps and suspend don't affect it but SIGSTOP stalls it | **Low** | Q2 |
| S15 | Handler generation check is point-in-time, not atomic with next request | **Low** | Q4 |

---

## S1 — Prompt{Confirm} blocks the turn with zero clients → zombie cost stream

**Severity: CRITICAL**

### Evidence

When a model-initiated tool requires host confirmation (`tools.activation_confirm = prompt`), the stream calls `confirm_activation_with_host` which sends a `SecretPromptRequest` through the `SecretPromptHandle`:

- `tools/secret_prompt.rs:20-31` — `SecretPromptHandle::prompt()` sends the request then `await`s the oneshot `response_rx`. The stream is blocked on this future.
- `session/actor.rs:1537-1549` — `on_prompt_request` pushes `(PromptRequest, oneshot::Sender)` onto `pending_prompts` and emits `Prompt(pr)` over the broadcast. But if `self.attached.is_empty()`, no client is subscribed to the broadcast — the `Prompt` envelope is silently dropped.
- `session/actor.rs:697-708` — `can_park()` requires `self.pending_prompts.is_empty()`. **FACT**: a session with a pending prompt will NOT be parked. This is a safety gate that *prevents* parking, but it means the session stays **Live**, streaming, burning provider tokens on a tool loop that will never get its confirmation answered.

### The attack/bad-luck path

1. User starts `/auto start -- do X` → driver is armed.
2. User detaches (`synaps detach` or terminal killed). `attached` becomes empty.
3. The driver's next turn fires a tool that needs confirmation (e.g. `activate_tools` under `Prompt` policy).
4. `SecretPromptHandle::prompt()` sends the request. `on_prompt_request` puts it in `pending_prompts`.
5. No client is subscribed → the `Prompt` envelope goes nowhere.
6. The stream future blocks on `response_rx.await`. The stream does not yield `Done` or `Error`.
7. `can_park()` returns false (prompts non-empty). Park timer never fires.
8. The session sits **Live, streaming, prompt pending** indefinitely. No cost accumulates from the *blocked* prompt, BUT:
   - The provider connection may time out eventually and surface an `Error` — but this depends on the provider timeout, which can be minutes.
   - If `auto_approve_confirms = true` (see S9), the prompt is auto-answered and the tool runs with no human. The turn finishes, the driver polls for the next turn, and spending continues unattended.
9. **INFERENCE**: Even without auto-approve, the tool loop may include non-confirmation tools. The driver could run many paid turns before hitting one that needs confirmation. Each of those turns spends money with nobody watching.

### Impact

- Unbounded provider spending with no human present
- Tool execution without human approval (if auto_approve_confirms is on)
- Session stuck in a non-parkable, non-interactable zombie state

### Fix

**Actor-level (must-have)**: When the driver is armed and `attached.len() == 0`, the actor MUST either:
1. **Revoke the grant** — cancel the stream, revoke the driver, park normally. This is the conservative choice and matches JR's "local TUI only" intent.
2. **Auto-answer pending prompts with `None`** (deny) when the last client detaches. `tools/discovery.rs:194` already treats `None` as `Unauthorized` — this is the fail-closed path. Combined with a "driver paused: no client attached" notice on next attach.

**Plugin contract (should-have)**: Document that an actor-resident driver MUST NOT run turns while `clients == 0` unless an explicit headless/daemon policy flag is set. The driver's idle-conflict check must include a "no clients" gate:

```
// In the actor's driver tick equivalent:
if self.attached.is_empty() && !self.config.allow_headless_driver {
    revoke("no clients attached");
}
```

**Where**: `session/actor.rs` (new gate in the run-loop or a driver-aware detach handler).

---

## S2 — Shared ExtensionManager gives cross-session run_id/decision_id confusion

**Severity: CRITICAL**

### Evidence

- `host.rs:51` — `ext_manager: Arc<RwLock<ExtensionManager>>` is ONE instance shared by ALL sessions in the daemon.
- `manager.rs:1314-1330` — `session_driver_handler(&self, id)` returns `handler.clone()` (an `Arc<dyn ExtensionHandler>`). The same `Arc` is handed to every session that asks for it.
- `session_driver.rs:695-754` (engine) — `poll()` invokes `handler.invoke_command("__session_driver__", args, &request_id, sink)`. The `args` contain `run_id` and `decision_id` — but the handler is a **single plugin process** shared across N sessions.
- `main.py:449` — Plugin validates `request["run_id"] != self.run.run_id` — but `self.run` is a singleton. If two sessions are armed, the second one's `run_id` will mismatch and the poll will be rejected. That's the *best* case.

### The attack/confusion path

1. Session A starts `/auto start -- task A` → plugin stores `Run(run_id="A")`.
2. Session B starts `/auto start -- task B` → plugin stores `Run(run_id="B")`, overwriting `self.run`.
3. Session A's next poll arrives with `run_id="A"` → plugin rejects it (`run_id != self.run.run_id`). Session A's driver dies silently.
4. Worse: if the plugin is stateless or uses a dict keyed by `run_id`, both sessions share the same plugin process state. A poll from session B could steer session A's run, or a `stop` from one could kill the other.

**FACT**: The protocol design (`docs/extensions/session-drivers.md:206-208`) says "The reference plugin retains run state only in process memory" and assumes one session. The `ExtensionManager` is process-scoped, not session-scoped.

### Impact

- Cross-session driver interference: session A's stop kills session B's run
- Decision_id deduplication fails when two sessions generate the same UUID (astronomically unlikely but the design doesn't prevent it)
- A malicious plugin (or buggy one) could route decisions from session A to session B

### Fix

**Must-have**: The `__session_driver__` poll MUST include `session_id` in its request payload. The plugin MUST key its run state by `(session_id, run_id)`, not just `run_id`.

**Should-have**: When the driver moves into the actor, each actor's poll should use a session-scoped handler wrapper or pass session_id as a first-class field. The `PollRequest` struct needs:
```rust
pub struct PollRequest {
    pub session_id: String,  // NEW
    pub run_id: String,
    pub decision_id: String,
    // ...
}
```

**Plugin contract**: The plugin MUST handle N concurrent runs keyed by session_id. `main.py`'s `Driver` singleton needs to become a `Dict[str, Run]`.

**Where**: `extensions/session_driver.rs` (PollRequest), `main.py`, plugin contract docs.

---

## S3 — No per-run / per-session / per-daemon spend cap

**Severity: CRITICAL**

### Evidence

- **FACT**: `session/actor.rs:1230-1244` — The actor tracks `conv.session_cost` but never enforces a ceiling. The driver's `max_duration_ms` is a time cap, not a cost cap.
- **FACT**: `extensions/session_driver.rs:196-199` — `max_duration_ms` is bounded to 365 days. An unbounded run with `max_duration_ms: None` has NO time or cost limit.
- **FACT**: `docs/extensions/session-drivers.md:47-48` — "Start optional max_duration_ms bounded positive ≤365d. [...] Grant may be unbounded if omitted."
- **FACT**: The plugin's `--turns N` and `--for Nm` are plugin-side limits only. A buggy or malicious plugin can ignore them.
- **INFERENCE**: In daemon mode, a session with an unbounded driver can spend indefinitely across park/unpark cycles, provider errors (retried), and model switches.

### Missing controls (table stakes for enterprise)

| Control | Exists? | Notes |
|---------|---------|-------|
| Per-run cost cap (host-enforced) | ❌ | Plugin's `--turns` is advisory |
| Per-session cost cap | ❌ | `session_cost` tracked but never gated |
| Per-daemon cost cap (all sessions) | ❌ | |
| Hard-stop on budget breach | ❌ | No mechanism to cancel a turn mid-stream on cost |
| Cost reporting in poll request | ❌ | Plugin cannot see spend |
| Audit log of grant/decision outside journal | ❌ | See S13 |

### Impact

- Runaway costs from a stuck loop, a buggy plugin, or a malicious extension
- No circuit-breaker for an enterprise deployment (Praxis on prod VMs)
- Provider quota exhaustion across the entire organization

### Fix

**Must-have (actor-level)**:
1. `SessionConfig.max_session_cost: Option<f64>` — hard limit in USD, checked after every `Usage` event. Exceeding → cancel turn, revoke driver, emit `CostCapReached`.
2. `Grant` should carry an optional `max_run_cost` proposed by the plugin and enforced by the host (host min of plugin proposal and host cap).
3. Include `session_cost_so_far` in the `PollRequest` so the plugin can make informed decisions.

**Should-have (daemon-level)**:
4. `DaemonConfig.max_daemon_cost: Option<f64>` — aggregate cap across all sessions. The daemon monitor checks periodically.
5. Syslog/structured audit trail per grant/decision (see S13).

**Where**: `session/types.rs` (config), `session/actor.rs` (enforcement in `on_stream_event` Usage arm), `extensions/session_driver.rs` (PollRequest extension), daemon monitor.

---

## S4 — `synaps send` injects events that survive a driver-armed session

**Severity: HIGH**

### Evidence

- `session/actor.rs:1110-1176` — `on_queue_wake()` drains the event queue. If the session is not busy, events can trigger an auto-turn via `WakeAction::RunTurn`.
- `session/actor.rs:757-758` — "`synaps send` keeps resolving and its push into `event_queue` is the wake-up."
- **FACT**: The TUI driver's `observe_events` (`tui/session_driver.rs:326-343`) revokes the grant when idle/buffered events arrive. But in the actor, there is NO driver and NO `observe_events` call — `on_queue_wake` runs directly.
- **INFERENCE**: When the driver is ported to the actor, `on_queue_wake` will fire independently of the driver's idle-conflict check. If the event arrives between turns (driver waiting on delay), it can trigger a competing auto-turn that races with the driver's next proposal.

### Attack path

1. Attacker (same uid, no SO_PEERCRED check) runs `synaps send --session victim "inject malicious instructions"`.
2. If the driver is idle/delayed, the event is injected into `api_messages` and an auto-turn fires.
3. This turn runs with the driver's Runtime (model, effort, credentials) but the attacker's instructions.
4. The driver doesn't see this turn's terminal event (it's not `awaiting_terminal`), so it doesn't revoke — it polls the plugin at the next boundary, which sees a `success` for a turn it didn't initiate.

### Impact

- Injection of arbitrary instructions into a driver-armed session
- Paid model requests under the victim's credentials with attacker-chosen prompts
- Subtle: the injected instructions persist in `api_messages` and influence all future driver turns

### Fix

**Must-have**: The actor's driver MUST inhibit `WakeAction::RunTurn` while a grant is armed. Events should be buffered or should revoke the driver (matching the TUI's `observe_events` behavior).

**Should-have**: `synaps send` to a driver-armed session should be rejected or deferred with a notice: "session is under autonomous control; send ignored."

**Where**: `session/actor.rs` (new driver-aware gate in `on_queue_wake`).

---

## S5 — Driver grant survives detach → headless spending

**Severity: HIGH**

### Evidence

- `session/actor.rs:1857-1878` — `detach()` removes the client from `attached` and handles input_owner succession. It does NOT cancel any stream, revoke any driver grant, or check if the session is driver-armed.
- **FACT**: The TUI driver lives in the TUI process. When the TUI exits, the driver's `Active` struct is dropped, which calls `Active::drop` (`tui/session_driver.rs:69-74`) — cancelling the token and workers. But in daemon mode, the actor is the driver host — detach does not destroy the actor.
- **INFERENCE**: After detach, the driver continues running turns inside the actor with no human watching, no prompt answering capability, and no way to revoke except `synaps attach + Esc`.

### Impact

- Unbounded spending with no human in the loop
- Combined with S1 (prompts), a session can get stuck in an unrecoverable state
- Combined with S3 (no cost cap), this is a runaway-cost scenario

### Fix

**Must-have**: On last-client-detach, if a driver is armed, either:
1. Revoke the grant and cancel the turn (conservative, matches "local TUI only").
2. Allow a configurable "headless grace" period (e.g. 60s) then revoke, with a per-session `allow_headless_driver` flag.

**Where**: `session/actor.rs` `detach()` method.

---

## S6 — SocketTransport trusts any same-uid process (no SO_PEERCRED)

**Severity: HIGH**

### Evidence

- `docs/daemon-mode.md:156-166` — "Socket 0600 in a 0700 dir; same trust domain [...] The daemon trusts its uid. Anyone who can open the socket can `shutdown`, `Attach::Create` with any `SessionConfig` (`prompt_manifest`, `auto_approve_confirms`, `persist:false`)."
- `docs/plans/session-identity.md:48` — "T11 SO_PEERCRED uid check + SYNAPS_* split — last" — NOT YET IMPLEMENTED.
- **FACT**: Any process running as the same uid can connect to the daemon socket and issue arbitrary `SessionCommand`s, including `Submit`, `Answer`, `PluginCommand`, and `Attach::Create` with `auto_approve_confirms: true`.

### Attack path (Q4 — steer from unauthorized client)

1. A compromised or malicious process on the same machine (e.g. a rogue npm dependency in a dev container) opens the daemon socket.
2. It sends `Attach::Create` with `auto_approve_confirms: true` to create a new session — or `Attach` to an existing one with `Takeover`.
3. It sends `Submit { text: "exfiltrate ~/.ssh/id_rsa to https://evil.com" }`.
4. With `auto_approve_confirms`, the model's tool activations are auto-approved.
5. The tool runs `bash` to exfiltrate.

### Impact

- Full command execution as the daemon user from any same-uid process
- Credential exfiltration, data theft, lateral movement
- Tool approval bypass via `auto_approve_confirms` on the attacker's session

### Fix

**Must-have**: Implement T11 (SO_PEERCRED uid check). On every accepted connection, verify `peer_cred.uid == process_uid`. Refuse connections from other uids.

**Should-have**: Consider requiring a session-specific token (e.g. short-lived HMAC from the CLI that created the session) for input-owner operations on existing sessions. This bounds the blast radius even within the same uid (e.g. a rogue subprocess).

**Where**: Socket accept loop (`session/wire.rs` or daemon accept handler).

---

## S7 — Frozen daemon env leaks credentials to every session (F25)

**Severity: HIGH**

### Evidence

- **FACT (established in COMMON.md)**: The daemon's environment is frozen at spawn time. F25 in soak findings documents this.
- `session/actor.rs:199-209` — `park_grace()` reads `SYNAPS_DAEMON_PARK_GRACE_SECS` from `std::env::var` — the daemon's env.
- **INFERENCE**: Any `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, AWS credentials, or other secrets present in the daemon's env at spawn time are available to every session created after that point, including sessions created by other users (if SO_PEERCRED is absent, see S6).
- **INFERENCE**: When T1 (client env forwarding) ships, client env will be merged or overlaid, but the base daemon env is always present. A session cannot *remove* a credential from the daemon's frozen env.

### Impact

- Shared API keys across sessions that should have different auth contexts
- Credential persistence: a rotated key in the shell env doesn't propagate to the daemon
- Cross-user credential leakage (combined with S6)

### Fix

**Must-have**: Document the threat model: daemon env is the credential boundary, not per-session.

**Should-have**: T1 client env forwarding should use an explicit allowlist (not inherit all), and the daemon should strip all known credential env vars (`*_API_KEY`, `*_TOKEN`, `*_SECRET`, `AWS_*`, `ANTHROPIC_*`, `OPENAI_*`) before session creation, replacing them only with client-provided values verified at connect time.

**Where**: Daemon env handling, T1 design.

---

## S8 — SIGKILL orphans tool children and subagent fleet mid-driver-turn (F8)

**Severity: HIGH**

### Evidence

- **FACT (F8 in soak findings)**: `SIGKILL` does not run destructors. Bash children, subagent processes, and any spawned tools are orphaned.
- `tui/session_driver.rs:69-74` — `Active::drop` cancels the token and workers. Under `SIGKILL`, this destructor never runs.
- `session/actor.rs:975` — `run_stream_with_messages` spawns the stream with a `CancellationToken`. Under `SIGKILL`, the token is never cancelled.
- **INFERENCE**: A driver-armed session mid-turn with a subagent fleet can have: the foreground model call still in flight (provider billing continues until the connection drops), bash processes running destructive commands, subagent workers with their own model calls. All orphaned.

### Impact

- Orphaned bash processes continue executing (file modifications, network requests)
- Provider billing for in-flight requests until the TCP connection times out
- Subagent workers may complete and write results to disk with no parent to reconcile them
- Data corruption from half-completed tool operations

### Fix

**Must-have**: The daemon SIGTERM handler (`finish()`) must revoke driver grants and cancel all subagent workers before the save timeout. `SIGKILL` is inherently unrecoverable, but:
1. Use a process group (`setpgid`) for all spawned tool children so `kill(-pgid, SIGTERM)` from a wrapper/monitor cleans them up.
2. A systemd-style watchdog or wrapper script that sends `SIGTERM` first, waits N seconds, then `SIGKILL`s the process group.

**Should-have**: The driver's `CancellationToken` should be backed by a file-descriptor (e.g. eventfd or pipe) so orphaned children can detect parent death via `PR_SET_PDEATHSIG` or poll.

**Where**: Daemon signal handling, tool/shell spawn, systemd unit design.

---

## S9 — `auto_approve_confirms` on SessionConfig → driver bypasses tool activation gate

**Severity: HIGH**

### Evidence

- `session/types.rs:68-71` — `pub auto_approve_confirms: bool` — default `false`, but configurable per session.
- `session/actor.rs:975` — `self.config.auto_approve_confirms` is passed directly to `run_stream_with_messages`.
- `runtime/stream.rs:28-41` — `activation_policy()`: when `auto_approve_confirms = true`, returns `(ModelConfirmed, false)` — ALL tool activations are auto-approved, no prompt raised.
- `docs/daemon-mode.md:164-166` — "Anyone who can open the socket can `Attach::Create` with any `SessionConfig` (`prompt_manifest`, `auto_approve_confirms`, `persist:false`)."
- **INFERENCE**: A driver-armed session with `auto_approve_confirms = true` will auto-approve every tool activation the model requests. Combined with the driver's automatic turn continuation, this creates a fully autonomous agent with no human gates.

### Impact

- The model can activate any tool (bash, file write, network) without human confirmation
- Combined with S5 (no detach revocation), this runs indefinitely with full tool access
- A same-uid attacker (S6) can create sessions with this flag set

### Fix

**Must-have**: When a driver is armed, `auto_approve_confirms` MUST be overridden to `false` regardless of session config. The driver spec explicitly says "ordinary tool approval [...] gates still apply" (`session-drivers.md:25-26`).

**Should-have**: `auto_approve_confirms` should be a daemon-level config, not a per-session wire field. Remove it from `SessionConfig` and read it only from the host config.

**Where**: `session/actor.rs` (override before `start_turn` when driver is armed), `session/types.rs`.

---

## S10 — Plugin process-memory resurrection gap across daemon reload

**Severity: MEDIUM**

### Evidence

- `session/actor.rs:1884-1914` — `checkpoint()` cancels turns, answers prompts `None`, saves, and closes PTYs. It does NOT explicitly revoke a driver grant (because the TUI's `SessionDriver` struct doesn't exist in the actor yet).
- `docs/extensions/session-drivers.md:206-208` — "Process restart must never restore an active run automatically. The reference plugin retains run state only in process memory."
- `main.py:337-339` — On `initialize()`: `self.run = None; self.cache.clear(); self.last_stop = "No active run; initialization/restart never resumes a run."`
- **INFERENCE**: On daemon reload (`synaps daemon reload`), the extension manager does `shutdown_all` and re-discovers. The plugin process is killed and restarted. Its `self.run = None` on `initialize()`. BUT: the actor's in-memory driver state (if ported) would survive the reload — the `Grant` struct would still exist in the actor, pointing to a handler that was unloaded/reloaded. The handler-generation check would catch this (session_driver.rs tui:495-500), but there's a TOCTOU window.

### Impact

- Low probability: the handler-generation check is correct for the TUI case
- In the actor, the reload path must explicitly revoke all driver grants as part of checkpoint

### Fix

**Must-have**: The actor's `checkpoint()` must revoke any armed driver grant before closing PTYs.

**Where**: `session/actor.rs` `checkpoint()`.

---

## S11 — Context-continuation archive path lacks per-session isolation

**Severity: MEDIUM**

### Evidence

- `extensions/session_driver.rs:345-349` — `apply_context_mode(&self, runtime)` calls `runtime.context_management_command("auto")` — this enables archival of session content.
- `docs/extensions/session-drivers.md:49-54` — "Auto authorizes local archival of eligible session content [...] Archives exclude system/developer prompts, private reasoning, restricted/sensitive content and binary blocks."
- **INFERENCE**: The archive path is file-system based. In daemon mode with N sessions, multiple sessions may archive simultaneously. If the archive directory is shared (same user's data dir), session A's archives are readable by session B when context-continuation loads them. The screening/redaction is described as "not a universal secret detector" (`session-drivers.md:52`).

### Impact

- Cross-session information leakage through shared context archives
- Sensitive content from one session influencing another session's context
- Partial redaction means secrets may survive into archives

### Fix

**Should-have**: Archives should be session-scoped (keyed by session_id). Context continuation should only load archives from the same session chain, not from sibling sessions.

**Where**: Context-continuation archive read/write paths.

---

## S12 — `memory_user_scope` crosses session boundary

**Severity: MEDIUM**

### Evidence

- `tools/memory.rs:39-51` — `MemoryScope` enum has `Repository` and `User`. `User` scope accesses "user-wide notes" — shared across all sessions for the same user.
- `tools/memory.rs:88` — "user accesses user-wide notes only with host opt-in memory.user_scope = true".
- **INFERENCE**: When a driver-armed session runs tools that access `memory_user_scope`, it can read and write memories that other sessions (including non-driver sessions) depend on. A driver running at `xhigh` effort could flood the user-wide memory with generated content.

### Impact

- Cross-session state pollution via shared memory
- A runaway driver could fill user-scoped memory with garbage
- No per-session memory write rate limit

### Fix

**Should-have**: Rate-limit memory writes per session. Consider a read-only memory mode for driver-armed sessions unless explicitly opted in.

**Where**: `tools/memory.rs`, driver configuration.

---

## S13 — No audit log for grant/decision/spend outside the session journal

**Severity: MEDIUM**

### Evidence

- **FACT**: The session journal (`conv.save()`) records `api_messages`, cost, and tokens — but not the driver grant metadata (who authorized, when, which models, which plugin, each decision_id and its outcome).
- **FACT**: The `PollRequest` and its response are transient — they exist only in memory and in the plugin's process memory.
- **INFERENCE**: If a driver runs for 2 hours, makes 200 paid model calls, and then the session journal is corrupted or the daemon crashes, there is NO external record of what was authorized, what was spent, or what decisions were made.

### Impact

- No forensic trail for incident response
- No way to verify after the fact whether a run was legitimately authorized
- Enterprise compliance failure (SOC2, audit requirements)

### Fix

**Must-have**: Append-only audit log for driver events, written to a separate file or structured log:
```
{ts, session_id, event: "grant_start", plugin_id, run_id, models, max_duration_ms, context_mode}
{ts, session_id, event: "decision", decision_id, outcome, error_kind, model, effort, cost_so_far}
{ts, session_id, event: "grant_revoke", reason, total_cost, turns}
```

**Where**: New audit module, called from the actor's driver lifecycle hooks.

---

## S14 — Grant deadline is Instant-based — SIGSTOP stalls it

**Severity: LOW**

### Evidence

- `extensions/session_driver.rs:314-318` — `deadline = max_duration_ms.map(|ms| Instant::now().checked_add(Duration::from_millis(ms)))`.
- `Instant` is monotonic — NTP jumps don't affect it. But `SIGSTOP` freezes the process and `Instant::now()` doesn't advance during the stop.
- **INFERENCE**: A `kill -STOP <daemon_pid>; sleep 3600; kill -CONT` would give the driver an extra hour of real-world time.

### Impact

- Minor: requires root or same-uid to send SIGSTOP
- The deadline could expire much later in wall-clock time than intended

### Fix

**Low priority**: Document that `max_duration_ms` is process-monotonic, not wall-clock. For enterprise, consider a secondary wall-clock check (compare `SystemTime` at grant creation vs now, with tolerance for clock adjustments).

---

## S15 — Handler generation check is point-in-time, not atomic

**Severity: LOW**

### Evidence

- `docs/extensions/session-drivers.md:94-95` — "Pin a live lifecycle generation [...] This is a point-in-time observation, not an atomic process-exit/request lease."
- `tui/session_driver.rs:495-500` — `same_lifecycle()` checks `live_generation(handler)? == generation`. Between this check and the next `invoke_command`, the plugin could restart.
- **INFERENCE**: This is a documented limitation. The window is small (microseconds). The consequence is a poll delivered to a restarted plugin, which would reject it (unknown run_id) — fail-safe.

### Impact

- Theoretical: a poll could reach a restarted plugin that happens to accept it (only if the plugin is stateful and the run_id collides). The reference plugin resets state on initialize, so this is safe.

### Fix

**Low priority**: Acknowledged and documented. No practical exploit path with the reference plugin.

---

## Enterprise table-stakes assessment (for Praxis prod deployment)

| Requirement | Status | Blocker? |
|-------------|--------|----------|
| Per-session cost cap | ❌ Missing | **Yes** |
| Per-daemon cost cap | ❌ Missing | **Yes** |
| Hard-stop on budget | ❌ Missing | **Yes** |
| Audit log (grant/decision/spend) | ❌ Missing | **Yes** |
| SO_PEERCRED uid verification | ❌ T11 pending | **Yes** |
| Driver revocation on detach | ❌ Missing | **Yes** |
| Prompt{Confirm} with zero clients | ❌ Undefined behavior | **Yes** |
| Cross-session driver isolation | ❌ Shared ExtensionManager | **Yes** |
| Client env isolation (T1) | ❌ Pending | No (ship without headless driver) |
| SIGKILL orphan cleanup | ⚠️ Partial (F8) | No (operational, not code) |
| Context archive isolation | ⚠️ Unverified | No (low probability) |

---

## Open decisions for Haseeb

| # | Decision | Options | Recommendation |
|---|----------|---------|----------------|
| D1 | Should the driver revoke on last-client-detach? | (a) Always revoke (conservative), (b) Configurable headless grace, (c) Allow headless with explicit flag | (a) for first release; (b) later |
| D2 | How to handle Prompt{Confirm} with zero clients? | (a) Auto-deny (send None), (b) Revoke driver, (c) Buffer with timeout | (a) — fail-closed, matches existing semantics |
| D3 | Should `auto_approve_confirms` be blocked when driver is armed? | (a) Hard override to false, (b) Allow but warn, (c) Config-level only | (a) — the spec says tool gates remain |
| D4 | Should PollRequest include session_id? | (a) Add to protocol, (b) Actor wraps handler per-session, (c) Both | (c) — belt and suspenders |
| D5 | Should daemon ship with driver support before cost caps exist? | (a) Ship without driver in actor, (b) Ship with driver + cost caps, (c) Ship with driver + kill-switch env | (c) — `SYNAPS_DAEMON_DRIVER=0` default off |
| D6 | Should `synaps send` to a driver-armed session be rejected? | (a) Reject, (b) Queue and revoke driver, (c) Allow (current TUI behavior) | (b) — the TUI revokes on queued work |

---

## Size estimates per work item

| Work item | Size | Hours | Dependencies |
|-----------|------|-------|-------------|
| S1: Prompt{Confirm} zero-client gate | S | 2-4h | Actor driver port |
| S2: Session-scoped PollRequest + plugin multi-run | M | 4-8h | Plugin protocol change |
| S3: Per-session cost cap (host-enforced) | M | 6-10h | None |
| S3: Per-daemon cost cap | M | 4-6h | Per-session cap |
| S4: Driver-aware event queue gate | S | 2-4h | Actor driver port |
| S5: Detach-revocation gate | S | 2-3h | Actor driver port |
| S6: SO_PEERCRED (T11) | M | 4-8h | None |
| S7: Daemon env credential stripping | S | 2-4h | T1 design |
| S8: SIGKILL process-group cleanup | M | 4-8h | Systemd/process design |
| S9: auto_approve_confirms override | S | 1-2h | Actor driver port |
| S10: Checkpoint revokes driver | S | 1h | Actor driver port |
| S11: Archive session isolation | S | 2-4h | Context-continuation code |
| S12: Memory write rate limit | S | 2-4h | None |
| S13: Audit log module | M | 6-10h | None |

**Total critical path (S1+S2+S3+S5+S9)**: ~L — 20-30h before the driver can safely live in the actor.

---

## Risk ranking (final)

1. **CRITICAL — S3 (no spend cap)**: Every other finding is amplified by the absence of cost limits. This is the foundational guard rail.
2. **CRITICAL — S1 (zombie prompt)**: A driver-armed session with pending prompts and no clients is an unrecoverable money pit.
3. **CRITICAL — S2 (shared ExtensionManager)**: Two sessions arming the same plugin is a protocol-level design gap, not a bug to fix later.
4. **HIGH — S5 (detach ≠ revoke)**: The most likely real-world scenario (user closes laptop, driver keeps spending).
5. **HIGH — S9 (auto_approve_confirms)**: Combined with S5, this is full autonomous execution with no human gates.
6. **HIGH — S4 (synaps send injection)**: A same-uid attacker can hijack a driver session.
7. **HIGH — S6 (no SO_PEERCRED)**: Broad attack surface, but requires same-uid access.
8. **HIGH — S7 (frozen env)**: Operational hazard, not an active exploit.
9. **HIGH — S8 (SIGKILL orphans)**: Operational, mitigable with process groups.
10. **MEDIUM — S10-S13**: Important for hardening, not blocking for first daemon driver release (if S1-S9 are addressed).

---

*FACT/INFERENCE/UNKNOWN classification is inline per finding. All file:line references are to the read-only checkouts specified in COMMON.md.*
