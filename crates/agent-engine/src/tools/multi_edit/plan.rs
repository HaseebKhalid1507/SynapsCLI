//! Pure in-memory matching and application of one anchored edit.
//!
//! Everything here works on a `String` and never touches the filesystem, so
//! the rule that a failed edit writes nothing is easy to keep: the caller
//! applies every edit of a call to in-memory copies first and writes
//! nothing if any edit fails.

use super::{AnchorMode, Op};

/// One anchor match, as byte offsets into the content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Match {
    /// Start of the matched span.
    pub start: usize,
    /// End of the matched span (exclusive).
    pub end: usize,
}

/// What one edit did, for the per-edit result line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Applied {
    Edit {
        first_line: usize,
        last_line: usize,
        removed: usize,
        added: usize,
        note: Option<&'static str>,
    },
    /// `unless_present` was found: nothing changed.
    Skipped { line: usize },
}

/// Lines of `s` as `(start, content_end, next, terminated)`. The terminator
/// (`\n` or `\r\n`) is not part of the line content. A trailing newline does
/// not start an extra empty line.
pub(crate) fn lines(s: &str) -> Vec<(usize, usize, usize, bool)> {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < s.len() {
        match s[pos..].find('\n') {
            Some(i) => {
                let nl = pos + i;
                let end = if nl > pos && bytes[nl - 1] == b'\r' {
                    nl - 1
                } else {
                    nl
                };
                out.push((pos, end, nl + 1, true));
                pos = nl + 1;
            }
            None => {
                out.push((pos, s.len(), s.len(), false));
                pos = s.len();
            }
        }
    }
    out
}

/// 1-based line number of byte `offset`.
pub(crate) fn line_of(s: &str, offset: usize) -> usize {
    s.as_bytes()[..offset.min(s.len())]
        .iter()
        .filter(|&&b| b == b'\n')
        .count()
        + 1
}

/// Every match of `anchor` in `content` under `mode`, in byte order.
///
/// `exact` counts a match at every start position, so a self-overlapping
/// anchor (`aa` in `aaa`) is ambiguous rather than silently first-wins.
/// `line_regex` runs per line, so no match can cross a newline; with one
/// capture group the span is that group (a stand-in for look-around).
pub(crate) fn find_matches(
    content: &str,
    anchor: &str,
    mode: AnchorMode,
) -> Result<Vec<Match>, String> {
    if anchor.is_empty() {
        return Err("anchor is empty".into());
    }
    let mut out = Vec::new();
    match mode {
        AnchorMode::Exact => {
            let mut i = 0;
            while i <= content.len() {
                let Some(p) = content[i..].find(anchor) else {
                    break;
                };
                let start = i + p;
                let end = start + anchor.len();
                out.push(Match { start, end });
                let step = content[start..]
                    .chars()
                    .next()
                    .map(char::len_utf8)
                    .unwrap_or(1);
                i = start + step;
            }
        }
        AnchorMode::LinePrefix => {
            for (start, end, _, _) in lines(content) {
                if content[start..end].starts_with(anchor) {
                    out.push(Match { start, end });
                }
            }
        }
        AnchorMode::LineRegex => {
            let re = regex::RegexBuilder::new(anchor)
                .size_limit(1 << 20)
                .build()
                .map_err(|e| format!("invalid line_regex: {e}"))?;
            let groups = re.captures_len();
            if groups > 2 {
                return Err(format!(
                    "line_regex has {} capture groups; use at most one (write other groups as (?:...))",
                    groups - 1
                ));
            }
            for (start, end, _, _) in lines(content) {
                let line = &content[start..end];
                for caps in re.captures_iter(line) {
                    let span = if groups == 2 { caps.get(1) } else { caps.get(0) };
                    if let Some(m) = span {
                        out.push(Match {
                            start: start + m.start(),
                            end: start + m.end(),
                        });
                    }
                }
            }
        }
    }
    Ok(out)
}

fn match_lines(content: &str, matches: &[Match]) -> String {
    const SHOWN: usize = 8;
    let mut parts: Vec<String> = matches
        .iter()
        .take(SHOWN)
        .map(|m| format!("L{}", line_of(content, m.start)))
        .collect();
    if matches.len() > SHOWN {
        parts.push("…".into());
    }
    parts.join(", ")
}

/// Pick the one match the edit targets, or explain the count.
fn select(content: &str, matches: &[Match], occurrence: Option<u64>) -> Result<Match, String> {
    let n = matches.len();
    match occurrence {
        None if n == 1 => Ok(matches[0]),
        None if n == 0 => Err("anchor not found (0 matches; needs exactly 1)".into()),
        None => Err(format!(
            "anchor matched {n} times (at {}); needs exactly 1: give occurrence 1-{n} or a longer anchor",
            match_lines(content, matches)
        )),
        Some(k) if k >= 1 && (k as usize) <= n => Ok(matches[k as usize - 1]),
        Some(k) => Err(format!(
            "anchor matched {n} times{}; occurrence {k} does not exist",
            if n == 0 {
                String::new()
            } else {
                format!(" (at {})", match_lines(content, matches))
            }
        )),
    }
}

/// Lines covered by `len` bytes inserted at `pos` (in the new content).
fn span_lines(content: &str, pos: usize, len: usize) -> (usize, usize) {
    let first = line_of(content, pos);
    let last = if len == 0 {
        first
    } else {
        line_of(content, pos + len - 1)
    };
    (first, last)
}

/// Whether `content`'s dominant line ending is `\r\n`: more CRLF than bare
/// LF terminators. A tie, or no newline at all, is LF.
pub(crate) fn is_crlf(content: &str) -> bool {
    let lf = content.matches('\n').count();
    let crlf = content.matches("\r\n").count();
    crlf > lf - crlf
}

/// `s` with every `\n` not already preceded by `\r` written as `\r\n`.
pub(crate) fn to_crlf(s: &str) -> std::borrow::Cow<'_, str> {
    if !s.contains('\n') {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + s.len() / 16);
    let mut prev = '\0';
    for c in s.chars() {
        if c == '\n' && prev != '\r' {
            out.push('\r');
        }
        out.push(c);
        prev = c;
    }
    std::borrow::Cow::Owned(out)
}

/// Apply one edit to `content` in place. `text` is already expanded; for
/// `append_jsonl` it is the compact row without its newline.
///
/// `crlf`: the file's dominant line ending is `\r\n` ([`is_crlf`]). Then a
/// `\n` in `text`, an `exact` anchor and `unless_present` means `\r\n`, and
/// every newline this function adds itself is `\r\n`.
///
/// `insert_before`/`insert_after`, every anchor mode: text that ends in a
/// newline is whole lines, so it goes in at the start of the line where the
/// match starts (before) or after the line where the match ends (after);
/// other text goes right at the match (a `line_prefix` match is the line's
/// content, so that is the line's start or end). `append` adds a missing
/// final newline first unless the text starts with one.
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply(
    content: &mut String,
    op: Op,
    mode: AnchorMode,
    anchor: Option<&str>,
    occurrence: Option<u64>,
    text: &str,
    unless_present: Option<&str>,
    crlf: bool,
) -> Result<Applied, String> {
    let eol = if crlf { "\r\n" } else { "\n" };
    let adapt = |s: &'_ str| -> String {
        if crlf {
            to_crlf(s).into_owned()
        } else {
            s.to_string()
        }
    };
    if let Some(guard) = unless_present {
        let guard = adapt(guard);
        if let Some(at) = content.find(guard.as_str()) {
            // Report the line of the guard's first visible character, so
            // a guard written as "\n17. " points at the "17." line.
            let lead = guard.len() - guard.trim_start_matches(['\n', '\r']).len();
            return Ok(Applied::Skipped {
                line: line_of(content, at + lead),
            });
        }
    }
    let text = if op == Op::AppendJsonl { text.to_string() } else { adapt(text) };
    let text = text.as_str();
    let mut note = None;
    let (pos, removed, added) = match op {
        Op::Replace | Op::InsertBefore | Op::InsertAfter => {
            let anchor = anchor.ok_or("anchor is required")?;
            let anchor = if mode == AnchorMode::Exact { adapt(anchor) } else { anchor.to_string() };
            let matches = find_matches(content, &anchor, mode)?;
            let m = select(content, &matches, occurrence)?;
            // Text ending in a newline is whole lines: it goes in at a line
            // boundary, never mid-line. Other text goes right at the match.
            let whole_lines = text.ends_with('\n');
            match op {
                Op::Replace => {
                    content.replace_range(m.start..m.end, text);
                    (m.start, m.end - m.start, text.len())
                }
                Op::InsertBefore => {
                    let line_start = content[..m.start].rfind('\n').map_or(0, |i| i + 1);
                    let at = if whole_lines && line_start != m.start {
                        note = Some("text ends in a newline: inserted before the anchor's line");
                        line_start
                    } else {
                        m.start
                    };
                    content.insert_str(at, text);
                    (at, 0, text.len())
                }
                _ => {
                    let at = if whole_lines && m.end > 0 && !content[..m.end].ends_with('\n') {
                        match content[m.end..].find('\n') {
                            // A line_prefix match always ends at its line's
                            // end; say so only when it moved off mid-line.
                            Some(i) if i == 0 || content[m.end..m.end + i] == *"\r" => m.end + i + 1,
                            Some(i) => {
                                note = Some("text ends in a newline: inserted after the anchor's line");
                                m.end + i + 1
                            }
                            None => {
                                content.push_str(eol);
                                note = Some("added newline after the last line");
                                content.len()
                            }
                        }
                    } else {
                        m.end
                    };
                    content.insert_str(at, text);
                    (at, 0, text.len())
                }
            }
        }
        Op::Append => {
            if !content.is_empty()
                && !content.ends_with('\n')
                && !text.starts_with('\n')
                && !text.starts_with("\r\n")
            {
                content.push_str(eol);
                note = Some("added missing newline before the text");
            }
            let at = content.len();
            content.push_str(text);
            (at, 0, text.len())
        }
        Op::AppendJsonl => {
            if !content.is_empty() && !content.ends_with('\n') {
                content.push_str(eol);
                note = Some("added missing newline before the row");
            }
            let at = content.len();
            content.push_str(text);
            content.push_str(eol);
            (at, 0, text.len() + eol.len())
        }
    };
    let (first_line, last_line) = span_lines(content, pos, added);
    Ok(Applied::Edit {
        first_line,
        last_line,
        removed,
        added,
        note,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(
        content: &str,
        op: Op,
        mode: AnchorMode,
        anchor: Option<&str>,
        occurrence: Option<u64>,
        text: &str,
    ) -> Result<(String, Applied), String> {
        let mut s = content.to_string();
        let applied = apply(&mut s, op, mode, anchor, occurrence, text, None, false)?;
        Ok((s, applied))
    }

    #[test]
    fn lines_split_lf_and_crlf_without_a_phantom_last_line() {
        assert_eq!(lines(""), vec![]);
        assert_eq!(lines("a\n"), vec![(0, 1, 2, true)]);
        assert_eq!(lines("a\r\nb"), vec![(0, 1, 3, true), (3, 4, 4, false)]);
    }

    #[test]
    fn exact_counts_overlapping_starts() {
        let m = find_matches("aaa", "aa", AnchorMode::Exact).unwrap();
        assert_eq!(m.len(), 2, "a self-overlapping anchor must be ambiguous");
        let err = run("aaa", Op::Replace, AnchorMode::Exact, Some("aa"), None, "b").unwrap_err();
        assert!(err.contains("matched 2 times"), "{err}");
    }

    #[test]
    fn missing_anchor_never_inserts_anywhere() {
        // The find() == -1 class: nothing may land before the last char.
        for op in [Op::Replace, Op::InsertBefore, Op::InsertAfter] {
            let err = run("abc\n", op, AnchorMode::Exact, Some("zz"), None, "X").unwrap_err();
            assert!(err.contains("0 matches"), "{err}");
        }
    }

    #[test]
    fn two_matches_refused_with_lines_unless_occurrence() {
        let src = "x\nkey\ny\nkey\n";
        let err = run(src, Op::Replace, AnchorMode::Exact, Some("key"), None, "K").unwrap_err();
        assert!(err.contains("matched 2 times (at L2, L4)"), "{err}");
        let (out, _) = run(src, Op::Replace, AnchorMode::Exact, Some("key"), Some(2), "K").unwrap();
        assert_eq!(out, "x\nkey\ny\nK\n");
        let err = run(src, Op::Replace, AnchorMode::Exact, Some("key"), Some(3), "K").unwrap_err();
        assert!(err.contains("occurrence 3 does not exist"), "{err}");
    }

    #[test]
    fn unicode_anchor_and_text_are_byte_exact() {
        let src = "“quoted” → ≥ × 漢字 🦀 end\n";
        let (out, applied) = run(
            src,
            Op::InsertAfter,
            AnchorMode::Exact,
            Some("漢字 🦀"),
            None,
            " «新»",
        )
        .unwrap();
        assert_eq!(out, "“quoted” → ≥ × 漢字 🦀 «新» end\n");
        assert!(matches!(applied, Applied::Edit { added, .. } if added == " «新»".len()));
        // A near miss (straight quotes) is not a match.
        let err = run(src, Op::Replace, AnchorMode::Exact, Some("\"quoted\""), None, "q")
            .unwrap_err();
        assert!(err.contains("0 matches"), "{err}");
    }

    #[test]
    fn line_prefix_replaces_the_line_content_and_keeps_the_terminator() {
        let src = "# T (a), 10:00 CDT\r\nbody\n";
        let (out, applied) = run(
            src,
            Op::Replace,
            AnchorMode::LinePrefix,
            Some("# T "),
            None,
            "# T (a), 11:05 CDT",
        )
        .unwrap();
        assert_eq!(out, "# T (a), 11:05 CDT\r\nbody\n");
        assert!(matches!(applied, Applied::Edit { first_line: 1, last_line: 1, .. }));
    }

    #[test]
    fn line_prefix_insert_before_and_after_are_line_level() {
        let src = "a\n- 09:51 x\nb";
        let (out, applied) = run(
            src,
            Op::InsertBefore,
            AnchorMode::LinePrefix,
            Some("- 09:5"),
            None,
            "new\n",
        )
        .unwrap();
        assert_eq!(out, "a\nnew\n- 09:51 x\nb");
        assert!(matches!(applied, Applied::Edit { first_line: 2, last_line: 2, .. }));
        let (out, _) = run(src, Op::InsertAfter, AnchorMode::LinePrefix, Some("- 09"), None, "after\n")
            .unwrap();
        assert_eq!(out, "a\n- 09:51 x\nafter\nb");
    }

    #[test]
    fn line_prefix_insert_after_last_unterminated_line_adds_a_newline() {
        let (out, applied) =
            run("a\nlast", Op::InsertAfter, AnchorMode::LinePrefix, Some("la"), None, "x\n")
                .unwrap();
        assert_eq!(out, "a\nlast\nx\n");
        assert!(matches!(applied, Applied::Edit { note: Some(_), first_line: 3, .. }));
    }

    #[test]
    fn line_regex_group_replaces_only_the_group() {
        let src = "# Pria: STATUS (x), 2026-10-18 09:12 CDT\nkeep 2026-10-18 09:12 CDT\n";
        let (out, _) = run(
            src,
            Op::Replace,
            AnchorMode::LineRegex,
            Some(r"^# Pria: STATUS .*, 2026-10-18 (\S+) CDT$"),
            None,
            "$1 11:30",
        )
        .unwrap();
        // `$1` is verbatim text, never a capture reference.
        assert_eq!(
            out,
            "# Pria: STATUS (x), 2026-10-18 $1 11:30 CDT\nkeep 2026-10-18 09:12 CDT\n"
        );
    }

    #[test]
    fn line_regex_cannot_match_across_a_newline() {
        let err = run("ab\ncd\n", Op::Replace, AnchorMode::LineRegex, Some(r"b\s+c"), None, "")
            .unwrap_err();
        assert!(err.contains("0 matches"), "{err}");
        let err = run("ab\ncd\n", Op::Replace, AnchorMode::LineRegex, Some(r"(a)(b)"), None, "")
            .unwrap_err();
        assert!(err.contains("2 capture groups"), "{err}");
    }

    #[test]
    fn append_and_jsonl_add_a_missing_newline() {
        let (out, applied) = run("a", Op::Append, AnchorMode::Exact, None, None, "b").unwrap();
        assert_eq!(out, "a\nb");
        assert!(matches!(applied, Applied::Edit { first_line: 2, note: Some(_), .. }));
        // Text that starts with its own newline, an empty file and a
        // terminated last line get nothing extra.
        for (src, text, want) in [("a", "\nb", "a\nb"), ("", "b", "b"), ("a\n", "b", "a\nb")] {
            let (out, applied) = run(src, Op::Append, AnchorMode::Exact, None, None, text).unwrap();
            assert_eq!(out, want, "{src:?} + {text:?}");
            assert!(matches!(applied, Applied::Edit { note: None, .. }), "{src:?} + {text:?}");
        }
        let (out, applied) =
            run("{\"a\":1}", Op::AppendJsonl, AnchorMode::Exact, None, None, "{\"b\":2}").unwrap();
        assert_eq!(out, "{\"a\":1}\n{\"b\":2}\n");
        assert!(matches!(applied, Applied::Edit { first_line: 2, last_line: 2, note: Some(_), .. }));
    }

    /// Text that ends in a newline is whole lines in every anchor mode: it
    /// lands at a line boundary, never glued into the anchor's line. Other
    /// text goes right at the match.
    #[test]
    fn line_text_is_inserted_at_a_line_boundary_in_every_mode() {
        let src = "line one\nline two\n";
        let cases = [
            (Op::InsertAfter, AnchorMode::Exact, "line one", "line one\nline 1.5\nline two\n"),
            (Op::InsertAfter, AnchorMode::Exact, "one", "line one\nline 1.5\nline two\n"),
            (Op::InsertAfter, AnchorMode::Exact, "ne o", "line one\nline 1.5\nline two\n"),
            (Op::InsertAfter, AnchorMode::Exact, "line one\n", "line one\nline 1.5\nline two\n"),
            (Op::InsertAfter, AnchorMode::LineRegex, "o(n)e", "line one\nline 1.5\nline two\n"),
            (Op::InsertAfter, AnchorMode::LinePrefix, "line o", "line one\nline 1.5\nline two\n"),
            (Op::InsertBefore, AnchorMode::Exact, "two", "line one\nline 1.5\nline two\n"),
            (Op::InsertBefore, AnchorMode::LineRegex, "t(w)o", "line one\nline 1.5\nline two\n"),
            (Op::InsertBefore, AnchorMode::LinePrefix, "line t", "line one\nline 1.5\nline two\n"),
        ];
        for (op, mode, anchor, want) in cases {
            let (out, _) = run(src, op, mode, Some(anchor), None, "line 1.5\n").unwrap();
            assert_eq!(out, want, "{op:?} {mode:?} {anchor:?}");
        }
        // Moving off mid-line is reported; landing where the anchor already
        // ends a line is not.
        let (_, applied) =
            run(src, Op::InsertAfter, AnchorMode::Exact, Some("ne o"), None, "x\n").unwrap();
        assert!(
            matches!(applied, Applied::Edit { first_line: 2, note: Some(n), .. } if n.contains("after the anchor's line")),
            "{applied:?}"
        );
        let (_, applied) =
            run(src, Op::InsertAfter, AnchorMode::Exact, Some("line one"), None, "x\n").unwrap();
        assert!(matches!(applied, Applied::Edit { first_line: 2, note: None, .. }), "{applied:?}");
        // Inline text stays inline, in every mode.
        let (out, _) = run(src, Op::InsertAfter, AnchorMode::Exact, Some("one"), None, "!").unwrap();
        assert_eq!(out, "line one!\nline two\n");
        let (out, _) =
            run(src, Op::InsertAfter, AnchorMode::LinePrefix, Some("line o"), None, " (1)").unwrap();
        assert_eq!(out, "line one (1)\nline two\n");
        let (out, _) =
            run(src, Op::InsertBefore, AnchorMode::LinePrefix, Some("line t"), None, "- ").unwrap();
        assert_eq!(out, "line one\n- line two\n");
    }

    #[test]
    fn crlf_is_detected_by_majority_and_converted_once() {
        assert!(is_crlf("a\r\nb\r\nc\n"));
        assert!(!is_crlf("a\r\nb\nc\n"), "a tie is LF");
        assert!(!is_crlf("no newline") && !is_crlf(""));
        assert_eq!(to_crlf("a\nb\r\nc\n"), "a\r\nb\r\nc\r\n");
        assert!(matches!(to_crlf("abc"), std::borrow::Cow::Borrowed(_)));
    }

    /// In a CRLF file, `\n` in the text, an exact anchor and the guard means
    /// `\r\n`, and every newline the edit adds is `\r\n`: no mixed endings.
    #[test]
    fn a_crlf_file_gets_crlf_everywhere_an_edit_writes_a_newline() {
        let crlf = |src: &str, op, mode, anchor: Option<&str>, text: &str| {
            let mut s = src.to_string();
            let applied = apply(&mut s, op, mode, anchor, None, text, None, true).unwrap();
            (s, applied)
        };
        let src = "a = 1\r\nother = 2\r\n";
        let (out, _) = crlf(src, Op::InsertAfter, AnchorMode::LinePrefix, Some("other"), "added = 3\n");
        assert_eq!(out, "a = 1\r\nother = 2\r\nadded = 3\r\n");
        let (out, _) = crlf(src, Op::Replace, AnchorMode::Exact, Some("1\nother"), "10\nother");
        assert_eq!(out, "a = 10\r\nother = 2\r\n");
        let (out, _) = crlf("x\r\ny", Op::Append, AnchorMode::Exact, None, "z\n");
        assert_eq!(out, "x\r\ny\r\nz\r\n");
        let (out, _) = crlf("x\r\ny", Op::AppendJsonl, AnchorMode::Exact, None, "{}");
        assert_eq!(out, "x\r\ny\r\n{}\r\n");
        let (out, _) = crlf("x\r\nlast", Op::InsertAfter, AnchorMode::Exact, Some("last"), "n\n");
        assert_eq!(out, "x\r\nlast\r\nn\r\n");
        let mut s = src.to_string();
        let skipped =
            apply(&mut s, Op::Append, AnchorMode::Exact, None, None, "x", Some("1\nother"), true)
                .unwrap();
        assert_eq!(skipped, Applied::Skipped { line: 1 });
        // An LF file is written as given.
        let mut s = "a\n".to_string();
        apply(&mut s, Op::Append, AnchorMode::Exact, None, None, "b\r\nc\n", None, false).unwrap();
        assert_eq!(s, "a\nb\r\nc\n");
    }

    #[test]
    fn unless_present_skips_without_checking_the_anchor() {
        let mut s = "intro\n17. item\n".to_string();
        let applied = apply(
            &mut s,
            Op::InsertBefore,
            AnchorMode::Exact,
            Some("gone"),
            None,
            "x",
            Some("\n17. "),
            false,
        )
        .unwrap();
        assert_eq!(applied, Applied::Skipped { line: 2 });
        assert_eq!(s, "intro\n17. item\n");
    }
}
