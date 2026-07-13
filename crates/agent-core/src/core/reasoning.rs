//! Typed named reasoning level — single source of truth across providers.
//!
//! `ReasoningLevel` is a closed enum with exact parsing, display, and explicit
//! conversion to legacy Anthropic token budgets. It lives in `synaps-core` so
//! every crate (engine, TUI) can depend on the same type without circular deps.
//!
//! ## Invariants
//! - `Max` must never alias `XHigh`.
//! - `Ultra` must never map through a numeric bucket.
//! - `Off` and `Adaptive` are distinct.
//! - Unknown strings from external sources parse to `None` / are rejected at
//!   the call-site, not silently coerced.

use std::fmt;

/// A named reasoning / thinking depth level.
///
/// Ordering is from least to most intensive; `Off` < `Adaptive` < `Low` < … < `Ultra`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ReasoningLevel {
    /// No reasoning / thinking output.
    Off,
    /// Model decides reasoning depth automatically (Anthropic adaptive path).
    Adaptive,
    /// Low reasoning budget.
    Low,
    /// Medium reasoning budget.
    Medium,
    /// High reasoning budget.
    High,
    /// Extra-high reasoning budget (above high, below max).
    XHigh,
    /// Maximum named level (below ultra).
    Max,
    /// Ultra — highest intensity, only a subset of Codex models supports this.
    Ultra,
}

impl ReasoningLevel {
    /// Parse a level from its canonical wire/config string.
    ///
    /// Returns `None` for unknown strings; callers must not silently fallback.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "none" => Some(Self::Off),
            "adaptive" => Some(Self::Adaptive),
            "low" => Some(Self::Low),
            "medium" | "med" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" | "x-high" | "x_high" => Some(Self::XHigh),
            "max" => Some(Self::Max),
            "ultra" => Some(Self::Ultra),
            _ => None,
        }
    }

    /// The canonical string representation, used on the wire and in config.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Adaptive => "adaptive",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
            Self::Ultra => "ultra",
        }
    }

    /// Convert to the legacy Anthropic token budget used by the enabled+budget_tokens
    /// request shape. Returns `None` for levels that have no meaningful numeric
    /// mapping (Max, Ultra — these are only valid on Codex and use named effort values).
    pub fn to_legacy_budget(self) -> Option<u32> {
        match self {
            Self::Off | Self::Adaptive => Some(0),
            Self::Low => Some(2048),
            Self::Medium => Some(4096),
            Self::High => Some(16384),
            Self::XHigh => Some(32768),
            Self::Max | Self::Ultra => None,
        }
    }

    /// Build from a legacy token budget value (Anthropic path).
    ///
    /// `0` → `Adaptive`, positive values → their tier. Does not produce `Max`
    /// or `Ultra` — those are provider-named levels, never inferred from a budget.
    pub fn from_legacy_budget(budget: u32) -> Self {
        match budget {
            0 => Self::Adaptive,
            1..=2048 => Self::Low,
            2049..=4096 => Self::Medium,
            4097..=16384 => Self::High,
            _ => Self::XHigh,
        }
    }

    /// All levels that are strictly above XHigh (require Codex-specific support).
    pub fn requires_codex_support(self) -> bool {
        matches!(self, Self::Max | Self::Ultra)
    }
}

impl fmt::Display for ReasoningLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Default for ReasoningLevel {
    fn default() -> Self {
        Self::Medium
    }
}

/// Parse the `thinking` config key into either a named level or a raw numeric budget.
///
/// This is the single parse path for config files, `/thinking` commands, and
/// session resumption. Named levels win over numeric-only parsing; numeric values
/// that don't match a known level are treated as raw budget tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThinkingSpec {
    /// A recognized named level.
    Named(ReasoningLevel),
    /// A raw numeric budget (legacy Anthropic path, user-specified token count).
    Budget(u32),
}

impl ThinkingSpec {
    /// Parse from a string (config value or command argument).
    ///
    /// Returns `None` for strings that are neither a known name nor a non-negative integer.
    pub fn parse(s: &str) -> Option<Self> {
        // Named levels take priority.
        if let Some(level) = ReasoningLevel::parse(s) {
            return Some(Self::Named(level));
        }
        // Raw numeric budget (any non-negative integer).
        if let Ok(n) = s.trim().parse::<u32>() {
            return Some(Self::Budget(n));
        }
        None
    }

    /// The canonical level this spec resolves to, if deterministic.
    /// `Budget` values are bucketized via `ReasoningLevel::from_legacy_budget`.
    pub fn to_level(&self) -> ReasoningLevel {
        match self {
            Self::Named(l) => *l,
            Self::Budget(n) => ReasoningLevel::from_legacy_budget(*n),
        }
    }

    /// The numeric budget this spec resolves to for Anthropic requests.
    /// Named Max/Ultra return `None` because they have no valid numeric budget.
    pub fn to_budget(&self) -> Option<u32> {
        match self {
            Self::Named(l) => l.to_legacy_budget(),
            Self::Budget(n) => Some(*n),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Parse / round-trip ────────────────────────────────────────────────────

    #[test]
    fn all_canonical_strings_parse() {
        for (s, expected) in [
            ("off", ReasoningLevel::Off),
            ("none", ReasoningLevel::Off),
            ("adaptive", ReasoningLevel::Adaptive),
            ("low", ReasoningLevel::Low),
            ("medium", ReasoningLevel::Medium),
            ("med", ReasoningLevel::Medium),
            ("high", ReasoningLevel::High),
            ("xhigh", ReasoningLevel::XHigh),
            ("max", ReasoningLevel::Max),
            ("ultra", ReasoningLevel::Ultra),
        ] {
            assert_eq!(
                ReasoningLevel::parse(s),
                Some(expected),
                "failed to parse {s:?}"
            );
        }
    }

    #[test]
    fn case_insensitive_parse() {
        assert_eq!(ReasoningLevel::parse("MAX"), Some(ReasoningLevel::Max));
        assert_eq!(ReasoningLevel::parse("Ultra"), Some(ReasoningLevel::Ultra));
        assert_eq!(ReasoningLevel::parse("XHIGH"), Some(ReasoningLevel::XHigh));
        assert_eq!(ReasoningLevel::parse("  Low  "), Some(ReasoningLevel::Low));
    }

    #[test]
    fn unknown_strings_return_none() {
        assert_eq!(ReasoningLevel::parse(""), None);
        assert_eq!(ReasoningLevel::parse("bogus"), None);
        assert_eq!(ReasoningLevel::parse("maximum"), None);
        assert_eq!(ReasoningLevel::parse("hyper"), None);
        assert_eq!(ReasoningLevel::parse("xhigh+"), None);
    }

    #[test]
    fn as_str_round_trips() {
        for level in [
            ReasoningLevel::Off,
            ReasoningLevel::Adaptive,
            ReasoningLevel::Low,
            ReasoningLevel::Medium,
            ReasoningLevel::High,
            ReasoningLevel::XHigh,
            ReasoningLevel::Max,
            ReasoningLevel::Ultra,
        ] {
            let s = level.as_str();
            assert_eq!(
                ReasoningLevel::parse(s),
                Some(level),
                "as_str() → parse() failed for {:?}",
                level
            );
        }
    }

    #[test]
    fn display_matches_as_str() {
        assert_eq!(ReasoningLevel::Max.to_string(), "max");
        assert_eq!(ReasoningLevel::Ultra.to_string(), "ultra");
        assert_eq!(ReasoningLevel::XHigh.to_string(), "xhigh");
    }

    // ── Max must never alias XHigh ─────────────────────────────────────────────

    #[test]
    fn max_is_distinct_from_xhigh() {
        assert_ne!(ReasoningLevel::Max, ReasoningLevel::XHigh);
        assert_ne!(ReasoningLevel::Max.as_str(), ReasoningLevel::XHigh.as_str());
    }

    // ── Ultra must not map through a numeric budget bucket ────────────────────

    #[test]
    fn ultra_has_no_legacy_budget() {
        assert_eq!(ReasoningLevel::Ultra.to_legacy_budget(), None);
    }

    #[test]
    fn max_has_no_legacy_budget() {
        assert_eq!(ReasoningLevel::Max.to_legacy_budget(), None);
    }

    // ── Legacy budget conversion ──────────────────────────────────────────────

    #[test]
    fn known_legacy_budgets_convert_correctly() {
        assert_eq!(ReasoningLevel::Off.to_legacy_budget(), Some(0));
        assert_eq!(ReasoningLevel::Adaptive.to_legacy_budget(), Some(0));
        assert_eq!(ReasoningLevel::Low.to_legacy_budget(), Some(2048));
        assert_eq!(ReasoningLevel::Medium.to_legacy_budget(), Some(4096));
        assert_eq!(ReasoningLevel::High.to_legacy_budget(), Some(16384));
        assert_eq!(ReasoningLevel::XHigh.to_legacy_budget(), Some(32768));
    }

    #[test]
    fn from_legacy_budget_never_returns_max_or_ultra() {
        for budget in [0u32, 2048, 4096, 16384, 32768, 65536, u32::MAX] {
            let level = ReasoningLevel::from_legacy_budget(budget);
            assert!(
                !matches!(level, ReasoningLevel::Max | ReasoningLevel::Ultra),
                "from_legacy_budget({budget}) returned {level:?} which must not be Max/Ultra"
            );
        }
    }

    #[test]
    fn from_legacy_budget_round_trips_for_known_values() {
        for (budget, expected) in [
            (0u32, ReasoningLevel::Adaptive),
            (2048, ReasoningLevel::Low),
            (4096, ReasoningLevel::Medium),
            (16384, ReasoningLevel::High),
            (32768, ReasoningLevel::XHigh),
        ] {
            assert_eq!(
                ReasoningLevel::from_legacy_budget(budget),
                expected,
                "budget {budget}"
            );
        }
    }

    // ── Ordering ──────────────────────────────────────────────────────────────

    #[test]
    fn ordering_is_intensity() {
        let levels = [
            ReasoningLevel::Off,
            ReasoningLevel::Adaptive,
            ReasoningLevel::Low,
            ReasoningLevel::Medium,
            ReasoningLevel::High,
            ReasoningLevel::XHigh,
            ReasoningLevel::Max,
            ReasoningLevel::Ultra,
        ];
        for w in levels.windows(2) {
            assert!(w[0] < w[1], "{:?} should be less than {:?}", w[0], w[1]);
        }
    }

    // ── requires_codex_support ────────────────────────────────────────────────

    #[test]
    fn only_max_and_ultra_require_codex_support() {
        assert!(ReasoningLevel::Max.requires_codex_support());
        assert!(ReasoningLevel::Ultra.requires_codex_support());
        assert!(!ReasoningLevel::XHigh.requires_codex_support());
        assert!(!ReasoningLevel::High.requires_codex_support());
        assert!(!ReasoningLevel::Adaptive.requires_codex_support());
    }

    // ── ThinkingSpec ──────────────────────────────────────────────────────────

    #[test]
    fn thinking_spec_named_levels_parse() {
        assert_eq!(
            ThinkingSpec::parse("max"),
            Some(ThinkingSpec::Named(ReasoningLevel::Max))
        );
        assert_eq!(
            ThinkingSpec::parse("ultra"),
            Some(ThinkingSpec::Named(ReasoningLevel::Ultra))
        );
        assert_eq!(
            ThinkingSpec::parse("xhigh"),
            Some(ThinkingSpec::Named(ReasoningLevel::XHigh))
        );
    }

    #[test]
    fn thinking_spec_numeric_budget_parse() {
        assert_eq!(ThinkingSpec::parse("8192"), Some(ThinkingSpec::Budget(8192)));
        assert_eq!(ThinkingSpec::parse("0"), Some(ThinkingSpec::Budget(0)));
    }

    #[test]
    fn thinking_spec_parse_invalid() {
        assert_eq!(ThinkingSpec::parse("bogus"), None);
        assert_eq!(ThinkingSpec::parse(""), None);
    }

    #[test]
    fn thinking_spec_max_has_no_budget() {
        let spec = ThinkingSpec::Named(ReasoningLevel::Max);
        assert_eq!(spec.to_budget(), None);
        assert_eq!(spec.to_level(), ReasoningLevel::Max);
    }

    #[test]
    fn thinking_spec_budget_round_trips() {
        let spec = ThinkingSpec::Budget(8192);
        assert_eq!(spec.to_budget(), Some(8192));
        // 8192 is in the high tier (4097–16384)
        assert_eq!(spec.to_level(), ReasoningLevel::High);
    }
}
