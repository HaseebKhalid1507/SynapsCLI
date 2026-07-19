//! Task 10C wiring tests: the extension-provider route emits exactly one
//! schema-valid `TransportKind::Extension` record per actual provider IPC
//! turn, with a `None` wire section, reserved static endpoint identity, and
//! no extension-controlled content. All handlers are in-process fakes — no
//! extension processes, no real network.
//!
//! Tests that route serialize on a shared mutex and pin the Synaps base dir
//! to a per-test tempdir, because the trust gate and the audit trail read
//! and write files under the process-global base dir.

use super::extension::{
    EXTENSION_COMPLETE_PATH, EXTENSION_ENDPOINT_HOST, EXTENSION_PROVIDER_ERROR_CODE,
    EXTENSION_STREAM_PATH,
};
use super::{
    CollectingTraceSink, RequestTrace, StopReason, TraceContext, TranslationAction,
    TranslationElement, TransportKind, UsageProvenance,
};
use crate::extensions::hooks::events::{HookEvent, HookResult};
use crate::extensions::hooks::HookBus;
use crate::extensions::manager::ExtensionManager;
use crate::extensions::runtime::process::{
    ProviderCompleteParams, ProviderCompleteResult, ProviderStreamEvent,
    RegisteredProviderModelSpec, RegisteredProviderSpec,
};
use crate::extensions::runtime::ExtensionHandler;
use crate::runtime::openai::extension_route::route_extension_provider;
use agent_core::TurnOutcome;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

const SENTINEL: &str = "EXTENSION_TRACE_SENTINEL_9e4b_never_persist";

/// Serializes tests that mutate the process-global base dir (trust + audit).
static BASE_DIR_LOCK: Mutex<()> = Mutex::new(());

// ═══ Fake in-process provider handler ═══════════════════════════════════════

#[derive(Default)]
struct FakeProvider {
    /// Scripted `provider.complete` outcomes, popped per call.
    complete_results: Mutex<Vec<Result<ProviderCompleteResult, String>>>,
    /// Events forwarded to the stream sink before the stream result.
    stream_events: Vec<ProviderStreamEvent>,
    /// Scripted `provider.stream` final outcome.
    stream_result: Mutex<Option<Result<ProviderCompleteResult, String>>>,
    /// When true, `provider.stream` never resolves (cancel-mid-IPC tests).
    hang_stream: bool,
    /// Cancelled by the fake *during* `provider.complete`, modeling a user
    /// cancellation that lands while the IPC turn is active.
    cancel_during_complete: Option<CancellationToken>,
    complete_calls: AtomicUsize,
    stream_calls: AtomicUsize,
}

#[async_trait]
impl ExtensionHandler for FakeProvider {
    fn id(&self) -> &str {
        "fake-provider"
    }

    async fn handle(&self, _event: &HookEvent) -> HookResult {
        HookResult::Continue
    }

    async fn shutdown(&self) {}

    async fn provider_complete(
        &self,
        _params: ProviderCompleteParams,
    ) -> Result<ProviderCompleteResult, String> {
        self.complete_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(token) = &self.cancel_during_complete {
            token.cancel();
        }
        let mut script = self.complete_results.lock().unwrap();
        if script.is_empty() {
            return Err(format!("unscripted provider.complete {SENTINEL}"));
        }
        script.remove(0)
    }

    async fn provider_stream(
        &self,
        _params: ProviderCompleteParams,
        sink: tokio::sync::mpsc::UnboundedSender<ProviderStreamEvent>,
    ) -> Result<ProviderCompleteResult, String> {
        self.stream_calls.fetch_add(1, Ordering::SeqCst);
        if self.hang_stream {
            std::future::pending::<()>().await;
        }
        for event in &self.stream_events {
            let _ = sink.send(event.clone());
        }
        self.stream_result
            .lock()
            .unwrap()
            .take()
            .unwrap_or_else(|| Err(format!("unscripted provider.stream {SENTINEL}")))
    }
}

fn ok_result(stop_reason: &str) -> ProviderCompleteResult {
    ProviderCompleteResult {
        content: vec![json!({"type": "text", "text": format!("{SENTINEL} response")})],
        stop_reason: Some(stop_reason.to_string()),
        usage: Some(json!({
            "input_tokens": 3,
            "output_tokens": 5,
            "note": SENTINEL,
        })),
    }
}

// ═══ Harness ════════════════════════════════════════════════════════════════

struct Harness {
    sink: Arc<CollectingTraceSink>,
    trace: TraceContext,
    _tmp: tempfile::TempDir,
}

/// Pins the base dir to a fresh tempdir (caller must hold `BASE_DIR_LOCK`)
/// and builds a collecting trace context keyed inside the same tempdir.
fn harness() -> Harness {
    let tmp = tempfile::TempDir::new().unwrap();
    crate::config::set_base_dir_for_tests(tmp.path().to_path_buf());
    let key_path = tmp.path().join("trace").join("digest.key");
    let sink = CollectingTraceSink::new();
    let trace = TraceContext::with_sink(sink.clone()).with_key_path(key_path);
    Harness {
        sink,
        trace,
        _tmp: tmp,
    }
}

fn provider_spec(streaming: bool, tool_use: bool) -> RegisteredProviderSpec {
    RegisteredProviderSpec {
        id: "prov".to_string(),
        display_name: "Fake".to_string(),
        description: "fake provider".to_string(),
        models: vec![RegisteredProviderModelSpec {
            id: "model-x".to_string(),
            display_name: None,
            capabilities: json!({"streaming": streaming, "tool_use": tool_use}),
            context_window: None,
        }],
        config_schema: None,
    }
}

async fn manager_with(
    handler: Arc<FakeProvider>,
    streaming: bool,
    tool_use: bool,
    with_tools: bool,
) -> Arc<tokio::sync::RwLock<ExtensionManager>> {
    let hook_bus = Arc::new(HookBus::new());
    let mut manager = if with_tools {
        ExtensionManager::new_with_tools(
            hook_bus,
            Arc::new(tokio::sync::RwLock::new(crate::ToolRegistry::new())),
        )
    } else {
        ExtensionManager::new(hook_bus)
    };
    manager
        .register_provider_handler_for_tests("plug", provider_spec(streaming, tool_use), handler)
        .unwrap();
    Arc::new(tokio::sync::RwLock::new(manager))
}

fn messages() -> Vec<crate::SharedMessage> {
    vec![Arc::new(json!({
        "role": "user",
        "content": [{"type": "text", "text": format!("{SENTINEL} hello")}],
    }))]
}

#[allow(clippy::type_complexity)]
async fn route(
    manager: &Arc<tokio::sync::RwLock<ExtensionManager>>,
    trace: &TraceContext,
    cancel: &CancellationToken,
    tools_schema: Arc<Vec<Value>>,
) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    route_extension_provider(
        manager.clone(),
        "plug",
        "prov",
        "model-x",
        "plug:prov:model-x",
        &tools_schema,
        &Some(format!("{SENTINEL} system")),
        &messages(),
        &tx,
        None,
        None,
        0,
        cancel,
        None,
        None,
        trace,
    )
    .await
}

fn assert_schema_valid_and_content_free(records: &[RequestTrace]) {
    for record in records {
        let json = serde_json::to_string(record).expect("record serializes");
        assert!(
            !json.contains(SENTINEL),
            "raw content leaked into serialized trace: {json}"
        );
        let back: RequestTrace = serde_json::from_str(&json).expect("record re-validates on read");
        assert_eq!(&back, record, "record must round-trip deterministically");
    }
}

fn assert_extension_identity(record: &RequestTrace, path: &str) {
    assert_eq!(record.transport, TransportKind::Extension);
    assert_eq!(record.endpoint.host(), EXTENSION_ENDPOINT_HOST);
    assert_eq!(record.endpoint.path(), path);
    assert_eq!(record.model.as_str(), "plug:prov/model-x");
    assert!(
        record.wire.is_none(),
        "extension records never claim wire bytes"
    );
    assert_eq!(record.attempt, 1);
    assert!(record.outcome.retries.is_empty());
    assert!(record.outcome.http_status.is_none());
    assert!(record.outcome.provider_request_id.is_none());
    assert!(record.outcome.timings.send_start_unix_ms.is_some());
    assert!(record.outcome.timings.stream_end_ms.is_some());
    assert!(record.outcome.timings.headers_ms.is_none());
    assert!(record.outcome.timings.first_byte_ms.is_none());
}

// ═══ Streaming path ═════════════════════════════════════════════════════════

#[tokio::test]
async fn streaming_success_emits_one_schema_valid_extension_record() {
    let _guard = BASE_DIR_LOCK.lock().unwrap();
    let h = harness();
    let handler = Arc::new(FakeProvider {
        stream_events: vec![
            ProviderStreamEvent::ThinkingDelta {
                text: format!("{SENTINEL} thinking"),
            },
            ProviderStreamEvent::TextDelta {
                text: format!("{SENTINEL} delta"),
            },
            ProviderStreamEvent::Usage {
                usage: json!({"input_tokens": 3, "output_tokens": 5}),
            },
            ProviderStreamEvent::Done,
        ],
        stream_result: Mutex::new(Some(Ok(ok_result("end_turn")))),
        ..Default::default()
    });
    let manager = manager_with(handler.clone(), true, false, false).await;

    let cancel = CancellationToken::new();
    let value = route(&manager, &h.trace, &cancel, Arc::new(Vec::new()))
        .await
        .expect("stream turn succeeds");
    assert_eq!(value["stop_reason"], "end_turn");

    assert_eq!(handler.stream_calls.load(Ordering::SeqCst), 1);
    assert_eq!(handler.complete_calls.load(Ordering::SeqCst), 0);
    let records = h.sink.records();
    assert_eq!(records.len(), 1, "exactly one record per stream IPC turn");
    let record = &records[0];
    assert_extension_identity(record, EXTENSION_STREAM_PATH);
    assert_eq!(record.outcome.terminal, TurnOutcome::Completed);
    assert_eq!(record.outcome.stop_reason, Some(StopReason::EndTurn));
    let usage = record.outcome.usage.expect("provider-reported usage");
    assert_eq!(usage.provenance, UsageProvenance::ProviderReported);
    assert_eq!(usage.input_tokens, Some(3));
    assert_eq!(usage.output_tokens, Some(5));
    assert!(
        record.outcome.timings.first_model_event_ms.is_some(),
        "first stream delta must mark the first model event"
    );
    assert_eq!(record.anatomy.message_count, 1);
    assert_eq!(record.anatomy.tool_count, 0);
    assert!(record.translation_losses.is_empty());
    assert_schema_valid_and_content_free(&records);
}

#[tokio::test]
async fn streaming_extension_error_emits_static_code_only() {
    let _guard = BASE_DIR_LOCK.lock().unwrap();
    let h = harness();
    let handler = Arc::new(FakeProvider {
        stream_result: Mutex::new(Some(Err(format!("{SENTINEL} exploded")))),
        ..Default::default()
    });
    let manager = manager_with(handler, true, false, false).await;

    let cancel = CancellationToken::new();
    let err = route(&manager, &h.trace, &cancel, Arc::new(Vec::new()))
        .await
        .expect_err("stream turn fails");
    assert!(err.to_string().contains("extension provider"));

    let records = h.sink.records();
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_extension_identity(record, EXTENSION_STREAM_PATH);
    match &record.outcome.terminal {
        TurnOutcome::ProviderFailed {
            code,
            correlation_id,
        } => {
            assert_eq!(code, EXTENSION_PROVIDER_ERROR_CODE);
            assert_eq!(correlation_id, record.request_id.as_str());
        }
        other => panic!("expected ProviderFailed, got {other:?}"),
    }
    assert!(record.outcome.stop_reason.is_none());
    assert!(record.outcome.usage.is_none());
    assert_schema_valid_and_content_free(&records);
}

#[tokio::test]
async fn streaming_cancel_while_ipc_active_emits_canceled_record() {
    let _guard = BASE_DIR_LOCK.lock().unwrap();
    let h = harness();
    let handler = Arc::new(FakeProvider {
        hang_stream: true,
        ..Default::default()
    });
    let manager = manager_with(handler.clone(), true, false, false).await;

    let cancel = CancellationToken::new();
    let canceller = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        canceller.cancel();
    });
    let err = route(&manager, &h.trace, &cancel, Arc::new(Vec::new()))
        .await
        .expect_err("canceled turn errors");
    assert!(err.to_string().contains("canceled"));

    assert_eq!(handler.stream_calls.load(Ordering::SeqCst), 1);
    let records = h.sink.records();
    assert_eq!(records.len(), 1, "cancel mid-IPC still records the attempt");
    let record = &records[0];
    assert_extension_identity(record, EXTENSION_STREAM_PATH);
    assert_eq!(record.outcome.terminal, TurnOutcome::Canceled);
    assert!(record.outcome.timings.first_model_event_ms.is_none());
    assert_schema_valid_and_content_free(&records);
}

// ═══ Non-streaming path ═════════════════════════════════════════════════════

#[tokio::test]
async fn nonstreaming_success_emits_complete_record_with_merge_loss() {
    let _guard = BASE_DIR_LOCK.lock().unwrap();
    let h = harness();
    let handler = Arc::new(FakeProvider {
        complete_results: Mutex::new(vec![Ok(ok_result("max_tokens"))]),
        ..Default::default()
    });
    let manager = manager_with(handler.clone(), false, false, false).await;

    let cancel = CancellationToken::new();
    route(&manager, &h.trace, &cancel, Arc::new(Vec::new()))
        .await
        .expect("complete turn succeeds");

    assert_eq!(handler.complete_calls.load(Ordering::SeqCst), 1);
    let records = h.sink.records();
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_extension_identity(record, EXTENSION_COMPLETE_PATH);
    assert_eq!(record.outcome.terminal, TurnOutcome::Completed);
    assert_eq!(record.outcome.stop_reason, Some(StopReason::MaxTokens));
    assert!(
        record.outcome.timings.first_model_event_ms.is_none(),
        "no stream events observed — never fabricated"
    );
    // The display path joins response text blocks: one structural Merged entry.
    assert_eq!(record.translation_losses.len(), 1);
    assert_eq!(
        record.translation_losses[0].action,
        TranslationAction::Merged
    );
    assert_eq!(
        record.translation_losses[0].element,
        TranslationElement::MessageBlock
    );
    assert_schema_valid_and_content_free(&records);
}

#[tokio::test]
async fn nonstreaming_extension_error_emits_failed_record() {
    let _guard = BASE_DIR_LOCK.lock().unwrap();
    let h = harness();
    let handler = Arc::new(FakeProvider {
        complete_results: Mutex::new(vec![Err(format!("{SENTINEL} broke"))]),
        ..Default::default()
    });
    let manager = manager_with(handler, false, false, false).await;

    let cancel = CancellationToken::new();
    route(&manager, &h.trace, &cancel, Arc::new(Vec::new()))
        .await
        .expect_err("complete turn fails");

    let records = h.sink.records();
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_extension_identity(record, EXTENSION_COMPLETE_PATH);
    match &record.outcome.terminal {
        TurnOutcome::ProviderFailed { code, .. } => {
            assert_eq!(code, EXTENSION_PROVIDER_ERROR_CODE)
        }
        other => panic!("expected ProviderFailed, got {other:?}"),
    }
    assert_schema_valid_and_content_free(&records);
}

#[tokio::test]
async fn nonstreaming_cancel_during_ipc_emits_canceled_record() {
    let _guard = BASE_DIR_LOCK.lock().unwrap();
    let h = harness();
    let cancel = CancellationToken::new();
    let handler = Arc::new(FakeProvider {
        complete_results: Mutex::new(vec![Ok(ok_result("end_turn"))]),
        cancel_during_complete: Some(cancel.clone()),
        ..Default::default()
    });
    let manager = manager_with(handler.clone(), false, false, false).await;

    let err = route(&manager, &h.trace, &cancel, Arc::new(Vec::new()))
        .await
        .expect_err("cancellation observed after IPC returns");
    assert!(err.to_string().contains("canceled"));

    assert_eq!(handler.complete_calls.load(Ordering::SeqCst), 1);
    let records = h.sink.records();
    assert_eq!(records.len(), 1);
    assert_extension_identity(&records[0], EXTENSION_COMPLETE_PATH);
    assert_eq!(records[0].outcome.terminal, TurnOutcome::Canceled);
    assert_schema_valid_and_content_free(&records);
}

// ═══ Gate ordering: no record before the actual IPC ═════════════════════════

#[tokio::test]
async fn trust_disabled_provider_makes_zero_ipc_and_zero_records() {
    let _guard = BASE_DIR_LOCK.lock().unwrap();
    let h = harness();
    let mut trust = crate::extensions::trust::ProviderTrustState::default();
    crate::extensions::trust::disable_provider(&mut trust, "plug:prov", None);
    crate::extensions::trust::save_trust_state(&trust).unwrap();

    let handler = Arc::new(FakeProvider::default());
    let manager = manager_with(handler.clone(), true, false, false).await;

    let cancel = CancellationToken::new();
    let err = route(&manager, &h.trace, &cancel, Arc::new(Vec::new()))
        .await
        .expect_err("disabled provider is blocked");
    assert!(err.to_string().contains("disabled by user trust settings"));

    assert_eq!(handler.stream_calls.load(Ordering::SeqCst), 0);
    assert_eq!(handler.complete_calls.load(Ordering::SeqCst), 0);
    assert!(h.sink.records().is_empty(), "blocked turn emits no record");
}

#[tokio::test]
async fn unavailable_provider_emits_no_record() {
    let _guard = BASE_DIR_LOCK.lock().unwrap();
    let h = harness();
    let manager = Arc::new(tokio::sync::RwLock::new(ExtensionManager::new(Arc::new(
        HookBus::new(),
    ))));

    let cancel = CancellationToken::new();
    let err = route(&manager, &h.trace, &cancel, Arc::new(Vec::new()))
        .await
        .expect_err("unknown provider fails");
    assert!(err.to_string().contains("is not available"));
    assert!(h.sink.records().is_empty());
}

#[tokio::test]
async fn cancelled_before_start_emits_no_record_and_no_ipc() {
    let _guard = BASE_DIR_LOCK.lock().unwrap();
    let h = harness();
    let handler = Arc::new(FakeProvider::default());
    let manager = manager_with(handler.clone(), true, false, false).await;

    let cancel = CancellationToken::new();
    cancel.cancel();
    let err = route(&manager, &h.trace, &cancel, Arc::new(Vec::new()))
        .await
        .expect_err("pre-cancelled turn errors");
    assert!(err.to_string().contains("canceled"));

    assert_eq!(handler.stream_calls.load(Ordering::SeqCst), 0);
    assert_eq!(handler.complete_calls.load(Ordering::SeqCst), 0);
    assert!(h.sink.records().is_empty());
}

// ═══ Tool loop: one outer turn, one record ══════════════════════════════════

#[tokio::test]
async fn tool_loop_with_multiple_interior_calls_emits_exactly_one_record() {
    let _guard = BASE_DIR_LOCK.lock().unwrap();
    let h = harness();
    let handler = Arc::new(FakeProvider {
        complete_results: Mutex::new(vec![
            Ok(ProviderCompleteResult {
                content: vec![json!({
                    "type": "tool_use",
                    "id": "t1",
                    "name": "not_a_registered_tool",
                    "input": {"query": SENTINEL},
                })],
                stop_reason: Some("tool_use".to_string()),
                usage: None,
            }),
            Ok(ok_result("end_turn")),
        ]),
        ..Default::default()
    });
    let manager = manager_with(handler.clone(), false, true, true).await;

    let tools: Arc<Vec<Value>> = Arc::new(vec![json!({
        "name": "not_a_registered_tool",
        "description": SENTINEL,
        "input_schema": {"type": "object"},
    })]);
    let cancel = CancellationToken::new();
    route(&manager, &h.trace, &cancel, tools)
        .await
        .expect("tool loop completes");

    assert_eq!(
        handler.complete_calls.load(Ordering::SeqCst),
        2,
        "the tool loop makes two interior provider.complete calls"
    );
    let records = h.sink.records();
    assert_eq!(
        records.len(),
        1,
        "one outer extension turn == one transport attempt record"
    );
    let record = &records[0];
    assert_extension_identity(record, EXTENSION_COMPLETE_PATH);
    assert_eq!(record.outcome.terminal, TurnOutcome::Completed);
    assert_eq!(record.outcome.stop_reason, Some(StopReason::EndTurn));
    assert_eq!(record.anatomy.tool_count, 1);
    // tool_use is supported here: no Unsupported entries, only the display merge.
    assert!(record
        .translation_losses
        .iter()
        .all(|l| l.action != TranslationAction::Unsupported));
    assert_schema_valid_and_content_free(&records);
}

// ═══ Capability-driven translation report ═══════════════════════════════════

#[tokio::test]
async fn tools_without_model_tool_use_report_unsupported_with_safe_ids_only() {
    let _guard = BASE_DIR_LOCK.lock().unwrap();
    let h = harness();
    let handler = Arc::new(FakeProvider {
        stream_result: Mutex::new(Some(Ok(ok_result("end_turn")))),
        ..Default::default()
    });
    let manager = manager_with(handler, true, false, false).await;

    let tools: Arc<Vec<Value>> = Arc::new(vec![
        json!({"name": "alpha_tool", "description": SENTINEL, "input_schema": {}}),
        json!({"name": format!("bad name {SENTINEL}!"), "description": "x", "input_schema": {}}),
    ]);
    let cancel = CancellationToken::new();
    route(&manager, &h.trace, &cancel, tools)
        .await
        .expect("stream turn succeeds");

    let records = h.sink.records();
    assert_eq!(records.len(), 1);
    let losses: Vec<_> = records[0]
        .translation_losses
        .iter()
        .filter(|l| l.action == TranslationAction::Unsupported)
        .collect();
    assert_eq!(losses.len(), 2, "each exposed tool is reported unsupported");
    assert!(losses.iter().all(|l| l.element == TranslationElement::Tool));
    assert_eq!(
        losses[0].element_id.as_ref().map(|id| id.as_str()),
        Some("alpha_tool")
    );
    assert!(
        losses[1].element_id.is_none(),
        "an unvalidated tool name is never copied into a record"
    );
    assert_schema_valid_and_content_free(&records);
}

// ═══ Disabled tracing never changes provider behavior ═══════════════════════

#[tokio::test]
async fn disabled_trace_context_routes_normally_with_zero_records() {
    let _guard = BASE_DIR_LOCK.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    crate::config::set_base_dir_for_tests(tmp.path().to_path_buf());
    let handler = Arc::new(FakeProvider {
        stream_result: Mutex::new(Some(Ok(ok_result("end_turn")))),
        stream_events: vec![ProviderStreamEvent::TextDelta {
            text: "hi".to_string(),
        }],
        ..Default::default()
    });
    let manager = manager_with(handler.clone(), true, false, false).await;

    let cancel = CancellationToken::new();
    let trace = TraceContext::disabled();
    let value = route(&manager, &trace, &cancel, Arc::new(Vec::new()))
        .await
        .expect("disabled tracing must not change routing");
    assert_eq!(value["stop_reason"], "end_turn");
    assert_eq!(handler.stream_calls.load(Ordering::SeqCst), 1);
}

// ═══ Pure helper coverage ═══════════════════════════════════════════════════

#[test]
fn stop_reason_mapping_is_closed_and_usage_parsing_is_typed() {
    use super::extension::{stop_reason_from_extension, usage_from_extension_value};
    assert_eq!(stop_reason_from_extension("end_turn"), StopReason::EndTurn);
    assert_eq!(stop_reason_from_extension("tool_use"), StopReason::ToolUse);
    assert_eq!(stop_reason_from_extension(SENTINEL), StopReason::Other);

    // Non-numeric or hostile shapes yield no usage — never copied values.
    assert!(usage_from_extension_value(None).is_none());
    assert!(usage_from_extension_value(Some(&json!("free text"))).is_none());
    assert!(usage_from_extension_value(Some(&json!({"note": SENTINEL}))).is_none());
    let usage =
        usage_from_extension_value(Some(&json!({"input_tokens": 7, "output_tokens": "nope"})))
            .expect("one valid metric suffices");
    assert_eq!(usage.input_tokens, Some(7));
    assert_eq!(usage.output_tokens, None);
}
