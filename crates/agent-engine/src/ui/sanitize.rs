//! `ui_sanitize`: the one sanitizer for every string a card renders (§4.3).
//!
//! It is `sanitize_prompt_text` (`session::types`) plus the rest of the
//! default-ignorable set the event renderer drops (`events::format`): WJ,
//! BOM, soft hyphen, variation selectors, tag characters. ZWJ and ZWNJ are
//! kept, as the prompt sanitizer keeps them, for emoji and scripts.
//!
//! Line endings first (v3.2 #1): `\r\n` and a lone `\r` become `\n` before
//! anything else looks at the string. Then `\t` becomes a space, and a
//! single-line field folds `\n` to a space.
//!
//! Two uses, one table:
//! - **Validation** of what the agent wrote ([`refused_char`]): a C0/C1
//!   control (other than `\n`, `\t` and a line-ending `\r`) or a bidi
//!   embedding, override or isolate is refused with a teaching line, never
//!   rewritten.
//! - **Normalization and display** ([`ui_sanitize`]): everything else in
//!   the set is dropped. It is idempotent, and it never adds a byte.
//!
//! Display sanitizing never changes data sent to the model: answer values
//! are validated (`answer::parse_answer`), not rewritten. A client turns
//! CRLF into `\n` in values before it commits; a `\r` in a value is refused.

use std::borrow::Cow;

use crate::events::format::is_invisible;

pub use agent_core::core::ui_types::Lines;

use super::BY_MAX_BYTES;

/// `\r\n` and a lone `\r` as `\n`.
pub fn normalize_newlines(s: &str) -> Cow<'_, str> {
    if s.contains('\r') {
        Cow::Owned(s.replace("\r\n", "\n").replace('\r', "\n"))
    } else {
        Cow::Borrowed(s)
    }
}

/// The rendered form of `s`.
pub fn ui_sanitize(s: &str, lines: Lines) -> String {
    normalize_newlines(s)
        .chars()
        .filter_map(|c| sanitize_char(c, lines))
        .collect()
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
        '\n' | '\t' | '\r' => None,
        c if c.is_control() => Some("a control character"),
        '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' => Some("a bidi control"),
        _ => None,
    }
}

/// Whether a user-typed value holds a C0/C1 control (§3.3 #11). `\n` and
/// `\t` are allowed only where `lines` is `Multi` (`long` fields); `\r`
/// never is.
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

/// `s` cut to at most `max` bytes on a character boundary, ending in `…`
/// when it was cut.
pub fn cut_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let room = max.saturating_sub('…'.len_utf8());
    let end = s
        .char_indices()
        .map(|(i, c)| i + c.len_utf8())
        .take_while(|&end| end <= room)
        .last()
        .unwrap_or(0);
    format!("{}…", &s[..end])
}

/// `AnswerState.by`, a roster label for other clients' receipts: sanitized,
/// one line, at most 64 bytes, cut with `…`, never refused (v3.2 #11).
pub fn roster_label(raw: &str) -> String {
    cut_bytes(ui_sanitize(raw, Lines::Single).trim(), BY_MAX_BYTES)
}

/// The key labels are compared by: Unicode full case folding
/// (`CaseFolding.txt`, statuses C and F, as Python's `str.casefold`), on
/// the sanitized label. No other normalization (no NFC). Used for "labels
/// must differ (ignoring case)" and for matching `default` to a label, so
/// the match is never ambiguous (v3.2 #4, Shady #439 M3).
///
/// Built from the standard library's case tables, character by character:
/// lowercase, uppercase, lowercase, which is full case folding except for
/// U+0131 (dotless i, which folding keeps) and Cherokee (which folding maps
/// to uppercase). The corpus pins it against `str.casefold` for every
/// scalar value (`casefold.json`).
pub fn fold_key(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c == '\u{0131}' {
            out.push(c);
            continue;
        }
        for l in c.to_lowercase() {
            for u in l.to_uppercase() {
                for f in u.to_lowercase() {
                    if matches!(f, '\u{AB70}'..='\u{ABBF}' | '\u{13F8}'..='\u{13FD}') {
                        out.extend(f.to_uppercase());
                    } else {
                        out.push(f);
                    }
                }
            }
        }
    }
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
            let expected_multi = if c == '\r' {
                Some('\n')
            } else {
                expected_multi
            };
            let expected_single = if matches!(c, '\n' | '\r') {
                Some(' ')
            } else {
                expected_multi
            };
            let as_string = |c: Option<char>| c.map(String::from).unwrap_or_default();
            assert_eq!(
                ui_sanitize(&s, Lines::Multi),
                as_string(expected_multi),
                "U+{:04X} multi",
                c as u32
            );
            assert_eq!(
                ui_sanitize(&s, Lines::Single),
                as_string(expected_single),
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
            '\u{009B}', // C0, DEL, C1
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
        // v3.2 #1: line endings before anything else.
        assert_eq!(
            ui_sanitize("a\r\nb\rc\r\r\nd", Lines::Multi),
            "a\nb\nc\n\nd"
        );
        assert_eq!(ui_sanitize("a\r\nb\rc", Lines::Single), "a b c");
        assert_eq!(ui_sanitize("\r\n", Lines::Multi), "\n");
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
                '\n' | '\t' | '\r' => None,
                c if c.is_control() => Some("a control character"),
                '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' => Some("a bidi control"),
                _ => None,
            };
            assert_eq!(refusal(c), expect, "U+{:04X}", c as u32);
            if expect.is_some() {
                assert_eq!(
                    ui_sanitize(&c.to_string(), Lines::Multi),
                    "",
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
    fn roster_labels_are_cut_to_64_bytes() {
        assert_eq!(
            roster_label(&"n".repeat(BY_MAX_BYTES)),
            "n".repeat(BY_MAX_BYTES)
        );
        let cut = roster_label(&"n".repeat(BY_MAX_BYTES + 1));
        assert_eq!(cut, format!("{}…", "n".repeat(BY_MAX_BYTES - 3)));
        assert_eq!(cut.len(), BY_MAX_BYTES);
        // 4-byte characters: cut on a boundary, never over.
        let wide = roster_label(&"\u{1F600}".repeat(17));
        assert!(wide.len() <= BY_MAX_BYTES && wide.ends_with('…'), "{wide}");
        assert_eq!(wide.len(), 15 * 4 + 3);
        assert_eq!(roster_label(" desk\u{202E}-2\n(#3) "), "desk-2 (#3)");
        assert_eq!(roster_label(""), "");
    }

    #[test]
    fn fold_key_is_full_case_folding() {
        for (a, b) in [
            ("Yes", "YES"),
            ("\u{3A3}", "\u{3C2}"),
            ("\u{3C3}", "\u{3C2}"),
            ("Stra\u{DF}e", "STRASSE"),
            ("\u{1E9E}", "ss"),
            ("\u{FB01}le", "FILE"),
            ("\u{212A}", "k"),
            ("\u{1C4}", "\u{1C5}"),
            ("\u{13A0}", "\u{AB70}"),
            ("\u{130}", "i\u{307}"),
        ] {
            assert_eq!(fold_key(a), fold_key(b), "{a} ~ {b}");
        }
        for (a, b) in [
            ("\u{131}", "i"),
            ("\u{131}", "I"),
            ("e\u{301}", "\u{E9}"),
            ("a", "b"),
        ] {
            assert_ne!(fold_key(a), fold_key(b), "{a} !~ {b}");
        }
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
