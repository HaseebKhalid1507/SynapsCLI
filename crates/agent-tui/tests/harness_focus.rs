//! P7.9 — FocusManager / ModalStack acceptance suite.
//!
//! These scenarios drive the real input pipeline (via `TestHarness`) and assert
//! *observable* behaviour — modal-stack depth, which pane owns input, focus-ring
//! traversal, and the two blessed z-order divergences from design §5.5:
//! secret-prompt-over-modal and toast-over-prompt. Nothing here pokes at
//! private routing internals; every assertion is something a user could see on
//! screen or feel through the keyboard.
//!
//! Design refs: `docs/plans/2026-07-09-p7-focusmanager-modalstack-design.md`
//! §7 (P7.9 scope) and §5.5 (z-order / secret-prompt coexistence).

use agent_tui::tui::testing::TestHarness;
use crossterm::event::{KeyCode, KeyModifiers};

fn esc(h: &mut TestHarness) {
    h.key(KeyCode::Esc, KeyModifiers::empty());
}

// ── 1. settings → marketplace: two-deep push/pop ──────────────────────────────

/// Open Settings, then the Plugins marketplace on top of it → stack depth 2.
/// One Esc pops the marketplace (Settings resumes, depth 1); a second Esc pops
/// Settings (depth 0, back to the base Chat pane). Proves LIFO unwind.
#[test]
fn scenario_01_settings_then_marketplace_two_deep_unwind() {
    let mut h = TestHarness::boot();

    h.open_settings_modal();
    assert_eq!(
        h.modal_stack_depth(),
        1,
        "settings should be the only modal"
    );
    assert_eq!(h.top_pane_name(), "settings");

    // The Plugins modal is the marketplace surface; opening it nests on top.
    h.open_plugins_modal();
    assert_eq!(
        h.modal_stack_depth(),
        2,
        "marketplace should nest over settings"
    );
    assert_eq!(h.top_pane_name(), "plugins", "marketplace must own input");

    esc(&mut h);
    assert_eq!(h.modal_stack_depth(), 1, "one Esc pops the marketplace");
    assert_eq!(
        h.top_pane_name(),
        "settings",
        "settings must resume input after the marketplace closes"
    );

    esc(&mut h);
    assert_eq!(h.modal_stack_depth(), 0, "second Esc pops settings");
    assert_eq!(h.top_pane_name(), "chat", "base pane resumes at depth 0");
}

// ── 2. settings → PluginEditor nesting ────────────────────────────────────────

/// Open Settings, then the nested PluginCustom editor. The editor becomes the
/// top pane; Esc pops just the editor (edit_mode cleared) and lands back on the
/// still-open Settings modal — the modal itself does NOT close.
#[test]
fn scenario_02_settings_plugin_editor_nesting() {
    let mut h = TestHarness::boot();

    h.open_settings_modal();
    h.open_plugin_editor();
    assert_eq!(h.modal_stack_depth(), 2, "editor nests over settings");
    assert_eq!(h.top_pane_name(), "plugin-editor");
    assert!(
        h.plugin_editor_active(),
        "PluginCustom editor should be live"
    );

    esc(&mut h);
    assert!(
        !h.plugin_editor_active(),
        "Esc must clear the nested editor (edit_mode cleared)"
    );
    assert_eq!(h.modal_stack_depth(), 1, "settings survives the editor Esc");
    assert_eq!(
        h.top_pane_name(),
        "settings",
        "settings resumes input after the editor pops"
    );
}

// ── 3. Tab focus traversal (FocusManager ring) ───────────────────────────────

/// Tab drives the two-slot FocusManager ring in BOTH the plugins and settings
/// modals. The ring's synced projection (`*_focus_side`) must toggle
/// Left → Right → Left on successive Tabs. (BackTab on a two-slot ring is the
/// same rotation, so Tab exercises the traversal in both directions.)
#[test]
fn scenario_03_tab_focus_traversal_plugins_and_settings() {
    // Plugins ring.
    let mut h = TestHarness::boot();
    h.open_plugins_modal();
    assert_eq!(
        h.plugins_focus_side(),
        Some("left"),
        "plugins start on left"
    );
    h.key(KeyCode::Tab, KeyModifiers::empty());
    assert_eq!(h.plugins_focus_side(), Some("right"), "Tab → right pane");
    h.key(KeyCode::Tab, KeyModifiers::empty());
    assert_eq!(h.plugins_focus_side(), Some("left"), "Tab → back to left");

    // Settings ring.
    let mut h = TestHarness::boot();
    h.open_settings_modal();
    assert_eq!(
        h.settings_focus_side(),
        Some("left"),
        "settings start on left"
    );
    h.key(KeyCode::Tab, KeyModifiers::empty());
    assert_eq!(h.settings_focus_side(), Some("right"), "Tab → right pane");
    h.key(KeyCode::Tab, KeyModifiers::empty());
    assert_eq!(h.settings_focus_side(), Some("left"), "Tab → back to left");
}

// ── 4. input does NOT reach an occluded pane ─────────────────────────────────

/// With Settings on top, keystrokes must route to the top pane ONLY. Typing
/// text does not leak into the occluded Chat input buffer, while a Tab is
/// consumed by Settings (its focus ring moves) — dual proof that exactly the
/// top-of-stack pane owns input.
#[test]
fn scenario_04_input_does_not_reach_occluded_pane() {
    let mut h = TestHarness::boot();
    h.open_settings_modal();

    // Text goes to settings, NOT to the occluded chat input.
    h.type_str("leak?");
    assert_eq!(
        h.input_contents(),
        "",
        "chat input must stay empty while a modal occludes it"
    );

    // The same keystroke stream reaches the top pane: Tab moves settings focus.
    assert_eq!(h.settings_focus_side(), Some("left"));
    h.key(KeyCode::Tab, KeyModifiers::empty());
    assert_eq!(
        h.settings_focus_side(),
        Some("right"),
        "Tab must be consumed by the top pane (settings), proving it owns input"
    );
}

// ── 5. secret-prompt-over-modal (§5.5 blessed divergence) ────────────────────

/// The GATE-2 blessed baseline: a secret prompt activated while a modal is open
/// paints TOPMOST. The prompt owns input (top of stack) and its chrome — title
/// + the "Enter submit · Esc cancel" footer — is visible over the modal.
#[test]
fn scenario_05_secret_prompt_paints_over_modal() {
    let mut h = TestHarness::boot();
    h.open_settings_modal();

    h.activate_secret_prompt("VAULT-KEY", "Enter vault token");
    assert!(h.secret_prompt_active(), "secret prompt must be active");
    assert_eq!(
        h.top_pane_name(),
        "secret-prompt",
        "§5.5: secret prompt takes top-of-stack over the open modal"
    );
    assert_eq!(h.modal_stack_depth(), 2, "settings + secret prompt coexist");

    let frame = h.snapshot();
    assert!(
        frame.contains("VAULT-KEY"),
        "prompt title must be visible topmost:\n{frame}"
    );
    assert!(
        frame.contains("Enter submit"),
        "prompt footer must paint above the modal:\n{frame}"
    );
}

// ── 6. toast-over-prompt corollary (GATE-2 note) ─────────────────────────────

/// Toasts render before the stack-driven modal pass, and the secret prompt
/// issues a `Clear` over its centred rect. So a dead-centre toast — geometry
/// that overlaps the prompt box — must end up painted UNDER the prompt: its
/// text is fully occluded while the prompt chrome remains visible.
#[test]
fn scenario_06_prompt_paints_over_center_toast() {
    let mut h = TestHarness::boot();

    h.push_center_toast("zorder", "ZZOCCLUDEDZZ");
    h.activate_secret_prompt("VAULT-KEY", "Enter vault token");

    let frame = h.snapshot();
    // Prompt chrome is on top…
    assert!(
        frame.contains("Enter submit"),
        "prompt must paint above the centre toast:\n{frame}"
    );
    // …and the centred toast beneath it is Clear-ed away.
    assert!(
        !frame.contains("ZZOCCLUDEDZZ"),
        "centre toast must be occluded by the prompt's Clear:\n{frame}"
    );
}

// ── 7. synthetic-modal extensibility: routing is stack-data-driven ───────────

/// The architecture goal: input routing and the modal render pass are driven by
/// ModalStack *data*, not per-pane special-casing in input.rs. We witness this
/// with existing panes (PaneOutcome is crate-private and unreachable from an
/// integration test): pushing distinct modals makes `top_pane_name` track the
/// stack top, and Esc unwinds them in strict LIFO order — the exact behaviour a
/// newly-added synthetic pane would inherit for free, no routing edits needed.
#[test]
fn scenario_07_routing_is_stack_data_driven() {
    let mut h = TestHarness::boot();

    // Heterogeneous nest: Settings, then Models, then Plugins on top.
    h.open_settings_modal();
    h.open_models_modal();
    h.open_plugins_modal();
    assert_eq!(h.modal_stack_depth(), 3);
    assert_eq!(
        h.top_pane_name(),
        "plugins",
        "top-of-stack pane owns input purely from stack data"
    );

    // Esc unwinds LIFO — the dispatcher follows the stack, not a hardcoded order.
    esc(&mut h);
    assert_eq!(h.top_pane_name(), "models", "LIFO: models resumes");
    assert_eq!(h.modal_stack_depth(), 2);

    esc(&mut h);
    assert_eq!(h.top_pane_name(), "settings", "LIFO: settings resumes");
    assert_eq!(h.modal_stack_depth(), 1);

    esc(&mut h);
    assert_eq!(h.top_pane_name(), "chat", "base pane at depth 0");
    assert_eq!(h.modal_stack_depth(), 0);
}
