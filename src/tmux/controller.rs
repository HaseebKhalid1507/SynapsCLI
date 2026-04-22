//! TmuxController — manages the control mode connection to tmux.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::{oneshot, RwLock};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

use super::protocol::TmuxEvent;
use super::state::TmuxState;

/// Result of a command sent to tmux control mode.
#[derive(Debug)]
pub struct CommandResult {
    pub lines: Vec<String>,
    pub success: bool,
}

/// The main tmux controller. Owns the control mode process and state.
pub struct TmuxController {
    /// Child process handle for the control mode client
    child: Option<Child>,
    /// Writer to control mode stdin (wrapped in Mutex for interior mutability)
    writer: Option<Arc<tokio::sync::Mutex<tokio::process::ChildStdin>>>,
    /// Tracked state of all tmux objects
    state: Arc<RwLock<TmuxState>>,
    /// FIFO queue of pending command response waiters.
    /// Commands are serialized through `writer` mutex, so responses arrive
    /// in the same order. The reader task pops the front waiter on each
    /// `%begin`, collects data lines, and resolves on `%end`/`%error`.
    response_queue: Arc<tokio::sync::Mutex<VecDeque<oneshot::Sender<CommandResult>>>>,
    /// Set to true when the reader task detects the control mode pipe has closed.
    alive: Arc<AtomicBool>,
    /// Session name
    pub session_name: String,
}

impl TmuxController {
    /// Format a tmux command string for control mode.
    pub fn format_command(cmd: &str, args: &[&str]) -> String {
        if args.is_empty() {
            format!("{}\n", cmd)
        } else {
            format!("{} {}\n", cmd, args.join(" "))
        }
    }

    /// Create a new controller (does not start the connection yet).
    pub fn new(session_name: String) -> Self {
        Self {
            child: None,
            writer: None,
            state: Arc::new(RwLock::new(TmuxState::new("", &session_name))),
            response_queue: Arc::new(tokio::sync::Mutex::new(VecDeque::new())),
            alive: Arc::new(AtomicBool::new(false)),
            session_name,
        }
    }

    /// Get a handle to the shared state.
    pub fn state(&self) -> Arc<RwLock<TmuxState>> {
        Arc::clone(&self.state)
    }

    /// Check if the control mode connection is alive.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    /// Start a tmux session and connect via control mode.
    ///
    /// Uses a single `tmux -CC new-session` call to create the session
    /// AND connect in control mode atomically.  Waits for the initial
    /// `%begin`/`%end` handshake before returning to guarantee the pipe
    /// is live.
    pub async fn start(&mut self) -> Result<(), String> {
        let tmux_path = crate::tmux::find_tmux()
            .ok_or_else(|| "tmux not found in PATH".to_string())?;

        // Kill any stale session with the same name (clean slate model)
        let has_existing = Command::new(&tmux_path)
            .args(["has-session", "-t", &self.session_name])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false);

        if has_existing {
            tracing::info!("killing stale tmux session '{}'", self.session_name);
            let _ = Command::new(&tmux_path)
                .args(["kill-session", "-t", &self.session_name])
                .status()
                .await;
        }

        // Get terminal size
        let (cols, rows) = crossterm::terminal::size().unwrap_or((200, 50));

        // Single-step: create session AND attach in control mode.
        // This is the canonical way to use tmux control mode — avoids the
        // race between create-detached and attach that caused broken pipes.
        let mut child = Command::new(&tmux_path)
            .args([
                "-CC", "new-session",
                "-s", &self.session_name,
                "-x", &cols.to_string(),
                "-y", &rows.to_string(),
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("Failed to start tmux control mode: {}", e))?;

        let stdout = child.stdout.take()
            .ok_or_else(|| "Failed to capture stdout".to_string())?;
        let stdin = child.stdin.take()
            .ok_or_else(|| "Failed to capture stdin".to_string())?;
        let stderr = child.stderr.take();

        // Spawn a task to drain stderr and log it (so we can diagnose failures)
        if let Some(stderr) = stderr {
            tokio::spawn(async move {
                let reader = BufReader::new(stderr);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::warn!("tmux stderr: {}", line);
                }
            });
        }

        let writer = Arc::new(tokio::sync::Mutex::new(stdin));
        self.writer = Some(writer);
        self.child = Some(child);
        self.alive.store(true, Ordering::Relaxed);

        // Channel for the reader task to signal that the initial handshake
        // (%begin/%end for the implicit command on connect) has completed.
        let (ready_tx, ready_rx) = oneshot::channel::<()>();

        // Start reader task
        let response_queue = Arc::clone(&self.response_queue);
        let alive = Arc::clone(&self.alive);
        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            let mut current_waiter: Option<oneshot::Sender<CommandResult>> = None;
            let mut current_lines: Vec<String> = Vec::new();
            let mut in_command = false;
            let mut ready_tx = Some(ready_tx);
            let mut handshake_done = false;

            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(event) = TmuxEvent::parse(&line) {
                    match &event {
                        TmuxEvent::Begin { .. } => {
                            current_lines.clear();
                            in_command = true;

                            if handshake_done {
                                // Normal command — pop waiter from FIFO queue
                                let mut q = response_queue.lock().await;
                                current_waiter = q.pop_front();
                            }
                            // First %begin is the implicit handshake — no waiter
                        }
                        TmuxEvent::End { .. } | TmuxEvent::Error { .. } => {
                            let success = matches!(&event, TmuxEvent::End { .. });

                            if in_command {
                                if !handshake_done {
                                    // Initial handshake complete — signal readiness
                                    handshake_done = true;
                                    if let Some(tx) = ready_tx.take() {
                                        let _ = tx.send(());
                                    }
                                } else if let Some(sender) = current_waiter.take() {
                                    let _ = sender.send(CommandResult {
                                        lines: current_lines.drain(..).collect(),
                                        success,
                                    });
                                }
                                current_lines.clear();
                                in_command = false;
                            }
                        }
                        TmuxEvent::Data(data) => {
                            if in_command {
                                current_lines.push(data.clone());
                            }
                        }
                        _ => {
                            tracing::trace!("tmux event: {:?}", event);
                        }
                    }
                }
            }

            // Stdout closed — control mode process has exited
            alive.store(false, Ordering::Relaxed);

            // Drain any remaining waiters with an error
            let mut q = response_queue.lock().await;
            while let Some(sender) = q.pop_front() {
                let _ = sender.send(CommandResult {
                    lines: vec!["control mode disconnected".to_string()],
                    success: false,
                });
            }

            // If we never completed the handshake, unblock start()
            if let Some(tx) = ready_tx.take() {
                let _ = tx.send(());
            }

            tracing::debug!("tmux control mode reader exited");
        });

        // Wait for the initial handshake with a timeout.
        // tmux sends %begin/%end on connect — if we don't see it within 5s,
        // the process probably died immediately.
        match tokio::time::timeout(std::time::Duration::from_secs(5), ready_rx).await {
            Ok(Ok(())) if self.alive.load(Ordering::Relaxed) => {
                tracing::info!("tmux control mode connected to session '{}'", self.session_name);
                Ok(())
            }
            _ => {
                // Try to get exit status for a better error message
                let exit_info = if let Some(ref mut child) = self.child {
                    match child.try_wait() {
                        Ok(Some(status)) => format!(" (exit status: {})", status),
                        _ => String::new(),
                    }
                } else {
                    String::new()
                };
                self.alive.store(false, Ordering::Relaxed);
                Err(format!(
                    "tmux control mode failed to start{} — check `tmux` version and session name '{}'",
                    exit_info, self.session_name
                ))
            }
        }
    }

    /// Send a command to tmux control mode and wait for its response.
    ///
    /// Commands are serialized: the writer mutex ensures only one command
    /// is in-flight at a time, and the reader task resolves responses in
    /// FIFO order via the `response_queue`.
    pub async fn execute(&self, cmd: &str, args: &[&str]) -> Result<CommandResult, String> {
        // Check liveness before attempting to write
        if !self.alive.load(Ordering::Relaxed) {
            return Err("tmux control mode is not running".to_string());
        }

        let writer = self.writer.as_ref()
            .ok_or_else(|| "Control mode not connected".to_string())?;

        let formatted = Self::format_command(cmd, args);

        // Create a oneshot channel for this command's response
        let (tx, rx) = oneshot::channel();

        // Enqueue the waiter BEFORE writing the command
        {
            let mut q = self.response_queue.lock().await;
            q.push_back(tx);
        }

        // Write to stdin (serialized by the mutex)
        {
            let mut w = writer.lock().await;
            if let Err(e) = w.write_all(formatted.as_bytes()).await {
                // Write failed — remove our waiter and report
                let mut q = self.response_queue.lock().await;
                q.pop_back(); // best-effort remove (it's the last one we pushed)
                return Err(format!("Failed to write command: {} — control mode may have exited", e));
            }
            if let Err(e) = w.flush().await {
                let mut q = self.response_queue.lock().await;
                q.pop_back();
                return Err(format!("Failed to flush command: {} — control mode may have exited", e));
            }
        }

        // Wait for the reader task to resolve our response
        match tokio::time::timeout(std::time::Duration::from_secs(10), rx).await {
            Ok(Ok(result)) => {
                if result.success {
                    Ok(result)
                } else {
                    Err(format!("tmux command failed: {}", result.lines.join("\n")))
                }
            }
            Ok(Err(_)) => Err("Response channel closed — tmux control mode has exited".to_string()),
            Err(_) => Err("Timed out waiting for tmux response (10s)".to_string()),
        }
    }

    /// Execute a command and return the first line of output.
    pub async fn execute_single(&self, cmd: &str, args: &[&str]) -> Result<String, String> {
        let result = self.execute(cmd, args).await?;
        Ok(result.lines.into_iter().next().unwrap_or_default())
    }

    /// Kill the tmux session and clean up.
    pub async fn shutdown(&mut self) -> Result<(), String> {
        self.alive.store(false, Ordering::Relaxed);
        if let Some(tmux_path) = crate::tmux::find_tmux() {
            let _ = Command::new(&tmux_path)
                .args(["kill-session", "-t", &self.session_name])
                .status()
                .await;
        }
        if let Some(mut child) = self.child.take() {
            let _ = child.kill().await;
        }
        self.writer = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_command() {
        let cmd = TmuxController::format_command("split-window", &["-h", "-P", "-F", "#{pane_id}"]);
        assert_eq!(cmd, "split-window -h -P -F #{pane_id}\n");
    }

    #[test]
    fn test_format_command_no_args() {
        let cmd = TmuxController::format_command("list-windows", &[]);
        assert_eq!(cmd, "list-windows\n");
    }

    #[test]
    fn test_new_controller() {
        let controller = TmuxController::new("test-session".to_string());
        assert_eq!(controller.session_name, "test-session");
        assert!(!controller.is_alive());
    }

    #[tokio::test]
    async fn test_execute_when_disconnected() {
        let controller = TmuxController::new("test-disconnected".to_string());
        let result = controller.execute("list-windows", &[]).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not running"));
    }

    #[tokio::test]
    async fn test_start_requires_tmux() {
        let mut controller = TmuxController::new("test-start".to_string());
        if crate::tmux::find_tmux().is_none() {
            let result = controller.start().await;
            assert!(result.is_err());
        }
    }

    #[tokio::test]
    async fn test_response_queue_ordering() {
        let queue: Arc<tokio::sync::Mutex<VecDeque<oneshot::Sender<CommandResult>>>> =
            Arc::new(tokio::sync::Mutex::new(VecDeque::new()));

        let (tx1, rx1) = oneshot::channel();
        let (tx2, rx2) = oneshot::channel();

        {
            let mut q = queue.lock().await;
            q.push_back(tx1);
            q.push_back(tx2);
        }

        {
            let mut q = queue.lock().await;
            let sender = q.pop_front().unwrap();
            sender.send(CommandResult { lines: vec!["first".to_string()], success: true }).unwrap();
        }
        {
            let mut q = queue.lock().await;
            let sender = q.pop_front().unwrap();
            sender.send(CommandResult { lines: vec!["second".to_string()], success: true }).unwrap();
        }

        let r1 = rx1.await.unwrap();
        let r2 = rx2.await.unwrap();
        assert_eq!(r1.lines, vec!["first"]);
        assert_eq!(r2.lines, vec!["second"]);
    }

    #[tokio::test]
    async fn test_execute_when_dead() {
        // Simulate a controller where alive=false but writer exists
        let controller = TmuxController::new("test-dead".to_string());
        // alive defaults to false, so execute should fail immediately
        let result = controller.execute("list-windows", &[]).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not running"));
    }
}
