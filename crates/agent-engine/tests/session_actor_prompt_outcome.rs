//! Contract v2 §3: `PromptResolved.outcome` says what the session applied
//! when a prompt left the pending set, read by the gate the prompt was
//! raised for; a late `Answer` is answered `AnswerRefused` to its sender,
//! never broadcast; the policy refusals of a pending prompt's answer stay
//! the `Refused` they always were.
mod session_actor_common;
use session_actor_common::*;

use std::time::Duration;

use agent_engine::session::{
    AttachMode, Audience, ClientKind, ClientMeta, ClientTransport, Envelope, LocalTransport, PromptOutcome,
    SessionCommand, SessionEventWire,
};
use serial_test::serial;

async fn attach(handle: &agent_engine::session::SessionHandle, kind: ClientKind) -> LocalTransport {
    attach_as(handle, kind, AttachMode::Mirror).await
}

async fn attach_as(handle: &agent_engine::session::SessionHandle, kind: ClientKind, mode: AttachMode) -> LocalTransport {
    LocalTransport::attach_with(handle.clone(), ClientMeta::new(kind), mode).await.unwrap().0
}

/// The outcome `seen` carries for prompt `id` (`None` when it never resolved).
fn outcome_of(seen: &[Envelope], id: u64) -> Option<Option<PromptOutcome>> {
    seen.iter().find_map(|e| match e.event {
        SessionEventWire::PromptResolved { prompt_id, outcome } if prompt_id == id => Some(outcome),
        _ => None,
    })
}

/// The `AnswerRefused` `t` gets next: `(prompt_id, outcome)`.
async fn answer_refused(t: &mut LocalTransport) -> (u64, Option<PromptOutcome>) {
    let seen = until(t, |e| matches!(e, SessionEventWire::AnswerRefused { .. })).await;
    let env = seen.last().unwrap();
    assert_eq!(env.audience, Audience::Client(t.client_id()), "addressed to its sender");
    match env.event {
        SessionEventWire::AnswerRefused { prompt_id, outcome } => (prompt_id, outcome),
        _ => unreachable!(),
    }
}

/// The `Refused` addressed to `t` next.
async fn refused(t: &mut LocalTransport) -> (String, String) {
    let me = t.client_id();
    let seen = until(t, |e| matches!(e, SessionEventWire::Refused { client, .. } if *client == me)).await;
    assert!(!seen.iter().any(|e| matches!(e.event, SessionEventWire::PromptResolved { .. })), "{seen:#?}");
    match &seen.last().unwrap().event {
        SessionEventWire::Refused { command, reason, .. } => (command.clone(), reason.clone()),
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

fn allow_all_armed(seen: &[Envelope]) -> bool {
    seen.iter().any(|e| matches!(&e.event, SessionEventWire::SystemNotice(n) if n.contains("Allow all armed")))
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

/// An activation Confirm answered Allow all: that is no answer at the
/// activation gate, so `denied`, the gate stays shut and no latch is
/// announced.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn an_activation_confirm_answered_allow_all_resolves_denied() {
    let _h = Home::new();
    let (url, _) = stub_seq(&[SSE_PROMPT_TOOL_USE, SSE_HI]).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = activation_host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let mut a = attach(&handle, ClientKind::Tui).await;
    let pid = raise(&mut a).await;
    answer(&a, pid, Some("always")).await;
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    assert_eq!(outcome_of(&seen, pid), Some(Some(PromptOutcome::Denied)), "{seen:#?}");
    assert_eq!(tool_results(&last_conversation(&seen)), vec!["blocked".to_string()]);
    assert!(!allow_all_armed(&seen), "{seen:#?}");
    end(&mut a).await;
}

/// The same gate answered y: `allowed`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn an_activation_confirm_answered_yes_resolves_allowed() {
    let _h = Home::new();
    let (url, _) = stub_seq(&[SSE_PROMPT_TOOL_USE, SSE_HI]).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = activation_host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let mut a = attach(&handle, ClientKind::Tui).await;
    let pid = raise(&mut a).await;
    answer(&a, pid, Some(" Yes ")).await;
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    assert_eq!(outcome_of(&seen, pid), Some(Some(PromptOutcome::Allowed)), "{seen:#?}");
    assert_eq!(tool_results(&last_conversation(&seen)), vec!["allowed".to_string()]);
    end(&mut a).await;
}

/// A tool-call Confirm with no latch scope answered Allow all: `allowed`,
/// this call only. Nothing is armed, so nothing is announced.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn allow_all_without_a_scope_allows_this_call_only() {
    let _h = Home::new();
    let (url, _) = stub_seq(&[SSE_PROMPT_TOOL_USE, SSE_HI]).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = unscoped_confirm_host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let mut a = attach(&handle, ClientKind::Tui).await;
    a.send_from_self(submit("go")).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Prompt(_))).await;
    let SessionEventWire::Prompt(pr) = &seen.last().unwrap().event else { unreachable!() };
    assert!(pr.confirm_scope.is_none(), "no latch, no scope");
    let pid = pr.id;
    answer(&a, pid, Some("always")).await;
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    assert_eq!(outcome_of(&seen, pid), Some(Some(PromptOutcome::Allowed)), "{seen:#?}");
    assert_eq!(tool_results(&last_conversation(&seen)), vec!["allowed".to_string()]);
    assert!(!allow_all_armed(&seen), "{seen:#?}");
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

/// A Secret reaches its waiting call byte for byte: never trimmed, never
/// case-folded, never read as a confirm token. `answered`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_secret_reaches_its_call_byte_for_byte() {
    const SECRET: &str = "  AbC x\u{00FC}Z \t";
    let _h = Home::new();
    let (url, _) = stub_seq(&[SSE_PROMPT_TOOL_USE, SSE_HI]).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = prompt_host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let mut a = attach(&handle, ClientKind::Tui).await;
    *LAST_SECRET.lock().unwrap() = None;
    let pid = raise(&mut a).await;
    answer(&a, pid, Some(SECRET)).await;
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    assert_eq!(outcome_of(&seen, pid), Some(Some(PromptOutcome::Answered)), "{seen:#?}");
    let got = LAST_SECRET.lock().unwrap().clone().expect("the call ran").expect("a value, not a cancel");
    assert!(got.as_bytes() == SECRET.as_bytes(), "the call got {:?} ({} bytes), not the value sent", got, got.len());
    end(&mut a).await;
}

/// An empty Secret is a value: `answered`, and the waiting call gets `""`
/// (not a cancel).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn an_empty_secret_resolves_answered() {
    let _h = Home::new();
    let (url, _) = stub_seq(&[SSE_PROMPT_TOOL_USE, SSE_HI]).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = prompt_host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let mut a = attach(&handle, ClientKind::Tui).await;
    *LAST_SECRET.lock().unwrap() = None;
    let pid = raise(&mut a).await;
    answer(&a, pid, Some("")).await;
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    assert_eq!(outcome_of(&seen, pid), Some(Some(PromptOutcome::Answered)), "{seen:#?}");
    assert_eq!(tool_results(&last_conversation(&seen)), vec!["answered:0".to_string()]);
    assert_eq!(*LAST_SECRET.lock().unwrap(), Some(Some(String::new())));
    end(&mut a).await;
}

/// A Secret answered with nothing: `denied`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_secret_answered_with_nothing_resolves_denied() {
    let _h = Home::new();
    let (url, _) = stub_seq(&[SSE_PROMPT_TOOL_USE, SSE_HI]).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = prompt_host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let mut a = attach(&handle, ClientKind::Tui).await;
    let pid = raise(&mut a).await;
    answer(&a, pid, None).await;
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    assert_eq!(outcome_of(&seen, pid), Some(Some(PromptOutcome::Denied)), "{seen:#?}");
    assert_eq!(tool_results(&last_conversation(&seen)), vec!["cancelled".to_string()]);
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
    assert_eq!(answer_refused(&mut a).await, (pid, Some(PromptOutcome::Cancelled)));
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
    assert_eq!(answer_refused(&mut b).await, (pid, Some(PromptOutcome::TimedOut)));
    end(&mut b).await;
}

/// Two clients answer; the second answer is late. The first applies
/// (`allowed`); the second is refused to its sender alone, with what
/// applied, and changes nothing. An id never raised is refused with no
/// outcome (not remembered, not "denied").
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_late_answer_is_refused_to_its_sender_with_what_applied() {
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
    assert_eq!(answer_refused(&mut b).await, (pid, Some(PromptOutcome::Allowed)));

    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    assert_eq!(tool_results(&last_conversation(&seen)), vec!["allowed".to_string()], "the late deny changed nothing");
    assert_eq!(
        seen.iter().filter(|e| matches!(e.event, SessionEventWire::PromptResolved { .. })).count(),
        1,
        "resolved once"
    );

    answer(&b, pid + 100, Some("y")).await;
    assert_eq!(answer_refused(&mut b).await, (pid + 100, None));
    // A reads to past b's refusals: none reached it.
    let a_seen = drain_for(&mut a, Duration::from_millis(300)).await;
    assert!(
        !seen.iter().chain(&a_seen).any(|e| matches!(e.event, SessionEventWire::AnswerRefused { .. })),
        "only the late client is told: {seen:#?} {a_seen:#?}"
    );
    end(&mut a).await;
}

/// A host's late answer is refused to the host: no attachment hears it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_hosts_late_answer_reaches_no_attachment() {
    let _h = Home::new();
    let (url, _) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let mut a = attach(&handle, ClientKind::Tui).await;
    let mut rx = handle.subscribe();
    handle.send(SessionCommand::Answer { prompt_id: 41, value: Some("y".into()) }).await.unwrap();
    let env = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match rx.recv().await {
                Ok(env) if matches!(env.event, SessionEventWire::AnswerRefused { .. }) => break env,
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(e) => panic!("{e}"),
            }
        }
    })
    .await
    .expect("the host's refusal");
    assert_eq!(env.audience, Audience::Host);
    let seen = drain_for(&mut a, Duration::from_millis(300)).await;
    assert!(!seen.iter().any(|e| matches!(e.event, SessionEventWire::AnswerRefused { .. })), "{seen:#?}");
    end(&mut a).await;
}

/// The policy refusals of a pending prompt's answer are unchanged
/// `Refused{command: "answer"}`, broadcast as on dev (the owner sees both,
/// naming their senders), and read the raw Allow-all token for every
/// Confirm: a non-owner's `always` on an activation prompt is refused,
/// though that gate would deny it; an observer may not answer at all. The
/// prompt stays pending for the owner, whose `always` there is a denial.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn policy_refusals_are_unchanged_and_read_the_raw_token() {
    let _h = Home::new();
    let (url, _) = stub_seq(&[SSE_PROMPT_TOOL_USE, SSE_HI]).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = activation_host().await;
    let handle = host.create_session(exclusive_cfg()).await.unwrap();
    let mut a = attach(&handle, ClientKind::Tui).await;
    let mut b = attach(&handle, ClientKind::Attach).await;
    let mut o = attach_as(&handle, ClientKind::Attach, AttachMode::Observe).await;
    let pid = raise(&mut a).await;
    until(&mut b, |e| matches!(e, SessionEventWire::Prompt(_))).await;
    until(&mut o, |e| matches!(e, SessionEventWire::Prompt(_))).await;

    answer(&b, pid, Some(" ALWAYS ")).await;
    assert_eq!(refused(&mut b).await, ("answer".to_string(), "only the input owner can choose Allow all".to_string()));
    answer(&o, pid, Some("y")).await;
    assert_eq!(
        refused(&mut o).await,
        ("answer".to_string(), "observe-mode clients cannot answer confirm prompts".to_string())
    );

    // Both reached the owner too, unaddressed, with their senders.
    let (bid, oid) = (b.client_id(), o.client_id());
    let mut at_owner = Vec::new();
    for _ in 0..2 {
        let seen = until(&mut a, |e| matches!(e, SessionEventWire::Refused { .. })).await;
        let env = seen.last().unwrap();
        assert_eq!(env.audience, Audience::All, "broadcast as on dev: {env:?}");
        match &env.event {
            SessionEventWire::Refused { client, command, reason } => at_owner.push((*client, command.clone(), reason.clone())),
            _ => unreachable!(),
        }
    }
    assert_eq!(
        at_owner,
        vec![
            (bid, "answer".to_string(), "only the input owner can choose Allow all".to_string()),
            (oid, "answer".to_string(), "observe-mode clients cannot answer confirm prompts".to_string()),
        ]
    );

    answer(&a, pid, Some("always")).await;
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    assert_eq!(outcome_of(&seen, pid), Some(Some(PromptOutcome::Denied)), "{seen:#?}");
    assert_eq!(tool_results(&last_conversation(&seen)), vec!["blocked".to_string()]);
    end(&mut a).await;
}
