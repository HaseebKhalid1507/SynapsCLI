//! A daemon reload must not end work: the turn a reload cut is resumed once
//! per reload (`SessionCommand::ResumeAfterReload`, sent by the new image's
//! rehydrate), and a session whose cut turn was already answered gets
//! nothing.
//!
//! In-process actor, loopback Anthropic SSE stub, isolated HOME. The
//! reload is played as far as the actor sees it: `Checkpoint{Reload}` cuts
//! the running turn, `ReloadAborted` lifts the fence (no exec here), then
//! the rehydrate's command arrives, twice for the same generation.

mod session_actor_common;
use session_actor_common::*;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_engine::session::wire::CHECKPOINT_QUERY_ID;
use agent_engine::session::{
    CheckpointReason, ClientKind, ClientMeta, ClientTransport, Envelope, LocalTransport,
    SessionCommand, SessionEventWire, TurnTrigger,
};
use futures::StreamExt;
use serde_json::Value;
use serial_test::serial;

type Bodies = Arc<Mutex<Vec<Value>>>;

/// The requests numbered in `hang` stream one text delta and never end (a
/// reload cuts them); every other one is a whole reply (`SSE_HI`). Bodies
/// recorded.
async fn provider(hang: &'static [usize]) -> (String, Arc<AtomicUsize>, Bodies) {
    let hits = Arc::new(AtomicUsize::new(0));
    let seen: Bodies = Arc::default();
    let (hits_c, seen_c) = (Arc::clone(&hits), Arc::clone(&seen));
    let app = axum::Router::new().fallback(move |body: axum::body::Bytes| {
        let (hits, seen) = (Arc::clone(&hits_c), Arc::clone(&seen_c));
        async move {
            let hit = hits.fetch_add(1, Ordering::SeqCst);
            if let Ok(v) = serde_json::from_slice::<Value>(&body) {
                seen.lock().unwrap().push(v);
            }
            let hangs = hang.contains(&hit);
            let stream = futures::stream::once(async move {
                Ok::<_, std::io::Error>(axum::body::Bytes::from(if hangs {
                    SSE_PREFIX
                } else {
                    SSE_HI
                }))
            })
            .chain(futures::stream::unfold(hangs, |hangs| async move {
                if !hangs {
                    return None;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
                Some((Ok(axum::body::Bytes::from(": keep-alive\n\n")), hangs))
            }));
            axum::response::Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(axum::body::Body::from_stream(stream))
                .unwrap()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), hits, seen)
}

/// The resume turns among `seen`: `TurnStarted` as a queued message.
fn resume_turns(seen: &[Envelope]) -> Vec<Option<String>> {
    seen.iter()
        .filter_map(|e| match &e.event {
            SessionEventWire::TurnStarted { trigger: TurnTrigger::QueuedAuto, user_text, .. } => {
                Some(user_text.clone())
            }
            _ => None,
        })
        .collect()
}

fn turns_started(seen: &[Envelope]) -> usize {
    seen.iter()
        .filter(|e| matches!(e.event, SessionEventWire::TurnStarted { .. }))
        .count()
}

/// A message's text: a string content, or its text blocks joined.
fn text_of(message: &Value) -> String {
    match &message["content"] {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks.iter().filter_map(|b| b["text"].as_str()).collect(),
        _ => String::new(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_cut_turn_is_resumed_once_per_reload_and_an_answered_one_never() {
    let _h = Home::new();
    let (url, hits, bodies) = provider(&[0]).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let (handle, task) = host.create_session_unspawned(cfg()).await.unwrap();
    tokio::spawn(task.run());
    let (mut client, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Test))
        .await
        .unwrap();

    // A turn in flight, past its first text.
    client.send(submit("never finishes")).await.unwrap();
    until(&mut client, |e| {
        matches!(
            e,
            SessionEventWire::Stream(agent_engine::StreamEvent::Llm(agent_engine::LlmEvent::Text(_)))
        )
    })
    .await;

    // The reload's checkpoint cuts it, and its record says so.
    handle.send(SessionCommand::Checkpoint { reason: CheckpointReason::Reload, deadline: None, skip_quiesce_grace: false }).await.unwrap();
    let seen = until(&mut client, |e| {
        matches!(e, SessionEventWire::QueryResult { id, .. } if *id == CHECKPOINT_QUERY_ID)
    })
    .await;
    let SessionEventWire::QueryResult { value, .. } = &seen.last().unwrap().event else {
        unreachable!()
    };
    assert_eq!(value["record"]["turn_cut"], Value::Bool(true), "{value}");
    // No exec here: lift the fence the way a failed exec does.
    handle.send(SessionCommand::ReloadAborted).await.unwrap();
    assert!(drain_for(&mut client, Duration::from_millis(500)).await.iter().all(|e| {
        !matches!(e.event, SessionEventWire::TurnStarted { .. })
    }));
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    // The rehydrate's command, twice for the same reload: ONE resume turn.
    let at = chrono::DateTime::parse_from_rfc3339("2026-10-19T05:46:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let resume = |generation, turn_cut| SessionCommand::ResumeAfterReload {
        generation,
        reloaded_at: at,
        turn_cut,
        cut_tail: None,
        stopped: false,
        reply: None,
    };
    handle.send(resume(7, Some(true))).await.unwrap();
    handle.send(resume(7, Some(true))).await.unwrap();
    let seen = drain_for(&mut client, Duration::from_millis(2500)).await;
    let text = agent_engine::daemon::reload::resume_text(at);
    assert_eq!(resume_turns(&seen), vec![Some(text.clone())], "{seen:#?}");
    assert_eq!(turns_started(&seen), 1, "{seen:#?}");
    assert!(seen.iter().any(|e| matches!(e.event, SessionEventWire::Idle)), "the resumed turn ended");
    assert_eq!(hits.load(Ordering::SeqCst), 2, "one new provider request");
    let sent = bodies.lock().unwrap()[1].clone();
    let messages = sent["messages"].as_array().unwrap();
    // The request merges the marker and the resume (two user messages in
    // history) into its newest user message.
    let newest = text_of(messages.last().unwrap());
    let marker = agent_engine::engine::interrupt::InterruptReason::Restart.marker();
    assert!(
        newest.starts_with(marker) && newest.ends_with(&text),
        "the resume follows the cut turn's marker: {newest}"
    );
    assert_eq!(messages[messages.len() - 2]["role"], "assistant", "the cut turn's partial reply");

    // A later reload of the same session: its cut turn is answered now, so
    // nothing is queued, whatever the record says.
    handle.send(resume(8, None)).await.unwrap();
    handle.send(resume(9, Some(true))).await.unwrap();
    let seen = drain_for(&mut client, Duration::from_millis(1000)).await;
    assert_eq!(turns_started(&seen), 0, "{seen:#?}");
    assert_eq!(hits.load(Ordering::SeqCst), 2);

    end(&mut client).await;
}

/// A session the reload found idle (`turn_cut: false`) gets nothing, even
/// when its history still ends with an earlier reload's unanswered marker.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn an_idle_session_is_not_resumed() {
    let _h = Home::new();
    let (url, hits, _bodies) = provider(&[0]).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let (handle, task) = host.create_session_unspawned(cfg()).await.unwrap();
    tokio::spawn(task.run());
    let (mut client, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Test))
        .await
        .unwrap();
    client.send(submit("never finishes")).await.unwrap();
    until(&mut client, |e| {
        matches!(
            e,
            SessionEventWire::Stream(agent_engine::StreamEvent::Llm(agent_engine::LlmEvent::Text(_)))
        )
    })
    .await;
    handle.send(SessionCommand::Checkpoint { reason: CheckpointReason::Reload, deadline: None, skip_quiesce_grace: false }).await.unwrap();
    until(&mut client, |e| {
        matches!(e, SessionEventWire::QueryResult { id, .. } if *id == CHECKPOINT_QUERY_ID)
    })
    .await;
    handle.send(SessionCommand::ReloadAborted).await.unwrap();

    // A second reload finds it idle: its checkpoint records no cut.
    handle.send(SessionCommand::Checkpoint { reason: CheckpointReason::Reload, deadline: None, skip_quiesce_grace: false }).await.unwrap();
    let seen = until(&mut client, |e| {
        matches!(e, SessionEventWire::QueryResult { id, .. } if *id == CHECKPOINT_QUERY_ID)
    })
    .await;
    let SessionEventWire::QueryResult { value, .. } = &seen.last().unwrap().event else {
        unreachable!()
    };
    assert_eq!(value["record"]["turn_cut"], Value::Bool(false), "{value}");
    handle.send(SessionCommand::ReloadAborted).await.unwrap();
    handle
        .send(SessionCommand::ResumeAfterReload {
            generation: 3,
            reloaded_at: chrono::Utc::now(),
            turn_cut: Some(false),
            cut_tail: None,
            stopped: false,
            reply: None,
        })
        .await
        .unwrap();
    let seen = drain_for(&mut client, Duration::from_millis(1000)).await;
    assert_eq!(turns_started(&seen), 0, "{seen:#?}");
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    end(&mut client).await;
}

/// The reload being rehydrated decides, never an earlier one: a session
/// whose unanswered restart marker an earlier reload left (whose image
/// resumed nothing) is NOT resumed by a reload whose record has no
/// `turn_cut` (an older image did it), while a turn this reload cut
/// (`turn_cut: Some(true)`) is, once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_marker_from_an_earlier_reload_is_not_resumed_and_one_this_reload_wrote_is() {
    let _h = Home::new();
    let (url, hits, bodies) = provider(&[0, 1]).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let (handle, task) = host.create_session_unspawned(cfg()).await.unwrap();
    tokio::spawn(task.run());
    let (mut client, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Test))
        .await
        .unwrap();
    let text_seen = |e: &SessionEventWire| {
        matches!(
            e,
            SessionEventWire::Stream(agent_engine::StreamEvent::Llm(agent_engine::LlmEvent::Text(_)))
        )
    };
    let checkpoint_record = |seen: &Vec<Envelope>| {
        let SessionEventWire::QueryResult { value, .. } = &seen.last().unwrap().event else {
            unreachable!()
        };
        value["record"]["turn_cut"].clone()
    };

    // An earlier reload cuts a turn; its image resumes nothing (no exec
    // here: the fence lifts the way a failed exec lifts it).
    client.send(submit("cut by an earlier reload")).await.unwrap();
    until(&mut client, text_seen).await;
    handle.send(SessionCommand::Checkpoint { reason: CheckpointReason::Reload, deadline: None, skip_quiesce_grace: false }).await.unwrap();
    let seen = until(&mut client, |e| {
        matches!(e, SessionEventWire::QueryResult { id, .. } if *id == CHECKPOINT_QUERY_ID)
    })
    .await;
    assert_eq!(checkpoint_record(&seen), Value::Bool(true));
    handle.send(SessionCommand::ReloadAborted).await.unwrap();

    // This reload's record has no `turn_cut` (an older image did it): the
    // history still ends with the earlier, unanswered marker, and nothing
    // is queued.
    let at = chrono::DateTime::parse_from_rfc3339("2026-10-19T19:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    handle
        .send(SessionCommand::ResumeAfterReload {
            generation: 11,
            reloaded_at: at,
            turn_cut: None,
            cut_tail: None,
            stopped: false,
            reply: None,
        })
        .await
        .unwrap();
    let seen = drain_for(&mut client, Duration::from_millis(1000)).await;
    assert_eq!(turns_started(&seen), 0, "{seen:#?}");
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    // A turn this reload cuts, recorded as such: resumed, once.
    client.send(submit("cut by this reload")).await.unwrap();
    until(&mut client, text_seen).await;
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    handle.send(SessionCommand::Checkpoint { reason: CheckpointReason::Reload, deadline: None, skip_quiesce_grace: false }).await.unwrap();
    let seen = until(&mut client, |e| {
        matches!(e, SessionEventWire::QueryResult { id, .. } if *id == CHECKPOINT_QUERY_ID)
    })
    .await;
    assert_eq!(checkpoint_record(&seen), Value::Bool(true));
    handle.send(SessionCommand::ReloadAborted).await.unwrap();
    for _ in 0..2 {
        handle
            .send(SessionCommand::ResumeAfterReload {
                generation: 12,
                reloaded_at: at,
                turn_cut: Some(true),
                cut_tail: None,
                stopped: false,
                reply: None,
            })
            .await
            .unwrap();
    }
    let seen = drain_for(&mut client, Duration::from_millis(2500)).await;
    let text = agent_engine::daemon::reload::resume_text(at);
    assert_eq!(resume_turns(&seen), vec![Some(text.clone())], "{seen:#?}");
    assert_eq!(turns_started(&seen), 1, "{seen:#?}");
    assert_eq!(hits.load(Ordering::SeqCst), 3, "one new provider request");
    let sent = bodies.lock().unwrap()[2].clone();
    let newest = text_of(sent["messages"].as_array().unwrap().last().unwrap());
    assert!(newest.ends_with(&text), "the resume is the newest message: {newest}");

    end(&mut client).await;
}

/// A clean stop's record restored by a later start (`stopped`): the same
/// once-only rules, the stop's message, and the actor's decision answered
/// for the restore's summary.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_turn_a_clean_stop_cut_is_resumed_once_with_the_stops_message() {
    let _h = Home::new();
    let (url, hits, _bodies) = provider(&[0]).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let (handle, task) = host.create_session_unspawned(cfg()).await.unwrap();
    tokio::spawn(task.run());
    let (mut client, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Test))
        .await
        .unwrap();
    client.send(submit("never finishes")).await.unwrap();
    until(&mut client, |e| {
        matches!(
            e,
            SessionEventWire::Stream(agent_engine::StreamEvent::Llm(agent_engine::LlmEvent::Text(_)))
        )
    })
    .await;
    // The stop checkpoints the session as a reload does.
    handle
        .send(SessionCommand::Checkpoint { reason: CheckpointReason::Reload, deadline: None, skip_quiesce_grace: false })
        .await
        .unwrap();
    until(&mut client, |e| {
        matches!(e, SessionEventWire::QueryResult { id, .. } if *id == CHECKPOINT_QUERY_ID)
    })
    .await;
    handle.send(SessionCommand::ReloadAborted).await.unwrap();
    drain_for(&mut client, Duration::from_millis(300)).await;

    let at = chrono::DateTime::parse_from_rfc3339("2026-10-19T18:02:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let ask = |turn_cut| {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let cmd = SessionCommand::ResumeAfterReload {
            generation: 10,
            reloaded_at: at,
            turn_cut,
            cut_tail: Some(0),
            stopped: true,
            reply: Some(tx),
        };
        (cmd, rx)
    };
    let (cmd, rx) = ask(Some(true));
    handle.send(cmd).await.unwrap();
    assert_eq!(rx.await.unwrap(), "resumed");
    let (cmd, rx) = ask(Some(true));
    handle.send(cmd).await.unwrap();
    assert_eq!(rx.await.unwrap(), "skip", "once per record");
    let seen = drain_for(&mut client, Duration::from_millis(2500)).await;
    let text = agent_engine::daemon::reload::stop_resume_text(at);
    assert!(text.starts_with("resume: the daemon was stopped and started again mid-turn (18:02 UTC)"));
    assert_eq!(resume_turns(&seen), vec![Some(text)], "{seen:#?}");
    assert_eq!(turns_started(&seen), 1, "{seen:#?}");
    assert_eq!(hits.load(Ordering::SeqCst), 2, "one new provider request");

    end(&mut client).await;
}

/// Post one event to the session's own inbox socket, as a subagent
/// completion, an inbox wake or a fleet message arrives.
async fn post_event(session_id: &str, text: &str) {
    let reg = agent_engine::events::registry::find_session_registration(session_id).expect("registered");
    let event = agent_engine::events::types::Event::simple(
        "test",
        text,
        Some(agent_engine::events::types::Severity::High),
    );
    // The platform transport (a Unix socket, or a named pipe on Windows).
    agent_engine::events::socket::send_to_session_socket(&reg.socket_path, &serde_json::to_vec(&event).unwrap())
        .await
        .unwrap();
}

/// A turn in flight that an event reached, cut by a reload (or stop)
/// checkpoint, which flushes the event in after the turn's marker; the
/// fence lifted as a failed exec lifts it. Returns the checkpoint's record.
async fn cut_a_turn_an_event_reached(
    handle: &agent_engine::session::SessionHandle,
    client: &mut LocalTransport,
) -> Value {
    client.send(submit("never finishes")).await.unwrap();
    until(client, |e| {
        matches!(
            e,
            SessionEventWire::Stream(agent_engine::StreamEvent::Llm(agent_engine::LlmEvent::Text(_)))
        )
    })
    .await;
    post_event(handle.id.as_str(), "sa_3 finished").await;
    until(client, |e| matches!(e, SessionEventWire::External(_))).await;
    handle
        .send(SessionCommand::Checkpoint { reason: CheckpointReason::Reload, deadline: None, skip_quiesce_grace: false })
        .await
        .unwrap();
    let seen = until(client, |e| {
        matches!(e, SessionEventWire::QueryResult { id, .. } if *id == CHECKPOINT_QUERY_ID)
    })
    .await;
    let SessionEventWire::QueryResult { value, .. } = &seen.last().unwrap().event else {
        unreachable!()
    };
    let record = value["record"].clone();
    assert_eq!(record["turn_cut"], Value::Bool(true), "{record}");
    // The event is the cut turn's own tail, after its marker.
    assert_eq!(record["cut_tail"], serde_json::json!(1), "{record}");
    handle.send(SessionCommand::ReloadAborted).await.unwrap();
    drain_for(client, Duration::from_millis(300)).await;
    record
}

fn ask_resume(
    record: &Value,
    cut_tail: Option<u32>,
    stopped: bool,
    at: chrono::DateTime<chrono::Utc>,
) -> (SessionCommand, tokio::sync::oneshot::Receiver<&'static str>) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let cmd = SessionCommand::ResumeAfterReload {
        generation: 21,
        reloaded_at: at,
        turn_cut: record["turn_cut"].as_bool(),
        cut_tail,
        stopped,
        reply: Some(tx),
    };
    (cmd, rx)
}

/// An event that reached the turn before the reload cut it is part of
/// that turn (flushed after its marker at the cut), not a re-prompt: the
/// turn is resumed exactly once. Counted from the marker alone, the event
/// would look like a re-prompt (`cut_tail: Some(0)` shows that skip).
async fn an_event_inside_the_cut_turn_does_not_stop_its_resume(stopped: bool) {
    let _h = Home::new();
    let (url, hits, bodies) = provider(&[0]).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let (handle, task) = host.create_session_unspawned(cfg()).await.unwrap();
    tokio::spawn(task.run());
    let (mut client, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Test))
        .await
        .unwrap();
    let record = cut_a_turn_an_event_reached(&handle, &mut client).await;
    let tail = record["cut_tail"].as_u64().map(|n| n as u32);
    let at = chrono::DateTime::parse_from_rfc3339("2026-10-19T19:05:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);

    let (cmd, rx) = ask_resume(&record, Some(0), stopped, at);
    handle.send(cmd).await.unwrap();
    assert_eq!(rx.await.unwrap(), "skip", "the tail counted as a re-prompt");
    let (cmd, rx) = ask_resume(&record, tail, stopped, at);
    handle.send(cmd).await.unwrap();
    assert_eq!(rx.await.unwrap(), "resumed");
    let (cmd, rx) = ask_resume(&record, tail, stopped, at);
    handle.send(cmd).await.unwrap();
    assert_eq!(rx.await.unwrap(), "skip", "once per record");

    let seen = drain_for(&mut client, Duration::from_millis(2500)).await;
    let text = if stopped {
        agent_engine::daemon::reload::stop_resume_text(at)
    } else {
        agent_engine::daemon::reload::resume_text(at)
    };
    assert_eq!(resume_turns(&seen), vec![Some(text.clone())], "{seen:#?}");
    assert_eq!(turns_started(&seen), 1, "{seen:#?}");
    assert_eq!(hits.load(Ordering::SeqCst), 2, "one new provider request");
    let sent = bodies.lock().unwrap()[1].clone();
    let body = serde_json::to_string(&sent).unwrap();
    assert!(body.contains("sa_3 finished"), "the event stays in history: {body}");
    let newest = text_of(sent["messages"].as_array().unwrap().last().unwrap());
    assert!(newest.ends_with(&text), "the resume is the newest message: {newest}");

    end(&mut client).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_reload_resumes_a_cut_turn_an_event_reached_once() {
    an_event_inside_the_cut_turn_does_not_stop_its_resume(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_clean_stop_resumes_a_cut_turn_an_event_reached_once() {
    an_event_inside_the_cut_turn_does_not_stop_its_resume(true).await;
}

/// An event that arrives AFTER the cut (an operator's sweep or a fleet
/// message re-prompting the session, which arrive as events) is a
/// re-prompt: the cut turn is not resumed a second time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn an_event_after_the_cut_counts_as_a_reprompt_and_nothing_is_resumed() {
    let _h = Home::new();
    // The cut turn and the turn the later event starts both hang.
    let (url, hits, _bodies) = provider(&[0, 1]).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let (handle, task) = host.create_session_unspawned(cfg()).await.unwrap();
    tokio::spawn(task.run());
    let (mut client, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Test))
        .await
        .unwrap();
    let record = cut_a_turn_an_event_reached(&handle, &mut client).await;
    let tail = record["cut_tail"].as_u64().map(|n| n as u32);

    post_event(handle.id.as_str(), "resume: the daemon reloaded; pick up the cut turn").await;
    until(&mut client, |e| {
        matches!(e, SessionEventWire::TurnStarted { trigger: TurnTrigger::EventAuto, .. })
    })
    .await;
    // The event's turn reaches the provider as the stub's second request.
    // `TurnStarted` is emitted before that request goes out, so wait for it
    // (bounded) rather than reading the count at once.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while hits.load(Ordering::SeqCst) < 2 {
        assert!(tokio::time::Instant::now() < deadline, "the event's turn never reached the provider");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(hits.load(Ordering::SeqCst), 2);

    let at = chrono::Utc::now();
    for stopped in [false, true] {
        let (cmd, rx) = ask_resume(&record, tail, stopped, at);
        handle.send(cmd).await.unwrap();
        assert_eq!(rx.await.unwrap(), "skip", "re-prompted after the cut (stopped: {stopped})");
    }
    let seen = drain_for(&mut client, Duration::from_millis(1000)).await;
    assert!(resume_turns(&seen).is_empty(), "{seen:#?}");
    assert!(
        !seen.iter().any(|e| matches!(&e.event, SessionEventWire::Steered { text, .. } if text.starts_with("resume: the daemon"))),
        "no resume queued behind the event's turn: {seen:#?}"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 2, "no new provider request");

    end(&mut client).await;
}
