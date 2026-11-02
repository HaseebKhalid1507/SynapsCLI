//! The child share (child spend) is snapshot-only, like the keep-warm
//! labels: it survives a save and load in both modes and loads clamped
//! under the totals it is part of. (That it is never journaled, and that
//! the 0.10.1 reader still parses every record, is pinned in
//! `snapshot_only_fields`.)
use super::*;

fn session_with_share() -> Session {
    let mut s = Session::new("m", "medium", None);
    s.api_messages.push(std::sync::Arc::new(
        serde_json::json!({"role": "user", "content": "q"}),
    ));
    s.session_cost = 1.5;
    s.total_input_tokens = 100;
    s.total_output_tokens = 10;
    s.child_cost = 0.5;
    s.child_tokens = KeepwarmTokens {
        input: 40,
        output: 4,
        cache_read: 300,
        cache_creation: 7,
    };
    s
}

#[test]
fn the_child_share_survives_save_and_load() {
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path().join("sessions");
    for persistence in [SessionPersistence::Json, SessionPersistence::Journal] {
        let s = session_with_share();
        save_session_in_dir(&dir, &s, persistence).unwrap();
        let loaded = load_session_in_dir(&dir, &s.id).unwrap();
        assert_eq!(loaded.child_cost, 0.5, "{persistence:?}");
        assert_eq!(loaded.child_tokens, s.child_tokens, "{persistence:?}");
        assert_eq!(loaded.session_cost, 1.5, "{persistence:?}");
    }
}

#[test]
fn a_session_without_a_share_saves_no_child_keys() {
    let mut s = Session::new("m", "medium", None);
    s.api_messages.push(std::sync::Arc::new(
        serde_json::json!({"role": "user", "content": "q"}),
    ));
    let json = snapshot_json(&s, None).unwrap();
    assert!(!json.contains("child_cost") && !json.contains("child_tokens"), "{json}");
}

#[test]
fn a_stale_child_share_loads_clamped_under_the_totals() {
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path().join("sessions");
    let mut s = Session::new("m", "medium", None);
    s.api_messages.push(std::sync::Arc::new(
        serde_json::json!({"role": "user", "content": "q"}),
    ));
    s.session_cost = 0.25;
    s.total_input_tokens = 3;
    s.total_output_tokens = 1;
    s.child_cost = 0.75; // e.g. a stale snapshot label
    s.child_tokens.input = 10;
    s.child_tokens.output = 10;
    save_session_in_dir(&dir, &s, SessionPersistence::Json).unwrap();
    let loaded = load_session_in_dir(&dir, &s.id).unwrap();
    assert_eq!(loaded.child_cost, 0.25);
    assert_eq!(loaded.child_tokens.input, 3);
    assert_eq!(loaded.child_tokens.output, 1);

    // A negative label reads as no share.
    s.child_cost = -1.0;
    save_session_in_dir(&dir, &s, SessionPersistence::Json).unwrap();
    let loaded = load_session_in_dir(&dir, &s.id).unwrap();
    assert_eq!(loaded.child_cost, 0.0);
}
