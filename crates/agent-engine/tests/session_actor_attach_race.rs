//! #457 — an attach takes the answer to its own `Attach`. Concurrent
//! attaches to one session (both queued behind a parked session's load)
//! each get their own client id and the snapshot built for their own
//! `ClientMeta`; a refusal is addressed; a waiter that gave up leaves
//! nobody attached. Real actor, `LocalTransport::attach_with`.

mod session_actor_common;
use session_actor_common::*;

use std::time::Duration;

use agent_engine::session::{
    delivers, AttachMode, AttachTicket, ClientId, ClientKind, ClientMeta, ClientTransport,
    EndReason, HistoryMode, LocalTransport, SessionCommand, SessionConfig, SessionEventWire,
    SessionHandle, SessionLifecycle, Subscriber, TransportError,
};
use serial_test::serial;

struct Grace;
impl Grace {
    fn zero() -> Self {
        std::env::set_var("SYNAPS_DAEMON_PARK_GRACE_SECS", "0");
        Self
    }
}
impl Drop for Grace {
    fn drop(&mut self) {
        std::env::remove_var("SYNAPS_DAEMON_PARK_GRACE_SECS");
    }
}

fn meta(kind: ClientKind, history: HistoryMode) -> ClientMeta {
    let mut m = ClientMeta::new(kind);
    m.history = history;
    m
}

async fn wait_parked(handle: &SessionHandle) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while handle.lifecycle() != SessionLifecycle::Parked {
        assert!(tokio::time::Instant::now() < deadline, "the session parks");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// A persisted, exclusive-input session with one finished turn, parked.
async fn parked_session(host: &std::sync::Arc<agent_engine::EngineHost>) -> SessionHandle {
    let handle = host
        .create_session(SessionConfig { persist: true, ..exclusive_cfg() })
        .await
        .unwrap();
    let (mut t, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui)).await.unwrap();
    t.send(submit("hello")).await.unwrap();
    until(&mut t, |e| matches!(e, SessionEventWire::Idle)).await;
    t.send(SessionCommand::Detach { client: t.client_id() }).await.unwrap();
    until(&mut t, |e| matches!(e, SessionEventWire::ClientLeft { .. })).await;
    wait_parked(&handle).await;
    handle
}

async fn end(handle: &SessionHandle) {
    let _ = handle.send(SessionCommand::End { reason: EndReason::HostShutdown }).await;
}

/// Two clients attach at once to a parked session: both Attaches are queued
/// behind the load before either is answered. One is a full-history TUI,
/// the other a digest client, so a swapped snapshot shows. Detaching one
/// leaves the other attached and owning input.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn concurrent_attaches_to_a_parked_session_each_get_their_own_answer() {
    let _h = Home::new();
    let _g = Grace::zero();
    let (url, _) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = parked_session(&host).await;

    let (a, b) = tokio::join!(
        LocalTransport::attach_with(handle.clone(), meta(ClientKind::Tui, HistoryMode::Full), AttachMode::Mirror),
        LocalTransport::attach_with(handle.clone(), meta(ClientKind::Attach, HistoryMode::Digest), AttachMode::Mirror),
    );
    let (mut a, snap_a) = a.unwrap();
    let (mut b, snap_b) = b.unwrap();
    assert_eq!(handle.lifecycle(), SessionLifecycle::Live);

    assert_ne!(a.client_id(), b.client_id(), "each attach got its own client id");
    assert_eq!(snap_a.conversation.api_messages.len(), 2, "the full client got the history");
    assert!(snap_a.display_tail.is_none(), "the full client got no digest");
    assert!(snap_b.conversation.api_messages.is_empty(), "the digest client got no history");
    assert_eq!(snap_b.conversation.messages_len, 2);
    assert!(snap_b.display_tail.is_some(), "the digest client got its tail");

    // The session agrees with what each was told: two clients, by kind.
    let kind_of = |id: ClientId| handle.presence().roster.iter().find(|c| c.id == id).map(|c| c.kind);
    assert_eq!(handle.presence().clients, 2);
    assert_eq!(kind_of(a.client_id()), Some(ClientKind::Tui));
    assert_eq!(kind_of(b.client_id()), Some(ClientKind::Attach));
    // a's Attach was sent first: it owns input, and both were told so.
    assert_eq!(handle.presence().input_owner, Some(a.client_id()));
    assert_eq!(snap_a.input_owner, Some(a.client_id()));
    assert_eq!(snap_b.input_owner, Some(a.client_id()));

    // Neither is delivered the other's `Attached`; each sees b leave.
    b.send(SessionCommand::Detach { client: b.client_id() }).await.unwrap();
    let left = b.client_id();
    for t in [&mut a, &mut b] {
        let seen = until(t, |e| matches!(e, SessionEventWire::ClientLeft { client } if *client == left)).await;
        assert!(
            !seen.iter().any(|e| matches!(e.event, SessionEventWire::Attached { .. })),
            "an attach's answer is not delivered to attached clients"
        );
    }
    // Detaching b removed b, not a: a is attached, owns input, and drives.
    let p = handle.presence();
    assert_eq!(p.roster.iter().map(|c| c.id).collect::<Vec<_>>(), [a.client_id()]);
    assert_eq!(p.input_owner, Some(a.client_id()));
    a.send(submit("again")).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    assert_eq!(last_conversation(&seen).api_messages.len(), 4, "a's turn ran");
    end(&handle).await;
}

/// `ensure_live` fails (the parked session's journal is locked elsewhere):
/// each of two concurrent attaches gets a refusal, the refusal is addressed
/// to no subscriber, and once the lock is gone an attach succeeds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_refused_attach_is_addressed_to_its_asker() {
    let _h = Home::new();
    let _g = Grace::zero();
    let (url, _) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = parked_session(&host).await;

    let lock = agent_core::session_lock::SessionLock::try_acquire(
        &agent_core::session_lock::sessions_dir(),
        &handle.journal_id(),
        agent_core::session_lock::LockHolder { pid: std::process::id(), kind: "test".into() },
    )
    .expect("the parked session released its journal lock");

    let mut watch = handle.subscribe();
    let (a, b) = tokio::join!(
        LocalTransport::attach_with(handle.clone(), meta(ClientKind::Tui, HistoryMode::Full), AttachMode::Mirror),
        LocalTransport::attach_with(handle.clone(), meta(ClientKind::Attach, HistoryMode::Digest), AttachMode::Mirror),
    );
    for r in [a.map(|_| ()), b.map(|_| ())] {
        assert!(
            matches!(r, Err(TransportError::Refused(ref m)) if m.contains("could not be restored")),
            "refused: {r:?}"
        );
    }
    let mut refusals = 0;
    while let Ok(env) = watch.try_recv() {
        if matches!(env.event, SessionEventWire::AttachRefused { .. }) {
            refusals += 1;
            assert!(delivers(&env, Subscriber::Attachment(ClientId(1))).is_none(), "not for an attachment");
            assert!(delivers(&env, Subscriber::Host).is_none(), "not for a host listener");
        }
    }
    assert_eq!(refusals, 2, "one refusal per attach");
    assert_eq!(handle.presence().clients, 0);

    lock.release_and_remove();
    let (t, snap) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui)).await.unwrap();
    assert_eq!(snap.conversation.api_messages.len(), 2);
    assert_eq!(handle.presence().roster.iter().map(|c| c.id).collect::<Vec<_>>(), [t.client_id()]);
    end(&handle).await;
}

/// The orphan: a waiter gave up (timed out) before the session got to its
/// Attach. The session attaches nobody for it, so no client stays attached
/// with no one to detach it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn an_attach_whose_waiter_gave_up_attaches_nobody() {
    let _h = Home::new();
    let _g = Grace::zero();
    let (url, _) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = parked_session(&host).await;

    let mut watch = handle.subscribe();
    let ticket = AttachTicket::new();
    assert!(ticket.abandon());
    handle
        .send(SessionCommand::Attach {
            client: ClientMeta::new(ClientKind::Tui),
            mode: AttachMode::Mirror,
            owner: None,
            ticket: Some(ticket),
        })
        .await
        .unwrap();
    // The next attach is answered after it: by then the abandoned one was handled.
    let (t, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Attach)).await.unwrap();
    let p = handle.presence();
    assert_eq!(p.roster.iter().map(|c| (c.id, c.kind)).collect::<Vec<_>>(), [(t.client_id(), ClientKind::Attach)]);
    assert_eq!(p.input_owner, Some(t.client_id()), "the abandoned attach took no input");
    while let Ok(env) = watch.try_recv() {
        assert!(
            !matches!(env.event, SessionEventWire::ClientJoined { client, .. } if client != t.client_id()),
            "nobody joined for the abandoned attach"
        );
    }
    end(&handle).await;
}
