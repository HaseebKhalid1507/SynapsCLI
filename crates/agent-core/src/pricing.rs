//! Centralised pricing logic for Anthropic models.
//!
//! All cost calculations live here so that the engine, TUI, and any future
//! consumer share a single source of truth. Update this file — and only this
//! file — whenever Anthropic changes its pricing.
//!
//! Prices are in USD per million tokens.
//! Source: <https://platform.claude.com/docs/en/about-claude/pricing>
//! ("Model pricing", "Batch processing", "Long context pricing" and
//! "Feature-specific pricing → Prompt caching").
//!
//! | Model                         | Input  | Output | Cache read |
//! |-------------------------------|--------|--------|------------|
//! | Fable 5.1 / Mythos 5.1        | $10.00 | $50.00 | 0.025×     |
//! | Fable 5 / Mythos 5            | $10.00 | $50.00 | 0.10×      |
//! | Opus 5.5                      | $4.00  | $20.00 | 0.05×      |
//! | Opus 4.5 – 5                  | $5.00  | $25.00 | 0.10×      |
//! | Opus 4 / 4.1 (and Opus 3)     | $15.00 | $75.00 | 0.10×      |
//! | Sonnet 5.5                    | $2.00  | $10.00 | 0.05×      |
//! | Sonnet 5                      | $2.00  | $10.00 | 0.10×      |
//! | Sonnet (other)                | $3.00  | $15.00 | 0.10×      |
//! | Haiku 5.5, prompt ≤ 100k      | $0.10  | $0.50  | 0.10×      |
//! | Haiku 5.5, prompt > 100k      | $0.50  | $2.50  | 0.10×      |
//! | Haiku 3.5                     | $0.80  | $4.00  | 0.10×      |
//! | Haiku (other)                 | $1.00  | $5.00  | 0.10×      |
//!
//! **Sonnet 5.** The page leaves the Sonnet 5 base input/output cells blank,
//! but every other cell of that row pins it at $2 / $10: the prompt-caching
//! columns read $2.50 (5m write = 1.25 × $2), $4 (1h write = 2 × $2) and
//! $0.20 (read = 0.1 × $2), and the Batch row reads $1 / $5 (the documented
//! 50% batch discount on $2 / $10). $3 / $15 fits none of them.
//!
//! **Haiku 5.5** is priced by prompt length ("Long context pricing"): a prompt
//! of over 100,000 tokens pays the higher row. The prompt is everything the
//! request sent — uncached input + cache reads + cache writes — so
//! [`calculate_cost_split`] picks the tier per request from those counts.
//!
//! Cache pricing (relative to input price):
//! - Cache reads:       0.10× input price, except where the docs list a
//!   per-model multiplier (see [`cache_read_multiplier`])
//! - Cache write (5m):  1.25× input price  (5-minute TTL, the default)
//! - Cache write (1h):  2.00× input price  (1-hour TTL, opt-in via `cache_ttl`)

/// Haiku 5.5 bills a prompt of more than this many tokens at the higher tier
/// (<https://platform.claude.com/docs/en/about-claude/pricing>, "Long context
/// pricing").
const HAIKU_5_5_TIER_TOKENS: u64 = 100_000;

/// `true` for Claude Opus 4 / 4.1 (and the retired Opus 3), which the docs
/// list at $15 / $75 — unlike Opus 4.5 and later ($5 / $25 or less).
/// IDs: `claude-opus-4-0`, `claude-opus-4-20250514`, `claude-opus-4-1`,
/// `claude-opus-4-1-20250805`, `claude-opus-4`, Vertex's
/// `claude-opus-4-1@20250805` / `claude-opus-4@20250514`, Bedrock's
/// `anthropic.claude-opus-4-1-20250805-v1:0`, `claude-3-opus…`.
///
/// After `opus-4` comes either nothing / a non-`-` separator (`@`, `:`), or
/// `-` and a digit run: the run `0` or `1` is the 4.0 / 4.1 minor version, an
/// 8-digit run is the 4.0 snapshot date; any other run (`5`, `6`, …) is a
/// later, cheaper Opus.
fn is_legacy_opus(m: &str) -> bool {
    if m.contains("3-opus") || m.contains("opus-3") {
        return true;
    }
    let Some(i) = m.find("opus-4") else {
        return false;
    };
    let rest = &m[i + "opus-4".len()..];
    let Some(after_dash) = rest.strip_prefix('-') else {
        // "", "@20250514", ":0" — plain Opus 4.
        return !rest.starts_with(|c: char| c.is_ascii_digit());
    };
    let run_len = after_dash
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(after_dash.len());
    let run = &after_dash[..run_len];
    run == "0" || run == "1" || run.len() == 8
}

/// `true` for Claude Sonnet 5 itself (`claude-sonnet-5`,
/// `claude-sonnet-5-20260101`, `claude-sonnet-5@…`), not for a later
/// `sonnet-5-N` minor version the table does not list.
fn is_sonnet_5(m: &str) -> bool {
    let Some(i) = m.find("sonnet-5") else {
        return false;
    };
    let rest = &m[i + "sonnet-5".len()..];
    let Some(after_dash) = rest.strip_prefix('-') else {
        return !rest.starts_with(|c: char| c.is_ascii_digit());
    };
    let run_len = after_dash
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(after_dash.len());
    run_len == 8 || run_len == 0
}

/// Returns `(input_price_per_mtok, output_price_per_mtok)` for the given model
/// string and the request's prompt size (uncached input + cache read + cache
/// write tokens; only Haiku 5.5 is tiered on it). Matching is substring-based
/// so it works with full model IDs like `claude-opus-4-5-20251101` as well as
/// short names like `claude-opus`.
///
/// Falls back to Sonnet pricing for unknown models.
#[inline]
fn model_prices(model: &str, prompt_tokens: u64) -> (f64, f64) {
    match model {
        // InferX publishes DeepSeek V4 Flash at $0/M input and output.
        // Keep this exact ID before generic DeepSeek entries are ever added.
        "inferx/deepseek-v4-flash" | "deepseek-v4-flash" => (0.0, 0.0),
        m if m.contains("fable") || m.contains("mythos") => (10.0, 50.0),
        m if m.contains("opus-5-5") => (4.0, 20.0),
        m if is_legacy_opus(m) => (15.0, 75.0),
        m if m.contains("opus") => (5.0, 25.0),
        m if m.contains("sonnet-5-5") => (2.0, 10.0),
        // Sonnet 5: derived from the cache and batch cells (module docs).
        m if is_sonnet_5(m) => (2.0, 10.0),
        m if m.contains("sonnet") => (3.0, 15.0),
        m if m.contains("haiku-5-5") => {
            if prompt_tokens > HAIKU_5_5_TIER_TOKENS {
                (0.5, 2.5)
            } else {
                (0.1, 0.5)
            }
        }
        m if m.contains("haiku-3-5") || m.contains("3-5-haiku") => (0.8, 4.0),
        m if m.contains("haiku") => (1.0, 5.0),
        _ => (3.0, 15.0), // default: Sonnet pricing
    }
}

/// Cache-read price as a multiple of the input price
/// (<https://platform.claude.com/docs/en/about-claude/pricing>, "Prompt
/// caching"). The docs list 0.025×
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
/// 1h write 2.0×. Haiku 5.5's tier is chosen from this request's prompt
/// (`input_tokens + cache_read + cache_write_5m + cache_write_1h`).
pub fn calculate_cost_split(
    model: &str,
    input_tokens: u64,
    output_tokens: u64,
    cache_read: u64,
    cache_write_5m: u64,
    cache_write_1h: u64,
) -> f64 {
    let prompt = input_tokens
        .saturating_add(cache_read)
        .saturating_add(cache_write_5m)
        .saturating_add(cache_write_1h);
    let (input_price, output_price) = model_prices(model, prompt);
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
            "claude-opus-4-1@20250805",
            "claude-opus-4@20250514",
            "claude-opus-4-0@20250514",
            "anthropic.claude-opus-4-1-20250805-v1:0",
            "anthropic.claude-opus-4-20250514-v1:0",
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
            "claude-opus-4-5@20251101",
            "claude-opus-4-6@20260101",
            "anthropic.claude-opus-4-5-20251101-v1:0",
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

    #[test]
    fn sonnet_5_is_two_and_ten() {
        // $2 + $10 = $12 for 1M in + 1M out; reads 0.1 × $2 = $0.20/MTok,
        // 5m writes $2.50, 1h writes $4 — the page's Sonnet 5 cache cells.
        for m in [
            "claude-sonnet-5",
            "claude-sonnet-5@20260101",
            "claude-sonnet-5-20260101",
            "anthropic/claude-sonnet-5",
        ] {
            let base = calculate_cost(m, 1_000_000, 1_000_000, 0, 0);
            assert!((base - 12.0).abs() < 1e-9, "{m}: ${base}");
            let read = calculate_cost(m, 0, 0, 1_000_000, 0);
            assert!((read - 0.20).abs() < 1e-9, "{m}: read ${read}");
            let w5 = calculate_cost_split(m, 0, 0, 0, 1_000_000, 0);
            assert!((w5 - 2.50).abs() < 1e-9, "{m}: 5m ${w5}");
            let w1 = calculate_cost_split(m, 0, 0, 0, 0, 1_000_000);
            assert!((w1 - 4.0).abs() < 1e-9, "{m}: 1h ${w1}");
        }
        // Sonnet 5.5 and 4.x are unaffected.
        let s55 = calculate_cost("claude-sonnet-5-5", 1_000_000, 1_000_000, 0, 0);
        assert!((s55 - 12.0).abs() < 1e-9);
        let s46 = calculate_cost("claude-sonnet-4-6", 1_000_000, 1_000_000, 0, 0);
        assert!((s46 - 18.0).abs() < 1e-9);
        // An unlisted later minor keeps the family default.
        let s51 = calculate_cost("claude-sonnet-5-1", 1_000_000, 1_000_000, 0, 0);
        assert!((s51 - 18.0).abs() < 1e-9);
    }

    #[test]
    fn haiku_5_5_low_tier_up_to_100k_prompt() {
        let m = "claude-haiku-5-5";
        // 100_000-token prompt (exactly at the boundary) + 1_000 out:
        // 0.1 × $0.10 + 0.001 × $0.50 = $0.0105.
        let cost = calculate_cost(m, 100_000, 1_000, 0, 0);
        assert!((cost - 0.0105).abs() < 1e-12, "${cost}");
        // Mixed prompt still ≤ 100k: 40k in + 50k read + 10k 5m write.
        // 0.04×0.10 + 0.05×0.01 + 0.01×0.125 = 0.004 + 0.0005 + 0.00125.
        let cost = calculate_cost_split(m, 40_000, 0, 50_000, 10_000, 0);
        assert!((cost - 0.00575).abs() < 1e-12, "${cost}");
    }

    #[test]
    fn haiku_5_5_high_tier_over_100k_prompt() {
        let m = "claude-haiku-5-5";
        // 100_001 uncached in: 0.100001 × $0.50.
        let cost = calculate_cost(m, 100_001, 0, 0, 0);
        assert!((cost - 0.100_001 * 0.5).abs() < 1e-12, "${cost}");
        // Prompt crosses 100k only through cache reads + 1h writes; the
        // whole request (output too) moves to the high tier.
        // 1k in ($0.0005) + 90k read (0.09 × $0.05) + 10k 1h write
        // (0.01 × $1) + 1M out ($2.50).
        let cost = calculate_cost_split(m, 1_000, 1_000_000, 90_000, 0, 10_000);
        let want = 0.0005 + 0.0045 + 0.01 + 2.5;
        assert!((cost - want).abs() < 1e-12, "${cost} != ${want}");
    }
}
