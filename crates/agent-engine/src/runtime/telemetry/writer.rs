//! Bounded, non-blocking background writer for observability persistence
//! (Task 11, spec §6.5).
//!
//! One cloneable [`TelemetryWriter`] handle serves BOTH record kinds:
//!
//! - legacy per-request [`TelemetryRecord`]s → the existing
//!   `~/.cache/synaps/api-log.jsonl` (format unchanged: one raw JSON object
//!   per line, exactly what the old synchronous `write_record` produced);
//! - metadata-only [`RequestTrace`]s (Tasks 7–10) → a private trace log at
//!   `~/.cache/synaps/request-trace.jsonl`. Each line is the unwrapped
//!   `synaps-request-trace/1` object, so any schema-aware reader can parse
//!   lines directly.
//!
//! Design contract:
//!
//! - **Never blocks the request path.** Enqueue is a `try_send` on a bounded
//!   queue; overflow increments a dropped counter (never waits). No awaits,
//!   no hidden runtime dependency — the worker is a dedicated OS thread, so
//!   sync contexts can enqueue too.
//! - **One worker thread per writer, but not per file.** Within one writer
//!   only its worker thread touches the files. Several writers can share the
//!   same files, though: every daemon session runtime starts its own writer
//!   on `~/.cache/synaps/*.jsonl`, and a TUI or another daemon process can
//!   overlap. Appends are line-sized `O_APPEND` writes; size-capped rotation
//!   (stat, shift, rename) runs under an exclusive `flock` on `<log>.lock`
//!   with a re-stat after acquiring it, so two writers never rotate the same
//!   file twice. If the lock can't be had within [`ROTATE_LOCK_WAIT`] the
//!   rotation is skipped (counted, warned once) and the line is appended past
//!   the cap: an oversized log beats a lost record.
//! - **Private files.** All paths use the Phase 1 `private_fs` helpers:
//!   parent `0700`, file `0600`, `O_NOFOLLOW` (a planted symlink is refused,
//!   never followed).
//! - **Observable, quiet failure.** Atomic counters for
//!   enqueued/written/dropped/serialization/I-O failures; at most one
//!   warning per persistent failure class (queue overflow, serialization,
//!   open/write, rotate). Warnings carry a class label only — never record
//!   content or paths. The open/write latch re-arms after a successful
//!   write so a *new* persistent failure warns again.
//! - **Bounded shutdown.** [`TelemetryWriter::shutdown`] stops intake,
//!   drains until the deadline, and returns typed stats. Dropping the last
//!   handle simply disconnects the queue: the worker drains what is buffered
//!   and exits detached — `Drop` can never hang.
//!
//! Production wiring rule (documented until Task 12 adds explicit trace
//! config/UI): telemetry `basic`/`full` enables BOTH legacy telemetry
//! persistence and metadata-only trace persistence through this writer;
//! `off` disables both. The trace schema is structurally metadata-only, so
//! this can never silently persist raw content.

use super::{RecordStamp, TelemetryRecord};
use crate::runtime::trace::{RequestTrace, TraceSink};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex as StdMutex};
use std::time::{Duration, Instant};

/// Default bounded queue capacity: deep enough that a briefly slow disk
/// never drops records, small enough to bound memory (records are boxed).
pub const DEFAULT_QUEUE_CAPACITY: usize = 1024;

/// Default per-file size cap before rotation (matches the legacy writer).
pub const DEFAULT_MAX_FILE_BYTES: u64 = 50 * 1024 * 1024;

/// Default rotated generations kept per log: one (`<path>.1`), the
/// historical behaviour. Raised by the `telemetry_log_generations` config key.
pub const DEFAULT_MAX_GENERATIONS: u32 = 1;

/// Longest a writer waits for another writer's rotation of the same log
/// (`<log>.lock`) before appending past the cap instead.
pub const ROTATE_LOCK_WAIT: Duration = Duration::from_secs(2);

/// Highest generation any writer may keep (the config cap). A rotation
/// prunes `<log>.{N+1}` up to this, so lowering N, or a writer that started
/// with a larger N, never strands old generations on disk.
const MAX_GENERATIONS: u32 = agent_core::config::MAX_TELEMETRY_LOG_GENERATIONS;

/// Default budget for the shutdown flush at process-exit epilogues
/// (Task 11): long enough for a healthy disk to drain a full queue,
/// short enough that a hung filesystem can never stall a clean exit.
pub const DEFAULT_SHUTDOWN_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

/// Lock a writer mutex, recovering from poison instead of panicking.
///
/// The request path (enqueue) and the shutdown path must never propagate a
/// panic that happened while another thread held one of these mutexes: the
/// protected state (an `Option<SyncSender>` / `Option<JoinHandle>` / `bool`)
/// is always internally consistent regardless of where the poisoning panic
/// occurred, so recovery is safe — the caller degrades to the normal
/// enqueue/drop or drain semantics.
fn lock_recover<T>(m: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Default telemetry log path: `~/.cache/synaps/api-log.jsonl`.
pub fn default_telemetry_log_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".cache/synaps/api-log.jsonl"))
}

/// Default trace log path: `~/.cache/synaps/request-trace.jsonl`.
pub fn default_trace_log_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".cache/synaps/request-trace.jsonl"))
}

/// Writer configuration. `None` paths resolve to the installation defaults.
#[derive(Debug, Clone)]
pub struct WriterOptions {
    pub telemetry_path: Option<PathBuf>,
    pub trace_path: Option<PathBuf>,
    /// Bounded queue capacity (jobs, both kinds combined).
    pub capacity: usize,
    /// Size cap per log file before rotation to `<path>.1`.
    pub max_file_bytes: u64,
    /// Rotated generations kept per log (`<path>.1` .. `<path>.N`, oldest
    /// highest). Default [`DEFAULT_MAX_GENERATIONS`] (1); `0` is read as 1.
    pub max_generations: u32,
    /// Test seam: artificial per-job delay inside the worker, simulating
    /// slow storage deterministically (never set in production).
    pub write_delay: Option<Duration>,
    /// Test seam: the worker waits on this gate before each job, so a test
    /// can hold storage "stuck" for exactly as long as it needs, with no
    /// wall clock involved (never set in production).
    pub write_gate: Option<WriteGate>,
}

/// A gate the writer's worker waits on before each job ([`WriterOptions::write_gate`]).
/// Starts closed; [`open`](Self::open) releases the worker for good.
#[derive(Debug, Clone, Default)]
pub struct WriteGate(Arc<(StdMutex<bool>, Condvar)>);

impl WriteGate {
    /// A closed gate: the worker blocks before its next job until opened.
    pub fn closed() -> Self {
        Self::default()
    }

    /// Open the gate; every current and future wait returns.
    pub fn open(&self) {
        let (open, cv) = &*self.0;
        *lock_recover(open) = true;
        cv.notify_all();
    }

    fn wait(&self) {
        let (open, cv) = &*self.0;
        let mut guard = lock_recover(open);
        while !*guard {
            guard = cv.wait(guard).unwrap_or_else(|e| e.into_inner());
        }
    }
}

impl Default for WriterOptions {
    fn default() -> Self {
        Self {
            telemetry_path: None,
            trace_path: None,
            capacity: DEFAULT_QUEUE_CAPACITY,
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            max_generations: DEFAULT_MAX_GENERATIONS,
            write_delay: None,
            write_gate: None,
        }
    }
}

/// Snapshot of the writer's atomic counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WriterStats {
    /// Jobs accepted onto the queue.
    pub enqueued: u64,
    /// Records fully persisted (serialized + appended).
    pub written: u64,
    /// Jobs rejected: queue full or writer already shut down.
    pub dropped: u64,
    /// Records that failed to serialize (kept, counted, never retried).
    pub serialization_failures: u64,
    /// Append/open/rotate I/O failures (record lost, counted).
    pub io_failures: u64,
    /// Warnings actually emitted for queue overflow (warn-once latch).
    pub overflow_warnings: u64,
    /// Warnings actually emitted for serialization failures.
    pub serialization_warnings: u64,
    /// Warnings actually emitted for open/write failures.
    pub io_warnings: u64,
    /// Warnings actually emitted for rotation failures.
    pub rotate_warnings: u64,
}

/// Typed result of a bounded shutdown flush.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownOutcome {
    /// Queue fully drained before the deadline.
    Flushed(WriterStats),
    /// Deadline hit with jobs still in flight; the worker is detached and
    /// keeps draining in the background, but the caller stops waiting.
    TimedOut(WriterStats),
}

impl ShutdownOutcome {
    pub fn stats(&self) -> WriterStats {
        match self {
            Self::Flushed(s) | Self::TimedOut(s) => *s,
        }
    }

    pub fn is_flushed(&self) -> bool {
        matches!(self, Self::Flushed(_))
    }
}

/// Cloneable handle to the bounded background writer.
#[derive(Debug, Clone)]
pub struct TelemetryWriter {
    shared: Arc<Shared>,
    /// Identity stamped onto every telemetry record enqueued through this
    /// handle ([`Self::with_stamp`], [`Self::with_session`]). Same queue,
    /// same files.
    stamp: Option<Arc<RecordStamp>>,
}

/// A queued persistence job: tagged by record kind. Boxed so the queue slot
/// stays one pointer wide regardless of record size.
#[derive(Debug)]
enum Job {
    Telemetry(Box<TelemetryRecord>),
    Trace(Box<RequestTrace>),
}

/// Atomic counters + warn-once latches. Warnings carry a class label only —
/// never record content or filesystem paths.
#[derive(Debug, Default)]
struct Counters {
    enqueued: AtomicU64,
    written: AtomicU64,
    dropped: AtomicU64,
    serialization_failures: AtomicU64,
    io_failures: AtomicU64,
    overflow_warnings: AtomicU64,
    serialization_warnings: AtomicU64,
    io_warnings: AtomicU64,
    rotate_warnings: AtomicU64,
    /// Warn-once latches. Overflow and serialization latch once per writer;
    /// the open/write and rotate latches re-arm after the next success so a
    /// *new* persistent failure surfaces again.
    overflow_warned: AtomicBool,
    serialization_warned: AtomicBool,
    io_warned: AtomicBool,
    rotate_warned: AtomicBool,
}

impl Counters {
    fn warn_once(latch: &AtomicBool, count: &AtomicU64, class: &str, msg: &str) {
        if !latch.swap(true, Ordering::Relaxed) {
            count.fetch_add(1, Ordering::Relaxed);
            // Class label only — no record content, no paths.
            tracing::warn!(class, "{msg}");
        }
    }

    fn snapshot(&self) -> WriterStats {
        WriterStats {
            enqueued: self.enqueued.load(Ordering::Relaxed),
            written: self.written.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            serialization_failures: self.serialization_failures.load(Ordering::Relaxed),
            io_failures: self.io_failures.load(Ordering::Relaxed),
            overflow_warnings: self.overflow_warnings.load(Ordering::Relaxed),
            serialization_warnings: self.serialization_warnings.load(Ordering::Relaxed),
            io_warnings: self.io_warnings.load(Ordering::Relaxed),
            rotate_warnings: self.rotate_warnings.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug)]
struct Shared {
    /// `None` after shutdown — dropping the sender disconnects the channel,
    /// which is the worker's drain-and-exit signal.
    tx: StdMutex<Option<SyncSender<Job>>>,
    latest_traces: StdMutex<std::collections::BTreeMap<String, RequestTrace>>,
    /// Joined only on a fully-drained shutdown; otherwise the worker stays
    /// detached (it exits on its own once the queue drains).
    worker: StdMutex<Option<std::thread::JoinHandle<()>>>,
    done: StdMutex<bool>,
    done_cv: Condvar,
    counters: Counters,
}

/// Everything the worker thread owns. Other writers may share the same
/// files; rotation is serialized across them by `<log>.lock`.
struct WorkerCfg {
    telemetry_path: Option<PathBuf>,
    trace_path: Option<PathBuf>,
    max_file_bytes: u64,
    max_generations: u32,
    write_delay: Option<Duration>,
    write_gate: Option<WriteGate>,
    latest_traces: Arc<Shared>,
}

impl TelemetryWriter {
    /// Spawn the dedicated worker thread and return a cloneable handle.
    /// `None` paths resolve to the installation defaults at spawn time.
    pub fn new(options: WriterOptions) -> Self {
        let (tx, rx) = sync_channel::<Job>(options.capacity.max(1));
        let shared = Arc::new(Shared {
            tx: StdMutex::new(Some(tx)),
            latest_traces: StdMutex::new(std::collections::BTreeMap::new()),
            worker: StdMutex::new(None),
            done: StdMutex::new(false),
            done_cv: Condvar::new(),
            counters: Counters::default(),
        });
        let cfg = WorkerCfg {
            telemetry_path: options.telemetry_path.or_else(default_telemetry_log_path),
            trace_path: options.trace_path.or_else(default_trace_log_path),
            max_file_bytes: options.max_file_bytes,
            max_generations: options.max_generations.max(1),
            write_delay: options.write_delay,
            write_gate: options.write_gate,
            latest_traces: Arc::clone(&shared),
        };
        let worker_shared = Arc::clone(&shared);
        let handle = std::thread::Builder::new()
            .name("synaps-telemetry-writer".to_string())
            .spawn(move || worker_loop(rx, cfg, worker_shared))
            .expect("spawning the telemetry writer thread cannot fail");
        *lock_recover(&shared.worker) = Some(handle);
        Self {
            shared,
            stamp: None,
        }
    }

    /// A handle onto the same writer that stamps `stamp` (replacing this
    /// handle's) on every telemetry record enqueued through it.
    pub fn with_stamp(&self, stamp: RecordStamp) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
            stamp: Some(Arc::new(stamp)),
        }
    }

    /// This handle, also stamping the persisted conversation id
    /// (`session_id`); the rest of its stamp is kept.
    pub fn with_session(&self, session_id: Option<String>) -> Self {
        let mut stamp = self.stamp.as_deref().cloned().unwrap_or_default();
        stamp.session_id = session_id;
        self.with_stamp(stamp)
    }

    /// Enqueue a legacy telemetry record. Non-blocking; overflow drops.
    pub fn enqueue_telemetry(&self, mut record: TelemetryRecord) {
        if let Some(stamp) = &self.stamp {
            stamp.apply(&mut record);
        }
        self.enqueue(Job::Telemetry(Box::new(record)));
    }

    /// Enqueue a metadata-only request trace. Non-blocking; overflow drops.
    pub fn enqueue_trace(&self, record: RequestTrace) {
        lock_recover(&self.shared.latest_traces)
            .insert(record.request_id.as_str().to_string(), record.clone());
        self.enqueue(Job::Trace(Box::new(record)));
    }

    pub fn enrich_trace(
        &self,
        request_id: &crate::runtime::trace::TraceId,
        events: Vec<crate::runtime::trace::ToolExecutionEvent>,
    ) {
        if let Some(record) = lock_recover(&self.shared.latest_traces).get_mut(request_id.as_str())
        {
            record.execution_events = events;
        }
    }

    pub fn trace_snapshot(
        &self,
        request_id: &crate::runtime::trace::TraceId,
    ) -> Option<RequestTrace> {
        lock_recover(&self.shared.latest_traces)
            .get(request_id.as_str())
            .cloned()
    }

    fn enqueue(&self, job: Job) {
        let c = &self.shared.counters;
        // Poison-tolerant: a panic on another thread while it held this
        // mutex must never panic the request path — recover and degrade to
        // normal enqueue/drop semantics (drops are counted, never silent).
        let guard = lock_recover(&self.shared.tx);
        let Some(tx) = guard.as_ref() else {
            // Shut down: quiet, counted drop (records are best-effort).
            c.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        };
        match tx.try_send(job) {
            Ok(()) => {
                c.enqueued.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Full(_)) => {
                c.dropped.fetch_add(1, Ordering::Relaxed);
                Counters::warn_once(
                    &c.overflow_warned,
                    &c.overflow_warnings,
                    "queue_overflow",
                    "telemetry writer queue full — dropping records (counted, best-effort)",
                );
            }
            Err(TrySendError::Disconnected(_)) => {
                c.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Snapshot the counters.
    pub fn stats(&self) -> WriterStats {
        self.shared.counters.snapshot()
    }

    /// Stop accepting new records, drain the queue until `timeout`, and
    /// return typed stats. On timeout the worker stays detached and keeps
    /// draining in the background — the caller never waits past the
    /// deadline. Safe from sync contexts; async callers should use
    /// [`Self::shutdown_async`]. Idempotent.
    ///
    /// "Flushed" means every queued record was appended into the OS file
    /// buffers (`write(2)` returned) — there is deliberately no `fsync`:
    /// these are best-effort diagnostic logs, and surviving a kernel crash
    /// is not part of their contract.
    pub fn shutdown(&self, timeout: Duration) -> ShutdownOutcome {
        // Stop intake: dropping the sender disconnects the channel; the
        // worker drains whatever is buffered and then marks `done`.
        drop(lock_recover(&self.shared.tx).take());

        let deadline = Instant::now() + timeout;
        let mut done = lock_recover(&self.shared.done);
        while !*done {
            let now = Instant::now();
            let Some(remaining) = deadline
                .checked_duration_since(now)
                .filter(|d| !d.is_zero())
            else {
                break;
            };
            let (guard, _timeout) = self
                .shared
                .done_cv
                .wait_timeout(done, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            done = guard;
        }
        let flushed = *done;
        drop(done);

        if flushed {
            // Worker has drained; the join is immediate.
            if let Some(handle) = lock_recover(&self.shared.worker).take() {
                let _ = handle.join();
            }
        }
        let stats = self.stats();
        if flushed {
            ShutdownOutcome::Flushed(stats)
        } else {
            ShutdownOutcome::TimedOut(stats)
        }
    }

    /// Async-safe wrapper around [`Self::shutdown`] (runs on the blocking
    /// pool so an executor thread is never parked).
    pub async fn shutdown_async(&self, timeout: Duration) -> ShutdownOutcome {
        let writer = self.clone();
        tokio::task::spawn_blocking(move || writer.shutdown(timeout))
            .await
            .unwrap_or_else(|_| ShutdownOutcome::TimedOut(self.stats()))
    }
}

/// Worker: drains jobs until every sender is gone, then signals `done`.
/// This is the ONLY code that touches the log files.
fn worker_loop(rx: Receiver<Job>, cfg: WorkerCfg, shared: Arc<Shared>) {
    while let Ok(job) = rx.recv() {
        if let Some(gate) = &cfg.write_gate {
            gate.wait();
        }
        if let Some(delay) = cfg.write_delay {
            std::thread::sleep(delay);
        }
        process_job(job, &cfg, &shared.counters);
    }
    // Channel disconnected and fully drained.
    let mut done = lock_recover(&shared.done);
    *done = true;
    shared.done_cv.notify_all();
}

fn process_job(job: Job, cfg: &WorkerCfg, c: &Counters) {
    let (serialized, path) = match &job {
        Job::Telemetry(r) => (serde_json::to_string(r), cfg.telemetry_path.as_deref()),
        Job::Trace(r) => {
            let latest = lock_recover(&cfg.latest_traces.latest_traces)
                .get(r.request_id.as_str())
                .cloned()
                .unwrap_or_else(|| (**r).clone());
            (serde_json::to_string(&latest), cfg.trace_path.as_deref())
        }
    };
    let line = match serialized {
        Ok(line) => line,
        Err(_) => {
            note_serialization_failure(c);
            return;
        }
    };
    let Some(path) = path else {
        // No resolvable destination (e.g. HOME unset): counted I/O failure.
        note_io_failure(c);
        return;
    };
    append_private_line(path, &line, cfg.max_file_bytes, cfg.max_generations, c);
}

fn note_serialization_failure(c: &Counters) {
    c.serialization_failures.fetch_add(1, Ordering::Relaxed);
    Counters::warn_once(
        &c.serialization_warned,
        &c.serialization_warnings,
        "serialization",
        "telemetry record failed to serialize — dropped (counted)",
    );
}

fn note_io_failure(c: &Counters) {
    c.io_failures.fetch_add(1, Ordering::Relaxed);
    Counters::warn_once(
        &c.io_warned,
        &c.io_warnings,
        "open_write",
        "telemetry log unwritable — records dropped (counted); \
         will warn again after the next successful write",
    );
}

/// `<path>.<n>`.
fn generation_path(path: &std::path::Path, n: u32) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(format!(".{n}"));
    PathBuf::from(p)
}

/// Append one line with private-fs hardening + size-capped rotation
/// ([`rotate_if_over`]).
fn append_private_line(
    path: &std::path::Path,
    line: &str,
    max_bytes: u64,
    generations: u32,
    c: &Counters,
) {
    if let Some(parent) = path.parent() {
        if agent_core::core::private_fs::ensure_private_dir(parent).is_err() {
            note_io_failure(c);
            return;
        }
    }

    rotate_if_over(path, max_bytes, generations, c);

    // Created 0600 with O_NOFOLLOW (CWE-59); pre-existing broader modes are
    // repaired by the helper. A planted symlink is refused, never followed.
    match agent_core::core::private_fs::open_private_append(path) {
        Ok(mut f) => {
            use std::io::Write;
            if writeln!(f, "{line}").is_ok() {
                c.written.fetch_add(1, Ordering::Relaxed);
                // Success re-arms the open/write warn latch.
                c.io_warned.store(false, Ordering::Relaxed);
            } else {
                note_io_failure(c);
            }
        }
        Err(_) => note_io_failure(c),
    }
}

fn over_cap(path: &std::path::Path, max_bytes: u64) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.len() > max_bytes)
}

fn note_rotate_failure(c: &Counters, msg: &str) {
    c.io_failures.fetch_add(1, Ordering::Relaxed);
    Counters::warn_once(&c.rotate_warned, &c.rotate_warnings, "rotate", msg);
}

/// Repair a broader mode inherited from older builds (best-effort, policy
/// §5.4). Renames keep our own 0600.
fn chmod_private(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Exclusive `flock` on `<log>.lock` (created 0600, `O_NOFOLLOW`), waiting
/// at most [`ROTATE_LOCK_WAIT`]. `None` = not acquired. Released on drop.
fn lock_rotation(path: &std::path::Path) -> Option<std::fs::File> {
    use fs4::fs_std::FileExt;
    let mut lock_path = path.as_os_str().to_owned();
    lock_path.push(".lock");
    let file = agent_core::core::private_fs::open_private_append(std::path::Path::new(&lock_path)).ok()?;
    let deadline = Instant::now() + ROTATE_LOCK_WAIT;
    loop {
        match FileExt::try_lock_exclusive(&file) {
            Ok(true) => return Some(file),
            Ok(false) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            _ => return None,
        }
    }
}

/// Size-capped rotation: once the live file is over `max_bytes`, under
/// `<log>.lock` and only if a re-stat still finds it over (another writer
/// may have just rotated it): shift `<log>.k` to `<log>.k+1` from the top
/// (`<log>.N` is clobbered), rename the live file to `<log>.1`, then delete
/// `<log>.{N+1}` .. `<log>.{MAX}`. N = 1 (the default) is the historical
/// single generation. A missing generation is skipped.
fn rotate_if_over(path: &std::path::Path, max_bytes: u64, generations: u32, c: &Counters) {
    if !over_cap(path, max_bytes) {
        return;
    }
    let Some(_lock) = lock_rotation(path) else {
        note_rotate_failure(c, "telemetry log rotation lock unavailable — appending past the size cap");
        return;
    };
    if !over_cap(path, max_bytes) {
        return; // rotated by another writer while we waited
    }
    let n = generations.clamp(1, MAX_GENERATIONS);
    for k in (1..n).rev() {
        let from = generation_path(path, k);
        if std::fs::symlink_metadata(&from).is_err() {
            continue;
        }
        let to = generation_path(path, k + 1);
        match std::fs::rename(&from, &to) {
            Ok(()) => chmod_private(&to),
            Err(_) => note_rotate_failure(c, "telemetry log generation shift failed — a generation may be lost"),
        }
    }
    let rotated = generation_path(path, 1);
    if std::fs::rename(path, &rotated).is_ok() {
        chmod_private(&rotated);
        c.rotate_warned.store(false, Ordering::Relaxed);
    } else {
        // Fall through to the append: an oversized log beats a lost record.
        note_rotate_failure(c, "telemetry log rotation failed — appending past the size cap");
    }
    // Best-effort prune of generations beyond this writer's N (left by a
    // larger N earlier, or by another writer that started with one).
    for k in n + 1..=MAX_GENERATIONS {
        let stale = generation_path(path, k);
        if std::fs::symlink_metadata(&stale).is_ok() {
            let _ = std::fs::remove_file(&stale);
        }
    }
}

/// Production [`TraceSink`]: enqueues each record onto the shared writer.
#[derive(Debug)]
pub struct WriterTraceSink {
    writer: TelemetryWriter,
}

impl WriterTraceSink {
    pub fn new(writer: TelemetryWriter) -> Self {
        Self { writer }
    }
}

impl TraceSink for WriterTraceSink {
    fn emit(&self, record: RequestTrace) {
        self.writer.enqueue_trace(record);
    }

    fn enrich_execution_events(
        &self,
        request_id: &crate::runtime::trace::TraceId,
        events: Vec<crate::runtime::trace::ToolExecutionEvent>,
    ) {
        self.writer.enrich_trace(request_id, events);
    }

    fn snapshot_for_request(
        &self,
        request_id: &crate::runtime::trace::TraceId,
    ) -> Option<RequestTrace> {
        self.writer.trace_snapshot(request_id)
    }

    fn enabled(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::telemetry::TelemetryRecord;
    use crate::runtime::trace::{
        CacheMeta, EndpointMeta, RequestAnatomy, RequestTrace, TimingStages, TraceId,
        TraceSchemaVersion, TransportKind, TransportOutcome,
    };
    use std::time::Instant;

    const SENTINEL: &str = "RAW-CONTENT-SENTINEL-9f2c";

    fn tmp_writer(
        dir: &std::path::Path,
        capacity: usize,
        delay: Option<Duration>,
    ) -> (TelemetryWriter, PathBuf, PathBuf) {
        let telemetry = dir.join("synaps/api-log.jsonl");
        let trace = dir.join("synaps/request-trace.jsonl");
        let w = TelemetryWriter::new(WriterOptions {
            telemetry_path: Some(telemetry.clone()),
            trace_path: Some(trace.clone()),
            capacity,
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            max_generations: DEFAULT_MAX_GENERATIONS,
            write_delay: delay,
            write_gate: None,
        });
        (w, telemetry, trace)
    }

    fn sample_telemetry() -> TelemetryRecord {
        TelemetryRecord {
            ts: 42,
            model: "claude-sonnet-4-6".to_string(),
            attempt: 1,
            total_ms: 100,
            ..Default::default()
        }
    }

    /// Minimal schema-valid trace record. Byte lengths are *derived from* a
    /// sentinel string but the record structurally cannot carry the string
    /// itself — the persistence test asserts its absence on disk.
    fn sample_trace() -> RequestTrace {
        RequestTrace {
            schema: TraceSchemaVersion,
            session_id: TraceId::new("sess-w1").unwrap(),
            turn_id: TraceId::new("turn-w1").unwrap(),
            request_id: TraceId::new("req-w1").unwrap(),
            execution_events: Vec::new(),
            attempt: 1,
            model: agent_core::prompt::QualifiedModelId::parse("anthropic/claude-sonnet-4-6")
                .unwrap(),
            transport: TransportKind::AnthropicMessages,
            endpoint: EndpointMeta::new("api.anthropic.com", "/v1/messages").unwrap(),
            anatomy: RequestAnatomy {
                system_segment_count: 1,
                message_count: 1,
                block_count: SENTINEL.len() as u32 % 7,
                tool_count: 0,
            },
            wire: None,
            system_segments: vec![],
            messages: vec![],
            tools: vec![],
            cache: CacheMeta::default(),
            translation_losses: vec![],
            outcome: TransportOutcome {
                timings: TimingStages::default(),
                retries: vec![],
                provider_request_id: None,
                http_status: Some(200),
                stop_reason: None,
                usage: None,
                terminal: agent_core::TurnOutcome::Completed,
            },
        }
    }

    fn read_lines(path: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    // ── Success path: flush writes everything, formats preserved ─────────

    #[test]
    fn flush_writes_telemetry_jsonl_in_legacy_format() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (w, telemetry_path, _) = tmp_writer(tmp.path(), 16, None);
        w.enqueue_telemetry(sample_telemetry());
        let out = w.shutdown(Duration::from_secs(5));
        assert!(out.is_flushed());
        assert_eq!(out.stats().written, 1);
        assert_eq!(out.stats().enqueued, 1);
        assert_eq!(out.stats().dropped, 0);

        let lines = read_lines(&telemetry_path);
        assert_eq!(lines.len(), 1);
        // Exact legacy format: the raw serde_json object, unwrapped.
        let expected = serde_json::to_string(&sample_telemetry()).unwrap();
        assert_eq!(lines[0], expected);
        // Legacy skip-none contract intact.
        assert!(!lines[0].contains("request_id"));
        assert!(lines[0].contains("\"model\":\"claude-sonnet-4-6\""));
    }

    #[test]
    fn trace_lines_are_unwrapped_schema_valid_and_content_free() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (w, _, trace_path) = tmp_writer(tmp.path(), 16, None);
        w.enqueue_trace(sample_trace());
        assert!(w.shutdown(Duration::from_secs(5)).is_flushed());

        let lines = read_lines(&trace_path);
        assert_eq!(lines.len(), 1);
        // Unwrapped: a schema-aware reader parses the line directly.
        let parsed: RequestTrace = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(parsed, sample_trace());
        assert!(lines[0].contains("synaps-request-trace/1"));
        // No raw content can appear on disk.
        assert!(!lines[0].contains(SENTINEL));
    }

    #[test]
    fn both_kinds_interleave_without_cross_contamination() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (w, telemetry_path, trace_path) = tmp_writer(tmp.path(), 16, None);
        w.enqueue_telemetry(sample_telemetry());
        w.enqueue_trace(sample_trace());
        w.enqueue_telemetry(sample_telemetry());
        let out = w.shutdown(Duration::from_secs(5));
        assert_eq!(out.stats().written, 3);
        assert_eq!(read_lines(&telemetry_path).len(), 2);
        assert_eq!(read_lines(&trace_path).len(), 1);
    }

    // ── Overflow: dropped counter + warn once ────────────────────────────

    #[test]
    fn overflow_increments_dropped_and_warns_once() {
        let tmp = tempfile::TempDir::new().unwrap();
        // Capacity 1 + slow worker: most enqueues must overflow.
        let (w, _, _) = tmp_writer(tmp.path(), 1, Some(Duration::from_millis(200)));
        for _ in 0..20 {
            w.enqueue_telemetry(sample_telemetry());
        }
        let stats = w.stats();
        assert!(stats.dropped > 0, "queue overflow must drop: {stats:?}");
        assert_eq!(
            stats.overflow_warnings, 1,
            "exactly one overflow warning: {stats:?}"
        );
        assert_eq!(stats.enqueued + stats.dropped, 20);
        w.shutdown(Duration::from_secs(5));
    }

    #[test]
    fn enqueue_after_shutdown_counts_dropped_never_panics() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (w, _, _) = tmp_writer(tmp.path(), 16, None);
        assert!(w.shutdown(Duration::from_secs(5)).is_flushed());
        w.enqueue_telemetry(sample_telemetry());
        w.enqueue_trace(sample_trace());
        assert_eq!(w.stats().dropped, 2);
    }

    // ── Latency: slow storage never delays the enqueue path ──────────────

    #[test]
    fn enqueue_is_fast_even_when_worker_is_slow() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (w, _, _) = tmp_writer(tmp.path(), 1, Some(Duration::from_millis(500)));
        let start = Instant::now();
        for _ in 0..100 {
            w.enqueue_telemetry(sample_telemetry());
        }
        assert!(
            start.elapsed() < Duration::from_millis(250),
            "enqueue must never block on storage: {:?}",
            start.elapsed()
        );
        // Bounded even though the worker still sleeps on the queued job.
        let out = w.shutdown(Duration::from_millis(100));
        assert!(!out.is_flushed());
    }

    #[test]
    fn trace_sink_emit_is_fast_with_slow_storage() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (w, _, _) = tmp_writer(tmp.path(), 1, Some(Duration::from_millis(500)));
        let sink = WriterTraceSink::new(w.clone());
        assert!(sink.enabled());
        let start = Instant::now();
        for _ in 0..50 {
            sink.emit(sample_trace());
        }
        assert!(
            start.elapsed() < Duration::from_millis(250),
            "sink emit must never block on storage: {:?}",
            start.elapsed()
        );
        w.shutdown(Duration::from_millis(100));
    }

    // ── Broken storage: counted, warned once, request path unaffected ────

    #[test]
    fn broken_path_counts_io_failures_and_warns_once() {
        let tmp = tempfile::TempDir::new().unwrap();
        // Parent "dir" is a regular file → every append must fail.
        let blocker = tmp.path().join("blocked");
        std::fs::write(&blocker, b"not a dir").unwrap();
        let w = TelemetryWriter::new(WriterOptions {
            telemetry_path: Some(blocker.join("api-log.jsonl")),
            trace_path: Some(blocker.join("request-trace.jsonl")),
            capacity: 16,
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            max_generations: 1,
            write_delay: None,
            write_gate: None,
        });
        for _ in 0..3 {
            w.enqueue_telemetry(sample_telemetry());
        }
        let out = w.shutdown(Duration::from_secs(5));
        assert!(out.is_flushed(), "broken storage must not stall drain");
        let stats = out.stats();
        assert_eq!(stats.io_failures, 3);
        assert_eq!(stats.io_warnings, 1, "warn once per failure class");
        assert_eq!(stats.written, 0);
    }

    #[test]
    fn io_warning_latch_rearms_after_success() {
        let tmp = tempfile::TempDir::new().unwrap();
        let good = tmp.path().join("synaps/api-log.jsonl");
        let blocker = tmp.path().join("blocked");
        std::fs::write(&blocker, b"not a dir").unwrap();
        // Trace path broken, telemetry path good: fail → succeed → fail.
        let w = TelemetryWriter::new(WriterOptions {
            telemetry_path: Some(good.clone()),
            trace_path: Some(blocker.join("request-trace.jsonl")),
            capacity: 16,
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            max_generations: 1,
            write_delay: None,
            write_gate: None,
        });
        w.enqueue_trace(sample_trace()); // fail → warn #1
        w.enqueue_telemetry(sample_telemetry()); // success → latch re-arms
        w.enqueue_trace(sample_trace()); // fail → warn #2
        let stats = w.shutdown(Duration::from_secs(5)).stats();
        assert_eq!(stats.io_failures, 2);
        assert_eq!(stats.io_warnings, 2);
        assert_eq!(stats.written, 1);
    }

    #[test]
    fn serialization_failure_counted_and_warned_once() {
        // `TelemetryRecord`/`RequestTrace` serialization is structurally
        // infallible (serde_json maps even non-finite floats to null), so
        // the failure class is exercised directly on the worker's private
        // failure path rather than via a contrived record.
        let c = Counters::default();
        note_serialization_failure(&c);
        note_serialization_failure(&c);
        let stats = c.snapshot();
        assert_eq!(stats.serialization_failures, 2);
        assert_eq!(stats.serialization_warnings, 1, "warn once per writer");
    }

    // ── Rotation: single worker owns writes, order preserved ─────────────

    #[test]
    fn rotation_at_cap_preserves_order_and_parseability() {
        let tmp = tempfile::TempDir::new().unwrap();
        let telemetry_path = tmp.path().join("synaps/api-log.jsonl");
        let w = TelemetryWriter::new(WriterOptions {
            telemetry_path: Some(telemetry_path.clone()),
            trace_path: None,
            capacity: 64,
            max_file_bytes: 64, // one record (~90B) exceeds the cap
            max_generations: 1,
            write_delay: None,
            write_gate: None,
        });
        for ts in 0..3u64 {
            let mut r = sample_telemetry();
            r.ts = ts;
            w.enqueue_telemetry(r);
        }
        let stats = w.shutdown(Duration::from_secs(5)).stats();
        assert_eq!(stats.written, 3);

        // One-generation rotation with a cap below one record's size keeps
        // the last two records: `.1` holds the previous generation, the
        // live file the newest. Every surviving line parses and order holds.
        let rotated = tmp.path().join("synaps/api-log.jsonl.1");
        assert!(rotated.exists(), "size cap must rotate to .1");
        let ts_of = |lines: Vec<String>| -> Vec<u64> {
            lines
                .iter()
                .map(|l| {
                    serde_json::from_str::<serde_json::Value>(l).unwrap()["ts"]
                        .as_u64()
                        .unwrap()
                })
                .collect()
        };
        assert_eq!(ts_of(read_lines(&rotated)), vec![1]);
        assert_eq!(ts_of(read_lines(&telemetry_path)), vec![2]);
        assert!(
            !tmp.path().join("synaps/api-log.jsonl.2").exists(),
            "the default keeps exactly one generation"
        );
    }

    fn rotating_writer(telemetry_path: &std::path::Path, generations: u32) -> TelemetryWriter {
        TelemetryWriter::new(WriterOptions {
            telemetry_path: Some(telemetry_path.to_path_buf()),
            trace_path: None,
            capacity: 1024,
            max_file_bytes: 64, // every record exceeds the cap: each write rotates
            max_generations: generations,
            write_delay: None,
            write_gate: None,
        })
    }

    fn write_n(w: &TelemetryWriter, from: u64, count: u64) {
        for ts in from..from + count {
            let mut r = sample_telemetry();
            r.ts = ts;
            w.enqueue_telemetry(r);
        }
    }

    /// Lowering N (a later writer, or another session's writer started
    /// with a smaller N) prunes `.{N+1}` .. `.{MAX}` on its next rotation,
    /// so nothing is stranded; a gap past N is pruned too.
    #[test]
    fn rotation_prunes_generations_beyond_n_including_after_n_is_lowered() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("synaps/api-log.jsonl");
        let w = rotating_writer(&path, 5);
        write_n(&w, 0, 7);
        assert!(w.shutdown(Duration::from_secs(5)).is_flushed());
        for k in 1..=5 {
            assert!(generation_path(&path, k).exists(), ".{k} kept at N=5");
        }
        // A stale generation far past any N (left by an older N=100 writer).
        std::fs::write(generation_path(&path, 42), b"{\"ts\":999}\n").unwrap();

        // N lowered to 2 for the next writer on the same files.
        let w = rotating_writer(&path, 2);
        write_n(&w, 100, 1);
        let stats = w.shutdown(Duration::from_secs(5)).stats();
        assert_eq!((stats.written, stats.io_failures), (1, 0));
        assert!(generation_path(&path, 1).exists());
        assert!(generation_path(&path, 2).exists());
        for k in [3, 4, 5, 6, 42] {
            assert!(!generation_path(&path, k).exists(), ".{k} pruned at N=2");
        }
    }

    /// A generation shifted up keeps (or regains) mode 0600, even one an
    /// older build left 0644.
    #[cfg(unix)]
    #[test]
    fn shifted_generations_are_repaired_to_0600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("synaps/api-log.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let old = generation_path(&path, 1);
        std::fs::write(&old, b"{\"ts\":1}\n").unwrap();
        std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o644)).unwrap();
        let w = rotating_writer(&path, 3);
        write_n(&w, 10, 2);
        assert!(w.shutdown(Duration::from_secs(5)).is_flushed());
        for k in 1..=2 {
            let mode = std::fs::metadata(generation_path(&path, k)).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, ".{k}");
        }
    }

    /// Several writers on the same files (every daemon session has its own)
    /// rotate under `<log>.lock` with a re-stat: no writer rotates a file
    /// another just rotated, so no generation is shifted twice. With N above
    /// the number of rotations nothing may fall off the end: every record
    /// survives, and the generations are contiguous with none empty.
    #[test]
    fn concurrent_writers_on_one_log_never_double_rotate() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("synaps/api-log.jsonl");
        const WRITERS: u64 = 4;
        const EACH: u64 = 20; // 80 records, < MAX_GENERATIONS + 1 files
        let writers: Vec<_> = (0..WRITERS).map(|_| rotating_writer(&path, MAX_GENERATIONS)).collect();
        let threads: Vec<_> = writers
            .iter()
            .enumerate()
            .map(|(i, w)| {
                let w = w.clone();
                std::thread::spawn(move || write_n(&w, i as u64 * 1000, EACH))
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        for w in &writers {
            assert!(w.shutdown(Duration::from_secs(10)).is_flushed());
        }
        let total_written: u64 = writers.iter().map(|w| w.stats().written).sum();
        assert_eq!(total_written, WRITERS * EACH);

        let mut on_disk = read_lines(&path).len() as u64;
        let mut k = 1;
        while generation_path(&path, k).exists() {
            let n = read_lines(&generation_path(&path, k)).len();
            assert!(n > 0, ".{k} is empty: a double rotation");
            on_disk += n as u64;
            k += 1;
        }
        let highest = (1..=MAX_GENERATIONS).filter(|k| generation_path(&path, *k).exists()).max().unwrap_or(0);
        assert_eq!(highest, k - 1, "generations are contiguous (no gap from a double shift)");
        assert_eq!(on_disk, WRITERS * EACH, "no record lost to a double rotation");
    }

    /// `max_generations = N` keeps `.1` (newest) .. `.N` (oldest) for both
    /// logs; anything older is clobbered.
    #[test]
    fn rotation_keeps_n_generations_for_both_logs() {
        let tmp = tempfile::TempDir::new().unwrap();
        let telemetry_path = tmp.path().join("synaps/api-log.jsonl");
        let trace_path = tmp.path().join("synaps/request-trace.jsonl");
        let w = TelemetryWriter::new(WriterOptions {
            telemetry_path: Some(telemetry_path.clone()),
            trace_path: Some(trace_path.clone()),
            capacity: 64,
            max_file_bytes: 64, // every record exceeds the cap
            max_generations: 3,
            write_delay: None,
            write_gate: None,
        });
        for ts in 0..6u64 {
            let mut r = sample_telemetry();
            r.ts = ts;
            w.enqueue_telemetry(r);
            w.enqueue_trace(sample_trace());
        }
        let stats = w.shutdown(Duration::from_secs(5)).stats();
        assert_eq!(stats.written, 12);
        assert_eq!(stats.io_failures, 0);

        let ts = |path: PathBuf| -> Vec<u64> {
            read_lines(&path)
                .iter()
                .map(|l| {
                    serde_json::from_str::<serde_json::Value>(l).unwrap()["ts"]
                        .as_u64()
                        .unwrap()
                })
                .collect()
        };
        assert_eq!(ts(telemetry_path.clone()), vec![5]);
        assert_eq!(ts(generation_path(&telemetry_path, 1)), vec![4]);
        assert_eq!(ts(generation_path(&telemetry_path, 2)), vec![3]);
        assert_eq!(ts(generation_path(&telemetry_path, 3)), vec![2]);
        assert!(!generation_path(&telemetry_path, 4).exists());
        for n in 1..=3 {
            assert_eq!(read_lines(&generation_path(&trace_path, n)).len(), 1);
        }
        assert!(!generation_path(&trace_path, 4).exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for n in 1..=3 {
                let mode = std::fs::metadata(generation_path(&telemetry_path, n))
                    .unwrap()
                    .permissions()
                    .mode();
                assert_eq!(mode & 0o777, 0o600, "generation {n} stays private");
            }
        }
    }

    // ── Bounded shutdown ─────────────────────────────────────────────────

    #[test]
    fn shutdown_is_bounded_with_slow_sink() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (w, _, _) = tmp_writer(tmp.path(), 32, Some(Duration::from_millis(300)));
        for _ in 0..10 {
            w.enqueue_telemetry(sample_telemetry());
        }
        let start = Instant::now();
        let out = w.shutdown(Duration::from_millis(200));
        let elapsed = start.elapsed();
        assert!(!out.is_flushed(), "10×300ms cannot drain in 200ms");
        assert!(
            elapsed < Duration::from_millis(800),
            "shutdown must respect its deadline: {elapsed:?}"
        );
    }

    #[test]
    fn shutdown_is_idempotent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (w, _, _) = tmp_writer(tmp.path(), 16, None);
        w.enqueue_telemetry(sample_telemetry());
        assert!(w.shutdown(Duration::from_secs(5)).is_flushed());
        assert!(w.shutdown(Duration::from_secs(5)).is_flushed());
    }

    #[tokio::test]
    async fn shutdown_async_flushes_off_the_executor() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (w, telemetry_path, _) = tmp_writer(tmp.path(), 16, None);
        w.enqueue_telemetry(sample_telemetry());
        let out = w.shutdown_async(Duration::from_secs(5)).await;
        assert!(out.is_flushed());
        assert_eq!(read_lines(&telemetry_path).len(), 1);
    }

    // ── Poisoned mutex: request path degrades, never panics ──────────────

    /// A panic on another thread while it held the intake mutex must never
    /// propagate into the request path: enqueue recovers the (always
    /// internally consistent) state and keeps working; shutdown still
    /// drains and the queued record is persisted.
    #[test]
    fn enqueue_degrades_on_poisoned_mutex_instead_of_panicking() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (w, telemetry_path, _) = tmp_writer(tmp.path(), 16, None);

        // Poison the intake mutex deliberately.
        let w2 = w.clone();
        let poisoner = std::thread::spawn(move || {
            let _guard = w2.shared.tx.lock().unwrap();
            panic!("intentional poison for the degradation test");
        });
        assert!(poisoner.join().is_err(), "poisoner must have panicked");
        assert!(w.shared.tx.is_poisoned(), "mutex must be poisoned");

        // Request path: no panic; the record is accepted (recovered state).
        w.enqueue_telemetry(sample_telemetry());
        assert_eq!(w.stats().enqueued, 1, "poison recovery keeps intake open");
        assert_eq!(w.stats().dropped, 0);

        // Shutdown path is poison-tolerant too and still drains.
        let out = w.shutdown(Duration::from_secs(5));
        assert!(out.is_flushed());
        assert_eq!(out.stats().written, 1);
        assert_eq!(read_lines(&telemetry_path).len(), 1);
    }

    // ── Fatal-outcome epilogue: failure records survive the final flush ──

    /// Chat-like fatal shutdown: the last record carries a failed terminal
    /// outcome and is enqueued just before the owner's epilogue flush (the
    /// same call site serves the success and failure return paths). The
    /// bounded flush must persist it.
    #[test]
    fn fatal_outcome_record_persisted_by_epilogue_flush() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (w, _, trace_path) = tmp_writer(tmp.path(), 16, None);
        let mut trace = sample_trace();
        trace.outcome.terminal = agent_core::TurnOutcome::ProviderFailed {
            code: "api_error".to_string(),
            correlation_id: "corr-fatal-1".to_string(),
        };
        trace.outcome.http_status = Some(500);
        w.enqueue_trace(trace);

        let out = w.shutdown(DEFAULT_SHUTDOWN_FLUSH_TIMEOUT);
        assert!(out.is_flushed());
        let lines = read_lines(&trace_path);
        assert_eq!(lines.len(), 1);
        let parsed: RequestTrace = serde_json::from_str(&lines[0]).unwrap();
        assert!(matches!(
            parsed.outcome.terminal,
            agent_core::TurnOutcome::ProviderFailed { .. }
        ));
    }

    // ── Private-mode hardening (spec §5.4) ───────────────────────────────

    #[cfg(unix)]
    mod private_modes {
        use super::*;
        use serial_test::serial;
        use std::os::unix::fs::PermissionsExt;

        struct UmaskGuard {
            old: libc::mode_t,
        }
        impl UmaskGuard {
            fn set(mask: libc::mode_t) -> Self {
                Self {
                    old: unsafe { libc::umask(mask) },
                }
            }
        }
        impl Drop for UmaskGuard {
            fn drop(&mut self) {
                unsafe {
                    libc::umask(self.old);
                }
            }
        }

        fn mode_of(path: &std::path::Path) -> u32 {
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777
        }

        #[test]
        #[serial(umask)]
        fn writer_creates_0600_files_and_0700_dir_under_permissive_umask() {
            let _umask = UmaskGuard::set(0);
            let tmp = tempfile::TempDir::new().unwrap();
            let (w, telemetry_path, trace_path) = tmp_writer(tmp.path(), 16, None);
            w.enqueue_telemetry(sample_telemetry());
            w.enqueue_trace(sample_trace());
            assert!(w.shutdown(Duration::from_secs(5)).is_flushed());
            assert_eq!(mode_of(telemetry_path.parent().unwrap()), 0o700);
            assert_eq!(mode_of(&telemetry_path), 0o600);
            assert_eq!(mode_of(&trace_path), 0o600);
        }

        #[test]
        fn writer_refuses_symlink_targets() {
            let tmp = tempfile::TempDir::new().unwrap();
            let dir = tmp.path().join("synaps");
            std::fs::create_dir_all(&dir).unwrap();
            let victim = tmp.path().join("victim.jsonl");
            std::fs::write(&victim, "").unwrap();
            std::os::unix::fs::symlink(&victim, dir.join("api-log.jsonl")).unwrap();
            std::os::unix::fs::symlink(&victim, dir.join("request-trace.jsonl")).unwrap();
            let (w, _, _) = tmp_writer(tmp.path(), 16, None);
            w.enqueue_telemetry(sample_telemetry());
            w.enqueue_trace(sample_trace());
            let stats = w.shutdown(Duration::from_secs(5)).stats();
            assert_eq!(
                std::fs::read_to_string(&victim).unwrap(),
                "",
                "no bytes may be written through a planted symlink"
            );
            assert_eq!(stats.io_failures, 2);
            assert_eq!(stats.written, 0);
        }
    }
}
