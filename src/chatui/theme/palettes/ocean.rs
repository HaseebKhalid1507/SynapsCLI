use ratatui::style::Color;
use super::super::Theme;

impl Theme {
/// Built-in theme: "ocean" — Deep sea bioluminescence palette
pub(in crate::chatui::theme) fn ocean() -> Self {
    Self {
        code_fg: Color::Rgb(64, 224, 208),
        code_bg: Color::Rgb(5, 10, 20),
        heading_color: Color::Rgb(0, 206, 209),
        quote_color: Color::Rgb(72, 118, 155),
        list_bullet_color: Color::Rgb(32, 178, 170),
        table_border_color: Color::Rgb(25, 50, 75),
        table_header_color: Color::Rgb(0, 206, 209),
        table_cell_color: Color::Rgb(176, 216, 230),

        bg: Color::Rgb(3, 8, 16),
        border: Color::Rgb(15, 30, 45),
        border_active: Color::Rgb(0, 206, 209),
        muted: Color::Rgb(45, 75, 105),

        user_color: Color::Rgb(170, 210, 245),
        user_bg: Color::Rgb(3, 8, 16),
        claude_label: Color::Rgb(64, 224, 208),
        claude_text: Color::Rgb(176, 216, 230),
        thinking_color: Color::Rgb(35, 65, 95),
        tool_label: Color::Rgb(0, 206, 209),
        tool_param: Color::Rgb(72, 118, 155),
        tool_result_color: Color::Rgb(135, 175, 215),
        tool_result_ok: Color::Rgb(64, 224, 208),
        error_color: Color::Rgb(255, 99, 71),
        warning_color: Color::Rgb(100, 200, 180),

        header_fg: Color::Rgb(0, 206, 209),
        status_streaming: Color::Rgb(32, 178, 170),
        status_ready: Color::Rgb(64, 224, 208),
        help_fg: Color::Rgb(25, 45, 65),
        input_fg: Color::Rgb(224, 240, 255),
        prompt_fg: Color::Rgb(0, 206, 209),
        separator: Color::Rgb(12, 24, 36),
        cost_color: Color::Rgb(32, 178, 170),

        subagent_border: Color::Rgb(20, 60, 100),
        subagent_name: Color::Rgb(0, 206, 209),
        subagent_status: Color::Rgb(100, 149, 237),
        subagent_done: Color::Rgb(64, 224, 208),
        subagent_time: Color::Rgb(72, 118, 155),
        event_icon: Color::Rgb(255, 180, 50),
        event_source: Color::Rgb(120, 180, 255),
        event_text: Color::Rgb(200, 200, 210),
        event_critical: Color::Rgb(255, 80, 80),
    }
}
}
