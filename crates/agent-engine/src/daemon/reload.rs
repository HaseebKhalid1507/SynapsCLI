//! `synaps daemon reload` (PLAN-phase3 §2.8, C3): version gate → drain →
//! `Checkpoint{Reload}` every session → reload-state → announce
//! `Reloading`/`Bye{Reloading}` → `execv` self (same pid, adopted flock,
//! CLOEXEC listener) → new image rehydrates sessions from reload-state
//! BEFORE accepting.
//!
//! What reload cannot preserve (§2.8, stated once): in-flight turns
//! (checkpointed = cancelled; partial history + restart marker kept), pending prompts (answered
//! `None`), PTY/background shells (closed, announced), `turn_replay`,
//! input ownership (the owner reclaims it on
//! reconnect via `was_owner`). What it preserves — each session's
//! `Checkpoint{Reload}` reply carries a `SessionReloadRecord`: its journal
//! and id (the rehydrated session continues the same journal), its
//! `SessionConfig` as created (cwd, system, prompt manifest, compaction
//! policy, auto-compact, …), its keep-warm pin, its lifecycle (a Parked
//! session comes back Parked), the non-persisted runtime knobs
//! (`settings_replay`: context window, compaction model, retries,
//! timeouts, tool-output cap, worker grants, `/system`), the CURRENT
//! model/thinking (`/model` mid-session survives), and the session's exact
//! tool activations (pins only, re-validated by the new runtime at its first
//! turn start — `SessionToolSet::restore_carried`).
//!
//! Windows has no `execv`, and a byte-range lock cannot be inherited, so
//! step 5 is a hand-off between two processes (see `daemon::win`): the old
//! image starts the new one with the reload-state and a hand-off event; the
//! new image queues its `daemon.lock` request behind the old one and sets
//! the event; the old image exits — discarding what an exec would — and its
//! release grants the queued request, so the liveness oracle never reads
//! "free". The new image does this before booting anything, then proceeds
//! exactly as after an exec. Same guarantees, one difference: the pid
//! changes (clients learn it from the next `Welcome`).

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::registry::{self, DaemonLock, DaemonPaths};
use super::DaemonState;
use crate::session::wire::{ClientFrame, CHECKPOINT_QUERY_ID, PROTOCOL_VERSION};
use crate::session::{
    CheckpointReason, SessionCommand, SessionConfig, SessionEventWire, SessionHandle,
    SessionLifecycle, SessionReloadRecord, SessionSetting,
};

/// Env: path of the reload-state file handed to the new image.
pub const RELOAD_STATE_ENV: &str = "SYNAPS_DAEMON_RELOAD_STATE";
/// Env: inherited `daemon.lock` fd (mandatory when `RELOAD_STATE_ENV` is set).
pub const LOCK_FD_ENV: &str = "SYNAPS_DAEMON_LOCK_FD";
/// Env: the subagent handle counter read at the `execv` itself, later than
/// `ReloadState::next_subagent_id` (written after the checkpoints): a worker
/// that ignored its stop could still dispatch in between.
pub const SUBAGENT_ID_FLOOR_ENV: &str = "SYNAPS_DAEMON_SUBAGENT_ID_FLOOR";
/// Windows: env naming the event the new image sets once its `daemon.lock`
/// request is queued behind the old image (which then exits).
pub const HANDOFF_EVENT_ENV: &str = "SYNAPS_DAEMON_HANDOFF_EVENT";
/// Windows: env carrying the old image's pid; the new image waits for it to
/// be gone (and its pipe instances with it) before it binds.
pub const HANDOFF_PID_ENV: &str = "SYNAPS_DAEMON_HANDOFF_PID";
/// Windows: how long the old image waits for the new one to queue its lock
/// request — process start and reading the reload-state; nothing boots first.
#[cfg(windows)]
const HANDOFF_ARM_TIMEOUT: Duration = Duration::from_secs(15);
/// Windows: how long the new image waits for the old one to let go.
#[cfg(windows)]
const HANDOFF_EXIT_TIMEOUT: Duration = Duration::from_secs(30);
/// Env: default drain budget in seconds.
pub const DRAIN_SECS_ENV: &str = "SYNAPS_DAEMON_RELOAD_DRAIN_SECS";
/// Tracing target of the reload-resume lines (inside the daemon log's
/// `agent_engine=debug` directive, so they reach `synaps.log`).
pub const LOG_TARGET: &str = "agent_engine::reload";

/// The message a session whose turn a reload cut gets in the new image
/// (`SessionCommand::ResumeAfterReload`). `reloaded_at`: when the old image
/// wrote the reload state, i.e. the reload itself.
pub fn resume_text(reloaded_at: chrono::DateTime<chrono::Utc>) -> String {
    format!(
        "resume: the daemon reloaded mid-turn ({} UTC); your last turn was cut, so check your \
         last tool call before trusting it, then continue.",
        reloaded_at.format("%H:%M")
    )
}

/// The message a session gets when a reload is refused after its drain
/// paused the session's turn at a round boundary
/// (`SessionCommand::ReloadRefused`, spec §5): the turn resumes in place.
/// `refused_at`: when the reload was refused.
pub fn refused_resume_text(refused_at: chrono::DateTime<chrono::Utc>) -> String {
    format!(
        "resume: a daemon reload was refused ({} UTC) after your turn paused at a round \
         boundary; nothing else changed; continue.",
        refused_at.format("%H:%M")
    )
}

pub use super::worker_check::forced_line;

/// The message a session whose turn a clean daemon stop cut gets at the
/// next start (`ResumeAfterReload { stopped: true }`, from the stop's own
/// crash record). `stopped_at`: when the stop wrote that record.
pub fn stop_resume_text(stopped_at: chrono::DateTime<chrono::Utc>) -> String {
    format!(
        "resume: the daemon was stopped and started again mid-turn ({} UTC); your last turn was \
         cut, so check your last tool call before trusting it, then continue.",
        stopped_at.format("%H:%M")
    )
}
const DEFAULT_DRAIN: Duration = Duration::from_secs(30);
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// Per-session `Checkpoint{Reload}` wait: `cancel_turn` (the drain + its
/// bounded save) + stopping the session's workers + the checkpoint's own
/// bounded save + 1 s margin. The actor sizes its wait for its workers'
/// spend from the same budget (child spend).
const CHECKPOINT_BUDGET: Duration = crate::session::budgets::CHECKPOINT_BUDGET;
/// What clients are told to wait before reconnecting.
pub const RETRY_AFTER_MS: u64 = 500;

#[derive(Debug, Clone, Default)]
pub struct ReloadRequest {
    pub now: bool,
    pub drain_secs: Option<u64>,
    pub exe: Option<PathBuf>,
    /// Go ahead without the worker records the check finds not durable
    /// (spec §5; [`super::worker_check`]).
    pub force: bool,
}

impl ReloadRequest {
    pub fn from_frame(frame: &ClientFrame) -> Option<Self> {
        match frame {
            ClientFrame::Reload { now, drain_secs, exe, force } => Some(Self {
                now: *now,
                drain_secs: *drain_secs,
                exe: exe.clone(),
                force: *force,
            }),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub enum ReloadError {
    /// Refused before anything was disturbed (`RefuseReason::ReloadRefused`).
    Refused(String),
    /// `execv` returned: the old image continues; sessions are checkpointed.
    ExecFailed(String),
}

impl std::fmt::Display for ReloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(w) => write!(f, "reload refused: {w}"),
            Self::ExecFailed(e) => write!(f, "reload exec failed: {e}"),
        }
    }
}

/// `<exe> daemon --print-version` output.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrintVersion {
    pub binary_version: String,
    pub protocol_version: u32,
}

impl PrintVersion {
    pub fn current() -> Self {
        Self {
            binary_version: crate::session::wire::binary_version(),
            protocol_version: PROTOCOL_VERSION,
        }
    }
}

/// One recorded session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReloadSession {
    pub id: String,
    pub journal_id: String,
    pub config: SessionConfig,
    pub keep_warm: bool,
    pub lifecycle: SessionLifecycle,
    /// Non-persisted knobs re-`Set` after create (incl. `/system`).
    #[serde(default)]
    pub settings_replay: Vec<SessionSetting>,
    /// Per-session `/keepwarm` override (task #444), re-applied after create.
    #[serde(default)]
    pub cache_keepwarm: Option<String>,
    pub input_owner_kind: Option<crate::session::ClientKind>,
    /// Workers the checkpoint stopped (or that still owed the parent a
    /// completion wake), handed to the new image (`AdoptWorkers`).
    #[serde(default)]
    pub workers: Vec<crate::runtime::subagent::CarriedWorker>,
    /// The session's exact tool activations, restored into the new image's
    /// runtime (re-validated there; see `SessionToolSet::restore_carried`).
    /// Empty from an older image or a checkpoint that did not confirm.
    #[serde(default)]
    pub tool_activations: Vec<crate::tools::activation::CarriedActivation>,
    /// Whether the checkpoint cut this session's running turn
    /// (`SessionReloadRecord::turn_cut`). `None`: no checkpoint reply, or an
    /// older image that does not record it; the new image then resumes
    /// nothing (`ResumeAfterReload`).
    #[serde(default)]
    pub turn_cut: Option<bool>,
    /// With `turn_cut`: the messages that followed the cut turn's marker at
    /// the checkpoint (`SessionReloadRecord::cut_tail`). `None`: an older
    /// image's record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cut_tail: Option<u32>,
    /// The checkpoint's (or the published entry's) armed capacity resume
    /// (`SessionReloadRecord::capacity_stop`). `None`: none, no checkpoint
    /// reply, or an older image. Read leniently: one that does not parse
    /// drops only itself (`capacity_resume::lenient_stop`).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::session::capacity_resume::lenient_stop"
    )]
    pub capacity_stop: Option<crate::session::CapacityStop>,
    /// When the session parked (`SessionReloadRecord::parked_since`, or the
    /// published entry's), so a session restored Parked keeps its eviction
    /// clock. `None`: Live, or an older image's record (the clock then
    /// starts at the restore's park). Not `parked_since`: the crash record
    /// flattens this beside its own key of that name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parked_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// `registry_dir()/daemon[-P].reload.json` body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReloadState {
    pub generation: u64,
    pub written_at: chrono::DateTime<chrono::Utc>,
    pub sessions: Vec<ReloadSession>,
    pub expected_clients: usize,
    /// The subagent handle counter (`sa_N`) at the exec. The new image never
    /// hands out a lower id, so a model's remembered handles keep meaning
    /// the worker they named. `0` (older reload-state): derived from the
    /// carried workers only.
    #[serde(default)]
    pub next_subagent_id: u64,
    /// The mask the daemon was first started with (`daemon.umask`), so that
    /// `inherit` in the new image means that mask and not the one the old
    /// image set (an exec inherits it). Absent from older images' states
    /// (then the new image takes the mask it has); older images ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_umask: Option<u32>,
    /// The spawned-child ledger (`session::children`), within its caps. An
    /// older image ignores the field; a state without it restores none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<crate::session::children::ChildRecord>,
}

impl ReloadState {
    /// The first handle number the new image may use: past the old image's
    /// counter and every carried worker id.
    pub fn subagent_id_floor(&self) -> u64 {
        let carried = self
            .sessions
            .iter()
            .flat_map(|s| &s.workers)
            .filter_map(|w| crate::runtime::subagent::handle_number(&w.id))
            .max()
            .map_or(0, |n| n + 1);
        self.next_subagent_id.max(carried)
    }
}

/// Upper bound for reading [`ReloadState::base_umask`] early (the state file
/// is the daemon's own, 0600 in its 0700 registry dir; a larger or
/// unreadable file just means no carried mask). A byte bound, not a time
/// bound: a stalled filesystem can still make the read wait.
const CARRIED_BASE_MAX_BYTES: u64 = 16 * 1024 * 1024;

/// `path` opened for reading if it is a regular file, judged on the open
/// file (`fstat`), so a path swapped for a FIFO after a check can't block
/// the start: the open is `O_NONBLOCK` (it never waits for a writer) and
/// `O_CLOEXEC`, and anything but a regular file is refused after it.
#[cfg(unix)]
pub(crate) fn open_regular_file(path: &Path) -> Option<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .ok()?;
    file.metadata().ok()?.is_file().then_some(file)
}

#[cfg(not(unix))]
pub(crate) fn open_regular_file(path: &Path) -> Option<std::fs::File> {
    let file = std::fs::File::open(path).ok()?;
    file.metadata().ok()?.is_file().then_some(file)
}

/// New-image side, before anything is started: the started-with mask from
/// the reload state named by [`RELOAD_STATE_ENV`], if any. Reads only (the
/// env and the state file stay as they are for [`adopt_from_env`]); any
/// problem is `None`, which makes the current mask the base.
pub fn carried_base_umask() -> Option<u32> {
    carried_base_umask_in(Path::new(&std::env::var_os(RELOAD_STATE_ENV)?))
}

/// [`carried_base_umask`] of the state file at `path`.
fn carried_base_umask_in(path: &Path) -> Option<u32> {
    use std::io::Read;
    #[derive(Deserialize)]
    struct Carried {
        #[serde(default)]
        base_umask: Option<u32>,
    }
    // A regular file only: a stat first (the fast path), then the open
    // file's own type, which is what counts.
    if !std::fs::metadata(path).ok()?.is_file() {
        return None;
    }
    let file = open_regular_file(path)?;
    let mut body = String::new();
    file.take(CARRIED_BASE_MAX_BYTES + 1).read_to_string(&mut body).ok()?;
    if body.len() as u64 > CARRIED_BASE_MAX_BYTES {
        return None;
    }
    serde_json::from_str::<Carried>(&body).ok()?.base_umask
}

pub fn reload_state_path(paths: &DaemonPaths) -> PathBuf {
    let stem = paths
        .json
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "daemon".into());
    paths.dir.join(format!("{stem}.reload.json"))
}

/// The executable to re-exec: argv[0] canonicalised (NOT `/proc/self/exe`,
/// which reads "(deleted)" after an in-place rebuild); `current_exe` only
/// as a fallback when argv[0] is not resolvable (e.g. bare name on PATH).
pub fn resolve_exe() -> PathBuf {
    if let Some(a0) = std::env::args_os().next() {
        let p = PathBuf::from(&a0);
        if p.components().count() > 1 {
            if let Ok(c) = p.canonicalize() {
                return c;
            }
        }
    }
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("synaps"))
}

fn semver_tuple(s: &str) -> Option<(u64, u64, u64)> {
    let core = s.split(['-', '+']).next()?;
    let mut it = core.split('.').map(|p| p.parse::<u64>().ok());
    Some((it.next()??, it.next()??, it.next()??))
}

/// Directional gate (§2.1): new protocol ≥ ours (clients must still be able
/// to reconnect) and new binary ≥ ours (newer OR EQUAL; same-version
/// rebuilds are the common case).
pub fn version_gate(current: &PrintVersion, new: &PrintVersion) -> Result<(), String> {
    if new.protocol_version < current.protocol_version {
        return Err(format!(
            "new binary speaks protocol {} < {}: reconnecting clients would be refused",
            new.protocol_version, current.protocol_version
        ));
    }
    match (semver_tuple(&current.binary_version), semver_tuple(&new.binary_version)) {
        (Some(c), Some(n)) if n < c => Err(format!(
            "new binary {} is older than the running {} (reload is newer-or-equal only)",
            new.binary_version, current.binary_version
        )),
        (Some(_), Some(_)) => Ok(()),
        _ => Err(format!(
            "cannot compare versions {:?} vs {:?}",
            current.binary_version, new.binary_version
        )),
    }
}

/// M8: `--exe` must be something we would trust to `execv` in our own pid:
/// an existing regular file (canonicalised), executable, owned by our uid
/// (or root — package-installed binaries), not group/world-writable.
/// Returns the canonical path. Same-uid 0600 socket → not a privilege
/// boundary; this is a footgun guard, not a sandbox.
#[cfg(unix)]
pub fn validate_exe(exe: &Path) -> Result<PathBuf, String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let canon = exe
        .canonicalize()
        .map_err(|e| format!("--exe {}: {e}", exe.display()))?;
    let md = std::fs::metadata(&canon).map_err(|e| format!("--exe {}: {e}", canon.display()))?;
    if !md.is_file() {
        return Err(format!("--exe {}: not a regular file", canon.display()));
    }
    let mode = md.permissions().mode();
    if mode & 0o111 == 0 {
        return Err(format!("--exe {}: not executable", canon.display()));
    }
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    if md.uid() != uid && md.uid() != 0 {
        return Err(format!(
            "--exe {}: owned by uid {} (expected {} or root)",
            canon.display(),
            md.uid(),
            uid
        ));
    }
    if mode & 0o022 != 0 {
        return Err(format!(
            "--exe {}: group/world-writable (mode {:o})",
            canon.display(),
            mode & 0o777
        ));
    }
    Ok(canon)
}

#[cfg(not(unix))]
pub fn validate_exe(exe: &Path) -> Result<PathBuf, String> {
    exe.canonicalize().map_err(|e| format!("--exe {}: {e}", exe.display()))
}

/// Run `<exe> daemon --print-version` (5 s) and parse it.
pub async fn probe_version(exe: &Path) -> Result<PrintVersion, String> {
    let out = tokio::time::timeout(
        VERSION_PROBE_TIMEOUT,
        tokio::process::Command::new(exe)
            .args(["daemon", "--print-version"])
            .env("SYNAPS_DAEMON", "1")
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .output(),
    )
    .await
    .map_err(|_| format!("{} --print-version timed out", exe.display()))?
    .map_err(|e| format!("cannot run {}: {e}", exe.display()))?;
    if !out.status.success() {
        return Err(format!(
            "{} --print-version exited {:?}: {}",
            exe.display(),
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("{} --print-version: unparsable output ({e})", exe.display()))
}

/// `Checkpoint{Reload}` → the actor's `SessionReloadRecord` (`None` when
/// the checkpoint did not confirm in budget). Also a clean stop's
/// (`crash_record::record_stop`).
/// `skip_quiesce_grace`: the reload's drain ran to its cap
/// (`Checkpoint::skip_quiesce_grace`).
pub(crate) async fn checkpoint_one(
    handle: SessionHandle,
    skip_quiesce_grace: bool,
) -> Option<SessionReloadRecord> {
    let mut rx = handle.subscribe();
    // One absolute deadline: the actor sizes its wait for its workers'
    // spend from it (child spend).
    let deadline = tokio::time::Instant::now() + CHECKPOINT_BUDGET;
    if handle
        .send(SessionCommand::Checkpoint {
            reason: CheckpointReason::Reload,
            deadline: Some(deadline),
            skip_quiesce_grace,
        })
        .await
        .is_err()
    {
        return None;
    }
    let wait = async {
        loop {
            match rx.recv().await {
                Ok(env) => {
                    if let SessionEventWire::QueryResult { id, value, .. } = env.event {
                        if id == CHECKPOINT_QUERY_ID {
                            return serde_json::from_value(value["record"].clone()).ok();
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            }
        }
    };
    tokio::time::timeout_at(deadline, wait).await.unwrap_or(None)
}

/// How often the drain asks every live session whether it is idle.
const DRAIN_REPOLL: Duration = Duration::from_millis(200);

/// What the reload's drain saw when it ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DrainOutcome {
    /// From the drain's start to its end.
    pub elapsed: Duration,
    /// Live sessions at the last pass.
    pub sessions: usize,
    /// Of those, the ones whose probe answered idle.
    pub idle: usize,
    /// The ones whose probe answered busy, or did not answer in time.
    pub busy: usize,
    /// The drain ran to its cap with a session still busy.
    pub cap_used: bool,
}

/// [`drain_with`] with no worker record writes to wait for.
#[cfg(test)]
pub(crate) async fn drain(cap: Duration, live: impl Fn() -> Vec<SessionHandle>) -> DrainOutcome {
    drain_with(cap, live, || 0).await
}

/// Step 2's drain (§2.8): wait up to `cap` for every live session
/// (`live`, read again each pass) to be idle, asking every
/// [`DRAIN_REPOLL`]. Each pass asks every session at once under the
/// drain's one deadline (`t0 + cap`); a probe with no answer by then counts
/// as busy, so neither a pass nor the sleep between passes runs past the
/// cap. `writing` (read each pass) counts worker record writes in flight
/// (spec §5): while any is, the drain does not end early. A zero cap
/// (`--now`) asks nothing: `idle` and `busy` stay 0. Ends with one INFO
/// line.
pub(crate) async fn drain_with(
    cap: Duration,
    live: impl Fn() -> Vec<SessionHandle>,
    writing: impl Fn() -> usize,
) -> DrainOutcome {
    let t0 = tokio::time::Instant::now();
    let deadline = t0 + cap;
    let mut writers = 0;
    let (sessions, idle, busy, cap_used) = if cap.is_zero() {
        (live().len(), 0, 0, false)
    } else {
        loop {
            let handles = live();
            let answers = futures::future::join_all(handles.iter().map(|h| async move {
                tokio::time::timeout_at(deadline, super::session_is_idle(h)).await.unwrap_or(false)
            }))
            .await;
            let sessions = handles.len();
            let idle = answers.iter().filter(|&&a| a).count();
            let busy = sessions - idle;
            writers = writing();
            if busy == 0 && writers == 0 {
                break (sessions, idle, busy, false);
            }
            let next = tokio::time::Instant::now() + DRAIN_REPOLL;
            if next >= deadline {
                tokio::time::sleep_until(deadline).await;
                break (sessions, idle, busy, true);
            }
            tokio::time::sleep_until(next).await;
        }
    };
    let elapsed = t0.elapsed();
    tracing::info!(
        elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        cap_ms = u64::try_from(cap.as_millis()).unwrap_or(u64::MAX),
        sessions,
        idle,
        busy,
        writers,
        cap_used,
        "daemon: reload drain ended"
    );
    DrainOutcome { elapsed, sessions, idle, busy, cap_used }
}

/// `Checkpoint{Reload}` every handle concurrently, after the drain. A drain
/// that ran to its cap has already given every quiesced turn its chance to
/// end at a round boundary, so the checkpoints skip the quiesce grace. A
/// drain that ended early, and `--now` (no drain at all, so the grace is a
/// turn's only chance), keep it.
pub(crate) async fn checkpoint_all(
    handles: &[SessionHandle],
    drained: &DrainOutcome,
) -> Vec<Option<SessionReloadRecord>> {
    let skip_quiesce_grace = drained.cap_used;
    futures::future::join_all(handles.iter().cloned().map(|h| checkpoint_one(h, skip_quiesce_grace))).await
}

/// The session as it IS: the actor's record when the checkpoint confirmed,
/// else what the handle alone knows (journal, cwd, keep-warm, lifecycle,
/// current model from the published view, the `/keepwarm` override).
pub(crate) fn record(handle: &SessionHandle, rec: Option<SessionReloadRecord>) -> ReloadSession {
    let meta = handle.meta();
    let journal_id = handle.journal_id();
    let turn_cut = rec.as_ref().map(|r| r.turn_cut);
    let cut_tail = rec.as_ref().and_then(|r| r.cut_tail);
    let capacity_stop = rec.as_ref().and_then(|r| r.capacity_stop.clone());
    // The checkpoint's, else the last published entry's (a session whose
    // checkpoint timed out keeps its eviction clock too).
    let parked_at = match &rec {
        Some(r) => r.parked_since,
        None => handle.live_entry().and_then(|e| e.parked_since),
    };
    if rec.is_none() {
        tracing::warn!(
            session = %handle.id,
            "reload: no checkpoint reply in time — this session's workers are not carried: \
             no archive is guaranteed and their parent will not be woken"
        );
    }
    let (config, keep_warm, lifecycle, settings_replay, cache_keepwarm, model, workers, tool_activations) = match rec {
        Some(r) => (
            r.config,
            r.keep_warm,
            r.lifecycle,
            r.settings_replay,
            r.cache_keepwarm,
            r.model,
            r.workers,
            r.tool_activations,
        ),
        None => (
            SessionConfig {
                cwd: meta.cwd.clone(),
                // Like `/keepwarm`: the user's `/memory` choice survives a
                // reload whose checkpoint did not confirm.
                memory_mode: handle.memory_mode(),
                env: None,
            env_stripped: Vec::new(),
                ..SessionConfig::default()
            },
            // The pin is on the handle too, so it survives a reload whose
            // checkpoint did not confirm.
            handle.pinned(),
            handle.lifecycle(),
            Vec::new(),
            // Persisted independently of the checkpoint: an explicit
            // `/keepwarm` choice must survive a reload that timed out.
            handle.cache_keepwarm_override(),
            handle.view().model.clone(),
            // No checkpoint reply: whatever workers ran are lost to the exec
            // without a record (the actor never answered).
            Vec::new(),
            // Nor are its tool activations known.
            Vec::new(),
        ),
    };
    ReloadSession {
        id: meta.id.as_str().to_string(),
        journal_id: journal_id.clone(),
        config: SessionConfig {
            continue_session: Some(Some(journal_id)),
            model_override: Some(model),
            keep_warm,
            // The journal carries the CURRENT name (saveas after create);
            // re-applying the create-time `--name` would undo a rename.
            name: None,
            ..config
        },
        keep_warm,
        lifecycle,
        settings_replay,
        cache_keepwarm,
        // As of this record's write: informational, read by nothing on
        // restore (a restored session starts with no clients).
        input_owner_kind: handle.presence().input_owner_kind,
        workers,
        tool_activations,
        turn_cut,
        cut_tail,
        capacity_stop,
        parked_at,
    }
}

/// Steps 1–4 (§2.8): gate, drain, checkpoint, reload-state, announce.
/// Nothing irreversible has happened when this returns `Ok`; the caller
/// (the requesting connection) sends its own `Bye{Reloading}`, flushes,
/// then calls [`exec`].
pub struct Prepared {
    pub exe: PathBuf,
    pub rs_path: PathBuf,
    pub generation: u64,
    /// The capacity-resume seal this reload closed before its checkpoints
    /// (a failed exec reopens it).
    pub seal: Option<u64>,
    /// The workers whose records a forced reload went ahead without.
    pub waived: Vec<crate::session::wire::WaivedWorker>,
}

pub async fn prepare(
    state: &Arc<DaemonState>,
    paths: &DaemonPaths,
    req: ReloadRequest,
) -> Result<Prepared, ReloadError> {
    // 1. version gate — before anything is disturbed.
    // The recorded exe from this image's own record (not daemon.json, which
    // can be lost while the daemon runs).
    let exe = req.exe.clone().unwrap_or_else(|| state.daemon_info().exe.unwrap_or_else(resolve_exe));
    let exe = validate_exe(&exe).map_err(ReloadError::Refused)?;
    let new = probe_version(&exe).await.map_err(ReloadError::Refused)?;
    version_gate(&PrintVersion::current(), &new).map_err(ReloadError::Refused)?;
    // A crash restore still bringing sessions back: the reload state would
    // miss the ones it has not restored yet (they'd stay on disk).
    if state.restoring.running() {
        return Err(ReloadError::Refused(
            "a crash restore is still running; reload once it is done (its summary line is in the daemon log)".into(),
        ));
    }
    if state.reloading.swap(true, Ordering::SeqCst) {
        return Err(ReloadError::Refused("a reload is already in progress".into()));
    }
    #[cfg(feature = "testing")]
    hold_worker_writer_for_test();
    // (§5) Before anything is quiesced: a worker run whose record is
    // unresolved refuses a normal reload at once, so no turn pauses.
    let unresolved = super::worker_check::unresolved(&super::worker_check::writers_snapshot());
    if !unresolved.is_empty() && !req.force {
        let why = super::worker_check::refusal(&unresolved);
        tracing::warn!(target: LOG_TARGET, "reload refused: {why}");
        state.reloading.store(false, Ordering::SeqCst);
        return Err(ReloadError::Refused(why));
    }
    let generation = state.generation + 1;
    tracing::info!(exe = %exe.display(), new = %new.binary_version, generation, "daemon: reload accepted");
    // Quiesce every live session before the drain. A running
    // `wait` returns `interrupted` at once, and a turn starts no new provider
    // round after its tool round, so the drain ends early instead of
    // waiting out a blocked tool. Every abort path clears it
    // (`abort_checkpoints`); an exec leaves with it set.
    for h in state.live_sessions() {
        h.quiesce(Some(crate::events::queue::Quiesce::Reload));
    }

    // 2. drain (refuse new work), then checkpoint every session concurrently.
    let cap = if req.now {
        Duration::ZERO
    } else {
        req.drain_secs
            .map(Duration::from_secs)
            .or_else(|| std::env::var(DRAIN_SECS_ENV).ok()?.parse().ok().map(Duration::from_secs))
            .unwrap_or(DEFAULT_DRAIN)
    };
    // A worker run whose record write is in flight is busy too (§5).
    let drained = drain_with(cap, || state.live_sessions(), || {
        super::worker_check::writing(&super::worker_check::writers_snapshot())
    })
    .await;
    // (§5) After the drain, before anything is stopped: writes still in
    // flight get a bounded wait to land. A record still not durable refuses
    // a normal reload: every session's quiesce lifts and a turn the drain
    // paused resumes in place. `--force` goes ahead without those records.
    let unpreserved = super::worker_check::settle(
        super::worker_check::WORKER_RECORD_WAIT,
        super::worker_check::writers_snapshot,
    )
    .await;
    let waived: Vec<_> = if unpreserved.is_empty() {
        Vec::new()
    } else if req.force {
        let waived: Vec<_> = unpreserved.iter().map(|u| u.waived()).collect();
        if let Some(line) = forced_line(&waived) {
            tracing::warn!(target: LOG_TARGET, "{line}");
        }
        waived
    } else {
        let why = super::worker_check::refusal(&unpreserved);
        tracing::warn!(target: LOG_TARGET, "reload refused: {why}");
        refuse_after_drain(state).await;
        state.reloading.store(false, Ordering::SeqCst);
        return Err(ReloadError::Refused(why));
    };
    // The checkpoints carry these workers with a wake that says so.
    crate::tools::worker_records::mark_waived(waived.iter().map(|w| w.worker.clone()));
    // Before the checkpoints: no capacity resume is dispatched from here
    // on, so the hand-off never holds a resume that was also given (a
    // resume the seal refuses stays armed and rides the checkpoint).
    let link = state.host.record_link();
    let seal = link.as_ref().map(|l| l.seal());
    let handles = state.live_sessions();
    let results = checkpoint_all(&handles, &drained).await;
    let mut sessions = Vec::with_capacity(handles.len());
    for (h, rec) in handles.iter().zip(results) {
        if rec.is_none() {
            tracing::warn!(session = %h.id, "reload: checkpoint did not confirm; recording from the handle");
        }
        sessions.push(record(h, rec));
    }

    // 3. reload-state (0600, atomic).
    let rs = ReloadState {
        generation,
        written_at: chrono::Utc::now(),
        sessions,
        expected_clients: state.connections.load(Ordering::SeqCst),
        // After every checkpoint: no session can launch a worker any more.
        next_subagent_id: crate::tools::peek_next_subagent_id(),
        base_umask: super::umask::outcome().map(|o| o.base),
        children: crate::session::children::snapshot_for_reload(),
    };
    let rs_path = reload_state_path(paths);
    // If either write fails we early-return WITHOUT reaching exec() (which is the
    // only other place `reloading` is cleared), so reset the drain flag here —
    // otherwise a failed reload-state write bricks the daemon (it refuses every
    // new Create/Submit/Compact until restart).
    let written = serde_json::to_vec_pretty(&rs)
        .map_err(|e| e.to_string())
        .and_then(|body| registry::write_private_atomic(&rs_path, &body).map_err(|e| e.to_string()));
    if let Err(e) = written {
        // No exec will follow: every checkpointed session lifts its fence
        // and hears about the workers its checkpoint stopped.
        abort_checkpoints(state).await;
        state.reloading.store(false, Ordering::SeqCst);
        if let (Some(link), Some(seal)) = (link, seal) {
            link.unseal(seal);
        }
        return Err(ReloadError::ExecFailed(e));
    }
    // A new image that dies before its own first record write still leaves
    // the next start a record of these sessions, with this reload's cut
    // turns as the checkpoints recorded them (`crash_record`).
    if let Some(rec) = &state.crash_record {
        rec.hand_off(&rs);
    }

    // 4. announce — every OTHER conn selects on `reload_announce` and sends
    //    Event(Reloading) + Bye{Reloading}; give their writers ≤ 1 s.
    state.reload_generation.store(generation, Ordering::SeqCst);
    state.fire_announce();
    let t1 = std::time::Instant::now();
    while state.connections.load(Ordering::SeqCst) > 1 && t1.elapsed() < Duration::from_secs(crate::session::budgets::RELOAD_ANNOUNCE_WAIT_SECS) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(Prepared { exe, rs_path, generation, seal, waived })
}

/// Step 5: rewrite `daemon.json` (generation+1, same pid), stop sidecars,
/// hand the flock to the next image and `execv`. Returns only on failure —
/// then `daemon.json`/reload-state are restored, extension discovery is
/// re-spawned (the sidecars were stopped for the exec — M4: no zombie
/// daemon without hooks), and the old image keeps serving (sessions
/// checkpointed but alive; clients that got `Bye` reconnect to THIS image
/// — a `reconnect_of.generation` mismatch is tolerated, the daemon answers
/// with its own generation).
pub async fn exec(state: &Arc<DaemonState>, p: Prepared) -> ReloadError {
    // From this image's own record, not daemon.json (which can be lost while
    // the daemon runs). The record is held and written under the metadata
    // lock, so a heal in the hand-off window writes the same.
    // `previous` stays untouched: a failed exec puts it back whole.
    let previous = state.daemon_info();
    let prev_generation = previous.generation;
    let info = registry::DaemonInfo {
        generation: p.generation,
        started_at: chrono::Utc::now(),
        exe: Some(p.exe.clone()),
        ..previous.clone()
    };
    if let Err(e) = state.meta.publish(info) {
        tracing::warn!(path = %state.paths.json.display(), error = %e, "daemon: daemon.json write failed at reload");
    }
    let _ = tokio::time::timeout(
        Duration::from_secs(crate::session::budgets::RELOAD_EXT_SHUTDOWN_SECS),
        async { state.host.ext_manager().write().await.shutdown_all().await },
    )
    .await;

    #[cfg(unix)]
    let err = exec_self(&p.exe, &p.rs_path, state);
    #[cfg(windows)]
    let err = exec_successor(&p.exe, &p.rs_path, state).await;
    tracing::error!(error = %err, "daemon: reload exec failed; continuing on the old image");
    // The record before this attempt, whole: exe and started_at too, so the
    // next default reload uses the old exe, not the one that failed.
    if let Err(e) = state.meta.publish(previous) {
        tracing::warn!(
            path = %state.paths.json.display(),
            error = %e,
            "daemon: daemon.json write failed after the failed reload exec"
        );
    }
    let _ = std::fs::remove_file(&p.rs_path);
    state.reload_generation.store(prev_generation, Ordering::SeqCst);
    state.reset_announce();
    // Sidecars were shut down for the exec: bring them back (bounded like
    // `run_foreground`'s first discovery) so the surviving image has hooks.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let _loader = crate::extensions::loader::spawn_discover_and_load(
        Arc::clone(state.host.ext_manager()),
        tx,
        None,
    );
    let wait = async {
        while let Some(ev) = rx.recv().await {
            if let crate::extensions::loader::ExtensionLoaderEvent::Finished { loaded, failed } = ev {
                tracing::info!(loaded = loaded.len(), failed = failed.len(), "daemon: extensions re-discovered after failed reload");
                break;
            }
        }
    };
    if tokio::time::timeout(
        Duration::from_secs(crate::session::budgets::RELOAD_EXT_REDISCOVERY_SECS),
        wait,
    )
    .await
    .is_err()
    {
        tracing::warn!("daemon: extension re-discovery still running; resuming the sessions");
    }
    // The checkpoint fenced every session and stopped its workers for an
    // exec that did not happen: their parents are still here and still
    // waiting — lift the fences and tell them.
    abort_checkpoints(state).await;
    state.reloading.store(false, Ordering::SeqCst);
    if let Some(rec) = &state.crash_record {
        rec.resume_writes();
    }
    // This image goes on: its own seal reopens (a stop's stays).
    if let (Some(link), Some(seal)) = (state.host.record_link(), p.seal) {
        link.unseal(seal);
    }
    ReloadError::ExecFailed(err)
}

#[cfg(unix)]
fn exec_self(exe: &Path, rs_path: &Path, state: &DaemonState) -> String {
    use std::os::unix::process::CommandExt;
    let Some(lock_fd) = state.lock_fd.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|l| l.raw_fd()) else {
        return "no lock fd to inherit".into();
    };
    // Clear FD_CLOEXEC on the lock so the new image adopts the SAME flock
    // (liveness oracle never flickers). The listener keeps CLOEXEC: it is
    // closed at exec and the new image rebinds the path.
    // SAFETY: fcntl on an fd we own.
    unsafe {
        let flags = libc::fcntl(lock_fd, libc::F_GETFD);
        if flags >= 0 {
            libc::fcntl(lock_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
        }
    }
    let mut args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    if args.is_empty() || args.iter().all(|a| a != "daemon") {
        args = vec!["daemon".into(), "--foreground".into()];
    }
    let mut cmd = std::process::Command::new(exe);
    cmd.args(&args)
        .env("SYNAPS_DAEMON", "1")
        .env(LOCK_FD_ENV, lock_fd.to_string())
        .env(RELOAD_STATE_ENV, rs_path)
        .env(SUBAGENT_ID_FLOOR_ENV, crate::tools::peek_next_subagent_id().to_string());
    #[cfg(all(target_os = "linux", feature = "testing"))]
    leave_children_for_test();
    let e = cmd.exec();
    // Only reached on failure: restore CLOEXEC.
    // SAFETY: as above.
    unsafe {
        let flags = libc::fcntl(lock_fd, libc::F_GETFD);
        if flags >= 0 {
            libc::fcntl(lock_fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
        }
    }
    e.to_string()
}

/// Windows `execv`: start the new image with the reload-state and a
/// hand-off event, wait for it to queue its `daemon.lock` request behind
/// ours, then exit — no destructors (they would unlink the `daemon.json`
/// that now names the next generation), no session `End` (the new image
/// rehydrates them): what the exec discards on Unix. Returns only on
/// failure, with this image intact.
#[cfg(windows)]
async fn exec_successor(exe: &Path, rs_path: &Path, state: &DaemonState) -> String {
    use super::win;
    let armed = match win::Event::create_unique("handoff") {
        Ok(e) => e,
        Err(e) => return format!("hand-off event: {e}"),
    };
    let mut args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    if args.is_empty() || args.iter().all(|a| a != "daemon") {
        args = vec!["daemon".into(), "--foreground".into()];
    }
    let mut env = crate::process_scope::inherited_env();
    env.retain(|(k, _)| k != super::READY_EVENT_ENV);
    crate::process_scope::set_env(&mut env, "SYNAPS_DAEMON", "1");
    crate::process_scope::set_env(&mut env, RELOAD_STATE_ENV, rs_path.as_os_str());
    crate::process_scope::set_env(&mut env, HANDOFF_EVENT_ENV, armed.name());
    crate::process_scope::set_env(&mut env, HANDOFF_PID_ENV, std::process::id().to_string());
    crate::process_scope::set_env(&mut env, SUBAGENT_ID_FLOOR_ENV, crate::tools::peek_next_subagent_id().to_string());
    let child = match win::spawn_detached(exe, &args, &env) {
        Ok(c) => c,
        Err(e) => return format!("cannot start {}: {e}", exe.display()),
    };
    let waited = tokio::task::spawn_blocking(move || {
        let outcome = win::wait_event_or_exit(&armed, &child, HANDOFF_ARM_TIMEOUT);
        (child, outcome)
    })
    .await;
    let (child, outcome) = match waited {
        Ok(r) => r,
        Err(e) => return format!("hand-off wait: {e}"),
    };
    match outcome {
        win::Wait::Signalled => {
            tracing::info!(pid = child.id(), "daemon: reload hand-off armed; this image exits");
            state.host.flush_logs();
            std::process::exit(0);
        }
        win::Wait::Exited => {
            let mut child = child;
            let code = child.wait(Duration::ZERO);
            let tail = win::stderr_tail(child.take_stderr(), 64 * 1024, Duration::from_millis(500));
            let why = tail
                .lines()
                .map(str::trim)
                .rfind(|l| !l.is_empty())
                .map(|l| l.trim_start_matches("synaps daemon: ").to_string())
                .unwrap_or_else(|| "no output".into());
            format!(
                "new image exited (code {}) before taking over: {why}",
                code.map_or_else(|| "?".to_string(), |c| c.to_string())
            )
        }
        win::Wait::TimedOut => {
            child.kill();
            let _ = child.wait(Duration::from_secs(5));
            format!("new image did not take over within {HANDOFF_ARM_TIMEOUT:?}")
        }
    }
}

/// What a new image adopts: the held lock, the reload-state and its path.
type Adopted = (DaemonLock, ReloadState, PathBuf);

/// Windows: the hand-off taken at the top of `run_foreground`, held here
/// for [`adopt_from_env`].
#[cfg(windows)]
static TAKEN_OVER: std::sync::Mutex<Option<Adopted>> = std::sync::Mutex::new(None);

/// Windows: take `daemon.lock` over from the previous image now, before
/// anything boots (the previous image exits on our signal, as the exec
/// would have replaced it). A no-op unless this is a reload.
#[cfg(windows)]
pub fn take_over_from_env(paths: &DaemonPaths) -> anyhow::Result<()> {
    if let Some(adopted) = handoff_from_env(paths)? {
        *TAKEN_OVER.lock().unwrap_or_else(|e| e.into_inner()) = Some(adopted);
    }
    Ok(())
}

/// New-image side (`Daemon::start`), Windows: what [`take_over_from_env`]
/// took, or the hand-off done now.
#[cfg(windows)]
pub fn adopt_from_env(paths: &DaemonPaths) -> anyhow::Result<Option<Adopted>> {
    if let Some(adopted) = TAKEN_OVER.lock().unwrap_or_else(|e| e.into_inner()).take() {
        return Ok(Some(adopted));
    }
    handoff_from_env(paths)
}

/// Windows hand-off, new-image side: parse the reload-state first (a bad
/// one fails while the old image can still carry on), queue `daemon.lock`
/// behind the old image and set its hand-off event, take the grant when it
/// exits, then wait for its process to be gone. The event is mandatory —
/// without it the old image would never let go (refuse, as Unix refuses a
/// reload-state without its lock fd).
#[cfg(windows)]
fn handoff_from_env(paths: &DaemonPaths) -> anyhow::Result<Option<Adopted>> {
    let Some(rs_path) = std::env::var_os(RELOAD_STATE_ENV) else {
        return Ok(None);
    };
    let rs_path = PathBuf::from(rs_path);
    std::env::remove_var(RELOAD_STATE_ENV);
    let event = std::env::var(HANDOFF_EVENT_ENV).ok();
    std::env::remove_var(HANDOFF_EVENT_ENV);
    let old_pid = std::env::var(HANDOFF_PID_ENV).ok().and_then(|s| s.parse::<u32>().ok());
    std::env::remove_var(HANDOFF_PID_ENV);
    let id_floor = std::env::var(SUBAGENT_ID_FLOOR_ENV).ok().and_then(|s| s.parse::<u64>().ok());
    std::env::remove_var(SUBAGENT_ID_FLOOR_ENV);
    let event = event.ok_or_else(|| {
        anyhow::anyhow!("{RELOAD_STATE_ENV} set without {HANDOFF_EVENT_ENV}: refusing to start (the lock is the liveness oracle)")
    })?;
    let body = std::fs::read_to_string(&rs_path)?;
    let mut rs: ReloadState = serde_json::from_str(&body)?;
    if let Some(floor) = id_floor {
        rs.next_subagent_id = rs.next_subagent_id.max(floor);
    }
    // Opened before the signal, so its pid cannot be reused under us.
    let old = old_pid.and_then(super::win::Process::open);
    let mut signal_err = None;
    let lock = DaemonLock::acquire_handoff(paths, HANDOFF_EXIT_TIMEOUT, || {
        signal_err = super::win::signal_event(&event).err();
    })
    .map_err(|e| match &signal_err {
        Some(s) => anyhow::anyhow!("taking daemon.lock over: {e} (the previous image was never told: {s})"),
        None => anyhow::anyhow!("taking daemon.lock over from the previous image: {e}"),
    })?;
    if let Some(old) = old {
        if !old.wait(Duration::from_secs(10)) {
            tracing::warn!(pid = old_pid, "daemon: the previous image still runs after releasing daemon.lock");
        }
    }
    tracing::info!(generation = rs.generation, "daemon: took daemon.lock over from the previous image");
    // The previous image read our stderr; it is gone.
    super::win::detach_stdio();
    Ok(Some((lock, rs, rs_path)))
}

/// Test seam (the `testing` feature only; never in a release build): with
/// `SYNAPS_TEST_LEAVE_CHILDREN` naming a file, the old image leaves two
/// children it never waits for right before the exec, `true` already exited
/// (a zombie at the exec) and `sleep 5` still running, and writes their pids
/// there ("<exited> <running>"). The new image inherits both.
#[cfg(all(target_os = "linux", feature = "testing"))]
#[allow(clippy::zombie_processes)] // leaving them unwaited is the point
fn leave_children_for_test() {
    let Some(path) = std::env::var_os("SYNAPS_TEST_LEAVE_CHILDREN") else {
        return;
    };
    let Ok(exited) = std::process::Command::new("true").spawn() else {
        return;
    };
    let exited = exited.id();
    let t0 = std::time::Instant::now();
    while !std::fs::read_to_string(format!("/proc/{exited}/stat")).is_ok_and(|s| s.contains(") Z "))
        && t0.elapsed() < Duration::from_secs(5)
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    let Ok(running) = std::process::Command::new("sleep").arg("5").spawn() else {
        return;
    };
    let _ = std::fs::write(path, format!("{exited} {}\n", running.id()));
}

/// The reload hand-off variables as the environment holds them (Unix: the
/// exec hand-off; Windows hands off through `take_over_from_env`).
#[cfg(unix)]
#[derive(Debug, Default, Clone)]
pub(crate) struct HandOffVars {
    pub state: Option<std::ffi::OsString>,
    pub lock_fd: Option<String>,
    pub id_floor: Option<String>,
}

#[cfg(unix)]
impl HandOffVars {
    fn from_env() -> Self {
        let text = |k: &str| std::env::var_os(k).map(|v| v.to_string_lossy().into_owned());
        Self {
            state: std::env::var_os(RELOAD_STATE_ENV),
            lock_fd: text(LOCK_FD_ENV),
            id_floor: text(SUBAGENT_ID_FLOOR_ENV),
        }
    }

    fn is_empty(&self) -> bool {
        self.state.is_none() && self.lock_fd.is_none() && self.id_floor.is_none()
    }

    /// Nothing after this (the carried mask, [`adopt_from_env`], the
    /// processes the daemon starts) sees them.
    fn remove_from_env() {
        std::env::remove_var(RELOAD_STATE_ENV);
        std::env::remove_var(LOCK_FD_ENV);
        std::env::remove_var(SUBAGENT_ID_FLOOR_ENV);
    }
}

/// What the environment's reload hand-off is, judged before any of it is
/// used. The environment alone proves nothing: a stop + start that copies a
/// reloaded daemon's environment (`/proc/<pid>/environ`) into a fresh
/// start hands it every variable of an earlier reload.
#[cfg(unix)]
#[derive(Debug)]
pub(crate) enum EnvHandOff {
    /// No hand-off variable is set: a fresh start.
    Absent,
    /// An exec'd image's hand-off. `fd` is open on this daemon's lock file
    /// (same device and inode as the lock path), and the state file parsed.
    Real { fd: i32, state: ReloadState, path: PathBuf, id_floor: Option<u64> },
    /// The variables are set, but the fd they name is not this daemon's
    /// lock: closed, another file, no fd number, or no state path at all.
    /// The start goes fresh and never touches that fd.
    Stale { why: String },
    /// The fd IS this daemon's lock, but the state file is missing or does
    /// not parse: a reload whose hand-off broke. Refused, as before (a fresh
    /// start could not take the flock that fd holds, and must not close it).
    Broken { why: String },
}

/// Judge the hand-off `vars` against this daemon's lock file `lock`. Reads
/// only: an `fstat` of the fd, a `stat` of the lock and a read of the state
/// file. The fd is never adopted, closed or changed here.
#[cfg(unix)]
pub(crate) fn classify_hand_off(vars: &HandOffVars, lock: &Path) -> EnvHandOff {
    if vars.is_empty() {
        return EnvHandOff::Absent;
    }
    let state = match vars.state.as_deref() {
        None => Err(format!("{RELOAD_STATE_ENV} is not set")),
        Some(p) => read_reload_state(Path::new(p)).map(|s| (s, PathBuf::from(p))),
    };
    let fd = match vars.lock_fd.as_deref() {
        None => Err(format!("{LOCK_FD_ENV} is not set")),
        Some(s) => match s.trim().parse::<i32>() {
            Ok(fd) if fd >= 0 => fd_is_lock(fd, lock).map(|()| fd),
            _ => Err(format!("{LOCK_FD_ENV}={s:?} is not an fd number")),
        },
    };
    match (state, fd) {
        (Ok((state, path)), Ok(fd)) => EnvHandOff::Real {
            fd,
            state,
            path,
            id_floor: vars.id_floor.as_deref().and_then(|s| s.trim().parse().ok()),
        },
        (Err(why), Ok(_)) if vars.state.is_some() => EnvHandOff::Broken { why },
        (state, fd) => EnvHandOff::Stale {
            why: [state.err(), fd.err()].into_iter().flatten().collect::<Vec<_>>().join("; "),
        },
    }
}

/// The state file at `path`, a regular file only (opened `O_NONBLOCK`, so a
/// FIFO left at the path can't block the start).
// Windows: read by the tests only (its hand-off reads the state itself).
#[cfg_attr(not(unix), allow(dead_code))]
fn read_reload_state(path: &Path) -> Result<ReloadState, String> {
    use std::io::Read;
    let shown = path.display();
    match std::fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!("the state file {shown} does not exist"));
        }
        Err(e) => return Err(format!("the state file {shown}: {e}")),
        Ok(m) if !m.is_file() => return Err(format!("the state file {shown} is not a regular file")),
        Ok(_) => {}
    }
    let mut file = open_regular_file(path).ok_or_else(|| format!("the state file {shown} can't be opened as a regular file"))?;
    let mut body = String::new();
    file.read_to_string(&mut body).map_err(|e| format!("the state file {shown}: {e}"))?;
    serde_json::from_str(&body).map_err(|e| format!("the state file {shown} {}", parse_failure(&e)))
}

/// A record or state file that does not parse, for an error or a log line:
/// serde's class and where (line, column), never its message (it quotes the
/// stored value).
pub(crate) fn parse_failure(e: &serde_json::Error) -> String {
    use serde_json::error::Category;
    let class = match e.classify() {
        Category::Io => "io",
        Category::Syntax => "syntax",
        Category::Data => "data",
        Category::Eof => "eof",
    };
    format!("does not parse ({class}, line {}, column {})", e.line(), e.column())
}

/// `Ok` if `fd` is open on the file at `lock` (same device and inode).
#[cfg(unix)]
#[allow(clippy::unnecessary_cast)] // the stat field types differ between platforms
fn fd_is_lock(fd: i32, lock: &Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: fstat only writes one stat struct to a valid out-pointer; it
    // reads the fd's status and never changes, closes or adopts the fd.
    if unsafe { libc::fstat(fd, st.as_mut_ptr()) } != 0 {
        let e = std::io::Error::last_os_error();
        return Err(if e.raw_os_error() == Some(libc::EBADF) {
            format!("fd {fd} is not open")
        } else {
            format!("fd {fd}: {e}")
        });
    }
    // SAFETY: fstat returned 0, so it filled the struct.
    let st = unsafe { st.assume_init() };
    let m = std::fs::metadata(lock).map_err(|e| format!("the lock file {}: {e}", lock.display()))?;
    if (st.st_dev as u64, st.st_ino as u64) != (m.dev(), m.ino()) {
        return Err(format!("fd {fd} is not {}", lock.display()));
    }
    Ok(())
}

/// A stale hand-off: one WARN in the daemon log and one line on stderr,
/// then the variables are removed. The fd they name is never touched.
#[cfg(unix)]
fn forget_stale_hand_off(why: &str) {
    tracing::warn!(target: LOG_TARGET, "daemon: stale reload hand-off in the environment ({why}); starting fresh");
    agent_core::eprintln_lossy!("synaps daemon: stale reload hand-off in the environment ({why}); starting fresh");
    HandOffVars::remove_from_env();
}

/// `synaps daemon --foreground`, first thing (before the carried mask reads
/// the state path): a STALE hand-off in the environment is reported and
/// removed, so the start is fresh from here on. A real or absent one is
/// left as it is for [`adopt_from_env`]. `true` if one was removed.
#[cfg(unix)]
pub fn discard_stale_hand_off(paths: &DaemonPaths) -> bool {
    match classify_hand_off(&HandOffVars::from_env(), &paths.lock) {
        EnvHandOff::Stale { why } => {
            forget_stale_hand_off(&why);
            true
        }
        _ => false,
    }
}

/// `synaps daemon --foreground`, FIRST thing (before the inherited-child
/// snapshot, the host boot and everything after them, any of which may
/// start a process): a REAL hand-off's lock fd gets `FD_CLOEXEC` back at
/// once. The old image cleared it so that the fd survives the exec. Left
/// clear, every process this image starts (an extension sidecar first, then
/// MCP servers and tools) inherits an fd on `daemon.lock`, and one that
/// outlives the daemon holds the flock, so every later fresh start refuses.
/// `fcntl(F_SETFD)` on the fd `LOCK_FD_ENV` names, and only if it is open on
/// `paths.lock` (same device and inode, as [`classify_hand_off`] checks):
/// the flock and the open file description stay as they are, and
/// [`adopt_from_env`] adopts the same fd later. That covers a real hand-off
/// and a broken one (the lock fd with a bad state file, which
/// `Daemon::start` refuses only after the sidecars have started). A stale fd
/// (not the lock) or no hand-off is not touched. `true` if the flag was set.
#[cfg(unix)]
pub fn seal_hand_off_lock(paths: &DaemonPaths) -> bool {
    seal_hand_off_lock_with(&HandOffVars::from_env(), &paths.lock)
}

#[cfg(unix)]
fn seal_hand_off_lock_with(vars: &HandOffVars, lock: &Path) -> bool {
    let Some(fd) = vars.lock_fd.as_deref().and_then(|s| s.trim().parse::<i32>().ok()).filter(|fd| *fd >= 0) else {
        return false;
    };
    fd_is_lock(fd, lock).is_ok() && set_cloexec(fd)
}

#[cfg(unix)]
fn set_cloexec(fd: i32) -> bool {
    // SAFETY: F_GETFD/F_SETFD on the hand-off fd, verified open on
    // daemon.lock; only its close-on-exec flag changes.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        flags >= 0 && libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) == 0
    }
}

/// New-image side (`Daemon::start`): a REAL hand-off (see
/// [`classify_hand_off`]: the state file parses and `LOCK_FD_ENV` is open on
/// `paths.lock`) ADOPTS that fd's flock and returns the recorded state. A
/// stale one is reported and removed, and the start is fresh (`None`): the
/// caller then takes the flock itself, so the flock stays the liveness
/// oracle. A real fd with a broken state refuses to start. The adopt sets
/// `FD_CLOEXEC` (as [`seal_hand_off_lock`] does earlier for
/// `run_foreground`) before `Daemon::start` starts any process of its own.
#[cfg(unix)]
pub fn adopt_from_env(paths: &DaemonPaths) -> anyhow::Result<Option<Adopted>> {
    match classify_hand_off(&HandOffVars::from_env(), &paths.lock) {
        EnvHandOff::Absent => Ok(None),
        EnvHandOff::Stale { why } => {
            forget_stale_hand_off(&why);
            Ok(None)
        }
        EnvHandOff::Broken { why } => {
            HandOffVars::remove_from_env();
            anyhow::bail!("reload hand-off: {why}: refusing to start (the inherited {LOCK_FD_ENV} is this daemon's lock)")
        }
        EnvHandOff::Real { fd, mut state, path, id_floor } => {
            HandOffVars::remove_from_env();
            let lock = DaemonLock::adopt(fd)?;
            if let Some(floor) = id_floor {
                state.next_subagent_id = state.next_subagent_id.max(floor);
            }
            Ok(Some((lock, state, path)))
        }
    }
}

/// Rehydrate every recorded session BEFORE accepting. A session that
/// fails to come back is logged and skipped (its journal is on disk;
/// `--continue` brings it back).
///
/// A session whose journal was never written (no turn yet — `save` skips
/// an empty conversation) cannot be continued; it is recreated fresh under
/// a new id and aliased from the old one.
/// F23: detect "is live in another process" in the error string — this is
/// the `SessionLockError::Held` case, where an in-process runtime holds
/// the journal lock. Instead of aliasing to an empty impostor, register a
/// Parked placeholder under the SAME id.
pub(crate) fn is_lock_held_error(e: &str) -> bool {
    e.contains("is live in another process")
}

/// Extract `"pid N (kind)"` from the lock-held error message.
pub(crate) fn parse_holder_from_error(e: &str) -> String {
    // "session X is live in another process (pid N, kind) — …"
    if let Some(start) = e.find("(pid ") {
        let after = &e[start + 1..]; // "pid N, kind) — …"
        if let Some(end) = after.find(')') {
            return after[..end].to_string();
        }
    }
    "unknown holder".to_string()
}

/// No exec after the checkpoints (it failed, or the reload stopped before
/// it): every live session lifts its reload fence and is handed back the
/// workers its checkpoint stopped (`ReloadAborted`, a no-op for a session
/// that never checkpointed). The actor re-announces from its own record,
/// so a checkpoint that answered too late for the reload record is covered.
async fn abort_checkpoints(state: &Arc<DaemonState>) {
    for h in state.live_sessions() {
        h.quiesce(None);
    }
    let budget = Duration::from_secs(crate::session::budgets::RELOAD_ABORT_SEND_SECS);
    let sends = state
        .live_sessions()
        .into_iter()
        .map(|h| async move { send_reload_aborted(&h, budget).await });
    futures::future::join_all(sends).await;
}

/// `ReloadAborted` must arrive: dropped, the session would wait out the
/// fence's backstop. `SessionHandle::send` never waits for queue room, so
/// retry a full queue (the actor drains it) for up to `budget`.
pub(crate) async fn send_reload_aborted(h: &SessionHandle, budget: Duration) -> bool {
    match send_with_room(h, || SessionCommand::ReloadAborted, budget).await {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(
                session = %h.id,
                error = %e,
                "reload: could not tell the session the exec failed; its reload fence lifts at the backstop"
            );
            false
        }
    }
}

/// `cmd()` to `h`, retrying a full queue for up to `budget`.
async fn send_with_room(
    h: &SessionHandle,
    cmd: impl Fn() -> SessionCommand,
    budget: Duration,
) -> Result<(), crate::session::TransportError> {
    use crate::session::TransportError;
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        match h.send(cmd()).await {
            Err(TransportError::Backpressure) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            done => return done,
        }
    }
}

/// A reload refused after its drain, before anything was stopped (spec
/// §5): every live session is told ([`refuse_sessions`]), so a turn the
/// drain paused at a round boundary resumes in place.
async fn refuse_after_drain(state: &Arc<DaemonState>) {
    refuse_sessions(&state.live_sessions(), chrono::Utc::now()).await;
}

/// Tell each of `handles`, all at once, that the reload was refused after
/// its drain (`ReloadRefused`, spec §5). The quiesce is not lifted here:
/// each session lifts its own when it handles the command, so a turn whose
/// end the session handles first still finds the quiesce up, and gets its
/// restart marker and then its one resume. Only a session the command
/// cannot reach (gone, or its queue full past the budget) has its quiesce
/// lifted from here: it keeps working, and a turn the drain paused stays
/// paused. `pub` so the actor tests drive this production order.
#[doc(hidden)]
pub async fn refuse_sessions(handles: &[SessionHandle], refused_at: chrono::DateTime<chrono::Utc>) {
    let budget = Duration::from_secs(crate::session::budgets::RELOAD_ABORT_SEND_SECS);
    let sends = handles.iter().map(|h| async move {
        if let Err(e) = send_with_room(h, || SessionCommand::ReloadRefused { refused_at }, budget).await {
            h.quiesce(None);
            tracing::warn!(
                session = %h.id,
                error = %e,
                "reload: could not tell the session the reload was refused; its quiesce lifts from here, and a turn the drain paused stays paused"
            );
        }
    });
    futures::future::join_all(sends).await;
}

/// Test seam (the `testing` feature only; never in a release build): with
/// `SYNAPS_TEST_HOLD_WORKER_WRITER` set to `publishing` or `unresolved`,
/// this image holds, from its first reload on, one writer-table entry
/// (`sa_test`, session `test-session`) in that state for no real run, so a
/// test can drive the reload's worker-record check (spec §5) with worker
/// rollover off.
#[cfg(feature = "testing")]
fn hold_worker_writer_for_test() {
    use crate::tools::worker_records::{PendingOp, PublicationState, TestWriter};
    static HELD: std::sync::OnceLock<Option<TestWriter>> = std::sync::OnceLock::new();
    HELD.get_or_init(|| {
        let state = match std::env::var("SYNAPS_TEST_HOLD_WORKER_WRITER").ok()?.as_str() {
            "publishing" => PublicationState::Pending(PendingOp::Head(1)),
            "unresolved" => PublicationState::Unresolved,
            _ => return None,
        };
        Some(TestWriter::hold("sa_test", Some("test-session"), Some(0), state))
    });
}

/// The config a session is restored with: as saved, except that the
/// restore never waits on `on_session_start`. Each restored session runs
/// the hook in the background (the deferred path quick-start sessions
/// use), so the restore of N sessions does not take N hook round trips
/// while the socket waits. A session restored Parked runs it only once it
/// is unparked. Its `continue_session` is the journal id this daemon
/// recorded, so it is loaded by that exact id
/// ([`ContinueLookup::ExactJournalId`]): no chain or name look-up, which
/// would read the header of every session file, and no session merely
/// named like the id can take its place.
///
/// [`ContinueLookup::ExactJournalId`]: crate::session::ContinueLookup::ExactJournalId
fn restore_config(saved: &SessionConfig, lifecycle: SessionLifecycle) -> SessionConfig {
    SessionConfig {
        await_extensions: false,
        restore_parked: matches!(lifecycle, SessionLifecycle::Parked | SessionLifecycle::Parking),
        continue_lookup: crate::session::ContinueLookup::ExactJournalId,
        ..saved.clone()
    }
}

pub async fn rehydrate(state: &Arc<DaemonState>, rs: &ReloadState) {
    // Before any session can dispatch: ids continue where the old image
    // stopped, so `sa_1` never names two different workers.
    crate::tools::seed_next_subagent_id(rs.subagent_id_floor());
    // `daemon.reload_resume = off`: a cut turn stays cut (the session waits
    // for its user, as before).
    let resume = state.host.config().daemon.reload_resume;
    if !resume {
        tracing::info!(
            target: LOG_TARGET,
            generation = rs.generation,
            "reload resume: off (daemon.reload_resume = off); cut turns are not resumed"
        );
    }
    for s in &rs.sessions {
        rehydrate_one(
            state,
            s,
            &After::Reload {
                resume,
                generation: rs.generation,
                reloaded_at: rs.written_at,
                stopped: false,
            },
            None,
        )
        .await;
    }
}

/// What a rehydrated session is told after it is back: a reload's resume
/// (`ResumeAfterReload`), also for a clean stop's record (`stopped`), or a
/// crash restore's (`ResumeAfterCrash`).
pub(crate) enum After {
    Reload {
        resume: bool,
        generation: u64,
        reloaded_at: chrono::DateTime<chrono::Utc>,
        /// A clean stop's record (restored by a later start), not a reload.
        stopped: bool,
    },
    Crash {
        resume: bool,
        /// The dead daemon's process key (`session::live::process_key`).
        crash: String,
        /// The dead daemon's last record write.
        crashed_at: chrono::DateTime<chrono::Utc>,
        turn_in_flight: bool,
        lost_workers: Vec<String>,
    },
}

/// How one session's rehydrate went.
pub(crate) enum Rehydrated {
    /// Back Live (`parked`: restored Parked), with the actor's resume
    /// answer when one was asked for.
    Restored {
        parked: bool,
        resume: Option<tokio::sync::oneshot::Receiver<&'static str>>,
    },
    /// Journal locked by another process: a Parked placeholder holds the id
    /// (a reload's rehydrate only).
    Placeholder,
    /// Not back (why).
    Failed(String),
}

/// Ask a session that is back Live for `after`'s resume decision: the actor
/// decides ("resumed", "skip", "none") and answers on the returned channel.
/// `None`: nothing asked (the resume is off for it) or the send failed.
pub(crate) async fn ask_resume(
    h: &SessionHandle,
    s: &ReloadSession,
    after: &After,
) -> Option<tokio::sync::oneshot::Receiver<&'static str>> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let cmd = match after {
        After::Reload { resume: false, .. } | After::Crash { resume: false, .. } => return None,
        After::Reload { generation, reloaded_at, stopped, .. } => SessionCommand::ResumeAfterReload {
            generation: *generation,
            reloaded_at: *reloaded_at,
            turn_cut: s.turn_cut,
            cut_tail: s.cut_tail,
            stopped: *stopped,
            reply: Some(tx),
        },
        After::Crash { crash, crashed_at, turn_in_flight, lost_workers, .. } => SessionCommand::ResumeAfterCrash {
            crash: crash.clone(),
            crashed_at: *crashed_at,
            turn_in_flight: *turn_in_flight,
            lost_workers: lost_workers.clone(),
            reply: Some(tx),
        },
    };
    h.send(cmd).await.is_ok().then_some(rx)
}

/// The capacity resume a rehydrated session's actor is handed
/// (`ArmCapacityResume`): its record's, unless a client loaded the session
/// first (`found_live`: that actor is the client's, and a person is there).
fn capacity_stop_to_arm(s: &ReloadSession, found_live: bool) -> Option<crate::session::CapacityStop> {
    let stop = s.capacity_stop.clone()?;
    if found_live {
        tracing::info!(
            target: crate::session::capacity_resume::LOG_TARGET,
            session = %s.id,
            event = "capacity_resume",
            phase = "not_armed",
            why = "client_loaded",
            "capacity resume: not armed; a client loaded the session first"
        );
        return None;
    }
    Some(stop)
}

/// Rehydrate one recorded session (see [`rehydrate`]): create it from its
/// journal, re-apply the non-persisted knobs, then `after`'s resume, the
/// carried workers, the capacity resume its record carried
/// (`ArmCapacityResume`; not to a session a client loaded first, nor to one
/// recreated without its history), and the Park. A reload's rehydrate (`from_record:
/// None`) gives a lock-held journal a placeholder and an uncontinuable one
/// a fresh session aliased from the old id. A crash record's restore
/// (`from_record: Some(parked_since)`, the parked time it recorded) does
/// neither: either one fails the session (the restore gives it up; it stays
/// on disk, never recreated empty under its id).
pub(crate) async fn rehydrate_one(
    state: &Arc<DaemonState>,
    s: &ReloadSession,
    after: &After,
    from_record: Option<Option<chrono::DateTime<chrono::Utc>>>,
) -> Rehydrated {
    let mut config = restore_config(&s.config, s.lifecycle);
    // When it parked: the crash record's time for its restore; a reload's
    // own record for a session it restores Parked (`None`: an older image's
    // record, and never for a Live one), so its eviction clock continues.
    config.restore_parked_since = match from_record {
        Some(parked_since) => parked_since,
        None => s.parked_at.filter(|_| config.restore_parked),
    };
    // Recreated without its history: the history a capacity resume pins is
    // gone, so it is not handed over.
    let mut fresh = false;
    let created = match state.create(config.clone()).await {
        Ok(h) => Ok(h),
        Err(e) if from_record.is_some() && is_lock_held_error(&e) => {
            return Rehydrated::Failed(format!("journal locked by {}", parse_holder_from_error(&e)));
        }
        Err(e) if from_record.is_some() => return Rehydrated::Failed(e),
        // F23: journal locked by another process — register a Parked
        // placeholder under the SAME id. No alias, no fresh session.
        Err(e) if is_lock_held_error(&e) => {
            let holder = parse_holder_from_error(&e);
            tracing::warn!(
                session = %s.id, holder = %holder,
                "daemon: journal locked by another process; registering Parked placeholder"
            );
            let sid = crate::session::SessionId::from(s.id.as_str());
            let (handle, _task) = crate::session::handle::locked_placeholder::spawn(
                sid,
                s.config.clone(),
                holder,
            );
            state.insert(handle);
            if let Some(stop) = &s.capacity_stop {
                crate::session::capacity_resume::note_given_up(&s.id, stop);
            }
            return Rehydrated::Placeholder;
        }
        Err(e) if s.config.continue_session.is_some() => {
            tracing::warn!(session = %s.id, error = %e, "daemon: journal not continuable; recreating fresh");
            fresh = true;
            state
                .create(SessionConfig {
                    continue_session: None,
                    // A fresh session parks on its own clock.
                    restore_parked_since: None,
                    ..config.clone()
                })
                .await
        }
        Err(e) => Err(e),
    };
    let h = match created {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(session = %s.id, error = %e, "daemon: session could not be rehydrated");
            // A crash restore's give-up drops it with the carried entry
            // (`crash_record`); a reload's rehydrate has nothing that
            // carries it.
            if let (None, Some(stop)) = (from_record, &s.capacity_stop) {
                crate::session::capacity_resume::note_given_up(&s.id, stop);
            }
            return Rehydrated::Failed(e);
        }
    };
    let new_id = h.id.as_str().to_string();
    if new_id != s.id {
        state
            .reload_aliases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(s.id.clone(), new_id.clone());
    }
    // A crash restore whose create found the session already live (a
    // client's `--continue` got there first): that actor is the client's
    // now. It gets only the one resume decision and the carried workers
    // (both idempotent), never the recorded knobs or a Park.
    let found_live = from_record.is_some()
        && crate::host::CREATE_PROBE
            .try_with(|p| p.found_live.load(std::sync::atomic::Ordering::SeqCst))
            .unwrap_or(false);
    if found_live {
        tracing::info!(session = %s.id, "daemon: crash restore found the session live (a client loaded it first)");
    }
    // Upstream's carried tool activations, first of the recorded state,
    // before anything can start a turn: the same conversation continues in
    // this image, so the grants it held come with it (re-validated by the
    // new runtime at its first turn start). A session recreated fresh
    // (journal not continuable) is a new conversation and inherits none;
    // one a client loaded first keeps its own.
    if !fresh && !found_live && h.meta().continued && !s.tool_activations.is_empty() {
        let _ = h
            .send(SessionCommand::RestoreToolActivations {
                activations: s.tool_activations.clone(),
            })
            .await;
    }
    // Non-persisted knobs (host-originated `Set`: no owner check).
    for (i, setting) in s.settings_replay.iter().enumerate().filter(|_| !found_live) {
        let _ = h
            .send(SessionCommand::Set {
                id: u64::MAX - i as u64,
                setting: setting.clone(),
            })
            .await;
    }
    if s.keep_warm && !found_live {
        let _ = h.send(SessionCommand::KeepWarm { on: true }).await;
    }
    if let Some(mode) = s.cache_keepwarm.as_ref().filter(|_| !found_live) {
        let _ = h
            .send(SessionCommand::EngineCommand {
                id: u64::MAX,
                name: "keepwarm".into(),
                arg: mode.clone(),
            })
            .await;
    }
    // A turn the reload (or crash) cut starts again (once per generation
    // or crash), before the worker wakes below: they then arrive in that
    // turn, not as a turn of their own beside it. A Parked session had no
    // turn running.
    let parked = matches!(s.lifecycle, SessionLifecycle::Parked | SessionLifecycle::Parking);
    let resume_reply = if parked { None } else { ask_resume(&h, s, after).await };
    // Before any Park: re-register the stopped workers and wake their
    // parent (it was waiting on completions the exec ate). After a reload
    // the actor resumes the ones the reload stopped itself; a session
    // going back to Parked leaves them to the model, and so (not built for
    // them yet) do a stop restore and a crash restore.
    if !s.workers.is_empty() {
        let resume = match after {
            After::Reload { stopped: false, resume: false, .. } => {
                crate::session::WorkerResume::Off("daemon.reload_resume is off")
            }
            After::Reload { stopped: false, .. } if parked && !found_live => {
                crate::session::WorkerResume::Off("its session was parked")
            }
            After::Reload { stopped: false, .. } => crate::session::WorkerResume::Auto,
            _ => crate::session::WorkerResume::Never,
        };
        let _ = h
            .send(SessionCommand::AdoptWorkers { workers: s.workers.clone(), resume })
            .await;
    }
    // The capacity resume the record carried, last before the Park: the
    // actor arms it only if nothing above (a resumed or noted turn, a
    // worker's wake) or a client already continued the session.
    match capacity_stop_to_arm(s, found_live) {
        Some(stop) if fresh => crate::session::capacity_resume::note_given_up(&s.id, &stop),
        Some(stop) => {
            let _ = h.send(SessionCommand::ArmCapacityResume { stop }).await;
        }
        None => {}
    }
    if parked && !found_live {
        let _ = h.send(SessionCommand::Park).await;
    } else if !found_live {
        // Upstream #304, last, after the pin, keep-warm, workers and
        // capacity resume it weighs: the old image's park timer died with
        // the exec, and a session nobody reconnects to gets no detach to
        // arm one. A session a client loaded first has that client's.
        let _ = h.send(SessionCommand::ArmPark).await;
    }
    let parked = parked && !found_live;
    let how = match after {
        After::Reload { stopped: false, .. } => "reload",
        After::Reload { stopped: true, .. } => "stop restore",
        After::Crash { .. } => "crash restore",
    };
    tracing::info!(old = %s.id, new = %new_id, lifecycle = ?s.lifecycle, how, "daemon: session rehydrated after {how}");
    Rehydrated::Restored { parked, resume: resume_reply }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The session's tool activations ride the reload-state file; a file
    /// from an older image (no field) reads as none.
    #[test]
    fn reload_session_carries_tool_activations() {
        use crate::tools::activation::{CarriedActivation, CarriedProvenance};
        let carried = vec![CarriedActivation {
            tool_id: "builtin:subagent_start".into(),
            schema_digest: "cd".repeat(32),
            provenance: CarriedProvenance::BuiltinRuntime,
        }];
        let s = ReloadSession {
            id: "s".into(),
            journal_id: "j".into(),
            config: SessionConfig::default(),
            keep_warm: false,
            lifecycle: SessionLifecycle::Live,
            settings_replay: Vec::new(),
            cache_keepwarm: None,
            input_owner_kind: None,
            workers: Vec::new(),
            tool_activations: carried.clone(),
            turn_cut: None,
            cut_tail: None,
            capacity_stop: None,
            parked_at: None,
        };
        let v = serde_json::to_value(&s).unwrap();
        let back: ReloadSession = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(back.tool_activations, carried);
        // Never via the create config's wire form.
        assert!(v["config"].get("restore_tool_activations").is_none());

        let mut older = v;
        older.as_object_mut().unwrap().remove("tool_activations");
        let back: ReloadSession = serde_json::from_value(older).unwrap();
        assert!(back.tool_activations.is_empty());
    }

    /// A writer for a test subscriber that keeps everything written to it.
    #[derive(Clone, Default)]
    struct Capture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap_or_else(|p| p.into_inner()).extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
        type Writer = Capture;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// The reload-resume lines reach the daemon log: the daemon's own filter
    /// (`with_crate_directives` over a `RUST_LOG`-unset base, what
    /// `init_logging` builds) keeps an INFO line at [`LOG_TARGET`], and drops
    /// one at an unrelated target (the control).
    #[test]
    fn the_daemon_log_filter_keeps_reload_resume_lines() {
        use tracing_subscriber::filter::{EnvFilter, LevelFilter};
        let unset = EnvFilter::builder()
            .with_default_directive(LevelFilter::ERROR.into())
            .parse_lossy("");
        let out = Capture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(agent_core::logging::with_crate_directives(unset))
            .with_writer(out.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: LOG_TARGET, action = "turn_started", "reload resume: kept");
            tracing::info!(target: "reload", "reload resume: outside the crate directives");
        });
        let text = String::from_utf8(out.0.lock().unwrap_or_else(|p| p.into_inner()).clone())
            .expect("utf-8 log");
        assert!(text.contains("reload resume: kept"), "{text:?}");
        assert!(text.contains("INFO"), "{text:?}");
        assert!(!text.contains("outside the crate directives"), "{text:?}");
    }

    #[test]
    fn the_resume_text_names_the_reload_time_and_what_to_check() {
        let at = chrono::DateTime::parse_from_rfc3339("2026-10-19T05:46:12Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(
            resume_text(at),
            "resume: the daemon reloaded mid-turn (05:46 UTC); your last turn was cut, so check \
             your last tool call before trusting it, then continue."
        );
    }

    /// A session recorded by an older image (no `turn_cut`) parses as
    /// "unknown": the new image decides from the history.
    #[test]
    fn a_session_from_an_older_image_has_no_turn_cut() {
        let s: ReloadSession = serde_json::from_value(serde_json::json!({
            "id": "s1",
            "journal_id": "j1",
            "config": {},
            "keep_warm": false,
            "lifecycle": "live",
            "input_owner_kind": null
        }))
        .unwrap();
        assert_eq!(s.turn_cut, None);
        assert_eq!(s.cut_tail, None);
        let back: ReloadSession = serde_json::from_value(serde_json::to_value(ReloadSession {
            turn_cut: Some(true),
            cut_tail: Some(2),
            ..s
        })
        .unwrap())
        .unwrap();
        assert_eq!(back.turn_cut, Some(true));
        assert_eq!(back.cut_tail, Some(2));
    }

    /// A session's armed capacity resume rides the reload record and the
    /// reload state; one recorded by an older image has none, and a field a
    /// newer image adds to it doesn't break the parse.
    #[test]
    fn a_capacity_stop_rides_the_reload_record_and_an_older_one_has_none() {
        let old: ReloadSession = serde_json::from_value(serde_json::json!({
            "id": "s1", "journal_id": "j1", "config": {}, "keep_warm": false,
            "lifecycle": "live", "input_owner_kind": null
        }))
        .unwrap();
        assert_eq!(old.capacity_stop, None);
        let stop = crate::session::CapacityStop {
            resume_at: chrono::Utc::now() + chrono::Duration::hours(30),
            stopped_at: chrono::Utc::now(),
            kind: "all_cooling_down".into(),
            history_len: 4,
            last_digest: "ab".repeat(32),
            attempt: 1,
        };
        let mut v = serde_json::to_value(ReloadSession { capacity_stop: Some(stop.clone()), ..old.clone() }).unwrap();
        v["capacity_stop"]["later_field"] = serde_json::json!(7);
        let back: ReloadSession = serde_json::from_value(v).unwrap();
        assert_eq!(back.capacity_stop, Some(stop.clone()));
        assert!(serde_json::to_value(&old).unwrap().get("capacity_stop").is_none(), "none is not written");

        let rec: crate::session::SessionReloadRecord = serde_json::from_value(serde_json::json!({
            "config": {}, "keep_warm": false, "lifecycle": "live", "model": "m", "thinking_level": "low"
        }))
        .unwrap();
        assert_eq!(rec.capacity_stop, None);
        let rec = crate::session::SessionReloadRecord { capacity_stop: Some(stop.clone()), ..rec };
        let back: crate::session::SessionReloadRecord =
            serde_json::from_value(serde_json::to_value(&rec).unwrap()).unwrap();
        assert_eq!(back.capacity_stop, Some(stop));
    }

    /// The JSON time at `key`, if any.
    fn time_at(v: &serde_json::Value, key: &str) -> Option<chrono::DateTime<chrono::Utc>> {
        v.get(key).map(|t| serde_json::from_value(t.clone()).expect("a time"))
    }

    fn park_time() -> chrono::DateTime<chrono::Utc> {
        "2026-10-27T18:00:00.123456789Z".parse().unwrap()
    }

    /// A parked session's park time rides the checkpoint's record
    /// (`parked_since`) and the reload state (`parked_at`), so the new image
    /// continues its eviction clock; one an older image recorded has none,
    /// and none is not written.
    #[test]
    fn a_park_time_rides_the_reload_record_and_an_older_one_has_none() {
        let t = park_time();
        let old = serde_json::json!({
            "id": "s1", "journal_id": "j1", "config": {}, "keep_warm": false,
            "lifecycle": "parked", "input_owner_kind": null
        });
        let back = serde_json::to_value(serde_json::from_value::<ReloadSession>(old.clone()).unwrap()).unwrap();
        assert_eq!(back.get("parked_at"), None, "an older image's session: none, and none written");
        let mut v = old;
        v["parked_at"] = serde_json::json!(t);
        let back = serde_json::to_value(serde_json::from_value::<ReloadSession>(v).unwrap()).unwrap();
        assert_eq!(time_at(&back, "parked_at"), Some(t), "the reload state keeps it");

        let old = serde_json::json!({
            "config": {}, "keep_warm": false, "lifecycle": "parked", "model": "m", "thinking_level": "low"
        });
        let back = serde_json::to_value(serde_json::from_value::<SessionReloadRecord>(old.clone()).unwrap()).unwrap();
        assert_eq!(back.get("parked_since"), None, "an older image's record: none, and none written");
        let mut v = old;
        v["parked_since"] = serde_json::json!(t);
        let back = serde_json::to_value(serde_json::from_value::<SessionReloadRecord>(v).unwrap()).unwrap();
        assert_eq!(time_at(&back, "parked_since"), Some(t), "the checkpoint's record keeps it");
    }

    /// `record`: the park time is the checkpoint's; with no checkpoint
    /// reply, the one the actor last published (none if it never did).
    #[tokio::test]
    async fn the_recorded_park_time_is_the_checkpoints_else_the_last_published() {
        let t = park_time();
        let (h, _task) = SessionHandle::echo_for_test(crate::session::SessionId::from("s-parked"));
        let rec: SessionReloadRecord = serde_json::from_value(serde_json::json!({
            "config": {}, "keep_warm": false, "lifecycle": "parked", "model": "m", "thinking_level": "low",
            "parked_since": t
        }))
        .unwrap();
        let s = serde_json::to_value(record(&h, Some(rec.clone()))).unwrap();
        assert_eq!(time_at(&s, "parked_at"), Some(t), "the checkpoint's");
        let s = serde_json::to_value(record(&h, None)).unwrap();
        assert_eq!(s.get("parked_at"), None, "nothing published: none");
        h.live_slot().store(Some(Arc::new(crate::session::live::LiveEntry {
            record: rec,
            turn_in_flight: false,
            running_workers: Vec::new(),
            resumed_at: None,
            parked_since: Some(t),
            gen: 1,
        })));
        let s = serde_json::to_value(record(&h, None)).unwrap();
        assert_eq!(time_at(&s, "parked_at"), Some(t), "the last published entry's");
    }

    /// D1: the record names the input owner's declared kind as presence last
    /// published it (the crash record's `daemon.live.json` too), and null
    /// with no owner. It used to be None whatever the owner.
    #[tokio::test]
    async fn the_record_names_the_input_owners_declared_kind() {
        let (h, _task) = SessionHandle::echo_for_test(crate::session::SessionId::from("s-owner"));
        let s = serde_json::to_value(record(&h, None)).unwrap();
        assert_eq!(s["input_owner_kind"], serde_json::Value::Null, "no owner: null");
        h.set_presence_for_test(crate::session::handle::Presence {
            clients: 2,
            input_owner: Some(crate::session::ClientId(11)),
            input_owner_kind: Some(crate::session::ClientKind::Chat),
            awaiting_input: 0,
            ..Default::default()
        });
        let s = serde_json::to_value(record(&h, None)).unwrap();
        assert_eq!(s["input_owner_kind"], "chat", "the owner's declared kind");
        let back: ReloadSession = serde_json::from_value(s).unwrap();
        assert_eq!(back.input_owner_kind, Some(crate::session::ClientKind::Chat), "reads back");
    }

    /// A1, the reload record: a recorded capacity resume that does not parse
    /// drops only itself, in the reload state the new image reads and in a
    /// checkpoint's record. Every session reads with its other fields, the
    /// valid resume beside them stays, and each dropped one logs exactly one
    /// WARN of a fixed class, through the daemon's log filter. A state file
    /// that does not parse at all names a fixed class and where, never
    /// serde's message (it quotes the stored value).
    #[test]
    fn a_malformed_capacity_resume_drops_only_itself_from_the_reload_record() {
        use crate::session::capacity_resume::fixtures::{all_levels_log, malformed_stops, SENTINEL};
        use tracing_subscriber::filter::{EnvFilter, LevelFilter};
        let session = |id: &str| -> ReloadSession {
            serde_json::from_value(serde_json::json!({
                "id": id, "journal_id": id, "config": {}, "keep_warm": false,
                "lifecycle": "live", "input_owner_kind": null, "turn_cut": true
            }))
            .unwrap()
        };
        let good = crate::session::CapacityStop {
            resume_at: chrono::Utc::now() + chrono::Duration::hours(1),
            stopped_at: chrono::Utc::now(),
            kind: "all_cooling_down".into(),
            history_len: 4,
            last_digest: "ab".repeat(32),
            attempt: 0,
        };
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join("daemon.reload.json");
        let kept = ReloadSession { capacity_stop: Some(good.clone()), ..session("s-good") };
        let mut v = serde_json::to_value(ReloadState { sessions: vec![kept], ..state(None) }).unwrap();
        let malformed = malformed_stops();
        for (id, stop) in &malformed {
            let mut s = serde_json::to_value(session(id)).unwrap();
            s["capacity_stop"] = stop.clone();
            v["sessions"].as_array_mut().unwrap().push(s);
        }
        std::fs::write(&file, serde_json::to_vec(&v).unwrap()).unwrap();

        let unset = EnvFilter::builder()
            .with_default_directive(LevelFilter::ERROR.into())
            .parse_lossy("");
        let out = Capture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(agent_core::logging::with_crate_directives(unset))
            .with_writer(out.clone())
            .with_ansi(false)
            .finish();
        let (back, records) = tracing::subscriber::with_default(subscriber, || {
            let back = read_reload_state(&file);
            let records: Vec<Result<crate::session::SessionReloadRecord, String>> = malformed
                .iter()
                .map(|(_, stop)| {
                    let mut rec = serde_json::json!({
                        "config": {}, "keep_warm": false, "lifecycle": "live", "model": "m",
                        "thinking_level": "low", "turn_cut": true
                    });
                    rec["capacity_stop"] = stop.clone();
                    serde_json::from_value(rec).map_err(|e| e.to_string())
                })
                .collect();
            (back, records)
        });
        let back = back.expect("the state reads");
        let ids: Vec<_> = back.sessions.iter().map(|s| s.id.as_str()).collect();
        let mut want = vec!["s-good"];
        want.extend(malformed.iter().map(|(id, _)| *id));
        assert_eq!(ids, want, "every session reads");
        assert_eq!(back.sessions[0].capacity_stop, Some(good), "the valid resume stays");
        for s in &back.sessions[1..] {
            assert_eq!(s.capacity_stop, None, "{}: dropped", s.id);
            assert_eq!(s.turn_cut, Some(true), "{}: the rest reads", s.id);
        }
        for rec in records {
            let rec = rec.expect("a checkpoint's record reads");
            assert_eq!(rec.capacity_stop, None, "dropped");
            assert!(rec.turn_cut, "the rest reads");
        }
        let text = String::from_utf8(out.0.lock().unwrap_or_else(|p| p.into_inner()).clone()).unwrap();
        let dropped = "capacity resume: a recorded resume does not parse; dropped";
        assert_eq!(text.matches(dropped).count(), 2 * malformed.len(), "one WARN each: {text:?}");
        assert_eq!(text.matches("WARN").count(), 2 * malformed.len(), "{text:?}");
        assert_eq!(text.matches("class=\"malformed\"").count(), 2 * malformed.len(), "{text:?}");
        assert!(text.contains("agent_engine::session::capacity_resume"), "{text:?}");
        assert!(!text.contains(SENTINEL), "no stored value in the log: {text:?}");
        let (again, all) = all_levels_log(|| {
            let back = read_reload_state(&file).map(|s| s.sessions.len());
            for (_, stop) in &malformed {
                let mut rec = serde_json::json!({
                    "config": {}, "keep_warm": false, "lifecycle": "live", "model": "m",
                    "thinking_level": "low", "turn_cut": true
                });
                rec["capacity_stop"] = stop.clone();
                let rec = serde_json::from_value::<crate::session::SessionReloadRecord>(rec);
                assert_eq!(rec.expect("a checkpoint's record reads").capacity_stop, None);
            }
            back
        });
        assert_eq!(again, Ok(want.len()), "the state reads");
        assert_eq!(all.matches("class=\"malformed\"").count(), 2 * malformed.len(), "{all:?}");
        assert!(!all.contains(SENTINEL), "no stored value at any level: {all:?}");

        v["generation"] = serde_json::json!(SENTINEL);
        std::fs::write(&file, serde_json::to_vec(&v).unwrap()).unwrap();
        let why = read_reload_state(&file).expect_err("unusable");
        assert!(why.contains("does not parse (data, line 1, column "), "{why}");
        assert!(!why.contains(SENTINEL), "{why}");
    }

    /// AB1, the reload record: a recorded resume past the JSON reader's
    /// limits (a number out of range, nesting past 128 levels), as raw text
    /// in the state file, drops only itself through the new image's own read
    /// from bytes (`read_reload_state`): its raw text is taken apart before
    /// any typed decoding. Every session reads, with its other fields; a
    /// valid resume beside them stays; each dropped one logs one WARN of a
    /// fixed class; the stored value reaches no line at any level. A
    /// checkpoint's record arrives in process as a `Value` (no number out
    /// of range fits one); one nested past 128 levels drops only itself too.
    #[test]
    fn a_capacity_resume_past_the_readers_limits_drops_only_itself_from_the_reload_record() {
        use crate::session::capacity_resume::fixtures::{
            all_levels_log, daemon_log, raw_malformed_stops, with_raw_stops, SENTINEL,
        };
        let session = |id: &str| -> ReloadSession {
            serde_json::from_value(serde_json::json!({
                "id": id, "journal_id": id, "config": {}, "keep_warm": false,
                "lifecycle": "live", "input_owner_kind": null, "turn_cut": true
            }))
            .unwrap()
        };
        let good = crate::session::CapacityStop {
            resume_at: chrono::Utc::now() + chrono::Duration::hours(1),
            stopped_at: chrono::Utc::now(),
            kind: "all_cooling_down".into(),
            history_len: 4,
            last_digest: "ab".repeat(32),
            attempt: 0,
        };
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join("daemon.reload.json");
        let raw = raw_malformed_stops();
        let mut sessions = vec![ReloadSession { capacity_stop: Some(good.clone()), ..session("s-good") }];
        sessions.extend(raw.iter().map(|(id, _)| session(id)));
        let doc = serde_json::to_value(ReloadState { sessions, ..state(None) }).unwrap();
        std::fs::write(&file, with_raw_stops(doc, &raw)).unwrap();

        let (back, text) = daemon_log(|| read_reload_state(&file));
        let back = back.expect("the state reads");
        let ids: Vec<_> = back.sessions.iter().map(|s| s.id.as_str()).collect();
        let mut want = vec!["s-good"];
        want.extend(raw.iter().map(|(id, _)| *id));
        assert_eq!(ids, want, "every session reads");
        assert_eq!(back.sessions[0].capacity_stop, Some(good), "the valid resume stays");
        for s in &back.sessions[1..] {
            assert_eq!(s.capacity_stop, None, "{}: dropped", s.id);
            assert_eq!(s.turn_cut, Some(true), "{}: the rest reads", s.id);
        }
        let dropped = "capacity resume: a recorded resume does not parse; dropped";
        assert_eq!(text.matches(dropped).count(), raw.len(), "one WARN each: {text:?}");
        assert_eq!(text.matches("WARN").count(), raw.len(), "{text:?}");
        assert_eq!(text.matches("class=\"malformed\"").count(), raw.len(), "{text:?}");
        assert!(!text.contains(SENTINEL), "no stored value in the log: {text:?}");

        let mut deep = serde_json::json!(SENTINEL);
        for _ in 0..200 {
            deep = serde_json::json!([deep]);
        }
        let mut rec = serde_json::json!({
            "config": {}, "keep_warm": false, "lifecycle": "live", "model": "m",
            "thinking_level": "low", "turn_cut": true
        });
        rec["capacity_stop"] = deep;
        let ((again, checkpoint), all) = all_levels_log(|| {
            let again = read_reload_state(&file).map(|s| s.sessions.len());
            (again, serde_json::from_value::<crate::session::SessionReloadRecord>(rec))
        });
        assert_eq!(again, Ok(want.len()), "the state reads");
        let checkpoint = checkpoint.expect("a checkpoint's record reads");
        assert_eq!(checkpoint.capacity_stop, None, "dropped");
        assert!(checkpoint.turn_cut, "the rest reads");
        assert_eq!(all.matches("class=\"malformed\"").count(), raw.len() + 1, "{all:?}");
        assert!(!all.contains(SENTINEL), "no stored value at any level: {all:?}");
    }

    /// A rehydrated session's actor is handed the resume its record
    /// carried, but never when a client loaded the session first (one INFO
    /// line, through the daemon's log filter), and nothing without one.
    #[test]
    fn a_rehydrate_arms_the_recorded_resume_unless_a_client_loaded_the_session() {
        use tracing_subscriber::filter::{EnvFilter, LevelFilter};
        let none: ReloadSession = serde_json::from_value(serde_json::json!({
            "id": "s-loaded", "journal_id": "s-loaded", "config": {}, "keep_warm": false,
            "lifecycle": "live", "input_owner_kind": null
        }))
        .unwrap();
        let stop = crate::session::CapacityStop {
            resume_at: chrono::Utc::now() + chrono::Duration::hours(1),
            stopped_at: chrono::Utc::now(),
            kind: "all_cooling_down".into(),
            history_len: 4,
            last_digest: "ab".repeat(32),
            attempt: 0,
        };
        let armed = ReloadSession { capacity_stop: Some(stop.clone()), ..none.clone() };
        let unset = EnvFilter::builder()
            .with_default_directive(LevelFilter::ERROR.into())
            .parse_lossy("");
        let out = Capture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(agent_core::logging::with_crate_directives(unset))
            .with_writer(out.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            assert_eq!(capacity_stop_to_arm(&armed, false), Some(stop.clone()));
            assert_eq!(capacity_stop_to_arm(&none, false), None);
            assert_eq!(capacity_stop_to_arm(&none, true), None);
            assert_eq!(capacity_stop_to_arm(&armed, true), None, "a client loaded it first");
        });
        let text = String::from_utf8(out.0.lock().unwrap_or_else(|p| p.into_inner()).clone()).unwrap();
        assert_eq!(text.matches("client_loaded").count(), 1, "{text:?}");
        assert!(text.contains("session=s-loaded") && text.contains("INFO"), "{text:?}");
        assert!(text.contains("agent_engine::session::capacity_resume"), "{text:?}");
    }

    /// Reload-state written by an older image (no workers, no counter)
    /// still parses: the new fields default.
    #[test]
    fn reload_state_from_an_older_image_parses() {
        let rs: ReloadState = serde_json::from_value(serde_json::json!({
            "generation": 9,
            "written_at": "2026-10-14T18:17:03Z",
            "sessions": [],
            "expected_clients": 1
        }))
        .unwrap();
        assert_eq!(rs.next_subagent_id, 0);
        assert_eq!(rs.subagent_id_floor(), 0);
    }

    /// The new image's first handle is past the old counter AND past every
    /// carried id (an older writer that only carried workers still works).
    #[test]
    fn subagent_id_floor_covers_the_counter_and_every_carried_id() {
        let worker = |id: &str| crate::runtime::subagent::CarriedWorker {
            id: id.into(),
            subagent_id: 0,
            agent: String::new(),
            status: "cancelled".into(),
            archive_path: None,
            stopped_by_reload: true,
            wake_lost: false,
            wake: true,
            elapsed_secs: 0,
            preview: String::new(),
            ..Default::default()
        };
        let session = |workers| ReloadSession {
            id: "s".into(),
            journal_id: "j".into(),
            config: SessionConfig::default(),
            keep_warm: false,
            lifecycle: SessionLifecycle::Live,
            settings_replay: Vec::new(),
            cache_keepwarm: None,
            input_owner_kind: None,
            workers,
            tool_activations: Vec::new(),
            turn_cut: None,
            cut_tail: None,
            capacity_stop: None,
            parked_at: None,
        };
        let mut rs = ReloadState {
            generation: 10,
            written_at: chrono::Utc::now(),
            sessions: vec![session(vec![worker("sa_288"), worker("sa_286")]), session(vec![])],
            expected_clients: 0,
            next_subagent_id: 0,
            base_umask: None,
            children: Vec::new(),
        };
        assert_eq!(rs.subagent_id_floor(), 289);
        rs.next_subagent_id = 300;
        assert_eq!(rs.subagent_id_floor(), 300);
        // Round trip keeps both.
        let back: ReloadState = serde_json::from_slice(&serde_json::to_vec(&rs).unwrap()).unwrap();
        assert_eq!(back.subagent_id_floor(), 300);
        assert_eq!(back.sessions[0].workers.len(), 2);
    }

    fn pv(b: &str, p: u32) -> PrintVersion {
        PrintVersion { binary_version: b.into(), protocol_version: p }
    }

    fn state(base_umask: Option<u32>) -> ReloadState {
        ReloadState { generation: 2, written_at: chrono::Utc::now(), sessions: vec![], expected_clients: 1, next_subagent_id: 0, base_umask, children: Vec::new() }
    }

    /// The hand-off variables of a test: `state` as a path, `fd` as given.
    #[cfg(unix)]
    fn hand_off_vars(state: Option<&Path>, fd: Option<&str>) -> HandOffVars {
        HandOffVars {
            state: state.map(|p| p.as_os_str().to_owned()),
            lock_fd: fd.map(str::to_string),
            id_floor: Some("20".into()),
        }
    }

    /// The fd flags (`F_GETFD`) of `fd`, `None` if it is not open.
    #[cfg(unix)]
    fn hand_off_fd_flags(fd: i32) -> Option<i32> {
        // SAFETY: F_GETFD only reads the fd's flags.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        (flags >= 0).then_some(flags)
    }

    /// A number no fd can have (above any RLIMIT_NOFILE).
    #[cfg(unix)]
    const HAND_OFF_CLOSED_FD: &str = "2147483647";

    #[cfg(unix)]
    #[test]
    fn hand_off_absent_without_any_variable() {
        let d = tempfile::tempdir().unwrap();
        assert!(matches!(classify_hand_off(&HandOffVars::default(), &d.path().join("daemon.lock")), EnvHandOff::Absent));
    }

    #[cfg(unix)]
    #[test]
    fn hand_off_with_a_missing_state_file_and_a_closed_fd_is_stale() {
        let d = tempfile::tempdir().unwrap();
        let lock = d.path().join("daemon.lock");
        std::fs::write(&lock, "").unwrap();
        let vars = hand_off_vars(Some(&d.path().join("daemon.reload.json")), Some(HAND_OFF_CLOSED_FD));
        match classify_hand_off(&vars, &lock) {
            EnvHandOff::Stale { why } => {
                assert!(why.contains("daemon.reload.json does not exist"), "{why}");
                assert!(why.contains(&format!("fd {HAND_OFF_CLOSED_FD} is not open")), "{why}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn hand_off_with_a_parsed_state_file_and_a_closed_fd_is_stale() {
        let d = tempfile::tempdir().unwrap();
        let lock = d.path().join("daemon.lock");
        std::fs::write(&lock, "").unwrap();
        let file = d.path().join("daemon.reload.json");
        std::fs::write(&file, serde_json::to_string(&state(None)).unwrap()).unwrap();
        match classify_hand_off(&hand_off_vars(Some(&file), Some(HAND_OFF_CLOSED_FD)), &lock) {
            EnvHandOff::Stale { why } => assert_eq!(why, format!("fd {HAND_OFF_CLOSED_FD} is not open")),
            other => panic!("{other:?}"),
        }
        assert!(file.exists(), "the stale state file is left on disk");
    }

    #[cfg(unix)]
    #[test]
    fn hand_off_with_an_fd_open_on_another_file_is_stale_and_the_fd_is_untouched() {
        use std::os::unix::io::AsRawFd;
        let d = tempfile::tempdir().unwrap();
        let lock = d.path().join("daemon.lock");
        std::fs::write(&lock, "").unwrap();
        let file = d.path().join("daemon.reload.json");
        std::fs::write(&file, serde_json::to_string(&state(None)).unwrap()).unwrap();
        let other = std::fs::File::create(d.path().join("other")).unwrap();
        let fd = other.as_raw_fd();
        let before = hand_off_fd_flags(fd).expect("open");
        match classify_hand_off(&hand_off_vars(Some(&file), Some(&fd.to_string())), &lock) {
            EnvHandOff::Stale { why } => assert_eq!(why, format!("fd {fd} is not {}", lock.display())),
            other => panic!("{other:?}"),
        }
        assert_eq!(hand_off_fd_flags(fd), Some(before), "the fd is still open with the same flags");
    }

    #[cfg(unix)]
    #[test]
    fn hand_off_with_no_fd_number_or_no_state_path_is_stale() {
        let d = tempfile::tempdir().unwrap();
        let lock = d.path().join("daemon.lock");
        let held = std::fs::File::create(&lock).unwrap();
        let file = d.path().join("daemon.reload.json");
        std::fs::write(&file, serde_json::to_string(&state(None)).unwrap()).unwrap();
        for fd in ["x", "-1", ""] {
            match classify_hand_off(&hand_off_vars(Some(&file), Some(fd)), &lock) {
                EnvHandOff::Stale { why } => assert!(why.contains("is not an fd number"), "{fd:?}: {why}"),
                other => panic!("{fd:?}: {other:?}"),
            }
        }
        match classify_hand_off(&hand_off_vars(Some(&file), None), &lock) {
            EnvHandOff::Stale { why } => assert_eq!(why, format!("{LOCK_FD_ENV} is not set")),
            other => panic!("{other:?}"),
        }
        // The lock's own fd but no state path: never a reload's hand-off.
        use std::os::unix::io::AsRawFd;
        match classify_hand_off(&hand_off_vars(None, Some(&held.as_raw_fd().to_string())), &lock) {
            EnvHandOff::Stale { why } => assert_eq!(why, format!("{RELOAD_STATE_ENV} is not set")),
            other => panic!("{other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn hand_off_with_the_lock_fd_and_a_parsed_state_is_real_and_the_fd_is_not_adopted_yet() {
        use std::os::unix::io::AsRawFd;
        let d = tempfile::tempdir().unwrap();
        let lock = d.path().join("daemon.lock");
        let held = std::fs::File::create(&lock).unwrap();
        let fd = held.as_raw_fd();
        let before = hand_off_fd_flags(fd).expect("open");
        let file = d.path().join("daemon.reload.json");
        std::fs::write(&file, serde_json::to_string(&state(Some(0o002))).unwrap()).unwrap();
        match classify_hand_off(&hand_off_vars(Some(&file), Some(&fd.to_string())), &lock) {
            EnvHandOff::Real { fd: got, state, path, id_floor } => {
                assert_eq!(got, fd);
                assert_eq!((state.generation, state.base_umask), (2, Some(0o002)));
                assert_eq!(path, file);
                assert_eq!(id_floor, Some(20));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(hand_off_fd_flags(fd), Some(before), "classifying never adopts the fd");
    }

    #[cfg(unix)]
    #[test]
    fn hand_off_with_the_lock_fd_and_a_missing_or_bad_state_is_refused() {
        use std::os::unix::io::AsRawFd;
        let d = tempfile::tempdir().unwrap();
        let lock = d.path().join("daemon.lock");
        let held = std::fs::File::create(&lock).unwrap();
        let fd = held.as_raw_fd().to_string();
        let file = d.path().join("daemon.reload.json");
        match classify_hand_off(&hand_off_vars(Some(&file), Some(&fd)), &lock) {
            EnvHandOff::Broken { why } => assert!(why.ends_with("daemon.reload.json does not exist"), "{why}"),
            other => panic!("{other:?}"),
        }
        std::fs::write(&file, "not json").unwrap();
        match classify_hand_off(&hand_off_vars(Some(&file), Some(&fd)), &lock) {
            EnvHandOff::Broken { why } => assert!(why.contains("does not parse"), "{why}"),
            other => panic!("{other:?}"),
        }
    }

    /// Clear `FD_CLOEXEC` on `fd`, as the old image does for the exec.
    #[cfg(unix)]
    fn hand_off_clear_cloexec(fd: i32) {
        let flags = hand_off_fd_flags(fd).expect("open");
        // SAFETY: F_SETFD on a test-owned fd.
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) }, 0);
    }

    #[cfg(unix)]
    #[test]
    fn hand_off_seal_sets_cloexec_on_the_real_lock_fd_only_and_keeps_its_flock() {
        use fs4::fs_std::FileExt;
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::io::AsRawFd;
        let d = tempfile::tempdir().unwrap();
        let lock = d.path().join("daemon.lock");
        let held = std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(&lock).unwrap();
        assert!(FileExt::try_lock_exclusive(&held).unwrap());
        let fd = held.as_raw_fd();
        hand_off_clear_cloexec(fd);
        let file = d.path().join("daemon.reload.json");
        std::fs::write(&file, serde_json::to_string(&state(None)).unwrap()).unwrap();
        assert!(seal_hand_off_lock_with(&hand_off_vars(Some(&file), Some(&fd.to_string())), &lock));
        let flags = hand_off_fd_flags(fd).expect("still open");
        assert_ne!(flags & libc::FD_CLOEXEC, 0, "the real lock fd is close-on-exec again");
        // The same fd and open file description: same inode, flock still ours.
        let m = held.metadata().unwrap();
        let l = std::fs::metadata(&lock).unwrap();
        assert_eq!((m.dev(), m.ino()), (l.dev(), l.ino()));
        let other = std::fs::File::open(&lock).unwrap();
        assert!(!FileExt::try_lock_exclusive(&other).unwrap(), "the flock is still held through the sealed fd");
        // A broken hand-off (the lock fd, the state file gone), which the
        // start then refuses: the lock fd is sealed all the same.
        hand_off_clear_cloexec(fd);
        let gone = d.path().join("gone.json");
        assert!(matches!(classify_hand_off(&hand_off_vars(Some(&gone), Some(&fd.to_string())), &lock), EnvHandOff::Broken { .. }));
        assert!(seal_hand_off_lock_with(&hand_off_vars(Some(&gone), Some(&fd.to_string())), &lock));
        assert_ne!(hand_off_fd_flags(fd).expect("still open") & libc::FD_CLOEXEC, 0, "a broken hand-off's lock fd is sealed too");
    }

    #[cfg(unix)]
    #[test]
    fn hand_off_seal_leaves_a_stale_or_absent_hand_off_alone() {
        use std::os::unix::io::AsRawFd;
        let d = tempfile::tempdir().unwrap();
        let lock = d.path().join("daemon.lock");
        std::fs::write(&lock, "").unwrap();
        let file = d.path().join("daemon.reload.json");
        std::fs::write(&file, serde_json::to_string(&state(None)).unwrap()).unwrap();
        let other = std::fs::File::create(d.path().join("other")).unwrap();
        let fd = other.as_raw_fd();
        hand_off_clear_cloexec(fd);
        let before = hand_off_fd_flags(fd).unwrap();
        assert_eq!(before & libc::FD_CLOEXEC, 0);
        assert!(!seal_hand_off_lock_with(&hand_off_vars(Some(&file), Some(&fd.to_string())), &lock));
        assert_eq!(hand_off_fd_flags(fd), Some(before), "a stale fd keeps its flags");
        assert!(!seal_hand_off_lock_with(&HandOffVars::default(), &lock));
        assert_eq!(hand_off_fd_flags(fd), Some(before));
        assert!(!seal_hand_off_lock_with(&hand_off_vars(Some(&file), Some(HAND_OFF_CLOSED_FD)), &lock));
        assert!(!seal_hand_off_lock_with(&hand_off_vars(Some(&file), Some("not-an-fd")), &lock));
    }

    #[test]
    fn the_started_with_mask_rides_the_reload_state_and_older_shapes_ignore_it() {
        let body = serde_json::to_string(&state(Some(0o002))).unwrap();
        assert!(body.contains("\"base_umask\":2"), "{body}");
        // The state shape of a binary before daemon.umask (no unknown-field
        // refusal): it reads a new state as before.
        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct Before {
            generation: u64,
            written_at: chrono::DateTime<chrono::Utc>,
            sessions: Vec<ReloadSession>,
            expected_clients: usize,
        }
        let old: Before = serde_json::from_str(&body).unwrap();
        assert_eq!((old.generation, old.expected_clients), (2, 1));
        // An older image's state: no carried mask.
        let older = serde_json::to_string(&state(None)).unwrap();
        assert!(!older.contains("base_umask"), "{older}");
        assert_eq!(serde_json::from_str::<ReloadState>(&older).unwrap().base_umask, None);
        assert_eq!(serde_json::from_str::<ReloadState>(&body).unwrap().base_umask, Some(0o002));
    }

    #[cfg(unix)]
    #[test]
    fn the_carried_mask_is_read_only_from_a_regular_state_file() {
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join("daemon.reload.json");
        std::fs::write(&file, serde_json::to_string(&state(Some(0o002))).unwrap()).unwrap();
        assert_eq!(carried_base_umask_in(&file), Some(0o002));
        std::fs::write(&file, serde_json::to_string(&state(None)).unwrap()).unwrap();
        assert_eq!(carried_base_umask_in(&file), None, "an older image's state");
        std::fs::write(&file, "not json").unwrap();
        assert_eq!(carried_base_umask_in(&file), None);
        assert_eq!(carried_base_umask_in(&d.path().join("missing")), None);
        assert_eq!(carried_base_umask_in(d.path()), None, "a directory");
        // A FIFO is refused before it is opened (an open would block).
        let fifo = d.path().join("fifo");
        let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: mkfifo on a NUL-terminated path in our temp dir.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        assert_eq!(carried_base_umask_in(&fifo), None);
        // Past the stat (a FIFO swapped in after it): the open doesn't block
        // and the open file's type refuses it. On a thread with a deadline, so
        // a blocking open fails the test instead of hanging it.
        let (tx, rx) = std::sync::mpsc::channel();
        let swapped = fifo.clone();
        std::thread::spawn(move || {
            let _ = tx.send(open_regular_file(&swapped).is_some());
        });
        assert_eq!(
            rx.recv_timeout(std::time::Duration::from_secs(5)),
            Ok(false),
            "a FIFO is refused after the open, without blocking"
        );
        assert!(open_regular_file(&file).is_some(), "a regular file opens");
        // Over the cap: not read.
        let big = d.path().join("big.json");
        let f = std::fs::File::create(&big).unwrap();
        f.set_len(CARRIED_BASE_MAX_BYTES + 1).unwrap();
        assert_eq!(carried_base_umask_in(&big), None);
    }

    #[test]
    fn gate_is_directional_newer_or_equal() {
        let cur = pv("0.9.0", 2);
        assert!(version_gate(&cur, &pv("0.9.0", 2)).is_ok(), "equal allowed");
        assert!(version_gate(&cur, &pv("0.9.1", 2)).is_ok());
        assert!(version_gate(&cur, &pv("1.0.0-rc1", 3)).is_ok());
        assert!(version_gate(&cur, &pv("0.8.9", 2)).is_err(), "older refused");
        assert!(version_gate(&cur, &pv("0.9.0", 1)).is_err(), "older protocol refused");
        assert!(version_gate(&cur, &pv("garbage", 2)).is_err());
        // Two-digit minor (daemon dev builds report 0.9.1 → 0.10.0): compared
        // numerically, not as strings — "10" < "9" lexically would refuse it.
        assert!(
            version_gate(&pv("0.9.1", 3), &pv("0.10.0", 3)).is_ok(),
            "0.10.0 is newer than 0.9.1"
        );
        assert!(
            version_gate(&pv("0.10.0", 3), &pv("0.9.1", 3)).is_err(),
            "0.9.1 is older than 0.10.0"
        );
    }

    #[cfg(unix)]
    #[test]
    fn validate_exe_refuses_missing_dirs_non_exec_and_writable() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        assert!(validate_exe(&d.path().join("nope")).unwrap_err().contains("nope"));
        assert!(validate_exe(d.path()).unwrap_err().contains("not a regular file"));
        let f = d.path().join("bin");
        std::fs::write(&f, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(validate_exe(&f).unwrap_err().contains("not executable"));
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(validate_exe(&f).unwrap_err().contains("writable"));
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(validate_exe(&f).unwrap(), f.canonicalize().unwrap());
        // A symlink resolves to its target.
        let l = d.path().join("link");
        std::os::unix::fs::symlink(&f, &l).unwrap();
        assert_eq!(validate_exe(&l).unwrap(), f.canonicalize().unwrap());
    }

    /// A restored session never waits on `on_session_start` at create; the
    /// rest of its saved config is kept. One restored Parked defers the hook
    /// to its unpark, and that flag never goes on the wire.
    #[test]
    fn a_restore_does_not_wait_on_the_session_start_hook() {
        let saved = SessionConfig {
            model_override: Some("claude-sonnet-4-5".into()),
            keep_warm: true,
            await_extensions: true,
            ..Default::default()
        };
        let restored = restore_config(&saved, SessionLifecycle::Live);
        assert!(!restored.await_extensions);
        assert!(!restored.restore_parked);
        let as_saved = SessionConfig {
            await_extensions: true,
            ..restored
        };
        assert_eq!(
            serde_json::to_value(&as_saved).unwrap(),
            serde_json::to_value(&saved).unwrap()
        );
        for parked in [SessionLifecycle::Parked, SessionLifecycle::Parking] {
            let restored = restore_config(&saved, parked);
            assert!(restored.restore_parked, "{parked:?}");
            assert!(!restored.await_extensions);
            let wire = serde_json::to_value(&restored).unwrap();
            assert!(wire.get("restore_parked").is_none(), "never on the wire");
        }
    }

    #[test]
    fn reload_state_path_follows_profile() {
        let d = tempfile::tempdir().unwrap();
        let p = registry::daemon_paths_in(d.path(), Some("work"));
        assert!(reload_state_path(&p).ends_with("daemon-work.reload.json"), "{:?}", reload_state_path(&p));
        let p = registry::daemon_paths_in(d.path(), None);
        assert!(reload_state_path(&p).ends_with("daemon.reload.json"));
    }

    /// Task #444: a reload whose checkpoint did not confirm (`rec == None`)
    /// still carries the explicit `/keepwarm` choice, read from the handle,
    /// instead of resurrecting the config default.
    #[test]
    fn keepwarm_override_survives_an_unconfirmed_checkpoint() {
        use crate::core::config::CacheKeepWarm;
        use crate::session::handle::echo::{meta_for, view_for};
        let id = crate::session::SessionId("kw-reload".into());
        let (handle, ep) = SessionHandle::new(meta_for(&id), view_for());
        assert_eq!(record(&handle, None).cache_keepwarm, None, "no override yet");
        // What the actor writes on `/keepwarm off`.
        *ep.cache_keepwarm.lock().unwrap() = Some(CacheKeepWarm::Off);
        assert_eq!(record(&handle, None).cache_keepwarm.as_deref(), Some("off"));
        *ep.cache_keepwarm.lock().unwrap() = Some(CacheKeepWarm::Hours(6));
        assert_eq!(record(&handle, None).cache_keepwarm.as_deref(), Some("6h"));
    }

    /// Likewise the pin: an unconfirmed checkpoint must not unpin a
    /// session (it would park, and stop holding the daemon's idle exit).
    #[test]
    fn the_pin_survives_an_unconfirmed_checkpoint() {
        use crate::session::handle::echo::{meta_for, view_for};
        let id = crate::session::SessionId("pin-reload".into());
        let (handle, ep) = SessionHandle::new(meta_for(&id), view_for());
        assert!(!record(&handle, None).keep_warm);
        ep.keep_warm.store(true, std::sync::atomic::Ordering::Release);
        let r = record(&handle, None);
        assert!(r.keep_warm && r.config.keep_warm);
    }

    /// A `memory_mode` this image does not know (written by a newer one)
    /// reads as `Off` — the consent-safe reading — instead of failing the
    /// session's config and with it the whole reload state, so no session
    /// would be rehydrated. A missing or null value stays no choice.
    #[test]
    fn an_unknown_memory_mode_reads_as_off() {
        use crate::session::SessionMemoryMode;
        let cfg = |v: serde_json::Value| {
            serde_json::from_value::<SessionConfig>(serde_json::json!({ "memory_mode": v }))
        };
        assert_eq!(cfg(serde_json::json!("off")).unwrap().memory_mode, Some(SessionMemoryMode::Off));
        assert_eq!(
            cfg(serde_json::json!("capture_and_recall")).unwrap().memory_mode,
            Some(SessionMemoryMode::CaptureAndRecall)
        );
        assert_eq!(
            cfg(serde_json::json!("recall_forever")).unwrap().memory_mode,
            Some(SessionMemoryMode::Off)
        );
        assert_eq!(cfg(serde_json::json!(7)).unwrap().memory_mode, Some(SessionMemoryMode::Off));
        assert_eq!(cfg(serde_json::Value::Null).unwrap().memory_mode, None);
        let missing = serde_json::from_value::<SessionConfig>(serde_json::json!({})).unwrap();
        assert_eq!(missing.memory_mode, None);
        let rs: ReloadState = serde_json::from_value(serde_json::json!({
            "generation": 2,
            "written_at": "2026-10-14T18:17:03Z",
            "sessions": [{
                "id": "s1", "journal_id": "s1", "keep_warm": false, "lifecycle": "live",
                "config": { "memory_mode": "recall_forever" }
            }],
            "expected_clients": 0
        }))
        .expect("the reload state still parses");
        assert_eq!(rs.sessions[0].config.memory_mode, Some(SessionMemoryMode::Off));
    }

    /// Every restored session (reload, crash or stop restore; Live or
    /// Parked) looks its journal up by its exact id. A create from a client
    /// cannot ask for that: the field is never on the wire, and a config
    /// read from the wire looks its query up as a user's `--continue`.
    #[test]
    fn restore_config_looks_the_journal_up_by_its_exact_id() {
        use crate::session::ContinueLookup;
        let saved = SessionConfig {
            continue_session: Some(Some("20250101-000000-beef".into())),
            ..Default::default()
        };
        assert_eq!(saved.continue_lookup, ContinueLookup::Query);
        for lifecycle in [SessionLifecycle::Live, SessionLifecycle::Parked, SessionLifecycle::Parking] {
            let restored = restore_config(&saved, lifecycle);
            assert_eq!(
                restored.continue_lookup,
                ContinueLookup::ExactJournalId,
                "a {lifecycle:?} restore looks its journal up as a user's --continue"
            );
            assert_eq!(restored.continue_session, saved.continue_session, "{lifecycle:?}");
            let wire = serde_json::to_value(&restored).unwrap();
            assert!(wire.get("continue_lookup").is_none(), "never on the wire");
            let read: SessionConfig = serde_json::from_value(wire).unwrap();
            assert_eq!(read.continue_lookup, ContinueLookup::Query, "a wire config is a user lookup");
        }
    }

    /// The restore without the hook wait keeps #255's `/memory` carry: the
    /// session's recorded choice rides in the saved config, Parked or not.
    #[test]
    fn a_restore_keeps_the_sessions_memory_choice() {
        use crate::session::SessionMemoryMode;
        for mode in [Some(SessionMemoryMode::Off), Some(SessionMemoryMode::CaptureOnly), None] {
            let saved = SessionConfig {
                memory_mode: mode,
                await_extensions: true,
                ..Default::default()
            };
            for lifecycle in [SessionLifecycle::Live, SessionLifecycle::Parked, SessionLifecycle::Parking] {
                let restored = restore_config(&saved, lifecycle);
                assert_eq!(restored.memory_mode, mode, "{lifecycle:?}");
                assert!(!restored.await_extensions, "{lifecycle:?}");
            }
        }
    }

    /// Likewise the session's `/memory` choice: a reload without a
    /// checkpoint reply must not re-grant memory the user turned off.
    #[test]
    fn memory_choice_survives_an_unconfirmed_checkpoint() {
        use crate::session::handle::echo::{meta_for, view_for};
        use crate::session::SessionMemoryMode;
        let id = crate::session::SessionId("mem-reload".into());
        let (handle, ep) = SessionHandle::new(meta_for(&id), view_for());
        assert_eq!(record(&handle, None).config.memory_mode, None);
        *ep.memory_mode.lock().unwrap() = Some(SessionMemoryMode::Off);
        assert_eq!(record(&handle, None).config.memory_mode, Some(SessionMemoryMode::Off));
    }

    /// A lock-held placeholder (F23) never answers a checkpoint, so the
    /// next reload records it from its handle: its memory choice must ride
    /// along, or a session reloaded twice comes back in the default.
    #[tokio::test]
    async fn a_locked_placeholder_keeps_its_memory_choice() {
        use crate::session::SessionMemoryMode;
        let sid = crate::session::SessionId("locked-mem".into());
        let config = SessionConfig {
            memory_mode: Some(SessionMemoryMode::Off),
            ..SessionConfig::default()
        };
        let (handle, task) =
            crate::session::handle::locked_placeholder::spawn(sid, config, "pid 1 (tui)".into());
        assert_eq!(record(&handle, None).config.memory_mode, Some(SessionMemoryMode::Off));
        task.abort();
    }

    /// `ReloadAborted` reaches a session whose command queue is full (the
    /// actor drains it) instead of being dropped, which would leave the
    /// session fenced until the backstop.
    #[tokio::test]
    async fn reload_aborted_waits_for_room_in_a_full_queue() {
        use crate::session::handle::echo::{meta_for, view_for};
        let id = crate::session::SessionId("abort-full".into());
        let (handle, mut ep) = SessionHandle::new(meta_for(&id), view_for());
        while handle.send(SessionCommand::Save).await.is_ok() {}
        let drain = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let mut got_abort = false;
            while let Some(a) = ep.cmd_rx.recv().await {
                if matches!(a.cmd, SessionCommand::ReloadAborted) {
                    got_abort = true;
                    break;
                }
            }
            got_abort
        });
        assert!(send_reload_aborted(&handle, Duration::from_secs(5)).await);
        assert!(drain.await.unwrap(), "delivered");
        // No room ever: gives up within its budget, reporting it.
        let id = crate::session::SessionId("abort-stuck".into());
        let (stuck, _ep) = SessionHandle::new(meta_for(&id), view_for());
        while stuck.send(SessionCommand::Save).await.is_ok() {}
        assert!(!send_reload_aborted(&stuck, Duration::from_millis(100)).await);
    }

    /// Spec §5, a refusal after the drain: the daemon does not lift the
    /// quiesce; it stays up until the session handles its `ReloadRefused`
    /// (the actor lifts its own), so a turn whose end the session handles
    /// first still finds it up.
    #[tokio::test]
    async fn a_refusal_leaves_the_quiesce_up_until_the_session_handles_it() {
        use crate::events::queue::Quiesce;
        use crate::session::handle::echo::{meta_for, view_for};
        let id = crate::session::SessionId("refuse-held".into());
        let (handle, mut ep) = SessionHandle::new(meta_for(&id), view_for());
        handle.quiesce(Some(Quiesce::Reload));
        let at = chrono::Utc::now();
        refuse_sessions(std::slice::from_ref(&handle), at).await;
        assert_eq!(handle.quiesced(), Some(Quiesce::Reload));
        let queued = ep.cmd_rx.try_recv().expect("the refusal is queued");
        assert!(
            matches!(queued.cmd, SessionCommand::ReloadRefused { refused_at } if refused_at == at),
            "{:?}",
            queued.cmd
        );
        assert!(ep.cmd_rx.try_recv().is_err(), "one command");
    }

    /// A session the refusal cannot reach (its actor is gone) has its
    /// quiesce lifted by the daemon, so it keeps working.
    #[tokio::test]
    async fn a_refusal_lifts_the_quiesce_of_a_session_it_cannot_reach() {
        use crate::events::queue::Quiesce;
        use crate::session::handle::echo::{meta_for, view_for};
        let id = crate::session::SessionId("refuse-gone".into());
        let (handle, ep) = SessionHandle::new(meta_for(&id), view_for());
        drop(ep);
        handle.quiesce(Some(Quiesce::Reload));
        refuse_sessions(std::slice::from_ref(&handle), chrono::Utc::now()).await;
        assert_eq!(handle.quiesced(), None);
    }

    /// The child ledger rides in the reload state, and the state stays
    /// readable both ways: the previous images' structs (no `children`, no
    /// `deny_unknown_fields`) read a new state, and a state written by a
    /// previous image reads as no children. The image with `base_umask`
    /// (1000b7d0) keeps its mask through a round trip as well.
    #[test]
    fn the_child_ledger_rides_in_the_reload_state_and_old_images_ignore_it() {
        use crate::session::children::{ChildIdentity, ChildRecord, ChildState};
        /// `ReloadState` as of the previous image (deploy/jr-next 716c05de).
        #[derive(Deserialize, Serialize)]
        struct PreviousReloadState {
            generation: u64,
            written_at: chrono::DateTime<chrono::Utc>,
            sessions: Vec<ReloadSession>,
            expected_clients: usize,
        }
        /// `ReloadState` as of deploy/jr-next 1000b7d0 (with `base_umask`).
        #[derive(Deserialize, Serialize)]
        struct UmaskReloadState {
            generation: u64,
            written_at: chrono::DateTime<chrono::Utc>,
            sessions: Vec<ReloadSession>,
            expected_clients: usize,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            base_umask: Option<u32>,
        }
        let record = ChildRecord {
            session_id: "s1".into(),
            tool_id: "toolu_1".into(),
            tool_name: "bash".into(),
            handle_id: Some("sa_2".into()),
            shell_session_id: None,
            child: ChildIdentity::new(
                42,
                42,
                42,
                crate::proc_identity::InstanceKey::Linux { boot_id: "b".into(), start_ticks: 7 },
            ),
            state: ChildState::Ended,
            exit_code: Some(0),
            signal: None,
        };
        let new = ReloadState {
            generation: 3,
            written_at: chrono::Utc::now(),
            sessions: Vec::new(),
            expected_clients: 1,
            next_subagent_id: 0,
            base_umask: Some(0o022),
            children: vec![record.clone()],
        };
        let body = serde_json::to_string(&new).unwrap();
        assert!(body.contains("\"children\""), "{body}");
        let back: ReloadState = serde_json::from_str(&body).unwrap();
        assert_eq!(back.children, vec![record]);
        let previous: PreviousReloadState = serde_json::from_str(&body).expect("the previous image reads a new state");
        assert_eq!((previous.generation, previous.expected_clients), (3, 1));
        let old_body = serde_json::to_string(&previous).unwrap();
        let from_old: ReloadState = serde_json::from_str(&old_body).expect("a new image reads an old state");
        assert!(from_old.children.is_empty());
        let umask_image: UmaskReloadState = serde_json::from_str(&body).expect("the umask image reads a new state");
        assert_eq!(umask_image.base_umask, Some(0o022));
        let from_umask: ReloadState = serde_json::from_str(&serde_json::to_string(&umask_image).unwrap())
            .expect("a new image reads the umask image's state");
        assert_eq!((from_umask.base_umask, from_umask.children.len()), (Some(0o022), 0));
        let empty = ReloadState { children: Vec::new(), ..back };
        assert!(!serde_json::to_string(&empty).unwrap().contains("children"), "an empty ledger is omitted");
        // The carried record as text, byte for byte (it is the state's last
        // field): the old fields, then the child's `instance`.
        let tail = &body[body.find(r#","children":"#).unwrap()..];
        assert_eq!(
            tail,
            concat!(
                r#","children":[{"session_id":"s1","tool_id":"toolu_1","tool_name":"bash","handle_id":"sa_2","#,
                r#""shell_session_id":null,"child":{"pid":42,"start_ticks":7,"boot_id":"b","sid":42,"pgid":42,"#,
                r#""instance":{"kind":"linux","boot_id":"b","start_ticks":7}},"#,
                r#""state":"ended","exit_code":0,"signal":null}]}"#
            )
        );
        // A state written before `instance` existed still loads, with the
        // Linux key its fields name.
        let instance = r#","instance":{"kind":"linux","boot_id":"b","start_ticks":7}"#;
        let before_instance = body.replace(instance, "");
        assert!(!before_instance.contains("instance"), "{before_instance}");
        let from_before: ReloadState = serde_json::from_str(&before_instance).expect("a state without instance loads");
        assert_eq!(from_before.children, new.children);
    }

    // ── the drain's cap ──

    /// How a stand-in session answers the drain's idle probe.
    #[derive(Clone, Copy)]
    enum Probe {
        /// Idle, after this long.
        Idle(Duration),
        /// Busy (a turn streaming), at once.
        Busy,
        /// Never.
        Silent,
    }

    fn answer(
        events: &tokio::sync::broadcast::Sender<crate::session::Envelope>,
        sid: &crate::session::SessionId,
        id: u64,
        value: serde_json::Value,
    ) {
        let _ = events.send(crate::session::Envelope {
            session_id: sid.clone(),
            seq: 0,
            ts: chrono::Utc::now(),
            event: SessionEventWire::QueryResult { id, value, client_tools: None },
        });
    }

    /// A stand-in session for the drain: answers the idle probe as `probe`
    /// says, and records each checkpoint's `skip_quiesce_grace`, answering
    /// it at once (with no record).
    fn drain_stub(name: &str, probe: Probe) -> (SessionHandle, Arc<std::sync::Mutex<Vec<bool>>>) {
        use crate::session::handle::echo::{meta_for, view_for};
        use crate::session::wire::IDLE_PROBE_QUERY_ID;
        let sid = crate::session::SessionId(name.into());
        let (handle, mut ep) = SessionHandle::new(meta_for(&sid), view_for());
        let skips = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = Arc::clone(&skips);
        tokio::spawn(async move {
            while let Some(addressed) = ep.cmd_rx.recv().await {
                match addressed.cmd {
                    SessionCommand::Query { id: IDLE_PROBE_QUERY_ID, .. } => match probe {
                        Probe::Idle(after) => {
                            let (events, sid) = (ep.events.clone(), sid.clone());
                            tokio::spawn(async move {
                                tokio::time::sleep(after).await;
                                let idle = serde_json::json!({"streaming": false, "pending_prompts": 0});
                                answer(&events, &sid, IDLE_PROBE_QUERY_ID, idle);
                            });
                        }
                        Probe::Busy => {
                            let busy = serde_json::json!({"streaming": true, "pending_prompts": 0});
                            answer(&ep.events, &sid, IDLE_PROBE_QUERY_ID, busy);
                        }
                        Probe::Silent => {}
                    },
                    SessionCommand::Checkpoint { skip_quiesce_grace, .. } => {
                        seen.lock().unwrap().push(skip_quiesce_grace);
                        let done = serde_json::json!({"ok": true, "record": null});
                        answer(&ep.events, &sid, CHECKPOINT_QUERY_ID, done);
                    }
                    _ => {}
                }
            }
        });
        (handle, skips)
    }

    fn counts(out: &DrainOutcome) -> (usize, usize, usize, bool) {
        (out.sessions, out.idle, out.busy, out.cap_used)
    }

    /// (a) The drain never runs past its cap. A session whose probe never
    /// answers counts as busy at the cap; one that answers busy is re-polled
    /// up to the cap and not past it. Paused time: `elapsed` is the drain's
    /// own clock. Asserted on the returned outcome (the fields its one INFO
    /// line reports), not on a captured log: a thread-local subscriber can
    /// miss the line while other tests run beside it.
    #[tokio::test(start_paused = true)]
    async fn the_drain_ends_at_its_cap_when_a_probe_never_answers() {
        let cap = Duration::from_secs(1);
        let (idle, _) = drain_stub("d-idle", Probe::Idle(Duration::ZERO));
        let (silent, _) = drain_stub("d-silent", Probe::Silent);
        let live = vec![idle, silent];
        let drained = drain(cap, || live.clone()).await;
        assert_eq!(
            drained.elapsed.as_millis(),
            cap.as_millis(),
            "the drain ends at its cap, not before or after: {drained:?}"
        );
        assert_eq!(counts(&drained), (2, 1, 1, true), "{drained:?}");

        let cap = Duration::from_millis(1100);
        let (busy, _) = drain_stub("d-busy", Probe::Busy);
        let drained = drain(cap, || vec![busy.clone()]).await;
        assert!(
            drained.elapsed <= cap + Duration::from_millis(5),
            "the re-poll slept past the cap: {:?}",
            drained.elapsed
        );
        assert_eq!(counts(&drained), (1, 0, 1, true), "{drained:?}");
    }

    /// (b) The probes run at once: eight sessions that each take 1.5 s to
    /// answer idle end the drain in about 1.5 s, not 8 x 1.5 s.
    #[tokio::test(start_paused = true)]
    async fn the_drain_probes_every_session_at_once() {
        let live: Vec<_> = (0..8)
            .map(|i| drain_stub(&format!("d-slow-{i}"), Probe::Idle(Duration::from_millis(1500))).0)
            .collect();
        let drained = drain(Duration::from_secs(30), || live.clone()).await;
        assert!(
            drained.elapsed < Duration::from_secs(2),
            "the probes ran one after another: {:?}",
            drained.elapsed
        );
        assert_eq!(counts(&drained), (8, 8, 0, false), "{drained:?}");
    }

    /// (c) A drain that ran to its cap checkpoints with no quiesce grace;
    /// one that ended early (every session idle) keeps it, and so does
    /// `--now` (no drain: the grace is a turn's only chance to end at a
    /// round boundary).
    #[tokio::test(start_paused = true)]
    async fn the_checkpoint_skips_the_quiesce_grace_only_after_a_full_cap_drain() {
        let (busy, skips) = drain_stub("c-busy", Probe::Busy);
        let live = vec![busy];
        let drained = drain(Duration::from_secs(1), || live.clone()).await;
        assert!(drained.cap_used, "{drained:?}");
        checkpoint_all(&live, &drained).await;
        assert_eq!(*skips.lock().unwrap(), vec![true], "a full-cap drain skips the grace");

        let (idle, skips) = drain_stub("c-idle", Probe::Idle(Duration::ZERO));
        let live = vec![idle];
        let drained = drain(Duration::from_secs(30), || live.clone()).await;
        assert!(!drained.cap_used, "{drained:?}");
        checkpoint_all(&live, &drained).await;
        assert_eq!(*skips.lock().unwrap(), vec![false], "an early drain keeps the grace");

        let (now, skips) = drain_stub("c-now", Probe::Busy);
        let live = vec![now];
        let drained = drain(Duration::ZERO, || live.clone()).await;
        assert!(!drained.cap_used, "{drained:?}");
        checkpoint_all(&live, &drained).await;
        assert_eq!(*skips.lock().unwrap(), vec![false], "--now keeps the grace");
    }

    /// (d) Every session idle: the drain ends at once, as before; so does a
    /// drain with no session.
    #[tokio::test(start_paused = true)]
    async fn the_drain_ends_early_when_every_session_is_idle() {
        let live: Vec<_> =
            (0..3).map(|i| drain_stub(&format!("d-idle-{i}"), Probe::Idle(Duration::ZERO)).0).collect();
        let drained = drain(Duration::from_secs(30), || live.clone()).await;
        assert!(drained.elapsed < DRAIN_REPOLL, "{drained:?}");
        assert_eq!(counts(&drained), (3, 3, 0, false), "{drained:?}");
        let drained = drain(Duration::from_secs(30), Vec::new).await;
        assert!(drained.elapsed < DRAIN_REPOLL, "{drained:?}");
        assert_eq!(counts(&drained), (0, 0, 0, false), "{drained:?}");
    }
}
