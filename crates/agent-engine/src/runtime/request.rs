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
///   max_tokens, messages, model, [output_config], [stream], [system], thinking, tools
/// Optional-field PRESENCE must also match: `output_config` only for adaptive
/// models with a mapped effort, `stream` only on the streaming transport,
/// `system` only when `build_system_blocks` returns Some. Guarded by
/// `runtime::body_golden` — do not reorder fields or change skip conditions
/// without regenerating fixtures (and accepting fleet-wide cache invalidation).
#[derive(serde::Serialize)]
pub(super) struct RequestBody<'a> {
    max_tokens: u64,
    messages: &'a [SharedMessage],
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
        system_prompt: &Option<String>,
        auth_type: &str,
        thinking_budget: u32,
        reasoning_level: agent_core::reasoning::ReasoningLevel,
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
                        !reasoning_level.requires_codex_support(),
                        "reasoning level '{reasoning_level}' must be rejected upstream \
                         before reaching the Anthropic request body for {model}"
                    );
                    let level = crate::core::models::thinking_level_for_budget(thinking_budget);
                    crate::core::models::effort_for_thinking_level(level)
                        .map(|effort| json!({ "effort": effort }))
                }
            }
        } else {
            None
        };
        Self {
            max_tokens: HelperMethods::max_tokens_for_model(model),
            messages,
            model,
            output_config,
            stream: if stream { Some(true) } else { None },
            system: HelperMethods::build_system_blocks(auth_type, system_prompt, ttl),
            thinking,
            tools: MarkedTools::new(tools_schema, ttl),
        }
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
}

impl<'a> MarkedTools<'a> {
    pub(super) fn new(tools: &'a [Value], ttl: CacheTtl) -> Self {
        let marked_last = tools.last().map(|t| {
            let mut t = t.clone();
            t["cache_control"] = cache_control_value(ttl, MarkerSite::StablePrefix);
            t
        });
        Self { tools, marked_last }
    }
}

impl Serialize for MarkedTools<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(self.tools.len()))?;
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
        seq.end()
    }
}

impl ApiMethods {
    /// Build the auth header for Anthropic requests.
    /// Returns `(header_name, header_value, auth_type)`.
    pub(super) async fn build_auth_header(
        auth: &Arc<RwLock<AuthState>>,
    ) -> (String, String, String) {
        let (auth_token, auth_type) = {
            let a = auth.read().await;
            (a.auth_token.clone(), a.auth_type.clone())
        };
        let (name, value) = if auth_type == "oauth" {
            (
                "authorization".to_string(),
                format!("Bearer {}", auth_token),
            )
        } else {
            ("x-api-key".to_string(), auth_token)
        };
        (name, value, auth_type)
    }

    /// Build the `anthropic-beta` header value. Returns `None` when no beta
    /// flags apply.
    ///
    /// The extended-cache-ttl token is added ONLY for API-key auth: the live
    /// probe confirmed 1h TTL is honored bare over OAuth, and the OAuth beta
    /// set is part of the pool-routing fingerprint — we do not perturb it for
    /// a feature that works without it (spec §3.4).
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
        let messages: Vec<crate::SharedMessage> =
            vec![Arc::new(json!({"role": "user", "content": "hi"}))];
        let body = RequestBody::new(
            model,
            &messages,
            &[],
            &None,
            "api_key",
            thinking_budget,
            level,
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
