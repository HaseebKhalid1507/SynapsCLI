//! B3 — `Parked` (PLAN-phase3 §2.5, §5.3): grace timer, guards, park/unpark,
//! event wake, `AttachRefused`, lifecycle events, no duplicate index record.

mod session_actor_common;
use session_actor_common::*;

use std::sync::Arc;
use std::time::Duration;

use agent_engine::session::{
    ClientKind, ClientMeta, ClientTransport, EndReason, LocalTransport, SessionCommand,
    SessionConfig, SessionEventWire, SessionHandle, SessionLifecycle, SessionQuery,
    SessionSetting, TurnTrigger,
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
        assert!(handle.pinned() && handle.meta_live().pinned, "the create-time pin is published");

        // /pin off → parks; KeepWarm on while parked wakes + pins.
        let (mut b, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
            .await
            .unwrap();
        b.send(SessionCommand::KeepWarm { on: false }).await.unwrap();
        until(&mut b, |e| matches!(e, SessionEventWire::SystemNotice(n) if n.starts_with("unpinned"))).await;
        assert!(!handle.pinned());
        detach(&mut b).await;
        assert!(wait_lifecycle(&handle, SessionLifecycle::Parked, Duration::from_secs(5)).await);
        handle.send(SessionCommand::KeepWarm { on: true }).await.unwrap();
        assert!(wait_lifecycle(&handle, SessionLifecycle::Live, Duration::from_secs(5)).await);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(handle.lifecycle(), SessionLifecycle::Live);
        assert!(handle.pinned());
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

/// Drop one valid event into the shared inbox the way `synaps send` does
/// (write aside, rename in) and return its path.
fn drop_inbox_event(home: &Home, text: &str) -> std::path::PathBuf {
    let inbox = home.dir.path().join(".synaps-cli").join("inbox");
    std::fs::create_dir_all(&inbox).unwrap();
    let event = agent_engine::events::types::Event::simple(
        "test",
        text,
        Some(agent_engine::events::types::Severity::High),
    );
    let path = inbox.join(format!("{}.json", event.id));
    let tmp = inbox.join(format!("{}.json.tmp", event.id));
    std::fs::write(&tmp, serde_json::to_vec(&event).unwrap()).unwrap();
    std::fs::rename(&tmp, &path).unwrap();
    path
}

/// What `watch` saw for `for_`: (unparked, EventAuto turns started).
async fn watch_for(
    watch: &mut tokio::sync::broadcast::Receiver<agent_engine::session::Envelope>,
    for_: Duration,
) -> (bool, usize) {
    let mut saw_live = false;
    let mut turns = 0;
    let deadline = tokio::time::Instant::now() + for_;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return (saw_live, turns);
        }
        match tokio::time::timeout(left, watch.recv()).await {
            Ok(Ok(env)) => match env.event {
                SessionEventWire::Lifecycle(SessionLifecycle::Live) => saw_live = true,
                SessionEventWire::TurnStarted {
                    trigger: TurnTrigger::EventAuto,
                    ..
                } => turns += 1,
                _ => {}
            },
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            _ => return (saw_live, turns),
        }
    }
}

/// The shared inbox is untargeted mail: it must never unpark a finished
/// session (the 3f61 wakes). The file stays for a session someone is at.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn an_inbox_file_does_not_unpark_a_parked_session_and_stays_on_disk() {
    let h = Home::new();
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

    let mut watch = handle.subscribe();
    let file = drop_inbox_event(&h, "not for you");
    // Past the watcher's 2 s rescan.
    let (saw_live, turns) = watch_for(&mut watch, Duration::from_secs(4)).await;
    assert!(!saw_live, "an inbox file unparked the session");
    assert_eq!(turns, 0, "an inbox file started a turn");
    assert_eq!(handle.lifecycle(), SessionLifecycle::Parked);
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), hits_before);
    assert!(file.exists(), "the file is left for an attached session");
    handle
        .send(SessionCommand::End {
            reason: EndReason::HostShutdown,
        })
        .await
        .unwrap();
    handle.closed().await;
}

/// Only an input owner opens the inbox: a detached Live session and an
/// `Observe` client (the keep-warm observer) leave the file; a `Mirror`
/// attach takes it once and runs exactly one EventAuto turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_detached_session_ignores_the_inbox_until_an_input_owner_attaches() {
    use agent_engine::session::AttachMode;
    let h = Home::new();
    let _g = Grace::set("never");
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
    assert_eq!(handle.lifecycle(), SessionLifecycle::Live);

    let mut watch = handle.subscribe();
    let file = drop_inbox_event(&h, "for whoever is at the keyboard");
    let (_, turns) = watch_for(&mut watch, Duration::from_secs(3)).await;
    assert_eq!(turns, 0, "a detached session took the inbox file");
    assert!(file.exists());

    let (_observer, _) = LocalTransport::attach_with(
        handle.clone(),
        ClientMeta::new(ClientKind::Attach),
        AttachMode::Observe,
    )
    .await
    .unwrap();
    let (_, turns) = watch_for(&mut watch, Duration::from_secs(3)).await;
    assert_eq!(turns, 0, "an Observe client opened the inbox");
    assert!(file.exists(), "an Observe client never owns input");

    let (mut b, _) = LocalTransport::attach_with(
        handle.clone(),
        ClientMeta::new(ClientKind::Tui),
        AttachMode::Mirror,
    )
    .await
    .unwrap();
    let seen = until(&mut b, |e| {
        matches!(
            e,
            SessionEventWire::TurnStarted {
                trigger: TurnTrigger::EventAuto,
                ..
            }
        )
    })
    .await;
    assert!(!seen.is_empty());
    until(&mut b, |e| matches!(e, SessionEventWire::Idle)).await;
    assert!(!file.exists(), "the owner's session took the file");
    // A second rescan finds nothing more: delivered exactly once.
    let (_, turns) = watch_for(&mut watch, Duration::from_millis(2_500)).await;
    assert_eq!(turns, 1, "exactly one EventAuto turn for one inbox file");
    let left: Vec<_> = std::fs::read_dir(file.parent().unwrap())
        .unwrap()
        .flatten()
        .collect();
    assert!(left.is_empty(), "inbox should be empty, found {left:?}");
    end(&mut b).await;
}

/// H2: a session whose journal vanished while Parked is NOT a zombie —
/// `unpark` rebuilds an empty conversation under the same id (current
/// model/thinking kept) and the attach succeeds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn unpark_missing_journal_restores_fresh_conversation() {
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

    let (mut b, snap) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Attach))
        .await
        .expect("attach restores a fresh conversation");
    assert_eq!(handle.lifecycle(), SessionLifecycle::Live);
    assert!(snap.conversation.api_messages.is_empty(), "fresh conversation");
    assert_eq!(snap.conversation.header.id, handle.id.as_str(), "same session id");
    assert_eq!(snap.view.model, MODEL);
    assert_eq!(host.sessions().len(), 1);
    // Turns work on the rebuilt conversation and the journal comes back.
    b.send(submit("again")).await.unwrap();
    let seen = until(&mut b, |e| matches!(e, SessionEventWire::Idle)).await;
    assert_eq!(last_conversation(&seen).api_messages.len(), 2);
    assert!(agent_engine::core::session::Session::load(handle.id.as_str()).is_ok());
    end(&mut b).await;
}

/// H2 → F18: a session that never ran a turn has nothing on disk (`save`
/// skips an empty conversation), so it must never park into something that
/// cannot be restored. It must not stay Live forever either (Runtime
/// resident, nobody attached): at the grace deadline it ENDS.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn empty_session_never_parks_it_ends_idle() {
    let _h = Home::new();
    let _g = Grace::set("0");
    std::env::set_var("SYNAPS_DAEMON_IDLE_END_GRACE_SECS", "0");
    let (url, _) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(pcfg()).await.unwrap();
    let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    detach(&mut a).await;
    assert!(
        !wait_lifecycle(&handle, SessionLifecycle::Parked, Duration::from_secs(2)).await,
        "never-saved session must not park"
    );
    assert_ne!(handle.lifecycle(), SessionLifecycle::Parked);
    tokio::time::timeout(Duration::from_secs(5), handle.closed())
        .await
        .expect("empty idle session must END at the grace deadline, not stay Live");
    assert!(agent_engine::core::session::Session::load(handle.id.as_str()).is_err());
    // RC soak F-NEW-1: no journal → no orphan `.lock` either.
    let lock = agent_engine::core::session_lock::sessions_dir().join(format!("{}.lock", handle.id.as_str()));
    assert!(!lock.exists(), "orphan lock file left behind: {}", lock.display());
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

/// B4: `--idle-exit` fires once every session is parked (REVIEW §5 retired):
/// a real `Daemon` over the host map, one session, detach → park → the idle
/// monitor requests shutdown without ever probing (waking) the actor.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn idle_exit_fires_once_all_sessions_parked() {
    let _h = Home::new();
    let _g = Grace::set("0");
    let (url, _) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let run = tempfile::TempDir::new().unwrap();
    let d = agent_engine::daemon::Daemon::start(
        Arc::clone(&host),
        agent_engine::daemon::DaemonOpts {
            runtime_dir: Some(run.path().to_path_buf()),
            idle_exit: Some(Duration::from_millis(300)),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let token = d.shutdown_token();

    // One map: a session created on the host is what the daemon lists.
    let handle = host.create_session(pcfg()).await.unwrap();
    assert_eq!(d.state.live_sessions().len(), 1);
    let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    a.send(submit("hi")).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    let metas = d.state.session_metas();
    assert_eq!(metas[0].clients, 1);
    assert_eq!(metas[0].input_owner, Some(a.client_id()));
    assert_eq!(metas[0].lifecycle, SessionLifecycle::Live);
    assert_eq!(metas[0].journal_id, handle.id.as_str());
    assert!(!token.is_cancelled(), "a client is attached: not idle");

    detach(&mut a).await;
    assert!(wait_lifecycle(&handle, SessionLifecycle::Parked, Duration::from_secs(5)).await);
    assert_eq!(d.state.session_metas()[0].lifecycle, SessionLifecycle::Parked);
    assert_eq!(d.state.session_metas()[0].clients, 0);
    tokio::time::timeout(Duration::from_secs(10), token.cancelled())
        .await
        .expect("idle-exit fires with a parked session");
    assert_eq!(handle.lifecycle(), SessionLifecycle::Parked, "the probe never woke it");
    d.wait().await;
    assert!(!handle.is_alive(), "shutdown_all ended the parked session");
    assert_eq!(host.sessions().len(), 0);
}

/// The old image's half of a reload, for one persisted session with
/// history: a turn, then the session ends (its journal stays on disk). The
/// new image's half is `reload::rehydrate` over the returned record.
async fn reloaded_live_session(host: &Arc<agent_engine::EngineHost>) -> agent_engine::daemon::reload::ReloadState {
    let handle = host.create_session(pcfg()).await.unwrap();
    let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    a.send(submit("hi")).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    let journal = handle.journal_id();
    end(&mut a).await;
    while handle.is_alive() {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    agent_engine::daemon::reload::ReloadState {
        generation: 2,
        written_at: chrono::Utc::now(),
        sessions: vec![agent_engine::daemon::reload::ReloadSession {
            id: journal.clone(),
            journal_id: journal.clone(),
            config: SessionConfig {
                continue_session: Some(Some(journal)),
                ..pcfg()
            },
            keep_warm: false,
            lifecycle: SessionLifecycle::Live,
            settings_replay: Vec::new(),
            cache_keepwarm: None,
            input_owner_kind: None,
            workers: Vec::new(),
            tool_activations: Vec::new(),
        }],
        expected_clients: 1,
        next_subagent_id: 0,
    }
}

async fn start_daemon(
    host: &Arc<agent_engine::EngineHost>,
    run: &tempfile::TempDir,
) -> agent_engine::daemon::Daemon {
    agent_engine::daemon::Daemon::start(
        Arc::clone(host),
        agent_engine::daemon::DaemonOpts {
            runtime_dir: Some(run.path().to_path_buf()),
            ..Default::default()
        },
    )
    .await
    .unwrap()
}

/// A reload rehydrates a session that no client reconnects to: it must
/// still park after the grace. The old image's park timer dies with the
/// exec, and only an attach or detach armed a new one, so a session whose
/// client left just before a reload (or never came back after it) stayed
/// Live with zero clients forever (2026-10-25: Live 9.5 h, through four
/// reloads, never parked).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_session_rehydrated_with_no_client_parks_after_the_grace() {
    let _h = Home::new();
    let _g = Grace::set("0");
    let (url, _) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let run = tempfile::TempDir::new().unwrap();
    let d = start_daemon(&host, &run).await;
    let rs = reloaded_live_session(&host).await;

    agent_engine::daemon::reload::rehydrate(&d.state, &rs).await;
    let reborn = d.state.live_sessions().pop().expect("rehydrated");
    assert!(reborn.meta().continued, "the same conversation, from its journal");
    assert_eq!(d.state.session_metas()[0].clients, 0, "nobody reconnected");
    assert!(
        wait_lifecycle(&reborn, SessionLifecycle::Parked, Duration::from_secs(5)).await,
        "a rehydrated session with no client must park after the grace, not stay Live"
    );
    reborn
        .send(SessionCommand::End { reason: EndReason::HostShutdown })
        .await
        .unwrap();
}

/// The grace the rehydrate arms is the reconnect grace, not a park: a client
/// that reconnects within it keeps the session Live, and its later detach
/// parks it as usual.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_client_reconnecting_after_a_reload_keeps_the_session_live() {
    let _h = Home::new();
    let _g = Grace::set("1");
    let (url, _) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let run = tempfile::TempDir::new().unwrap();
    let d = start_daemon(&host, &run).await;
    let rs = reloaded_live_session(&host).await;

    agent_engine::daemon::reload::rehydrate(&d.state, &rs).await;
    let reborn = d.state.live_sessions().pop().expect("rehydrated");
    let (mut a, _) = LocalTransport::attach(reborn.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    assert!(
        !wait_lifecycle(&reborn, SessionLifecycle::Parked, Duration::from_millis(2500)).await,
        "an attached client cancels the rehydrate's grace"
    );
    assert_eq!(reborn.lifecycle(), SessionLifecycle::Live);
    detach(&mut a).await;
    assert!(
        wait_lifecycle(&reborn, SessionLifecycle::Parked, Duration::from_secs(5)).await,
        "the detach arms the grace as usual"
    );
    reborn
        .send(SessionCommand::End { reason: EndReason::HostShutdown })
        .await
        .unwrap();
}

/// Parked sessions leave the daemon's map after `SYNAPS_DAEMON_PARKED_EVICT_SECS`
/// (Ended(Evicted)); the journal stays and `--continue <id>` rebuilds it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn parked_session_is_evicted_after_age_and_continue_rebuilds_it() {
    let _h = Home::new();
    let _g = Grace::set("0");
    std::env::set_var("SYNAPS_DAEMON_PARKED_EVICT_SECS", "1");
    let (url, _) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(pcfg()).await.unwrap();
    let id = handle.id.clone();
    let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    a.send(submit("hi")).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    detach(&mut a).await;
    assert!(wait_lifecycle(&handle, SessionLifecycle::Parked, Duration::from_secs(5)).await);

    // Evicted after the age: handle closes, journal remains.
    tokio::time::timeout(Duration::from_secs(5), handle.closed())
        .await
        .expect("parked session must be evicted after SYNAPS_DAEMON_PARKED_EVICT_SECS");
    assert!(agent_engine::core::session::Session::load(id.as_str()).is_ok(), "journal kept");
    std::env::remove_var("SYNAPS_DAEMON_PARKED_EVICT_SECS");

    // --continue rebuilds it from disk with history intact.
    let handle2 = host
        .create_session(SessionConfig {
            continue_session: Some(Some(id.as_str().to_string())),
            ..pcfg()
        })
        .await
        .unwrap();
    let (mut b, snap) = LocalTransport::attach(handle2.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    assert!(snap.conversation.api_messages.len() >= 2, "history rebuilt: {}", snap.conversation.api_messages.len());
    handle2
        .send(SessionCommand::End { reason: EndReason::HostShutdown })
        .await
        .unwrap();
    let _ = b.next_event().await;
}

/// Parking drops the runtime and with it the worker registry: a background
/// worker still running would be killed (its handle's shutdown channel
/// drops) and reported as a truncated completion nobody can collect. A
/// detached session therefore does not park while a worker runs, nor while
/// a finished worker's result waits to be collected; it parks once the
/// parent has it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_detached_session_does_not_park_under_its_workers() {
    use agent_engine::tools::{SubagentHandle, SubagentState, SubagentStatus};
    let _h = Home::new();
    let _g = Grace::set("0");
    let (url, _) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let (handle, task) = host.create_session_unspawned(pcfg()).await.unwrap();
    let registry = task.subagent_registry();
    tokio::spawn(task.run());
    let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();

    // A background worker, running when the turn ends.
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();
    let state = Arc::new(std::sync::RwLock::new(SubagentState::new()));
    registry.lock().unwrap().register(SubagentHandle::new(
        "sa_1".into(),
        1,
        "reviewer".into(),
        "review it all".into(),
        MODEL.into(),
        String::new(),
        0,
        Arc::clone(&state),
        None,
        Some(shutdown_tx),
        None,
    ));
    a.send(submit("hello")).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;

    detach(&mut a).await;
    assert!(
        !wait_lifecycle(&handle, SessionLifecycle::Parked, Duration::from_millis(2500)).await,
        "parked over a running worker"
    );
    assert!(!state.read().unwrap().cancel_requested, "the worker was not stopped");
    assert!(
        matches!(shutdown_rx.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Empty)),
        "its shutdown channel is intact"
    );

    // It finishes; the parent has not collected it yet: still live.
    {
        let mut s = state.write().unwrap();
        s.status = SubagentStatus::Completed;
        s.finished_at = Some(std::time::Instant::now());
        s.finalization_started = true;
        s.finalization_ready = true;
        s.completion_settled = true;
    }
    assert!(
        !wait_lifecycle(&handle, SessionLifecycle::Parked, Duration::from_millis(2500)).await,
        "parked over an uncollected result"
    );

    // Collected: nothing left to lose, the session parks.
    registry.lock().unwrap().get_mut("sa_1").unwrap().mark_collected();
    let (mut b, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    detach(&mut b).await;
    assert!(
        wait_lifecycle(&handle, SessionLifecycle::Parked, Duration::from_secs(5)).await,
        "parks once the worker is collected"
    );
    handle
        .send(SessionCommand::End { reason: EndReason::HostShutdown })
        .await
        .unwrap();
}

/// A fake running worker `sa_N` over `state` (no thread).
fn fake_worker(
    registry: &std::sync::Mutex<agent_engine::runtime::subagent::SubagentRegistry>,
    id: &str,
    state: &Arc<std::sync::RwLock<agent_engine::tools::SubagentState>>,
) {
    registry.lock().unwrap().register(agent_engine::tools::SubagentHandle::new(
        id.into(),
        id.trim_start_matches("sa_").parse().unwrap(),
        "reviewer".into(),
        "review".into(),
        MODEL.into(),
        String::new(),
        0,
        Arc::clone(state),
        None,
        None,
        None,
    ));
}

fn finish(state: &Arc<std::sync::RwLock<agent_engine::tools::SubagentState>>) {
    let mut s = state.write().unwrap();
    s.status = agent_engine::tools::SubagentStatus::Completed;
    s.finished_at = Some(std::time::Instant::now());
    s.finalization_started = true;
    s.finalization_ready = true;
    s.completion_settled = true;
}

/// The last running worker finishing (and collected) re-arms the park from
/// the subagent tick: the session parks within a tick, not at the next
/// worker re-check (set far out here to prove it is not what parks it).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn the_last_worker_finishing_re_arms_the_park() {
    use agent_engine::session::actor::testing::WorkerParkRecheck;
    let _h = Home::new();
    let _g = Grace::set("0");
    let _r = WorkerParkRecheck::set(Duration::from_secs(600));
    let (url, _) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let (handle, task) = host.create_session_unspawned(pcfg()).await.unwrap();
    let registry = task.subagent_registry();
    tokio::spawn(task.run());
    let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    // Running through the turn: the 1 Hz subagent tick outlives it.
    let state = Arc::new(std::sync::RwLock::new(agent_engine::tools::SubagentState::new()));
    fake_worker(&registry, "sa_1", &state);
    a.send(submit("hello")).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    detach(&mut a).await;
    assert!(!wait_lifecycle(&handle, SessionLifecycle::Parked, Duration::from_millis(1500)).await);

    registry.lock().unwrap().get_mut("sa_1").unwrap().mark_collected();
    finish(&state);
    assert!(
        wait_lifecycle(&handle, SessionLifecycle::Parked, Duration::from_secs(4)).await,
        "the tick re-armed the park when the last worker finished"
    );
    handle.send(SessionCommand::End { reason: EndReason::HostShutdown }).await.unwrap();
}

/// A park held by a finished, uncollected worker is re-checked on its own:
/// once the result is collected (in a turn, or here directly — nothing
/// else re-arms the park), the next re-check parks the session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_park_held_by_workers_is_rechecked() {
    use agent_engine::session::actor::testing::WorkerParkRecheck;
    let _h = Home::new();
    let _g = Grace::set("0");
    let _r = WorkerParkRecheck::set(Duration::from_millis(300));
    let (url, _) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let (handle, task) = host.create_session_unspawned(pcfg()).await.unwrap();
    let registry = task.subagent_registry();
    tokio::spawn(task.run());
    let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    a.send(submit("hello")).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    // Finished on its own after the turn, result not collected yet.
    let state = Arc::new(std::sync::RwLock::new(agent_engine::tools::SubagentState::new()));
    fake_worker(&registry, "sa_2", &state);
    finish(&state);
    detach(&mut a).await;
    assert!(
        !wait_lifecycle(&handle, SessionLifecycle::Parked, Duration::from_millis(1500)).await,
        "held while the result waits"
    );

    registry.lock().unwrap().get_mut("sa_2").unwrap().mark_collected();
    assert!(
        wait_lifecycle(&handle, SessionLifecycle::Parked, Duration::from_secs(3)).await,
        "the re-check parked it"
    );
    handle.send(SessionCommand::End { reason: EndReason::HostShutdown }).await.unwrap();
}

/// A worker re-check that finds the session parkable does not park it on
/// the spot: the reconnect grace still runs first, from the re-check. (The
/// re-check used to park at once, seconds after a detach or a collecting
/// wake turn, well inside the grace.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_worker_recheck_still_honours_the_reconnect_grace() {
    use agent_engine::session::actor::testing::WorkerParkRecheck;
    let _h = Home::new();
    let _g = Grace::set("2");
    let _r = WorkerParkRecheck::set(Duration::from_millis(300));
    let (url, _) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let (handle, task) = host.create_session_unspawned(pcfg()).await.unwrap();
    let registry = task.subagent_registry();
    tokio::spawn(task.run());
    let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    a.send(submit("hello")).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    let state = Arc::new(std::sync::RwLock::new(agent_engine::tools::SubagentState::new()));
    fake_worker(&registry, "sa_3", &state);
    finish(&state);
    detach(&mut a).await;
    // Collected right away: the next re-check (300 ms) finds it parkable.
    registry.lock().unwrap().get_mut("sa_3").unwrap().mark_collected();
    assert!(
        !wait_lifecycle(&handle, SessionLifecycle::Parked, Duration::from_millis(1500)).await,
        "parked inside the 2 s reconnect grace"
    );
    assert!(
        wait_lifecycle(&handle, SessionLifecycle::Parked, Duration::from_secs(4)).await,
        "parks once the grace has run"
    );
    handle.send(SessionCommand::End { reason: EndReason::HostShutdown }).await.unwrap();
}

// ── A park and a pending `on_session_start` ─────────────────────────────────
//
// A park that happens takes a hook not called yet out of line (nothing runs
// for a parked session); the unpark puts it back. A park that does not
// happen (#255 holds it while a worker's result waits, and re-checks it)
// takes nothing out: the hook keeps its place in line and runs while the
// session is live. Two test seams make the hook pending after a turn: the
// host's session-start gate is held, and that turn puts a fresh hook back in
// line at its end. A third, `in_line`, shows the gate's line itself: the
// tests wait for a hook to be registered there instead of assuming the
// turn's `Idle` means it is (the hook joins the line from its own task).
//
// What the timed waits show: "not parked within 1500 ms" and "no hook within
// 800 ms" are negative observations over that window, not barriers; the
// counts read later check that the work did happen, and a later window that
// it did not happen again. `call_order` records when each call started.

use agent_engine::session::session_start_testing as start_seam;

/// One turn on `t`, ending with `sid`'s hook back in line (behind a held gate).
async fn turn_leaving_the_hook_in_line(t: &mut LocalTransport, sid: &str) {
    start_seam::requeue_after_next_turn(sid);
    t.send(submit("hello")).await.unwrap();
    until(t, |e| matches!(e, SessionEventWire::Idle)).await;
}

/// Wait (bounded) until the hooks in line at `host`'s gate are exactly
/// `want`, in line order (journal ids).
async fn line_is(host: &agent_engine::EngineHost, want: &[&str]) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if start_seam::in_line(host) == want {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Wait (bounded) until `n` hooks of `sid` have run since `base`.
async fn hooks_run(sid: &str, base: u64, n: u64) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        if start_seam::calls(sid) >= base + n {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

/// Session A's hook is in line before session B's. A's park is held by its
/// worker's uncollected result and re-checked every 300 ms; none of those
/// parks happens, so A's hook keeps its place: released, it runs first, with
/// A live, then B's. Once collected, A parks; its hook has run, so its
/// unpark runs none.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_park_held_by_workers_leaves_the_hook_in_line() {
    use agent_engine::session::actor::testing::WorkerParkRecheck;
    let _h = Home::new();
    let _g = Grace::set("0");
    let _r = WorkerParkRecheck::set(Duration::from_millis(300));
    let (url, _) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let (a_handle, task) = host.create_session_unspawned(pcfg()).await.unwrap();
    let registry = task.subagent_registry();
    tokio::spawn(task.run());
    let b_handle = host.create_session(pcfg()).await.unwrap();
    let (mut a, _) = LocalTransport::attach(a_handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    let (mut b, _) = LocalTransport::attach(b_handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    let (sa, sb) = (a_handle.journal_id(), b_handle.journal_id());
    let hold = start_seam::hold_gate(&host).await;
    let (ca, cb) = (start_seam::calls(&sa), start_seam::calls(&sb));
    turn_leaving_the_hook_in_line(&mut a, &sa).await;
    assert!(line_is(&host, &[sa.as_str()]).await, "A's hook is in line: {:?}", start_seam::in_line(&host));
    turn_leaving_the_hook_in_line(&mut b, &sb).await;
    assert!(
        line_is(&host, &[sa.as_str(), sb.as_str()]).await,
        "B's hook is in line behind A's: {:?}",
        start_seam::in_line(&host)
    );

    // A's worker finished; its result is not collected: the park is held.
    let state = Arc::new(std::sync::RwLock::new(agent_engine::tools::SubagentState::new()));
    fake_worker(&registry, "sa_2", &state);
    finish(&state);
    detach(&mut a).await;
    assert!(
        !wait_lifecycle(&a_handle, SessionLifecycle::Parked, Duration::from_millis(1500)).await,
        "held while the result waits"
    );
    assert_eq!(start_seam::calls(&sa), ca, "A's hook still waits");
    assert_eq!(start_seam::in_line(&host), [sa.as_str(), sb.as_str()], "the held park left A's hook in line");

    let before = start_seam::call_order().len();
    drop(hold);
    assert!(hooks_run(&sb, cb, 1).await, "B's hook runs");
    assert!(hooks_run(&sa, ca, 1).await, "A's hook runs");
    let order: Vec<String> = start_seam::call_order()[before..]
        .iter()
        .filter(|s| **s == sa || **s == sb)
        .cloned()
        .collect();
    assert_eq!(order, [sa.clone(), sb.clone()], "A's hook kept its place in line");
    assert_eq!(a_handle.lifecycle(), SessionLifecycle::Live, "A's hook ran while A was live");

    // Collected: A parks. Its hook has run: the unpark runs none.
    registry.lock().unwrap().get_mut("sa_2").unwrap().mark_collected();
    assert!(
        wait_lifecycle(&a_handle, SessionLifecycle::Parked, Duration::from_secs(3)).await,
        "the re-check parked it"
    );
    let (_a2, _) = LocalTransport::attach(a_handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    assert!(wait_lifecycle(&a_handle, SessionLifecycle::Live, Duration::from_secs(5)).await);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(start_seam::calls(&sa), ca + 1, "once per session");
    assert_eq!(start_seam::calls(&sb), cb + 1, "once per session");
    for h in [&a_handle, &b_handle] {
        h.send(SessionCommand::End { reason: EndReason::HostShutdown }).await.unwrap();
    }
}

/// A's park is held by its worker, then the result is collected and the
/// re-check parks it: that park takes the hook out of line, so nothing runs
/// for the parked session when the gate frees. An event (as `synaps send`)
/// unparks it: the hook runs once, and the event's turn sends its request
/// after it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_park_that_happens_takes_the_hook_out_of_line_and_the_unpark_runs_it_once() {
    use agent_engine::session::actor::testing::WorkerParkRecheck;
    let _h = Home::new();
    let _g = Grace::set("0");
    let _r = WorkerParkRecheck::set(Duration::from_millis(300));
    let (url, hits) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let (handle, task) = host.create_session_unspawned(pcfg()).await.unwrap();
    let registry = task.subagent_registry();
    let queue = task.event_queue();
    tokio::spawn(task.run());
    let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    let sid = handle.journal_id();
    let hold = start_seam::hold_gate(&host).await;
    let calls = start_seam::calls(&sid);
    turn_leaving_the_hook_in_line(&mut a, &sid).await;
    assert!(line_is(&host, &[sid.as_str()]).await, "the hook is in line: {:?}", start_seam::in_line(&host));

    let state = Arc::new(std::sync::RwLock::new(agent_engine::tools::SubagentState::new()));
    fake_worker(&registry, "sa_2", &state);
    finish(&state);
    detach(&mut a).await;
    assert!(
        !wait_lifecycle(&handle, SessionLifecycle::Parked, Duration::from_millis(1500)).await,
        "held while the result waits"
    );
    assert_eq!(start_seam::in_line(&host), [sid.as_str()], "the held park left the hook in line");
    registry.lock().unwrap().get_mut("sa_2").unwrap().mark_collected();
    assert!(
        wait_lifecycle(&handle, SessionLifecycle::Parked, Duration::from_secs(3)).await,
        "the re-check parked it"
    );
    assert!(line_is(&host, &[]).await, "the park took the hook out of line: {:?}", start_seam::in_line(&host));

    drop(hold);
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(start_seam::calls(&sid), calls, "no hook for a parked session");
    assert_eq!(handle.lifecycle(), SessionLifecycle::Parked);

    // The whole round (unpark, hook, turn, park again: the grace is 0) can
    // fit between two lifecycle polls: watch the events instead.
    let hits_before = hits.load(std::sync::atomic::Ordering::SeqCst);
    let mut watch = handle.subscribe();
    queue
        .push(agent_engine::events::types::Event::simple("test", "wake up", None))
        .unwrap();
    let (mut saw_live, mut saw_turn) = (false, false);
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
            Ok(Err(_)) => break,
            Err(_) => continue,
        }
    }
    assert!(saw_live, "the event unparks it");
    assert!(saw_turn, "the event's turn starts");
    assert!(hooks_run(&sid, calls, 1).await, "the unpark runs the hook");
    assert!(
        hits.load(std::sync::atomic::Ordering::SeqCst) > hits_before,
        "the event's turn sent its request"
    );
    // Its turn ended and it parked again: no second hook.
    assert!(wait_lifecycle(&handle, SessionLifecycle::Parked, Duration::from_secs(5)).await);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(start_seam::calls(&sid), calls + 1, "the hook ran once");
    handle.send(SessionCommand::End { reason: EndReason::HostShutdown }).await.unwrap();
}
