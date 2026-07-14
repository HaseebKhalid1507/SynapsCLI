//! Golden byte-identity gate for the Anthropic request body (#128 Slice 4).
//!
//! The prompt-cache key is the exact request-body prefix bytes — ANY change to
//! key order, whitespace, or optional-field presence invalidates cached
//! prefixes fleet-wide. This module freezes the legacy `json!`-assembled body
//! (`old_body_bytes`, a verbatim replica of the pre-slice-4 assembly in
//! api.rs/api_sync.rs) and asserts the borrowing `RequestBody` serializer
//! produces byte-identical output, both live (new == old) and against
//! committed fixtures (`tests/fixtures/golden_body/*.json`).
//!
//! Regenerate fixtures (only when the wire format INTENTIONALLY changes):
//!   GOLDEN_WRITE=1 cargo test -p synaps-engine golden_write
//!
//! Note: serde_json here has NO `preserve_order` feature — `Value` objects are
//! BTreeMap-backed, so the legacy body serialized keys alphabetically:
//!   max_tokens, messages, model, [output_config], [stream], [system], thinking, tools
//! `RequestBody`'s field declaration order reproduces exactly that.

use super::helpers::{cache_control_value, HelperMethods, MarkerSite};
use crate::core::config::CacheTtl;
use crate::SharedMessage;
use serde_json::{json, Value};
use std::sync::Arc;

/// Identity string expected when no user config has been loaded. Fixtures for
/// the oauth scenario embed this; if a prior test in this binary loaded a user
/// config with a custom identity, the file comparison is skipped (the live
/// old-vs-new comparison still runs and is the actual hard gate).
const EXPECTED_DEFAULT_IDENTITY: &str =
    "You are an AI assistant running in SynapsCLI, an open-source agent runtime.";

struct Scenario {
    name: &'static str,
    model: &'static str,
    thinking_budget: u32,
    /// Explicit reasoning level. Defaults to `Adaptive` for legacy scenarios
    /// so byte-identity with pre-Off-semantics fixtures is preserved.
    reasoning_level: agent_core::reasoning::ReasoningLevel,
    ttl: CacheTtl,
    tools: Vec<Value>,
    system_prompt: Option<String>,
    auth_type: &'static str,
    messages: Vec<SharedMessage>,
    stream: bool,
    identity_sensitive: bool,
}

fn two_tools() -> Vec<Value> {
    vec![
        json!({
            "name": "get_weather",
            "description": "Get current weather for a location.",
            "input_schema": {
                "type": "object",
                "properties": {"location": {"type": "string"}},
                "required": ["location"]
            }
        }),
        json!({
            "name": "read_file",
            "description": "Read a file from disk.",
            "input_schema": {
                "type": "object",
                "properties": {"path": {"type": "string"}, "limit": {"type": "integer"}},
                "required": ["path"]
            }
        }),
    ]
}

/// Plain history — string contents, trailing user string (annotate must
/// coerce it to blocks and stamp cache_control).
fn plain_history() -> Vec<SharedMessage> {
    vec![
        Arc::new(json!({"role": "user", "content": "What's the capital of France?"})),
        Arc::new(json!({"role": "assistant", "content": [{"type": "text", "text": "Paris."}]})),
        Arc::new(json!({"role": "user", "content": "And of Japan?"})),
    ]
}

/// Agentic history — thinking + tool_use + tool_result blocks; last message
/// already block-shaped (tool_result), so annotate stamps the existing block.
fn tool_history() -> Vec<SharedMessage> {
    vec![
        Arc::new(json!({"role": "user", "content": "What's the weather in Tokyo?"})),
        Arc::new(json!({"role": "assistant", "content": [
            {"type": "thinking", "thinking": "Need the weather tool.", "signature": "sig-abc123"},
            {"type": "text", "text": "Let me check."},
            {"type": "tool_use", "id": "toolu_01", "name": "get_weather", "input": {"location": "Tokyo"}}
        ]})),
        Arc::new(json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_01", "content": "22°C, clear skies"}
        ]})),
    ]
}

fn scenarios() -> Vec<Scenario> {
    let legacy = "claude-sonnet-4-5-20250929"; // enabled+budget_tokens path
    let adaptive = "claude-opus-4-7"; // adaptive path (128K max_tokens)
    vec![
        Scenario {
            name: "plain_no_tools_5m",
            model: legacy,
            thinking_budget: 16384,
            reasoning_level: agent_core::reasoning::ReasoningLevel::Adaptive,
            ttl: CacheTtl::FiveMinutes,
            tools: vec![],
            system_prompt: None,
            auth_type: "api_key",
            messages: plain_history(),
            stream: true,
            identity_sensitive: false,
        },
        Scenario {
            name: "tools_5m",
            model: legacy,
            thinking_budget: 16384,
            reasoning_level: agent_core::reasoning::ReasoningLevel::Adaptive,
            ttl: CacheTtl::FiveMinutes,
            tools: two_tools(),
            system_prompt: None,
            auth_type: "api_key",
            messages: tool_history(),
            stream: true,
            identity_sensitive: false,
        },
        Scenario {
            name: "tools_1h",
            model: legacy,
            thinking_budget: 16384,
            reasoning_level: agent_core::reasoning::ReasoningLevel::Adaptive,
            ttl: CacheTtl::OneHour,
            tools: two_tools(),
            system_prompt: None,
            auth_type: "api_key",
            messages: tool_history(),
            stream: true,
            identity_sensitive: false,
        },
        Scenario {
            name: "tools_hybrid",
            model: legacy,
            thinking_budget: 16384,
            reasoning_level: agent_core::reasoning::ReasoningLevel::Adaptive,
            ttl: CacheTtl::Hybrid,
            tools: two_tools(),
            system_prompt: None,
            auth_type: "api_key",
            messages: tool_history(),
            stream: true,
            identity_sensitive: false,
        },
        Scenario {
            name: "system_api_key_5m",
            model: legacy,
            thinking_budget: 16384,
            reasoning_level: agent_core::reasoning::ReasoningLevel::Adaptive,
            ttl: CacheTtl::FiveMinutes,
            tools: two_tools(),
            system_prompt: Some("You are a terse assistant.".into()),
            auth_type: "api_key",
            messages: plain_history(),
            stream: true,
            identity_sensitive: false,
        },
        Scenario {
            name: "system_oauth_hybrid",
            model: legacy,
            thinking_budget: 16384,
            reasoning_level: agent_core::reasoning::ReasoningLevel::Adaptive,
            ttl: CacheTtl::Hybrid,
            tools: two_tools(),
            system_prompt: Some("You are a terse assistant.".into()),
            auth_type: "oauth",
            messages: tool_history(),
            stream: true,
            identity_sensitive: true,
        },
        Scenario {
            name: "adaptive_no_effort",
            model: adaptive,
            thinking_budget: 0, // "adaptive" sentinel → no output_config
            reasoning_level: agent_core::reasoning::ReasoningLevel::Adaptive,
            ttl: CacheTtl::FiveMinutes,
            tools: two_tools(),
            system_prompt: None,
            auth_type: "api_key",
            messages: plain_history(),
            stream: true,
            identity_sensitive: false,
        },
        Scenario {
            name: "adaptive_effort_high",
            model: adaptive,
            thinking_budget: 16384, // "high" → output_config.effort present
            // Runtime invariant: budget 16384 always pairs with named High
            // (from_legacy_budget sync). The named level drives effort now.
            reasoning_level: agent_core::reasoning::ReasoningLevel::High,
            ttl: CacheTtl::FiveMinutes,
            tools: two_tools(),
            system_prompt: None,
            auth_type: "api_key",
            messages: plain_history(),
            stream: true,
            identity_sensitive: false,
        },
        Scenario {
            name: "legacy_adaptive_fallback",
            model: legacy,
            thinking_budget: 0, // sentinel leaks into legacy path → 16384 fallback
            reasoning_level: agent_core::reasoning::ReasoningLevel::Adaptive,
            ttl: CacheTtl::FiveMinutes,
            tools: vec![],
            system_prompt: None,
            auth_type: "api_key",
            messages: plain_history(),
            stream: true,
            identity_sensitive: false,
        },
        Scenario {
            name: "sync_no_stream_1h",
            model: legacy,
            thinking_budget: 16384,
            reasoning_level: agent_core::reasoning::ReasoningLevel::Adaptive,
            ttl: CacheTtl::OneHour,
            tools: two_tools(),
            system_prompt: Some("You are a terse assistant.".into()),
            auth_type: "api_key",
            messages: tool_history(),
            stream: false,
            identity_sensitive: false,
        },
        Scenario {
            name: "empty_history_no_tools_5m",
            model: legacy,
            thinking_budget: 16384,
            reasoning_level: agent_core::reasoning::ReasoningLevel::Adaptive,
            ttl: CacheTtl::FiveMinutes,
            tools: vec![],
            system_prompt: None,
            auth_type: "api_key",
            messages: vec![],
            stream: true,
            identity_sensitive: false,
        },
        // S241 gate-review Finding-3 armor: the four adaptive scenarios above
        // are all 5m/stream/no-system. TTL and adaptive-mode were argued
        // independent (shared cache_control_value helper) — these two combos
        // turn that argument into gated fact.
        Scenario {
            name: "adaptive_tools_1h",
            model: adaptive,
            thinking_budget: 16384,
            // Runtime invariant: budget 16384 always pairs with named High
            // (from_legacy_budget sync). The named level drives effort now.
            reasoning_level: agent_core::reasoning::ReasoningLevel::High,
            ttl: CacheTtl::OneHour,
            tools: two_tools(),
            system_prompt: None,
            auth_type: "api_key",
            messages: plain_history(),
            stream: true,
            identity_sensitive: false,
        },
        Scenario {
            name: "adaptive_sync_system_5m",
            model: adaptive,
            thinking_budget: 16384,
            // Runtime invariant: budget 16384 always pairs with named High
            // (from_legacy_budget sync). The named level drives effort now.
            reasoning_level: agent_core::reasoning::ReasoningLevel::High,
            ttl: CacheTtl::FiveMinutes,
            tools: vec![],
            system_prompt: Some("You review Rust code.".to_string()),
            auth_type: "api_key",
            messages: plain_history(),
            stream: false,
            identity_sensitive: false,
        },
        // Off semantics: thinking field omitted entirely.
        Scenario {
            name: "off_legacy_no_thinking_field",
            model: legacy,
            thinking_budget: 0,
            reasoning_level: agent_core::reasoning::ReasoningLevel::Off,
            ttl: CacheTtl::FiveMinutes,
            tools: vec![],
            system_prompt: None,
            auth_type: "api_key",
            messages: plain_history(),
            stream: true,
            identity_sensitive: false,
        },
    ]
}

/// Replica of the pre-slice-4 body assembly (api.rs:707-781 / api_sync.rs:80-118
/// at the time fixtures were generated). The *assembly* (json! shape, marker
/// probes, key layout) is verbatim and frozen on purpose — it is the legacy
/// truth the new serializer must reproduce byte-for-byte.
///
/// SCOPE HONESTY (S241 gate review): the sanitize/annotate pipeline below is
/// the CURRENT (slice-3, Arc-based) helper code, not the pre-slice-3 Vec code —
/// both sides of the old-vs-new comparison share it. This gate therefore guards
/// the slice-4 ASSEMBLY only; semantic drift inside the helpers themselves is
/// invisible here and is owned by the helpers.rs unit suite (see
/// `sanitize_leaves_missing_content_key_absent` for one such frozen divergence:
/// the old Vec-era code accidentally inserted `"content": null` on assistant
/// messages lacking a content key via IndexMut; the Arc port does not).
fn old_body_bytes(s: &Scenario) -> Vec<u8> {
    use agent_core::reasoning::ReasoningLevel;
    let mut cleaned_messages = s.messages.to_vec();
    HelperMethods::sanitize_thinking_blocks(&mut cleaned_messages);
    HelperMethods::annotate_cache_breakpoint(&mut cleaned_messages, s.ttl);

    let thinking_level = crate::core::models::thinking_level_for_budget(s.thinking_budget);

    let mut body = if s.reasoning_level == ReasoningLevel::Off {
        // Off: omit thinking field entirely.
        json!({
            "model": s.model,
            "max_tokens": HelperMethods::max_tokens_for_model(s.model),
            "messages": cleaned_messages,
            "tools": &s.tools,
            "stream": true,
        })
    } else {
        json!({
            "model": s.model,
            "max_tokens": HelperMethods::max_tokens_for_model(s.model),
            "messages": cleaned_messages,
            "tools": &s.tools,
            "stream": true,
            "thinking": if crate::core::models::model_supports_adaptive_thinking(s.model) {
                json!({ "type": "adaptive", "display": "summarized" })
            } else {
                let budget = if s.thinking_budget == 0 { crate::core::models::DEFAULT_LEGACY_ADAPTIVE_FALLBACK } else { s.thinking_budget };
                json!({ "type": "enabled", "budget_tokens": budget, "display": "summarized" })
            }
        })
    };

    // Sync transport (api_sync::call_api) never had a "stream" key — removing
    // from a BTreeMap-backed Value is byte-identical to never inserting it.
    if !s.stream {
        body.as_object_mut().unwrap().remove("stream");
    }

    if crate::core::models::model_supports_adaptive_thinking(s.model)
        && s.reasoning_level != agent_core::reasoning::ReasoningLevel::Off
    {
        if let Some(effort) = crate::core::models::effort_for_thinking_level(thinking_level) {
            body["output_config"] = json!({"effort": effort});
        }
    }

    // Inlined legacy HelperMethods::mark_last_tool (operated on the assembled body).
    if let Some(tool_list) = body["tools"].as_array_mut() {
        if let Some(last_tool) = tool_list.last_mut() {
            last_tool["cache_control"] = cache_control_value(s.ttl, MarkerSite::StablePrefix);
        }
    }

    if let Some(system) = HelperMethods::build_system_blocks(s.auth_type, &s.system_prompt, s.ttl) {
        body["system"] = system;
    }

    serde_json::to_vec(&body).expect("legacy body serialization")
}

/// New-path bytes via the borrowing `RequestBody` serializer.
fn new_body_bytes(s: &Scenario) -> (Vec<u8>, bool, bool) {
    let mut cleaned_messages = s.messages.to_vec();
    HelperMethods::sanitize_thinking_blocks(&mut cleaned_messages);
    HelperMethods::annotate_cache_breakpoint(&mut cleaned_messages, s.ttl);

    let body = super::request::RequestBody::new(
        s.model,
        &cleaned_messages,
        &s.tools,
        &s.system_prompt,
        s.auth_type,
        s.thinking_budget,
        s.reasoning_level,
        None,
        s.ttl,
        s.stream,
    );
    (
        serde_json::to_vec(&body).expect("new body serialization"),
        body.has_tool_marker(),
        body.has_system_marker(),
    )
}

fn fixture_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("golden_body")
}

fn identity_is_default() -> bool {
    crate::core::config::get_identity() == EXPECTED_DEFAULT_IDENTITY
}

/// Finding-2 armor (S241 gate review): if the runtime's DEFAULT_IDENTITY ever
/// drifts from the constant the oauth fixtures were generated with, this test
/// FAILS LOUDLY instead of letting `identity_is_default()` silently skip the
/// `system_oauth_hybrid` fixture comparison forever. Const-to-const compare —
/// no global state, immune to test-order config pollution. On failure: decide
/// consciously, regenerate fixtures (GOLDEN_WRITE=1), update both constants.
#[test]
fn default_identity_constant_has_not_drifted() {
    assert_eq!(
        crate::core::config::DEFAULT_IDENTITY,
        EXPECTED_DEFAULT_IDENTITY,
        "DEFAULT_IDENTITY drifted from the golden-fixture constant. The \
         system_oauth_hybrid fixture would silently stop being compared. \
         Regenerate fixtures (GOLDEN_WRITE=1) and update EXPECTED_DEFAULT_IDENTITY."
    );
}

/// Fixture generator — replica (legacy) code only. Run explicitly:
///   GOLDEN_WRITE=1 cargo test -p synaps-engine golden_write
#[test]
fn golden_write() {
    if std::env::var("GOLDEN_WRITE").as_deref() != Ok("1") {
        return;
    }
    assert!(
        identity_is_default(),
        "refusing to generate fixtures with a non-default identity loaded"
    );
    let dir = fixture_dir();
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    for s in scenarios() {
        let bytes = old_body_bytes(&s);
        std::fs::write(dir.join(format!("{}.json", s.name)), &bytes).expect("write fixture");
    }
}

/// THE GATE (#128 Slice 4): for every scenario, the borrowing serializer must
/// produce bytes identical to (1) the frozen legacy assembly, live, and
/// (2) the committed fixture file. Any divergence = prompt-cache invalidation.
#[test]
fn golden_gate_body_bytes_identical() {
    let dir = fixture_dir();
    let default_identity = identity_is_default();
    for s in scenarios() {
        let old = old_body_bytes(&s);
        let (new, has_tool_marker, has_system_marker) = new_body_bytes(&s);

        // Live gate: new serializer vs frozen legacy replica.
        assert_eq!(
            new,
            old,
            "scenario `{}`: RequestBody bytes diverge from legacy assembly\nold: {}\nnew: {}",
            s.name,
            String::from_utf8_lossy(&old),
            String::from_utf8_lossy(&new),
        );

        // Marker probes must match the legacy body-inspection semantics.
        let old_val: Value = serde_json::from_slice(&old).unwrap();
        assert_eq!(
            has_tool_marker,
            old_val["tools"].as_array().is_some_and(|t| !t.is_empty()),
            "scenario `{}`: has_tool_marker mismatch",
            s.name
        );
        assert_eq!(
            has_system_marker,
            old_val.get("system").is_some(),
            "scenario `{}`: has_system_marker mismatch",
            s.name
        );

        // Fixture gate: committed bytes (regression armor across releases).
        if s.identity_sensitive && !default_identity {
            eprintln!(
                "scenario `{}`: skipping FILE comparison (non-default identity loaded \
                 by another test); live old==new gate still enforced",
                s.name
            );
            continue;
        }
        let path = dir.join(format!("{}.json", s.name));
        let fixture = std::fs::read(&path)
            .unwrap_or_else(|e| panic!("missing fixture {} — {e}", path.display()));
        assert_eq!(
            new,
            fixture,
            "scenario `{}`: bytes diverge from committed fixture {}",
            s.name,
            path.display()
        );
    }
}
