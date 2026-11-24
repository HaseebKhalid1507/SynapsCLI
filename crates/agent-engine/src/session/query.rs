//! Typed query failures, and the bounded job runner a query that awaits
//! runs in (arch-review-435-436-437.md Contract v3 §1.3–1.4, v3.1 §3).
//!
//! Only what `SessionQuery::CacheExpiry` needs is built here: one job kind,
//! keyed by requester. A job's reply is emitted in exactly one place, the
//! actor's `finish`, which starts by removing the job's entry
//! ([`QueryJobs::take`]); every other path either removes it first or finds
//! nothing. The deadline is enforced inside the task, so a timeout is an
//! ordinary completion.
//!
//! | Path | Terminal reply |
//! |---|---|
//! | task returns | `finish(gen, result)`, or `stale` |
//! | deadline | `finish(gen, timeout)`, from inside the task |
//! | panic | `JoinError::id()` → `tasks` → `finish(gen, internal)` |
//! | new ask, same requester, other id | `superseded` first, then `abort()` |
//! | new ask, same requester, same id | removed without a reply, then `abort()` |
//! | requester detaches | removed without a reply, aborted |
//! | session ends | the set is dropped; `Ended` covers it |
//! | an aborted task joins cancelled | nothing (abort always follows a terminal transition) |
//!
//! Blocking work: the one blocking step a `CacheExpiry` probe reaches is
//! image projection, which already runs under a process-wide permit owned
//! by its blocking closure (`image_limits`); a probe takes that permit
//! without waiting and answers `busy` when it is held.

use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::task::{AbortHandle, JoinSet};

use super::ClientId;

/// The longest a query job runs before it answers `timeout`. It bounds the
/// reply, not any syscall a job is blocked in.
pub const QUERY_JOB_DEADLINE: Duration = Duration::from_secs(5);
/// At most this many jobs per session are live ...
pub const QUERY_JOBS_ACTIVE: usize = 4;
/// ... and at most this many tasks (aborted ones not yet reaped count).
pub const QUERY_JOBS_TASKS: usize = 8;

/// Why a typed query has no answer. Append-only; an unknown code reads as
/// `internal`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryErrorCode {
    /// Too many jobs, or the blocking budget is spent: ask again later.
    Busy,
    /// The job ran past [`QUERY_JOB_DEADLINE`].
    Timeout,
    /// A newer ask of the same query from the same requester replaced it.
    Superseded,
    /// What the answer was computed from changed before it was sent: ask
    /// once more.
    Stale,
    /// The session cannot answer (a failed wake, ending).
    Unavailable,
    #[serde(other)]
    Internal,
}

/// A typed query's failure: `QueryResult.value` is then
/// `{"error": {"code": .., "message": ..}}`, never the success struct.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryError {
    pub code: QueryErrorCode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl QueryError {
    pub fn new(code: QueryErrorCode) -> Self {
        Self { code, message: None }
    }

    pub fn with_message(code: QueryErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: Some(message.into()) }
    }

    /// The `QueryResult.value` that carries this error.
    pub fn to_value(&self) -> Value {
        serde_json::json!({ "error": self })
    }
}

impl std::fmt::Display for QueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let code = serde_json::to_value(self.code).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
        match &self.message {
            Some(m) => write!(f, "{code}: {m}"),
            None => f.write_str(&code),
        }
    }
}

/// Read a typed query's `QueryResult.value`: its success struct, or the
/// error it carries (a bare `{"error": "text"}` reads as `internal`).
pub fn decode_query_reply<T: serde::de::DeserializeOwned>(value: &Value) -> Result<T, QueryError> {
    if let Some(e) = value.get("error") {
        return Err(serde_json::from_value(e.clone())
            .unwrap_or_else(|_| QueryError { code: QueryErrorCode::Internal, message: e.as_str().map(str::to_string) }));
    }
    serde_json::from_value(value.clone()).map_err(|e| QueryError::with_message(QueryErrorCode::Internal, e.to_string()))
}

pub(crate) type JobGen = u64;

/// A live job: who asked, under which id, and what it must still match.
pub(crate) struct Job<A> {
    pub(crate) from: Option<ClientId>,
    pub(crate) id: u64,
    pub(crate) ask: A,
    abort: AbortHandle,
}

impl<A> Job<A> {
    pub(crate) fn abort(&self) {
        self.abort.abort();
    }
}

/// The session's query jobs (one kind: keyed by requester).
pub(crate) struct QueryJobs<T, A> {
    set: JoinSet<(JobGen, Result<T, QueryError>)>,
    /// Every task still in `set`, by task id.
    tasks: HashMap<tokio::task::Id, JobGen>,
    active: HashMap<JobGen, Job<A>>,
    next: JobGen,
}

impl<T: Send + 'static, A> Default for QueryJobs<T, A> {
    fn default() -> Self {
        Self { set: JoinSet::new(), tasks: HashMap::new(), active: HashMap::new(), next: 0 }
    }
}

impl<T: Send + 'static, A> QueryJobs<T, A> {
    /// Before `from` asks again under `id`: its pending ask with this id is
    /// dropped without a reply (the id now names the new ask) and aborted;
    /// its asks under other ids are returned, for the caller to answer
    /// `superseded` and then [`Job::abort`].
    pub(crate) fn supersede(&mut self, from: Option<ClientId>, id: u64) -> Vec<Job<A>> {
        let mine: Vec<JobGen> = self.active.iter().filter(|(_, j)| j.from == from).map(|(g, _)| *g).collect();
        let mut out = Vec::new();
        for g in mine {
            let job = self.active.remove(&g).expect("listed");
            if job.id == id {
                job.abort();
            } else {
                out.push(job);
            }
        }
        out
    }

    /// No room for another job: answer `busy`.
    pub(crate) fn full(&self) -> bool {
        self.active.len() >= QUERY_JOBS_ACTIVE || self.set.len() >= QUERY_JOBS_TASKS
    }

    /// Run `work` for `from`'s ask `id`, bounded by [`QUERY_JOB_DEADLINE`].
    pub(crate) fn spawn<F>(&mut self, from: Option<ClientId>, id: u64, ask: A, work: F)
    where
        F: std::future::Future<Output = Result<T, QueryError>> + Send + 'static,
    {
        let gen = self.next;
        self.next += 1;
        let abort = self.set.spawn(async move {
            let r = match tokio::time::timeout(QUERY_JOB_DEADLINE, work).await {
                Ok(r) => r,
                Err(_) => Err(QueryError::new(QueryErrorCode::Timeout)),
            };
            (gen, r)
        });
        self.tasks.insert(abort.id(), gen);
        self.active.insert(gen, Job { from, id, ask, abort });
    }

    /// The terminal transition: the job's entry, if it is still live.
    pub(crate) fn take(&mut self, gen: JobGen) -> Option<Job<A>> {
        self.active.remove(&gen)
    }

    /// `c` left: its jobs end without a reply.
    pub(crate) fn client_gone(&mut self, c: ClientId) {
        self.active.retain(|_, j| {
            let keep = j.from != Some(c);
            if !keep {
                j.abort();
            }
            keep
        });
    }

    /// The next completion (a panic is `internal`); pends while none can
    /// come. Cancel-safe.
    pub(crate) async fn next(&mut self) -> (JobGen, Result<T, QueryError>) {
        loop {
            if self.set.is_empty() {
                return std::future::pending().await;
            }
            match self.set.join_next_with_id().await {
                Some(Ok((task, done))) => {
                    self.tasks.remove(&task);
                    return done;
                }
                Some(Err(e)) => {
                    let gen = self.tasks.remove(&e.id());
                    if let (true, Some(gen)) = (e.is_panic(), gen) {
                        return (gen, Err(QueryError::with_message(QueryErrorCode::Internal, "the query job panicked")));
                    }
                }
                None => {}
            }
        }
    }

    #[cfg(test)]
    fn live(&self) -> usize {
        self.active.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn me() -> Option<ClientId> {
        Some(ClientId(7))
    }

    fn boom() -> Result<u32, QueryError> {
        panic!("boom")
    }

    #[test]
    fn an_error_is_its_own_shape_and_an_unknown_code_is_internal() {
        let v = QueryError::with_message(QueryErrorCode::Busy, "full").to_value();
        assert_eq!(v, serde_json::json!({"error": {"code": "busy", "message": "full"}}));
        let e = decode_query_reply::<serde_json::Value>(&serde_json::json!({"error": {"code": "meteor"}})).unwrap_err();
        assert_eq!(e.code, QueryErrorCode::Internal);
        let e = decode_query_reply::<serde_json::Value>(&serde_json::json!({"error": "old text"})).unwrap_err();
        assert_eq!((e.code, e.message.as_deref()), (QueryErrorCode::Internal, Some("old text")));
        assert_eq!(decode_query_reply::<u32>(&serde_json::json!(3)).unwrap(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn the_deadline_is_a_completion_and_a_panic_is_internal() {
        let mut jobs: QueryJobs<u32, ()> = QueryJobs::default();
        jobs.spawn(me(), 1, (), async { std::future::pending().await });
        let (gen, r) = jobs.next().await;
        assert_eq!(r.unwrap_err().code, QueryErrorCode::Timeout);
        assert!(jobs.take(gen).is_some());
        jobs.spawn(me(), 2, (), async { boom() });
        let (gen, r) = jobs.next().await;
        assert_eq!(r.unwrap_err().code, QueryErrorCode::Internal);
        assert_eq!(jobs.take(gen).map(|j| j.id), Some(2));
    }

    #[tokio::test(start_paused = true)]
    async fn supersede_answers_the_old_id_once_and_its_late_result_finds_nothing() {
        let mut jobs: QueryJobs<u32, ()> = QueryJobs::default();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        jobs.spawn(me(), 1, (), async move {
            let _ = rx.await;
            Ok(1)
        });
        // Same requester, another id: the old one is handed back to answer.
        let old = jobs.supersede(me(), 2);
        assert_eq!(old.iter().map(|j| j.id).collect::<Vec<_>>(), vec![1]);
        // Its result was already on its way: it joins and finds no entry.
        let _ = tx.send(());
        tokio::task::yield_now().await;
        old[0].abort();
        jobs.spawn(me(), 2, (), async { Ok(2) });
        let mut ids = Vec::new();
        for _ in 0..2 {
            let (gen, _) = tokio::time::timeout(Duration::from_secs(1), jobs.next()).await.unwrap_or((u64::MAX, Ok(0)));
            if let Some(j) = jobs.take(gen) {
                ids.push(j.id);
            }
        }
        assert_eq!(ids, vec![2], "only the new ask answers");
        // The same id again: dropped without a reply.
        jobs.spawn(me(), 3, (), async { std::future::pending().await });
        assert!(jobs.supersede(me(), 3).is_empty());
        assert_eq!(jobs.live(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_detached_requester_s_jobs_end_without_a_reply_and_the_set_is_bounded() {
        let mut jobs: QueryJobs<u32, ()> = QueryJobs::default();
        for id in 0..QUERY_JOBS_ACTIVE as u64 {
            jobs.spawn(Some(ClientId(id)), id, (), async { std::future::pending().await });
        }
        assert!(jobs.full(), "four live");
        jobs.client_gone(ClientId(0));
        assert!(!jobs.full());
        let (gen, _) = jobs.next().await;
        assert!(jobs.take(gen).is_some(), "a live one timed out; the detached one was reaped silently");
    }
}
