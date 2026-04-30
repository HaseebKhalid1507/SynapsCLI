pub const DEFAULT_MAX_VOICE_TRANSCRIPT_CHARS: usize = 16_000;

pub fn sanitize_voice_transcript(input: &str, max_chars: usize) -> String {
    let normalized = input.replace("\r\n", "\n").replace('\r', "\n");
    let without_ansi = strip_ansi_escape_sequences(&normalized);
    let mut output = String::new();
    for ch in without_ansi.chars() {
        if output.chars().count() >= max_chars {
            break;
        }
        match ch {
            '\n' | '\t' => output.push(ch),
            c if c.is_control() => {}
            c => output.push(c),
        }
    }
    output
}

fn strip_ansi_escape_sequences(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' && matches!(chars.peek(), Some('[')) {
            chars.next();
            for next in chars.by_ref() {
                if ('@'..='~').contains(&next) {
                    break;
                }
            }
        } else {
            output.push(ch);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_crlf_and_stray_cr() {
        assert_eq!(sanitize_voice_transcript("a\r\nb\rc", 100), "a\nb\nc");
    }

    #[test]
    fn strips_ansi_escape_sequences() {
        assert_eq!(sanitize_voice_transcript("hello \u{1b}[31mred\u{1b}[0m", 100), "hello red");
    }

    #[test]
    fn strips_disallowed_control_characters_but_keeps_tab_and_newline() {
        assert_eq!(sanitize_voice_transcript("a\u{0000}b\t c\n", 100), "ab\t c\n");
    }

    #[test]
    fn truncates_by_chars_without_splitting_unicode() {
        assert_eq!(sanitize_voice_transcript("🙂🙂🙂abc", 4), "🙂🙂🙂a");
    }

    #[test]
    fn zero_max_chars_returns_empty_string() {
        assert_eq!(sanitize_voice_transcript("hello", 0), "");
    }
}
