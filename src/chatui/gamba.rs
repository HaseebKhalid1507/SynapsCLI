use super::app::App;

/// Find the GamblersDen binary: check $PATH first, then the dev build path.
fn which_gamba() -> Option<std::path::PathBuf> {
    // 1. Check $PATH (works if user installed to /usr/local/bin, ~/.cargo/bin, etc.)
    if let Ok(output) = std::process::Command::new("which")
        .arg("gamblers-den")
        .output()
    {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Some(std::path::PathBuf::from(path));
            }
        }
    }
    // 2. Fallback: dev build path
    std::env::var("HOME").ok()
        .map(|h| std::path::PathBuf::from(h).join("Projects/GamblersDen/target/release/gamblers-den"))
        .filter(|p| p.exists())
}

impl App {
    pub(crate) fn restore_terminal(&self) {
        crossterm::terminal::enable_raw_mode().ok();
        crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::EnterAlternateScreen
        ).ok();
    }

    /// Yield terminal to casino — tears down TUI, spawns GamblersDen.
    /// Returns Ok(()) if launched, Err(msg) if failed.
    pub(crate) fn launch_gamba(&mut self) -> std::result::Result<(), String> {
        if self.gamba_child.is_some() {
            return Err("🎰 Casino already running!".to_string());
        }
        let bin = which_gamba().ok_or_else(|| {
            "GamblersDen binary not found. Install it to $PATH or ~/Projects/GamblersDen/target/release/".to_string()
        })?;

        // Tear down our TUI
        crossterm::terminal::disable_raw_mode().ok();
        crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::LeaveAlternateScreen
        ).ok();
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
