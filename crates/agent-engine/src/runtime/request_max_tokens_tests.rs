//! B32: the per-request output cap is window-aware and is the ONE value
//! both the wire (`max_tokens`) and the context assessment's output reserve
//! use (`HelperMethods::max_tokens_for_request`).
//!
//! R1 ("usable 200K"): a native Anthropic request on a window of at most
//! 200K is sized to the room left (`HelperMethods::output_tokens_for_request`,
//! `stream::size_request`), on its calibrated footprint once the window has
//! a provider-reported input; the wire still sends exactly the reserve.

use super::context::{ContextAssessment, ContextBreakdown};
use super::continuation::{
    successor_budget, ContinuationState, TokenCalibration, ADVISORY_RESERVE_TOKENS,
};
use super::helpers::{HelperMethods, MAX_TOKENS_FOR_TEST};
use super::request::RequestBody;
use super::stream::{pre_request_context_budget, size_request, RequestSizing};
use crate::core::config::CacheTtl;
use crate::SharedMessage;
use agent_core::core::config::{ContextManagementConfig, ContextManagementMode};
use agent_core::core::context_policy::{
    assess_context, ContextAction, ContextReason, ContextState, ContextThresholds,
};
use agent_core::reasoning::ReasoningLevel;
use serde_json::{json, Value};
use std::sync::Arc;

/// The default `max_tool_output` (bytes): the next tool result's reserve.
const TOOL_RESULT_BYTES: u64 = 30_000;
/// System prompt + tool schemas of a typical worker request.
const OVERHEAD: u64 = 15_000;
const WINDOWS: [u64; 2] = [200_000, 1_000_000];
/// Every named level with a legacy budget (Max/Ultra keep the prior one).
const LEVELS: [ReasoningLevel; 6] = [
    ReasoningLevel::Off,
    ReasoningLevel::Adaptive,
    ReasoningLevel::Low,
    ReasoningLevel::Medium,
    ReasoningLevel::High,
    ReasoningLevel::XHigh,
];

fn budget_of(level: ReasoningLevel) -> u32 {
    level.to_legacy_budget().expect("a named level with a budget")
}

fn messages() -> Vec<SharedMessage> {
    vec![Arc::new(json!({"role": "user", "content": "hi"}))]
}

/// The wire body for `model` at `window`, as the stream transport builds it.
fn wire(model: &str, budget: u32, level: ReasoningLevel, window: u64) -> Value {
    let messages = messages();
    let body = RequestBody::new(
        model,
        &messages,
        &[],
        &[],
        &None,
        "api_key",
        budget,
        level,
        None,
        CacheTtl::FiveMinutes,
        true,
        window,
    );
    let value = serde_json::to_value(&body).unwrap();
    assert_eq!(value["max_tokens"], json!(body.max_tokens()));
    value
}

/// The output the pre-request assessment reserves for the same request.
fn reserved(model: &str, budget: u32, window: u64) -> u64 {
    pre_request_context_budget(
        model,
        window,
        None,
        &[],
        &messages(),
        u64::from(budget),
        TOOL_RESULT_BYTES,
        None,
        None,
    )
    .sizing
    .reserves
    .output_tokens
}

struct OverrideGuard;

impl OverrideGuard {
    fn set(limit: u64) -> Self {
        MAX_TOKENS_FOR_TEST.with(|cell| cell.set(Some(limit)));
        Self
    }
}

impl Drop for OverrideGuard {
    fn drop(&mut self) {
        MAX_TOKENS_FOR_TEST.with(|cell| cell.set(None));
    }
}

#[test]
fn the_request_cap_is_128k_only_for_opus_above_a_200k_window() {
    for (model, window, want) in [
        ("claude-opus-4-6", 200_000, 64_000),
        ("claude-opus-4-7", 200_000, 64_000),
        ("claude-opus-5-5", 200_000, 64_000),
        ("claude-opus-5-5", 150_000, 64_000),
        ("claude-opus-5-5", 200_001, 128_000),
        ("claude-opus-4-6", 1_000_000, 128_000),
        ("claude-opus-4-7", 1_000_000, 128_000),
        ("claude-opus-5-5", 1_000_000, 128_000),
        ("github-copilot/claude-opus-4.7", 200_000, 64_000),
        ("claude-sonnet-4-6", 200_000, 64_000),
        ("claude-sonnet-4-6", 1_000_000, 64_000),
        ("claude-sonnet-5-5", 1_000_000, 64_000),
        ("claude-haiku-4-5", 200_000, 64_000),
        ("claude-haiku-4-5", 1_000_000, 64_000),
        ("gpt-6-astra", 1_050_000, 64_000),
        ("google-gemini/gemini-2.5-pro", 1_000_000, 64_000),
    ] {
        assert_eq!(
            HelperMethods::max_tokens_for_request(model, window),
            want,
            "{model}@{window}"
        );
    }
    // The model maximum (the summary routes' ceiling) is unchanged.
    assert_eq!(HelperMethods::max_tokens_for_model("claude-opus-5-5"), 128_000);
    assert_eq!(HelperMethods::max_tokens_for_model("claude-sonnet-4-6"), 64_000);
}

#[test]
fn the_live_test_override_is_honoured_at_any_window() {
    let _guard = OverrideGuard::set(16_000);
    for model in ["claude-opus-5-5", "claude-sonnet-4-6"] {
        for window in WINDOWS {
            assert_eq!(HelperMethods::max_tokens_for_request(model, window), 16_000);
            assert_eq!(wire(model, 0, ReasoningLevel::Off, window)["max_tokens"], 16_000);
        }
    }
}

/// The hard history budget (`window - (max(reserves + advisory,
/// reserve_tokens) + overhead)`, as the stream's rollover computes it) holds
/// the default successor target at every thinking level, for every model
/// class whose thinking spends inside the output cap. Red before B32 for
/// Opus at 200K (128K reserved: ~16.5K budget vs a ~78K target).
/// Wires that reserve thinking separately (Gemini, unroutable) are outside
/// B32: at 200K their budget falls below the target from medium up.
#[test]
fn the_hard_history_budget_holds_the_successor_target() {
    let config = ContextManagementConfig::default();
    for model in [
        "claude-opus-5-5",
        "claude-opus-4-7",
        "claude-opus-4-6",
        "claude-sonnet-4-6",
        "claude-haiku-4-5",
        "openai-codex/gpt-5.2-codex",
    ] {
        for window in WINDOWS {
            let thresholds = ContextThresholds::resolve(&config, window).unwrap();
            let target = successor_budget(
                thresholds.target_tokens(&config),
                thresholds.pressure_tokens,
                OVERHEAD,
                None,
            );
            for level in LEVELS {
                let assessment = pre_request_context_budget(
                    model,
                    window,
                    None,
                    &[],
                    &[],
                    u64::from(budget_of(level)),
                    TOOL_RESULT_BYTES,
                    None,
                    None,
                )
                .assessment;
                let history_budget = window.saturating_sub(
                    assessment
                        .reserves
                        .total()
                        .saturating_add(ADVISORY_RESERVE_TOKENS)
                        .max(config.reserve_tokens)
                        .saturating_add(OVERHEAD),
                );
                assert!(
                    history_budget >= target.history_tokens,
                    "{model}@{window} {level:?}: history budget {history_budget} < target {}",
                    target.history_tokens
                );
            }
        }
    }
}

/// The wire's `max_tokens` is exactly the output the assessment reserved
/// where the request is sized (R1: native Anthropic at 200K), for adaptive
/// and budgeted Opus and for Sonnet; the wire sends the window-aware cap at
/// 200K and 1M.
///
/// PARITY (E2, upstream #341): out of R1's scope (the 1M window) the
/// assessment reserves #341's expected output, the thinking (16,384 for a
/// zero budget) + 16,384 under the cap, while the wire still sends the cap.
#[test]
fn the_wire_max_tokens_is_the_reserved_output() {
    for model in ["claude-opus-4-7", "claude-opus-4-6", "claude-sonnet-4-6"] {
        for window in WINDOWS {
            for level in LEVELS {
                let budget = budget_of(level);
                let sent = wire(model, budget, level, window)["max_tokens"]
                    .as_u64()
                    .unwrap();
                let expected = if model.contains("opus") && window > 200_000 {
                    128_000
                } else {
                    64_000
                };
                assert_eq!(sent, expected, "{model}@{window} {level:?}");
                let thinking = if budget == 0 {
                    16_384
                } else {
                    u64::from(budget)
                };
                let want_reserved = if window <= 200_000 {
                    sent
                } else {
                    expected.min(thinking + 16_384)
                };
                assert_eq!(
                    reserved(model, budget, window),
                    want_reserved,
                    "{model}@{window} {level:?}"
                );
            }
        }
    }
}

/// The runtime's own assessment reserves under the window-aware cap: the
/// model's native window (200K) and a 200K override cap Opus at 64K, the
/// 1M override at 128K.
///
/// PARITY (E2, upstream #341): unsized (context management off) the
/// reserve is the expected output, the thinking + 16,384 under that cap,
/// not the cap itself; a budget above both caps shows the cap binding.
#[tokio::test]
async fn the_runtime_assessment_reserves_the_window_aware_cap() {
    let mut runtime = crate::Runtime::new_headless();
    runtime.set_model("claude-opus-4-7".to_string());
    assert_eq!(runtime.model(), "claude-opus-4-7");
    let expected = u64::from(runtime.thinking_budget()) + 16_384;
    assert_eq!(expected, 20_480);
    for window in [None, Some(200_000), Some(1_000_000)] {
        runtime.set_context_window(window);
        let assessment = runtime.assess_context(&messages()).await;
        assert_eq!(assessment.reserves.output_tokens, expected, "{window:?}");
    }
    runtime.set_thinking_budget(120_000);
    for (window, want) in [
        (None, 64_000),
        (Some(200_000), 64_000),
        (Some(1_000_000), 128_000),
    ] {
        runtime.set_context_window(window);
        let assessment = runtime.assess_context(&messages()).await;
        assert_eq!(assessment.reserves.output_tokens, want, "{window:?}");
    }
}

/// The window a request is sized by: the runtime's effective window when
/// the caller passes it, else 1M with the 1M flag, else the model's own.
#[test]
fn the_api_options_window_falls_back_to_the_flag_then_the_model() {
    use super::api::ApiOptions;
    let model = "claude-opus-4-7";
    assert_eq!(ApiOptions::default().effective_context_window(model), 200_000);
    assert_eq!(
        ApiOptions::default().effective_context_window("gpt-6-astra"),
        1_050_000
    );
    let flagged = ApiOptions {
        use_1m_context: true,
        ..ApiOptions::default()
    };
    assert_eq!(flagged.effective_context_window(model), 1_000_000);
    // A model that can't take 1M is sized by its own ceiling.
    assert_eq!(
        flagged.effective_context_window("claude-haiku-4-5"),
        200_000
    );
    let explicit = ApiOptions {
        use_1m_context: true,
        diagnostic_context_window: Some(500_000),
        ..ApiOptions::default()
    };
    assert_eq!(explicit.effective_context_window(model), 500_000);
}

/// Thinking fits below `max_tokens` at 200K for every level that reaches
/// the Anthropic wire: budgeted models send `budget_tokens` strictly below
/// the cap (Off sends none; Adaptive sends the 16,384 fallback; Max, Ultra
/// and UltraCode keep the prior budget, at most the named 32,768); adaptive
/// models send no `budget_tokens` at all (the thinking field does not depend
/// on the execution plan that carries `max`, so the plan is omitted here).
#[test]
fn every_thinking_level_fits_below_max_tokens_at_200k() {
    let window = 200_000;
    for model in ["claude-opus-4-6", "claude-sonnet-4-6", "claude-sonnet-4-5"] {
        let named = LEVELS.map(|level| (level, budget_of(level)));
        let kept = [
            ReasoningLevel::Max,
            ReasoningLevel::Ultra,
            ReasoningLevel::UltraCode,
        ]
        .map(|level| (level, 32_768));
        for (level, budget) in named.into_iter().chain(kept) {
            let body = wire(model, budget, level, window);
            let max = body["max_tokens"].as_u64().unwrap();
            if level == ReasoningLevel::Off {
                assert!(body.get("thinking").is_none(), "{model} {level:?}");
                continue;
            }
            assert_eq!(body["thinking"]["type"], "enabled", "{model} {level:?}");
            let sent = body["thinking"]["budget_tokens"].as_u64().unwrap();
            assert!(sent < max, "{model} {level:?}: {sent} >= {max}");
            let want = if budget == 0 { 16_384 } else { u64::from(budget) };
            assert_eq!(sent, want, "{model} {level:?}: named budgets are unchanged");
        }
    }
    for model in ["claude-opus-4-7", "claude-opus-5-5", "claude-sonnet-4-7"] {
        for level in LEVELS {
            let body = wire(model, budget_of(level), level, window);
            assert!(
                body["thinking"].get("budget_tokens").is_none(),
                "{model} {level:?}: {body}"
            );
        }
    }
}

/// A custom budget (`/thinking N`, config, resume) at or over the cap is
/// clamped strictly below it; under the cap it is sent as is.
#[test]
fn a_custom_budget_at_or_over_the_cap_is_clamped_below_it() {
    for (budget, window, want_max, want_budget) in [
        (100_000, 200_000, 64_000, 63_999),
        (64_000, 200_000, 64_000, 63_999),
        (63_999, 200_000, 64_000, 63_999),
        (100_000, 1_000_000, 128_000, 100_000),
        (200_000, 1_000_000, 128_000, 127_999),
    ] {
        let body = wire("claude-opus-4-6", budget, ReasoningLevel::XHigh, window);
        assert_eq!(body["max_tokens"], json!(want_max), "{budget}@{window}");
        assert_eq!(
            body["thinking"]["budget_tokens"],
            json!(want_budget),
            "{budget}@{window}"
        );
    }
    assert_eq!(HelperMethods::anthropic_thinking_budget(0, 64_000), 16_384);
    assert_eq!(HelperMethods::anthropic_thinking_budget(32_768, 64_000), 32_768);
    assert_eq!(HelperMethods::anthropic_thinking_budget(70_000, 64_000), 63_999);
}

// ── R1: usable 200K ─────────────────────────────────────────────────────────

const OPUS: &str = "claude-opus-5-5";
/// The xhigh thinking budget.
const XHIGH: u64 = 32_768;
const NATIVE: u64 = 200_000;

/// The uncalibrated assessment of a request to `model` at `window` whose
/// estimated input footprint is `estimated` tokens, with today's reserves
/// for that request (static output cap, flat 15% margin), exactly as the
/// stream's pre-request assessment builds them.
fn estimated_at(model: &str, window: u64, thinking: u64, estimated: u64) -> ContextAssessment {
    let base = pre_request_context_budget(
        model,
        window,
        None,
        &[],
        &[],
        thinking,
        TOOL_RESULT_BYTES,
        None,
        None,
    )
    .assessment;
    ContextAssessment {
        breakdown: ContextBreakdown {
            system_tokens: 0,
            tool_schema_tokens: 0,
            history_tokens: estimated,
            framing_tokens: 0,
            skill_tokens: 0,
            memory_tokens: 0,
        },
        ..base
    }
}

/// Whether the context policy admits a request sized as `sizing`: anything
/// but the hard-capacity stop, which is what rolls the context over.
fn admits(sizing: &RequestSizing, window: u64) -> bool {
    let config = ContextManagementConfig {
        mode: ContextManagementMode::Auto,
        ..ContextManagementConfig::default()
    };
    let decision = assess_context(
        &config,
        &ContextState::default(),
        sizing.admission_budget(window),
    );
    if decision.action == ContextAction::HardStop {
        assert_eq!(decision.reason, ContextReason::HardCapacity);
        return false;
    }
    true
}

/// R1 at 200K Opus with xhigh thinking (32,768) and the default 30,000-byte
/// tool cap: sized to the room left, `max_tokens` runs from the 64,000 cap
/// down to the floor, 49,152 (thinking + 16,384), at the input ceiling:
/// 110,336 estimated tokens before the window's first provider-reported
/// input and 130,336 calibrated tokens after it.
///
/// PARITY (E2, upstream #341): the unsized assessment ("today") already
/// reserves the expected output, thinking + 16,384 = 49,152, not the cap, so
/// its ceiling is 110,336 too; R1 keeps that timing before calibration,
/// sends the room left instead of the cap, and raises the ceiling once
/// calibrated. Ours had a 24,000 floor (ceilings 95,488 -> 102,720 ->
/// 122,720), which the test's name keeps.
#[test]
fn the_200k_opus_ceiling_rises_from_95_488_to_122_720_calibrated() {
    // Before (#341): 200,000 - (49,152 + 10,000 + 30,000 + 512).
    let today = |estimated: u64| RequestSizing {
        footprint_tokens: estimated,
        reserves: estimated_at(OPUS, NATIVE, XHIGH, estimated).reserves,
        wire_max_tokens: None,
    };
    assert_eq!(today(0).reserves.output_tokens, 49_152);
    assert_eq!(today(0).reserves.safety_margin_tokens, 30_000);
    assert!(admits(&today(110_336), NATIVE));
    assert!(!admits(&today(110_337), NATIVE));

    // After, before the first reported input: the estimate and the flat
    // 15%. The output is the room left, from the cap down to the floor,
    // which it meets at the same ceiling as the unsized reserve.
    let uncalibrated = |estimated: u64| {
        size_request(
            OPUS,
            &estimated_at(OPUS, NATIVE, XHIGH, estimated),
            XHIGH,
            None,
        )
    };
    assert_eq!(uncalibrated(95_488).reserves.output_tokens, 64_000);
    assert_eq!(uncalibrated(95_488).wire_max_tokens, Some(64_000));
    assert_eq!(uncalibrated(100_000).reserves.output_tokens, 59_488);
    let ceiling = uncalibrated(110_336);
    assert_eq!(
        ceiling,
        RequestSizing {
            wire_max_tokens: Some(49_152),
            ..today(110_336)
        }
    );
    assert!(admits(&ceiling, NATIVE));
    assert_eq!(ceiling.reserves.output_tokens, 49_152);
    assert!(!admits(&uncalibrated(110_337), NATIVE));

    // After, calibrated: a sample of 80,000 reported for 100,000 estimated
    // (ratio 1.25, as in the field), then 50,336 estimated tokens appended.
    let sample = TokenCalibration {
        provider_tokens: 80_000,
        estimated_tokens: 100_000,
    };
    let calibrated = |estimated: u64| {
        size_request(
            OPUS,
            &estimated_at(OPUS, NATIVE, XHIGH, estimated),
            XHIGH,
            Some(sample),
        )
    };
    let ceiling = calibrated(150_336);
    assert_eq!(ceiling.footprint_tokens, 130_336);
    assert_eq!(ceiling.reserves.safety_margin_tokens, 10_000);
    assert_eq!(ceiling.reserves.tool_result_tokens, 10_000);
    assert_eq!(ceiling.reserves.output_tokens, 49_152);
    assert_eq!(ceiling.wire_max_tokens, Some(49_152));
    assert!(admits(&ceiling, NATIVE));
    assert!(!admits(&calibrated(150_337), NATIVE));
    assert_eq!(
        NATIVE - (XHIGH + 16_384) - 10_000 - 10_000 - ADVISORY_RESERVE_TOKENS,
        130_336
    );
}

/// The sized output never drops below the request's thinking budget +
/// 16,384 (bounded by the cap) and always stays above the `budget_tokens`
/// the wire sends, at every footprint, calibrated or not.
///
/// PARITY (E2, upstream #341): the floor is #341's expected output reserve
/// (`ANSWER_FLOOR_TOKENS` = `EXPECTED_RESPONSE_TOKENS`); ours was 24,000,
/// which the test's name keeps.
#[test]
fn the_output_never_drops_below_thinking_plus_24k_and_stays_above_the_budget() {
    assert_eq!(HelperMethods::output_floor_tokens(64_000, 32_768), 49_152);
    assert_eq!(HelperMethods::output_floor_tokens(64_000, 16_384), 32_768);
    // Adaptive (0) counts as the legacy fallback budget the wire would send.
    assert_eq!(HelperMethods::output_floor_tokens(64_000, 0), 32_768);
    assert_eq!(HelperMethods::output_floor_tokens(64_000, 2_048), 18_432);
    // A budget the wire clamps below the cap leaves the cap.
    assert_eq!(HelperMethods::output_floor_tokens(64_000, 63_999), 64_000);
    assert_eq!(HelperMethods::output_floor_tokens(64_000, 100_000), 64_000);
    for model in [
        "claude-opus-4-6",
        "claude-opus-5-5",
        "claude-sonnet-4-6",
        "claude-opus-4-1",
    ] {
        let cap = HelperMethods::max_tokens_for_request(model, NATIVE);
        for budget in LEVELS
            .map(budget_of)
            .into_iter()
            .chain([40_000, 63_999, 100_000])
        {
            let sent_budget = u64::from(HelperMethods::anthropic_thinking_budget(budget, cap));
            let floor = (sent_budget + 16_384).min(cap);
            for estimated in (0..=NATIVE).step_by(4_096) {
                let sample = TokenCalibration {
                    provider_tokens: estimated * 4 / 5,
                    estimated_tokens: estimated,
                };
                for calibration in [None, Some(sample)] {
                    let sizing = size_request(
                        model,
                        &estimated_at(model, NATIVE, u64::from(budget), estimated),
                        u64::from(budget),
                        calibration,
                    );
                    let output = sizing.reserves.output_tokens;
                    let at = format!("{model} budget {budget} at {estimated} {calibration:?}");
                    assert_eq!(sizing.wire_max_tokens, Some(output), "{at}");
                    assert!(output >= floor, "{at}: {output} < {floor}");
                    assert!(output > sent_budget, "{at}: {output} <= {sent_budget}");
                    assert!(output <= cap, "{at}: {output} > {cap}");
                }
            }
        }
    }
}

/// Before the current window's first provider-reported input a request is
/// admitted on the uncalibrated estimate with the flat 15% margin; the
/// stream only reads a sample measured in the CURRENT window.
#[test]
fn before_the_first_reported_input_the_flat_15_percent_applies() {
    let assessment = estimated_at(OPUS, NATIVE, XHIGH, 90_000);
    let flat = size_request(OPUS, &assessment, XHIGH, None);
    assert_eq!(flat.footprint_tokens, 90_000);
    assert_eq!(flat.reserves.safety_margin_tokens, 30_000);
    assert_eq!(flat.reserves.tool_result_tokens, 10_000);
    let sample = TokenCalibration {
        provider_tokens: 72_000,
        estimated_tokens: 90_000,
    };
    let calibrated = size_request(OPUS, &assessment, XHIGH, Some(sample));
    assert_eq!(calibrated.footprint_tokens, 72_000);
    assert_eq!(calibrated.reserves.safety_margin_tokens, 10_000);

    let mut state = ContinuationState::default();
    assert_eq!(state.window_calibration_for(OPUS), None);
    state.record_calibration(OPUS, sample);
    assert_eq!(state.window_calibration_for(OPUS), Some(sample));
    assert_eq!(state.window_calibration_for("claude-sonnet-4-6"), None);
    // A committed rollover starts the next window uncalibrated; the ratio
    // itself is kept for successor sizing.
    state.window += 1;
    assert_eq!(state.window_calibration_for(OPUS), None);
    assert_eq!(state.calibration_for(OPUS), Some(sample));
    state.record_calibration(OPUS, sample);
    assert_eq!(state.window_calibration_for(OPUS), Some(sample));
    // An implausible report clears it.
    state.record_calibration(
        OPUS,
        TokenCalibration {
            provider_tokens: 0,
            estimated_tokens: 90_000,
        },
    );
    assert_eq!(state.window_calibration_for(OPUS), None);

    // Through the stream's pre-request assessment: the static assessment
    // (telemetry, calibration probe, successor sizing) is unchanged; the
    // budget is admitted on the sized footprint and reserves.
    let messages = messages();
    let budget_with = |calibration| {
        pre_request_context_budget(
            OPUS,
            NATIVE,
            None,
            &[],
            &messages,
            XHIGH,
            TOOL_RESULT_BYTES,
            None,
            calibration,
        )
    };
    let first = budget_with(None);
    let estimated = first.assessment.used_tokens();
    assert_eq!(first.budget.used_tokens, estimated);
    assert_eq!(first.sizing.reserves.safety_margin_tokens, 30_000);
    assert_eq!(first.sizing.wire_max_tokens, Some(64_000));
    let later = budget_with(Some(TokenCalibration {
        provider_tokens: 120_000,
        estimated_tokens: estimated,
    }));
    assert_eq!(later.assessment, first.assessment);
    assert_eq!(later.budget.used_tokens, 120_000);
    assert_eq!(later.sizing.reserves.output_tokens, 59_488);
    assert_eq!(later.sizing.wire_max_tokens, Some(59_488));
    assert_eq!(
        later.budget.required_next_round_tokens,
        59_488 + 10_000 + 10_000 + ADVISORY_RESERVE_TOKENS
    );
}

/// After it, the footprint is the reported input plus a conservative
/// estimate of what was appended since; a shrunken estimate (nothing
/// appended) is admitted on whichever pair leaves more room.
#[test]
fn after_the_first_reported_input_the_footprint_is_that_input_plus_the_appended_estimate() {
    let size = |estimated: u64, sample: TokenCalibration| {
        let sizing = size_request(
            OPUS,
            &estimated_at(OPUS, NATIVE, XHIGH, estimated),
            XHIGH,
            Some(sample),
        );
        (sizing.footprint_tokens, sizing.reserves.safety_margin_tokens)
    };
    // The estimator read above the provider (the usual case): the growth
    // counts at the estimate, never scaled down.
    let high = TokenCalibration {
        provider_tokens: 80_000,
        estimated_tokens: 100_000,
    };
    assert_eq!(size(110_000, high), (90_000, 10_000));
    // The estimator read below: the growth is scaled up by 100/80, rounded up.
    let low = TokenCalibration {
        provider_tokens: 100_000,
        estimated_tokens: 80_000,
    };
    assert_eq!(size(90_000, low), (112_500, 10_000));
    assert_eq!(size(80_001, low), (100_002, 10_000));
    // A small shrink: the sample bounds a history that only lost content.
    assert_eq!(size(99_000, high), (80_000, 10_000));
    // A large shrink (an in-place compaction): the stale sample would leave
    // less room than the flat estimate, so the flat one applies.
    assert_eq!(size(40_000, high), (40_000, 30_000));
}

/// Windows above 200K and every other provider and wire keep today's output
/// cap, margin and footprint exactly, even with a calibration sample.
#[test]
fn windows_above_200k_and_other_wires_keep_todays_output_and_margin() {
    let sample = TokenCalibration {
        provider_tokens: 60_000,
        estimated_tokens: 75_000,
    };
    for (model, window) in [
        ("claude-opus-5-5", 1_000_000),
        ("claude-opus-4-6", 1_000_000),
        ("claude-opus-5-5", 200_001),
        ("openai-codex/gpt-5.2-codex", 200_000),
        ("github-copilot/claude-opus-4.7", 200_000),
        ("google-gemini/gemini-2.5-pro", 200_000),
    ] {
        let at = format!("{model}@{window}");
        assert!(!HelperMethods::dynamic_output_in_scope(model, window), "{at}");
        let cap = HelperMethods::max_tokens_for_request(model, window);
        assert_eq!(
            HelperMethods::output_tokens_for_request(model, window, XHIGH, 0),
            cap,
            "{at}"
        );
        for estimated in [0, 75_000, 95_488, 150_000, window] {
            let assessment = estimated_at(model, window, XHIGH, estimated);
            for calibration in [None, Some(sample)] {
                assert_eq!(
                    size_request(model, &assessment, XHIGH, calibration),
                    RequestSizing {
                        footprint_tokens: estimated,
                        reserves: assessment.reserves,
                        wire_max_tokens: None,
                    },
                    "{at} {estimated} {calibration:?}"
                );
                assert_eq!(
                    assessment.reserves.safety_margin_tokens,
                    window * 15 / 100,
                    "{at}"
                );
            }
        }
        let sized = pre_request_context_budget(
            model,
            window,
            None,
            &[],
            &messages(),
            XHIGH,
            TOOL_RESULT_BYTES,
            None,
            Some(sample),
        );
        assert_eq!(sized.budget.used_tokens, sized.assessment.used_tokens(), "{at}");
        assert_eq!(
            sized.budget.required_next_round_tokens,
            sized.assessment.reserves.total() + ADVISORY_RESERVE_TOKENS,
            "{at}"
        );
    }
    // Cloud routes never reach the native transport either.
    assert!(!HelperMethods::dynamic_output_in_scope(
        "aws-bedrock/anthropic.claude-3-haiku#synaps-context=ctx-test",
        NATIVE
    ));
    // In scope: the native Anthropic wire at or below 200K.
    for (model, window) in [
        ("claude-opus-5-5", 200_000),
        ("anthropic/claude-opus-4-6", 200_000),
        ("claude-opus-4-1", 200_000),
        ("claude-sonnet-4-6", 150_000),
    ] {
        assert!(
            HelperMethods::dynamic_output_in_scope(model, window),
            "{model}@{window}"
        );
    }
}

/// The body the transport sends carries exactly the sized reserve as
/// `max_tokens`, and the thinking block (part of the prompt-cache key) is
/// untouched by it.
#[test]
fn the_wire_max_tokens_is_the_sized_reserved_output() {
    let messages = messages();
    for (model, level) in [
        ("claude-opus-4-6", ReasoningLevel::XHigh),
        ("claude-opus-4-6", ReasoningLevel::High),
        ("claude-opus-4-6", ReasoningLevel::Off),
        ("claude-opus-4-6", ReasoningLevel::Adaptive),
        ("claude-opus-5-5", ReasoningLevel::XHigh),
        ("claude-sonnet-4-6", ReasoningLevel::Medium),
        ("claude-opus-4-1", ReasoningLevel::XHigh),
    ] {
        let budget = budget_of(level);
        for estimated in [0, 95_488, 100_000, 102_720, 120_000, 140_000] {
            let sample = TokenCalibration {
                provider_tokens: estimated * 4 / 5,
                estimated_tokens: estimated,
            };
            for calibration in [None, Some(sample)] {
                let at = format!("{model} {level:?} at {estimated} {calibration:?}");
                let sizing = size_request(
                    model,
                    &estimated_at(model, NATIVE, u64::from(budget), estimated),
                    u64::from(budget),
                    calibration,
                );
                let reserved = sizing.wire_max_tokens.expect("in scope");
                assert_eq!(reserved, sizing.reserves.output_tokens, "{at}");
                let mut body = RequestBody::new(
                    model,
                    &messages,
                    &[],
                    &[],
                    &None,
                    "api_key",
                    budget,
                    level,
                    None,
                    CacheTtl::FiveMinutes,
                    true,
                    NATIVE,
                );
                let before = serde_json::to_value(&body).unwrap();
                assert!(body.apply_reserved_max_tokens(reserved), "{at}");
                let sent = serde_json::to_value(&body).unwrap();
                assert_eq!(sent["max_tokens"], json!(reserved), "{at}");
                assert_eq!(sent["thinking"], before["thinking"], "{at}");
                if let Some(budget_tokens) = sent["thinking"]["budget_tokens"].as_u64() {
                    assert!(reserved > budget_tokens, "{at}");
                }
            }
        }
    }
}

/// Models before Claude 4.5 reject `input + max_tokens` above the window:
/// every admitted request keeps its footprint, `max_tokens`, the tool
/// reserve, the margin and the advisory reserve within the window.
#[test]
fn an_older_model_keeps_input_plus_max_tokens_within_the_window() {
    let model = "claude-opus-4-1";
    assert!(HelperMethods::dynamic_output_in_scope(model, NATIVE));
    let mut admitted = 0;
    for level in LEVELS {
        let thinking = u64::from(budget_of(level));
        for estimated in (0..=NATIVE).step_by(1_024) {
            let read_high = TokenCalibration {
                provider_tokens: estimated * 4 / 5,
                estimated_tokens: estimated,
            };
            let read_low = TokenCalibration {
                provider_tokens: estimated * 5 / 4,
                estimated_tokens: estimated,
            };
            for calibration in [None, Some(read_high), Some(read_low)] {
                let sizing = size_request(
                    model,
                    &estimated_at(model, NATIVE, thinking, estimated),
                    thinking,
                    calibration,
                );
                if !admits(&sizing, NATIVE) {
                    continue;
                }
                admitted += 1;
                let max_tokens = sizing.wire_max_tokens.expect("in scope");
                assert!(
                    sizing.footprint_tokens
                        + max_tokens
                        + sizing.reserves.tool_result_tokens
                        + sizing.reserves.safety_margin_tokens
                        + ADVISORY_RESERVE_TOKENS
                        <= NATIVE,
                    "{level:?} at {estimated} {calibration:?}"
                );
            }
        }
    }
    assert!(admitted > 0);
}

/// The runtime's own assessment (`synaps chat`'s compaction trigger, the
/// context query) reserves the stream's uncalibrated sized output when
/// context management is on; off, no request is sized and it keeps the
/// expected output (PARITY E2, upstream #341: thinking + 16,384 = 49,152
/// under the 64K cap, not the cap).
#[tokio::test]
async fn the_runtime_assessment_reserves_the_sized_output_when_context_managed() {
    let model = "claude-opus-4-6";
    let mut runtime = crate::Runtime::new_headless();
    runtime.set_model(model.to_string());
    runtime.set_thinking_budget(32_768);
    runtime.set_max_tool_output(30_000);
    runtime.set_context_window(Some(NATIVE));
    // About 97,000 estimated tokens of history (2.5 ASCII chars a token),
    // where the room left is below the 64K cap.
    let long: Vec<SharedMessage> = vec![Arc::new(json!({
        "role": "user",
        "content": "x".repeat(242_500),
    }))];
    let off = runtime.assess_context(&long).await;
    assert_eq!(off.reserves.output_tokens, 49_152);
    assert_eq!(off.reserves.safety_margin_tokens, 30_000);

    runtime
        .continuation
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .config
        .mode = ContextManagementMode::Auto;
    let on = runtime.assess_context(&long).await;
    assert_eq!(on.used_tokens(), off.used_tokens());
    let room = NATIVE
        .saturating_sub(off.used_tokens() + 10_000 + 30_000 + ADVISORY_RESERVE_TOKENS);
    let want = HelperMethods::output_tokens_for_request(model, NATIVE, XHIGH, room);
    assert_eq!(on.reserves.output_tokens, want);
    assert!((49_152..64_000).contains(&want), "{want}");
    assert_eq!(on.reserves.safety_margin_tokens, 30_000);
    // A short history keeps the cap.
    let short = runtime.assess_context(&messages()).await;
    assert_eq!(short.reserves.output_tokens, 64_000);
}
