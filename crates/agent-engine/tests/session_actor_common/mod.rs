//! Shared harness for the `session_actor_*` integration tests: temp HOME,
//! loopback Anthropic SSE stub, host/config helpers, event drains.
#![allow(dead_code)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agent_engine::session::{
    ClientTransport, EndReason, Envelope, LocalTransport, SessionCommand, SessionConfig,
    SessionEventWire,
};
use agent_engine::{EngineHost, HostOpts};
use axum::response::IntoResponse;

pub const MODEL: &str = "claude-sonnet-4-5";

pub const SSE_HI: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_01\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
    "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,",
    "\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,",
    "\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",",
    "\"stop_sequence\":null},\"usage\":{\"input_tokens\":10,\"output_tokens\":1,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

/// Started stream with one text delta and NO terminal event.
pub const SSE_PREFIX: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_01\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
    "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,",
    "\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,",
    "\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
);

/// Turn requesting the `prompt_fixture` tool; follow-up round serves SSE_HI.
pub const SSE_PROMPT_TOOL_USE: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_p1\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
    "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,",
    "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_prompt\",\"name\":\"prompt_fixture\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",",
    "\"stop_sequence\":null},\"usage\":{\"input_tokens\":10,\"output_tokens\":5,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

pub struct Home {
    pub dir: tempfile::TempDir,
}

impl Home {
    pub fn new() -> Self {
        let dir = tempfile::TempDir::new().unwrap();
        let base = dir.path().join(".synaps-cli");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("config"), "").unwrap();
        std::fs::write(
            base.join("auth.json"),
            "{\"anthropic\": {\"type\": \"oauth\", \"refresh\": \"r\", \"access\": \"synthetic-token\", \"expires\": 9999999999999}}",
        )
        .unwrap();
        for k in ["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "GOOGLE_API_KEY"] {
            std::env::remove_var(k);
        }
        std::env::set_var("HOME", dir.path());
        std::env::set_var("SYNAPS_BASE_DIR", &base);
        Self { dir }
    }
}

/// `endless`: after the body, keep-alive comments forever (cancel fixture).
pub async fn stub(body: &'static str, endless: bool) -> (String, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_c = Arc::clone(&hits);
    let app = axum::Router::new().fallback(move || {
        let hits = Arc::clone(&hits_c);
        async move {
            hits.fetch_add(1, Ordering::SeqCst);
            if endless {
                let stream = futures::stream::once(async move {
                    Ok::<_, std::io::Error>(axum::body::Bytes::from(body))
                })
                .chain(futures::stream::unfold((), |()| async {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    Some((Ok(axum::body::Bytes::from(": keep-alive\n\n")), ()))
                }));
                axum::response::Response::builder()
                    .status(200)
                    .header("content-type", "text/event-stream")
                    .body(axum::body::Body::from_stream(stream))
                    .unwrap()
            } else {
                (
                    axum::http::StatusCode::OK,
                    [("content-type", "text/event-stream")],
                    body.to_string(),
                )
                    .into_response()
            }
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), hits)
}

use futures::StreamExt;

/// Sequenced stub: hit `i` serves `bodies[min(i, len-1)]` (all terminal).
pub async fn stub_seq(bodies: &'static [&'static str]) -> (String, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_c = Arc::clone(&hits);
    let app = axum::Router::new().fallback(move || {
        let hits = Arc::clone(&hits_c);
        async move {
            let i = hits.fetch_add(1, Ordering::SeqCst);
            let body = bodies[i.min(bodies.len() - 1)];
            (
                axum::http::StatusCode::OK,
                [("content-type", "text/event-stream")],
                body.to_string(),
            )
                .into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), hits)
}

/// Stub whose response is delayed `delay` per hit (slow compaction fixture).
pub async fn stub_slow(body: &'static str, delay: Duration) -> (String, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_c = Arc::clone(&hits);
    let app = axum::Router::new().fallback(move || {
        let hits = Arc::clone(&hits_c);
        async move {
            hits.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(delay).await;
            (
                axum::http::StatusCode::OK,
                [("content-type", "text/event-stream")],
                body.to_string(),
            )
                .into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), hits)
}

pub async fn host() -> Arc<EngineHost> {
    EngineHost::boot(HostOpts {
        profile: None,
        no_extensions: true,
    })
    .await
    .unwrap()
}

pub fn cfg() -> SessionConfig {
    SessionConfig {
        model_override: Some(MODEL.into()),
        persist: false,
        ..SessionConfig::default()
    }
}

pub async fn next(t: &mut LocalTransport) -> Envelope {
    tokio::time::timeout(Duration::from_secs(30), t.next_event())
        .await
        .expect("timely")
        .expect("alive")
}

pub async fn until(
    t: &mut LocalTransport,
    pred: impl Fn(&SessionEventWire) -> bool,
) -> Vec<Envelope> {
    let mut out = Vec::new();
    loop {
        let env = next(t).await;
        let done = pred(&env.event);
        out.push(env);
        if done {
            return out;
        }
    }
}

/// Drain everything that arrives within `d` (no waiting for a terminal).
pub async fn drain_for(t: &mut LocalTransport, d: Duration) -> Vec<Envelope> {
    let mut out = Vec::new();
    let deadline = tokio::time::Instant::now() + d;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return out;
        }
        match tokio::time::timeout(left, t.next_event()).await {
            Ok(Some(env)) => out.push(env),
            _ => return out,
        }
    }
}

pub fn submit(text: &str) -> SessionCommand {
    SessionCommand::Submit {
        text: text.into(),
        attachments: vec![],
    }
}

pub fn last_conversation(seen: &[Envelope]) -> agent_engine::session::ConversationSnapshot {
    seen.iter()
        .rev()
        .find_map(|e| match &e.event {
            SessionEventWire::Conversation(c) => Some(c.clone()),
            _ => None,
        })
        .expect("a Conversation envelope")
}

pub async fn end(t: &mut LocalTransport) {
    t.send(SessionCommand::End {
        reason: EndReason::ClientQuit,
    })
    .await
    .unwrap();
    until(t, |e| matches!(e, SessionEventWire::Ended { .. })).await;
}

/// Builtin-origin tool that asks for a secret through the stream's
/// `SecretPromptHandle` and reports only its length (never the value).
pub struct PromptFixtureTool;

#[async_trait::async_trait]
impl agent_engine::Tool for PromptFixtureTool {
    fn name(&self) -> &str {
        "prompt_fixture"
    }
    fn description(&self) -> &str {
        "prompts for a secret"
    }
    fn parameters(&self) -> agent_engine::Value {
        serde_json::json!({"type": "object"})
    }
    fn origin(&self) -> agent_engine::tools::ToolOrigin {
        agent_engine::tools::ToolOrigin::Builtin
    }
    async fn execute(
        &self,
        _params: agent_engine::Value,
        ctx: agent_engine::ToolContext,
    ) -> agent_engine::Result<String> {
        let handle = ctx
            .capabilities
            .secret_prompt
            .expect("actor passes its SecretPromptHandle to the stream");
        Ok(match handle.prompt("Secret".into(), "enter secret".into()).await {
            Some(v) => format!("answered:{}", v.len()),
            None => "cancelled".to_string(),
        })
    }
}

pub async fn prompt_host() -> Arc<EngineHost> {
    let host = host().await;
    host.parts()
        .tools
        .write()
        .await
        .register(Arc::new(PromptFixtureTool));
    host
}

pub fn prompt_id(env: &Envelope) -> Option<u64> {
    match &env.event {
        SessionEventWire::Prompt(p) => Some(p.id),
        _ => None,
    }
}

pub fn tool_results(snap: &agent_engine::session::ConversationSnapshot) -> Vec<String> {
    snap.api_messages
        .iter()
        .filter_map(|m| m["content"].as_array())
        .flat_map(|b| b.iter())
        .filter(|b| b["type"] == "tool_result")
        .map(|b| b["content"].as_str().unwrap_or("").to_string())
        .collect()
}
