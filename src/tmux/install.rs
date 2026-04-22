//! tmux installation flow when tmux is not found.

use std::io::{self, Write};

/// Return the sequence of shell commands to install tmux from source.
pub fn install_commands() -> Vec<String> {
    vec![
        "git clone https://github.com/tmux/tmux.git".to_string(),
        "cd tmux".to_string(),
        "sh autogen.sh".to_string(),
        "./configure && make".to_string(),
        "sudo make install".to_string(),
    ]
}

/// Print the install prompt and return true if user accepts.
pub fn prompt_install() -> bool {
    eprintln!("\x1b[1;31mError:\x1b[0m tmux is not installed.\n");
    eprintln!("tmux mode requires tmux to be available in your PATH.\n");
    eprintln!("Would you like to install tmux from source? [y/N]\n");
    eprintln!("This will run:");
    for cmd in install_commands() {
        eprintln!("  {}", cmd);
    }
    eprint!("\n> ");
    io::stderr().flush().ok();

    let mut input = String::new();
    if io::stdin().read_line(&mut input).is_ok() {
        return input.trim().eq_ignore_ascii_case("y");
    }
    false
}

/// Execute the install commands. Returns Ok(()) on success.
pub async fn run_install() -> Result<(), String> {
    use tokio::process::Command;

    let tmpdir = std::env::temp_dir().join("synaps-tmux-install");
    let _ = std::fs::remove_dir_all(&tmpdir);
    std::fs::create_dir_all(&tmpdir)
        .map_err(|e| format!("Failed to create temp dir: {}", e))?;

    eprintln!("\nCloning tmux...");
    let status = Command::new("git")
        .args(["clone", "https://github.com/tmux/tmux.git"])
        .current_dir(&tmpdir)
        .status()
        .await
        .map_err(|e| format!("git clone failed: {}", e))?;
    if !status.success() {
        return Err("git clone failed".to_string());
    }

    let tmux_dir = tmpdir.join("tmux");

    eprintln!("Running autogen.sh...");
    let status = Command::new("sh")
        .arg("autogen.sh")
        .current_dir(&tmux_dir)
        .status()
        .await
        .map_err(|e| format!("autogen.sh failed: {}", e))?;
    if !status.success() {
        return Err("autogen.sh failed. You may need: automake, autoconf, pkg-config".to_string());
    }

    eprintln!("Configuring and building...");
    let status = Command::new("sh")
        .args(["-c", "./configure && make"])
        .current_dir(&tmux_dir)
        .status()
        .await
        .map_err(|e| format!("configure/make failed: {}", e))?;
    if !status.success() {
        return Err("Build failed. You may need: libevent-dev, ncurses-dev".to_string());
    }

    eprintln!("Installing (may require sudo password)...");
    let status = Command::new("sudo")
        .args(["make", "install"])
        .current_dir(&tmux_dir)
        .status()
        .await
        .map_err(|e| format!("make install failed: {}", e))?;
    if !status.success() {
        return Err("make install failed".to_string());
    }

    let _ = std::fs::remove_dir_all(&tmpdir);

    if crate::tmux::find_tmux().is_some() {
        eprintln!("\n\x1b[1;32m✓\x1b[0m tmux installed successfully!");
        Ok(())
    } else {
        Err("tmux installed but not found in PATH".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_install_commands_are_valid() {
        let cmds = install_commands();
        assert_eq!(cmds.len(), 5);
        assert!(cmds[0].contains("git clone"));
        assert!(cmds[1].contains("cd tmux"));
        assert!(cmds[2].contains("autogen.sh"));
        assert!(cmds[3].contains("configure"));
        assert!(cmds[4].contains("make install"));
    }
}
