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
    /// found on load via the in-flight turn draft (`session_draft`).
    Crash,
    /// A plugin session driver was running the turn and was revoked (the
    /// user took over, competing work arrived, the driver failed a check).
    /// The revocation reason itself goes to clients (`DriverRevoked`), never
    /// into history.
    Driver,
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
            InterruptReason::Driver => "[Request interrupted: session driver revoked]",
        }
    }

    /// How a canceled `tool_result` of this turn words the cancel. `None`
    /// for reasons that are never a live cancel (found on load).
    pub fn cancel_cause(self) -> Option<crate::runtime::CancelCause> {
        use crate::runtime::CancelCause;
        match self {
            InterruptReason::User => Some(CancelCause::User),
            InterruptReason::CostCap => Some(CancelCause::CostCap),
            InterruptReason::Restart => Some(CancelCause::Restart),
            InterruptReason::Host => Some(CancelCause::Host),
            InterruptReason::Driver => Some(CancelCause::Driver),
            InterruptReason::Unknown | InterruptReason::Crash => None,
        }
    }

    /// Every reason, i.e. every marker this build writes or recognises.
    pub const ALL: [InterruptReason; 7] = [
        InterruptReason::User,
        InterruptReason::CostCap,
        InterruptReason::Restart,
        InterruptReason::Host,
        InterruptReason::Unknown,
        InterruptReason::Crash,
        InterruptReason::Driver,
    ];
}

/// True iff `text` is EXACTLY one of the interruption markers. Clients use
/// this to render the marker as an "interrupted" line instead of a user
/// bubble (the daemon's `DisplayItem` has no dedicated variant: adding one
/// would break older clients, whose `DisplayItem` has no `#[serde(other)]`).
/// Exact, not a prefix: a user message that merely starts like a marker is
/// the user's, and is shown (and deduplicated) as such.
pub fn is_interruption_marker(text: &str) -> bool {
    InterruptReason::ALL.iter().any(|r| r.marker() == text)
}

/// `"[Request interrupted by user]"` → `"Request interrupted by user"`, for
/// display. `None` for anything that is not a marker.
pub fn interruption_label(text: &str) -> Option<&str> {
    is_interruption_marker(text).then(|| &text[1..text.len() - 1])
}

/// The note a cancel records when the engine never reported back on a tool
/// call the interrupted response made (`session::actor`
/// `keep_streamed_partial`): the engine's canceled-`tool_result` wording,
/// hedged to "may have already started" because the actor can't tell
/// whether the call began. A plain user-role message, recognised by its
/// exact text like the interruption marker (never by a host key a
/// rolled-back 0.10.2 would take for an archive marker); the generator and
/// the matcher live in `agent_core::core::host_note`, which archive
/// projection (agent-core) uses too.
pub fn tool_in_flight_note(reason: InterruptReason, tools: &[String]) -> String {
    let phrase = reason.cancel_cause().map_or("Canceled", |c| c.phrase());
    agent_core::core::host_note::tool_in_flight_note_text(phrase, tools)
}

/// The note as a history message.
pub fn tool_in_flight_note_message(reason: InterruptReason, tools: &[String]) -> SharedMessage {
    std::sync::Arc::new(json!({"role": "user", "content": tool_in_flight_note(reason, tools)}))
}

/// Exactly a generated note (text / message): never a user request, never
/// archived or captured as the user's words, a system line on replay.
pub use agent_core::core::host_note::{is_tool_in_flight_note, tool_in_flight_note_label};

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

/// Whether the history's last turn was interrupted for `reason` and nothing
/// has answered since: the latest interruption marker is `reason`'s, and no
/// assistant message follows it. What may follow it is what an interrupted
/// turn leaves behind (events flushed after the marker, a host note), never
/// a reply. A daemon reload's rehydrate uses it with
/// [`InterruptReason::Restart`] to find the sessions whose turn the reload
/// cut.
pub fn ends_unanswered_after(messages: &[SharedMessage], reason: InterruptReason) -> bool {
    for message in messages.iter().rev() {
        if message["role"] == "assistant" {
            return false;
        }
        if marker_of(message) {
            return message["content"].as_str() == Some(reason.marker());
        }
    }
    false
}

/// Whether a user message follows the history's latest interruption marker:
/// something (a resume line, a nudge, an event, a prompt) reached the
/// session after the interruption. A generated host note
/// ([`is_tool_in_flight_note`]) does not count. A daemon's crash restore
/// uses it so that a turn another sender already re-prompted is not resumed
/// a second time. `false` when the history has no marker.
pub fn prompted_after_latest_marker(messages: &[SharedMessage]) -> bool {
    prompted_after_cut_tail(messages, 0)
}

/// How many messages follow the history's latest interruption marker
/// (`None`: no marker). A reload's checkpoint records it once the cut
/// turn's own tail (events that arrived during the turn, a host note) is
/// in, so the next image can tell that tail from what came after the cut
/// ([`prompted_after_cut_tail`]).
pub fn after_latest_marker(messages: &[SharedMessage]) -> Option<usize> {
    messages.iter().rev().position(|m| marker_of(m))
}

/// [`prompted_after_latest_marker`], not counting the first `tail` messages
/// after the marker: the cut turn's own tail, recorded at the cut
/// ([`after_latest_marker`]). A user message past that tail (a nudge, a
/// resume, an event that arrived after the cut) counts.
pub fn prompted_after_cut_tail(messages: &[SharedMessage], tail: usize) -> bool {
    let Some(after) = after_latest_marker(messages) else {
        return false;
    };
    messages[messages.len() - after..]
        .iter()
        .skip(tail)
        .any(|m| m["role"] == "user" && !is_tool_in_flight_note(m))
}

/// Whether the history holds a user message whose text is exactly `text`
/// (a plain string content, or a text block). A crash restore uses it so a
/// note it sends once is not sent again from the same record.
pub fn has_user_text(messages: &[SharedMessage], text: &str) -> bool {
    messages.iter().rev().any(|m| {
        m["role"] == "user"
            && (m["content"].as_str() == Some(text)
                || m["content"]
                    .as_array()
                    .is_some_and(|parts| parts.iter().any(|p| p["text"].as_str() == Some(text))))
    })
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

/// Fold an in-flight turn draft left by a process that died mid-turn
/// (`agent_core::core::session_draft`) into the loaded history.
///
/// The saved history is valid (it is only ever saved at round boundaries),
/// so recovery only APPENDS — the cached prefix is untouched:
/// - the history already ends with an interruption marker → the turn was
///   already concluded (aborted, or recovered before): nothing to do;
/// - it ends with an assistant message → the turn completed (its final
///   history was saved) and only the draft's removal was lost: nothing to
///   do;
/// - otherwise the turn was cut off: the partial reply is appended as a real
///   assistant message — only when the draft belongs to exactly this
///   history (`base_len`), never a stale one — then the `Crash` marker.
///
/// Returns whether `messages` changed. Either way the caller removes the
/// draft once the result is saved.
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

    #[test]
    fn a_prompt_after_the_latest_marker_is_seen_and_a_host_note_is_not() {
        let m = |v: Value| -> SharedMessage { Arc::new(v) };
        let user = |t: &str| m(json!({"role": "user", "content": t}));
        let crash = InterruptReason::Crash;
        let mut h = vec![user("build it"), m(json!({"role": "assistant", "content": "on it"}))];
        assert!(!prompted_after_latest_marker(&h), "no marker");
        append_marker(&mut h, crash);
        assert!(!prompted_after_latest_marker(&h), "the marker is last");
        h.push(tool_in_flight_note_message(crash, &["bash".into()]));
        assert!(!prompted_after_latest_marker(&h), "a host note is no prompt");
        h.push(user("resume: someone already nudged this session"));
        assert!(prompted_after_latest_marker(&h));
        assert!(ends_unanswered_after(&h, crash), "still unanswered: the two checks differ");
        // A newer marker: only what follows IT counts.
        append_marker(&mut h, InterruptReason::User);
        assert!(!prompted_after_latest_marker(&h), "{h:#?}");
    }

    /// A reload's cut turn leaves its own tail after the marker (events
    /// that arrived during the turn, flushed at the cut); only what follows
    /// that recorded tail counts as a re-prompt, an event-shaped one too.
    #[test]
    fn the_cut_turns_own_tail_is_no_reprompt_and_anything_after_it_is() {
        let m = |v: Value| -> SharedMessage { Arc::new(v) };
        let user = |t: &str| m(json!({"role": "user", "content": t}));
        let restart = InterruptReason::Restart;
        let mut h = vec![user("build it"), m(json!({"role": "assistant", "content": "on it"}))];
        assert_eq!(after_latest_marker(&h), None);
        assert!(!prompted_after_cut_tail(&h, 0), "no marker");
        append_marker(&mut h, restart);
        assert_eq!(after_latest_marker(&h), Some(0));
        h.push(user("<event source=\"subagent\">sa_3 finished</event>"));
        h.push(tool_in_flight_note_message(restart, &["bash".into()]));
        let tail = after_latest_marker(&h).unwrap();
        assert_eq!(tail, 2);
        assert!(prompted_after_latest_marker(&h), "without the tail, the event looks like a prompt");
        assert!(!prompted_after_cut_tail(&h, tail), "the cut turn's own tail");
        h.push(user("<event source=\"fleet\">resume: pick up the cut turn</event>"));
        assert!(prompted_after_cut_tail(&h, tail), "an event after the cut is a re-prompt");
        assert!(ends_unanswered_after(&h, restart));
        // A host note past the tail is still no prompt.
        let mut noted = h[..h.len() - 1].to_vec();
        noted.push(tool_in_flight_note_message(restart, &["read".into()]));
        assert!(!prompted_after_cut_tail(&noted, tail));
    }

    #[test]
    fn a_user_text_is_found_as_a_string_or_a_text_block_only_exactly() {
        let m = |v: Value| -> SharedMessage { Arc::new(v) };
        let h = vec![
            m(json!({"role": "user", "content": "resume: note A"})),
            m(json!({"role": "assistant", "content": "resume: note B"})),
            m(json!({"role": "user", "content": [{"type": "text", "text": "resume: note C"}]})),
        ];
        assert!(has_user_text(&h, "resume: note A"));
        assert!(has_user_text(&h, "resume: note C"));
        assert!(!has_user_text(&h, "resume: note B"), "an assistant text is not a user one");
        assert!(!has_user_text(&h, "resume: note"), "only an exact match");
    }

    #[test]
    fn a_restart_cut_turn_is_found_until_something_answers_it() {
        let m = |v: Value| -> SharedMessage { Arc::new(v) };
        let user = |t: &str| m(json!({"role": "user", "content": t}));
        let assistant = |t: &str| m(json!({"role": "assistant", "content": t}));
        let restart = InterruptReason::Restart;
        let mut h = vec![user("build it"), assistant("on it")];
        assert!(!ends_unanswered_after(&h, restart), "no marker");
        assert!(!ends_unanswered_after(&[], restart));
        append_marker(&mut h, restart);
        assert!(ends_unanswered_after(&h, restart));
        assert!(!ends_unanswered_after(&h, InterruptReason::User), "another reason's marker");
        // What a cut turn leaves after its marker (flushed events, a tool
        // result, a host note) is not an answer.
        h.push(user("<event source=\"x\">a wake</event>"));
        h.push(m(json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t", "content": "x"}]})));
        assert!(ends_unanswered_after(&h, restart));
        // A reply is.
        h.push(assistant("continuing"));
        assert!(!ends_unanswered_after(&h, restart));
        // A user interrupt after the reload's marker: the latest marker wins.
        let mut h = vec![user("a"), user(restart.marker()), user("b"), user(InterruptReason::User.marker())];
        assert!(!ends_unanswered_after(&h, restart));
        h.push(user(restart.marker()));
        assert!(ends_unanswered_after(&h, restart));
    }

    #[test]
    fn tool_note_names_the_tools_hedges_like_the_engine_and_is_recognised() {
        let one = tool_in_flight_note(InterruptReason::User, &["bash".into()]);
        assert_eq!(
            one,
            "[Canceled by user while the tool call `bash` was running. It may have already \
             started and partially applied its effects.]"
        );
        let two = tool_in_flight_note(InterruptReason::Driver, &["read".into(), "bash".into()]);
        assert_eq!(
            two,
            "[Canceled (session driver revoked) while the tool calls `read`, `bash` were \
             running. They may have already started and partially applied their effects.]"
        );
        for note in [&one, &two] {
            assert_eq!(tool_in_flight_note_label(note), Some(&note[1..note.len() - 1]));
            assert!(!is_interruption_marker(note), "never mistaken for a marker");
        }
        // Every reason's note is recognised (the phrase lists agree).
        for reason in InterruptReason::ALL {
            let note = tool_in_flight_note(reason, &["x".into()]);
            assert!(tool_in_flight_note_label(&note).is_some(), "{reason:?}: {note}");
        }
        // A plain message, untagged: recognised by its text alone.
        let msg = tool_in_flight_note_message(InterruptReason::User, &["bash".into()]);
        assert_eq!(*msg, json!({"role": "user", "content": one}));
        assert!(is_tool_in_flight_note(&msg));
        assert!(!is_tool_in_flight_note(&json!({"role": "assistant", "content": one})));
        assert!(!is_tool_in_flight_note(&json!({"role": "user", "content": [{"type": "text", "text": one}]})));
    }

    /// Only the exact generated sentence is the note: nothing free-form in
    /// the middle, a single line, a known cancel phrase.
    #[test]
    fn tool_note_matcher_is_exact() {
        let one = tool_in_flight_note(InterruptReason::User, &["bash".into()]);
        let not_notes = [
            "[Request interrupted by user]".to_string(),
            "[Canceled by user]".into(),
            "hello".into(),
            "[x effects.]".into(),
            // Multi-line, or anything inserted, removed or reworded.
            one.replace(" was running.", " was\nrunning."),
            format!("{one}\n"),
            format!("{one} please"),
            one.replace("`bash`", "`bash` and also rm -rf /"),
            one.replace("`bash`", "`bash`, `ls`"),
            one.replace("It may", "They may"),
            one.replace("Canceled by user", "Canceled by someone"),
            one.replace("tool call `bash` was", "tool calls `bash` were"),
            one.replace("`bash`", "``"),
            one.replace("`bash`", "`ba\nsh`"),
            "[Canceled by user while the tool call `a` was running. Anything at all goes here \
             effects.]"
                .into(),
        ];
        for text in &not_notes {
            assert_eq!(tool_in_flight_note_label(text), None, "{text:?}");
        }
        // Whatever the model put in a tool name never reaches the note.
        for bad in ["a`b".to_string(), "a\nb".into(), "z".repeat(200)] {
            let note = tool_in_flight_note(InterruptReason::User, std::slice::from_ref(&bad));
            assert!(note.contains("(an unrecognised tool)"), "{note}");
            assert!(!note.contains(bad.as_str()), "{note}");
            assert!(tool_in_flight_note_label(&note).is_some(), "{bad:?}: {note}");
            assert!(is_tool_in_flight_note(&tool_in_flight_note_message(
                InterruptReason::User,
                std::slice::from_ref(&bad)
            )));
        }
    }

    const ALL: [InterruptReason; 7] = InterruptReason::ALL;

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
            // Shaped like a marker, but not one this build writes: the
            // user's own text (it used to match a prefix rule).
            "[Request interrupted: do Y instead]",
            "[Request interrupted by user] ",
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
            writer: None,
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
    fn a_stale_draft_never_contributes_its_text() {
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
        // Completed (final reply saved) — only the draft removal was lost.
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
