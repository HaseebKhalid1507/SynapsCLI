use ratatui::style::Color;
use super::super::Theme;

impl Theme {
/// Built-in theme: "solarized-dark" — Ethan Schoonover's classic
pub(in crate::chatui::theme) fn solarized_dark() -> Self {
    Self {
        code_fg: Color::Rgb(133, 153, 0),   // green
        code_bg: Color::Rgb(0, 36, 43),      // base03
        heading_color: Color::Rgb(38, 139, 210), // blue
        quote_color: Color::Rgb(88, 110, 117),   // base01
        list_bullet_color: Color::Rgb(42, 161, 152), // cyan
        table_border_color: Color::Rgb(7, 54, 66),   // base02
        table_header_color: Color::Rgb(38, 139, 210),
        table_cell_color: Color::Rgb(147, 161, 161), // base1

        bg: Color::Rgb(0, 43, 54),           // base03
        border: Color::Rgb(7, 54, 66),       // base02
        border_active: Color::Rgb(38, 139, 210),
        muted: Color::Rgb(88, 110, 117),     // base01

        user_color: Color::Rgb(253, 246, 227), // base3
        user_bg: Color::Rgb(7, 54, 66),
        claude_label: Color::Rgb(42, 161, 152), // cyan
        claude_text: Color::Rgb(147, 161, 161),
        thinking_color: Color::Rgb(7, 54, 66),
        tool_label: Color::Rgb(38, 139, 210),
        tool_param: Color::Rgb(88, 110, 117),
        tool_result_color: Color::Rgb(133, 153, 0),
        tool_result_ok: Color::Rgb(42, 161, 152),
        error_color: Color::Rgb(220, 50, 47),   // red
        warning_color: Color::Rgb(181, 137, 0),

        header_fg: Color::Rgb(131, 148, 150),   // base0
        status_streaming: Color::Rgb(181, 137, 0), // yellow
        status_ready: Color::Rgb(42, 161, 152),
        help_fg: Color::Rgb(7, 54, 66),
        input_fg: Color::Rgb(238, 232, 213),    // base2
        prompt_fg: Color::Rgb(42, 161, 152),
        separator: Color::Rgb(7, 54, 66),
        cost_color: Color::Rgb(181, 137, 0),

        subagent_border: Color::Rgb(7, 54, 66),
        subagent_name: Color::Rgb(108, 113, 196), // violet
        subagent_status: Color::Rgb(88, 110, 117),
        subagent_done: Color::Rgb(42, 161, 152),
        subagent_time: Color::Rgb(88, 110, 117),
        event_icon: Color::Rgb(255, 180, 50),
        event_source: Color::Rgb(120, 180, 255),
        event_text: Color::Rgb(200, 200, 210),
        event_critical: Color::Rgb(255, 80, 80),
    }
}
}
