//! Reactive workers across a daemon reload.
//!
//! `synaps daemon reload` `execv`s the daemon in place. Worker threads
//! cannot survive that: before this module the exec destroyed running
//! workers with no archive, no wake for the waiting parent, their in-flight
//! shell commands left running unreaped, and the handle counter restarted at
//! `sa_1`. Now the checkpoint stops every running worker
//! ([`SubagentRegistry::stop_for_reload`]) and waits for its archive, the
//! reload record carries each worker ([`SubagentRegistry::reload_carry`]),
//! and the next image re-registers the ids against their archives
//! ([`SubagentRegistry::adopt_carried`]) and wakes the parent.

use std::sync::{Arc, RwLock};

use super::{SubagentRegistry, SubagentState, SubagentStatus};

/// A worker carried across a daemon reload (`ReloadSession::workers`).
///
/// A reload `execv`s the daemon: worker threads cannot survive it. So the
/// checkpoint stops every running worker (its archive is the resumable
/// state), and the new image re-registers each id against its archive and
/// tells the parent, which would otherwise wait for a completion that never
/// comes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CarriedWorker {
    pub id: String,
    #[serde(default)]
    pub subagent_id: u64,
    #[serde(default)]
    pub agent: String,
    /// `running` never appears: the checkpoint has stopped them.
    pub status: String,
    /// The resume archive, when the worker's finalizer wrote one.
    #[serde(default)]
    pub archive_path: Option<std::path::PathBuf>,
    /// A daemon reload (this one or an earlier one) stopped this run, not
    /// the user. A label for status and the wake's wording; whether the
    /// parent is told again is [`Self::wake`].
    #[serde(default)]
    pub stopped_by_reload: bool,
    /// Finished on its own, and its completion wake never reached the
    /// parent: still queued in the old image's event queue (which the exec
    /// discards), or not yet pushed when the checkpoint ended.
    #[serde(default)]
    pub wake_lost: bool,
    /// The next image must wake the parent about this worker: this reload
    /// stopped it, its completion wake was lost, or the reload wake an
    /// earlier image posted is still undelivered. Exactly once per stop:
    /// a later reload does not repeat a delivered wake.
    #[serde(default)]
    pub wake: bool,
    #[serde(default)]
    pub elapsed_secs: u64,
    /// Latest words of the worker (≤ 300 chars), for the wake preview.
    #[serde(default)]
    pub preview: String,
}

impl CarriedWorker {
    /// Whether the parent must be woken about this worker after the reload.
    pub fn needs_wake(&self) -> bool {
        self.wake
    }
}

/// At most this many reaped archives ride along a reload (paths only).
pub const MAX_CARRIED_ARCHIVES: usize = 1024;

/// `sa_12` → 12 (handle ids from `subagent_start`/`resume`/`subagent`).
pub fn handle_number(id: &str) -> Option<u64> {
    id.strip_prefix("sa_")?.parse().ok()
}

impl SubagentRegistry {
    /// Every worker this registry knows, for a daemon reload: live handles
    /// (stopped by the checkpoint beforehand) and reaped ids with an archive.
    /// Collected handles and reaped ids only carry their archive path, so a
    /// later `subagent_resume`/`subagent_collect` by the old id still works.
    /// `undelivered_wakes`: handle ids whose completion event is still queued.
    pub fn reload_carry(
        &self,
        undelivered_wakes: &std::collections::HashSet<String>,
    ) -> Vec<CarriedWorker> {
        let mut carried: Vec<CarriedWorker> = self
            .handles
            .values()
            .map(|h| {
                let s = h.state.read().unwrap_or_else(|p| p.into_inner());
                let terminal = s.finalization_ready || !matches!(s.status, SubagentStatus::Running);
                // Flagged by `stop_for_reload`, or still running: the exec
                // will destroy it either way (a user cancel still in flight
                // stays a user cancel).
                let stopped_by_reload =
                    s.stopped_by_reload || (!terminal && !s.cancel_requested);
                // A completion the parent never got: its wake is still
                // queued, or the finalizer had not pushed it yet (it finished
                // on its own during the checkpoint and outlasted the wait).
                // A cancel withholds the wake, so it owes none.
                let wake_lost = !stopped_by_reload
                    && !h.collected
                    && (undelivered_wakes.contains(&h.id)
                        || (!s.completion_settled && !s.cancel_requested));
                let latest = s.last_response_text().trim();
                let source = if latest.is_empty() { s.partial_text.trim() } else { latest };
                CarriedWorker {
                    id: h.id.clone(),
                    subagent_id: h.subagent_id,
                    agent: h.agent_name.clone(),
                    status: if terminal {
                        s.status.as_str().to_string()
                    } else {
                        // Not finalized in time: the exec destroys it, and
                        // its last archive (if any) is all that is left.
                        "cancelled".to_string()
                    },
                    archive_path: s.archive_path.clone(),
                    stopped_by_reload,
                    wake_lost,
                    wake: stopped_by_reload || wake_lost,
                    elapsed_secs: h.elapsed_secs() as u64,
                    preview: source.chars().take(300).collect(),
                }
            })
            .collect();
        let live: std::collections::HashSet<&String> = self.handles.keys().collect();
        let mut reaped: Vec<(&String, &std::path::PathBuf)> = self
            .archived
            .iter()
            .filter(|(id, _)| !live.contains(id))
            .collect();
        // Newest ids first when the cap bites.
        reaped.sort_by_key(|(id, _)| std::cmp::Reverse(handle_number(id)));
        carried.extend(reaped.into_iter().take(MAX_CARRIED_ARCHIVES).map(|(id, path)| {
            CarriedWorker {
                id: id.clone(),
                subagent_id: handle_number(id).unwrap_or(0),
                agent: String::new(),
                status: "archived".into(),
                archive_path: Some(path.clone()),
                stopped_by_reload: self.stopped_by_reload.contains(id),
                wake_lost: false,
                wake: false,
                elapsed_secs: 0,
                preview: String::new(),
            }
        }));
        // Wakes an earlier reload made this image post: carried again only
        // while still queued (this exec would discard it), never once the
        // parent has it, however many reloads follow.
        for (id, posted) in &self.reload_woken {
            let pending = undelivered_wakes.contains(id);
            match carried.iter_mut().find(|w| &w.id == id) {
                Some(w) if pending && !self.handles.contains_key(id) => {
                    *w = CarriedWorker { wake: true, ..posted.clone() };
                }
                Some(w) => w.wake = pending,
                None if pending => carried.push(CarriedWorker { wake: true, ..posted.clone() }),
                None => {}
            }
        }
        carried.sort_by_key(|w| (handle_number(&w.id), w.id.clone()));
        carried
    }

    /// Re-register a worker carried over a daemon reload: its id resolves to
    /// its archive (`subagent_status`/`collect`/`resume`). Never shadows a
    /// live handle.
    pub fn adopt_carried(&mut self, worker: &CarriedWorker) {
        if self.handles.contains_key(&worker.id) {
            return;
        }
        if let Some(path) = &worker.archive_path {
            self.archived.insert(worker.id.clone(), path.clone());
        }
        if worker.stopped_by_reload {
            self.stopped_by_reload.insert(worker.id.clone());
        }
    }

    /// This image posted the parent's wake for a carried worker
    /// (`AdoptWorkers`): see [`Self::reload_carry`].
    pub fn note_reload_wake(&mut self, worker: &CarriedWorker) {
        self.reload_woken.insert(worker.id.clone(), worker.clone());
    }

    /// Whether a daemon reload (not the user) stopped `id`'s run.
    pub fn was_stopped_by_reload(&self, id: &str) -> bool {
        self.stopped_by_reload.contains(id)
            || self.handles.get(id).is_some_and(|h| {
                h.state.read().unwrap_or_else(|p| p.into_inner()).stopped_by_reload
            })
    }

    /// Stop every running worker for a daemon reload: cancel it, flagged so
    /// the carried record and the next image say "stopped by reload", not
    /// "cancelled by user". Returns every worker whose finalizer has not
    /// settled, to await before the exec: the ones stopped here, one whose
    /// user cancel is already in flight, and one finishing on its own (its
    /// completion wake must reach the queue the reload carries).
    pub fn stop_for_reload(&mut self) -> Vec<(String, Arc<RwLock<SubagentState>>)> {
        let mut awaited = Vec::new();
        for h in self.handles.values_mut() {
            let stop = {
                // Decided under the lock the worker thread takes to record
                // its completion (only while no cancel is requested): the
                // worker either finished first (it keeps its completion) or
                // is stopped here (it ends cancelled), never both.
                let mut s = h.state.write().unwrap_or_else(|p| p.into_inner());
                if s.completion_settled {
                    continue;
                }
                let running = matches!(s.status, SubagentStatus::Running) && !s.finalization_started;
                let stop = running && !s.cancel_requested;
                if stop {
                    s.stopped_by_reload = true;
                    s.cancel_requested = true;
                }
                stop
            };
            if stop {
                h.cancel();
            }
            awaited.push((h.id.clone(), Arc::clone(&h.state)));
        }
        awaited.sort_by(|a, b| a.0.cmp(&b.0));
        awaited
    }
}

#[cfg(test)]
mod tests {
    use super::super::SubagentHandle;
    use super::*;

    /// A registered-looking handle whose thread never existed (channels
    /// dropped: `cancel()` only flags it).
    fn make_handle(id: &str) -> SubagentHandle {
        let (steer_tx, _) = tokio::sync::mpsc::unbounded_channel();
        let (shutdown_tx, _) = tokio::sync::oneshot::channel();
        let (_, result_rx) = tokio::sync::oneshot::channel();
        SubagentHandle::new(
            id.to_string(),
            handle_number(id).unwrap_or(0),
            "test-agent".to_string(),
            "test task".to_string(),
            "claude-sonnet-4-6".to_string(),
            "You are a test agent.".to_string(),
            300,
            Arc::new(RwLock::new(SubagentState::new())),
            Some(steer_tx),
            Some(shutdown_tx),
            Some(result_rx),
        )
    }

    fn make_finished_handle(id: &str) -> SubagentHandle {
        let h = make_handle(id);
        {
            let mut s = h.state.write().unwrap();
            s.status = SubagentStatus::Completed;
            s.finished_at = Some(std::time::Instant::now());
        }
        h
    }

    fn finalize_as(h: &SubagentHandle, status: SubagentStatus, archive: Option<&str>) {
        let mut s = h.state.write().unwrap();
        if s.cancel_requested {
            s.status = SubagentStatus::Cancelled;
        } else {
            s.status = status;
        }
        s.finished_at = Some(std::time::Instant::now());
        s.finalization_started = true;
        s.finalization_ready = true;
        s.completion_settled = true;
        s.archive_path = archive.map(std::path::PathBuf::from);
        s.partial_text = "latest words".into();
    }

    /// A reload stops exactly the running workers, flagged as the reload's
    /// doing; a user cancel already in flight stays the user's, but is still
    /// awaited (its archive must land before the exec). A worker that
    /// finished on its own but whose finalizer has not settled is awaited
    /// too (its wake must reach the carried queue), never stopped.
    #[test]
    fn stop_for_reload_cancels_running_workers_flagged_and_skips_finished() {
        let mut registry = SubagentRegistry::new();
        let running = make_handle("sa_4");
        let user_cancelled = make_handle("sa_5");
        let finished = make_finished_handle("sa_6");
        finalize_as(&finished, SubagentStatus::Completed, Some("/a/sa_6.json"));
        let finishing = make_finished_handle("sa_7");
        let (running_state, user_state, finished_state, finishing_state) = (
            Arc::clone(&running.state),
            Arc::clone(&user_cancelled.state),
            Arc::clone(&finished.state),
            Arc::clone(&finishing.state),
        );
        registry.register(running);
        registry.register(user_cancelled);
        registry.register(finished);
        registry.register(finishing);
        registry.get_mut("sa_5").unwrap().cancel();

        let stopping: Vec<String> = registry.stop_for_reload().into_iter().map(|(id, _)| id).collect();
        assert_eq!(stopping, ["sa_4", "sa_5", "sa_7"]);
        let r = running_state.read().unwrap();
        assert!(r.cancel_requested && r.stopped_by_reload);
        let u = user_state.read().unwrap();
        assert!(u.cancel_requested && !u.stopped_by_reload, "a user cancel stays a user cancel");
        let f = finished_state.read().unwrap();
        assert!(!f.cancel_requested && !f.stopped_by_reload);
        let g = finishing_state.read().unwrap();
        assert!(!g.cancel_requested && !g.stopped_by_reload, "its completion stands");
        assert_eq!(g.status, SubagentStatus::Completed);
        drop((r, u, f, g));
        // Idempotent: nothing new to flag, still awaited until settled.
        assert_eq!(registry.stop_for_reload().len(), 3);
    }

    /// The race the checkpoint must not lose: a worker finishes on its own
    /// while the reload stops the others. Until its finalizer settles, its
    /// wake may not be in the carried queue yet: it is owed (lost) unless it
    /// was cancelled. Once settled, only a still-queued wake is owed.
    #[test]
    fn a_worker_finishing_during_the_checkpoint_keeps_its_wake() {
        let mut registry = SubagentRegistry::new();
        registry.register(make_finished_handle("sa_20"));
        let cancelled = make_finished_handle("sa_21");
        cancelled.state.write().unwrap().cancel_requested = true;
        registry.register(cancelled);
        let none = std::collections::HashSet::new();
        let carried = registry.reload_carry(&none);
        let by = |id: &str| carried.iter().find(|w| w.id == id).unwrap().clone();
        let w = by("sa_20");
        assert!(w.wake_lost && w.wake && !w.stopped_by_reload, "{w:?}");
        assert_eq!(w.status, "completed");
        assert!(!by("sa_21").wake, "a cancel owes no wake");

        // Settled, and its wake delivered: nothing owed.
        finalize_as(registry.get("sa_20").unwrap(), SubagentStatus::Completed, Some("/a/sa_20.json"));
        assert!(!registry.reload_carry(&none).iter().any(|w| w.wake));
        // Settled, wake still queued: owed.
        let queued: std::collections::HashSet<String> = ["sa_20".to_string()].into();
        let w = registry.reload_carry(&queued).into_iter().find(|w| w.id == "sa_20").unwrap();
        assert!(w.wake_lost && w.wake);
    }

    /// What a reload records per worker, and how the next image's registry
    /// answers for those ids afterwards.
    #[test]
    fn reload_carry_then_adopt_resolves_old_ids_to_their_archives() {
        let mut old = SubagentRegistry::new();
        // Stopped by the reload and finalized in time (archive written).
        let stopped = make_handle("sa_10");
        // Stopped, but did not finalize before the exec.
        let stuck = make_handle("sa_11");
        // Finished before the reload, wake still queued (undelivered).
        let owed = make_finished_handle("sa_12");
        // Finished, wake already delivered, not yet collected.
        let delivered = make_finished_handle("sa_13");
        // Collected.
        let mut collected = make_finished_handle("sa_14");
        collected.mark_collected();
        for (h, path) in [
            (&owed, "/a/sa_12.json"),
            (&delivered, "/a/sa_13.json"),
            (&collected, "/a/sa_14.json"),
        ] {
            finalize_as(h, SubagentStatus::Completed, Some(path));
        }
        let stopped_state = Arc::clone(&stopped.state);
        for h in [stopped, stuck, owed, delivered, collected] {
            old.register(h);
        }
        old.archived.insert("sa_3".into(), "/a/sa_3.json".into());
        assert_eq!(old.stop_for_reload().len(), 2);
        {
            let h = old.get("sa_10").unwrap();
            finalize_as(h, SubagentStatus::Completed, Some("/a/sa_10.json"));
        }
        assert_eq!(stopped_state.read().unwrap().status, SubagentStatus::Cancelled);

        let undelivered: std::collections::HashSet<String> = ["sa_12".to_string()].into();
        let carried = old.reload_carry(&undelivered);
        let ids: Vec<&str> = carried.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, ["sa_3", "sa_10", "sa_11", "sa_12", "sa_13", "sa_14"]);
        let by = |id: &str| carried.iter().find(|w| w.id == id).unwrap().clone();

        let w = by("sa_10");
        assert!(w.stopped_by_reload && !w.wake_lost && w.needs_wake());
        assert_eq!(w.status, "cancelled");
        assert_eq!(w.archive_path.as_deref(), Some(std::path::Path::new("/a/sa_10.json")));
        assert_eq!(w.preview, "latest words");
        assert_eq!(w.subagent_id, 10);

        let w = by("sa_11");
        assert!(w.stopped_by_reload && w.needs_wake(), "the exec destroys it: still announced");
        assert_eq!(w.archive_path, None);

        let w = by("sa_12");
        assert!(!w.stopped_by_reload && w.wake_lost && w.needs_wake());
        assert_eq!(w.status, "completed");
        assert!(!by("sa_13").needs_wake(), "its wake was delivered: no duplicate");
        assert!(!by("sa_14").needs_wake());
        let w = by("sa_3");
        assert!(!w.needs_wake());
        assert_eq!(w.status, "archived");

        // The next image (as `adopt_workers` does it).
        let mut new = SubagentRegistry::new();
        for w in &carried {
            new.adopt_carried(w);
            if w.needs_wake() {
                new.note_reload_wake(w);
            }
        }
        assert_eq!(new.archive_path("sa_10"), Some("/a/sa_10.json".into()));
        assert_eq!(new.archive_path("sa_3"), Some("/a/sa_3.json".into()));
        assert_eq!(new.archive_path("sa_11"), None, "no archive: nothing to resolve");
        assert!(new.was_stopped_by_reload("sa_10"));
        assert!(new.was_stopped_by_reload("sa_11"));
        assert!(!new.was_stopped_by_reload("sa_12"));
        assert!(new.list_active().is_empty(), "carried workers are not live");
        assert!(new.display_rows().is_empty(), "nor shown as running");

        // A live handle is never shadowed by a carried record.
        let mut live = SubagentRegistry::new();
        let h = make_handle("sa_10");
        finalize_as(&h, SubagentStatus::Completed, Some("/live/sa_10.json"));
        live.register(h);
        live.adopt_carried(&by("sa_10"));
        assert_eq!(live.archive_path("sa_10"), Some("/live/sa_10.json".into()));

        // A second reload, every wake delivered: the records ride along
        // (old ids keep resolving, the label stays) but nobody is woken
        // again — a repeated "resume it" would start a duplicate worker.
        let again = new.reload_carry(&Default::default());
        assert!(again.iter().all(|w| !w.needs_wake()), "{again:?}");
        let labelled: Vec<&str> =
            again.iter().filter(|w| w.stopped_by_reload).map(|w| w.id.as_str()).collect();
        // Only ids with an archive survive as records (sa_11 had none).
        assert_eq!(labelled, ["sa_10"]);

        // A second reload before the parent got its wakes: exactly the
        // undelivered ones are carried again, as first posted.
        let queued: std::collections::HashSet<String> =
            ["sa_11".to_string(), "sa_12".to_string()].into();
        let again = new.reload_carry(&queued);
        let woken: Vec<&CarriedWorker> = again.iter().filter(|w| w.needs_wake()).collect();
        assert_eq!(woken.iter().map(|w| w.id.as_str()).collect::<Vec<_>>(), ["sa_11", "sa_12"]);
        assert_eq!(*woken[0], by("sa_11"), "the stuck worker's stop, as posted");
        assert_eq!(*woken[1], by("sa_12"), "the lost completion, as posted");
        assert!(!again.iter().any(|w| w.id == "sa_10" && w.needs_wake()));
    }

    #[test]
    fn reload_carry_caps_reaped_archives_keeping_the_newest() {
        let mut registry = SubagentRegistry::new();
        for n in 0..(MAX_CARRIED_ARCHIVES as u64 + 10) {
            registry
                .archived
                .insert(format!("sa_{n}"), format!("/a/sa_{n}.json").into());
        }
        let carried = registry.reload_carry(&Default::default());
        assert_eq!(carried.len(), MAX_CARRIED_ARCHIVES);
        assert_eq!(carried.first().unwrap().id, "sa_10");
        assert_eq!(carried.last().unwrap().id, format!("sa_{}", MAX_CARRIED_ARCHIVES as u64 + 9));
    }

    #[test]
    fn carried_worker_reads_records_without_the_optional_fields() {
        let w: CarriedWorker =
            serde_json::from_value(serde_json::json!({"id": "sa_7", "status": "cancelled"})).unwrap();
        assert_eq!(w.id, "sa_7");
        assert!(!w.needs_wake());
        assert_eq!(handle_number("sa_7"), Some(7));
        assert_eq!(handle_number("x_7"), None);
    }
}
