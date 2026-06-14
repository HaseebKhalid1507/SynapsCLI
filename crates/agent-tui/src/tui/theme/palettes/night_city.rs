use ratatui::style::Color;
use super::super::Theme;

impl Theme {
/// Built-in theme: "night-city" — premium neon-noir.
/// Inspiration: Cyberpunk 2077 (cyan/yellow neon), Akira (red), Blade Runner
/// (amber haze on near-black), Neuromancer (console green). Tuned for calm:
/// near-black base, lots of dim secondary text, neon used only where it means
/// something so the eye can rest.
pub(in crate::tui::theme) fn night_city() -> Self {
    // Neon accent set
    const CYAN: Color = Color::Rgb(34, 211, 238); // cyber-cyan
    const MAGENTA: Color = Color::Rgb(255, 46, 136); // hot magenta
    const AMBER: Color = Color::Rgb(255, 179, 71); // Blade Runner amber / CRT
    const LIME: Color = Color::Rgb(108, 240, 122); // Neuromancer console green
    const VIOLET: Color = Color::Rgb(181, 133, 255);
    const YELLOW: Color = Color::Rgb(252, 214, 70); // cyberpunk electric yellow
    const RED: Color = Color::Rgb(255, 59, 71); // Akira red

    Self {
        // Markdown
        code_fg: Color::Rgb(200, 228, 240),
        code_bg: Color::Rgb(13, 15, 24),
        heading_color: CYAN,
        quote_color: Color::Rgb(96, 108, 134),
        list_bullet_color: AMBER,
        table_border_color: Color::Rgb(36, 42, 62),
        table_header_color: CYAN,
        table_cell_color: Color::Rgb(190, 200, 216),

        // Base — deep blue-black haze, not pure black
        bg: Color::Rgb(10, 11, 17),
        border: Color::Rgb(30, 35, 52),
        border_active: CYAN,
        muted: Color::Rgb(74, 82, 104),

        // Messages
        user_color: Color::Rgb(220, 228, 242),
        user_bg: Color::Rgb(15, 17, 27),
        claude_label: CYAN,
        claude_text: Color::Rgb(198, 207, 223),
        thinking_color: Color::Rgb(92, 84, 124),
        tool_label: CYAN, // generic fallback; per-tool accents override this
        tool_param: Color::Rgb(108, 118, 144),
        tool_result_color: Color::Rgb(132, 150, 180),
        tool_result_ok: LIME,
        error_color: RED,
        warning_color: AMBER,

        // UI chrome
        header_fg: CYAN,
        status_streaming: YELLOW,
        status_ready: LIME,
        help_fg: Color::Rgb(58, 66, 88),
        input_fg: Color::Rgb(220, 228, 242),
        prompt_fg: MAGENTA,
        separator: Color::Rgb(22, 26, 40),
        cost_color: AMBER,

        // Subagent panel
        subagent_border: Color::Rgb(58, 40, 92),
        subagent_name: VIOLET,
        subagent_status: Color::Rgb(120, 170, 210),
        subagent_done: LIME,
        subagent_time: Color::Rgb(90, 100, 126),

        // Event bus
        event_icon: AMBER,
        event_source: CYAN,
        event_text: Color::Rgb(205, 212, 226),
        event_critical: RED,
        // Night City neon tool accents — explicit so Default::default() stays clean.
        tool_bash: LIME,                          // Neuromancer console green
        tool_read: CYAN,                          // cyber-cyan
        tool_write: MAGENTA,                      // hot magenta
        tool_edit: AMBER,                         // Blade Runner amber
        tool_grep: Color::Rgb(90, 200, 220),      // muted teal
        tool_find: VIOLET,
        tool_ls: Color::Rgb(130, 160, 200),       // slate blue
        tool_subagent: Color::Rgb(210, 120, 230), // soft magenta
        tool_ext: YELLOW,                         // electric yellow
        tool_generic: Color::Rgb(132, 150, 180),  // dim steel
        ..Self::default()
    }
}
}
