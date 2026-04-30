use ratatui::style::Color;
use super::super::Theme;

impl Theme {
/// Built-in theme: "gruvbox" — warm earthy tones with orange/yellow/aqua accents on dark background
pub(in crate::chatui::theme) fn gruvbox() -> Self {
    Self {
        code_fg: Color::Rgb(235, 219, 178),
        code_bg: Color::Rgb(60, 56, 54),
        heading_color: Color::Rgb(254, 128, 25),
        quote_color: Color::Rgb(146, 131, 116),
        list_bullet_color: Color::Rgb(250, 189, 47),
        table_border_color: Color::Rgb(80, 73, 69),
        table_header_color: Color::Rgb(142, 192, 124),
        table_cell_color: Color::Rgb(213, 196, 161),

        bg: Color::Rgb(40, 40, 40),
        border: Color::Rgb(80, 73, 69),
        border_active: Color::Rgb(254, 128, 25),
        muted: Color::Rgb(146, 131, 116),

        user_color: Color::Rgb(235, 219, 178),
        user_bg: Color::Rgb(50, 48, 47),
        claude_label: Color::Rgb(211, 134, 155),
        claude_text: Color::Rgb(213, 196, 161),
        thinking_color: Color::Rgb(146, 131, 116),
        tool_label: Color::Rgb(142, 192, 124),
        tool_param: Color::Rgb(250, 189, 47),
        tool_result_color: Color::Rgb(131, 165, 152),
        tool_result_ok: Color::Rgb(184, 187, 38),
        error_color: Color::Rgb(251, 73, 52),
        warning_color: Color::Rgb(250, 189, 47),

        header_fg: Color::Rgb(254, 128, 25),
        status_streaming: Color::Rgb(131, 165, 152),
        status_ready: Color::Rgb(184, 187, 38),
        help_fg: Color::Rgb(102, 92, 84),
        input_fg: Color::Rgb(235, 219, 178),
        prompt_fg: Color::Rgb(211, 134, 155),
        separator: Color::Rgb(60, 56, 54),
        cost_color: Color::Rgb(250, 189, 47),

        subagent_border: Color::Rgb(80, 73, 69),
        subagent_name: Color::Rgb(254, 128, 25),
        subagent_status: Color::Rgb(177, 98, 134),
        subagent_done: Color::Rgb(184, 187, 38),
        subagent_time: Color::Rgb(146, 131, 116),
        event_icon: Color::Rgb(255, 180, 50),
        event_source: Color::Rgb(120, 180, 255),
        event_text: Color::Rgb(200, 200, 210),
        event_critical: Color::Rgb(255, 80, 80),
    }
}
}
