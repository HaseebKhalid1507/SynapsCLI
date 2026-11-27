//! Centralised pricing logic for Anthropic and other providers' models.
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
//! | Fable 5.2 / Mythos 5.2        | $8.00  | $40.00 | 0.025×     |
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
//! | Haiku 3 (historical)          | $0.25  | $1.25  | fallback* |
//! | Haiku (other)                 | $1.00  | $5.00  | 0.10×      |
//!
//! Historical Opus 3 / Haiku 3 input and output rates:
//! <https://www.anthropic.com/news/claude-3-family> (verified 2026-10-10).
//! *Legacy cache rates use the standard multiplier fallback; historical cache
//! support and prices were not verified from that launch source.
//!
//! **Sonnet 5.** The official model overview explicitly lists $2 input / $10
//! output per million tokens (verified 2026-10-10):
//! <https://platform.claude.com/docs/en/models/sonnet-5/overview>.
//! These are standard global direct-API estimates, not complete invoices:
//! fast-mode/residency multipliers and server-tool charges are not modeled.
//! Unknown-model fallbacks and routed/cloud/subscription model IDs are estimates,
//! not verified provider invoice rates or net charges after credits/discounts.
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
//!
//! **Other providers** (OpenAI, Google Gemini, xAI, Groq, Kimi) are priced from
//! [`provider_rates`]: exact model IDs under the route prefix Synaps uses
//! (`openai-codex/gpt-5.5`, `google/gemini-2.5-flash`, `groq/openai/gpt-oss-20b`,
//! ...), each with its own input, output and cached-input list price and, where
//! the provider publishes one, a long-context tier. Subscription routes (Codex,
//! Copilot, Gemini Code Assist) show the API list price of the same model ID.
//! Any other model keeps the Sonnet fallback.

/// Haiku 5.5 bills a prompt of more than this many tokens at the higher tier
/// (<https://platform.claude.com/docs/en/about-claude/pricing>, "Long context
/// pricing").
const HAIKU_5_5_TIER_TOKENS: u64 = 100_000;

/// USD per million tokens for one non-Anthropic price row.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Rates {
    input: f64,
    output: f64,
    cached: f64,
}

const fn rates(input: f64, output: f64, cached: f64) -> Rates {
    Rates {
        input,
        output,
        cached,
    }
}

/// A non-Anthropic model's list price: base rates, plus the rates a request
/// whose prompt is at least `.0` tokens pays instead.
#[derive(Debug, Clone, Copy, PartialEq)]
struct ProviderRow {
    base: Rates,
    long: Option<(u64, Rates)>,
}

const fn flat(input: f64, output: f64, cached: f64) -> ProviderRow {
    ProviderRow {
        base: rates(input, output, cached),
        long: None,
    }
}

const fn tiered(base: Rates, long_from: u64, long: Rates) -> ProviderRow {
    ProviderRow {
        base,
        long: Some((long_from, long)),
    }
}

// OpenAI, Standard tier: <https://developers.openai.com/api/docs/pricing>
// (read 2026-11-23). "Long" is ">272K input tokens", so 272_001 and up.
// gpt-5.6-sol is the page's current (promotional, to 2026-11-30) price;
// gpt-5.3-codex and gpt-5-mini are listed as deprecated.
fn openai_row(id: &str) -> Option<ProviderRow> {
    const LONG: u64 = 272_001;
    Some(match id {
        "gpt-6.1-sol" => tiered(rates(2.0, 10.0, 0.1), LONG, rates(4.0, 15.0, 0.2)),
        "gpt-6-astra" => tiered(rates(10.0, 50.0, 1.0), LONG, rates(20.0, 75.0, 2.0)),
        "gpt-5.6-sol" => tiered(rates(4.0, 20.0, 0.4), LONG, rates(8.0, 30.0, 0.8)),
        "gpt-5.6-terra" => tiered(rates(2.0, 12.0, 0.2), LONG, rates(4.0, 18.0, 0.4)),
        "gpt-5.6-luna" => tiered(rates(0.2, 1.2, 0.02), LONG, rates(0.4, 1.8, 0.04)),
        "gpt-5.5" => tiered(rates(5.0, 30.0, 0.5), LONG, rates(10.0, 45.0, 1.0)),
        "gpt-5.4" => tiered(rates(2.5, 15.0, 0.25), LONG, rates(5.0, 22.5, 0.5)),
        "gpt-5.4-mini" => flat(0.75, 4.5, 0.075),
        "gpt-5.3-codex" => flat(1.75, 14.0, 0.175),
        "gpt-5-mini" => flat(0.25, 2.0, 0.025),
        _ => return None,
    })
}

// Google Gemini API, Standard paid tier, text input:
// <https://aistudio.google.com/docs/pricing> (ai.google.dev/gemini-api/docs/pricing
// redirects there; read 2026-11-23). Long tier is "prompts > 200k tokens".
fn gemini_row(id: &str) -> Option<ProviderRow> {
    const LONG: u64 = 200_001;
    Some(match id {
        "gemini-3.1-pro-preview" => tiered(rates(2.0, 12.0, 0.2), LONG, rates(4.0, 18.0, 0.4)),
        "gemini-3-flash-preview" => flat(0.5, 3.0, 0.05),
        "gemini-3.1-flash-lite" => flat(0.25, 1.5, 0.025),
        "gemini-2.5-pro" => tiered(rates(1.25, 10.0, 0.125), LONG, rates(2.5, 15.0, 0.25)),
        "gemini-2.5-flash" => flat(0.3, 2.5, 0.03),
        _ => return None,
    })
}

// xAI: <https://docs.x.ai/developers/pricing> (read 2026-11-23); `-latest`
// aliases from <https://docs.x.ai/developers/models>. Long tier is
// "≥ 200k prompt tokens".
fn xai_row(id: &str) -> Option<ProviderRow> {
    const LONG: u64 = 200_000;
    Some(match id {
        "grok-4.7" | "grok-4.6" => tiered(rates(2.0, 6.0, 0.5), LONG, rates(4.0, 12.0, 1.0)),
        "grok-4.5" | "grok-4.5-latest" => tiered(rates(2.0, 6.0, 0.3), LONG, rates(4.0, 12.0, 0.6)),
        "grok-4.3"
        | "grok-4.3-latest"
        | "grok-4.20-0309-reasoning"
        | "grok-4.20-0309-non-reasoning"
        | "grok-4.20-multi-agent-0309" => tiered(rates(1.25, 2.5, 0.2), LONG, rates(2.5, 5.0, 0.4)),
        _ => return None,
    })
}

// Groq: <https://console.groq.com/docs/models> (read 2026-11-23); cached
// input is "a 50% discount" per <https://console.groq.com/docs/prompt-caching>.
fn groq_row(id: &str) -> Option<ProviderRow> {
    Some(match id {
        "openai/gpt-oss-120b" => flat(0.15, 0.6, 0.075),
        "openai/gpt-oss-20b" => flat(0.075, 0.3, 0.0375),
        _ => return None,
    })
}

// Kimi (Moonshot AI) API: <https://platform.kimi.ai/docs/pricing/chat>
// (platform.moonshot.ai redirects there; read 2026-11-23).
fn kimi_row(id: &str) -> Option<ProviderRow> {
    Some(match id {
        "kimi-k3.1-pro" => flat(2.0, 10.0, 0.2),
        "kimi-k3.1-pro-highspeed" => flat(4.0, 20.0, 0.2),
        "kimi-k3" => flat(3.0, 15.0, 0.3),
        "kimi-k2.7-code" => flat(0.95, 4.0, 0.19),
        "kimi-k2.7-code-highspeed" => flat(1.9, 8.0, 0.38),
        "kimi-k2.6" => flat(0.95, 4.0, 0.16),
        _ => return None,
    })
}

/// The non-Anthropic price row for a full Synaps model string. The route
/// prefix picks the provider (the same ID can cost differently elsewhere, e.g.
/// `groq/openai/gpt-oss-20b`); a bare or unknown-prefix ID has no row.
fn provider_row(model: &str) -> Option<ProviderRow> {
    let (prefix, id) = model.split_once('/')?;
    match prefix {
        "openai-codex" => openai_row(id),
        "github-copilot" => openai_row(id).or_else(|| gemini_row(id)),
        "google" | "google-gemini" => gemini_row(id),
        "xai-auth" => xai_row(id),
        "groq" => groq_row(id),
        "kimi" => kimi_row(id),
        _ => None,
    }
}

/// [`provider_row`] at the tier this request's prompt size selects.
fn provider_rates(model: &str, prompt_tokens: u64) -> Option<Rates> {
    let row = provider_row(model)?;
    Some(match row.long {
        Some((from, long)) if prompt_tokens >= from => long,
        _ => row.base,
    })
}

/// Match a complete version, accepting dotted and hyphenated minor versions.
/// Only documented ID suffix shapes are stripped: snapshot dates, Vertex dates,
/// and Bedrock revisions. Similar-looking version prefixes are not aliases.
fn is_version(model: &str, version: &str) -> bool {
    fn suffix_ok(rest: &str) -> bool {
        if matches!(rest, "" | "-latest" | ":0" | "-v1:0") {
            return true;
        }
        let Some(date) = rest.strip_prefix('-').or_else(|| rest.strip_prefix('@')) else {
            return false;
        };
        date.len() >= 8
            && date.as_bytes()[..8].iter().all(u8::is_ascii_digit)
            && matches!(&date[8..], "" | "-v1:0")
    }
    let dotted = version.rsplit_once('-').and_then(|(prefix, minor)| {
        (prefix.ends_with(|c: char| c.is_ascii_digit())
            && minor.bytes().all(|c| c.is_ascii_digit()))
        .then(|| format!("{prefix}.{minor}"))
    });
    let matched = [Some(version), dotted.as_deref()]
        .into_iter()
        .flatten()
        .any(|v| {
            model.match_indices(v).any(|(i, _)| {
                (i == 0 || matches!(model.as_bytes()[i - 1], b'-' | b'/' | b'.'))
                    && suffix_ok(&model[i + v.len()..])
            })
        });
    matched
}

fn is_legacy_opus(m: &str) -> bool {
    ["3-opus", "opus-3", "opus-4", "opus-4-0", "opus-4-1"]
        .iter()
        .any(|v| is_version(m, v))
}

fn is_sonnet_5(m: &str) -> bool {
    is_version(m, "sonnet-5")
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
    listed_prices(model, prompt_tokens).unwrap_or((3.0, 15.0)) // default: Sonnet pricing
}

/// Whether this table has a price for `model`, i.e. its cost is not the
/// Sonnet fallback guess. Callers that must not guess (a worker's reported
/// cost) omit the cost when this is false.
pub fn has_list_price(model: &str) -> bool {
    listed_prices(model, 0).is_some()
}

/// Whether `model` bills a prompt of `larger` tokens at another price tier
/// than one of `smaller` tokens: a long-context tier lies between them, and
/// the WHOLE larger request (cached input and output included) pays it.
/// `false` for a flat-priced model and for one this table does not know.
pub fn prompt_price_tier_differs(model: &str, smaller: u64, larger: u64) -> bool {
    listed_prices(model, smaller) != listed_prices(model, larger)
}

/// [`model_prices`] without the fallback: `None` for a model not in the table.
fn listed_prices(model: &str, prompt_tokens: u64) -> Option<(f64, f64)> {
    anthropic_prices(model, prompt_tokens).or_else(|| {
        let r = provider_rates(model, prompt_tokens)?;
        Some((r.input, r.output))
    })
}

/// The Anthropic (and InferX) arms; these take precedence over provider rows.
fn anthropic_prices(model: &str, prompt_tokens: u64) -> Option<(f64, f64)> {
    Some(match model {
        // InferX publishes DeepSeek V4 Flash at $0/M input and output.
        // Keep this exact ID before generic DeepSeek entries are ever added.
        "inferx/deepseek-v4-flash" | "deepseek-v4-flash" => (0.0, 0.0),
        // Fable 5.2 / Mythos 5.2: $8 / $40 in the model overview (verified
        // 2026-11-12: <https://platform.claude.com/docs/en/models/fable-5-2/overview>).
        m if is_version(m, "fable-5-2") || is_version(m, "mythos-5-2") => (8.0, 40.0),
        m if m.contains("fable") || m.contains("mythos") => (10.0, 50.0),
        m if is_version(m, "opus-5-5") => (4.0, 20.0),
        m if is_legacy_opus(m) => (15.0, 75.0),
        m if m.contains("opus") => (5.0, 25.0),
        m if is_version(m, "sonnet-5-5") => (2.0, 10.0),
        // Sonnet 5: explicit base rates in the model overview (module docs).
        m if is_sonnet_5(m) => (2.0, 10.0),
        m if m.contains("sonnet") => (3.0, 15.0),
        m if is_version(m, "haiku-5-5") => {
            if prompt_tokens > HAIKU_5_5_TIER_TOKENS {
                (0.5, 2.5)
            } else {
                (0.1, 0.5)
            }
        }
        m if is_version(m, "haiku-3-5")
            || is_version(m, "3-5-haiku")
            || is_version(m, "3.5-haiku") =>
        {
            (0.8, 4.0)
        }
        m if is_version(m, "3-haiku") || is_version(m, "haiku-3") => (0.25, 1.25),
        m if m.contains("haiku") => (1.0, 5.0),
        _ => return None,
    })
}

/// Cache-read price as a multiple of the input price
/// (<https://platform.claude.com/docs/en/about-claude/pricing>, "Prompt
/// caching"). The docs list 0.025×
/// for Fable 5.1 / 5.2 and Mythos 5.1 / 5.2 and 0.05× for Opus 5.5 / Sonnet 5.5; every
/// other Anthropic model uses the standard 0.1× (also the conservative default
/// for models this table does not know). Other providers' rows use their own
/// cached-input price over their input price (base tier).
pub fn cache_read_multiplier(model: &str) -> f64 {
    match model {
        m if is_version(m, "fable-5-1")
            || is_version(m, "mythos-5-1")
            || is_version(m, "fable-5-2")
            || is_version(m, "mythos-5-2") =>
        {
            0.025
        }
        m if is_version(m, "opus-5-5") || is_version(m, "sonnet-5-5") => 0.05,
        m if anthropic_prices(m, 0).is_some() => 0.1,
        m => provider_row(m).map_or(0.1, |r| r.base.cached / r.base.input),
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
    if anthropic_prices(model, prompt).is_none() {
        if let Some(r) = provider_rates(model, prompt) {
            // Cached input at the row's own listed price for this tier.
            return (input_tokens as f64 / 1_000_000.0) * r.input
                + (cache_read as f64 / 1_000_000.0) * r.cached
                + (cache_write_5m as f64 / 1_000_000.0) * r.input * 1.25
                + (cache_write_1h as f64 / 1_000_000.0) * r.input * 2.0
                + (output_tokens as f64 / 1_000_000.0) * r.output;
        }
    }
    let (input_price, output_price) = model_prices(model, prompt);
    (input_tokens as f64 / 1_000_000.0) * input_price
        + (cache_read as f64 / 1_000_000.0) * input_price * cache_read_multiplier(model)
        + (cache_write_5m as f64 / 1_000_000.0) * input_price * 1.25
        + (cache_write_1h as f64 / 1_000_000.0) * input_price * 2.0
        + (output_tokens as f64 / 1_000_000.0) * output_price
}

/// Split-aware cost for callers holding an aggregate plus an *optional* TTL
/// split (the shape of `SessionEvent::Usage`). When either split bucket is
/// present, preserve its known count and assign any aggregate remainder to
/// the conservative 1h rate. This is an estimate, not an inferred TTL. A split
/// sum exceeding the aggregate is malformed metadata: retain the larger split
/// sum rather than dropping known writes or lowering the request's price tier.
/// Arithmetic saturates. When both are `None`, TTL is explicitly unknown and
/// the existing aggregate-at-5m estimate is retained (potentially underbilling).
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
        (c5, c1) => {
            let c5 = c5.unwrap_or(0);
            let c1 = c1.unwrap_or(0);
            let remainder = cache_creation.saturating_sub(c5.saturating_add(c1));
            calculate_cost_split(
                model,
                input_tokens,
                output_tokens,
                cache_read,
                c5,
                c1.saturating_add(remainder),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optional_split_preserves_aggregate_known_ttls_and_request_tier() {
        // (input, aggregate, reported 5m, reported 1h, billed 5m, billed 1h)
        let cases = [
            (0, 100_001, Some(0), None, 0, 100_001),
            (0, 100_001, None, Some(0), 0, 100_001),
            (60_000, 40_000, Some(10_000), None, 10_000, 30_000),
            (60_000, 40_001, Some(10_000), None, 10_000, 30_001),
            (60_000, 40_001, None, Some(10_000), 0, 40_001),
            (60_000, 40_001, Some(10_000), Some(10_000), 10_000, 30_001),
            (60_000, 1, Some(20_000), Some(20_001), 20_000, 20_001),
            (60_000, 1, Some(40_001), None, 40_001, 0),
            (60_000, 40_001, Some(40_001), None, 40_001, 0),
            (60_000, 40_001, None, Some(40_001), 0, 40_001),
            (60_000, 40_001, Some(10_000), Some(30_001), 10_000, 30_001),
            (0, u64::MAX, Some(1), None, 1, u64::MAX - 1),
            (0, 1, Some(u64::MAX), Some(u64::MAX), u64::MAX, u64::MAX),
        ];
        for (input, aggregate, c5, c1, billed5, billed1) in cases {
            let actual = calculate_cost_optional_split(
                "claude-haiku-5.5",
                input,
                1_000,
                0,
                aggregate,
                c5,
                c1,
            );
            assert_eq!(
                actual,
                calculate_cost_split("claude-haiku-5.5", input, 1_000, 0, billed5, billed1,),
                "input={input}, aggregate={aggregate}, split={c5:?}/{c1:?}"
            );
            assert!(actual.is_finite() && actual > 0.0);
        }
        // No TTL evidence: intentionally retain the historical 5m estimate.
        assert_eq!(
            calculate_cost_optional_split("claude-haiku-5.5", 60_000, 1_000, 0, 40_001, None, None,),
            calculate_cost("claude-haiku-5.5", 60_000, 1_000, 0, 40_001)
        );
    }

    #[test]
    fn historical_haiku_3_is_bounded_and_not_haiku_3_5() {
        for model in [
            "claude-3-haiku",
            "claude-3-haiku-20240307",
            "haiku-3",
            "claude-haiku-3@20240307",
            "anthropic.claude-3-haiku-20240307-v1:0",
        ] {
            assert_eq!(calculate_cost(model, 1_000_000, 0, 0, 0), 0.25, "{model}");
            assert_eq!(calculate_cost(model, 0, 1_000_000, 0, 0), 1.25, "{model}");
        }
        for model in [
            "claude-3-5-haiku",
            "claude-3.5-haiku",
            "haiku-3-5",
            "haiku-3.5",
        ] {
            assert_eq!(
                calculate_cost(model, 1_000_000, 1_000_000, 0, 0),
                4.8,
                "{model}"
            );
        }
        for model in [
            "haiku-30",
            "haiku-3garbage",
            "3-haikuish",
            "not3-haiku",
            "claude-3-haiku-20240307oops",
        ] {
            assert_eq!(
                calculate_cost(model, 1_000_000, 1_000_000, 0, 0),
                6.0,
                "{model}"
            );
        }
    }

    #[test]
    fn version_aliases_preserve_prices_and_cache_rates() {
        for model in [
            "github-copilot/claude-opus-4.8",
            "claude-opus-4.7",
            "claude-opus-4-8",
            "claude-opus-4-7",
        ] {
            assert_eq!(calculate_cost(model, 1_000_000, 1_000_000, 0, 0), 30.0);
        }
        for model in [
            "claude-opus-4.1",
            "claude-opus-4-1",
            "claude-opus-4-20250514",
            "claude-opus-4.1@20250805",
            "anthropic.claude-opus-4-1-20250805-v1:0",
            "claude-3-opus-20240229",
        ] {
            assert_eq!(calculate_cost(model, 1_000_000, 1_000_000, 0, 0), 90.0);
        }
        for (dotted, dashed) in [
            ("opus-5.5", "opus-5-5"),
            ("sonnet-5.5", "sonnet-5-5"),
            ("haiku-5.5", "haiku-5-5"),
            ("fable-5.1", "fable-5-1"),
            ("fable-5.2", "fable-5-2"),
            ("mythos-5.2", "mythos-5-2"),
            ("mythos-5.1", "mythos-5-1"),
            ("haiku-3.5", "haiku-3-5"),
        ] {
            assert_eq!(
                calculate_cost_split(dotted, 60_000, 1_000, 20_000, 5_000, 10_000),
                calculate_cost_split(dashed, 60_000, 1_000, 20_000, 5_000, 10_000)
            );
            assert_eq!(cache_read_multiplier(dotted), cache_read_multiplier(dashed));
        }
        for model in [
            "opus-4garbage",
            "opus-4.10",
            "opus-4-1oops",
            "opus-4-12345678oops",
            "opus-30",
            "notopus-4",
            "3-opusish",
        ] {
            assert!(!is_legacy_opus(model), "{model}");
        }
        assert_eq!(cache_read_multiplier("opus-5-50"), 0.1);
        assert_eq!(cache_read_multiplier("fable-5.10"), 0.1);
    }

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

    /// The fallback above is a guess; `has_list_price` tells it apart.
    #[test]
    fn has_list_price_is_false_only_for_the_fallback() {
        for m in [
            "claude-sonnet-4-6",
            "anthropic/claude-opus-5-5",
            "claude-haiku-5-5",
            "claude-fable-5",
            "inferx/deepseek-v4-flash",
        ] {
            assert!(has_list_price(m), "{m}");
        }
        for m in ["gpt-99-turbo", "openai/gpt-5", "z-ai/glm-5.2", ""] {
            assert!(!has_list_price(m), "{m}");
        }
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
        // Fable 5.1: $10 × 0.025 = $0.25/MTok; Fable 5.2: $8 × 0.025 = $0.20
        // (the documented cache-read price); Opus 5.5: $4 × 0.05 = $0.20;
        // Sonnet 5.5: $2 × 0.05 = $0.10; Opus 4.7: $5 × 0.1 = $0.50.
        for (m, want) in [
            ("claude-fable-5-1", 0.25),
            ("claude-mythos-5-1", 0.25),
            ("claude-fable-5-2", 0.20),
            ("claude-mythos-5-2", 0.20),
            ("claude-fable-5-2-20261109", 0.20),
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

    /// The tier boundary comes from the price rows, for any tiered model.
    #[test]
    fn a_long_context_tier_between_two_prompt_sizes_is_seen() {
        // Haiku 5.5: over 100,000 prompt tokens.
        assert!(prompt_price_tier_differs("claude-haiku-5-5", 90_000, 400_000));
        assert!(prompt_price_tier_differs("claude-haiku-5-5", 100_000, 100_001));
        assert!(!prompt_price_tier_differs("claude-haiku-5-5", 90_000, 100_000));
        assert!(!prompt_price_tier_differs("claude-haiku-5-5", 150_000, 400_000));
        // Flat and unknown models have none.
        assert!(!prompt_price_tier_differs("claude-opus-5-5", 90_000, 900_000));
        assert!(!prompt_price_tier_differs("claude-haiku-4-5", 90_000, 900_000));
        assert!(!prompt_price_tier_differs("no-such-model", 1, 900_000));
        // A provider row with a long tier.
        assert!(prompt_price_tier_differs("google/gemini-2.5-pro", 1_000, 900_000));
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

    /// `(model, input, output, cached)` per 1M at the base tier, measured on a
    /// 100k-token prompt (below every long-context threshold).
    fn assert_rates(cases: &[(&str, f64, f64, f64)]) {
        for &(m, input, output, cached) in cases {
            let i = calculate_cost(m, 100_000, 0, 0, 0) * 10.0;
            let o = calculate_cost(m, 0, 1_000_000, 0, 0);
            let c = calculate_cost(m, 0, 0, 100_000, 0) * 10.0;
            assert!((i - input).abs() < 1e-9, "{m}: input ${i} != ${input}");
            assert!((o - output).abs() < 1e-9, "{m}: output ${o} != ${output}");
            assert!((c - cached).abs() < 1e-9, "{m}: cached ${c} != ${cached}");
            assert!(
                (cache_read_multiplier(m) - cached / input).abs() < 1e-12,
                "{m}"
            );
            assert!(has_list_price(m), "{m}");
        }
    }

    #[test]
    fn openai_rows_at_api_list_price() {
        assert_rates(&[
            ("openai-codex/gpt-5.5", 5.0, 30.0, 0.5),
            ("openai-codex/gpt-6.1-sol", 2.0, 10.0, 0.1),
            ("openai-codex/gpt-6-astra", 10.0, 50.0, 1.0),
            ("openai-codex/gpt-5.6-luna", 0.2, 1.2, 0.02),
            ("openai-codex/gpt-5.4-mini", 0.75, 4.5, 0.075),
            ("github-copilot/gpt-5.4", 2.5, 15.0, 0.25),
            ("github-copilot/gpt-5.3-codex", 1.75, 14.0, 0.175),
            ("github-copilot/gpt-5-mini", 0.25, 2.0, 0.025),
        ]);
    }

    #[test]
    fn gemini_rows_at_api_list_price() {
        assert_rates(&[
            ("google/gemini-2.5-flash", 0.3, 2.5, 0.03),
            ("google-gemini/gemini-2.5-pro", 1.25, 10.0, 0.125),
            ("google-gemini/gemini-3-flash-preview", 0.5, 3.0, 0.05),
            ("google-gemini/gemini-3.1-flash-lite", 0.25, 1.5, 0.025),
            ("github-copilot/gemini-3.1-pro-preview", 2.0, 12.0, 0.2),
        ]);
    }

    #[test]
    fn xai_groq_and_kimi_rows_at_list_price() {
        assert_rates(&[
            ("xai-auth/grok-4.7", 2.0, 6.0, 0.5),
            ("xai-auth/grok-4.5-latest", 2.0, 6.0, 0.3),
            ("xai-auth/grok-4.20-0309-non-reasoning", 1.25, 2.5, 0.2),
            ("groq/openai/gpt-oss-120b", 0.15, 0.6, 0.075),
            ("groq/openai/gpt-oss-20b", 0.075, 0.3, 0.0375),
            ("kimi/kimi-k3.1-pro", 2.0, 10.0, 0.2),
            ("kimi/kimi-k3", 3.0, 15.0, 0.3),
            ("kimi/kimi-k2.6", 0.95, 4.0, 0.16),
        ]);
    }

    #[test]
    fn long_context_tier_moves_the_whole_request() {
        // (model, last base-tier prompt, base (in, out, cached), long (in, out, cached))
        for (m, edge, base, long) in [
            (
                "openai-codex/gpt-5.5",
                272_000,
                (5.0, 30.0, 0.5),
                (10.0, 45.0, 1.0),
            ),
            (
                "google-gemini/gemini-2.5-pro",
                200_000,
                (1.25, 10.0, 0.125),
                (2.5, 15.0, 0.25),
            ),
            (
                "github-copilot/gemini-3.1-pro-preview",
                200_000,
                (2.0, 12.0, 0.2),
                (4.0, 18.0, 0.4),
            ),
            (
                "xai-auth/grok-4.7",
                199_999,
                (2.0, 6.0, 0.5),
                (4.0, 12.0, 1.0),
            ),
        ] {
            for (prompt, (i, o, c)) in [(edge, base), (edge + 1, long)] {
                // Half the prompt cached; 1M output.
                let read = prompt / 2;
                let fresh = prompt - read;
                let want = fresh as f64 / 1e6 * i + read as f64 / 1e6 * c + o;
                let cost = calculate_cost(m, fresh, 1_000_000, read, 0);
                assert!(
                    (cost - want).abs() < 1e-9,
                    "{m} @ {prompt}: ${cost} != ${want}"
                );
            }
        }
        // A flat row never changes tier.
        let m = "openai-codex/gpt-5.4-mini";
        let cost = calculate_cost(m, 1_000_000, 0, 0, 0);
        assert!((cost - 0.75).abs() < 1e-9, "${cost}");
    }

    #[test]
    fn unlisted_or_misrouted_provider_ids_keep_the_fallback() {
        for m in [
            // Offered by Synaps but not on the provider's price page.
            "openai-codex/gpt-5.3-codex-spark",
            "google-gemini/gemini-3.5-flash",
            "google-gemini/gemini-3-pro-preview",
            "xai-auth/grok-latest",
            "groq/llama-3.3-70b-versatile",
            "kimi-code/kimi-for-coding",
            // Right ID, wrong or missing route prefix.
            "openai/gpt-oss-120b",
            "gpt-5.5",
            "openrouter/openai/gpt-5.5",
            "kimi/gpt-5.5",
        ] {
            assert!(!has_list_price(m), "{m}");
            let cost = calculate_cost(m, 1_000_000, 1_000_000, 1_000_000, 0);
            assert!((cost - 18.3).abs() < 1e-9, "{m}: ${cost}");
            assert_eq!(cache_read_multiplier(m), 0.1, "{m}");
        }
    }

    #[test]
    fn claude_prices_unchanged_on_provider_routes() {
        for (m, want) in [
            ("github-copilot/claude-opus-4.8", 30.0),
            ("github-copilot/claude-sonnet-4.6", 18.0),
            ("anthropic/claude-sonnet-5", 12.0),
            ("claude-haiku-4-5", 6.0),
        ] {
            let cost = calculate_cost(m, 1_000_000, 1_000_000, 0, 0);
            assert!((cost - want).abs() < 1e-9, "{m}: ${cost}");
            assert_eq!(cache_read_multiplier(m), 0.1, "{m}");
        }
    }
}
