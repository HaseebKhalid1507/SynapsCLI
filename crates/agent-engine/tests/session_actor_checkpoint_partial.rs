//! A checkpoint keeps the assistant text a client has already seen.
//!
//! The provider streams a visible text delta, then holds its response; a
//! host checkpoint lands; then the provider fails the response with a
//! non-retryable in-stream error, either before the checkpoint's quiesce
//! grace ends or only after its cancel. The kept history must still carry
//! the delta.
//!
//! On a provider error the runtime publishes the history it had BEFORE the
//! response (runtime/stream.rs, the `call_api_stream_inner` error arm), so a
//! round that fails while the checkpoint still lets it run can lose text the
//! client saw; a cancel instead returns the content assembled so far. This
//! file only uses API the base line has too, so the same test runs before
//! and after the quiesce grace exists.
//!
//! An "empty cancelled response" with a visible delta is not built here: a
//! cancelled Anthropic stream returns the content parsed so far
//! (`classify_stream_outcome`), so it is empty only when no text was parsed,
//! or after a retry reset withdrew the text from every client.

mod session_actor_common;
use session_actor_common::*;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agent_engine::session::wire::CHECKPOINT_QUERY_ID;
use agent_engine::session::{
    CheckpointReason, ClientKind, ClientMeta, ClientTransport, LocalTransport, SessionCommand,
    SessionEventWire,
};
use agent_engine::{LlmEvent, StreamEvent};
use futures::StreamExt;
use serial_test::serial;

/// A started response with one visible text delta and no terminal event.
const SSE_VISIBLE_DELTA: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_pp\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
    "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,",
    "\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,",
    "\"delta\":{\"type\":\"text_delta\",\"text\":\"seen-delta\"}}\n\n",
);

/// A non-retryable in-stream error; the response ends after it.
const SSE_FORCED_ERROR: &str = concat!(
    "data: {\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",",
    "\"message\":\"forced\"}}\n\n",
);

/// Every hit streams the visible delta at once, then waits for one permit of
/// `gate` before it sends the forced error and ends the response.
async fn stub_delta_then_error(gate: Arc<tokio::sync::Semaphore>) -> (String, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_c = Arc::clone(&hits);
    let app = axum::Router::new().fallback(move || {
        let hits = Arc::clone(&hits_c);
        let gate = Arc::clone(&gate);
        async move {
            hits.fetch_add(1, Ordering::SeqCst);
            let first = futures::stream::once(async move {
                Ok::<_, std::io::Error>(axum::body::Bytes::from(SSE_VISIBLE_DELTA))
            });
            let rest = futures::stream::once(async move {
                if let Ok(permit) = gate.acquire().await {
                    permit.forget();
                }
                Ok::<_, std::io::Error>(axum::body::Bytes::from(SSE_FORCED_ERROR))
            });
            axum::response::Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(axum::body::Body::from_stream(first.chain(rest)))
                .unwrap()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), hits)
}

/// One session: the delta is seen, a host checkpoint lands, and the forced
/// error comes 300 ms later (`error_inside_grace`: well inside a 2 s grace)
/// or only after the checkpoint answered. Returns the kept history (JSON).
async fn kept_history(error_inside_grace: bool) -> String {
    let _home = Home::new();
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let (url, hits) = stub_delta_then_error(Arc::clone(&gate)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Test))
        .await
        .unwrap();
    until(&mut a, |e| {
        matches!(e, SessionEventWire::ClientJoined { .. })
    })
    .await;

    a.send(submit("go")).await.unwrap();
    until(&mut a, |e| {
        matches!(
            e,
            SessionEventWire::Stream(StreamEvent::Llm(LlmEvent::Text(t))) if t.contains("seen-delta")
        )
    })
    .await;
    handle
        .send(SessionCommand::Checkpoint {
            reason: CheckpointReason::HostRequest,
            deadline: None,
            skip_quiesce_grace: false,
        })
        .await
        .unwrap();
    if error_inside_grace {
        tokio::time::sleep(Duration::from_millis(300)).await;
        gate.add_permits(1);
    }
    let seen = until(
        &mut a,
        |e| matches!(e, SessionEventWire::QueryResult { id, .. } if *id == CHECKPOINT_QUERY_ID),
    )
    .await;
    if !error_inside_grace {
        gate.add_permits(1);
    }
    assert_eq!(hits.load(Ordering::SeqCst), 1, "one provider request");
    let history = serde_json::to_string(&last_conversation(&seen).api_messages).unwrap();
    end(&mut a).await;
    history
}

/// Both sides of the grace's end; every case is run and reported before the
/// assertion, so one run shows which side loses the delta.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn checkpoint_preserves_delta_when_provider_returns_prepartial_history() {
    let mut lost = Vec::new();
    for (case, inside) in [
        ("provider error inside the grace", true),
        ("provider error after the checkpoint's cancel", false),
    ] {
        let history = kept_history(inside).await;
        let kept = history.contains("seen-delta");
        eprintln!("r-partial case: {case}: kept={kept} history={history}");
        if !kept {
            lost.push(format!(
                "{case}: the kept history lacks the visible delta: {history}"
            ));
        }
    }
    assert!(lost.is_empty(), "{}", lost.join("\n"));
}
