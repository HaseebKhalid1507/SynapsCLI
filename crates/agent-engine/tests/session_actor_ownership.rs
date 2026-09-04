//! B1 — input ownership (Mirror/Observe/Takeover), `Refused`,
//! `InputOwnerChanged`, and `Checkpoint` (PLAN-phase3 §2.5, §5.3).

mod session_actor_common;
use session_actor_common::*;

use std::time::Duration;

use agent_engine::session::wire::CHECKPOINT_QUERY_ID;
use agent_engine::session::{
    AttachMode, CheckpointReason, ClientKind, ClientMeta, ClientTransport, LocalTransport,
    OwnerChangeReason, SessionCommand, SessionEventWire, SessionQuery, SessionSetting,
};
use serial_test::serial;

async fn attach(
    handle: &agent_engine::session::SessionHandle,
    kind: ClientKind,
    mode: AttachMode,
) -> (LocalTransport, agent_engine::session::AttachSnapshot) {
    LocalTransport::attach_with(handle.clone(), ClientMeta::new(kind), mode)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn first_mirror_owns_second_mirror_is_read_only_with_notice() {
    let _h = Home::new();
    let (url, hits) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();

    let (mut a, snap_a) = attach(&handle, ClientKind::Tui, AttachMode::Mirror).await;
    assert_eq!(snap_a.input_owner, Some(a.client_id()));
    assert_eq!(a.input_owner(), Some(a.client_id()));

    let (mut b, snap_b) = attach(&handle, ClientKind::Attach, AttachMode::Mirror).await;
    assert_eq!(snap_b.input_owner, Some(a.client_id()), "a keeps input");
    let seen = until(&mut b, |e| matches!(e, SessionEventWire::SystemNotice(_))).await;
    match &seen.last().unwrap().event {
        SessionEventWire::SystemNotice(n) => {
            assert!(n.contains("input is owned by client #1"), "{n}");
            assert!(n.contains("--takeover"), "{n}");
        }
        _ => unreachable!(),
    }

    // b's Submit is refused with no side effect: no TurnStarted, no stub hit.
    b.send_from_self(submit("hello")).await.unwrap();
    let seen = until(&mut b, |e| matches!(e, SessionEventWire::Refused { .. })).await;
    match &seen.last().unwrap().event {
        SessionEventWire::Refused {
            client,
            command,
            reason,
        } => {
            assert_eq!(*client, b.client_id());
            assert_eq!(command, "submit");
            assert!(reason.contains("client #1"), "{reason}");
        }
        _ => unreachable!(),
    }
    assert!(!seen
        .iter()
        .any(|e| matches!(e.event, SessionEventWire::TurnStarted { .. })));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);

    // a's Submit runs.
    a.send_from_self(submit("hello")).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    assert!(seen
        .iter()
        .any(|e| matches!(e.event, SessionEventWire::TurnStarted { .. })));
    assert_eq!(last_conversation(&seen).api_messages.len(), 2);
    end(&mut a).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn observer_never_owns_and_answer_allowed_from_any_client() {
    let _h = Home::new();
    let (url, _) = stub_seq(&[SSE_PROMPT_TOOL_USE, SSE_HI]).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = prompt_host().await;
    let handle = host.create_session(cfg()).await.unwrap();

    // Observer first: still nobody owns.
    let (mut o, snap_o) = attach(&handle, ClientKind::Attach, AttachMode::Observe).await;
    assert_eq!(snap_o.input_owner, None);
    o.send_from_self(submit("nope")).await.unwrap();
    let seen = until(&mut o, |e| matches!(e, SessionEventWire::Refused { .. })).await;
    assert!(matches!(
        &seen.last().unwrap().event,
        SessionEventWire::Refused { reason, .. } if reason.contains("no owner")
    ));

    // Mirror after an observer takes ownership.
    let (mut a, snap_a) = attach(&handle, ClientKind::Tui, AttachMode::Mirror).await;
    assert_eq!(snap_a.input_owner, Some(a.client_id()));
    a.send_from_self(submit("go")).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Prompt(_))).await;
    let pid = prompt_id(seen.last().unwrap()).unwrap();

    // The observer may answer the prompt (SPEC §8).
    until(&mut o, |e| matches!(e, SessionEventWire::Prompt(_))).await;
    o.send_from_self(SessionCommand::Answer {
        prompt_id: pid,
        value: Some("s3cret".into()),
    })
    .await
    .unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    assert_eq!(
        tool_results(&last_conversation(&seen)),
        vec!["answered:6".to_string()]
    );
    // Observer may query too.
    o.send_from_self(SessionCommand::Query {
        id: 9,
        query: SessionQuery::Status,
    })
    .await
    .unwrap();
    until(&mut o, |e| matches!(e, SessionEventWire::QueryResult { id: 9, .. })).await;
    end(&mut a).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn takeover_steals_and_notifies_previous_owner() {
    let _h = Home::new();
    let (url, _) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();

    let (mut a, _) = attach(&handle, ClientKind::Tui, AttachMode::Mirror).await;
    let (mut b, snap_b) = attach(&handle, ClientKind::Attach, AttachMode::Takeover).await;
    assert_eq!(snap_b.input_owner, Some(b.client_id()));

    let seen = until(&mut a, |e| matches!(e, SessionEventWire::InputOwnerChanged { .. })).await;
    match &seen.last().unwrap().event {
        SessionEventWire::InputOwnerChanged { from, to, reason } => {
            assert_eq!(*from, Some(a.client_id()));
            assert_eq!(*to, Some(b.client_id()));
            assert_eq!(*reason, OwnerChangeReason::Takeover);
        }
        _ => unreachable!(),
    }
    assert_eq!(a.input_owner(), Some(b.client_id()));

    a.send_from_self(submit("x")).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::Refused { .. })).await;
    b.send_from_self(submit("y")).await.unwrap();
    until(&mut b, |e| matches!(e, SessionEventWire::Idle)).await;

    // Host-originated sends bypass ownership.
    a.send(SessionCommand::Set {
        id: 3,
        setting: SessionSetting::ApiRetries { n: 1 },
    })
    .await
    .unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::SettingChanged(s) if s.id == 3)).await;
    end(&mut b).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn owner_detach_passes_to_oldest_non_observer() {
    let _h = Home::new();
    let (url, _) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();

    let (mut a, _) = attach(&handle, ClientKind::Tui, AttachMode::Mirror).await;
    let (mut o, _) = attach(&handle, ClientKind::Attach, AttachMode::Observe).await;
    let (mut b, _) = attach(&handle, ClientKind::Attach, AttachMode::Mirror).await;
    let (mut c, _) = attach(&handle, ClientKind::Attach, AttachMode::Mirror).await;

    a.send_from_self(SessionCommand::Detach {
        client: a.client_id(),
    })
    .await
    .unwrap();
    let seen = until(&mut b, |e| matches!(e, SessionEventWire::InputOwnerChanged { .. })).await;
    match &seen.last().unwrap().event {
        SessionEventWire::InputOwnerChanged { from, to, reason } => {
            assert_eq!(*from, Some(a.client_id()));
            assert_eq!(*to, Some(b.client_id()), "oldest non-observer, not o");
            assert_eq!(*reason, OwnerChangeReason::OwnerDetached);
        }
        _ => unreachable!(),
    }
    until(&mut o, |e| matches!(e, SessionEventWire::InputOwnerChanged { .. })).await;
    assert_eq!(o.input_owner(), Some(b.client_id()));
    c.send_from_self(submit("x")).await.unwrap();
    until(&mut c, |e| matches!(e, SessionEventWire::Refused { .. })).await;
    b.send_from_self(submit("y")).await.unwrap();
    until(&mut b, |e| matches!(e, SessionEventWire::Idle)).await;

    // Everyone gone → owner None; the next attach owns again.
    for t in [&mut b, &mut c, &mut o] {
        t.send_from_self(SessionCommand::Detach {
            client: t.client_id(),
        })
        .await
        .unwrap();
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (mut d, snap_d) = attach(&handle, ClientKind::Attach, AttachMode::Mirror).await;
    assert_eq!(snap_d.input_owner, Some(d.client_id()));
    end(&mut d).await;
}

/// `shell_start sleep <marker>` then `Checkpoint`: the PTY child is gone, the
/// stream is cancelled with abort context, the reply lands on
/// `CHECKPOINT_QUERY_ID`, and the session is still alive.
const SSE_SHELL_START: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_s1\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
    "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,",
    "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_sh\",\"name\":\"shell_start\"}}\n\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,",
    "\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"command\\\":\\\"sleep 271828\\\"}\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",",
    "\"stop_sequence\":null},\"usage\":{\"input_tokens\":10,\"output_tokens\":5,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

fn marker_alive() -> bool {
    std::process::Command::new("pgrep")
        .args(["-f", "sleep 271828"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn checkpoint_cancels_turn_saves_closes_ptys_answers_prompts_none() {
    let _h = Home::new();
    // Round 1: shell_start; round 2+: an endless stream so Checkpoint has a
    // turn to cancel.
    let (url, _) = stub_seq_endless_last(&[SSE_SHELL_START, SSE_PREFIX]).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host
        .create_session(agent_engine::session::SessionConfig {
            persist: true,
            ..cfg()
        })
        .await
        .unwrap();
    let (mut a, _) = attach(&handle, ClientKind::Tui, AttachMode::Mirror).await;
    a.send_from_self(submit("start a shell")).await.unwrap();
    // Wait for the tool result (PTY spawned) then the second round's delta.
    until(&mut a, |e| {
        matches!(
            e,
            SessionEventWire::Stream(agent_engine::StreamEvent::Llm(
                agent_engine::LlmEvent::ToolResult { .. }
            ))
        )
    })
    .await;
    assert!(marker_alive(), "PTY child running");
    until(&mut a, |e| {
        matches!(
            e,
            SessionEventWire::Stream(agent_engine::StreamEvent::Llm(
                agent_engine::LlmEvent::Text(_)
            ))
        )
    })
    .await;

    a.send(SessionCommand::Checkpoint {
        reason: CheckpointReason::Reload,
    })
    .await
    .unwrap();
    let seen = until(&mut a, |e| {
        matches!(e, SessionEventWire::QueryResult { id, .. } if *id == CHECKPOINT_QUERY_ID)
    })
    .await;
    assert!(seen.iter().any(|e| matches!(
        &e.event,
        SessionEventWire::SystemNotice(n) if n.contains("daemon reloading")
    )));
    assert!(seen.iter().any(|e| matches!(
        &e.event,
        SessionEventWire::SystemNotice(n) if n.starts_with("aborted")
    )));
    let conv = last_conversation(&seen);
    assert!(
        conv.abort_context.as_deref().unwrap_or("").contains("[response]: hi"),
        "abort context captured: {:?}",
        conv.abort_context
    );
    // PTY child killed (PtyHandle::drop) — allow the reap a moment.
    let mut gone = false;
    for _ in 0..20 {
        if !marker_alive() {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(gone, "shell_start child survived Checkpoint");
    assert!(handle.is_alive(), "checkpoint never ends the session");
    let saved = agent_engine::core::session::Session::load(handle.id.as_str()).expect("saved");
    assert!(saved.abort_context.is_some());
    end(&mut a).await;
}
