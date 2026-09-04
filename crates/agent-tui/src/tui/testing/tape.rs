//! P6.4 — replayable interaction tapes.
//!
//! A **tape** is a `serde`-serializable sequence of [`TapeStep`]s that fully
//! scripts a headless [`TestHarness`](super::TestHarness) session:
//!
//! - [`TapeStep::Event`] — a synthetic crossterm [`Event`] (key / paste /
//!   resize / mouse), stored in the serde-friendly [`SynthEvent`] mirror
//!   because crossterm's own `Event` is not compiled with its `serde` feature
//!   in this workspace.
//! - [`TapeStep::AdvanceClockMs`] — advance the P6.2 injectable test clock by
//!   N milliseconds (the ONLY way time moves under the harness — no
//!   wall-clock).
//! - Harness-driver steps: [`TapeStep::OpenModal`],
//!   [`TapeStep::DriveSlashCommands`] (the P6.3 *bounded* async drive), and
//!   [`TapeStep::Snapshot`] (a mid-tape frame checkpoint).
//!
//! Determinism contract: every source of nondeterminism the harness can reach
//! is pinned. Time only advances through `AdvanceClockMs` (frozen P6.2 clock),
//! async only runs through the hard-bounded P6.3 slash drive, and there are no
//! detached tasks. Record on one machine, replay byte-identically on another.
//!
//! ```rust,no_run
//! use agent_tui::tui::testing::TestHarness;
//! use agent_tui::tui::testing::tape::ModalKind;
//! use crossterm::event::{KeyCode, KeyModifiers};
//!
//! let mut h = TestHarness::boot();
//! let tape = {
//!     let mut rec = h.record_tape();
//!     rec.type_str("hi").advance_clock_ms(500);
//!     rec.open_modal(ModalKind::Settings);
//!     rec.key(KeyCode::Esc, KeyModifiers::empty());
//!     rec.finish()
//! };
//! let recorded = h.snapshot();
//!
//! // Round-trip through JSON, then replay byte-identically.
//! let json = tape.to_json();
//! let tape2 = agent_tui::tui::testing::tape::Tape::from_json(&json).unwrap();
//! assert_eq!(TestHarness::replay(&tape2), recorded);
//! ```

use std::path::PathBuf;

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use serde::{Deserialize, Serialize};

use super::TestHarness;

// ───────────────────────────────────────────────────────────────────────────
// Tape schema
// ───────────────────────────────────────────────────────────────────────────

/// A recorded session: an ordered list of [`TapeStep`]s.
///
/// `#[serde(transparent)]` makes the wire form a bare JSON array of steps —
/// i.e. `Tape` *is* `Vec<TapeStep>` on disk — while the newtype lets us hang
/// methods and the fixture loader off it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(transparent)]
pub struct Tape {
    /// The ordered steps. Public so callers can inspect / hand-build tapes.
    pub steps: Vec<TapeStep>,
}

impl Tape {
    /// An empty tape.
    pub fn new() -> Self {
        Self { steps: Vec::new() }
    }

    /// Number of steps.
    pub fn len(&self) -> usize {
        self.steps.len()
    }

    /// Whether the tape has no steps.
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// Serialize to pretty JSON (the on-disk / artifact form).
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("Tape is always JSON-serializable")
    }

    /// Parse a tape from JSON, e.g. a committed fixture under
    /// `tests/fixtures/tapes/`.
    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }
}

/// One step in a [`Tape`].
///
/// Externally tagged so the JSON reads self-describingly:
/// `{"Event": …}`, `{"AdvanceClockMs": 500}`, `{"OpenModal": "Settings"}`,
/// `"DriveSlashCommands"`, `"Snapshot"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TapeStep {
    /// A synthetic input event through the production dispatch surface.
    Event(SynthEvent),
    /// Advance the frozen P6.2 test clock by this many milliseconds.
    AdvanceClockMs(u64),
    /// Harness driver: open a modal directly (bypasses async command dispatch),
    /// mirroring the `TestHarness::open_*_modal` helpers.
    OpenModal(ModalKind),
    /// Harness driver: run the P6.3 bounded async slash-command drive for every
    /// slash command recorded so far.
    DriveSlashCommands,
    /// Checkpoint: materialize a frame here (a no-op for state, but records
    /// author intent and forces line-cache maintenance mid-tape).
    Snapshot,
    /// PLAN-phase3 §5.1 layer 2: one session envelope (the serde mirror of
    /// `SessionEventWire`) through the production presentation arm
    /// (`stream_handler::handle_session_event_arm`) — the same code path
    /// under `LocalTransport` and `SocketTransport`. `Conversation` carries
    /// its digest only (no messages), as on the wire. Stored as the wire
    /// JSON (`WireSessionEvent` has no `PartialEq`).
    SessionEvent(serde_json::Value),
}

/// Which modal an [`TapeStep::OpenModal`] opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModalKind {
    /// The settings modal.
    Settings,
    /// The models modal.
    Models,
    /// The plugins modal.
    Plugins,
}

impl ModalKind {
    fn apply(self, h: &mut TestHarness) {
        match self {
            ModalKind::Settings => {
                h.open_settings_modal();
            }
            ModalKind::Models => {
                h.open_models_modal();
            }
            ModalKind::Plugins => {
                h.open_plugins_modal();
            }
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Serializable crossterm Event mirror
// ───────────────────────────────────────────────────────────────────────────

/// A serde-serializable mirror of the crossterm [`Event`] subset the harness
/// injects. crossterm 0.29 is built here WITHOUT its `serde` feature, so we
/// carry our own bounded, versioned shape instead of pulling a new dep.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SynthEvent {
    /// A key press. `mods` is the raw [`KeyModifiers`] bitset.
    Key {
        /// The key code.
        code: SynthKey,
        /// [`KeyModifiers::bits`] — 0 == no modifiers.
        mods: u8,
    },
    /// A bracketed paste.
    Paste(String),
    /// A terminal resize to `(cols, rows)`.
    Resize(u16, u16),
    /// A mouse event.
    Mouse {
        /// The kind of mouse action.
        kind: SynthMouse,
        /// Column (0-based).
        col: u16,
        /// Row (0-based).
        row: u16,
        /// [`KeyModifiers::bits`] active during the mouse action.
        mods: u8,
    },
}

/// Serializable mirror of the crossterm [`KeyCode`] subset the harness drives.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SynthKey {
    /// A character key.
    Char(char),
    /// Function key `F(n)`.
    F(u8),
    /// Enter.
    Enter,
    /// Escape.
    Esc,
    /// Backspace.
    Backspace,
    /// Tab.
    Tab,
    /// Shift-Tab.
    BackTab,
    /// Left arrow.
    Left,
    /// Right arrow.
    Right,
    /// Up arrow.
    Up,
    /// Down arrow.
    Down,
    /// Home.
    Home,
    /// End.
    End,
    /// Page Up.
    PageUp,
    /// Page Down.
    PageDown,
    /// Delete.
    Delete,
    /// Insert.
    Insert,
    /// Null (used by some paste/hand-built events).
    Null,
}

/// Serializable mirror of [`MouseEventKind`] (+ button).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SynthMouse {
    /// Button pressed.
    Down(SynthButton),
    /// Button released.
    Up(SynthButton),
    /// Drag with button held.
    Drag(SynthButton),
    /// Pointer moved, no button.
    Moved,
    /// Wheel down.
    ScrollDown,
    /// Wheel up.
    ScrollUp,
    /// Wheel left.
    ScrollLeft,
    /// Wheel right.
    ScrollRight,
}

/// Serializable mirror of [`MouseButton`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SynthButton {
    /// Left button.
    Left,
    /// Right button.
    Right,
    /// Middle button.
    Middle,
}

impl SynthEvent {
    /// Capture a live crossterm [`Event`] into its serializable mirror.
    ///
    /// Panics on event kinds the harness never records (focus in/out) — a
    /// deterministic authoring error, not a silent lossy encode.
    pub fn from_event(ev: &Event) -> Self {
        match ev {
            Event::Key(k) => SynthEvent::Key {
                code: SynthKey::from_code(k.code),
                mods: k.modifiers.bits(),
            },
            Event::Paste(s) => SynthEvent::Paste(s.clone()),
            Event::Resize(c, r) => SynthEvent::Resize(*c, *r),
            Event::Mouse(m) => SynthEvent::Mouse {
                kind: SynthMouse::from_kind(m.kind),
                col: m.column,
                row: m.row,
                mods: m.modifiers.bits(),
            },
            other => panic!(
                "SynthEvent::from_event: unrecordable event {other:?} — the tape \
                 format covers key/paste/resize/mouse only"
            ),
        }
    }

    /// Reconstruct the live crossterm [`Event`] for replay.
    pub fn to_event(&self) -> Event {
        match self {
            SynthEvent::Key { code, mods } => Event::Key(KeyEvent::new(
                code.to_code(),
                KeyModifiers::from_bits_truncate(*mods),
            )),
            SynthEvent::Paste(s) => Event::Paste(s.clone()),
            SynthEvent::Resize(c, r) => Event::Resize(*c, *r),
            SynthEvent::Mouse {
                kind,
                col,
                row,
                mods,
            } => Event::Mouse(MouseEvent {
                kind: kind.to_kind(),
                column: *col,
                row: *row,
                modifiers: KeyModifiers::from_bits_truncate(*mods),
            }),
        }
    }
}

impl SynthKey {
    fn from_code(code: KeyCode) -> Self {
        match code {
            KeyCode::Char(c) => SynthKey::Char(c),
            KeyCode::F(n) => SynthKey::F(n),
            KeyCode::Enter => SynthKey::Enter,
            KeyCode::Esc => SynthKey::Esc,
            KeyCode::Backspace => SynthKey::Backspace,
            KeyCode::Tab => SynthKey::Tab,
            KeyCode::BackTab => SynthKey::BackTab,
            KeyCode::Left => SynthKey::Left,
            KeyCode::Right => SynthKey::Right,
            KeyCode::Up => SynthKey::Up,
            KeyCode::Down => SynthKey::Down,
            KeyCode::Home => SynthKey::Home,
            KeyCode::End => SynthKey::End,
            KeyCode::PageUp => SynthKey::PageUp,
            KeyCode::PageDown => SynthKey::PageDown,
            KeyCode::Delete => SynthKey::Delete,
            KeyCode::Insert => SynthKey::Insert,
            KeyCode::Null => SynthKey::Null,
            other => panic!(
                "SynthKey::from_code: unrecordable key {other:?} — extend SynthKey \
                 if this key belongs in the tape vocabulary"
            ),
        }
    }

    fn to_code(&self) -> KeyCode {
        match self {
            SynthKey::Char(c) => KeyCode::Char(*c),
            SynthKey::F(n) => KeyCode::F(*n),
            SynthKey::Enter => KeyCode::Enter,
            SynthKey::Esc => KeyCode::Esc,
            SynthKey::Backspace => KeyCode::Backspace,
            SynthKey::Tab => KeyCode::Tab,
            SynthKey::BackTab => KeyCode::BackTab,
            SynthKey::Left => KeyCode::Left,
            SynthKey::Right => KeyCode::Right,
            SynthKey::Up => KeyCode::Up,
            SynthKey::Down => KeyCode::Down,
            SynthKey::Home => KeyCode::Home,
            SynthKey::End => KeyCode::End,
            SynthKey::PageUp => KeyCode::PageUp,
            SynthKey::PageDown => KeyCode::PageDown,
            SynthKey::Delete => KeyCode::Delete,
            SynthKey::Insert => KeyCode::Insert,
            SynthKey::Null => KeyCode::Null,
        }
    }
}

impl SynthMouse {
    fn from_kind(kind: MouseEventKind) -> Self {
        match kind {
            MouseEventKind::Down(b) => SynthMouse::Down(SynthButton::from_btn(b)),
            MouseEventKind::Up(b) => SynthMouse::Up(SynthButton::from_btn(b)),
            MouseEventKind::Drag(b) => SynthMouse::Drag(SynthButton::from_btn(b)),
            MouseEventKind::Moved => SynthMouse::Moved,
            MouseEventKind::ScrollDown => SynthMouse::ScrollDown,
            MouseEventKind::ScrollUp => SynthMouse::ScrollUp,
            MouseEventKind::ScrollLeft => SynthMouse::ScrollLeft,
            MouseEventKind::ScrollRight => SynthMouse::ScrollRight,
        }
    }

    fn to_kind(&self) -> MouseEventKind {
        match self {
            SynthMouse::Down(b) => MouseEventKind::Down(b.to_btn()),
            SynthMouse::Up(b) => MouseEventKind::Up(b.to_btn()),
            SynthMouse::Drag(b) => MouseEventKind::Drag(b.to_btn()),
            SynthMouse::Moved => MouseEventKind::Moved,
            SynthMouse::ScrollDown => MouseEventKind::ScrollDown,
            SynthMouse::ScrollUp => MouseEventKind::ScrollUp,
            SynthMouse::ScrollLeft => MouseEventKind::ScrollLeft,
            SynthMouse::ScrollRight => MouseEventKind::ScrollRight,
        }
    }
}

impl SynthButton {
    fn from_btn(b: MouseButton) -> Self {
        match b {
            MouseButton::Left => SynthButton::Left,
            MouseButton::Right => SynthButton::Right,
            MouseButton::Middle => SynthButton::Middle,
        }
    }

    fn to_btn(self) -> MouseButton {
        match self {
            SynthButton::Left => MouseButton::Left,
            SynthButton::Right => MouseButton::Right,
            SynthButton::Middle => MouseButton::Middle,
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Recording
// ───────────────────────────────────────────────────────────────────────────

/// A recording wrapper around a live [`TestHarness`]. Every driver method both
/// mutates the harness (so the caller sees real state) AND appends the matching
/// [`TapeStep`], so the tape captured by [`TapeRecorder::finish`] replays into
/// the identical final frame.
pub struct TapeRecorder<'h> {
    harness: &'h mut TestHarness,
    steps: Vec<TapeStep>,
}

impl<'h> TapeRecorder<'h> {
    pub(super) fn new(harness: &'h mut TestHarness) -> Self {
        Self {
            harness,
            steps: Vec::new(),
        }
    }

    /// Record + dispatch a key press.
    pub fn key(&mut self, code: KeyCode, mods: KeyModifiers) -> &mut Self {
        self.record_event(Event::Key(KeyEvent::new(code, mods)))
    }

    /// Record + dispatch a string as individual char key presses.
    pub fn type_str(&mut self, text: &str) -> &mut Self {
        for ch in text.chars() {
            self.key(KeyCode::Char(ch), KeyModifiers::empty());
        }
        self
    }

    /// Record + dispatch a bracketed paste.
    pub fn paste(&mut self, text: &str) -> &mut Self {
        self.record_event(Event::Paste(text.to_string()))
    }

    /// Record + dispatch a mouse event.
    pub fn mouse(&mut self, event: MouseEvent) -> &mut Self {
        self.record_event(Event::Mouse(event))
    }

    /// Record + dispatch a resize.
    pub fn resize(&mut self, cols: u16, rows: u16) -> &mut Self {
        self.harness.resize(cols, rows);
        self.steps
            .push(TapeStep::Event(SynthEvent::Resize(cols, rows)));
        self
    }

    /// Record + dispatch a raw event (escape hatch).
    pub fn event(&mut self, event: Event) -> &mut Self {
        self.record_event(event)
    }

    /// Record + apply a clock advance (the only time source under replay).
    pub fn advance_clock_ms(&mut self, ms: u64) -> &mut Self {
        self.harness.advance_clock_ms(ms);
        self.steps.push(TapeStep::AdvanceClockMs(ms));
        self
    }

    /// Record + open a modal directly.
    pub fn open_modal(&mut self, kind: ModalKind) -> &mut Self {
        kind.apply(self.harness);
        self.steps.push(TapeStep::OpenModal(kind));
        self
    }

    /// Record + run the P6.3 bounded async slash-command drive.
    pub fn drive_slash_commands(&mut self) -> &mut Self {
        self.harness.drive_slash_commands();
        self.steps.push(TapeStep::DriveSlashCommands);
        self
    }

    /// Record + feed one session envelope (layer-2 differential tapes).
    pub fn session_event(&mut self, ev: agent_engine::session::wire::WireSessionEvent) -> &mut Self {
        let json = serde_json::to_value(&ev).expect("WireSessionEvent serialises");
        self.harness.feed_event(ev.into());
        self.steps.push(TapeStep::SessionEvent(json));
        self
    }

    /// Record a snapshot checkpoint and return the rendered frame.
    pub fn snapshot(&mut self) -> String {
        self.steps.push(TapeStep::Snapshot);
        self.harness.snapshot()
    }

    /// Finish recording and take the [`Tape`].
    pub fn finish(self) -> Tape {
        Tape { steps: self.steps }
    }

    fn record_event(&mut self, event: Event) -> &mut Self {
        self.steps
            .push(TapeStep::Event(SynthEvent::from_event(&event)));
        self.harness.event(event);
        self
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Recording / replay API on TestHarness
// ───────────────────────────────────────────────────────────────────────────

impl TestHarness {
    /// Begin recording a tape against this harness. Drive the returned
    /// [`TapeRecorder`], then call [`TapeRecorder::finish`] to take the tape.
    pub fn record_tape(&mut self) -> TapeRecorder<'_> {
        TapeRecorder::new(self)
    }

    /// Replay a tape into a FRESH 80×24 harness and return the final frame.
    ///
    /// Deterministic + bounded: a fresh boot with the frozen P6.2 clock, time
    /// only moving through `AdvanceClockMs`, async only through the P6.3
    /// bounded drive. No wall-clock, no unbounded await.
    pub fn replay(tape: &Tape) -> String {
        let mut h = TestHarness::boot();
        h.apply_tape(tape)
    }

    /// Replay into a fresh harness at an explicit geometry (for tapes that
    /// assume a non-default starting size).
    pub fn replay_with_size(tape: &Tape, cols: u16, rows: u16) -> String {
        let mut h = TestHarness::boot_with_size(cols, rows);
        h.apply_tape(tape)
    }

    /// Replay a tape and assert its final frame equals `expected`. On mismatch,
    /// dump the tape (JSON) + the actual final frame to `target/replay-artifacts/`
    /// and panic with BOTH paths named, so a CI failure is reproducible from the
    /// artifacts alone.
    pub fn replay_expect(tape: &Tape, expected: &str, label: &str) {
        let actual = TestHarness::replay(tape);
        if actual != expected {
            let (tape_path, frame_path) = write_replay_artifacts(label, tape, &actual);
            panic!(
                "replay '{label}' diverged from the expected final frame.\n  \
                 tape  → {}\n  frame → {}\n--- actual final frame ---\n{actual}",
                tape_path.display(),
                frame_path.display()
            );
        }
    }

    /// Apply every step of `tape` to `self` in order, returning the final frame.
    fn apply_tape(&mut self, tape: &Tape) -> String {
        for step in &tape.steps {
            match step {
                TapeStep::Event(se) => {
                    self.event(se.to_event());
                }
                TapeStep::AdvanceClockMs(ms) => {
                    self.advance_clock_ms(*ms);
                }
                TapeStep::OpenModal(kind) => {
                    kind.apply(self);
                }
                TapeStep::DriveSlashCommands => {
                    self.drive_slash_commands();
                }
                TapeStep::Snapshot => {
                    let _ = self.snapshot();
                }
                TapeStep::SessionEvent(json) => {
                    let ev: agent_engine::session::wire::WireSessionEvent =
                        serde_json::from_value(json.clone()).expect("tape SessionEvent parses");
                    self.feed_event(ev.into());
                }
            }
        }
        self.snapshot()
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Failure artifacts
// ───────────────────────────────────────────────────────────────────────────

/// Directory under the workspace `target/` where replay artifacts land.
fn target_artifact_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("CARGO_TARGET_DIR") {
        return PathBuf::from(dir).join("replay-artifacts");
    }
    // This crate lives at `<workspace>/crates/agent-tui`; the shared target
    // dir is two levels up. Fall back to the manifest dir if the layout is
    // unexpected — create_dir_all makes either path usable.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let ws = match manifest.parent().and_then(|p| p.parent()) {
        Some(p) => p.to_path_buf(),
        None => manifest.clone(),
    };
    ws.join("target").join("replay-artifacts")
}

/// A short, stable-ish token so concurrent failures don't clobber each other:
/// a hash of the tape JSON. Deterministic in the tape's content.
fn tape_token(json: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    json.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Sanitize a label into a filesystem-safe stem.
fn sanitize(label: &str) -> String {
    label
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// Write `tape` (as JSON) and `actual_frame` (as text) into
/// `target/replay-artifacts/` and return `(tape_path, frame_path)`.
///
/// `pub` so a test that wants to eyeball artifacts without a panic can call it,
/// and so the paths are testable.
pub fn write_replay_artifacts(label: &str, tape: &Tape, actual_frame: &str) -> (PathBuf, PathBuf) {
    let dir = target_artifact_dir();
    // Best-effort: if the dir can't be created we still return the intended
    // paths so the panic message is informative.
    let _ = std::fs::create_dir_all(&dir);

    let json = tape.to_json();
    let stem = format!("{}-{}", sanitize(label), tape_token(&json));
    let tape_path = dir.join(format!("{stem}.tape.json"));
    let frame_path = dir.join(format!("{stem}.frame.txt"));

    let _ = std::fs::write(&tape_path, &json);
    let _ = std::fs::write(&frame_path, actual_frame);

    (tape_path, frame_path)
}

// ───────────────────────────────────────────────────────────────────────────
// Unit tests (schema round-trip; replay tests live in tests/harness_scenarios.rs)
// ───────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tape_step_json_shapes_are_stable() {
        let tape = Tape {
            steps: vec![
                TapeStep::Event(SynthEvent::Key {
                    code: SynthKey::Char('h'),
                    mods: 0,
                }),
                TapeStep::AdvanceClockMs(500),
                TapeStep::OpenModal(ModalKind::Settings),
                TapeStep::DriveSlashCommands,
                TapeStep::Snapshot,
            ],
        };
        let json = serde_json::to_string(&tape).unwrap();
        // transparent newtype ⇒ bare array; unit variants ⇒ strings.
        assert!(json.starts_with('['), "Tape must serialize as a bare array");
        assert!(json.contains(r#""DriveSlashCommands""#));
        assert!(json.contains(r#""Snapshot""#));
        assert!(json.contains(r#"{"AdvanceClockMs":500}"#));

        let back: Tape = serde_json::from_str(&json).unwrap();
        assert_eq!(tape, back, "tape must round-trip through JSON");
    }

    #[test]
    fn synth_event_round_trips_through_crossterm() {
        for ev in [
            Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL)),
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty())),
            Event::Paste("pasted".to_string()),
            Event::Resize(120, 40),
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 10,
                row: 10,
                modifiers: KeyModifiers::empty(),
            }),
        ] {
            let synth = SynthEvent::from_event(&ev);
            assert_eq!(synth.to_event(), ev, "event must survive the mirror");
        }
    }
}
