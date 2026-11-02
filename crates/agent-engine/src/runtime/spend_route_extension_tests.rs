//! Child spend on the extension provider route (P1d): the streamed call,
//! the direct `provider.complete` and the host tool loop. Extension usage
//! is reported in Anthropic field names; a streamed report is a snapshot
//! (the last with counts is kept, the call's own final usage last of all).
//! The provider is an in-process fake.
use super::*;

use crate::extensions::hooks::events::{HookEvent, HookResult};
use crate::extensions::hooks::HookBus;
use crate::extensions::manager::ExtensionManager;
use crate::extensions::runtime::process::{
    ProviderCompleteParams, ProviderCompleteResult, ProviderStreamEvent,
    RegisteredProviderModelSpec, RegisteredProviderSpec,
};
use crate::extensions::runtime::ExtensionHandler;
use async_trait::async_trait;

use crate::runtime::child_spend::RouteSpend;

const EXT: &str = "plug:prov:model-x";

type CallResult = Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>>;

/// Scripted results per call.
#[derive(Default)]
struct Provider {
    complete_results: Mutex<Vec<Result<ProviderCompleteResult, String>>>,
    stream_events: Vec<ProviderStreamEvent>,
    stream_result: Mutex<Option<Result<ProviderCompleteResult, String>>>,
    /// The stream stays open after its events.
    stall: bool,
    /// Cancelled just before a call returns: the turn is cancelled as the
    /// paid result comes back.
    cancel_on_return: Option<tokio_util::sync::CancellationToken>,
}

#[async_trait]
impl ExtensionHandler for Provider {
    fn id(&self) -> &str {
        "spend-provider"
    }

    async fn handle(&self, _event: &HookEvent) -> HookResult {
        HookResult::Continue
    }

    async fn shutdown(&self) {}

    async fn provider_complete(
        &self,
        _params: ProviderCompleteParams,
    ) -> Result<ProviderCompleteResult, String> {
        let next = {
            let mut script = self.complete_results.lock().unwrap();
            if script.is_empty() {
                Err("unscripted provider.complete".to_string())
            } else {
                script.remove(0)
            }
        };
        if let Some(cancel) = &self.cancel_on_return {
            cancel.cancel();
        }
        next
    }

    async fn provider_stream(
        &self,
        _params: ProviderCompleteParams,
        sink: tokio::sync::mpsc::Sender<ProviderStreamEvent>,
    ) -> Result<ProviderCompleteResult, String> {
        for event in &self.stream_events {
            let _ = sink.send(event.clone()).await;
        }
        if self.stall {
            std::future::pending::<()>().await;
        }
        if let Some(cancel) = &self.cancel_on_return {
            cancel.cancel();
        }
        self.stream_result
            .lock()
            .unwrap()
            .take()
            .unwrap_or_else(|| Err("unscripted provider.stream".into()))
    }
}

fn e_usage(input: u64, output: u64) -> serde_json::Value {
    json!({"input_tokens": input, "output_tokens": output})
}

fn reported(input: u64, output: u64) -> ProviderStreamEvent {
    ProviderStreamEvent::Usage {
        usage: e_usage(input, output),
    }
}

fn says(t: &str) -> ProviderStreamEvent {
    ProviderStreamEvent::TextDelta { text: t.into() }
}

fn answer(usage: Option<serde_json::Value>) -> ProviderCompleteResult {
    ProviderCompleteResult {
        content: vec![json!({"type": "text", "text": "spend ok"})],
        stop_reason: Some("end_turn".to_string()),
        usage,
    }
}

fn tool_call(input: u64, output: u64) -> ProviderCompleteResult {
    ProviderCompleteResult {
        content: vec![json!({
            "type": "tool_use",
            "id": "toolu_spend",
            "name": "not_a_registered_tool",
            "input": {"query": "spend"},
        })],
        stop_reason: Some("tool_use".to_string()),
        usage: Some(e_usage(input, output)),
    }
}

/// Extension counts as booked: input and output only.
fn e_tokens(input: u64, output: u64) -> SpendTokens {
    chat_tokens(input, output, 0)
}

async fn manager(
    handler: Arc<Provider>,
    streaming: bool,
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
    let spec = RegisteredProviderSpec {
        id: "prov".to_string(),
        display_name: "Spend".to_string(),
        description: "spend provider".to_string(),
        models: vec![RegisteredProviderModelSpec {
            id: "model-x".to_string(),
            display_name: None,
            capabilities: json!({"streaming": streaming, "tool_use": with_tools}),
            context_window: None,
        }],
        config_schema: None,
    };
    manager
        .register_provider_handler_for_tests("plug", spec, handler)
        .unwrap();
    Arc::new(tokio::sync::RwLock::new(manager))
}

/// How the call goes: streamed, a direct `provider.complete`, or the host
/// tool loop.
#[derive(Clone, Copy)]
enum Path {
    Stream,
    Complete,
    ToolLoop,
}

/// One extension provider call on `path` with `tap`.
async fn extension(
    handler: &Arc<Provider>,
    path: Path,
    tap: Option<&SpendTap>,
    cancel: &tokio_util::sync::CancellationToken,
    tx: &tokio::sync::mpsc::UnboundedSender<crate::StreamEvent>,
) -> CallResult {
    let (streaming, with_tools) = match path {
        Path::Stream => (true, false),
        Path::Complete => (false, false),
        Path::ToolLoop => (false, true),
    };
    let tools = if with_tools {
        vec![json!({
            "name": "not_a_registered_tool",
            "description": "a tool the host doesn't have",
            "input_schema": {"type": "object"},
        })]
    } else {
        vec![]
    };
    crate::runtime::openai::extension_route::route_extension_provider(
        manager(Arc::clone(handler), streaming, with_tools).await,
        "plug",
        "prov",
        "model-x",
        EXT,
        &Arc::new(tools),
        &Some("You are a worker.".to_owned()),
        &history(),
        tx,
        None,
        None,
        0,
        cancel,
        None,
        None,
        None,
        None,
        &crate::runtime::trace::TraceContext::disabled(),
        RouteSpend { tap, model: EXT },
    )
    .await
}

/// One whole call; its events (debug lines) and result.
async fn extension_run(
    handler: &Arc<Provider>,
    path: Path,
    tap: Option<&SpendTap>,
    cancel: &tokio_util::sync::CancellationToken,
) -> (Vec<String>, String) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let result = extension(handler, path, tap, cancel, &tx).await;
    drop(tx);
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(format!("{event:?}"));
    }
    (events, format!("{:?}", result.map_err(|e| e.to_string())))
}

fn streamed(
    events: Vec<ProviderStreamEvent>,
    result: Result<ProviderCompleteResult, String>,
) -> Arc<Provider> {
    Arc::new(Provider {
        stream_events: events,
        stream_result: Mutex::new(Some(result)),
        ..Default::default()
    })
}

fn completing(results: Vec<Result<ProviderCompleteResult, String>>) -> Arc<Provider> {
    Arc::new(Provider {
        complete_results: Mutex::new(results),
        ..Default::default()
    })
}

// ─── Streamed ────────────────────────────────────────────────────────────

/// A streamed answer books its final usage once, priced at the worker's
/// qualified model; the events and the result are the same as without a
/// tap.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn extension_a_streamed_answer_books_its_usage_once_and_changes_nothing_else() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (spend, log) = tap();
    let events = vec![
        says("spend "),
        reported(3, 1),
        says("ok"),
        reported(3, 5),
        ProviderStreamEvent::Done,
    ];
    let cancel = tokio_util::sync::CancellationToken::new();
    let with_tap = extension_run(
        &streamed(events.clone(), Ok(answer(Some(e_usage(3, 5))))),
        Path::Stream,
        Some(&spend),
        &cancel,
    )
    .await;
    assert!(with_tap.1.starts_with("Ok("), "{}", with_tap.1);
    assert_eq!(booked(&log), vec![(0, priced(EXT, e_tokens(3, 5)))]);
    let without = extension_run(
        &streamed(events, Ok(answer(Some(e_usage(3, 5))))),
        Path::Stream,
        None,
        &cancel,
    )
    .await;
    assert_eq!(with_tap, without, "a tap changes no event and no result");
    assert_eq!(booked(&log).len(), 1, "a call without a tap books nothing");
}

/// The call's own final usage wins over the streamed snapshots.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn extension_the_final_usage_wins_over_the_streamed_reports() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (spend, log) = tap();
    let handler = streamed(vec![reported(3, 1)], Ok(answer(Some(e_usage(4, 9)))));
    extension_run(&handler, Path::Stream, Some(&spend), &Default::default()).await;
    assert_eq!(booked(&log), vec![(0, priced(EXT, e_tokens(4, 9)))]);
}

/// A streamed report stands when the result has no usage.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn extension_a_streamed_report_stands_when_the_result_has_none() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (spend, log) = tap();
    let handler = streamed(vec![reported(3, 5)], Ok(answer(None)));
    extension_run(&handler, Path::Stream, Some(&spend), &Default::default()).await;
    assert_eq!(booked(&log), vec![(0, priced(EXT, e_tokens(3, 5)))]);
}

/// A count-less final usage leaves the streamed report.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn extension_a_count_less_final_usage_leaves_the_streamed_report() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (spend, log) = tap();
    let handler = streamed(vec![reported(3, 5)], Ok(answer(Some(json!({})))));
    extension_run(&handler, Path::Stream, Some(&spend), &Default::default()).await;
    assert_eq!(booked(&log), vec![(0, priced(EXT, e_tokens(3, 5)))]);
}

/// A stream that ends in an error books what it reported.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn extension_a_failed_stream_books_what_it_reported() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (spend, log) = tap();
    let handler = streamed(vec![says("spend "), reported(3, 1)], Err("fell over".into()));
    let (_, result) =
        extension_run(&handler, Path::Stream, Some(&spend), &Default::default()).await;
    assert!(result.starts_with("Err("), "{result}");
    assert_eq!(booked(&log), vec![(0, priced(EXT, e_tokens(3, 1)))]);
}

/// A streamed call whose result returns as the turn is cancelled was paid:
/// its final usage is booked before the cancel discards the result.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn extension_a_streamed_result_returned_as_the_turn_is_cancelled_is_booked() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (spend, log) = tap();
    let cancel = tokio_util::sync::CancellationToken::new();
    let handler = Arc::new(Provider {
        stream_events: vec![says("spend ok")],
        stream_result: Mutex::new(Some(Ok(answer(Some(e_usage(3, 5)))))),
        cancel_on_return: Some(cancel.clone()),
        ..Default::default()
    });
    let (_, result) = extension_run(&handler, Path::Stream, Some(&spend), &cancel).await;
    assert!(result.contains("operation canceled"), "{result}");
    assert_eq!(booked(&log), vec![(0, priced(EXT, e_tokens(3, 5)))]);
}

/// Read events until the text `after` arrives (the forwarder handles the
/// stream in order, so the report before it was shown), then cancel the
/// call (and let it end) or drop it; nothing is booked before.
async fn stop_after_text(
    call: impl std::future::Future<Output = CallResult>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<crate::StreamEvent>,
    cancel: Option<&tokio_util::sync::CancellationToken>,
    log: &Log,
) {
    let mut call = std::pin::pin!(call);
    let seen = async {
        while let Some(event) = rx.recv().await {
            if format!("{event:?}").contains("\"after\"") {
                return;
            }
        }
    };
    tokio::select! {
        result = &mut call => panic!("the stalled call ended: {result:?}"),
        () = seen => {}
    }
    assert!(booked(log).is_empty(), "nothing is booked while the call runs");
    if let Some(cancel) = cancel {
        cancel.cancel();
        assert!(call.await.is_err(), "a cancelled call ends with an error");
    }
}

fn stalled() -> Arc<Provider> {
    Arc::new(Provider {
        stream_events: vec![reported(3, 1), says("after")],
        stall: true,
        ..Default::default()
    })
}

/// A cancel after a streamed report books it once.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn extension_a_cancel_after_a_streamed_report_books_it() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (spend, log) = tap();
    let handler = stalled();
    let cancel = tokio_util::sync::CancellationToken::new();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    stop_after_text(
        extension(&handler, Path::Stream, Some(&spend), &cancel, &tx),
        rx,
        Some(&cancel),
        &log,
    )
    .await;
    assert_eq!(booked(&log), vec![(0, priced(EXT, e_tokens(3, 1)))]);
}

/// A dropped call books what the forwarder had seen, once.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn extension_a_dropped_stream_books_what_was_reported() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (spend, log) = tap();
    let handler = stalled();
    let cancel = tokio_util::sync::CancellationToken::new();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    stop_after_text(
        extension(&handler, Path::Stream, Some(&spend), &cancel, &tx),
        rx,
        None,
        &log,
    )
    .await;
    assert_eq!(booked(&log), vec![(0, priced(EXT, e_tokens(3, 1)))]);
}

// ─── Direct provider.complete ────────────────────────────────────────────

/// A direct answer books its usage once; the events and the result are
/// the same as without a tap.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn extension_a_direct_answer_books_its_usage_once_and_changes_nothing_else() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (spend, log) = tap();
    let cancel = tokio_util::sync::CancellationToken::new();
    let with_tap = extension_run(
        &completing(vec![Ok(answer(Some(e_usage(4, 2))))]),
        Path::Complete,
        Some(&spend),
        &cancel,
    )
    .await;
    assert!(with_tap.1.starts_with("Ok("), "{}", with_tap.1);
    assert_eq!(booked(&log), vec![(0, priced(EXT, e_tokens(4, 2)))]);
    let without = extension_run(
        &completing(vec![Ok(answer(Some(e_usage(4, 2))))]),
        Path::Complete,
        None,
        &cancel,
    )
    .await;
    assert_eq!(with_tap, without, "a tap changes no event and no result");
    assert_eq!(booked(&log).len(), 1, "a call without a tap books nothing");
}

/// A direct answer returned as the turn is cancelled was paid: it is
/// booked before the cancel discards it.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn extension_a_direct_answer_returned_as_the_turn_is_cancelled_is_booked() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (spend, log) = tap();
    let cancel = tokio_util::sync::CancellationToken::new();
    let handler = Arc::new(Provider {
        complete_results: Mutex::new(vec![Ok(answer(Some(e_usage(4, 2))))]),
        cancel_on_return: Some(cancel.clone()),
        ..Default::default()
    });
    let (_, result) = extension_run(&handler, Path::Complete, Some(&spend), &cancel).await;
    assert!(result.contains("operation canceled"), "{result}");
    assert_eq!(booked(&log), vec![(0, priced(EXT, e_tokens(4, 2)))]);
}

/// A direct call that fails carries no usage: nothing is booked.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn extension_a_failed_direct_call_books_nothing() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (spend, log) = tap();
    let handler = completing(vec![Err("fell over".into())]);
    let (_, result) =
        extension_run(&handler, Path::Complete, Some(&spend), &Default::default()).await;
    assert!(result.starts_with("Err("), "{result}");
    assert!(booked(&log).is_empty());
}

// ─── Host tool loop ──────────────────────────────────────────────────────

/// Each interior call books once, in order; the outer result is never
/// booked again.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn extension_a_tool_loop_books_each_interior_call_and_not_the_outer_result() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (spend, log) = tap();
    let handler = completing(vec![Ok(tool_call(5, 2)), Ok(answer(Some(e_usage(7, 3))))]);
    let (_, result) =
        extension_run(&handler, Path::ToolLoop, Some(&spend), &Default::default()).await;
    assert!(result.starts_with("Ok("), "{result}");
    assert_eq!(
        booked(&log),
        vec![
            (0, priced(EXT, e_tokens(5, 2))),
            (1, priced(EXT, e_tokens(7, 3))),
        ]
    );
}

/// A late error keeps the earlier interior calls booked.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn extension_a_late_tool_loop_error_keeps_the_earlier_calls_booked() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (spend, log) = tap();
    let handler = completing(vec![Ok(tool_call(5, 2)), Err("fell over".into())]);
    let (_, result) =
        extension_run(&handler, Path::ToolLoop, Some(&spend), &Default::default()).await;
    assert!(result.starts_with("Err("), "{result}");
    assert_eq!(booked(&log), vec![(0, priced(EXT, e_tokens(5, 2)))]);
}

/// The iteration limit after billed calls keeps every one booked.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn extension_the_tool_loop_limit_keeps_the_billed_calls_booked() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (spend, log) = tap();
    let handler = completing((0..8).map(|_| Ok(tool_call(1, 1))).collect());
    let (_, result) =
        extension_run(&handler, Path::ToolLoop, Some(&spend), &Default::default()).await;
    assert!(result.contains("iteration limit"), "{result}");
    let booked = booked(&log);
    assert_eq!(booked.len(), 8, "{booked:?}");
    assert!(booked
        .iter()
        .all(|(_, report)| *report == priced(EXT, e_tokens(1, 1))));
}

// ─── The router ──────────────────────────────────────────────────────────

/// Clears the process's extension routing when the test ends.
struct Routing;

impl Drop for Routing {
    fn drop(&mut self) {
        crate::runtime::openai::clear_extension_manager_for_routing();
    }
}

/// A worker runtime on an extension model books the call through the
/// router's spend.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, extension_routing)]
async fn extension_a_worker_turn_books_through_the_router() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (spend, log) = tap();
    let handler = completing(vec![Ok(answer(Some(e_usage(4, 2))))]);
    crate::runtime::openai::set_extension_manager_for_routing(
        manager(Arc::clone(&handler), false, false).await,
    );
    let _routing = Routing;
    let rt = runtime(EXT, Some(spend));
    let events = turn(&rt, Default::default()).await;
    assert!(
        events.iter().any(|e| e.contains("spend ok")),
        "{events:#?}"
    );
    assert_eq!(booked(&log), vec![(0, priced(EXT, e_tokens(4, 2)))]);
}
