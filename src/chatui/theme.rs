use ratatui::style::Color;
use std::sync::LazyLock;

/// All colors used by the TUI, grouped so they can be overridden from a
/// user theme file. Defaults match the current built-in look.
///
/// Field names are what the theme file uses as keys. Unknown keys are
/// ignored; missing keys keep the default. Colors are written as `#rrggbb`
/// or `#rgb` hex.
pub(crate) struct Theme {
    // Markdown
    pub(crate) code_fg: Color,
    pub(crate) code_bg: Color,
    pub(crate) heading_color: Color,
    pub(crate) quote_color: Color,
    pub(crate) list_bullet_color: Color,
    pub(crate) table_border_color: Color,
    pub(crate) table_header_color: Color,
    pub(crate) table_cell_color: Color,

    // Base
    pub(crate) bg: Color,
    pub(crate) border: Color,
    pub(crate) border_active: Color,
    pub(crate) muted: Color,

    // Messages
    pub(crate) user_color: Color,
    pub(crate) user_bg: Color,
    pub(crate) claude_label: Color,
    pub(crate) claude_text: Color,
    pub(crate) thinking_color: Color,
    pub(crate) tool_label: Color,
    pub(crate) tool_param: Color,
    pub(crate) tool_result_color: Color,
    pub(crate) tool_result_ok: Color,
    pub(crate) error_color: Color,
    pub(crate) warning_color: Color,

    // UI chrome
    pub(crate) header_fg: Color,
    pub(crate) status_streaming: Color,
    pub(crate) status_ready: Color,
    pub(crate) help_fg: Color,
    pub(crate) input_fg: Color,
    pub(crate) prompt_fg: Color,
    pub(crate) separator: Color,
    pub(crate) cost_color: Color,

    // Subagent panel
    pub(crate) subagent_border: Color,
    pub(crate) subagent_name: Color,
    pub(crate) subagent_status: Color,
    pub(crate) subagent_done: Color,
    pub(crate) subagent_time: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            code_fg: Color::Rgb(170, 210, 220),
            code_bg: Color::Rgb(14, 18, 24),
            heading_color: Color::Rgb(80, 210, 230),
            quote_color: Color::Rgb(85, 100, 120),
            list_bullet_color: Color::Rgb(50, 190, 210),
            table_border_color: Color::Rgb(35, 55, 70),
            table_header_color: Color::Rgb(80, 210, 230),
            table_cell_color: Color::Rgb(175, 185, 200),

            bg: Color::Rgb(10, 12, 18),
            border: Color::Rgb(28, 36, 50),
            border_active: Color::Rgb(50, 180, 210),
            muted: Color::Rgb(50, 58, 72),

            user_color: Color::Rgb(185, 195, 215),
            user_bg: Color::Rgb(16, 20, 30),
            claude_label: Color::Rgb(50, 200, 220),
            claude_text: Color::Rgb(192, 198, 210),
            thinking_color: Color::Rgb(45, 55, 75),
            tool_label: Color::Rgb(70, 170, 220),
            tool_param: Color::Rgb(65, 100, 135),
            tool_result_color: Color::Rgb(55, 120, 130),
            tool_result_ok: Color::Rgb(50, 175, 160),
            error_color: Color::Rgb(230, 70, 70),
            warning_color: Color::Rgb(220, 180, 60),

            header_fg: Color::Rgb(110, 125, 150),
            status_streaming: Color::Rgb(220, 175, 60),
            status_ready: Color::Rgb(50, 195, 190),
            help_fg: Color::Rgb(42, 52, 68),
            input_fg: Color::Rgb(188, 195, 210),
            prompt_fg: Color::Rgb(50, 180, 210),
            separator: Color::Rgb(24, 30, 42),
            cost_color: Color::Rgb(210, 170, 80),

            subagent_border: Color::Rgb(40, 45, 75),
            subagent_name: Color::Rgb(140, 130, 220),
            subagent_status: Color::Rgb(120, 140, 170),
            subagent_done: Color::Rgb(50, 195, 190),
            subagent_time: Color::Rgb(80, 95, 120),
        }
    }
}

impl Theme {
    /// Built-in theme: "neon-rain" — Cyberpunk/Akira/Blade Runner palette
    fn neon_rain() -> Self {
        Self {
            code_fg: Color::Rgb(0, 240, 255),
            code_bg: Color::Rgb(10, 6, 18),
            heading_color: Color::Rgb(255, 46, 136),
            quote_color: Color::Rgb(106, 90, 122),
            list_bullet_color: Color::Rgb(252, 238, 10),
            table_border_color: Color::Rgb(48, 32, 74),
            table_header_color: Color::Rgb(255, 46, 136),
            table_cell_color: Color::Rgb(216, 210, 224),

            bg: Color::Rgb(8, 6, 12),
            border: Color::Rgb(30, 21, 48),
            border_active: Color::Rgb(255, 46, 136),
            muted: Color::Rgb(74, 58, 90),

            user_color: Color::Rgb(232, 224, 255),
            user_bg: Color::Rgb(13, 8, 24),
            claude_label: Color::Rgb(0, 240, 255),
            claude_text: Color::Rgb(216, 210, 224),
            thinking_color: Color::Rgb(58, 42, 74),
            tool_label: Color::Rgb(255, 46, 136),
            tool_param: Color::Rgb(106, 74, 122),
            tool_result_color: Color::Rgb(138, 154, 204),
            tool_result_ok: Color::Rgb(0, 240, 255),
            error_color: Color::Rgb(255, 23, 68),
            warning_color: Color::Rgb(252, 238, 10),

            header_fg: Color::Rgb(255, 46, 136),
            status_streaming: Color::Rgb(252, 238, 10),
            status_ready: Color::Rgb(0, 240, 255),
            help_fg: Color::Rgb(42, 26, 58),
            input_fg: Color::Rgb(232, 224, 255),
            prompt_fg: Color::Rgb(255, 46, 136),
            separator: Color::Rgb(26, 15, 40),
            cost_color: Color::Rgb(252, 238, 10),

            subagent_border: Color::Rgb(80, 20, 80),
            subagent_name: Color::Rgb(255, 46, 136),
            subagent_status: Color::Rgb(160, 120, 200),
            subagent_done: Color::Rgb(0, 240, 255),
            subagent_time: Color::Rgb(106, 90, 122),
        }
    }

    /// Built-in theme: "amber" — warm CRT/retro terminal
    fn amber() -> Self {
        Self {
            code_fg: Color::Rgb(255, 200, 50),
            code_bg: Color::Rgb(16, 12, 8),
            heading_color: Color::Rgb(255, 176, 0),
            quote_color: Color::Rgb(120, 100, 60),
            list_bullet_color: Color::Rgb(255, 176, 0),
            table_border_color: Color::Rgb(60, 45, 20),
            table_header_color: Color::Rgb(255, 176, 0),
            table_cell_color: Color::Rgb(200, 180, 140),

            bg: Color::Rgb(10, 8, 5),
            border: Color::Rgb(40, 30, 15),
            border_active: Color::Rgb(255, 176, 0),
            muted: Color::Rgb(80, 65, 35),

            user_color: Color::Rgb(220, 200, 160),
            user_bg: Color::Rgb(18, 14, 8),
            claude_label: Color::Rgb(255, 200, 50),
            claude_text: Color::Rgb(200, 185, 150),
            thinking_color: Color::Rgb(60, 50, 30),
            tool_label: Color::Rgb(255, 176, 0),
            tool_param: Color::Rgb(140, 110, 50),
            tool_result_color: Color::Rgb(180, 150, 80),
            tool_result_ok: Color::Rgb(200, 170, 50),
            error_color: Color::Rgb(255, 80, 40),
            warning_color: Color::Rgb(255, 220, 100),

            header_fg: Color::Rgb(255, 176, 0),
            status_streaming: Color::Rgb(255, 220, 100),
            status_ready: Color::Rgb(200, 170, 50),
            help_fg: Color::Rgb(50, 40, 20),
            input_fg: Color::Rgb(220, 200, 160),
            prompt_fg: Color::Rgb(255, 176, 0),
            separator: Color::Rgb(30, 22, 10),
            cost_color: Color::Rgb(255, 200, 50),

            subagent_border: Color::Rgb(60, 45, 20),
            subagent_name: Color::Rgb(255, 176, 0),
            subagent_status: Color::Rgb(160, 140, 90),
            subagent_done: Color::Rgb(200, 170, 50),
            subagent_time: Color::Rgb(120, 100, 60),
        }
    }

    /// Built-in theme: "phosphor" — green monochrome CRT
    fn phosphor() -> Self {
        Self {
            code_fg: Color::Rgb(50, 255, 80),
            code_bg: Color::Rgb(5, 15, 8),
            heading_color: Color::Rgb(80, 255, 120),
            quote_color: Color::Rgb(30, 100, 50),
            list_bullet_color: Color::Rgb(50, 220, 80),
            table_border_color: Color::Rgb(20, 60, 30),
            table_header_color: Color::Rgb(80, 255, 120),
            table_cell_color: Color::Rgb(60, 200, 90),

            bg: Color::Rgb(3, 8, 5),
            border: Color::Rgb(15, 40, 20),
            border_active: Color::Rgb(50, 255, 80),
            muted: Color::Rgb(25, 70, 35),

            user_color: Color::Rgb(60, 220, 90),
            user_bg: Color::Rgb(5, 14, 8),
            claude_label: Color::Rgb(80, 255, 120),
            claude_text: Color::Rgb(55, 200, 80),
            thinking_color: Color::Rgb(15, 50, 25),
            tool_label: Color::Rgb(50, 255, 80),
            tool_param: Color::Rgb(30, 120, 50),
            tool_result_color: Color::Rgb(40, 160, 60),
            tool_result_ok: Color::Rgb(50, 220, 80),
            error_color: Color::Rgb(255, 60, 60),
            warning_color: Color::Rgb(80, 255, 120),

            header_fg: Color::Rgb(50, 255, 80),
            status_streaming: Color::Rgb(80, 255, 120),
            status_ready: Color::Rgb(50, 220, 80),
            help_fg: Color::Rgb(15, 40, 20),
            input_fg: Color::Rgb(60, 220, 90),
            prompt_fg: Color::Rgb(50, 255, 80),
            separator: Color::Rgb(10, 25, 12),
            cost_color: Color::Rgb(80, 255, 120),

            subagent_border: Color::Rgb(20, 60, 30),
            subagent_name: Color::Rgb(50, 255, 80),
            subagent_status: Color::Rgb(40, 160, 60),
            subagent_done: Color::Rgb(80, 255, 120),
            subagent_time: Color::Rgb(30, 100, 50),
        }
    }

    /// Built-in theme: "solarized-dark" — Ethan Schoonover's classic
    fn solarized_dark() -> Self {
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
        }
    }

    /// Built-in theme: "blood" — dark red, Doom/horror aesthetic
    fn blood() -> Self {
        Self {
            code_fg: Color::Rgb(255, 100, 80),
            code_bg: Color::Rgb(15, 5, 5),
            heading_color: Color::Rgb(255, 50, 50),
            quote_color: Color::Rgb(100, 50, 50),
            list_bullet_color: Color::Rgb(200, 60, 60),
            table_border_color: Color::Rgb(60, 20, 20),
            table_header_color: Color::Rgb(255, 50, 50),
            table_cell_color: Color::Rgb(200, 160, 160),

            bg: Color::Rgb(8, 3, 3),
            border: Color::Rgb(40, 15, 15),
            border_active: Color::Rgb(255, 50, 50),
            muted: Color::Rgb(80, 40, 40),

            user_color: Color::Rgb(220, 180, 180),
            user_bg: Color::Rgb(15, 5, 5),
            claude_label: Color::Rgb(255, 80, 60),
            claude_text: Color::Rgb(200, 170, 170),
            thinking_color: Color::Rgb(50, 25, 25),
            tool_label: Color::Rgb(255, 50, 50),
            tool_param: Color::Rgb(140, 70, 70),
            tool_result_color: Color::Rgb(180, 100, 80),
            tool_result_ok: Color::Rgb(200, 80, 60),
            error_color: Color::Rgb(255, 30, 30),
            warning_color: Color::Rgb(255, 150, 50),

            header_fg: Color::Rgb(255, 50, 50),
            status_streaming: Color::Rgb(255, 150, 50),
            status_ready: Color::Rgb(200, 80, 60),
            help_fg: Color::Rgb(50, 25, 25),
            input_fg: Color::Rgb(220, 180, 180),
            prompt_fg: Color::Rgb(255, 50, 50),
            separator: Color::Rgb(30, 10, 10),
            cost_color: Color::Rgb(255, 150, 50),

            subagent_border: Color::Rgb(60, 20, 20),
            subagent_name: Color::Rgb(255, 50, 50),
            subagent_status: Color::Rgb(160, 80, 80),
            subagent_done: Color::Rgb(200, 80, 60),
            subagent_time: Color::Rgb(100, 50, 50),
        }
    }

    /// Built-in theme: "ocean" — Deep sea bioluminescence palette
    fn ocean() -> Self {
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

            user_color: Color::Rgb(224, 240, 255),
            user_bg: Color::Rgb(8, 16, 28),
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
        }
    }

    /// Built-in theme: "rose-pine" — Muted, elegant purples and pinks
    fn rose_pine() -> Self {
        Self {
            code_fg: Color::Rgb(234, 154, 151),
            code_bg: Color::Rgb(15, 12, 18),
            heading_color: Color::Rgb(235, 111, 146),
            quote_color: Color::Rgb(144, 122, 169),
            list_bullet_color: Color::Rgb(156, 207, 216),
            table_border_color: Color::Rgb(45, 35, 55),
            table_header_color: Color::Rgb(235, 111, 146),
            table_cell_color: Color::Rgb(224, 222, 244),

            bg: Color::Rgb(13, 10, 16),
            border: Color::Rgb(35, 28, 42),
            border_active: Color::Rgb(235, 111, 146),
            muted: Color::Rgb(85, 75, 95),

            user_color: Color::Rgb(240, 237, 245),
            user_bg: Color::Rgb(18, 15, 22),
            claude_label: Color::Rgb(234, 154, 151),
            claude_text: Color::Rgb(224, 222, 244),
            thinking_color: Color::Rgb(65, 55, 75),
            tool_label: Color::Rgb(235, 111, 146),
            tool_param: Color::Rgb(144, 122, 169),
            tool_result_color: Color::Rgb(156, 207, 216),
            tool_result_ok: Color::Rgb(234, 154, 151),
            error_color: Color::Rgb(235, 111, 146),
            warning_color: Color::Rgb(246, 193, 119),

            header_fg: Color::Rgb(235, 111, 146),
            status_streaming: Color::Rgb(156, 207, 216),
            status_ready: Color::Rgb(234, 154, 151),
            help_fg: Color::Rgb(55, 45, 65),
            input_fg: Color::Rgb(240, 237, 245),
            prompt_fg: Color::Rgb(235, 111, 146),
            separator: Color::Rgb(25, 20, 32),
            cost_color: Color::Rgb(156, 207, 216),

            subagent_border: Color::Rgb(85, 65, 105),
            subagent_name: Color::Rgb(235, 111, 146),
            subagent_status: Color::Rgb(196, 167, 231),
            subagent_done: Color::Rgb(234, 154, 151),
            subagent_time: Color::Rgb(144, 122, 169),
        }
    }

    /// Built-in theme: "nord" — Arctic frost palette inspired by polar nights
    fn nord() -> Self {
        Self {
            code_fg: Color::Rgb(136, 192, 208),
            code_bg: Color::Rgb(18, 20, 25),
            heading_color: Color::Rgb(129, 161, 193),
            quote_color: Color::Rgb(94, 129, 172),
            list_bullet_color: Color::Rgb(163, 190, 140),
            table_border_color: Color::Rgb(45, 50, 65),
            table_header_color: Color::Rgb(129, 161, 193),
            table_cell_color: Color::Rgb(216, 222, 233),

            bg: Color::Rgb(16, 18, 22),
            border: Color::Rgb(35, 40, 50),
            border_active: Color::Rgb(129, 161, 193),
            muted: Color::Rgb(75, 85, 105),

            user_color: Color::Rgb(236, 239, 244),
            user_bg: Color::Rgb(22, 25, 30),
            claude_label: Color::Rgb(136, 192, 208),
            claude_text: Color::Rgb(216, 222, 233),
            thinking_color: Color::Rgb(55, 65, 85),
            tool_label: Color::Rgb(129, 161, 193),
            tool_param: Color::Rgb(94, 129, 172),
            tool_result_color: Color::Rgb(180, 142, 173),
            tool_result_ok: Color::Rgb(136, 192, 208),
            error_color: Color::Rgb(191, 97, 106),
            warning_color: Color::Rgb(235, 203, 139),

            header_fg: Color::Rgb(129, 161, 193),
            status_streaming: Color::Rgb(163, 190, 140),
            status_ready: Color::Rgb(136, 192, 208),
            help_fg: Color::Rgb(45, 55, 75),
            input_fg: Color::Rgb(236, 239, 244),
            prompt_fg: Color::Rgb(129, 161, 193),
            separator: Color::Rgb(28, 32, 40),
            cost_color: Color::Rgb(163, 190, 140),

            subagent_border: Color::Rgb(65, 75, 95),
            subagent_name: Color::Rgb(129, 161, 193),
            subagent_status: Color::Rgb(180, 142, 173),
            subagent_done: Color::Rgb(136, 192, 208),
            subagent_time: Color::Rgb(94, 129, 172),
        }
    }

    /// Built-in theme: "dracula" — Dark theme with vibrant purple, pink, and cyan accents
    fn dracula() -> Self {
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
        }
    }

    /// Built-in theme: "monokai" — classic vibrant dark theme with orange/pink/green/yellow accents
    fn monokai() -> Self {
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
        }
    }

    /// Built-in theme: "gruvbox" — warm earthy tones with orange/yellow/aqua accents on dark background
    fn gruvbox() -> Self {
        Self {
            code_fg: Color::Rgb(235, 219, 178),
            code_bg: Color::Rgb(60, 56, 54),
            heading_color: Color::Rgb(254, 128, 25),
            quote_color: Color::Rgb(146, 131, 116),
            list_bullet_color: Color::Rgb(250, 189, 47),
            table_border_color: Color::Rgb(80, 73, 69),
            table_header_color: Color::Rgb(142, 192, 124),
            table_cell_color: Color::Rgb(213, 196, 161),

            bg: Color::Rgb(40, 40, 40),
            border: Color::Rgb(80, 73, 69),
            border_active: Color::Rgb(254, 128, 25),
            muted: Color::Rgb(146, 131, 116),

            user_color: Color::Rgb(235, 219, 178),
            user_bg: Color::Rgb(50, 48, 47),
            claude_label: Color::Rgb(211, 134, 155),
            claude_text: Color::Rgb(213, 196, 161),
            thinking_color: Color::Rgb(146, 131, 116),
            tool_label: Color::Rgb(142, 192, 124),
            tool_param: Color::Rgb(250, 189, 47),
            tool_result_color: Color::Rgb(131, 165, 152),
            tool_result_ok: Color::Rgb(184, 187, 38),
            error_color: Color::Rgb(251, 73, 52),
            warning_color: Color::Rgb(250, 189, 47),

            header_fg: Color::Rgb(254, 128, 25),
            status_streaming: Color::Rgb(131, 165, 152),
            status_ready: Color::Rgb(184, 187, 38),
            help_fg: Color::Rgb(102, 92, 84),
            input_fg: Color::Rgb(235, 219, 178),
            prompt_fg: Color::Rgb(211, 134, 155),
            separator: Color::Rgb(60, 56, 54),
            cost_color: Color::Rgb(250, 189, 47),

            subagent_border: Color::Rgb(80, 73, 69),
            subagent_name: Color::Rgb(254, 128, 25),
            subagent_status: Color::Rgb(177, 98, 134),
            subagent_done: Color::Rgb(184, 187, 38),
            subagent_time: Color::Rgb(146, 131, 116),
        }
    }

    /// Built-in theme: "catppuccin" — soft pastels (lavender/mauve/peach/sky) on cozy dark base
    fn catppuccin() -> Self {
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
        }
    }

    /// Built-in theme: "tokyo-night" — dark blue-purple theme with soft blue/purple/cyan accents
    fn tokyo_night() -> Self {
        Self {
            code_fg: Color::Rgb(192, 202, 245),
            code_bg: Color::Rgb(36, 40, 59),
            heading_color: Color::Rgb(187, 154, 247),
            quote_color: Color::Rgb(86, 95, 137),
            list_bullet_color: Color::Rgb(125, 207, 255),
            table_border_color: Color::Rgb(41, 46, 66),
            table_header_color: Color::Rgb(122, 162, 247),
            table_cell_color: Color::Rgb(169, 177, 214),

            bg: Color::Rgb(26, 27, 38),
            border: Color::Rgb(41, 46, 66),
            border_active: Color::Rgb(122, 162, 247),
            muted: Color::Rgb(86, 95, 137),

            user_color: Color::Rgb(192, 202, 245),
            user_bg: Color::Rgb(36, 40, 59),
            claude_label: Color::Rgb(187, 154, 247),
            claude_text: Color::Rgb(169, 177, 214),
            thinking_color: Color::Rgb(86, 95, 137),
            tool_label: Color::Rgb(158, 206, 106),
            tool_param: Color::Rgb(255, 158, 100),
            tool_result_color: Color::Rgb(125, 207, 255),
            tool_result_ok: Color::Rgb(158, 206, 106),
            error_color: Color::Rgb(247, 118, 142),
            warning_color: Color::Rgb(224, 175, 104),

            header_fg: Color::Rgb(125, 207, 255),
            status_streaming: Color::Rgb(255, 158, 100),
            status_ready: Color::Rgb(158, 206, 106),
            help_fg: Color::Rgb(86, 95, 137),
            input_fg: Color::Rgb(192, 202, 245),
            prompt_fg: Color::Rgb(122, 162, 247),
            separator: Color::Rgb(52, 59, 88),
            cost_color: Color::Rgb(224, 175, 104),

            subagent_border: Color::Rgb(41, 46, 66),
            subagent_name: Color::Rgb(187, 154, 247),
            subagent_status: Color::Rgb(122, 162, 247),
            subagent_done: Color::Rgb(158, 206, 106),
            subagent_time: Color::Rgb(86, 95, 137),
        }
    }

    /// Built-in theme: "sunset" — warm sunset gradient feel
    fn sunset() -> Self {
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
        }
    }

    /// Built-in theme: "ice" — frozen arctic whites and pale blues
    fn ice() -> Self {
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
        }
    }

    /// Built-in theme: "forest" — deep forest greens and earthy browns
    fn forest() -> Self {
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
        }
    }

    /// Built-in theme: "lavender" — rich purple and violet tones
    fn lavender() -> Self {
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
        }
    }

    /// Get a built-in theme by name. Returns None if not found.
    fn builtin(name: &str) -> Option<Self> {
        match name {
            "default" => Some(Self::default()),
            "neon-rain" | "neon_rain" | "neonrain" => Some(Self::neon_rain()),
            "amber" => Some(Self::amber()),
            "phosphor" | "green" => Some(Self::phosphor()),
            "solarized" | "solarized-dark" | "solarized_dark" => Some(Self::solarized_dark()),
            "blood" | "doom" => Some(Self::blood()),
            "ocean" => Some(Self::ocean()),
            "rose-pine" | "rose_pine" | "rosepine" => Some(Self::rose_pine()),
            "nord" => Some(Self::nord()),
            "dracula" => Some(Self::dracula()),
            "monokai" => Some(Self::monokai()),
            "gruvbox" => Some(Self::gruvbox()),
            "catppuccin" | "ctp" => Some(Self::catppuccin()),
            "tokyo-night" | "tokyo_night" | "tokyonight" => Some(Self::tokyo_night()),
            "sunset" => Some(Self::sunset()),
            "ice" | "frost" => Some(Self::ice()),
            "forest" => Some(Self::forest()),
            "lavender" => Some(Self::lavender()),
            _ => None,
        }
    }

    /// Load a theme from a simple `key = #hex` file. Lines starting with
    /// `#` are comments; malformed lines and unknown keys are skipped
    /// silently so a bad theme can never take down the UI.
    fn load_from(path: &std::path::Path) -> Self {
        let mut t = Self::default();
        let Ok(content) = std::fs::read_to_string(path) else {
            return t;
        };
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, val)) = line.split_once('=') else { continue };
            let key = key.trim();
            let val = val.trim();
            let Some(color) = parse_hex_color(val) else { continue };
            t.set(key, color);
        }
        t
    }

    fn set(&mut self, key: &str, c: Color) {
        match key {
            "code_fg" => self.code_fg = c,
            "code_bg" => self.code_bg = c,
            "heading_color" => self.heading_color = c,
            "quote_color" => self.quote_color = c,
            "list_bullet_color" => self.list_bullet_color = c,
            "table_border_color" => self.table_border_color = c,
            "table_header_color" => self.table_header_color = c,
            "table_cell_color" => self.table_cell_color = c,

            "bg" => self.bg = c,
            "border" => self.border = c,
            "border_active" => self.border_active = c,
            "muted" => self.muted = c,

            "user_color" => self.user_color = c,
            "user_bg" => self.user_bg = c,
            "claude_label" => self.claude_label = c,
            "claude_text" => self.claude_text = c,
            "thinking_color" => self.thinking_color = c,
            "tool_label" => self.tool_label = c,
            "tool_param" => self.tool_param = c,
            "tool_result_color" => self.tool_result_color = c,
            "tool_result_ok" => self.tool_result_ok = c,
            "error_color" => self.error_color = c,
            "warning_color" => self.warning_color = c,

            "header_fg" => self.header_fg = c,
            "status_streaming" => self.status_streaming = c,
            "status_ready" => self.status_ready = c,
            "help_fg" => self.help_fg = c,
            "input_fg" => self.input_fg = c,
            "prompt_fg" => self.prompt_fg = c,
            "separator" => self.separator = c,
            "cost_color" => self.cost_color = c,

            "subagent_border" => self.subagent_border = c,
            "subagent_name" => self.subagent_name = c,
            "subagent_status" => self.subagent_status = c,
            "subagent_done" => self.subagent_done = c,
            "subagent_time" => self.subagent_time = c,
            _ => {} // unknown key: ignore
        }
    }
}

/// Parse `#rrggbb` or `#rgb` into a `Color::Rgb`. Returns `None` for anything
/// that doesn't match — malformed entries should be skipped, not crash.
fn parse_hex_color(s: &str) -> Option<Color> {
    let s = s.trim().trim_start_matches('#');
    match s.len() {
        6 => {
            let r = u8::from_str_radix(&s[0..2], 16).ok()?;
            let g = u8::from_str_radix(&s[2..4], 16).ok()?;
            let b = u8::from_str_radix(&s[4..6], 16).ok()?;
            Some(Color::Rgb(r, g, b))
        }
        3 => {
            let r = u8::from_str_radix(&s[0..1], 16).ok()?;
            let g = u8::from_str_radix(&s[1..2], 16).ok()?;
            let b = u8::from_str_radix(&s[2..3], 16).ok()?;
            Some(Color::Rgb(r * 17, g * 17, b * 17)) // 0xF -> 0xFF
        }
        _ => None,
    }
}

/// Global theme, loaded in this order:
/// 1. `~/.synaps-cli/theme` file (if exists) — overrides everything
/// 2. `theme = <name>` in config:
///    a. Check `~/.synaps-cli/themes/<name>` file first (user-editable)
///    b. Fall back to compiled-in builtin
/// 3. Falls back to default
pub(crate) static THEME: LazyLock<Theme> = LazyLock::new(|| {
    // First check for a theme file (highest priority)
    let path = synaps_cli::config::resolve_read_path("theme");
    if path.exists() {
        return Theme::load_from(&path);
    }

    // Then check config for a named built-in theme
    if let Ok(content) = std::fs::read_to_string(synaps_cli::config::resolve_read_path("config")) {
        for line in content.lines() {
            let line = line.trim();
            if line.starts_with('#') || line.is_empty() { continue; }
            if let Some((key, val)) = line.split_once('=') {
                if key.trim() == "theme" {
                    let name = val.trim();
                    // Check ~/.synaps-cli/themes/<name> file first
                    let theme_file = synaps_cli::config::base_dir().join("themes").join(name);
                    if theme_file.exists() {
                        return Theme::load_from(&theme_file);
                    }
                    // Fall back to compiled-in builtin
                    if let Some(theme) = Theme::builtin(name) {
                        return theme;
                    }
                }
            }
        }
    }

    Theme::default()
});
