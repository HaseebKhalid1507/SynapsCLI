//! `/sessions` Past tab through the headless harness: the real slash path,
//! tab to Past, a catalog loaded from a fixture sessions dir, live search,
//! facet selection, enter → `pending_switch = Continue`, rename and copy
//! emitted as actions (never run against the real disk / clipboard here).

use agent_core::session::Session;
use agent_core::session_journal::{save_session_in_dir, SessionPersistence};
use agent_tui::tui::testing::{sessions_test_now, TestHarness};
use crossterm::event::{KeyCode, KeyModifiers};
use std::sync::Arc;

fn key(h: &mut TestHarness, c: KeyCode) {
    h.key(c, KeyModifiers::empty());
}

struct Fixture {
    _tmp: tempfile::TempDir,
    dir: std::path::PathBuf,
    index: std::path::PathBuf,
}

/// `(id, title, model, hours ago, messages, cwd)`.
fn fixture(rows: &[(&str, &str, &str, i64, usize, &str)]) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap().join("sessions");
    std::fs::create_dir_all(&dir).unwrap();
    let mut index = String::new();
    for (id, title, model, hours, n, cwd) in rows {
        let mut s = Session::new(model, "low", None);
        s.id = (*id).into();
        s.title = (*title).into();
        s.updated_at = sessions_test_now() - chrono::Duration::hours(*hours);
        s.created_at = s.updated_at - chrono::Duration::minutes(30);
        s.session_cost = *n as f64 / 10.0;
        s.api_messages = (0..*n)
            .map(|i| {
                Arc::new(if i % 2 == 0 {
                    serde_json::json!({"role":"user","content":format!("{title} prompt {i}")})
                } else {
                    serde_json::json!({"role":"assistant","content":[{"type":"text","text":"ok"}]})
                })
            })
            .collect();
        save_session_in_dir(&dir, &s, SessionPersistence::Json).unwrap();
        index.push_str(
            &serde_json::json!({"schema_version":1,"session_id":id,"event":"start",
                "timestamp":"2026-10-01T00:00:00Z","cwd":cwd})
            .to_string(),
        );
        index.push('\n');
    }
    let index_path = dir.join("index.jsonl");
    std::fs::write(&index_path, index).unwrap();
    Fixture { _tmp: tmp, dir, index: index_path }
}

fn rows() -> Vec<(&'static str, &'static str, &'static str, i64, usize, &'static str)> {
    vec![
        ("20261025-090000-08e3", "any ai events in nyc today?", "anthropic/claude-opus-5-5", 2, 58, "/home/u"),
        ("20261025-020000-ab12", "fix the header gradient", "claude-opus-4-8", 9, 210, "/home/u/Projects/SynapsCLI"),
        ("20261024-080000-9f01", "midterm petri nets", "anthropic/claude-opus-4-8", 30, 96, "/home/u"),
        ("20261020-080000-5a5a", "sandbox junk", "claude-opus-5-5", 40, 20, "/tmp/synaps-web-sandbox/a"),
    ]
}

fn open_past(h: &mut TestHarness, fx: &Fixture) {
    h.type_str("/sessions").key(KeyCode::Enter, KeyModifiers::empty());
    h.drive_slash_commands();
    assert!(h.sessions_open());
    key(h, KeyCode::Tab);
    h.sessions_feed_catalog_dir(&fx.dir, &fx.index);
}

#[test]
fn past_tab_search_facet_and_enter_continue() {
    let fx = fixture(&rows());
    let mut h = TestHarness::boot_with_size(120, 34);
    open_past(&mut h, &fx);
    let frame = h.snapshot();
    assert!(frame.contains("Past 4"), "{frame}");
    assert!(frame.contains("hiding 1 (sandbox)"), "{frame}");
    assert!(frame.contains("any ai events in nyc today?"), "{frame}");
    assert_eq!(h.sessions_past_rows().len(), 3);

    // Live search with a token.
    key(&mut h, KeyCode::Char('/'));
    h.type_str("model:4-8");
    assert_eq!(h.sessions_past_rows(), vec!["20261025-020000-ab12", "20261024-080000-9f01"]);
    let frame = h.snapshot();
    assert!(frame.contains("/ model:4-8"), "{frame}");
    key(&mut h, KeyCode::Enter); // done editing

    // Facet: Project = SynapsCLI — move the sidebar cursor onto it, toggle.
    key(&mut h, KeyCode::Left);
    let on_synaps = |frame: &str| frame.lines().any(|l| l.contains('┃') && l.contains("SynapsCLI") && !l.contains('·'));
    for _ in 0..12 {
        if on_synaps(&h.snapshot()) {
            break;
        }
        key(&mut h, KeyCode::Down);
    }
    let frame = h.snapshot();
    assert!(on_synaps(&frame), "{frame}");
    key(&mut h, KeyCode::Enter);
    assert_eq!(h.sessions_past_rows(), vec!["20261025-020000-ab12"]);
    key(&mut h, KeyCode::Right);
    key(&mut h, KeyCode::Enter);
    assert_eq!(h.sessions_pending_continue().as_deref(), Some("20261025-020000-ab12"));
    assert_eq!(
        h.sessions_pending_continue_cwd(),
        Some(Some(std::path::PathBuf::from("/home/u/Projects/SynapsCLI"))),
        "the journal's cwd travels with the switch"
    );
    assert!(h.sessions_open(), "the switch itself is the dispatch loop's (PR 4)");
}

#[test]
fn past_rename_and_copy_are_actions() {
    let fx = fixture(&rows());
    let mut h = TestHarness::boot_with_size(120, 34);
    open_past(&mut h, &fx);
    h.take_actions();
    key(&mut h, KeyCode::Char('y'));
    assert_eq!(h.take_actions(), vec!["copy:20261025-090000-08e3"]);
    assert_eq!(h.sessions_notice().as_deref(), Some("copied 20261025-090000-08e3"));

    key(&mut h, KeyCode::Char('r'));
    h.type_str("kite");
    key(&mut h, KeyCode::Enter);
    assert_eq!(h.take_actions(), vec!["sessions-rename:20261025-090000-08e3:kite"]);
    h.sessions_feed_rename(Err("session is live (pid 7, tui) — rename it there with /saveas".into()));
    let frame = h.snapshot();
    assert!(frame.contains("session is live (pid 7, tui)"), "{frame}");
    assert_eq!(h.sessions_rename_state().unwrap().0, "kite");
    // esc: popup first, modal untouched.
    key(&mut h, KeyCode::Esc);
    assert!(h.sessions_rename_state().is_none());
    assert!(h.sessions_open());
    key(&mut h, KeyCode::Esc);
    assert!(!h.sessions_open());
    assert_eq!(h.modal_stack_depth(), 0);
}

#[test]
fn past_deep_search_emits_a_start_action() {
    let fx = fixture(&rows());
    let mut h = TestHarness::boot_with_size(120, 34);
    open_past(&mut h, &fx);
    h.take_actions();
    h.key(KeyCode::Char('f'), KeyModifiers::CONTROL);
    h.type_str("petri");
    key(&mut h, KeyCode::Enter);
    let actions = h.take_actions();
    assert!(actions.len() == 1 && actions[0].starts_with("sessions-deep:"), "{actions:?}");
    let frame = h.snapshot();
    assert!(frame.contains("deep ▸ petri"), "{frame}");
    assert!(frame.contains("0/3 files"), "{frame}");
}
