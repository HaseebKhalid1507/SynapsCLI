//! P8 — thin session-driver client.
//!
//! The autonomous driver's state machine (arm, tick, schedule, feedback,
//! terminal capture, submit routing, revoke) now lives entirely in the
//! `SessionActor` (P3–P7). The old ~1811-line TUI `session_driver.rs` was
//! deleted; this is all that survives on the client side:
//!
//!   * a tiny [`DriverUiState`] tracking "driver armed" (from `DriverArmed` /
//!     `DriverRevoked` wire events) for status-bar render + Esc/Ctrl-C routing,
//!   * event-render handlers that turn the actor's driver wire events into
//!     transcript messages + a persistent status toast.
//!
//! The client computes NOTHING about driver lifecycle itself — it reacts to
//! the actor's events. `/auto` sends `DriverStart`; Esc/Ctrl-C while armed
//! sends `Cancel`; the actor owns everything else.

use agent_engine::extensions::session_driver::{Outcome, Selection};

use super::app::{App, ChatMessage};

/// Toast id for the persistent driver status indicator (upsert/dismiss by id).
const STATUS_TOAST_ID: &str = "driver";

/// Client-side mirror of the actor's driver arm state. Every field is set
/// purely from wire events — never computed locally.
#[derive(Debug, Default, Clone)]
pub(crate) struct DriverUiState {
    /// True between `DriverArmed` and `DriverRevoked`. Gates Esc/Ctrl-C
    /// routing (→ `Cancel`) and the status-bar render.
    pub(crate) armed: bool,
    pub(crate) plugin_id: String,
    pub(crate) model: String,
    pub(crate) deadline_ms: Option<u64>,
}

impl DriverUiState {
    /// Whether the driver is armed on this session (Esc/Ctrl-C → `Cancel`).
    pub(crate) fn is_armed(&self) -> bool {
        self.armed
    }
}

fn fmt_deadline(deadline_ms: Option<u64>) -> String {
    match deadline_ms {
        None => "no deadline".to_string(),
        Some(ms) => {
            let secs = ms / 1000;
            if secs >= 60 {
                format!("{}m{}s left", secs / 60, secs % 60)
            } else {
                format!("{secs}s left")
            }
        }
    }
}

/// `DriverArmed`: the actor armed the driver. Track state, surface the plugin
/// notice, and pin a persistent status toast (plugin, model, deadline).
pub(crate) fn on_armed(
    app: &mut App,
    plugin_id: String,
    _run_id: String,
    _models: Vec<Selection>,
    selection: Selection,
    deadline_ms: Option<u64>,
    notice: String,
) {
    app.driver_ui = DriverUiState {
        armed: true,
        plugin_id: plugin_id.clone(),
        model: selection.model.clone(),
        deadline_ms,
    };
    if !notice.is_empty() {
        app.push_msg(ChatMessage::System(notice));
    }
    app.toasts.upsert(
        super::toast::Toast::new(
            STATUS_TOAST_ID,
            format!(
                "{} · {} · {}",
                plugin_id,
                selection.model,
                fmt_deadline(deadline_ms)
            ),
        )
        .titled("Driver armed")
        .ttl(None),
    );
    app.request_redraw();
}

/// `DriverRevoked`: the actor revoked the driver. Clear state, dismiss the
/// status toast, restore any undelivered steering to the input draft, and
/// surface the reason.
pub(crate) fn on_revoked(app: &mut App, reason: String, undelivered_steering: Vec<String>) {
    app.driver_ui = DriverUiState::default();
    app.toasts.dismiss(STATUS_TOAST_ID);
    // Restore undelivered steering to the draft so the user doesn't lose it
    // (the actor drained it into the event; the client owns the editor).
    if !undelivered_steering.is_empty() {
        let restored = undelivered_steering.join("\n");
        let existing = app.input_text();
        if existing.trim().is_empty() {
            app.set_input_text(&restored);
        } else {
            app.set_input_text(&format!("{restored}\n{existing}"));
        }
    }
    app.push_msg(ChatMessage::System(format!("session driver stopped: {reason}")));
    app.request_redraw();
}

/// `DriverTurnOutcome`: one autonomous turn settled. Surface it compactly.
pub(crate) fn on_turn_outcome(
    app: &mut App,
    outcome: Outcome,
    selection: Selection,
    feedback: Option<String>,
) {
    let outcome = match outcome {
        Outcome::Success => "success",
        Outcome::ProviderError => "provider error",
        Outcome::SelectionRejected => "selection rejected",
        Outcome::TimeCheckpoint => "time checkpoint",
        Outcome::Blocked => "blocked",
    };
    let mut line = format!("driver turn: {outcome} ({})", selection.model);
    if let Some(fb) = feedback {
        line.push_str(&format!(" · feedback: {fb}"));
    }
    app.push_msg(ChatMessage::System(line));
    app.request_redraw();
}
