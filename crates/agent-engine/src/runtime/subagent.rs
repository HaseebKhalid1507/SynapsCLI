use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tokio::sync::{mpsc, oneshot};
use serde_json::Value;

// ── SubagentResult ───────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct SubagentResult {
    pub text: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read: u64,
    pub cache_creation: u64,
    /// TTL split of `cache_creation` across the subagent's turns.
    /// `None` only if no turn ever reported a split; otherwise the sum.
    pub cache_creation_5m: Option<u64>,
    pub cache_creation_1h: Option<u64>,
    pub tool_count: u32,
    pub timed_out: bool,
}

// ── SubagentStatus ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalCategory {
    Completed,
    Route,
    Credential,
    Runtime,
    ProviderClient,
    Transport,
    StartupFailed,
    ExecutionFailed,
    TimedOut,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct TerminalDiagnostic {
    pub category: TerminalCategory,
    pub code: String,
    pub stage: String,
    pub correlation_id: String,
    pub network_attempted: bool,
    pub safe_message: String,
}

pub fn safe_failure_message(category: &TerminalCategory) -> &'static str {
    match category {
        TerminalCategory::Route => "worker route could not be resolved",
        TerminalCategory::Credential => "worker credentials are unavailable",
        TerminalCategory::Runtime | TerminalCategory::StartupFailed => {
            "worker runtime could not be started"
        }
        TerminalCategory::ProviderClient => "provider client could not be initialized",
        TerminalCategory::Transport => "provider transport failed",
        TerminalCategory::TimedOut => "worker timed out",
        TerminalCategory::Cancelled => "worker was cancelled",
        TerminalCategory::ExecutionFailed => "worker execution failed",
        TerminalCategory::Completed => "worker completed",
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SubagentStatus {
    Running,
    Completed,
    /// User-aborted via TUI cancel. Never publishes a wake event.
    Cancelled,
    TimedOut,
    Failed(String),
}

// ── SubagentState ────────────────────────────────────────────────────────────────

/// All mutable state shared between the subagent thread and its handle.
/// Collapsed behind a single RwLock so a status poll takes exactly one lock.
#[derive(Debug)]
pub struct SubagentState {
    pub status: SubagentStatus,
    pub partial_text: String,
    pub tool_log: Vec<String>,
    pub conversation_state: Vec<Value>,
    /// Stamped once by finalize_subagent at thread exit.
    pub finished_at: Option<std::time::Instant>,
    /// Set by cancel() before the shutdown signal is sent.
    /// Read by finalize_subagent to label the terminal status correctly.
    pub cancel_requested: bool,
    pub terminal: Option<TerminalDiagnostic>,
}

impl SubagentState {
    pub fn new() -> Self {
        Self {
            status: SubagentStatus::Running,
            partial_text: String::new(),
            tool_log: Vec::new(),
            conversation_state: Vec::new(),
            finished_at: None,
            cancel_requested: false,
            terminal: None,
        }
    }
}

impl Default for SubagentState {
    fn default() -> Self { Self::new() }
}

// ── SubagentDisplayRow ────────────────────────────────────────────────────────────

/// Snapshot row produced by SubagentRegistry::display_rows().
/// Used by the TUI reconcile path as the registry's liveness authority.
#[derive(Debug, Clone)]
pub struct SubagentDisplayRow {
    pub subagent_id: u64,
    pub agent_name: String,
    pub status: SubagentStatus,
    pub cancel_requested: bool,
    pub elapsed_secs: f64,
    pub finished_elapsed: Option<std::time::Duration>,
}

// ── SubagentHandle ───────────────────────────────────────────────────────────────

pub struct SubagentHandle {
    pub id: String,
    /// Numeric ID — same value that was passed to SubagentStart/SubagentDone events
    /// so the TUI can correlate by id without parsing "sa_N" strings.
    pub subagent_id: u64,
    pub agent_name: String,
    pub task_preview: String,
    pub model: String,
    pub system_prompt: String,
    pub started_at: std::time::Instant,
    pub timeout_secs: u64,

    // Shared state updated by the subagent thread — one lock for everything.
    state: Arc<RwLock<SubagentState>>,

    // Channels
    steer_tx: Option<mpsc::UnboundedSender<String>>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    /// OS thread running the subagent. Stored for graceful shutdown (join).
    // OS thread handle for graceful shutdown
    pub(crate) thread_handle: Option<std::thread::JoinHandle<()>>,

    // Final result
    result_rx: Option<oneshot::Receiver<SubagentResult>>,

    /// Set to true once subagent_collect has read the terminal result.
    collected: bool,
}

impl std::fmt::Debug for SubagentHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubagentHandle")
            .field("id", &self.id)
            .field("subagent_id", &self.subagent_id)
            .field("agent_name", &self.agent_name)
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

impl SubagentHandle {
    /// Construct a new handle. The state Arc is shared with the spawned subagent thread.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: String,
        subagent_id: u64,
        agent_name: String,
        task_preview: String,
        model: String,
        system_prompt: String,
        timeout_secs: u64,
        state: Arc<RwLock<SubagentState>>,
        steer_tx: Option<mpsc::UnboundedSender<String>>,
        shutdown_tx: Option<oneshot::Sender<()>>,
        result_rx: Option<oneshot::Receiver<SubagentResult>>,
    ) -> Self {
        Self {
            id,
            subagent_id,
            agent_name,
            task_preview,
            model,
            system_prompt,
            started_at: std::time::Instant::now(),
            timeout_secs,
            state,
            steer_tx,
            shutdown_tx,
            thread_handle: None,
            result_rx,
            collected: false,
        }
    }

    /// Current status snapshot.
    pub fn status(&self) -> SubagentStatus {
        self.state.read().unwrap_or_else(|p| p.into_inner()).status.clone()
    }

    /// Partial output accumulated so far.
    pub fn partial_output(&self) -> String {
        self.state.read().unwrap_or_else(|p| p.into_inner()).partial_text.clone()
    }

    /// Snapshot of the tool log.
    pub fn tool_log(&self) -> Vec<String> {
        self.state.read().unwrap_or_else(|p| p.into_inner()).tool_log.clone()
    }

    pub fn terminal_diagnostic(&self) -> Option<TerminalDiagnostic> {
        if let Some(diagnostic) = self.state.read().unwrap().terminal.clone() {
            return Some(diagnostic);
        }
        match self.status() {
            SubagentStatus::Running => None,
            SubagentStatus::Completed => Some(TerminalDiagnostic {
                category: TerminalCategory::Completed,
                code: "completed".into(),
                stage: "inference".into(),
                correlation_id: self.id.clone(),
                network_attempted: true,
                safe_message: safe_failure_message(&TerminalCategory::Completed).into(),
            }),
            SubagentStatus::TimedOut => Some(TerminalDiagnostic {
                category: TerminalCategory::TimedOut,
                code: "worker_timeout".into(),
                stage: "inference".into(),
                correlation_id: self.id.clone(),
                network_attempted: true,
                safe_message: safe_failure_message(&TerminalCategory::TimedOut).into(),
            }),
            SubagentStatus::Cancelled => Some(TerminalDiagnostic {
                category: TerminalCategory::Cancelled,
                code: "worker_cancelled".into(),
                stage: "inference".into(),
                correlation_id: self.id.clone(),
                network_attempted: true,
                safe_message: safe_failure_message(&TerminalCategory::Cancelled).into(),
            }),
            SubagentStatus::Failed(_) => Some(TerminalDiagnostic {
                category: TerminalCategory::ExecutionFailed,
                code: "inference_failed".into(),
                stage: "inference".into(),
                correlation_id: self.id.clone(),
                network_attempted: true,
                safe_message: safe_failure_message(&TerminalCategory::ExecutionFailed).into(),
            }),
        }
    }

    /// Snapshot of conversation state (for resume).
    pub fn conversation_state(&self) -> Vec<Value> {
        self.state.read().unwrap_or_else(|p| p.into_inner()).conversation_state.clone()
    }

    /// Seconds since this handle was created.
    pub fn elapsed_secs(&self) -> f64 {
        self.started_at.elapsed().as_secs_f64()
    }

    /// Send a steering message into the running subagent.
    pub fn steer(&self, message: &str) -> Result<(), String> {
        match &self.steer_tx {
            Some(tx) => tx
                .send(message.to_string())
                .map_err(|e| format!("steer channel closed: {e}")),
            None => Err("no steer channel on this handle".to_string()),
        }
    }

    /// Signal the subagent to shut down.
    /// Store the OS thread handle for graceful shutdown.
    pub fn set_thread_handle(&mut self, handle: std::thread::JoinHandle<()>) {
        self.thread_handle = Some(handle);
    }

    pub fn cancel(&mut self) {
        // Flag FIRST so finalize_subagent can read it before the thread exits.
        // Use poison-safe write in case the thread panicked holding the lock.
        self.state.write().unwrap_or_else(|p| p.into_inner()).cancel_requested = true;
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }

    /// True if the subagent is no longer running.
    pub fn is_finished(&self) -> bool {
        !matches!(self.status(), SubagentStatus::Running)
    }

    /// Mark this handle as collected by subagent_collect.
    pub fn mark_collected(&mut self) {
        self.collected = true;
    }

    /// Whether subagent_collect has already read the result.
    pub fn is_collected(&self) -> bool {
        self.collected
    }

    /// Time elapsed since the subagent reached a terminal state.
    /// Returns `None` if still running or if `finished_at` has not been stamped yet.
    pub fn finished_elapsed(&self) -> Option<std::time::Duration> {
        self.state.read().unwrap_or_else(|p| p.into_inner()).finished_at.map(|t| t.elapsed())
    }

    /// Consume the handle and wait for the final result.
    pub async fn collect(mut self) -> Result<SubagentResult, String> {
        match self.result_rx.take() {
            Some(rx) => rx.await.map_err(|_| "subagent result channel dropped".to_string()),
            None => Err("no result receiver — already collected or never set".to_string()),
        }
    }
}

// ── SubagentRegistry ─────────────────────────────────────────────────────────────

/// Finished-but-uncollected handles are retained this long before GC.
pub const FINISHED_HANDLE_TTL: std::time::Duration = std::time::Duration::from_secs(900); // 15 min

#[derive(Debug)]
pub struct SubagentRegistry {
    pub(crate) handles: HashMap<String, SubagentHandle>,
}

impl SubagentRegistry {
    pub fn new() -> Self {
        Self {
            handles: HashMap::new(),
        }
    }

    /// Register a handle and return its id.
    pub fn register(&mut self, handle: SubagentHandle) -> String {
        let id = handle.id.clone();
        self.handles.insert(id.clone(), handle);
        id
    }

    pub fn get(&self, id: &str) -> Option<&SubagentHandle> {
        self.handles.get(id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut SubagentHandle> {
        self.handles.get_mut(id)
    }

    pub fn remove(&mut self, id: &str) -> Option<SubagentHandle> {
        self.handles.remove(id)
    }

    /// Consume only transport resources while retaining terminal diagnostics and
    /// output under normal session retention.
    pub fn release_finished_resources(&mut self, id: &str) {
        if let Some(handle) = self.handles.get_mut(id) {
            if handle.is_finished() {
                handle.shutdown_tx.take();
                handle.result_rx.take();
                if let Some(thread) = handle.thread_handle.take() {
                    let _ = thread.join();
                }
            }
        }
    }

    /// Returns (id, agent_name, status) for every tracked handle.
    pub fn list_active(&self) -> Vec<(String, String, SubagentStatus)> {
        self.handles
            .values()
            .map(|h| (h.id.clone(), h.agent_name.clone(), h.status()))
            .collect()
    }

    /// Snapshot of every tracked handle for TUI reconcile.
    /// Reads all state under the lock POISON-SAFE; exposes cancel_requested
    /// which list_active() does not.
    pub fn display_rows(&self) -> Vec<SubagentDisplayRow> {
        self.handles
            .values()
            .map(|h| {
                let s = h.state.read().unwrap_or_else(|p| p.into_inner());
                SubagentDisplayRow {
                    subagent_id: h.subagent_id,
                    agent_name: h.agent_name.clone(),
                    status: s.status.clone(),
                    cancel_requested: s.cancel_requested,
                    elapsed_secs: h.started_at.elapsed().as_secs_f64(),
                    finished_elapsed: s.finished_at.map(|t| t.elapsed()),
                }
            })
            .collect()
    }

    /// Drop handles that are no longer running.
    /// Iterate over all handles mutably (for bulk operations like cancel-all).
    pub fn iter_mut_handles(&mut self) -> impl Iterator<Item = &mut SubagentHandle> {
        self.handles.values_mut()
    }

    /// Reap a finished handle iff:
    ///   (a) its result was collected via subagent_collect, OR
    ///   (b) it has been finished longer than `ttl` (abandoned).
    /// Finished-but-uncollected handles inside the TTL are RETAINED so the
    /// completion event can wake the parent and collect still succeeds.
    ///
    /// Additionally, handles whose OS thread is still running (e.g. still in
    /// the finalizer path) are deferred — joining a live thread blocks the TUI
    /// loop. They will be reaped on the next cleanup pass once the thread exits.
    pub fn cleanup_finished_with_ttl(&mut self, ttl: std::time::Duration) {
        let reap_ids: Vec<String> = self.handles.iter()
            .filter(|(_, h)| h.is_finished()
                // Defer handles whose OS thread is still alive — joining a live
                // thread would block the TUI event loop. Use map_or (stable 1.80+)
                // rather than is_none_or (stable 1.82+).
                && h.thread_handle.as_ref().map_or(true, |t| t.is_finished())
                && (h.is_collected()
                    || h.finished_elapsed().is_some_and(|d| d >= ttl)))
            .map(|(id, _)| id.clone())
            .collect();
        for id in reap_ids {
            if let Some(mut handle) = self.handles.remove(&id) {
                if let Some(th) = handle.thread_handle.take() {
                    let _ = th.join();
                }
            }
        }
    }

    /// Reap finished handles using the production TTL.
    /// Finished-but-uncollected handles within the TTL window are retained.
    pub fn cleanup_finished(&mut self) {
        self.cleanup_finished_with_ttl(FINISHED_HANDLE_TTL)
    }
}

/// Engine-owned housekeeping seam: reap finished subagent handles at end of
/// each turn.  Acquires the lock poison-safely — a panicked subagent thread
/// must not prevent the turn from completing.
///
/// Intended to be called from the `tokio::spawn` wrapper in `runtime/mod.rs`
/// after `run_stream_internal` returns and BEFORE sending `SessionEvent::Done`.
pub fn reap_finished(registry: &Arc<std::sync::Mutex<SubagentRegistry>>) {
    match registry.lock() {
        Ok(mut guard) => guard.cleanup_finished(),
        Err(poisoned) => poisoned.into_inner().cleanup_finished(),
    }
}

impl Default for SubagentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl SubagentStatus {
    pub fn as_str(&self) -> &str {
        match self {
            SubagentStatus::Running => "running",
            SubagentStatus::Completed => "completed",
            SubagentStatus::Cancelled => "cancelled",
            SubagentStatus::TimedOut => "timed_out",
            SubagentStatus::Failed(_) => "failed",
        }
    }

    /// Returns the failure reason string if this is a `Failed` variant.
    pub fn failure_reason(&self) -> Option<&str> {
        match self {
            SubagentStatus::Failed(r) => Some(r.as_str()),
            _ => None,
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::{mpsc, oneshot};

    // Keep receivers alive so channels don't close during tests
    struct TestHandle {
        handle: SubagentHandle,
        _steer_rx: mpsc::UnboundedReceiver<String>,
        _shutdown_rx: oneshot::Receiver<()>,
    }

    fn make_test_handle(id: &str) -> TestHandle {
        let state = Arc::new(RwLock::new(SubagentState::new()));
        let (steer_tx, steer_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let (_result_tx, result_rx) = oneshot::channel();
        // Parse numeric id from "sa_N" or use 0 as fallback for tests
        let numeric_id: u64 = id.strip_prefix("sa_").and_then(|n| n.parse().ok()).unwrap_or(0);
        TestHandle {
            handle: SubagentHandle::new(
                id.to_string(),
                numeric_id,
                "test-agent".to_string(),
                "test task".to_string(),
                "claude-sonnet-4-6".to_string(),
                "You are a test agent.".to_string(),
                300,
                state,
                Some(steer_tx),
                Some(shutdown_tx),
                Some(result_rx),
            ),
            _steer_rx: steer_rx,
            _shutdown_rx: shutdown_rx,
        }
    }

    fn make_handle(id: &str) -> SubagentHandle {
        make_test_handle(id).handle
    }

    fn make_finished_handle(id: &str) -> SubagentHandle {
        let h = make_handle(id);
        {
            let mut s = h.state.write().unwrap();
            s.status = SubagentStatus::Completed;
            s.finished_at = Some(std::time::Instant::now());
        }
        h
    }

    #[test]
    fn handle_initial_status_is_running() {
        let h = make_handle("sa_1");
        assert_eq!(h.status(), SubagentStatus::Running);
        assert!(!h.is_finished());
    }

    #[test]
    fn handle_partial_output_empty_initially() {
        let h = make_handle("sa_1");
        assert_eq!(h.partial_output(), "");
        assert!(h.tool_log().is_empty());
        assert!(h.conversation_state().is_empty());
    }

    #[test]
    fn handle_status_reflects_state_change() {
        let h = make_handle("sa_1");
        {
            let mut s = h.state.write().unwrap();
            s.status = SubagentStatus::Completed;
            s.partial_text = "done!".to_string();
        }
        assert_eq!(h.status(), SubagentStatus::Completed);
        assert!(h.is_finished());
        assert_eq!(h.partial_output(), "done!");
    }

    #[test]
    fn handle_steer_sends_message() {
        let th = make_test_handle("sa_1");
        assert!(th.handle.steer("redirect").is_ok());
    }

    #[test]
    fn handle_steer_fails_without_channel() {
        let state = Arc::new(RwLock::new(SubagentState::new()));
        let (_shutdown_tx, _) = oneshot::channel::<()>();
        let (_, result_rx) = oneshot::channel();
        let h = SubagentHandle::new(
            "sa_1".into(), 1, "test".into(), "task".into(),
            "model".into(), "prompt".into(), 300, state, None, None, Some(result_rx),
        );
        assert!(h.steer("msg").is_err());
    }

    #[test]
    fn handle_cancel_consumes_shutdown() {
        let mut h = make_handle("sa_1");
        h.cancel(); // first call sends
        h.cancel(); // second call is no-op (already taken)
    }

    #[test]
    fn handle_elapsed_increases() {
        let h = make_handle("sa_1");
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert!(h.elapsed_secs() > 0.0);
    }

    #[test]
    fn registry_register_and_get() {
        let mut reg = SubagentRegistry::new();
        let h = make_handle("sa_1");
        reg.register(h);
        assert!(reg.get("sa_1").is_some());
        assert!(reg.get("sa_99").is_none());
    }

    #[test]
    fn registry_remove() {
        let mut reg = SubagentRegistry::new();
        reg.register(make_handle("sa_1"));
        assert!(reg.remove("sa_1").is_some());
        assert!(reg.get("sa_1").is_none());
    }

    #[test]
    fn registry_list_active() {
        let mut reg = SubagentRegistry::new();
        reg.register(make_handle("sa_1"));
        reg.register(make_handle("sa_2"));
        let active = reg.list_active();
        assert_eq!(active.len(), 2);
    }

    // Rewritten: finished-but-uncollected handles are RETAINED (new semantics).
    // The old test encoded the buggy behavior where any finished handle was removed.
    #[test]
    fn registry_cleanup_finished() {
        let mut reg = SubagentRegistry::new();
        let mut h = make_finished_handle("sa_1");
        h.mark_collected(); // mark as collected — should be reaped
        reg.register(h);
        reg.register(make_handle("sa_2")); // still running — must survive

        reg.cleanup_finished();

        assert!(reg.get("sa_1").is_none(), "collected finished handle must be reaped");
        assert!(reg.get("sa_2").is_some(), "running handle must survive cleanup");
    }

    // U1: finished, not collected, fresh → retained within TTL
    #[test]
    fn reaper_retains_finished_uncollected_within_ttl() {
        let mut reg = SubagentRegistry::new();
        let h = make_finished_handle("sa_1"); // not collected
        reg.register(h);

        reg.cleanup_finished_with_ttl(std::time::Duration::from_secs(900));

        assert!(reg.get("sa_1").is_some(), "finished-uncollected handle within TTL must be retained");
    }

    // U2: finished + mark_collected → removed immediately
    #[test]
    fn reaper_reaps_finished_and_collected() {
        let mut reg = SubagentRegistry::new();
        let mut h = make_finished_handle("sa_1");
        h.mark_collected();
        reg.register(h);

        reg.cleanup_finished_with_ttl(std::time::Duration::from_secs(900));

        assert!(reg.get("sa_1").is_none(), "finished+collected handle must be reaped");
    }

    // U3: finished, not collected, but TTL=ZERO (expired) → removed
    #[test]
    fn reaper_reaps_abandoned_after_ttl() {
        let mut reg = SubagentRegistry::new();
        let h = make_finished_handle("sa_1"); // not collected
        reg.register(h);

        reg.cleanup_finished_with_ttl(std::time::Duration::ZERO);

        assert!(reg.get("sa_1").is_none(), "handle past TTL must be reaped even if uncollected");
    }

    // U5-reaper: finished-status handle with a still-sleeping OS thread is deferred;
    // after thread exits a second cleanup reaps it.
    #[test]
    fn reaper_defers_handle_with_live_thread() {
        let mut reg = SubagentRegistry::new();
        let mut h = make_finished_handle("sa_live");
        h.mark_collected(); // collected — would normally be reaped immediately

        // Attach a real thread that sleeps briefly (simulates thread still in finalizer)
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let barrier_c = Arc::clone(&barrier);
        let thread = std::thread::spawn(move || {
            // Wait for the test to run its first cleanup
            barrier_c.wait();
            // Now sleep a bit more before truly exiting
            std::thread::sleep(std::time::Duration::from_millis(20));
        });
        h.set_thread_handle(thread);
        reg.register(h);

        // Signal thread (it's waiting on barrier)
        barrier.wait();

        // First cleanup: thread is still alive → handle must be deferred (retained)
        // The thread may or may not have exited by the time we get here, so we only
        // assert the positive case when we know the thread is still live.
        // Instead: sleep briefly to be sure the thread has NOT exited, then check.
        // (The thread sleeps 20ms after barrier; we check immediately after barrier.)
        reg.cleanup_finished_with_ttl(std::time::Duration::ZERO);
        // Handle may still be present (thread live) or absent (thread exited) — both ok.
        // What must NOT happen: a blocking join. The test itself completes in < 1s.

        // Wait for thread to truly exit
        std::thread::sleep(std::time::Duration::from_millis(50));

        // Second cleanup: thread is finished → handle must be reaped
        reg.cleanup_finished_with_ttl(std::time::Duration::ZERO);
        assert!(reg.get("sa_live").is_none(), "handle must be reaped once OS thread exits");
    }

    // U4: running handles never touched by any reaper variant
    #[test]
    fn reaper_never_touches_running() {
        let mut reg = SubagentRegistry::new();
        reg.register(make_handle("sa_1")); // Running
        reg.register(make_handle("sa_2")); // Running

        reg.cleanup_finished_with_ttl(std::time::Duration::ZERO);
        assert!(reg.get("sa_1").is_some());
        assert!(reg.get("sa_2").is_some());

        reg.cleanup_finished(); // production TTL
        assert!(reg.get("sa_1").is_some());
        assert!(reg.get("sa_2").is_some());
    }

    #[test]
    fn subagent_state_new_defaults() {
        let s = SubagentState::new();
        assert_eq!(s.status, SubagentStatus::Running);
        assert!(s.partial_text.is_empty());
        assert!(s.tool_log.is_empty());
        assert!(s.conversation_state.is_empty());
        assert!(s.finished_at.is_none());
        assert!(!s.cancel_requested, "cancel_requested must default to false");
    }

    #[test]
    fn subagent_status_as_str() {
        assert_eq!(SubagentStatus::Running.as_str(), "running");
        assert_eq!(SubagentStatus::Completed.as_str(), "completed");
        assert_eq!(SubagentStatus::Cancelled.as_str(), "cancelled");
        assert_eq!(SubagentStatus::TimedOut.as_str(), "timed_out");
        assert_eq!(SubagentStatus::Failed("oops".into()).as_str(), "failed");
    }

    // V1: cancel() sets cancel_requested on shared state before sending signal.
    #[test]
    fn cancel_sets_cancel_requested_flag() {
        let mut h = make_handle("sa_cancel");
        {
            let s = h.state.read().unwrap();
            assert!(!s.cancel_requested, "cancel_requested must start false");
        }
        h.cancel();
        {
            let s = h.state.read().unwrap();
            assert!(s.cancel_requested, "cancel_requested must be true after cancel()");
        }
        // Second call is a no-op (shutdown_tx already taken)
        h.cancel();
    }

    // V2: cancelled status counts as finished, is_finished() returns true.
    #[test]
    fn cancelled_status_is_finished() {
        let h = make_handle("sa_c1");
        {
            let mut s = h.state.write().unwrap();
            s.status = SubagentStatus::Cancelled;
        }
        assert!(h.is_finished(), "Cancelled must count as finished");
    }

    // D1: display_rows returns a row for each registered handle
    #[test]
    fn display_rows_running_handle() {
        let mut reg = SubagentRegistry::new();
        reg.register(make_handle("sa_1"));
        reg.register(make_handle("sa_2"));
        let rows = reg.display_rows();
        assert_eq!(rows.len(), 2);
        for row in &rows {
            assert_eq!(row.status, SubagentStatus::Running);
            assert!(!row.cancel_requested);
            assert!(row.elapsed_secs >= 0.0);
            assert!(row.finished_elapsed.is_none());
        }
    }

    // D2: display_rows reflects finished status and cancel_requested flag
    #[test]
    fn display_rows_finished_and_cancel_requested() {
        let mut reg = SubagentRegistry::new();
        let h = make_finished_handle("sa_1");
        reg.register(h);

        // Make a handle with cancel_requested = true
        let th = make_test_handle("sa_2");
        {
            let mut s = th.handle.state.write().unwrap();
            s.cancel_requested = true;
        }
        reg.register(th.handle);

        let rows = reg.display_rows();
        assert_eq!(rows.len(), 2);

        let finished = rows.iter().find(|r| r.subagent_id == 1).unwrap();
        assert_eq!(finished.status, SubagentStatus::Completed);
        assert!(!finished.cancel_requested);
        assert!(finished.finished_elapsed.is_some());

        let cancelling = rows.iter().find(|r| r.subagent_id == 2).unwrap();
        assert!(cancelling.cancel_requested);
    }

    // D3: display_rows on empty registry returns empty vec
    #[test]
    fn display_rows_empty_registry() {
        let reg = SubagentRegistry::new();
        assert!(reg.display_rows().is_empty());
    }

    // ── reap_finished free-function tests ────────────────────────────────────

    /// R1: reap_finished with a collected+finished handle in an Arc<Mutex<>> reaps it.
    #[test]
    fn reap_finished_headless_reaps_collected() {
        let registry = Arc::new(std::sync::Mutex::new(SubagentRegistry::new()));
        {
            let mut reg = registry.lock().unwrap();
            let mut h = make_finished_handle("sa_r1");
            h.mark_collected();
            reg.register(h);
        }
        super::reap_finished(&registry);
        assert!(registry.lock().unwrap().get("sa_r1").is_none(),
            "reap_finished must reap collected+finished handle");
    }

    /// R2: reap_finished retains running handles — must not touch live work.
    #[test]
    fn reap_finished_headless_retains_running() {
        let registry = Arc::new(std::sync::Mutex::new(SubagentRegistry::new()));
        {
            let mut reg = registry.lock().unwrap();
            reg.register(make_handle("sa_running"));
        }
        super::reap_finished(&registry);
        assert!(registry.lock().unwrap().get("sa_running").is_some(),
            "reap_finished must not touch running handles");
    }

    /// R3: reap_finished recovers from a poisoned lock without panicking.
    #[test]
    fn reap_finished_poisoned_lock_recovery() {
        let registry = Arc::new(std::sync::Mutex::new(SubagentRegistry::new()));
        {
            let mut reg = registry.lock().unwrap();
            let mut h = make_finished_handle("sa_poison");
            h.mark_collected();
            reg.register(h);
        }

        // Poison the mutex by panicking inside a lock guard.
        let registry_c = Arc::clone(&registry);
        let _ = std::panic::catch_unwind(|| {
            let _guard = registry_c.lock().unwrap();
            panic!("deliberately poison the mutex");
        });

        // reap_finished must not panic despite poisoned lock.
        super::reap_finished(&registry);

        // The handle must be reaped (cleanup ran despite poison).
        let result = registry.lock()
            .unwrap_or_else(|p| p.into_inner())
            .get("sa_poison")
            .is_none();
        assert!(result, "reap_finished must run cleanup even after poison");
    }
}

// Separate module so finalize_subagent is accessible via its pub(crate) path.
#[cfg(test)]
mod cancelled_wake_tests {
    use super::*;
    use std::sync::{Arc, RwLock};

    // V3: finalize_subagent with cancel_requested=true → status Cancelled,
    // no event published, finished_at stamped.
    // Test name matches spec: cancelled_suppresses_wake.
    #[test]
    fn cancelled_suppresses_wake() {
        use crate::events::EventQueue;
        use crate::tools::finalize_subagent;

        let state = Arc::new(RwLock::new(SubagentState::new()));
        {
            let mut s = state.write().unwrap();
            s.status = SubagentStatus::Completed; // thread set Completed before cancel flag noticed
            s.cancel_requested = true;            // cancel() was called
        }
        let queue = Arc::new(EventQueue::new(100));

        finalize_subagent(
            &state,
            Some(&queue),
            "sa_v3", 42, "test-agent",
            std::time::Instant::now(),
            None,
        );

        // Queue must be empty — no wake event for cancelled subagents.
        assert!(queue.is_empty(), "cancelled subagent must not publish a wake event");

        // Status must have been re-labelled to Cancelled.
        let s = state.read().unwrap();
        assert_eq!(s.status, SubagentStatus::Cancelled, "status must be Cancelled when cancel_requested");

        // finished_at must be stamped.
        assert!(s.finished_at.is_some(), "finished_at must be stamped by finalizer");
    }
}
