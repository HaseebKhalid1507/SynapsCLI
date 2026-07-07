//! P4 smoke test — proves the headless harness boots, accepts input, and
//! renders a non-empty frame through `TestBackend`.
//!
//! This is deliberately a single test: the real scenario suite is a
//! follow-up task. Its job is to keep the harness itself compiling and
//! functioning as public (feature-gated) API from an external test crate.

use agent_tui::tui::testing::TestHarness;
use crossterm::event::{KeyCode, KeyModifiers};

#[test]
fn harness_boots_and_renders_nonempty_frame() {
    let mut h = TestHarness::boot();

    // Frame renders and has the expected geometry.
    let buf = h.render();
    assert_eq!(buf.area().width, 80);
    assert_eq!(buf.area().height, 24);

    // The frame is non-empty: header chrome ("SynapsCLI") and the ready
    // status must be present on a fresh boot.
    let frame = h.snapshot();
    assert!(!frame.trim().is_empty(), "rendered frame is blank");
    assert!(frame.contains("Synaps"), "header missing:\n{frame}");
    assert!(frame.contains("ready"), "ready status missing:\n{frame}");

    // Input goes through the production dispatch surface and lands in the
    // input box, both in state and on screen.
    h.type_str("hello harness");
    assert_eq!(h.input_contents(), "hello harness");
    let frame = h.snapshot();
    assert!(frame.contains("hello harness"), "typed text not rendered:\n{frame}");

    // Enter dispatches a Submit action (recorded, not executed — sync-only
    // harness; async execution is the P6 follow-up).
    h.key(KeyCode::Enter, KeyModifiers::empty());
    assert_eq!(h.take_actions(), vec!["submit:hello harness".to_string()]);

    // Resize reflows: the frame re-renders at the new geometry.
    h.resize(100, 30);
    let buf = h.render();
    assert_eq!(buf.area().width, 100);
    assert_eq!(buf.area().height, 30);
}
