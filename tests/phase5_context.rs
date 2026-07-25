//! Phase 5 / Task 29 — centralized request-aware context budgeting (spec §9.1).
//!
//! Contract under test:
//!
//! 1. The engine owns ONE budget calculation built from actual system
//!    segments, exposed tool schemas, history + framing, loaded skills and
//!    memories, the thinking reserve, the next tool-result reserve, the
//!    requested output reserve, and the provider window with a documented
//!    safety margin of at least 10%.
//! 2. The token estimator is CONSERVATIVE: it never understates the token
//!    count of representative English, code, JSON, CJK, emoji, tool-heavy,
//!    and skill-heavy content — therefore it never overstates remaining
//!    capacity.
//! 3. Compaction triggers strictly before provider exhaustion on every
//!    fixture class, leaving at least the documented reserve.
//! 4. No frontend keeps local token math on the compaction trigger path.

use serde_json::{json, Value};
use synaps_cli::runtime::context::{
    assess, conservative_token_estimate, estimator_for_model, ContextBudgetInputs,
    MIN_COMPACTION_MESSAGES, SAFETY_MARGIN_PERCENT,
};
use synaps_cli::SharedMessage;

// ─── fixture corpus ──────────────────────────────────────────────────────────
//
// Each fixture pairs representative content with an honest REFERENCE token
// count — an upper-bound estimate of what a real provider tokenizer would
// produce for that content class (rates from published BPE measurements).
// The conservative estimator must always land AT or ABOVE the reference.

struct Fixture {
    name: &'static str,
    text: String,
    /// Honest provider-tokenizer upper-bound for `text`.
    reference_tokens: u64,
}

fn english_fixture() -> Fixture {
    let para = "The request lifecycle hardening program centralizes context \
                budgeting inside the engine so that every frontend shares one \
                calculation and compaction always triggers before the provider \
                window is exhausted, with a documented safety margin. ";
    let text = para.repeat(12);
    // English prose: ~4 chars per token.
    let reference_tokens = (text.chars().count() as u64).div_ceil(4);
    Fixture {
        name: "english",
        text,
        reference_tokens,
    }
}

fn code_fixture() -> Fixture {
    let snippet = r#"
pub fn assess_context(window: u64, used: u64, reserves: u64) -> Assessment {
    let budget = window.saturating_sub(reserves);
    Assessment {
        remaining: budget.saturating_sub(used),
        should_compact: used >= budget,
    }
}
"#;
    let text = snippet.repeat(20);
    // Source code: ~3.5 chars per token (identifiers merge, punctuation splits).
    let reference_tokens = (text.chars().count() as u64 * 2).div_ceil(7);
    Fixture {
        name: "code",
        text,
        reference_tokens,
    }
}

fn json_fixture() -> Fixture {
    let obj = r#"{"tool":"memory_search","input":{"query":"budget","limit":8},"effect":{"class":"read","paths":["/tmp/a","/tmp/b"]},"ok":true,"count":42}"#;
    let text = obj.repeat(40);
    // Minified JSON: ~3 chars per token (quotes/braces tokenize separately).
    let reference_tokens = (text.chars().count() as u64).div_ceil(3);
    Fixture {
        name: "json",
        text,
        reference_tokens,
    }
}

fn cjk_fixture() -> Fixture {
    let para = "リクエストのライフサイクルを強化するプログラムは、コンテキスト\
                予算計算をエンジンに集中させます。上下文预算计算集中在引擎中，\
                所有前端共享同一个计算，压缩总是在提供者窗口耗尽之前触发。";
    let text = para.repeat(15);
    // CJK: worst realistic case ~1.5 tokens per character.
    let reference_tokens = (text.chars().count() as u64 * 3).div_ceil(2);
    Fixture {
        name: "cjk",
        text,
        reference_tokens,
    }
}

fn emoji_fixture() -> Fixture {
    let line = "status: ✅ deploy 🚀 review 👩‍💻 family 👨‍👩‍👧‍👦 flags 🇯🇵🇩🇪 tone 👍🏽 ";
    let text = line.repeat(30);
    // Emoji: ~3 tokens per 4-byte scalar (ZWJ sequences split per scalar);
    // ASCII filler at prose rate.
    let four_byte = text.chars().filter(|c| c.len_utf8() == 4).count() as u64;
    let three_byte = text.chars().filter(|c| c.len_utf8() == 3).count() as u64;
    let ascii = text.chars().filter(|c| c.is_ascii()).count() as u64;
    let reference_tokens = four_byte * 3 + three_byte * 2 + ascii.div_ceil(4);
    Fixture {
        name: "emoji",
        text,
        reference_tokens,
    }
}

fn tool_heavy_fixture() -> Fixture {
    // High-entropy tool output (hashes, base64, dense paths) is the WORST
    // ASCII case for BPE: ~2.6 chars per token.
    let line = "sha256:9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08 \
                blob=QmFzZTY0IGZpeHR1cmUgZGF0YSBmb3IgdG9rZW4gYnVkZ2V0aW5nIHRlc3Rz \
                /home/user/.cache/synaps/telemetry/9f86d081/чанк.bin ";
    let text = line.repeat(25);
    let ascii = text.chars().filter(|c| c.is_ascii()).count() as u64;
    let non_ascii_bytes: u64 = text
        .chars()
        .filter(|c| !c.is_ascii())
        .map(|c| c.len_utf8() as u64)
        .sum();
    let reference_tokens = (ascii * 5).div_ceil(13) + non_ascii_bytes;
    Fixture {
        name: "tool-heavy",
        text,
        reference_tokens,
    }
}

fn skill_heavy_fixture() -> Fixture {
    let body = r#"
# Skill: context-budgeting

Use when sizing a request against the provider window.

## Checklist
- [ ] account system segments and exposed schemas
- [ ] account history plus framing
- [ ] reserve thinking, tool-result, and output capacity

```rust
let budget = window - reserves; // keep >= 10% margin
```
"#;
    let text = body.repeat(18);
    // Markdown/prose/code mix: ~3.5 chars per token.
    let reference_tokens = (text.chars().count() as u64 * 2).div_ceil(7);
    Fixture {
        name: "skill-heavy",
        text,
        reference_tokens,
    }
}

fn all_fixtures() -> Vec<Fixture> {
    vec![
        english_fixture(),
        code_fixture(),
        json_fixture(),
        cjk_fixture(),
        emoji_fixture(),
        tool_heavy_fixture(),
        skill_heavy_fixture(),
    ]
}

// ─── shared inputs ───────────────────────────────────────────────────────────

const WINDOW: u64 = 200_000;
const THINKING_RESERVE: u64 = 8_000;
const OUTPUT_RESERVE: u64 = 64_000;
const TOOL_RESULT_BYTES: u64 = 50_000;

fn base_inputs<'a>(messages: &'a [SharedMessage], tools: &'a [Value]) -> ContextBudgetInputs<'a> {
    ContextBudgetInputs {
        model: "claude-sonnet-4-6",
        provider_window: WINDOW,
        system_prompt: None,
        tools_schema: tools,
        messages,
        skill_contents: &[],
        memory_contents: &[],
        thinking_budget_tokens: THINKING_RESERVE,
        next_tool_result_bytes: TOOL_RESULT_BYTES,
        output_reserve_tokens: OUTPUT_RESERVE,
    }
}

fn user_msg(text: &str) -> SharedMessage {
    std::sync::Arc::new(json!({"role": "user", "content": text}))
}

fn assistant_msg(text: &str) -> SharedMessage {
    std::sync::Arc::new(json!({"role": "assistant", "content": [{"type": "text", "text": text}]}))
}

// ─── 1. documented safety margin ─────────────────────────────────────────────

#[test]
fn documented_safety_margin_is_at_least_ten_percent() {
    assert!(
        (10..=15).contains(&SAFETY_MARGIN_PERCENT),
        "spec §9.1 requires a 10–15% pre-dispatch reserve; got {SAFETY_MARGIN_PERCENT}%"
    );

    // The margin must actually be carved out of the budget.
    let messages: Vec<SharedMessage> = vec![user_msg("hi")];
    let assessment = assess(&base_inputs(&messages, &[]));
    let margin = WINDOW * SAFETY_MARGIN_PERCENT / 100;
    assert!(
        assessment.budget_tokens() <= WINDOW - margin,
        "budget {} must exclude the {}-token safety margin",
        assessment.budget_tokens(),
        margin
    );
}

// ─── 2. conservative estimator per fixture class ─────────────────────────────

#[test]
fn estimator_never_understates_fixture_token_counts() {
    for fixture in all_fixtures() {
        let estimate = conservative_token_estimate(&fixture.text);
        assert!(
            estimate >= fixture.reference_tokens,
            "{}: estimate {} understates reference {} — remaining capacity \
             would be overstated",
            fixture.name,
            estimate,
            fixture.reference_tokens
        );
    }
}

#[test]
fn estimator_seam_is_conservative_for_every_known_model() {
    // Provider tokenizers plug in per model when available; until then every
    // model must resolve to the conservative estimator (spec §9.1).
    for model in [
        "claude-sonnet-4-6",
        "claude-opus-4-7",
        "gpt-5.2-codex",
        "gemini-3-pro",
        "unknown-model",
    ] {
        let estimator = estimator_for_model(model);
        for fixture in all_fixtures() {
            assert_eq!(
                estimator.estimate(&fixture.text),
                conservative_token_estimate(&fixture.text),
                "{model}/{}: estimator seam must be conservative until a real \
                 provider tokenizer is wired",
                fixture.name
            );
        }
    }
}

// ─── 3. trigger before exhaustion on every fixture class ─────────────────────

#[test]
fn compaction_triggers_before_provider_exhaustion_on_every_fixture_class() {
    for fixture in all_fixtures() {
        let chunk: String = fixture.text.chars().take(1_000).collect();
        let chunk_reference = fixture
            .reference_tokens
            .div_ceil(fixture.text.chars().count() as u64 / 1_000 + 1);

        let mut messages: Vec<SharedMessage> = Vec::new();
        let mut reference_history_tokens: u64 = 0;
        let mut triggered = false;
        let mut prev_remaining = u64::MAX;

        for round in 0..4_000 {
            messages.push(user_msg(&chunk));
            messages.push(assistant_msg(&chunk));
            reference_history_tokens += chunk_reference * 2;

            let assessment = assess(&base_inputs(&messages, &[]));
            assert!(
                assessment.remaining_tokens() <= prev_remaining,
                "{}: remaining capacity must shrink monotonically as history grows",
                fixture.name
            );
            prev_remaining = assessment.remaining_tokens();

            if assessment.should_compact() {
                triggered = true;
                assert!(
                    round > 0,
                    "{}: triggering on the very first exchange means the \
                     estimator is uselessly pessimistic",
                    fixture.name
                );
                break;
            }

            // INVARIANT: every state the trigger lets through must still fit
            // the provider window at honest tokenizer rates, with the hard
            // reserves (as the WIRE-SEMANTIC reserve model computes them —
            // thinking counted exactly once on inside-output wires) AND at
            // least a 10% window margin intact.
            let hard_reserves = assessment.reserves.thinking_tokens
                + assessment.reserves.output_tokens
                + assessment.reserves.tool_result_tokens;
            assert!(
                reference_history_tokens + hard_reserves + WINDOW / 10 <= WINDOW,
                "{}: non-triggering state at round {} would exhaust the \
                 provider window (reference {} tokens)",
                fixture.name,
                round,
                reference_history_tokens
            );
        }

        assert!(
            triggered,
            "{}: compaction never triggered before the loop bound",
            fixture.name
        );
    }
}

// ─── 4. every request segment counts toward usage ────────────────────────────

#[test]
fn exposed_tool_schemas_reduce_remaining_budget() {
    let messages: Vec<SharedMessage> = vec![user_msg("hello"), assistant_msg("hi")];
    let no_tools = assess(&base_inputs(&messages, &[]));

    let schemas: Vec<Value> = (0..24)
        .map(|i| {
            json!({
                "name": format!("tool_{i}"),
                "description": "A representative exposed tool schema with a \
                                realistic description body and typed input.",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "target path"},
                        "recursive": {"type": "boolean"}
                    },
                    "required": ["path"]
                }
            })
        })
        .collect();
    let with_tools = assess(&base_inputs(&messages, &schemas));

    assert!(
        with_tools.remaining_tokens() < no_tools.remaining_tokens(),
        "exposed schemas must consume budget: {} !< {}",
        with_tools.remaining_tokens(),
        no_tools.remaining_tokens()
    );
    assert!(with_tools.breakdown.tool_schema_tokens > 0);
}

#[test]
fn system_skill_and_memory_segments_count_toward_usage() {
    let messages: Vec<SharedMessage> = vec![user_msg("hello"), assistant_msg("hi")];
    let system = "You are a careful engineering agent. ".repeat(40);
    let skill = skill_heavy_fixture().text;
    let memory = "project uses worktrees for every implementation".repeat(10);

    let bare = assess(&base_inputs(&messages, &[]));

    let mut inputs = base_inputs(&messages, &[]);
    inputs.system_prompt = Some(&system);
    let skills = [skill.as_str()];
    let memories = [memory.as_str()];
    inputs.skill_contents = &skills;
    inputs.memory_contents = &memories;
    let loaded = assess(&inputs);

    assert!(loaded.breakdown.system_tokens > 0);
    assert!(loaded.breakdown.skill_tokens > 0);
    assert!(loaded.breakdown.memory_tokens > 0);
    assert!(
        loaded.remaining_tokens() < bare.remaining_tokens(),
        "system/skill/memory content must consume budget"
    );
    assert_eq!(
        loaded.used_tokens(),
        loaded.breakdown.system_tokens
            + loaded.breakdown.tool_schema_tokens
            + loaded.breakdown.history_tokens
            + loaded.breakdown.framing_tokens
            + loaded.breakdown.skill_tokens
            + loaded.breakdown.memory_tokens,
        "used_tokens must be exactly the sum of the typed breakdown"
    );
}

// ─── 5. degenerate windows never panic and fail toward compaction ────────────

#[test]
fn tiny_window_yields_zero_budget_and_triggers_with_min_history() {
    let messages: Vec<SharedMessage> = vec![
        user_msg("a"),
        assistant_msg("b"),
        user_msg("c"),
        assistant_msg("d"),
    ];
    let mut inputs = base_inputs(&messages, &[]);
    inputs.provider_window = 1_000; // far below the configured reserves
    let assessment = assess(&inputs);
    assert_eq!(assessment.budget_tokens(), 0, "reserves exceed the window");
    assert_eq!(assessment.remaining_tokens(), 0, "no underflow");
    assert!(
        assessment.should_compact(),
        "a window smaller than the reserves must fail toward compaction"
    );
}

#[test]
fn min_history_gate_prevents_recompaction_loops() {
    // A freshly compacted session (summary + ack) must not immediately
    // re-trigger, even when the summary itself is oversized.
    let huge = "x".repeat(2_000_000);
    let messages: Vec<SharedMessage> = vec![user_msg(&huge), assistant_msg("ready")];
    let assessment = assess(&base_inputs(&messages, &[]));
    assert!(messages.len() < MIN_COMPACTION_MESSAGES);
    assert!(
        !assessment.should_compact(),
        "fewer than {MIN_COMPACTION_MESSAGES} messages must never trigger \
         compaction (nothing meaningful to fold)"
    );
}

// The Runtime-surface parity proof (`Runtime::assess_context` IS the engine
// calculation, not a fork) lives in the engine unit tests next to
// `runtime::context`, because `Runtime::new_headless` is test-gated there.

// ─── 6. no frontend-local token math on the trigger path ─────────────────────

/// Spec §9.1 acceptance: all frontends consume the same engine budget
/// calculation — no per-frontend token estimation or threshold math may
/// remain on any compaction trigger path.
#[test]
fn frontend_trigger_paths_contain_no_local_token_math() {
    let root = env!("CARGO_MANIFEST_DIR");
    let read = |rel: &str| {
        std::fs::read_to_string(format!("{root}/{rel}"))
            .unwrap_or_else(|e| panic!("read {rel}: {e}"))
    };

    let chat = read("src/cmd/chat.rs");
    for forbidden in ["estimate_tokens", "compact_threshold", "80_000"] {
        assert!(
            !chat.contains(forbidden),
            "src/cmd/chat.rs still carries local trigger math ({forbidden})"
        );
    }
    assert!(
        chat.contains("assess_context"),
        "src/cmd/chat.rs must consume the engine context assessment"
    );

    // The legacy chars/4 estimator on the engine conversation state was the
    // source the frontend math forked from — it must be gone entirely.
    let conv_state = read("crates/agent-engine/src/engine/session.rs");
    assert!(
        !conv_state.contains("fn estimate_tokens"),
        "ConversationState::estimate_tokens (chars/4) must be removed in \
         favor of runtime::context"
    );

    // Manual-compaction frontends must not have grown their own trigger math.
    for rel in ["src/cmd/rpc.rs", "crates/agent-tui/src/tui/dispatch.rs"] {
        let src = read(rel);
        assert!(
            !src.contains("estimate_tokens"),
            "{rel} must not carry frontend-local token estimation"
        );
    }
}
