//! Explicit cloud invocation transport (AWS Bedrock, Azure OpenAI, Google
//! Vertex) — the `cloud_invoke` branch of the runtime dispatch, extracted
//! from `runtime/api.rs` for direct testability and Task 10B trace wiring.
//!
//! Contract preserved from Phase 1:
//! - **Text-only pre-flight (spec §5.5).** A mode that exposes tools must
//!   fail HERE — before the broker is constructed, before any credential
//!   lookup, before any network access, and before any trace record is
//!   begun: a request that never became an HTTP attempt emits **no**
//!   attempt record. The invoke-time guard inside the broker remains as
//!   defense in depth.
//! - **Broker-owned authority.** Hosts, auth, signing and provider bodies
//!   live behind the broker boundary; this module hands over normalized
//!   text messages only.
//! - **Provider-controlled errors stay out of trace/log.** Broker/provider
//!   error text flows only into the user-facing `RuntimeError`; trace
//!   records carry static codes.
//!
//! Trace wiring (Task 10B): one `synaps-request-trace/1` record per actual
//! `cloud_invoke` call — success, terminal failure, or cancellation — with
//! `TransportKind::CloudProxy` and `wire: None` (the exact provider bytes
//! are serialized behind the broker boundary, never in this process).

use crate::auth::{CloudProviderId, CredentialBroker};
use crate::runtime::trace::{google as trg, openai as tro, TraceContext};
use crate::{Result, RuntimeError};
use futures::StreamExt;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Run one explicit cloud invocation. `full_model` is the caller's original
/// route (context suffix included — used only for user-facing usage
/// labeling); `cloud_model` is the split `provider/model` identity.
///
/// `make_broker` is invoked only after the §5.5 text-only pre-flight
/// passes, so a tool-bearing request performs zero broker constructions and
/// zero invocations, and emits zero trace records.
#[allow(clippy::too_many_arguments)]
pub(super) async fn cloud_invoke_stream(
    provider: CloudProviderId,
    full_model: &str,
    cloud_model: &str,
    cloud_context: Option<&str>,
    has_tools: bool,
    make_broker: impl FnOnce() -> Arc<dyn CredentialBroker>,
    system_prompt: &Option<String>,
    messages: &[crate::SharedMessage],
    tx: &mpsc::UnboundedSender<crate::runtime::types::StreamEvent>,
    cancel: &CancellationToken,
    trace: &TraceContext,
) -> Result<Value> {
    // Spec §5.5 pre-flight: cloud routes are text-only. Failing here means
    // no broker, no credential lookup, no network — and no attempt record.
    crate::auth::preflight_cloud_capability(provider, has_tools)
        .map_err(|e| RuntimeError::Config(e.to_string()))?;

    let tracer = trg::begin_cloud_invoke_tracer(
        trace,
        provider,
        cloud_model,
        messages,
        system_prompt.as_deref(),
    )
    .await;
    let mut attempt = tro::StreamAttempt::new(tracer);

    let broker = make_broker();
    let mut normalized = Vec::new();
    if let Some(system) = system_prompt.as_ref().filter(|s| !s.is_empty()) {
        normalized.push(crate::auth::cloud::BrokerMessage {
            role: crate::auth::cloud::MessageRole::System,
            content: system.clone(),
        });
    }
    for message in messages {
        let role = match message["role"].as_str().unwrap_or("user") {
            "assistant" => crate::auth::cloud::MessageRole::Assistant,
            "system" => crate::auth::cloud::MessageRole::System,
            "tool" => crate::auth::cloud::MessageRole::Tool,
            _ => crate::auth::cloud::MessageRole::User,
        };
        let content = message["content"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| message["content"].to_string());
        normalized.push(crate::auth::cloud::BrokerMessage { role, content });
    }
    let request = crate::auth::cloud::InvokeRequest {
        messages: normalized,
        tools: Vec::new(),
        stream: true,
        options: Default::default(),
    };
    // One-shot explicit content capture (`/trace next content`): the exact
    // provider wire bytes live behind the broker boundary, but this
    // serialized broker request body IS the full pre-send request content
    // this process hands over — body only (normalized messages/options),
    // never credentials or transport headers, which the broker attaches
    // out of process.
    if let Some(request_id) = attempt.request_id() {
        if let Ok(body) = serde_json::to_vec(&request) {
            trace.capture_request_content(request_id, &body);
        }
    }
    let context_ref = cloud_context.unwrap_or(provider.as_str());
    let mut stream = match broker
        .cloud_invoke(provider, context_ref, cloud_model, request)
        .await
    {
        Ok(stream) => stream,
        Err(e) => {
            // Static code only — the broker/provider error text is
            // user-facing output, never trace content.
            attempt.finish_failed("cloud_invoke_error", None, None);
            return Err(RuntimeError::Config(e.to_string()));
        }
    };
    let mut trace_usage: Option<crate::runtime::trace::UsageMeta> = None;
    let mut text = String::new();
    loop {
        let event = tokio::select! {
            _ = cancel.cancelled() => {
                attempt.finish_canceled(None, trace_usage);
                return Err(RuntimeError::Config("cloud invocation cancelled".into()));
            }
            event = stream.next() => event,
        };
        let Some(event) = event else { break };
        let event = match event {
            Ok(event) => event,
            Err(e) => {
                attempt.finish_failed("cloud_stream_error", None, None);
                return Err(RuntimeError::Config(e.to_string()));
            }
        };
        match event {
            crate::auth::broker::CloudEvent::TextDelta { delta } => {
                attempt.mark_first_model_event();
                text.push_str(&delta);
                let _ = tx.send(crate::runtime::types::StreamEvent::Llm(
                    crate::runtime::types::LlmEvent::Text(delta),
                ));
            }
            crate::auth::broker::CloudEvent::ToolArguments { id, name, delta } => {
                attempt.mark_first_model_event();
                if let Some(name) = name {
                    let _ = tx.send(crate::runtime::types::StreamEvent::Llm(
                        crate::runtime::types::LlmEvent::ToolUseStart {
                            tool_name: name,
                            tool_id: id.clone(),
                        },
                    ));
                }
                let _ = tx.send(crate::runtime::types::StreamEvent::Llm(
                    crate::runtime::types::LlmEvent::ToolUseDelta { tool_id: id, delta },
                ));
            }
            crate::auth::broker::CloudEvent::Usage {
                input_tokens,
                output_tokens,
            } => {
                trace_usage = Some(trg::usage_from_cloud_event(input_tokens, output_tokens));
                let _ = tx.send(crate::runtime::types::StreamEvent::Session(
                    crate::runtime::types::SessionEvent::Usage {
                        input_tokens,
                        output_tokens,
                        cache_read_input_tokens: 0,
                        cache_creation_input_tokens: 0,
                        cache_creation_5m: None,
                        cache_creation_1h: None,
                        model: Some(full_model.into()),
                    },
                ));
            }
            crate::auth::broker::CloudEvent::Done => break,
        }
    }
    attempt.finish_success(None, None, None, trace_usage);
    Ok(serde_json::json!({"content":[{"type":"text","text":text}]}))
}
