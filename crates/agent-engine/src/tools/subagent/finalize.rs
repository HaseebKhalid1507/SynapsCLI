//! finalize.rs — single terminal finalizer for reactive subagent threads.
//!
//! Called exactly once at subagent-thread exit, outside `catch_unwind`.
//! Reads terminal status from shared state, stamps `finished_at`, and
//! publishes a completion event to the parent runtime's EventQueue so an
//! idle parent can wake and call `subagent_collect`.

use crate::events::types::{EventContent, EventSource, Severity};
use crate::events::{Event, EventQueue};
use crate::runtime::subagent::{SubagentState, SubagentStatus};
use std::sync::{Arc, RwLock};

/// Kill-switch for rollback: set SYNAPS_DISABLE_SUBAGENT_WAKE=1|true|yes to suppress
/// completion-event publication (state/reaper changes remain active).
fn wake_disabled_value(v: &str) -> bool {
    matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes")
}

fn wake_disabled() -> bool {
    std::env::var("SYNAPS_DISABLE_SUBAGENT_WAKE").is_ok_and(|v| wake_disabled_value(&v))
}

/// Build the completion event. Pure — unit-testable without threads.
/// `resumed_from`: set by resume.rs only (the prior handle_id this run continues from).
#[doc(hidden)]
pub fn build_completion_event(
    handle_id: &str,
    subagent_id: u64,
    agent_name: &str,
    status: &SubagentStatus,
    result_preview: &str, // caller pre-truncates to ≤300 chars
    duration_secs: f64,
    resumed_from: Option<&str>,
) -> Event {
    use chrono::Utc;
    use serde_json::json;
    use uuid::Uuid;

    let status_str = status.as_str();
    let reason_suffix = match status.failure_reason() {
        Some(r) => {
            let truncated: String = r.chars().take(300).collect();
            format!(" Reason: {}", truncated)
        }
        None => String::new(),
    };
    let text = format!(
        "Subagent '{agent_name}' ({handle_id}) finished with status '{status_str}' \
         after {duration_secs:.1}s.{reason_suffix} \
         Call subagent_collect with handle_id \"{handle_id}\" to retrieve the full result. \
         Preview: {result_preview}"
    );

    let mut data_map = json!({
        "handle_id":     handle_id,
        "subagent_id":   subagent_id,
        "agent_name":    agent_name,
        "status":        status_str,
        "duration_secs": (duration_secs * 10.0).round() / 10.0,
    });
    if let Some(reason) = status.failure_reason() {
        data_map["error"] = json!(reason);
    }
    if let Some(rf) = resumed_from {
        data_map["resumed_from"] = json!(rf);
    }

    Event {
        id: Uuid::new_v4().to_string(),
        timestamp: Utc::now(),
        source: EventSource {
            source_type: "subagent".to_string(),
            name: agent_name.to_string(),
            callback: None,
        },
        channel: None,
        sender: None,
        content: EventContent {
            text,
            content_type: "subagent_completion".to_string(),
            severity: Some(Severity::High),
            data: Some(data_map),
            disclosure: None,
        },
        expects_response: false,
        reply_to: None,
    }
}

/// Terminal finalizer. Called EXACTLY ONCE at subagent-thread exit,
/// OUTSIDE catch_unwind. Reads terminal status from shared state,
/// stamps finished_at, publishes the completion event with a
/// never-drop guarantee (push → push_priority fallback).
///
/// `resumed_from`: `Some(prior_handle_id)` when called from resume.rs, `None` from start.rs.
#[doc(hidden)]
pub fn finalize_subagent(
    state: &Arc<RwLock<SubagentState>>,
    parent_queue: Option<&Arc<EventQueue>>,
    handle_id: &str,
    subagent_id: u64,
    agent_name: &str,
    started_at: std::time::Instant,
    resumed_from: Option<&str>,
) {
    let (status, preview, cancelled) = {
        // R6: poison-safe — if the thread panicked while holding the write lock,
        // recover the inner value rather than re-panicking outside catch_unwind.
        let mut s = state.write().unwrap_or_else(|p| p.into_inner());

        if s.cancel_requested {
            // Honest label: user abort ≠ error, ≠ completion.
            s.status = SubagentStatus::Cancelled;
        } else if matches!(s.status, SubagentStatus::Running) {
            // Defensive: a thread exiting while still "Running" is itself a bug
            // (e.g. tokio-runtime build failure path exits early) — coerce to Failed
            // so the parent is never told a lie.
            s.status = SubagentStatus::Failed("thread exited without terminal status".into());
        }
        if s.finished_at.is_none() {
            s.finished_at = Some(std::time::Instant::now());
        }
        let preview: String = s.partial_text.chars().take(300).collect();
        (s.status.clone(), preview, s.cancel_requested)
    };

    if cancelled {
        tracing::info!("subagent {handle_id}: cancelled by user — suppressing completion wake");
        return;
    }

    if wake_disabled() {
        return;
    }

    let Some(queue) = parent_queue else {
        tracing::warn!("subagent {handle_id}: no parent event_queue — completion wake unavailable");
        return;
    };

    let ev = build_completion_event(
        handle_id,
        subagent_id,
        agent_name,
        &status,
        &preview,
        started_at.elapsed().as_secs_f64(),
        resumed_from,
    );

    if let Err(e) = queue.push(ev.clone()) {
        tracing::warn!("subagent {handle_id}: event queue full ({e}) — forcing priority push");
        queue.push_priority(ev); // control-plane: never dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventQueue;
    use crate::runtime::subagent::{SubagentState, SubagentStatus};
    use std::sync::{Arc, RwLock};

    fn make_state(status: SubagentStatus, text: &str) -> Arc<RwLock<SubagentState>> {
        let s = Arc::new(RwLock::new(SubagentState::new()));
        {
            let mut inner = s.write().unwrap();
            inner.status = status;
            inner.partial_text = text.to_string();
        }
        s
    }

    // U6: completion event schema
    #[test]
    fn completion_event_schema() {
        let ev = build_completion_event(
            "sa_7",
            7,
            "researcher",
            &SubagentStatus::Completed,
            "analysis done",
            3.5,
            None,
        );

        assert_eq!(ev.source.source_type, "subagent");
        assert_eq!(ev.source.name, "researcher");
        assert_eq!(ev.content.content_type, "subagent_completion");
        assert_eq!(ev.content.severity, Some(Severity::High));
        assert!(!ev.expects_response);

        let text = &ev.content.text;
        assert!(text.contains("sa_7"), "text must contain handle_id");
        assert!(text.contains("completed"), "text must contain status");
        assert!(
            text.contains("subagent_collect"),
            "text must be self-instructing"
        );

        let data = ev.content.data.as_ref().unwrap();
        assert_eq!(data["handle_id"], "sa_7");
        assert_eq!(data["status"], "completed");
        assert!(data["duration_secs"].is_number());
    }

    // U7: finalizer coerces Running → Failed
    #[test]
    fn finalizer_coerces_running_to_failed() {
        let state = make_state(SubagentStatus::Running, "");
        let queue = Arc::new(EventQueue::new(100));

        finalize_subagent(
            &state,
            Some(&queue),
            "sa_1",
            1,
            "inline",
            std::time::Instant::now(),
            None,
        );

        // State must now be Failed
        let s = state.read().unwrap();
        assert!(
            matches!(s.status, SubagentStatus::Failed(_)),
            "Running must be coerced to Failed"
        );
        drop(s);

        // Event must be published with status=failed
        let ev = queue.pop().unwrap();
        assert!(ev.content.text.contains("failed"));
    }

    // U8: completion survives full queue via push_priority
    #[test]
    fn completion_survives_full_queue() {
        use crate::events::types::Severity as Sev;
        let queue = Arc::new(EventQueue::new(2));
        // Pre-fill with 2 Medium events
        queue
            .push(Event::simple("x", "m1", Some(Sev::Medium)))
            .unwrap();
        queue
            .push(Event::simple("x", "m2", Some(Sev::Medium)))
            .unwrap();
        assert!(queue
            .push(Event::simple("x", "m3", Some(Sev::Medium)))
            .is_err());

        let state = make_state(SubagentStatus::Completed, "done");
        finalize_subagent(
            &state,
            Some(&queue),
            "sa_8",
            8,
            "worker",
            std::time::Instant::now(),
            None,
        );

        // The completion event must be in the queue (priority-pushed to front)
        assert_eq!(queue.len(), 2, "capacity still 2 after eviction");
        let front = queue.pop().unwrap();
        assert_eq!(
            front.content.content_type, "subagent_completion",
            "completion event must be at front after priority push"
        );
    }

    // kill_switch_value_parsing: exhaustive unit test for wake_disabled_value()
    // on: 1, true, TRUE, yes, Yes, " 1 "; off: "", 0, false, no, 2, on
    #[test]
    fn kill_switch_value_parsing() {
        // ON values
        assert!(wake_disabled_value("1"), "\"1\" must be recognized");
        assert!(wake_disabled_value("true"), "\"true\" must be recognized");
        assert!(
            wake_disabled_value("TRUE"),
            "\"TRUE\" must be recognized (case-insensitive)"
        );
        assert!(wake_disabled_value("yes"), "\"yes\" must be recognized");
        assert!(
            wake_disabled_value("Yes"),
            "\"Yes\" must be recognized (case-insensitive)"
        );
        assert!(
            wake_disabled_value(" 1 "),
            "\" 1 \" must be recognized (trimmed)"
        );
        // OFF values
        assert!(!wake_disabled_value(""), "empty string must be OFF");
        assert!(!wake_disabled_value("0"), "\"0\" must be OFF");
        assert!(!wake_disabled_value("false"), "\"false\" must be OFF");
        assert!(!wake_disabled_value("no"), "\"no\" must be OFF");
        assert!(!wake_disabled_value("2"), "\"2\" must be OFF");
        assert!(!wake_disabled_value("on"), "\"on\" must be OFF");
    }

    // Positive-path: kill-switch OFF → event is published when finalize runs
    #[test]
    fn kill_switch_off_event_published() {
        // Verify that with a real queue + no kill-switch, the event IS published.
        let state = make_state(SubagentStatus::Completed, "done");
        let queue = Arc::new(EventQueue::new(100));

        finalize_subagent(
            &state,
            Some(&queue),
            "sa_9",
            9,
            "silent",
            std::time::Instant::now(),
            None,
        );

        // Event should be published (kill-switch is OFF in normal test runs)
        assert!(
            !queue.is_empty(),
            "completion event must be published when kill-switch is off"
        );
        // And finished_at must be stamped
        let s = state.read().unwrap();
        assert!(
            s.finished_at.is_some(),
            "finished_at must be stamped by finalizer"
        );
    }

    // U10 lives in events/format.rs tests (prompt-injection stripping) — see that file.
    // Here we confirm the preview passes through without modification when clean.
    #[test]
    fn preview_in_event_text() {
        let ev = build_completion_event(
            "sa_10",
            10,
            "analyst",
            &SubagentStatus::Completed,
            "clean preview text",
            1.0,
            None,
        );
        assert!(ev.content.text.contains("clean preview text"));
    }

    // #3 finalize unit test: Failed("boom") must populate data["error"] and text must contain reason
    #[test]
    fn failed_reason_in_event_data_and_text() {
        let ev = build_completion_event(
            "sa_fail",
            11,
            "bomber",
            &SubagentStatus::Failed("boom".to_string()),
            "partial output",
            2.0,
            None,
        );

        let data = ev.content.data.as_ref().unwrap();
        assert_eq!(
            data["error"], "boom",
            "data[\"error\"] must contain the failure reason"
        );
        assert_eq!(data["status"], "failed");
        assert!(
            ev.content.text.contains("boom"),
            "event text must contain failure reason: {}",
            ev.content.text
        );
    }
}
