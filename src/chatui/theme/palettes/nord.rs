use ratatui::style::Color;
use super::super::Theme;

impl Theme {
/// Built-in theme: "nord" — Arctic frost palette inspired by polar nights
pub(in crate::chatui::theme) fn nord() -> Self {
    Self {
        code_fg: Color::Rgb(136, 192, 208),
        code_bg: Color::Rgb(18, 20, 25),
        heading_color: Color::Rgb(129, 161, 193),
        quote_color: Color::Rgb(94, 129, 172),
        list_bullet_color: Color::Rgb(163, 190, 140),
        table_border_color: Color::Rgb(45, 50, 65),
        table_header_color: Color::Rgb(129, 161, 193),
        table_cell_color: Color::Rgb(216, 222, 233),

        bg: Color::Rgb(16, 18, 22),
        border: Color::Rgb(35, 40, 50),
        border_active: Color::Rgb(129, 161, 193),
        muted: Color::Rgb(75, 85, 105),

        user_color: Color::Rgb(236, 239, 244),
        user_bg: Color::Rgb(22, 25, 30),
        claude_label: Color::Rgb(136, 192, 208),
        claude_text: Color::Rgb(216, 222, 233),
        thinking_color: Color::Rgb(55, 65, 85),
        tool_label: Color::Rgb(129, 161, 193),
        tool_param: Color::Rgb(94, 129, 172),
        tool_result_color: Color::Rgb(180, 142, 173),
        tool_result_ok: Color::Rgb(136, 192, 208),
        error_color: Color::Rgb(191, 97, 106),
        warning_color: Color::Rgb(235, 203, 139),

        header_fg: Color::Rgb(129, 161, 193),
        status_streaming: Color::Rgb(163, 190, 140),
        status_ready: Color::Rgb(136, 192, 208),
        help_fg: Color::Rgb(45, 55, 75),
        input_fg: Color::Rgb(236, 239, 244),
        prompt_fg: Color::Rgb(129, 161, 193),
        separator: Color::Rgb(28, 32, 40),
        cost_color: Color::Rgb(163, 190, 140),

        subagent_border: Color::Rgb(65, 75, 95),
        subagent_name: Color::Rgb(129, 161, 193),
        subagent_status: Color::Rgb(180, 142, 173),
        subagent_done: Color::Rgb(136, 192, 208),
        subagent_time: Color::Rgb(94, 129, 172),
        event_icon: Color::Rgb(255, 180, 50),
        event_source: Color::Rgb(120, 180, 255),
        event_text: Color::Rgb(200, 200, 210),
        event_critical: Color::Rgb(255, 80, 80),
    }
}
}
