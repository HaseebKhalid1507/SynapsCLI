//! `fork` (side chats) through the real session actor: the fork's
//! first request carries its parent's prefix byte for byte; it copies the
//! history up to the last completed turn, never the turn in flight, the
//! parent's prompts or workers; it starts no turn; it carries the sandbox
//! policy file; a fork can't fork; `forked_from` is in Status and the
//! journal.

mod session_actor_common;
use session_actor_common::*;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_engine::session::{
    ClientKind, ClientMeta, ClientTransport, LocalTransport, SessionCommand, SessionEventWire,
    SessionHandle, SessionId, SessionLifecycle, SessionQuery, SessionSetting,
};
use axum::response::IntoResponse;
use futures::StreamExt;
use serial_test::serial;

/// A user message with this text gets a stream that never ends.
const LONG: &str = "FORK-TEST-LONG-TURN";
/// The parent's extension session injection (set as a hook would).
const INJECTION: &str = "FORK-TEST-SESSION-INJECTION";
/// The parent's `/system` text.
const SYSTEM: &str = "FORK-TEST-SYSTEM-PROMPT.";
/// The message the fork and its parent each send after the fork.
const AGAIN: &str = "FORK-TEST-AGAIN";
/// The control session's message.
const CONTROL: &str = "FORK-TEST-CONTROL";

/// Records every request body. A streaming request gets `SSE_HI`, or a
/// stream that never ends when it carries [`LONG`]; anything else a JSON
/// message.
async fn recording_stub() -> (String, Arc<Mutex<Vec<String>>>) {
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let b = Arc::clone(&bodies);
    let app = axum::Router::new().fallback(move |body: String| {
        let b = Arc::clone(&b);
        async move {
            let streaming = body.contains("\"stream\":true");
            let long = body.contains(LONG);
            b.lock().unwrap().push(body);
            if streaming && long {
                let stream = futures::stream::once(async move {
                    Ok::<_, std::io::Error>(axum::body::Bytes::from(SSE_PREFIX))
                })
                .chain(futures::stream::unfold((), |()| async {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    Some((Ok(axum::body::Bytes::from(": keep-alive\n\n")), ()))
                }));
                return axum::response::Response::builder()
                    .status(200)
                    .header("content-type", "text/event-stream")
                    .body(axum::body::Body::from_stream(stream))
                    .unwrap();
            }
            if streaming {
                return (
                    axum::http::StatusCode::OK,
                    [("content-type", "text/event-stream")],
                    SSE_HI.to_string(),
                )
                    .into_response();
            }
            axum::Json(serde_json::json!({
                "id": "msg_c", "type": "message", "role": "assistant",
                "model": MODEL, "stop_reason": "end_turn",
                "content": [{"type": "text", "text": "summary"}],
                "usage": {"input_tokens": 10, "output_tokens": 5}
            }))
            .into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), bodies)
}

/// The streaming request bodies that carry `text`, in order.
fn streamed_with(bodies: &Arc<Mutex<Vec<String>>>, text: &str) -> Vec<String> {
    bodies
        .lock()
        .unwrap()
        .iter()
        .filter(|b| b.contains("\"stream\":true") && b.contains(text))
        .cloned()
        .collect()
}

/// A request's cached prefix as sent (tools → system → messages), with the
/// model and thinking settings: each field's raw JSON bytes.
fn prefix(body: &str) -> Vec<(&'static str, String)> {
    let fields: BTreeMap<String, Box<serde_json::value::RawValue>> =
        serde_json::from_str(body).expect("a JSON request body");
    ["tools", "system", "messages", "model", "thinking"]
        .into_iter()
        .map(|k| (k, fields.get(k).map(|v| v.get().to_string()).unwrap_or_default()))
        .collect()
}

fn write_config(home: &Home, body: &str) {
    std::fs::write(home.dir.path().join(".synaps-cli").join("config"), body).unwrap();
}

async fn attach(handle: SessionHandle) -> LocalTransport {
    LocalTransport::attach(handle, ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap()
        .0
}

async fn turn(t: &mut LocalTransport, text: &str) {
    t.send(submit(text)).await.unwrap();
    until(t, |e| matches!(e, SessionEventWire::Idle)).await;
}

async fn query(t: &mut LocalTransport, id: u64, query: SessionQuery) -> serde_json::Value {
    t.send(SessionCommand::Query { id, query }).await.unwrap();
    let seen = until(t, |e| matches!(e, SessionEventWire::QueryResult { id: i, .. } if *i == id)).await;
    match &seen.last().unwrap().event {
        SessionEventWire::QueryResult { value, .. } => value.clone(),
        _ => unreachable!(),
    }
}

/// What a fork answers: `Ok((session_id, message_count))` from `forked`,
/// `Err(reason)` from `refused` (`command: "fork"`).
async fn fork(t: &mut LocalTransport, label: &str) -> Result<(String, usize), String> {
    // As a client asks (the daemon's connection stamps its client id too).
    t.send_from_self(SessionCommand::Fork { label: label.into() }).await.unwrap();
    let seen = until(t, |e| match e {
        SessionEventWire::Forked { .. } => true,
        SessionEventWire::Refused { command, .. } => command == "fork",
        _ => false,
    })
    .await;
    match &seen.last().unwrap().event {
        SessionEventWire::Forked {
            client,
            session_id,
            label: answered,
            message_count,
        } => {
            assert_eq!(*client, Some(t.client_id()), "addressed to the asking client");
            assert_eq!(answered, label);
            Ok((session_id.clone(), *message_count))
        }
        SessionEventWire::Refused { reason, .. } => Err(reason.clone()),
        _ => unreachable!(),
    }
}

fn child_handle(host: &agent_engine::EngineHost, id: &str) -> SessionHandle {
    host.attach(&SessionId::from(id)).expect("the fork is a live session of this host")
}

async fn wait_lifecycle(handle: &SessionHandle, want: SessionLifecycle) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        if handle.lifecycle() == want {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

/// Registered after the parent's first turn froze its tool catalog.
struct LateTool;

#[async_trait::async_trait]
impl agent_engine::Tool for LateTool {
    fn name(&self) -> &str {
        "late_tool"
    }
    fn description(&self) -> &str {
        "Registered after the parent's catalog froze."
    }
    fn parameters(&self) -> agent_engine::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    fn origin(&self) -> agent_engine::tools::ToolOrigin {
        agent_engine::tools::ToolOrigin::Builtin
    }
    async fn execute(
        &self,
        _params: agent_engine::Value,
        _ctx: agent_engine::ToolContext,
    ) -> agent_engine::Result<String> {
        Ok(String::new())
    }
}

/// THE cache guarantee: the fork's first request and the parent's request
/// for the same messages carry the same tools, system and messages, byte for
/// byte (and the same model and thinking). The parent's prompt carries a
/// `/system` text, its extension session injection and a frozen tool
/// catalog that a fresh session would render differently (`late_tool`).
///
/// Mutants, each failing here: the fork does not copy the injection (runs its
/// own hook), the frozen catalog, or the history; or it takes its prompt from
/// the journal (no settings replay, no runtime prompt copy): the `/system`
/// text is runtime-only, so the fork's system block loses it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_forks_first_request_carries_its_parents_prefix_byte_for_byte() {
    let h = Home::new();
    write_config(&h, "progressive_tool_disclosure = true\n");
    let (url, bodies) = recording_stub().await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let parent_id = handle.id.to_string();
    host.parts()
        .hook_bus
        .set_session_injection_for(&parent_id, INJECTION.into())
        .await;
    let mut t = attach(handle.clone()).await;
    t.send(SessionCommand::Set {
        id: 1,
        setting: SessionSetting::SystemPrompt { text: SYSTEM.into() },
    })
    .await
    .unwrap();
    t.send(SessionCommand::Set {
        id: 2,
        setting: SessionSetting::ReasoningLevel {
            level: agent_core::reasoning::ReasoningLevel::Low,
        },
    })
    .await
    .unwrap();
    turn(&mut t, "hello").await;
    // The parent's catalog is frozen; a fresh session would list this tool.
    host.parts().tools.write().await.register(Arc::new(LateTool));

    let (child_id, message_count) = fork(&mut t, "Side: byte identity").await.expect("forked");
    assert_ne!(child_id, parent_id);
    assert_eq!(message_count, 2, "the completed turn: prompt and reply");
    let mut c = attach(child_handle(&host, &child_id)).await;
    turn(&mut c, AGAIN).await;
    turn(&mut t, AGAIN).await;

    let sent = streamed_with(&bodies, AGAIN);
    assert_eq!(sent.len(), 2, "one request each");
    let (child, parent) = (prefix(&sent[0]), prefix(&sent[1]));
    let parent_text: String = parent.iter().map(|(_, v)| v.as_str()).collect();
    assert!(parent_text.contains(INJECTION), "precondition: the injection is sent");
    assert!(parent_text.contains(SYSTEM), "precondition: the /system text is sent");
    assert!(parent_text.contains("Activatable tools"), "precondition: a catalog is sent");
    // (`tools` lists every deferred tool of the live catalog, the late one
    // too, for parent and fork alike; the frozen catalog is in `system`.)
    let system_of = |p: &[(&'static str, String)]| p.iter().find(|(k, _)| *k == "system").unwrap().1.clone();
    assert!(
        !system_of(&parent).contains("late_tool"),
        "precondition: the parent's catalog is frozen"
    );
    for ((name, c), (_, p)) in child.iter().zip(parent.iter()) {
        assert_eq!(c, p, "the fork's `{name}` differs from its parent's");
    }

    // The control: a fresh session's catalog does list the late tool, so the
    // fork's matching prefix is the copy, not the same catalog by chance.
    let fresh = host.create_session(cfg()).await.unwrap();
    let mut f = attach(fresh).await;
    turn(&mut f, CONTROL).await;
    let control = streamed_with(&bodies, CONTROL);
    assert_eq!(control.len(), 1);
    assert!(
        system_of(&prefix(&control[0])).contains("late_tool"),
        "a fresh catalog lists the late tool"
    );
    end(&mut f).await;
    end(&mut c).await;
    end(&mut t).await;
}

/// A fork made while a turn runs copies the history before that turn: not
/// the turn's prompt, nor anything the turn adds. Mutant: copy the live
/// history while streaming (the count is 3, the fork holds the prompt).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_turn_in_flight_is_not_copied() {
    let _h = Home::new();
    let (url, bodies) = recording_stub().await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let mut t = attach(handle.clone()).await;
    turn(&mut t, "hello").await;
    t.send(submit(LONG)).await.unwrap();
    // The turn is in flight once its request reached the server.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while streamed_with(&bodies, LONG).is_empty() {
        assert!(tokio::time::Instant::now() < deadline, "the long turn's request");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let (child_id, message_count) = fork(&mut t, "Side: mid-turn").await.expect("forked");
    assert_eq!(message_count, 2, "the history before the running turn");
    let mut c = attach(child_handle(&host, &child_id)).await;
    let messages = query(&mut c, 10, SessionQuery::Messages).await;
    let messages = messages.as_array().expect("messages");
    assert_eq!(messages.len(), 2);
    assert!(!serde_json::to_string(messages).unwrap().contains(LONG), "no in-flight prompt");
    let status = query(&mut c, 11, SessionQuery::Status).await;
    assert_eq!(status["streaming"], false, "{status}");
    t.send(SessionCommand::Cancel).await.unwrap();
    until(&mut t, |e| matches!(e, SessionEventWire::Idle)).await;
    end(&mut c).await;
    end(&mut t).await;
}

/// A fork starts no turn (no request until its first message) and copies
/// none of the parent's running work: no pending prompt, no turn, no
/// workers. Here the parent's turn waits on a secret prompt when it forks.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_fork_starts_no_turn_and_copies_no_prompt_or_worker() {
    let _h = Home::new();
    let (url, hits) = stub_seq(&[SSE_PROMPT_TOOL_USE, SSE_HI]).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = prompt_host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let mut t = attach(handle.clone()).await;
    t.send(submit("ask me")).await.unwrap();
    until(&mut t, |e| matches!(e, SessionEventWire::Prompt(_))).await;
    let parent = query(&mut t, 20, SessionQuery::Status).await;
    assert_eq!(parent["pending_prompts"], 1, "precondition: {parent}");
    let before = hits.load(std::sync::atomic::Ordering::SeqCst);

    let (child_id, message_count) = fork(&mut t, "Side: no work").await.expect("forked");
    assert_eq!(message_count, 0, "no completed turn yet");
    let mut c = attach(child_handle(&host, &child_id)).await;
    let status = query(&mut c, 21, SessionQuery::Status).await;
    assert_eq!(status["pending_prompts"], 0, "{status}");
    assert_eq!(status["streaming"], false, "{status}");
    assert_eq!(status["messages"], 0, "{status}");
    let rows = query(&mut c, 22, SessionQuery::SubagentRows).await;
    assert_eq!(rows, serde_json::json!([]), "no workers");
    let seen = drain_for(&mut c, Duration::from_millis(500)).await;
    assert!(
        !seen.iter().any(|e| matches!(e.event, SessionEventWire::TurnStarted { .. })),
        "no turn starts"
    );
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), before, "no request");
    end(&mut c).await;
    end(&mut t).await;
}

/// The parent's sandbox policy file is carried to the fork's id, byte for
/// byte; a policy that can't be carried refuses the fork and leaves no
/// session. Mutants: skip the carry (no file), drop the carried policy
/// without `keep()` (rolled back), or ignore a carry error (a fork without
/// its parent's policy).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_forks_sandbox_policy_is_carried_or_the_fork_is_refused() {
    let h = Home::new();
    let (url, _bodies) = recording_stub().await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let parent_id = handle.id.to_string();
    let base = h.dir.path().join(".synaps-cli");
    let sessions = base.join("sandbox").join("sessions");
    let lane = h.dir.path().join("lane");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::create_dir_all(&lane).unwrap();
    let policy = format!("mode = enforce\nwrite = {}\ncontact = Ask your foreman.\n", lane.display());
    std::fs::write(sessions.join(format!("{parent_id}.conf")), &policy).unwrap();
    let mut t = attach(handle.clone()).await;
    turn(&mut t, "hello").await;

    let (child_id, _) = fork(&mut t, "Side: sandbox").await.expect("forked");
    let carried = std::fs::read_to_string(sessions.join(format!("{child_id}.conf")))
        .expect("the fork's policy file");
    assert_eq!(carried, policy);

    // A policy that can't be carried: no fork.
    std::fs::write(sessions.join(format!("{parent_id}.conf")), "mode = sideways\n").unwrap();
    let live = host.sessions().len();
    let refused = fork(&mut t, "Side: bad policy").await.expect_err("refused");
    assert!(refused.contains("sandbox policy"), "{refused}");
    assert_eq!(host.sessions().len(), live, "no fork session is left");
    let mut c = attach(child_handle(&host, &child_id)).await;
    end(&mut c).await;
    end(&mut t).await;
}

/// A fork can't fork, and a fork's label is checked. Mutant: drop the
/// `forked_from` check and the fork forks.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_fork_cannot_fork_and_a_label_is_checked() {
    let _h = Home::new();
    let (url, _bodies) = recording_stub().await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let mut t = attach(handle.clone()).await;
    assert_eq!(fork(&mut t, "  ").await, Err("fork label is empty".to_string()));
    let long = "x".repeat(201);
    assert_eq!(
        fork(&mut t, &long).await,
        Err("fork label is longer than 200 bytes".to_string())
    );
    let (child_id, _) = fork(&mut t, "Side: once").await.expect("forked");
    let mut c = attach(child_handle(&host, &child_id)).await;
    assert_eq!(fork(&mut c, "Side: twice").await, Err("a forked session can't fork".to_string()));
    end(&mut c).await;
    end(&mut t).await;
}

/// `forked_from` names the parent, the copied message count and the time,
/// in the fork's Status (live and parked) and its journal; a session that
/// is no fork has `null`. Mutants: drop it from either Status branch, or
/// from the fork's first save.
///
/// The parent's `/system` text is runtime-only (never journaled): the fork
/// keeps it in its own settings replay, so it is still sent after the fork's
/// own park and unpark (a rebuild from the journal). Mutant: the fork starts
/// with an empty settings replay (its rebuilt runtime loses the text).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn forked_from_is_in_status_and_the_journal() {
    let _h = Home::new();
    let (url, bodies) = recording_stub().await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host
        .create_session(agent_engine::session::SessionConfig { persist: true, ..cfg() })
        .await
        .unwrap();
    let parent_id = handle.id.to_string();
    let mut t = attach(handle.clone()).await;
    t.send(SessionCommand::Set {
        id: 1,
        setting: SessionSetting::SystemPrompt { text: SYSTEM.into() },
    })
    .await
    .unwrap();
    turn(&mut t, "hello").await;
    let before = agent_engine::epoch_millis();
    let (child_id, _) = fork(&mut t, "Side: lineage").await.expect("forked");

    let parent = query(&mut t, 30, SessionQuery::Status).await;
    assert_eq!(parent["forked_from"], serde_json::Value::Null, "{parent}");
    let child = child_handle(&host, &child_id);
    let mut c = attach(child.clone()).await;
    let status = query(&mut c, 31, SessionQuery::Status).await;
    let from = &status["forked_from"];
    assert_eq!(from["session"], parent_id.as_str(), "{status}");
    assert_eq!(from["message_count"], 2, "{status}");
    assert!(from["at_ms"].as_u64().unwrap() >= before, "{status}");

    let journal = agent_core::session::Session::load(&child_id).expect("the fork's journal");
    let saved = journal.forked_from.clone().expect("forked_from in the journal");
    assert_eq!(saved.session, parent_id);
    assert_eq!(saved.message_count, 2);
    assert_eq!(saved.at_ms, from["at_ms"].as_u64().unwrap());
    assert_eq!(journal.api_messages.len(), 2);
    assert!(
        !serde_json::to_string(&journal).unwrap().contains(SYSTEM),
        "precondition: the /system text is runtime-only"
    );

    c.send(SessionCommand::Detach { client: c.client_id() }).await.unwrap();
    child.send(SessionCommand::Park).await.unwrap();
    assert!(wait_lifecycle(&child, SessionLifecycle::Parked).await, "the fork parks");
    // Asked without attaching: an attach would unpark it.
    let mut events = child.subscribe();
    child
        .send(SessionCommand::Query { id: 32, query: SessionQuery::Status })
        .await
        .unwrap();
    let parked = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let SessionEventWire::QueryResult { id: 32, value, .. } = events.recv().await.unwrap().event {
                return value;
            }
        }
    })
    .await
    .expect("the parked Status");
    assert_eq!(parked["lifecycle"], serde_json::json!(SessionLifecycle::Parked), "{parked}");
    assert_eq!(&parked["forked_from"], from, "{parked}");

    // The attach unparks the fork: its runtime is rebuilt from its journal
    // plus its settings replay, which still sets the parent's /system text.
    let mut c = attach(child.clone()).await;
    assert_eq!(child.lifecycle(), SessionLifecycle::Live, "the attach unparked it");
    turn(&mut c, AGAIN).await;
    let sent = streamed_with(&bodies, AGAIN);
    assert_eq!(sent.len(), 1);
    let system = prefix(&sent[0]).into_iter().find(|(k, _)| *k == "system").unwrap().1;
    assert!(system.contains(SYSTEM), "the fork still sends the parent's /system text");
    end(&mut c).await;
    end(&mut t).await;
}
