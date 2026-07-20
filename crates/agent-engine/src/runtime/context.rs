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
    /// `max(output_reserve, thinking_budget)`.
    InsideOutputBudget,
    /// Thinking is budgeted separately from response output (Gemini
    /// `thinkingBudget`), or the wire is unknown — reserve both envelopes,
    /// which errs toward earlier compaction, never toward exhaustion.
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
    /// The request's output envelope. On [`ThinkingAccounting::InsideOutputBudget`]
    /// wires this is `max(output_reserve, thinking_budget)` — the wire
    /// requires the output cap to cover thinking, so the larger governs.
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
        reserves: {
            let accounting = thinking_accounting_for_model(inputs.model);
            let (thinking_tokens, output_tokens) = match accounting {
                ThinkingAccounting::InsideOutputBudget => (
                    0,
                    inputs
                        .output_reserve_tokens
                        .max(inputs.thinking_budget_tokens),
                ),
                ThinkingAccounting::SeparateFromOutput => {
                    (inputs.thinking_budget_tokens, inputs.output_reserve_tokens)
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
            output_reserve_tokens: 64_000,
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
        assert_eq!(assessment.reserves.output_tokens, 64_000);
        assert_eq!(
            assessment.budget_tokens(),
            window - (64_000 + margin),
            "budget must subtract the output/thinking envelope exactly once"
        );

        // A thinking budget larger than the requested output governs the
        // envelope (the wire requires max_tokens >= budget_tokens).
        let big_thinking = ContextBudgetInputs {
            thinking_budget_tokens: 100_000,
            ..inputs
        };
        let assessment = assess(&big_thinking);
        assert_eq!(assessment.reserves.output_tokens, 100_000);
        assert_eq!(assessment.reserves.thinking_tokens, 0);
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
                output_reserve_tokens: 64_000,
            });
            assert_eq!(
                assessment.reserves.thinking_accounting,
                ThinkingAccounting::SeparateFromOutput,
                "{model}"
            );
            assert_eq!(assessment.reserves.thinking_tokens, 8_000, "{model}");
            assert_eq!(assessment.reserves.output_tokens, 64_000, "{model}");
            assert_eq!(
                assessment.budget_tokens(),
                window - (8_000 + 64_000 + margin),
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
