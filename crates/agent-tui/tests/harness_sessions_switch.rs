//! `/sessions` switching through the headless harness: Enter on a Live row
//! swaps this TUI onto that session via the production
//! `sessions::switch::perform` (a scripted daemon stands in for the socket).
//! A→B→A keeps each transcript its own; nothing of A (subagent rows, queued
//! steer, streaming state, prompts) leaks into B; the old session is only
//! detached — never cancelled — and only after the new attach worked; a
//! failed attach leaves you where you were; in-process refuses; Past's
//! Continue creates a session or, if its journal is already live, attaches.

use agent_engine::session::{SessionEventWire, SessionId, SessionLifecycle, SessionMeta};
use agent_engine::{AgentEvent, StreamEvent};
use agent_tui::tui::testing::fake_daemon::{FakeDaemon, FakeSession};
use agent_tui::tui::testing::{sessions_test_now, TestHarness};
use crossterm::event::{KeyCode, KeyModifiers};

const A: &str = "20261025-090000-aaaa";
const B: &str = "20261025-090100-bbbb";

fn meta(id: &str, name: Option<&str>) -> SessionMeta {
    SessionMeta {
        id: SessionId(id.to_string()),
        name: name.map(str::to_string),
        model: "anthropic/claude-opus-5-5".into(),
        cwd: Some("/home/u".into()),
        created_at: sessions_test_now() - chrono::Duration::hours(1),
        continued: false,
        continue_info: None,
        host_pid: 7,
        lifecycle: SessionLifecycle::Live,
        clients: 0,
        input_owner: None,
        awaiting_input: 0,
        roster: Vec::new(),
        client_input: None,
        journal_id: id.to_string(),
        locked_by: None,
        pinned: false,
        busy: false,
        state_since: None,
        park_due: None,
        evict_due: None,
        title: None,
        cost_usd: 0.0,
        messages: 0,
    }
}

fn setup() -> (TestHarness, FakeDaemon) {
    let mut h = TestHarness::boot_with_size(120, 30);
    let d = h.use_fake_daemon();
    d.add_live(FakeSession::new(A, Some("ember"), "hello from A", "answer for A"));
    d.add_live(FakeSession::new(B, Some("kite"), "hello from B", "answer for B"));
    (h, d)
}

/// Open the modal in daemon mode with the daemon's current list, select
/// `id`, press Enter (the harness drains the switch like the run loop).
fn switch_to(h: &mut TestHarness, d: &FakeDaemon, id: &str) {
    h.open_sessions_with(Some("/nonexistent/daemon.sock".into()));
    let own = h.own_session_id();
    let mut rows: Vec<SessionMeta> = d.live_ids().iter().map(|i| meta(i, None)).collect();
    if !rows.iter().any(|m| m.id.0 == own) {
        rows.push(meta(&own, None));
    }
    h.sessions_feed_live(Ok(rows));
    h.sessions_select(id);
    h.key(KeyCode::Enter, KeyModifiers::empty());
}

#[test]
fn switch_a_b_a_keeps_each_transcript() {
    let (mut h, d) = setup();
    switch_to(&mut h, &d, A);
    assert!(!h.sessions_open(), "the modal closes after a switch");
    assert_eq!(h.own_session_id(), A);
    let t = h.main_transcript_text();
    assert!(t.contains("hello from A") && t.contains("answer for A"), "{t}");
    assert!(t.contains("switched to ember (aaaa)"), "{t}");
    assert!(t.contains("keeps running"), "{t}");
    assert_eq!(h.session_name().as_deref(), Some("ember"));

    switch_to(&mut h, &d, B);
    assert_eq!(h.own_session_id(), B);
    let t = h.main_transcript_text();
    assert!(t.contains("hello from B"), "{t}");
    assert!(!t.contains("hello from A"), "A's transcript leaked into B: {t}");
    assert!(t.contains("switched to kite (bbbb) — ember (aaaa) keeps running"), "{t}");
    // A was detached, never cancelled or ended.
    let sent = d.sent_to(A);
    assert!(sent.iter().any(|c| c.starts_with("Detach")), "{sent:?}");
    assert!(!sent.iter().any(|c| c.starts_with("Cancel") || c.starts_with("End")), "{sent:?}");

    switch_to(&mut h, &d, A);
    assert_eq!(h.own_session_id(), A);
    let t = h.main_transcript_text();
    assert!(t.contains("hello from A") && !t.contains("hello from B"), "{t}");
    assert_eq!(d.connects(), vec![format!("attach:{A}"), format!("attach:{B}"), format!("attach:{A}")]);
    let frame = h.snapshot();
    assert!(frame.contains("ember"), "header shows the new session: {frame}");
}

#[test]
fn switching_away_mid_stream_leaves_a_streaming_and_leaks_nothing() {
    let (mut h, d) = setup();
    switch_to(&mut h, &d, A);
    // A is mid-turn with a subagent running and a queued steer.
    h.feed_event(SessionEventWire::TurnStarted {
        trigger: agent_engine::session::TurnTrigger::User,
        turn_baseline: 2,
        user_text: Some("go".into()),
    });
    h.feed_event(SessionEventWire::Stream(StreamEvent::Agent(AgentEvent::SubagentStart {
        subagent_id: 7,
        agent_name: "digger".into(),
        task_preview: "dig".into(),
    })));
    h.feed_event(SessionEventWire::Steered { text: "also check tests".into(), delivered: false });
    assert!(h.is_streaming());
    assert_eq!(h.subagent_count(), 1);
    assert_eq!(h.queued_message().as_deref(), Some("also check tests"));

    // /sessions opens mid-stream; Enter switches (daemon mode allows it).
    switch_to(&mut h, &d, B);
    assert_eq!(h.own_session_id(), B);
    assert!(!h.is_streaming(), "B is idle; A's turn does not render here");
    assert_eq!(h.subagent_count(), 0, "A's subagent rows leaked into B");
    assert_eq!(h.queued_message(), None, "A's queued steer leaked into B");
    let t = h.main_transcript_text();
    assert!(!t.contains("digger") && !t.contains("also check tests"), "{t}");
    // A only got a Detach: its turn keeps running on the daemon.
    let sent = d.sent_to(A);
    assert!(sent.iter().any(|c| c.starts_with("Detach")), "{sent:?}");
    assert!(!sent.iter().any(|c| c.starts_with("Cancel") || c.starts_with("End")), "{sent:?}");
}

#[test]
fn switching_onto_a_streaming_session_shows_it_streaming() {
    let (mut h, d) = setup();
    let mut busy = FakeSession::new("20261025-090200-cccc", Some("axon"), "long task", "partial");
    busy.streaming = true;
    d.add_live(busy);
    switch_to(&mut h, &d, "20261025-090200-cccc");
    assert!(h.is_streaming());
}

#[test]
fn failed_attach_keeps_you_where_you_were() {
    let (mut h, d) = setup();
    switch_to(&mut h, &d, A);
    let before = h.main_transcript_text();
    // A row the daemon no longer knows (closed between poll and Enter).
    h.open_sessions_with(Some("/nonexistent/daemon.sock".into()));
    h.sessions_feed_live(Ok(vec![meta(A, Some("ember")), meta("20261025-gone-dead", None)]));
    h.sessions_select("20261025-gone-dead");
    h.key(KeyCode::Enter, KeyModifiers::empty());
    assert_eq!(h.own_session_id(), A, "still on A");
    assert!(h.sessions_open(), "the modal stays open with the reason");
    let notice = h.sessions_notice().unwrap_or_default();
    assert!(notice.contains("switch failed") && notice.contains("unknown session"), "{notice}");
    assert_eq!(h.main_transcript_text(), before, "A's screen is untouched");
    assert!(d.sent_to(A).iter().all(|c| !c.starts_with("Detach")), "A was not detached");
}

#[test]
fn in_process_refuses_to_switch() {
    let mut h = TestHarness::boot_with_size(120, 30);
    let own = h.own_session_id();
    // In-process: the Live tab offers no switch at all…
    h.open_sessions_with(None);
    h.key(KeyCode::Enter, KeyModifiers::empty());
    assert_eq!(h.own_session_id(), own);
    // …and Continue mid-turn is refused (it would cancel the turn).
    h.set_streaming(true);
    h.request_continue("20261020-000000-past");
    assert_eq!(h.own_session_id(), own);
    assert!(h.sessions_open());
    assert!(h.sessions_notice().unwrap_or_default().contains("would cancel this turn"));
}

#[test]
fn continue_from_a_past_journal_creates_and_switches() {
    let (mut h, d) = setup();
    d.add_past(FakeSession::new("20261020-000000-past", Some("old"), "an old question", "an old answer"));
    h.open_sessions_with(Some("/nonexistent/daemon.sock".into()));
    h.request_continue("20261020-000000-past");
    assert_eq!(h.own_session_id(), "cont-20261020-000000-past");
    let t = h.main_transcript_text();
    assert!(t.contains("an old question"), "{t}");
    assert!(t.contains("switched to old"), "{t}");
    assert!(d.live_ids().contains(&"cont-20261020-000000-past".to_string()));
}

#[test]
fn continue_on_a_journal_that_is_already_live_attaches_to_it() {
    let (mut h, d) = setup();
    h.open_sessions_with(Some("/nonexistent/daemon.sock".into()));
    // B's journal is live (someone continued it meanwhile).
    h.request_continue(B);
    assert_eq!(h.own_session_id(), B, "attached to the live one, no second session");
    assert_eq!(d.live_ids().len(), 2);
    let t = h.main_transcript_text();
    assert!(t.contains("(already live)"), "{t}");
}

#[test]
fn enter_on_own_row_does_nothing() {
    let (mut h, d) = setup();
    switch_to(&mut h, &d, A);
    h.open_sessions_with(Some("/nonexistent/daemon.sock".into()));
    h.sessions_feed_live(Ok(vec![meta(A, Some("ember")), meta(B, Some("kite"))]));
    h.sessions_select(A);
    assert!(!h.snapshot().contains("enter switch"));
    h.key(KeyCode::Enter, KeyModifiers::empty());
    assert_eq!(h.own_session_id(), A);
    assert_eq!(d.connects().len(), 1);
    h.sessions_select(B);
    assert!(h.snapshot().contains("enter switch"));
}

#[test]
fn continue_runs_in_the_journal_folder_or_falls_back() {
    let (mut h, d) = setup();
    let dir = tempfile::TempDir::new().unwrap();
    d.add_past(FakeSession::new("20261020-000000-here", Some("here"), "q", "a"));
    d.add_past(FakeSession::new("20261020-000000-gone", Some("gone"), "q", "a"));
    // The journal's folder exists: the session is created there.
    h.open_sessions_with(Some("/nonexistent/daemon.sock".into()));
    h.request_continue_in("20261020-000000-here", Some(dir.path().to_path_buf()));
    assert_eq!(h.own_session_id(), "cont-20261020-000000-here");
    assert_eq!(
        d.connects().last().unwrap(),
        &format!("continue:20261020-000000-here@{}", dir.path().display())
    );
    let t = h.main_transcript_text();
    assert!(!t.contains("folder gone"), "{t}");
    let last = dir.path().file_name().unwrap().to_string_lossy().to_string();
    assert!(h.header_folder().contains(&last), "header shows the session's folder: {}", h.header_folder());
    // Gone: falls back to this TUI's cwd and says so.
    h.open_sessions_with(Some("/nonexistent/daemon.sock".into()));
    h.request_continue_in("20261020-000000-gone", Some("/nonexistent/was-here".into()));
    assert_eq!(h.own_session_id(), "cont-20261020-000000-gone");
    let here = std::env::current_dir().unwrap();
    assert_eq!(
        d.connects().last().unwrap(),
        &format!("continue:20261020-000000-gone@{}", here.display())
    );
    let t = h.main_transcript_text();
    assert!(t.contains("(folder gone, using "), "{t}");
}

#[test]
fn switch_line_is_newest_and_the_owner_toast_stays_quiet() {
    let (mut h, d) = setup();
    let mut busy = FakeSession::new("20261025-090200-cccc", Some("axon"), "long task", "earlier answer");
    busy.streaming = true;
    busy.replay_text = Some("partial reply so far".into());
    d.add_live(busy);
    switch_to(&mut h, &d, "20261025-090200-cccc");
    let t = h.main_transcript_text();
    let partial = t.find("partial reply so far").expect("replayed partial turn");
    let line = t.find("switched to axon").expect("switched line");
    assert!(line > partial, "switched line must come after the replay: {t}");
    assert!(t.trim_end().ends_with("keeps running"), "newest line: {t}");
    // The actor's InputOwnerChanged right after Attach: no toast on a switch.
    let toasts = h.toast_count();
    h.feed_event(SessionEventWire::InputOwnerChanged {
        from: None,
        to: Some(agent_engine::session::ClientId(1)),
        reason: agent_engine::session::OwnerChangeReason::Attach,
    });
    assert_eq!(h.toast_count(), toasts, "no 'you own input' toast after a switch");
    // A later real ownership change still toasts.
    h.feed_event(SessionEventWire::InputOwnerChanged {
        from: Some(agent_engine::session::ClientId(1)),
        to: Some(agent_engine::session::ClientId(2)),
        reason: agent_engine::session::OwnerChangeReason::Takeover,
    });
    h.feed_event(SessionEventWire::InputOwnerChanged {
        from: Some(agent_engine::session::ClientId(2)),
        to: Some(agent_engine::session::ClientId(1)),
        reason: agent_engine::session::OwnerChangeReason::Takeover,
    });
    assert!(h.snapshot().contains("you own input"), "{}", h.snapshot());
}

#[test]
#[ignore = "manual: prints the screen after a switch"]
fn dump_after_switch() {
    let (mut h, d) = setup();
    switch_to(&mut h, &d, A);
    switch_to(&mut h, &d, B);
    println!("{}", h.snapshot());
}
