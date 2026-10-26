use super::super::Theme;
use ratatui::style::Color;

impl Theme {
    /// Built-in theme: "paper" — the one light palette: dark ink on a light
    /// canvas, with the default theme's teal accents darkened for a light
    /// ground.
    ///
    /// Every surface and text colour is explicit: the derivations that lift
    /// from `bg` assume a dark canvas, so this palette never relies on them
    /// for its own surfaces. Code blocks stay dark islands (`code_bg`),
    /// because syntax highlighting always uses a dark syntect theme
    /// (`highlight.rs`); a light code background would put its light tokens
    /// on a light ground. Body text reads at 11:1 or more on the canvas, and
    /// accents used as text at 4.5:1 or more on the chrome.
    pub(in crate::tui::theme) fn paper() -> Self {
        Self {
            // Dark code islands, close to syntect's own base16-ocean.dark.
            code_fg: Color::Rgb(190, 220, 228),
            code_bg: Color::Rgb(30, 35, 46),
            heading_color: Color::Rgb(11, 111, 128),
            quote_color: Color::Rgb(107, 119, 138),
            list_bullet_color: Color::Rgb(12, 112, 128),
            table_border_color: Color::Rgb(201, 208, 219),
            table_header_color: Color::Rgb(11, 111, 128),
            table_cell_color: Color::Rgb(47, 56, 70),

            // Light-grey chrome around a brighter paper canvas.
            bg: Color::Rgb(232, 235, 240),
            message_bg: Color::Rgb(246, 247, 249),
            border: Color::Rgb(201, 208, 219),
            border_active: Color::Rgb(12, 112, 128),
            muted: Color::Rgb(122, 133, 151),
            system_msg: Color::Rgb(79, 91, 110),

            user_color: Color::Rgb(31, 38, 51),
            user_bg: Color::Rgb(224, 230, 238),
            claude_label: Color::Rgb(11, 114, 133),
            claude_text: Color::Rgb(39, 48, 61),
            thinking_color: Color::Rgb(150, 159, 174),
            tool_label: Color::Rgb(10, 110, 128),
            tool_param: Color::Rgb(90, 107, 130),
            tool_result_color: Color::Rgb(62, 95, 107),
            tool_result_ok: Color::Rgb(14, 116, 96),
            error_color: Color::Rgb(198, 40, 40),
            warning_color: Color::Rgb(138, 92, 0),

            header_fg: Color::Rgb(74, 85, 104),
            status_streaming: Color::Rgb(138, 92, 0),
            status_ready: Color::Rgb(12, 114, 98),
            help_fg: Color::Rgb(122, 133, 151),
            input_fg: Color::Rgb(31, 38, 51),
            prompt_fg: Color::Rgb(12, 112, 128),
            separator: Color::Rgb(211, 217, 226),
            cost_color: Color::Rgb(138, 97, 0),

            subagent_border: Color::Rgb(195, 201, 224),
            subagent_name: Color::Rgb(91, 79, 196),
            subagent_status: Color::Rgb(90, 102, 124),
            subagent_done: Color::Rgb(12, 114, 98),
            subagent_time: Color::Rgb(122, 133, 151),
            // Tool cards take this palette's colours (Theme::tool_*_background):
            // the canvas mixed toward `claude_text`, which darkens it here.
            tool_input_bg: Color::Reset,
            tool_output_bg: Color::Reset,
            ..Self::default()
        }
    }
}
