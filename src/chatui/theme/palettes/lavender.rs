use ratatui::style::Color;
use super::super::Theme;

impl Theme {
/// Built-in theme: "lavender" — rich purple and violet tones
pub(in crate::chatui::theme) fn lavender() -> Self {
    Self {
        code_fg: Color::Rgb(210, 190, 245),
        code_bg: Color::Rgb(18, 10, 28),
        heading_color: Color::Rgb(180, 130, 255),
        quote_color: Color::Rgb(130, 100, 180),
        list_bullet_color: Color::Rgb(200, 160, 255),
        table_border_color: Color::Rgb(70, 45, 110),
        table_header_color: Color::Rgb(180, 130, 255),
        table_cell_color: Color::Rgb(195, 180, 225),

        bg: Color::Rgb(12, 8, 20),
        border: Color::Rgb(50, 30, 80),
        border_active: Color::Rgb(170, 120, 255),
        muted: Color::Rgb(85, 60, 130),

        user_color: Color::Rgb(225, 215, 245),
        user_bg: Color::Rgb(18, 12, 30),
        claude_label: Color::Rgb(180, 130, 255),
        claude_text: Color::Rgb(205, 195, 230),
        thinking_color: Color::Rgb(55, 35, 85),
        tool_label: Color::Rgb(155, 110, 240),
        tool_param: Color::Rgb(120, 85, 190),
        tool_result_color: Color::Rgb(140, 180, 230),
        tool_result_ok: Color::Rgb(160, 220, 200),
        error_color: Color::Rgb(255, 95, 130),
        warning_color: Color::Rgb(220, 180, 240),

        header_fg: Color::Rgb(170, 120, 255),
        status_streaming: Color::Rgb(220, 170, 255),
        status_ready: Color::Rgb(160, 220, 200),
        help_fg: Color::Rgb(55, 38, 85),
        input_fg: Color::Rgb(220, 210, 245),
        prompt_fg: Color::Rgb(180, 130, 255),
        separator: Color::Rgb(30, 18, 48),
        cost_color: Color::Rgb(220, 170, 255),

        subagent_border: Color::Rgb(70, 45, 110),
        subagent_name: Color::Rgb(180, 130, 255),
        subagent_status: Color::Rgb(155, 110, 240),
        subagent_done: Color::Rgb(160, 220, 200),
        subagent_time: Color::Rgb(120, 85, 190),
        event_icon: Color::Rgb(255, 180, 50),
        event_source: Color::Rgb(120, 180, 255),
        event_text: Color::Rgb(200, 200, 210),
        event_critical: Color::Rgb(255, 80, 80),
    }
}
}
