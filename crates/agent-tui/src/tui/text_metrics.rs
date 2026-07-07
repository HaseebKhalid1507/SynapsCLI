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
/// Returns 0 for control / combining characters. Use `unwrap_or(0)` semantics
/// are baked in — callers don't need to handle `Option`.
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
