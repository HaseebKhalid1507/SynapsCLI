//! Request-construction helpers for Anthropic API calls.
//!
//! Extracted from `api.rs`. Holds auth/beta header builders shared by the
//! streaming and non-streaming code paths (`ApiMethods` impl block), plus the
//! borrowing `RequestBody` serializer (#128 Slice 4) that replaced the
//! `json!`-assembled `Value` body.

use serde::ser::{Serialize, SerializeSeq, Serializer};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::RwLock;

use super::api::{ApiMethods, ApiOptions};
use super::helpers::{cache_control_value, HelperMethods, MarkerSite};
use super::types::AuthState;
use crate::core::config::CacheTtl;
use crate::SharedMessage;

/// Anthropic `/v1/messages` request body that BORROWS the message history
/// instead of deep-rebuilding it into a `serde_json::Value` tree (kills copy
/// C8 — one full history tree per API round).
///
/// BYTE-IDENTITY CONTRACT (prompt-cache keys depend on it): the legacy body
/// was a `json!` map — serde_json without `preserve_order` backs `Value`
/// objects with a BTreeMap, so keys serialized ALPHABETICALLY. Field
/// declaration order below reproduces exactly that order:
///   [diagnostics], max_tokens, messages, model, [output_config], [stream], [system], thinking, tools
/// Optional-field PRESENCE must also match: `diagnostics` only when
/// `cache_diagnostics` is on (it must stay FIRST — the fail-soft path strips
/// it as a byte prefix, see `cache_diagnosis::strip_diagnostics`), `output_config` only for adaptive
/// models with a mapped effort, `stream` only on the streaming transport,
/// `system` only when `build_system_blocks` returns Some. Guarded by
/// `runtime::body_golden` — do not reorder fields or change skip conditions
/// without regenerating fixtures (and accepting fleet-wide cache invalidation).
#[derive(serde::Serialize)]
pub(super) struct RequestBody<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    diagnostics: Option<super::cache_diagnosis::DiagnosticsParam>,
    max_tokens: u64,
    /// Host `_synaps_*` keys are dropped here, while serializing.
    messages: super::continuation::WireMessages<'a>,
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_config: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<Value>,
    tools: MarkedTools<'a>,
}

impl<'a> RequestBody<'a> {
    /// Assemble the request body. `messages` must already be sanitized +
    /// cache-annotated (`sanitize_thinking_blocks` / `annotate_cache_breakpoint`).
    /// `stream: true` → streaming transport (`"stream":true`); `false` → sync
    /// transport (key absent, matching the legacy api_sync body).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        model: &'a str,
        messages: &'a [SharedMessage],
        tools_schema: &'a [Value],
        deferred_tools: &'a [Value],
        system_prompt: &Option<String>,
        auth_type: &str,
        thinking_budget: u32,
        reasoning_level: agent_core::reasoning::ReasoningLevel,
        execution_plan: Option<&crate::runtime::openai::catalog::AnthropicExecutionPlan>,
        ttl: CacheTtl,
        stream: bool,
    ) -> Self {
        use agent_core::reasoning::ReasoningLevel;
        let adaptive = crate::core::models::model_supports_adaptive_thinking(model);
        let thinking = if reasoning_level == ReasoningLevel::Off {
            // Off: omit the thinking field entirely (safest; no unsupported wire shape).
            None
        } else if adaptive {
            Some(json!({ "type": "adaptive", "display": "summarized" }))
        } else {
            let budget = if thinking_budget == 0 {
                crate::core::models::DEFAULT_LEGACY_ADAPTIVE_FALLBACK
            } else {
                thinking_budget
            };
            Some(json!({ "type": "enabled", "budget_tokens": budget, "display": "summarized" }))
        };
        let output_config = if adaptive && reasoning_level != ReasoningLevel::Off {
            if let Some(plan) = execution_plan {
                plan.wire_effort
                    .map(|effort| json!({ "effort": effort.as_str() }))
            } else {
                match reasoning_level {
                    // Adaptive: model decides — omit output_config.effort.
                    ReasoningLevel::Adaptive => None,
                    // The NAMED level is authoritative for the exact effort value.
                    ReasoningLevel::Low
                    | ReasoningLevel::Medium
                    | ReasoningLevel::High
                    | ReasoningLevel::XHigh => Some(json!({ "effort": reasoning_level.as_str() })),
                    // Max/Ultra are rejected upstream for Anthropic models; if a
                    // stale value leaks here, fall back to the legacy
                    // budget-derived mapping rather than inventing an unsupported
                    // named effort on the wire. Loud in debug builds — this is a
                    // validation-layer bug, not a valid wire state.
                    _ => {
                        debug_assert!(
                            !matches!(
                                reasoning_level,
                                ReasoningLevel::Max
                                    | ReasoningLevel::Ultra
                                    | ReasoningLevel::UltraCode
                            ),
                            "reasoning level '{reasoning_level}' must be rejected upstream \
                         before reaching the Anthropic request body for {model}"
                        );
                        let level = crate::core::models::thinking_level_for_budget(thinking_budget);
                        crate::core::models::effort_for_thinking_level(level)
                            .map(|effort| json!({ "effort": effort }))
                    }
                }
            }
        } else {
            None
        };
        Self {
            diagnostics: None,
            max_tokens: HelperMethods::max_tokens_for_model(model),
            messages: super::continuation::WireMessages(messages),
            model,
            output_config,
            stream: if stream { Some(true) } else { None },
            system: HelperMethods::build_system_blocks(auth_type, system_prompt, ttl),
            thinking,
            tools: MarkedTools::with_deferred(tools_schema, deferred_tools, ttl),
        }
    }

    /// Output cap for a prompt-cache keep-warm ping (task #444; real turns
    /// never call this). `max_tokens` is not part of the cache key, so the
    /// cached prefix is unaffected:
    /// - thinking off / adaptive: [`KEEPWARM_OUTPUT_TOKENS`] — adaptive
    ///   thinking has no `max_tokens` floor, so the cap bounds thinking +
    ///   text together. Not the documented `max_tokens: 0` pre-warm: that is
    ///   rejected with `stream: true`, and whether `stream` is part of the
    ///   cache key is undocumented, so the ping streams exactly like the
    ///   real turn it keeps warm (spec §7.1);
    /// - budgeted thinking (`type: "enabled"`): the API requires
    ///   `max_tokens > budget_tokens`, and the budget IS part of the cache
    ///   key, so the ping keeps the budget and gets `budget + 1`. What is
    ///   billed is the thinking actually produced for a trivial prompt; the
    ///   session ledger measures it.
    ///
    /// Never raises `max_tokens` above what the real turn would send.
    ///
    /// [`KEEPWARM_OUTPUT_TOKENS`]: super::keepwarm::KEEPWARM_OUTPUT_TOKENS
    pub(super) fn apply_keepwarm_max_tokens(&mut self) {
        let budget = self
            .thinking
            .as_ref()
            .filter(|t| t["type"] == "enabled")
            .and_then(|t| t["budget_tokens"].as_u64());
        let cap = match budget {
            Some(b) => b + super::keepwarm::KEEPWARM_OUTPUT_TOKENS,
            None => super::keepwarm::KEEPWARM_OUTPUT_TOKENS,
        };
        self.max_tokens = self.max_tokens.min(cap);
    }

    /// FNV-1a 64 over the serialized cache-key prefix of this request:
    /// model, `output_config` (effort), `system`, `thinking`, `tools` and the
    /// first `history_len` messages exactly as they go on the wire (cache
    /// markers included), plus the `anthropic-beta` header (`beta`: 1M
    /// context and the like select the serving configuration). `max_tokens`
    /// and `stream` are excluded: neither is rendered into the prompt. Two
    /// requests with equal fingerprints send byte-identical prefixes through
    /// the last history breakpoint.
    pub(super) fn prefix_fingerprint(&self, history_len: usize, beta: Option<&str>) -> u64 {
        struct Fnv(u64);
        impl std::io::Write for Fnv {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                for b in buf {
                    self.0 ^= u64::from(*b);
                    self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
                }
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut h = Fnv(0xcbf2_9ce4_8422_2325);
        let sep = |h: &mut Fnv| {
            let _ = std::io::Write::write_all(h, &[0x1f]);
        };
        let _ = serde_json::to_writer(&mut h, self.model);
        sep(&mut h);
        let _ = serde_json::to_writer(&mut h, &self.output_config);
        sep(&mut h);
        let _ = serde_json::to_writer(&mut h, &self.system);
        sep(&mut h);
        let _ = serde_json::to_writer(&mut h, &self.thinking);
        sep(&mut h);
        let _ = serde_json::to_writer(&mut h, &self.tools);
        sep(&mut h);
        let n = history_len.min(self.messages.0.len());
        let _ = serde_json::to_writer(&mut h, &super::continuation::WireMessages(&self.messages.0[..n]));
        sep(&mut h);
        let _ = serde_json::to_writer(&mut h, &beta);
        h.0
    }

    /// Opt this request into Anthropic cache diagnostics.
    pub(super) fn set_diagnostics(
        &mut self,
        diagnostics: super::cache_diagnosis::DiagnosticsParam,
    ) {
        self.diagnostics = Some(diagnostics);
    }

    /// The output cap this body will send (keep-warm worst-case pricing).
    pub(super) fn max_tokens(&self) -> u64 {
        self.max_tokens
    }

    /// Pre-assembly equivalent of the legacy `body["tools"]` non-empty probe.
    pub(super) fn has_tool_marker(&self) -> bool {
        !self.tools.tools.is_empty()
    }

    /// Pre-assembly equivalent of the legacy `body.get("system").is_some()`.
    pub(super) fn has_system_marker(&self) -> bool {
        self.system.is_some()
    }
}

/// Tool schemas with the prompt-cache marker on the LAST tool (so all tool
/// schemas land in the cached prefix). Pre-assembly replacement for the legacy
/// `HelperMethods::mark_last_tool` that mutated the assembled body: borrows
/// all tools except the last, which is cloned once and stamped with
/// `cache_control` (stable-prefix site — carries `"ttl":"1h"` under OneHour
/// and Hybrid). Serializes byte-identically to the legacy in-place mutation
/// (the clone's map is BTreeMap-backed, so the inserted key sorts the same).
pub(super) struct MarkedTools<'a> {
    tools: &'a [Value],
    marked_last: Option<Value>,
    /// `defer_loading` definitions, serialized after the marked loaded tools
    /// and never marked (a deferred tool may not carry `cache_control`).
    /// Empty ⇒ byte-identical to the legacy body.
    deferred: &'a [Value],
}

impl<'a> MarkedTools<'a> {
    #[cfg(test)]
    pub(super) fn new(tools: &'a [Value], ttl: CacheTtl) -> Self {
        Self::with_deferred(tools, &[], ttl)
    }

    pub(super) fn with_deferred(tools: &'a [Value], deferred: &'a [Value], ttl: CacheTtl) -> Self {
        let marked_last = tools.last().map(|t| {
            let mut t = t.clone();
            t["cache_control"] = cache_control_value(ttl, MarkerSite::StablePrefix);
            t
        });
        Self {
            tools,
            marked_last,
            deferred,
        }
    }
}

impl Serialize for MarkedTools<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(self.tools.len() + self.deferred.len()))?;
        match &self.marked_last {
            Some(last) => {
                for tool in &self.tools[..self.tools.len() - 1] {
                    seq.serialize_element(tool)?;
                }
                seq.serialize_element(last)?;
            }
            None => {
                for tool in self.tools {
                    seq.serialize_element(tool)?;
                }
            }
        }
        for tool in self.deferred {
            seq.serialize_element(tool)?;
        }
        seq.end()
    }
}

impl ApiMethods {
    /// Build the auth header for Anthropic requests.
    /// Returns `(header_name, header_value, auth_type)`.
    pub(super) async fn build_auth_header(
        auth: &Arc<RwLock<AuthState>>,
    ) -> (String, String, String) {
        let (name, value, kind, _) = Self::build_auth_snapshot(auth).await;
        (name, value, kind)
    }

    pub(super) async fn build_auth_snapshot(
        auth: &Arc<RwLock<AuthState>>,
    ) -> (String, String, String, Option<String>) {
        let (auth_token, auth_type, binding) = {
            let a = auth.read().await;
            (
                a.auth_token.clone(),
                a.auth_type.clone(),
                a.bound_credential.clone(),
            )
        };
        let (name, value) = if auth_type == "oauth" {
            (
                "authorization".to_string(),
                format!("Bearer {}", auth_token),
            )
        } else {
            ("x-api-key".to_string(), auth_token)
        };
        (name, value, auth_type, binding)
    }

    /// Build the `anthropic-beta` header value. Returns `None` when no beta
    /// flags apply.
    ///
    /// The extended-cache-ttl token is added ONLY for API-key auth: the live
    /// probe confirmed 1h TTL is honored bare over OAuth, and the OAuth beta
    /// set is part of the pool-routing fingerprint — we do not perturb it for
    /// a feature that works without it (spec §3.4). The cache-diagnosis token
    /// follows the same rule (probed: diagnostics are honored bare on OAuth).
    pub(super) fn build_beta_header(
        auth_type: &str,
        options: &ApiOptions,
        model: &str,
    ) -> Option<String> {
        let mut betas: Vec<&str> = Vec::new();
        if auth_type == "oauth" {
            betas.push("claude-code-20250219");
            betas.push("oauth-2025-04-20");
        }
        if options.use_1m_context && crate::core::models::model_supports_1m(model) {
            betas.push("context-1m-2025-08-07");
        }
        if auth_type != "oauth" && options.cache_ttl != crate::core::config::CacheTtl::FiveMinutes {
            betas.push("extended-cache-ttl-2025-04-11");
        }
        if super::cache_diagnosis::wants_beta_header(auth_type, options) {
            betas.push(super::cache_diagnosis::CACHE_DIAGNOSIS_BETA);
        }
        if betas.is_empty() {
            None
        } else {
            Some(betas.join(","))
        }
    }
}

#[cfg(test)]
mod beta_header_tests {
    use super::*;
    use crate::core::config::CacheTtl;

    fn opts(ttl: CacheTtl) -> ApiOptions {
        ApiOptions {
            cache_ttl: ttl,
            ..Default::default()
        }
    }

    const MODEL: &str = "claude-sonnet-4-6";

    #[test]
    fn api_key_5m_emits_no_header() {
        // DEFAULT MUST BE INVISIBLE: no header where there was none before.
        assert_eq!(
            ApiMethods::build_beta_header("api_key", &opts(CacheTtl::FiveMinutes), MODEL),
            None
        );
    }

    #[test]
    fn api_key_1h_and_hybrid_emit_extended_ttl_beta() {
        for ttl in [CacheTtl::OneHour, CacheTtl::Hybrid] {
            assert_eq!(
                ApiMethods::build_beta_header("api_key", &opts(ttl), MODEL).as_deref(),
                Some("extended-cache-ttl-2025-04-11"),
                "under {ttl:?}"
            );
        }
    }

    #[test]
    fn oauth_beta_set_unperturbed_by_cache_ttl() {
        // OAuth sends no new beta token in ANY mode — its beta set is part
        // of the pool-routing fingerprint and 1h works bare (live-probed).
        for ttl in [CacheTtl::FiveMinutes, CacheTtl::OneHour, CacheTtl::Hybrid] {
            assert_eq!(
                ApiMethods::build_beta_header("oauth", &opts(ttl), MODEL).as_deref(),
                Some("claude-code-20250219,oauth-2025-04-20"),
                "under {ttl:?}"
            );
        }
    }

    #[test]
    fn extended_ttl_comma_joins_with_1m_context() {
        let options = ApiOptions {
            use_1m_context: true,
            cache_ttl: CacheTtl::OneHour,
            ..Default::default()
        };
        // claude-sonnet-4-6 supports 1M — precondition assert so this test
        // fails LOUDLY (not silently no-ops) if the model table changes.
        assert!(
            crate::core::models::model_supports_1m(MODEL),
            "fixture model fell out of the 1M table — pick a 1M-capable fixture"
        );
        assert_eq!(
            ApiMethods::build_beta_header("api_key", &options, MODEL).as_deref(),
            Some("context-1m-2025-08-07,extended-cache-ttl-2025-04-11"),
        );
    }
}

#[cfg(test)]
mod anthropic_reasoning_body_tests {
    use super::*;
    use agent_core::reasoning::ReasoningLevel;

    const ADAPTIVE_MODEL: &str = "claude-opus-4-7";
    const FIXED_MODEL: &str = "claude-sonnet-4-6";

    fn body_json(model: &str, thinking_budget: u32, level: ReasoningLevel) -> serde_json::Value {
        body_json_with_plan(model, thinking_budget, level, None)
    }

    fn body_json_with_plan(
        model: &str,
        thinking_budget: u32,
        level: ReasoningLevel,
        plan: Option<&crate::runtime::openai::catalog::AnthropicExecutionPlan>,
    ) -> serde_json::Value {
        let messages: Vec<crate::SharedMessage> =
            vec![Arc::new(json!({"role": "user", "content": "hi"}))];
        let body = RequestBody::new(
            model,
            &messages,
            &[],
            &[],
            &None,
            "api_key",
            thinking_budget,
            level,
            plan,
            CacheTtl::FiveMinutes,
            false,
        );
        serde_json::to_value(&body).expect("serialize")
    }

    #[test]
    fn off_omits_thinking_and_output_config_on_both_shapes() {
        for model in [ADAPTIVE_MODEL, FIXED_MODEL] {
            let v = body_json(model, 4096, ReasoningLevel::Off);
            assert!(v.get("thinking").is_none(), "{model}");
            assert!(v.get("output_config").is_none(), "{model}");
        }
    }

    /// Max/Ultra must be rejected upstream by mutation validation; reaching
    /// the Anthropic RequestBody with one is a logic error — surfaced loudly
    /// in debug builds (release keeps the safe legacy-budget fallback).
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "rejected upstream")]
    fn max_level_reaching_anthropic_adaptive_body_panics_in_debug() {
        let _ = body_json(ADAPTIVE_MODEL, 16384, ReasoningLevel::Max);
    }

    #[test]
    fn adaptive_level_uses_adaptive_wire_and_omits_effort() {
        let v = body_json(ADAPTIVE_MODEL, 0, ReasoningLevel::Adaptive);
        assert_eq!(
            v["thinking"],
            json!({"type": "adaptive", "display": "summarized"})
        );
        assert!(
            v.get("output_config").is_none(),
            "Adaptive must omit output_config.effort (model decides)"
        );
    }

    /// The NAMED level is authoritative for effort — not the legacy budget.
    #[test]
    fn named_level_drives_exact_effort_on_adaptive_models() {
        for (level, effort) in [
            (ReasoningLevel::Low, "low"),
            (ReasoningLevel::Medium, "medium"),
            (ReasoningLevel::High, "high"),
            (ReasoningLevel::XHigh, "xhigh"),
        ] {
            // Deliberately mismatched legacy budget (4096 = medium tier):
            // effort must come from the named level, never the budget bucket.
            let v = body_json(ADAPTIVE_MODEL, 4096, level);
            assert_eq!(v["output_config"], json!({"effort": effort}), "{level}");
            assert_eq!(
                v["thinking"],
                json!({"type": "adaptive", "display": "summarized"})
            );
        }
    }

    #[test]
    fn typed_special_plans_drive_exact_effort_without_logical_wire_leak() {
        use crate::runtime::openai::catalog::{
            plan_anthropic_execution, AnthropicPlanPrerequisites, ExecutionRole,
        };
        for (level, effort) in [
            (ReasoningLevel::Max, "max"),
            (ReasoningLevel::UltraCode, "xhigh"),
            (ReasoningLevel::XHigh, "xhigh"),
        ] {
            let plan = plan_anthropic_execution(
                "anthropic/claude-fable-5",
                level,
                ExecutionRole::Foreground,
                AnthropicPlanPrerequisites::installed(),
                None,
            )
            .unwrap();
            let value = body_json_with_plan(ADAPTIVE_MODEL, 0, level, Some(&plan));
            assert_eq!(value["output_config"], json!({"effort": effort}));
            assert_eq!(value["thinking"]["type"], "adaptive");
            assert!(!serde_json::to_string(&value).unwrap().contains("ultracode"));
        }
    }

    /// Fixed-budget models keep enabled+budget_tokens exactly and must never
    /// receive named effort values (no output_config at all).
    #[test]
    fn fixed_budget_models_keep_exact_budget_and_never_get_effort() {
        let v = body_json(FIXED_MODEL, 8192, ReasoningLevel::High);
        assert_eq!(
            v["thinking"],
            json!({"type": "enabled", "budget_tokens": 8192, "display": "summarized"})
        );
        assert!(v.get("output_config").is_none());
    }
}
