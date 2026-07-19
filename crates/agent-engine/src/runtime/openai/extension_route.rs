//! Extension-provider routing (extracted from `openai/mod.rs::try_route`)
//! plus Task 10C transport trace integration.
//!
//! Behavior contract (unchanged from the inline routing block):
//!
//! - Provider existence, per-provider trust, and trust-state readability are
//!   checked **before** any IPC; a disabled provider never falls back to
//!   built-in routing.
//! - Every terminal branch appends one extension audit entry.
//! - Streaming is used only when the model declares `streaming` and no
//!   active tools are in play; otherwise the non-streaming turn runs
//!   (through the tool loop when a shared tool registry exists).
//!
//! Trace contract (Task 10C, see `trace::extension` module docs): a
//! `TransportKind::Extension` record begins only after every gate above and
//! immediately before the actual `provider_stream` / `provider_complete`
//! IPC; one outer extension turn is one transport attempt; the wire section
//! is always `None`; trace failure can never change provider behavior.

use std::sync::Arc;

use serde_json::Value;

use crate::extensions::manager::ExtensionManager;
use crate::runtime::trace::extension as trace_ext;
use crate::tools::{ToolCapabilities, ToolChannels, ToolContext, ToolLimits};

type RouteResult = Result<Value, Box<dyn std::error::Error + Send + Sync>>;

/// Fallback tool-session identity for callers that carry none (internal
/// non-stream helpers). Minted fresh per call: it can never inherit or
/// share grants, and the default-core policy it scopes is identical to the
/// runtime-threaded one (verified capabilities only, zero activations) —
/// failing closed is therefore "same policy, no shared identity", never an
/// ungated registry lookup.
fn local_gate_session() -> crate::tools::activation::SessionId {
    crate::tools::activation::SessionId::parse(&format!(
        "extension-route-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ))
    .expect("generated local gate session id is always valid")
}

/// Route one request through an extension-hosted provider. The caller has
/// already parsed `model` into `plugin:provider:model` and resolved the
/// routing manager. `tool_session_id` is the runtime-scoped tool-session
/// identity (Task 16) scoping the execution gate inside the interior tool
/// loop; `None` (internal/sync callers) fails closed to a locally minted
/// identity with the same default-core policy.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn route_extension_provider(
    manager: Arc<tokio::sync::RwLock<ExtensionManager>>,
    plugin_id: &str,
    provider_id: &str,
    model_id: &str,
    model: &str,
    tools_schema: &Arc<Vec<Value>>,
    system_prompt: &Option<String>,
    messages: &[crate::SharedMessage],
    tx: &tokio::sync::mpsc::UnboundedSender<crate::runtime::types::StreamEvent>,
    temperature: Option<f32>,
    max_tokens: Option<u32>,
    thinking_budget: u32,
    cancel: &tokio_util::sync::CancellationToken,
    tool_session_id: Option<&crate::tools::activation::SessionId>,
    session_tool_set: Option<&crate::tools::activation::SharedSessionToolSet>,
    trace: &crate::runtime::trace::TraceContext,
) -> RouteResult {
    let provider_runtime_id = format!("{}:{}", plugin_id, provider_id);
    let Some((handler, hook_bus, tools_shared, streaming, model_tool_use)) = ({
        let manager = manager.read().await;
        manager.provider(&provider_runtime_id).and_then(|provider| {
            provider.handler.as_ref().map(|handler| {
                let model_spec = provider.spec.models.iter().find(|m| m.id == model_id);
                let streaming = model_spec
                    .and_then(|m| m.capabilities.get("streaming"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let model_tool_use = model_spec
                    .and_then(|m| m.capabilities.get("tool_use"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                (
                    handler.clone(),
                    manager.hook_bus().clone(),
                    manager.tools_shared(),
                    streaming,
                    model_tool_use,
                )
            })
        })
    }) else {
        return Err(format!("Extension provider model '{}' is not available", model).into());
    };
    // Per-provider trust gate: a disabled provider must not be invoked.
    // The check runs before any IPC and we DO NOT silently fall back to
    // built-in routing — instead return a clear routing error.
    let trust = match crate::extensions::trust::load_trust_state() {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("trust.json corrupt or unreadable, failing closed: {e}");
            return Err(format!("Cannot route to provider: trust state unreadable: {e}").into());
        }
    };
    if !crate::extensions::trust::is_provider_enabled(&trust, &provider_runtime_id) {
        let _ = crate::extensions::audit::append_audit_entry(
            &crate::extensions::audit::new_audit_entry(
                plugin_id,
                provider_id,
                model_id,
                false,
                0,
                false,
                "blocked",
                Some("trust_disabled".to_string()),
            ),
        );
        return Err(format!(
            "Provider '{}' is disabled by user trust settings",
            provider_runtime_id
        )
        .into());
    }
    // Audit metadata captured up-front so each terminal branch can record an entry.
    let audit_plugin = plugin_id.to_string();
    let audit_provider = provider_id.to_string();
    let audit_model = model_id.to_string();
    let tools_exposed = !tools_schema.is_empty();
    let emit_audit =
        |streamed: bool, outcome: &str, error_class: Option<&str>, tools_requested: u32| {
            let _ = crate::extensions::audit::append_audit_entry(
                &crate::extensions::audit::new_audit_entry(
                    audit_plugin.clone(),
                    audit_provider.clone(),
                    audit_model.clone(),
                    tools_exposed,
                    tools_requested,
                    streamed,
                    outcome,
                    error_class.map(|s| s.to_string()),
                ),
            );
        };
    if cancel.is_cancelled() {
        // Cancelled before any IPC started: no request-attempt trace record.
        emit_audit(false, "error", Some("canceled"), 0);
        return Err("operation canceled".into());
    }
    let params = crate::extensions::runtime::process::ProviderCompleteParams {
        provider_id: provider_id.to_string(),
        model_id: model_id.to_string(),
        model: model.to_string(),
        // Arc-shared (#128): refcount bumps, not a deep copy. Serde's
        // `rc` feature serializes Arc<Value> transparently, so the
        // wire shape crossing the extension boundary is unchanged.
        messages: messages.to_vec(),
        system_prompt: system_prompt.clone(),
        // Generation-pinned for the WHOLE interior tool loop (Task 18): the
        // caller's schema (full set, or the flag-on session projection) is
        // snapshotted here and never re-projected between interior rounds.
        // Sound because the interior loop's ToolContext carries
        // `tool_activation: None` (below), so `activate_tools` cannot mutate
        // the retained session set mid-loop — activations only surface on
        // the next OUTER stream round, which re-projects. If activation
        // capability is ever wired into this route, `tools` must be
        // recomputed per interior round from the session set instead.
        tools: tools_schema.as_ref().clone(),
        temperature,
        max_tokens,
        thinking_budget,
    };
    let has_active_tools = model_tool_use && !tools_schema.is_empty();
    // Streaming path: forward TextDelta events as LlmEvent::Text deltas in real time.
    if streaming && !has_active_tools {
        // All gates passed — trace begins immediately before the IPC send.
        let mut attempt = trace_ext::ExtensionAttempt::new(
            trace_ext::begin_extension_tracer(
                trace,
                plugin_id,
                provider_id,
                model_id,
                true,
                messages,
                system_prompt.as_deref(),
                tools_schema,
                trace_ext::extension_capability_losses(tools_schema, model_tool_use, false),
            )
            .await,
        );
        let first_event = attempt.first_event_mark();
        let (sink_tx, mut sink_rx) = tokio::sync::mpsc::unbounded_channel::<
            crate::extensions::runtime::process::ProviderStreamEvent,
        >();
        let tx_clone = tx.clone();
        let forwarder = tokio::spawn(async move {
            use crate::extensions::runtime::process::ProviderStreamEvent;
            while let Some(event) = sink_rx.recv().await {
                match event {
                    ProviderStreamEvent::TextDelta { text } => {
                        first_event.mark();
                        let _ = tx_clone.send(crate::runtime::types::StreamEvent::Llm(
                            crate::runtime::types::LlmEvent::Text(text),
                        ));
                    }
                    ProviderStreamEvent::ToolUse { .. } => {
                        first_event.mark();
                        tracing::warn!("provider.stream tool_use event ignored (streaming tool-use not yet wired)");
                    }
                    ProviderStreamEvent::ThinkingDelta { .. }
                    | ProviderStreamEvent::Usage { .. } => {
                        // Model-authored events: mark first-event timing, then
                        // absorb — the final result aggregates them.
                        first_event.mark();
                    }
                    // Done / Error are transport markers, not model events.
                    _ => {}
                }
            }
        });
        let stream_fut = handler.provider_stream(params, sink_tx);
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                forwarder.abort();
                attempt.finish_canceled();
                emit_audit(true, "error", Some("canceled"), 0);
                return Err("operation canceled".into());
            }
            res = stream_fut => res,
        };
        let _ = forwarder.await;
        if cancel.is_cancelled() {
            attempt.finish_canceled();
            emit_audit(true, "error", Some("canceled"), 0);
            return Err("operation canceled".into());
        }
        // TODO(audit): tools_requested is reported as 0 for the streaming
        // path until ProviderStreamEvent::ToolUse is wired through the
        // forwarder; tool-use over streaming is not yet routed.
        match result {
            Ok(complete) => {
                attempt.finish_success(
                    complete
                        .stop_reason
                        .as_deref()
                        .map(trace_ext::stop_reason_from_extension),
                    trace_ext::usage_from_extension_value(complete.usage.as_ref()),
                );
                emit_audit(true, "ok", None, 0);
                Ok(serde_json::json!({
                    "content": complete.content,
                    "stop_reason": complete.stop_reason.unwrap_or_else(|| "end_turn".to_string()),
                    "usage": complete.usage.unwrap_or_else(|| serde_json::json!({}))
                }))
            }
            Err(e) => {
                attempt.finish_failed(trace_ext::EXTENSION_PROVIDER_ERROR_CODE);
                emit_audit(true, "error", Some("provider_error"), 0);
                Err(format!("extension provider: {e}").into())
            }
        }
    } else {
        // Non-streaming turn (direct complete, or the tool loop when a shared
        // registry exists). One outer turn == one transport attempt: the
        // tool-loop helper may perform several interior provider.complete
        // calls, which are deliberately not counted as separate attempts
        // (see `trace::extension` module docs).
        let mut attempt = trace_ext::ExtensionAttempt::new(
            trace_ext::begin_extension_tracer(
                trace,
                plugin_id,
                provider_id,
                model_id,
                false,
                messages,
                system_prompt.as_deref(),
                tools_schema,
                trace_ext::extension_capability_losses(tools_schema, model_tool_use, true),
            )
            .await,
        );
        let result = if let Some(tools) = tools_shared {
            let registry = tools.read().await;
            // ═══ EXECUTION GATE (Task 16/17, spec §7.1) ═══ ONE session
            // tool set for the whole interior tool loop, resolved here
            // under the SAME registry read guard the loop holds for the
            // entire outer call: the catalog cannot drift between interior
            // rounds, so one snapshot at one generation governs every tool
            // call the extension provider requests. When the runtime
            // threads its RETAINED per-stream set (Task 17), THAT set's
            // current state — including exact activations — is consumed at
            // its pinned generation; a stale retained set is DENIED typed,
            // never silently replaced by a fresh default-core mint. Only
            // callers with no retained handle at all fall back to a fresh
            // default-core set with zero activations.
            let session_tools = match crate::tools::activation::route_session_set(
                session_tool_set,
                registry.catalog(),
                || tool_session_id.cloned().unwrap_or_else(local_gate_session),
            ) {
                Ok(session_tools) => session_tools,
                Err(denial) => {
                    attempt.finish_failed(trace_ext::EXTENSION_PROVIDER_ERROR_CODE);
                    emit_audit(false, "error", Some("stale_session_tool_set"), 0);
                    return Err(format!("extension provider tool loop denied: {denial}").into());
                }
            };
            crate::extensions::runtime::process::complete_provider_with_tools(
                handler.clone(),
                params,
                &registry,
                &session_tools,
                &hook_bus,
                || ToolContext {
                    channels: ToolChannels {
                        tx_delta: None,
                        tx_events: None,
                    },
                    capabilities: ToolCapabilities {
                        watcher_exit_path: None,
                        tool_register_tx: None,
                        session_manager: None,
                        subagent_registry: None,
                        event_queue: None,
                        secret_prompt: None,
                        orchestration: None,
                        tool_activation: None,
                        mcp_leases: None,
                        extension_leases: None,
                    },
                    limits: ToolLimits {
                        max_tool_output: 30000,
                        max_tool_buffer: 256 * 1024,
                        bash_timeout: 30,
                        bash_max_timeout: 300,
                        subagent_timeout: 300,
                    },
                },
                30000,
                8,
            )
            .await
        } else {
            handler.provider_complete(params).await
        };
        if cancel.is_cancelled() {
            attempt.finish_canceled();
            emit_audit(false, "error", Some("canceled"), 0);
            return Err("operation canceled".into());
        }
        // TODO(audit): tools_requested is reported as 0 here; the
        // complete_provider_with_tools helper does not yet expose its
        // observed tool-use iteration count. Wire that through when the
        // helper grows a return-tuple or counter argument.
        match result {
            Ok(complete) => {
                let text = complete
                    .content
                    .iter()
                    .filter_map(|block| block.get("text").and_then(|v| v.as_str()))
                    .collect::<Vec<_>>()
                    .join("");
                if !text.is_empty() {
                    let _ = tx.send(crate::runtime::types::StreamEvent::Llm(
                        crate::runtime::types::LlmEvent::Text(text),
                    ));
                }
                attempt.finish_success(
                    complete
                        .stop_reason
                        .as_deref()
                        .map(trace_ext::stop_reason_from_extension),
                    trace_ext::usage_from_extension_value(complete.usage.as_ref()),
                );
                emit_audit(false, "ok", None, 0);
                Ok(serde_json::json!({
                    "content": complete.content,
                    "stop_reason": complete.stop_reason.unwrap_or_else(|| "end_turn".to_string()),
                    "usage": complete.usage.unwrap_or_else(|| serde_json::json!({}))
                }))
            }
            Err(e) => {
                attempt.finish_failed(trace_ext::EXTENSION_PROVIDER_ERROR_CODE);
                emit_audit(false, "error", Some("provider_error"), 0);
                Err(format!("extension provider: {e}").into())
            }
        }
    }
}
