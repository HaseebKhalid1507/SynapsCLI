//! Git-backed plugin install/uninstall/update.

use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

/// Shared git clone logic. Validates URL, ensures dest parent exists,
/// runs `git clone --depth=1`. Returns Ok(()) on success, cleans up on failure.
fn clone_repo(source_url: &str, dest: &Path) -> Result<(), String> {
    if source_url.starts_with('-') {
        return Err(format!("refusing suspicious url: {}", source_url));
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
    Ok(())
}

/// `git clone --depth=1 <url> <dest>`, then `git rev-parse HEAD`.
/// `dest` must not already exist.
pub fn install_plugin(source_url: &str, dest: &Path) -> Result<String, String> {
    if dest.exists() {
        return Err(format!("{} already exists on disk; uninstall first", dest.display()));
    }
    clone_repo(source_url, dest)?;
    rev_parse_head(dest)
}

/// Shallow-clone `marketplace_url` into a temp dir sibling to `dest`, then
/// move its `<subdir>` directly into place at `dest`. Returns the HEAD SHA
/// of the cloned marketplace. Used for Claude-Code-style marketplaces whose
/// plugins reference `./<subdir>` instead of their own standalone repos.
///
/// Guarantees:
/// - `subdir` must pass [`crate::skills::marketplace::is_safe_plugin_name`]
///   (no traversal, no path separators).
/// - `dest` must not exist.
/// - If the subdir doesn't exist inside the cloned repo, returns `Err` and
///   does not create `dest`.
pub fn install_plugin_from_subdir(
    marketplace_url: &str,
    subdir: &str,
    dest: &Path,
) -> Result<String, String> {
    if !crate::skills::marketplace::is_safe_plugin_name(subdir) {
        return Err(format!("refusing unsafe subdir name: {}", subdir));
    }
    if dest.exists() {
        return Err(format!("{} already exists on disk; uninstall first", dest.display()));
    }
    let parent = dest.parent().ok_or_else(|| "dest has no parent directory".to_string())?;
    let dest_name = dest.file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| "dest file name is not utf-8".to_string())?;
    let tmp = parent.join(format!(".{}-clone-tmp", dest_name));
    // Clean any stale temp from a prior aborted install.
    let _ = std::fs::remove_dir_all(&tmp);

    clone_repo(marketplace_url, &tmp)?;

    let sha = match rev_parse_head(&tmp) {
        Ok(s) => s,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(e);
        }
    };

    let src_subdir = tmp.join(subdir);
    if !src_subdir.is_dir() {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(format!("subdir '{}' not found in marketplace repo", subdir));
    }

    // Prefer rename (fast, same-filesystem); fall back to recursive copy.
    if std::fs::rename(&src_subdir, dest).is_err() {
        copy_dir_all(&src_subdir, dest).map_err(|e| {
            let _ = std::fs::remove_dir_all(&tmp);
            format!("copy {} to {}: {}", src_subdir.display(), dest.display(), e)
        })?;
    }
    let _ = std::fs::remove_dir_all(&tmp);
    Ok(sha)
}

fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dst_path = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &dst_path)?;
        } else if ty.is_file() {
            std::fs::copy(entry.path(), dst_path)?;
        }
        // Symlinks and other types are skipped intentionally.
    }
    Ok(())
}

/// `rm -rf <path>`. Missing path is OK.
pub fn uninstall_plugin(path: &Path) -> Result<(), String> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("remove {}: {}", path.display(), e)),
    }
}

/// Compute the plugin package checksum used by v1 plugin indexes.
///
/// The digest is sha256 over each regular file below `path` (excluding `.git`),
/// in lexical relative-path order. Each file contributes its relative path,
/// a NUL separator, its bytes, and another NUL. This makes the checksum stable
/// across machines while detecting file rename/content changes. Symlinks and
/// non-regular files are ignored, matching installer snapshot behavior.
pub fn plugin_dir_sha256(path: &Path) -> Result<String, String> {
    if !path.is_dir() {
        return Err(format!("{} is not a directory", path.display()));
    }
    let effective_root = path.join(".synaps-plugin").join("plugin.json");
    if effective_root.is_file() {
        hash_regular_files(path)
    } else {
        let mut candidates = Vec::new();
        collect_plugin_roots(path, path, &mut candidates)?;
        candidates.sort();
        if candidates.len() == 1 {
            hash_regular_files(&candidates[0])
        } else {
            hash_regular_files(path)
        }
    }
}

fn hash_regular_files(path: &Path) -> Result<String, String> {
    let mut files = Vec::new();
    collect_regular_files(path, path, &mut files)?;
    files.sort();

    let mut hasher = Sha256::new();
    for rel in files {
        let full = path.join(&rel);
        hasher.update(rel.to_string_lossy().as_bytes());
        hasher.update([0]);
        let bytes = std::fs::read(&full)
            .map_err(|e| format!("read {}: {}", full.display(), e))?;
        hasher.update(bytes);
        hasher.update([0]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

pub fn verify_plugin_dir_checksum(path: &Path, algorithm: &str, expected: &str) -> Result<(), String> {
    if algorithm != "sha256" {
        return Err(format!("unsupported plugin checksum algorithm: {}", algorithm));
    }
    let actual = plugin_dir_sha256(path)?;
    if actual != expected {
        return Err(format!(
            "plugin checksum mismatch: expected sha256:{}, got sha256:{}",
            expected, actual
        ));
    }
    Ok(())
}

fn collect_plugin_roots(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    for entry in std::fs::read_dir(dir).map_err(|e| format!("read dir {}: {}", dir.display(), e))? {
        let entry = entry.map_err(|e| format!("read dir {}: {}", dir.display(), e))?;
        let path = entry.path();
        if entry.file_name().to_string_lossy() == ".git" {
            continue;
        }
        let ty = entry.file_type().map_err(|e| format!("stat {}: {}", path.display(), e))?;
        if ty.is_dir() {
            if path.join(".synaps-plugin").join("plugin.json").is_file() && path != root {
                out.push(path);
            } else {
                collect_plugin_roots(root, &path, out)?;
            }
        }
    }
    Ok(())
}

fn collect_regular_files(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    for entry in std::fs::read_dir(dir).map_err(|e| format!("read dir {}: {}", dir.display(), e))? {
        let entry = entry.map_err(|e| format!("read dir {}: {}", dir.display(), e))?;
        let path = entry.path();
        let name = entry.file_name();
        if name.to_string_lossy() == ".git" {
            continue;
        }
        let ty = entry
            .file_type()
            .map_err(|e| format!("stat {}: {}", path.display(), e))?;
        if ty.is_dir() {
            collect_regular_files(root, &path, out)?;
        } else if ty.is_file() {
            let rel = path
                .strip_prefix(root)
                .map_err(|e| format!("strip prefix {}: {}", path.display(), e))?
                .to_path_buf();
            out.push(rel);
        }
    }
    Ok(())
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

    /// Like `mk_local_repo`, but puts the plugin content under `work/<sub>/`
    /// so the bare clone can be snapshotted via `install_plugin_from_subdir`.
    fn mk_local_repo_with_subdir(sub: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("work");
        std::fs::create_dir_all(work.join(sub)).unwrap();
        Command::new("git").args(["init", "-q"]).current_dir(&work).status().unwrap();
        Command::new("git").args(["config", "user.email", "t@t"]).current_dir(&work).status().unwrap();
        Command::new("git").args(["config", "user.name", "t"]).current_dir(&work).status().unwrap();
        std::fs::write(
            work.join(sub).join("SKILL.md"),
            "---\nname: demo\ndescription: d\n---\nbody",
        ).unwrap();
        std::fs::write(work.join("README.md"), "top level").unwrap();
        Command::new("git").args(["add", "."]).current_dir(&work).status().unwrap();
        Command::new("git").args(["commit", "-q", "-m", "init"]).current_dir(&work).status().unwrap();

        let bare = dir.path().join("bare.git");
        Command::new("git").args(["clone", "--bare", "-q",
            work.to_str().unwrap(), bare.to_str().unwrap()]).status().unwrap();
        (dir, bare)
    }

    #[test]
    fn install_plugin_from_subdir_snapshots_subdir_content() {
        let (_tmp, bare) = mk_local_repo_with_subdir("web");
        let dest_parent = tempfile::tempdir().unwrap();
        let dest = dest_parent.path().join("web");
        let sha = install_plugin_from_subdir(
            &format!("file://{}", bare.display()),
            "web",
            &dest,
        ).unwrap();
        assert_eq!(sha.len(), 40);
        // Subdir contents landed directly at dest.
        assert!(dest.join("SKILL.md").exists());
        // README.md from the parent repo was NOT copied in.
        assert!(!dest.join("README.md").exists());
        // No leftover temp clone.
        let tmp_leftover = dest_parent.path().join(".web-clone-tmp");
        assert!(!tmp_leftover.exists());
    }

    #[test]
    fn install_plugin_from_subdir_rejects_unsafe_subdir() {
        let (_tmp, bare) = mk_local_repo_with_subdir("web");
        let dest_parent = tempfile::tempdir().unwrap();
        let dest = dest_parent.path().join("web");
        let err = install_plugin_from_subdir(
            &format!("file://{}", bare.display()),
            "../evil",
            &dest,
        ).unwrap_err();
        assert!(err.contains("unsafe"));
        assert!(!dest.exists());
    }

    #[test]
    fn install_plugin_from_subdir_fails_when_subdir_missing() {
        let (_tmp, bare) = mk_local_repo_with_subdir("web");
        let dest_parent = tempfile::tempdir().unwrap();
        let dest = dest_parent.path().join("nope");
        let err = install_plugin_from_subdir(
            &format!("file://{}", bare.display()),
            "nope",
            &dest,
        ).unwrap_err();
        assert!(err.contains("not found"));
        assert!(!dest.exists());
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
    fn checksum_ignores_git_and_detects_content_changes() {
        let dir = tempfile::tempdir().unwrap();
        let plugin = dir.path().join("demo");
        std::fs::create_dir_all(plugin.join(".synaps-plugin")).unwrap();
        std::fs::create_dir_all(plugin.join(".git")).unwrap();
        std::fs::write(plugin.join(".synaps-plugin/plugin.json"), "{}").unwrap();
        std::fs::write(plugin.join("README.md"), "one").unwrap();
        std::fs::write(plugin.join(".git/HEAD"), "ignored").unwrap();

        let first = plugin_dir_sha256(&plugin).unwrap();
        assert_eq!(first.len(), 64);
        verify_plugin_dir_checksum(&plugin, "sha256", &first).unwrap();

        std::fs::write(plugin.join(".git/HEAD"), "still ignored").unwrap();
        assert_eq!(plugin_dir_sha256(&plugin).unwrap(), first);

        std::fs::write(plugin.join("README.md"), "two").unwrap();
        let second = plugin_dir_sha256(&plugin).unwrap();
        assert_ne!(second, first);
        let err = verify_plugin_dir_checksum(&plugin, "sha256", &first).unwrap_err();
        assert!(err.contains("checksum mismatch"));
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
