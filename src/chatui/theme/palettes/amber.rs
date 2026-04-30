use ratatui::style::Color;
use super::super::Theme;

impl Theme {
/// Built-in theme: "amber" — warm CRT/retro terminal
pub(in crate::chatui::theme) fn amber() -> Self {
    Self {
        code_fg: Color::Rgb(255, 200, 50),
        code_bg: Color::Rgb(16, 12, 8),
        heading_color: Color::Rgb(255, 176, 0),
        quote_color: Color::Rgb(120, 100, 60),
        list_bullet_color: Color::Rgb(255, 176, 0),
        table_border_color: Color::Rgb(60, 45, 20),
        table_header_color: Color::Rgb(255, 176, 0),
        table_cell_color: Color::Rgb(200, 180, 140),

        bg: Color::Rgb(10, 8, 5),
        border: Color::Rgb(40, 30, 15),
        border_active: Color::Rgb(255, 176, 0),
        muted: Color::Rgb(80, 65, 35),

        user_color: Color::Rgb(220, 200, 160),
        user_bg: Color::Rgb(18, 14, 8),
        claude_label: Color::Rgb(255, 200, 50),
        claude_text: Color::Rgb(200, 185, 150),
        thinking_color: Color::Rgb(60, 50, 30),
        tool_label: Color::Rgb(255, 176, 0),
        tool_param: Color::Rgb(140, 110, 50),
        tool_result_color: Color::Rgb(180, 150, 80),
        tool_result_ok: Color::Rgb(200, 170, 50),
        error_color: Color::Rgb(255, 80, 40),
        warning_color: Color::Rgb(255, 220, 100),

        header_fg: Color::Rgb(255, 176, 0),
        status_streaming: Color::Rgb(255, 220, 100),
        status_ready: Color::Rgb(200, 170, 50),
        help_fg: Color::Rgb(50, 40, 20),
        input_fg: Color::Rgb(220, 200, 160),
        prompt_fg: Color::Rgb(255, 176, 0),
        separator: Color::Rgb(30, 22, 10),
        cost_color: Color::Rgb(255, 200, 50),

        subagent_border: Color::Rgb(60, 45, 20),
        subagent_name: Color::Rgb(255, 176, 0),
        subagent_status: Color::Rgb(160, 140, 90),
        subagent_done: Color::Rgb(200, 170, 50),
        subagent_time: Color::Rgb(120, 100, 60),
        event_icon: Color::Rgb(255, 180, 50),
        event_source: Color::Rgb(120, 180, 255),
        event_text: Color::Rgb(200, 200, 210),
        event_critical: Color::Rgb(255, 80, 80),
    }
}
}
