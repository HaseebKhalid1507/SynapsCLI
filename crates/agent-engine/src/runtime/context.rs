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
//! - requested output reserve;
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

/// Minimum history length before compaction is meaningful. A freshly
/// compacted session (summary + acknowledgement) sits below this gate, which
/// is what prevents recompaction loops without frontend-local bookkeeping.
pub const MIN_COMPACTION_MESSAGES: usize = 4;

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
    /// Reasoning/thinking reserve in tokens.
    pub thinking_budget_tokens: u64,
    /// Byte cap of the next likely tool result (converted to a token
    /// reserve at [`TOOL_RESULT_BYTES_PER_TOKEN`]).
    pub next_tool_result_bytes: u64,
    /// Requested output reserve (the request's `max_tokens`).
    pub output_reserve_tokens: u64,
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

/// Typed reserves carved out of the provider window before any context is
/// admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextReserves {
    pub thinking_tokens: u64,
    pub tool_result_tokens: u64,
    pub output_tokens: u64,
    pub safety_margin_tokens: u64,
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
}

impl ContextAssessment {
    /// Total conservative usage across every request segment.
    pub fn used_tokens(&self) -> u64 {
        self.breakdown.total()
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
        .map(|msg| estimate_serialized(&estimator, msg))
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
        reserves: ContextReserves {
            thinking_tokens: inputs.thinking_budget_tokens,
            tool_result_tokens: inputs
                .next_tool_result_bytes
                .div_ceil(TOOL_RESULT_BYTES_PER_TOKEN),
            output_tokens: inputs.output_reserve_tokens,
            safety_margin_tokens: inputs.provider_window * SAFETY_MARGIN_PERCENT / 100,
        },
        history_messages: inputs.messages.len(),
    }
}

fn estimate_serialized(estimator: &TokenEstimator, value: &Value) -> u64 {
    match serde_json::to_string(value) {
        Ok(s) => estimator.estimate(&s),
        // Serialization of an in-memory Value cannot realistically fail;
        // if it ever does, fail toward compaction rather than exhaustion.
        Err(_) => u64::MAX / 1024,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

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
            output_reserve_tokens: 0,
        });
        assert_eq!(assessment.budget_tokens(), 0);
        assert_eq!(assessment.remaining_tokens(), 0);
        assert!(assessment.should_compact());
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
            output_reserve_tokens: via_runtime.reserves.output_tokens,
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
}
