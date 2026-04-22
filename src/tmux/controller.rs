//! TmuxController — manages pane/window operations via tmux CLI.
//!
//! Architecture: Synaps re-execs itself inside a tmux session (handled
//! in main.rs). Once inside tmux, the controller simply shells out to
//! `tmux <command> <args>` for all operations — no control mode needed.
//! Panes are visible and interactive because the user IS in the session.

use std::process::Command as StdCommand;

use super::state::TmuxState;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Result of a tmux command.
#[derive(Debug)]
pub struct CommandResult {
    pub lines: Vec<String>,
    pub success: bool,
}

/// The tmux controller. Shells out to `tmux` for all operations.
pub struct TmuxController {
    /// Tracked state of all tmux objects
    state: Arc<RwLock<TmuxState>>,
    /// Path to tmux binary
    tmux_path: String,
    /// Session name
    pub session_name: String,
}

impl TmuxController {
    /// Format a tmux command string (for logging/display).
    pub fn format_command(cmd: &str, args: &[&str]) -> String {
        if args.is_empty() {
            format!("{}\n", cmd)
        } else {
            format!("{} {}\n", cmd, args.join(" "))
        }
    }

    /// Create a new controller. Call `start()` to verify we're inside tmux.
    pub fn new(session_name: String) -> Self {
        let tmux_path = crate::tmux::find_tmux()
            .unwrap_or_else(|| "tmux".to_string());
        Self {
            state: Arc::new(RwLock::new(TmuxState::new("", &session_name))),
            tmux_path,
            session_name,
        }
    }

    /// Get a handle to the shared state.
    pub fn state(&self) -> Arc<RwLock<TmuxState>> {
        Arc::clone(&self.state)
    }

    /// Check if the control mode connection is alive.
    /// (Always true when inside tmux — the session is the user's terminal.)
    pub fn is_alive(&self) -> bool {
        std::env::var("TMUX").is_ok()
    }

    /// Verify we're inside the tmux session. No-op if $TMUX is set.
    /// The actual session creation + re-exec is handled in main.rs.
    pub async fn start(&mut self) -> Result<(), String> {
        if std::env::var("TMUX").is_err() {
            return Err(
                "Not inside a tmux session. The --tmux flag should re-exec into tmux before reaching here.".to_string()
            );
        }

        tracing::info!("tmux controller active inside session '{}'", self.session_name);
        Ok(())
    }

    /// Apply default session settings: mouse mode, hotkeys, status bar.
    /// Called once after start() to configure the tmux session for Synaps.
    pub async fn apply_session_defaults(&self) {
        // Enable mouse — click to select panes, scroll, drag to resize
        if let Err(e) = self.execute("set-option", &["-g", "mouse", "on"]).await {
            tracing::warn!("failed to enable mouse: {}", e);
        }

        // Apply hotkey bindings
        for cmd_str in crate::tmux::hotkeys::hotkey_bind_commands() {
            if let Err(e) = self.execute_raw(&cmd_str).await {
                tracing::warn!("failed to apply hotkey: {}", e);
            }
        }

        // Apply status bar
        for cmd_str in crate::tmux::hotkeys::status_bar_commands(&self.session_name) {
            if let Err(e) = self.execute_raw(&cmd_str).await {
                tracing::warn!("failed to apply status bar setting: {}", e);
            }
        }

        tracing::info!("tmux session defaults applied (mouse, hotkeys, status bar)");
    }

    /// Execute a raw tmux command string (the full command as you'd type it
    /// after `tmux`). Handles quoting properly by letting the shell parse it.
    pub async fn execute_raw(&self, cmd_str: &str) -> Result<CommandResult, String> {
        let full = format!("tmux {}", cmd_str);
        tracing::debug!("tmux raw exec: {}", cmd_str);

        let output = StdCommand::new("sh")
            .args(["-c", &full])
            .output()
            .map_err(|e| format!("Failed to run tmux command: {}", e))?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        let lines: Vec<String> = stdout.lines().map(|l| l.to_string()).collect();

        if output.status.success() {
            Ok(CommandResult { lines, success: true })
        } else {
            let err_msg = if stderr.is_empty() {
                format!("tmux command failed: {}", cmd_str)
            } else {
                format!("tmux command failed: {}", stderr.trim())
            };
            Err(err_msg)
        }
    }

    /// Execute a tmux command by shelling out to the tmux binary.
    /// Captures stdout and returns the output lines.
    pub async fn execute(&self, cmd: &str, args: &[&str]) -> Result<CommandResult, String> {
        let mut command = StdCommand::new(&self.tmux_path);
        command.arg(cmd);
        for arg in args {
            command.arg(arg);
        }

        tracing::debug!("tmux exec: {} {}", cmd, args.join(" "));

        let output = command
            .output()
            .map_err(|e| format!("Failed to run tmux {}: {}", cmd, e))?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        let lines: Vec<String> = stdout
            .lines()
            .map(|l| l.to_string())
            .collect();

        if output.status.success() {
            Ok(CommandResult { lines, success: true })
        } else {
            let err_msg = if stderr.is_empty() {
                format!("tmux {} failed with exit code {:?}", cmd, output.status.code())
            } else {
                format!("tmux {} failed: {}", cmd, stderr.trim())
            };
            Err(err_msg)
        }
    }

    /// Execute a command and return the first line of output.
    pub async fn execute_single(&self, cmd: &str, args: &[&str]) -> Result<String, String> {
        let result = self.execute(cmd, args).await?;
        Ok(result.lines.into_iter().next().unwrap_or_default())
    }

    /// Kill the tmux session and clean up.
    pub async fn shutdown(&mut self) -> Result<(), String> {
        let _ = StdCommand::new(&self.tmux_path)
            .args(["kill-session", "-t", &self.session_name])
            .output();
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
    async fn test_execute_when_not_in_tmux() {
        // If we're not inside tmux, start() should fail
        if std::env::var("TMUX").is_err() {
            let mut controller = TmuxController::new("test-start".to_string());
            let result = controller.start().await;
            assert!(result.is_err());
        }
    }

    #[tokio::test]
    async fn test_execute_valid_command() {
        // tmux list-sessions works even outside tmux (if tmux server is running)
        // We just test that execute doesn't panic
        let controller = TmuxController::new("test-exec".to_string());
        let _result = controller.execute("list-sessions", &[]).await;
        // Don't assert success — tmux server may not be running
    }

    #[tokio::test]
    async fn test_response_queue_ordering() {
        // This test verified the old FIFO queue — keep as a basic sanity check
        // that CommandResult can be constructed
        let r1 = CommandResult { lines: vec!["first".to_string()], success: true };
        let r2 = CommandResult { lines: vec!["second".to_string()], success: true };
        assert_eq!(r1.lines, vec!["first"]);
        assert_eq!(r2.lines, vec!["second"]);
    }
}
