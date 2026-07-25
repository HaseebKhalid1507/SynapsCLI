use arc_swap::ArcSwap;
use ratatui::style::Color;
use std::sync::Arc;
use std::sync::LazyLock;

mod palettes;

/// Identifies a piece of high-value modal chrome for per-part style
/// resolution. Each variant maps to an optional `<modal>.border` /
/// `<modal>.title` override key in the user theme file. When no override is
/// set the resolver falls back to the shared base token, so the default look
/// is byte-identical to before per-part overrides existed.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum ModalKind {
    Settings,
    Plugins,
    Models,
}

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

    // --- Per-part chrome overrides (P19.1) --------------------------------
    // Optional overlays keyed off dotted TOML names (e.g. `settings.border`).
    // `None` is the default for every field, which means "resolve to the base
    // token" — so an unconfigured theme renders exactly as it did before these
    // fields existed. Only set when a user opts in per part.
    pub(crate) settings_border: Option<Color>,
    pub(crate) settings_title: Option<Color>,
    pub(crate) plugins_border: Option<Color>,
    pub(crate) plugins_title: Option<Color>,
    pub(crate) models_border: Option<Color>,
    pub(crate) models_title: Option<Color>,
    /// Resting tint of the sidecar header pill (idle + unarmed). `None` falls
    /// back to `muted`, the color the pill uses today.
    pub(crate) sidecar_pill: Option<Color>,

    // --- Namespaced extension tokens (P19.2) -------------------------------
    /// User-TOML overrides for extension theme tokens, keyed by the full
    /// dotted name `ext.<plugin-id>.<token>`. Parsed by `set` from any theme
    /// file line whose key starts with `ext.`. These always WIN over
    /// manifest-declared values (see [`Theme::ext_token`]). Empty by default,
    /// so themes without `ext.*` keys are unaffected.
    pub(crate) ext_overrides: std::collections::HashMap<String, Color>,
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

            // Tool styling — accent colours default to Reset (sentinel: derive
            // from this theme's own palette via tool_accent()).  Only
            // night-city sets explicit neon values.  Backgrounds always
            // auto-derive from `bg`.
            tool_bash: Color::Reset,
            tool_read: Color::Reset,
            tool_write: Color::Reset,
            tool_edit: Color::Reset,
            tool_grep: Color::Reset,
            tool_find: Color::Reset,
            tool_ls: Color::Reset,
            tool_subagent: Color::Reset,
            tool_ext: Color::Reset,
            tool_generic: Color::Reset,
            tool_input_bg: Color::Reset,
            tool_output_bg: Color::Reset,

            // Per-part overrides are absent by default => resolvers fall back
            // to base tokens => zero visual change across all 18 palettes.
            settings_border: None,
            settings_title: None,
            plugins_border: None,
            plugins_title: None,
            models_border: None,
            models_title: None,
            sidecar_pill: None,

            // No user overrides for extension tokens by default.
            ext_overrides: std::collections::HashMap::new(),
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
                if line.starts_with('#') || line.is_empty() {
                    continue;
                }
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

            // Per-part chrome overrides (dotted keys). Present => Some(c).
            "settings.border" => self.settings_border = Some(c),
            "settings.title" => self.settings_title = Some(c),
            "plugins.border" => self.plugins_border = Some(c),
            "plugins.title" => self.plugins_title = Some(c),
            "models.border" => self.models_border = Some(c),
            "models.title" => self.models_title = Some(c),
            "sidecar.pill" => self.sidecar_pill = Some(c),

            // Extension token overrides (P19.2): any `ext.<id>.<token>` key
            // is stored verbatim; resolution happens in `ext_token`.
            k if k.starts_with("ext.") => {
                self.ext_overrides.insert(k.to_string(), c);
            }
            _ => {} // unknown key, ignore
        }
    }
}

/// Per-part style resolvers (P19.1).
///
/// Resolution rule: **part override if present, else the base token**. The
/// border resolver always returns a concrete `Color` (base = `border_active`)
/// so the call site is unconditional and identical to today when unset. The
/// title resolver returns `Option<Color>`: today no modal sets a title style,
/// so the call site applies `.title_style(..)` ONLY when `Some`, keeping the
/// unset path byte-identical.
impl Theme {
    /// Border color for a modal's outer block. Falls back to `border_active`.
    pub(crate) fn modal_border(&self, kind: ModalKind) -> Color {
        let part = match kind {
            ModalKind::Settings => self.settings_border,
            ModalKind::Plugins => self.plugins_border,
            ModalKind::Models => self.models_border,
        };
        part.unwrap_or(self.border_active)
    }

    /// Optional title color for a modal's block. `None` => leave the title
    /// unstyled exactly as today (do not call `.title_style`).
    pub(crate) fn modal_title(&self, kind: ModalKind) -> Option<Color> {
        match kind {
            ModalKind::Settings => self.settings_title,
            ModalKind::Plugins => self.plugins_title,
            ModalKind::Models => self.models_title,
        }
    }

    /// Resting tint of the sidecar header pill. Falls back to `muted`.
    pub(crate) fn sidecar_pill_color(&self) -> Color {
        self.sidecar_pill.unwrap_or(self.muted)
    }

    /// Resolve a namespaced extension theme token `ext.<ext_id>.<token>`
    /// (P19.2). Resolution order: user theme-TOML override (in
    /// `ext_overrides`) → extension manifest declaration (in the load-time
    /// registry) → `None`. Extension-rendered surfaces treat `None` as "use
    /// whatever base token you use today", so extensions that declare no
    /// tokens — and users that set no overrides — see zero change.
    pub(crate) fn ext_token(&self, ext_id: &str, token: &str) -> Option<Color> {
        let key = format!("ext.{ext_id}.{token}");
        if let Some(c) = self.ext_overrides.get(&key) {
            return Some(*c);
        }
        EXT_DECLARED_TOKENS
            .read()
            .ok()
            .and_then(|map| map.get(&key).copied())
    }
}

/// Extension-declared theme tokens, keyed `ext.<plugin-id>.<token>` (P19.2).
///
/// Lives OUTSIDE the `Theme` value on purpose: the theme in [`THEME`] is
/// rebuilt wholesale on palette switches (`set_theme`), and manifest-declared
/// tokens must survive that — they are a property of the loaded extensions,
/// not of the palette. User overrides, by contrast, live inside `Theme`
/// (parsed from the theme file) and therefore correctly reload with it.
static EXT_DECLARED_TOKENS: LazyLock<std::sync::RwLock<std::collections::HashMap<String, Color>>> =
    LazyLock::new(|| std::sync::RwLock::new(std::collections::HashMap::new()));

/// Merge one extension's manifest-declared theme tokens into the registry
/// under `ext.<ext_id>.<token>` (P19.2). Values are `#rgb`/`#rrggbb` hex;
/// unparseable entries are skipped (same forgiving posture as the theme-file
/// loader — the manifest validator upstream already rejects them at load).
/// Idempotent per (ext, token): reloading an extension re-registers cleanly.
pub(crate) fn register_ext_theme_tokens<'a>(
    ext_id: &str,
    tokens: impl IntoIterator<Item = (&'a str, &'a str)>,
) {
    if let Ok(mut map) = EXT_DECLARED_TOKENS.write() {
        for (token, value) in tokens {
            if let Some(color) = parse_hex_color(value) {
                map.insert(format!("ext.{ext_id}.{token}"), color);
            }
        }
    }
}

/// Drop all declared tokens for one extension (unload hygiene).
#[allow(dead_code)]
pub(crate) fn clear_ext_theme_tokens(ext_id: &str) {
    if let Ok(mut map) = EXT_DECLARED_TOKENS.write() {
        let prefix = format!("ext.{ext_id}.");
        map.retain(|k, _| !k.starts_with(&prefix));
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
            if line.starts_with('#') || line.is_empty() {
                continue;
            }
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

    // No theme configured: use the default palette.
    Theme::default()
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
    fn tool_styling_defaults_to_reset() {
        // Neon is NOT in Default — tool_* fields are Reset (derive-from-palette sentinel).
        let t = Theme::default();
        assert_eq!(t.tool_bash, Color::Reset);
        assert_eq!(t.tool_read, Color::Reset);
        assert_eq!(t.tool_generic, Color::Reset);
        assert_eq!(t.tool_input_bg, Color::Reset);
        assert_eq!(t.tool_output_bg, Color::Reset);
    }

    #[test]
    fn tool_styling_is_themeable() {
        let mut t = Theme::default();
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
    fn night_city_has_neon_tool_colors() {
        // night-city is the ONLY theme with explicit neon tool accents.
        let t = Theme::builtin("night-city").expect("night-city exists");
        assert_eq!(t.tool_bash, Color::Rgb(108, 240, 122));
        assert_eq!(t.tool_read, Color::Rgb(34, 211, 238));
        assert_eq!(t.tool_write, Color::Rgb(255, 46, 136));
        assert_eq!(t.tool_ext, Color::Rgb(252, 214, 70));
    }

    #[test]
    fn builtin_palettes_use_reset_tool_colors() {
        // Other palettes should NOT inherit neon — they get Reset via Default.
        let t = Theme::builtin("dracula").expect("dracula exists");
        assert_eq!(t.tool_bash, Color::Reset);
        assert_eq!(t.tool_input_bg, Color::Reset);
    }

    // ---- Per-part chrome overrides (P19.1) --------------------------------

    #[test]
    fn per_part_overrides_absent_by_default() {
        // The whole point: no part keys => every Option is None => resolvers
        // fall back to base tokens => frames are byte-identical to today.
        let t = Theme::default();
        assert_eq!(t.settings_border, None);
        assert_eq!(t.settings_title, None);
        assert_eq!(t.plugins_border, None);
        assert_eq!(t.plugins_title, None);
        assert_eq!(t.models_border, None);
        assert_eq!(t.models_title, None);
        assert_eq!(t.sidecar_pill, None);
    }

    #[test]
    fn modal_border_falls_back_to_base_token_when_unset() {
        // Regression guard: absent part key => resolver returns border_active,
        // identical to the value every modal used before P19.1.
        let t = Theme::default();
        assert_eq!(t.modal_border(ModalKind::Settings), t.border_active);
        assert_eq!(t.modal_border(ModalKind::Plugins), t.border_active);
        assert_eq!(t.modal_border(ModalKind::Models), t.border_active);
    }

    #[test]
    fn modal_title_is_none_when_unset() {
        // None => call sites skip `.title_style` => title unstyled as today.
        let t = Theme::default();
        assert_eq!(t.modal_title(ModalKind::Settings), None);
        assert_eq!(t.modal_title(ModalKind::Plugins), None);
        assert_eq!(t.modal_title(ModalKind::Models), None);
    }

    #[test]
    fn sidecar_pill_falls_back_to_muted_when_unset() {
        let t = Theme::default();
        assert_eq!(t.sidecar_pill_color(), t.muted);
    }

    #[test]
    fn fallback_holds_across_all_18_palettes() {
        // Every built-in palette, with no part overrides, must resolve each
        // part to its base token — the "zero regression across 18 palettes"
        // guarantee, asserted directly against the resolvers.
        // The 18 named palettes plus the built-in `default` (19 total).
        const NAMES: [&str; 19] = [
            "default",
            "night-city",
            "neon-rain",
            "amber",
            "phosphor",
            "solarized-dark",
            "blood",
            "ocean",
            "rose-pine",
            "nord",
            "dracula",
            "monokai",
            "gruvbox",
            "catppuccin",
            "tokyo-night",
            "sunset",
            "ice",
            "forest",
            "lavender",
        ];
        for name in NAMES {
            let t = Theme::builtin(name).unwrap_or_else(|| panic!("{name} exists"));
            for kind in [ModalKind::Settings, ModalKind::Plugins, ModalKind::Models] {
                assert_eq!(
                    t.modal_border(kind),
                    t.border_active,
                    "{name}: {kind:?} border must fall back to border_active"
                );
                assert_eq!(
                    t.modal_title(kind),
                    None,
                    "{name}: {kind:?} title must be None"
                );
            }
            assert_eq!(
                t.sidecar_pill_color(),
                t.muted,
                "{name}: pill falls back to muted"
            );
        }
    }

    #[test]
    fn settings_border_distinct_from_plugins_border() {
        // Acceptance (a): a user TOML setting settings.border != plugins.border
        // makes the resolver return *different* colors — so the two modals
        // render distinctly. Parsed through the real `set`/dotted-key path.
        let mut t = Theme::default();
        t.set("settings.border", Color::Rgb(0xAA, 0x00, 0x00));
        t.set("plugins.border", Color::Rgb(0x00, 0x00, 0xBB));
        let sb = t.modal_border(ModalKind::Settings);
        let pb = t.modal_border(ModalKind::Plugins);
        assert_eq!(sb, Color::Rgb(0xAA, 0x00, 0x00));
        assert_eq!(pb, Color::Rgb(0x00, 0x00, 0xBB));
        assert_ne!(sb, pb, "settings border must differ from plugins border");
        // Models was NOT overridden => still falls back to base token.
        assert_eq!(t.modal_border(ModalKind::Models), t.border_active);
    }

    #[test]
    fn per_part_keys_parse_from_theme_file_lines() {
        // Exercise the same dotted-key path the TOML loader uses.
        let mut t = Theme::default();
        t.set("settings.title", Color::Rgb(1, 2, 3));
        t.set("plugins.title", Color::Rgb(4, 5, 6));
        t.set("models.border", Color::Rgb(7, 8, 9));
        t.set("sidecar.pill", Color::Rgb(10, 11, 12));
        assert_eq!(
            t.modal_title(ModalKind::Settings),
            Some(Color::Rgb(1, 2, 3))
        );
        assert_eq!(t.modal_title(ModalKind::Plugins), Some(Color::Rgb(4, 5, 6)));
        assert_eq!(t.modal_border(ModalKind::Models), Color::Rgb(7, 8, 9));
        assert_eq!(t.sidecar_pill_color(), Color::Rgb(10, 11, 12));
        // Unknown dotted keys are still ignored, not panics.
        t.set("bogus.key", Color::Rgb(0, 0, 0));
    }

    // ---- Namespaced extension tokens (P19.2) -------------------------------
    // NOTE: the declared-token registry is a process-wide static; every test
    // below uses a unique ext id so tests stay independent under parallel run.

    #[test]
    fn ext_token_is_none_when_nothing_declared() {
        // Baseline: no manifest declaration, no user override => None.
        // This is the "existing extensions unaffected" guarantee.
        let t = Theme::default();
        assert_eq!(t.ext_token("p192-none-ext", "accent"), None);
    }

    #[test]
    fn manifest_declared_token_resolves() {
        // Acceptance: an extension ships a token; it resolves via ext_token.
        register_ext_theme_tokens("p192-decl-ext", [("accent", "#22d3ee")]);
        let t = Theme::default();
        assert_eq!(
            t.ext_token("p192-decl-ext", "accent"),
            Some(Color::Rgb(0x22, 0xd3, 0xee))
        );
        // A token the extension did NOT declare stays None.
        assert_eq!(t.ext_token("p192-decl-ext", "other"), None);
    }

    #[test]
    fn user_toml_override_wins_over_manifest() {
        // Acceptance: user theme TOML `ext.<id>.<token>` beats the manifest.
        register_ext_theme_tokens("p192-override-ext", [("accent", "#111111")]);
        let mut t = Theme::default();
        // Same dotted-key path the theme-file loader uses.
        t.set("ext.p192-override-ext.accent", Color::Rgb(0xAA, 0xBB, 0xCC));
        assert_eq!(
            t.ext_token("p192-override-ext", "accent"),
            Some(Color::Rgb(0xAA, 0xBB, 0xCC)),
            "user override must win over the manifest-declared value"
        );
    }

    #[test]
    fn user_override_resolves_even_without_declaration() {
        // A user may theme a token the extension never declared — still works.
        let mut t = Theme::default();
        t.set("ext.p192-useronly-ext.badge", Color::Rgb(1, 2, 3));
        assert_eq!(
            t.ext_token("p192-useronly-ext", "badge"),
            Some(Color::Rgb(1, 2, 3))
        );
    }

    #[test]
    fn declared_tokens_survive_palette_switch() {
        // The registry lives outside Theme, so switching palettes (a fresh
        // Theme value) must not lose manifest-declared tokens.
        register_ext_theme_tokens("p192-switch-ext", [("accent", "#fa0")]);
        let fresh = Theme::builtin("dracula").expect("dracula exists");
        assert_eq!(
            fresh.ext_token("p192-switch-ext", "accent"),
            Some(Color::Rgb(0xff, 0xaa, 0x00)), // #fa0 short-form expansion
        );
    }

    #[test]
    fn malformed_declared_values_are_skipped() {
        // register_ext_theme_tokens is forgiving: bad hex never lands.
        register_ext_theme_tokens("p192-badhex-ext", [("accent", "not-a-color")]);
        let t = Theme::default();
        assert_eq!(t.ext_token("p192-badhex-ext", "accent"), None);
    }

    #[test]
    fn clear_ext_theme_tokens_removes_only_that_extension() {
        register_ext_theme_tokens("p192-clear-a", [("accent", "#111111")]);
        register_ext_theme_tokens("p192-clear-b", [("accent", "#222222")]);
        clear_ext_theme_tokens("p192-clear-a");
        let t = Theme::default();
        assert_eq!(t.ext_token("p192-clear-a", "accent"), None);
        assert_eq!(
            t.ext_token("p192-clear-b", "accent"),
            Some(Color::Rgb(0x22, 0x22, 0x22))
        );
    }

    #[test]
    fn hello_ext_demo_token_resolves_and_user_override_wins() {
        // The P19.2 acceptance vehicle: hello-ext (examples/extensions/
        // hello-ext) declares `accent = #22d3ee` in its manifest. Simulate
        // exactly what the extension-loader arm does at load, then apply a
        // user TOML override and confirm it wins.
        register_ext_theme_tokens("hello-ext", [("accent", "#22d3ee")]);
        let mut t = Theme::default();
        assert_eq!(
            t.ext_token("hello-ext", "accent"),
            Some(Color::Rgb(0x22, 0xd3, 0xee)),
            "hello-ext's manifest token must resolve"
        );
        t.set("ext.hello-ext.accent", Color::Rgb(0xff, 0x00, 0xff));
        assert_eq!(
            t.ext_token("hello-ext", "accent"),
            Some(Color::Rgb(0xff, 0x00, 0xff)),
            "user TOML `ext.hello-ext.accent` must override the manifest"
        );
    }
}
