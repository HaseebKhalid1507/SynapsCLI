//! A daemon reload's check of the worker records this process still writes
//! (`docs/specs/subagent-context-rollover.md` §5).
//!
//! Every v2 run this process owns has an entry in the writer table
//! ([`writers_snapshot`]): a live run, or one whose finishing (or an
//! orphaned write of it) still holds the run's ownership. A run that ended
//! released it: the exec changes nothing it left on disk, so the check
//! never sees it. A normal reload goes ahead only when every run in the
//! table has a durable record and no record write in flight:
//!
//! 1. Before anything is quiesced, a run whose record is unresolved (no
//!    record this process can vouch for, and none coming) refuses the reload
//!    at once: no turn pauses.
//! 2. The drain counts a run whose record write is in flight, or whose spawn
//!    record is not written yet, as busy, within the drain's cap.
//! 3. After the drain, writes still in flight get up to
//!    [`WORKER_RECORD_WAIT`] to land (an owner's one retry of an uncertain
//!    publication lands inside it too). Whatever is still not durable then
//!    refuses the reload, before anything is stopped: the sessions' quiesce
//!    lifts and a turn the drain paused resumes in place
//!    (`SessionCommand::ReloadRefused`).
//!
//! `--force` goes ahead without those records; the reload's answer names
//! them, and so does one WARN line. No v2 run exists while worker rollover
//! is off, so a reload never waits or refuses here then.

use std::time::Duration;

use crate::session::wire::WaivedWorker;
pub(crate) use crate::tools::worker_records::writers_snapshot;
use crate::tools::worker_records::{WriterEntry, WriterVerdict};

/// How long record writes still in flight after the drain get to land
/// before they refuse a normal reload. A reload's worst case is the drain's
/// cap plus this.
pub const WORKER_RECORD_WAIT: Duration =
    Duration::from_secs(crate::session::budgets::WORKER_RECORD_WAIT_SECS);

/// How often the check reads the writer table again while it waits.
const WORKER_RECORD_REPOLL: Duration = Duration::from_millis(50);

/// Runs a refusal (or a forced reload's line) names before "and N more".
const NAMED: usize = 8;

/// A run whose record a reload would not preserve.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Unpreserved {
    pub worker: String,
    pub session: Option<String>,
    pub verdict: WriterVerdict,
}

impl Unpreserved {
    fn of(entry: &WriterEntry) -> Option<Self> {
        let verdict = entry.verdict();
        (verdict != WriterVerdict::Durable).then(|| Self {
            worker: entry.handle.clone(),
            session: entry.session.clone(),
            verdict,
        })
    }

    /// What a forced reload's answer says of it.
    pub fn waived(&self) -> WaivedWorker {
        WaivedWorker {
            worker: self.worker.clone(),
            session: self.session.clone(),
            why: self.verdict.code().to_owned(),
        }
    }
}

fn unpreserved(entries: &[WriterEntry]) -> Vec<Unpreserved> {
    entries.iter().filter_map(Unpreserved::of).collect()
}

/// Step 1, before the quiesce: the runs whose record is unresolved.
pub(crate) fn unresolved(entries: &[WriterEntry]) -> Vec<Unpreserved> {
    unpreserved(entries)
        .into_iter()
        .filter(|u| u.verdict == WriterVerdict::Unresolved)
        .collect()
}

/// Step 2, for the drain: the runs whose record write is in flight or whose
/// spawn record is not written yet.
pub(crate) fn writing(entries: &[WriterEntry]) -> usize {
    entries
        .iter()
        .filter(|e| matches!(e.verdict(), WriterVerdict::Publishing | WriterVerdict::Starting))
        .count()
}

/// Step 3, after the drain: wait up to `wait` for every run in `snapshot`
/// (read again every [`WORKER_RECORD_REPOLL`]) to have a durable record;
/// the runs that still do not. Unresolved runs wait for nothing: once only
/// they are left, it answers at once.
pub(crate) async fn settle(wait: Duration, snapshot: impl Fn() -> Vec<WriterEntry>) -> Vec<Unpreserved> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let left = unpreserved(&snapshot());
        if left.iter().all(|u| u.verdict == WriterVerdict::Unresolved) {
            return left;
        }
        let next = tokio::time::Instant::now() + WORKER_RECORD_REPOLL;
        if next >= deadline {
            tokio::time::sleep_until(deadline).await;
            return unpreserved(&snapshot());
        }
        tokio::time::sleep_until(next).await;
    }
}

/// `sa_3 (session s, why), sa_4 (why), and 2 more`.
fn named<'a>(runs: impl ExactSizeIterator<Item = (&'a str, Option<&'a str>, &'a str)>) -> String {
    let total = runs.len();
    let mut parts: Vec<String> = runs
        .take(NAMED)
        .map(|(worker, session, why)| match session {
            Some(session) => format!("{worker} (session {session}, {why})"),
            None => format!("{worker} ({why})"),
        })
        .collect();
    if total > NAMED {
        parts.push(format!("and {} more", total - NAMED));
    }
    parts.join(", ")
}

/// Why a normal reload was refused (the daemon's `ReloadRefused` reason;
/// the CLI prints it after "reload refused: refused: " and exits 3).
pub(crate) fn refusal(runs: &[Unpreserved]) -> String {
    let list = named(runs.iter().map(|u| {
        (
            u.worker.as_str(),
            u.session.as_deref(),
            u.verdict.reason().unwrap_or("durable"),
        )
    }));
    format!(
        "worker records are not durable: {list}; the daemon keeps running this image; no worker \
         was stopped and any paused turn resumes in place; retry, or reload with --force to go \
         ahead without them"
    )
}

/// What a forced reload went ahead without, in the past tense.
fn waived_reason(code: &str) -> &str {
    match code {
        "publishing" => "a record write was still in flight",
        "starting" => "its spawn record was not written yet",
        "unresolved" => "no durable record: its publication was unresolved",
        other => other,
    }
}

/// The one stderr line `synaps daemon reload --force` prints when the
/// reload went ahead without some workers' records (`None`: it waived
/// nothing).
pub fn forced_line(waived: &[WaivedWorker]) -> Option<String> {
    if waived.is_empty() {
        return None;
    }
    let list = named(
        waived
            .iter()
            .map(|w| (w.worker.as_str(), w.session.as_deref(), waived_reason(&w.why))),
    );
    Some(format!("reload forced: worker records not preserved: {list}"))
}

#[cfg(test)]
#[path = "worker_check_tests.rs"]
mod tests;
