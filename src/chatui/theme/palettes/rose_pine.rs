use ratatui::style::Color;
use super::super::Theme;

impl Theme {
/// Built-in theme: "rose-pine" — Muted, elegant purples and pinks
pub(in crate::chatui::theme) fn rose_pine() -> Self {
    Self {
        code_fg: Color::Rgb(234, 154, 151),
        code_bg: Color::Rgb(15, 12, 18),
        heading_color: Color::Rgb(235, 111, 146),
        quote_color: Color::Rgb(144, 122, 169),
        list_bullet_color: Color::Rgb(156, 207, 216),
        table_border_color: Color::Rgb(45, 35, 55),
        table_header_color: Color::Rgb(235, 111, 146),
        table_cell_color: Color::Rgb(224, 222, 244),

        bg: Color::Rgb(13, 10, 16),
        border: Color::Rgb(35, 28, 42),
        border_active: Color::Rgb(235, 111, 146),
        muted: Color::Rgb(85, 75, 95),

        user_color: Color::Rgb(240, 237, 245),
        user_bg: Color::Rgb(18, 15, 22),
        claude_label: Color::Rgb(234, 154, 151),
        claude_text: Color::Rgb(224, 222, 244),
        thinking_color: Color::Rgb(65, 55, 75),
        tool_label: Color::Rgb(235, 111, 146),
        tool_param: Color::Rgb(144, 122, 169),
        tool_result_color: Color::Rgb(156, 207, 216),
        tool_result_ok: Color::Rgb(234, 154, 151),
        error_color: Color::Rgb(235, 111, 146),
        warning_color: Color::Rgb(246, 193, 119),

        header_fg: Color::Rgb(235, 111, 146),
        status_streaming: Color::Rgb(156, 207, 216),
        status_ready: Color::Rgb(234, 154, 151),
        help_fg: Color::Rgb(55, 45, 65),
        input_fg: Color::Rgb(240, 237, 245),
        prompt_fg: Color::Rgb(235, 111, 146),
        separator: Color::Rgb(25, 20, 32),
        cost_color: Color::Rgb(156, 207, 216),

        subagent_border: Color::Rgb(85, 65, 105),
        subagent_name: Color::Rgb(235, 111, 146),
        subagent_status: Color::Rgb(196, 167, 231),
        subagent_done: Color::Rgb(234, 154, 151),
        subagent_time: Color::Rgb(144, 122, 169),
        event_icon: Color::Rgb(255, 180, 50),
        event_source: Color::Rgb(120, 180, 255),
        event_text: Color::Rgb(200, 200, 210),
        event_critical: Color::Rgb(255, 80, 80),
    }
}
}
