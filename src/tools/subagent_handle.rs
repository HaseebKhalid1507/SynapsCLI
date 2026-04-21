use std::collections::HashMap;
use std::sync::{Arc, Mutex};
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
/// Collapsed behind a single Mutex so a status poll takes exactly one lock.
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
    state: Arc<Mutex<SubagentState>>,

    // Channels
    steer_tx: Option<mpsc::UnboundedSender<String>>,
    shutdown_tx: Option<oneshot::Sender<()>>,

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
        state: Arc<Mutex<SubagentState>>,
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
            result_rx,
        }
    }

    /// Current status snapshot.
    pub fn status(&self) -> SubagentStatus {
        self.state.lock().unwrap().status.clone()
    }

    /// Partial output accumulated so far.
    pub fn partial_output(&self) -> String {
        self.state.lock().unwrap().partial_text.clone()
    }

    /// Snapshot of the tool log.
    pub fn tool_log(&self) -> Vec<String> {
        self.state.lock().unwrap().tool_log.clone()
    }

    /// Snapshot of conversation state (for resume).
    pub fn conversation_state(&self) -> Vec<Value> {
        self.state.lock().unwrap().conversation_state.clone()
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
    next_id: u64,
}

impl SubagentRegistry {
    pub fn new() -> Self {
        Self {
            handles: HashMap::new(),
            next_id: 0,
        }
    }

    /// Register a handle and return its id.
    pub fn register(&mut self, handle: SubagentHandle) -> String {
        let id = handle.id.clone();
        self.handles.insert(id.clone(), handle);
        self.next_id += 1;
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
