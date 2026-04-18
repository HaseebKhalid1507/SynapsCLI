# Synaps Skills & Plugins Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Replace the flat-`.md` skills loader with a plugin-based system that discovers `.synaps-plugin/` manifests, registers each skill as a dynamic slash command, and unifies user-initiated and model-initiated skill loading.

**Architecture:** New `src/skills/` module tree (`manifest`, `loader`, `config`, `registry`, `tool`) replaces the single-file `src/skills.rs`. Skills are discovered by walking `.synaps-cli/plugins/` and `.synaps-cli/skills/` (project-local and global), exposed via a `CommandRegistry` that dispatches `/skill-name` through a synthesized `load_skill` tool-result so both paths share one codepath.

**Tech Stack:** Rust 1.80+, `serde_json` for manifest parsing, existing `tokio`/`anyhow`/`tracing`/`async-trait`. No new dependencies.

**Design doc:** `docs/plans/2026-04-18-synaps-skills-plugins-design.md`

**Working in:** worktree at `.worktrees/skills-plugins/`, branch `feature/skills-plugins`.

---

## Baseline

Before starting: `cargo test 2>&1 | grep "^test result"` should show 98 passing, 0 failing.

---

## Task 1: Scaffold new module tree, preserve legacy

**Goal:** Convert `src/skills.rs` (single file) to `src/skills/` (module dir) without changing any behavior. Legacy code moves to `src/skills/legacy.rs`; new empty submodules added. Build + tests still green.

**Files:**
- Delete: `src/skills.rs`
- Create: `src/skills/mod.rs`
- Create: `src/skills/legacy.rs` (content = old `src/skills.rs`)
- Create: `src/skills/manifest.rs` (empty)
- Create: `src/skills/loader.rs` (empty)
- Create: `src/skills/config.rs` (empty)
- Create: `src/skills/registry.rs` (empty)
- Create: `src/skills/tool.rs` (empty)

**Step 1:** Create `src/skills/` directory.

```bash
mkdir src/skills
```

**Step 2:** Move existing `src/skills.rs` content into `src/skills/legacy.rs` verbatim (same code, same tests).

```bash
mv src/skills.rs src/skills/legacy.rs
```

**Step 3:** Write `src/skills/mod.rs`:

```rust
//! Skills and plugins subsystem.
//!
//! Legacy flat-.md loader currently lives in `legacy`; new plugin-based
//! submodules will be built in `manifest`, `loader`, `config`, `registry`,
//! `tool` and eventually supersede it.

mod legacy;
pub mod manifest;
pub mod loader;
pub mod config;
pub mod registry;
pub mod tool;

// Re-export legacy API so existing callers (chatui/main.rs) keep compiling.
pub use legacy::{Skill, load_skills, format_skills_for_prompt, parse_skills_config, setup_skill_tool};
```

**Step 4:** Create empty submodule files.

```bash
: > src/skills/manifest.rs
: > src/skills/loader.rs
: > src/skills/config.rs
: > src/skills/registry.rs
: > src/skills/tool.rs
```

**Step 5:** Run tests to verify the move is a no-op.

```bash
cargo test 2>&1 | grep "^test result"
```

Expected: same 98 passing, 0 failing.

**Step 6:** Commit.

```bash
git add -A && git commit -m "refactor: move src/skills.rs into src/skills/ module tree (legacy preserved)"
```

---

## Task 2: Manifest parsing — `PluginManifest`

**Goal:** Parse `.synaps-plugin/plugin.json`. Required field: `name`. Optional: `version`, `description`, `author`, `repository`, `license`. Unknown fields ignored (forward-compat).

**Files:**
- Modify: `src/skills/manifest.rs`

**Step 1:** Write failing tests in `src/skills/manifest.rs`:

```rust
//! Parse .synaps-plugin/plugin.json and .synaps-plugin/marketplace.json.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_manifest_minimal() {
        let json = r#"{"name":"web-tools"}"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.name, "web-tools");
        assert_eq!(m.version, None);
        assert_eq!(m.description, None);
    }

    #[test]
    fn plugin_manifest_full_with_extras() {
        let json = r#"{
            "name": "web-tools",
            "version": "1.0.0",
            "description": "Web tools",
            "author": {"name": "x"},
            "repository": "https://...",
            "license": "MIT",
            "unknown_field": 42
        }"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.name, "web-tools");
        assert_eq!(m.version.as_deref(), Some("1.0.0"));
        assert_eq!(m.description.as_deref(), Some("Web tools"));
    }

    #[test]
    fn plugin_manifest_missing_name_fails() {
        let json = r#"{"version":"1.0.0"}"#;
        let result: Result<PluginManifest, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }
}
```

**Step 2:** Run tests — should now PASS (the struct is already written above).

```bash
cargo test --lib skills::manifest 2>&1 | grep "^test result"
```

Expected: 3 passed.

**Step 3:** Commit.

```bash
git add src/skills/manifest.rs && git commit -m "feat: add PluginManifest parser"
```

---

## Task 3: Manifest parsing — `MarketplaceManifest`

**Goal:** Parse `.synaps-plugin/marketplace.json`. Lists plugins with relative `source` paths.

**Files:**
- Modify: `src/skills/manifest.rs`

**Step 1:** Append to `src/skills/manifest.rs`:

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct MarketplaceManifest {
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    pub plugins: Vec<MarketplacePluginEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MarketplacePluginEntry {
    pub name: String,
    pub source: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}
```

Append tests:

```rust
    #[test]
    fn marketplace_manifest_basic() {
        let json = r#"{
            "name": "pi-skills",
            "plugins": [
                {"name": "web-tools", "source": "./web-tools-plugin"},
                {"name": "dev-tools", "source": "./dev-tools", "version": "2.0.0"}
            ]
        }"#;
        let m: MarketplaceManifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.name, "pi-skills");
        assert_eq!(m.plugins.len(), 2);
        assert_eq!(m.plugins[0].name, "web-tools");
        assert_eq!(m.plugins[0].source, "./web-tools-plugin");
    }

    #[test]
    fn marketplace_manifest_missing_plugins_fails() {
        let json = r#"{"name":"empty"}"#;
        let result: Result<MarketplaceManifest, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn marketplace_entry_missing_source_fails() {
        let json = r#"{"name":"p","plugins":[{"name":"x"}]}"#;
        let result: Result<MarketplaceManifest, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }
```

**Step 2:** Run tests.

```bash
cargo test --lib skills::manifest 2>&1 | grep "^test result"
```

Expected: 6 passed.

**Step 3:** Commit.

```bash
git add src/skills/manifest.rs && git commit -m "feat: add MarketplaceManifest parser"
```

---

## Task 4: New `Skill` and `Plugin` data structures

**Goal:** Define the runtime representation used by the new loader. Deliberately different from `legacy::Skill` so the old flat loader keeps working until migration.

**Files:**
- Modify: `src/skills/mod.rs`

**Step 1:** Append to `src/skills/mod.rs` (after the existing re-exports):

```rust
use std::path::PathBuf;

/// A plugin discovered during skill loading.
#[derive(Debug, Clone)]
pub struct Plugin {
    pub name: String,
    pub root: PathBuf,
    pub marketplace: Option<String>,
    pub version: Option<String>,
    pub description: Option<String>,
}

/// A skill discovered during loading. Renamed `LoadedSkill` temporarily
/// to avoid clashing with the re-exported `legacy::Skill`; the legacy
/// alias will be removed in the final migration task.
#[derive(Debug, Clone)]
pub struct LoadedSkill {
    pub name: String,
    pub description: String,
    pub body: String,           // post-{baseDir} substitution
    pub plugin: Option<String>, // None for loose skills
    pub base_dir: PathBuf,      // absolute
    pub source_path: PathBuf,   // absolute path to SKILL.md
}
```

**Step 2:** Verify it compiles.

```bash
cargo build 2>&1 | tail -5
```

Expected: clean build.

**Step 3:** Commit.

```bash
git add src/skills/mod.rs && git commit -m "feat: add Plugin and LoadedSkill data structures"
```

---

## Task 5: Frontmatter + `{baseDir}` loader

**Goal:** Read one `SKILL.md`, parse frontmatter, substitute `{baseDir}`, return `LoadedSkill`. Port `parse_frontmatter` from `legacy.rs` (same logic, new location).

**Files:**
- Modify: `src/skills/loader.rs`

**Step 1:** Write failing tests in `src/skills/loader.rs`:

```rust
//! SKILL.md parsing, {baseDir} substitution, and filesystem discovery.

use std::path::{Path, PathBuf};
use crate::skills::LoadedSkill;

/// Parse YAML frontmatter from a markdown file.
/// Returns (frontmatter_fields, body).
pub(super) fn parse_frontmatter(text: &str) -> (Vec<(String, String)>, String) {
    if !text.starts_with("---") {
        return (vec![], text.to_string());
    }
    if let Some(end) = text[3..].find("\n---") {
        let frontmatter_str = &text[3..3 + end];
        let body = text[3 + end + 4..].trim().to_string();
        let fields: Vec<(String, String)> = frontmatter_str
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                if line.is_empty() { return None; }
                let (k, v) = line.split_once(':')?;
                Some((k.trim().to_string(), v.trim().trim_matches('"').to_string()))
            })
            .collect();
        (fields, body)
    } else {
        (vec![], text.to_string())
    }
}

/// Load a SKILL.md file into a `LoadedSkill`. Applies `{baseDir}` substitution.
/// Returns None if required frontmatter is missing or body is empty.
pub fn load_skill_file(skill_md: &Path, plugin: Option<&str>) -> Option<LoadedSkill> {
    let content = std::fs::read_to_string(skill_md).ok()?;
    let (fields, body) = parse_frontmatter(&content);

    let name = fields.iter().find(|(k, _)| k == "name").map(|(_, v)| v.clone())?;
    let description = fields.iter().find(|(k, _)| k == "description").map(|(_, v)| v.clone())?;

    if body.is_empty() {
        return None;
    }

    let base_dir = skill_md.parent()?.canonicalize().ok()?;
    let body = body.replace("{baseDir}", base_dir.to_str()?);

    Some(LoadedSkill {
        name,
        description,
        body,
        plugin: plugin.map(str::to_string),
        base_dir,
        source_path: skill_md.canonicalize().ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn frontmatter_valid() {
        let t = "---\nname: x\ndescription: y\n---\nBody text";
        let (fields, body) = parse_frontmatter(t);
        assert_eq!(fields.len(), 2);
        assert_eq!(body, "Body text");
    }

    #[test]
    fn frontmatter_absent() {
        let t = "Just body";
        let (fields, body) = parse_frontmatter(t);
        assert!(fields.is_empty());
        assert_eq!(body, "Just body");
    }

    fn write_skill(dir: &Path, content: &str) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join("SKILL.md");
        fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn load_skill_basic() {
        let tmp = tempdir();
        let skill_dir = tmp.join("my-skill");
        let path = write_skill(&skill_dir, "---\nname: my-skill\ndescription: desc\n---\nBody");
        let s = load_skill_file(&path, Some("plugin-x")).unwrap();
        assert_eq!(s.name, "my-skill");
        assert_eq!(s.description, "desc");
        assert_eq!(s.body, "Body");
        assert_eq!(s.plugin.as_deref(), Some("plugin-x"));
        assert!(s.base_dir.is_absolute());
    }

    #[test]
    fn load_skill_basedir_substitution() {
        let tmp = tempdir();
        let skill_dir = tmp.join("skill");
        let path = write_skill(&skill_dir, "---\nname: s\ndescription: d\n---\nRun {baseDir}/x.js");
        let s = load_skill_file(&path, None).unwrap();
        let expected = format!("Run {}/x.js", s.base_dir.to_str().unwrap());
        assert_eq!(s.body, expected);
    }

    #[test]
    fn load_skill_missing_frontmatter_returns_none() {
        let tmp = tempdir();
        let skill_dir = tmp.join("bad");
        let path = write_skill(&skill_dir, "no frontmatter here");
        assert!(load_skill_file(&path, None).is_none());
    }

    #[test]
    fn load_skill_missing_description_returns_none() {
        let tmp = tempdir();
        let skill_dir = tmp.join("bad2");
        let path = write_skill(&skill_dir, "---\nname: x\n---\nbody");
        assert!(load_skill_file(&path, None).is_none());
    }

    /// Create a unique tempdir under /tmp for tests.
    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "synaps-skills-test-{}", std::process::id()
        ));
        let unique = base.join(format!("{}", crate::epoch_millis()));
        std::fs::create_dir_all(&unique).unwrap();
        unique
    }
}
```

**Step 2:** Run tests.

```bash
cargo test --lib skills::loader 2>&1 | grep "^test result"
```

Expected: 6 passed (2 frontmatter + 4 load_skill).

**Step 3:** Commit.

```bash
git add src/skills/loader.rs && git commit -m "feat: add SKILL.md loader with {baseDir} substitution"
```

---

## Task 6: Full discovery walk — `load_all`

**Goal:** Walk the four discovery roots, combine marketplace + plugin + loose-skill passes, dedup, return `(Vec<Plugin>, Vec<LoadedSkill>)`.

**Files:**
- Modify: `src/skills/loader.rs`

**Step 1:** Add `load_all` to `src/skills/loader.rs`:

```rust
use crate::skills::{Plugin, manifest::{PluginManifest, MarketplaceManifest}};

/// The four default discovery roots, in priority order (local first, global second).
pub fn default_roots() -> Vec<PathBuf> {
    let mut roots = vec![
        PathBuf::from(".synaps-cli/plugins"),
        PathBuf::from(".synaps-cli/skills"),
    ];
    let home_plugins = crate::config::resolve_read_path_extended("plugins");
    let home_skills = crate::config::resolve_read_path_extended("skills");
    roots.push(home_plugins);
    roots.push(home_skills);
    roots
}

/// Walk the given roots and discover all plugins and skills.
/// Deduplicates on (plugin_name, skill_name); first occurrence wins.
pub fn load_all(roots: &[PathBuf]) -> (Vec<Plugin>, Vec<LoadedSkill>) {
    let mut plugins: Vec<Plugin> = Vec::new();
    let mut skills: Vec<LoadedSkill> = Vec::new();
    let mut seen: std::collections::HashSet<(Option<String>, String)> =
        std::collections::HashSet::new();

    for root in roots {
        walk_root(root, &mut plugins, &mut skills, &mut seen);
    }
    (plugins, skills)
}

fn walk_root(
    root: &Path,
    plugins: &mut Vec<Plugin>,
    skills: &mut Vec<LoadedSkill>,
    seen: &mut std::collections::HashSet<(Option<String>, String)>,
) {
    if !root.exists() { return; }

    // 1. Marketplace pass
    let marketplace_json = root.join(".synaps-plugin").join("marketplace.json");
    let marketplace_name = if marketplace_json.exists() {
        match std::fs::read_to_string(&marketplace_json)
            .ok()
            .and_then(|c| serde_json::from_str::<MarketplaceManifest>(&c).ok())
        {
            Some(m) => {
                for entry in &m.plugins {
                    let plugin_root = root.join(&entry.source);
                    load_plugin(&plugin_root, Some(&m.name), plugins, skills, seen);
                }
                Some(m.name)
            }
            None => {
                tracing::warn!("failed to parse {}", marketplace_json.display());
                None
            }
        }
    } else {
        None
    };

    // 2. Plugin pass (subdirs with .synaps-plugin/plugin.json)
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() { continue; }
            if path.join(".synaps-plugin").join("plugin.json").exists() {
                load_plugin(&path, marketplace_name.as_deref(), plugins, skills, seen);
            }
        }
    }

    // 3. Loose-skill pass (root/skills/<name>/SKILL.md)
    let loose_dir = root.join("skills");
    if loose_dir.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&loose_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() { continue; }
                let skill_md = path.join("SKILL.md");
                if skill_md.exists() {
                    if let Some(s) = load_skill_file(&skill_md, None) {
                        let key = (None, s.name.clone());
                        if seen.insert(key) { skills.push(s); }
                    }
                }
            }
        }
    }
}

fn load_plugin(
    plugin_root: &Path,
    marketplace: Option<&str>,
    plugins: &mut Vec<Plugin>,
    skills: &mut Vec<LoadedSkill>,
    seen: &mut std::collections::HashSet<(Option<String>, String)>,
) {
    let manifest_path = plugin_root.join(".synaps-plugin").join("plugin.json");
    let Ok(content) = std::fs::read_to_string(&manifest_path) else {
        tracing::warn!("failed to read {}", manifest_path.display());
        return;
    };
    let Ok(m): Result<PluginManifest, _> = serde_json::from_str(&content) else {
        tracing::warn!("failed to parse {}", manifest_path.display());
        return;
    };

    let Ok(root_abs) = plugin_root.canonicalize() else { return; };
    plugins.push(Plugin {
        name: m.name.clone(),
        root: root_abs,
        marketplace: marketplace.map(str::to_string),
        version: m.version.clone(),
        description: m.description.clone(),
    });

    let skills_dir = plugin_root.join("skills");
    if !skills_dir.is_dir() { return; }
    let Ok(entries) = std::fs::read_dir(&skills_dir) else { return; };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() { continue; }
        let skill_md = path.join("SKILL.md");
        if !skill_md.exists() { continue; }
        if let Some(s) = load_skill_file(&skill_md, Some(&m.name)) {
            let key = (Some(m.name.clone()), s.name.clone());
            if seen.insert(key) { skills.push(s); }
        }
    }
}
```

**Step 2:** Add tests to `src/skills/loader.rs`:

```rust
    #[test]
    fn load_all_loose_skill() {
        let tmp = tempdir();
        let skill_dir = tmp.join("skills").join("loose");
        write_skill(&skill_dir, "---\nname: loose\ndescription: d\n---\nBody");

        let (plugins, skills) = load_all(&[tmp.clone()]);
        assert!(plugins.is_empty());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "loose");
        assert_eq!(skills[0].plugin, None);
    }

    #[test]
    fn load_all_plugin_skill() {
        let tmp = tempdir();
        let plugin_dir = tmp.join("my-plugin");
        fs::create_dir_all(plugin_dir.join(".synaps-plugin")).unwrap();
        fs::write(
            plugin_dir.join(".synaps-plugin").join("plugin.json"),
            r#"{"name":"my-plugin"}"#,
        ).unwrap();
        write_skill(&plugin_dir.join("skills").join("s1"),
            "---\nname: s1\ndescription: d\n---\nBody");

        let (plugins, skills) = load_all(&[tmp.clone()]);
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].name, "my-plugin");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].plugin.as_deref(), Some("my-plugin"));
    }

    #[test]
    fn load_all_marketplace() {
        let tmp = tempdir();
        // marketplace.json at root
        fs::create_dir_all(tmp.join(".synaps-plugin")).unwrap();
        fs::write(tmp.join(".synaps-plugin").join("marketplace.json"),
            r#"{"name":"pi-skills","plugins":[{"name":"web","source":"./web"}]}"#).unwrap();
        // plugin at ./web
        let plugin_dir = tmp.join("web");
        fs::create_dir_all(plugin_dir.join(".synaps-plugin")).unwrap();
        fs::write(plugin_dir.join(".synaps-plugin").join("plugin.json"),
            r#"{"name":"web"}"#).unwrap();
        write_skill(&plugin_dir.join("skills").join("search"),
            "---\nname: search\ndescription: d\n---\nBody");

        let (plugins, skills) = load_all(&[tmp.clone()]);
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].marketplace.as_deref(), Some("pi-skills"));
        assert_eq!(skills.len(), 1);
    }

    #[test]
    fn load_all_dedup_priority() {
        let tmp_local = tempdir();
        let tmp_global = tempdir();
        // same skill name in both
        write_skill(&tmp_local.join("skills").join("dup"),
            "---\nname: dup\ndescription: local\n---\nBody");
        write_skill(&tmp_global.join("skills").join("dup"),
            "---\nname: dup\ndescription: global\n---\nBody");

        let (_p, skills) = load_all(&[tmp_local, tmp_global]);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].description, "local"); // local wins
    }
```

**Step 3:** Run tests.

```bash
cargo test --lib skills::loader 2>&1 | grep "^test result"
```

Expected: 10 passed.

**Step 4:** Commit.

```bash
git add src/skills/loader.rs && git commit -m "feat: add marketplace/plugin/loose-skill discovery walk"
```

---

## Task 7: Config — disable lists

**Goal:** Extend `SynapsConfig` with `disabled_plugins` and `disabled_skills`; add filter function in `src/skills/config.rs`.

**Files:**
- Modify: `src/core/config.rs`
- Modify: `src/skills/config.rs`

**Step 1:** Add fields to `SynapsConfig` in `src/core/config.rs`:

```rust
pub struct SynapsConfig {
    // ...existing fields...
    pub disabled_plugins: Vec<String>,
    pub disabled_skills: Vec<String>,
}
```

Update `Default::default()`:

```rust
disabled_plugins: Vec::new(),
disabled_skills: Vec::new(),
```

Add parse arms inside `load_config()` near the existing `skills` arm:

```rust
"disabled_plugins" => {
    config.disabled_plugins = val.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
}
"disabled_skills" => {
    config.disabled_skills = val.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
}
```

Update `test_synaps_config_default` to assert the new fields are empty vecs.

**Step 2:** Write `src/skills/config.rs`:

```rust
//! Apply disable lists to discovered skills.

use crate::skills::LoadedSkill;

pub fn filter_disabled(
    skills: Vec<LoadedSkill>,
    disabled_plugins: &[String],
    disabled_skills: &[String],
) -> Vec<LoadedSkill> {
    skills.into_iter().filter(|s| {
        if let Some(ref p) = s.plugin {
            if disabled_plugins.iter().any(|d| d == p) {
                tracing::debug!("skill '{}' disabled via disabled_plugins='{}'", s.name, p);
                return false;
            }
        }
        if disabled_skills.iter().any(|d| d == &s.name) {
            tracing::debug!("skill '{}' disabled via disabled_skills (bare)", s.name);
            return false;
        }
        if let Some(ref p) = s.plugin {
            let qualified = format!("{}:{}", p, s.name);
            if disabled_skills.iter().any(|d| d == &qualified) {
                tracing::debug!("skill '{}' disabled via disabled_skills (qualified)", qualified);
                return false;
            }
        }
        true
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn mk_skill(name: &str, plugin: Option<&str>) -> LoadedSkill {
        LoadedSkill {
            name: name.to_string(),
            description: String::new(),
            body: String::new(),
            plugin: plugin.map(str::to_string),
            base_dir: PathBuf::from("/"),
            source_path: PathBuf::from("/SKILL.md"),
        }
    }

    #[test]
    fn disable_by_plugin() {
        let s = vec![mk_skill("a", Some("p1")), mk_skill("b", Some("p2"))];
        let out = filter_disabled(s, &["p1".to_string()], &[]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "b");
    }

    #[test]
    fn disable_by_bare_name() {
        let s = vec![mk_skill("a", Some("p1")), mk_skill("a", Some("p2")), mk_skill("b", None)];
        let out = filter_disabled(s, &[], &["a".to_string()]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "b");
    }

    #[test]
    fn disable_by_qualified_name() {
        let s = vec![mk_skill("a", Some("p1")), mk_skill("a", Some("p2"))];
        let out = filter_disabled(s, &[], &["p1:a".to_string()]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].plugin.as_deref(), Some("p2"));
    }

    #[test]
    fn empty_filters_pass_through() {
        let s = vec![mk_skill("a", None), mk_skill("b", Some("p"))];
        let out = filter_disabled(s, &[], &[]);
        assert_eq!(out.len(), 2);
    }
}
```

**Step 3:** Run tests.

```bash
cargo test --lib skills::config 2>&1 | grep "^test result"
cargo test --lib config:: 2>&1 | grep "^test result"
```

Expected: 4 passed (skills::config), existing core::config tests still pass.

**Step 4:** Commit.

```bash
git add -A && git commit -m "feat: add disabled_plugins/disabled_skills config keys and filter"
```

---

## Task 8: `CommandRegistry` — collision handling + resolve

**Goal:** In-memory registry mapping slash command names → skills or built-ins, with qualified/unqualified dispatch.

**Files:**
- Modify: `src/skills/registry.rs`

**Step 1:** Write `src/skills/registry.rs`:

```rust
//! Slash command registry: built-ins + dynamically registered skills.

use std::collections::HashMap;
use std::sync::Arc;
use crate::skills::LoadedSkill;

/// Resolution outcome for a typed slash command.
#[derive(Debug)]
pub enum Resolution {
    /// A built-in command (dispatched via existing handle_command).
    Builtin,
    /// A single unambiguous skill.
    Skill(Arc<LoadedSkill>),
    /// Multiple skills share this unqualified name; user must qualify.
    Ambiguous(Vec<String>), // list of plugin-qualified names
    /// No such command.
    Unknown,
}

pub struct CommandRegistry {
    builtins: Vec<&'static str>,
    skills: HashMap<String, Vec<Arc<LoadedSkill>>>, // unqualified name -> all matches
    qualified: HashMap<String, Arc<LoadedSkill>>,   // "plugin:skill" -> single
}

impl CommandRegistry {
    pub fn new(builtins: &[&'static str], skills: Vec<LoadedSkill>) -> Self {
        let mut r = CommandRegistry {
            builtins: builtins.to_vec(),
            skills: HashMap::new(),
            qualified: HashMap::new(),
        };
        let builtins_set: std::collections::HashSet<&str> =
            builtins.iter().copied().collect();

        for s in skills {
            let arc = Arc::new(s);
            // Unqualified entry
            if builtins_set.contains(arc.name.as_str()) {
                tracing::warn!(
                    "skill '{}' shadowed by built-in; reachable only via qualified form '{}:{}'",
                    arc.name,
                    arc.plugin.as_deref().unwrap_or("?"),
                    arc.name
                );
            } else {
                r.skills.entry(arc.name.clone()).or_default().push(arc.clone());
            }
            // Qualified entry
            if let Some(ref p) = arc.plugin {
                let q = format!("{}:{}", p, arc.name);
                r.qualified.insert(q, arc.clone());
            }
        }
        r
    }

    pub fn resolve(&self, cmd: &str) -> Resolution {
        if cmd.contains(':') {
            return match self.qualified.get(cmd) {
                Some(s) => Resolution::Skill(s.clone()),
                None => Resolution::Unknown,
            };
        }
        if self.builtins.contains(&cmd) {
            return Resolution::Builtin;
        }
        match self.skills.get(cmd) {
            Some(v) if v.len() == 1 => Resolution::Skill(v[0].clone()),
            Some(v) => Resolution::Ambiguous(
                v.iter()
                    .map(|s| format!("{}:{}", s.plugin.as_deref().unwrap_or("?"), s.name))
                    .collect(),
            ),
            None => Resolution::Unknown,
        }
    }

    /// All commands for autocomplete/help: builtins + unique unqualified skill names, sorted.
    pub fn all_commands(&self) -> Vec<String> {
        let mut v: Vec<String> = self.builtins.iter().map(|s| s.to_string()).collect();
        v.extend(self.skills.keys().cloned());
        v.sort();
        v.dedup();
        v
    }

    pub fn all_skills(&self) -> Vec<Arc<LoadedSkill>> {
        let mut seen: std::collections::HashSet<(Option<String>, String)> =
            std::collections::HashSet::new();
        let mut out = Vec::new();
        for list in self.skills.values() {
            for s in list {
                let key = (s.plugin.clone(), s.name.clone());
                if seen.insert(key) { out.push(s.clone()); }
            }
        }
        for s in self.qualified.values() {
            let key = (s.plugin.clone(), s.name.clone());
            if seen.insert(key) { out.push(s.clone()); }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn mk(name: &str, plugin: Option<&str>) -> LoadedSkill {
        LoadedSkill {
            name: name.to_string(),
            description: String::new(),
            body: String::new(),
            plugin: plugin.map(str::to_string),
            base_dir: PathBuf::from("/"),
            source_path: PathBuf::from("/SKILL.md"),
        }
    }

    #[test]
    fn resolve_builtin() {
        let r = CommandRegistry::new(&["clear"], vec![]);
        assert!(matches!(r.resolve("clear"), Resolution::Builtin));
    }

    #[test]
    fn resolve_unknown() {
        let r = CommandRegistry::new(&["clear"], vec![]);
        assert!(matches!(r.resolve("xyz"), Resolution::Unknown));
    }

    #[test]
    fn resolve_unique_skill() {
        let r = CommandRegistry::new(&[], vec![mk("search", Some("p"))]);
        match r.resolve("search") {
            Resolution::Skill(s) => assert_eq!(s.name, "search"),
            _ => panic!(),
        }
    }

    #[test]
    fn resolve_ambiguous() {
        let r = CommandRegistry::new(&[], vec![
            mk("search", Some("p1")),
            mk("search", Some("p2")),
        ]);
        match r.resolve("search") {
            Resolution::Ambiguous(v) => {
                assert_eq!(v.len(), 2);
                assert!(v.iter().any(|s| s == "p1:search"));
                assert!(v.iter().any(|s| s == "p2:search"));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn resolve_qualified() {
        let r = CommandRegistry::new(&[], vec![
            mk("search", Some("p1")),
            mk("search", Some("p2")),
        ]);
        match r.resolve("p1:search") {
            Resolution::Skill(s) => assert_eq!(s.plugin.as_deref(), Some("p1")),
            _ => panic!(),
        }
    }

    #[test]
    fn builtin_shadows_skill_unqualified() {
        // Skill named "clear" should not win over the built-in.
        let r = CommandRegistry::new(&["clear"], vec![mk("clear", Some("p"))]);
        assert!(matches!(r.resolve("clear"), Resolution::Builtin));
        // Qualified form still works.
        match r.resolve("p:clear") {
            Resolution::Skill(s) => assert_eq!(s.name, "clear"),
            _ => panic!(),
        }
    }

    #[test]
    fn all_commands_sorted_and_deduped() {
        let r = CommandRegistry::new(&["clear", "model"], vec![
            mk("search", Some("p")),
            mk("help-me", None),
        ]);
        let cmds = r.all_commands();
        assert_eq!(cmds, vec!["clear", "help-me", "model", "search"]);
    }
}
```

**Step 2:** Run tests.

```bash
cargo test --lib skills::registry 2>&1 | grep "^test result"
```

Expected: 7 passed.

**Step 3:** Commit.

```bash
git add src/skills/registry.rs && git commit -m "feat: add CommandRegistry with collision handling"
```

---

## Task 9: New `LoadSkillTool` using `CommandRegistry`

**Goal:** Port `LoadSkillTool` to `src/skills/tool.rs`, feeding from `CommandRegistry::all_skills()`. Keep tool name `load_skill` for compat with existing transcripts.

**Files:**
- Modify: `src/skills/tool.rs`

**Step 1:** Write `src/skills/tool.rs`:

```rust
//! `load_skill` tool — model-initiated skill activation.

use std::sync::Arc;
use serde_json::json;
use crate::skills::{LoadedSkill, registry::{CommandRegistry, Resolution}};

pub struct LoadSkillTool {
    registry: Arc<CommandRegistry>,
}

impl LoadSkillTool {
    pub fn new(registry: Arc<CommandRegistry>) -> Self {
        Self { registry }
    }

    /// Produce the tool-result body for a successfully loaded skill.
    /// Shared between user-initiated (slash) and model-initiated (tool) paths.
    pub fn format_body(skill: &LoadedSkill) -> String {
        format!(
            "# Skill: {} — {}\n\nFollow these guidelines for the rest of this conversation.\n\n{}",
            skill.name, skill.description, skill.body
        )
    }
}

#[async_trait::async_trait]
impl crate::Tool for LoadSkillTool {
    fn name(&self) -> &str { "load_skill" }

    fn description(&self) -> &str {
        "Load a skill to guide your behavior for the current conversation. \
         Skills provide structured guidelines, checklists, and best practices. \
         Call this when a task would benefit from a specific methodology."
    }

    fn parameters(&self) -> serde_json::Value {
        let list: Vec<String> = self.registry.all_skills().iter()
            .map(|s| {
                let qualified = match &s.plugin {
                    Some(p) => format!("{}:{} — {}", p, s.name, s.description),
                    None => format!("{} — {}", s.name, s.description),
                };
                qualified
            })
            .collect();
        json!({
            "type": "object",
            "properties": {
                "skill": {
                    "type": "string",
                    "description": format!("Name of the skill to load (bare or plugin:skill). Available:\n{}", list.join("\n"))
                }
            },
            "required": ["skill"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: crate::ToolContext,
    ) -> crate::Result<String> {
        let name = params["skill"].as_str()
            .ok_or_else(|| crate::RuntimeError::Tool("Missing 'skill' parameter".to_string()))?;

        match self.registry.resolve(name) {
            Resolution::Skill(s) => Ok(Self::format_body(&s)),
            Resolution::Ambiguous(opts) => Err(crate::RuntimeError::Tool(format!(
                "ambiguous skill '{}'; specify one of: {}", name, opts.join(", ")
            ))),
            Resolution::Builtin | Resolution::Unknown => Err(crate::RuntimeError::Tool(
                format!("unknown skill '{}'", name)
            )),
        }
    }
}
```

**Step 2:** Append a small test to exercise `format_body`.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn format_body_includes_name_and_description() {
        let s = LoadedSkill {
            name: "x".into(),
            description: "y".into(),
            body: "z".into(),
            plugin: None,
            base_dir: PathBuf::from("/"),
            source_path: PathBuf::from("/SKILL.md"),
        };
        let out = LoadSkillTool::format_body(&s);
        assert!(out.contains("x"));
        assert!(out.contains("y"));
        assert!(out.contains("z"));
        assert!(out.contains("Follow these guidelines"));
    }
}
```

**Step 3:** Run tests.

```bash
cargo test --lib skills::tool 2>&1 | grep "^test result"
```

Expected: 1 passed.

**Step 4:** Commit.

```bash
git add src/skills/tool.rs && git commit -m "feat: port LoadSkillTool to new CommandRegistry-backed API"
```

---

## Task 10: Public `register()` entry point

**Goal:** One function `skills::register()` that loads config, walks roots, filters, builds registry, installs `LoadSkillTool`. Returns the `Arc<CommandRegistry>` so chatui can wire autocomplete.

**Files:**
- Modify: `src/skills/mod.rs`

**Step 1:** Append to `src/skills/mod.rs`:

```rust
use std::sync::Arc;
use crate::skills::registry::CommandRegistry;
use crate::skills::tool::LoadSkillTool;

/// Built-in command names. Keep in sync with the match in
/// `src/chatui/commands.rs::handle_command`.
pub const BUILTIN_COMMANDS: &[&str] = &[
    "clear", "model", "system", "thinking", "sessions",
    "resume", "theme", "gamba", "help", "quit", "exit",
];

/// Load all skills, apply disable filters, build the command registry,
/// and register the `load_skill` tool. Returns the registry for chatui wiring.
pub async fn register(
    tools: &Arc<tokio::sync::RwLock<crate::ToolRegistry>>,
    config: &crate::SynapsConfig,
) -> Arc<CommandRegistry> {
    let (plugins, mut skills) = loader::load_all(&loader::default_roots());
    skills = config::filter_disabled(skills, &config.disabled_plugins, &config.disabled_skills);

    tracing::info!(
        plugins = plugins.len(),
        skills = skills.len(),
        "loaded plugins and skills"
    );

    let registry = Arc::new(CommandRegistry::new(BUILTIN_COMMANDS, skills));
    let tool = LoadSkillTool::new(registry.clone());
    tools.write().await.register(Arc::new(tool));
    registry
}
```

**Step 2:** Verify build.

```bash
cargo build 2>&1 | tail -5
```

Expected: clean build.

**Step 3:** Commit.

```bash
git add src/skills/mod.rs && git commit -m "feat: add skills::register() entry point"
```

---

## Task 11: Wire registry into chatui — dispatch slash commands

**Goal:** In `src/chatui/commands.rs`, when a slash command isn't a built-in, consult the registry. If it's a skill, synthesize `assistant(tool_use) + user(tool_result) + user(arg)` and start a stream.

**Files:**
- Modify: `src/chatui/commands.rs`
- Modify: `src/chatui/main.rs`

**Step 1:** Extend `CommandAction` in `src/chatui/commands.rs:19`:

```rust
pub(super) enum CommandAction {
    None,
    StartStream,
    Quit,
    LaunchGamba,
    /// Synthesize load_skill tool-result + user message, then start stream.
    LoadSkill {
        skill: std::sync::Arc<synaps_cli::skills::LoadedSkill>,
        arg: String,
    },
}
```

**Step 2:** Change `handle_command`'s signature to accept `&Arc<CommandRegistry>` and change its unknown-command arm:

```rust
pub(super) async fn handle_command(
    cmd: &str,
    arg: &str,
    app: &mut App,
    runtime: &mut Runtime,
    system_prompt_path: &PathBuf,
    registry: &std::sync::Arc<synaps_cli::skills::registry::CommandRegistry>,
) -> CommandAction {
    use synaps_cli::skills::registry::Resolution;
    match cmd {
        // ...existing built-in arms...
        _ => {
            match registry.resolve(cmd) {
                Resolution::Skill(skill) => {
                    return CommandAction::LoadSkill { skill, arg: arg.to_string() };
                }
                Resolution::Ambiguous(opts) => {
                    app.push_msg(ChatMessage::Error(format!(
                        "ambiguous command /{}; try one of: {}",
                        cmd,
                        opts.iter().map(|o| format!("/{}", o)).collect::<Vec<_>>().join(", ")
                    )));
                }
                Resolution::Builtin | Resolution::Unknown => {
                    app.push_msg(ChatMessage::Error(format!("unknown command: /{}", cmd)));
                }
            }
        }
    }
    CommandAction::None
}
```

**Step 3:** In `src/chatui/main.rs`, handle `CommandAction::LoadSkill` by pushing the tool_use + tool_result + user messages and starting a stream. Add after the `LaunchGamba` arm (~line 260):

```rust
CommandAction::LoadSkill { skill, arg } => {
    use synaps_cli::skills::tool::LoadSkillTool;
    use serde_json::json;

    let tool_use_id = format!("toolu_skill_{}", uuid::Uuid::new_v4().simple());
    let body = LoadSkillTool::format_body(&skill);

    app.api_messages.push(json!({
        "role": "assistant",
        "content": [{
            "type": "tool_use",
            "id": tool_use_id,
            "name": "load_skill",
            "input": {"skill": skill.name.clone()}
        }]
    }));
    app.api_messages.push(json!({
        "role": "user",
        "content": [{
            "type": "tool_result",
            "tool_use_id": tool_use_id,
            "content": body
        }]
    }));
    let display_name = match &skill.plugin {
        Some(p) => format!("{}:{}", p, skill.name),
        None => skill.name.clone(),
    };
    app.push_msg(ChatMessage::System(format!("loaded skill: {}", display_name)));

    if !arg.is_empty() {
        app.api_messages.push(json!({"role": "user", "content": arg.clone()}));
        app.push_msg(ChatMessage::User(arg));
    }
    // Start a stream exactly like the normal InputAction::Submit path:
    // reuse the existing start-stream block by duplicating the setup OR
    // extract it into a helper. For v1, duplicate; refactor later if churn.
    let ct = CancellationToken::new();
    let (s_tx, s_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    app.status_text = Some("connecting…".to_string());
    app.streaming = true;
    // ... (mirror the rest of InputAction::Submit's stream-start path)
}
```

**Note for implementer:** the stream-start block after `app.streaming = true` in `InputAction::Submit` (around `main.rs:308` onwards) is ~60 lines. For this task, extract it into a helper `fn start_stream(app, runtime, ...)` first, then call it from both sites. If that refactor is risky, copy-paste — the code-review step will catch drift.

**Step 4:** Wire the registry into `main.rs` startup. Replace the existing skill registration block around `main.rs:107-123`:

```rust
// Replace lines 107-123 with:
let registry = synaps_cli::skills::register(&runtime.tools_shared(), &config).await;
let skill_count = registry.all_skills().len();
if skill_count > 0 {
    eprintln!("\x1b[2m  📚 {} skills available (type / to list)\x1b[0m", skill_count);
}
```

Then thread `registry` down into the event loop call to `handle_command(..., &registry)`.

**Step 5:** Build and test.

```bash
cargo build 2>&1 | tail -5
cargo test 2>&1 | grep "^test result"
```

Expected: clean build; all unit tests pass (UI integration is deferred to Task 13).

**Step 6:** Manual smoke test.

```bash
# Create a test skill in the worktree.
mkdir -p /tmp/synaps-test-home/.synaps-cli/skills/hello
cat > /tmp/synaps-test-home/.synaps-cli/skills/hello/SKILL.md <<'EOF'
---
name: hello
description: Say hello concisely
---
When the user greets you, reply with a single short sentence.
EOF

HOME=/tmp/synaps-test-home cargo run --bin chatui
# In the TUI: type "/hel<Tab>" — should autocomplete to /hello (after Task 12).
# Type "/hello hi" — expect "loaded skill: hello" system line, then model reply.
```

Expected: startup log shows `📚 1 skills available`; `/hello` loads the skill.

**Step 7:** Commit.

```bash
git add -A && git commit -m "feat: dispatch /skill-name commands via CommandRegistry"
```

---

## Task 12: Wire registry into autocomplete + /help

**Goal:** Dynamic command list in autocomplete; `/help` shows a Skills section.

**Files:**
- Modify: `src/chatui/commands.rs`
- Modify: `src/chatui/input.rs`

**Step 1:** Replace `ALL_COMMANDS: &[&str]` usage.

In `src/chatui/commands.rs`, the constant stays (built-ins only), but add:

```rust
pub(super) fn all_commands_with_skills(
    registry: &synaps_cli::skills::registry::CommandRegistry,
) -> Vec<String> {
    registry.all_commands()
}
```

Find every usage of `ALL_COMMANDS` in `src/chatui/input.rs` and `src/chatui/commands.rs` and replace with a call that takes `registry`. Pass the registry down through the event loop. (If `resolve_prefix` in `commands.rs:32` takes `&[&str]`, change its signature to `&[String]` or accept `&[&str]` from a short-lived `Vec<&str>` of converted strings.)

**Step 2:** Extend `/help` in `commands.rs:179`:

```rust
"help" => {
    let help_lines = [
        "/clear — reset conversation",
        "/model [name] — show or set model",
        // ...existing lines...
    ];
    for line in help_lines {
        app.push_msg(ChatMessage::System(line.to_string()));
    }
    let skills = registry.all_skills();
    if !skills.is_empty() {
        app.push_msg(ChatMessage::System(String::new()));
        app.push_msg(ChatMessage::System("## Skills".to_string()));
        let mut sorted = skills.clone();
        sorted.sort_by(|a, b| a.name.cmp(&b.name));
        for s in sorted {
            let display = match &s.plugin {
                Some(p) => format!("/{} ({}:{}) — {}", s.name, p, s.name, s.description),
                None => format!("/{} — {}", s.name, s.description),
            };
            app.push_msg(ChatMessage::System(display));
        }
    }
}
```

**Step 3:** Manual test — type `/hel<Tab>` should autocomplete through skills too; `/help` should list them.

**Step 4:** Build and test.

```bash
cargo build 2>&1 | tail -5
cargo test 2>&1 | grep "^test result"
```

**Step 5:** Commit.

```bash
git add -A && git commit -m "feat: skills appear in autocomplete and /help"
```

---

## Task 13: Integration test

**Goal:** End-to-end fixture test that spins up a temp home directory, plants a marketplace + plugin + loose skill, and verifies discovery + filtering.

**Files:**
- Create: `tests/skills_plugin.rs`

**Step 1:** Write the test:

```rust
//! End-to-end: temp HOME → discovered plugins/skills → CommandRegistry.

use std::fs;
use std::path::PathBuf;
use synaps_cli::skills::{loader, config::filter_disabled, registry::{CommandRegistry, Resolution}};
use synaps_cli::skills::BUILTIN_COMMANDS;

fn write(path: &std::path::Path, content: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

#[test]
fn end_to_end_discovery_and_dispatch() {
    let tmp = std::env::temp_dir().join(format!(
        "synaps-int-test-{}-{}",
        std::process::id(),
        synaps_cli::epoch_millis()
    ));
    fs::create_dir_all(&tmp).unwrap();

    // Marketplace at the root.
    write(
        &tmp.join(".synaps-plugin/marketplace.json"),
        r#"{"name":"m1","plugins":[{"name":"web","source":"./web"}]}"#,
    );
    // Plugin "web" with two skills.
    write(
        &tmp.join("web/.synaps-plugin/plugin.json"),
        r#"{"name":"web"}"#,
    );
    write(
        &tmp.join("web/skills/search/SKILL.md"),
        "---\nname: search\ndescription: Web search\n---\nBody",
    );
    // Collides with built-in "clear".
    write(
        &tmp.join("web/skills/clear/SKILL.md"),
        "---\nname: clear\ndescription: unrelated\n---\nBody",
    );
    // Loose skill.
    write(
        &tmp.join("skills/unique/SKILL.md"),
        "---\nname: unique\ndescription: Loose\n---\nBody",
    );

    let (plugins, skills) = loader::load_all(&[tmp.clone()]);
    assert_eq!(plugins.len(), 1);
    assert_eq!(skills.len(), 3);

    let registry = CommandRegistry::new(BUILTIN_COMMANDS, skills);

    // Built-in wins over the skill named "clear"...
    assert!(matches!(registry.resolve("clear"), Resolution::Builtin));
    // ...but qualified form reaches the skill.
    assert!(matches!(registry.resolve("web:clear"), Resolution::Skill(_)));
    // Unique skill resolves.
    assert!(matches!(registry.resolve("search"), Resolution::Skill(_)));
    // Loose skill resolves.
    assert!(matches!(registry.resolve("unique"), Resolution::Skill(_)));

    // Disable filter removes the plugin-qualified skill.
    let (_p, skills) = loader::load_all(&[tmp.clone()]);
    let filtered = filter_disabled(skills, &[], &["web:search".to_string()]);
    let registry = CommandRegistry::new(BUILTIN_COMMANDS, filtered);
    assert!(matches!(registry.resolve("search"), Resolution::Unknown));
    assert!(matches!(registry.resolve("unique"), Resolution::Skill(_)));

    fs::remove_dir_all(&tmp).ok();
}
```

**Step 2:** Run it.

```bash
cargo test --test skills_plugin 2>&1 | grep "^test result"
```

Expected: 1 passed.

**Step 3:** Commit.

```bash
git add tests/skills_plugin.rs && git commit -m "test: integration test for plugin discovery + dispatch"
```

---

## Task 14: Migration — delete legacy, update callers, CHANGELOG

**Goal:** Remove `src/skills/legacy.rs` and the `skills` config key. Update `main.rs` to stop calling the old auto-load path. Document breaking changes.

**Files:**
- Delete: `src/skills/legacy.rs`
- Modify: `src/skills/mod.rs`
- Modify: `src/core/config.rs`
- Modify: `src/chatui/main.rs`
- Modify: `CHANGELOG.md`

**Step 1:** Remove the legacy re-exports in `src/skills/mod.rs`:

```rust
// DELETE: mod legacy;
// DELETE: pub use legacy::{Skill, load_skills, format_skills_for_prompt, parse_skills_config, setup_skill_tool};
```

**Step 2:** Delete the file.

```bash
rm src/skills/legacy.rs
```

**Step 3:** In `src/core/config.rs`:
- Delete the `skills: Option<Vec<String>>` field from `SynapsConfig`.
- Delete its arm in `Default::default()`.
- Delete the `"skills" =>` arm in `load_config()`.
- Update tests (`test_synaps_config_default` removes the `config.skills` assertion).

**Step 4:** In `src/chatui/main.rs`, delete the now-broken block that used `config.skills` / `format_skills_for_prompt`. The new `register()` call from Task 10 replaces it entirely.

**Step 5:** `grep` for any remaining references.

```bash
grep -rn "format_skills_for_prompt\|parse_skills_config\|setup_skill_tool\|load_skills\|config\.skills" src/ tests/
```

Expected: no output (all callers migrated).

**Step 6:** Add to `CHANGELOG.md` under a new `## Unreleased` heading:

```markdown
## Unreleased

### Added
- Skills & plugins subsystem: discover plugins under `.synaps-cli/plugins/` and `~/.synaps-cli/plugins/`, register each skill as a dynamic slash command (`/skill-name <args>`), and expose the same skills to the model via the `load_skill` tool. Supports `.synaps-plugin/marketplace.json` (multiple plugins from one clone) and `.synaps-plugin/plugin.json` (per-plugin metadata). See `docs/plans/2026-04-18-synaps-skills-plugins-design.md`.
- Config keys `disabled_plugins` and `disabled_skills` for blocking discovered skills (comma-separated list). Qualified names (`plugin:skill`) supported.

### Breaking
- Flat `.md` files under `.synaps-cli/skills/` no longer load. Convert each to a folder: `.synaps-cli/skills/<name>/SKILL.md`.
- The `skills = "a, b"` config key (allowlist) has been removed. To block skills, use `disabled_skills`; to block whole plugins, use `disabled_plugins`.
- Skills are no longer auto-injected into the system prompt at startup. Every skill load is now an explicit event in the conversation (via `/skill-name` or the `load_skill` tool).
```

**Step 7:** Full build + test.

```bash
cargo build 2>&1 | tail -5
cargo test 2>&1 | grep "^test result"
```

Expected: clean build; all tests pass (count should be ~98 baseline minus old skills-rs tests plus new module tests plus integration test).

**Step 8:** Commit.

```bash
git add -A && git commit -m "refactor: remove legacy flat-.md skill loader; migrate to plugin system"
```

---

## Task 15: Final verification

**Goal:** Run the manual checklist from the design doc before handing off.

**Steps:**

1. **pi-skills compat:** clone pi-skills into a test home, rename `.claude-plugin` → `.synaps-plugin` everywhere, start chatui, confirm skills appear in `/help` and autocomplete.

   ```bash
   mkdir -p /tmp/synaps-pi-home/.synaps-cli/plugins
   git clone https://github.com/maha-media/pi-skills /tmp/synaps-pi-home/.synaps-cli/plugins/pi-skills
   find /tmp/synaps-pi-home/.synaps-cli/plugins/pi-skills -type d -name .claude-plugin -exec sh -c 'mv "$1" "${1%/.claude-plugin}/.synaps-plugin"' _ {} \;
   HOME=/tmp/synaps-pi-home cargo run --bin chatui
   # In TUI: type /help — expect Skills section listing exa-search, transcribe, etc.
   ```

2. **Invocation:** type `/exa-search "rust async"` in the TUI; expect `loaded skill: pi-skills:exa-search` (or `exa-search` if unique), then the model should call Bash with the absolute path to `search.js`.

3. **Disable config:** add `disabled_skills = exa-search` to `/tmp/synaps-pi-home/.synaps-cli/config`; restart chatui; confirm `/exa-search` is now unknown and `/help` omits it.

4. **Collision:** create two loose skills with the same name under `.synaps-cli/skills/a/SKILL.md` and another plugin providing the same name; confirm `/<name>` returns an ambiguity error suggesting the qualified forms.

5. **Malformed manifest:** break one `plugin.json` syntactically; confirm startup logs a warning, chat still starts, other plugins still load.

6. **Clean baseline reconfirm:** `cargo test` → all pass. No stale `tracing::warn!` in normal runs.

7. **Finishing:** use `superpowers:finishing-a-development-branch` to merge, squash, or PR.

---

## Open items deliberately deferred (YAGNI)

- `/plugins install <git-url>` command.
- Marketplace signing / trust prompts.
- Hot-reload via `src/watcher/`.
- Per-skill tool-scoping (e.g., skill that forbids Bash).
- `enabled_skills` allowlist — users invert via `disabled_*`.
