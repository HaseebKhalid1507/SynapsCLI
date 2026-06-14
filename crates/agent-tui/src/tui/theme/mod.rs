use arc_swap::ArcSwap;
use ratatui::style::Color;
use std::sync::Arc;
use std::sync::LazyLock;

mod palettes;

/// All colors used by the TUI, grouped so they can be overridden from a
/// user theme file. Defaults match the current built-in look.
///
/// Field names are what the theme file uses as keys. Unknown keys are
/// ignored; missing keys keep the default. Colors are written as `#rrggbb`
/// or `#rgb` hex.
pub(crate) struct Theme {
    // Markdown
    pub(crate) code_fg: Color,
    pub(crate) code_bg: Color,
    pub(crate) heading_color: Color,
    pub(crate) quote_color: Color,
    pub(crate) list_bullet_color: Color,
    pub(crate) table_border_color: Color,
    pub(crate) table_header_color: Color,
    pub(crate) table_cell_color: Color,

    // Base
    pub(crate) bg: Color,
    pub(crate) border: Color,
    pub(crate) border_active: Color,
    pub(crate) muted: Color,

    // Messages
    pub(crate) user_color: Color,
    pub(crate) user_bg: Color,
    pub(crate) claude_label: Color,
    pub(crate) claude_text: Color,
    pub(crate) thinking_color: Color,
    pub(crate) tool_label: Color,
    pub(crate) tool_param: Color,
    pub(crate) tool_result_color: Color,
    pub(crate) tool_result_ok: Color,
    pub(crate) error_color: Color,
    pub(crate) warning_color: Color,

    // UI chrome
    pub(crate) header_fg: Color,
    pub(crate) status_streaming: Color,
    pub(crate) status_ready: Color,
    pub(crate) help_fg: Color,
    pub(crate) input_fg: Color,
    pub(crate) prompt_fg: Color,
    pub(crate) separator: Color,
    pub(crate) cost_color: Color,

    // Subagent panel
    pub(crate) subagent_border: Color,
    pub(crate) subagent_name: Color,
    pub(crate) subagent_status: Color,
    pub(crate) subagent_done: Color,
    pub(crate) subagent_time: Color,

    // Event bus
    pub(crate) event_icon: Color,
    pub(crate) event_source: Color,
    pub(crate) event_text: Color,
    pub(crate) event_critical: Color,

    // Tool styling — per-tool gutter/accent colours + panel backgrounds.
    // `tool_input_bg`/`tool_output_bg` default to `Color::Reset`, which means
    // "auto-derive a subtle tint from `bg`"; set them in a theme to override.
    pub(crate) tool_bash: Color,
    pub(crate) tool_read: Color,
    pub(crate) tool_write: Color,
    pub(crate) tool_edit: Color,
    pub(crate) tool_grep: Color,
    pub(crate) tool_find: Color,
    pub(crate) tool_ls: Color,
    pub(crate) tool_subagent: Color,
    pub(crate) tool_ext: Color,
    pub(crate) tool_generic: Color,
    pub(crate) tool_input_bg: Color,
    pub(crate) tool_output_bg: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            code_fg: Color::Rgb(170, 210, 220),
            code_bg: Color::Rgb(14, 18, 24),
            heading_color: Color::Rgb(80, 210, 230),
            quote_color: Color::Rgb(85, 100, 120),
            list_bullet_color: Color::Rgb(50, 190, 210),
            table_border_color: Color::Rgb(35, 55, 70),
            table_header_color: Color::Rgb(80, 210, 230),
            table_cell_color: Color::Rgb(175, 185, 200),

            bg: Color::Rgb(10, 12, 18),
            border: Color::Rgb(28, 36, 50),
            border_active: Color::Rgb(50, 180, 210),
            muted: Color::Rgb(50, 58, 72),

            user_color: Color::Rgb(185, 195, 215),
            user_bg: Color::Rgb(16, 20, 30),
            claude_label: Color::Rgb(50, 200, 220),
            claude_text: Color::Rgb(192, 198, 210),
            thinking_color: Color::Rgb(45, 55, 75),
            tool_label: Color::Rgb(70, 170, 220),
            tool_param: Color::Rgb(65, 100, 135),
            tool_result_color: Color::Rgb(55, 120, 130),
            tool_result_ok: Color::Rgb(50, 175, 160),
            error_color: Color::Rgb(230, 70, 70),
            warning_color: Color::Rgb(220, 180, 60),

            header_fg: Color::Rgb(110, 125, 150),
            status_streaming: Color::Rgb(220, 175, 60),
            status_ready: Color::Rgb(50, 195, 190),
            help_fg: Color::Rgb(42, 52, 68),
            input_fg: Color::Rgb(188, 195, 210),
            prompt_fg: Color::Rgb(50, 180, 210),
            separator: Color::Rgb(24, 30, 42),
            cost_color: Color::Rgb(210, 170, 80),

            subagent_border: Color::Rgb(40, 45, 75),
            subagent_name: Color::Rgb(140, 130, 220),
            subagent_status: Color::Rgb(120, 140, 170),
            subagent_done: Color::Rgb(50, 195, 190),
            subagent_time: Color::Rgb(80, 95, 120),

            event_icon: Color::Rgb(255, 180, 50),
            event_source: Color::Rgb(120, 180, 255),
            event_text: Color::Rgb(200, 200, 210),
            event_critical: Color::Rgb(255, 80, 80),

            // Tool styling (Night City neon set; backgrounds auto-derive)
            tool_bash: Color::Rgb(108, 240, 122),
            tool_read: Color::Rgb(34, 211, 238),
            tool_write: Color::Rgb(255, 46, 136),
            tool_edit: Color::Rgb(255, 179, 71),
            tool_grep: Color::Rgb(90, 200, 220),
            tool_find: Color::Rgb(181, 133, 255),
            tool_ls: Color::Rgb(130, 160, 200),
            tool_subagent: Color::Rgb(210, 120, 230),
            tool_ext: Color::Rgb(252, 214, 70),
            tool_generic: Color::Rgb(132, 150, 180),
            tool_input_bg: Color::Reset,
            tool_output_bg: Color::Reset,
        }
    }
}

impl Theme {
    /// Dispatcher for builtin themes
    fn builtin(name: &str) -> Option<Self> {
        match name {
            "default" => Some(Self::default()),
            "night-city" => Some(Self::night_city()),
            "neon-rain" => Some(Self::neon_rain()),
            "amber" => Some(Self::amber()),
            "phosphor" => Some(Self::phosphor()),
            "solarized-dark" => Some(Self::solarized_dark()),
            "blood" => Some(Self::blood()),
            "ocean" => Some(Self::ocean()),
            "rose-pine" => Some(Self::rose_pine()),
            "nord" => Some(Self::nord()),
            "dracula" => Some(Self::dracula()),
            "monokai" => Some(Self::monokai()),
            "gruvbox" => Some(Self::gruvbox()),
            "catppuccin" => Some(Self::catppuccin()),
            "tokyo-night" => Some(Self::tokyo_night()),
            "sunset" => Some(Self::sunset()),
            "ice" => Some(Self::ice()),
            "forest" => Some(Self::forest()),
            "lavender" => Some(Self::lavender()),
            _ => None,
        }
    }

    /// Load theme from a TOML-like file. Unknown keys are ignored, missing
    /// keys retain defaults. Allows loading user themes.
    fn load_from(path: &std::path::Path) -> Self {
        let mut theme = Self::default();
        if let Ok(content) = std::fs::read_to_string(path) {
            for line in content.lines() {
                let line = line.trim();
                if line.starts_with('#') || line.is_empty() { continue; }
                if let Some((key, value)) = line.split_once('=') {
                    let key = key.trim();
                    let value = value.trim().trim_matches('"').trim_matches('\'');
                    if let Some(color) = parse_hex_color(value) {
                        theme.set(key, color);
                    }
                }
            }
        }
        theme
    }

    /// Sets a field by string name. Used by theme loading.
    fn set(&mut self, key: &str, c: Color) {
        match key {
            "code_fg" => self.code_fg = c,
            "code_bg" => self.code_bg = c,
            "heading_color" => self.heading_color = c,
            "quote_color" => self.quote_color = c,
            "list_bullet_color" => self.list_bullet_color = c,
            "table_border_color" => self.table_border_color = c,
            "table_header_color" => self.table_header_color = c,
            "table_cell_color" => self.table_cell_color = c,
            "bg" => self.bg = c,
            "border" => self.border = c,
            "border_active" => self.border_active = c,
            "muted" => self.muted = c,
            "user_color" => self.user_color = c,
            "user_bg" => self.user_bg = c,
            "claude_label" => self.claude_label = c,
            "claude_text" => self.claude_text = c,
            "thinking_color" => self.thinking_color = c,
            "tool_label" => self.tool_label = c,
            "tool_param" => self.tool_param = c,
            "tool_result_color" => self.tool_result_color = c,
            "tool_result_ok" => self.tool_result_ok = c,
            "error_color" => self.error_color = c,
            "warning_color" => self.warning_color = c,
            "header_fg" => self.header_fg = c,
            "status_streaming" => self.status_streaming = c,
            "status_ready" => self.status_ready = c,
            "help_fg" => self.help_fg = c,
            "input_fg" => self.input_fg = c,
            "prompt_fg" => self.prompt_fg = c,
            "separator" => self.separator = c,
            "cost_color" => self.cost_color = c,
            "subagent_border" => self.subagent_border = c,
            "subagent_name" => self.subagent_name = c,
            "subagent_status" => self.subagent_status = c,
            "subagent_done" => self.subagent_done = c,
            "subagent_time" => self.subagent_time = c,
            "tool_bash" => self.tool_bash = c,
            "tool_read" => self.tool_read = c,
            "tool_write" => self.tool_write = c,
            "tool_edit" => self.tool_edit = c,
            "tool_grep" => self.tool_grep = c,
            "tool_find" => self.tool_find = c,
            "tool_ls" => self.tool_ls = c,
            "tool_subagent" => self.tool_subagent = c,
            "tool_ext" => self.tool_ext = c,
            "tool_generic" => self.tool_generic = c,
            "tool_input_bg" => self.tool_input_bg = c,
            "tool_output_bg" => self.tool_output_bg = c,
            _ => {}, // unknown key, ignore
        }
    }
}

/// Parse `#rrggbb` or `#rgb` into a `Color::Rgb`. Returns `None` for anything
/// that doesn't match — malformed entries should be skipped, not crash.
fn parse_hex_color(s: &str) -> Option<Color> {
    let s = s.trim().trim_start_matches('#');
    match s.len() {
        6 => {
            let r = u8::from_str_radix(&s[0..2], 16).ok()?;
            let g = u8::from_str_radix(&s[2..4], 16).ok()?;
            let b = u8::from_str_radix(&s[4..6], 16).ok()?;
            Some(Color::Rgb(r, g, b))
        }
        3 => {
            let r = u8::from_str_radix(&s[0..1], 16).ok()?;
            let g = u8::from_str_radix(&s[1..2], 16).ok()?;
            let b = u8::from_str_radix(&s[2..3], 16).ok()?;
            Some(Color::Rgb(r * 17, g * 17, b * 17)) // 0xF -> 0xFF
        }
        _ => None,
    }
}

/// Global theme, loaded in this order:
/// 1. `~/.synaps-cli/theme` file (if exists) — overrides everything
/// 2. `theme = <name>` in config:
///    a. Check `~/.synaps-cli/themes/<name>` file first (user-editable)
///    b. Fall back to compiled-in builtin
/// 3. Falls back to default
pub(crate) fn load_theme_from_config() -> Theme {
    // First check for a theme file (highest priority)
    let path = synaps_cli::config::resolve_read_path("theme");
    if path.exists() {
        return Theme::load_from(&path);
    }

    // Then check config for a named built-in theme
    if let Ok(content) = std::fs::read_to_string(synaps_cli::config::resolve_read_path("config")) {
        for line in content.lines() {
            let line = line.trim();
            if line.starts_with('#') || line.is_empty() { continue; }
            if let Some((key, val)) = line.split_once('=') {
                if key.trim() == "theme" {
                    let name = val.trim();
                    let theme_file = synaps_cli::config::base_dir().join("themes").join(name);
                    if theme_file.exists() {
                        return Theme::load_from(&theme_file);
                    }
                    if let Some(theme) = Theme::builtin(name) {
                        return theme;
                    }
                }
            }
        }
    }

    // No theme file and no named theme in config: ship the premium default.
    Theme::night_city()
}

pub(crate) fn load_theme_by_name(name: &str) -> Option<Theme> {
    let theme_file = synaps_cli::config::base_dir().join("themes").join(name);
    if theme_file.exists() {
        return Some(Theme::load_from(&theme_file));
    }
    Theme::builtin(name)
}

pub(crate) static THEME: LazyLock<ArcSwap<Theme>> =
    LazyLock::new(|| ArcSwap::from_pointee(load_theme_from_config()));

pub(crate) fn set_theme(theme: Theme) {
    THEME.store(Arc::new(theme));
}
#[cfg(test)]
mod theme_tests {
    use super::*;

    #[test]
    fn tool_styling_is_themeable() {
        let mut t = Theme::default();
        // Defaults: Night City neon set; panel backgrounds auto-derive.
        assert_eq!(t.tool_bash, Color::Rgb(108, 240, 122));
        assert_eq!(t.tool_read, Color::Rgb(34, 211, 238));
        assert_eq!(t.tool_input_bg, Color::Reset);
        assert_eq!(t.tool_output_bg, Color::Reset);

        // A theme file can override each via its key name.
        t.set("tool_bash", Color::Rgb(1, 2, 3));
        t.set("tool_read", Color::Rgb(4, 5, 6));
        t.set("tool_input_bg", Color::Rgb(7, 8, 9));
        t.set("tool_output_bg", Color::Rgb(10, 11, 12));
        assert_eq!(t.tool_bash, Color::Rgb(1, 2, 3));
        assert_eq!(t.tool_read, Color::Rgb(4, 5, 6));
        assert_eq!(t.tool_input_bg, Color::Rgb(7, 8, 9));
        assert_eq!(t.tool_output_bg, Color::Rgb(10, 11, 12));
    }

    #[test]
    fn builtin_palettes_inherit_tool_defaults() {
        // A palette that doesn't set tool colours still gets the neon defaults
        // via `..Self::default()`.
        let t = Theme::builtin("dracula").expect("dracula exists");
        assert_eq!(t.tool_bash, Color::Rgb(108, 240, 122));
        assert_eq!(t.tool_input_bg, Color::Reset);
    }
}
