//! Pure in-memory matching and application of one anchored edit.
//!
//! Everything here works on a `String` and never touches the filesystem, so
//! the all-or-nothing rule is easy to keep: the caller applies every edit of
//! a call to in-memory copies first and writes nothing if any edit fails.

use super::{AnchorMode, Op};

/// One anchor match, as byte offsets into the content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Match {
    /// Start of the matched span.
    pub start: usize,
    /// End of the matched span (exclusive).
    pub end: usize,
    /// Offset just past the matched line's terminator (line_prefix only;
    /// equals `end` for the other modes).
    pub line_next: usize,
    /// Whether the matched line ends with a terminator (line_prefix only).
    pub terminated: bool,
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
                out.push(Match {
                    start,
                    end,
                    line_next: end,
                    terminated: false,
                });
                let step = content[start..]
                    .chars()
                    .next()
                    .map(char::len_utf8)
                    .unwrap_or(1);
                i = start + step;
            }
        }
        AnchorMode::LinePrefix => {
            for (start, end, next, terminated) in lines(content) {
                if content[start..end].starts_with(anchor) {
                    out.push(Match {
                        start,
                        end,
                        line_next: next,
                        terminated,
                    });
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
                            line_next: start + m.end(),
                            terminated: false,
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

/// Apply one edit to `content` in place. `text` is already expanded; for
/// `append_jsonl` it is the compact row without its newline.
pub(crate) fn apply(
    content: &mut String,
    op: Op,
    mode: AnchorMode,
    anchor: Option<&str>,
    occurrence: Option<u64>,
    text: &str,
    unless_present: Option<&str>,
) -> Result<Applied, String> {
    if let Some(guard) = unless_present {
        if let Some(at) = content.find(guard) {
            // Report the line of the guard's first visible character, so
            // a guard written as "\n17. " points at the "17." line.
            let lead = guard.len() - guard.trim_start_matches(['\n', '\r']).len();
            return Ok(Applied::Skipped {
                line: line_of(content, at + lead),
            });
        }
    }
    let mut note = None;
    let (pos, removed, added) = match op {
        Op::Replace | Op::InsertBefore | Op::InsertAfter => {
            let anchor = anchor.ok_or("anchor is required")?;
            let matches = find_matches(content, anchor, mode)?;
            let m = select(content, &matches, occurrence)?;
            match op {
                Op::Replace => {
                    content.replace_range(m.start..m.end, text);
                    (m.start, m.end - m.start, text.len())
                }
                Op::InsertBefore => {
                    content.insert_str(m.start, text);
                    (m.start, 0, text.len())
                }
                _ => {
                    let at = if mode == AnchorMode::LinePrefix {
                        if !m.terminated {
                            content.insert(m.line_next, '\n');
                            note = Some("added newline after the last line");
                            m.line_next + 1
                        } else {
                            m.line_next
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
            let at = content.len();
            content.push_str(text);
            (at, 0, text.len())
        }
        Op::AppendJsonl => {
            if !content.is_empty() && !content.ends_with('\n') {
                content.push('\n');
                note = Some("added missing newline before the row");
            }
            let at = content.len();
            content.push_str(text);
            content.push('\n');
            (at, 0, text.len() + 1)
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
        let applied = apply(&mut s, op, mode, anchor, occurrence, text, None)?;
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
    fn append_is_verbatim_and_jsonl_adds_a_missing_newline() {
        let (out, _) = run("a", Op::Append, AnchorMode::Exact, None, None, "b").unwrap();
        assert_eq!(out, "ab");
        let (out, applied) =
            run("{\"a\":1}", Op::AppendJsonl, AnchorMode::Exact, None, None, "{\"b\":2}").unwrap();
        assert_eq!(out, "{\"a\":1}\n{\"b\":2}\n");
        assert!(matches!(applied, Applied::Edit { first_line: 2, last_line: 2, note: Some(_), .. }));
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
        )
        .unwrap();
        assert_eq!(applied, Applied::Skipped { line: 2 });
        assert_eq!(s, "intro\n17. item\n");
    }
}
