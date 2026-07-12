//! Centralised pricing logic for Anthropic models.
//!
//! All cost calculations live here so that the engine, TUI, and any future
//! consumer share a single source of truth. Update this file — and only this
//! file — whenever Anthropic changes its pricing.
//!
//! Prices are in USD per million tokens (as of 2026-06).
//! Source: <https://www.anthropic.com/pricing>
//!
//! | Model   | Input  | Output |
//! |---------|--------|--------|
//! | Fable   | $10.00 | $50.00 |
//! | Opus    | $5.00  | $25.00 |
//! | Sonnet  | $3.00  | $15.00 |
//! | Haiku   | $1.00  | $5.00  |
//!
//! Cache pricing (relative to input price):
//! - Cache reads:       0.10× input price  (prompt-cache hit)
//! - Cache write (5m):  1.25× input price  (5-minute TTL, the default)
//! - Cache write (1h):  2.00× input price  (1-hour TTL, opt-in via `cache_ttl`)

/// Returns `(input_price_per_mtok, output_price_per_mtok)` for the given model
/// string. Matching is substring-based so it works with full model IDs like
/// `claude-opus-4-5-20251101` as well as short names like `claude-opus`.
///
/// Falls back to Sonnet pricing for unknown models.
#[inline]
fn model_prices(model: &str) -> (f64, f64) {
    match model {
        m if m.contains("fable") => (10.0, 50.0),
        m if m.contains("opus") => (5.0, 25.0),
        m if m.contains("sonnet") => (3.0, 15.0),
        m if m.contains("haiku") => (1.0, 5.0),
        _ => (3.0, 15.0), // default: Sonnet pricing
    }
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
/// reads 0.10× | 5m write 1.25× | 1h write 2.0×.
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
        + (cache_read as f64 / 1_000_000.0) * input_price * 0.1
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
    fn unknown_model_falls_back_to_sonnet() {
        let cost_unknown = calculate_cost("gpt-99-turbo", 1_000_000, 0, 0, 0);
        let cost_sonnet = calculate_cost("claude-sonnet-4-5", 1_000_000, 0, 0, 0);
        assert!((cost_unknown - cost_sonnet).abs() < 1e-9);
    }

    #[test]
    fn zero_usage_is_zero_cost() {
        assert_eq!(calculate_cost("claude-opus-4-5", 0, 0, 0, 0), 0.0);
    }
}
