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
//! timeouts, tool-output cap, worker grants, `/system`), and the CURRENT
//! model/thinking (`/model` mid-session survives).

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
/// Env: default drain budget in seconds.
pub const DRAIN_SECS_ENV: &str = "SYNAPS_DAEMON_RELOAD_DRAIN_SECS";
const DEFAULT_DRAIN: Duration = Duration::from_secs(30);
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// Per-session `Checkpoint{Reload}` wait: `cancel_turn` (the drain + its
/// bounded save) + stopping the session's workers + the checkpoint's own
/// bounded save + 1 s margin.
const CHECKPOINT_BUDGET: Duration =
    Duration::from_secs(crate::session::budgets::RELOAD_CHECKPOINT_BUDGET_SECS);
/// What clients are told to wait before reconnecting.
pub const RETRY_AFTER_MS: u64 = 500;

#[derive(Debug, Clone, Default)]
pub struct ReloadRequest {
    pub now: bool,
    pub drain_secs: Option<u64>,
    pub exe: Option<PathBuf>,
}

impl ReloadRequest {
    pub fn from_frame(frame: &ClientFrame) -> Option<Self> {
        match frame {
            ClientFrame::Reload { now, drain_secs, exe } => {
                Some(Self { now: *now, drain_secs: *drain_secs, exe: exe.clone() })
            }
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
/// the checkpoint did not confirm in budget).
async fn checkpoint_one(handle: SessionHandle) -> Option<SessionReloadRecord> {
    let mut rx = handle.subscribe();
    if handle
        .send(SessionCommand::Checkpoint { reason: CheckpointReason::Reload })
        .await
        .is_err()
    {
        return None;
    }
    let wait = async {
        loop {
            match rx.recv().await {
                Ok(env) => {
                    if let SessionEventWire::QueryResult { id, value } = env.event {
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
    tokio::time::timeout(CHECKPOINT_BUDGET, wait).await.unwrap_or(None)
}

/// The session as it IS: the actor's record when the checkpoint confirmed,
/// else what the handle alone knows (journal, cwd, keep-warm, lifecycle,
/// current model from the published view, the `/keepwarm` override).
fn record(handle: &SessionHandle, rec: Option<SessionReloadRecord>) -> ReloadSession {
    let meta = handle.meta();
    let journal_id = handle.journal_id();
    if rec.is_none() {
        tracing::warn!(
            session = %handle.id,
            "reload: no checkpoint reply in time — this session's workers are not carried: \
             no archive is guaranteed and their parent will not be woken"
        );
    }
    let (config, keep_warm, lifecycle, settings_replay, cache_keepwarm, model, workers) = match rec {
        Some(r) => (
            r.config,
            r.keep_warm,
            r.lifecycle,
            r.settings_replay,
            r.cache_keepwarm,
            r.model,
            r.workers,
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
        input_owner_kind: None,
        workers,
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
    if state.reloading.swap(true, Ordering::SeqCst) {
        return Err(ReloadError::Refused("a reload is already in progress".into()));
    }
    let generation = state.generation + 1;
    tracing::info!(exe = %exe.display(), new = %new.binary_version, generation, "daemon: reload accepted");

    // 2. drain (refuse new work), then checkpoint every session concurrently.
    let drain = if req.now {
        Duration::ZERO
    } else {
        req.drain_secs
            .map(Duration::from_secs)
            .or_else(|| std::env::var(DRAIN_SECS_ENV).ok()?.parse().ok().map(Duration::from_secs))
            .unwrap_or(DEFAULT_DRAIN)
    };
    let t0 = std::time::Instant::now();
    while t0.elapsed() < drain {
        let mut all_idle = true;
        for h in state.live_sessions() {
            if !super::session_is_idle(&h).await {
                all_idle = false;
                break;
            }
        }
        if all_idle {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let handles = state.live_sessions();
    let results = futures::future::join_all(handles.iter().cloned().map(checkpoint_one)).await;
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
        return Err(ReloadError::ExecFailed(e));
    }

    // 4. announce — every OTHER conn selects on `reload_announce` and sends
    //    Event(Reloading) + Bye{Reloading}; give their writers ≤ 1 s.
    state.reload_generation.store(generation, Ordering::SeqCst);
    state.fire_announce();
    let t1 = std::time::Instant::now();
    while state.connections.load(Ordering::SeqCst) > 1 && t1.elapsed() < Duration::from_secs(crate::session::budgets::RELOAD_ANNOUNCE_WAIT_SECS) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(Prepared { exe, rs_path, generation })
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

    let err = exec_self(&p.exe, &p.rs_path, state);
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

#[cfg(not(unix))]
fn exec_self(_exe: &Path, _rs_path: &Path, _state: &DaemonState) -> String {
    "reload is unix-only".into()
}

/// New-image side (`Daemon::start`): when `RELOAD_STATE_ENV` is set, the
/// lock is ADOPTED from `LOCK_FD_ENV` (mandatory — refuse to start without
/// it, risk §6.8) and the recorded generation is returned.
pub fn adopt_from_env() -> anyhow::Result<Option<(DaemonLock, ReloadState, PathBuf)>> {
    let Some(rs_path) = std::env::var_os(RELOAD_STATE_ENV) else {
        return Ok(None);
    };
    let rs_path = PathBuf::from(rs_path);
    std::env::remove_var(RELOAD_STATE_ENV);
    let fd: i32 = std::env::var(LOCK_FD_ENV)
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow::anyhow!("{RELOAD_STATE_ENV} set without {LOCK_FD_ENV}: refusing to start (the flock is the liveness oracle)"))?;
    std::env::remove_var(LOCK_FD_ENV);
    let id_floor = std::env::var(SUBAGENT_ID_FLOOR_ENV).ok().and_then(|s| s.parse::<u64>().ok());
    std::env::remove_var(SUBAGENT_ID_FLOOR_ENV);
    let lock = DaemonLock::adopt(fd)?;
    let body = std::fs::read_to_string(&rs_path)?;
    let mut rs: ReloadState = serde_json::from_str(&body)?;
    if let Some(floor) = id_floor {
        rs.next_subagent_id = rs.next_subagent_id.max(floor);
    }
    Ok(Some((lock, rs, rs_path)))
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
pub(super) fn is_lock_held_error(e: &str) -> bool {
    e.contains("is live in another process")
}

/// Extract `"pid N (kind)"` from the lock-held error message.
pub(super) fn parse_holder_from_error(e: &str) -> String {
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
    use crate::session::TransportError;
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        match h.send(SessionCommand::ReloadAborted).await {
            Ok(()) => return true,
            Err(TransportError::Backpressure) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(e) => {
                tracing::warn!(
                    session = %h.id,
                    error = %e,
                    "reload: could not tell the session the exec failed; its reload fence lifts at the backstop"
                );
                return false;
            }
        }
    }
}

/// What a recreated session gets back after `create` (reload's rehydrate,
/// and the pinned-session restore in `pins.rs`): the non-persisted knobs
/// (host-originated `Set`: no owner check), the pin, and the `/keepwarm`
/// override. The journal carries the conversation, model and name.
pub(super) async fn apply_record(
    h: &SessionHandle,
    settings_replay: &[SessionSetting],
    keep_warm: bool,
    cache_keepwarm: Option<&str>,
) {
    for (i, setting) in settings_replay.iter().enumerate() {
        let _ = h
            .send(SessionCommand::Set {
                id: u64::MAX - i as u64,
                setting: setting.clone(),
            })
            .await;
    }
    if keep_warm {
        let _ = h.send(SessionCommand::KeepWarm { on: true }).await;
    }
    if let Some(mode) = cache_keepwarm {
        let _ = h
            .send(SessionCommand::EngineCommand {
                id: u64::MAX,
                name: "keepwarm".into(),
                arg: mode.to_string(),
            })
            .await;
    }
}

pub async fn rehydrate(state: &Arc<DaemonState>, rs: &ReloadState) {
    // Before any session can dispatch: ids continue where the old image
    // stopped, so `sa_1` never names two different workers.
    crate::tools::seed_next_subagent_id(rs.subagent_id_floor());
    for s in &rs.sessions {
        let created = match state.create(s.config.clone()).await {
            Ok(h) => Ok(h),
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
                continue;
            }
            Err(e) if s.config.continue_session.is_some() => {
                tracing::warn!(session = %s.id, error = %e, "daemon: journal not continuable; recreating fresh");
                state
                    .create(SessionConfig {
                        continue_session: None,
                        ..s.config.clone()
                    })
                    .await
            }
            Err(e) => Err(e),
        };
        match created {
            Ok(h) => {
                let new_id = h.id.as_str().to_string();
                if new_id != s.id {
                    state
                        .reload_aliases
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(s.id.clone(), new_id.clone());
                }
                apply_record(&h, &s.settings_replay, s.keep_warm, s.cache_keepwarm.as_deref()).await;
                // Before any Park: re-register the stopped workers and wake
                // their parent (it was waiting on completions the exec ate).
                if !s.workers.is_empty() {
                    let _ = h
                        .send(SessionCommand::AdoptWorkers { workers: s.workers.clone() })
                        .await;
                }
                if matches!(s.lifecycle, SessionLifecycle::Parked | SessionLifecycle::Parking) {
                    let _ = h.send(SessionCommand::Park).await;
                }
                tracing::info!(old = %s.id, new = %new_id, lifecycle = ?s.lifecycle, "daemon: session rehydrated after reload");
            }
            Err(e) => tracing::warn!(session = %s.id, error = %e, "daemon: session could not be rehydrated"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        };
        let mut rs = ReloadState {
            generation: 10,
            written_at: chrono::Utc::now(),
            sessions: vec![session(vec![worker("sa_288"), worker("sa_286")]), session(vec![])],
            expected_clients: 0,
            next_subagent_id: 0,
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
}
