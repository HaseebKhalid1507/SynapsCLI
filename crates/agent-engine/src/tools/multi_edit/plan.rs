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

pub(crate) fn lines(_s: &str) -> Vec<(usize, usize, usize, bool)> {
    Vec::new()
}

pub(crate) fn line_of(_s: &str, _offset: usize) -> usize {
    0
}

pub(crate) fn find_matches(
    _content: &str,
    _anchor: &str,
    _mode: AnchorMode,
) -> Result<Vec<Match>, String> {
    Ok(Vec::new())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn apply(
    _content: &mut String,
    _op: Op,
    _mode: AnchorMode,
    _anchor: Option<&str>,
    _occurrence: Option<u64>,
    _text: &str,
    _unless_present: Option<&str>,
) -> Result<Applied, String> {
    Err("not implemented".into())
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
