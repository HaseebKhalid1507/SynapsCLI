use ratatui::style::Color;
use super::super::Theme;

impl Theme {
/// Built-in theme: "ice" — frozen arctic whites and pale blues
pub(in crate::chatui::theme) fn ice() -> Self {
    Self {
        code_fg: Color::Rgb(200, 230, 255),
        code_bg: Color::Rgb(8, 12, 18),
        heading_color: Color::Rgb(220, 240, 255),
        quote_color: Color::Rgb(140, 180, 220),
        list_bullet_color: Color::Rgb(180, 220, 255),
        table_border_color: Color::Rgb(60, 80, 120),
        table_header_color: Color::Rgb(210, 235, 255),
        table_cell_color: Color::Rgb(160, 200, 240),

        bg: Color::Rgb(5, 8, 12),
        border: Color::Rgb(40, 60, 90),
        border_active: Color::Rgb(180, 220, 255),
        muted: Color::Rgb(70, 90, 130),

        user_color: Color::Rgb(190, 225, 255),
        user_bg: Color::Rgb(8, 11, 16),
        claude_label: Color::Rgb(220, 240, 255),
        claude_text: Color::Rgb(170, 210, 250),
        thinking_color: Color::Rgb(50, 70, 100),
        tool_label: Color::Rgb(200, 230, 255),
        tool_param: Color::Rgb(120, 160, 200),
        tool_result_color: Color::Rgb(150, 190, 230),
        tool_result_ok: Color::Rgb(180, 220, 255),
        error_color: Color::Rgb(255, 120, 140),
        warning_color: Color::Rgb(180, 200, 230),

        header_fg: Color::Rgb(200, 230, 255),
        status_streaming: Color::Rgb(180, 220, 255),
        status_ready: Color::Rgb(170, 210, 250),
        help_fg: Color::Rgb(60, 80, 120),
        input_fg: Color::Rgb(190, 225, 255),
        prompt_fg: Color::Rgb(220, 240, 255),
        separator: Color::Rgb(30, 45, 65),
        cost_color: Color::Rgb(180, 220, 255),

        subagent_border: Color::Rgb(60, 80, 120),
        subagent_name: Color::Rgb(200, 230, 255),
        subagent_status: Color::Rgb(150, 190, 230),
        subagent_done: Color::Rgb(180, 220, 255),
        subagent_time: Color::Rgb(120, 160, 200),
        event_icon: Color::Rgb(255, 180, 50),
        event_source: Color::Rgb(120, 180, 255),
        event_text: Color::Rgb(200, 200, 210),
        event_critical: Color::Rgb(255, 80, 80),
    }
}
}
