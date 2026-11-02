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
/// `rollover_notice`: the run's notice when it fell back to rollover off.
/// `evicted_windows`: a resumed run's listed windows its store had evicted
/// (0 = none): above 0 the text says so and `data.evicted_windows` counts
/// them; counts only, never IDs.
#[doc(hidden)]
#[allow(clippy::too_many_arguments)]
pub fn build_completion_event(
    handle_id: &str,
    subagent_id: u64,
    agent_name: &str,
    status: &SubagentStatus,
    result_preview: &str, // caller pre-truncates to ≤300 chars
    duration_secs: f64,
    resumed_from: Option<&str>,
    rollover_notice: Option<&str>,
    evicted_windows: u64,
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
    // A run that fell back to rollover off says so (bounded at launch).
    let notice_suffix = rollover_notice
        .map(|notice| format!(" Note: {notice}."))
        .unwrap_or_default();
    let evicted_suffix = match evicted_windows {
        0 => String::new(),
        1 => " Note: 1 earlier context window was no longer in memory.".to_owned(),
        n => format!(" Note: {n} earlier context windows were no longer in memory."),
    };
    let text = format!(
        "Subagent '{agent_name}' ({handle_id}) finished with status '{status_str}' \
         after {duration_secs:.1}s.{reason_suffix}{notice_suffix}{evicted_suffix} \
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
    if let Some(notice) = rollover_notice {
        data_map["rollover_notice"] = json!(notice);
    }
    if evicted_windows > 0 {
        data_map["evicted_windows"] = json!(evicted_windows);
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
        peer: None,
    }
}

/// The wake a new daemon image posts for a worker carried over a reload
/// (`SessionCommand::AdoptWorkers`): stopped mid-run by the reload, or
/// finished with its completion wake still queued when the exec discarded
/// the queue. Same shape as [`build_completion_event`], so it routes, wakes
/// and renders like one.
pub fn build_reload_carry_event(w: &crate::runtime::subagent::CarriedWorker) -> Event {
    use serde_json::json;
    let agent = if w.agent.is_empty() { "subagent" } else { w.agent.as_str() };
    let id = w.id.as_str();
    let mut ev = if w.stopped_by_reload {
        let crash_head = super::crash_head::left_head(w);
        let resume = if let (None, Some(head)) = (&w.archive_path, crash_head) {
            // Its final record was lost (train30 item 4): the head the run
            // left is what is left of it, and resumes as a crash checkpoint.
            format!(
                "Its final record was lost (it was not published before the reload), so its head \
                 record is its crash checkpoint: subagent_resume with archive_path \"{}\" continues \
                 it from there (the checkpoint may predate its last work).",
                head.display()
            )
        } else if w.archive_path.is_some() {
            format!(
                "Its conversation is archived: subagent_resume with handle_id \"{id}\" continues it \
                 where it stopped (its last tool call reads as canceled — it may have partially run); \
                 subagent_collect reads its partial result."
            )
        } else {
            format!(
                "It did not stop in time to archive this run, so subagent_resume with handle_id \"{id}\" \
                 continues from its previous archive or restarts it from its task."
            )
        };
        let waived = if w.record_waived {
            " The reload was forced while its record was not durable, so that record was not \
             preserved: what it did last may be missing."
        } else {
            ""
        };
        let text = format!(
            "Subagent '{agent}' ({id}) was stopped after {}s by a daemon reload, not by the user, \
             and is no longer running.{waived} {resume} Preview: {}",
            w.elapsed_secs, w.preview
        );
        let mut ev = build_completion_event(
            id,
            w.subagent_id,
            agent,
            &SubagentStatus::Cancelled,
            &w.preview,
            w.elapsed_secs as f64,
            None,
            // A carried worker's rollover notice does not cross a reload.
            None,
            0,
        );
        ev.content.text = text;
        if let Some(data) = ev.content.data.as_mut() {
            data["stopped_by"] = json!("daemon_reload");
            if let (None, Some(head)) = (&w.archive_path, crash_head) {
                data["archive_path"] = json!(head.to_string_lossy());
                data["final_lost"] = json!(true);
            }
            if w.record_waived {
                data["record_waived"] = json!(true);
            }
        }
        ev
    } else {
        let status = match w.status.as_str() {
            "completed" => SubagentStatus::Completed,
            "timed_out" => SubagentStatus::TimedOut,
            "cancelled" => SubagentStatus::Cancelled,
            other => SubagentStatus::Failed(other.to_string()),
        };
        build_completion_event(
            id,
            w.subagent_id,
            agent,
            &status,
            &w.preview,
            w.elapsed_secs as f64,
            None,
            None,
            // Nothing of a carried worker's resume crosses a reload.
            0,
        )
    };
    if let (Some(data), Some(path)) = (ev.content.data.as_mut(), &w.archive_path) {
        data["archive_path"] = json!(path);
    }
    ev.content.severity = Some(Severity::High);
    ev
}

/// The resume text a daemon resume of a reload-stopped worker sends it as
/// its next user turn.
pub const DAEMON_RESUME_INSTRUCTIONS: &str = "[The daemon reloaded and stopped you mid-run; this resume \
     was started by the daemon. Check your last tool call before trusting it, then continue.]";

/// The parent's wake for a reload-stopped worker the daemon resumed itself
/// as `new` (in place of [`build_reload_carry_event`]'s "resume it"). The
/// completion then arrives from `new`, as any resumed worker's does.
/// `evicted_windows`: listed windows of its lineage the store had evicted
/// (0 = none); above 0 the text and `data.evicted_windows` carry the count.
pub fn build_reload_resumed_event(
    w: &crate::runtime::subagent::CarriedWorker,
    new: &str,
    evicted_windows: u64,
) -> Event {
    use serde_json::json;
    let agent = if w.agent.is_empty() { "subagent" } else { w.agent.as_str() };
    let old = w.id.as_str();
    let mut ev = build_completion_event(
        old,
        w.subagent_id,
        agent,
        &SubagentStatus::Running,
        &w.preview,
        w.elapsed_secs as f64,
        None,
        None,
        evicted_windows,
    );
    ev.content.text = format!(
        "Subagent '{agent}' ({old}) was stopped by a daemon reload and resumed by the daemon as \
         {new}; its completion will arrive as {new}."
    );
    if let Some(note) = super::lineage::evicted_parent_note(evicted_windows as usize) {
        ev.content.text.push_str(&format!(" {note}."));
    }
    ev.content.content_type = "subagent_resumed".to_string();
    if let Some(data) = ev.content.data.as_mut() {
        data["resumed_as"] = json!(new);
        data["stopped_by"] = json!("daemon_reload");
    }
    ev.content.severity = Some(Severity::High);
    ev
}

/// [`build_reload_carry_event`] for a reload-stopped worker the daemon did
/// not resume itself, saying why.
pub fn build_reload_left_event(w: &crate::runtime::subagent::CarriedWorker, reason: &str) -> Event {
    let mut ev = build_reload_carry_event(w);
    let reason = reason.trim().trim_end_matches('.');
    ev.content.text.push_str(&format!(" Not resumed automatically: {reason}."));
    if let Some(data) = ev.content.data.as_mut() {
        data["not_resumed"] = serde_json::json!(reason);
    }
    ev
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
    let dir = state
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .archive_dir
        .clone();
    finalize_subagent_impl(
        state,
        parent_queue,
        handle_id,
        subagent_id,
        agent_name,
        started_at,
        resumed_from,
        dir.as_deref(),
    )
}

/// [`finalize_subagent`] with an explicit resume-archive directory (tests).
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn finalize_subagent_in(
    state: &Arc<RwLock<SubagentState>>,
    parent_queue: Option<&Arc<EventQueue>>,
    handle_id: &str,
    subagent_id: u64,
    agent_name: &str,
    started_at: std::time::Instant,
    resumed_from: Option<&str>,
    archive_dir: &std::path::Path,
) {
    finalize_subagent_impl(
        state,
        parent_queue,
        handle_id,
        subagent_id,
        agent_name,
        started_at,
        resumed_from,
        Some(archive_dir),
    );
}

/// Marks [`SubagentState::completion_settled`] when the finalizer returns.
struct SettleOnDrop<'a>(&'a Arc<RwLock<SubagentState>>);

impl Drop for SettleOnDrop<'_> {
    fn drop(&mut self) {
        self.0.write().unwrap_or_else(|p| p.into_inner()).completion_settled = true;
    }
}

#[allow(clippy::too_many_arguments)]
fn finalize_subagent_impl(
    state: &Arc<RwLock<SubagentState>>,
    parent_queue: Option<&Arc<EventQueue>>,
    handle_id: &str,
    subagent_id: u64,
    agent_name: &str,
    started_at: std::time::Instant,
    resumed_from: Option<&str>,
    archive_dir: Option<&std::path::Path>,
) {
    let (status, preview, cancelled, rollover_notice, visibility, evicted_windows) = {
        // R6: poison-safe — if the thread panicked while holding the write lock,
        // recover the inner value rather than re-panicking outside catch_unwind.
        let mut s = state.write().unwrap_or_else(|p| p.into_inner());

        if s.finalization_started {
            return;
        }
        s.finalization_started = true;
        if s.cancel_requested {
            // Honest label: user abort ≠ error, ≠ completion.
            s.status = SubagentStatus::Cancelled;
        } else if matches!(s.status, SubagentStatus::Running) {
            // Defensive: a thread exiting while still "Running" is itself a bug
            // (e.g. tokio-runtime build failure path exits early) — coerce to Failed
            // so the parent is never told a lie.
            s.status = SubagentStatus::Failed("thread exited without terminal status".into());
        }
        // The worker's latest word (its result), not the first words of the run.
        let latest = s.last_response_text().trim();
        let source = if latest.is_empty() {
            s.partial_text.trim()
        } else {
            latest
        };
        let preview: String = source.chars().take(300).collect();
        (
            s.status.clone(),
            preview,
            s.cancel_requested,
            s.rollover_notice.clone(),
            s.visibility.clone(),
            s.evicted_windows,
        )
    };
    // However this returns from here on: settled, so a reload's checkpoint
    // knows the wake (if any) is in the queue it is about to carry.
    let _settled = SettleOnDrop(state);

    // Archive the full conversation (every terminal status, cancel included) so
    // subagent_resume and subagent_collect still work after the handle is reaped.
    let v2_records = state.read().unwrap_or_else(|p| p.into_inner()).v2_records;
    if v2_records {
        // A v2 run published its final itself (`launch.rs`), or failed to and
        // said why; a v1 archive would take the final's name.
        let mut s = state.write().unwrap_or_else(|p| p.into_inner());
        if s.archive_path.is_none() && s.archive_error.is_none() {
            s.archive_error = Some(
                "the worker ended without publishing its final record (its head record is \
                 the crash checkpoint); history retained in memory"
                    .into(),
            );
        }
    } else if let Some(dir) = archive_dir {
        super::archive::archive_finished_worker(state, handle_id, dir);
    } else {
        let mut s = state.write().unwrap_or_else(|p| p.into_inner());
        if s.history.is_some() || s.archive_meta.is_some() {
            s.archive_error =
                Some("missing launch archive directory; history retained in memory".into());
        }
    }
    {
        let mut s = state.write().unwrap_or_else(|p| p.into_inner());
        s.finished_at = Some(std::time::Instant::now());
        s.finalization_ready = true;
    }

    if cancelled {
        // A reload's stop is re-announced by the next image (AdoptWorkers);
        // this image's queue dies with the exec.
        let reload_stop = {
            let s = state.read().unwrap_or_else(|p| p.into_inner());
            s.stopped_by_reload
                .then(|| (s.archive_path.is_some(), s.archive_error.clone(), s.v2_head.clone()))
        };
        if let Some((false, error, head)) = &reload_stop {
            // Nothing final of the run was written (its write failed, e.g.
            // with EMFILE during the drain): never "archived".
            let error = error.as_deref().unwrap_or("no reason recorded");
            match head {
                Some(head) => tracing::warn!(
                    "subagent {handle_id}: stopped by daemon reload, but its final record was not \
                     published ({error}); its head record {} is kept as the crash checkpoint; the \
                     next image wakes the parent",
                    head.display()
                ),
                None => tracing::warn!(
                    "subagent {handle_id}: stopped by daemon reload, but its conversation was not \
                     saved ({error}); the next image wakes the parent"
                ),
            }
        } else if reload_stop.is_some() {
            tracing::info!("subagent {handle_id}: stopped by daemon reload — archived; the next image wakes the parent");
        } else {
            tracing::info!("subagent {handle_id}: cancelled by user — suppressing completion wake");
        }
        return;
    }

    if wake_disabled() {
        return;
    }

    let Some(queue) = parent_queue else {
        tracing::warn!("subagent {handle_id}: no parent event_queue — completion wake unavailable");
        return;
    };

    let mut ev = build_completion_event(
        handle_id,
        subagent_id,
        agent_name,
        &status,
        &preview,
        started_at.elapsed().as_secs_f64(),
        resumed_from,
        rollover_notice.as_deref(),
        evicted_windows,
    );
    // A v2 run's counters and notices (§8); nothing for a v1 run.
    if let (Some(visibility), Some(data)) = (&visibility, ev.content.data.as_mut()) {
        visibility.add_to_completion(data);
    }

    if let Err(e) = queue.push(ev.clone()) {
        tracing::warn!("subagent {handle_id}: event queue full ({e}) — forcing priority push");
        queue.push_priority(ev); // control-plane: never dropped
    }
}

/// Fix for workers that outlive their cancellation in the orchestration
/// registry: a user-cancelled worker suppresses its completion wake (above),
/// so nothing would ever prompt the model to collect it, and it stayed
/// "unreconciled" forever — deferring context rollover until the hard limit.
///
/// Once the finalizer has run for a cancelled worker, retire it in the
/// orchestration registry. Called from the worker thread right after
/// [`finalize_subagent`], and again by the launcher after `mark_running`
/// (the thread can finish before the launcher marks it running). Returns
/// whether the worker is now retired; a no-op for anything not cancelled.
#[doc(hidden)]
pub fn retire_if_cancelled(
    state: &Arc<RwLock<SubagentState>>,
    orchestration: &crate::orchestration::OrchestrationRuntime,
    handle_id: &str,
) -> bool {
    let cancelled_and_final = {
        let s = state.read().unwrap_or_else(|p| p.into_inner());
        s.cancel_requested && s.finalization_ready
    };
    cancelled_and_final && orchestration.retire_cancelled(handle_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventQueue;

    fn carried(stopped: bool, wake_lost: bool, archive: Option<&str>) -> crate::runtime::subagent::CarriedWorker {
        crate::runtime::subagent::CarriedWorker {
            id: "sa_288".into(),
            subagent_id: 288,
            agent: "integrator".into(),
            status: if stopped { "cancelled" } else { "completed" }.into(),
            archive_path: archive.map(std::path::PathBuf::from),
            stopped_by_reload: stopped,
            wake_lost,
            wake: stopped || wake_lost,
            elapsed_secs: 1234,
            preview: "polling the build".into(),
            ..Default::default()
        }
    }

    /// The daemon's own resume replaces "resume it" with who continues it,
    /// once, and a worker it left says why.
    #[test]
    fn reload_resumed_and_left_events_name_the_new_handle_or_the_reason() {
        let w = carried(true, false, Some("/h/sa_288.json"));
        let ev = build_reload_resumed_event(&w, "sa_301", 0);
        assert_eq!(
            ev.content.text,
            "Subagent 'integrator' (sa_288) was stopped by a daemon reload and resumed by the daemon as \
             sa_301; its completion will arrive as sa_301."
        );
        assert_eq!(ev.content.content_type, "subagent_resumed");
        assert_eq!(ev.content.severity, Some(Severity::High));
        let data = ev.content.data.as_ref().unwrap();
        assert_eq!(data["handle_id"], "sa_288");
        assert_eq!(data["resumed_as"], "sa_301");
        assert_eq!(data["stopped_by"], "daemon_reload");

        let ev = build_reload_left_event(&w, "its session was parked");
        let text = &ev.content.text;
        assert!(text.starts_with(&build_reload_carry_event(&w).content.text), "{text}");
        assert!(text.ends_with(" Not resumed automatically: its session was parked."), "{text}");
        assert_eq!(ev.content.content_type, "subagent_completion");
        assert!(DAEMON_RESUME_INSTRUCTIONS.starts_with("[The daemon reloaded and stopped you mid-run;"));
    }

    /// Train30 item 4b: a reload-stopped worker whose final record was lost
    /// (its head is the crash checkpoint) tells the parent so, with the
    /// head's path to resume from, in its text and data; without a head the
    /// wake is today's.
    #[test]
    fn a_reload_stopped_worker_whose_final_was_lost_names_its_crash_checkpoint() {
        let head = "/h/1-x/sa_288.head.json";
        let w = crate::runtime::subagent::CarriedWorker {
            crash_head: Some(head.into()),
            ..carried(true, false, None)
        };
        let ev = build_reload_left_event(&w, "its final record was lost");
        let text = &ev.content.text;
        assert!(text.contains("by a daemon reload, not by the user"), "{text}");
        assert!(text.contains("Its final record was lost"), "{text}");
        assert!(text.contains("crash checkpoint"), "{text}");
        assert!(
            text.contains(&format!("subagent_resume with archive_path \"{head}\"")),
            "{text}"
        );
        assert!(!text.contains("did not stop in time"), "{text}");
        let data = ev.content.data.as_ref().unwrap();
        assert_eq!(data["archive_path"], head);
        assert_eq!(data["final_lost"], true);
        assert_eq!(data["stopped_by"], "daemon_reload");

        let plain = build_reload_carry_event(&carried(true, false, None));
        assert!(plain.content.text.contains("It did not stop in time to archive this run"));
        let data = plain.content.data.as_ref().unwrap();
        assert!(data.get("archive_path").is_none() && data.get("final_lost").is_none());
    }

    /// B31 (a): a daemon resume whose store evicted listed windows tells the
    /// parent the count in its wake, text and data; none, nothing new.
    #[test]
    fn reload_resumed_event_counts_evicted_windows_only_above_zero() {
        let w = carried(true, false, Some("/h/sa_288.json"));
        let none = build_reload_resumed_event(&w, "sa_301", 0);
        assert!(none.content.data.as_ref().unwrap().get("evicted_windows").is_none());
        assert!(!none.content.text.contains("no longer in memory"));
        let ev = build_reload_resumed_event(&w, "sa_301", 2);
        assert_eq!(
            ev.content.text,
            "Subagent 'integrator' (sa_288) was stopped by a daemon reload and resumed by the daemon as \
             sa_301; its completion will arrive as sa_301. 2 earlier context windows of the resumed \
             worker are no longer in memory; it continues from its summary."
        );
        let data = ev.content.data.as_ref().unwrap();
        assert_eq!(data["evicted_windows"], 2);
        assert_eq!(data["resumed_as"], "sa_301");
        let one = build_reload_resumed_event(&w, "sa_301", 1);
        assert!(one.content.text.ends_with(
            " 1 earlier context window of the resumed worker is no longer in memory; it continues \
             from its summary."
        ));
        assert_eq!(one.content.data.as_ref().unwrap()["evicted_windows"], 1);
    }

    /// B31 (a): the completion event carries `evicted_windows` and its note
    /// only when the resumed run's store evicted any; count only.
    #[test]
    fn completion_event_counts_evicted_windows_only_above_zero() {
        let event = |evicted| {
            build_completion_event(
                "sa_9",
                9,
                "scout",
                &SubagentStatus::Completed,
                "done",
                1.0,
                Some("sa_8"),
                None,
                evicted,
            )
        };
        let none = event(0);
        assert!(none.content.data.as_ref().unwrap().get("evicted_windows").is_none());
        assert!(!none.content.text.contains("no longer in memory"));
        let one = event(1);
        assert_eq!(one.content.data.as_ref().unwrap()["evicted_windows"], 1);
        assert!(
            one.content
                .text
                .contains("after 1.0s. Note: 1 earlier context window was no longer in memory. Call"),
            "{}",
            one.content.text
        );
        let three = event(3);
        assert_eq!(three.content.data.as_ref().unwrap()["evicted_windows"], 3);
        assert!(
            three
                .content
                .text
                .contains(" Note: 3 earlier context windows were no longer in memory. Call"),
            "{}",
            three.content.text
        );
    }

    /// The reload wake says who stopped the worker (not the user), that it
    /// is resumable, and routes exactly like a completion.
    #[test]
    fn reload_carry_event_for_a_stopped_worker_points_at_resume() {
        let ev = build_reload_carry_event(&carried(true, false, Some("/h/sa_288.json")));
        assert_eq!(ev.content.content_type, "subagent_completion");
        assert_eq!(ev.source.source_type, "subagent");
        assert_eq!(ev.content.severity, Some(Severity::High));
        let text = &ev.content.text;
        assert!(text.contains("(sa_288)") && text.contains("daemon reload"), "{text}");
        assert!(text.contains("not by the user"), "{text}");
        assert!(text.contains("subagent_resume with handle_id \"sa_288\""), "{text}");
        assert!(text.contains("polling the build"), "{text}");
        let data = ev.content.data.unwrap();
        assert_eq!(data["handle_id"], "sa_288");
        assert_eq!(data["status"], "cancelled");
        assert_eq!(data["stopped_by"], "daemon_reload");
        assert_eq!(data["archive_path"], "/h/sa_288.json");

        let text = build_reload_carry_event(&carried(true, false, None)).content.text;
        assert!(text.contains("did not stop in time"), "{text}");
    }

    /// Spec §5: a forced reload that went ahead without the worker's record
    /// says so in the wake, once, and in its data; an ordinary stop does not.
    #[test]
    fn a_forced_reload_wake_says_the_record_was_not_preserved() {
        let waived = crate::runtime::subagent::CarriedWorker {
            record_waived: true,
            ..carried(true, false, Some("/h/sa_288.json"))
        };
        let ev = build_reload_carry_event(&waived);
        let text = &ev.content.text;
        assert!(
            text.contains(
                "and is no longer running. The reload was forced while its record was not durable, so \
                 that record was not preserved: what it did last may be missing. "
            ),
            "{text}"
        );
        assert_eq!(text.matches("not preserved").count(), 1, "{text}");
        assert_eq!(ev.content.data.as_ref().unwrap()["record_waived"], true);

        let ev = build_reload_carry_event(&carried(true, false, Some("/h/sa_288.json")));
        assert!(!ev.content.text.contains("not preserved"), "{}", ev.content.text);
        assert!(ev.content.data.unwrap().get("record_waived").is_none());
    }

    /// A finished worker whose wake the exec discarded gets the ordinary
    /// completion wake again.
    #[test]
    fn reload_carry_event_for_a_lost_wake_is_the_normal_completion() {
        let ev = build_reload_carry_event(&carried(false, true, Some("/h/sa_288.json")));
        let text = &ev.content.text;
        assert!(text.contains("finished with status 'completed'"), "{text}");
        assert!(!text.contains("daemon reload"), "{text}");
        let data = ev.content.data.unwrap();
        assert_eq!(data["status"], "completed");
        assert!(data.get("stopped_by").is_none());
    }
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

    #[test]
    fn completion_is_published_only_with_recovery_outcome_and_only_once() {
        let dir = tempfile::tempdir().unwrap();
        let state = make_state(SubagentStatus::Completed, "done");
        {
            let mut s = state.write().unwrap();
            s.archive_meta = Some(crate::runtime::subagent::SubagentArchiveMeta {
                agent_name: "worker".into(),
                model: "model".into(),
                system_prompt: "system".into(),
                timeout_secs: 1,
                cwd: None,
                thinking: None,
                watch_usage: None,
            });
            s.history = Some(vec![Arc::new(
                serde_json::json!({"role":"user", "content":"task"}),
            )]);
            assert_eq!(s.public_status(), SubagentStatus::Running);
        }
        let queue = Arc::new(EventQueue::new(100));
        let started = std::time::Instant::now();
        finalize_subagent_in(
            &state,
            Some(&queue),
            "sa_order",
            1,
            "worker",
            started,
            None,
            dir.path(),
        );
        {
            let s = state.read().unwrap();
            assert!(s.finalization_ready);
            assert!(s.finished_at.is_some());
            assert!(super::super::archive::read_archive(s.archive_path.as_ref().unwrap()).is_ok());
            assert_eq!(s.public_status(), SubagentStatus::Completed);
        }
        let count = queue.len();
        assert_eq!(count, 1);
        state.write().unwrap().cancel_requested = true;
        finalize_subagent_in(
            &state,
            Some(&queue),
            "sa_order",
            1,
            "worker",
            started,
            None,
            dir.path(),
        );
        assert_eq!(queue.len(), count);
        assert_eq!(state.read().unwrap().status, SubagentStatus::Completed);
    }

    fn orchestration_with_running(id: &str) -> crate::orchestration::OrchestrationRuntime {
        let model = agent_core::prompt::QualifiedModelId::parse("openrouter/z-ai/glm-5.2").unwrap();
        let orchestration =
            crate::orchestration::OrchestrationRuntime::baseline(model, 3, 8).unwrap();
        orchestration.resolve_and_authorize(id, None).unwrap();
        orchestration.mark_starting(id).unwrap();
        orchestration.mark_running(id).unwrap();
        orchestration
    }

    /// A user-cancelled worker publishes no completion wake, so the model is
    /// never told to collect it. It must not stay "unreconciled" forever:
    /// that deferred context rollover until the hard limit refused every
    /// further request (incident 2026-10-13).
    #[test]
    fn cancelled_worker_is_retired_from_orchestration() {
        let dir = tempfile::tempdir().unwrap();
        let orchestration = orchestration_with_running("sa_cancel");
        let state = make_state(SubagentStatus::Running, "partial");
        state.write().unwrap().cancel_requested = true;

        // Before the finalizer runs there is nothing to retire yet.
        assert!(!retire_if_cancelled(&state, &orchestration, "sa_cancel"));
        assert!(orchestration.is_unreconciled("sa_cancel"));

        let queue = Arc::new(EventQueue::new(100));
        finalize_subagent_in(
            &state,
            Some(&queue),
            "sa_cancel",
            1,
            "worker",
            std::time::Instant::now(),
            None,
            dir.path(),
        );
        assert!(queue.is_empty(), "cancel still suppresses the wake");
        assert!(retire_if_cancelled(&state, &orchestration, "sa_cancel"));
        assert!(!orchestration.is_unreconciled("sa_cancel"));
        assert!(
            orchestration.unreconciled_runtime_handles().is_empty(),
            "nothing left to pin rollover"
        );
        // Idempotent: the launcher's post-mark_running re-check is harmless,
        // and a later model collect still succeeds.
        assert!(retire_if_cancelled(&state, &orchestration, "sa_cancel"));
        orchestration
            .terminal_and_collect(
                "sa_cancel",
                agent_core::orchestration::WorkerTerminal::Failed,
            )
            .unwrap();
        orchestration.reconcile("sa_cancel").unwrap();
    }

    /// Completed (not cancelled) workers keep the collect-and-attest contract.
    #[test]
    fn completed_worker_is_never_auto_reconciled() {
        let dir = tempfile::tempdir().unwrap();
        let orchestration = orchestration_with_running("sa_done");
        let state = make_state(SubagentStatus::Completed, "result");
        finalize_subagent_in(
            &state,
            None,
            "sa_done",
            1,
            "worker",
            std::time::Instant::now(),
            None,
            dir.path(),
        );
        assert!(!retire_if_cancelled(&state, &orchestration, "sa_done"));
        assert!(orchestration.is_unreconciled("sa_done"));
    }

    #[test]
    fn cancelled_finalizer_is_ready_without_duplicate_publication() {
        let dir = tempfile::tempdir().unwrap();
        let state = make_state(SubagentStatus::Running, "cancelled");
        state.write().unwrap().cancel_requested = true;
        let queue = Arc::new(EventQueue::new(100));
        for _ in 0..2 {
            finalize_subagent_in(
                &state,
                Some(&queue),
                "sa_cancel",
                1,
                "worker",
                std::time::Instant::now(),
                None,
                dir.path(),
            );
        }
        assert!(queue.is_empty());
        let s = state.read().unwrap();
        assert!(s.finalization_ready);
        assert!(s.finished_at.is_some());
        assert_eq!(s.public_status(), SubagentStatus::Cancelled);
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
            None,
            0,
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
            None,
            0,
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
            None,
            0,
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
