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
    // b learns it from ITS snapshot; the owner is told nothing.
    let owned = snap_b.input_owned_elsewhere(b.client_id()).expect("owned elsewhere");
    assert!(owned.contains("input is owned by tui (#1)"), "{owned}");
    assert!(owned.contains("--takeover"), "{owned}");
    assert_eq!(snap_a.input_owned_elsewhere(a.client_id()), None, "a owns it");
    let a_saw = drain_for(&mut a, std::time::Duration::from_millis(300)).await;
    assert!(
        !a_saw.iter().any(|e| matches!(
            &e.event,
            SessionEventWire::SystemNotice(n) if n.contains("owned by")
        )),
        "the owner is not told its own input is owned: {a_saw:#?}"
    );

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
            assert_eq!(reason, "input owned by tui (#1)");
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

/// #148 C2: an observer may not answer a tool-call Confirm at all; a
/// non-owner Mirror may answer it but may not arm "Allow all"; the input
/// owner can, and arming it is announced to EVERY attached client.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn confirm_answers_are_owner_gated_and_allow_all_is_announced() {
    let _h = Home::new();
    let (url, _) = stub_seq(&[SSE_PROMPT_TOOL_USE, SSE_HI]).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = confirm_host().await;
    let handle = host.create_session(cfg()).await.unwrap();

    let (mut a, snap_a) = attach(&handle, ClientKind::Tui, AttachMode::Mirror).await;
    assert_eq!(snap_a.input_owner, Some(a.client_id()));
    let (mut b, _) = attach(&handle, ClientKind::Attach, AttachMode::Mirror).await;
    let (mut o, _) = attach(&handle, ClientKind::Attach, AttachMode::Observe).await;

    a.send_from_self(submit("go")).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Prompt(_))).await;
    let SessionEventWire::Prompt(pr) = &seen.last().unwrap().event else {
        unreachable!()
    };
    let pid = pr.id;
    assert_eq!(
        pr.confirm_scope.as_ref().map(|s| s.extensions.clone()),
        Some(vec!["jev".to_string()]),
        "the wire prompt names the source extension"
    );
    until(&mut b, |e| matches!(e, SessionEventWire::Prompt(_))).await;
    until(&mut o, |e| matches!(e, SessionEventWire::Prompt(_))).await;

    // Observer: any answer to a Confirm is refused, and the prompt stays.
    o.send_from_self(SessionCommand::Answer {
        prompt_id: pid,
        value: Some("y".into()),
    })
    .await
    .unwrap();
    let o_id = o.client_id();
    let seen = until(&mut o, |e| {
        matches!(e, SessionEventWire::Refused { client, .. } if *client == o_id)
    })
    .await;
    assert!(matches!(
        &seen.last().unwrap().event,
        SessionEventWire::Refused { client, command, reason }
            if *client == o.client_id() && command == "answer" && reason.contains("observe")
    ), "{seen:#?}");
    assert!(!seen
        .iter()
        .any(|e| matches!(e.event, SessionEventWire::PromptResolved { .. })));

    // Non-owner Mirror: "always" is refused (cannot arm the latch).
    b.send_from_self(SessionCommand::Answer {
        prompt_id: pid,
        value: Some("always".into()),
    })
    .await
    .unwrap();
    let b_id = b.client_id();
    let seen = until(&mut b, |e| {
        matches!(e, SessionEventWire::Refused { client, .. } if *client == b_id)
    })
    .await;
    assert!(matches!(
        &seen.last().unwrap().event,
        SessionEventWire::Refused { client, command, reason }
            if *client == b.client_id() && command == "answer" && reason.contains("input owner")
    ), "{seen:#?}");
    assert!(!seen
        .iter()
        .any(|e| matches!(e.event, SessionEventWire::PromptResolved { .. })));

    // Owner: "always" is accepted, the call runs, and every client — the
    // observer included — is told the latch is armed and for whom.
    a.send_from_self(SessionCommand::Answer {
        prompt_id: pid,
        value: Some("always".into()),
    })
    .await
    .unwrap();
    let armed = |e: &SessionEventWire| {
        matches!(e, SessionEventWire::SystemNotice(n)
            if n.contains("Allow all") && n.contains("jev") && n.contains("/confirms reset"))
    };
    until(&mut o, armed).await;
    until(&mut b, armed).await;
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    assert!(seen.iter().any(|e| armed(&e.event)), "{seen:#?}");
    assert_eq!(
        tool_results(&last_conversation(&seen)),
        vec!["allowed".to_string()]
    );
    end(&mut a).await;
}

/// prA N3: approving a call whose input an extension rewrote is recorded
/// for every attached client (the tool card shows the model's original
/// input), and the approved call runs the rewritten input.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn approving_rewritten_input_is_announced_to_every_client() {
    let _h = Home::new();
    let (url, _) = stub_seq(&[SSE_PROMPT_TOOL_USE, SSE_HI]).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = rewritten_confirm_host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let (mut a, _) = attach(&handle, ClientKind::Tui, AttachMode::Mirror).await;
    let (mut o, _) = attach(&handle, ClientKind::Attach, AttachMode::Observe).await;
    a.send_from_self(submit("go")).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Prompt(_))).await;
    let SessionEventWire::Prompt(pr) = &seen.last().unwrap().event else {
        unreachable!()
    };
    assert!(pr.prompt.contains("Input rewritten by: rewriter"), "{}", pr.prompt);
    assert!(pr.prompt.contains("echo rewritten"), "{}", pr.prompt);
    // prA I-R3-1: injected credentials are redacted in the body every
    // client gets — the observer's live copy and a later attach's replay.
    const SECRETS: [&str; 3] = ["SECRET-BEARER-123", "SECRET-KEY-456", "SECRET-FIELD-789"];
    let no_secrets = |body: &str| {
        for s in SECRETS {
            assert!(!body.contains(s), "{s} leaked:\n{body}");
        }
        assert!(body.contains("[REDACTED]"), "{body}");
    };
    no_secrets(&pr.prompt);
    let seen_o = until(&mut o, |e| matches!(e, SessionEventWire::Prompt(_))).await;
    let SessionEventWire::Prompt(observed) = &seen_o.last().unwrap().event else {
        unreachable!()
    };
    no_secrets(&observed.prompt);
    let (late, snap) = attach(&handle, ClientKind::Attach, AttachMode::Observe).await;
    let replayed = snap
        .pending_prompts
        .iter()
        .find(|p| p.id == pr.id)
        .expect("pending prompt replayed on attach");
    no_secrets(&replayed.prompt);
    drop(late);
    a.send_from_self(SessionCommand::Answer {
        prompt_id: pr.id,
        value: Some("y".into()),
    })
    .await
    .unwrap();
    let marked = |e: &SessionEventWire| {
        matches!(e, SessionEventWire::SystemNotice(n)
            if n.contains("rewritten by rewriter") && n.contains("bash"))
    };
    until(&mut o, marked).await;
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    assert!(seen.iter().any(|e| marked(&e.event)), "{seen:#?}");
    assert_eq!(
        tool_results(&last_conversation(&seen)),
        vec!["ran:echo rewritten".to_string()]
    );
    end(&mut a).await;
}

/// A plain (non-always) answer from a non-owner Mirror is still accepted:
/// only arming the latch is owner-only.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn non_owner_mirror_may_answer_a_confirm_once() {
    let _h = Home::new();
    let (url, _) = stub_seq(&[SSE_PROMPT_TOOL_USE, SSE_HI]).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = confirm_host().await;
    let handle = host.create_session(cfg()).await.unwrap();

    let (mut a, _) = attach(&handle, ClientKind::Tui, AttachMode::Mirror).await;
    let (mut b, _) = attach(&handle, ClientKind::Attach, AttachMode::Mirror).await;
    a.send_from_self(submit("go")).await.unwrap();
    let seen = until(&mut b, |e| matches!(e, SessionEventWire::Prompt(_))).await;
    let pid = prompt_id(seen.last().unwrap()).unwrap();
    b.send_from_self(SessionCommand::Answer {
        prompt_id: pid,
        value: Some("y".into()),
    })
    .await
    .unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    assert!(!seen.iter().any(|e| matches!(
        &e.event,
        SessionEventWire::SystemNotice(n) if n.contains("Allow all")
    )));
    assert_eq!(
        tool_results(&last_conversation(&seen)),
        vec!["allowed".to_string()]
    );
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

    let (a, _) = attach(&handle, ClientKind::Tui, AttachMode::Mirror).await;
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
/// stream is cancelled (history kept + restart marker), the reply lands on
/// `CHECKPOINT_QUERY_ID`, and the session is still alive.
#[cfg(feature = "command-exec")]
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

#[cfg(feature = "command-exec")]
fn marker_alive() -> bool {
    std::process::Command::new("pgrep")
        .args(["-f", "sleep 271828"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(feature = "command-exec")] // drives the bash / shell_* tools
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
    assert!(seen
        .iter()
        .any(|e| matches!(&e.event, SessionEventWire::Aborted { .. })));
    // The checkpointed turn is kept as real history: the completed
    // `shell_start` round, the partial second-round reply, then the
    // restart marker. (The old recap dropped the tool round entirely.)
    let conv = last_conversation(&seen);
    assert!(conv.abort_context.is_none());
    let msgs = &conv.api_messages;
    let roles: Vec<&str> = msgs.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, ["user", "assistant", "user", "assistant", "user"], "{msgs:#?}");
    assert_eq!(msgs[1]["content"][0]["name"], "shell_start");
    assert_eq!(msgs[2]["content"][0]["type"], "tool_result");
    assert_eq!(msgs[3]["content"], serde_json::json!([{"type": "text", "text": "hi"}]));
    assert_eq!(
        msgs[4]["content"],
        agent_engine::engine::interrupt::InterruptReason::Restart.marker()
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
    assert!(saved.abort_context.is_none());
    assert_eq!(saved.api_messages, conv.api_messages, "journal == adopted history");
    end(&mut a).await;
}

/// M1: `Checkpoint` from a non-owner (an `--observe` mirror) is `Refused`
/// with no side effect — the owner's turn keeps streaming. The host
/// (`from: None`) still checkpoints.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn checkpoint_is_owner_only_from_clients() {
    let _h = Home::new();
    let (url, _) = stub(SSE_HI, true).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let (mut a, _) = attach(&handle, ClientKind::Tui, AttachMode::Mirror).await;
    let (mut b, _) = attach(&handle, ClientKind::Attach, AttachMode::Observe).await;

    a.send_from_self(submit("hello")).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::TurnStarted { .. })).await;

    b.send_from_self(SessionCommand::Checkpoint {
        reason: CheckpointReason::HostRequest,
    })
    .await
    .unwrap();
    let seen = until(&mut b, |e| matches!(e, SessionEventWire::Refused { .. })).await;
    match &seen.last().unwrap().event {
        SessionEventWire::Refused { client, command, .. } => {
            assert_eq!(*client, b.client_id());
            assert_eq!(command, "checkpoint");
        }
        _ => unreachable!(),
    }
    assert!(!seen.iter().any(|e| matches!(
        e.event,
        SessionEventWire::Aborted { .. } | SessionEventWire::QueryResult { id: CHECKPOINT_QUERY_ID, .. }
    )));
    b.send_from_self(SessionCommand::Query {
        id: 3,
        query: SessionQuery::Status,
    })
    .await
    .unwrap();
    let seen = until(&mut b, |e| matches!(e, SessionEventWire::QueryResult { id: 3, .. })).await;
    match &seen.last().unwrap().event {
        SessionEventWire::QueryResult { value, .. } => {
            assert_eq!(value["streaming"], true, "turn still running: {value}")
        }
        _ => unreachable!(),
    }

    // Host-originated checkpoint lands.
    handle
        .send(SessionCommand::Checkpoint {
            reason: CheckpointReason::HostRequest,
        })
        .await
        .unwrap();
    let seen = until(&mut a, |e| {
        matches!(e, SessionEventWire::QueryResult { id, .. } if *id == CHECKPOINT_QUERY_ID)
    })
    .await;
    assert!(seen
        .iter()
        .any(|e| matches!(e.event, SessionEventWire::Aborted { .. })));
    end(&mut a).await;
}
