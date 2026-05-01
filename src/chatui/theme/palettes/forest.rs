use ratatui::style::Color;
use super::super::Theme;

impl Theme {
/// Built-in theme: "forest" — deep forest greens and earthy browns
pub(in crate::chatui::theme) fn forest() -> Self {
    Self {
        code_fg: Color::Rgb(140, 200, 120),
        code_bg: Color::Rgb(15, 20, 10),
        heading_color: Color::Rgb(160, 220, 140),
        quote_color: Color::Rgb(100, 140, 80),
        list_bullet_color: Color::Rgb(120, 180, 100),
        table_border_color: Color::Rgb(60, 80, 40),
        table_header_color: Color::Rgb(150, 210, 130),
        table_cell_color: Color::Rgb(110, 160, 90),

        bg: Color::Rgb(8, 12, 6),
        border: Color::Rgb(50, 70, 35),
        border_active: Color::Rgb(120, 180, 100),
        muted: Color::Rgb(70, 90, 50),

        user_color: Color::Rgb(130, 190, 110),
        user_bg: Color::Rgb(12, 16, 8),
        claude_label: Color::Rgb(160, 220, 140),
        claude_text: Color::Rgb(120, 180, 100),
        thinking_color: Color::Rgb(40, 60, 30),
        tool_label: Color::Rgb(140, 200, 120),
        tool_param: Color::Rgb(90, 130, 70),
        tool_result_color: Color::Rgb(110, 160, 90),
        tool_result_ok: Color::Rgb(130, 190, 110),
        error_color: Color::Rgb(220, 80, 60),
        warning_color: Color::Rgb(180, 170, 80),

        header_fg: Color::Rgb(140, 200, 120),
        status_streaming: Color::Rgb(120, 180, 100),
        status_ready: Color::Rgb(130, 190, 110),
        help_fg: Color::Rgb(50, 70, 35),
        input_fg: Color::Rgb(130, 190, 110),
        prompt_fg: Color::Rgb(160, 220, 140),
        separator: Color::Rgb(25, 35, 20),
        cost_color: Color::Rgb(120, 180, 100),

        subagent_border: Color::Rgb(60, 80, 40),
        subagent_name: Color::Rgb(140, 200, 120),
        subagent_status: Color::Rgb(110, 160, 90),
        subagent_done: Color::Rgb(130, 190, 110),
        subagent_time: Color::Rgb(90, 130, 70),
        event_icon: Color::Rgb(255, 180, 50),
        event_source: Color::Rgb(120, 180, 255),
        event_text: Color::Rgb(200, 200, 210),
        event_critical: Color::Rgb(255, 80, 80),
    }
}
}
