//! `/sessions` through the headless harness: opens idle and mid-stream (via
//! the real slash paths), own session gets no close, close goes through the
//! confirm popup before any daemon request is emitted, daemon-down and
//! in-process fallbacks, esc closes and pops the stack.

use agent_engine::session::{SessionId, SessionLifecycle, SessionMeta};
use agent_tui::tui::testing::{sessions_test_now, TestHarness};
use crossterm::event::{KeyCode, KeyModifiers};

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

fn daemon_list(h: &TestHarness) -> Vec<SessionMeta> {
    let mut own = meta(&h.own_session_id(), Some("ember"));
    own.clients = 1;
    own.busy = true;
    own.state_since = Some(sessions_test_now() - chrono::Duration::seconds(65));
    let mut idle = meta("20261025-090200-49f6", Some("axon"));
    idle.state_since = Some(sessions_test_now() - chrono::Duration::minutes(4));
    idle.park_due = Some(sessions_test_now() + chrono::Duration::seconds(42));
    let mut parked = meta("20261024-181829-08e3", Some("kite"));
    parked.lifecycle = SessionLifecycle::Parked;
    parked.state_since = Some(sessions_test_now() - chrono::Duration::minutes(12));
    parked.evict_due = Some(sessions_test_now() + chrono::Duration::minutes(48));
    parked.pinned = false;
    vec![own, idle, parked]
}

fn key(h: &mut TestHarness, c: KeyCode) {
    h.key(c, KeyModifiers::empty());
}

#[test]
fn slash_sessions_opens_the_modal_idle() {
    let mut h = TestHarness::boot_with_size(120, 30);
    h.type_str("/sessions").key(KeyCode::Enter, KeyModifiers::empty());
    h.drive_slash_commands();
    assert!(h.sessions_open());
    assert_eq!(h.top_pane_name(), "sessions");
    let frame = h.snapshot();
    assert!(frame.contains("Sessions"), "{frame}");
    // Harness = in-process transport: only this session + the notice.
    assert!(frame.contains("no daemon (in-process)"), "{frame}");
    // Old text dump is gone: nothing pushed into the transcript.
    assert!(!h.main_transcript_text().contains("session(s):"));
    key(&mut h, KeyCode::Esc);
    assert!(!h.sessions_open());
    assert_eq!(h.modal_stack_depth(), 0);
}

#[test]
fn slash_sessions_opens_mid_stream() {
    let mut h = TestHarness::boot_with_size(120, 30);
    h.set_streaming(true);
    h.type_str("/sessions").key(KeyCode::Enter, KeyModifiers::empty());
    h.drive_streaming_input();
    let actions = h.take_actions();
    assert!(
        !actions.iter().any(|a| a.starts_with("streaming-refused")),
        "{actions:?}"
    );
    assert!(h.sessions_open(), "{actions:?}");
    let frame = h.snapshot();
    // Own row shows the live busy flag.
    assert!(frame.contains("streaming"), "{frame}");
    assert!(h.is_streaming(), "opening the modal never touches the turn");
}

#[test]
fn daemon_rows_render_per_state() {
    let mut h = TestHarness::boot_with_size(120, 30);
    h.open_sessions_with(Some("/nonexistent/daemon.sock".into()));
    assert!(h.snapshot().contains("asking the daemon"));
    let list = daemon_list(&h);
    h.sessions_feed_live(Ok(list));
    let frame = h.snapshot();
    for want in ["LIVE", "PARKED", "streaming 1m", "idle 4m · parks in 42s", "parked 12m · evicts in 48m", "you"] {
        assert!(frame.contains(want), "missing {want:?}:\n{frame}");
    }
}

#[test]
fn own_session_gets_no_close() {
    let mut h = TestHarness::boot_with_size(120, 30);
    h.open_sessions_with(Some("/nonexistent/daemon.sock".into()));
    let list = daemon_list(&h);
    h.sessions_feed_live(Ok(list));
    // Selection starts on our own session.
    assert!(!h.snapshot().contains("x close"));
    key(&mut h, KeyCode::Char('x'));
    assert!(!h.sessions_confirm_open());
    assert!(h.take_actions().iter().all(|a| !a.starts_with("sessions-close")));
    assert!(h.sessions_notice().unwrap().contains("this session"));
}

#[test]
fn close_requires_confirm_before_any_daemon_request() {
    let mut h = TestHarness::boot_with_size(120, 30);
    h.open_sessions_with(Some("/nonexistent/daemon.sock".into()));
    let list = daemon_list(&h);
    h.sessions_feed_live(Ok(list));
    key(&mut h, KeyCode::Down); // → axon (idle)
    assert!(h.snapshot().contains("x close"));
    key(&mut h, KeyCode::Char('x'));
    assert!(h.sessions_confirm_open());
    assert!(h.snapshot().contains("close axon?"));
    assert!(
        h.take_actions().iter().all(|a| !a.starts_with("sessions-")),
        "no daemon request before confirm"
    );
    key(&mut h, KeyCode::Char('n'));
    assert!(!h.sessions_confirm_open());
    assert!(h.take_actions().is_empty());
    key(&mut h, KeyCode::Char('x'));
    key(&mut h, KeyCode::Char('y'));
    assert_eq!(h.take_actions(), vec!["sessions-close:20261025-090200-49f6".to_string()]);
    // A daemon refusal lands inline.
    h.sessions_mark_pending("20261025-090200-49f6");
    h.sessions_feed_op(Err("session axon has 1 client attached".into()));
    let frame = h.snapshot();
    assert!(frame.contains("axon not closed: session axon has 1 client attached"), "{frame}");
    assert!(h.sessions_open(), "a refusal keeps the modal open");
}

#[test]
fn pin_emits_a_daemon_request() {
    let mut h = TestHarness::boot_with_size(120, 30);
    h.open_sessions_with(Some("/nonexistent/daemon.sock".into()));
    let list = daemon_list(&h);
    h.sessions_feed_live(Ok(list));
    key(&mut h, KeyCode::End); // → kite (parked)
    key(&mut h, KeyCode::Char('p'));
    assert_eq!(h.take_actions(), vec!["sessions-pin:20261024-181829-08e3:true".to_string()]);
}

#[test]
fn daemon_down_falls_back_to_this_session() {
    let mut h = TestHarness::boot_with_size(120, 30);
    h.open_sessions_with(Some("/nonexistent/daemon.sock".into()));
    let list = daemon_list(&h);
    h.sessions_feed_live(Ok(list));
    h.sessions_feed_live(Err("daemon unreachable: connection refused".into()));
    let frame = h.snapshot();
    assert!(frame.contains("daemon unreachable"), "{frame}");
    assert!(!frame.contains("axon"), "{frame}");
    assert!(frame.contains("Live 1"), "{frame}");
}

#[test]
fn renders_at_common_widths_without_panic() {
    for (w, hgt) in [(80u16, 24u16), (120, 30), (160, 40)] {
        let mut h = TestHarness::boot_with_size(w, hgt);
        h.open_sessions_with(Some("/nonexistent/daemon.sock".into()));
        let list = daemon_list(&h);
        h.sessions_feed_live(Ok(list));
        let frame = h.snapshot();
        assert!(frame.contains("Sessions"), "{w}: {frame}");
        assert!(frame.contains("parks in 42s"), "{w}: {frame}");
        key(&mut h, KeyCode::Tab);
        let frame = h.snapshot();
        assert!(frame.contains("loading saved sessions") || frame.contains("Past"), "{w}: {frame}");
    }
}

#[test]
fn in_process_offers_no_pin_or_close() {
    let mut h = TestHarness::boot_with_size(120, 30);
    h.open_sessions_with(None);
    let frame = h.snapshot();
    assert!(!frame.contains("p pin"), "{frame}");
    assert!(!frame.contains("x close"), "{frame}");
    key(&mut h, KeyCode::Char('p'));
    key(&mut h, KeyCode::Char('x'));
    assert!(h.take_actions().is_empty());
    assert!(h.sessions_notice().unwrap().contains("no daemon"));
}
