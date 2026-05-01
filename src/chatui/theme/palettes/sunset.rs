use ratatui::style::Color;
use super::super::Theme;

impl Theme {
/// Built-in theme: "sunset" — warm sunset gradient feel
pub(in crate::chatui::theme) fn sunset() -> Self {
    Self {
        code_fg: Color::Rgb(255, 180, 120),
        code_bg: Color::Rgb(25, 12, 8),
        heading_color: Color::Rgb(255, 150, 100),
        quote_color: Color::Rgb(200, 120, 80),
        list_bullet_color: Color::Rgb(255, 140, 90),
        table_border_color: Color::Rgb(120, 60, 40),
        table_header_color: Color::Rgb(255, 160, 110),
        table_cell_color: Color::Rgb(220, 140, 100),

        bg: Color::Rgb(15, 8, 10),
        border: Color::Rgb(80, 40, 50),
        border_active: Color::Rgb(255, 140, 90),
        muted: Color::Rgb(100, 50, 60),

        user_color: Color::Rgb(255, 170, 130),
        user_bg: Color::Rgb(20, 10, 12),
        claude_label: Color::Rgb(255, 140, 90),
        claude_text: Color::Rgb(240, 160, 120),
        thinking_color: Color::Rgb(80, 40, 50),
        tool_label: Color::Rgb(255, 150, 100),
        tool_param: Color::Rgb(200, 100, 70),
        tool_result_color: Color::Rgb(220, 130, 90),
        tool_result_ok: Color::Rgb(255, 160, 110),
        error_color: Color::Rgb(255, 80, 80),
        warning_color: Color::Rgb(255, 200, 100),

        header_fg: Color::Rgb(255, 150, 100),
        status_streaming: Color::Rgb(255, 140, 90),
        status_ready: Color::Rgb(240, 160, 120),
        help_fg: Color::Rgb(80, 40, 50),
        input_fg: Color::Rgb(255, 170, 130),
        prompt_fg: Color::Rgb(255, 150, 100),
        separator: Color::Rgb(60, 30, 35),
        cost_color: Color::Rgb(255, 140, 90),

        subagent_border: Color::Rgb(120, 60, 40),
        subagent_name: Color::Rgb(255, 150, 100),
        subagent_status: Color::Rgb(220, 130, 90),
        subagent_done: Color::Rgb(255, 160, 110),
        subagent_time: Color::Rgb(180, 90, 60),
        event_icon: Color::Rgb(255, 180, 50),
        event_source: Color::Rgb(120, 180, 255),
        event_text: Color::Rgb(200, 200, 210),
        event_critical: Color::Rgb(255, 80, 80),
    }
}
}
