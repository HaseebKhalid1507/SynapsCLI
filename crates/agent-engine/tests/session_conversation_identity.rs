//! Conversation identity (watch-sessions design §3.1, §4.4): an actor is
//! found by the conversation it holds (`SessionHandle::holds`), never by its
//! bare actor id. `/clear` lets the old conversation go and publishes the
//! new one; resuming the old one builds a second actor under a minted id;
//! map removal is by identity; a lock-held placeholder is retried in place.

mod session_actor_common;
use session_actor_common::*;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agent_engine::daemon::{Daemon, DaemonOpts, SessionFactory};
use agent_engine::session::socket_transport::SocketTransport;
use agent_engine::session::wire::{Attach, Hello};
use agent_engine::session::{
    AttachMode, ClientKind, ClientMeta, ClientTransport, CompactionPolicyWire, EndReason,
    LocalTransport, SessionCommand, SessionConfig, SessionEventWire, SessionHandle, SessionId,
    SessionLifecycle,
};
use agent_engine::EngineHost;
use axum::response::IntoResponse;
use serial_test::serial;

/// Streaming turns → SSE_HI; the non-streaming compaction call → a summary.
async fn stub_compact() -> String {
    let app = axum::Router::new().fallback(move |body: String| async move {
        if body.contains("\"stream\":true") {
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
            "content": [{"type": "text", "text": "SUMMARY: the user said hello."}],
            "usage": {"input_tokens": 50, "output_tokens": 10}
        }))
        .into_response()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

async fn boot() -> Arc<EngineHost> {
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", stub_compact().await);
    host().await
}

fn pcfg() -> SessionConfig {
    SessionConfig {
        persist: true,
        ..cfg()
    }
}

fn resume(q: &str) -> SessionConfig {
    SessionConfig {
        continue_session: Some(Some(q.into())),
        ..pcfg()
    }
}

async fn attach(handle: &SessionHandle) -> LocalTransport {
    LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap()
        .0
}

async fn turn(t: &mut LocalTransport, text: &str) {
    t.send(submit(text)).await.unwrap();
    until(t, |e| matches!(e, SessionEventWire::Idle)).await;
}

/// `/clear`; returns the new journal id.
async fn clear(t: &mut LocalTransport) -> String {
    t.send(SessionCommand::NewSession).await.unwrap();
    let seen = until(t, |e| matches!(e, SessionEventWire::Cleared { .. })).await;
    match &seen.last().unwrap().event {
        SessionEventWire::Cleared { session_id } => session_id.clone(),
        _ => unreachable!(),
    }
}

async fn wait_gone(host: &EngineHost, id: &SessionId) {
    let t0 = std::time::Instant::now();
    while host.attach(id).is_some() {
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "{id} never left the map"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A named session with one turn, `/clear`ed: actor `A` now on journal `N`.
async fn cleared(host: &Arc<EngineHost>) -> (SessionHandle, LocalTransport, String, String) {
    let a = host
        .create_session(SessionConfig {
            name: Some("axel".into()),
            ..pcfg()
        })
        .await
        .unwrap();
    let mut t = attach(&a).await;
    turn(&mut t, "first conversation").await;
    let journal_a = a.journal_id();
    assert_eq!(journal_a, a.id.as_str());
    let n = clear(&mut t).await;
    assert_ne!(n, journal_a);
    (a, t, journal_a, n)
}

/// §4.4 fix 3: after `/clear` the listing shows the new conversation — no
/// name, no title, zero messages and cost, its own `created_at` — at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn clear_lists_the_new_conversation_at_once() {
    let _h = Home::new();
    let host = boot().await;
    let a = host
        .create_session(SessionConfig {
            name: Some("axel".into()),
            ..pcfg()
        })
        .await
        .unwrap();
    let mut t = attach(&a).await;
    turn(&mut t, "first conversation").await;
    let before = host.sessions().into_iter().find(|m| m.id == a.id).unwrap();
    assert_eq!(before.name.as_deref(), Some("axel"));
    assert_eq!(before.messages, 2);
    assert!(before.title.is_some());
    tokio::time::sleep(Duration::from_millis(20)).await;

    let n = clear(&mut t).await;
    let after = host.sessions();
    assert_eq!(after.len(), 1);
    let m = &after[0];
    assert_eq!(
        (m.id.as_str(), m.journal_id.as_str()),
        (a.id.as_str(), n.as_str())
    );
    assert_eq!(m.name, None, "the fresh session has no name");
    assert_eq!((m.messages, m.cost_usd, m.title.as_deref()), (0, 0.0, None));
    assert!(
        m.created_at > before.created_at,
        "dated from the new conversation"
    );
    assert!(!a.holds(a.id.as_str()), "the old conversation is let go");
    end(&mut t).await;
}

/// §4.4 fixes 1, 2, 4: resuming the cleared conversation `A` (by id or by
/// its name) builds a second actor under a minted id — the cleared actor is
/// untouched — and ending either one leaves the other listed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn resuming_a_cleared_conversation_is_a_second_actor() {
    let _h = Home::new();
    let host = boot().await;
    let (a, mut ta, journal_a, n) = cleared(&host).await;

    let r = host.create_session(resume(&journal_a)).await.unwrap();
    assert_ne!(
        r.id, a.id,
        "a minted actor id: the map is keyed by actor id"
    );
    assert_eq!(r.journal_id(), journal_a);
    assert_eq!(a.journal_id(), n, "the cleared actor is untouched");
    // By id again, and by the old name (on disk, not on the cleared actor):
    // the actor that holds `A` now.
    for q in [journal_a.as_str(), "axel"] {
        let again = host.create_session(resume(q)).await.unwrap();
        assert_eq!(again.id, r.id, "continue {q} → the resumed actor");
    }
    let mut listed: Vec<(String, String)> = host
        .sessions()
        .into_iter()
        .map(|m| (m.id.0, m.journal_id))
        .collect();
    listed.sort();
    let mut want = vec![
        (a.id.0.clone(), n.clone()),
        (r.id.0.clone(), journal_a.clone()),
    ];
    want.sort();
    assert_eq!(listed, want);

    // Ending the resumed actor leaves the cleared one.
    let mut tr = attach(&r).await;
    end(&mut tr).await;
    wait_gone(&host, &r.id).await;
    assert_eq!(host.attach(&a.id).map(|h| h.journal_id()), Some(n.clone()));

    // And the other way round.
    let r2 = host.create_session(resume(&journal_a)).await.unwrap();
    assert_ne!(r2.id, a.id);
    end(&mut ta).await;
    wait_gone(&host, &a.id).await;
    let left: Vec<_> = host
        .sessions()
        .into_iter()
        .map(|m| (m.id, m.journal_id))
        .collect();
    assert_eq!(left, vec![(r2.id.clone(), journal_a)]);
    let mut t2 = attach(&r2).await;
    end(&mut t2).await;
}

/// After a linked-successor compaction `A → B` the actor still holds `A`:
/// resuming `A` attaches to it even when the disk chain does not lead to
/// `B` (the successor's link not written yet), not by its actor id.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn compaction_then_resume_by_the_old_id_attaches_to_the_same_actor() {
    let _h = Home::new();
    let host = boot().await;
    let a = host
        .create_session(SessionConfig {
            compaction_policy: CompactionPolicyWire::LinkedSuccessor,
            ..pcfg()
        })
        .await
        .unwrap();
    let mut t = attach(&a).await;
    turn(&mut t, "hello 0").await;
    turn(&mut t, "hello 1").await;
    t.send(SessionCommand::Compact { instructions: None })
        .await
        .unwrap();
    until(&mut t, |e| matches!(e, SessionEventWire::Idle)).await;
    let b = a.journal_id();
    assert_ne!(b, a.id.as_str(), "compacted into a successor");
    assert_eq!(a.journal().supersedes, vec![a.id.0.clone()]);

    // Cut the forward link on disk: only `supersedes` says `A` is here.
    let mut pred = agent_engine::core::session::Session::load(a.id.as_str()).unwrap();
    assert_eq!(pred.compacted_into.as_deref(), Some(b.as_str()));
    pred.compacted_into = None;
    pred.save().await.unwrap();

    for q in [a.id.as_str(), b.as_str()] {
        let again = host.create_session(resume(q)).await.unwrap();
        assert_eq!(again.id, a.id, "continue {q} → the live actor");
    }
    assert_eq!(host.sessions().len(), 1, "no second actor");
    end(&mut t).await;
}

/// §4.4 fix 4: an actor whose task ends after another session was adopted
/// under its id (a resume once `finish` freed the journal lock) removes
/// only itself, never the newcomer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn an_ending_actor_never_removes_a_successor_under_its_id() {
    let _h = Home::new();
    let host = boot().await;
    let id = SessionId::from("20261101-000000-r4ce");
    let (old, old_task) = SessionHandle::echo_for_test(id.clone());
    host.adopt_session(old.clone());
    let (new, _new_task) = SessionHandle::echo_for_test(id.clone());
    host.adopt_session(new.clone());

    old.send(SessionCommand::End {
        reason: EndReason::ClientQuit,
    })
    .await
    .unwrap();
    old_task.await.unwrap();
    assert!(
        host.remove_session_if(&id, &old.session_ref()).is_none(),
        "not the entry's session"
    );
    let listed = host.attach(&id).expect("the newcomer stays");
    assert!(listed.is_session(&new.session_ref()));
    assert!(host.remove_session_if(&id, &new.session_ref()).is_some());
    assert!(host.attach(&id).is_none());
}

fn placeholder(id: &SessionId, cfg: SessionConfig) -> SessionHandle {
    agent_engine::session::handle::locked_placeholder::spawn(id.clone(), cfg, "pid 1 (tui)".into())
        .0
}

async fn start_daemon(
    host: Arc<EngineHost>,
    home: &Home,
    factory: Option<SessionFactory>,
) -> Daemon {
    Daemon::start(
        host,
        DaemonOpts {
            runtime_dir: Some(home.dir.path().join("run")),
            factory,
            ..Default::default()
        },
    )
    .await
    .expect("daemon start")
}

async fn stop(d: Daemon) {
    SocketTransport::shutdown(&d.paths.sock, true)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(20), d.wait())
        .await
        .expect("daemon wait");
}

async fn attach_existing(
    sock: std::path::PathBuf,
    id: SessionId,
) -> Result<agent_engine::session::AttachSnapshot, agent_engine::session::TransportError> {
    let conn = SocketTransport::connect(&sock, Hello::new(ClientKind::Test)).await?;
    let (_t, snap) = SocketTransport::attach(
        conn,
        Attach::Existing {
            session_id: id,
            mode: AttachMode::Mirror,
        },
    )
    .await?;
    Ok(snap)
}

/// §4.4 fix 5: while a placeholder's real `create` runs (held here), the
/// session stays listed; the real session then replaces it in place.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_placeholder_retry_never_leaves_the_map_empty() {
    let home = Home::new();
    let host = boot().await;
    let entered = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(tokio::sync::Notify::new());
    let factory: SessionFactory = {
        let (entered, gate) = (Arc::clone(&entered), Arc::clone(&gate));
        Arc::new(move |cfg: SessionConfig| {
            let (entered, gate) = (Arc::clone(&entered), Arc::clone(&gate));
            Box::pin(async move {
                entered.fetch_add(1, Ordering::SeqCst);
                gate.notified().await;
                let id = cfg
                    .continue_session
                    .flatten()
                    .expect("a retry continues the journal");
                Ok(SessionHandle::echo_for_test(SessionId::from(id)).0)
            })
        })
    };
    let d = start_daemon(host, &home, Some(factory)).await;
    let sock = d.paths.sock.clone();
    let id = SessionId::from("20261101-000000-p1ac");
    d.state.insert(placeholder(&id, SessionConfig::default()));

    let attaching = tokio::spawn(attach_existing(sock.clone(), id.clone()));
    let t0 = std::time::Instant::now();
    while entered.load(Ordering::SeqCst) == 0 {
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "the retry never ran"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let during = SocketTransport::sessions(&sock).await.unwrap();
    let row = during
        .iter()
        .find(|m| m.id == id)
        .expect("listed while the retry runs");
    assert_eq!(row.lifecycle, SessionLifecycle::Parked);
    assert!(row.locked_by.is_some());

    gate.notify_one();
    let snap = tokio::time::timeout(Duration::from_secs(10), attaching)
        .await
        .unwrap()
        .unwrap()
        .expect("attached");
    assert_eq!(snap.meta.id, id);
    assert!(snap.meta.locked_by.is_none(), "the real session");
    let after = SocketTransport::sessions(&sock).await.unwrap();
    let rows: Vec<_> = after.iter().filter(|m| m.id == id).collect();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].locked_by.is_none(), "replaced in place");
    stop(d).await;
}

/// The same retry through the real host: the retrying placeholder is
/// neither a resume target nor an id holder, so the real actor takes its id
/// and replaces it (it used to be handed the placeholder back).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_released_journal_replaces_its_placeholder_through_the_host() {
    let home = Home::new();
    let host = boot().await;
    let first = host.create_session(pcfg()).await.unwrap();
    let id = first.id.clone();
    let mut t = attach(&first).await;
    turn(&mut t, "on disk").await;
    end(&mut t).await;
    wait_gone(&host, &id).await;

    let d = start_daemon(Arc::clone(&host), &home, None).await;
    let sock = d.paths.sock.clone();
    d.state.insert(placeholder(&id, pcfg()));
    let snap = tokio::time::timeout(
        Duration::from_secs(20),
        attach_existing(sock.clone(), id.clone()),
    )
    .await
    .expect("timely")
    .expect("attached to the real session");
    assert_eq!(snap.meta.id, id);
    assert!(snap.meta.locked_by.is_none());
    assert_eq!(
        snap.conversation.api_messages.len(),
        2,
        "the journal's history"
    );
    let rows: Vec<_> = host.sessions().into_iter().filter(|m| m.id == id).collect();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].locked_by.is_none());
    stop(d).await;
}
