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
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
    /// The daemon resumed this worker after an earlier reload: the run that
    /// continues it. Its id keeps following that run in the next image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumed_as: Option<String>,
    /// This run is the daemon's own resume of a reload-stopped worker (see
    /// [`DAEMON_RESUME_MIN_RUN_SECS`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub resumed_by_daemon: bool,
    /// A forced reload went ahead although this run's record was not
    /// durable or still being written (spec §5): the wake says so.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub record_waived: bool,
    /// A v2 run stopped without its final record (`archive_path` is
    /// `None`): its head record, the crash checkpoint `subagent_resume`
    /// continues from by this path. The next image marks it cancelled when
    /// it leaves the worker to the model. Absent from older images.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crash_head: Option<std::path::PathBuf>,
    /// The run whose v2 final `archive_path` names (`ArchivedRecord::
    /// v2_final_run`), so the next image reads that final as this run's
    /// own. `None`: a v1 archive, or a record an older image wrote (its
    /// path then reads as a v1 archive, which refuses a v2 final: collect
    /// and status fail closed). Older images ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub v2_final_run: Option<String>,
}

impl CarriedWorker {
    /// Whether the parent must be woken about this worker after the reload.
    pub fn needs_wake(&self) -> bool {
        self.wake
    }
}

/// At most this many reaped archives ride along a reload (paths only).
pub const MAX_CARRIED_ARCHIVES: usize = 1024;

/// At most this many reload-stopped workers per session are resumed by the
/// daemon after one reload, oldest handle first; the rest are left to the
/// model with today's wake. Only a worker actually resumed counts: a
/// launch that is refused (the resume size rule) or fails takes no slot,
/// and the next worker in line gets the try ([`DaemonResumeSlots`]). The
/// same as the default `subagent.max_concurrent`, so the workers the daemon
/// resumes can all run at once.
pub const DAEMON_RESUME_CAP: usize = 16;

/// Once per reload generation: a run the daemon itself started by resuming
/// a reload-stopped worker is resumed again by the daemon after the next
/// reload only if it had run at least this long (10 minutes) when that
/// reload stopped it. A worker cut again sooner (reloads in quick
/// succession) is left to the model, so a reload loop can't keep
/// re-launching it.
pub const DAEMON_RESUME_MIN_RUN_SECS: u64 = 600;

/// What the new image does about one worker whose wake it owes the parent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonResume {
    /// The daemon resumes it from its archive.
    Resume,
    /// Left to the model: today's wake, with this reason added.
    Leave(String),
}

/// [`DaemonResume`] for each of `woken` (in order): `None` for a worker the
/// reload did not stop (a lost completion keeps its plain wake). A worker
/// the reload stopped is resumed when its conversation was archived and it
/// is not a daemon resume cut again within [`DAEMON_RESUME_MIN_RUN_SECS`].
/// The cap is not applied here: the launches are tried in
/// [`daemon_resume_order`], and [`DaemonResumeSlots`] counts the ones that
/// started a run.
pub fn plan_daemon_resumes(woken: &[&CarriedWorker]) -> Vec<Option<DaemonResume>> {
    let mut plan: Vec<Option<DaemonResume>> = vec![None; woken.len()];
    for (i, w) in woken.iter().enumerate() {
        if !w.stopped_by_reload {
            continue;
        }
        plan[i] = Some(if w.archive_path.is_none() && w.crash_head.is_some() {
            // Its head is the crash checkpoint: the model resumes it by
            // that path if it wants it (it may predate the last work).
            DaemonResume::Leave("its final record was lost".into())
        } else if w.archive_path.is_none() {
            DaemonResume::Leave("its conversation was not archived".into())
        } else if w.resumed_by_daemon && w.elapsed_secs < DAEMON_RESUME_MIN_RUN_SECS {
            DaemonResume::Leave(format!(
                "the daemon already resumed it after the previous reload and it ran only {}s since",
                w.elapsed_secs
            ))
        } else {
            DaemonResume::Resume
        });
    }
    plan
}

/// The order the daemon tries its resumes in: the indexes of `plan`'s
/// [`DaemonResume::Resume`] entries (one per worker of `woken`), oldest
/// handle first.
pub fn daemon_resume_order(woken: &[&CarriedWorker], plan: &[Option<DaemonResume>]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..woken.len())
        .filter(|&i| matches!(plan.get(i), Some(Some(DaemonResume::Resume))))
        .collect();
    order.sort_by_key(|&i| (handle_number(&woken[i].id), woken[i].id.clone()));
    order
}

/// The daemon's resume slots for one session after one reload: at most
/// [`DAEMON_RESUME_CAP`] workers resumed. Only a launch that started a run
/// takes a slot; one refused (the resume size rule) or failed leaves it to
/// the next worker in line.
#[derive(Debug, Default)]
pub struct DaemonResumeSlots {
    resumed: usize,
}

impl DaemonResumeSlots {
    /// `Some(why)` once every slot is taken: the next worker is left to the
    /// model with this reason.
    pub fn full(&self) -> Option<String> {
        (self.resumed >= DAEMON_RESUME_CAP).then(|| {
            format!("the daemon resumes at most {DAEMON_RESUME_CAP} workers per session after a reload")
        })
    }

    /// A launch ended; `resumed`: it started the run that continues the
    /// worker.
    pub fn launched(&mut self, resumed: bool) {
        if resumed {
            self.resumed += 1;
        }
    }
}

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
                    resumed_as: None,
                    resumed_by_daemon: self.resumed_by_daemon.contains(&h.id),
                    record_waived: crate::tools::worker_records::record_waived(&h.id),
                    // A v2 run whose final was not published (its write
                    // failed, or the exec comes first): its head is the
                    // crash checkpoint the next image names and marks.
                    crash_head: if s.v2_records && s.archive_path.is_none() {
                        s.v2_head.clone()
                    } else {
                        None
                    },
                    // Set only with the final's path (`archived_record`).
                    v2_final_run: s.archive_path.as_ref().and(s.v2_final_run.clone()),
                }
            })
            .collect();
        let live: std::collections::HashSet<&String> = self.handles.keys().collect();
        let mut reaped: Vec<(&String, &super::ArchivedRecord)> = self
            .archived
            .iter()
            .filter(|(id, _)| !live.contains(id))
            .collect();
        // Newest ids first when the cap bites.
        reaped.sort_by_key(|(id, _)| std::cmp::Reverse(handle_number(id)));
        // A path and, for a v2 final (here or on a live handle above), the
        // run that published it: the next image reads that final only as
        // that run's (resume reads records by their file either way).
        carried.extend(reaped.into_iter().take(MAX_CARRIED_ARCHIVES).map(|(id, record)| {
            CarriedWorker {
                id: id.clone(),
                subagent_id: handle_number(id).unwrap_or(0),
                agent: String::new(),
                status: "archived".into(),
                archive_path: Some(record.path.clone()),
                stopped_by_reload: self.stopped_by_reload.contains(id),
                wake_lost: false,
                wake: false,
                elapsed_secs: 0,
                preview: String::new(),
                resumed_as: None,
                resumed_by_daemon: self.resumed_by_daemon.contains(id),
                // An ended run: it owns nothing a reload could waive.
                record_waived: false,
                crash_head: None,
                v2_final_run: record.v2_final_run.clone(),
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
        // The daemon's own resumes: the old id keeps following the run that
        // continues it. One still being launched when this reload came
        // (not reached today: the actor launches before it takes another
        // command) is announced the old way and not resumed twice.
        for (old, resumed) in &self.daemon_resumes {
            if let Some(w) = carried.iter_mut().find(|w| &w.id == old) {
                match resumed {
                    Some(new) => w.resumed_as = Some(new.clone()),
                    None => {
                        w.wake = true;
                        w.resumed_by_daemon = true;
                        w.elapsed_secs = 0;
                    }
                }
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
            self.archived.insert(
                worker.id.clone(),
                super::ArchivedRecord {
                    path: path.clone(),
                    v2_final_run: worker.v2_final_run.clone(),
                },
            );
        }
        if worker.stopped_by_reload {
            self.stopped_by_reload.insert(worker.id.clone());
        }
        if let Some(new) = &worker.resumed_as {
            self.daemon_resumes.insert(worker.id.clone(), Some(new.clone()));
        }
        if worker.resumed_by_daemon {
            self.resumed_by_daemon.insert(worker.id.clone());
        }
    }

    /// This registry already holds `id` (a running or adopted worker),
    /// already posted its carried wake, or already resumed it: a repeated
    /// `AdoptWorkers` (the same record restored twice into one session)
    /// adopts, wakes and resumes nothing.
    pub fn adopted_already(&self, id: &str) -> bool {
        self.handles.contains_key(id)
            || self.reload_woken.contains_key(id)
            || self.daemon_resumes.contains_key(id)
    }

    /// The daemon starts resuming `old` (see [`Self::resumed_as`]).
    pub fn begin_daemon_resume(&mut self, old: &str) {
        self.daemon_resumes.insert(old.to_string(), None);
    }

    /// The daemon's resume of `old` ended: `Some(new)` started the run that
    /// continues it, `None` failed (the old id is the model's again).
    pub fn finish_daemon_resume(&mut self, old: &str, new: Option<&str>) {
        match new {
            Some(new) => {
                self.daemon_resumes.insert(old.to_string(), Some(new.to_string()));
                self.resumed_by_daemon.insert(new.to_string());
            }
            None => {
                self.daemon_resumes.remove(old);
            }
        }
    }

    /// Whether the daemon resumed `id` after a reload: `None` it did not,
    /// `Some(None)` its launch is under way, `Some(Some(new))` the run that
    /// continues it now (followed across later daemon resumes of that run).
    pub fn resumed_as(&self, id: &str) -> Option<Option<String>> {
        let Some(mut current) = self.daemon_resumes.get(id)?.clone() else {
            return Some(None);
        };
        // Bounded: each hop is a later reload's resume.
        for _ in 0..64 {
            match self.daemon_resumes.get(&current) {
                Some(Some(next)) => current = next.clone(),
                Some(None) => return Some(None),
                None => break,
            }
        }
        Some(Some(current))
    }

    /// This image posted the parent's wake for a carried worker
    /// (`AdoptWorkers`): see [`Self::reload_carry`].
    pub fn note_reload_wake(&mut self, worker: &CarriedWorker) {
        self.reload_woken.insert(worker.id.clone(), worker.clone());
    }

    /// A reload that did not exec: what of its record still owes the parent
    /// a wake in THIS image. Only the workers the reload stopped — their
    /// finalizers withheld the completion wake, expecting the next image to
    /// post one. A completion it carried as `wake_lost` is still queued
    /// here (or still coming from its finalizer), and a reload wake an
    /// earlier image posted is still queued too: re-posting either would
    /// wake the parent twice. Everything else keeps its record, `wake` off.
    pub fn rewake_after_failed_exec(&self, carried: Vec<CarriedWorker>) -> Vec<CarriedWorker> {
        carried
            .into_iter()
            .map(|w| {
                let wake = w.wake && w.stopped_by_reload && !self.reload_woken.contains_key(&w.id);
                CarriedWorker { wake, ..w }
            })
            .collect()
    }

    /// Whether a daemon reload (not the user) stopped `id`'s run.
    pub fn was_stopped_by_reload(&self, id: &str) -> bool {
        self.stopped_by_reload.contains(id)
            || self.handles.get(id).is_some_and(|h| {
                h.state.read().unwrap_or_else(|p| p.into_inner()).stopped_by_reload
            })
    }

    /// The reload did not exec: launches work again, under the launch fence
    /// that was there before the reload (a revoked driver's stays). A no-op
    /// without a reload fence.
    pub fn lift_reload_fence(&mut self) {
        if let Some(previous) = self.reload_fence.take() {
            self.set_spawn_cancellation(previous);
        }
    }

    /// Stop every running worker for a daemon reload: cancel it, flagged so
    /// the carried record and the next image say "stopped by reload", not
    /// "cancelled by user". Returns every worker whose finalizer has not
    /// settled, to await before the exec: the ones stopped here, one whose
    /// user cancel is already in flight, and one finishing on its own (its
    /// completion wake must reach the queue the reload carries).
    ///
    /// Installs the reload fence (idempotent): a worker registered from now
    /// until [`Self::lift_reload_fence`] is stopped at birth, flagged the
    /// same way. Call again after the turn cancel to collect those too.
    pub fn stop_for_reload(&mut self) -> Vec<(String, Arc<RwLock<SubagentState>>)> {
        if self.reload_fence.is_none() {
            let fence = crate::CancellationToken::new();
            fence.cancel();
            let previous = self.spawn_cancellation.take();
            self.set_spawn_cancellation(Some(fence));
            self.reload_fence = Some(previous);
        }
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

    /// Train30 item 4b: a reload carries a v2 run's head as its crash
    /// checkpoint when its final record was not published (a stop whose
    /// final write failed, or a run not finalized before the exec), never
    /// beside a published final; the next image leaves it to the model,
    /// saying its final was lost. The field rides the reload record and is
    /// absent when unset.
    #[test]
    fn a_v2_run_without_its_final_carries_its_head_as_the_crash_checkpoint() {
        let mut registry = SubagentRegistry::new();
        let head = std::path::PathBuf::from("/a/1-x/sa_40.head.json");
        let stopped_handle = |id: &str, head: &str| {
            let h = make_handle(id);
            {
                let mut s = h.state.write().unwrap();
                s.v2_records = true;
                s.v2_head = Some(head.into());
                s.cancel_requested = true;
                s.stopped_by_reload = true;
            }
            h
        };
        let failed = stopped_handle("sa_40", "/a/1-x/sa_40.head.json");
        finalize_as(&failed, SubagentStatus::Cancelled, None);
        let published = stopped_handle("sa_41", "/a/1-x/sa_41.head.json");
        finalize_as(&published, SubagentStatus::Cancelled, Some("/a/1-x/sa_41.json"));
        // Still running at the exec: never finalized.
        let unfinished = make_handle("sa_42");
        {
            let mut s = unfinished.state.write().unwrap();
            s.v2_records = true;
            s.v2_head = Some("/a/1-x/sa_42.head.json".into());
        }
        registry.register(failed);
        registry.register(published);
        registry.register(unfinished);
        let carried = registry.reload_carry(&Default::default());
        let by = |id: &str| carried.iter().find(|w| w.id == id).unwrap().clone();
        let w = by("sa_40");
        assert!(w.stopped_by_reload && w.archive_path.is_none());
        assert_eq!(w.crash_head, Some(head.clone()));
        assert_eq!(by("sa_41").crash_head, None, "its final is published");
        assert_eq!(
            by("sa_42").crash_head,
            Some("/a/1-x/sa_42.head.json".into()),
            "not finalized before the exec"
        );
        let back: CarriedWorker =
            serde_json::from_value(serde_json::to_value(&w).unwrap()).unwrap();
        assert_eq!(back.crash_head, Some(head));
        assert!(serde_json::to_value(by("sa_41")).unwrap().get("crash_head").is_none());

        assert_eq!(
            plan_daemon_resumes(&[&w]),
            vec![Some(DaemonResume::Leave("its final record was lost".into()))]
        );
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
        old.archived.insert(
            "sa_3".into(),
            super::super::ArchivedRecord {
                path: "/a/sa_3.json".into(),
                v2_final_run: None,
            },
        );
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

    /// A worker registered after the reload stopped the others (a turn's
    /// `subagent_start` racing the checkpoint) is stopped at birth, as the
    /// reload's doing: carried and announced like the rest, never labelled
    /// a user cancel that owes nobody a wake. A failed exec lifts the fence
    /// and restores the launch fence that was there before.
    #[test]
    fn a_worker_registered_after_the_reload_stop_is_stopped_by_the_reload() {
        let mut registry = SubagentRegistry::new();
        registry.register(make_handle("sa_1"));
        assert_eq!(registry.stop_for_reload().len(), 1);

        let late = make_handle("sa_2");
        let late_state = Arc::clone(&late.state);
        registry.register_with_cancellation(late, None);
        {
            let s = late_state.read().unwrap();
            assert!(s.cancel_requested && s.stopped_by_reload, "stopped at birth, by the reload");
        }
        // Even when the turn's own launch token is already cancelled (the
        // checkpoint's `cancel_turn`), the reload's label wins.
        let turn = crate::CancellationToken::new();
        turn.cancel();
        let racing = make_handle("sa_3");
        let racing_state = Arc::clone(&racing.state);
        registry.register_with_cancellation(racing, Some(&turn));
        assert!(racing_state.read().unwrap().stopped_by_reload);
        // The turn cancel after the stop leaves them the reload's.
        assert!(registry.cancel_running(super::super::WorkerCancelScope::All).is_empty());
        // Both are awaited before the exec (their archives) and carried.
        let awaited: Vec<String> = registry.stop_for_reload().into_iter().map(|(id, _)| id).collect();
        assert_eq!(awaited, ["sa_1", "sa_2", "sa_3"]);
        let carried = registry.reload_carry(&Default::default());
        for id in ["sa_2", "sa_3"] {
            let w = carried.iter().find(|w| w.id == id).unwrap();
            assert!(w.stopped_by_reload && w.needs_wake(), "{w:?}");
        }

        // Failed exec: launches work again.
        registry.lift_reload_fence();
        let after = make_handle("sa_4");
        let after_state = Arc::clone(&after.state);
        registry.register(after);
        assert!(!after_state.read().unwrap().cancel_requested);

        // The fence that was there before the reload comes back (a revoked
        // driver's cancelled launch fence keeps cancelling late launches).
        let mut fenced = SubagentRegistry::new();
        let driver = crate::CancellationToken::new();
        driver.cancel();
        fenced.set_spawn_cancellation(Some(driver));
        fenced.stop_for_reload();
        fenced.lift_reload_fence();
        let late = make_handle("sa_9");
        let late_state = Arc::clone(&late.state);
        fenced.register(late);
        let s = late_state.read().unwrap();
        assert!(s.cancel_requested && !s.stopped_by_reload, "the driver's fence, not the reload's");
    }

    /// A failed exec re-announces only the workers that reload stopped: the
    /// completions it carried are still in this image's queue, and so is a
    /// reload wake posted earlier.
    #[test]
    fn a_failed_exec_rewakes_only_the_workers_the_reload_stopped() {
        let mut registry = SubagentRegistry::new();
        registry.register(make_handle("sa_1"));
        registry.register(make_finished_handle("sa_2"));
        let stopped_earlier = CarriedWorker {
            id: "sa_3".into(),
            subagent_id: 3,
            agent: String::new(),
            status: "cancelled".into(),
            archive_path: Some("/a/sa_3.json".into()),
            stopped_by_reload: true,
            wake_lost: false,
            wake: true,
            elapsed_secs: 0,
            preview: String::new(),
            ..Default::default()
        };
        registry.adopt_carried(&stopped_earlier);
        registry.note_reload_wake(&stopped_earlier);
        registry.stop_for_reload();
        let queued: std::collections::HashSet<String> = ["sa_3".to_string()].into();
        let carried = registry.reload_carry(&queued);
        assert_eq!(carried.iter().filter(|w| w.needs_wake()).count(), 3, "{carried:?}");

        let rewake = registry.rewake_after_failed_exec(carried.clone());
        let woken: Vec<&str> =
            rewake.iter().filter(|w| w.needs_wake()).map(|w| w.id.as_str()).collect();
        assert_eq!(woken, ["sa_1"], "only this reload's own stop: {rewake:?}");
        assert_eq!(rewake.len(), carried.len(), "every record is kept");
    }

    #[test]
    fn reload_carry_caps_reaped_archives_keeping_the_newest() {
        let mut registry = SubagentRegistry::new();
        for n in 0..(MAX_CARRIED_ARCHIVES as u64 + 10) {
            registry.archived.insert(
                format!("sa_{n}"),
                super::super::ArchivedRecord {
                    path: format!("/a/sa_{n}.json").into(),
                    v2_final_run: None,
                },
            );
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

    /// The run that published a v2 final rides along a reload with its path
    /// (a live handle, a tombstone and a reaped record alike), through the
    /// reload record's JSON and every later reload, so the next image reads
    /// that final as the run's own. A v1 archive carries none, and its
    /// record keeps the bytes an older image wrote and reads.
    #[test]
    fn a_v2_final_carries_its_run_across_the_reload_record() {
        let mut old = SubagentRegistry::new();
        let v2_final = |id: &str, run: &str| {
            let h = make_finished_handle(id);
            finalize_as(&h, SubagentStatus::Completed, Some(&format!("/a/{id}.json")));
            h.state.write().unwrap().v2_final_run = Some(run.into());
            h
        };
        old.register(v2_final("sa_20", "run-20"));
        old.register(v2_final("sa_21", "run-21"));
        old.release_finished_resources("sa_21");
        let v1 = make_finished_handle("sa_22");
        finalize_as(&v1, SubagentStatus::Completed, Some("/a/sa_22.json"));
        old.register(v1);
        for (id, run) in [("sa_3", Some("run-3")), ("sa_4", None)] {
            old.archived.insert(
                id.into(),
                super::super::ArchivedRecord {
                    path: format!("/a/{id}.json").into(),
                    v2_final_run: run.map(String::from),
                },
            );
        }
        let record = |id: &str, run: Option<&str>| {
            Some(super::super::ArchivedRecord {
                path: format!("/a/{id}.json").into(),
                v2_final_run: run.map(String::from),
            })
        };
        let runs = [
            ("sa_3", Some("run-3")),
            ("sa_4", None),
            ("sa_20", Some("run-20")),
            ("sa_21", Some("run-21")),
            ("sa_22", None),
        ];

        let carried = old.reload_carry(&Default::default());
        let json = serde_json::to_string(&carried).unwrap();
        let back: Vec<CarriedWorker> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, carried);
        let mut new = SubagentRegistry::new();
        for w in &back {
            new.adopt_carried(w);
        }
        for (id, run) in runs {
            assert_eq!(new.archived_record(id), record(id, run), "{id}");
        }
        // And again at the next reload, from the adopted records.
        let again = new.reload_carry(&Default::default());
        let mut newer = SubagentRegistry::new();
        for w in &again {
            newer.adopt_carried(w);
        }
        for (id, run) in runs {
            assert_eq!(newer.archived_record(id), record(id, run), "{id}");
        }

        // A v1 entry writes no key: the bytes an older image wrote.
        for w in serde_json::to_value(&carried).unwrap().as_array().unwrap() {
            let v2 = matches!(w["id"].as_str(), Some("sa_3" | "sa_20" | "sa_21"));
            assert_eq!(w.get("v2_final_run").is_some(), v2, "{w}");
        }
        // An older image's struct (no such field) reads the new record.
        #[derive(serde::Deserialize)]
        struct OlderCarriedWorker {
            id: String,
            status: String,
            #[serde(default)]
            archive_path: Option<std::path::PathBuf>,
        }
        let older: Vec<OlderCarriedWorker> = serde_json::from_str(&json).unwrap();
        assert_eq!(
            older.iter().map(|w| (&w.id, &w.status, &w.archive_path)).collect::<Vec<_>>(),
            carried.iter().map(|w| (&w.id, &w.status, &w.archive_path)).collect::<Vec<_>>()
        );
        // A record an older image wrote names no run: its path is adopted
        // as a v1 archive (a v2 final there is then refused, never read).
        let w: CarriedWorker = serde_json::from_value(serde_json::json!({
            "id": "sa_9", "status": "archived", "archive_path": "/a/sa_9.json"
        }))
        .unwrap();
        new.adopt_carried(&w);
        assert_eq!(new.archived_record("sa_9"), record("sa_9", None));
    }

    fn stopped(id: &str, archived: bool) -> CarriedWorker {
        CarriedWorker {
            id: id.into(),
            subagent_id: handle_number(id).unwrap_or(0),
            agent: "digger".into(),
            status: "cancelled".into(),
            archive_path: archived.then(|| format!("/a/{id}.json").into()),
            stopped_by_reload: true,
            wake: true,
            elapsed_secs: 30,
            ..Default::default()
        }
    }

    /// What the next image resumes itself: a reload-stopped worker with an
    /// archive. Not a lost completion (its plain wake stays), not one with
    /// no archive (mutant: drop the archive check). The plan caps nothing
    /// (the cap counts the runs started, [`DaemonResumeSlots`]); the
    /// launches go oldest handle first, whatever order the record hands
    /// the workers over in.
    #[test]
    fn the_daemon_tries_every_archived_reload_stopped_worker_oldest_first() {
        let lost = CarriedWorker { stopped_by_reload: false, wake_lost: true, ..stopped("sa_2", true) };
        let unarchived = stopped("sa_3", false);
        // Twenty candidates, more than the cap, out of handle order.
        let mut candidates: Vec<CarriedWorker> = (4..24).rev().map(|n| stopped(&format!("sa_{n}"), true)).collect();
        candidates.push(lost.clone());
        candidates.push(unarchived.clone());
        let woken: Vec<&CarriedWorker> = candidates.iter().collect();
        let plan = plan_daemon_resumes(&woken);
        let of = |id: &str| plan[candidates.iter().position(|w| w.id == id).unwrap()].clone();
        assert_eq!(of("sa_2"), None, "a lost completion keeps its plain wake");
        assert_eq!(
            of("sa_3"),
            Some(DaemonResume::Leave("its conversation was not archived".into()))
        );
        for n in 4..24 {
            assert_eq!(of(&format!("sa_{n}")), Some(DaemonResume::Resume), "sa_{n}");
        }
        let order: Vec<&str> =
            daemon_resume_order(&woken, &plan).into_iter().map(|i| woken[i].id.as_str()).collect();
        let oldest_first: Vec<String> = (4..24).map(|n| format!("sa_{n}")).collect();
        assert_eq!(order, oldest_first);
    }

    /// The cap counts workers actually resumed: sixteen per session after
    /// one reload, and a launch refused or failed takes no slot (mutant:
    /// every launch takes one).
    #[test]
    fn only_a_worker_actually_resumed_takes_one_of_sixteen_slots() {
        let mut slots = DaemonResumeSlots::default();
        slots.launched(false);
        slots.launched(false);
        for n in 0..DAEMON_RESUME_CAP {
            assert_eq!(slots.full(), None, "slot {n} is free");
            slots.launched(true);
        }
        assert_eq!(
            slots.full().as_deref(),
            Some("the daemon resumes at most 16 workers per session after a reload")
        );
        assert_eq!(DAEMON_RESUME_CAP, 16);
    }

    /// Once per reload generation: a run the daemon started is resumed by
    /// it again only if it ran at least ten minutes before the next reload
    /// cut it (mutant: drop the `resumed_by_daemon` check).
    #[test]
    fn a_daemon_resume_cut_again_within_ten_minutes_is_left_to_the_model() {
        let quick = CarriedWorker { resumed_by_daemon: true, elapsed_secs: 599, ..stopped("sa_5", true) };
        let long = CarriedWorker { resumed_by_daemon: true, elapsed_secs: 600, ..stopped("sa_6", true) };
        let first = CarriedWorker { elapsed_secs: 5, ..stopped("sa_7", true) };
        let plan = plan_daemon_resumes(&[&quick, &long, &first]);
        let Some(DaemonResume::Leave(why)) = &plan[0] else { panic!("{plan:?}") };
        assert!(why.contains("already resumed it after the previous reload"), "{why}");
        assert_eq!(plan[1], Some(DaemonResume::Resume));
        assert_eq!(plan[2], Some(DaemonResume::Resume), "a first stop is resumed however short");
        assert_eq!(DAEMON_RESUME_MIN_RUN_SECS, 600);
    }

    /// The old id follows the run the daemon resumed it as, in this image
    /// and the next: a repeated `AdoptWorkers` resumes nothing again, and
    /// the resumed run is carried as the daemon's own.
    #[test]
    fn a_daemon_resume_alias_survives_the_next_reload() {
        let mut registry = SubagentRegistry::new();
        registry.adopt_carried(&stopped("sa_4", true));
        assert_eq!(registry.resumed_as("sa_4"), None);
        assert!(!registry.adopted_already("sa_4"));
        registry.begin_daemon_resume("sa_4");
        assert_eq!(registry.resumed_as("sa_4"), Some(None), "being resumed");
        assert!(registry.adopted_already("sa_4"), "a repeated adopt launches nothing");
        registry.finish_daemon_resume("sa_4", Some("sa_9"));
        assert_eq!(registry.resumed_as("sa_4"), Some(Some("sa_9".into())));
        registry.register(make_handle("sa_9"));

        // The next reload stops sa_9 and carries both.
        registry.stop_for_reload();
        finalize_as(registry.get("sa_9").unwrap(), SubagentStatus::Completed, Some("/a/sa_9.json"));
        let carried = registry.reload_carry(&Default::default());
        let by = |id: &str| carried.iter().find(|w| w.id == id).unwrap().clone();
        assert_eq!(by("sa_4").resumed_as.as_deref(), Some("sa_9"));
        assert!(!by("sa_4").needs_wake(), "its news was delivered as sa_9's");
        let w = by("sa_9");
        assert!(w.resumed_by_daemon && w.stopped_by_reload && w.needs_wake(), "{w:?}");

        // The image after: sa_4 still follows sa_9, and a second daemon
        // resume of sa_9 extends the chain.
        let mut next = SubagentRegistry::new();
        for w in &carried {
            next.adopt_carried(w);
        }
        assert_eq!(next.resumed_as("sa_4"), Some(Some("sa_9".into())));
        assert!(next.adopted_already("sa_4"));
        next.begin_daemon_resume("sa_9");
        assert_eq!(next.resumed_as("sa_4"), Some(None), "the chain's run is being resumed");
        next.finish_daemon_resume("sa_9", Some("sa_12"));
        assert_eq!(next.resumed_as("sa_4"), Some(Some("sa_12".into())));
        // A failed launch hands the id back to the model.
        next.begin_daemon_resume("sa_12");
        next.finish_daemon_resume("sa_12", None);
        assert_eq!(next.resumed_as("sa_12"), None);
        assert_eq!(next.resumed_as("sa_4"), Some(Some("sa_12".into())));

        // An unknown or older record reads without the new fields.
        let w: CarriedWorker = serde_json::from_value(serde_json::json!({"id": "sa_1", "status": "cancelled"})).unwrap();
        assert_eq!((w.resumed_as, w.resumed_by_daemon), (None, false));
        let plain = serde_json::to_value(stopped("sa_1", true)).unwrap();
        assert!(plain.get("resumed_as").is_none() && plain.get("resumed_by_daemon").is_none(), "{plain}");
    }

    /// A launch still under way when a reload comes (not reached today: the
    /// actor finishes its launches first) is announced the old way and
    /// never resumed twice.
    #[test]
    fn a_daemon_resume_still_launching_is_carried_as_a_plain_wake() {
        let mut registry = SubagentRegistry::new();
        registry.adopt_carried(&stopped("sa_4", true));
        registry.begin_daemon_resume("sa_4");
        // `wait`/`collect`/`subagent_resume` read this as "being resumed".
        assert_eq!(registry.resumed_as("sa_4"), Some(None));
        assert_eq!(registry.resumed_as("sa_5"), None);
        let carried = registry.reload_carry(&Default::default());
        let w = carried.iter().find(|w| w.id == "sa_4").unwrap();
        assert!(w.needs_wake() && w.resumed_by_daemon && w.resumed_as.is_none(), "{w:?}");
        assert!(matches!(plan_daemon_resumes(&[w])[0], Some(DaemonResume::Leave(_))));
    }
}
