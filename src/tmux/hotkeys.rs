//! Convenience hotkey bindings for Synaps tmux mode.

/// Generate the tmux bind-key commands for Synaps convenience keys.
pub fn hotkey_bind_commands() -> Vec<String> {
    vec![
        "bind-key -T prefix F resize-pane -Z".to_string(),
        "bind-key -T prefix S display-message 'Cycling subagent display...'".to_string(),
        "bind-key -T prefix L display-message 'Cycling layout...'".to_string(),
        "bind-key -T prefix N select-pane -t :.+".to_string(),
        "bind-key -T prefix P select-pane -t :.-".to_string(),
        "bind-key -T prefix H display-message 'Synaps: F=fullscreen S=subagent L=layout N/P=nav H=help'".to_string(),
    ]
}

/// Generate the mouse enable/disable command.
pub fn mouse_command(enabled: bool) -> String {
    if enabled {
        "set-option -g mouse on".to_string()
    } else {
        "set-option -g mouse off".to_string()
    }
}

/// Generate status bar format commands for Synaps.
pub fn status_bar_commands(session_name: &str) -> Vec<String> {
    vec![
        "set-option -g status-position bottom".to_string(),
        format!("set-option -g status-left '#[fg=cyan,bold][{}] '", session_name),
        "set-option -g status-right '#[fg=yellow]C-b H:help #[fg=white]%H:%M'".to_string(),
        "set-option -g status-style 'bg=black,fg=white'".to_string(),
        "set-option -g window-status-current-style 'fg=green,bold'".to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hotkey_commands_not_empty() {
        let cmds = hotkey_bind_commands();
        assert!(!cmds.is_empty());
    }

    #[test]
    fn test_hotkey_commands_contain_bind_key() {
        let cmds = hotkey_bind_commands();
        for cmd in &cmds {
            assert!(cmd.starts_with("bind-key"), "Expected bind-key command: {}", cmd);
        }
    }

    #[test]
    fn test_mouse_enable_command() {
        let cmd = mouse_command(true);
        assert!(cmd.contains("mouse"));
        assert!(cmd.contains("on"));

        let cmd = mouse_command(false);
        assert!(cmd.contains("mouse"));
        assert!(cmd.contains("off"));
    }

    #[test]
    fn test_status_bar_commands() {
        let cmds = status_bar_commands("my-project");
        assert!(cmds.iter().any(|c| c.contains("my-project")));
        assert!(cmds.iter().any(|c| c.contains("status-position")));
    }
}
