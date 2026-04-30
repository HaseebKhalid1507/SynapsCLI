use ratatui::style::Color;
use super::super::Theme;

impl Theme {
/// Built-in theme: "blood" — dark red, Doom/horror aesthetic
pub(in crate::chatui::theme) fn blood() -> Self {
    Self {
        code_fg: Color::Rgb(255, 100, 80),
        code_bg: Color::Rgb(15, 5, 5),
        heading_color: Color::Rgb(255, 50, 50),
        quote_color: Color::Rgb(100, 50, 50),
        list_bullet_color: Color::Rgb(200, 60, 60),
        table_border_color: Color::Rgb(60, 20, 20),
        table_header_color: Color::Rgb(255, 50, 50),
        table_cell_color: Color::Rgb(200, 160, 160),

        bg: Color::Rgb(8, 3, 3),
        border: Color::Rgb(40, 15, 15),
        border_active: Color::Rgb(255, 50, 50),
        muted: Color::Rgb(80, 40, 40),

        user_color: Color::Rgb(220, 180, 180),
        user_bg: Color::Rgb(15, 5, 5),
        claude_label: Color::Rgb(255, 80, 60),
        claude_text: Color::Rgb(200, 170, 170),
        thinking_color: Color::Rgb(50, 25, 25),
        tool_label: Color::Rgb(255, 50, 50),
        tool_param: Color::Rgb(140, 70, 70),
        tool_result_color: Color::Rgb(180, 100, 80),
        tool_result_ok: Color::Rgb(200, 80, 60),
        error_color: Color::Rgb(255, 30, 30),
        warning_color: Color::Rgb(255, 150, 50),

        header_fg: Color::Rgb(255, 50, 50),
        status_streaming: Color::Rgb(255, 150, 50),
        status_ready: Color::Rgb(200, 80, 60),
        help_fg: Color::Rgb(50, 25, 25),
        input_fg: Color::Rgb(220, 180, 180),
        prompt_fg: Color::Rgb(255, 50, 50),
        separator: Color::Rgb(30, 10, 10),
        cost_color: Color::Rgb(255, 150, 50),

        subagent_border: Color::Rgb(60, 20, 20),
        subagent_name: Color::Rgb(255, 50, 50),
        subagent_status: Color::Rgb(160, 80, 80),
        subagent_done: Color::Rgb(200, 80, 60),
        subagent_time: Color::Rgb(100, 50, 50),
        event_icon: Color::Rgb(255, 180, 50),
        event_source: Color::Rgb(120, 180, 255),
        event_text: Color::Rgb(200, 200, 210),
        event_critical: Color::Rgb(255, 80, 80),
    }
}
}
