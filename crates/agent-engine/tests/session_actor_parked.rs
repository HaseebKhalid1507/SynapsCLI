//! B3 — `Parked` (PLAN-phase3 §2.5, §5.3): grace timer, guards, park/unpark,
//! event wake, `AttachRefused`, lifecycle events, no duplicate index record.

mod session_actor_common;
use session_actor_common::*;

use std::sync::Arc;
use std::time::Duration;

use agent_engine::session::{
    ClientKind, ClientMeta, ClientTransport, EndReason, LocalTransport, SessionCommand,
    SessionConfig, SessionEventWire, SessionHandle, SessionLifecycle, SessionQuery,
    SessionSetting, TransportError, TurnTrigger,
};
use serial_test::serial;

fn pcfg() -> SessionConfig {
    SessionConfig {
        persist: true,
        ..cfg()
    }
}

struct Grace;
impl Grace {
    fn set(v: &str) -> Self {
        std::env::set_var("SYNAPS_DAEMON_PARK_GRACE_SECS", v);
        Self
    }
}
impl Drop for Grace {
    fn drop(&mut self) {
        std::env::remove_var("SYNAPS_DAEMON_PARK_GRACE_SECS");
    }
}

async fn detach(t: &mut LocalTransport) {
    t.send(SessionCommand::Detach {
        client: t.client_id(),
    })
    .await
    .unwrap();
    until(t, |e| matches!(e, SessionEventWire::ClientLeft { .. })).await;
}

/// Poll `handle.lifecycle()` until `want` (≤ `for_`), returning whether seen.
async fn wait_lifecycle(handle: &SessionHandle, want: SessionLifecycle, for_: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + for_;
    while tokio::time::Instant::now() < deadline {
        if handle.lifecycle() == want {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    handle.lifecycle() == want
}

async fn status(t: &mut LocalTransport, id: u64) -> serde_json::Value {
    t.send(SessionCommand::Query {
        id,
        query: SessionQuery::Status,
    })
    .await
    .unwrap();
    let seen = until(t, |e| matches!(e, SessionEventWire::QueryResult { id: i, .. } if *i == id)).await;
    match &seen.last().unwrap().event {
        SessionEventWire::QueryResult { value, .. } => value.clone(),
        _ => unreachable!(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn parks_after_grace_when_detached_and_idle_and_attach_unparks() {
    let _h = Home::new();
    let _g = Grace::set("0");
    let (url, _) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(pcfg()).await.unwrap();

    let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    // Non-persisted knob + an aborted turn so unpark has state to restore.
    a.send(SessionCommand::Set {
        id: 1,
        setting: SessionSetting::ContextWindow {
            tokens: Some(1_000_000),
        },
    })
    .await
    .unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::SettingChanged(_))).await;
    a.send(submit("hello")).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    let before = last_conversation(&seen);
    assert_eq!(before.api_messages.len(), 2);
    let view_before = (*a.view()).clone();
    assert_eq!(view_before.context_window, 1_000_000);

    // A second subscriber watches lifecycle events land in order.
    let mut watch = handle.subscribe();
    detach(&mut a).await;
    assert!(
        wait_lifecycle(&handle, SessionLifecycle::Parked, Duration::from_secs(5)).await,
        "grace 0 → parked"
    );
    let mut seq = Vec::new();
    while let Ok(env) = watch.try_recv() {
        if let SessionEventWire::Lifecycle(l) = env.event {
            seq.push(l);
        }
    }
    assert_eq!(seq, vec![SessionLifecycle::Parking, SessionLifecycle::Parked]);
    assert!(handle.is_alive(), "actor task lives on while parked");
    let reg = agent_engine::events::registry::find_session_registration(handle.id.as_str());
    assert!(reg.is_some(), "per-session UDS still registered while parked");

    // Attach → unpark: conversation, abort_context-free history, settings.
    let (mut b, snap) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Attach))
        .await
        .unwrap();
    assert_eq!(handle.lifecycle(), SessionLifecycle::Live);
    assert_eq!(snap.conversation.api_messages, before.api_messages);
    assert_eq!(snap.conversation.abort_context, before.abort_context);
    assert_eq!(snap.view.context_window, 1_000_000, "settings replayed");
    assert_eq!(snap.view.model, MODEL);
    let st = status(&mut b, 7).await;
    assert_eq!(st["lifecycle"], "live");
    assert_eq!(st["messages"], 2);
    // Turns work after unpark.
    b.send(submit("again")).await.unwrap();
    let seen = until(&mut b, |e| matches!(e, SessionEventWire::Idle)).await;
    assert_eq!(last_conversation(&seen).api_messages.len(), 4);
    end(&mut b).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn streaming_and_pending_prompt_block_park_until_done() {
    let _h = Home::new();
    let _g = Grace::set("0");
    let (url, _) = stub_seq(&[SSE_PROMPT_TOOL_USE, SSE_HI]).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = prompt_host().await;
    let handle = host.create_session(pcfg()).await.unwrap();
    let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    a.send(submit("go")).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Prompt(_))).await;
    let pid = prompt_id(seen.last().unwrap()).unwrap();
    detach(&mut a).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        handle.lifecycle(),
        SessionLifecycle::Live,
        "pending prompt (and the running turn) block parking"
    );

    let (mut b, snap) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Attach))
        .await
        .unwrap();
    assert!(snap.streaming);
    assert_eq!(snap.pending_prompts.len(), 1);
    b.send(SessionCommand::Answer {
        prompt_id: pid,
        value: Some("abc".into()),
    })
    .await
    .unwrap();
    until(&mut b, |e| matches!(e, SessionEventWire::Idle)).await;
    detach(&mut b).await;
    assert!(
        wait_lifecycle(&handle, SessionLifecycle::Parked, Duration::from_secs(5)).await,
        "answer → Done → detached+idle → parks"
    );
    handle
        .send(SessionCommand::End {
            reason: EndReason::HostShutdown,
        })
        .await
        .unwrap();
    handle.closed().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn keep_warm_never_parks_and_never_grace_disables() {
    let _h = Home::new();
    let (url, _) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    {
        let _g = Grace::set("0");
        let handle = host
            .create_session(SessionConfig {
                keep_warm: true,
                ..pcfg()
            })
            .await
            .unwrap();
        let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
            .await
            .unwrap();
        a.send(submit("hi")).await.unwrap();
        until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
        detach(&mut a).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(handle.lifecycle(), SessionLifecycle::Live, "--keep-warm pins");

        // /keep-warm off → parks; KeepWarm on while parked wakes + pins.
        let (mut b, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
            .await
            .unwrap();
        b.send(SessionCommand::KeepWarm { on: false }).await.unwrap();
        until(&mut b, |e| matches!(e, SessionEventWire::SystemNotice(n) if n.contains("keep-warm off"))).await;
        detach(&mut b).await;
        assert!(wait_lifecycle(&handle, SessionLifecycle::Parked, Duration::from_secs(5)).await);
        handle.send(SessionCommand::KeepWarm { on: true }).await.unwrap();
        assert!(wait_lifecycle(&handle, SessionLifecycle::Live, Duration::from_secs(5)).await);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(handle.lifecycle(), SessionLifecycle::Live);
        handle
            .send(SessionCommand::End {
                reason: EndReason::HostShutdown,
            })
            .await
            .unwrap();
        handle.closed().await;
    }
    {
        let _g = Grace::set("never");
        let handle = host.create_session(pcfg()).await.unwrap();
        let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
            .await
            .unwrap();
        a.send(submit("hi")).await.unwrap();
        until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
        detach(&mut a).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(handle.lifecycle(), SessionLifecycle::Live, "never = Parked disabled");
        handle
            .send(SessionCommand::End {
                reason: EndReason::HostShutdown,
            })
            .await
            .unwrap();
        handle.closed().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn event_injection_wakes_parked_session_and_runs_turn() {
    use tokio::io::AsyncWriteExt;
    let _h = Home::new();
    let _g = Grace::set("0");
    let (url, hits) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(pcfg()).await.unwrap();
    let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    a.send(submit("hi")).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    detach(&mut a).await;
    assert!(wait_lifecycle(&handle, SessionLifecycle::Parked, Duration::from_secs(5)).await);
    let hits_before = hits.load(std::sync::atomic::Ordering::SeqCst);

    // `synaps send --session X`: the per-session UDS is still bound.
    let mut watch = handle.subscribe();
    let reg = agent_engine::events::registry::find_session_registration(handle.id.as_str())
        .expect("registered while parked");
    let event = agent_engine::events::types::Event::simple(
        "test",
        "wake up",
        Some(agent_engine::events::types::Severity::High),
    );
    let mut c = tokio::net::UnixStream::connect(&reg.socket_path).await.unwrap();
    c.write_all(&serde_json::to_vec(&event).unwrap()).await.unwrap();
    c.shutdown().await.unwrap();

    // Live again, an EventAuto turn ran, and it parked again afterwards.
    let mut saw_live = false;
    let mut saw_turn = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(1), watch.recv()).await {
            Ok(Ok(env)) => match env.event {
                SessionEventWire::Lifecycle(SessionLifecycle::Live) => saw_live = true,
                SessionEventWire::TurnStarted {
                    trigger: TurnTrigger::EventAuto,
                    ..
                } => saw_turn = true,
                SessionEventWire::Lifecycle(SessionLifecycle::Parked) if saw_turn => break,
                _ => {}
            },
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            _ => break,
        }
    }
    assert!(saw_live, "unparked on event wake");
    assert!(saw_turn, "EventAuto turn after wake");
    assert!(hits.load(std::sync::atomic::Ordering::SeqCst) > hits_before);
    assert!(wait_lifecycle(&handle, SessionLifecycle::Parked, Duration::from_secs(5)).await);
    let saved = agent_engine::core::session::Session::load(handle.id.as_str()).unwrap();
    assert_eq!(saved.api_messages.len(), 4, "hi/reply + event/reply on disk");
    handle
        .send(SessionCommand::End {
            reason: EndReason::HostShutdown,
        })
        .await
        .unwrap();
    handle.closed().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn unpark_failure_yields_attach_refused_and_session_stays_parked() {
    let _h = Home::new();
    let _g = Grace::set("0");
    let (url, _) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(pcfg()).await.unwrap();
    let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    a.send(submit("hi")).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    detach(&mut a).await;
    assert!(wait_lifecycle(&handle, SessionLifecycle::Parked, Duration::from_secs(5)).await);

    // Journal gone → unpark cannot load it.
    assert!(agent_engine::core::session::Session::load(handle.id.as_str()).is_ok());
    agent_engine::core::session::delete_session_file(handle.id.as_str()).unwrap();
    assert!(agent_engine::core::session::Session::load(handle.id.as_str()).is_err());
    let err = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Attach))
        .await
        .err()
        .expect("attach refused");
    match err {
        TransportError::Refused(m) => assert!(m.contains("could not be restored"), "{m}"),
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(handle.lifecycle(), SessionLifecycle::Parked, "stays Parked, not corrupted");
    assert!(handle.is_alive());
    assert_eq!(host.sessions().len(), 1, "still listed");

    // End while parked: on_session_end + Ended, no save (file stays absent).
    let mut watch = handle.subscribe();
    handle
        .send(SessionCommand::End {
            reason: EndReason::HostShutdown,
        })
        .await
        .unwrap();
    let mut ended = false;
    while let Ok(env) = tokio::time::timeout(Duration::from_secs(10), watch.recv()).await {
        if let Ok(env) = env {
            if matches!(env.event, SessionEventWire::Ended { .. }) {
                ended = true;
                break;
            }
        } else {
            break;
        }
    }
    assert!(ended);
    handle.closed().await;
    assert!(
        agent_engine::core::session::Session::load(handle.id.as_str()).is_err(),
        "end while parked never saves"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn no_duplicate_index_start_record_on_unpark_and_status_never_wakes() {
    let _h = Home::new();
    let _g = Grace::set("0");
    let (url, _) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = Arc::clone(&host().await);
    let handle = host.create_session(pcfg()).await.unwrap();
    let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    a.send(submit("hi")).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    detach(&mut a).await;
    assert!(wait_lifecycle(&handle, SessionLifecycle::Parked, Duration::from_secs(5)).await);

    // The idle probe (Query{Status} from the host) answers without waking.
    assert!(agent_engine::daemon::session_is_idle(&handle).await);
    let mut rx = handle.subscribe();
    handle
        .send(SessionCommand::Query {
            id: 42,
            query: SessionQuery::Status,
        })
        .await
        .unwrap();
    let v = loop {
        let env = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        if let SessionEventWire::QueryResult { id: 42, value } = env.event {
            break value;
        }
    };
    assert_eq!(v["lifecycle"], "parked");
    assert_eq!(handle.lifecycle(), SessionLifecycle::Parked, "Status never wakes");

    let starts = |id: &str| {
        agent_engine::core::session_index::read_recent(10_000)
            .unwrap_or_default()
            .into_iter()
            .filter(|r| {
                r.session_id == id
                    && r.event == agent_engine::core::session_index::SessionIndexEventKind::Start
            })
            .count()
    };
    assert_eq!(starts(handle.id.as_str()), 1);
    let (mut b, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Attach))
        .await
        .unwrap();
    assert_eq!(handle.lifecycle(), SessionLifecycle::Live);
    assert_eq!(starts(handle.id.as_str()), 1, "unpark appends no START record");
    end(&mut b).await;
}
