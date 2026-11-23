//! `ui_sanitize`: the one sanitizer for every string a card renders (§4.3).
//!
//! It is `sanitize_prompt_text` (`session::types`) plus the rest of the
//! default-ignorable set the event renderer drops (`events::format`): WJ,
//! BOM, soft hyphen, variation selectors, tag characters. ZWJ and ZWNJ are
//! kept, as the prompt sanitizer keeps them, for emoji and scripts. `\t`
//! becomes a space. A single-line field also folds `\n` to a space.
//!
//! Two uses, one table:
//! - **Validation** of what the agent wrote ([`refused_char`]): a C0/C1
//!   control (other than `\n` and `\t`) or a bidi embedding, override or
//!   isolate is refused with a teaching line, never rewritten.
//! - **Normalization and display** ([`ui_sanitize`]): everything else in
//!   the set is dropped. It is idempotent, and it never adds a character.
//!
//! Display sanitizing never changes data sent to the model: answer values
//! are validated (`answer::parse_answer`), not rewritten.

use crate::events::format::is_invisible;

/// Whether a field is one line (question, labels, hints, placeholders, text
/// values) or many (`detail`, `long` values).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lines {
    Single,
    Multi,
}

/// The rendered form of `s`.
pub fn ui_sanitize(s: &str, lines: Lines) -> String {
    s.chars().filter_map(|c| sanitize_char(c, lines)).collect()
}

fn sanitize_char(c: char, lines: Lines) -> Option<char> {
    match c {
        '\n' if lines == Lines::Multi => Some('\n'),
        '\n' | '\t' => Some(' '),
        // ZWNJ and ZWJ: kept for scripts and emoji sequences.
        '\u{200C}' | '\u{200D}' => Some(c),
        c if c.is_control() => None,
        // Line and paragraph separators (the prompt sanitizer drops them).
        '\u{2028}' | '\u{2029}' => None,
        c if is_invisible(c) => None,
        c => Some(c),
    }
}

/// Why an agent-written string is refused, if it is: the first character
/// that can reorder or break what the user reads. The caller names the
/// field and the code point.
pub fn refused_char(s: &str) -> Option<(char, &'static str)> {
    s.chars().find_map(|c| refusal(c).map(|why| (c, why)))
}

fn refusal(c: char) -> Option<&'static str> {
    match c {
        '\n' | '\t' => None,
        c if c.is_control() => Some("a control character"),
        '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' => Some("a bidi control"),
        _ => None,
    }
}

/// Whether a user-typed value holds a C0/C1 control (§3.3 #11). `\n` and
/// `\t` are allowed only where `lines` is `Multi` (`long` fields).
pub fn value_control(s: &str, lines: Lines) -> Option<char> {
    s.chars().find(|&c| match c {
        '\n' | '\t' => lines == Lines::Single,
        c => c.is_control(),
    })
}

/// Unicode scalar count: the unit of every `chars` bound in §4.4 and of
/// JSON Schema's `minLength` / `maxLength`.
pub fn char_len(s: &str) -> usize {
    s.chars().count()
}

/// `s` cut to at most `max` characters on a character boundary, ending in
/// `…` when it was cut.
pub fn cut_chars(s: &str, max: usize) -> String {
    if char_len(s) <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// The advisory secret heuristic of §4.7,
/// `/pass(word)?|token|secret|api.?key|private.?key/i`. A match adds a host
/// footnote ("Don't type secrets here"); it never refuses a card.
pub fn looks_secret(label: &str) -> bool {
    static SECRET: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?i)pass(word)?|token|secret|api.?key|private.?key")
            .expect("static regex")
    });
    SECRET.is_match(label)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::types::sanitize_prompt_text;

    const ZWJ: char = '\u{200D}';
    const ZWNJ: char = '\u{200C}';

    fn all_chars() -> impl Iterator<Item = char> {
        (0..=0x10FFFFu32).filter_map(char::from_u32)
    }

    /// Every scalar value: ui_sanitize drops exactly what the prompt
    /// sanitizer drops plus the event renderer's default-ignorables, minus
    /// ZWJ/ZWNJ; `\t` and (single-line) `\n` become spaces; nothing else
    /// changes.
    #[test]
    fn drops_exactly_the_prompt_and_event_sets_keeping_zwj_zwnj() {
        for c in all_chars() {
            let s = c.to_string();
            let prompt_drops = sanitize_prompt_text(&s).is_empty();
            let event_drops = is_invisible(c);
            let expected_multi = match c {
                '\n' => Some('\n'),
                '\t' => Some(' '),
                ZWJ | ZWNJ => Some(c),
                _ if prompt_drops || event_drops => None,
                _ => Some(c),
            };
            let expected_single = if c == '\n' { Some(' ') } else { expected_multi };
            assert_eq!(
                sanitize_char(c, Lines::Multi),
                expected_multi,
                "U+{:04X} multi",
                c as u32
            );
            assert_eq!(
                sanitize_char(c, Lines::Single),
                expected_single,
                "U+{:04X} single",
                c as u32
            );
        }
    }

    #[test]
    fn named_characters() {
        let dropped = [
            '\u{0000}',
            '\u{0007}',
            '\u{001B}',
            '\u{007F}',
            '\u{0085}',
            '\u{009B}',
            '\r', // C0, DEL, C1
            '\u{200B}',
            '\u{200E}',
            '\u{200F}',
            '\u{061C}', // ZWSP, LRM, RLM, ALM
            '\u{202A}',
            '\u{202B}',
            '\u{202C}',
            '\u{202D}',
            '\u{202E}', // embeddings, overrides
            '\u{2066}',
            '\u{2067}',
            '\u{2068}',
            '\u{2069}', // isolates
            '\u{2028}',
            '\u{2029}', // separators
            '\u{2060}',
            '\u{FEFF}',
            '\u{00AD}', // WJ, BOM, soft hyphen
            '\u{FE00}',
            '\u{FE0F}',
            '\u{E0100}',
            '\u{E01EF}', // variation selectors
            '\u{E0001}',
            '\u{E0041}',
            '\u{E007F}', // tag characters
        ];
        for c in dropped {
            assert_eq!(
                ui_sanitize(&format!("a{c}b"), Lines::Multi),
                "ab",
                "U+{:04X}",
                c as u32
            );
        }
        assert_eq!(ui_sanitize("a\tb\nc", Lines::Multi), "a b\nc");
        assert_eq!(ui_sanitize("a\tb\nc", Lines::Single), "a b c");
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
        assert_eq!(ui_sanitize(family, Lines::Single), family);
        let persian = "\u{0645}\u{06CC}\u{200C}\u{062E}\u{0648}\u{0627}\u{0647}\u{0645}";
        assert_eq!(ui_sanitize(persian, Lines::Single), persian);
        let url = "see [x](https://evil.example/a?b=<c>) or https://x.test";
        assert_eq!(
            ui_sanitize(url, Lines::Single),
            url,
            "URL-like text is text"
        );
    }

    #[test]
    fn idempotent_and_never_longer() {
        let samples = [
            "",
            "plain",
            "a\u{202E}b\u{200B}c\n\td\u{1F600}\u{FE0F}",
            "\u{0085}\u{2028}x\r\n",
        ];
        for s in samples {
            for lines in [Lines::Single, Lines::Multi] {
                let once = ui_sanitize(s, lines);
                assert_eq!(ui_sanitize(&once, lines), once);
                assert!(char_len(&once) <= char_len(s));
                assert!(once.len() <= s.len());
            }
        }
    }

    #[test]
    fn refusal_class_is_controls_and_bidi_overrides_only() {
        for c in all_chars() {
            let expect = match c {
                '\n' | '\t' => None,
                c if c.is_control() => Some("a control character"),
                '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' => Some("a bidi control"),
                _ => None,
            };
            assert_eq!(refusal(c), expect, "U+{:04X}", c as u32);
            if expect.is_some() {
                assert_eq!(
                    sanitize_char(c, Lines::Multi),
                    None,
                    "every refused char is also dropped"
                );
            }
        }
        assert_eq!(
            refused_char("ok\u{202E}x\u{1B}"),
            Some(('\u{202E}', "a bidi control"))
        );
        assert_eq!(
            refused_char("a\u{200B}b\u{200E}"),
            None,
            "marks and ZWSP are dropped, not refused"
        );
    }

    #[test]
    fn value_controls() {
        assert_eq!(value_control("a\nb\tc", Lines::Multi), None);
        assert_eq!(value_control("a\nb", Lines::Single), Some('\n'));
        assert_eq!(value_control("a\tb", Lines::Single), Some('\t'));
        assert_eq!(value_control("a\rb", Lines::Multi), Some('\r'));
        assert_eq!(value_control("a\u{0085}", Lines::Multi), Some('\u{0085}'));
        assert_eq!(
            value_control("\u{202E}bidi ok in values", Lines::Single),
            None
        );
    }

    #[test]
    fn cut_on_char_boundaries() {
        assert_eq!(cut_chars("abc", 3), "abc");
        assert_eq!(cut_chars("abcd", 3), "ab…");
        assert_eq!(char_len(&cut_chars(&"\u{1F600}".repeat(81), 80)), 80);
        assert!(cut_chars(&"\u{1F600}".repeat(81), 80).ends_with('…'));
    }

    #[test]
    fn secret_heuristic_is_the_documented_regex() {
        for yes in [
            "Password",
            "passphrase",
            "GitHub token",
            "Client secret",
            "API key",
            "api_key",
            "apikey",
            "Private key",
            "private-key",
        ] {
            assert!(looks_secret(yes), "{yes}");
        }
        for no in ["Version", "Notes", "Publish", "Keyboard layout", "apiary"] {
            assert!(!looks_secret(no), "{no}");
        }
    }
}
