use ratatui::style::Color;
use super::super::Theme;

impl Theme {
/// Built-in theme: "tokyo-night" — dark blue-purple theme with soft blue/purple/cyan accents
pub(in crate::chatui::theme) fn tokyo_night() -> Self {
    Self {
        code_fg: Color::Rgb(192, 202, 245),
        code_bg: Color::Rgb(36, 40, 59),
        heading_color: Color::Rgb(187, 154, 247),
        quote_color: Color::Rgb(86, 95, 137),
        list_bullet_color: Color::Rgb(125, 207, 255),
        table_border_color: Color::Rgb(41, 46, 66),
        table_header_color: Color::Rgb(122, 162, 247),
        table_cell_color: Color::Rgb(169, 177, 214),

        bg: Color::Rgb(26, 27, 38),
        border: Color::Rgb(41, 46, 66),
        border_active: Color::Rgb(122, 162, 247),
        muted: Color::Rgb(86, 95, 137),

        user_color: Color::Rgb(192, 202, 245),
        user_bg: Color::Rgb(36, 40, 59),
        claude_label: Color::Rgb(187, 154, 247),
        claude_text: Color::Rgb(169, 177, 214),
        thinking_color: Color::Rgb(86, 95, 137),
        tool_label: Color::Rgb(158, 206, 106),
        tool_param: Color::Rgb(255, 158, 100),
        tool_result_color: Color::Rgb(125, 207, 255),
        tool_result_ok: Color::Rgb(158, 206, 106),
        error_color: Color::Rgb(247, 118, 142),
        warning_color: Color::Rgb(224, 175, 104),

        header_fg: Color::Rgb(125, 207, 255),
        status_streaming: Color::Rgb(255, 158, 100),
        status_ready: Color::Rgb(158, 206, 106),
        help_fg: Color::Rgb(86, 95, 137),
        input_fg: Color::Rgb(192, 202, 245),
        prompt_fg: Color::Rgb(122, 162, 247),
        separator: Color::Rgb(52, 59, 88),
        cost_color: Color::Rgb(224, 175, 104),

        subagent_border: Color::Rgb(41, 46, 66),
        subagent_name: Color::Rgb(187, 154, 247),
        subagent_status: Color::Rgb(122, 162, 247),
        subagent_done: Color::Rgb(158, 206, 106),
        subagent_time: Color::Rgb(86, 95, 137),
        event_icon: Color::Rgb(255, 180, 50),
        event_source: Color::Rgb(120, 180, 255),
        event_text: Color::Rgb(200, 200, 210),
        event_critical: Color::Rgb(255, 80, 80),
    }
}
}
