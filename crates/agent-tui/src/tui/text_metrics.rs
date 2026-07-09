//! Centralized text-measurement module — P8.
//!
//! All display-width calculations in the TUI crate go through here.
//! This is the single place to swap width policy when P16 (terminal capability
//! negotiation) lands — everything else just calls these functions.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Display width of a string in terminal cells.
///
/// Delegates to `UnicodeWidthStr::width`. CJK and emoji characters count as 2
/// cells; ASCII counts as 1; zero-width characters count as 0.
#[inline]
pub(crate) fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Display width of a single character in terminal cells.
///
/// Returns 0 for control / combining characters. The `unwrap_or(0)` semantics
/// are baked in — callers don't need to handle `Option`.
///
/// # Footgun: tab is width 0
/// `char_width('\t')` returns **0**, not a tab stop. `unicode-width` classifies
/// TAB (U+0009) as a control character. Any caller that needs visual tab
/// expansion (e.g. rendering `\t` as N columns) must handle it explicitly —
/// this module deliberately does not, because tab width is a rendering policy,
/// not a character property. Same applies to `\n`, `\r`, and other C0 controls.
///
/// # Load-bearing: ZWSP width 0
/// ZWSP (U+200B, ZERO WIDTH SPACE) correctly returns **0** here. This is
/// **load-bearing** for `THINKING_PLACEHOLDER` (`transcript.rs:50` =
/// `"\u{2026}\u{200B}"`) — the placeholder uses ZWSP to produce a non-empty
/// string that still measures as 1 display cell (the ellipsis only), allowing
/// the render path to distinguish "placeholder" from "truly empty". Changing
/// the width-0 policy here would silently break thinking-state rendering.
#[inline]
pub(crate) fn char_width(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(0)
}

/// Width of a &str up to but not including the byte at `byte_pos` (for cursor placement).
///
/// Useful for cursor placement: `width_prefix(input, cursor_byte)` gives the
/// number of display columns before the cursor.
///
/// # Panics
/// Panics in debug if `byte_pos` is not on a UTF-8 character boundary.
#[allow(dead_code)]
#[inline]
pub(crate) fn width_prefix(s: &str, byte_pos: usize) -> usize {
    UnicodeWidthStr::width(&s[..byte_pos])
}

#[cfg(test)]
mod tests {
    use super::{char_width, width, width_prefix};
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

    // ─────────────────────────────────────────────────────────────────────────
    // Unit tests — width()
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn width_empty_string_is_zero() {
        assert_eq!(width(""), 0);
    }

    #[test]
    fn width_ascii_hello_is_five() {
        assert_eq!(width("hello"), 5);
    }

    #[test]
    fn width_cjk_three_chars_is_six_cells() {
        // Each CJK character occupies 2 terminal cells.
        assert_eq!(width("日本語"), 6);
    }

    #[test]
    fn width_dice_emoji_is_two_cells() {
        assert_eq!(width("🎲"), 2);
    }

    #[test]
    fn width_mixed_ascii_cjk_is_four_cells() {
        // 'a' = 1, '日' = 2, 'b' = 1 → 4
        assert_eq!(width("a日b"), 4);
    }

    #[test]
    fn width_zero_width_space_is_zero() {
        // U+200B ZERO WIDTH SPACE — unicode-width returns Some(0), so width = 0.
        assert_eq!(width("\u{200B}"), 0);
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Unit tests — char_width()
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn char_width_ascii_a_is_one() {
        assert_eq!(char_width('a'), 1);
    }

    #[test]
    fn char_width_cjk_nihon_is_two() {
        assert_eq!(char_width('日'), 2);
    }

    #[test]
    fn char_width_nul_control_is_zero() {
        // unicode-width 0.2 returns None for NUL (control char).
        // Spike's unwrap_or(0) wrapping must produce 0.
        assert_eq!(char_width('\0'), 0);
        // Double-check the raw library agrees this is the right interpretation.
        assert_eq!(UnicodeWidthChar::width('\0').unwrap_or(0), 0);
    }

    #[test]
    fn char_width_tab_is_zero_per_unicode_width() {
        // U+0009 TAB — unicode-width 0.2 returns None (control char), so
        // char_width produces 0.  Callers wanting visual tab expansion need
        // to handle this themselves; the policy module deliberately doesn't.
        assert_eq!(char_width('\t'), 0);
        // Make the raw library contract explicit so a future unicode-width
        // upgrade that changes this breaks loudly here first.
        assert_eq!(
            UnicodeWidthChar::width('\t').unwrap_or(0),
            0,
            "unicode-width's tab handling changed — review char_width policy"
        );
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Equivalence tests — text_metrics must match unicode_width 1:1
    // ─────────────────────────────────────────────────────────────────────────
    //
    // These are the regression guard: for every input the wrapper must return
    // exactly the same value as the underlying library.  If this ever breaks,
    // Spike introduced a diverging policy — which is intentional for P16 but
    // must be a deliberate, tracked change.

    fn assert_width_eq(s: &str) {
        let expected = UnicodeWidthStr::width(s);
        let got = width(s);
        assert_eq!(
            got, expected,
            "width({s:?}): got {got}, want {expected} (from UnicodeWidthStr)"
        );
    }

    fn assert_char_width_eq(c: char) {
        let expected = UnicodeWidthChar::width(c).unwrap_or(0);
        let got = char_width(c);
        assert_eq!(
            got, expected,
            "char_width({c:?}): got {got}, want {expected} (from UnicodeWidthChar)"
        );
    }

    #[test]
    fn equivalence_ascii_strings() {
        for s in &["hello", "world", "test string"] {
            assert_width_eq(s);
        }
    }

    #[test]
    fn equivalence_cjk_strings() {
        for s in &["日本語", "中文", "한글"] {
            assert_width_eq(s);
        }
    }

    #[test]
    fn equivalence_emoji_strings() {
        for s in &["🎲", "🎮🎯"] {
            assert_width_eq(s);
        }
    }

    #[test]
    fn equivalence_mixed_ascii_cjk_emoji() {
        assert_width_eq("hello 日本 🎲");
    }

    #[test]
    fn equivalence_empty_string() {
        assert_width_eq("");
    }

    #[test]
    fn equivalence_whitespace_only() {
        for s in &[" ", "   ", "\n", "\r\n"] {
            assert_width_eq(s);
        }
    }

    #[test]
    fn equivalence_long_ascii_string() {
        let long: String = "abcdefghijklmnopqrstuvwxyz ".repeat(8); // 216 chars
        assert!(long.len() > 200, "sanity: string is long enough");
        assert_width_eq(&long);
    }

    #[test]
    fn equivalence_char_width_ascii() {
        for c in 'a'..='z' {
            assert_char_width_eq(c);
        }
    }

    #[test]
    fn equivalence_char_width_cjk() {
        for c in ['日', '本', '語', '中', '文'] {
            assert_char_width_eq(c);
        }
    }

    #[test]
    fn equivalence_char_width_emoji() {
        for c in ['🎲', '🎮', '🎯'] {
            assert_char_width_eq(c);
        }
    }

    #[test]
    fn equivalence_char_width_control_chars() {
        for c in ['\0', '\t', '\n', '\r', '\x1b'] {
            assert_char_width_eq(c);
        }
    }

    // ─────────────────────────────────────────────────────────────────────────
    // width_prefix() tests — lock in behavior for P16 activation
    // ─────────────────────────────────────────────────────────────────────────
    //
    // width_prefix is #[allow(dead_code)] today.  These tests are the safety
    // net so when P16 activates it, we know the contract was always met.

    #[test]
    fn width_prefix_zero_byte_pos_is_zero() {
        // Prefix of 0 bytes = empty string = 0 cells.
        assert_eq!(width_prefix("hello", 0), 0);
    }

    #[test]
    fn width_prefix_full_ascii_string() {
        // Prefix of all 5 bytes of "hello" = "hello" = 5 cells.
        assert_eq!(width_prefix("hello", 5), 5);
    }

    #[test]
    fn width_prefix_cjk_one_char_boundary() {
        // "日本" — "日" is 3 UTF-8 bytes, width 2.  Prefix of 3 bytes = "日" = 2 cells.
        assert_eq!(width_prefix("日本", 3), 2);
    }

    #[test]
    fn width_prefix_mixed_ascii_cjk_four_bytes() {
        // "a日b" — 'a' is byte 0, '日' occupies bytes 1-3, 'b' is byte 4.
        // Prefix of 4 bytes = "a日" = 1 + 2 = 3 cells.
        assert_eq!(width_prefix("a日b", 4), 3);
    }
}
