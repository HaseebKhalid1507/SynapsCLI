//! Centralised pricing logic for Anthropic models.
//!
//! All cost calculations live here so that the engine, TUI, and any future
//! consumer share a single source of truth. Update this file — and only this
//! file — whenever Anthropic changes its pricing.
//!
//! Prices are in USD per million tokens.
//! Source: <https://platform.claude.com/docs/en/about-claude/pricing>
//! ("Model pricing" and "Feature-specific pricing → Prompt caching").
//!
//! | Model                         | Input  | Output | Cache read |
//! |-------------------------------|--------|--------|------------|
//! | Fable 5.1 / Mythos 5.1        | $10.00 | $50.00 | 0.025×     |
//! | Fable 5 / Mythos 5            | $10.00 | $50.00 | 0.10×      |
//! | Opus 5.5                      | $4.00  | $20.00 | 0.05×      |
//! | Opus 4.5 – 5                  | $5.00  | $25.00 | 0.10×      |
//! | Opus 4 / 4.1 (and Opus 3)     | $15.00 | $75.00 | 0.10×      |
//! | Sonnet 5.5                    | $2.00  | $10.00 | 0.05×      |
//! | Sonnet (other)                | $3.00  | $15.00 | 0.10×      |
//! | Haiku 5.5 (>100k-token tier)  | $0.50  | $2.50  | 0.10×      |
//! | Haiku 3.5                     | $0.80  | $4.00  | 0.10×      |
//! | Haiku (other)                 | $1.00  | $5.00  | 0.10×      |
//!
//! Haiku 5.5 is tiered by prompt length; the higher (>100k) tier is used —
//! the conservative choice for a cost display and for spend caps. Models the
//! table does not list explicitly (e.g. Sonnet 5, whose list price the page
//! does not state in full) fall back to the family default above.
//!
//! Cache pricing (relative to input price):
//! - Cache reads:       0.10× input price, except where the docs list a
//!   per-model multiplier (see [`cache_read_multiplier`])
//! - Cache write (5m):  1.25× input price  (5-minute TTL, the default)
//! - Cache write (1h):  2.00× input price  (1-hour TTL, opt-in via `cache_ttl`)

/// `true` for Claude Opus 4 / 4.1 (and the retired Opus 3), which the docs
/// list at $15 / $75 — unlike Opus 4.5 and later ($5 / $25 or less).
/// IDs: `claude-opus-4-0`, `claude-opus-4-20250514`, `claude-opus-4-1`,
/// `claude-opus-4-1-20250805`, `claude-opus-4`, `claude-3-opus…`.
fn is_legacy_opus(m: &str) -> bool {
    if m.contains("3-opus") || m.contains("opus-3") {
        return true;
    }
    let Some(i) = m.find("opus-4") else {
        return false;
    };
    let rest = &m[i + "opus-4".len()..];
    rest.is_empty()
        || rest.starts_with("-0")
        || rest == "-1"
        || rest.starts_with("-1-")
        || rest.starts_with("-2")
        || rest.starts_with('@')
}

/// Returns `(input_price_per_mtok, output_price_per_mtok)` for the given model
/// string. Matching is substring-based so it works with full model IDs like
/// `claude-opus-4-5-20251101` as well as short names like `claude-opus`.
///
/// Falls back to Sonnet pricing for unknown models.
#[inline]
fn model_prices(model: &str) -> (f64, f64) {
    match model {
        // InferX publishes DeepSeek V4 Flash at $0/M input and output.
        // Keep this exact ID before generic DeepSeek entries are ever added.
        "inferx/deepseek-v4-flash" | "deepseek-v4-flash" => (0.0, 0.0),
        m if m.contains("fable") || m.contains("mythos") => (10.0, 50.0),
        m if m.contains("opus-5-5") => (4.0, 20.0),
        m if is_legacy_opus(m) => (15.0, 75.0),
        m if m.contains("opus") => (5.0, 25.0),
        m if m.contains("sonnet-5-5") => (2.0, 10.0),
        m if m.contains("sonnet") => (3.0, 15.0),
        m if m.contains("haiku-5-5") => (0.5, 2.5),
        m if m.contains("haiku-3-5") || m.contains("3-5-haiku") => (0.8, 4.0),
        m if m.contains("haiku") => (1.0, 5.0),
        _ => (3.0, 15.0), // default: Sonnet pricing
    }
}

/// Cache-read price as a multiple of the input price. The docs list 0.025×
/// for Fable 5.1 / Mythos 5.1 and 0.05× for Opus 5.5 / Sonnet 5.5; every
/// other model uses the standard 0.1× (also the conservative default for
/// models this table does not know).
pub fn cache_read_multiplier(model: &str) -> f64 {
    match model {
        m if m.contains("fable-5-1") || m.contains("mythos-5-1") => 0.025,
        m if m.contains("opus-5-5") || m.contains("sonnet-5-5") => 0.05,
        _ => 0.1,
    }
}

/// `(input, output)` USD per million tokens for `model` (see the table).
pub fn prices_per_mtok(model: &str) -> (f64, f64) {
    model_prices(model)
}

/// Calculate the USD cost of a single model turn (no cache-write TTL split).
///
/// Thin wrapper over [`calculate_cost_split`]: the aggregate `cache_creation`
/// count is billed at the 5m write rate (1.25×). When the user opted into 1h
/// caching but the TTL split didn't arrive, this under-bills (fail-cheap, not
/// fail-expensive — cost display is informational, not invoiced).
///
/// # Arguments
/// * `model`           – Model identifier string (e.g. `"claude-sonnet-4-5"`).
/// * `input_tokens`    – Uncached input tokens billed at full input rate.
/// * `output_tokens`   – Output / generated tokens (includes adaptive thinking).
/// * `cache_read`      – Tokens served from the prompt cache (0.10× input rate).
/// * `cache_creation`  – Tokens written to the prompt cache (1.25× input rate).
///
/// # Returns
/// Cost in USD for this turn.
pub fn calculate_cost(
    model: &str,
    input_tokens: u64,
    output_tokens: u64,
    cache_read: u64,
    cache_creation: u64,
) -> f64 {
    calculate_cost_split(
        model,
        input_tokens,
        output_tokens,
        cache_read,
        cache_creation,
        0,
    )
}

/// Calculate the USD cost of a single model turn with the cache-write TTL
/// split made first-class.
///
/// Cache pricing relative to input price:
/// reads [`cache_read_multiplier`] (0.10× by default) | 5m write 1.25× |
/// 1h write 2.0×.
pub fn calculate_cost_split(
    model: &str,
    input_tokens: u64,
    output_tokens: u64,
    cache_read: u64,
    cache_write_5m: u64,
    cache_write_1h: u64,
) -> f64 {
    let (input_price, output_price) = model_prices(model);
    (input_tokens as f64 / 1_000_000.0) * input_price
        + (cache_read as f64 / 1_000_000.0) * input_price * cache_read_multiplier(model)
        + (cache_write_5m as f64 / 1_000_000.0) * input_price * 1.25
        + (cache_write_1h as f64 / 1_000_000.0) * input_price * 2.0
        + (output_tokens as f64 / 1_000_000.0) * output_price
}

/// Split-aware cost for callers holding an aggregate plus an *optional* TTL
/// split (the shape of `SessionEvent::Usage`). When either split bucket is
/// present the split rates apply; when both are `None`, the aggregate is
/// billed at the 5m rate — fail-cheap, never fail-expensive.
#[allow(clippy::too_many_arguments)]
pub fn calculate_cost_optional_split(
    model: &str,
    input_tokens: u64,
    output_tokens: u64,
    cache_read: u64,
    cache_creation: u64,
    cache_creation_5m: Option<u64>,
    cache_creation_1h: Option<u64>,
) -> f64 {
    match (cache_creation_5m, cache_creation_1h) {
        (None, None) => calculate_cost(
            model,
            input_tokens,
            output_tokens,
            cache_read,
            cache_creation,
        ),
        (c5, c1) => calculate_cost_split(
            model,
            input_tokens,
            output_tokens,
            cache_read,
            c5.unwrap_or(0),
            c1.unwrap_or(0),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fable_pricing() {
        // 1M input + 1M output, no cache → $10 + $50 = $60
        let cost = calculate_cost("claude-fable-5", 1_000_000, 1_000_000, 0, 0);
        assert!((cost - 60.0).abs() < 1e-9, "expected $60, got ${cost}");
    }

    #[test]
    fn opus_pricing() {
        // 1M input + 1M output, no cache → $5 + $25 = $30
        let cost = calculate_cost("claude-opus-4-5", 1_000_000, 1_000_000, 0, 0);
        assert!((cost - 30.0).abs() < 1e-9, "expected $30, got ${cost}");
    }

    #[test]
    fn sonnet_pricing() {
        // 1M input + 1M output → $3 + $15 = $18
        let cost = calculate_cost("claude-sonnet-4-5", 1_000_000, 1_000_000, 0, 0);
        assert!((cost - 18.0).abs() < 1e-9, "expected $18, got ${cost}");
    }

    #[test]
    fn haiku_pricing() {
        // 1M input + 1M output → $1 + $5 = $6
        let cost = calculate_cost("claude-haiku-4-5", 1_000_000, 1_000_000, 0, 0);
        assert!((cost - 6.0).abs() < 1e-9, "expected $6, got ${cost}");
    }

    #[test]
    fn cache_read_bills_at_tenth_input_rate() {
        // 1M cache-read tokens for Sonnet: 0.1 × $3 = $0.30
        let cost = calculate_cost("claude-sonnet-4-5", 0, 0, 1_000_000, 0);
        assert!((cost - 0.30).abs() < 1e-9, "expected $0.30, got ${cost}");
    }

    #[test]
    fn cache_creation_bills_at_125_percent_input_rate() {
        // 1M cache-write tokens for Sonnet: 1.25 × $3 = $3.75
        let cost = calculate_cost("claude-sonnet-4-5", 0, 0, 0, 1_000_000);
        assert!((cost - 3.75).abs() < 1e-9, "expected $3.75, got ${cost}");
    }

    #[test]
    fn cache_write_1h_bills_at_double_input_rate() {
        // 1M 1h cache-write tokens for Sonnet: 2.0 × $3 = $6.00 (spec §5)
        let cost = calculate_cost_split("claude-sonnet-4-5", 0, 0, 0, 0, 1_000_000);
        assert!((cost - 6.0).abs() < 1e-9, "expected $6.00, got ${cost}");
    }

    #[test]
    fn split_mixes_5m_and_1h_rates() {
        // Sonnet: 1M @ 1.25× ($3.75) + 1M @ 2.0× ($6.00) = $9.75
        let cost = calculate_cost_split("claude-sonnet-4-5", 0, 0, 0, 1_000_000, 1_000_000);
        assert!((cost - 9.75).abs() < 1e-9, "expected $9.75, got ${cost}");
    }

    #[test]
    fn wrapper_equals_split_with_zero_1h() {
        // calculate_cost(m,i,o,r,w) == calculate_cost_split(m,i,o,r,w,0)
        let cases: &[(&str, u64, u64, u64, u64)] = &[
            ("claude-sonnet-4-5", 1000, 2000, 3000, 4000),
            ("claude-opus-4-5", 0, 0, 0, 1_000_000),
            ("claude-haiku-4-5", 123, 456, 789, 1011),
            ("gpt-99-turbo", 50, 60, 70, 80),
            ("claude-fable-5", 0, 0, 0, 0),
        ];
        for &(m, i, o, r, w) in cases {
            let a = calculate_cost(m, i, o, r, w);
            let b = calculate_cost_split(m, i, o, r, w, 0);
            assert!((a - b).abs() < 1e-12, "{m}: {a} != {b}");
        }
    }

    #[test]
    fn inferx_deepseek_v4_flash_is_free() {
        let cost = calculate_cost("inferx/deepseek-v4-flash", 1_000_000, 1_000_000, 0, 0);
        assert_eq!(cost, 0.0);
    }

    #[test]
    fn unknown_model_falls_back_to_sonnet() {
        let cost_unknown = calculate_cost("gpt-99-turbo", 1_000_000, 0, 0, 0);
        let cost_sonnet = calculate_cost("claude-sonnet-4-5", 1_000_000, 0, 0, 0);
        assert!((cost_unknown - cost_sonnet).abs() < 1e-9);
    }

    #[test]
    fn zero_usage_is_zero_cost() {
        assert_eq!(calculate_cost("claude-opus-4-5", 0, 0, 0, 0), 0.0);
    }

    #[test]
    fn opus_4_and_4_1_are_legacy_priced() {
        for m in [
            "claude-opus-4-0",
            "claude-opus-4-20250514",
            "claude-opus-4-1",
            "claude-opus-4-1-20250805",
            "claude-opus-4",
            "claude-3-opus-20240229",
        ] {
            let cost = calculate_cost(m, 1_000_000, 1_000_000, 0, 0);
            assert!((cost - 90.0).abs() < 1e-9, "{m}: ${cost}");
        }
        for m in [
            "claude-opus-4-5",
            "claude-opus-4-5-20251101",
            "claude-opus-4-6",
            "claude-opus-4-7",
            "claude-opus-4-8",
            "claude-opus-5",
        ] {
            let cost = calculate_cost(m, 1_000_000, 1_000_000, 0, 0);
            assert!((cost - 30.0).abs() < 1e-9, "{m}: ${cost}");
        }
    }

    #[test]
    fn documented_cache_read_multipliers() {
        // Fable 5.1: $10 × 0.025 = $0.25/MTok; Opus 5.5: $4 × 0.05 = $0.20;
        // Sonnet 5.5: $2 × 0.05 = $0.10; Opus 4.7: $5 × 0.1 = $0.50.
        for (m, want) in [
            ("claude-fable-5-1", 0.25),
            ("claude-mythos-5-1", 0.25),
            ("claude-opus-5-5", 0.20),
            ("claude-sonnet-5-5", 0.10),
            ("claude-opus-4-7", 0.50),
            ("claude-fable-5", 1.0),
            ("gpt-99-turbo", 0.30),
        ] {
            let cost = calculate_cost(m, 0, 0, 1_000_000, 0);
            assert!((cost - want).abs() < 1e-9, "{m}: ${cost} != ${want}");
        }
    }
}
