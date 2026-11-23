//! `PromptResolved.outcome` says what the session applied when a
//! prompt left the pending set, and a late `Answer` is refused with that
//! outcome instead of being dropped without a word.
mod session_actor_common;
use session_actor_common::*;

use std::time::Duration;

use agent_engine::session::{
    AttachMode, ClientKind, ClientMeta, ClientTransport, Envelope, LocalTransport, PromptOutcome,
    SessionCommand, SessionEventWire,
};
use serial_test::serial;

async fn attach(handle: &agent_engine::session::SessionHandle, kind: ClientKind) -> LocalTransport {
    LocalTransport::attach_with(handle.clone(), ClientMeta::new(kind), AttachMode::Mirror)
        .await
        .unwrap()
        .0
}

/// The outcome `seen` carries for prompt `id` (`None` when it never resolved).
fn outcome_of(seen: &[Envelope], id: u64) -> Option<Option<PromptOutcome>> {
    seen.iter().find_map(|e| match e.event {
        SessionEventWire::PromptResolved { prompt_id, outcome } if prompt_id == id => Some(outcome),
        _ => None,
    })
}

/// The reason of the `Refused{command: "answer"}` addressed to `t`.
async fn answer_refusal(t: &mut LocalTransport) -> String {
    let me = t.client_id();
    let seen = until(t, |e| matches!(e, SessionEventWire::Refused { client, .. } if *client == me)).await;
    match &seen.last().unwrap().event {
        SessionEventWire::Refused { command, reason, .. } => {
            assert_eq!(command, "answer", "{seen:#?}");
            reason.clone()
        }
        _ => unreachable!(),
    }
}

/// Submit, wait for the tool's prompt: its id.
async fn raise(t: &mut LocalTransport) -> u64 {
    t.send_from_self(submit("go")).await.unwrap();
    let seen = until(t, |e| matches!(e, SessionEventWire::Prompt(_))).await;
    prompt_id(seen.last().unwrap()).unwrap()
}

async fn answer(t: &LocalTransport, prompt_id: u64, value: Option<&str>) {
    t.send_from_self(SessionCommand::Answer { prompt_id, value: value.map(str::to_string) })
        .await
        .unwrap();
}

/// A tool-call Confirm answered Allow: `allowed`, and the gate opened.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_confirm_answered_allow_resolves_allowed() {
    let _h = Home::new();
    let (url, _) = stub_seq(&[SSE_PROMPT_TOOL_USE, SSE_HI]).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = confirm_host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let mut a = attach(&handle, ClientKind::Tui).await;
    let pid = raise(&mut a).await;
    answer(&a, pid, Some("y")).await;
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    assert_eq!(outcome_of(&seen, pid), Some(Some(PromptOutcome::Allowed)), "{seen:#?}");
    assert_eq!(tool_results(&last_conversation(&seen)), vec!["allowed".to_string()]);
    end(&mut a).await;
}

/// A tool-call Confirm answered Deny: `denied`, and the gate stayed shut.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_confirm_answered_deny_resolves_denied() {
    let _h = Home::new();
    let (url, _) = stub_seq(&[SSE_PROMPT_TOOL_USE, SSE_HI]).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = confirm_host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let mut a = attach(&handle, ClientKind::Tui).await;
    let pid = raise(&mut a).await;
    answer(&a, pid, Some("n")).await;
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    assert_eq!(outcome_of(&seen, pid), Some(Some(PromptOutcome::Denied)), "{seen:#?}");
    assert_eq!(tool_results(&last_conversation(&seen)), vec!["blocked".to_string()]);
    end(&mut a).await;
}

/// A Secret prompt given a value: `answered`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_secret_given_a_value_resolves_answered() {
    let _h = Home::new();
    let (url, _) = stub_seq(&[SSE_PROMPT_TOOL_USE, SSE_HI]).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = prompt_host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let mut a = attach(&handle, ClientKind::Tui).await;
    let pid = raise(&mut a).await;
    answer(&a, pid, Some("s3cret")).await;
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    assert_eq!(outcome_of(&seen, pid), Some(Some(PromptOutcome::Answered)), "{seen:#?}");
    assert_eq!(tool_results(&last_conversation(&seen)), vec!["answered:6".to_string()]);
    end(&mut a).await;
}

/// A cancel while a Confirm waits: `cancelled`, not `denied`, though the
/// gate's own result reads like a denial ("Tool call confirmation denied").
/// An Allow sent after it is refused with what applied.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_cancel_before_the_gate_resolves_cancelled_and_refuses_a_late_allow() {
    let _h = Home::new();
    let (url, hits) = stub_seq(&[SSE_PROMPT_TOOL_USE, SSE_HI]).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = confirm_host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let mut a = attach(&handle, ClientKind::Tui).await;
    let pid = raise(&mut a).await;
    a.send_from_self(SessionCommand::Cancel).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    assert_eq!(outcome_of(&seen, pid), Some(Some(PromptOutcome::Cancelled)), "{seen:#?}");
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1, "the turn ended at the gate");

    answer(&a, pid, Some("y")).await;
    let reason = answer_refusal(&mut a).await;
    assert_eq!(reason, format!("prompt {pid} was already resolved: cancelled"));
    end(&mut a).await;
}

/// Nobody attached by the abandonment deadline: `timed_out`. Seen on a raw
/// subscription (no client is attached when it happens).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn an_unattended_prompt_resolves_timed_out() {
    let _h = Home::new();
    std::env::set_var("SYNAPS_DAEMON_PROMPT_ABANDON_SECS", "1");
    let (url, _) = stub_seq(&[SSE_PROMPT_TOOL_USE, SSE_HI]).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = confirm_host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let mut raw = handle.subscribe();
    let mut a = attach(&handle, ClientKind::Tui).await;
    let pid = raise(&mut a).await;
    a.send(SessionCommand::Detach { client: a.client_id() }).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::ClientLeft { .. })).await;
    drop(a);

    let outcome = tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            match raw.recv().await {
                Ok(env) => {
                    if let SessionEventWire::PromptResolved { prompt_id, outcome } = env.event {
                        if prompt_id == pid {
                            break outcome;
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(e) => panic!("the session went away: {e}"),
            }
        }
    })
    .await
    .expect("the deadline resolves the prompt");
    std::env::remove_var("SYNAPS_DAEMON_PROMPT_ABANDON_SECS");
    assert_eq!(outcome, Some(PromptOutcome::TimedOut));

    let mut b = attach(&handle, ClientKind::Attach).await;
    answer(&b, pid, Some("y")).await;
    assert_eq!(answer_refusal(&mut b).await, format!("prompt {pid} was already resolved: timed_out"));
    end(&mut b).await;
}

/// Two clients answer; the second answer is late. The first applies
/// (`allowed`), the second is refused to its sender only, with what
/// applied, and changes nothing. An id that was never raised is refused
/// as unknown.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_late_answer_is_refused_with_what_applied() {
    let _h = Home::new();
    let (url, _) = stub_seq(&[SSE_PROMPT_TOOL_USE, SSE_HI]).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = confirm_host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let mut a = attach(&handle, ClientKind::Tui).await;
    let mut b = attach(&handle, ClientKind::Attach).await;
    let pid = raise(&mut a).await;
    until(&mut b, |e| matches!(e, SessionEventWire::Prompt(_))).await;

    answer(&a, pid, Some("y")).await;
    let b_seen = until(&mut b, |e| matches!(e, SessionEventWire::PromptResolved { .. })).await;
    assert_eq!(outcome_of(&b_seen, pid), Some(Some(PromptOutcome::Allowed)));
    answer(&b, pid, Some("n")).await;
    assert_eq!(answer_refusal(&mut b).await, format!("prompt {pid} was already resolved: allowed"));

    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    assert_eq!(tool_results(&last_conversation(&seen)), vec!["allowed".to_string()], "the late deny changed nothing");
    let a_id = a.client_id();
    assert!(
        !seen.iter().any(|e| matches!(e.event, SessionEventWire::Refused { client, .. } if client == a_id)),
        "only the late client is refused: {seen:#?}"
    );
    assert_eq!(
        seen.iter().filter(|e| matches!(e.event, SessionEventWire::PromptResolved { .. })).count(),
        1,
        "resolved once"
    );

    answer(&b, pid + 100, Some("y")).await;
    assert_eq!(answer_refusal(&mut b).await, format!("no pending prompt {}", pid + 100));
    end(&mut a).await;
}
