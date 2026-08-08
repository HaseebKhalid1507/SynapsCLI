//! GamblersDen integration — casino subprocess spawning and terminal handoff.
use super::app::App;

/// Find the GamblersDen binary: check sibling to current exe, then $PATH, then dev path.
fn which_gamba() -> Option<std::path::PathBuf> {
    // 1. Check next to our own binary (bundled build)
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join("hidden");
            if sibling.exists() {
                return Some(sibling);
            }
        }
    }
    // 2. Check $PATH
    if let Ok(output) = std::process::Command::new("which").arg("hidden").output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Some(std::path::PathBuf::from(path));
            }
        }
    }
    // 3. Fallback: dev build path
    std::env::var("HOME")
        .ok()
        .map(|h| std::path::PathBuf::from(h).join("Projects/GamblersDen/target/release/hidden"))
        .filter(|p| p.exists())
}

impl App {
    pub(crate) fn restore_terminal(&self) {
        crossterm::terminal::enable_raw_mode().ok();
        let mut stdout = std::io::stdout();
        crossterm::execute!(
            stdout,
            crossterm::terminal::EnterAlternateScreen,
            // Mirror setup_terminal(): the casino subprocess disables mouse
            // capture + bracketed paste on its way out, so re-enable both or
            // the TUI loses scroll-wheel events (and paste) after returning.
            crossterm::event::EnableMouseCapture,
            crossterm::event::EnableBracketedPaste
        )
        .ok();
        // Re-push kitty keyboard protocol — the subprocess may have popped or
        // reset it, breaking modifier-heavy chords (Ctrl+Shift, Ctrl+Alt, etc.).
        // Best-effort, same as setup_terminal().
        let _ = crossterm::execute!(
            stdout,
            crossterm::event::PushKeyboardEnhancementFlags(
                crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | crossterm::event::KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
            )
        );
        // Hide the hardware cursor — setup_terminal() hides it for the whole
        // TUI lifecycle. Without this the cursor reappears at a stale position
        // and jitters during the first redraws after returning from the casino.
        crossterm::execute!(stdout, crossterm::cursor::Hide).ok();
    }

    /// Yield terminal to casino — tears down TUI, spawns GamblersDen.
    /// Returns Ok(()) if launched, Err(msg) if failed.
    pub(crate) fn launch_gamba(&mut self) -> std::result::Result<(), String> {
        if self.gamba_child.is_some() {
            return Err("🎰 Casino already running!".to_string());
        }
        let bin = which_gamba().ok_or_else(|| "🎰 Nothing to see here...".to_string())?;

        // Tear down our TUI — full inverse of setup_terminal() so the casino
        // subprocess inherits a clean cooked terminal.
        crossterm::terminal::disable_raw_mode().ok();
        let mut stdout = std::io::stdout();
        let _ = crossterm::execute!(stdout, crossterm::event::PopKeyboardEnhancementFlags);
        crossterm::execute!(
            stdout,
            crossterm::event::DisableBracketedPaste,
            crossterm::event::DisableMouseCapture,
            crossterm::cursor::Show,
            crossterm::terminal::LeaveAlternateScreen
        )
        .ok();
        // Spawn the casino (non-blocking)
        match std::process::Command::new(&bin)
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .spawn()
        {
            Ok(child) => {
                self.gamba_child = Some(child);
                Ok(())
            }
            Err(e) => {
                self.restore_terminal();
                Err(format!("Failed to launch casino: {}", e))
            }
        }
    }

    /// Kill the GamblersDen child process and reclaim the terminal.
    /// Returns a message to display, or None if no casino was running.
    pub(crate) fn reclaim_gamba(&mut self) -> Option<String> {
        if let Some(mut child) = self.gamba_child.take() {
            child.kill().ok();
            child.wait().ok();
            self.restore_terminal();
            Some("🎰 Back from the casino. Response ready.".to_string())
        } else {
            None
        }
    }

    /// Check if the GamblersDen child exited on its own (user quit the casino).
    /// Returns a message if it did.
    pub(crate) fn check_gamba_exited(&mut self) -> Option<String> {
        if let Some(ref mut child) = self.gamba_child {
            match child.try_wait() {
                Ok(Some(_status)) => {
                    self.gamba_child = None;
                    self.restore_terminal();
                    Some("🎰 Back from the casino. How'd you do, degen?".to_string())
                }
                _ => None,
            }
        } else {
            None
        }
    }
}
