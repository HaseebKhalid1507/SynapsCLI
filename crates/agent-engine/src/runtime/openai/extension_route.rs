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
use crate::tools::TurnToolContext;

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
///
/// `tool_context` is what the turn's tool calls are given: the one the
/// caller's own tool loop makes its contexts from, so the tools of this
/// loop run in the chat's folder with the chat's environment, act on the
/// chat's project, and the hook events and `tool.call`s they cause name the
/// conversation and the folder. `None` (internal callers) = a call for no
/// chat ([`TurnToolContext::no_chat`]). The internal callers offer no tools
/// (compaction), and a request that offers none runs none: the loop is
/// entered only when `tools_schema` is not empty.
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
    tool_context: Option<&TurnToolContext>,
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
        // Sound because the interior loop's ToolContext carries no
        // `tool_activation` (a `TurnToolContext` never does; only a stream
        // dispatch adds it), so `activate_tools` cannot mutate the retained
        // session set mid-loop — activations only surface on the next OUTER
        // stream round, which re-projects. If activation capability is ever
        // wired into this route, `tools` must be recomputed per interior
        // round from the session set instead.
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
        // registry exists and tools were offered). One outer turn == one
        // transport attempt: the tool-loop helper may perform several
        // interior provider.complete calls, which are deliberately not
        // counted as separate attempts (see `trace::extension` module docs).
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
        // The tool loop runs what the model was offered. A request that
        // offers no tools (compaction's) gets one provider call and nothing
        // in its answer is run: such a request is no turn and has no tool
        // context, so a tool call would run for no chat, in the process
        // folder with the process environment.
        let result = if let Some(tools) = tools_shared.filter(|_| tools_exposed) {
            // A snapshot, not the lock: the loop's tools are the chat's and
            // may read this registry themselves (starting a worker does),
            // which under a read guard held for the whole provider round
            // would wait for ever behind any writer that queued meanwhile.
            let registry = tools.read().await.clone();
            // ═══ EXECUTION GATE (Task 16/17, spec §7.1) ═══ ONE session
            // tool set for the whole interior tool loop, resolved here
            // against the SAME registry snapshot the loop uses for the
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
            let turn_tools = tool_context.cloned().unwrap_or_else(TurnToolContext::no_chat);
            // Raced against the turn's cancel as the stream loop races each
            // tool call: a stop drops the round, the tool call it is running
            // included, and ends below as a cancelled turn. `None` = cancelled.
            let looped = crate::runtime::stream::await_tool_call(
                cancel,
                // Boxed: inline, the loop's future made the turn's future
                // too large for a tokio worker's stack in a debug build.
                Box::pin(crate::extensions::runtime::process::complete_provider_with_tools(
                    handler.clone(),
                    params,
                    &registry,
                    &session_tools,
                    &hook_bus,
                    // A call's context is made from the turn's, like a call of
                    // the caller's own loop, the turn's event channel included
                    // (never this route's `tx`: under usage metering that is a
                    // relay which ends with the round, and a worker started
                    // here reports for longer). No delta lane: these calls are
                    // not announced to the frontend, so partial output would
                    // have no tool call to belong to.
                    || turn_tools.for_call(None),
                    turn_tools.limits().max_tool_output,
                    8,
                    &mut tools_requested,
                )),
            )
            .await
            .0;
            match looped {
                Some(result) => result,
                None => Err("operation canceled".to_string()),
            }
        } else {
            handler.provider_complete(params).await
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

#[cfg(test)]
mod cancel_tests {
    use super::*;
    use crate::extensions::hooks::events::{HookEvent, HookResult};
    use crate::extensions::runtime::process::{
        ProviderCompleteParams, ProviderCompleteResult, RegisteredProviderModelSpec,
        RegisteredProviderSpec,
    };
    use crate::extensions::runtime::ExtensionHandler;
    use serde_json::json;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    /// Asks for the `sleeper` tool on every round.
    struct AsksForSleeper;

    #[async_trait::async_trait]
    impl ExtensionHandler for AsksForSleeper {
        fn id(&self) -> &str {
            "asks-for-sleeper"
        }
        async fn handle(&self, _event: &HookEvent) -> HookResult {
            HookResult::Continue
        }
        async fn shutdown(&self) {}
        async fn provider_complete(
            &self,
            _params: ProviderCompleteParams,
        ) -> Result<ProviderCompleteResult, String> {
            Ok(ProviderCompleteResult {
                content: vec![json!({
                    "type": "tool_use", "id": "t1", "name": "sleeper", "input": {}
                })],
                stop_reason: Some("tool_use".to_string()),
                usage: None,
            })
        }
    }

    /// A tool that takes 30 s, and says when it has started.
    struct Sleeper(Arc<tokio::sync::Notify>);

    #[async_trait::async_trait]
    impl crate::Tool for Sleeper {
        fn name(&self) -> &str {
            "sleeper"
        }
        fn description(&self) -> &str {
            "sleeps"
        }
        fn parameters(&self) -> Value {
            json!({"type": "object"})
        }
        fn origin(&self) -> crate::tools::ToolOrigin {
            crate::tools::ToolOrigin::Builtin
        }
        async fn execute(&self, _params: Value, _ctx: crate::ToolContext) -> crate::Result<String> {
            self.0.notify_one();
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok("woke".to_string())
        }
    }

    /// A stop while the extension model's tool loop runs a tool ends the
    /// turn at once as cancelled; it does not wait for the tool.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn a_running_tool_call_in_the_extension_loop_is_cancelled() {
        let _base = crate::test_env::BaseDirGuard::new();
        let started = Arc::new(tokio::sync::Notify::new());
        let mut registry = crate::ToolRegistry::new();
        registry.register(Arc::new(Sleeper(started.clone())));
        let mut manager = ExtensionManager::new_with_tools(
            Arc::new(crate::extensions::hooks::HookBus::new()),
            Arc::new(tokio::sync::RwLock::new(registry)),
        );
        manager
            .register_provider_handler_for_tests(
                "plug",
                RegisteredProviderSpec {
                    id: "prov".to_string(),
                    display_name: "Fake".to_string(),
                    description: "fake provider".to_string(),
                    models: vec![RegisteredProviderModelSpec {
                        id: "model-x".to_string(),
                        display_name: None,
                        capabilities: json!({"streaming": false, "tool_use": true}),
                        context_window: None,
                    }],
                    config_schema: None,
                },
                Arc::new(AsksForSleeper),
            )
            .unwrap();
        let manager = Arc::new(tokio::sync::RwLock::new(manager));
        let tools = Arc::new(vec![json!({
            "name": "sleeper", "description": "sleeps", "input_schema": {"type": "object"}
        })]);
        let messages: Vec<crate::SharedMessage> =
            vec![Arc::new(json!({"role": "user", "content": "go"}))];
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let trace = crate::runtime::trace::TraceContext::disabled();

        let route = route_extension_provider(
            manager,
            "plug",
            "prov",
            "model-x",
            "plug:prov:model-x",
            &tools,
            &None,
            &messages,
            &tx,
            None,
            None,
            0,
            &cancel,
            None,
            None,
            None,
            &trace,
        );
        let stop = async {
            tokio::time::timeout(Duration::from_secs(10), started.notified())
                .await
                .expect("the tool never started");
            cancel.cancel();
            std::time::Instant::now()
        };
        let (result, stopped_at) = tokio::time::timeout(
            Duration::from_secs(10),
            async { tokio::join!(route, stop) },
        )
        .await
        .expect("the loop waited for the tool");
        let waited = stopped_at.elapsed();
        assert!(waited < Duration::from_secs(1), "returned {waited:?} after the stop");
        let error = result.expect_err("a stopped turn is not a success").to_string();
        assert_eq!(error, "operation canceled");
    }
}

/// `caller` in the extension model's tool loop: a tool the model of a
/// `plugin:provider:model` chat asks for is called with the turn's own tool
/// context, so an extension tool among them is told the author of that
/// chat's binding.
#[cfg(test)]
mod caller_tests {
    use super::*;
    use crate::extensions::hooks::events::{HookEvent, HookResult};
    use crate::extensions::runtime::process::{
        ProviderCompleteParams, ProviderCompleteResult, RegisteredExtensionToolSpec,
        RegisteredProviderModelSpec, RegisteredProviderSpec,
    };
    use crate::extensions::runtime::ExtensionHandler;
    use crate::memory_backend::MemoryBinding;
    use crate::tools::test_helpers::{create_tool_context, memory_binding};
    use agent_core::config::MemoryBackendKind;
    use agent_core::memory::forum::Author;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use tokio_util::sync::CancellationToken;

    /// A model that asks for `whoami` once a turn, then answers.
    #[derive(Default)]
    struct AsksWhoOnce(AtomicUsize);

    #[async_trait::async_trait]
    impl ExtensionHandler for AsksWhoOnce {
        fn id(&self) -> &str {
            "asks-who"
        }
        async fn handle(&self, _event: &HookEvent) -> HookResult {
            HookResult::Continue
        }
        async fn shutdown(&self) {}
        async fn provider_complete(
            &self,
            _params: ProviderCompleteParams,
        ) -> Result<ProviderCompleteResult, String> {
            let asks = self.0.fetch_add(1, Ordering::SeqCst) % 2 == 0;
            Ok(ProviderCompleteResult {
                content: vec![if asks {
                    json!({"type": "tool_use", "id": "t1", "name": "whoami", "input": {}})
                } else {
                    json!({"type": "text", "text": "done"})
                }],
                stop_reason: Some(if asks { "tool_use" } else { "end_turn" }.to_string()),
                usage: None,
            })
        }
    }

    /// The extension behind `whoami`: `(session_id, caller)` of each
    /// `tool.call`, as `ProcessExtension` would put them on the wire.
    #[derive(Default)]
    struct Recorder(Mutex<Vec<(Option<String>, Option<Author>)>>);

    #[async_trait::async_trait]
    impl ExtensionHandler for Recorder {
        fn id(&self) -> &str {
            "caller-recorder"
        }
        async fn handle(&self, _event: &HookEvent) -> HookResult {
            HookResult::Continue
        }
        async fn call_tool_as(
            &self,
            _name: &str,
            _input: Value,
            session_id: Option<&str>,
            _cwd: Option<&std::path::Path>,
            caller: Option<&Author>,
        ) -> Result<Value, String> {
            self.0
                .lock()
                .unwrap()
                .push((session_id.map(str::to_string), caller.cloned()));
            Ok(json!("here"))
        }
        async fn shutdown(&self) {}
    }

    /// The real `ExtensionTool` under the name the model asks for, with a
    /// built-in origin so the loop's default tool set admits it.
    struct Admitted(crate::tools::ExtensionTool);

    #[async_trait::async_trait]
    impl crate::Tool for Admitted {
        fn name(&self) -> &str {
            "whoami"
        }
        fn description(&self) -> &str {
            "asks the recording extension who is calling"
        }
        fn parameters(&self) -> Value {
            json!({"type": "object"})
        }
        fn origin(&self) -> crate::tools::ToolOrigin {
            crate::tools::ToolOrigin::Builtin
        }
        async fn execute(&self, params: Value, ctx: crate::ToolContext) -> crate::Result<String> {
            self.0.execute(params, ctx).await
        }
    }

    async fn one_turn(
        manager: Arc<tokio::sync::RwLock<ExtensionManager>>,
        tools: &Arc<Vec<Value>>,
        messages: &[crate::SharedMessage],
        trace: &crate::runtime::trace::TraceContext,
        chat: &str,
        binding: MemoryBinding,
    ) {
        let mut capabilities = create_tool_context().capabilities;
        capabilities.memory_backend = Some(binding);
        capabilities.session_id = Some(chat.to_string());
        let turn_tools =
            TurnToolContext::new(capabilities, TurnToolContext::no_chat().limits().clone(), None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let routed = route_extension_provider(
            manager,
            "plug",
            "prov",
            "model-x",
            "plug:prov:model-x",
            tools,
            &None,
            messages,
            &tx,
            None,
            None,
            0,
            &cancel,
            None,
            None,
            Some(&turn_tools),
            trace,
        )
        .await;
        assert!(routed.is_ok(), "{chat}: {:?}", routed.err().map(|e| e.to_string()));
    }

    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn a_tool_call_in_the_extension_loop_names_the_turns_own_author() {
        let base = crate::test_env::BaseDirGuard::new();
        let recorder = Arc::new(Recorder::default());
        let mut registry = crate::ToolRegistry::new();
        registry.register(Arc::new(Admitted(crate::tools::ExtensionTool::new(
            "caller-recorder",
            RegisteredExtensionToolSpec {
                name: "whoami".into(),
                description: "records the caller".into(),
                input_schema: json!({"type": "object"}),
            },
            recorder.clone(),
        ))));
        let mut manager = ExtensionManager::new_with_tools(
            Arc::new(crate::extensions::hooks::HookBus::new()),
            Arc::new(tokio::sync::RwLock::new(registry)),
        );
        manager
            .register_provider_handler_for_tests(
                "plug",
                RegisteredProviderSpec {
                    id: "prov".to_string(),
                    display_name: "Fake".to_string(),
                    description: "fake provider".to_string(),
                    models: vec![RegisteredProviderModelSpec {
                        id: "model-x".to_string(),
                        display_name: None,
                        capabilities: json!({"streaming": false, "tool_use": true}),
                        context_window: None,
                    }],
                    config_schema: None,
                },
                Arc::new(AsksWhoOnce::default()),
            )
            .unwrap();
        let manager = Arc::new(tokio::sync::RwLock::new(manager));
        let tools = Arc::new(vec![json!({
            "name": "whoami", "description": "who", "input_schema": {"type": "object"}
        })]);
        let messages: Vec<crate::SharedMessage> =
            vec![Arc::new(json!({"role": "user", "content": "go"}))];
        let trace = crate::runtime::trace::TraceContext::disabled();

        // One turn of a chat with `binding`, on the extension's model.
        let turn = |chat: &'static str, binding: MemoryBinding| {
            one_turn(manager.clone(), &tools, &messages, &trace, chat, binding)
        };
        let dirs = [base.path().join("a"), base.path().join("b")];
        let bindings = dirs
            .each_ref()
            .map(|dir| memory_binding(MemoryBackendKind::Legacy, dir));
        turn("chat-a", bindings[0].clone()).await;
        turn("chat-b", bindings[1].clone()).await;
        turn("chat-a", bindings[0].clone()).await;

        let calls = recorder.0.lock().unwrap().clone();
        assert_eq!(calls.len(), 3, "{calls:?}");
        for ((session_id, caller), (chat, binding)) in
            calls.iter().zip([("chat-a", &bindings[0]), ("chat-b", &bindings[1]), ("chat-a", &bindings[0])])
        {
            assert_eq!(session_id.as_deref(), Some(chat));
            let caller = caller.as_ref().unwrap_or_else(|| panic!("{chat}: no caller"));
            caller.validate().unwrap();
            assert_eq!(caller, binding.forum_author(), "{chat}: its own binding's author");
        }
        let (of_a, of_b) = (calls[0].1.clone().unwrap(), calls[1].1.clone().unwrap());
        assert_ne!(of_a.actor, of_b.actor);
        assert_ne!(of_a.group, of_b.group);
    }
}
