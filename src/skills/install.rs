//! Git-backed plugin install/uninstall/update.

use std::path::Path;
use std::process::Command;

/// `git clone --depth=1 <url> <dest>`, then `git rev-parse HEAD`.
/// `dest` must not already exist.
pub fn install_plugin(source_url: &str, dest: &Path) -> Result<String, String> {
    if source_url.starts_with('-') {
        return Err(format!("refusing suspicious url: {}", source_url));
    }
    if dest.exists() {
        return Err(format!("{} already exists on disk; uninstall first", dest.display()));
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("mkdir {}: {}", parent.display(), e))?;
    }
    let out = Command::new("git")
        .args(["clone", "--depth=1", "-q", "--", source_url])
        .arg(dest)
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "git not found on PATH".to_string()
            } else {
                format!("spawn git: {}", e)
            }
        })?;
    if !out.status.success() {
        let _ = std::fs::remove_dir_all(dest);
        return Err(format!(
            "git clone failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    rev_parse_head(dest)
}

/// `rm -rf <path>`. Missing path is OK.
pub fn uninstall_plugin(path: &Path) -> Result<(), String> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("remove {}: {}", path.display(), e)),
    }
}

/// `git -C <path> pull --ff-only`, then capture new SHA.
pub fn update_plugin(install_path: &Path) -> Result<String, String> {
    let out = Command::new("git")
        .args(["-C"])
        .arg(install_path)
        .args(["pull", "--ff-only", "-q"])
        .output()
        .map_err(|e| format!("spawn git: {}", e))?;
    if !out.status.success() {
        return Err(format!(
            "git pull failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    rev_parse_head(install_path)
}

/// `git ls-remote <url> HEAD` → first column (SHA). Network op.
pub fn ls_remote_head(source_url: &str) -> Result<String, String> {
    if source_url.starts_with('-') {
        return Err(format!("refusing suspicious url: {}", source_url));
    }
    let out = Command::new("git")
        .args(["ls-remote", "--", source_url, "HEAD"])
        .output()
        .map_err(|e| format!("spawn git: {}", e))?;
    if !out.status.success() {
        return Err(format!(
            "git ls-remote failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let sha = stdout
        .split_whitespace()
        .next()
        .ok_or("empty ls-remote output")?;
    if sha.len() != 40 {
        return Err(format!("unexpected ls-remote output: {}", stdout));
    }
    Ok(sha.to_string())
}

fn rev_parse_head(repo: &Path) -> Result<String, String> {
    let out = Command::new("git")
        .args(["-C"])
        .arg(repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .map_err(|e| format!("spawn git: {}", e))?;
    if !out.status.success() {
        return Err(format!(
            "git rev-parse failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// Build a throwaway local bare git repo to clone from (no network).
    fn mk_local_repo() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        Command::new("git").args(["init", "-q"]).current_dir(&work).status().unwrap();
        Command::new("git").args(["config", "user.email", "t@t"]).current_dir(&work).status().unwrap();
        Command::new("git").args(["config", "user.name", "t"]).current_dir(&work).status().unwrap();
        std::fs::write(work.join("SKILL.md"),
            "---\nname: demo\ndescription: d\n---\nbody").unwrap();
        Command::new("git").args(["add", "."]).current_dir(&work).status().unwrap();
        Command::new("git").args(["commit", "-q", "-m", "init"]).current_dir(&work).status().unwrap();

        let bare = dir.path().join("bare.git");
        Command::new("git").args(["clone", "--bare", "-q",
            work.to_str().unwrap(), bare.to_str().unwrap()]).status().unwrap();
        (dir, bare)
    }

    #[test]
    fn install_clones_and_returns_sha() {
        let (_tmp, bare) = mk_local_repo();
        let dest_parent = tempfile::tempdir().unwrap();
        let dest = dest_parent.path().join("demo");
        let sha = install_plugin(
            &format!("file://{}", bare.display()),
            &dest,
        ).unwrap();
        assert!(dest.join("SKILL.md").exists());
        assert_eq!(sha.len(), 40);
    }

    #[test]
    fn install_refuses_if_target_exists() {
        let (_tmp, bare) = mk_local_repo();
        let dest_parent = tempfile::tempdir().unwrap();
        let dest = dest_parent.path().join("demo");
        std::fs::create_dir_all(&dest).unwrap();
        let err = install_plugin(
            &format!("file://{}", bare.display()),
            &dest,
        ).unwrap_err();
        assert!(err.contains("already"));
    }

    #[test]
    fn uninstall_removes_directory() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("demo");
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("x"), "y").unwrap();
        uninstall_plugin(&p).unwrap();
        assert!(!p.exists());
    }

    #[test]
    fn uninstall_missing_dir_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("nothere");
        assert!(uninstall_plugin(&p).is_ok());
    }

    #[test]
    fn ls_remote_head_returns_sha_on_real_repo() {
        let (_tmp, bare) = mk_local_repo();
        let sha = ls_remote_head(&format!("file://{}", bare.display())).unwrap();
        assert_eq!(sha.len(), 40);
    }

    #[test]
    fn update_plugin_fast_forwards_and_returns_new_sha() {
        let (_tmp, bare) = mk_local_repo();
        let dest_parent = tempfile::tempdir().unwrap();
        let dest = dest_parent.path().join("demo");
        let initial_sha = install_plugin(
            &format!("file://{}", bare.display()),
            &dest,
        ).unwrap();

        // Push a second commit to the bare repo.
        let pusher_parent = tempfile::tempdir().unwrap();
        let pusher = pusher_parent.path().join("push");
        Command::new("git").args(["clone", "-q"])
            .arg(&bare).arg(&pusher).status().unwrap();
        Command::new("git").args(["config", "user.email", "t@t"]).current_dir(&pusher).status().unwrap();
        Command::new("git").args(["config", "user.name", "t"]).current_dir(&pusher).status().unwrap();
        std::fs::write(pusher.join("second.md"), "more").unwrap();
        Command::new("git").args(["add", "."]).current_dir(&pusher).status().unwrap();
        Command::new("git").args(["commit", "-q", "-m", "second"]).current_dir(&pusher).status().unwrap();
        Command::new("git").args(["push", "-q"]).current_dir(&pusher).status().unwrap();

        let updated_sha = update_plugin(&dest).unwrap();
        assert_eq!(updated_sha.len(), 40);
        assert_ne!(updated_sha, initial_sha);
    }
}
