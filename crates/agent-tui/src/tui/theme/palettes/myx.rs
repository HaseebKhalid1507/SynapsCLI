use super::super::mxc::{self, MxcColors};
use super::super::Theme;

/// Myx's own default/fallback palette (its `TOKYONIGHT` constant — what Myx
/// shows before any album art is analyzed), expressed as MXC tokens. This is
/// the static snapshot the "myx" theme renders when Myx is not installed or
/// not running.
pub(in crate::tui::theme) const MYX_DEFAULT: MxcColors = MxcColors {
    primary: (0x82, 0xaa, 0xff),
    secondary: (0xc0, 0x99, 0xff),
    accent: (0xff, 0x96, 0x6c),
    error: (0xff, 0x75, 0x7f),
    warning: (0xff, 0x96, 0x6c),
    success: (0xc3, 0xe8, 0x8d),
    info: (0x82, 0xaa, 0xff),
    text: (0xc8, 0xd3, 0xf5),
    text_muted: (0x82, 0x8b, 0xb8),
    background: (0x1a, 0x1b, 0x26),
    background_panel: (0x1e, 0x20, 0x30),
    background_element: (0x22, 0x24, 0x36),
    border: (0x73, 0x7a, 0xa2),
    border_active: (0x90, 0x99, 0xb2),
    border_subtle: (0x54, 0x5c, 0x7e),
    border_dimmest: (0x2a, 0x2c, 0x41),
};

impl Theme {
    /// Built-in theme: "myx" — album-reactive colors via the Myx music
    /// player's MXC protocol. Statically this is Myx's default look; while
    /// active, a background subscriber live-swaps the palette on every track
    /// change (see `theme::mxc`).
    ///
    /// Deliberately built through the SAME mapping function the live path
    /// uses, so the static snapshot and the socket-fed palettes can never
    /// disagree about where a token lands.
    pub(in crate::tui::theme) fn myx() -> Self {
        mxc::theme_from_mxc(&MYX_DEFAULT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_myx_is_the_mapped_myx_default() {
        // The invariant the whole design rests on: static == map(default).
        let t = Theme::myx();
        let mapped = mxc::theme_from_mxc(&MYX_DEFAULT);
        assert_eq!(t.bg, mapped.bg);
        assert_eq!(t.message_bg, mapped.message_bg);
        assert_eq!(t.claude_label, mapped.claude_label);
        assert_eq!(t.border_active, mapped.border_active);
    }

    #[test]
    fn static_myx_matches_myx_tokyonight_base() {
        use ratatui::style::Color;
        let t = Theme::myx();
        assert_eq!(t.bg, Color::Rgb(0x1e, 0x20, 0x30)); // chrome = library panel
        assert_eq!(t.message_bg, Color::Rgb(0x1a, 0x1b, 0x26)); // canvas = myx background
        assert_eq!(t.claude_label, Color::Rgb(0x82, 0xaa, 0xff));
        assert_eq!(t.error_color, Color::Rgb(0xff, 0x75, 0x7f));
    }
}
