//! Centralised pricing logic for Anthropic models.
//!
//! All cost calculations live here so that the engine, TUI, and any future
//! consumer share a single source of truth. Update this file — and only this
//! file — whenever Anthropic changes its pricing.
//!
//! Prices are in USD per million tokens (as of 2026-04).
//! Source: <https://www.anthropic.com/pricing>
//!
//! | Model   | Input  | Output |
//! |---------|--------|--------|
//! | Opus    | $5.00  | $25.00 |
//! | Sonnet  | $3.00  | $15.00 |
//! | Haiku   | $1.00  | $5.00  |
//!
//! Cache pricing (relative to input price):
//! - Cache reads:    0.10× input price  (prompt-cache hit)
//! - Cache creation: 1.25× input price  (5-minute TTL write)

/// Returns `(input_price_per_mtok, output_price_per_mtok)` for the given model
/// string. Matching is substring-based so it works with full model IDs like
/// `claude-opus-4-5-20251101` as well as short names like `claude-opus`.
///
/// Falls back to Sonnet pricing for unknown models.
#[inline]
fn model_prices(model: &str) -> (f64, f64) {
    match model {
        m if m.contains("opus")   => (5.0, 25.0),
        m if m.contains("sonnet") => (3.0, 15.0),
        m if m.contains("haiku")  => (1.0,  5.0),
        _                         => (3.0, 15.0), // default: Sonnet pricing
    }
}

/// Calculate the USD cost of a single model turn.
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
    let (input_price, output_price) = model_prices(model);
    (input_tokens    as f64 / 1_000_000.0) * input_price
        + (cache_read     as f64 / 1_000_000.0) * input_price * 0.1
        + (cache_creation as f64 / 1_000_000.0) * input_price * 1.25
        + (output_tokens  as f64 / 1_000_000.0) * output_price
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn unknown_model_falls_back_to_sonnet() {
        let cost_unknown = calculate_cost("gpt-99-turbo", 1_000_000, 0, 0, 0);
        let cost_sonnet  = calculate_cost("claude-sonnet-4-5", 1_000_000, 0, 0, 0);
        assert!((cost_unknown - cost_sonnet).abs() < 1e-9);
    }

    #[test]
    fn zero_usage_is_zero_cost() {
        assert_eq!(calculate_cost("claude-opus-4-5", 0, 0, 0, 0), 0.0);
    }
}
