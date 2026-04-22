//! TmuxController — manages the control mode connection to tmux.
//!
//! tmux requires a real PTY even for control mode (`tmux -CC`).
//! We use `portable-pty` to provide one, then read/write the master fd
//! for the control mode protocol.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::{oneshot, RwLock};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

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
    /// Writer to the PTY master (sends commands to tmux CC stdin)
    writer: Option<Arc<std::sync::Mutex<Box<dyn Write + Send>>>>,
    /// Tracked state of all tmux objects
    state: Arc<RwLock<TmuxState>>,
    /// FIFO queue of pending command response waiters.
    response_queue: Arc<tokio::sync::Mutex<VecDeque<oneshot::Sender<CommandResult>>>>,
    /// Set to false when the reader task detects the control mode pipe has closed.
    alive: Arc<AtomicBool>,
    /// Child process killer handle
    child_killer: Option<Box<dyn portable_pty::ChildKiller + Send + Sync>>,
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
            writer: None,
            state: Arc::new(RwLock::new(TmuxState::new("", &session_name))),
            response_queue: Arc::new(tokio::sync::Mutex::new(VecDeque::new())),
            alive: Arc::new(AtomicBool::new(false)),
            child_killer: None,
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
    /// Spawns `tmux -CC new-session` inside a real PTY (tmux requires one
    /// even for control mode). Waits for the initial `%begin`/`%end`
    /// handshake before returning.
    pub async fn start(&mut self) -> Result<(), String> {
        let tmux_path = crate::tmux::find_tmux()
            .ok_or_else(|| "tmux not found in PATH".to_string())?;

        // Kill any stale session with the same name (clean slate model)
        let has_existing = std::process::Command::new(&tmux_path)
            .args(["has-session", "-t", &self.session_name])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        if has_existing {
            tracing::info!("killing stale tmux session '{}'", self.session_name);
            let _ = std::process::Command::new(&tmux_path)
                .args(["kill-session", "-t", &self.session_name])
                .status();
        }

        // Get terminal size
        let (cols, rows) = crossterm::terminal::size().unwrap_or((200, 50));

        // Open a PTY pair — tmux needs a real terminal even for -CC mode
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("Failed to open PTY for tmux: {}", e))?;

        // Build the command: tmux -CC new-session -s <name> -x <cols> -y <rows>
        let mut cmd = CommandBuilder::new(&tmux_path);
        cmd.args([
            "-CC", "new-session",
            "-s", &self.session_name,
            "-x", &cols.to_string(),
            "-y", &rows.to_string(),
        ]);
        cmd.env("TERM", "xterm-256color");

        // Spawn on the slave side of the PTY
        let mut child = pair.slave
            .spawn_command(cmd)
            .map_err(|e| format!("Failed to spawn tmux control mode: {}", e))?;

        // Drop the slave — the child process owns its end now
        drop(pair.slave);

        // Get writer (master → child stdin)
        let writer = pair.master
            .take_writer()
            .map_err(|e| format!("Failed to take PTY writer: {}", e))?;

        // Get reader (child stdout → master)
        let reader = pair.master
            .try_clone_reader()
            .map_err(|e| format!("Failed to clone PTY reader: {}", e))?;

        // Keep master alive so the PTY doesn't close
        // (portable-pty drops the fd when the master is dropped)
        std::mem::forget(pair.master);

        self.writer = Some(Arc::new(std::sync::Mutex::new(writer)));
        self.child_killer = Some(child.clone_killer());
        self.alive.store(true, Ordering::Relaxed);

        // Channel for readiness signal
        let (ready_tx, ready_rx) = oneshot::channel::<Result<(), String>>();

        // Spawn blocking reader task — reads control mode output from the PTY
        let response_queue = Arc::clone(&self.response_queue);
        let alive = Arc::clone(&self.alive);
        tokio::task::spawn_blocking(move || {
            let buf_reader = BufReader::new(reader);
            let mut current_waiter: Option<oneshot::Sender<CommandResult>> = None;
            let mut current_lines: Vec<String> = Vec::new();
            let mut in_command = false;
            let mut ready_tx = Some(ready_tx);
            let mut handshake_done = false;

            for line_result in buf_reader.lines() {
                let line = match line_result {
                    Ok(l) => l,
                    Err(_) => break,
                };

                // Strip DCS escape sequences that tmux sends on connect.
                // The DCS `\x1bP1000p` may be glued to the first real line
                // (no newline separator), so we strip it rather than skip.
                let line = if let Some(pos) = line.find("%begin") {
                    line[pos..].to_string()
                } else if let Some(pos) = line.find("%end") {
                    line[pos..].to_string()
                } else if let Some(pos) = line.find("%error") {
                    line[pos..].to_string()
                } else if let Some(pos) = line.find("%output") {
                    line[pos..].to_string()
                } else if let Some(pos) = line.find("%window") {
                    line[pos..].to_string()
                } else if let Some(pos) = line.find("%session") {
                    line[pos..].to_string()
                } else if let Some(pos) = line.find("%layout") {
                    line[pos..].to_string()
                } else if let Some(pos) = line.find("%pane") {
                    line[pos..].to_string()
                } else if line.starts_with('\x1b') || line.starts_with('\u{1b}') {
                    // Pure escape sequence with no tmux event — skip
                    continue;
                } else if line.is_empty() {
                    continue;
                } else {
                    line
                };

                if let Some(event) = TmuxEvent::parse(&line) {
                    match &event {
                        TmuxEvent::Begin { .. } => {
                            current_lines.clear();
                            in_command = true;

                            if handshake_done {
                                // Normal command — pop waiter from FIFO queue
                                // Use blocking approach: try_lock in a loop
                                let rt = tokio::runtime::Handle::current();
                                if let Ok(mut q) = rt.block_on(async {
                                    Ok::<_, ()>(response_queue.lock().await)
                                }) {
                                    current_waiter = q.pop_front();
                                }
                            }
                        }
                        TmuxEvent::End { .. } | TmuxEvent::Error { .. } => {
                            let success = matches!(&event, TmuxEvent::End { .. });

                            if in_command {
                                if !handshake_done {
                                    handshake_done = true;
                                    if let Some(tx) = ready_tx.take() {
                                        let _ = tx.send(Ok(()));
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

            // Reader exited — control mode is dead
            alive.store(false, Ordering::Relaxed);

            // Drain pending waiters
            let rt = tokio::runtime::Handle::current();
            let _ = rt.block_on(async {
                let mut q = response_queue.lock().await;
                while let Some(sender) = q.pop_front() {
                    let _ = sender.send(CommandResult {
                        lines: vec!["control mode disconnected".to_string()],
                        success: false,
                    });
                }
            });

            // If we never completed handshake, signal failure
            if let Some(tx) = ready_tx.take() {
                let _ = tx.send(Err("tmux control mode exited before handshake completed".to_string()));
            }

            // Wait for child to exit
            let _ = child.wait();

            tracing::debug!("tmux control mode reader exited");
        });

        // Wait for handshake with timeout
        match tokio::time::timeout(std::time::Duration::from_secs(5), ready_rx).await {
            Ok(Ok(Ok(()))) => {
                tracing::info!("tmux control mode connected to session '{}'", self.session_name);
                Ok(())
            }
            Ok(Ok(Err(e))) => {
                self.alive.store(false, Ordering::Relaxed);
                Err(e)
            }
            Ok(Err(_)) => {
                self.alive.store(false, Ordering::Relaxed);
                Err("tmux control mode channel dropped before handshake".to_string())
            }
            Err(_) => {
                self.alive.store(false, Ordering::Relaxed);
                Err(format!(
                    "tmux control mode timed out waiting for handshake (5s) — session '{}'",
                    self.session_name
                ))
            }
        }
    }

    /// Send a command to tmux control mode and wait for its response.
    pub async fn execute(&self, cmd: &str, args: &[&str]) -> Result<CommandResult, String> {
        if !self.alive.load(Ordering::Relaxed) {
            return Err("tmux control mode is not running".to_string());
        }

        let writer = self.writer.as_ref()
            .ok_or_else(|| "Control mode not connected".to_string())?;

        let formatted = Self::format_command(cmd, args);

        // Create oneshot for response
        let (tx, rx) = oneshot::channel();

        // Enqueue waiter BEFORE writing
        {
            let mut q = self.response_queue.lock().await;
            q.push_back(tx);
        }

        // Write to PTY (synchronous write via std::sync::Mutex)
        {
            let mut w = writer.lock()
                .map_err(|_| "Writer mutex poisoned".to_string())?;
            w.write_all(formatted.as_bytes())
                .map_err(|e| {
                    format!("Failed to write command: {} — control mode may have exited", e)
                })?;
            w.flush()
                .map_err(|e| {
                    format!("Failed to flush command: {} — control mode may have exited", e)
                })?;
        }

        // Wait for response
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
            let _ = std::process::Command::new(&tmux_path)
                .args(["kill-session", "-t", &self.session_name])
                .status();
        }
        if let Some(ref mut killer) = self.child_killer {
            let _ = killer.kill();
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
        let controller = TmuxController::new("test-dead".to_string());
        let result = controller.execute("list-windows", &[]).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not running"));
    }
}
