//! Shared UTF-8-safe, byte-budgeted text truncation (T2).
//!
//! [`BoundedText`] is the one workspace-wide utility for producing
//! byte-bounded, valid-UTF-8 previews of arbitrary (user- or model-derived)
//! strings. Production code must never byte-index-slice such strings
//! directly; route through this type or [`crate::truncate_str`] (the
//! borrowing primitive this type is built on).

/// A byte-bounded, char-boundary-safe view of a string, with exact
/// accounting of what was kept and what was cut.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedText {
    /// The retained prefix — always valid UTF-8, at most the byte budget.
    pub text: String,
    /// Byte length of the original input.
    pub original_bytes: usize,
    /// Byte length of `text` (≤ budget, ≤ `original_bytes`).
    pub retained_bytes: usize,
    /// Whether anything was cut (`retained_bytes < original_bytes`).
    pub truncated: bool,
}

impl BoundedText {
    /// Truncate `s` to at most `max_bytes` bytes at a valid UTF-8 boundary.
    /// Never panics; greedy (keeps the longest prefix that fits).
    pub fn new(s: &str, max_bytes: usize) -> Self {
        let text = crate::truncate_str(s, max_bytes);
        Self {
            original_bytes: s.len(),
            retained_bytes: text.len(),
            truncated: text.len() < s.len(),
            text: text.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_text_reports_exact_accounting() {
        let bt = BoundedText::new("hello", 10);
        assert_eq!(bt.text, "hello");
        assert_eq!(bt.original_bytes, 5);
        assert_eq!(bt.retained_bytes, 5);
        assert!(!bt.truncated);

        let bt = BoundedText::new("hello", 3);
        assert_eq!(bt.text, "hel");
        assert_eq!(bt.original_bytes, 5);
        assert_eq!(bt.retained_bytes, 3);
        assert!(bt.truncated);
    }

    /// Property-style sweep: for a deterministic corpus of adversarial
    /// Unicode inputs (multibyte Latin, emoji + ZWJ, CJK, combining marks,
    /// LCG-generated mixes) and EVERY byte budget from 0 to len+2, the
    /// constructor never panics, never exceeds the budget, always yields a
    /// valid-UTF-8 prefix, accounts bytes exactly, and is greedy.
    #[test]
    fn bounded_text_property_sweep_never_exceeds_budget() {
        let pool: Vec<char> = "aé汉字テ🌟👨\u{200D}👩\u{301}\u{300}あ€𝄞\u{FFFD}"
            .chars()
            .collect();
        let mut corpus: Vec<String> = vec![
            String::new(),
            "plain ascii".into(),
            "héllo wörld".into(),
            "🌟👨\u{200D}👩\u{200D}👧\u{200D}👦🌟".into(),
            "汉字漢字テスト한글".into(),
            "e\u{301}\u{300}o\u{308}".into(),
            "a🌟b汉c\u{301}d".into(),
        ];
        // Deterministic LCG generator — no new dependencies.
        let mut state: u64 = 0x2545_F491_4F6C_DD1D;
        for len in [1usize, 3, 7, 17, 33] {
            let mut s = String::new();
            for _ in 0..len {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                s.push(pool[(state >> 33) as usize % pool.len()]);
            }
            corpus.push(s);
        }
        for s in &corpus {
            for budget in 0..=s.len() + 2 {
                let bt = BoundedText::new(s, budget);
                assert!(
                    bt.retained_bytes <= budget,
                    "budget exceeded: {s:?} @ {budget}"
                );
                assert_eq!(bt.retained_bytes, bt.text.len());
                assert_eq!(bt.original_bytes, s.len());
                assert!(s.starts_with(&bt.text), "not a prefix: {s:?} @ {budget}");
                assert_eq!(bt.truncated, bt.retained_bytes < s.len());
                // Greedy: the next char (if any) must not have fit.
                if bt.truncated {
                    let next_len = s[bt.retained_bytes..]
                        .chars()
                        .next()
                        .map(char::len_utf8)
                        .unwrap_or(0);
                    assert!(
                        bt.retained_bytes + next_len > budget,
                        "not greedy: {s:?} @ {budget}"
                    );
                }
            }
        }
    }
}
