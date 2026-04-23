# Plugin Agent Symlinks + Namespaced Resolution — Implementation Plan

**Goal:** Plugin-provided agents are discoverable via `~/.synaps-cli/agents/` symlinks and resolvable via `plugin:agent` syntax.
**Architecture:** Symlink management in install.rs, namespaced resolution in agent.rs, call-site wiring in actions.rs, BBE prompt updates.
**Design Doc:** `docs/plans/2026-04-23-plugin-agent-symlinks-design.md`
**Estimated Tasks:** 8 tasks
**Complexity:** Medium

---

### Task 1: Add `sync_plugin_agent_symlinks()` to `src/skills/install.rs`

**Files:**
- Modify: `src/skills/install.rs`

**Step 1: Write failing test**
```rust
#[test]
fn sync_plugin_agent_symlinks_creates_symlinks() {
    let tmp = tempfile::tempdir().unwrap();
    let plugin_dir = tmp.path().join("my-plugin");
    let agents_dir = plugin_dir.join("skills").join("bbe").join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    std::fs::write(
        agents_dir.join("sage.md"),
        "---\nname: bbe-sage\ndescription: test\n---\nYou are sage.",
    ).unwrap();

    let global_agents = tmp.path().join("agents");
    sync_plugin_agent_symlinks(&plugin_dir, &global_agents);

    let link = global_agents.join("bbe-sage.md");
    assert!(link.exists(), "symlink should exist");
    assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
    let content = std::fs::read_to_string(&link).unwrap();
    assert!(content.contains("You are sage."));
}
```

**Step 2: Verify it fails**
Run: `cargo test --lib sync_plugin_agent_symlinks_creates_symlinks`
Expected: FAIL — "cannot find function `sync_plugin_agent_symlinks`"

**Step 3: Implement**

Add to `src/skills/install.rs`:

```rust
/// Scan a plugin directory for agent .md files and create symlinks in the
/// global agents directory (~/.synaps-cli/agents/). Uses the frontmatter
/// `name` field as the symlink basename. Skips files without frontmatter
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
        if link_path.exists() && !link_path.symlink_metadata()
            .map(|m| m.file_type().is_symlink()).unwrap_or(false)
        {
            tracing::debug!("skipping agent symlink '{}': regular file exists", link_path.display());
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

/// Walk plugin_dir/skills/*/agents/*.md and return all .md paths found.
fn discover_plugin_agents(plugin_dir: &Path) -> Vec<std::path::PathBuf> {
    let mut result = Vec::new();
    let skills_dir = plugin_dir.join("skills");
    let Ok(entries) = std::fs::read_dir(&skills_dir) else { return result };
    for entry in entries.flatten() {
        let agents_dir = entry.path().join("agents");
        if !agents_dir.is_dir() { continue; }
        let Ok(agents) = std::fs::read_dir(&agents_dir) else { continue };
        for agent in agents.flatten() {
            let path = agent.path();
            if path.extension().and_then(|e| e.to_str()) == Some("md") {
                result.push(path);
            }
        }
    }
    result
}

/// Parse just the `name` field from YAML frontmatter.
fn parse_agent_frontmatter_name(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    if !content.starts_with("---") { return None; }
    let rest = content.strip_prefix("---")?;
    let end = rest.find("\n---")?;
    let frontmatter = &rest[..end];
    for line in frontmatter.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("name:") {
            let name = value.trim().trim_matches('"');
            if !name.is_empty() { return Some(name.to_string()); }
        }
    }
    None
}
```

**Step 4: Verify it passes**
Run: `cargo test --lib sync_plugin_agent_symlinks_creates_symlinks`
Expected: PASS

**Step 5: Commit**
```bash
git add -A && git commit -m "feat: add sync_plugin_agent_symlinks for plugin agent discovery"
```

---

### Task 2: Add `remove_plugin_agent_symlinks()` to `src/skills/install.rs`

**Files:**
- Modify: `src/skills/install.rs`

**Step 1: Write failing test**
```rust
#[test]
fn remove_plugin_agent_symlinks_removes_only_owned_links() {
    let tmp = tempfile::tempdir().unwrap();
    let plugin_dir = tmp.path().join("my-plugin");
    let agents_dir = plugin_dir.join("skills").join("bbe").join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    std::fs::write(
        agents_dir.join("sage.md"),
        "---\nname: bbe-sage\ndescription: test\n---\nYou are sage.",
    ).unwrap();

    let global_agents = tmp.path().join("agents");
    sync_plugin_agent_symlinks(&plugin_dir, &global_agents);
    assert!(global_agents.join("bbe-sage.md").exists());

    // Also create a regular file that should NOT be removed
    std::fs::write(global_agents.join("my-custom.md"), "custom").unwrap();

    remove_plugin_agent_symlinks(&plugin_dir, &global_agents);

    assert!(!global_agents.join("bbe-sage.md").exists(), "symlink should be removed");
    assert!(global_agents.join("my-custom.md").exists(), "regular file should remain");
}
```

**Step 2: Verify it fails**
Run: `cargo test --lib remove_plugin_agent_symlinks_removes_only`
Expected: FAIL — "cannot find function `remove_plugin_agent_symlinks`"

**Step 3: Implement**

```rust
/// Remove symlinks in the global agents directory that point into the given
/// plugin directory. Only removes symlinks, never regular files. Safe to
/// call even if the plugin is already partially deleted.
pub fn remove_plugin_agent_symlinks(plugin_dir: &Path, agents_dir: &Path) {
    let canonical_plugin = plugin_dir.canonicalize().unwrap_or_else(|_| plugin_dir.to_path_buf());
    for agent_path in discover_plugin_agents(plugin_dir) {
        let Some(name) = parse_agent_frontmatter_name(&agent_path) else { continue };
        let link_path = agents_dir.join(format!("{}.md", name));

        // Only remove if it's a symlink pointing into this plugin
        let Ok(meta) = link_path.symlink_metadata() else { continue };
        if !meta.file_type().is_symlink() { continue; }
        let Ok(target) = std::fs::read_link(&link_path) else { continue };
        let resolved = target.canonicalize().unwrap_or(target);
        if resolved.starts_with(&canonical_plugin) {
            let _ = std::fs::remove_file(&link_path);
        }
    }
}
```

**Step 4: Verify it passes**
Run: `cargo test --lib remove_plugin_agent_symlinks_removes_only`
Expected: PASS

**Step 5: Commit**
```bash
git add -A && git commit -m "feat: add remove_plugin_agent_symlinks for clean uninstall"
```

---

### Task 3: Additional edge-case tests for symlink functions

**Files:**
- Modify: `src/skills/install.rs`

**Step 1: Write tests**
```rust
#[test]
fn sync_does_not_clobber_regular_files() {
    let tmp = tempfile::tempdir().unwrap();
    let plugin_dir = tmp.path().join("my-plugin");
    let agents_dir = plugin_dir.join("skills").join("bbe").join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    std::fs::write(
        agents_dir.join("sage.md"),
        "---\nname: my-agent\ndescription: d\n---\nbody",
    ).unwrap();

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
    let plugin_dir = tmp.path().join("my-plugin");
    let agents_dir = plugin_dir.join("skills").join("bbe").join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    std::fs::write(agents_dir.join("noname.md"), "No frontmatter here").unwrap();

    let global_agents = tmp.path().join("agents");
    sync_plugin_agent_symlinks(&plugin_dir, &global_agents);

    // agents/ dir should be created but empty
    assert!(global_agents.exists());
    assert_eq!(std::fs::read_dir(&global_agents).unwrap().count(), 0);
}

#[test]
fn sync_replaces_stale_symlink() {
    let tmp = tempfile::tempdir().unwrap();
    let plugin_dir = tmp.path().join("my-plugin");
    let agents_dir = plugin_dir.join("skills").join("bbe").join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    std::fs::write(
        agents_dir.join("sage.md"),
        "---\nname: bbe-sage\ndescription: d\n---\nbody",
    ).unwrap();

    let global_agents = tmp.path().join("agents");
    std::fs::create_dir_all(&global_agents).unwrap();

    // Create a stale symlink pointing nowhere
    #[cfg(unix)]
    std::os::unix::fs::symlink("/nonexistent/path", global_agents.join("bbe-sage.md")).unwrap();

    sync_plugin_agent_symlinks(&plugin_dir, &global_agents);

    let link = global_agents.join("bbe-sage.md");
    assert!(link.exists(), "symlink should point to real file now");
    let content = std::fs::read_to_string(&link).unwrap();
    assert!(content.contains("body"));
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
```

**Step 2: Verify they fail**
Run: `cargo test --lib sync_does_not_clobber sync_skips sync_replaces parse_agent_frontmatter`
Expected: Tests should PASS (they test existing code from Tasks 1-2)

**Step 3: Verify they pass**
Run: `cargo test --lib -- install::tests`
Expected: All PASS

**Step 4: Commit**
```bash
git add -A && git commit -m "test: edge-case tests for plugin agent symlinks"
```

---

### Task 4: Wire symlinks into plugin install/update/uninstall actions

**Files:**
- Modify: `src/chatui/plugins/actions.rs`

**Step 1: Write failing test**

This is an integration-level wiring change. No isolated unit test — verified by the existing install/uninstall flows + manual test. We'll verify compilation and that the call sites reference the new functions.

**Step 2: Implement**

In `src/chatui/plugins/actions.rs`, add at the top:
```rust
use synaps_cli::skills::install::{sync_plugin_agent_symlinks, remove_plugin_agent_symlinks};
```

Add a helper:
```rust
fn global_agents_dir() -> PathBuf {
    synaps_cli::config::resolve_write_path("agents")
}
```

In `run_install_flow()`, after `reload_registry(registry, config);` (line 267):
```rust
    sync_plugin_agent_symlinks(&dest, &global_agents_dir());
```

In `apply_update()`, after `reload_registry(registry, config);` (line 360):
```rust
    sync_plugin_agent_symlinks(&dir, &global_agents_dir());
```

In `apply_uninstall()`, BEFORE the `spawn_blocking(|| uninstall_plugin(...))` call (before line 285):
```rust
    remove_plugin_agent_symlinks(&dir, &global_agents_dir());
```

In `apply_remove_marketplace()`, inside the loop BEFORE each uninstall (before `spawn_blocking`):
```rust
    remove_plugin_agent_symlinks(&dir, &global_agents_dir());
```

**Step 3: Verify it compiles**
Run: `cargo build`
Expected: Compiles cleanly

**Step 4: Commit**
```bash
git add -A && git commit -m "feat: wire plugin agent symlinks into install/update/uninstall"
```

---

### Task 5: Add `plugin:agent` namespaced resolution to `resolve_agent_prompt()`

**Files:**
- Modify: `src/tools/agent.rs`

**Step 1: Write failing test**
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_namespaced_agent_finds_plugin_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let agents_dir = tmp.path().join("my-plugin").join("skills").join("bbe").join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("sage.md"),
            "---\nname: bbe-sage\ndescription: d\n---\nYou are sage.",
        ).unwrap();

        let result = resolve_namespaced_agent("sage", tmp.path().join("my-plugin"));
        assert!(result.is_ok());
        assert!(result.unwrap().contains("You are sage."));
    }

    #[test]
    fn resolve_namespaced_agent_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_dir = tmp.path().join("my-plugin");
        std::fs::create_dir_all(&plugin_dir).unwrap();

        let result = resolve_namespaced_agent("nonexistent", plugin_dir);
        assert!(result.is_err());
    }

    #[test]
    fn strip_frontmatter_removes_yaml_header() {
        let input = "---\nname: x\n---\nBody text";
        assert_eq!(strip_frontmatter(input), "Body text");
    }

    #[test]
    fn strip_frontmatter_passes_through_plain_text() {
        assert_eq!(strip_frontmatter("Just text"), "Just text");
    }
}
```

**Step 2: Verify it fails**
Run: `cargo test --lib resolve_namespaced_agent`
Expected: FAIL — "cannot find function `resolve_namespaced_agent`"

**Step 3: Implement**

Update `resolve_agent_prompt()` in `src/tools/agent.rs`:

```rust
pub fn resolve_agent_prompt(name: &str) -> std::result::Result<String, String> {
    // 1. File path — name contains '/'
    if name.contains('/') {
        let path = expand_path(name);
        let content = std::fs::read_to_string(&path)
            .map_err(|e| format!("Failed to read agent file '{}': {}", path.display(), e))?;
        return Ok(strip_frontmatter(&content));
    }

    // 2. Namespaced — "plugin:agent" syntax
    if let Some((plugin, agent)) = name.split_once(':') {
        let plugins_dir = crate::config::base_dir().join("plugins");
        let plugin_dir = plugins_dir.join(plugin);
        if !plugin_dir.is_dir() {
            return Err(format!(
                "Plugin '{}' not found at {}",
                plugin, plugin_dir.display()
            ));
        }
        return resolve_namespaced_agent(agent, plugin_dir);
    }

    // 3. Bare name — ~/.synaps-cli/agents/<name>.md
    let agents_dir = crate::config::base_dir().join("agents");
    let agent_path = agents_dir.join(format!("{}.md", name));

    if agent_path.exists() {
        let content = std::fs::read_to_string(&agent_path)
            .map_err(|e| format!("Failed to read agent '{}': {}", agent_path.display(), e))?;
        return Ok(strip_frontmatter(&content));
    }

    Err(format!(
        "Agent '{}' not found. Searched:\n  - {}\nCreate the file or pass a system_prompt directly.",
        name, agent_path.display()
    ))
}

/// Search plugin_dir/skills/*/agents/<agent>.md for a matching agent file.
fn resolve_namespaced_agent(agent: &str, plugin_dir: std::path::PathBuf) -> std::result::Result<String, String> {
    let skills_dir = plugin_dir.join("skills");
    let Ok(entries) = std::fs::read_dir(&skills_dir) else {
        return Err(format!("No skills directory in plugin at {}", plugin_dir.display()));
    };
    for entry in entries.flatten() {
        let agent_path = entry.path().join("agents").join(format!("{}.md", agent));
        if agent_path.exists() {
            let content = std::fs::read_to_string(&agent_path)
                .map_err(|e| format!("Failed to read agent '{}': {}", agent_path.display(), e))?;
            return Ok(strip_frontmatter(&content));
        }
    }
    Err(format!(
        "Agent '{}' not found in plugin at {}. Searched skills/*/agents/{}.md",
        agent, plugin_dir.display(), agent
    ))
}
```

**Step 4: Verify it passes**
Run: `cargo test --lib -- agent::tests`
Expected: PASS

**Step 5: Commit**
```bash
git add -A && git commit -m "feat: add plugin:agent namespaced resolution to resolve_agent_prompt"
```

---

### Task 6: Sync existing plugins on startup

**Files:**
- Modify: `src/skills/mod.rs`

**Step 1: Implement**

In `src/skills/mod.rs`, in the `register()` function, after loading plugins/skills, sync agent symlinks for all discovered plugins:

```rust
    // Sync agent symlinks for all discovered plugins
    let global_agents_dir = crate::config::resolve_write_path("agents");
    for plugin in &plugins {
        install::sync_plugin_agent_symlinks(&plugin.root, &global_agents_dir);
    }
```

This ensures symlinks exist even if the plugin was installed before this feature was added.

**Step 2: Verify it compiles and passes**
Run: `cargo build && cargo test --lib`
Expected: Compiles, all tests pass

**Step 3: Commit**
```bash
git add -A && git commit -m "feat: sync plugin agent symlinks on startup"
```

---

### Task 7: Update BBE SKILL.md agent references

**Files:**
- Modify: `~/.synaps-cli/plugins/dev-tools/skills/black-box-engineering/SKILL.md`

**Step 1: Implement**

Replace all occurrences of `<skill_dir>/agents/sage.md` etc. with bare names:

```
agent: "<skill_dir>/agents/sage.md"   →  agent: "bbe-sage"
agent: "<skill_dir>/agents/quinn.md"  →  agent: "bbe-quinn"
agent: "<skill_dir>/agents/glitch.md" →  agent: "bbe-glitch"
agent: "<skill_dir>/agents/arbiter.md" → agent: "bbe-arbiter"
```

**Step 2: Verify**
Run: `grep -n 'skill_dir.*agents' ~/.synaps-cli/plugins/dev-tools/skills/black-box-engineering/SKILL.md`
Expected: No matches

**Step 3: Commit** (in the plugin repo, not this repo)

---

### Task 8: Update BBE orchestrator.md agent references

**Files:**
- Modify: `~/.synaps-cli/plugins/dev-tools/skills/black-box-engineering/agents/orchestrator.md`

**Step 1: Implement**

Replace all occurrences of `<skill_dir>/agents/<agent>.md` with bare names:

```
agent: "<skill_dir>/agents/sage.md"    → agent: "bbe-sage"
agent: "<skill_dir>/agents/quinn.md"   → agent: "bbe-quinn"
agent: "<skill_dir>/agents/glitch.md"  → agent: "bbe-glitch"
agent: "<skill_dir>/agents/arbiter.md" → agent: "bbe-arbiter"
agent: "<skill_dir>/agents/<agent>.md" → agent: "bbe-<agent>"
```

Also update `run-pipeline.sh` to not pass `skill_dir` as a task parameter (it's no longer needed for agent resolution).

**Step 2: Verify**
Run: `grep -n 'skill_dir.*agents' ~/.synaps-cli/plugins/dev-tools/skills/black-box-engineering/agents/orchestrator.md`
Expected: No matches

**Step 3: Commit** (in the plugin repo, not this repo)
