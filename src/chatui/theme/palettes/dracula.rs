use ratatui::style::Color;
use super::super::Theme;

impl Theme {
/// Built-in theme: "dracula" — Dark theme with vibrant purple, pink, and cyan accents
pub(in crate::chatui::theme) fn dracula() -> Self {
    Self {
        code_fg: Color::Rgb(139, 233, 253),
        code_bg: Color::Rgb(15, 12, 20),
        heading_color: Color::Rgb(189, 147, 249),
        quote_color: Color::Rgb(98, 114, 164),
        list_bullet_color: Color::Rgb(80, 250, 123),
        table_border_color: Color::Rgb(40, 35, 50),
        table_header_color: Color::Rgb(189, 147, 249),
        table_cell_color: Color::Rgb(248, 248, 242),

        bg: Color::Rgb(12, 10, 18),
        border: Color::Rgb(30, 25, 40),
        border_active: Color::Rgb(189, 147, 249),
        muted: Color::Rgb(68, 71, 90),

        user_color: Color::Rgb(248, 248, 242),
        user_bg: Color::Rgb(18, 15, 25),
        claude_label: Color::Rgb(139, 233, 253),
        claude_text: Color::Rgb(248, 248, 242),
        thinking_color: Color::Rgb(55, 50, 70),
        tool_label: Color::Rgb(189, 147, 249),
        tool_param: Color::Rgb(255, 121, 198),
        tool_result_color: Color::Rgb(80, 250, 123),
        tool_result_ok: Color::Rgb(139, 233, 253),
        error_color: Color::Rgb(255, 85, 85),
        warning_color: Color::Rgb(241, 250, 140),

        header_fg: Color::Rgb(189, 147, 249),
        status_streaming: Color::Rgb(241, 250, 140),
        status_ready: Color::Rgb(139, 233, 253),
        help_fg: Color::Rgb(40, 35, 55),
        input_fg: Color::Rgb(248, 248, 242),
        prompt_fg: Color::Rgb(189, 147, 249),
        separator: Color::Rgb(22, 18, 30),
        cost_color: Color::Rgb(241, 250, 140),

        subagent_border: Color::Rgb(80, 65, 100),
        subagent_name: Color::Rgb(189, 147, 249),
        subagent_status: Color::Rgb(255, 121, 198),
        subagent_done: Color::Rgb(139, 233, 253),
        subagent_time: Color::Rgb(98, 114, 164),
        event_icon: Color::Rgb(255, 180, 50),
        event_source: Color::Rgb(120, 180, 255),
        event_text: Color::Rgb(200, 200, 210),
        event_critical: Color::Rgb(255, 80, 80),
    }
}
}
