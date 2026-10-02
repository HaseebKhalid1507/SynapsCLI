use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::core::config::base_dir;

/// Discriminator written into every registration so the registry can tell
/// its own files from other `*.json` sharing the run dir (`daemon.json`,
/// tooling drops). Files missing it (older binaries) are treated as sessions.
pub const REGISTRATION_KIND: &str = "session";

fn default_kind() -> String {
    REGISTRATION_KIND.to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRegistration {
    /// Always `"session"`; see [`REGISTRATION_KIND`].
    #[serde(default = "default_kind")]
    pub kind: String,
    pub session_id: String,
    pub name: Option<String>,
    pub socket_path: String,
    pub pid: u32,
    pub started_at: DateTime<Utc>,
}

/// Returns the session runtime directory, creating it mode 0700 if needed.
///
/// `SYNAPS_RUNTIME_DIR` deliberately controls only ephemeral Unix sockets and
/// registration files. Persistent state and plugin discovery remain rooted at
/// `SYNAPS_BASE_DIR`; this avoids the 108-byte `sun_path` limit when the base
/// directory is an EFS session path.
pub fn registry_dir() -> PathBuf {
    let dir = std::env::var_os("SYNAPS_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| base_dir().join("run"));
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!("registry: failed to create run dir {:?}: {}", dir, e);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    dir
}

/// Sanitize a session ID for safe use in filenames and socket paths.
/// Rejects path separators, `..`, and non-printable characters.
/// Returns the sanitized string (replaces unsafe chars with `_`).
pub fn sanitize_session_id(raw: &str) -> String {
    // Only allow alphanumeric, hyphens, and underscores. Dots are not needed
    // in session IDs (format is {name}-{timestamp}-{pid}) and allowing them
    // complicates path traversal prevention (single-pass ".." replace is
    // incomplete for "..." inputs).
    raw.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
}

/// Returns the Unix domain socket path for a session.
/// Sockets live in the registry dir — `$SYNAPS_RUNTIME_DIR` when set (e.g.
/// `/run/user/<uid>/synaps`), otherwise `$SYNAPS_BASE_DIR/run` (default
/// `~/.synaps-cli/run/`) — which is user-owned and mode 0700, avoiding /tmp
/// symlink squatting and TOCTOU races.
pub fn socket_path_for_session(session_id: &str) -> String {
    socket_path_in_dir(&registry_dir(), session_id)
}

fn socket_path_in_dir(dir: &std::path::Path, session_id: &str) -> String {
    let safe_id = sanitize_session_id(session_id);
    dir.join(format!("{}.sock", safe_id))
        .to_string_lossy()
        .into_owned()
}

/// Write `{session_id}.json` atomically (tmp + rename). Chmod 0600 on Unix.
pub fn register_session(reg: &SessionRegistration) -> Result<(), String> {
    register_session_in(reg, &registry_dir())
}

fn register_session_in(reg: &SessionRegistration, dir: &std::path::Path) -> Result<(), String> {
    let safe_id = sanitize_session_id(&reg.session_id);
    let path = dir.join(format!("{}.json", safe_id));
    let tmp = path.with_extension("tmp");

    let json = serde_json::to_string(reg).map_err(|e| format!("serialize error: {}", e))?;

    std::fs::write(&tmp, &json).map_err(|e| format!("write error: {}", e))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }

    std::fs::rename(&tmp, &path).map_err(|e| format!("rename error: {}", e))?;

    Ok(())
}

/// Rewrite `{session_id}.json` with a new `name` (rename / `saveas`). The
/// rest of the record (socket, pid, started_at) is kept. Errors when there
/// is no registration to update.
pub fn update_session_name(session_id: &str, name: Option<&str>) -> Result<(), String> {
    update_session_name_in(session_id, name, &registry_dir())
}

fn update_session_name_in(session_id: &str, name: Option<&str>, dir: &std::path::Path) -> Result<(), String> {
    let safe_id = sanitize_session_id(session_id);
    let path = dir.join(format!("{}.json", safe_id));
    let content = std::fs::read_to_string(&path).map_err(|e| format!("read {}: {}", path.display(), e))?;
    let mut reg: SessionRegistration =
        serde_json::from_str(&content).map_err(|e| format!("parse {}: {}", path.display(), e))?;
    reg.name = name.map(str::to_string);
    register_session_in(&reg, dir)
}

/// Remove the registration file. Best-effort — never panics.
/// Also removes the socket file at `socket_path` if it exists.
pub fn unregister_session(session_id: &str) {
    unregister_session_in(session_id, &registry_dir());
}

fn unregister_session_in(session_id: &str, dir: &std::path::Path) {
    let safe_id = sanitize_session_id(session_id);
    let path = dir.join(format!("{}.json", safe_id));

    // Load first so we can clean up the socket.
    if let Ok(content) = std::fs::read_to_string(&path) {
        if let Ok(reg) = serde_json::from_str::<SessionRegistration>(&content) {
            let sock = std::path::Path::new(&reg.socket_path);
            // Only delete if socket_path is inside the registry dir — prevents
            // a crafted JSON from causing arbitrary file deletion.
            if sock.starts_with(dir) && sock.extension().is_some_and(|e| e == "sock") {
                let _ = std::fs::remove_file(sock);
            }
        }
    }

    let _ = std::fs::remove_file(&path);
}

/// Remove `reg`'s registration and socket, but only while the file on disk
/// is still this exact registration (same owner pid, start time and socket).
///
/// This is what a session calls when it ends. The check matters because the
/// same id can be registered again while an older owner is still winding
/// down (a second actor on one id, a rehydrated session after a reload): the
/// old owner must not delete the newer registration. Best-effort and
/// idempotent: a missing or foreign file is left alone.
pub fn unregister_owned(reg: &SessionRegistration) {
    unregister_owned_in(reg, &registry_dir());
}

fn unregister_owned_in(reg: &SessionRegistration, dir: &std::path::Path) {
    let safe_id = sanitize_session_id(&reg.session_id);
    let path = dir.join(format!("{}.json", safe_id));
    let Ok(content) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(on_disk) = serde_json::from_str::<SessionRegistration>(&content) else {
        return;
    };
    if on_disk.kind != REGISTRATION_KIND
        || on_disk.pid != reg.pid
        || on_disk.started_at != reg.started_at
        || on_disk.socket_path != reg.socket_path
    {
        tracing::debug!(session = %reg.session_id, "registry: registration was replaced; leaving the newer one");
        return;
    }
    remove_registration_files(dir, &path, &on_disk);
}

/// Unlink a registration file and its socket. The socket is only removed
/// when it lies inside the registry dir, so a crafted file can never delete
/// anything elsewhere.
fn remove_registration_files(
    dir: &std::path::Path,
    path: &std::path::Path,
    reg: &SessionRegistration,
) {
    let sock = std::path::Path::new(&reg.socket_path);
    if sock.starts_with(dir) && sock.extension().is_some_and(|e| e == "sock") {
        let _ = std::fs::remove_file(sock);
    }
    let _ = std::fs::remove_file(path);
}

/// Remove registrations that no running session can own and return how
/// many were removed. Always: an owner pid that no longer exists. With
/// `own_pid` (the daemon, at startup): also registrations naming that pid,
/// which can only be leftovers. A reload re-execs the same pid without
/// running any session's shutdown, and the new image has not registered
/// anything yet when it sweeps.
pub fn sweep_stale_registrations(own_pid: Option<u32>) -> usize {
    sweep_stale_registrations_in(&registry_dir(), own_pid)
}

fn sweep_stale_registrations_in(dir: &std::path::Path, own_pid: Option<u32>) -> usize {
    scan_registrations(dir, own_pid).1
}

/// Returns true if a process with `pid` is alive (Unix: `kill(pid, 0)`).
fn pid_is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: kill with signal 0 never sends a signal; it only checks
        // whether the process exists and we have permission to signal it.
        let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
        result == 0
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

/// Files in the run dir that are never session registrations, whatever
/// their contents: the daemon's own `daemon.json` / `daemon-<profile>.json`.
fn is_reserved_json(path: &std::path::Path) -> bool {
    path.file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(|stem| stem == "daemon" || stem.starts_with("daemon-"))
}

/// Read all registration files, prune stale ones (dead PID), return live set.
///
/// Only files this registry wrote are ever unlinked: a `*.json` that is not
/// a `SessionRegistration` (parse failure, or `kind != "session"`) is logged
/// and skipped — `daemon.json` lives in the same dir and `synaps send` must
/// never destroy it.
pub fn list_active_sessions() -> Vec<SessionRegistration> {
    list_active_sessions_in(&registry_dir())
}

fn list_active_sessions_in(dir: &std::path::Path) -> Vec<SessionRegistration> {
    scan_registrations(dir, None).0
}

/// Read every session registration in `dir`, unlink the stale ones (dead
/// owner pid, or `own_pid` when given) and return `(live, removed)`.
fn scan_registrations(
    dir: &std::path::Path,
    own_pid: Option<u32>,
) -> (Vec<SessionRegistration>, usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (Vec::new(), 0);
    };

    let mut live = Vec::new();
    let mut removed = 0;

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.extension().is_some_and(|e| e == "json") || is_reserved_json(&path) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let reg = match serde_json::from_str::<SessionRegistration>(&content) {
            Ok(reg) if reg.kind == REGISTRATION_KIND => reg,
            Ok(reg) => {
                tracing::debug!(path = %path.display(), kind = %reg.kind, "registry: not a session registration — skipped");
                continue;
            }
            Err(e) => {
                tracing::debug!(path = %path.display(), error = %e, "registry: unparseable json — skipped, not ours");
                continue;
            }
        };

        if own_pid != Some(reg.pid) && pid_is_alive(reg.pid) {
            live.push(reg);
        } else {
            remove_registration_files(dir, &path, &reg);
            removed += 1;
        }
    }

    (live, removed)
}

/// Resolve a query to a registration. Resolution order:
/// 1. Exact session_id
/// 2. Name match
/// 3. Partial session_id prefix (unambiguous)
pub fn find_session_registration(query: &str) -> Option<SessionRegistration> {
    find_session_registration_in(query, &registry_dir())
}

fn find_session_registration_in(query: &str, dir: &std::path::Path) -> Option<SessionRegistration> {
    let sessions = list_active_sessions_in(dir);

    // 1. Exact ID
    if let Some(reg) = sessions.iter().find(|r| r.session_id == query) {
        return Some(reg.clone());
    }

    // 2. Name match
    if let Some(reg) = sessions.iter().find(|r| r.name.as_deref() == Some(query)) {
        return Some(reg.clone());
    }

    // 3. Partial prefix — only if unambiguous
    let matches: Vec<_> = sessions
        .iter()
        .filter(|r| r.session_id.starts_with(query))
        .collect();

    if matches.len() == 1 {
        Some(matches[0].clone())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use tempfile::TempDir;

    fn tmp_registry() -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();
        dir
    }

    fn make_reg(id: &str, name: Option<&str>, pid: u32) -> SessionRegistration {
        SessionRegistration {
            kind: REGISTRATION_KIND.to_string(),
            session_id: id.to_string(),
            name: name.map(|s| s.to_string()),
            socket_path: socket_path_for_session(id),
            pid,
            started_at: Utc::now(),
        }
    }

    fn dir_buf(tmp: &TempDir) -> PathBuf {
        tmp.path().to_path_buf()
    }

    #[test]
    fn register_creates_file() {
        let tmp = tmp_registry();
        let dir = dir_buf(&tmp);
        let reg = make_reg("abc-1234", None, std::process::id());
        register_session_in(&reg, &dir).unwrap();
        assert!(dir.join("abc-1234.json").exists());
    }

    #[test]
    fn list_returns_live_sessions() {
        let tmp = tmp_registry();
        let dir = dir_buf(&tmp);
        let pid = std::process::id();
        let reg = make_reg("live-0001", Some("my-agent"), pid);
        register_session_in(&reg, &dir).unwrap();

        let sessions = list_active_sessions_in(&dir);
        assert!(sessions.iter().any(|r| r.session_id == "live-0001"));
    }

    #[test]
    fn find_by_exact_id() {
        let tmp = tmp_registry();
        let dir = dir_buf(&tmp);
        let reg = make_reg("find-exact-01", None, std::process::id());
        register_session_in(&reg, &dir).unwrap();

        let found = find_session_registration_in("find-exact-01", &dir);
        assert!(found.is_some());
        assert_eq!(found.unwrap().session_id, "find-exact-01");
    }

    #[test]
    fn find_by_name() {
        let tmp = tmp_registry();
        let dir = dir_buf(&tmp);
        let reg = make_reg("named-session-01", Some("prod-agent"), std::process::id());
        register_session_in(&reg, &dir).unwrap();

        let found = find_session_registration_in("prod-agent", &dir);
        assert!(found.is_some());
        assert_eq!(found.unwrap().session_id, "named-session-01");
    }

    #[test]
    fn find_by_partial_prefix() {
        let tmp = tmp_registry();
        let dir = dir_buf(&tmp);
        let reg = make_reg("prefix-abcdef-01", None, std::process::id());
        register_session_in(&reg, &dir).unwrap();

        let found = find_session_registration_in("prefix-abc", &dir);
        assert!(found.is_some());
        assert_eq!(found.unwrap().session_id, "prefix-abcdef-01");
    }

    #[test]
    fn ambiguous_prefix_returns_none() {
        let tmp = tmp_registry();
        let dir = dir_buf(&tmp);
        let pid = std::process::id();
        register_session_in(&make_reg("dup-aaaa-01", None, pid), &dir).unwrap();
        register_session_in(&make_reg("dup-aaaa-02", None, pid), &dir).unwrap();

        let found = find_session_registration_in("dup-aaaa", &dir);
        assert!(found.is_none(), "ambiguous prefix should return None");
    }

    /// Bug: `synaps send` used to unlink every `run/*.json` it could not
    /// parse as a registration — `daemon.json` included.
    #[test]
    fn foreign_json_is_never_unlinked() {
        let tmp = tmp_registry();
        let dir = dir_buf(&tmp);
        let daemon = dir.join("daemon.json");
        std::fs::write(
            &daemon,
            r#"{"pid":1,"version":"0.9.0","exe":"/usr/bin/synaps","socket":"/x/daemon.sock","protocol":2}"#,
        )
        .unwrap();
        let garbage = dir.join("garbage.json");
        std::fs::write(&garbage, "this is not json {").unwrap();
        // Parses as a registration structurally but declares another kind.
        let other = dir.join("other-kind.json");
        std::fs::write(
            &other,
            r#"{"kind":"widget","session_id":"x","name":null,"socket_path":"/x.sock","pid":1,"started_at":"2024-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        // A legacy registration without `kind` still counts as a session.
        let legacy = dir.join("legacy-0001.json");
        std::fs::write(
            &legacy,
            format!(
                r#"{{"session_id":"legacy-0001","name":"old","socket_path":"{}","pid":{},"started_at":"2024-01-01T00:00:00Z"}}"#,
                dir.join("legacy-0001.sock").display(),
                std::process::id()
            ),
        )
        .unwrap();
        let reg = make_reg("live-0002", Some("ambient"), std::process::id());
        register_session_in(&reg, &dir).unwrap();

        for _ in 0..2 {
            let found = find_session_registration_in("ambient", &dir).expect("session resolved");
            assert_eq!(found.session_id, "live-0002");
            assert!(find_session_registration_in("old", &dir).is_some(), "legacy reg counts");
            let ids: Vec<_> = list_active_sessions_in(&dir).into_iter().map(|r| r.session_id).collect();
            assert!(!ids.contains(&"x".to_string()), "foreign kind not listed");
            assert!(daemon.exists(), "daemon.json must survive");
            assert!(garbage.exists(), "garbage.json must survive");
            assert!(other.exists(), "other-kind.json must survive");
            assert!(legacy.exists());
        }
        assert!(std::fs::read_to_string(&daemon).unwrap().contains("0.9.0"));
    }

    /// Bug: the name was frozen at create — `saveas` never reached the
    /// registry, so `synaps send --session <name>` missed until a
    /// `--continue`.
    #[test]
    fn update_name_rewrites_registration() {
        let tmp = tmp_registry();
        let dir = dir_buf(&tmp);
        let reg = make_reg("rename-0001", None, std::process::id());
        register_session_in(&reg, &dir).unwrap();
        assert!(find_session_registration_in("ambient", &dir).is_none());

        update_session_name_in("rename-0001", Some("ambient"), &dir).unwrap();
        let found = find_session_registration_in("ambient", &dir).expect("resolves by new name");
        assert_eq!(found.session_id, "rename-0001");
        assert_eq!(found.socket_path, reg.socket_path, "rest of the record kept");

        update_session_name_in("rename-0001", None, &dir).unwrap();
        assert!(find_session_registration_in("ambient", &dir).is_none(), "cleared");
        assert!(update_session_name_in("ghost", Some("x"), &dir).is_err(), "no reg → error");
    }

    #[test]
    fn unregister_removes_file() {
        let tmp = tmp_registry();
        let dir = dir_buf(&tmp);
        let reg = make_reg("unreg-0001", None, std::process::id());
        register_session_in(&reg, &dir).unwrap();

        let path = dir.join("unreg-0001.json");
        assert!(path.exists());

        unregister_session_in("unreg-0001", &dir);
        assert!(!path.exists());
    }

    #[test]
    fn unregister_is_idempotent() {
        let tmp = tmp_registry();
        let dir = dir_buf(&tmp);
        // Should not panic even if the file was never registered
        unregister_session_in("ghost-session-99", &dir);
    }

    #[test]
    fn stale_pid_pruned() {
        let tmp = tmp_registry();
        let dir = dir_buf(&tmp);
        // PID 999999 is effectively guaranteed to not exist
        let reg = make_reg("stale-dead-pid", None, 999999);
        register_session_in(&reg, &dir).unwrap();

        let sessions = list_active_sessions_in(&dir);
        assert!(
            !sessions.iter().any(|r| r.session_id == "stale-dead-pid"),
            "stale registration should have been pruned"
        );

        // File should also be gone
        assert!(!dir.join("stale-dead-pid.json").exists());
    }

    /// A registration whose socket lives in `dir`, with a real socket file,
    /// so removal of both can be observed.
    fn reg_with_socket(dir: &std::path::Path, id: &str, pid: u32) -> SessionRegistration {
        let reg = SessionRegistration {
            kind: REGISTRATION_KIND.to_string(),
            session_id: id.to_string(),
            name: None,
            socket_path: socket_path_in_dir(dir, id),
            pid,
            started_at: Utc::now(),
        };
        std::fs::write(&reg.socket_path, b"").unwrap();
        register_session_in(&reg, dir).unwrap();
        reg
    }

    #[test]
    fn unregister_owned_removes_its_own_registration_and_socket() {
        let tmp = tmp_registry();
        let dir = dir_buf(&tmp);
        let reg = reg_with_socket(&dir, "owned", std::process::id());
        unregister_owned_in(&reg, &dir);
        assert!(!dir.join("owned.json").exists());
        assert!(!dir.join("owned.sock").exists());
        // Idempotent: shutdown() and Drop both call it.
        unregister_owned_in(&reg, &dir);
    }

    #[test]
    fn unregister_owned_leaves_a_newer_registration_of_the_same_id() {
        let tmp = tmp_registry();
        let dir = dir_buf(&tmp);
        let old = reg_with_socket(&dir, "same-id", std::process::id());
        std::thread::sleep(std::time::Duration::from_millis(2));
        let newer = reg_with_socket(&dir, "same-id", std::process::id());
        assert_ne!(old.started_at, newer.started_at);

        unregister_owned_in(&old, &dir);
        assert!(dir.join("same-id.json").exists(), "newer registration kept");
        assert!(dir.join("same-id.sock").exists(), "newer socket kept");
        let live = list_active_sessions_in(&dir);
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].started_at, newer.started_at);
    }

    #[test]
    fn a_renamed_registration_is_still_owned_and_removed() {
        let tmp = tmp_registry();
        let dir = dir_buf(&tmp);
        let reg = reg_with_socket(&dir, "renamed", std::process::id());
        update_session_name_in("renamed", Some("ambient"), &dir).unwrap();
        unregister_owned_in(&reg, &dir);
        assert!(
            !dir.join("renamed.json").exists(),
            "a rename keeps ownership"
        );
    }

    #[cfg(unix)]
    #[test]
    fn daemon_start_sweep_removes_dead_and_own_pid_and_keeps_other_live_owners() {
        let tmp = tmp_registry();
        let dir = dir_buf(&tmp);
        let own = std::process::id();
        // SAFETY: getppid has no preconditions; the parent (the test runner's
        // parent shell) is alive and owned by the same user.
        let other_live = unsafe { libc::getppid() } as u32;
        reg_with_socket(&dir, "dead-owner", 999_999);
        reg_with_socket(&dir, "pre-reload", own);
        reg_with_socket(&dir, "other-process", other_live);
        std::fs::write(dir.join("daemon.json"), r#"{"pid":999999}"#).unwrap();

        assert_eq!(sweep_stale_registrations_in(&dir, Some(own)), 2);
        for gone in ["dead-owner", "pre-reload"] {
            assert!(
                !dir.join(format!("{gone}.json")).exists(),
                "{gone} card removed"
            );
            assert!(
                !dir.join(format!("{gone}.sock")).exists(),
                "{gone} socket removed"
            );
        }
        assert!(dir.join("other-process.json").exists());
        assert!(dir.join("other-process.sock").exists());
        assert!(
            dir.join("daemon.json").exists(),
            "daemon.json is never a session"
        );
    }

    #[test]
    fn sweep_without_own_pid_keeps_registrations_of_this_process() {
        let tmp = tmp_registry();
        let dir = dir_buf(&tmp);
        reg_with_socket(&dir, "mine", std::process::id());
        reg_with_socket(&dir, "dead", 999_999);
        assert_eq!(sweep_stale_registrations_in(&dir, None), 1);
        assert!(dir.join("mine.json").exists());
        assert!(!dir.join("dead.json").exists());
    }

    #[test]
    // Keyed to the same lock as every HOME/SYNAPS_BASE_DIR mutator: unkeyed
    // `#[serial]` and `#[serial(synaps_base_dir)]` do NOT exclude each other,
    // so this base-dir *reader* raced mutator tests that point
    // SYNAPS_BASE_DIR at a tempdir under /tmp.
    #[serial(synaps_base_dir)]
    fn socket_path_format() {
        let path = socket_path_for_session("20240101-120000-ab12");
        // Sockets now live in the registry dir, not /tmp
        assert!(
            path.ends_with("/run/20240101-120000-ab12.sock"),
            "got: {}",
            path
        );
        assert!(!path.contains("/tmp/"), "socket should not be in /tmp");
    }

    // Regression: an EFS-backed SYNAPS_BASE_DIR can exceed Linux's 108-byte
    // sockaddr_un.sun_path limit. Runtime sockets must be independently rooted.
    #[cfg(unix)]
    #[test]
    fn short_runtime_dirs_keep_long_session_socket_bindable_and_isolated() {
        use std::os::unix::net::UnixListener;
        let long_session = "s".repeat(80);
        // These model deterministic per-UID runtime roots created by guest-agent.
        let path_a = socket_path_in_dir(std::path::Path::new("/tmp/a"), &long_session);
        let path_b = socket_path_in_dir(std::path::Path::new("/tmp/b"), &long_session);
        assert!(path_a.len() < 108, "Unix socket path too long: {path_a}");
        assert_ne!(
            path_a, path_b,
            "per-user runtime roots must isolate same session ids"
        );

        // Bind under SHORT, /tmp-rooted dirs. The whole point is short socket
        // paths; some platforms' tempdir() (e.g. macOS /var/folders/...) is long
        // enough to blow the sun_path limit and flake this exact assertion, so we
        // control the root length explicitly. Two sibling dirs model two per-uid
        // runtime roots.
        let pid = std::process::id();
        let a = std::path::PathBuf::from(format!("/tmp/sa{pid}"));
        let b = std::path::PathBuf::from(format!("/tmp/sb{pid}"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let _a = UnixListener::bind(socket_path_in_dir(&a, &long_session)).unwrap();
        let _b = UnixListener::bind(socket_path_in_dir(&b, &long_session)).unwrap();
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }

    #[cfg(unix)]
    #[test]
    fn registration_file_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tmp_registry();
        let dir = dir_buf(&tmp);
        let reg = make_reg("perms-check-01", None, std::process::id());
        register_session_in(&reg, &dir).unwrap();

        let path = dir.join("perms-check-01.json");
        let perms = std::fs::metadata(&path).unwrap().permissions();
        assert_eq!(perms.mode() & 0o777, 0o600, "registry file should be 0600");
    }
}

// ── daemon paths (Phase 2 B) ──────────────────────────────────────────────────

/// Files the `synaps daemon` keeps under `registry_dir()` (0700). One daemon
/// per profile: `daemon.{sock,lock,json,pid}` or `daemon-<P>.{…}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonPaths {
    pub dir: PathBuf,
    pub sock: PathBuf,
    pub lock: PathBuf,
    pub json: PathBuf,
    pub pid: PathBuf,
}

/// Daemon file paths for a profile, under `registry_dir()`.
pub fn daemon_paths(profile: Option<&str>) -> DaemonPaths {
    daemon_paths_in(&registry_dir(), profile)
}

/// Same, rooted at an explicit dir (tests; `--socket` overrides only `.sock`).
pub fn daemon_paths_in(dir: &std::path::Path, profile: Option<&str>) -> DaemonPaths {
    let stem = match profile.map(sanitize_session_id).filter(|p| !p.is_empty()) {
        Some(p) => format!("daemon-{p}"),
        None => "daemon".to_string(),
    };
    DaemonPaths {
        dir: dir.to_path_buf(),
        sock: dir.join(format!("{stem}.sock")),
        lock: dir.join(format!("{stem}.lock")),
        json: dir.join(format!("{stem}.json")),
        pid: dir.join(format!("{stem}.pid")),
    }
}

#[cfg(test)]
mod daemon_paths_tests {
    use super::*;

    #[test]
    fn daemon_paths_are_profile_scoped_and_sanitised() {
        let d = std::path::Path::new("/run/x");
        let p = daemon_paths_in(d, None);
        assert_eq!(p.sock, d.join("daemon.sock"));
        assert_eq!(p.lock, d.join("daemon.lock"));
        assert_eq!(p.json, d.join("daemon.json"));
        assert_eq!(p.pid, d.join("daemon.pid"));
        let p = daemon_paths_in(d, Some("work/../evil"));
        assert_eq!(p.sock, d.join("daemon-work____evil.sock"));
    }
}
