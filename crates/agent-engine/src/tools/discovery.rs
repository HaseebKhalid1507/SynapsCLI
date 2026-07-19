//! Model-facing discovery and exact-activation builtins (Task 17, spec
//! §4.2, §7.2, §7.3, §7.7): `search_tools` and `activate_tools`.
//!
//! Both tools operate ONLY on the capability context the host injects into
//! [`crate::tools::ToolCapabilities::tool_activation`]:
//!
//! - a PASSIVE catalog snapshot (compact descriptors + digests; reading it
//!   starts no process, touches no network, exposes no schema);
//! - the RETAINED per-stream [`SharedSessionToolSet`] handle, so a
//!   confirmed activation mutates the same set the `ExecutionGate` and the
//!   next provider round consume;
//! - a host-supplied [`ActivationAuthority`], populated exclusively by host
//!   policy — never from model-authored JSON arguments.
//!
//! `search_tools` returns bounded compact descriptors with stable
//! [`ToolId`]s only — no full schemas, no factories, no source paths, no
//! credentials. `activate_tools` performs deterministic atomic bulk
//! activation of EXACT known ids through host grant issuance; source-wide,
//! provider-wide, wildcard, unknown, and malformed identities fail typed
//! with zero mutation. Without host confirmation authority the request is
//! denied before any grant, set, or schema-generation mutation.

use serde_json::{json, Value};

use super::activation::{activate_model_initiated, ActivationAuthority, SharedSessionToolSet};
use super::catalog::{DiscoveryIndex, DiscoveryQuery, SearchLimits, ToolCatalog, ToolId};
use super::{Tool, ToolContext, ToolOrigin};
use crate::{Result, RuntimeError};
use agent_core::BoundedText;

/// Default bounded result-count budget for `search_tools`.
pub const SEARCH_TOOLS_MAX_RESULTS: usize = 16;
/// Default bounded serialized byte budget for `search_tools` hits.
pub const SEARCH_TOOLS_MAX_RESULT_BYTES: usize = 8 * 1024;
/// Maximum number of ids in one `activate_tools` batch.
pub const ACTIVATE_TOOLS_MAX_BATCH: usize = 16;
/// Echo bound for hostile identity strings inside typed denials.
const ID_ECHO_MAX_BYTES: usize = 128;

/// Capability context for the discovery/activation builtins. Constructed by
/// the host per round from the round's catalog snapshot, the retained
/// session-set handle, and host confirmation policy.
#[derive(Clone)]
pub struct ActivationCapability {
    catalog: ToolCatalog,
    session_set: SharedSessionToolSet,
    authority: ActivationAuthority,
}

impl ActivationCapability {
    pub fn new(
        catalog: ToolCatalog,
        session_set: SharedSessionToolSet,
        authority: ActivationAuthority,
    ) -> Self {
        Self {
            catalog,
            session_set,
            authority,
        }
    }

    pub fn catalog(&self) -> &ToolCatalog {
        &self.catalog
    }

    pub fn session_set(&self) -> &SharedSessionToolSet {
        &self.session_set
    }

    pub fn authority(&self) -> ActivationAuthority {
        self.authority
    }
}

impl std::fmt::Debug for ActivationCapability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActivationCapability")
            .field("authority", &self.authority)
            .finish_non_exhaustive()
    }
}

fn require_capability(ctx: &ToolContext) -> Result<ActivationCapability> {
    ctx.capabilities.tool_activation.clone().ok_or_else(|| {
        RuntimeError::Tool("tool discovery/activation is not available in this context".to_string())
    })
}

fn require_query(params: &Value) -> Result<DiscoveryQuery> {
    let raw = params["query"]
        .as_str()
        .ok_or_else(|| RuntimeError::Tool("Missing 'query' parameter (string)".to_string()))?;
    DiscoveryQuery::parse(raw)
        .map_err(|err| RuntimeError::Tool(format!("invalid discovery query: {err}")))
}

// ── search_tools ────────────────────────────────────────────────────────────

/// Bounded, deterministic, descriptor-only capability search over the
/// passive catalog snapshot. Pure and local: no factory invocation, no
/// process start, no network, no schema exposure, no grant.
pub struct SearchToolsTool;

#[async_trait::async_trait]
impl Tool for SearchToolsTool {
    fn name(&self) -> &str {
        "search_tools"
    }

    fn description(&self) -> &str {
        "Search locally known tools by keyword. Returns bounded compact \
         descriptors with stable tool ids (never full schemas). Use \
         activate_tools with an exact returned id to request activation."
    }

    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Builtin
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Case-insensitive keyword matched against tool ids, summaries, and tags."
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<String> {
        let capability = require_capability(&ctx)?;
        let query = require_query(&params)?;
        let limits = SearchLimits::new(SEARCH_TOOLS_MAX_RESULTS, SEARCH_TOOLS_MAX_RESULT_BYTES)
            .expect("static search_tools budgets are within caps");
        let index = DiscoveryIndex::build(capability.catalog())
            .map_err(|err| RuntimeError::Tool(format!("tool discovery unavailable: {err}")))?;
        let results = index.search(&query, &limits);
        let body = json!({
            "generation": results.generation(),
            "truncated": results.truncated(),
            "tools": results.hits(),
        });
        serde_json::to_string(&body)
            .map_err(|err| RuntimeError::Tool(format!("failed to serialize search results: {err}")))
    }
}

// ── activate_tools ──────────────────────────────────────────────────────────

/// Model-initiated deterministic exact activation. Every requested id must
/// be an exact known cataloged `ToolId`; the whole batch validates first
/// (host grant issuance: exact id, current generation/digest, source trust)
/// and applies atomically in stable id order with exactly one session
/// schema-generation advance. Requires host-supplied confirmation
/// authority; a model-authored JSON flag can never substitute for it.
pub struct ActivateToolsTool;

#[async_trait::async_trait]
impl Tool for ActivateToolsTool {
    fn name(&self) -> &str {
        "activate_tools"
    }

    fn description(&self) -> &str {
        "Request session-scoped activation of exact tool ids returned by \
         search_tools. Activation is subject to host confirmation policy; \
         only the exact requested tools are activated for this session."
    }

    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Builtin
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "tools": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Exact stable tool ids (namespace:name) from search_tools. No wildcards."
                }
            },
            "required": ["tools"]
        })
    }

    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<String> {
        let capability = require_capability(&ctx)?;
        let requested = params["tools"].as_array().ok_or_else(|| {
            RuntimeError::Tool("Missing 'tools' parameter (array of tool id strings)".to_string())
        })?;
        if requested.is_empty() {
            return Err(RuntimeError::Tool(
                "no tool ids requested: 'tools' must name at least one exact id".to_string(),
            ));
        }
        if requested.len() > ACTIVATE_TOOLS_MAX_BATCH {
            return Err(RuntimeError::Tool(format!(
                "too many tool ids requested: {} exceeds limit {ACTIVATE_TOOLS_MAX_BATCH}",
                requested.len()
            )));
        }
        // Parse-at-boundary: every entry must be an exact canonical ToolId.
        // Source-wide ("builtin"), wildcard ("ns:*"), empty, and oversized
        // spellings fail typed here, before any authority/grant work.
        let mut tool_ids = Vec::with_capacity(requested.len());
        for entry in requested {
            let raw = entry
                .as_str()
                .ok_or_else(|| RuntimeError::Tool("tool ids must be strings".to_string()))?;
            let id = ToolId::parse(raw).map_err(|err| {
                RuntimeError::Tool(format!(
                    "invalid tool id {:?}: {err} (exact namespace:name ids only; \
                     wildcards and source-wide requests are not accepted)",
                    BoundedText::new(raw, ID_ECHO_MAX_BYTES).text
                ))
            })?;
            tool_ids.push(id);
        }
        // Authority check + host grant issuance + atomic bulk activation.
        // The write guard is short-lived and never held across an await.
        let activated = {
            let mut set = capability
                .session_set()
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            activate_model_initiated(
                capability.authority(),
                &mut set,
                capability.catalog(),
                &tool_ids,
            )
            .map_err(|err| RuntimeError::Tool(format!("tool activation denied: {err}")))?
        };
        let ids: Vec<&str> = {
            let mut sorted: Vec<&ToolId> = tool_ids.iter().collect();
            sorted.sort();
            sorted.into_iter().map(ToolId::as_str).collect()
        };
        let schema_generation = capability
            .session_set()
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .schema_generation();
        let body = json!({
            "activated": ids,
            "count": activated,
            "schema_generation": schema_generation,
        });
        serde_json::to_string(&body).map_err(|err| {
            RuntimeError::Tool(format!("failed to serialize activation result: {err}"))
        })
    }
}
