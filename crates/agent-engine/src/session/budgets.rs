//! Teardown budgets shared by every session host (values copied from
//! `agent-tui/src/tui/signals.rs`; `signals.rs` re-exports these on day 2).
//!
//! - `SAVE_TIMEOUT_SECS`  — session save + index end record (data safety first)
//! - `HOOKS_TIMEOUT_SECS` — `on_session_end` hook emit (concurrent, fail-open)
//! - `TEARDOWN_TIMEOUT_SECS` = their sum.

pub const SAVE_TIMEOUT_SECS: u64 = 2;
pub const HOOKS_TIMEOUT_SECS: u64 = 5;
pub const TEARDOWN_TIMEOUT_SECS: u64 = SAVE_TIMEOUT_SECS + HOOKS_TIMEOUT_SECS;

pub const SAVE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(SAVE_TIMEOUT_SECS);
pub const HOOKS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(HOOKS_TIMEOUT_SECS);

/// `SessionActor::cancel_turn`: how long a cancelled turn's stream is drained
/// for the engine's cancel-path history, final Usage and any in-flight
/// context-head checkpoint. The engine's provider/tool awaits are
/// cancellation-first, so the tail normally arrives in milliseconds; on
/// expiry the actor keeps its last adopted (valid) history. Also the grace a
/// driver-revoked turn gets (`revoked_turn_deadline`).
///
/// One exception, deliberately unbounded: a context-head checkpoint the
/// engine hands over while draining is persisted durably before its receipt
/// is answered (a durability barrier; dropping it would latch the session).
pub const CANCEL_DRAIN_TIMEOUT_MS: u64 = 1_000;
pub const CANCEL_DRAIN_TIMEOUT: std::time::Duration =
    std::time::Duration::from_millis(CANCEL_DRAIN_TIMEOUT_MS);
/// Whole-second ceiling of the drain, for the budget sums below/elsewhere.
pub const CANCEL_DRAIN_TIMEOUT_SECS: u64 = CANCEL_DRAIN_TIMEOUT_MS.div_ceil(1000);
/// Worst case of `cancel_turn`: the drain, then the bounded wait for the
/// interrupted turn's save before it announces `Idle` (`announce_idle`).
pub const CANCEL_TURN_TIMEOUT_SECS: u64 = CANCEL_DRAIN_TIMEOUT_SECS + SAVE_TIMEOUT_SECS;

/// `SessionActor::checkpoint` for a daemon reload: how long the workers it
/// stopped get to finish stopping (cancel → the runtime's cancel-path
/// history → the finalizer's resume archive) before the exec destroys them.
/// Cancellation is cooperative and normally takes milliseconds; a worker
/// blocked in a tool that ignores cancellation is abandoned with whatever
/// archive it already had.
pub const WORKER_RELOAD_STOP_TIMEOUT_SECS: u64 = 10;
pub const WORKER_RELOAD_STOP_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(WORKER_RELOAD_STOP_TIMEOUT_SECS);

/// Longest a reload checkpoint fences its session (no queue-wake turns,
/// keep-warm pings, driver ticks or parking) waiting for the `execv` or for
/// the daemon to report it failed (`ReloadAborted`). Both normally come
/// within seconds; this only bounds a reload that never reports back (a
/// daemon bug: clients cannot send a reload checkpoint).
pub const RELOAD_FENCE_TIMEOUT_SECS: u64 = 120;
pub const RELOAD_FENCE_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(RELOAD_FENCE_TIMEOUT_SECS);

// ── daemon reload steps after the checkpoints (`daemon::reload`) ──
/// Per-session `Checkpoint{Reload}` wait: `cancel_turn` (the drain + its
/// bounded save) + stopping the session's workers + the checkpoint's own
/// bounded save + 1 s margin. Sessions checkpoint concurrently.
pub const RELOAD_CHECKPOINT_BUDGET_SECS: u64 =
    CANCEL_TURN_TIMEOUT_SECS + WORKER_RELOAD_STOP_TIMEOUT_SECS + SAVE_TIMEOUT_SECS + 1;
/// Other connections' `Bye{Reloading}` writers, after the announce.
pub const RELOAD_ANNOUNCE_WAIT_SECS: u64 = 1;
/// The requesting connection flushes its own `Bye` before the exec.
pub const RELOAD_BYE_FLUSH_SECS: u64 = 1;
/// Extension sidecars shut down for the exec.
pub const RELOAD_EXT_SHUTDOWN_SECS: u64 = 5;
/// A failed exec re-discovers the extensions before the sessions resume.
pub const RELOAD_EXT_REDISCOVERY_SECS: u64 = 10;
/// Delivering `ReloadAborted` to a session whose command queue is full.
pub const RELOAD_ABORT_SEND_SECS: u64 = 5;
/// Longest a session stays fenced before its process is replaced or it is
/// told the exec failed: the slowest other session's checkpoint, the
/// announce, the flush, the sidecar shutdown, then (failed exec) the
/// re-discovery and the abort delivery.
pub const RELOAD_FENCE_WORST_CASE_SECS: u64 = RELOAD_CHECKPOINT_BUDGET_SECS
    + RELOAD_ANNOUNCE_WAIT_SECS
    + RELOAD_BYE_FLUSH_SECS
    + RELOAD_EXT_SHUTDOWN_SECS
    + RELOAD_EXT_REDISCOVERY_SECS
    + RELOAD_ABORT_SEND_SECS;
// The backstop must never lift a fence while the exec (or its abort) is
// still coming: a turn run then could save history holding a completion
// the next image re-posts, waking the parent twice.
const _: () = assert!(RELOAD_FENCE_TIMEOUT_SECS > 2 * RELOAD_FENCE_WORST_CASE_SECS);

/// Session end's bounded observability flush (`finish` STEP 3).
pub const OBSERVABILITY_FLUSH_TIMEOUT_SECS: u64 =
    crate::runtime::telemetry::DEFAULT_SHUTDOWN_FLUSH_TIMEOUT.as_secs();

/// Worst case of `SessionActor::finish`: cancel a running turn, one bounded
/// save, the `on_session_end` hooks, the observability flush.
pub const SESSION_END_TIMEOUT_SECS: u64 = CANCEL_TURN_TIMEOUT_SECS
    + SAVE_TIMEOUT_SECS
    + HOOKS_TIMEOUT_SECS
    + OBSERVABILITY_FLUSH_TIMEOUT_SECS;

/// `SessionActor::create` bound on `EngineHost::extensions_ready()`: the
/// loader guard should make this unreachable; it exists so a session can
/// never hang on a loader that never reports (warns, then proceeds).
pub const EXTENSIONS_READY_TIMEOUT_SECS: u64 = 30;
pub const EXTENSIONS_READY_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(EXTENSIONS_READY_TIMEOUT_SECS);

/// `SessionActor::unpark` bound (B3): journal load + runtime rebuild.
/// Transports wait `ATTACH_TIMEOUT_PARKED` (25 s) for a parked attach.
pub const UNPARK_TIMEOUT_SECS: u64 = 20;
pub const UNPARK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(UNPARK_TIMEOUT_SECS);
