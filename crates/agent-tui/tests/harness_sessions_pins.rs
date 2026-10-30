//! `/sessions` and pins that survive daemon restarts: pins that are not live
//! (waiting / failed / not restored yet) are listed on the Live tab with ⚑
//! and their state; `p` clears one. Past rows that are pinned show ⚑; `p`
//! on a Past row pins it into the daemon (or unpins a pinned one). Hints and
//! notices say a pin comes back after restarts.

use agent_core::session::Session;
use agent_core::session_journal::{save_session_in_dir, SessionPersistence};
use agent_engine::daemon::pins::PinEntry;
use agent_engine::session::{SessionId, SessionLifecycle, SessionMeta};
use agent_tui::tui::testing::fake_daemon::FakeSession;
use agent_tui::tui::testing::{sessions_test_now, TestHarness};
use crossterm::event::{KeyCode, KeyModifiers};

const SOCK: &str = "/nonexistent/daemon.sock";

fn key(h: &mut TestHarness, c: KeyCode) {
    h.key(c, KeyModifiers::empty());
}

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

fn pin(id: &str, name: Option<&str>, state: Option<&str>, reason: Option<&str>) -> PinEntry {
    PinEntry {
        journal_id: id.into(),
        name: name.map(Into::into),
        cwd: Some("/home/u/Projects/axon".into()),
        state: state.map(Into::into),
        reason: reason.map(Into::into),
        ..Default::default()
    }
}

/// Own session + one live pinned session; the book also holds two pins that
/// are not live (one waiting, one failed) and the live one.
fn open_with_pins(h: &mut TestHarness) {
    h.open_sessions_with(Some(SOCK.into()));
    let mut own = meta(&h.own_session_id(), Some("ember"));
    own.clients = 1;
    let mut live = meta("20261025-090200-49f6", Some("axon"));
    live.pinned = true;
    h.sessions_feed_live(Ok(vec![own, live]));
    h.sessions_feed_pins(vec![
        pin("20261025-090200-49f6", Some("axon"), None, None),
        pin("20261024-120000-a8b1", Some("kite"), Some("waiting"), Some("held by pid 4242, tui")),
        pin("20261023-120000-fa11", None, Some("failed"), Some("journal 20261023-120000-fa11 is missing")),
    ]);
}

#[test]
fn pins_that_are_not_live_are_listed_with_their_state_and_p_clears_one() {
    let mut h = TestHarness::boot_with_size(140, 34);
    open_with_pins(&mut h);
    let frame = h.snapshot();
    for want in [
        "PINNED \u{00b7} NOT LIVE",
        "kite",
        "waiting: held by pid 4242, tui",
        "failed: journal 20261023-120000-fa11 is missing",
        "2 pinned not live",
    ] {
        assert!(frame.contains(want), "missing {want:?}:\n{frame}");
    }
    assert_eq!(frame.matches('\u{2691}').count(), 3, "⚑ on the live pin and both offline ones:\n{frame}");
    assert!(!frame.contains("Projects/axon  \u{2691}"), "{frame}");

    // ↓ past the live rows onto the waiting pin.
    h.sessions_select("20261024-120000-a8b1");
    let frame = h.snapshot();
    let footer = frame.lines().find(|l| l.contains("tab past")).unwrap_or_default().to_string();
    assert!(footer.contains("enter continue") && footer.contains("p unpin") && !footer.contains("x close"), "hint on a not-live pin:\n{frame}");
    assert!(frame.contains("it comes back on the next daemon start"), "detail strip:\n{frame}");
    key(&mut h, KeyCode::Char('x'));
    assert!(h.sessions_notice().unwrap().contains("not live"), "no close for a pin that isn't live");
    key(&mut h, KeyCode::Char('p'));
    assert_eq!(h.take_actions(), vec!["sessions-clear-pin:20261024-120000-a8b1".to_string()]);
    h.sessions_feed_op_for("clear-pin", "20261024-120000-a8b1", Ok("unpinned".into()));
    assert_eq!(h.sessions_notice().as_deref(), Some("unpinned \u{2014} it won't come back"));

    // The next book read drops it from the list.
    h.sessions_feed_pins(vec![
        pin("20261025-090200-49f6", Some("axon"), None, None),
        pin("20261023-120000-fa11", None, Some("failed"), Some("journal 20261023-120000-fa11 is missing")),
    ]);
    let frame = h.snapshot();
    assert!(!frame.contains("kite"), "{frame}");
    assert!(frame.contains("1 pinned not live"), "{frame}");
}

#[test]
fn arrow_keys_reach_the_not_live_pins_and_enter_continues_one() {
    let mut h = TestHarness::boot_with_size(140, 34);
    open_with_pins(&mut h);
    key(&mut h, KeyCode::End);
    assert!(h.snapshot().contains("journal 20261023-120000-fa11 is missing"));
    key(&mut h, KeyCode::Up);
    // Enter continues its journal (the harness drains the switch as the run
    // loop does, so check what reached the daemon).
    let d = h.use_fake_daemon();
    d.add_past(FakeSession::new("20261024-120000-a8b1", Some("kite"), "q", "a"));
    key(&mut h, KeyCode::Enter);
    assert!(d.connects().last().unwrap().starts_with("continue:20261024-120000-a8b1@"), "{:?}", d.connects());
}

#[test]
fn pinning_a_live_session_says_it_comes_back_after_restarts() {
    let mut h = TestHarness::boot_with_size(140, 34);
    open_with_pins(&mut h);
    h.sessions_select(&h.own_session_id());
    assert!(h.snapshot().contains("p pin"));
    key(&mut h, KeyCode::Char('p'));
    let own = h.own_session_id();
    assert_eq!(h.take_actions(), vec![format!("sessions-pin:{own}:true")]);
    h.sessions_feed_op_for("pin", &own, Ok("pinned".into()));
    assert_eq!(h.sessions_notice().as_deref(), Some("pinned \u{2014} it stays loaded and comes back after restarts"));
    h.sessions_select("20261025-090200-49f6");
    assert!(h.snapshot().contains("p unpin"));
}

// ── Past ──────────────────────────────────────────────────────────────────

struct Fixture {
    _tmp: tempfile::TempDir,
    dir: std::path::PathBuf,
    index: std::path::PathBuf,
}

fn fixture(ids: &[(&str, &str, i64)]) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap().join("sessions");
    std::fs::create_dir_all(&dir).unwrap();
    let mut index = String::new();
    for (id, title, hours) in ids {
        let mut s = Session::new("claude-opus-5-5", "low", None);
        s.id = (*id).into();
        s.title = (*title).into();
        s.updated_at = sessions_test_now() - chrono::Duration::hours(*hours);
        s.created_at = s.updated_at - chrono::Duration::minutes(30);
        s.api_messages = (0..6)
            .map(|i| {
                std::sync::Arc::new(if i % 2 == 0 {
                    serde_json::json!({"role":"user","content":format!("{title} prompt {i}")})
                } else {
                    serde_json::json!({"role":"assistant","content":[{"type":"text","text":"ok"}]})
                })
            })
            .collect();
        save_session_in_dir(&dir, &s, SessionPersistence::Json).unwrap();
        index.push_str(
            &serde_json::json!({"schema_version":1,"session_id":id,"event":"start",
                "timestamp":"2026-10-01T00:00:00Z","cwd":"/home/u/Projects/kite"})
            .to_string(),
        );
        index.push('\n');
    }
    let index_path = dir.join("index.jsonl");
    std::fs::write(&index_path, index).unwrap();
    Fixture { _tmp: tmp, dir, index: index_path }
}

#[test]
fn past_rows_show_their_pin_and_p_pins_or_unpins() {
    let fx = fixture(&[("20261024-120000-a8b1", "the waiting one", 2), ("20261020-080000-5a5a", "plain old chat", 9)]);
    let mut h = TestHarness::boot_with_size(140, 34);
    open_with_pins(&mut h);
    key(&mut h, KeyCode::Tab);
    h.sessions_feed_catalog_dir(&fx.dir, &fx.index);
    assert_eq!(h.sessions_past_rows(), vec!["20261024-120000-a8b1".to_string(), "20261020-080000-5a5a".to_string()]);
    let frame = h.snapshot();
    let row = |needle: &str| frame.lines().find(|l| l.contains(needle)).unwrap_or_default().to_string();
    assert!(row("the waiting one").contains('\u{2691}'), "a pinned Past row shows ⚑:\n{frame}");
    assert!(!row("plain old chat").contains('\u{2691}'), "{frame}");

    // Selected: the pinned one → p unpins it (it won't come back).
    assert!(frame.contains("p unpin"), "{frame}");
    key(&mut h, KeyCode::Char('p'));
    assert_eq!(h.take_actions(), vec!["sessions-clear-pin:20261024-120000-a8b1".to_string()]);

    // The plain one → p continues it into the daemon, pinned.
    key(&mut h, KeyCode::Down);
    let frame = h.snapshot();
    let footer = frame.lines().find(|l| l.contains("enter continue")).unwrap_or_default().to_string();
    assert!(footer.contains("p pin") && !footer.contains("p unpin"), "{frame}");
    key(&mut h, KeyCode::Char('p'));
    assert_eq!(h.take_actions(), vec!["sessions-pin-past:20261020-080000-5a5a".to_string()]);
    h.sessions_feed_op_for("pin-past", "20261020-080000-5a5a", Ok("pinned".into()));
    assert_eq!(h.sessions_notice().as_deref(), Some("pinned \u{2014} it stays loaded and comes back after restarts"));
    assert!(h.sessions_pending_continue().is_none(), "pinning from Past never switches this TUI");
}

#[test]
fn past_p_needs_a_daemon() {
    let fx = fixture(&[("20261020-080000-5a5a", "plain old chat", 9)]);
    let mut h = TestHarness::boot_with_size(140, 34);
    h.type_str("/sessions").key(KeyCode::Enter, KeyModifiers::empty());
    h.drive_slash_commands();
    key(&mut h, KeyCode::Tab);
    h.sessions_feed_catalog_dir(&fx.dir, &fx.index);
    key(&mut h, KeyCode::Char('p'));
    assert!(h.take_actions().iter().all(|a| !a.starts_with("sessions-")));
    assert!(h.sessions_notice().unwrap().contains("no daemon"));
}
