//! tmux integration configuration.

/// Layout preset for tmux pane arrangements.
#[derive(Debug, Clone, PartialEq)]
pub enum LayoutPreset {
    Split,
    Fullscreen,
    Tiled,
    Custom(String),
}

/// How subagents are displayed in tmux.
#[derive(Debug, Clone, PartialEq)]
pub enum SubagentDisplay {
    Window,
    Pane,
}

/// Configuration for tmux integration.
#[derive(Debug, Clone)]
pub struct TmuxConfig {
    pub enabled: bool,
    pub session_name: Option<String>,
    pub default_layout: LayoutPreset,
    pub subagent_display: SubagentDisplay,
    pub mouse: bool,
    pub split_ratio: u32,
    pub shell_position: String,
}

impl Default for TmuxConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            session_name: None,
            default_layout: LayoutPreset::Split,
            subagent_display: SubagentDisplay::Pane,
            mouse: true,
            split_ratio: 60,
            shell_position: "right".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tmux_config_default() {
        let config = TmuxConfig::default();
        assert!(!config.enabled);
        assert!(config.session_name.is_none());
        assert_eq!(config.default_layout, LayoutPreset::Split);
        assert_eq!(config.subagent_display, SubagentDisplay::Pane);
        assert!(config.mouse);
        assert_eq!(config.split_ratio, 60);
    }
}
