use ratatui::style::Color;
use super::super::Theme;

impl Theme {
/// Built-in theme: "neon-rain" — Cyberpunk/Akira/Blade Runner palette
pub(in crate::chatui::theme) fn neon_rain() -> Self {
    Self {
        code_fg: Color::Rgb(0, 240, 255),
        code_bg: Color::Rgb(10, 6, 18),
        heading_color: Color::Rgb(255, 46, 136),
        quote_color: Color::Rgb(106, 90, 122),
        list_bullet_color: Color::Rgb(252, 238, 10),
        table_border_color: Color::Rgb(48, 32, 74),
        table_header_color: Color::Rgb(255, 46, 136),
        table_cell_color: Color::Rgb(216, 210, 224),

        bg: Color::Rgb(8, 6, 12),
        border: Color::Rgb(30, 21, 48),
        border_active: Color::Rgb(255, 46, 136),
        muted: Color::Rgb(74, 58, 90),

        user_color: Color::Rgb(232, 224, 255),
        user_bg: Color::Rgb(13, 8, 24),
        claude_label: Color::Rgb(0, 240, 255),
        claude_text: Color::Rgb(216, 210, 224),
        thinking_color: Color::Rgb(58, 42, 74),
        tool_label: Color::Rgb(255, 46, 136),
        tool_param: Color::Rgb(106, 74, 122),
        tool_result_color: Color::Rgb(138, 154, 204),
        tool_result_ok: Color::Rgb(0, 240, 255),
        error_color: Color::Rgb(255, 23, 68),
        warning_color: Color::Rgb(252, 238, 10),

        header_fg: Color::Rgb(255, 46, 136),
        status_streaming: Color::Rgb(252, 238, 10),
        status_ready: Color::Rgb(0, 240, 255),
        help_fg: Color::Rgb(42, 26, 58),
        input_fg: Color::Rgb(232, 224, 255),
        prompt_fg: Color::Rgb(255, 46, 136),
        separator: Color::Rgb(26, 15, 40),
        cost_color: Color::Rgb(252, 238, 10),

        subagent_border: Color::Rgb(80, 20, 80),
        subagent_name: Color::Rgb(255, 46, 136),
        subagent_status: Color::Rgb(160, 120, 200),
        subagent_done: Color::Rgb(0, 240, 255),
        subagent_time: Color::Rgb(106, 90, 122),
        event_icon: Color::Rgb(255, 180, 50),
        event_source: Color::Rgb(120, 180, 255),
        event_text: Color::Rgb(200, 200, 210),
        event_critical: Color::Rgb(255, 80, 80),
    }
}
}
