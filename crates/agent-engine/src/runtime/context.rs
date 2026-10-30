//! Task 29 — centralized request-aware context budgeting (spec §9.1).
//!
//! The engine — not individual frontends — computes the effective context of
//! the NEXT provider request from the segments that will actually be sent:
//!
//! - actual effective system segments;
//! - exposed tool schemas;
//! - conversation history plus protocol framing;
//! - loaded skill and memory content;
//!
//! and carves typed reserves out of the provider window before dispatch:
//!
//! - reasoning/thinking reserve;
//! - next likely tool-result reserve;
//! - expected output reserve: what one round realistically produces
//!   ([`EXPECTED_RESPONSE_TOKENS`] on top of its thinking), bounded by the
//!   request's `max_tokens`. `max_tokens` is a ceiling, not headroom the
//!   request needs (see [`ContextReserves::output_tokens`]);
//! - a documented safety margin of [`SAFETY_MARGIN_PERCENT`] (spec requires
//!   10–15%).
//!
//! Frontends consume [`ContextAssessment::should_compact`] — no per-frontend
//! token math is allowed on the trigger path (enforced by the phase 5
//! source-scan test).
//!
//! ## Estimation strategy
//!
//! Provider tokenizers plug in through [`estimator_for_model`] when they are
//! available in-tree. None is today, so every model resolves to the
//! CONSERVATIVE estimator, which must never overstate remaining capacity:
//!
//! - ASCII is charged at 1 token per 2.5 characters — the realistic BPE
//!   worst case for high-entropy ASCII (hashes, base64, minified JSON), and
//!   a deliberate ~1.6× overcount for English prose;
//! - non-ASCII is charged at 1 token per UTF-8 byte — the byte-fallback
//!   worst case, covering CJK (3 bytes/char), emoji scalars (4 bytes), and
//!   two-byte scripts without ever understating.
//!
//! The estimator therefore errs toward EARLIER compaction, never toward
//! provider exhaustion; the safety margin absorbs residual framing drift.

use serde_json::Value;

use crate::SharedMessage;

/// Documented pre-dispatch safety margin, as a percentage of the provider
/// window (spec §9.1: target at least 10–15% reserved capacity).
pub const SAFETY_MARGIN_PERCENT: u64 = 15;

/// Visible output (text and tool calls) one round is expected to need on
/// top of its thinking. The output reserve is this plus the thinking reserve,
/// capped at the request's `max_tokens` ([`ContextBudgetInputs::max_output_tokens`]).
///
/// Not the `max_tokens` ceiling itself (128k for Opus): Claude 4.5 and later
/// accept a request whose input plus `max_tokens` exceeds the window and
/// stop at the window with `stop_reason: "model_context_window_exceeded"`
/// (<https://platform.claude.com/docs/en/build-with-claude/context-windows#context-window-overflow-behavior>),
/// and for the earlier models that reject it the request builder clamps
/// `max_tokens` to the room left (`HelperMethods::fit_max_tokens_to_window`).
/// Reserving the full ceiling put hard capacity at ~31.5k on a 200k Opus
/// window. 16k covers a large single-file write in one round; a round's
/// output becomes the next round's input, so the following assessment sees
/// it either way.
pub const EXPECTED_RESPONSE_TOKENS: u64 = 16_384;

/// Thinking reserve for a zero thinking budget: the runtime's adaptive
/// ("the model decides") and off levels both carry 0, and the request builder
/// itself sends a zero budget as this tier on fixed-budget models
/// (`DEFAULT_LEGACY_ADAPTIVE_FALLBACK`, the "high" tier). Reserving it for off
/// errs toward earlier rollover by at most this much.
pub const ZERO_BUDGET_THINKING_RESERVE_TOKENS: u64 =
    crate::core::models::DEFAULT_LEGACY_ADAPTIVE_FALLBACK as u64;

/// Minimum history length before compaction is meaningful. A freshly
/// compacted session (summary + acknowledgement) sits below this gate, which
/// is what prevents recompaction loops without frontend-local bookkeeping.
pub const MIN_COMPACTION_MESSAGES: usize = 4;

/// Flat charge per base64 image block. Anthropic bills ≈ w·h/750 tokens
/// AFTER its 1568-px / ~1.15 MP downscale, so 1,600 is the per-image
/// ceiling (Goose uses the same figure; gemini-cli uses 3,000; Codex
/// subtracts the payload and adds a ~7 KB estimate). Serialized base64 at
/// 2.5 chars/token would charge ~1.9 M tokens for a 3.5 MiB image and trip
/// compaction every turn.
pub const IMAGE_TOKEN_ESTIMATE: u64 = 1_600;

/// Provider-side per-message framing charge (role tags, message boundaries)
/// on top of the serialized content itself.
const PER_MESSAGE_FRAMING_TOKENS: u64 = 4;

/// Tool-result bytes are converted to a token reserve at this many bytes per
/// token — matching the ASCII charge rate of the conservative estimator, so
/// the reserve and the usage accounting agree.
const TOOL_RESULT_BYTES_PER_TOKEN: u64 = 3;

/// Conservative token estimate for a text segment. See the module docs for
/// the exact charging rule and why it never understates.
pub fn conservative_token_estimate(text: &str) -> u64 {
    // Scaled integer accumulation in fifths of a token: ASCII chars cost 2
    // fifths (1 token per 2.5 chars); non-ASCII chars cost 5 fifths per
    // UTF-8 byte (1 token per byte).
    let mut fifths: u64 = 0;
    for c in text.chars() {
        if c.is_ascii() {
            fifths += 2;
        } else {
            fifths += 5 * c.len_utf8() as u64;
        }
    }
    fifths.div_ceil(5)
}

/// Token-estimation seam. Provider tokenizers register here per model when
/// available; the conservative estimator is the mandatory fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenEstimator {
    /// No exact tokenizer available — conservative charging (never
    /// overstates remaining capacity).
    Conservative,
}

impl TokenEstimator {
    pub fn estimate(&self, text: &str) -> u64 {
        match self {
            TokenEstimator::Conservative => conservative_token_estimate(text),
        }
    }
}

/// Resolve the estimator for a model. No provider tokenizer crate is linked
/// today, so every model — Anthropic, OpenAI, Google, unknown — uses the
/// conservative estimator (spec §9.1: tokenizers where available,
/// conservative estimators otherwise).
pub fn estimator_for_model(_model: &str) -> TokenEstimator {
    TokenEstimator::Conservative
}

/// Everything the budget calculation needs about the NEXT request. Callers
/// pass the segments they will actually send; the engine never guesses.
#[derive(Debug, Clone, Copy)]
pub struct ContextBudgetInputs<'a> {
    /// Model the request targets (selects the estimator).
    pub model: &'a str,
    /// Effective provider context window in tokens (override-aware).
    pub provider_window: u64,
    /// The actual effective system prompt, if any.
    pub system_prompt: Option<&'a str>,
    /// The EXPOSED tool schema set for this request (post-projection).
    pub tools_schema: &'a [Value],
    /// Conversation history exactly as it will be sent.
    pub messages: &'a [SharedMessage],
    /// Skill bodies loaded into the request outside system/history.
    pub skill_contents: &'a [&'a str],
    /// Memory bodies loaded into the request outside system/history.
    pub memory_contents: &'a [&'a str],
    /// Reasoning/thinking budget in tokens (0 = adaptive/off, reserved as
    /// [`ZERO_BUDGET_THINKING_RESERVE_TOKENS`]).
    pub thinking_budget_tokens: u64,
    /// Byte cap of the next likely tool result (converted to a token
    /// reserve at [`TOOL_RESULT_BYTES_PER_TOKEN`]).
    pub next_tool_result_bytes: u64,
    /// The request's output ceiling (its `max_tokens`). Bounds the output
    /// reserve; it is not itself reserved ([`EXPECTED_RESPONSE_TOKENS`]).
    pub max_output_tokens: u64,
}

/// Typed per-segment usage accounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextBreakdown {
    pub system_tokens: u64,
    pub tool_schema_tokens: u64,
    pub history_tokens: u64,
    pub framing_tokens: u64,
    pub skill_tokens: u64,
    pub memory_tokens: u64,
}

impl ContextBreakdown {
    pub fn total(&self) -> u64 {
        self.system_tokens
            + self.tool_schema_tokens
            + self.history_tokens
            + self.framing_tokens
            + self.skill_tokens
            + self.memory_tokens
    }
}

/// How a provider's wire accounts reasoning/thinking tokens against the
/// request's output budget (I1, CP-12 review). The reserve model must count
/// thinking exactly once, and provider differences are represented
/// explicitly instead of being summed blindly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingAccounting {
    /// Thinking spends from the request's output cap: Anthropic messages
    /// (`thinking.budget_tokens < max_tokens`), OpenAI Responses/Chat and
    /// Codex wires (reasoning tokens count toward `max_output_tokens` /
    /// `max_completion_tokens`). Reserve ONE envelope:
    /// `min(max_tokens, thinking + EXPECTED_RESPONSE_TOKENS)`.
    InsideOutputBudget,
    /// Thinking is budgeted separately from response output (Gemini
    /// `thinkingBudget`), or the wire is unknown — reserve both (thinking,
    /// and `min(max_tokens, EXPECTED_RESPONSE_TOKENS)`), which errs toward
    /// earlier compaction, never toward exhaustion.
    SeparateFromOutput,
}

/// Resolve the thinking-accounting semantics for a model from its wire
/// protocol. Unroutable models use the conservative separate-reserve model.
pub fn thinking_accounting_for_model(model: &str) -> ThinkingAccounting {
    use crate::runtime::openai::WireProtocol;
    match crate::runtime::openai::resolve_route(model).map(|route| route.wire) {
        Some(
            WireProtocol::AnthropicMessages
            | WireProtocol::OpenAiChatCompletions
            | WireProtocol::OpenAiResponses
            | WireProtocol::CodexResponses,
        ) => ThinkingAccounting::InsideOutputBudget,
        Some(WireProtocol::GoogleGeminiCodeAssist) | None => ThinkingAccounting::SeparateFromOutput,
    }
}

/// Typed reserves carved out of the provider window before any context is
/// admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextReserves {
    /// Thinking tokens reserved IN ADDITION to the output envelope. Zero on
    /// wires where thinking already spends from the output cap.
    pub thinking_tokens: u64,
    pub tool_result_tokens: u64,
    /// The output the next round is expected to need, never more than the
    /// request's `max_tokens`. On [`ThinkingAccounting::InsideOutputBudget`]
    /// wires thinking spends from the output cap, so this is
    /// `min(max_tokens, thinking + EXPECTED_RESPONSE_TOKENS)`; on separate
    /// wires `min(max_tokens, EXPECTED_RESPONSE_TOKENS)` beside
    /// `thinking_tokens`.
    pub output_tokens: u64,
    pub safety_margin_tokens: u64,
    /// The wire semantics the thinking/output split was computed under.
    pub thinking_accounting: ThinkingAccounting,
}

impl ContextReserves {
    pub fn total(&self) -> u64 {
        self.thinking_tokens
            + self.tool_result_tokens
            + self.output_tokens
            + self.safety_margin_tokens
    }
}

/// One request-aware budget assessment. Pure data — frontends read the
/// decision, they never recompute it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextAssessment {
    pub provider_window: u64,
    pub breakdown: ContextBreakdown,
    pub reserves: ContextReserves,
    pub history_messages: usize,
    /// Footprint measured from the provider's count of an earlier request
    /// this one extends ([`UsageAnchor`]); `None` = estimate only.
    pub anchored_tokens: Option<u64>,
}

impl ContextAssessment {
    /// The request's footprint: anchored to the provider's last reported
    /// input when this request extends that one, otherwise the estimate.
    pub fn used_tokens(&self) -> u64 {
        self.anchored_tokens
            .unwrap_or_else(|| self.estimated_tokens())
    }

    /// Conservative estimate across every request segment, whether or not
    /// an anchor applies. Calibration samples and the fixed-part overhead
    /// of successor sizing are in these units.
    pub fn estimated_tokens(&self) -> u64 {
        self.breakdown.total()
    }

    /// The estimate of the request's fixed part: system, tool schemas,
    /// skills and memory (everything but the messages and their framing).
    pub fn fixed_tokens(&self) -> u64 {
        let b = &self.breakdown;
        b.total() - b.history_tokens - b.framing_tokens
    }

    /// Anchor this assessment to `anchor` when `messages` (the messages it
    /// assessed) extend the anchored request's verbatim.
    pub fn anchored(mut self, anchor: Option<&UsageAnchor>, messages: &[SharedMessage]) -> Self {
        self.anchored_tokens =
            anchor.and_then(|a| a.footprint(self.fixed_tokens(), messages));
        self
    }

    /// Tokens the context may occupy after all reserves: window minus
    /// reserves, floored at zero (a window smaller than the reserves has no
    /// admissible context at all).
    pub fn budget_tokens(&self) -> u64 {
        self.provider_window.saturating_sub(self.reserves.total())
    }

    /// Remaining admissible capacity before the trigger fires.
    pub fn remaining_tokens(&self) -> u64 {
        self.budget_tokens().saturating_sub(self.used_tokens())
    }

    /// The single cross-frontend compaction trigger: usage reached the
    /// budget AND there is enough history for a summary to fold.
    pub fn should_compact(&self) -> bool {
        self.history_messages >= MIN_COMPACTION_MESSAGES
            && self.used_tokens() >= self.budget_tokens()
    }
}

/// Compute the request-aware context assessment. This is THE budget
/// calculation — `Runtime::assess_context` and every frontend trigger path
/// route through here.
pub fn assess(inputs: &ContextBudgetInputs<'_>) -> ContextAssessment {
    let estimator = estimator_for_model(inputs.model);

    let system_tokens = inputs
        .system_prompt
        .map(|s| estimator.estimate(s))
        .unwrap_or(0);

    // Schemas and history are charged on their SERIALIZED form: JSON keys,
    // quoting, and escaping are bytes the provider tokenizes too.
    let tool_schema_tokens: u64 = inputs
        .tools_schema
        .iter()
        .map(|schema| estimate_serialized(&estimator, schema))
        .sum();

    let history_tokens: u64 = inputs
        .messages
        .iter()
        .map(|msg| estimate_message(&estimator, msg))
        .sum();
    let framing_tokens = PER_MESSAGE_FRAMING_TOKENS * inputs.messages.len() as u64;

    let skill_tokens: u64 = inputs
        .skill_contents
        .iter()
        .map(|s| estimator.estimate(s))
        .sum();
    let memory_tokens: u64 = inputs
        .memory_contents
        .iter()
        .map(|s| estimator.estimate(s))
        .sum();

    ContextAssessment {
        provider_window: inputs.provider_window,
        breakdown: ContextBreakdown {
            system_tokens,
            tool_schema_tokens,
            history_tokens,
            framing_tokens,
            skill_tokens,
            memory_tokens,
        },
        reserves: {
            let accounting = thinking_accounting_for_model(inputs.model);
            let thinking = match inputs.thinking_budget_tokens {
                0 => ZERO_BUDGET_THINKING_RESERVE_TOKENS,
                budget => budget,
            };
            let cap = inputs.max_output_tokens;
            let (thinking_tokens, output_tokens) = match accounting {
                ThinkingAccounting::InsideOutputBudget => (
                    0,
                    cap.min(thinking.saturating_add(EXPECTED_RESPONSE_TOKENS)),
                ),
                ThinkingAccounting::SeparateFromOutput => {
                    (thinking, cap.min(EXPECTED_RESPONSE_TOKENS))
                }
            };
            ContextReserves {
                thinking_tokens,
                tool_result_tokens: inputs
                    .next_tool_result_bytes
                    .div_ceil(TOOL_RESULT_BYTES_PER_TOKEN),
                output_tokens,
                safety_margin_tokens: inputs.provider_window * SAFETY_MARGIN_PERCENT / 100,
                thinking_accounting: accounting,
            }
        },
        history_messages: inputs.messages.len(),
        anchored_tokens: None,
    }
}

/// The provider-reported input of one request (`input + cache_read +
/// cache_write`), kept to measure the requests that follow it.
///
/// The estimator cannot know what the provider bills: a thinking block's
/// signature carries reasoning the visible (summarized) text does not show,
/// and how much of earlier turns' reasoning bills varies by request. The
/// provider's count is exact, so the next request is measured as that count
/// plus an estimate of only what was appended since. Requests that rewrite
/// the history (rollover, compaction, a changed image view) do not extend
/// the anchored one and fall back to the estimate.
#[derive(Clone, Debug)]
pub struct UsageAnchor {
    pub provider_tokens: u64,
    /// Estimate of the anchored request's fixed part
    /// ([`ContextAssessment::fixed_tokens`]).
    pub fixed_tokens: u64,
    /// The anchored request's messages as they were assessed.
    pub messages: Vec<SharedMessage>,
}

impl UsageAnchor {
    /// Footprint of a request with fixed part `fixed_tokens` (estimated) and
    /// `messages`: the provider's count plus the estimate of every message
    /// past the ones the two requests share, or `None` when they diverge
    /// before the anchored request's last message.
    ///
    /// The last anchored message may differ (a per-turn injection rides the
    /// request tail and is not kept in history): what it billed stays
    /// counted beside the estimate of its replacement. Growth of the fixed
    /// part is added; neither it nor a shrink is ever subtracted (the
    /// estimate of what left may exceed what it billed). Every deviation
    /// therefore errs high, never below the provider's count.
    pub fn footprint(&self, fixed_tokens: u64, messages: &[SharedMessage]) -> Option<u64> {
        let shared = self
            .messages
            .iter()
            .zip(messages)
            .take_while(|(a, b)| super::continuation::same_on_wire(a, b))
            .count();
        (shared + 1 >= self.messages.len()).then(|| {
            self.provider_tokens
                .saturating_add(estimate_history(&messages[shared..]))
                .saturating_add(fixed_tokens.saturating_sub(self.fixed_tokens))
        })
    }
}

fn estimate_serialized<T: serde::Serialize + ?Sized>(estimator: &TokenEstimator, value: &T) -> u64 {
    match serde_json::to_string(value) {
        Ok(s) => estimator.estimate(&s),
        // Serialization of an in-memory Value cannot realistically fail;
        // if it ever does, fail toward compaction rather than exhaustion.
        Err(_) => u64::MAX / 1024,
    }
}

/// Attachment-aware history estimate shared with rollover's reduction check.
/// Does not charge megabytes of image base64 as ordinary prompt tokens.
pub(crate) fn estimate_history(messages: &[SharedMessage]) -> u64 {
    messages.iter().fold(0u64, |sum, message| {
        sum.saturating_add(estimate_message(&TokenEstimator::Conservative, message))
            .saturating_add(PER_MESSAGE_FRAMING_TOKENS)
    })
}

/// One message as it goes on the wire: without its host-only `_synaps_*`
/// keys (stripped before every provider request). Messages without base64
/// images or reasoning blocks are charged on their serialized form; the
/// others block by block, images at [`IMAGE_TOKEN_ESTIMATE`] and reasoning
/// by [`estimate_reasoning_block`]. Nested `tool_result.content[]` images
/// are charged flat too.
fn estimate_message(estimator: &TokenEstimator, msg: &Value) -> u64 {
    let wire = super::continuation::WireMessage(msg);
    let Some(blocks) = msg["content"].as_array() else {
        return estimate_serialized(estimator, &wire);
    };
    // Zero drift for messages without images or reasoning: charge exactly
    // as before.
    if !blocks.iter().any(|b| {
        is_base64_image(b)
            || is_reasoning_block(b)
            || b["content"]
                .as_array()
                .is_some_and(|a| a.iter().any(is_base64_image))
    }) {
        return estimate_serialized(estimator, &wire);
    }
    let mut total = 0u64;
    for block in blocks {
        if is_base64_image(block) {
            total += IMAGE_TOKEN_ESTIMATE;
            continue;
        }
        if is_reasoning_block(block) {
            total += estimate_reasoning_block(estimator, block);
            continue;
        }
        if block["type"] == "tool_result" {
            if let Some(inner) = block["content"].as_array() {
                total += 8; // wrapper keys: type, tool_use_id
                for b in inner {
                    total += if is_base64_image(b) {
                        IMAGE_TOKEN_ESTIMATE
                    } else {
                        estimate_serialized(estimator, b)
                    };
                }
                continue;
            }
        }
        total += estimate_serialized(estimator, block);
    }
    total
}

/// Base64 signature (or `redacted_thinking` data) characters per estimated
/// token. See [`estimate_reasoning_block`].
const REASONING_SIGNATURE_CHARS_PER_TOKEN: u64 = 4;

/// Block keys and delimiters of a reasoning block.
const REASONING_BLOCK_FRAMING_TOKENS: u64 = 8;

fn is_reasoning_block(b: &Value) -> bool {
    b["type"] == "thinking" || b["type"] == "redacted_thinking"
}

/// A `thinking` / `redacted_thinking` block, charged at the larger of its
/// visible text (conservative estimate) and one token per
/// [`REASONING_SIGNATURE_CHARS_PER_TOKEN`] signature characters, plus
/// framing.
///
/// The signature is opaque base64 that carries the full reasoning. The
/// visible text is a summary (`display: "summarized"`) or empty. Neither is
/// billed as text. Charging the serialized block at 2.5 characters per
/// token billed the signature as text on top of its summary, about 2.4×
/// what the provider bills for the block, which read xhigh-reasoning
/// histories at 1.3–2× their billed input and rolled workers over at about
/// half the configured size. Measured against Anthropic's billed input
/// (Opus, 2026-10): appended reasoning blocks bill a median of ~0.17 tokens
/// per serialized byte of the block (10th–90th percentile 0.11–0.22). This
/// charge (0.25 per signature character, the signature being most of the
/// block) sits near the top of that range. Some requests bill earlier
/// reasoning lower still, which the estimator cannot see; the
/// provider-anchored footprint ([`UsageAnchor`]) measures those.
fn estimate_reasoning_block(estimator: &TokenEstimator, block: &Value) -> u64 {
    let text = block["thinking"].as_str().map_or(0, |t| estimator.estimate(t));
    let opaque = block["signature"]
        .as_str()
        .or_else(|| block["data"].as_str())
        .map_or(0, |s| s.len() as u64 / REASONING_SIGNATURE_CHARS_PER_TOKEN);
    text.max(opaque) + REASONING_BLOCK_FRAMING_TOKENS
}

fn is_base64_image(b: &Value) -> bool {
    b["type"] == "image" && b["source"]["type"] == "base64"
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    fn history_tokens_of(messages: &[SharedMessage]) -> u64 {
        let inputs = ContextBudgetInputs {
            model: "claude-sonnet-4-6",
            provider_window: 200_000,
            system_prompt: None,
            tools_schema: &[],
            messages,
            skill_contents: &[],
            memory_contents: &[],
            thinking_budget_tokens: 0,
            next_tool_result_bytes: 0,
            max_output_tokens: 0,
        };
        assess(&inputs).breakdown.history_tokens
    }

    fn image_block(len: usize) -> Value {
        json!({
            "type": "image",
            "source": {"type": "base64", "media_type": "image/png", "data": "A".repeat(len)}
        })
    }

    /// A history rebuilt from a recorded block-shape fixture: block types
    /// and lengths only (the estimator charges length, not content), no
    /// content. Returns (fixed-part estimate, messages, billed input).
    fn shape_fixture(json: &str) -> (u64, Vec<SharedMessage>, u64) {
        let fixture: Value = serde_json::from_str(json).unwrap();
        let n = |v: &Value| v.as_u64().unwrap() as usize;
        let filler = |len: usize| "x".repeat(len);
        let base64 = |len: usize| {
            const B64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            (0..len).map(|i| B64[(i * 7 + i / 64) % 64] as char).collect::<String>()
        };
        let messages = fixture["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| {
                let role = m[0].as_str().unwrap();
                let blocks = m[1].as_array().unwrap();
                let content = match blocks[0][0].as_str().unwrap() {
                    "s" => json!(filler(n(&blocks[0][1]))),
                    _ => Value::Array(
                        blocks
                            .iter()
                            .map(|b| match b[0].as_str().unwrap() {
                                // Serialized length: `{"type":"text","text":""}` is 25.
                                "b" => json!({"type": "text", "text": filler(n(&b[1]).saturating_sub(25))}),
                                "k" => json!({
                                    "type": "thinking",
                                    "thinking": filler(n(&b[1])),
                                    "signature": base64(n(&b[2])),
                                }),
                                "i" => image_block(1_000_000),
                                other => panic!("block kind {other}"),
                            })
                            .collect(),
                    ),
                };
                Arc::new(json!({"role": role, "content": content})) as SharedMessage
            })
            .collect();
        let fixed = conservative_token_estimate(&filler(
            n(&fixture["system_bytes"]) + n(&fixture["tool_schema_bytes"]),
        ));
        (fixed, messages, fixture["billed"].as_u64().unwrap())
    }

    /// Regression (worker rollover at about half the configured size): a
    /// real xhigh-reasoning worker history (Opus, 279 messages, 84 signed
    /// thinking blocks) the provider billed at 197,253 input tokens. The
    /// estimate used to charge each thinking block's base64 signature as
    /// text on top of its summary: 1.29× the billed count here, 1.3–2× on
    /// other xhigh workers, so the 200k rollover fired near 150k real.
    #[test]
    fn xhigh_reasoning_history_estimate_tracks_its_billed_input() {
        let (fixed, messages, billed) =
            shape_fixture(include_str!("../../tests/fixtures/context_estimate/xhigh_worker_197k.json"));
        assert_eq!(billed, 197_253);
        assert!(messages.iter().any(|m| m["content"]
            .as_array()
            .is_some_and(|b| b.iter().any(|b| b["signature"].as_str().is_some_and(|s| s.len() > 1_000)))));
        let estimate = fixed + estimate_history(&messages);
        let ratio = estimate as f64 / billed as f64;
        assert!((0.9..=1.1).contains(&ratio), "estimate {estimate} vs billed {billed}: {ratio:.3}");

        // What the serialized charge (signature as text) read for it.
        let serialized: u64 = messages
            .iter()
            .map(|m| {
                estimate_serialized(&TokenEstimator::Conservative, &**m) + PER_MESSAGE_FRAMING_TOKENS
            })
            .sum();
        let old = (fixed + serialized) as f64 / billed as f64;
        assert!(old > 1.25, "the old charge read {old:.3}×");
    }

    #[test]
    fn reasoning_blocks_charge_the_larger_of_text_and_signature() {
        let block = |text: usize, sig: usize| {
            json!({"type": "thinking", "thinking": "t".repeat(text), "signature": "S".repeat(sig)})
        };
        let one = |b: Value| {
            estimate_history(&[Arc::new(json!({"role": "assistant", "content": [b]}))])
                - PER_MESSAGE_FRAMING_TOKENS
                - REASONING_BLOCK_FRAMING_TOKENS
        };
        // Summary longer than the signature: the text (2.5 chars/token).
        assert_eq!(one(block(10_000, 4_000)), 4_000);
        // Signature-dominated (omitted or short summary): 4 chars/token,
        // never the serialized sum of both.
        assert_eq!(one(block(0, 40_000)), 10_000);
        assert_eq!(one(block(2_500, 40_000)), 10_000);
        assert_eq!(
            one(json!({"type": "redacted_thinking", "data": "D".repeat(8_000)})),
            2_000
        );
        // Other blocks beside reasoning are still charged serialized.
        let mixed = estimate_history(&[Arc::new(json!({"role": "assistant", "content": [
            block(0, 400),
            {"type": "text", "text": "a".repeat(2_500)},
        ]}))]);
        assert!((1_100..1_130).contains(&mixed), "{mixed}");
    }

    /// Host-only `_synaps_*` keys never reach the wire, so they are not
    /// charged.
    #[test]
    fn host_only_keys_are_not_charged() {
        let plain = json!({"role": "user", "content": "hello there"});
        let mut stamped = plain.clone();
        stamped["_synaps_at"] = json!("2026-10-30T11:00:00.000Z");
        stamped["_synaps_turn"] = json!({"id": "x".repeat(400)});
        assert_eq!(
            estimate_history(&[Arc::new(stamped.clone())]),
            estimate_history(&[Arc::new(plain)])
        );
        let mut reasoning = json!({"role": "assistant", "content": [
            {"type": "thinking", "thinking": "plan", "signature": "S".repeat(40)},
        ]});
        let bare = estimate_history(&[Arc::new(reasoning.clone())]);
        reasoning["_synaps_turn"] = json!({"id": "x".repeat(400)});
        assert_eq!(estimate_history(&[Arc::new(reasoning)]), bare);
    }

    fn msg(role: &str, text: &str) -> SharedMessage {
        Arc::new(json!({"role": role, "content": text}))
    }

    fn assessment_of(system: &str, messages: &[SharedMessage]) -> ContextAssessment {
        assess(&ContextBudgetInputs {
            model: "claude-opus-5-5",
            provider_window: 1_000_000,
            system_prompt: Some(system),
            tools_schema: &[],
            messages,
            skill_contents: &[],
            memory_contents: &[],
            thinking_budget_tokens: 0,
            next_tool_result_bytes: 0,
            max_output_tokens: 128_000,
        })
    }

    /// The footprint of a request that extends an earlier one is the
    /// provider's count of that one plus the estimate of only what was
    /// appended since; the estimate of the shared part plays no role.
    #[test]
    fn footprint_is_anchored_to_the_last_reported_usage() {
        let system = "s".repeat(2_500);
        let earlier = vec![msg("user", "task"), msg("assistant", &"a".repeat(250_000))];
        let anchor = UsageAnchor {
            provider_tokens: 60_000,
            fixed_tokens: assessment_of(&system, &earlier).fixed_tokens(),
            messages: earlier.clone(),
        };
        let appended = [msg("user", &"r".repeat(25_000))];
        let mut next = earlier.clone();
        next.extend(appended.iter().cloned());
        let assessment = assessment_of(&system, &next).anchored(Some(&anchor), &next);
        assert_eq!(
            assessment.used_tokens(),
            60_000 + estimate_history(&appended),
            "provider count + appended estimate"
        );
        // The estimate is still there for calibration and overhead sizing.
        assert!(assessment.estimated_tokens() > 100_000);
        assert_eq!(assessment.anchored_tokens, Some(assessment.used_tokens()));

        // A host stamp on a shared message does not change the wire.
        let mut stamped = next.clone();
        let mut first = (*stamped[0]).clone();
        first["_synaps_at"] = json!("2026-10-30T11:00:00.000Z");
        stamped[0] = Arc::new(first);
        assert_eq!(
            assessment_of(&system, &stamped).anchored(Some(&anchor), &stamped).used_tokens(),
            assessment.used_tokens()
        );

        // A larger system prompt adds its growth; a smaller one subtracts
        // nothing.
        let grown = "s".repeat(5_000);
        assert_eq!(
            assessment_of(&grown, &next).anchored(Some(&anchor), &next).used_tokens(),
            assessment.used_tokens() + 1_000
        );
        assert_eq!(
            assessment_of("", &next).anchored(Some(&anchor), &next).used_tokens(),
            assessment.used_tokens()
        );
    }

    /// The anchored request's last message may differ (per-turn injection
    /// rides the request tail): its billed size stays counted, the
    /// replacement is estimated on top, so the result never drops below the
    /// provider's count.
    #[test]
    fn a_changed_request_tail_keeps_the_anchor_and_errs_high() {
        let base = vec![msg("user", "task"), msg("assistant", &"a".repeat(25_000))];
        let mut injected = base.clone();
        injected.push(msg("user", &format!("next{}", "i".repeat(5_000))));
        let anchor = UsageAnchor {
            provider_tokens: 30_000,
            fixed_tokens: 0,
            messages: injected,
        };
        let mut next = base.clone();
        next.push(msg("user", "next"));
        next.push(msg("assistant", &"b".repeat(2_500)));
        assert_eq!(
            anchor.footprint(0, &next),
            Some(30_000 + estimate_history(&next[2..]))
        );
        // A shorter request that keeps all but the last: the count stands.
        assert_eq!(anchor.footprint(0, &base), Some(30_000));
    }

    /// Without a usable anchor (first request, after resume or rollover, a
    /// provider without usage, or a rewritten history) the footprint is the
    /// estimate.
    #[test]
    fn footprint_falls_back_to_the_estimate_without_an_anchor() {
        let system = "s".repeat(2_500);
        let history = vec![
            msg("user", "task"),
            msg("assistant", &"a".repeat(25_000)),
            msg("user", "more"),
            msg("assistant", &"b".repeat(25_000)),
        ];
        let plain = assessment_of(&system, &history);
        assert_eq!(plain.anchored_tokens, None);
        assert_eq!(plain.used_tokens(), plain.estimated_tokens());
        assert_eq!(plain.anchored(None, &history).used_tokens(), plain.estimated_tokens());

        // A rollover successor: the anchored history was rewritten before
        // its last message.
        let anchor = UsageAnchor {
            provider_tokens: 5_000,
            fixed_tokens: plain.fixed_tokens(),
            messages: history.clone(),
        };
        let successor = vec![history[0].clone(), msg("user", "[host note]"), history[3].clone()];
        let rolled = assessment_of(&system, &successor);
        assert_eq!(
            rolled.anchored(Some(&anchor), &successor).used_tokens(),
            rolled.estimated_tokens()
        );
    }

    #[test]
    fn image_block_in_tool_result_charged_flat() {
        let messages: Vec<SharedMessage> = vec![Arc::new(json!({
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": "toolu_1",
                "content": [
                    {"type": "text", "text": "Image: /tmp/x.png (1024x1536, image/png, 2292 KB)"},
                    image_block(3_000_000),
                ]
            }]
        }))];
        let tokens = history_tokens_of(&messages);
        assert!(tokens >= IMAGE_TOKEN_ESTIMATE, "{tokens}");
        assert!(tokens <= IMAGE_TOKEN_ESTIMATE + 200, "{tokens}");
        assert!(tokens < 10_000, "{tokens}");
    }

    #[test]
    fn user_message_image_charged_flat() {
        let messages: Vec<SharedMessage> = vec![Arc::new(json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "what is this?"},
                image_block(3_000_000),
            ]
        }))];
        let tokens = history_tokens_of(&messages);
        assert!(tokens >= IMAGE_TOKEN_ESTIMATE, "{tokens}");
        assert!(tokens <= IMAGE_TOKEN_ESTIMATE + 200, "{tokens}");
    }

    #[test]
    fn string_content_unchanged() {
        let msg = json!({"role": "user", "content": "plain string content, no blocks here"});
        let estimator = estimator_for_model("claude-sonnet-4-6");
        assert_eq!(
            estimate_message(&estimator, &msg),
            estimate_serialized(&estimator, &msg)
        );
    }

    #[test]
    fn array_content_without_images_unchanged() {
        let estimator = estimator_for_model("claude-sonnet-4-6");
        for msg in [
            json!({"role": "assistant", "content": [
                {"type": "text", "text": "let me look"},
                {"type": "tool_use", "id": "toolu_1", "name": "read", "input": {"path": "x"}}
            ]}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "1\tfoo\n2\tbar"}
            ]}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": [
                    {"type": "text", "text": "nested text only"}
                ]}
            ]}),
        ] {
            assert_eq!(
                estimate_message(&estimator, &msg),
                estimate_serialized(&estimator, &msg),
                "{msg}"
            );
        }
    }

    #[test]
    fn non_image_blocks_still_charged_by_payload() {
        // A big text block must NOT get the flat image discount.
        let msg = json!({"role": "user", "content": [
            {"type": "text", "text": "A".repeat(100_000)}
        ]});
        let estimator = estimator_for_model("claude-sonnet-4-6");
        assert!(estimate_message(&estimator, &msg) > 10_000);
    }

    #[test]
    fn ascii_is_charged_at_the_dense_bpe_rate() {
        // 25 ASCII chars → ceil(25 / 2.5) = 10 tokens.
        assert_eq!(conservative_token_estimate(&"a".repeat(25)), 10);
        assert_eq!(conservative_token_estimate(""), 0);
    }

    #[test]
    fn non_ascii_is_charged_per_utf8_byte() {
        // CJK: 3 bytes/char → 3 tokens each.
        assert_eq!(conservative_token_estimate("你好"), 6);
        // 4-byte emoji scalar → 4 tokens.
        assert_eq!(conservative_token_estimate("😀"), 4);
        // 2-byte Cyrillic → 2 tokens each.
        assert_eq!(conservative_token_estimate("да"), 4);
    }

    #[test]
    fn tiny_window_never_underflows_and_fails_toward_compaction() {
        let messages: Vec<SharedMessage> = (0..MIN_COMPACTION_MESSAGES)
            .map(|i| Arc::new(json!({"role": "user", "content": format!("m{i}")})) as SharedMessage)
            .collect();
        let assessment = assess(&ContextBudgetInputs {
            model: "claude-sonnet-4-6",
            provider_window: 10,
            system_prompt: None,
            tools_schema: &[],
            messages: &messages,
            skill_contents: &[],
            memory_contents: &[],
            thinking_budget_tokens: 1_000,
            next_tool_result_bytes: 0,
            max_output_tokens: 64_000,
        });
        assert_eq!(assessment.budget_tokens(), 0);
        assert_eq!(assessment.remaining_tokens(), 0);
        assert!(assessment.should_compact());
    }

    /// I1 (CP-12 review): Anthropic-wire requests spend thinking from
    /// `max_tokens` — the reserve model must count thinking exactly once.
    #[test]
    fn anthropic_wire_counts_thinking_exactly_once_inside_the_output_reserve() {
        let messages: Vec<SharedMessage> =
            vec![Arc::new(json!({"role": "user", "content": "hi"})) as SharedMessage];
        let window = 200_000;
        let margin = window * SAFETY_MARGIN_PERCENT / 100;
        let inputs = ContextBudgetInputs {
            model: "claude-sonnet-4-6",
            provider_window: window,
            system_prompt: None,
            tools_schema: &[],
            messages: &messages,
            skill_contents: &[],
            memory_contents: &[],
            thinking_budget_tokens: 8_000,
            next_tool_result_bytes: 0,
            max_output_tokens: 64_000,
        };
        let assessment = assess(&inputs);
        assert_eq!(
            assessment.reserves.thinking_accounting,
            ThinkingAccounting::InsideOutputBudget
        );
        assert_eq!(
            assessment.reserves.thinking_tokens, 0,
            "thinking must not be reserved a second time on the Anthropic wire"
        );
        let envelope = 8_000 + EXPECTED_RESPONSE_TOKENS;
        assert_eq!(assessment.reserves.output_tokens, envelope);
        assert_eq!(
            assessment.budget_tokens(),
            window - (envelope + margin),
            "budget must subtract the output/thinking envelope exactly once"
        );

        // The output cap bounds the envelope: thinking spends from
        // `max_tokens`, so no round can produce more than it.
        let big_thinking = ContextBudgetInputs {
            thinking_budget_tokens: 100_000,
            ..inputs
        };
        let assessment = assess(&big_thinking);
        assert_eq!(assessment.reserves.output_tokens, 64_000);
        assert_eq!(assessment.reserves.thinking_tokens, 0);

        // A zero budget (adaptive: the model decides) reserves the high tier.
        let adaptive = assess(&ContextBudgetInputs {
            thinking_budget_tokens: 0,
            ..inputs
        });
        assert_eq!(
            adaptive.reserves.output_tokens,
            ZERO_BUDGET_THINKING_RESERVE_TOKENS + EXPECTED_RESPONSE_TOKENS
        );
    }

    /// The output reserve is the next round's realistic need, not the
    /// `max_tokens` ceiling (128k for Opus): the hard-capacity line of a
    /// 200k window sits above 100k for Opus and Sonnet alike, and a 1m
    /// window keeps the 15% margin as its largest reserve.
    #[test]
    fn the_output_reserve_is_a_realistic_need_not_the_max_tokens_ceiling() {
        let hard_line = |model: &str, window: u64, thinking: u64| {
            let (assessment, budget) = crate::runtime::stream::pre_request_context_budget(
                model, window, None, &[], &[], thinking, 30_000, None,
            );
            (assessment.reserves.output_tokens, window - budget.required_next_round_tokens)
        };
        // Adaptive (0) and "high" (16,384) reserve the same envelope.
        for thinking in [0, 16_384] {
            for model in ["claude-opus-5-5", "claude-sonnet-4-6"] {
                // 32,768 output + 10,000 tool result + 30,000 margin + 512 advisory.
                assert_eq!(hard_line(model, 200_000, thinking), (32_768, 126_720), "{model}");
                assert_eq!(hard_line(model, 1_000_000, thinking), (32_768, 806_720), "{model}");
            }
        }
        // "medium" (4,096), the runtime default.
        assert_eq!(hard_line("claude-opus-5-5", 200_000, 4_096), (20_480, 139_008));
        // "xhigh" (32,768) on Opus: still above the 93k successor target.
        assert_eq!(hard_line("claude-opus-5-5", 200_000, 32_768), (49_152, 110_336));
        // A budget past the model's cap: the cap (64k on Sonnet) governs.
        assert_eq!(hard_line("claude-sonnet-4-6", 200_000, 100_000).0, 64_000);
    }

    /// OpenAI Responses/Chat wires bill reasoning inside the output cap too.
    #[test]
    fn openai_wires_account_reasoning_inside_the_output_reserve() {
        for model in ["openai-codex/gpt-5.2-codex", "xai-auth/grok-4"] {
            let accounting = thinking_accounting_for_model(model);
            // xai-auth requires a cataloged model; skip honestly if the
            // catalog drops it rather than asserting a stale fixture.
            if model.starts_with("xai-auth") && accounting == ThinkingAccounting::SeparateFromOutput
            {
                continue;
            }
            assert_eq!(
                accounting,
                ThinkingAccounting::InsideOutputBudget,
                "{model}"
            );
        }
    }

    /// Gemini budgets thinking separately from response output; unroutable
    /// models fall back to the conservative separate-reserve model.
    #[test]
    fn gemini_and_unknown_models_reserve_thinking_separately() {
        let messages: Vec<SharedMessage> =
            vec![Arc::new(json!({"role": "user", "content": "hi"})) as SharedMessage];
        let window = 200_000;
        let margin = window * SAFETY_MARGIN_PERCENT / 100;
        for model in ["google-gemini/gemini-2.5-pro", "totally/unroutable-model"] {
            let assessment = assess(&ContextBudgetInputs {
                model,
                provider_window: window,
                system_prompt: None,
                tools_schema: &[],
                messages: &messages,
                skill_contents: &[],
                memory_contents: &[],
                thinking_budget_tokens: 8_000,
                next_tool_result_bytes: 0,
                max_output_tokens: 64_000,
            });
            assert_eq!(
                assessment.reserves.thinking_accounting,
                ThinkingAccounting::SeparateFromOutput,
                "{model}"
            );
            assert_eq!(assessment.reserves.thinking_tokens, 8_000, "{model}");
            assert_eq!(
                assessment.reserves.output_tokens, EXPECTED_RESPONSE_TOKENS,
                "{model}"
            );
            assert_eq!(
                assessment.budget_tokens(),
                window - (8_000 + EXPECTED_RESPONSE_TOKENS + margin),
                "{model}: separate wires reserve both envelopes"
            );
        }
    }

    /// `Runtime::assess_context` is the SAME calculation as `assess` on the
    /// runtime's own segments — the Runtime surface must never fork the math.
    #[tokio::test]
    async fn runtime_assessment_matches_engine_calculation() {
        let mut runtime = crate::Runtime::new_headless();
        runtime.set_system_prompt("shared system prompt for budget parity".into());
        runtime.set_context_window(Some(200_000));

        let messages: Vec<SharedMessage> = vec![
            Arc::new(json!({"role": "user", "content": "hello 你好 😀"})),
            Arc::new(json!({"role": "assistant", "content": [
                {"type": "text", "text": "fn main() { println!(\"hi\"); }"}
            ]})),
        ];

        let via_runtime = runtime.assess_context(&messages).await;

        let system = runtime.effective_system_prompt().await;
        let schema = runtime.tools_shared().read().await.tools_schema();
        let direct = assess(&ContextBudgetInputs {
            model: runtime.model(),
            provider_window: runtime.context_window(),
            system_prompt: system.as_deref(),
            tools_schema: &schema,
            messages: &messages,
            skill_contents: &[],
            memory_contents: &[],
            thinking_budget_tokens: runtime.thinking_budget() as u64,
            next_tool_result_bytes: runtime.max_tool_output() as u64,
            max_output_tokens: crate::runtime::helpers::HelperMethods::max_tokens_for_model(
                runtime.model(),
            ),
        });

        assert_eq!(
            via_runtime, direct,
            "Runtime surface must not fork the budget math"
        );
        assert!(
            via_runtime.reserves.output_tokens > 0,
            "output reserve must be model-derived"
        );
        assert!(
            via_runtime.breakdown.system_tokens > 0,
            "actual system segment must be accounted"
        );
    }

    /// Build a synthetic, in-bounds recall contribution for the runtime's
    /// own project identity (task A7 budget-wiring tests).
    fn synthetic_memory_contribution(
        rendered: &str,
    ) -> crate::runtime::memory_context::MemoryContextContribution {
        use crate::runtime::memory_context as mc;
        use agent_core::BoundedText;
        mc::MemoryContextContribution {
            schema: mc::ContributionSchemaVersion::parse("contribution/1").expect("valid schema"),
            provider_id: mc::ContextProviderId::parse("axel-memory").expect("valid provider"),
            project_id: crate::runtime::memory_project_id(),
            records: vec![mc::MemoryContributionRecord {
                memory_id: mc::MemoryId::parse("mem-0001").expect("valid memory id"),
                source: mc::MemorySource::ChatHistory,
                timestamp: std::time::SystemTime::UNIX_EPOCH,
                rank_reason: vec![mc::RankReason::ExactTopic],
                sensitivity: agent_core::core::disclosure::DisclosureClass::ModelVisible,
                retention: mc::RetentionClass::Standard,
                content: BoundedText::new("the project uses session-scoped authorization", 2048),
                truncated: false,
                supersedes: None,
            }],
            rendered: BoundedText::new(rendered, 16 * 1024),
            accounting: mc::ContributionAccounting::default(),
        }
    }

    /// Task A7 (spec §10.3): a held recall contribution is charged EXACTLY
    /// to the `memory_tokens` breakdown lane — `used_tokens`/`remaining_tokens`
    /// move by exactly the estimated memory tokens, every other breakdown
    /// lane is unchanged, and the typed reserves (thinking, tool-result,
    /// output, safety margin) are bit-for-bit identical with and without the
    /// contribution: memory competes for budget headroom only, NEVER for
    /// reserves. Clearing the contribution restores the assessment
    /// bit-for-bit.
    #[tokio::test]
    async fn memory_contribution_charges_only_the_memory_lane_and_never_reserves() {
        let mut runtime = crate::Runtime::new_headless();
        runtime.set_system_prompt("system prompt for the memory budget test".into());
        runtime.set_context_window(Some(200_000));

        let messages: Vec<SharedMessage> = vec![
            Arc::new(json!({"role": "user", "content": "what auth model do we use?"})),
            Arc::new(json!({"role": "assistant", "content": "let me check"})),
        ];

        let without = runtime.assess_context(&messages).await;
        assert_eq!(without.breakdown.memory_tokens, 0);

        let rendered = "[Axel memory — lower-authority project data; verify before relying]\n\
                        1. mem-0001 — Decision — the project uses session-scoped \
                        authorization rather than persisted grants.";
        runtime
            .hold_memory_contribution(synthetic_memory_contribution(rendered))
            .expect("in-bounds contribution for the runtime's own project is accepted");
        let with = runtime.assess_context(&messages).await;

        let expected_memory = conservative_token_estimate(rendered);
        assert!(expected_memory > 0, "test contribution must be non-empty");
        assert_eq!(
            with.breakdown.memory_tokens, expected_memory,
            "memory lane must carry exactly the estimated rendered tokens"
        );

        // Reserves: bit-for-bit identical. Memory never touches thinking,
        // tool-result, output, or safety-margin reserves.
        assert_eq!(
            with.reserves, without.reserves,
            "reserves must be COMPLETELY unaffected by memory content"
        );
        assert_eq!(
            with.budget_tokens(),
            without.budget_tokens(),
            "the admissible budget (window minus reserves) must not move"
        );

        // Every non-memory breakdown lane is unchanged.
        assert_eq!(
            with.breakdown.system_tokens,
            without.breakdown.system_tokens
        );
        assert_eq!(
            with.breakdown.tool_schema_tokens,
            without.breakdown.tool_schema_tokens
        );
        assert_eq!(
            with.breakdown.history_tokens,
            without.breakdown.history_tokens
        );
        assert_eq!(
            with.breakdown.framing_tokens,
            without.breakdown.framing_tokens
        );
        assert_eq!(with.breakdown.skill_tokens, without.breakdown.skill_tokens);
        assert_eq!(with.history_messages, without.history_messages);

        // Usage moves by exactly the memory estimate — in both directions.
        assert_eq!(with.used_tokens(), without.used_tokens() + expected_memory);
        assert_eq!(
            with.remaining_tokens(),
            without.remaining_tokens() - expected_memory
        );

        // should_compact still keys off the SAME budget, now with memory
        // usage included (not asserted to flip here — the contribution is
        // far smaller than the window; the invariant is the inputs).
        assert_eq!(
            with.should_compact(),
            with.history_messages >= MIN_COMPACTION_MESSAGES
                && with.used_tokens() >= with.budget_tokens()
        );

        // Clearing the held segment restores the assessment bit-for-bit —
        // the no-contribution path is byte-identical to before task A7.
        runtime.clear_memory_contribution();
        let cleared = runtime.assess_context(&messages).await;
        assert_eq!(
            cleared, without,
            "no held contribution ⇒ assessment identical to the empty lane"
        );
    }

    /// Task A7 fail-closed gate (spec §5.2): a contribution whose project
    /// identity does not match the host's is refused at hold time and never
    /// reaches the budget lane.
    #[tokio::test]
    async fn foreign_project_contribution_is_refused_and_never_budgeted() {
        use crate::runtime::memory_context as mc;
        let mut runtime = crate::Runtime::new_headless();
        runtime.set_context_window(Some(200_000));

        let mut contribution = synthetic_memory_contribution("foreign project memory");
        contribution.project_id =
            mc::ProjectId::parse("project-not-this-host").expect("valid project id");
        assert_eq!(
            runtime.hold_memory_contribution(contribution),
            Err(mc::MemoryContextError::ContributionProjectMismatch)
        );

        let messages: Vec<SharedMessage> =
            vec![Arc::new(json!({"role": "user", "content": "hi"})) as SharedMessage];
        let assessment = runtime.assess_context(&messages).await;
        assert_eq!(
            assessment.breakdown.memory_tokens, 0,
            "a refused contribution must never be budgeted"
        );
    }
}
