use ratatui::style::Color;
use super::super::Theme;

impl Theme {
/// Built-in theme: "monokai" — classic vibrant dark theme with orange/pink/green/yellow accents
pub(in crate::chatui::theme) fn monokai() -> Self {
    Self {
        code_fg: Color::Rgb(248, 248, 242),
        code_bg: Color::Rgb(39, 40, 34),
        heading_color: Color::Rgb(249, 38, 114),
        quote_color: Color::Rgb(117, 113, 94),
        list_bullet_color: Color::Rgb(253, 151, 31),
        table_border_color: Color::Rgb(73, 72, 62),
        table_header_color: Color::Rgb(166, 226, 46),
        table_cell_color: Color::Rgb(230, 219, 116),

        bg: Color::Rgb(33, 34, 28),
        border: Color::Rgb(73, 72, 62),
        border_active: Color::Rgb(253, 151, 31),
        muted: Color::Rgb(117, 113, 94),

        user_color: Color::Rgb(248, 248, 242),
        user_bg: Color::Rgb(39, 40, 34),
        claude_label: Color::Rgb(174, 129, 255),
        claude_text: Color::Rgb(230, 219, 116),
        thinking_color: Color::Rgb(117, 113, 94),
        tool_label: Color::Rgb(166, 226, 46),
        tool_param: Color::Rgb(253, 151, 31),
        tool_result_color: Color::Rgb(102, 217, 239),
        tool_result_ok: Color::Rgb(166, 226, 46),
        error_color: Color::Rgb(249, 38, 114),
        warning_color: Color::Rgb(230, 219, 116),

        header_fg: Color::Rgb(253, 151, 31),
        status_streaming: Color::Rgb(102, 217, 239),
        status_ready: Color::Rgb(166, 226, 46),
        help_fg: Color::Rgb(117, 113, 94),
        input_fg: Color::Rgb(248, 248, 242),
        prompt_fg: Color::Rgb(174, 129, 255),
        separator: Color::Rgb(58, 58, 50),
        cost_color: Color::Rgb(230, 219, 116),

        subagent_border: Color::Rgb(73, 72, 62),
        subagent_name: Color::Rgb(249, 38, 114),
        subagent_status: Color::Rgb(174, 129, 255),
        subagent_done: Color::Rgb(166, 226, 46),
        subagent_time: Color::Rgb(117, 113, 94),
        event_icon: Color::Rgb(255, 180, 50),
        event_source: Color::Rgb(120, 180, 255),
        event_text: Color::Rgb(200, 200, 210),
        event_critical: Color::Rgb(255, 80, 80),
    }
}
}
