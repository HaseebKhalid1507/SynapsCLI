//! Contract v2 §1 (PR 0a): a reply reaches only the client that asked, in
//! process (`LocalTransport`). Every client numbers its own requests, so
//! two clients' request 5 share an id; with every reply broadcast, each took
//! the other's as its own. A shared reply (`SettingChanged`, `Resumed`)
//! still reaches everyone, view-only for those who did not ask; a host's
//! reply reaches no attachment, and no attachment can answer a host waiter.

mod session_actor_common;
use session_actor_common::*;

use std::time::Duration;

use agent_engine::session::wire::{
    IDLE_PROBE_QUERY_ID, RENAME_QUERY_ID_BASE, VIEW_CHANGED_SETTING_ID,
};
use agent_engine::session::{
    AttachMode, CheckpointReason, ClientKind, ClientMeta, ClientTransport, Envelope,
    LocalTransport, SessionCommand, SessionConfig, SessionEventWire, SessionHandle, SessionQuery,
    SessionSetting,
};
use serial_test::serial;

const QUIET: Duration = Duration::from_millis(400);

async fn attach(handle: &SessionHandle, kind: ClientKind) -> LocalTransport {
    LocalTransport::attach_with(handle.clone(), ClientMeta::new(kind), AttachMode::Mirror)
        .await
        .unwrap()
        .0
}

fn query_results(seen: &[Envelope], want: u64) -> Vec<serde_json::Value> {
    seen.iter()
        .filter_map(|e| match &e.event {
            SessionEventWire::QueryResult { id, value } if *id == want => Some(value.clone()),
            _ => None,
        })
        .collect()
}

/// `(id, setting)` of every `SettingChanged`.
fn settings(seen: &[Envelope]) -> Vec<(u64, String)> {
    seen.iter()
        .filter_map(|e| match &e.event {
            SessionEventWire::SettingChanged(a) => Some((a.id, a.setting.clone())),
            _ => None,
        })
        .collect()
}

/// Everything up to the reply `QueryResult{id}`, then whatever else
/// arrives within [`QUIET`] (a second reply to the same id would).
async fn reply_and_rest(t: &mut LocalTransport, id: u64) -> Vec<Envelope> {
    let mut seen = until(t, |e| matches!(e, SessionEventWire::QueryResult { id: rid, .. } if *rid == id)).await;
    seen.extend(drain_for(t, QUIET).await);
    seen
}

async fn boot() -> std::sync::Arc<agent_engine::EngineHost> {
    let (url, _) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    host().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn colliding_query_ids_reach_only_their_asker() {
    let _h = Home::new();
    let host = boot().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let mut a = attach(&handle, ClientKind::Tui).await;
    let mut b = attach(&handle, ClientKind::Attach).await;

    a.send(SessionCommand::Query { id: 5, query: SessionQuery::Status }).await.unwrap();
    b.send(SessionCommand::Query { id: 5, query: SessionQuery::View }).await.unwrap();
    let ra = query_results(&reply_and_rest(&mut a, 5).await, 5);
    let rb = query_results(&reply_and_rest(&mut b, 5).await, 5);
    assert_eq!(ra.len(), 1, "a gets its own reply alone: {ra:#?}");
    assert!(ra[0].get("streaming").is_some(), "a's Status: {}", ra[0]);
    assert_eq!(rb.len(), 1, "b gets its own reply alone: {rb:#?}");
    assert!(rb[0].get("thinking_level").is_some() && rb[0].get("streaming").is_none(), "b's View: {}", rb[0]);
    end(&mut a).await;
}

/// `Set` answers its asker with its id; everyone else mirrors the view from
/// the same `SettingChanged`, its id projected to `VIEW_CHANGED_SETTING_ID`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn colliding_set_ids_answer_only_their_asker() {
    let _h = Home::new();
    let host = boot().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let mut a = attach(&handle, ClientKind::Tui).await;
    let mut b = attach(&handle, ClientKind::Attach).await;

    a.send(SessionCommand::Set { id: 7, setting: SessionSetting::ApiRetries { n: 2 } }).await.unwrap();
    b.send(SessionCommand::Set { id: 7, setting: SessionSetting::MaxToolOutput { bytes: 9000 } })
        .await
        .unwrap();
    // Each client sees both: its own reply and the other's view change.
    let mut sa = Vec::new();
    let mut sb = Vec::new();
    while sa.len() < 2 {
        sa.extend(settings(&[next(&mut a).await]));
    }
    while sb.len() < 2 {
        sb.extend(settings(&[next(&mut b).await]));
    }
    sa.extend(settings(&drain_for(&mut a, QUIET).await));
    sb.extend(settings(&drain_for(&mut b, QUIET).await));
    let own = |s: &[(u64, String)]| s.iter().filter(|(id, _)| *id == 7).cloned().collect::<Vec<_>>();
    let (own_a, own_b) = (own(&sa), own(&sb));
    assert_eq!(own_a.len(), 1, "a: one reply to its Set 7: {sa:?}");
    assert_eq!(own_b.len(), 1, "b: one reply to its Set 7: {sb:?}");
    assert_ne!(own_a[0].1, own_b[0].1, "each its own setting: {sa:?} / {sb:?}");
    assert!(sa.contains(&(VIEW_CHANGED_SETTING_ID, own_b[0].1.clone())), "a sees b's, view-only: {sa:?}");
    assert!(sb.contains(&(VIEW_CHANGED_SETTING_ID, own_a[0].1.clone())), "b sees a's, view-only: {sb:?}");
    end(&mut a).await;
}

/// A resume error is a `QueryResult{id}`: the asker's alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn colliding_resume_errors_reach_only_their_asker() {
    let _h = Home::new();
    let host = boot().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let mut a = attach(&handle, ClientKind::Tui).await;
    let mut b = attach(&handle, ClientKind::Attach).await;

    a.send(SessionCommand::Resume { id: 9, query: "no-such-session-aaaa".into() }).await.unwrap();
    b.send(SessionCommand::Resume { id: 9, query: "no-such-session-bbbb".into() }).await.unwrap();
    let ra = query_results(&reply_and_rest(&mut a, 9).await, 9);
    let rb = query_results(&reply_and_rest(&mut b, 9).await, 9);
    assert_eq!(ra.len(), 1, "a: {ra:#?}");
    assert_eq!(rb.len(), 1, "b: {rb:#?}");
    assert_eq!(ra[0]["kind"], "error");
    assert_eq!(rb[0]["kind"], "error");
    end(&mut a).await;
}

/// A successful resume is shared state: the other client sees `Resumed`
/// too, view-only, so its own `Resume 4` can't take it for its reply.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn another_clients_resume_is_view_only() {
    let _h = Home::new();
    let host = boot().await;
    let persist = || SessionConfig { persist: true, ..cfg() };
    let x = host.create_session(persist()).await.unwrap();
    let journal_x = x.journal_id();
    let mut tx = attach(&x, ClientKind::Tui).await;
    tx.send(submit("conversation x")).await.unwrap();
    until(&mut tx, |e| matches!(e, SessionEventWire::Idle)).await;
    end(&mut tx).await;
    let t0 = std::time::Instant::now();
    while host.attach(&x.id).is_some() {
        assert!(t0.elapsed() < Duration::from_secs(10), "x never left");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let handle = host.create_session(persist()).await.unwrap();
    let mut a = attach(&handle, ClientKind::Tui).await;
    let mut b = attach(&handle, ClientKind::Attach).await;
    a.send(SessionCommand::Resume { id: 4, query: journal_x.clone() }).await.unwrap();
    let resumed = |seen: &[Envelope]| {
        seen.iter()
            .filter_map(|e| match &e.event {
                SessionEventWire::Resumed { id, new_id, .. } => Some((*id, new_id.clone())),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let ra = resumed(&until(&mut a, |e| matches!(e, SessionEventWire::Resumed { .. })).await);
    let rb = resumed(&until(&mut b, |e| matches!(e, SessionEventWire::Resumed { .. })).await);
    assert_eq!(ra, vec![(4, journal_x.clone())], "the asker's reply");
    assert_eq!(rb, vec![(VIEW_CHANGED_SETTING_ID, journal_x)], "the other client's copy is view-only");
    end(&mut a).await;
}

/// A host's reply (`SessionHandle::send`) is the host's: its own
/// subscriber reads it; no attachment does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_host_reply_reaches_no_attachment() {
    let _h = Home::new();
    let host = boot().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let mut a = attach(&handle, ClientKind::Tui).await;
    let mut rx = handle.subscribe();
    handle.send(SessionCommand::Query { id: 11, query: SessionQuery::View }).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let SessionEventWire::QueryResult { id: 11, .. } = rx.recv().await.unwrap().event {
                return;
            }
        }
    })
    .await
    .expect("the host reads its reply");
    let seen = drain_for(&mut a, QUIET).await;
    assert!(query_results(&seen, 11).is_empty(), "never the attachment's: {seen:#?}");
    end(&mut a).await;
}

/// A reload fence's `busy` to a host rename answers the host, not the room.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn the_reload_fence_busy_reply_is_the_hosts() {
    let _h = Home::new();
    let host = boot().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let mut a = attach(&handle, ClientKind::Tui).await;
    let mut rx = handle.subscribe();
    handle.send(SessionCommand::Checkpoint { reason: CheckpointReason::Reload }).await.unwrap();
    let rename = RENAME_QUERY_ID_BASE + 77;
    handle
        .send(SessionCommand::EngineCommand { id: rename, name: "rename".into(), arg: r#"{"name":"x"}"#.into() })
        .await
        .unwrap();
    let busy = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let SessionEventWire::QueryResult { id, value } = rx.recv().await.unwrap().event {
                if id == rename {
                    return value;
                }
            }
        }
    })
    .await
    .expect("the host reads the fence's reply");
    assert_eq!(busy["code"], "busy", "{busy}");
    let seen = drain_for(&mut a, QUIET).await;
    assert!(query_results(&seen, rename).is_empty(), "never the attachment's: {seen:#?}");
    handle.send(SessionCommand::ReloadAborted).await.unwrap();
    end(&mut a).await;
}

/// `LocalTransport::send` is the attachment's own (= `send_from_self`), as
/// a socket client's frames are: a read-only mirror's input is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn local_send_is_stamped_with_its_attachment() {
    let _h = Home::new();
    let host = boot().await;
    let handle = host.create_session(exclusive_cfg()).await.unwrap();
    let mut a = attach(&handle, ClientKind::Tui).await;
    let mut b = attach(&handle, ClientKind::Attach).await;
    assert_eq!(a.input_owner(), Some(a.client_id()));

    b.send(submit("not the owner")).await.unwrap();
    let seen = until(&mut b, |e| matches!(e, SessionEventWire::Refused { .. } | SessionEventWire::TurnStarted { .. })).await;
    match &seen.last().unwrap().event {
        SessionEventWire::Refused { client, command, .. } => {
            assert_eq!(*client, b.client_id());
            assert_eq!(command, "submit");
        }
        other => panic!("a non-owner's send ran a turn: {other:?}"),
    }
    end(&mut a).await;
}

/// A client may send the idle probe's id; its reply is its own and never
/// satisfies the daemon's probe, which still sees the running turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_client_cannot_answer_the_idle_probe() {
    let _h = Home::new();
    let (url, _) = stub(SSE_PREFIX, true).await; // never finishes
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let mut a = attach(&handle, ClientKind::Tui).await;
    a.send(submit("run")).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::TurnStarted { .. })).await;
    for _ in 0..10 {
        // A View has no `streaming`: taken for the probe's reply it reads idle.
        a.send(SessionCommand::Query { id: IDLE_PROBE_QUERY_ID, query: SessionQuery::View }).await.unwrap();
        assert!(!agent_engine::daemon::session_is_idle(&handle).await, "a turn is running");
    }
    let seen = drain_for(&mut a, QUIET).await;
    assert!(!query_results(&seen, IDLE_PROBE_QUERY_ID).is_empty(), "the client got its own replies");
    a.send(SessionCommand::Cancel).await.unwrap();
    end(&mut a).await;
}

/// Client ids are never reused: a reply to an attachment that is gone
/// (a late completion) reaches no later attachment.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_reply_to_a_gone_attachment_reaches_no_successor() {
    let _h = Home::new();
    let host = boot().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let a = attach(&handle, ClientKind::Tui).await;
    let gone = a.client_id();
    a.send(SessionCommand::Detach { client: gone }).await.unwrap();
    drop(a);
    let mut c = attach(&handle, ClientKind::Tui).await;
    assert_ne!(c.client_id(), gone);
    handle.send_from(gone, SessionCommand::Query { id: 3, query: SessionQuery::View }).await.unwrap();
    let seen = drain_for(&mut c, QUIET).await;
    assert!(query_results(&seen, 3).is_empty(), "{seen:#?}");
    end(&mut c).await;
}
