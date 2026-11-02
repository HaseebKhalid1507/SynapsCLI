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
//!
//! Child spend (a worker's tap; inert otherwise): the streamed call books
//! its last report with counts once, when it returns or its guard drops; the
//! direct call books its result when it returns; the host tool loop books
//! each interior `provider_complete` and never the outer result. Retries
//! inside one extension RPC are the extension's own and stay invisible.

use std::sync::Arc;

use serde_json::Value;

use crate::extensions::manager::ExtensionManager;
use crate::runtime::child_spend::{ReportedUsage, RouteSpend};
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
/// identity with the same default-core policy. `sandbox` is the session's
/// write-sandbox binding: bash/PTY commands the interior loop runs are
/// sandboxed exactly as in the session's own turns (`None`: no sandbox).
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
    memory_backend: Option<&crate::memory_backend::MemoryBinding>,
    sandbox: Option<&Arc<crate::sandbox::SandboxBinding>>,
    trace: &crate::runtime::trace::TraceContext,
    spend: RouteSpend<'_>,
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
        // CP-11 fix-2 (B): BOUNDED provider-event handoff. A slow forwarder
        // backpressures the provider_stream loop (and transitively the
        // sidecar) instead of retaining hostile TextDelta floods; the
        // forwarder itself feeds the outer boundary, whose retention is
        // policy-bounded by the stream relay.
        let (sink_tx, mut sink_rx) =
            tokio::sync::mpsc::channel::<crate::extensions::runtime::process::ProviderStreamEvent>(
                crate::extensions::runtime::process::PROVIDER_EVENT_QUEUE_CAPACITY,
            );
        let tx_clone = tx.clone();
        // Honest audit metric: `ToolUse` events genuinely arrive here and are
        // dropped (streaming tool-use is not routed yet). Reporting 0
        // regardless contradicted `tools_requested`'s documented meaning
        // ("0 if no tool-use loop ran") and hid the drop from the TUI, which
        // only renders `tools=N` when N > 0. Count what the provider actually
        // asked for, even though we discard it.
        let stream_tool_uses = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let stream_tool_uses_fwd = std::sync::Arc::clone(&stream_tool_uses);
        // One paid attempt. The forwarder shows the guard each streamed
        // report (a snapshot: the last with counts is kept) and the call's
        // own final usage is shown when it returns; only this route books it.
        // Booking fences the forwarder: a report it handles later (a cancel
        // aborts it) is ignored, and the forwarder never books.
        let attempt_spend = spend.attempt();
        let stream_spend = attempt_spend.observer();
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
                        stream_tool_uses_fwd.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        tracing::warn!("provider.stream tool_use event ignored (streaming tool-use not yet wired)");
                    }
                    ProviderStreamEvent::ThinkingDelta { .. } => {
                        // Model-authored events: mark first-event timing, then
                        // absorb — the final result aggregates them.
                        first_event.mark();
                    }
                    ProviderStreamEvent::Usage { usage } => {
                        // Absorbed as above; the attempt's guard keeps it.
                        first_event.mark();
                        stream_spend.observe(&ReportedUsage::from_anthropic_json(&usage));
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
                // The guard's drop at this return books what the forwarder
                // has shown it.
                forwarder.abort();
                attempt.finish_canceled();
                emit_audit(
                    true,
                    "error",
                    Some("canceled"),
                    stream_tool_uses.load(std::sync::atomic::Ordering::Relaxed),
                );
                return Err("operation canceled".into());
            }
            res = stream_fut => res,
        };
        let _ = forwarder.await;
        // The call returned and was paid: its own final usage (with counts it
        // wins over the streamed snapshots) is booked now, before the cancel
        // check below could discard it.
        if let Ok(complete) = &result {
            if let Some(usage) = &complete.usage {
                attempt_spend.observe(&ReportedUsage::from_anthropic_json(usage));
            }
        }
        attempt_spend.settle();
        // Forwarder joined: the counter is final.
        let stream_tool_uses = stream_tool_uses.load(std::sync::atomic::Ordering::Relaxed);
        if cancel.is_cancelled() {
            attempt.finish_canceled();
            emit_audit(true, "error", Some("canceled"), stream_tool_uses);
            return Err("operation canceled".into());
        }
        match result {
            Ok(complete) => {
                attempt.finish_success(
                    complete
                        .stop_reason
                        .as_deref()
                        .map(trace_ext::stop_reason_from_extension),
                    trace_ext::usage_from_extension_value(complete.usage.as_ref()),
                );
                emit_audit(true, "ok", None, stream_tool_uses);
                Ok(serde_json::json!({
                    "content": complete.content,
                    "stop_reason": complete.stop_reason.unwrap_or_else(|| "end_turn".to_string()),
                    "usage": complete.usage.unwrap_or_else(|| serde_json::json!({}))
                }))
            }
            Err(e) => {
                attempt.finish_failed(trace_ext::EXTENSION_PROVIDER_ERROR_CODE);
                emit_audit(true, "error", Some("provider_error"), stream_tool_uses);
                Err(format!("extension provider: {e}").into())
            }
        }
    } else {
        // Non-streaming turn (direct complete, or the tool loop when a shared
        // registry exists). One outer turn == one transport attempt: the
        // tool-loop helper may perform several interior provider.complete
        // calls, which are deliberately not counted as separate attempts
        // (see `trace::extension` module docs).
        // Populated by the tool-loop helper; stays 0 on the direct-complete
        // branch, where no loop runs.
        let mut tools_requested: u32 = 0;
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
            // its pinned generation; a drifted retained set is SERVED (a
            // wholesale denial would kill the round for an unrelated
            // catalog mutation) and never silently replaced by a fresh
            // default-core mint. Only callers with no retained handle at
            // all fall back to a fresh default-core set with zero
            // activations. The audit trail is per tool now: each drifted
            // member's call is denied typed inside the loop by
            // `ExecutionGate::authorize`, which emits a tracing::warn per
            // drift denial/survival (see tools/activation.rs).
            let session_tools = crate::tools::activation::route_session_set(
                session_tool_set,
                registry.catalog(),
                || tool_session_id.cloned().unwrap_or_else(local_gate_session),
            );
            // The configured limits: this loop runs our own tools (bash among
            // them) with no runtime in reach.
            let tool_limits = ToolLimits::configured();
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
                        session_allow_all: None,
                        launch_cancel: None,
                        memory_backend: memory_backend.cloned(),
                        telemetry: None,
                        // Deliberately none: this route carries no session
                        // continuation, so `context_checkpoint` fails typed
                        // here instead of reaching any other state.
                        continuation: None,
                        watcher_exit_path: None,
                        tool_register_tx: None,
                        session_manager: None,
                        subagent_registry: None,
                        event_queue: None,
                        delegation_parent: None,
                        codex_parent_plan: None,
                        worker_context: None,
                        secret_prompt: None,
                        orchestration: None,
                        tool_activation: None,
                        mcp_leases: None,
                        extension_leases: None,
                        memory_context: None,
                        cwd: None,
                        env: None,
                        env_stripped: Vec::new(),
                        env_warned: Default::default(),
                        sandbox: sandbox.cloned(),
                        session_id: None,
                        tool_use_id: None,
                        turn_correlation_id: None,
                        spend: None,
                    },
                    limits: tool_limits,
                },
                tool_limits.max_tool_output,
                8,
                &mut tools_requested,
                // Each interior call books itself; this outer result is never
                // booked again.
                spend,
            )
            .await
        } else {
            let result = handler.provider_complete(params).await;
            // The call returned and was paid: book its usage now, before the
            // cancel check below could discard it.
            spend.book_returned(result.as_ref().ok().and_then(|c| c.usage.as_ref()));
            result
        };
        if cancel.is_cancelled() {
            attempt.finish_canceled();
            emit_audit(false, "error", Some("canceled"), tools_requested);
            return Err("operation canceled".into());
        }
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
                emit_audit(false, "ok", None, tools_requested);
                Ok(serde_json::json!({
                    "content": complete.content,
                    "stop_reason": complete.stop_reason.unwrap_or_else(|| "end_turn".to_string()),
                    "usage": complete.usage.unwrap_or_else(|| serde_json::json!({}))
                }))
            }
            Err(e) => {
                attempt.finish_failed(trace_ext::EXTENSION_PROVIDER_ERROR_CODE);
                emit_audit(false, "error", Some("provider_error"), tools_requested);
                Err(format!("extension provider: {e}").into())
            }
        }
    }
}
