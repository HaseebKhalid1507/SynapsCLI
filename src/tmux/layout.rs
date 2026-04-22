//! Layout preset logic for tmux pane arrangements.

use super::config::LayoutPreset;

/// Generate the tmux commands needed to apply a layout preset.
/// `self_pane` is the Synaps TUI pane ID, `split_ratio` is the TUI width percentage.
pub fn layout_commands(preset: &LayoutPreset, self_pane: &str, split_ratio: u32) -> Vec<String> {
    match preset {
        LayoutPreset::Split => {
            let shell_pct = 100 - split_ratio;
            vec![
                format!("split-window -h -t {} -l {}% -d", self_pane, shell_pct),
            ]
        }
        LayoutPreset::Fullscreen => {
            // No splits — TUI takes full window
            vec![]
        }
        LayoutPreset::Tiled => {
            vec![
                format!("select-layout -t {} tiled", self_pane),
            ]
        }
        LayoutPreset::Custom(layout_string) => {
            vec![
                format!("select-layout -t {} {}", self_pane, layout_string),
            ]
        }
    }
}

/// Get the next layout preset in the cycle.
pub fn next_preset(current: &LayoutPreset) -> LayoutPreset {
    match current {
        LayoutPreset::Split => LayoutPreset::Fullscreen,
        LayoutPreset::Fullscreen => LayoutPreset::Tiled,
        LayoutPreset::Tiled => LayoutPreset::Split,
        LayoutPreset::Custom(_) => LayoutPreset::Split,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_layout_commands() {
        let cmds = layout_commands(&LayoutPreset::Split, "%0", 60);
        assert!(!cmds.is_empty());
        assert!(cmds.iter().any(|c| c.contains("split-window")));
        assert!(cmds.iter().any(|c| c.contains("40%")));
    }

    #[test]
    fn test_fullscreen_layout_no_split() {
        let cmds = layout_commands(&LayoutPreset::Fullscreen, "%0", 60);
        assert!(cmds.is_empty());
    }

    #[test]
    fn test_tiled_layout_commands() {
        let cmds = layout_commands(&LayoutPreset::Tiled, "%0", 50);
        assert!(cmds.iter().any(|c| c.contains("tiled")));
    }

    #[test]
    fn test_custom_layout() {
        let cmds = layout_commands(&LayoutPreset::Custom("my-layout".to_string()), "%0", 50);
        assert!(cmds.iter().any(|c| c.contains("my-layout")));
    }

    #[test]
    fn test_next_preset_cycle() {
        assert_eq!(next_preset(&LayoutPreset::Split), LayoutPreset::Fullscreen);
        assert_eq!(next_preset(&LayoutPreset::Fullscreen), LayoutPreset::Tiled);
        assert_eq!(next_preset(&LayoutPreset::Tiled), LayoutPreset::Split);
        assert_eq!(next_preset(&LayoutPreset::Custom("x".to_string())), LayoutPreset::Split);
    }
}
