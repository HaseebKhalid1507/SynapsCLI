//! TmuxController — manages the control mode connection to tmux.

use std::collections::VecDeque;
use std::sync::Arc;
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
            session_name,
        }
    }

    /// Get a handle to the shared state.
    pub fn state(&self) -> Arc<RwLock<TmuxState>> {
        Arc::clone(&self.state)
    }

    /// Start a tmux session and connect via control mode.
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

        // Create detached session
        let create_status = Command::new(&tmux_path)
            .args([
                "new-session", "-d",
                "-s", &self.session_name,
                "-x", &cols.to_string(),
                "-y", &rows.to_string(),
            ])
            .status()
            .await
            .map_err(|e| format!("Failed to create tmux session: {}", e))?;

        if !create_status.success() {
            return Err("Failed to create tmux session".to_string());
        }

        // Attach in control mode
        let mut child = Command::new(&tmux_path)
            .args(["-CC", "attach-session", "-t", &self.session_name])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("Failed to start control mode: {}", e))?;

        let stdout = child.stdout.take()
            .ok_or_else(|| "Failed to capture stdout".to_string())?;
        let stdin = child.stdin.take()
            .ok_or_else(|| "Failed to capture stdin".to_string())?;

        let writer = Arc::new(tokio::sync::Mutex::new(stdin));
        self.writer = Some(writer);
        self.child = Some(child);

        // Start reader task — reads control mode output lines and dispatches
        // command responses to the FIFO queue of oneshot senders.
        let response_queue = Arc::clone(&self.response_queue);
        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            // Current waiter being filled (popped from queue on %begin)
            let mut current_waiter: Option<oneshot::Sender<CommandResult>> = None;
            let mut current_lines: Vec<String> = Vec::new();
            let mut in_command = false;

            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(event) = TmuxEvent::parse(&line) {
                    match &event {
                        TmuxEvent::Begin { .. } => {
                            // Pop the next waiter from the FIFO queue
                            let mut q = response_queue.lock().await;
                            current_waiter = q.pop_front();
                            current_lines.clear();
                            in_command = true;
                        }
                        TmuxEvent::End { .. } => {
                            if in_command {
                                if let Some(sender) = current_waiter.take() {
                                    let _ = sender.send(CommandResult {
                                        lines: current_lines.drain(..).collect(),
                                        success: true,
                                    });
                                }
                                in_command = false;
                            }
                        }
                        TmuxEvent::Error { .. } => {
                            if in_command {
                                if let Some(sender) = current_waiter.take() {
                                    let _ = sender.send(CommandResult {
                                        lines: current_lines.drain(..).collect(),
                                        success: false,
                                    });
                                }
                                in_command = false;
                            }
                        }
                        TmuxEvent::Data(data) => {
                            if in_command {
                                current_lines.push(data.clone());
                            }
                        }
                        _ => {
                            // Notifications (%output, %window-add, etc.) — log for now
                            tracing::trace!("tmux event: {:?}", event);
                        }
                    }
                }
            }
            tracing::debug!("tmux control mode reader exited");
        });

        tracing::info!("tmux control mode connected to session '{}'", self.session_name);
        Ok(())
    }

    /// Send a command to tmux control mode and wait for its response.
    ///
    /// Commands are serialized: the writer mutex ensures only one command
    /// is in-flight at a time, and the reader task resolves responses in
    /// FIFO order via the `response_queue`.
    pub async fn execute(&self, cmd: &str, args: &[&str]) -> Result<CommandResult, String> {
        let writer = self.writer.as_ref()
            .ok_or_else(|| "Control mode not connected".to_string())?;

        let formatted = Self::format_command(cmd, args);

        // Create a oneshot channel for this command's response
        let (tx, rx) = oneshot::channel();

        // Enqueue the waiter BEFORE writing the command (so the reader
        // task can find it when %begin arrives).
        {
            let mut q = self.response_queue.lock().await;
            q.push_back(tx);
        }

        // Write to stdin (serialized by the mutex)
        {
            let mut w = writer.lock().await;
            w.write_all(formatted.as_bytes()).await
                .map_err(|e| format!("Failed to write command: {}", e))?;
            w.flush().await
                .map_err(|e| format!("Failed to flush: {}", e))?;
        }

        // Wait for the reader task to resolve our response
        match tokio::time::timeout(std::time::Duration::from_secs(10), rx).await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(_)) => Err("Response channel closed — tmux control mode may have exited".to_string()),
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
    }

    #[tokio::test]
    async fn test_execute_when_disconnected() {
        let controller = TmuxController::new("test-disconnected".to_string());
        let result = controller.execute("list-windows", &[]).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not connected"));
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
        // Verify that the response queue is FIFO
        let queue: Arc<tokio::sync::Mutex<VecDeque<oneshot::Sender<CommandResult>>>> =
            Arc::new(tokio::sync::Mutex::new(VecDeque::new()));

        let (tx1, rx1) = oneshot::channel();
        let (tx2, rx2) = oneshot::channel();

        {
            let mut q = queue.lock().await;
            q.push_back(tx1);
            q.push_back(tx2);
        }

        // Pop first — should be tx1
        {
            let mut q = queue.lock().await;
            let sender = q.pop_front().unwrap();
            sender.send(CommandResult { lines: vec!["first".to_string()], success: true }).unwrap();
        }
        // Pop second — should be tx2
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
}
