use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tokio::sync::{mpsc, oneshot};
use serde_json::Value;

// ── SubagentResult ───────────────────────────────────────────────────────────────

pub struct SubagentResult {
    pub text: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read: u64,
    pub cache_creation: u64,
    pub tool_count: u32,
}

// ── SubagentStatus ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum SubagentStatus {
    Running,
    Completed,
    TimedOut,
    Failed(String),
}

// ── SubagentState ────────────────────────────────────────────────────────────────

/// All mutable state shared between the subagent thread and its handle.
/// Collapsed behind a single RwLock so a status poll takes exactly one lock.
pub struct SubagentState {
    pub status: SubagentStatus,
    pub partial_text: String,
    pub tool_log: Vec<String>,
    pub conversation_state: Vec<Value>,
}

impl SubagentState {
    pub fn new() -> Self {
        Self {
            status: SubagentStatus::Running,
            partial_text: String::new(),
            tool_log: Vec::new(),
            conversation_state: Vec::new(),
        }
    }
}

impl Default for SubagentState {
    fn default() -> Self { Self::new() }
}

// ── SubagentHandle ───────────────────────────────────────────────────────────────

pub struct SubagentHandle {
    pub id: String,
    pub agent_name: String,
    pub task_preview: String,
    pub model: String,
    pub started_at: std::time::Instant,
    pub timeout_secs: u64,

    // Shared state updated by the subagent thread — one lock for everything.
    state: Arc<RwLock<SubagentState>>,

    // Channels
    steer_tx: Option<mpsc::UnboundedSender<String>>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    /// OS thread running the subagent. Stored for graceful shutdown (join).
    // OS thread handle for graceful shutdown
    thread_handle: Option<std::thread::JoinHandle<()>>,

    // Final result
    result_rx: Option<oneshot::Receiver<SubagentResult>>,
}

impl SubagentHandle {
    /// Construct a new handle. The state Arc is shared with the spawned subagent thread.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: String,
        agent_name: String,
        task_preview: String,
        model: String,
        timeout_secs: u64,
        state: Arc<RwLock<SubagentState>>,
        steer_tx: Option<mpsc::UnboundedSender<String>>,
        shutdown_tx: Option<oneshot::Sender<()>>,
        result_rx: Option<oneshot::Receiver<SubagentResult>>,
    ) -> Self {
        Self {
            id,
            agent_name,
            task_preview,
            model,
            started_at: std::time::Instant::now(),
            timeout_secs,
            state,
            steer_tx,
            shutdown_tx,
            thread_handle: None,
            result_rx,
        }
    }

    /// Current status snapshot.
    pub fn status(&self) -> SubagentStatus {
        self.state.read().unwrap().status.clone()
    }

    /// Partial output accumulated so far.
    pub fn partial_output(&self) -> String {
        self.state.read().unwrap().partial_text.clone()
    }

    /// Snapshot of the tool log.
    pub fn tool_log(&self) -> Vec<String> {
        self.state.read().unwrap().tool_log.clone()
    }

    /// Snapshot of conversation state (for resume).
    pub fn conversation_state(&self) -> Vec<Value> {
        self.state.read().unwrap().conversation_state.clone()
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
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }

    /// True if the subagent is no longer running.
    pub fn is_finished(&self) -> bool {
        !matches!(self.status(), SubagentStatus::Running)
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

pub struct SubagentRegistry {
    handles: HashMap<String, SubagentHandle>,
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

    /// Returns (id, agent_name, status) for every tracked handle.
    pub fn list_active(&self) -> Vec<(String, String, SubagentStatus)> {
        self.handles
            .values()
            .map(|h| (h.id.clone(), h.agent_name.clone(), h.status()))
            .collect()
    }

    /// Drop handles that are no longer running.
    /// Iterate over all handles mutably (for bulk operations like cancel-all).
    pub fn iter_mut_handles(&mut self) -> impl Iterator<Item = &mut SubagentHandle> {
        self.handles.values_mut()
    }

    pub fn cleanup_finished(&mut self) {
        self.handles.retain(|_, h| !h.is_finished());
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
            SubagentStatus::TimedOut => "timed_out",
            SubagentStatus::Failed(_) => "failed",
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
        TestHandle {
            handle: SubagentHandle::new(
                id.to_string(),
                "test-agent".to_string(),
                "test task".to_string(),
                "claude-sonnet-4-6".to_string(),
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
            "sa_1".into(), "test".into(), "task".into(),
            "model".into(), 300, state, None, None, Some(result_rx),
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

    #[test]
    fn registry_cleanup_finished() {
        let mut reg = SubagentRegistry::new();
        let h = make_handle("sa_1");
        {
            let mut s = h.state.write().unwrap();
            s.status = SubagentStatus::Completed;
        }
        reg.register(h);
        reg.register(make_handle("sa_2")); // still running
        reg.cleanup_finished();
        assert!(reg.get("sa_1").is_none()); // completed, cleaned up
        assert!(reg.get("sa_2").is_some()); // still running, kept
    }

    #[test]
    fn subagent_state_new_defaults() {
        let s = SubagentState::new();
        assert_eq!(s.status, SubagentStatus::Running);
        assert!(s.partial_text.is_empty());
        assert!(s.tool_log.is_empty());
        assert!(s.conversation_state.is_empty());
    }

    #[test]
    fn subagent_status_as_str() {
        assert_eq!(SubagentStatus::Running.as_str(), "running");
        assert_eq!(SubagentStatus::Completed.as_str(), "completed");
        assert_eq!(SubagentStatus::TimedOut.as_str(), "timed_out");
        assert_eq!(SubagentStatus::Failed("oops".into()).as_str(), "failed");
    }
}
