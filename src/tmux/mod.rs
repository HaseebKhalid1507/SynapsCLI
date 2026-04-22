//! tmux native integration — control mode, layouts, tools.

pub mod config;
pub mod protocol;
pub mod state;
pub mod controller;
pub mod layout;
pub mod hotkeys;
pub mod install;

pub use config::TmuxConfig;
pub use controller::TmuxController;

use std::process::Command as StdCommand;

/// Generate an auto session name based on git repo or directory name.
pub fn auto_session_name() -> String {
    // Try git repo name first
    if let Ok(output) = StdCommand::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
    {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if let Some(name) = std::path::Path::new(&path).file_name() {
                return format!("synaps-{}", name.to_string_lossy());
            }
        }
    }
    // Fall back to current directory name
    if let Ok(cwd) = std::env::current_dir() {
        if let Some(name) = cwd.file_name() {
            return format!("synaps-{}", name.to_string_lossy());
        }
    }
    // Last resort
    format!("synaps-{}", std::process::id())
}

/// Check if tmux is available in PATH.
pub fn find_tmux() -> Option<String> {
    StdCommand::new("which")
        .arg("tmux")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_auto_session_name_in_git_repo() {
        let name = super::auto_session_name();
        assert!(name.starts_with("synaps-"));
    }

    #[test]
    fn test_find_tmux_returns_option() {
        let _result = super::find_tmux();
    }
}
