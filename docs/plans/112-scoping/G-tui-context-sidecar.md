# G — TUI delta scoping: context-continuation UX, sidecar UI, settings/thinking, lifecycle

Everything in `crates/agent-tui/src/tui/*` that is NOT the session-driver
module or the multimodal-attachment engine. Scope: the non-driver TUI delta
between `origin/dev` and `origin/feat/context-continuation`.

**Governing constraint (COMMON.md, daemon-mode.md):** On dev the TUI is a thin
client of a `SessionActor` over `SessionCommand`/`SessionEventWire`. Upstream's
TUI drives `Runtime` directly. Every upstream hunk must be re-expressed as either
(a) actor-side (`SessionCommand`/`SessionEventWire` addition, `actor.rs` handling)
or (b) render-only in the TUI. Moving upstream code into `app.rs` and calling
`runtime.*` is not an option — the runtime is not in the TUI process under the
daemon.

---

## 1  Context-head UX

### 1.1  What upstream does

On upstream the TUI owns the `ContextHeadPersistence` state, intercepts
`StreamEvent::Session(SessionEvent::ContextHeadCheckpoint)` directly, calls
`app.persist_context_head()` (filesystem I/O), completes the receipt oneshot,
and blocks inference when the head is poisoned:

| Symbol | upstream location | What it does |
|--------|-------------------|--------------|
| `App.context_head` | `app.rs:+83` (upstream) | `ContextHeadPersistence` field on the TUI struct |
| `persist_context_head` | `stream_handler.rs:+88..+102` (upstream) | Async save + receipt completion in the stream-event arm |
| `is_blocked` guard | `stream_handler.rs:+107`, `+179` (upstream) | Blocks `MessageHistory` and post-Done work when head is poisoned |
| `response_preview` | `app.rs:+37` (upstream) | `Option<(usize, Option<ChatMessage>)>` for `ResponseReset` rollback |
| `ResponseStart` arm | `stream_handler.rs:+41..+48` (upstream) | Captures transcript index + last msg for preview rollback |
| `ResponseReset` arm | `stream_handler.rs:+49..+54` (upstream) | Calls `transcript.reset_response_preview(start, last)` |

### 1.2  What dev already has (actor-side)

Phase 9 (Wall 1, commit da74f413) landed the actor-owned checkpoint:

| Symbol | dev location | What it does |
|--------|--------------|--------------|
| `handle_context_head_checkpoint` | `actor.rs:680–712` | Actor intercepts `ContextHeadCheckpoint`, calls `conv.persist_context_head`, completes receipt |
| `conv.context_head` | `actor.rs:1075` | `is_blocked` guard on the actor's `Done` path |
| `ContextHeadPersistence::default()` reset | `actor.rs:1561` | Cleared on `LinkedSuccessor` (compaction id change) |
| stream_handler no-op arms | `stream_handler.rs:192` | `ResponseStart \| ResponseReset => {}` — **phase 2 left these as no-ops** |
| `ContextHeadCheckpoint => {}` | `stream_handler.rs:193` | TUI-side no-op — actor handles it |

### 1.3  What still needs to happen

**`ResponseStart`/`ResponseReset` rendering (render-only, no actor change).**
The no-op at `stream_handler.rs:192` must implement the transcript rollback so
the user sees the model's reset on context rollover. This is pure TUI state:

- On `ResponseStart`: snapshot `(transcript.messages().len(), last_msg)` into a
  field on `App` (or a local in the handler).
- On `ResponseReset`: call a `transcript.reset_response_preview(start, last)`
  method that truncates back to the snapshot.

The `TranscriptStore` does not have `reset_response_preview` on dev. Upstream's
implementation was on an `app.rs` field (`response_preview`) — but the
transcript store is the correct home (it owns the message vec). Porting the
method into `TranscriptStore` is render-only: no actor, no wire changes.

**`/context status|auto|off` command surface.**  Upstream routes these through
`handle_engine_command` on the local `Runtime`. On dev, engine commands are
already routed as `SessionCommand::EngineCommand { name, arg }` → actor →
`QueryResult`. **No new command is needed**; `/context` already exists in
`help.json` and the engine command dispatch. Confirm the actor-side handler
responds to `context` with the right output. **FACT:** `help.json` on dev at
`crates/agent-engine/assets/help.json` already has the `context-command` entry
(existing on both branches). Upstream adds a `budget-command` entry (+21 lines,
`help.json:+561`).

**Blocked-inference notice.** On dev the actor emits `SystemNotice` when the
head is blocked (`actor.rs:1075`). The TUI already renders `SystemNotice` as a
`ChatMessage::System`. No additional event type needed — the existing
`SystemNotice` path covers it. **Verify** the notice text matches upstream's
user-facing message ("Context rollover deferred" or similar).

**Durability-latch recovery surface.** Upstream had `context_head_recovery_*`
functions and a `blocked_app` concept for the TUI. On dev, recovery is actor-side:
the actor owns the blocked state and unblocks on successful save or `/context off`.
The TUI needs no recovery surface beyond what `SystemNotice` provides.

### 1.4  Disposition table

| upstream symbol | Lines | Disposition |
|-----------------|-------|-------------|
| `App.context_head` | ~5 | **deliberate-skip**: actor owns this on dev (`actor.rs:1075,1561`) |
| `App.response_preview` | ~5 | **port-as-render**: new field on App/TranscriptStore |
| `persist_context_head` (stream_handler) | ~15 | **already-on-dev-as** `actor.rs:680–712` |
| `is_blocked` guards (stream_handler) | ~10 | **already-on-dev-as** `actor.rs:1075` |
| `ResponseStart` arm | ~8 | **port-as-render**: fill the no-op at `stream_handler.rs:192` |
| `ResponseReset` arm | ~6 | **port-as-render**: fill the no-op at `stream_handler.rs:192` |
| `reset_response_preview` | ~20 | **port-as-render**: new method on `TranscriptStore` |
| `/context` command routing | 0 | **already-on-dev-as** `EngineCommand` |
| `/budget` command + help.json | ~21 | **port-as-render**: add help.json entry; routing exists via `EngineCommand` |
| `cap_resumed_display` | ~5 | **already-on-dev-as** `display::display_tail` (`helpers.rs` uses it) |
| `capture_abort_context` | ~10 | **already-on-dev-as** `Aborted { context_saved }` envelope |

---

## 2  Sidecar UI

### 2.1  Diff summary

`sidecar.rs`: dev 511 lines → upstream 1,235 lines (+727 upstream-only, −3 dev-only).
The delta is almost entirely additive. It corresponds to "Phase 7 slice F —
plugin self-config" described in the task brief.

### 2.2  Inventory

| upstream symbol | upstream lines | What it does | Disposition |
|-----------------|----------------|--------------|-------------|
| `SidecarStartup` struct | ~30 | Owned `JoinHandle<Result<SidecarUiState>>` + `DiscoveredSidecar` + label; `Drop` aborts task | **needs-decision** (daemon-side or TUI-side? see §2.3) |
| `SidecarStartup::start()` | ~35 | Spawns async task: acquires manager lock, queries `sidecar_spawn_args`, spawns `SidecarManager`, waits for `ready_after_init` | **needs-decision** |
| `spawn_args_unsupported()` | ~8 | Matches legacy unsupported-method error strings for fallback | port alongside `SidecarStartup` |
| `toggle()` | ~60 | Non-blocking toggle: checks `sidecars_disabled`, finds target, drains events, creates/removes `SidecarStartup`, press/release | **needs-decision** |
| `next_startup()` | ~20 | `select_all` across pending `SidecarStartup` JoinHandles — polled in the event loop | **needs-decision** |
| `finish_startup()` | ~30 | Completes a startup: removes pending, validates plugin still enabled, inserts state | **needs-decision** |
| `drain_events()` | ~20 | Bounded (64) poll of `manager.try_next_event()` per sidecar | **port-as-render** (events are local UI state) |
| `retain_enabled()` | ~8 | Prunes `sidecar_starts`/`sidecars` against enabled registry | **port-as-render** |
| `status()` | ~25 | Status string for `/sidecar status` command | **port-as-render** |
| `SidecarUiStatus::Loading` | ~5 | New variant in the enum | **port-as-render** |
| `ready_after_init` wait loop | ~24 | Waits for `status:ready` after `Init` before considering sidecar alive | **needs-decision** |
| `next_event()` | ~20 | `select_all` across live sidecar event streams | **port-as-render** (replaces inline `select_all` in dev's `mod.rs` loop) |
| `handle_event` loading/initializing arm | ~6 | Sets `Loading` status for pre-init states | **port-as-render** |
| `App.sidecar_starts` | ~3 | `HashMap<String, SidecarStartup>` field on App | **needs-decision** |
| `App.sidecars_disabled` | ~1 | Bool flag on App (from `--no-extensions`) | **port-as-render** |
| Test module `async_startup` | ~300 | Comprehensive tests: fixture(), registry(), toggle-before-ready, delayed completion, disable cancellation, etc. | port with feature |
| `docs/sidecar-protocol.md` delta | +41 | Documents `ready_after_init`, non-blocking TUI startup, timeouts | **port-as-docs** |

### 2.3  Daemon architecture tension

**Core question:** Under the daemon, sidecars are daemon-side processes managed
by `SidecarManager` (which lives in the engine's `sidecar/manager.rs`). Upstream's
code places `SidecarStartup`, `SidecarUiState`, and the full lifecycle in the
TUI process because upstream has no daemon.

On dev, `SidecarManager` init-write timeout was already updated in PR #120 (the
engine half). The TUI-side sidecar state (`SidecarUiState`, `sidecars` HashMap,
draw code) is render state — but the process spawn/lifecycle/toggle is mixed:

- **Process management** (spawn, stdin/stdout pipes, `SidecarManager::spawn`,
  `press()`/`release()`, `shutdown()`) → should be actor-side under the daemon.
  The actor already has access to the `SidecarManager` via the engine.
- **UI state** (`SidecarUiStatus`, `display_name`, draw code, event cards,
  `InsertText`) → stays in the TUI.

**However:** the engine's `sidecar/manager.rs` was updated in #120 but does NOT
currently expose sidecar lifecycle as `SessionCommand`/`SessionEventWire`. No
`SidecarToggle` command or `SidecarEvent` wire event exists on dev.

**Recommendation:** Port upstream's sidecar code as **render-only for the in-process
path** (`TransportMode::Local`). The socket/daemon path either (a) disables
sidecars (today's state — the thin client has no sidecar rendering) or
(b) requires future `SessionCommand::SidecarToggle`/`SessionEventWire::SidecarUpdate`.
This keeps the merge simple and defers the daemon sidecar protocol to a
dedicated spec. Upstream's code is correct for in-process; the daemon question is
separate.

---

## 3  Settings / thinking dispatch

### 3.1  Diff summary

`settings/defs.rs`: 113 upstream-only lines, 134 dev-only lines. The structural
change is the dispatch architecture:

| Aspect | upstream | dev |
|--------|----------|-----|
| Dispatch signature | `fn(Runtime, App, &str) -> Result<(), String>` | `fn(App, &str) -> SettingApply` |
| `SettingApply` enum | **Does not exist** — direct mutation | `Session(SessionSetting)` or `Local(Result)` |
| Runtime mutation | `runtime.set_model(…)`, `runtime.set_reasoning_level_checked(…)` | `SessionSetting::Model`, `SessionSetting::ReasoningLevel` → actor |
| Validation | Local on `Runtime` | Client pre-check + actor re-validates |

`settings/mod.rs`: 11 upstream-only, 18 dev-only. Changes `RuntimeRead` trait
bound to concrete `synaps_cli::Runtime`; visibility changes (`pub(crate)` →
`pub(super)`).

### 3.2  Disposition

| upstream symbol | Lines | Disposition |
|-----------------|-------|-------------|
| `apply_setting_dispatch` (4-arg) | ~40 | **deliberate-skip**: dev's 3-arg `SettingApply` architecture is correct for daemon; upstream's direct-mutation is incompatible |
| `restore_session_reasoning()` | ~12 (`commands.rs:+374..+385`) | **port-as-actor-cmd**: reasoning-level clamping on resume. Dev's `Resume` command already handles this via `Resumed { clamp_notice }` wire event (`types.rs`). Upstream's standalone fn is superseded |
| `resume_clamps_unsupported_saved_level` logic | implicit | **already-on-dev-as** `Resumed.clamp_notice` |
| `thinking_dispatch_*` pattern | macro body | **deliberate-skip**: dev's `SettingApply::Session(SessionSetting::ReasoningLevel)` is the correct abstraction |
| `RuntimeSnapshot::from_runtime(Runtime)` | ~4 | **deliberate-skip**: dev uses `RuntimeRead` trait (more general) |
| `Focus`/`ActiveEditor`/`SettingsState` visibility changes | ~6 | **deliberate-skip**: dev's `pub(crate)` is intentional (testing module needs access) |

**Net:** No settings/thinking code needs porting. Dev's `SettingApply` architecture
is strictly more correct under the daemon. Upstream's direct-mutation pattern would
break every socket-transport session.

---

## 4  Lifecycle / observability

### 4.1  `lifecycle.rs` delta

| upstream symbol | upstream lines | What it does | Disposition |
|-----------------|----------------|--------------|-------------|
| `flush_observability(runtime)` | ~10 | Async bounded flush of telemetry writer via `runtime.shutdown_observability_async()` | **deliberate-skip**: on dev the actor owns observability flush inside `SessionActor::finish` (`actor.rs`); TUI never calls `runtime.*` |
| `emergency_flush_and_exit(runtime)` | ~10 | 1s flush + `emergency_teardown_terminal` + `exit(1)` | **deliberate-skip**: dev's `emergency_exit()` (`lifecycle.rs:98–102`) calls `EngineHost::flush_installed_logs()` + teardown + exit — actor-side flush |
| `flush_observability_within(runtime, budget)` | ~12 | Internal helper | **deliberate-skip**: actor-owned |

### 4.2  `signals.rs` delta

| upstream symbol | upstream lines (removed from dev) | What it does on dev | Disposition |
|-----------------|-----------------------------------|---------------------|-------------|
| `SignalBackend` enum | dev-only ~25 | `Thread`/`Tokio` backend selection for socket vs in-process | **keep-dev**: daemon socket client needs `Tokio` backend |
| `spawn_shutdown_signal_task_with()` | dev-only ~35 | Backend-aware signal listener | **keep-dev** |
| `tokio_backend_delivers_sigterm` test | dev-only ~25 | Validates tokio backend | **keep-dev** |
| `backend_selection` test | dev-only ~6 | Unit test | **keep-dev** |
| `budget_tests` module | dev-only ~15 | `session_end_wait_covers_actor_finish_worst_case` | **keep-dev** |
| `SESSION_END_TIMEOUT_SECS` | dev-only ~8 | Computed from actor budgets | **keep-dev** |
| Budgets imported from `agent_engine::session::budgets` | dev-only ~5 | `SAVE_TIMEOUT_SECS`, `HOOKS_TIMEOUT_SECS`, `TEARDOWN_TIMEOUT_SECS` | **keep-dev** |

Upstream's `signals.rs` removes all daemon-aware signal infrastructure. Dev's version
is strictly more capable. **No porting needed; all upstream removals are deliberate-skip.**

### 4.3  `run_setup.rs` delta

| upstream symbol | upstream lines | What it does | Disposition |
|-----------------|----------------|--------------|-------------|
| `RunContext` struct (upstream version) | ~25 | `runtime`, `stream`, `cancel_token`, `steer_tx`, `background`, `ext_mgr_shared` (non-Optional) | **deliberate-skip**: dev's `RunContext` has `link`, `http`, `prompt_bridge`, `mode`, `ext_mgr_shared` (Optional) — actor architecture |
| `TransportMode` enum | dev-only ~12 | `Local { host }` / `Socket` | **keep-dev** |
| `LazyHttp` struct | dev-only ~40 | Lazy HTTP client for catalog fetches | **keep-dev** |
| `push_boot_notice()` / `take_boot_notices()` | dev-only ~12 | Queued notices for `--attach` fallback | **keep-dev** |
| `scrollback_from_env()` | dev-only ~20 | Scrollback cap for socket path | **keep-dev** |
| `session_from_header()` / `apply_header()` | dev-only ~30 | Build `Session` from `SessionHeader` | **keep-dev** |
| `CompactionApplied` / `ResumePending` / `DaemonLostInfo` | dev-only ~25 | Deferred presentation state | **keep-dev** |

**Upstream's `run_setup` is a complete rewrite** that calls
`synaps_cli::engine::setup::boot()` (no daemon, no actor). Dev's `run_setup`
builds `EngineHost::boot_and_install()`, creates a session on the actor, attaches
via `LocalTransport`. **No porting — dev's architecture is the target.**

### 4.4  `mod.rs` delta

563 diff lines. Upstream has:
- No `run_loop` split (dev separates `run()` → `run_setup()` + `run_loop()` so
  the socket `attach` path shares the loop)
- Direct `stream: Pin<Box<dyn Stream>>` polling
- `driver_timer` (50ms interval for session_driver ticks)
- `compact_task` polling inlined in the tick arm
- `sidecar::next_startup()` arm
- No `idle_purge`, no `client_diet`, no `session_link`, no `prompt_bridge`

Dev has:
- `run_loop(ctx)` shared between `run()` and `run_attached()`
- `link.recv()` for session events (envelope-based)
- `idle_purge`, `client_diet`, `quit_guard`
- `sidecar_event` arm with inline `select_all`
- `prompt_bridge.answers_rx` arm
- Separate `handle_session_event_arm` for envelopes

**Disposition:** Upstream's `mod.rs` is the **most structurally incompatible** file.
Every select arm must be re-expressed through the `SessionLink`/envelope
architecture. The compaction poll → `CompactionStarted`/`Applied`/`Failed`
envelopes (already on dev). The stream poll → `Stream(ev)` envelopes. The
driver timer → future driver-actor protocol. **Nothing from upstream's `mod.rs`
ports directly. The sidecar startup arm is the one net addition needing design.**

### 4.5  `loop_arms.rs` delta

93 upstream-only, 27 dev-only. Changes:
- `handle_widget_event`: `pub(crate)` → `fn` (visibility, upstream). **keep-dev** (test access).
- `handle_extension_loader_event`: `RuntimeView` → `Runtime`; `Option<&ExtMgr>` →
  `&ExtMgr`. **keep-dev** (daemon: `ext_mgr` is `None` on socket path).
- `handle_animation_tick`: gains `runtime` arg for `subagent_registry()` lock.
  **keep-dev** (dev reads `SubagentRows` from cached envelope, not lock).
- Compaction poll (~70 lines): upstream polls `compact_task` JoinHandle inline.
  **already-on-dev-as** `CompactionApplied`/`CompactionFailed`/`CompactionCancelled`
  envelopes handled in `handle_session_event_arm`.

### 4.6  Other files

| File | upstream +/− | Disposition |
|------|--------------|-------------|
| `highlight.rs` | +16, −242 | **keep-dev**: upstream removes curated dump, idle eviction, `SyntaxCache` — all daemon client-diet features. Dev's version is strictly superior |
| `testing.rs` | +12, −128 | **keep-dev**: upstream removes `ScriptedTransport`, `feed_events`, `set_history`, `sent_commands`, `activate_prompt_with_kind`, `activate_confirm_prompt` — all envelope-driven test infra. Dev's version is the daemon test harness |
| `draw.rs` | +22, −64 | Mixed. `SidecarUiStatus::Loading` pill colour/text → **port-as-render** (~10 lines). `RuntimeRead` → `Runtime` → **keep-dev**. Confirm-prompt modal simplification (removes `PromptKind` dispatch, `wrapped_line_count`) → **keep-dev** (dev has the richer modal) |
| `transcript.rs` | +50, −367 | **keep-dev**: upstream removes scrollback cap (`set_scrollback`, `enforce_scrollback`, `drain_front`, `scrollback_dropped`, `SCROLLBACK_*` consts), `ChatMessage::approx_len()`. All daemon client-diet features |
| `dispatch.rs` | +488 upstream, −403 dev | **keep-dev**: upstream's dispatch calls `runtime.*` directly; dev's calls `link.send(SessionCommand::*)`. Architecturally incompatible |
| `commands.rs` | +384 upstream, −241 dev | Mixed. Attachment commands → **separate scope** (B-driver-port). `/budget` → **port-as-render** (routed via `EngineCommand`). `apply_interactive_command_result` refactor → **port-as-render** (~15 lines). `restore_session_reasoning` → **deliberate-skip** (actor-side via `Resumed`) |
| `input.rs` | +76, −43 | Mixed. Session-driver Esc/Ctrl-C override → **separate scope**. Attachment Enter guard → **separate scope**. `GrantWorkerModel` removal → **keep-dev** (dev needs it). `/` splitn whitespace → **port-as-render** (~1 line bugfix) |
| `helpers.rs` | +115, −279 | `apply_setting` async→sync + `SettingApply` removal → **keep-dev**. `rebuild_display_messages` inline JSON walk → **keep-dev** (dev uses `display::display_tail`). `DISPLAY_TAIL_ITEMS` removal → **keep-dev** |
| `session_driver/` (full module) | +1,811 | **Separate scope** (B-driver-port.md, E-driver-port-spec.md). Not ported in this area |

---

## 5  Docs and tests

| Path | upstream +/− | Disposition |
|------|--------------|-------------|
| `help.json` `/budget` entry | +21 | **port-as-render**: pure content addition, no code |
| `AGENTS.md` build worker ceiling | +6 | **port-as-docs**: `.cargo/config.toml` reference; applies to both |
| `AGENTS.md` Axel memory docs | +15 | **port-as-docs**: documents `memory.backend = axel`; engine half already landed |
| `AGENTS.md` context management docs | +15 | **port-as-docs**: documents `/context auto`; engine half already landed |
| `docs/extensions/contract.json` permission reorder | ~20 | **port-as-docs**: cosmetic sort; no semantic change |
| `docs/extensions/contract.json` `session_driver` section | +47 | **separate scope**: driver contract (E-driver-port-spec.md) |
| `docs/sidecar-protocol.md` | +41 | **port-as-docs**: `ready_after_init`, non-blocking startup docs |
| `tests/turn_budget_stream.rs` `wall_clock_exhaustion_…` | +56 | **port-as-engine-test**: tests engine-level budget recovery; no TUI dep. Lands with `/budget` command |
| `crates/agent-tui/tests/harness_session_events.rs` | dev-only | **keep-dev**: envelope-driven harness tests |
| `crates/agent-tui/tests/highlight_curated.rs` | dev-only | **keep-dev**: curated syntax dump golden tests |
| `crates/agent-tui/tests/highlight_mem.rs` | dev-only | **keep-dev**: jemalloc heap measurement for idle eviction |
| Highlight fixture deletions (14 `.txt` files) | upstream deletes | **keep-dev**: fixtures for curated dump golden tests |
| `harness_scenarios.rs` delta | upstream simplifies | **keep-dev**: dev has richer harness surface |

---

## 6  Actor-vs-render split — required wire additions

Across all five areas, the following `SessionCommand`/`SessionEventWire`
additions are needed:

| Need | Wire type | Direction | Why |
|------|-----------|-----------|-----|
| **None for context-head** | — | — | Actor already handles `ContextHeadCheckpoint` (phase 9). `SystemNotice` covers blocked state. `ResponseReset` render is TUI-local |
| **None for `/context`, `/budget`** | — | — | Already routed as `EngineCommand` → `QueryResult` |
| Sidecar toggle (future) | `SessionCommand::SidecarToggle { plugin_id }` | C→S | Only needed when sidecars run daemon-side; in-process path is TUI-local |
| Sidecar lifecycle event (future) | `SessionEventWire::SidecarLifecycle { plugin_id, event }` | S→C | Ditto |
| **None for settings** | — | — | `SessionSetting` + `SettingApply` already covers all keys |
| **None for lifecycle** | — | — | Actor owns flush/save/hooks; `Ended` covers teardown |

**Net: zero wire additions required for immediate porting.** The sidecar daemon
protocol is deferred until sidecars move to the actor.

---

## 7  Phase plan

| # | Size | Hours | What | Tests | DARK story |
|---|------|-------|------|-------|------------|
| G1 | S | 2 | `ResponseStart`/`ResponseReset` render arms in `stream_handler.rs:192` + `reset_response_preview` on `TranscriptStore` | Unit test: push msgs → `ResponseStart` → push more → `ResponseReset` → verify truncation. Harness test via `feed_events` with a `Stream(ResponseReset)` envelope | Always active (render code). No dark flag needed — the engine only sends these events when `/context auto` is enabled, which is already dark (`context_management.mode = off`) |
| G2 | S | 1 | `SidecarUiStatus::Loading` variant + draw code (pill colour/text) in `draw.rs` | Existing sidecar pill tests + new `Loading` variant case | Render-only. Visible only when sidecar protocol v2 negotiates `ready_after_init` |
| G3 | M | 4 | Sidecar non-blocking startup: `SidecarStartup`, `toggle()`, `next_startup()`, `finish_startup()`, `drain_events()`, `retain_enabled()`, `status()`. Port into `sidecar.rs`. Wire `next_startup` arm into `mod.rs` event loop (in-process path only) | Port upstream's `async_startup` test module. Requires Unix (Python fixture sidecar). Add harness test for `sidecars_disabled` guard | In-process only. Socket path skips the arm (`app.sidecar_starts.is_empty()` is always true). Dark until a plugin opts into `ready_after_init` |
| G4 | S | 1 | `help.json` `/budget` entry + `AGENTS.md` doc additions (Axel memory, context management, build worker ceiling) | `export_pretty_matches_committed_docs_tools_json` drift check auto-catches help.json | Content-only |
| G5 | S | 1 | `docs/sidecar-protocol.md` + `docs/extensions/contract.json` permission reorder | Contract drift test (`contract_json_matches_rust_hook_and_permission_catalogs`) | Docs-only |
| G6 | S | 2 | `tests/turn_budget_stream.rs` new test `wall_clock_exhaustion_can_resume_retained_history_after_explicit_extension` | Self-contained integration test against loopback provider | Engine-only; exercises `/budget time` command |
| G7 | S | 1 | Cleanup: `/` command `splitn(2, char::is_whitespace)` bugfix in `input.rs:476`; `apply_interactive_command_result` refactor in `commands.rs` (extract return value) | Existing command-dispatch tests | Behavioral bugfix (whitespace in slash args) |
| | | **12** | **Total** | | |

### Phase dependencies

```
G1  ──┐
G2  ──┼── G3 (Loading status needed before startup code)
      │
G4  ──┤
G5  ──┤   (independent)
G6  ──┤
G7  ──┘
```

G1 and G2 are prerequisites for G3 (the sidecar startup code references
`SidecarUiStatus::Loading` and the context-head render arms should be in place
before the event loop is touched). G4–G7 are fully independent.

---

## 8  Risks

| # | Risk | Likelihood | Impact | Mitigation |
|---|------|------------|--------|------------|
| R1 | `reset_response_preview` truncation leaves orphaned thinking/tool cards in the transcript | Medium | Low (cosmetic) | Unit test with interleaved tool-use messages before reset; harness golden test |
| R2 | Sidecar `next_startup` arm in the `tokio::select!` interacts badly with the existing inline `select_all` for sidecar events | Medium | Medium (event loop correctness) | Factor both into `sidecar::next_event()` (upstream's design) and the separate `next_startup()`. Test with two sidecars, one pending + one live |
| R3 | `ready_after_init` wait loop blocks the startup task for 30s if the sidecar never sends `state: ready` | Low | Low (timeout handles it) | Timeout is correct; verify the error message surfaces in the TUI |
| R4 | Upstream's `drain_events` bounded loop (64 iterations) may miss events from a noisy sidecar | Low | Low (next toggle retries) | Acceptable; the `Loading` fallback is the correct UX |

---

## 9  Open questions (≤ 3)

1. **Q1 — Sidecar process ownership under daemon:** When sidecars move to the
   actor (future), should `SidecarManager` be per-session or per-daemon?
   Upstream's design is per-session (`App` owns the HashMap). The daemon has one
   `ExtensionManager` across sessions (`crates/agent-engine/src/extensions/manager.rs:19`,
   one `Arc` per daemon). A per-session sidecar means N concurrent sidecar
   processes for N sessions. Decision deferred but affects wire protocol design.
   *Cite: `crates/agent-engine/src/extensions/manager.rs:19` (one `Arc` per daemon),
   `SYNTHESIS.md` driver multi-tenancy §Q3.*

2. **Q2 — `ResponseReset` transcript rollback depth:** Upstream's implementation
   (`stream_handler.rs:+49–54`) rolls back to the `ResponseStart` snapshot
   index. If a `ResponseReset` arrives without a prior `ResponseStart` (edge
   case: actor restart mid-stream), what should the TUI do? Current dev no-op
   is safe. Proposed: guard with `if let Some(snapshot) = …` and silently
   ignore orphaned resets. *Cite: `stream_handler.rs:192` (dev no-op).*

3. **Q3 — `/budget` command routing: confirmed resolved.** The engine command
   handler for `"budget"` already exists at
   `crates/agent-engine/src/engine/commands.rs:199` (`budget_command(arg, runtime)`).
   It is registered in `crates/agent-engine/src/skills/mod.rs:229`
   (`BUILTIN_COMMANDS`). Routing via `SessionCommand::EngineCommand { name:
   "budget", .. }` works with zero wire additions. The only remaining work is
   the `help.json` entry (G4) and the integration test (G6). *Cite:
   `commands.rs:199`, `skills/mod.rs:229`, `types.rs:530`.*
