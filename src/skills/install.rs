//! Git-backed plugin install/uninstall/update.

use std::path::Path;
use std::process::Command;


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

// ── Plugin agent symlinks ────────────────────────────────────────────────────

/// Scan a plugin directory for agent `.md` files and create symlinks in the
/// global agents directory (`~/.synaps-cli/agents/`). Uses the frontmatter
/// `name` field as the symlink basename. Skips files without a frontmatter
/// name, and never clobbers regular (non-symlink) files.
pub fn sync_plugin_agent_symlinks(plugin_dir: &Path, agents_dir: &Path) {
    let _ = std::fs::create_dir_all(agents_dir);
    for agent_path in discover_plugin_agents(plugin_dir) {
        let Some(name) = parse_agent_frontmatter_name(&agent_path) else { continue };
        let link_path = agents_dir.join(format!("{}.md", name));
        let abs_target = match agent_path.canonicalize() {
            Ok(p) => p,
            Err(_) => continue,
        };

        // Don't clobber user-owned regular files
        if link_path.symlink_metadata().is_ok()
            && !link_path
                .symlink_metadata()
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false)
        {
            tracing::debug!(
                "skipping agent symlink '{}': regular file exists",
                link_path.display()
            );
            continue;
        }

        // Remove existing symlink (idempotent update)
        let _ = std::fs::remove_file(&link_path);

        #[cfg(unix)]
        {
            if let Err(e) = std::os::unix::fs::symlink(&abs_target, &link_path) {
                tracing::warn!("failed to symlink agent '{}': {}", name, e);
            }
        }
    }
}

/// Remove symlinks in the global agents directory that point into the given
/// plugin directory. Only removes symlinks, never regular files. Safe to
/// call even if the plugin is already partially deleted.
pub fn remove_plugin_agent_symlinks(plugin_dir: &Path, agents_dir: &Path) {
    let canonical_plugin = plugin_dir
        .canonicalize()
        .unwrap_or_else(|_| plugin_dir.to_path_buf());
    for agent_path in discover_plugin_agents(plugin_dir) {
        let Some(name) = parse_agent_frontmatter_name(&agent_path) else { continue };
        let link_path = agents_dir.join(format!("{}.md", name));

        // Only remove if it's a symlink pointing into this plugin
        let Ok(meta) = link_path.symlink_metadata() else { continue };
        if !meta.file_type().is_symlink() {
            continue;
        }
        let Ok(target) = std::fs::read_link(&link_path) else { continue };
        let resolved = target.canonicalize().unwrap_or(target);
        if resolved.starts_with(&canonical_plugin) {
            let _ = std::fs::remove_file(&link_path);
        }
    }
}

/// Walk `plugin_dir/skills/*/agents/*.md` and return all `.md` paths found.
fn discover_plugin_agents(plugin_dir: &Path) -> Vec<std::path::PathBuf> {
    let mut result = Vec::new();
    let skills_dir = plugin_dir.join("skills");
    let Ok(entries) = std::fs::read_dir(&skills_dir) else {
        return result;
    };
    for entry in entries.flatten() {
        let agents_dir = entry.path().join("agents");
        if !agents_dir.is_dir() {
            continue;
        }
        let Ok(agents) = std::fs::read_dir(&agents_dir) else {
            continue;
        };
        for agent in agents.flatten() {
            let path = agent.path();
            if path.extension().and_then(|e| e.to_str()) == Some("md") {
                result.push(path);
            }
        }
    }
    result
}

/// Parse just the `name` field from YAML frontmatter of an agent file.
fn parse_agent_frontmatter_name(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    if !content.starts_with("---") {
        return None;
    }
    let rest = content.strip_prefix("---")?;
    let end = rest.find("\n---")?;
    let frontmatter = &rest[..end];
    for line in frontmatter.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("name:") {
            let name = value.trim().trim_matches('"');
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    None
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

    // ── Agent symlink tests ───────────────────────────────────���──────────────

    /// Helper: create a plugin dir with an agent file under skills/<skill>/agents/.
    fn mk_plugin_with_agent(
        root: &std::path::Path,
        plugin: &str,
        skill: &str,
        agent_file: &str,
        agent_content: &str,
    ) -> std::path::PathBuf {
        let plugin_dir = root.join(plugin);
        let agents_dir = plugin_dir.join("skills").join(skill).join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(agents_dir.join(agent_file), agent_content).unwrap();
        plugin_dir
    }

    #[test]
    fn sync_plugin_agent_symlinks_creates_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_dir = mk_plugin_with_agent(
            tmp.path(), "my-plugin", "bbe", "sage.md",
            "---\nname: bbe-sage\ndescription: test\n---\nYou are sage.",
        );

        let global_agents = tmp.path().join("agents");
        sync_plugin_agent_symlinks(&plugin_dir, &global_agents);

        let link = global_agents.join("bbe-sage.md");
        assert!(link.exists(), "symlink should exist");
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        let content = std::fs::read_to_string(&link).unwrap();
        assert!(content.contains("You are sage."));
    }

    #[test]
    fn remove_plugin_agent_symlinks_removes_only_owned_links() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_dir = mk_plugin_with_agent(
            tmp.path(), "my-plugin", "bbe", "sage.md",
            "---\nname: bbe-sage\ndescription: test\n---\nYou are sage.",
        );

        let global_agents = tmp.path().join("agents");
        sync_plugin_agent_symlinks(&plugin_dir, &global_agents);
        assert!(global_agents.join("bbe-sage.md").exists());

        // Also create a regular file that should NOT be removed
        std::fs::write(global_agents.join("my-custom.md"), "custom").unwrap();

        remove_plugin_agent_symlinks(&plugin_dir, &global_agents);

        assert!(!global_agents.join("bbe-sage.md").exists(), "symlink should be removed");
        assert!(global_agents.join("my-custom.md").exists(), "regular file should remain");
    }

    #[test]
    fn sync_does_not_clobber_regular_files() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_dir = mk_plugin_with_agent(
            tmp.path(), "my-plugin", "bbe", "sage.md",
            "---\nname: my-agent\ndescription: d\n---\nbody",
        );

        let global_agents = tmp.path().join("agents");
        std::fs::create_dir_all(&global_agents).unwrap();
        std::fs::write(global_agents.join("my-agent.md"), "user content").unwrap();

        sync_plugin_agent_symlinks(&plugin_dir, &global_agents);

        // Regular file should remain untouched
        let content = std::fs::read_to_string(global_agents.join("my-agent.md")).unwrap();
        assert_eq!(content, "user content");
    }

    #[test]
    fn sync_skips_agents_without_frontmatter_name() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_dir = mk_plugin_with_agent(
            tmp.path(), "my-plugin", "bbe", "noname.md",
            "No frontmatter here",
        );

        let global_agents = tmp.path().join("agents");
        sync_plugin_agent_symlinks(&plugin_dir, &global_agents);

        // agents/ dir should be created but empty
        assert!(global_agents.exists());
        assert_eq!(std::fs::read_dir(&global_agents).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn sync_replaces_stale_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_dir = mk_plugin_with_agent(
            tmp.path(), "my-plugin", "bbe", "sage.md",
            "---\nname: bbe-sage\ndescription: d\n---\nbody",
        );

        let global_agents = tmp.path().join("agents");
        std::fs::create_dir_all(&global_agents).unwrap();

        // Create a stale symlink pointing nowhere
        std::os::unix::fs::symlink("/nonexistent/path", global_agents.join("bbe-sage.md")).unwrap();

        sync_plugin_agent_symlinks(&plugin_dir, &global_agents);

        let link = global_agents.join("bbe-sage.md");
        assert!(link.exists(), "symlink should point to real file now");
        let content = std::fs::read_to_string(&link).unwrap();
        assert!(content.contains("body"));
    }

    #[test]
    fn sync_handles_multiple_skills_with_agents() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_dir = tmp.path().join("my-plugin");

        // Two skills each with agents
        let agents1 = plugin_dir.join("skills").join("bbe").join("agents");
        std::fs::create_dir_all(&agents1).unwrap();
        std::fs::write(agents1.join("sage.md"),
            "---\nname: bbe-sage\ndescription: d\n---\nsage body").unwrap();
        std::fs::write(agents1.join("quinn.md"),
            "---\nname: bbe-quinn\ndescription: d\n---\nquinn body").unwrap();

        let agents2 = plugin_dir.join("skills").join("other").join("agents");
        std::fs::create_dir_all(&agents2).unwrap();
        std::fs::write(agents2.join("helper.md"),
            "---\nname: other-helper\ndescription: d\n---\nhelper body").unwrap();

        let global_agents = tmp.path().join("agents");
        sync_plugin_agent_symlinks(&plugin_dir, &global_agents);

        assert!(global_agents.join("bbe-sage.md").exists());
        assert!(global_agents.join("bbe-quinn.md").exists());
        assert!(global_agents.join("other-helper.md").exists());
    }

    #[test]
    fn parse_agent_frontmatter_name_works() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.md");
        std::fs::write(&path, "---\nname: my-agent\ndescription: d\n---\nbody").unwrap();
        assert_eq!(parse_agent_frontmatter_name(&path), Some("my-agent".to_string()));
    }

    #[test]
    fn parse_agent_frontmatter_name_returns_none_without_name() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.md");
        std::fs::write(&path, "Just some text").unwrap();
        assert_eq!(parse_agent_frontmatter_name(&path), None);
    }

    #[test]
    fn parse_agent_frontmatter_name_returns_none_for_empty_name() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.md");
        std::fs::write(&path, "---\nname: \ndescription: d\n---\nbody").unwrap();
        assert_eq!(parse_agent_frontmatter_name(&path), None);
    }

    #[test]
    fn discover_plugin_agents_finds_md_files() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_dir = mk_plugin_with_agent(
            tmp.path(), "p", "s", "a.md", "content",
        );
        // Also add a non-md file that should be ignored
        let agents_dir = plugin_dir.join("skills").join("s").join("agents");
        std::fs::write(agents_dir.join("readme.txt"), "ignore").unwrap();

        let found = discover_plugin_agents(&plugin_dir);
        assert_eq!(found.len(), 1);
        assert!(found[0].ends_with("a.md"));
    }
}
