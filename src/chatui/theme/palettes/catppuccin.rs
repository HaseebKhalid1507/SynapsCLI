use ratatui::style::Color;
use super::super::Theme;

impl Theme {
/// Built-in theme: "catppuccin" — soft pastels (lavender/mauve/peach/sky) on cozy dark base
pub(in crate::chatui::theme) fn catppuccin() -> Self {
    Self {
        code_fg: Color::Rgb(205, 214, 244),
        code_bg: Color::Rgb(49, 50, 68),
        heading_color: Color::Rgb(203, 166, 247),
        quote_color: Color::Rgb(108, 112, 134),
        list_bullet_color: Color::Rgb(250, 179, 135),
        table_border_color: Color::Rgb(88, 91, 112),
        table_header_color: Color::Rgb(180, 190, 254),
        table_cell_color: Color::Rgb(166, 173, 200),

        bg: Color::Rgb(30, 30, 46),
        border: Color::Rgb(88, 91, 112),
        border_active: Color::Rgb(180, 190, 254),
        muted: Color::Rgb(108, 112, 134),

        user_color: Color::Rgb(205, 214, 244),
        user_bg: Color::Rgb(49, 50, 68),
        claude_label: Color::Rgb(203, 166, 247),
        claude_text: Color::Rgb(166, 173, 200),
        thinking_color: Color::Rgb(108, 112, 134),
        tool_label: Color::Rgb(137, 220, 235),
        tool_param: Color::Rgb(250, 179, 135),
        tool_result_color: Color::Rgb(148, 226, 213),
        tool_result_ok: Color::Rgb(166, 227, 161),
        error_color: Color::Rgb(243, 139, 168),
        warning_color: Color::Rgb(249, 226, 175),

        header_fg: Color::Rgb(250, 179, 135),
        status_streaming: Color::Rgb(137, 220, 235),
        status_ready: Color::Rgb(166, 227, 161),
        help_fg: Color::Rgb(88, 91, 112),
        input_fg: Color::Rgb(205, 214, 244),
        prompt_fg: Color::Rgb(180, 190, 254),
        separator: Color::Rgb(69, 71, 90),
        cost_color: Color::Rgb(249, 226, 175),

        subagent_border: Color::Rgb(88, 91, 112),
        subagent_name: Color::Rgb(203, 166, 247),
        subagent_status: Color::Rgb(180, 190, 254),
        subagent_done: Color::Rgb(166, 227, 161),
        subagent_time: Color::Rgb(108, 112, 134),
        event_icon: Color::Rgb(255, 180, 50),
        event_source: Color::Rgb(120, 180, 255),
        event_text: Color::Rgb(200, 200, 210),
        event_critical: Color::Rgb(255, 80, 80),
    }
}
}
