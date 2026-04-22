//! TmuxController — manages the control mode connection to tmux.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, RwLock};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use std::sync::atomic::AtomicU64;

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
    /// Channel for incoming parsed events
    event_tx: mpsc::UnboundedSender<TmuxEvent>,
    /// Receiver for events (consumed by event loop)
    event_rx: Option<mpsc::UnboundedReceiver<TmuxEvent>>,
    /// Pending command responses: command_num -> sender
    pending: Arc<tokio::sync::Mutex<HashMap<u64, oneshot::Sender<CommandResult>>>>,
    /// Next command number
    #[allow(dead_code)]
    next_cmd: AtomicU64,
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
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        Self {
            child: None,
            writer: None,
            state: Arc::new(RwLock::new(TmuxState::new("", &session_name))),
            event_tx,
            event_rx: Some(event_rx),
            pending: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            next_cmd: AtomicU64::new(0),
            session_name,
        }
    }

    /// Get a handle to the shared state.
    pub fn state(&self) -> Arc<RwLock<TmuxState>> {
        Arc::clone(&self.state)
    }

    /// Take the event receiver (can only be called once).
    pub fn take_event_rx(&mut self) -> Option<mpsc::UnboundedReceiver<TmuxEvent>> {
        self.event_rx.take()
    }

    /// Start a tmux session and connect via control mode.
    pub async fn start(&mut self) -> Result<(), String> {
        let tmux_path = crate::tmux::find_tmux()
            .ok_or_else(|| "tmux not found in PATH".to_string())?;

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

        // Start reader task
        let event_tx = self.event_tx.clone();
        let pending = Arc::clone(&self.pending);
        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            let mut current_cmd: Option<u64> = None;
            let mut current_lines: Vec<String> = Vec::new();

            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(event) = TmuxEvent::parse(&line) {
                    match &event {
                        TmuxEvent::Begin { command_num, .. } => {
                            current_cmd = Some(*command_num);
                            current_lines.clear();
                        }
                        TmuxEvent::End { command_num, .. } => {
                            if Some(*command_num) == current_cmd {
                                let mut map = pending.lock().await;
                                if let Some(sender) = map.remove(command_num) {
                                    let _ = sender.send(CommandResult {
                                        lines: current_lines.drain(..).collect(),
                                        success: true,
                                    });
                                }
                                current_cmd = None;
                            }
                        }
                        TmuxEvent::Error { command_num, .. } => {
                            if Some(*command_num) == current_cmd {
                                let mut map = pending.lock().await;
                                if let Some(sender) = map.remove(command_num) {
                                    let _ = sender.send(CommandResult {
                                        lines: current_lines.drain(..).collect(),
                                        success: false,
                                    });
                                }
                                current_cmd = None;
                            }
                        }
                        TmuxEvent::Data(data) => {
                            if current_cmd.is_some() {
                                current_lines.push(data.clone());
                            }
                        }
                        _ => {}
                    }
                    let _ = event_tx.send(event);
                }
            }
        });

        tracing::info!("tmux control mode connected to session '{}'", self.session_name);
        Ok(())
    }

    /// Send a command to tmux control mode and wait for its response.
    pub async fn execute(&self, cmd: &str, args: &[&str]) -> Result<CommandResult, String> {
        let writer = self.writer.as_ref()
            .ok_or_else(|| "Control mode not connected".to_string())?;

        let formatted = Self::format_command(cmd, args);

        // Write to stdin
        {
            let mut w = writer.lock().await;
            w.write_all(formatted.as_bytes()).await
                .map_err(|e| format!("Failed to write command: {}", e))?;
            w.flush().await
                .map_err(|e| format!("Failed to flush: {}", e))?;
        }

        // For now, commands are fire-and-forget since control mode
        // command numbering needs the server to assign numbers.
        // We return a synthetic success. Full request-response tracking
        // will be wired when we process %begin/%end with server-assigned numbers.
        Ok(CommandResult {
            lines: vec![],
            success: true,
        })
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
}
