//! §5.2 — `SessionActor` unit tests through `EngineHost::create_session` +
//! `LocalTransport`, against a loopback Anthropic SSE stub.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agent_engine::session::{
    ClientKind, ClientMeta, ClientTransport, EndReason, Envelope, LocalTransport, SessionCommand,
    SessionConfig, SessionEventWire, SessionSetting,
};
use agent_engine::{EngineHost, HostOpts, SessionEvent, StreamEvent};
use axum::response::IntoResponse;
use serial_test::serial;

const MODEL: &str = "claude-sonnet-4-5";

const SSE_HI: &str = concat!(
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
const SSE_PREFIX: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_01\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
    "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,",
    "\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,",
    "\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
);

struct Home {
    _dir: tempfile::TempDir,
}

impl Home {
    fn new() -> Self {
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
        Self { _dir: dir }
    }
}

/// `endless`: after the prefix, keep-alive comments forever (cancel fixture).
async fn stub(body: &'static str, endless: bool) -> (String, Arc<AtomicUsize>) {
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
async fn stub_seq(bodies: &'static [&'static str]) -> (String, Arc<AtomicUsize>) {
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

async fn host() -> Arc<EngineHost> {
    EngineHost::boot(HostOpts {
        profile: None,
        no_extensions: true,
    })
    .await
    .unwrap()
}

fn cfg() -> SessionConfig {
    SessionConfig {
        model_override: Some(MODEL.into()),
        persist: false,
        ..SessionConfig::default()
    }
}

async fn next(t: &mut LocalTransport) -> Envelope {
    tokio::time::timeout(Duration::from_secs(30), t.next_event())
        .await
        .expect("timely")
        .expect("alive")
}

async fn until(t: &mut LocalTransport, pred: impl Fn(&SessionEventWire) -> bool) -> Vec<Envelope> {
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

fn submit(text: &str) -> SessionCommand {
    SessionCommand::Submit {
        text: text.into(),
        attachments: vec![],
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn attach_detach_counts_and_seq_is_gapless() {
    let _h = Home::new();
    let (url, _) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    assert_eq!(host.sessions().len(), 1);
    assert!(host.attach(&handle.id).is_some());

    let (mut a, snap_a) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    assert_eq!(snap_a.clients.len(), 1);
    assert_eq!(snap_a.view.model, MODEL);
    let (mut b, snap_b) =
        LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Attach))
            .await
            .unwrap();
    assert_eq!(snap_b.clients.len(), 2);
    assert_ne!(a.client_id(), b.client_id());

    a.send(submit("hello")).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    let seqs: Vec<u64> = seen.iter().map(|e| e.seq).collect();
    assert!(seqs.windows(2).all(|w| w[1] == w[0] + 1), "gapless: {seqs:?}");
    assert!(seen
        .iter()
        .any(|e| matches!(e.event, SessionEventWire::TurnStarted { .. })));
    assert!(seen.iter().any(|e| matches!(
        e.event,
        SessionEventWire::Stream(StreamEvent::Session(SessionEvent::Done))
    )));
    let conv = seen
        .iter()
        .rev()
        .find_map(|e| match &e.event {
            SessionEventWire::Conversation(c) => Some(c.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(conv.api_messages.len(), 2);
    assert_eq!(conv.tokens.input, 10);

    // Both clients saw the same stream.
    let seen_b = until(&mut b, |e| matches!(e, SessionEventWire::Idle)).await;
    let sa: Vec<String> = seen
        .iter()
        .filter(|e| matches!(e.event, SessionEventWire::Stream(_)))
        .map(|e| format!("{:?}", e.event))
        .collect();
    let sb: Vec<String> = seen_b
        .iter()
        .filter(|e| matches!(e.event, SessionEventWire::Stream(_)))
        .map(|e| format!("{:?}", e.event))
        .collect();
    assert_eq!(sa, sb);

    b.send(SessionCommand::Detach {
        client: b.client_id(),
    })
    .await
    .unwrap();
    let left = until(&mut a, |e| matches!(e, SessionEventWire::ClientLeft { .. })).await;
    assert!(!left.is_empty());
    assert!(handle.is_alive());

    a.send(SessionCommand::End {
        reason: EndReason::ClientQuit,
    })
    .await
    .unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::Ended { .. })).await;
    handle.closed().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(host.sessions().len(), 0, "actor removed itself from the host map");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn detach_does_not_cancel_stream_end_does() {
    let _h = Home::new();
    let (url, _) = stub(SSE_PREFIX, true).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Test))
        .await
        .unwrap();
    a.send(submit("go")).await.unwrap();
    until(&mut a, |e| {
        matches!(e, SessionEventWire::Stream(StreamEvent::Llm(agent_engine::LlmEvent::Text(_))))
    })
    .await;

    // Detach every client mid-turn: the stream keeps running.
    a.send(SessionCommand::Detach {
        client: a.client_id(),
    })
    .await
    .unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::ClientLeft { .. })).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(handle.is_alive());

    // A fresh client attaches mid-turn: streaming=true, replay non-empty.
    let (mut b, snap) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Attach))
        .await
        .unwrap();
    assert!(snap.streaming, "turn still running after every client detached");
    assert!(snap
        .replay
        .iter()
        .any(|e| matches!(e.event, SessionEventWire::Stream(_))));

    // End cancels the turn first, then finishes.
    b.send(SessionCommand::End {
        reason: EndReason::HostShutdown,
    })
    .await
    .unwrap();
    let tail = until(&mut b, |e| matches!(e, SessionEventWire::Ended { .. })).await;
    assert!(tail
        .iter()
        .any(|e| matches!(e.event, SessionEventWire::Conversation(_))));
    handle.closed().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn set_applies_and_publishes_view() {
    let _h = Home::new();
    let (url, _) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Test))
        .await
        .unwrap();
    a.send(SessionCommand::Set(SessionSetting::ContextWindow {
        tokens: Some(4242),
    }))
    .await
    .unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::SettingChanged(_))).await;
    match &seen.last().unwrap().event {
        SessionEventWire::SettingChanged(s) => {
            assert!(s.ok);
            assert_eq!(s.setting, "context_window");
            assert_eq!(s.view.context_window, 4242);
        }
        _ => unreachable!(),
    }
    assert_eq!(a.view().context_window, 4242);
    a.send(SessionCommand::End {
        reason: EndReason::ClientQuit,
    })
    .await
    .unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::Ended { .. })).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn create_session_sets_cwd_and_session_id() {
    let _h = Home::new();
    let (url, _) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let tmp = tempfile::TempDir::new().unwrap();
    let handle = host
        .create_session(SessionConfig {
            cwd: Some(tmp.path().to_path_buf()),
            ..cfg()
        })
        .await
        .unwrap();
    assert_eq!(handle.meta().cwd.as_deref(), Some(tmp.path()));
    assert_eq!(handle.meta().host_pid, std::process::id());
    // Registered under the real session registry (synaps send path).
    let reg = agent_engine::events::registry::find_session_registration(handle.id.as_str());
    assert!(reg.is_some(), "session registered");
    let (mut a, snap) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Test))
        .await
        .unwrap();
    assert_eq!(snap.meta.id, handle.id);
    a.send(SessionCommand::End {
        reason: EndReason::ClientQuit,
    })
    .await
    .unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::Ended { .. })).await;
    handle.closed().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        agent_engine::events::registry::find_session_registration(handle.id.as_str()).is_none(),
        "unregistered at End"
    );
}

fn last_conversation(seen: &[Envelope]) -> agent_engine::session::ConversationSnapshot {
    seen.iter()
        .rev()
        .find_map(|e| match &e.event {
            SessionEventWire::Conversation(c) => Some(c.clone()),
            _ => None,
        })
        .expect("a Conversation envelope")
}

async fn end(t: &mut LocalTransport) {
    t.send(SessionCommand::End {
        reason: EndReason::ClientQuit,
    })
    .await
    .unwrap();
    until(t, |e| matches!(e, SessionEventWire::Ended { .. })).await;
}

/// §11 #1: `Cancel` while idle is a no-op — at most an `Idle` echo. No
/// "aborted" notice, no Conversation (= no abort_context, no save), and the
/// next Submit carries no `[ABORT CONTEXT` prefix.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn cancel_while_idle_is_a_noop() {
    let _h = Home::new();
    let (url, _) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Test))
        .await
        .unwrap();

    until(&mut a, |e| matches!(e, SessionEventWire::ClientJoined { .. })).await;

    // Never streamed: Cancel on a fresh session.
    a.send(SessionCommand::Cancel).await.unwrap();
    let env = next(&mut a).await;
    assert!(matches!(env.event, SessionEventWire::Idle), "got {:?}", env.event);

    a.send(submit("hello")).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;

    a.send(SessionCommand::Cancel).await.unwrap();
    let env = next(&mut a).await;
    assert!(matches!(env.event, SessionEventWire::Idle), "got {:?}", env.event);

    a.send(submit("again")).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    assert!(
        !seen.iter().any(|e| matches!(&e.event,
            SessionEventWire::SystemNotice(n) if n.starts_with("aborted"))),
        "no abort notice"
    );
    let conv = last_conversation(&seen);
    assert_eq!(conv.api_messages.len(), 4);
    assert_eq!(conv.api_messages[2]["content"], "again");
    assert!(conv.abort_context.is_none());
    end(&mut a).await;
}

/// §11 #1: a `Cancel` that lands after `Done` must not scrape the finished
/// turn into `abort_context` (turn_log is cleared with the stream).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn cancel_after_done_does_not_scrape_previous_turn() {
    let _h = Home::new();
    let (url, hits) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Test))
        .await
        .unwrap();

    a.send(submit("first")).await.unwrap();
    // Fire Cancel the moment Done is forwarded; it queues behind the
    // actor's Done handling and must see streaming=false.
    until(&mut a, |e| {
        matches!(
            e,
            SessionEventWire::Stream(StreamEvent::Session(SessionEvent::Done))
        )
    })
    .await;
    a.send(SessionCommand::Cancel).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    assert!(last_conversation(&seen).abort_context.is_none());
    // Drain the Cancel's own Idle echo if it was not already consumed.
    let mut idles = seen.iter().filter(|e| matches!(e.event, SessionEventWire::Idle)).count();
    while idles < 2 {
        let env = next(&mut a).await;
        assert!(
            matches!(env.event, SessionEventWire::Idle),
            "only Idle may follow an idle Cancel, got {:?}",
            env.event
        );
        idles += 1;
    }

    a.send(submit("second")).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    let conv = last_conversation(&seen);
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    assert_eq!(conv.api_messages[2]["content"], "second");
    assert!(!conv.api_messages[2]["content"]
        .as_str()
        .unwrap()
        .contains("ABORT CONTEXT"));
    end(&mut a).await;
}
