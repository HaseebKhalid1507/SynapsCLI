//! `/settings` while a turn streams: the modal opens over the live
//! transcript, stream events keep applying behind it, Esc closes only the
//! modal, local settings apply live, session settings apply from the next
//! turn (with a notice), and rows that can't change mid-turn are refused.
//!
//! Every test pins `SYNAPS_BASE_DIR` to a temp dir (config writes never
//! touch `~/.synaps-cli`), serialized by one file-level lock.

use agent_engine::session::wire::{
    WireLlmEvent, WireSessionEvent as W, WireSessionStreamEvent, WireStreamEvent,
};
use agent_engine::session::{SessionEventWire, TurnTrigger};
use agent_tui::tui::testing::TestHarness;
use crossterm::event::{KeyCode, KeyModifiers};
use std::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Temp `SYNAPS_BASE_DIR` for one test; restored on drop.
struct BaseDir {
    dir: tempfile::TempDir,
    old: Option<String>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl BaseDir {
    fn new() -> Self {
        let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::TempDir::new().expect("tempdir");
        let old = std::env::var("SYNAPS_BASE_DIR").ok();
        std::env::set_var("SYNAPS_BASE_DIR", dir.path());
        std::fs::write(dir.path().join("config"), "").expect("write config");
        Self { dir, old, _lock: lock }
    }

    fn config(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("config")).unwrap_or_default()
    }
}

impl Drop for BaseDir {
    fn drop(&mut self) {
        match &self.old {
            Some(v) => std::env::set_var("SYNAPS_BASE_DIR", v),
            None => std::env::remove_var("SYNAPS_BASE_DIR"),
        }
    }
}

fn text(t: &str) -> SessionEventWire {
    W::Stream {
        event: WireStreamEvent::Llm(WireLlmEvent::Text { text: t.into() }),
    }
    .into()
}

fn done() -> SessionEventWire {
    W::Stream {
        event: WireStreamEvent::Session(WireSessionStreamEvent::Done),
    }
    .into()
}

/// A harness with a turn in flight (the production `TurnStarted` arm).
fn streaming_harness() -> TestHarness {
    let mut h = TestHarness::boot_with_size(100, 30);
    h.feed_event(W::TurnStarted {
        turn_baseline: 1,
        trigger: TurnTrigger::User,
        user_text: Some("run sleep 20".into()),
    }
    .into());
    h.feed_event(text("working on it"));
    assert!(h.is_streaming(), "TurnStarted must start the turn");
    h
}

/// Type `/settings` mid-turn and run it through the streaming dispatch.
fn open_settings_mid_turn(h: &mut TestHarness) {
    h.type_str("/settings");
    h.key(KeyCode::Enter, KeyModifiers::NONE);
    h.drive_streaming_input();
}

#[test]
fn settings_opens_mid_stream() {
    let _base = BaseDir::new();
    let mut h = streaming_harness();
    open_settings_mid_turn(&mut h);
    let actions = h.take_actions();
    assert!(
        !actions.iter().any(|a| a.starts_with("streaming-refused:")),
        "/settings must not be refused mid-turn: {actions:?}"
    );
    assert!(
        !actions.iter().any(|a| a.starts_with("streaming-steer:")),
        "/settings must not leak into the stream as a steer: {actions:?}"
    );
    assert_eq!(h.top_pane_name(), "settings");
    assert!(
        !h.main_transcript_text().contains("can't run while streaming"),
        "{}",
        h.main_transcript_text()
    );
    assert!(h.is_streaming(), "opening settings must not end the turn");
}

#[test]
fn stream_deltas_keep_applying_behind_the_modal() {
    let _base = BaseDir::new();
    let mut h = streaming_harness();
    open_settings_mid_turn(&mut h);
    assert_eq!(h.top_pane_name(), "settings");
    h.feed_event(text(" — still streaming behind settings"));
    let transcript = h.main_transcript_text();
    assert!(
        transcript.contains("still streaming behind settings"),
        "deltas must keep applying while settings is open:\n{transcript}"
    );
    // The frame renders with the modal on top (the draw path survives).
    let frame = h.snapshot();
    assert!(frame.contains("Settings"), "modal must draw:\n{frame}");
    h.feed_event(done());
    assert!(!h.is_streaming(), "Done ends the turn behind the modal");
    assert_eq!(h.top_pane_name(), "settings", "turn end leaves the modal open");
}

#[test]
fn esc_closes_settings_without_aborting_the_turn() {
    let _base = BaseDir::new();
    let mut h = streaming_harness();
    open_settings_mid_turn(&mut h);
    assert_eq!(h.top_pane_name(), "settings");
    h.take_actions();
    h.key(KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(h.top_pane_name(), "chat", "Esc closes the modal");
    let actions = h.take_actions();
    assert!(
        !actions.iter().any(|a| a == "abort"),
        "Esc in settings must not abort the turn: {actions:?}"
    );
    assert!(h.is_streaming(), "the turn keeps running");
    // Ctrl+C inside the modal is swallowed (no quit, no abort), as for every
    // modal; it acts again once the modal is closed.
    open_settings_mid_turn(&mut h);
    h.take_actions();
    h.key(KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(!h.quit_requested(), "Ctrl+C in settings must not quit");
    assert_eq!(h.top_pane_name(), "settings");
    assert!(h.is_streaming());
}

#[test]
fn theme_applies_live_mid_turn() {
    let base = BaseDir::new();
    let mut h = streaming_harness();
    open_settings_mid_turn(&mut h);
    assert_eq!(h.top_pane_name(), "settings");
    // Draw once first: the active theme is loaded lazily from config, and
    // loading it after the write below would make "dracula" the starting
    // theme (no fade to run).
    h.snapshot();
    h.apply_setting("theme", "dracula");
    assert!(
        base.config().contains("theme = dracula"),
        "theme persisted: {}",
        base.config()
    );
    assert!(h.theme_transition_active(), "theme applies now, mid-turn");
    assert!(
        !h.main_transcript_text().contains("next turn"),
        "a local setting has no next-turn notice:\n{}",
        h.main_transcript_text()
    );
    assert!(h.is_streaming());
}

#[test]
fn model_change_mid_turn_applies_from_the_next_turn_with_a_notice() {
    let base = BaseDir::new();
    let mut h = streaming_harness();
    open_settings_mid_turn(&mut h);
    assert_eq!(h.top_pane_name(), "settings");
    let before = h.view_model();
    let target = if before == "claude-sonnet-4-6" {
        "claude-opus-4-6"
    } else {
        "claude-sonnet-4-6"
    };
    h.apply_setting("model", target);
    assert_eq!(h.view_model(), target, "the session holds the new model");
    assert!(
        base.config().contains(&format!("model = {target}")),
        "model persisted: {}",
        base.config()
    );
    let transcript = h.main_transcript_text();
    assert!(
        transcript.contains("model change applies from the next turn"),
        "mid-turn session change must say when it takes effect:\n{transcript}"
    );
    let note = h.settings_row_note();
    assert!(
        matches!(&note, Some((k, m)) if k == "model" && m.contains("next turn")),
        "the row says so too: {note:?}"
    );
    assert!(h.is_streaming(), "the running turn is untouched");
}

#[test]
fn model_change_while_idle_has_no_next_turn_notice() {
    let _base = BaseDir::new();
    let mut h = TestHarness::boot_with_size(100, 30);
    h.run_slash_command("settings", "");
    assert_eq!(h.top_pane_name(), "settings");
    let target = if h.view_model() == "claude-sonnet-4-6" {
        "claude-opus-4-6"
    } else {
        "claude-sonnet-4-6"
    };
    h.apply_setting("model", target);
    assert_eq!(h.view_model(), target);
    assert!(
        !h.main_transcript_text().contains("next turn"),
        "{}",
        h.main_transcript_text()
    );
}

#[test]
fn provider_key_is_refused_mid_turn_and_left_unchanged() {
    let base = BaseDir::new();
    let mut h = streaming_harness();
    open_settings_mid_turn(&mut h);
    // Providers category → right pane → row 0 (local URL) editor.
    h.key(KeyCode::Down, KeyModifiers::NONE);
    h.key(KeyCode::Tab, KeyModifiers::NONE);
    h.key(KeyCode::Enter, KeyModifiers::NONE);
    h.type_str("http://localhost:1234");
    h.key(KeyCode::Enter, KeyModifiers::NONE);
    let note = h.settings_row_note();
    assert!(
        matches!(&note, Some((k, m)) if k == "provider.local.url" && m.contains("turn")),
        "provider edits are refused mid-turn with a reason: {note:?}"
    );
    assert!(
        !base.config().contains("provider.local.url"),
        "nothing written: {}",
        base.config()
    );
    assert_eq!(h.top_pane_name(), "settings");
}

#[test]
fn plugin_marketplace_is_refused_mid_turn() {
    let _base = BaseDir::new();
    let mut h = streaming_harness();
    open_settings_mid_turn(&mut h);
    // Plugins is the 9th category (Model … Memory, Plugins).
    for _ in 0..8 {
        h.key(KeyCode::Down, KeyModifiers::NONE);
    }
    h.key(KeyCode::Tab, KeyModifiers::NONE);
    h.take_actions();
    h.key(KeyCode::Enter, KeyModifiers::NONE);
    let actions = h.take_actions();
    assert!(
        !actions.iter().any(|a| a == "open-plugins-marketplace"),
        "marketplace must not open mid-turn: {actions:?}"
    );
    let note = h.settings_row_note();
    assert!(
        matches!(&note, Some((k, m)) if k == "plugins" && m.contains("turn")),
        "refused with a reason: {note:?}"
    );
}

/// A tool's prompt that arrives while settings is open mid-turn goes on top
/// of the modal and takes the keys; closing it leaves settings open.
#[test]
fn a_mid_turn_prompt_lands_on_top_of_settings() {
    let _base = BaseDir::new();
    let mut h = streaming_harness();
    open_settings_mid_turn(&mut h);
    let _rx = h.activate_confirm_prompt("Confirm tool activation", "activate: shell");
    assert_eq!(h.top_pane_name(), "secret-prompt", "the prompt owns input");
    h.key(KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(h.top_pane_name(), "settings", "settings resumes under it");
    assert!(h.is_streaming());
}
