//! The extension provider route's spend gate: an extension provider call
//! with no tap passes the same parameters to the provider, emits the same
//! events and returns the same result as before child spend, on the
//! streamed path, the direct `provider.complete` path and the host tool
//! loop. The provider is an in-process fake that records every call.
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
use serde_json::Value;

/// Scripted results per call; records every call's parameters.
#[derive(Default)]
struct Provider {
    complete_results: Mutex<Vec<Result<ProviderCompleteResult, String>>>,
    stream_events: Vec<ProviderStreamEvent>,
    stream_result: Mutex<Option<Result<ProviderCompleteResult, String>>>,
    seen: Mutex<Vec<String>>,
}

impl Provider {
    fn record(&self, kind: &str, params: &ProviderCompleteParams) {
        self.seen.lock().unwrap().push(format!(
            "=== {kind}\n{}\n",
            serde_json::to_string_pretty(params).unwrap()
        ));
    }
}

#[async_trait]
impl ExtensionHandler for Provider {
    fn id(&self) -> &str {
        "gate-provider"
    }

    async fn handle(&self, _event: &HookEvent) -> HookResult {
        HookResult::Continue
    }

    async fn shutdown(&self) {}

    async fn provider_complete(
        &self,
        params: ProviderCompleteParams,
    ) -> Result<ProviderCompleteResult, String> {
        self.record("provider.complete", &params);
        let mut script = self.complete_results.lock().unwrap();
        if script.is_empty() {
            return Err("unscripted provider.complete".into());
        }
        script.remove(0)
    }

    async fn provider_stream(
        &self,
        params: ProviderCompleteParams,
        sink: tokio::sync::mpsc::Sender<ProviderStreamEvent>,
    ) -> Result<ProviderCompleteResult, String> {
        self.record("provider.stream", &params);
        for event in &self.stream_events {
            let _ = sink.send(event.clone()).await;
        }
        self.stream_result
            .lock()
            .unwrap()
            .take()
            .unwrap_or_else(|| Err("unscripted provider.stream".into()))
    }
}

fn usage(input: u64, output: u64) -> Value {
    json!({"input_tokens": input, "output_tokens": output})
}

fn text_result(text: &str, input: u64, output: u64) -> ProviderCompleteResult {
    ProviderCompleteResult {
        content: vec![json!({"type": "text", "text": text})],
        stop_reason: Some("end_turn".to_string()),
        usage: Some(usage(input, output)),
    }
}

async fn manager(
    handler: Arc<Provider>,
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
    let spec = RegisteredProviderSpec {
        id: "prov".to_string(),
        display_name: "Gate".to_string(),
        description: "gate provider".to_string(),
        models: vec![RegisteredProviderModelSpec {
            id: "model-x".to_string(),
            display_name: None,
            capabilities: json!({"streaming": streaming, "tool_use": tool_use}),
            context_window: None,
        }],
        config_schema: None,
    };
    manager
        .register_provider_handler_for_tests("plug", spec, handler)
        .unwrap();
    Arc::new(tokio::sync::RwLock::new(manager))
}

/// One extension provider call; compare the provider calls, events and
/// result with `name`'s fixtures.
async fn extension_gate(
    name: &str,
    handler: Provider,
    streaming: bool,
    tool_use: bool,
    with_tools: bool,
    tools: Vec<Value>,
) {
    let _base = crate::test_env::BaseDirGuard::new();
    let handler = Arc::new(handler);
    let manager = manager(handler.clone(), streaming, tool_use, with_tools).await;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let cancel = tokio_util::sync::CancellationToken::new();
    let result = crate::runtime::openai::extension_route::route_extension_provider(
        manager,
        "plug",
        "prov",
        "model-x",
        "plug:prov:model-x",
        &Arc::new(tools),
        &Some("You are a gate.".to_owned()),
        &history(),
        &tx,
        None,
        None,
        0,
        &cancel,
        None,
        None,
        None,
        None,
        &crate::runtime::trace::TraceContext::disabled(),
        crate::runtime::child_spend::RouteSpend::NONE,
    )
    .await;
    drop(tx);
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    let (mut events, wire) = render_events(events);
    events.push_str(&format!(
        "=> {}\n",
        match &result {
            Ok(value) => serde_json::to_string(value).unwrap(),
            Err(e) => format!("Err({e})"),
        }
    ));
    let calls = handler.seen.lock().unwrap().concat();
    check(&format!("{name}_requests.txt"), &stable(&calls));
    check(&format!("{name}_events.txt"), &stable(&events));
    check(&format!("{name}_wire.txt"), &stable(&wire));
}

/// The streamed path with no tap: streamed Usage is absorbed and the final
/// result carries the usage, as before.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn spend_gate_extension_stream_is_unchanged_without_a_tap() {
    extension_gate(
        "extension_stream",
        Provider {
            stream_events: vec![
                ProviderStreamEvent::TextDelta {
                    text: "gate ".into(),
                },
                ProviderStreamEvent::Usage { usage: usage(3, 1) },
                ProviderStreamEvent::TextDelta { text: "ok".into() },
                ProviderStreamEvent::Usage { usage: usage(3, 5) },
                ProviderStreamEvent::Done,
            ],
            stream_result: Mutex::new(Some(Ok(text_result("gate ok", 3, 5)))),
            ..Default::default()
        },
        true,
        false,
        false,
        vec![],
    )
    .await;
}

/// The same with an error after a streamed usage report.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn spend_gate_extension_stream_error_is_unchanged_without_a_tap() {
    extension_gate(
        "extension_stream_error",
        Provider {
            stream_events: vec![
                ProviderStreamEvent::TextDelta {
                    text: "gate ".into(),
                },
                ProviderStreamEvent::Usage { usage: usage(3, 1) },
            ],
            stream_result: Mutex::new(Some(Err("provider fell over".into()))),
            ..Default::default()
        },
        true,
        false,
        false,
        vec![],
    )
    .await;
}

/// The direct `provider.complete` path (no streaming, no host tools).
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn spend_gate_extension_complete_is_unchanged_without_a_tap() {
    extension_gate(
        "extension_complete",
        Provider {
            complete_results: Mutex::new(vec![Ok(text_result("gate ok", 4, 2))]),
            ..Default::default()
        },
        false,
        false,
        false,
        vec![],
    )
    .await;
}

/// The host tool loop: a tool call (an unregistered tool, so the host
/// answers with an error result), then the final answer; two interior
/// `provider.complete` calls.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn spend_gate_extension_tool_loop_is_unchanged_without_a_tap() {
    extension_gate(
        "extension_tool_loop",
        Provider {
            complete_results: Mutex::new(vec![
                Ok(ProviderCompleteResult {
                    content: vec![json!({
                        "type": "tool_use",
                        "id": "toolu_gate",
                        "name": "not_a_registered_tool",
                        "input": {"query": "gate"},
                    })],
                    stop_reason: Some("tool_use".to_string()),
                    usage: Some(usage(6, 2)),
                }),
                Ok(text_result("gate ok", 9, 3)),
            ]),
            ..Default::default()
        },
        false,
        true,
        true,
        vec![json!({
            "name": "not_a_registered_tool",
            "description": "a tool the host doesn't have",
            "input_schema": {"type": "object"},
        })],
    )
    .await;
}
