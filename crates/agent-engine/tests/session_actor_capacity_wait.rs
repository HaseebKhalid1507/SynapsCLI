//! A turn waiting for Anthropic account capacity keeps its work across a
//! daemon reload: the reload's checkpoint cuts the waiting turn (nothing was
//! sent), the cut is recorded, and the rehydrate's resume (sent twice for the
//! same reload) starts ONE resumed turn, which enters a fresh wait instead of
//! sending blindly. Esc during that wait ends it with no error.
//!
//! In-process actor, loopback Anthropic SSE stub (never reached), isolated
//! HOME, no daemon. Automatic selection is a scripted broker installed for
//! every session of this test binary (`turn_broker_testing`): every account
//! is cooling down until a reset 2 h away, so the wait never wakes here.

mod session_actor_common;
use session_actor_common::*;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agent_engine::auth::{
    AccessToken, AccountFailure, AccountFailureKind, AccountSelector, BrokerError,
    CredentialBroker, CredentialRef, OAuthProviderId, PinnedToken, ProxyByteStream, ProxyRequest,
    ProxyResponse, SelectionFailure, SelectionFailureKind,
};
use agent_engine::core::session::Session;
use agent_engine::engine::interrupt::InterruptReason;
use agent_engine::session::wire::CHECKPOINT_QUERY_ID;
use agent_engine::session::{
    CheckpointReason, ClientKind, ClientMeta, ClientTransport, EndReason, Envelope,
    LocalTransport, SessionCommand, SessionConfig, SessionEventWire, TurnTrigger,
};
use agent_engine::{SessionEvent, StreamEvent};
use serde_json::Value;
use serial_test::serial;

/// Automatic selection with every account cooling down until `reset_ms`.
struct CoolingBroker {
    reset_ms: u64,
    selections: AtomicUsize,
}

impl CoolingBroker {
    fn selections(&self) -> usize {
        self.selections.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl CredentialBroker for CoolingBroker {
    async fn access_token(&self, _p: OAuthProviderId) -> Result<AccessToken, BrokerError> {
        Err(BrokerError::Denied("legacy path must not be used".into()))
    }
    fn account_selector(&self, _p: OAuthProviderId) -> AccountSelector {
        AccountSelector::Auto
    }
    fn active_cooldown(&self, _cred: &CredentialRef) -> Option<u64> {
        Some(self.reset_ms)
    }
    async fn access_token_for(&self, _cred: &CredentialRef) -> Result<AccessToken, BrokerError> {
        Err(BrokerError::Denied("no explicit vend in this test".into()))
    }
    async fn access_token_pinned_for(
        &self,
        _provider: OAuthProviderId,
        _model: Option<&str>,
    ) -> Result<PinnedToken, BrokerError> {
        self.selections.fetch_add(1, Ordering::SeqCst);
        Err(BrokerError::NoAccountAvailable {
            provider: "anthropic".into(),
            reason: "scripted".into(),
            failure: Some(SelectionFailure {
                kind: SelectionFailureKind::AllCoolingDown,
                accounts: vec![AccountFailure {
                    account: "claude4".into(),
                    kind: AccountFailureKind::Exhausted,
                    http_status: None,
                    retry_after_s: None,
                    until_ms: Some(self.reset_ms),
                    cooling_down: true,
                }],
                earliest_reset_ms: Some(self.reset_ms),
                usage_read_retry_ms: None,
            }),
        })
    }
    async fn proxy(&self, _r: ProxyRequest) -> Result<ProxyResponse, BrokerError> {
        Err(BrokerError::Denied("not implemented in stub".into()))
    }
    async fn proxy_stream(&self, _r: ProxyRequest) -> Result<ProxyByteStream, BrokerError> {
        Err(BrokerError::Denied("not implemented in stub".into()))
    }
    async fn anthropic_usage(&self) -> Result<Value, BrokerError> {
        Err(BrokerError::Denied("not implemented in stub".into()))
    }
    async fn capabilities(
        &self,
    ) -> Result<Vec<agent_engine::auth::ProviderStatus>, BrokerError> {
        Ok(vec![])
    }
}

fn is_wait_notice(e: &SessionEventWire) -> bool {
    matches!(
        e,
        SessionEventWire::Stream(StreamEvent::Session(SessionEvent::Notice(text)))
            if text.starts_with("⏸ ") && text.contains("; waiting until ")
    )
}

fn wait_notices(seen: &[Envelope]) -> usize {
    seen.iter().filter(|e| is_wait_notice(&e.event)).count()
}

fn errors(seen: &[Envelope]) -> Vec<String> {
    seen.iter()
        .filter_map(|e| match &e.event {
            SessionEventWire::Stream(StreamEvent::Session(SessionEvent::Error(err))) => {
                Some(err.message.clone())
            }
            _ => None,
        })
        .collect()
}

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

/// Review S1: a reload checkpoint while a turn waits for capacity resumes
/// it exactly once, into a fresh wait; nothing is ever sent, and Esc during
/// the fresh wait ends it with no error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_reload_during_a_capacity_wait_resumes_once_into_a_fresh_wait() {
    let _h = Home::new();
    let broker = Arc::new(CoolingBroker {
        reset_ms: agent_engine::epoch_millis() + 2 * 3600 * 1000,
        selections: AtomicUsize::new(0),
    });
    let _installed = agent_engine::runtime::turn_broker_testing::install_global(broker.clone());
    let (url, hits) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let (handle, task) = host.create_session_unspawned(cfg()).await.unwrap();
    tokio::spawn(task.run());
    let (mut client, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Test))
        .await
        .unwrap();

    // The turn starts and waits (its notice), with nothing sent.
    client.send(submit("work that waits for capacity")).await.unwrap();
    let seen = until(&mut client, is_wait_notice).await;
    assert!(errors(&seen).is_empty(), "{seen:#?}");
    assert_eq!(broker.selections(), 1);
    assert_eq!(hits.load(Ordering::SeqCst), 0);

    // The reload's checkpoint cuts the waiting turn, and its record says so.
    handle
        .send(SessionCommand::Checkpoint { reason: CheckpointReason::Reload, deadline: None, skip_quiesce_grace: false })
        .await
        .unwrap();
    let seen = until(&mut client, |e| {
        matches!(e, SessionEventWire::QueryResult { id, .. } if *id == CHECKPOINT_QUERY_ID)
    })
    .await;
    assert!(errors(&seen).is_empty(), "a cut wait is not an error: {seen:#?}");
    let SessionEventWire::QueryResult { value, .. } = &seen.last().unwrap().event else {
        unreachable!()
    };
    assert_eq!(value["record"]["turn_cut"], Value::Bool(true), "{value}");
    handle.send(SessionCommand::ReloadAborted).await.unwrap();
    let seen = drain_for(&mut client, Duration::from_millis(500)).await;
    assert_eq!(turns_started(&seen), 0, "nothing starts by itself: {seen:#?}");

    // The rehydrate's command, twice for the same reload: ONE resumed turn,
    // which enters a fresh wait (a new selection, a new notice), sending
    // nothing.
    let at = chrono::DateTime::parse_from_rfc3339("2026-10-20T03:40:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let resume = || SessionCommand::ResumeAfterReload {
        generation: 7,
        reloaded_at: at,
        turn_cut: Some(true),
        cut_tail: None,
        stopped: false,
        reply: None,
    };
    handle.send(resume()).await.unwrap();
    handle.send(resume()).await.unwrap();
    let mut seen = until(&mut client, is_wait_notice).await;
    seen.extend(drain_for(&mut client, Duration::from_millis(1500)).await);
    let text = agent_engine::daemon::reload::resume_text(at);
    assert_eq!(resume_turns(&seen), vec![Some(text)], "{seen:#?}");
    assert_eq!(turns_started(&seen), 1, "{seen:#?}");
    assert_eq!(wait_notices(&seen), 1, "one fresh wait: {seen:#?}");
    assert!(errors(&seen).is_empty(), "{seen:#?}");
    assert_eq!(broker.selections(), 2, "the resumed turn selected once, then waits");
    assert_eq!(hits.load(Ordering::SeqCst), 0, "nothing was sent blindly");

    // Esc during the fresh wait: the turn ends with no error, still nothing
    // sent and no further selection.
    client.send(SessionCommand::Cancel).await.unwrap();
    let seen = until(&mut client, |e| matches!(e, SessionEventWire::Idle)).await;
    assert!(errors(&seen).is_empty(), "Esc is not an error: {seen:#?}");
    assert_eq!(broker.selections(), 2);
    assert_eq!(hits.load(Ordering::SeqCst), 0);

    end(&mut client).await;
}

fn text(m: &agent_engine::SharedMessage) -> String {
    match &m["content"] {
        Value::String(s) => s.clone(),
        other => other[0]["text"].as_str().unwrap_or("").to_string(),
    }
}

/// Poll the session file until `pred` holds (saves run on a blocking pool).
async fn on_disk_until(id: &str, pred: impl Fn(&[agent_engine::SharedMessage]) -> bool) -> Vec<String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(s) = Session::load(id) {
            if pred(&s.api_messages) {
                return s.api_messages.iter().map(text).collect();
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "session file never reached the expected state: {:#?}",
            Session::load(id).map(|s| s.api_messages)
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Review S1 remainder: the owner closes the session (End) while its turn
/// waits for capacity. The session ends once, with no error; the user's
/// message is kept on disk, followed by the interruption marker; nothing
/// was sent and nothing was selected after the close.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn closing_the_session_during_a_capacity_wait_keeps_its_history() {
    let _h = Home::new();
    let broker = Arc::new(CoolingBroker {
        reset_ms: agent_engine::epoch_millis() + 2 * 3600 * 1000,
        selections: AtomicUsize::new(0),
    });
    let _installed = agent_engine::runtime::turn_broker_testing::install_global(broker.clone());
    let (url, hits) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let (handle, task) = host
        .create_session_unspawned(SessionConfig { persist: true, ..cfg() })
        .await
        .unwrap();
    tokio::spawn(task.run());
    let (mut client, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Test))
        .await
        .unwrap();
    let id = handle.journal_id();

    // The turn starts and waits (its notice), with nothing sent.
    client.send(submit("work that waits for capacity")).await.unwrap();
    let seen = until(&mut client, is_wait_notice).await;
    assert!(errors(&seen).is_empty(), "{seen:#?}");
    assert_eq!(broker.selections(), 1);
    assert_eq!(hits.load(Ordering::SeqCst), 0);

    // The owner closes the session during the wait.
    client
        .send(SessionCommand::End { reason: EndReason::ClientQuit })
        .await
        .unwrap();
    let mut seen = until(&mut client, |e| matches!(e, SessionEventWire::Ended { .. })).await;
    seen.extend(drain_for(&mut client, Duration::from_millis(500)).await);
    assert!(errors(&seen).is_empty(), "closing during a wait is not an error: {seen:#?}");
    let ended = seen
        .iter()
        .filter(|e| matches!(e.event, SessionEventWire::Ended { .. }))
        .count();
    assert_eq!(ended, 1, "{seen:#?}");
    assert_eq!(turns_started(&seen), 0, "nothing starts after the close: {seen:#?}");
    assert_eq!(wait_notices(&seen), 0, "no new wait: {seen:#?}");
    assert_eq!(broker.selections(), 1, "no selection after the close");
    assert_eq!(hits.load(Ordering::SeqCst), 0, "nothing was sent");

    // The history is kept: the user's message, then the marker. Polled
    // until the marker is saved, so an earlier save can't end the poll.
    let saved = on_disk_until(&id, |m| {
        m.last().is_some_and(|last| text(last) == InterruptReason::User.marker())
    })
    .await;
    assert_eq!(
        saved,
        ["work that waits for capacity".to_string(), InterruptReason::User.marker().to_string()],
    );
}
