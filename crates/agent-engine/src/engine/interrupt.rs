//! How an interrupted turn is recorded in conversation history.
//!
//! When a turn is cancelled, the engine publishes the REAL partial history —
//! the assistant's partial message (text / signed thinking / completed tool
//! calls), every completed tool round, and a canceled `tool_result` for any
//! call that did not finish. The frontend adopts that history verbatim and
//! then appends ONE short, factual user message: the interruption marker.
//!
//! This replaces the old "ABORT CONTEXT" recap, which re-described the
//! model's own reasoning and tool calls as text inside the NEXT user message.
//! A user turn that impersonates the model's output reads as a prompt
//! injection, and current models refuse it. The marker carries no
//! instructions and claims nothing about what the model did — the history
//! already says that, in the provider's own message format.
//!
//! Prompt caching: the marker is only ever APPENDED after the adopted
//! history, never folded into or edited onto an existing message, so every
//! byte the provider already cached stays identical.

use serde_json::{json, Value};

use crate::SharedMessage;

/// Why a turn was interrupted. Selects the marker text only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptReason {
    /// Esc / Cancel command / quitting the client mid-turn.
    User,
    /// The host's session cost cap tripped mid-turn.
    CostCap,
    /// The daemon checkpointed the session to restart (`daemon reload`).
    Restart,
    /// Host-side teardown: `daemon stop`, a host checkpoint request, an
    /// error ending the session.
    Host,
    /// A session persisted by an older Synaps with an `abort_context`
    /// recap: the turn was interrupted, the reason is not recorded.
    Unknown,
    /// The process died with a turn open (crash, `kill -9`, power loss):
    /// found on load via the in-flight turn sidecar (`session_draft`).
    Crash,
}

impl InterruptReason {
    /// The exact marker text appended to history.
    pub fn marker(self) -> &'static str {
        match self {
            InterruptReason::User => "[Request interrupted by user]",
            InterruptReason::CostCap => "[Request interrupted: session cost cap reached]",
            InterruptReason::Restart => "[Request interrupted: Synaps restarted]",
            InterruptReason::Host => "[Request interrupted: session stopped by the host]",
            InterruptReason::Unknown => "[Request interrupted]",
            InterruptReason::Crash => "[Request interrupted: Synaps stopped unexpectedly]",
        }
    }
}

const MARKER_PREFIX: &str = "[Request interrupted";

/// True iff `text` is one of the interruption markers. Clients use this to
/// render the marker as an "interrupted" line instead of a user bubble
/// (the daemon's `DisplayItem` has no dedicated variant: adding one would
/// break older clients, whose `DisplayItem` has no `#[serde(other)]`).
pub fn is_interruption_marker(text: &str) -> bool {
    text.starts_with(MARKER_PREFIX)
        && text.ends_with(']')
        && text.len() <= 96
        && !text.contains('\n')
}

/// `"[Request interrupted by user]"` → `"Request interrupted by user"`, for
/// display. `None` for anything that is not a marker.
pub fn interruption_label(text: &str) -> Option<&str> {
    is_interruption_marker(text).then(|| &text[1..text.len() - 1])
}

fn marker_of(message: &Value) -> bool {
    message["role"] == "user"
        && message["content"]
            .as_str()
            .is_some_and(is_interruption_marker)
}

/// Append the marker for `reason` unless the history already ends with a
/// marker (a second interruption with no turn in between records nothing
/// new). Returns whether a message was appended.
pub fn append_marker(messages: &mut Vec<SharedMessage>, reason: InterruptReason) -> bool {
    if messages.last().is_some_and(|m| marker_of(m)) {
        return false;
    }
    messages.push(std::sync::Arc::new(
        json!({"role": "user", "content": reason.marker()}),
    ));
    true
}

/// Load-time migration for sessions saved before this change. A non-`None`
/// `abort_context` means the saved history ends at the interrupted turn and
/// the recap was waiting to be prepended to the next user message. The recap
/// is DISCARDED (it is exactly the injection-shaped text this module
/// replaces) and the marker is appended in its place.
///
/// Returns whether the session was migrated. Idempotent: `abort_context` is
/// taken, so a second call is a no-op.
pub fn migrate_legacy_abort_context(
    messages: &mut Vec<SharedMessage>,
    abort_context: &mut Option<String>,
) -> bool {
    if abort_context.take().is_none() {
        return false;
    }
    // A recap with nothing to attach to (empty history) is simply dropped.
    if !messages.is_empty() {
        append_marker(messages, InterruptReason::Unknown);
    }
    true
}

/// Fold an in-flight turn sidecar left by a process that died mid-turn
/// (`agent_core::core::session_draft`) into the loaded history.
///
/// The saved history is valid (it is only ever saved at round boundaries),
/// so recovery only APPENDS — the cached prefix is untouched:
/// - the history already ends with an interruption marker → the turn was
///   already concluded (aborted, or recovered before): nothing to do;
/// - it ends with an assistant message → the turn completed (its final
///   history was saved) and only the sidecar's removal was lost: nothing to
///   do;
/// - otherwise the turn was cut off: the partial reply is appended as a real
///   assistant message — only when the sidecar belongs to exactly this
///   history (`base_len`), never a stale one — then the `Crash` marker.
///
/// Returns whether `messages` changed. Either way the caller removes the
/// sidecar once the result is saved.
pub fn recover_crashed_turn(
    messages: &mut Vec<SharedMessage>,
    draft: &agent_core::core::session_draft::TurnDraft,
) -> bool {
    match messages.last() {
        // Nothing to attach to: the prompt checkpoint never landed.
        None => return false,
        Some(last) if marker_of(last) || last["role"] == "assistant" => return false,
        Some(_) => {}
    }
    if draft.base_len == messages.len() && !draft.partial_text.trim().is_empty() {
        messages.push(std::sync::Arc::new(json!({
            "role": "assistant",
            "content": [{"type": "text", "text": draft.partial_text}]
        })));
    }
    append_marker(messages, InterruptReason::Crash);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    const ALL: [InterruptReason; 6] = [
        InterruptReason::User,
        InterruptReason::CostCap,
        InterruptReason::Restart,
        InterruptReason::Host,
        InterruptReason::Unknown,
        InterruptReason::Crash,
    ];

    fn user(text: &str) -> SharedMessage {
        Arc::new(json!({"role": "user", "content": text}))
    }

    #[test]
    fn every_marker_is_recognised_and_labelled() {
        for r in ALL {
            let m = r.marker();
            assert!(is_interruption_marker(m), "{m}");
            let label = interruption_label(m).unwrap();
            assert!(label.starts_with("Request interrupted"), "{label}");
            assert!(!label.starts_with('[') && !label.ends_with(']'));
        }
    }

    #[test]
    fn markers_carry_no_instructions_or_recap() {
        for r in ALL {
            let m = r.marker().to_lowercase();
            for banned in ["abort context", "continue", "you ", "your ", "tool", "note"] {
                assert!(!m.contains(banned), "{m:?} contains {banned:?}");
            }
        }
    }

    #[test]
    fn ordinary_user_text_is_not_a_marker() {
        for t in [
            "",
            "[Request interrupted by user] but actually do Y",
            "[Request interrupted by user]\nand more",
            "please [Request interrupted by user]",
            "[ABORT CONTEXT — your previous response was interrupted]",
        ] {
            assert!(!is_interruption_marker(t), "{t:?}");
        }
    }

    #[test]
    fn append_is_append_only_and_deduplicated() {
        let first = user("do X");
        let mut msgs = vec![first.clone()];
        assert!(append_marker(&mut msgs, InterruptReason::User));
        assert!(
            !append_marker(&mut msgs, InterruptReason::CostCap),
            "no double marker"
        );
        assert_eq!(msgs.len(), 2);
        assert!(
            Arc::ptr_eq(&msgs[0], &first),
            "earlier messages are never rebuilt"
        );
        assert_eq!(msgs[1]["content"], "[Request interrupted by user]");
    }

    #[test]
    fn legacy_abort_context_is_dropped_and_replaced_by_the_marker() {
        let mut msgs = vec![user("do X")];
        let mut ctx = Some("(System note — ABORT CONTEXT: …)".to_string());
        assert!(migrate_legacy_abort_context(&mut msgs, &mut ctx));
        assert!(ctx.is_none());
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1]["content"], "[Request interrupted]");
        assert!(!serde_json::to_string(&msgs)
            .unwrap()
            .contains("ABORT CONTEXT"));
        // Idempotent.
        assert!(!migrate_legacy_abort_context(&mut msgs, &mut ctx));
        assert_eq!(msgs.len(), 2);
    }

    fn draft(base_len: usize, text: &str) -> agent_core::core::session_draft::TurnDraft {
        agent_core::core::session_draft::TurnDraft {
            base_len,
            partial_text: text.into(),
        }
    }

    fn assistant(text: &str) -> SharedMessage {
        Arc::new(json!({"role": "assistant", "content": [{"type": "text", "text": text}]}))
    }

    #[test]
    fn crash_mid_reply_keeps_the_partial_reply_then_the_marker() {
        let prompt = user("do X");
        let mut msgs = vec![prompt.clone()];
        assert!(recover_crashed_turn(&mut msgs, &draft(1, "Let me look")));
        assert_eq!(msgs.len(), 3);
        assert!(Arc::ptr_eq(&msgs[0], &prompt), "append-only");
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[1]["content"][0]["text"], "Let me look");
        assert_eq!(msgs[2]["content"], InterruptReason::Crash.marker());
    }

    #[test]
    fn crash_with_no_streamed_text_records_only_the_marker() {
        for text in ["", "  \n"] {
            let mut msgs = vec![user("do X")];
            assert!(recover_crashed_turn(&mut msgs, &draft(1, text)));
            assert_eq!(msgs.len(), 2);
            assert_eq!(msgs[1]["content"], InterruptReason::Crash.marker());
        }
    }

    #[test]
    fn a_stale_sidecar_never_contributes_its_text() {
        // The round the text belonged to was committed (history grew past
        // base_len) or its save never landed (history shorter): marker only.
        for base in [0, 2, 5] {
            let mut msgs = vec![user("do X"), assistant("calling"), user("tool result")];
            assert!(recover_crashed_turn(&mut msgs, &draft(base, "stale text")));
            assert_eq!(msgs.len(), 4, "base {base}");
            assert!(!serde_json::to_string(&msgs).unwrap().contains("stale text"));
            assert_eq!(msgs[3]["content"], InterruptReason::Crash.marker());
        }
    }

    #[test]
    fn a_concluded_turn_is_left_alone() {
        // Completed (final reply saved) — only the sidecar removal was lost.
        let mut done = vec![user("do X"), assistant("done")];
        assert!(!recover_crashed_turn(&mut done, &draft(2, "done")));
        assert_eq!(done.len(), 2);
        // Aborted (marker saved) — same.
        let mut aborted = vec![user("do X"), user(InterruptReason::User.marker())];
        assert!(!recover_crashed_turn(&mut aborted, &draft(1, "partial")));
        assert_eq!(aborted.len(), 2);
        // Empty history — nothing to attach to.
        let mut empty = Vec::new();
        assert!(!recover_crashed_turn(&mut empty, &draft(0, "partial")));
        assert!(empty.is_empty());
    }

    #[test]
    fn recovery_is_idempotent_through_its_own_marker() {
        let mut msgs = vec![user("do X")];
        let d = draft(1, "partial");
        assert!(recover_crashed_turn(&mut msgs, &d));
        let once = msgs.clone();
        assert!(!recover_crashed_turn(&mut msgs, &d), "second load (removal lost)");
        assert_eq!(msgs, once);
    }

    #[test]
    fn legacy_migration_without_history_just_drops_the_recap() {
        let mut msgs = Vec::new();
        let mut ctx = Some("recap".to_string());
        assert!(migrate_legacy_abort_context(&mut msgs, &mut ctx));
        assert!(msgs.is_empty() && ctx.is_none());
    }
}
