use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use chrono::{DateTime, Utc};

use crate::core::config::base_dir;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRegistration {
    pub session_id: String,
    pub name: Option<String>,
    pub socket_path: String,
    pub pid: u32,
    pub started_at: DateTime<Utc>,
}

/// Returns `~/.synaps-cli/run/`, creating it (mode 0700) if it doesn't exist.
pub fn registry_dir() -> PathBuf {
    let dir = base_dir().join("run");
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

/// Returns the Unix domain socket path for a session.
/// Sockets live in the registry dir (~/.synaps-cli/run/) which is user-owned
/// and mode 0700, avoiding /tmp symlink squatting and TOCTOU races.
pub fn socket_path_for_session(session_id: &str) -> String {
    registry_dir().join(format!("{}.sock", session_id))
        .to_string_lossy()
        .into_owned()
}

/// Write `{session_id}.json` atomically (tmp + rename). Chmod 0600 on Unix.
pub fn register_session(reg: &SessionRegistration) -> Result<(), String> {
    register_session_in(reg, &registry_dir())
}

fn register_session_in(reg: &SessionRegistration, dir: &PathBuf) -> Result<(), String> {
    let path = dir.join(format!("{}.json", reg.session_id));
    let tmp = path.with_extension("tmp");

    let json = serde_json::to_string(reg)
        .map_err(|e| format!("serialize error: {}", e))?;

    std::fs::write(&tmp, &json)
        .map_err(|e| format!("write error: {}", e))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }

    std::fs::rename(&tmp, &path)
        .map_err(|e| format!("rename error: {}", e))?;

    Ok(())
}

/// Remove the registration file. Best-effort — never panics.
/// Also removes the socket file at `socket_path` if it exists.
pub fn unregister_session(session_id: &str) {
    unregister_session_in(session_id, &registry_dir());
}

fn unregister_session_in(session_id: &str, dir: &PathBuf) {
    let path = dir.join(format!("{}.json", session_id));

    // Load first so we can clean up the socket.
    if let Ok(content) = std::fs::read_to_string(&path) {
        if let Ok(reg) = serde_json::from_str::<SessionRegistration>(&content) {
            let sock = std::path::Path::new(&reg.socket_path);
            if sock.exists() {
                let _ = std::fs::remove_file(sock);
            }
        }
    }

    let _ = std::fs::remove_file(&path);
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

/// Read all registration files, prune stale ones (dead PID), return live set.
pub fn list_active_sessions() -> Vec<SessionRegistration> {
    list_active_sessions_in(&registry_dir())
}

fn list_active_sessions_in(dir: &PathBuf) -> Vec<SessionRegistration> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut live = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "json") {
            let Ok(content) = std::fs::read_to_string(&path) else { continue };
            let Ok(reg) = serde_json::from_str::<SessionRegistration>(&content) else {
                let _ = std::fs::remove_file(&path);
                continue;
            };

            if pid_is_alive(reg.pid) {
                live.push(reg);
            } else {
                let sock = std::path::Path::new(&reg.socket_path);
                if sock.exists() {
                    let _ = std::fs::remove_file(sock);
                }
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    live
}

/// Resolve a query to a registration. Resolution order:
/// 1. Exact session_id
/// 2. Name match
/// 3. Partial session_id prefix (unambiguous)
pub fn find_session_registration(query: &str) -> Option<SessionRegistration> {
    find_session_registration_in(query, &registry_dir())
}

fn find_session_registration_in(query: &str, dir: &PathBuf) -> Option<SessionRegistration> {
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
    use tempfile::TempDir;

    fn tmp_registry() -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();
        dir
    }

    fn make_reg(id: &str, name: Option<&str>, pid: u32) -> SessionRegistration {
        SessionRegistration {
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

    #[test]
    fn socket_path_format() {
        let path = socket_path_for_session("20240101-120000-ab12");
        // Sockets now live in the registry dir, not /tmp
        assert!(path.ends_with("/run/20240101-120000-ab12.sock"), "got: {}", path);
        assert!(!path.contains("/tmp/"), "socket should not be in /tmp");
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
