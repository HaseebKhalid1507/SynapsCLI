use ratatui::style::Color;
use super::super::Theme;

impl Theme {
/// Built-in theme: "phosphor" — green monochrome CRT
pub(in crate::chatui::theme) fn phosphor() -> Self {
    Self {
        code_fg: Color::Rgb(50, 255, 80),
        code_bg: Color::Rgb(5, 15, 8),
        heading_color: Color::Rgb(80, 255, 120),
        quote_color: Color::Rgb(30, 100, 50),
        list_bullet_color: Color::Rgb(50, 220, 80),
        table_border_color: Color::Rgb(20, 60, 30),
        table_header_color: Color::Rgb(80, 255, 120),
        table_cell_color: Color::Rgb(60, 200, 90),

        bg: Color::Rgb(3, 8, 5),
        border: Color::Rgb(15, 40, 20),
        border_active: Color::Rgb(50, 255, 80),
        muted: Color::Rgb(25, 70, 35),

        user_color: Color::Rgb(60, 220, 90),
        user_bg: Color::Rgb(5, 14, 8),
        claude_label: Color::Rgb(80, 255, 120),
        claude_text: Color::Rgb(55, 200, 80),
        thinking_color: Color::Rgb(15, 50, 25),
        tool_label: Color::Rgb(50, 255, 80),
        tool_param: Color::Rgb(30, 120, 50),
        tool_result_color: Color::Rgb(40, 160, 60),
        tool_result_ok: Color::Rgb(50, 220, 80),
        error_color: Color::Rgb(255, 60, 60),
        warning_color: Color::Rgb(80, 255, 120),

        header_fg: Color::Rgb(50, 255, 80),
        status_streaming: Color::Rgb(80, 255, 120),
        status_ready: Color::Rgb(50, 220, 80),
        help_fg: Color::Rgb(15, 40, 20),
        input_fg: Color::Rgb(60, 220, 90),
        prompt_fg: Color::Rgb(50, 255, 80),
        separator: Color::Rgb(10, 25, 12),
        cost_color: Color::Rgb(80, 255, 120),

        subagent_border: Color::Rgb(20, 60, 30),
        subagent_name: Color::Rgb(50, 255, 80),
        subagent_status: Color::Rgb(40, 160, 60),
        subagent_done: Color::Rgb(80, 255, 120),
        subagent_time: Color::Rgb(30, 100, 50),
        event_icon: Color::Rgb(255, 180, 50),
        event_source: Color::Rgb(120, 180, 255),
        event_text: Color::Rgb(200, 200, 210),
        event_critical: Color::Rgb(255, 80, 80),
    }
}
}
