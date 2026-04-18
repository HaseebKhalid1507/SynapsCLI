# Plugins & Skills Management UI — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Build a Claude-Code-parity plugin manager on top of the existing skills subsystem: add marketplaces by URL, browse/install/uninstall/update per plugin, enable/disable from `/settings`, with a hot-reloadable command registry.

**Architecture:** Two UI surfaces (`/plugins` full-screen modal for heavy ops, `/settings → Plugins` for toggles). State persisted in `~/.synaps-cli/plugins.json`. HTTPS-only metadata fetch; per-plugin `git clone` install. TOFU host-trust model (owner-scoped). Interior-mutable `CommandRegistry` so install/uninstall/toggle take effect immediately without restart.

**Tech Stack:** Rust 1.80+, `reqwest` (HTTPS fetch), `serde`/`serde_json` (state file), `tokio` (async), `ratatui` (TUI), `git` CLI (clone/pull/ls-remote), existing `CommandRegistry` + `loader`.

**Design doc:** `docs/plans/2026-04-18-plugins-settings-design.md`.

---

## Conventions

- Every task is TDD: write failing test → verify failure → implement → verify pass → commit.
- Commits are small and frequent (one per task unless sub-steps are tightly coupled).
- Use `cargo test` from the worktree root; use `cargo test --test <name>` for a specific integration file.
- **Do not** add `Co-Authored-By` trailers to commits (user preference).
- All new modules live under `src/skills/` (library code) or `src/chatui/plugins/` (UI code). Mirror the `settings/` layout.
- Error messages render inline under the triggering row — no popups except the TOFU confirm.

---

## Task 1: Plugins state file — schema + round-trip

**Files:**
- Create: `src/skills/state.rs`
- Modify: `src/skills/mod.rs` (add `pub mod state;`)

**Step 1: Write the failing test**

Append to the new `src/skills/state.rs` a `#[cfg(test)] mod tests` with:

```rust
#[test]
fn plugins_state_round_trip() {
    let s = PluginsState {
        marketplaces: vec![Marketplace {
            name: "pi-skills".into(),
            url: "https://github.com/maha-media/pi-skills".into(),
            description: Some("…".into()),
            last_refreshed: Some("2026-04-18T12:00:00Z".into()),
            cached_plugins: vec![CachedPlugin {
                name: "web".into(),
                source: "https://github.com/maha-media/pi-web.git".into(),
                version: Some("1.0".into()),
                description: Some("Web tools".into()),
            }],
        }],
        installed: vec![InstalledPlugin {
            name: "web".into(),
            marketplace: Some("pi-skills".into()),
            source_url: "https://github.com/maha-media/pi-web.git".into(),
            installed_commit: "abc123".into(),
            latest_commit: Some("abc123".into()),
            installed_at: "2026-04-18T12:01:00Z".into(),
        }],
        trusted_hosts: vec!["github.com/maha-media".into()],
    };
    let json = serde_json::to_string(&s).unwrap();
    let back: PluginsState = serde_json::from_str(&json).unwrap();
    assert_eq!(back.marketplaces.len(), 1);
    assert_eq!(back.installed.len(), 1);
    assert_eq!(back.trusted_hosts, vec!["github.com/maha-media"]);
}

#[test]
fn plugins_state_defaults_to_empty() {
    let empty: PluginsState = serde_json::from_str("{}").unwrap();
    assert!(empty.marketplaces.is_empty());
    assert!(empty.installed.is_empty());
    assert!(empty.trusted_hosts.is_empty());
}

#[test]
fn plugins_state_load_missing_file_is_empty() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("plugins.json");
    let loaded = PluginsState::load_from(&path).unwrap();
    assert!(loaded.marketplaces.is_empty());
}

#[test]
fn plugins_state_save_and_load_round_trip_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("plugins.json");
    let mut s = PluginsState::default();
    s.trusted_hosts.push("github.com/x".into());
    s.save_to(&path).unwrap();
    let back = PluginsState::load_from(&path).unwrap();
    assert_eq!(back.trusted_hosts, vec!["github.com/x"]);
}

#[test]
fn plugins_state_load_malformed_is_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("plugins.json");
    std::fs::write(&path, "not json").unwrap();
    assert!(PluginsState::load_from(&path).is_err());
}
```

Add `tempfile` to `[dev-dependencies]` in `Cargo.toml` if not already present:
```toml
tempfile = "3"
```
(It may already be present — check first.)

**Step 2: Run tests to verify they fail**

Run: `cargo test -p synaps-cli --lib skills::state`
Expected: compile errors (`PluginsState` undefined).

**Step 3: Implement `src/skills/state.rs`**

```rust
//! Persisted plugin management state: ~/.synaps-cli/plugins.json.

use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginsState {
    #[serde(default)]
    pub marketplaces: Vec<Marketplace>,
    #[serde(default)]
    pub installed: Vec<InstalledPlugin>,
    #[serde(default)]
    pub trusted_hosts: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Marketplace {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub last_refreshed: Option<String>,
    #[serde(default)]
    pub cached_plugins: Vec<CachedPlugin>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedPlugin {
    pub name: String,
    pub source: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledPlugin {
    pub name: String,
    #[serde(default)]
    pub marketplace: Option<String>,
    pub source_url: String,
    pub installed_commit: String,
    #[serde(default)]
    pub latest_commit: Option<String>,
    pub installed_at: String,
}

impl PluginsState {
    pub fn load_from(path: &Path) -> std::io::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(c) => serde_json::from_str(&c)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }

    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        // atomic write via temp + rename
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)
    }

    /// Resolve the on-disk path for the current profile.
    pub fn default_path() -> std::path::PathBuf {
        crate::config::resolve_write_path("plugins.json")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // paste the 5 tests from Step 1 here
}
```

Register the module: add `pub mod state;` to `src/skills/mod.rs` (below existing `pub mod …` lines).

**Step 4: Run tests to verify they pass**

Run: `cargo test -p synaps-cli --lib skills::state`
Expected: 5 passing tests.

**Step 5: Commit**

```bash
git add src/skills/state.rs src/skills/mod.rs Cargo.toml
git commit -m "feat: add PluginsState persistence model"
```

---

## Task 2: URL normalization + https validation

**Files:**
- Create: `src/skills/marketplace.rs`
- Modify: `src/skills/mod.rs` (add `pub mod marketplace;`)

**Step 1: Failing tests**

Put in `src/skills/marketplace.rs` a `#[cfg(test)] mod tests`:

```rust
#[test]
fn normalize_github_url_to_raw() {
    let out = normalize_marketplace_url("https://github.com/maha-media/pi-skills").unwrap();
    assert_eq!(out, "https://raw.githubusercontent.com/maha-media/pi-skills/HEAD/.synaps-plugin/marketplace.json");
}

#[test]
fn normalize_github_url_with_git_suffix() {
    let out = normalize_marketplace_url("https://github.com/a/b.git").unwrap();
    assert_eq!(out, "https://raw.githubusercontent.com/a/b/HEAD/.synaps-plugin/marketplace.json");
}

#[test]
fn normalize_github_url_with_trailing_slash() {
    let out = normalize_marketplace_url("https://github.com/a/b/").unwrap();
    assert_eq!(out, "https://raw.githubusercontent.com/a/b/HEAD/.synaps-plugin/marketplace.json");
}

#[test]
fn normalize_non_github_url_passes_through() {
    // Raw URLs to other hosts are kept as-is.
    let raw = "https://example.com/some/path/marketplace.json";
    assert_eq!(normalize_marketplace_url(raw).unwrap(), raw);
}

#[test]
fn reject_http_url() {
    let err = normalize_marketplace_url("http://github.com/a/b").unwrap_err();
    assert!(err.contains("https"));
}

#[test]
fn reject_empty_url() {
    assert!(normalize_marketplace_url("").is_err());
}

#[test]
fn reject_ssh_url() {
    assert!(normalize_marketplace_url("git@github.com:a/b.git").is_err());
}
```

**Step 2: Run — expect compile error**

Run: `cargo test -p synaps-cli --lib skills::marketplace`

**Step 3: Implement**

Minimum content of `src/skills/marketplace.rs`:

```rust
//! Marketplace URL normalization + metadata fetch.

/// Convert a user-provided URL to the HTTPS URL that yields `marketplace.json`.
/// - `https://github.com/<owner>/<repo>[.git][/]` → raw.githubusercontent.com form.
/// - Any other `https://…` URL is returned unchanged (caller assumed to have pasted a raw URL).
/// - `http://`, `ssh://`, `git@…`, empty → error.
pub fn normalize_marketplace_url(input: &str) -> Result<String, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("URL is empty".into());
    }
    if !s.starts_with("https://") {
        return Err("only https:// URLs are supported".into());
    }
    // GitHub rewrite.
    let gh_prefix = "https://github.com/";
    if let Some(rest) = s.strip_prefix(gh_prefix) {
        let rest = rest.trim_end_matches('/');
        let rest = rest.strip_suffix(".git").unwrap_or(rest);
        let parts: Vec<&str> = rest.splitn(3, '/').collect();
        if parts.len() < 2 || parts[0].is_empty() || parts[1].is_empty() {
            return Err("GitHub URL must be https://github.com/<owner>/<repo>".into());
        }
        return Ok(format!(
            "https://raw.githubusercontent.com/{}/{}/HEAD/.synaps-plugin/marketplace.json",
            parts[0], parts[1]
        ));
    }
    Ok(s.to_string())
}

#[cfg(test)]
mod tests { /* paste tests */ }
```

Add `pub mod marketplace;` to `src/skills/mod.rs`.

**Step 4: Run — expect 7 passing**

Run: `cargo test -p synaps-cli --lib skills::marketplace`

**Step 5: Commit**

```bash
git add src/skills/marketplace.rs src/skills/mod.rs
git commit -m "feat: add marketplace URL normalization with https-only validation"
```

---

## Task 3: TOFU host derivation + trust lookup

**Files:**
- Modify: `src/skills/marketplace.rs` (extend)

**Step 1: Failing tests (append to `src/skills/marketplace.rs` tests)**

```rust
#[test]
fn derive_trust_host_from_github_url() {
    assert_eq!(
        trust_host_for_source("https://github.com/maha-media/pi-web.git").unwrap(),
        "github.com/maha-media"
    );
}

#[test]
fn derive_trust_host_from_gitlab_url() {
    assert_eq!(
        trust_host_for_source("https://gitlab.com/org/repo.git").unwrap(),
        "gitlab.com/org"
    );
}

#[test]
fn derive_trust_host_strips_www() {
    assert_eq!(
        trust_host_for_source("https://www.github.com/x/y").unwrap(),
        "github.com/x"
    );
}

#[test]
fn derive_trust_host_rejects_no_owner() {
    assert!(trust_host_for_source("https://example.com/").is_err());
}

#[test]
fn derive_trust_host_rejects_http() {
    assert!(trust_host_for_source("http://github.com/a/b").is_err());
}
```

**Step 2: Run — expect failures**

**Step 3: Implement (append to `src/skills/marketplace.rs`)**

```rust
/// Owner-scoped host key for TOFU trust: `github.com/<owner>`.
/// Returns Err if URL is malformed, not https, or lacks an owner path segment.
pub fn trust_host_for_source(source_url: &str) -> Result<String, String> {
    let s = source_url.trim();
    if !s.starts_with("https://") {
        return Err("only https:// source URLs are supported".into());
    }
    let rest = &s["https://".len()..];
    let (host_raw, path) = rest.split_once('/').ok_or("missing path in URL")?;
    let host = host_raw.strip_prefix("www.").unwrap_or(host_raw);
    let owner = path.split('/').next().ok_or("missing owner in URL")?;
    if owner.is_empty() { return Err("missing owner in URL".into()); }
    Ok(format!("{}/{}", host, owner))
}

/// Is the given source URL's owner-host already trusted?
pub fn is_trusted(source_url: &str, trusted_hosts: &[String]) -> bool {
    match trust_host_for_source(source_url) {
        Ok(h) => trusted_hosts.iter().any(|t| t == &h),
        Err(_) => false,
    }
}
```

**Step 4: Run — expect all tests pass**

Run: `cargo test -p synaps-cli --lib skills::marketplace`

**Step 5: Commit**

```bash
git add src/skills/marketplace.rs
git commit -m "feat: add owner-scoped TOFU host derivation for plugin sources"
```

---

## Task 4: Marketplace metadata validator

**Files:**
- Modify: `src/skills/marketplace.rs` (extend)
- Use: existing `MarketplaceManifest` in `src/skills/manifest.rs`

**Step 1: Failing tests (append)**

```rust
#[test]
fn validate_manifest_accepts_https_sources() {
    let m: crate::skills::manifest::MarketplaceManifest = serde_json::from_str(r#"
        {"name":"x","plugins":[{"name":"p","source":"https://github.com/a/b.git"}]}
    "#).unwrap();
    assert!(validate_manifest(&m).is_ok());
}

#[test]
fn validate_manifest_rejects_relative_source() {
    let m: crate::skills::manifest::MarketplaceManifest = serde_json::from_str(r#"
        {"name":"x","plugins":[{"name":"p","source":"./p"}]}
    "#).unwrap();
    let err = validate_manifest(&m).unwrap_err();
    assert!(err.contains("relative"));
    assert!(err.contains("'p'"));
}

#[test]
fn validate_manifest_rejects_http_source() {
    let m: crate::skills::manifest::MarketplaceManifest = serde_json::from_str(r#"
        {"name":"x","plugins":[{"name":"p","source":"http://x/y"}]}
    "#).unwrap();
    let err = validate_manifest(&m).unwrap_err();
    assert!(err.contains("https"));
}

#[test]
fn validate_manifest_reports_first_bad_entry() {
    let m: crate::skills::manifest::MarketplaceManifest = serde_json::from_str(r#"
        {"name":"x","plugins":[
            {"name":"ok","source":"https://github.com/a/b.git"},
            {"name":"bad","source":"./b"}
        ]}
    "#).unwrap();
    let err = validate_manifest(&m).unwrap_err();
    assert!(err.contains("'bad'"));
}
```

**Step 2: Run — expect failures**

**Step 3: Implement (append)**

```rust
use crate::skills::manifest::MarketplaceManifest;

pub fn validate_manifest(m: &MarketplaceManifest) -> Result<(), String> {
    for p in &m.plugins {
        let s = p.source.trim();
        if s.starts_with("./") || s.starts_with("../") || !s.contains("://") {
            return Err(format!(
                "plugin '{}' uses unsupported relative source path '{}'",
                p.name, s
            ));
        }
        if !s.starts_with("https://") {
            return Err(format!(
                "plugin '{}' source must be https:// (got '{}')",
                p.name, s
            ));
        }
    }
    Ok(())
}
```

**Step 4: Run — expect all pass**

**Step 5: Commit**

```bash
git add src/skills/marketplace.rs
git commit -m "feat: validate marketplace manifest rejects relative and http sources"
```

---

## Task 5: HTTP metadata fetcher

**Files:**
- Modify: `src/skills/marketplace.rs` (extend)

**Step 1: Failing test**

Append an async integration-style test that uses a local `TcpListener` to serve one HTTP response:

```rust
#[tokio::test]
async fn fetch_marketplace_json_success() {
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let body = r#"{"name":"mk","plugins":[]}"#;
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(), body
        );
        sock.write_all(resp.as_bytes()).await.unwrap();
    });

    let url = format!("http://127.0.0.1:{}/x", port);
    // Bypass the https-only guard by calling fetch_raw (internal helper).
    let body = fetch_raw(&url).await.unwrap();
    assert!(body.contains(r#""name":"mk""#));
}

#[tokio::test]
async fn fetch_marketplace_json_404_returns_error() {
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let resp = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
        sock.write_all(resp.as_bytes()).await.unwrap();
    });
    let url = format!("http://127.0.0.1:{}/x", port);
    let err = fetch_raw(&url).await.unwrap_err();
    assert!(err.contains("404"));
}
```

**Step 2: Run — expect compile errors**

**Step 3: Implement (append to `src/skills/marketplace.rs`)**

```rust
use std::time::Duration;

pub async fn fetch_manifest(url: &str) -> Result<MarketplaceManifest, String> {
    let https_url = if url.starts_with("http://") {
        // only allowed from internal callers; public callers go via normalize.
        return Err("only https:// URLs are supported".into());
    } else {
        url.to_string()
    };
    let body = fetch_raw(&https_url).await?;
    let m: MarketplaceManifest = serde_json::from_str(&body)
        .map_err(|e| format!("invalid marketplace.json: {}", e))?;
    validate_manifest(&m)?;
    Ok(m)
}

/// Low-level: GET the URL and return the body. Used by fetch_manifest and
/// by tests (which need http:// against a local loopback server).
pub(crate) async fn fetch_raw(url: &str) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| format!("reqwest build: {}", e))?;
    let resp = client.get(url).send().await
        .map_err(|e| format!("failed to fetch: {}", e))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("failed to fetch marketplace.json: {}", status));
    }
    resp.text().await.map_err(|e| format!("read body: {}", e))
}
```

**Step 4: Run — expect all pass**

Run: `cargo test -p synaps-cli --lib skills::marketplace`

**Step 5: Commit**

```bash
git add src/skills/marketplace.rs
git commit -m "feat: fetch and validate marketplace metadata over HTTPS with 10s timeout"
```

---

## Task 6: Git install + uninstall + update ops

**Files:**
- Create: `src/skills/install.rs`
- Modify: `src/skills/mod.rs` (`pub mod install;`)

**Step 1: Failing tests**

```rust
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
}
```

**Step 2: Run — expect compile errors**

**Step 3: Implement `src/skills/install.rs`**

```rust
//! Git-backed plugin install/uninstall/update.

use std::path::Path;
use std::process::Command;

/// `git clone --depth=1 <url> <dest>`, then `git rev-parse HEAD`.
/// `dest` must not already exist.
pub fn install_plugin(source_url: &str, dest: &Path) -> Result<String, String> {
    if dest.exists() {
        return Err(format!("{} already exists on disk; uninstall first", dest.display()));
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("mkdir {}: {}", parent.display(), e))?;
    }
    let out = Command::new("git")
        .args(["clone", "--depth=1", "-q", source_url])
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
    let out = Command::new("git")
        .args(["ls-remote", source_url, "HEAD"])
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
mod tests { /* paste tests */ }
```

Register: add `pub mod install;` to `src/skills/mod.rs`.

**Step 4: Run — expect all pass**

Run: `cargo test -p synaps-cli --lib skills::install`
Expected: 5 passing (each test uses a local bare repo, no real network).

**Step 5: Commit**

```bash
git add src/skills/install.rs src/skills/mod.rs
git commit -m "feat: add git-backed install/uninstall/update/ls-remote ops"
```

---

## Task 7: CommandRegistry hot-reload

**Files:**
- Modify: `src/skills/registry.rs`
- Modify: `src/skills/mod.rs` (register signature changes may be needed)

**Step 1: Failing test (append to `src/skills/registry.rs` tests)**

```rust
#[test]
fn rebuild_replaces_skills() {
    let r = CommandRegistry::new(&["clear"], vec![mk("old", None)]);
    assert!(matches!(r.resolve("old"), Resolution::Skill(_)));
    assert!(matches!(r.resolve("new"), Resolution::Unknown));
    r.rebuild_with(vec![mk("new", None)]);
    assert!(matches!(r.resolve("old"), Resolution::Unknown));
    assert!(matches!(r.resolve("new"), Resolution::Skill(_)));
}

#[test]
fn rebuild_visible_through_shared_arc() {
    let r = std::sync::Arc::new(CommandRegistry::new(&[], vec![mk("a", None)]));
    let r2 = r.clone();
    r.rebuild_with(vec![mk("b", None)]);
    assert!(matches!(r2.resolve("b"), Resolution::Skill(_)));
    assert!(matches!(r2.resolve("a"), Resolution::Unknown));
}
```

**Step 2: Run — expect compile error**

**Step 3: Implement**

Refactor `CommandRegistry` to wrap its mutable state in `std::sync::RwLock`:

```rust
use std::sync::RwLock;

struct Inner {
    skills: HashMap<String, Vec<Arc<LoadedSkill>>>,
    qualified: HashMap<String, Arc<LoadedSkill>>,
}

pub struct CommandRegistry {
    builtins: Vec<&'static str>,
    inner: RwLock<Inner>,
}

impl CommandRegistry {
    pub fn new(builtins: &[&'static str], skills: Vec<LoadedSkill>) -> Self {
        let r = CommandRegistry {
            builtins: builtins.to_vec(),
            inner: RwLock::new(Inner { skills: HashMap::new(), qualified: HashMap::new() }),
        };
        r.rebuild_with(skills);
        r
    }

    /// Atomically replace the skill set. Built-ins are unchanged.
    pub fn rebuild_with(&self, skills: Vec<LoadedSkill>) {
        let builtins_set: std::collections::HashSet<&str> =
            self.builtins.iter().copied().collect();
        let mut new_skills: HashMap<String, Vec<Arc<LoadedSkill>>> = HashMap::new();
        let mut new_qualified: HashMap<String, Arc<LoadedSkill>> = HashMap::new();
        for s in skills {
            let arc = Arc::new(s);
            if builtins_set.contains(arc.name.as_str()) {
                tracing::warn!("skill '{}' shadowed by built-in", arc.name);
            } else {
                new_skills.entry(arc.name.clone()).or_default().push(arc.clone());
            }
            if let Some(ref p) = arc.plugin {
                new_qualified.insert(format!("{}:{}", p, arc.name), arc.clone());
            }
        }
        let mut w = self.inner.write().unwrap();
        w.skills = new_skills;
        w.qualified = new_qualified;
    }

    pub fn resolve(&self, cmd: &str) -> Resolution {
        let r = self.inner.read().unwrap();
        if cmd.contains(':') {
            return match r.qualified.get(cmd) {
                Some(s) => Resolution::Skill(s.clone()),
                None => Resolution::Unknown,
            };
        }
        if self.builtins.contains(&cmd) {
            return Resolution::Builtin;
        }
        match r.skills.get(cmd) {
            Some(v) if v.len() == 1 => Resolution::Skill(v[0].clone()),
            Some(v) => Resolution::Ambiguous(
                v.iter().map(|s| format!("{}:{}",
                    s.plugin.as_deref().unwrap_or("?"), s.name)).collect()
            ),
            None => Resolution::Unknown,
        }
    }

    pub fn all_commands(&self) -> Vec<String> {
        let r = self.inner.read().unwrap();
        let mut v: Vec<String> = self.builtins.iter().map(|s| s.to_string()).collect();
        v.extend(r.skills.keys().cloned());
        v.sort();
        v.dedup();
        v
    }

    pub fn all_skills(&self) -> Vec<Arc<LoadedSkill>> {
        let r = self.inner.read().unwrap();
        let mut seen: std::collections::HashSet<(Option<String>, String)> =
            std::collections::HashSet::new();
        let mut out = Vec::new();
        for list in r.skills.values() {
            for s in list {
                let key = (s.plugin.clone(), s.name.clone());
                if seen.insert(key) { out.push(s.clone()); }
            }
        }
        for s in r.qualified.values() {
            let key = (s.plugin.clone(), s.name.clone());
            if seen.insert(key) { out.push(s.clone()); }
        }
        out
    }
}
```

**Step 4: Run all tests — expect green**

Run: `cargo test`
Expected: existing 145 plus 2 new = 147.

**Step 5: Commit**

```bash
git add src/skills/registry.rs
git commit -m "feat: make CommandRegistry hot-reloadable via rebuild_with"
```

---

## Task 8: CommandRegistry::plugins() summary API

**Files:**
- Modify: `src/skills/registry.rs`

**Step 1: Failing test**

Append:

```rust
#[test]
fn plugins_summary_groups_by_plugin_name() {
    let r = CommandRegistry::new(&[], vec![
        mk("a", Some("p1")),
        mk("b", Some("p1")),
        mk("c", Some("p2")),
        mk("loose", None),
    ]);
    let mut plugins = r.plugins();
    plugins.sort_by(|a, b| a.name.cmp(&b.name));
    assert_eq!(plugins.len(), 2);
    assert_eq!(plugins[0].name, "p1");
    assert_eq!(plugins[0].skill_count, 2);
    assert_eq!(plugins[1].name, "p2");
    assert_eq!(plugins[1].skill_count, 1);
}
```

**Step 2: Run — expect failure**

**Step 3: Implement**

```rust
#[derive(Debug, Clone)]
pub struct PluginSummary {
    pub name: String,
    pub skill_count: usize,
}

impl CommandRegistry {
    pub fn plugins(&self) -> Vec<PluginSummary> {
        let r = self.inner.read().unwrap();
        let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
        for s in r.qualified.values() {
            if let Some(ref p) = s.plugin {
                let key = (p.clone(), s.name.clone());
                if seen.insert(key) {
                    *counts.entry(p.clone()).or_insert(0) += 1;
                }
            }
        }
        counts.into_iter()
            .map(|(name, skill_count)| PluginSummary { name, skill_count })
            .collect()
    }
}
```

**Step 4: Run — expect pass**

**Step 5: Commit**

```bash
git add src/skills/registry.rs
git commit -m "feat: expose PluginSummary listing from CommandRegistry"
```

---

## Task 9: Registry rebuild helper on the skills module

**Files:**
- Modify: `src/skills/mod.rs`

**Step 1: Failing test (in `tests/skills_plugin.rs`)**

Append a test that writes a SKILL.md to disk, calls `reload`, and asserts new skill is visible:

```rust
#[tokio::test]
async fn reload_picks_up_new_skill() {
    use synaps_cli::{ToolRegistry, SynapsConfig};
    use synaps_cli::skills::{register, reload_registry};
    use std::sync::Arc;
    use tokio::sync::RwLock;

    let dir = tempfile::tempdir().unwrap();
    let plugins_root = dir.path().join(".synaps-cli").join("plugins");
    std::fs::create_dir_all(&plugins_root).unwrap();

    // Point HOME so default_roots() picks up our dir.
    std::env::set_var("HOME", dir.path());

    let tools = Arc::new(RwLock::new(ToolRegistry::new()));
    let config = SynapsConfig::default();
    let registry = register(&tools, &config).await;

    // No skill yet.
    assert!(matches!(registry.resolve("fresh"), synaps_cli::skills::registry::Resolution::Unknown));

    // Drop a new skill on disk.
    let skill_dir = plugins_root.join("freshplug").join("skills").join("fresh");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        plugins_root.join("freshplug").join(".synaps-plugin").join("plugin.json"),
        r#"{"name":"freshplug"}"#,
    ).ok();
    std::fs::create_dir_all(plugins_root.join("freshplug").join(".synaps-plugin")).unwrap();
    std::fs::write(
        plugins_root.join("freshplug").join(".synaps-plugin").join("plugin.json"),
        r#"{"name":"freshplug"}"#,
    ).unwrap();
    std::fs::write(skill_dir.join("SKILL.md"),
        "---\nname: fresh\ndescription: d\n---\nbody").unwrap();

    reload_registry(&registry, &config);

    assert!(matches!(registry.resolve("fresh"), synaps_cli::skills::registry::Resolution::Skill(_)));
}
```

(Note: this test mutates `HOME`; serialize with `#[serial]` if other tests conflict, or wrap in a module-level mutex. Simpler: ensure tests in this file don't run in parallel by setting `#[cfg(test)]` attribute or pass `-- --test-threads=1` when flaky. For now keep it and observe.)

**Step 2: Run — expect failure (no `reload_registry` function)**

**Step 3: Implement in `src/skills/mod.rs`**

Add below `register`:

```rust
/// Re-walks discovery roots and swaps in the new skill set atomically.
/// Built-ins and the existing `load_skill` tool registration are unchanged.
pub fn reload_registry(registry: &CommandRegistry, config: &crate::SynapsConfig) {
    let (_plugins, mut skills) = loader::load_all(&loader::default_roots());
    skills = config::filter_disabled(skills, &config.disabled_plugins, &config.disabled_skills);
    tracing::info!(skills = skills.len(), "reloaded skills");
    registry.rebuild_with(skills);
}
```

**Step 4: Run — expect pass**

Run: `cargo test --test skills_plugin`

**Step 5: Commit**

```bash
git add src/skills/mod.rs tests/skills_plugin.rs
git commit -m "feat: add reload_registry for hot-swap of discovered skills"
```

---

## Task 10: `/settings → Plugins` category schema + render

**Files:**
- Modify: `src/chatui/settings/schema.rs`
- Modify: `src/chatui/settings/draw.rs`
- Modify: `src/chatui/settings/mod.rs`

**Step 1: Failing test (in `src/chatui/settings/schema.rs`)**

```rust
#[test]
fn plugins_category_is_present() {
    assert!(CATEGORIES.contains(&Category::Plugins));
}

#[test]
fn plugins_category_label() {
    assert_eq!(Category::Plugins.label(), "Plugins");
}
```

**Step 2: Run — expect failures**

**Step 3: Implement**

Edit `src/chatui/settings/schema.rs`:

```rust
pub(crate) enum Category {
    Model,
    Agent,
    ToolLimits,
    Appearance,
    Plugins,   // new
}

impl Category {
    pub fn label(&self) -> &'static str {
        match self {
            Category::Model => "Model",
            Category::Agent => "Agent",
            Category::ToolLimits => "Tool Limits",
            Category::Appearance => "Appearance",
            Category::Plugins => "Plugins",
        }
    }
}

pub(crate) const CATEGORIES: [Category; 5] = [
    Category::Model,
    Category::Agent,
    Category::ToolLimits,
    Category::Appearance,
    Category::Plugins,
];
```

Edit `src/chatui/settings/mod.rs`: extend `RuntimeSnapshot`:

```rust
pub(crate) struct RuntimeSnapshot {
    // … existing fields …
    pub plugins: Vec<PluginRow>,
    pub disabled_plugins: Vec<String>,
}

pub(crate) struct PluginRow {
    pub name: String,
    pub skill_count: usize,
}
```

Extend `from_runtime` to accept the registry:

```rust
pub fn from_runtime(
    runtime: &synaps_cli::Runtime,
    registry: &synaps_cli::skills::registry::CommandRegistry,
) -> Self {
    let config = synaps_cli::config::load_config();
    let plugins: Vec<PluginRow> = registry.plugins().into_iter()
        .map(|p| PluginRow { name: p.name, skill_count: p.skill_count })
        .collect();
    Self {
        // …
        plugins,
        disabled_plugins: config.disabled_plugins.clone(),
        // …
    }
}
```

Edit `src/chatui/settings/draw.rs::render_settings`. At the top, detect the Plugins category and render dynamic rows instead of `ALL_SETTINGS`:

```rust
let current_cat = schema::CATEGORIES[state.category_idx];
if current_cat == schema::Category::Plugins {
    render_plugins_list(frame, area, state, snap);
    return;
}
// … existing code for ALL_SETTINGS categories …
```

Implement `render_plugins_list`:

```rust
fn render_plugins_list(frame: &mut Frame, area: Rect, state: &SettingsState, snap: &RuntimeSnapshot) {
    let mut lines = Vec::new();
    if snap.plugins.is_empty() {
        lines.push(ratatui::text::Line::from(vec![ratatui::text::Span::styled(
            "  No plugins installed. Open /plugins to add a marketplace.",
            Style::default().fg(THEME.help_fg),
        )]));
    } else {
        for (i, p) in snap.plugins.iter().enumerate() {
            let disabled = snap.disabled_plugins.iter().any(|d| d == &p.name);
            let status = if disabled { "✗ disabled" } else { "✓ enabled" };
            let skills_part = if p.skill_count > 0 { format!("  ({} skills)", p.skill_count) } else { String::new() };
            let selected = i == state.setting_idx && state.focus == Focus::Right;
            let style = if selected {
                Style::default().fg(THEME.claude_label)
            } else {
                Style::default().fg(THEME.claude_text)
            };
            lines.push(ratatui::text::Line::from(vec![ratatui::text::Span::styled(
                format!("  {:<20} {}{}", p.name, status, skills_part),
                style,
            )]));
        }
    }
    frame.render_widget(Paragraph::new(lines), area);
}
```

Also update `SettingsState::current_settings`: when category is Plugins, returns empty Vec (no static SettingDef rows). Instead, the cursor bound in Plugins category is `snap.plugins.len()`. Update `input.rs` navigation bounds in a follow-up task — here we just get the rendering working.

Finally, **update every call site of `RuntimeSnapshot::from_runtime`** in `src/chatui/main.rs` to pass the registry (grep for `RuntimeSnapshot::from_runtime` and add the second arg).

**Step 4: Run all tests**

Run: `cargo test`
Expected: all pass, including the 2 new schema tests.

**Step 5: Commit**

```bash
git add src/chatui/settings/ src/chatui/main.rs
git commit -m "feat: add Plugins category to /settings with dynamic plugin list"
```

---

## Task 11: `/settings → Plugins` enable/disable toggle

**Files:**
- Modify: `src/chatui/settings/input.rs`
- Modify: `src/chatui/main.rs`

**Step 1: Failing test (in `src/chatui/settings/input.rs`)**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn snap() -> RuntimeSnapshot {
        RuntimeSnapshot {
            model: "m".into(),
            thinking: "medium".into(),
            max_tool_output: 0,
            bash_timeout: 0,
            bash_max_timeout: 0,
            subagent_timeout: 0,
            api_retries: 0,
            theme_name: "t".into(),
            plugins: vec![
                super::super::PluginRow { name: "p1".into(), skill_count: 1 },
                super::super::PluginRow { name: "p2".into(), skill_count: 2 },
            ],
            disabled_plugins: vec!["p2".into()],
        }
    }

    #[test]
    fn enter_on_plugin_row_toggles_off() {
        let mut state = SettingsState::new();
        // Move to Plugins category.
        state.category_idx = super::super::schema::CATEGORIES
            .iter().position(|c| *c == super::super::schema::Category::Plugins).unwrap();
        state.focus = Focus::Right;
        state.setting_idx = 0; // "p1", currently enabled
        let out = handle_event(&mut state, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snap());
        match out {
            InputOutcome::TogglePlugin { name, enabled } => {
                assert_eq!(name, "p1");
                assert_eq!(enabled, false);
            }
            _ => panic!("expected TogglePlugin"),
        }
    }

    #[test]
    fn enter_on_disabled_plugin_toggles_on() {
        let mut state = SettingsState::new();
        state.category_idx = super::super::schema::CATEGORIES
            .iter().position(|c| *c == super::super::schema::Category::Plugins).unwrap();
        state.focus = Focus::Right;
        state.setting_idx = 1; // "p2", disabled
        let out = handle_event(&mut state, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snap());
        match out {
            InputOutcome::TogglePlugin { name, enabled } => {
                assert_eq!(name, "p2");
                assert_eq!(enabled, true);
            }
            _ => panic!("expected TogglePlugin"),
        }
    }
}
```

**Step 2: Run — expect compile fail (no `TogglePlugin` variant yet)**

**Step 3: Implement**

Add variant to `InputOutcome`:

```rust
pub(crate) enum InputOutcome {
    None,
    Close,
    Apply { key: &'static str, value: String },
    TogglePlugin { name: String, enabled: bool },
}
```

In `handle_event`, before the existing Enter handler, add:

```rust
// Plugins category: Enter or Space toggles plugin.
if state.focus == Focus::Right {
    let cat = super::schema::CATEGORIES[state.category_idx];
    if cat == super::schema::Category::Plugins {
        match key.code {
            KeyCode::Enter | KeyCode::Char(' ') => {
                if let Some(row) = snap.plugins.get(state.setting_idx) {
                    let disabled = snap.disabled_plugins.iter().any(|d| d == &row.name);
                    return InputOutcome::TogglePlugin {
                        name: row.name.clone(),
                        enabled: disabled, // toggle → if was disabled, now enabled
                    };
                }
                return InputOutcome::None;
            }
            _ => {}
        }
    }
}
```

Also update down/up navigation bounds: when category is Plugins, the number of rows is `snap.plugins.len()` (not `state.current_settings().len()`). Add a helper:

```rust
fn row_count(state: &SettingsState, snap: &RuntimeSnapshot) -> usize {
    let cat = super::schema::CATEGORIES[state.category_idx];
    if cat == super::schema::Category::Plugins {
        snap.plugins.len()
    } else {
        state.current_settings().len()
    }
}
```

Use it in the Down handler.

**Wire into chatui** (`src/chatui/main.rs`): find the existing `InputOutcome::Apply` match; add a new arm:

```rust
InputOutcome::TogglePlugin { name, enabled } => {
    let mut config = synaps_cli::config::load_config();
    if enabled {
        config.disabled_plugins.retain(|p| p != &name);
    } else if !config.disabled_plugins.iter().any(|p| p == &name) {
        config.disabled_plugins.push(name.clone());
    }
    let csv = config.disabled_plugins.join(", ");
    let _ = synaps_cli::config::write_config_value("disabled_plugins", &csv);
    synaps_cli::skills::reload_registry(&registry, &config);
}
```

**Step 4: Run — expect all pass**

Run: `cargo test`

**Step 5: Commit**

```bash
git add src/chatui/settings/input.rs src/chatui/main.rs
git commit -m "feat: wire Enter/Space toggle for plugins in /settings"
```

---

## Task 12: `/plugins` modal state machine

**Files:**
- Create: `src/chatui/plugins/mod.rs`
- Create: `src/chatui/plugins/state.rs`
- Modify: `src/chatui/mod.rs` (or wherever submodules are declared)

**Step 1: Failing tests (in `src/chatui/plugins/state.rs`)**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::state::{PluginsState, Marketplace, CachedPlugin, InstalledPlugin};

    fn mk_state() -> PluginsModalState {
        let file = PluginsState {
            marketplaces: vec![
                Marketplace {
                    name: "pi".into(),
                    url: "https://github.com/m/pi".into(),
                    description: None,
                    last_refreshed: None,
                    cached_plugins: vec![
                        CachedPlugin {
                            name: "web".into(),
                            source: "https://github.com/m/web.git".into(),
                            version: None,
                            description: None,
                        },
                    ],
                },
            ],
            installed: vec![
                InstalledPlugin {
                    name: "tools".into(),
                    marketplace: None,
                    source_url: "https://github.com/x/tools.git".into(),
                    installed_commit: "aaa".into(),
                    latest_commit: Some("aaa".into()),
                    installed_at: "now".into(),
                },
            ],
            trusted_hosts: vec![],
        };
        PluginsModalState::new(file)
    }

    #[test]
    fn left_rows_include_installed_and_marketplaces_and_add() {
        let s = mk_state();
        let rows = s.left_rows();
        assert_eq!(rows[0], LeftRow::Installed);
        assert_eq!(rows[1], LeftRow::Marketplace("pi".into()));
        assert_eq!(rows.last().unwrap(), &LeftRow::AddMarketplace);
    }

    #[test]
    fn right_rows_when_installed_selected() {
        let mut s = mk_state();
        s.selected_left = 0; // Installed
        let rows = s.right_rows();
        assert_eq!(rows.len(), 1);
        assert!(matches!(&rows[0], RightRow::Installed(p) if p.name == "tools"));
    }

    #[test]
    fn right_rows_when_marketplace_selected() {
        let mut s = mk_state();
        s.selected_left = 1; // pi
        let rows = s.right_rows();
        assert_eq!(rows.len(), 1);
        assert!(matches!(&rows[0], RightRow::Browseable { plugin, installed: false } if plugin.name == "web"));
    }

    #[test]
    fn right_rows_when_add_marketplace_selected() {
        let mut s = mk_state();
        s.selected_left = s.left_rows().len() - 1;
        let rows = s.right_rows();
        assert!(rows.is_empty());
    }

    #[test]
    fn move_down_clamps_in_left_pane() {
        let mut s = mk_state();
        let last = s.left_rows().len() - 1;
        for _ in 0..100 { s.move_left_down(); }
        assert_eq!(s.selected_left, last);
    }
}
```

**Step 2: Run — expect failures**

**Step 3: Implement**

`src/chatui/plugins/mod.rs`:

```rust
//! /plugins full-screen modal.
pub(crate) mod state;
pub(crate) mod draw;
pub(crate) mod input;

pub(crate) use state::PluginsModalState;
pub(crate) use draw::render;
pub(crate) use input::{handle_event, InputOutcome};
```

`src/chatui/plugins/state.rs`:

```rust
use crate::skills::state::{PluginsState, InstalledPlugin, CachedPlugin};

#[derive(Debug, PartialEq, Eq)]
pub enum LeftRow {
    Installed,
    Marketplace(String),
    AddMarketplace,
}

pub enum RightRow<'a> {
    Installed(&'a InstalledPlugin),
    Browseable { plugin: &'a CachedPlugin, installed: bool },
}

pub enum Focus { Left, Right }

pub enum RightMode {
    List,
    Detail { row_idx: usize },
    AddMarketplaceEditor { buffer: String, error: Option<String> },
    TrustPrompt { plugin_name: String, host: String, pending_source: String },
    Confirm { prompt: String, on_yes: ConfirmAction },
}

pub enum ConfirmAction {
    Uninstall(String),       // plugin name
    RemoveMarketplace(String),
}

pub struct PluginsModalState {
    pub file: PluginsState,
    pub selected_left: usize,
    pub selected_right: usize,
    pub focus: Focus,
    pub mode: RightMode,
    pub row_error: Option<String>,
}

impl PluginsModalState {
    pub fn new(file: PluginsState) -> Self {
        Self {
            file,
            selected_left: 0,
            selected_right: 0,
            focus: Focus::Left,
            mode: RightMode::List,
            row_error: None,
        }
    }

    pub fn left_rows(&self) -> Vec<LeftRow> {
        let mut rows = vec![LeftRow::Installed];
        for m in &self.file.marketplaces {
            rows.push(LeftRow::Marketplace(m.name.clone()));
        }
        rows.push(LeftRow::AddMarketplace);
        rows
    }

    pub fn right_rows(&self) -> Vec<RightRow<'_>> {
        let left = self.left_rows();
        match left.get(self.selected_left) {
            Some(LeftRow::Installed) => self.file.installed.iter()
                .map(RightRow::Installed).collect(),
            Some(LeftRow::Marketplace(mname)) => {
                let Some(m) = self.file.marketplaces.iter().find(|m| &m.name == mname) else {
                    return Vec::new();
                };
                m.cached_plugins.iter()
                    .map(|p| RightRow::Browseable {
                        plugin: p,
                        installed: self.file.installed.iter().any(|i| i.name == p.name),
                    })
                    .collect()
            }
            _ => Vec::new(),
        }
    }

    pub fn move_left_down(&mut self) {
        let n = self.left_rows().len();
        if self.selected_left + 1 < n { self.selected_left += 1; self.selected_right = 0; }
    }
    pub fn move_left_up(&mut self) {
        if self.selected_left > 0 { self.selected_left -= 1; self.selected_right = 0; }
    }
    pub fn move_right_down(&mut self) {
        let n = self.right_rows().len();
        if self.selected_right + 1 < n { self.selected_right += 1; }
    }
    pub fn move_right_up(&mut self) {
        if self.selected_right > 0 { self.selected_right -= 1; }
    }
}

#[cfg(test)]
mod tests { /* paste */ }
```

Declare the module in `src/chatui/mod.rs` (or the file where `pub(crate) mod settings;` is — follow the same pattern).

For **draw.rs** and **input.rs**, create minimal stubs that compile — they'll be filled in by subsequent tasks. Use `#![allow(unused)]` at module tops if needed to silence unused-state warnings for now.

`src/chatui/plugins/draw.rs`:

```rust
use ratatui::Frame;
use ratatui::layout::Rect;
use super::PluginsModalState;

pub(crate) fn render(_frame: &mut Frame, _area: Rect, _state: &PluginsModalState) {
    // fleshed out in later task
}
```

`src/chatui/plugins/input.rs`:

```rust
use crossterm::event::KeyEvent;
use super::PluginsModalState;

pub(crate) enum InputOutcome {
    None,
    Close,
}

pub(crate) fn handle_event(_state: &mut PluginsModalState, _key: KeyEvent) -> InputOutcome {
    InputOutcome::None
}
```

**Step 4: Run — expect pass**

Run: `cargo test --lib chatui::plugins`

**Step 5: Commit**

```bash
git add src/chatui/plugins/ src/chatui/mod.rs
git commit -m "feat: add /plugins modal state machine skeleton"
```

---

## Task 13: `/plugins` command wiring + App state

**Files:**
- Modify: `src/chatui/commands.rs`
- Modify: `src/chatui/app.rs`
- Modify: `src/chatui/main.rs`

**Step 1: Failing test (add to `src/chatui/commands.rs` tests module if one exists, else create)**

```rust
#[test]
fn plugins_is_in_all_commands() {
    assert!(ALL_COMMANDS.contains(&"plugins"));
}
```

**Step 2: Run — expect failure**

**Step 3: Implement**

In `src/chatui/commands.rs`:
- Add `"plugins"` to `ALL_COMMANDS`.
- Add to `CommandAction`:
  ```rust
  /// Open the /plugins modal.
  OpenPlugins,
  ```
- Add a new match arm:
  ```rust
  "plugins" => {
      // If arg == "reload", return a distinct action; else open modal.
      if arg.trim() == "reload" {
          return CommandAction::ReloadPlugins;
      }
      return CommandAction::OpenPlugins;
  }
  ```
- Add `ReloadPlugins` variant to `CommandAction`.

In `src/chatui/app.rs`, add to `App`:

```rust
pub(crate) plugins: Option<super::plugins::PluginsModalState>,
```

And initialize in `App::new`: `plugins: None`.

In `src/chatui/main.rs`, wire the handler:
- Match `CommandAction::OpenPlugins` → load `PluginsState` from disk, create `PluginsModalState`, store in `app.plugins`.
- Match `CommandAction::ReloadPlugins` → call `synaps_cli::skills::reload_registry(&registry, &config)` and push a `ChatMessage::System("plugins reloaded")`.
- In the render path, if `app.plugins.is_some()`, call `plugins::render(frame, area, app.plugins.as_ref().unwrap())` after the main chat is drawn.
- In the input routing, if `app.plugins.is_some()`, route key events to `plugins::handle_event`; on `Close`, set `app.plugins = None`.

**Step 4: Run all tests**

Run: `cargo test && cargo build --bin chatui`

**Step 5: Commit**

```bash
git add src/chatui/commands.rs src/chatui/app.rs src/chatui/main.rs
git commit -m "feat: wire /plugins and /plugins reload commands"
```

---

## Task 14: `/plugins` modal rendering (two-pane)

**Files:**
- Modify: `src/chatui/plugins/draw.rs`

**No TDD here** — rendering is visual; the state machine is already tested. Implement `render` to produce the layout from the design doc. Use `src/chatui/settings/draw.rs` as a close template. Include:

- Outer `Clear` + titled border block.
- Vertical split: main content + footer hint bar.
- Horizontal split of content: ~20-char left pane + right pane.
- Left pane: render `state.left_rows()` with a marker for the selected row (only when `focus == Left`).
- Right pane: dispatch on `state.mode`:
  - `List` → iterate `right_rows()`.
  - `Detail { row_idx }` → render a scrollable detail panel (see below).
  - `AddMarketplaceEditor { buffer, error }` → show `[{buffer}_]` with inline error.
  - `TrustPrompt { .. }` → render a centered box (reuse the Clear+Block pattern).
  - `Confirm { prompt, .. }` → inline y/n line.
- Footer hint bar: context-sensitive strings based on `state.focus` and `state.mode`.

Keep it visual — no tests. Verify by `cargo build --bin chatui`.

**Commit:**

```bash
git add src/chatui/plugins/draw.rs
git commit -m "feat: render /plugins two-pane layout"
```

---

## Task 15: `/plugins` input + Close/navigation/AddMarketplace editor

**Files:**
- Modify: `src/chatui/plugins/input.rs`

**Step 1: Failing tests**

Extend the existing test module in `input.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::state::PluginsState;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(c: KeyCode) -> KeyEvent { KeyEvent::new(c, KeyModifiers::NONE) }

    #[test]
    fn esc_in_list_closes() {
        let mut s = crate::chatui::plugins::PluginsModalState::new(PluginsState::default());
        assert!(matches!(handle_event(&mut s, key(KeyCode::Esc)), InputOutcome::Close));
    }

    #[test]
    fn tab_toggles_focus() {
        let mut s = crate::chatui::plugins::PluginsModalState::new(PluginsState::default());
        handle_event(&mut s, key(KeyCode::Tab));
        assert!(matches!(s.focus, crate::chatui::plugins::state::Focus::Right));
    }

    #[test]
    fn enter_on_add_marketplace_opens_editor() {
        let mut s = crate::chatui::plugins::PluginsModalState::new(PluginsState::default());
        s.selected_left = s.left_rows().len() - 1; // AddMarketplace
        s.focus = crate::chatui::plugins::state::Focus::Right;
        handle_event(&mut s, key(KeyCode::Enter));
        assert!(matches!(s.mode, crate::chatui::plugins::state::RightMode::AddMarketplaceEditor { .. }));
    }

    #[test]
    fn esc_in_add_marketplace_editor_returns_to_list() {
        let mut s = crate::chatui::plugins::PluginsModalState::new(PluginsState::default());
        s.mode = crate::chatui::plugins::state::RightMode::AddMarketplaceEditor {
            buffer: "x".into(), error: None,
        };
        handle_event(&mut s, key(KeyCode::Esc));
        assert!(matches!(s.mode, crate::chatui::plugins::state::RightMode::List));
    }
}
```

**Step 2: Run — expect failures**

**Step 3: Implement**

Replace the stub in `src/chatui/plugins/input.rs`:

```rust
use crossterm::event::{KeyCode, KeyEvent};
use super::{PluginsModalState};
use super::state::{Focus, RightMode, LeftRow};

pub(crate) enum InputOutcome {
    None,
    Close,
    /// Caller should: fetch marketplace metadata and, on success, insert into state.
    AddMarketplace(String),
    /// Caller should: install the given plugin (from marketplace at index, plugin at index).
    Install { marketplace: String, plugin: String },
    Uninstall(String),
    Update(String),
    RefreshMarketplace(String),
    RemoveMarketplace(String),
    TrustAndInstall { plugin_name: String, host: String, source: String },
    /// Toggle disabled state of a plugin. enabled=true means "make it enabled".
    TogglePlugin { name: String, enabled: bool },
}

pub(crate) fn handle_event(state: &mut PluginsModalState, key: KeyEvent) -> InputOutcome {
    // Editor / prompt / confirm modes first.
    match &state.mode {
        RightMode::AddMarketplaceEditor { .. } => return editor_key(state, key),
        RightMode::TrustPrompt { .. } => return trust_key(state, key),
        RightMode::Confirm { .. } => return confirm_key(state, key),
        RightMode::Detail { .. } => return detail_key(state, key),
        RightMode::List => {}
    }

    match key.code {
        KeyCode::Esc => InputOutcome::Close,
        KeyCode::Tab => {
            state.focus = match state.focus { Focus::Left => Focus::Right, Focus::Right => Focus::Left };
            state.row_error = None;
            InputOutcome::None
        }
        KeyCode::Up => {
            match state.focus { Focus::Left => state.move_left_up(), Focus::Right => state.move_right_up() }
            InputOutcome::None
        }
        KeyCode::Down => {
            match state.focus { Focus::Left => state.move_left_down(), Focus::Right => state.move_right_down() }
            InputOutcome::None
        }
        KeyCode::Enter => list_enter(state),
        KeyCode::Char('i') if matches!(state.focus, Focus::Right) => install_on_row(state),
        KeyCode::Char('e') if matches!(state.focus, Focus::Right) => toggle_installed(state, true),
        KeyCode::Char('d') if matches!(state.focus, Focus::Right) => toggle_installed(state, false),
        KeyCode::Char('u') if matches!(state.focus, Focus::Right) => update_on_row(state),
        KeyCode::Char('U') if matches!(state.focus, Focus::Right) => {
            ask_uninstall(state)
        }
        KeyCode::Char('r') if matches!(state.focus, Focus::Left) => refresh_selected_marketplace(state),
        KeyCode::Char('R') if matches!(state.focus, Focus::Left) => ask_remove_marketplace(state),
        _ => InputOutcome::None,
    }
}

fn list_enter(state: &mut PluginsModalState) -> InputOutcome {
    let rows = state.left_rows();
    match rows.get(state.selected_left) {
        Some(LeftRow::AddMarketplace) if matches!(state.focus, Focus::Right | Focus::Left) => {
            state.mode = RightMode::AddMarketplaceEditor { buffer: String::new(), error: None };
            state.focus = Focus::Right;
            InputOutcome::None
        }
        Some(_) if matches!(state.focus, Focus::Right) => {
            // Detail view for selected right row.
            if !state.right_rows().is_empty() {
                state.mode = RightMode::Detail { row_idx: state.selected_right };
            }
            InputOutcome::None
        }
        _ => InputOutcome::None,
    }
}

fn editor_key(state: &mut PluginsModalState, key: KeyEvent) -> InputOutcome {
    let RightMode::AddMarketplaceEditor { buffer, error } = &mut state.mode else { return InputOutcome::None };
    match key.code {
        KeyCode::Esc => { state.mode = RightMode::List; InputOutcome::None }
        KeyCode::Backspace => { buffer.pop(); *error = None; InputOutcome::None }
        KeyCode::Char(c) => { buffer.push(c); *error = None; InputOutcome::None }
        KeyCode::Enter => {
            let url = buffer.trim().to_string();
            if url.is_empty() { return InputOutcome::None; }
            InputOutcome::AddMarketplace(url)
        }
        _ => InputOutcome::None,
    }
}

fn trust_key(state: &mut PluginsModalState, key: KeyEvent) -> InputOutcome {
    let RightMode::TrustPrompt { plugin_name, host, pending_source } = &state.mode else {
        return InputOutcome::None;
    };
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            let out = InputOutcome::TrustAndInstall {
                plugin_name: plugin_name.clone(),
                host: host.clone(),
                source: pending_source.clone(),
            };
            state.mode = RightMode::List;
            out
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
            state.mode = RightMode::List;
            InputOutcome::None
        }
        _ => InputOutcome::None,
    }
}

fn confirm_key(state: &mut PluginsModalState, key: KeyEvent) -> InputOutcome {
    let RightMode::Confirm { on_yes, .. } = &state.mode else { return InputOutcome::None };
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            let action = match on_yes {
                crate::chatui::plugins::state::ConfirmAction::Uninstall(n) => InputOutcome::Uninstall(n.clone()),
                crate::chatui::plugins::state::ConfirmAction::RemoveMarketplace(n) => InputOutcome::RemoveMarketplace(n.clone()),
            };
            state.mode = RightMode::List;
            action
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
            state.mode = RightMode::List;
            InputOutcome::None
        }
        _ => InputOutcome::None,
    }
}

fn detail_key(state: &mut PluginsModalState, key: KeyEvent) -> InputOutcome {
    match key.code {
        KeyCode::Esc => { state.mode = RightMode::List; InputOutcome::None }
        _ => InputOutcome::None,
    }
}

fn install_on_row(state: &mut PluginsModalState) -> InputOutcome {
    use super::state::{LeftRow, RightRow};
    let left = state.left_rows();
    let Some(LeftRow::Marketplace(mname)) = left.get(state.selected_left) else {
        return InputOutcome::None;
    };
    let rows = state.right_rows();
    match rows.get(state.selected_right) {
        Some(RightRow::Browseable { plugin, installed: false }) => {
            // TOFU check done by the main loop; we just emit the intent.
            InputOutcome::Install { marketplace: mname.clone(), plugin: plugin.name.clone() }
        }
        Some(RightRow::Browseable { installed: true, .. }) => {
            state.row_error = Some("already installed".into());
            InputOutcome::None
        }
        _ => InputOutcome::None,
    }
}

fn toggle_installed(state: &mut PluginsModalState, enabled: bool) -> InputOutcome {
    let rows = state.right_rows();
    if let Some(super::state::RightRow::Installed(p)) = rows.get(state.selected_right) {
        return InputOutcome::TogglePlugin { name: p.name.clone(), enabled };
    }
    InputOutcome::None
}

fn update_on_row(state: &mut PluginsModalState) -> InputOutcome {
    let rows = state.right_rows();
    if let Some(super::state::RightRow::Installed(p)) = rows.get(state.selected_right) {
        return InputOutcome::Update(p.name.clone());
    }
    InputOutcome::None
}

fn ask_uninstall(state: &mut PluginsModalState) -> InputOutcome {
    let rows = state.right_rows();
    if let Some(super::state::RightRow::Installed(p)) = rows.get(state.selected_right) {
        state.mode = RightMode::Confirm {
            prompt: format!("Uninstall '{}'? y/n", p.name),
            on_yes: crate::chatui::plugins::state::ConfirmAction::Uninstall(p.name.clone()),
        };
    }
    InputOutcome::None
}

fn refresh_selected_marketplace(state: &mut PluginsModalState) -> InputOutcome {
    if let Some(LeftRow::Marketplace(n)) = state.left_rows().get(state.selected_left) {
        return InputOutcome::RefreshMarketplace(n.clone());
    }
    InputOutcome::None
}

fn ask_remove_marketplace(state: &mut PluginsModalState) -> InputOutcome {
    if let Some(LeftRow::Marketplace(n)) = state.left_rows().get(state.selected_left) {
        state.mode = RightMode::Confirm {
            prompt: format!("Remove marketplace '{}'? y/n", n),
            on_yes: crate::chatui::plugins::state::ConfirmAction::RemoveMarketplace(n.clone()),
        };
    }
    InputOutcome::None
}

#[cfg(test)]
mod tests { /* paste */ }
```

**Step 4: Run all tests**

Run: `cargo test`

**Step 5: Commit**

```bash
git add src/chatui/plugins/input.rs
git commit -m "feat: add /plugins input state machine with navigation and actions"
```

---

## Task 16: Wire `/plugins` side-effects into chatui main loop

**Files:**
- Modify: `src/chatui/main.rs`

**No new unit tests** (the side-effects are network/filesystem and are tested end-to-end in Task 17). Implementation:

In the event-loop input branch where `app.plugins.is_some()`, handle each `InputOutcome` variant:

- `None` → no-op.
- `Close` → `app.plugins = None`.
- `AddMarketplace(url)` → call `marketplace::normalize_marketplace_url`; if ok, `runtime.block_on(marketplace::fetch_manifest(&normalized))`; on success, push a new `Marketplace` entry (with `cached_plugins` from the manifest); `PluginsState::save_to(default_path)`; sync the loaded state back into `app.plugins`. On error, set the editor's `error`.
- `Install { marketplace, plugin }` → look up the `CachedPlugin`; compute `trust_host_for_source(source)`; if not trusted, switch to `RightMode::TrustPrompt`. Otherwise run the install flow (Task 17).
- `TrustAndInstall { plugin_name, host, source }` → add `host` to `file.trusted_hosts`; proceed with install as above.
- `Uninstall(name)` → `install::uninstall_plugin(~/.synaps-cli/plugins/<name>)`; remove from `file.installed`; save file; reload registry.
- `Update(name)` → `install::update_plugin(install_path)`; update `installed_commit`; save; reload registry.
- `RefreshMarketplace(name)` → fetch manifest anew; update `cached_plugins` + `last_refreshed`; for each installed plugin from this marketplace, call `ls_remote_head(source_url)` and update `latest_commit`; save file.
- `RemoveMarketplace(name)` → retain-filter `file.marketplaces`; save; (installed plugins keep working).
- `TogglePlugin { name, enabled }` → same implementation as in `/settings` handler (Task 11), plus reload registry.

Implementation note: reuse a helper like `fn commit_plugins_state(file: &PluginsState)` that writes to `PluginsState::default_path()`.

Any of these may fail — set `app.plugins.as_mut().unwrap().row_error = Some(e)` on failure.

**Commit:**

```bash
git add src/chatui/main.rs
git commit -m "feat: wire /plugins side effects (add/install/uninstall/update/refresh/remove)"
```

---

## Task 17: End-to-end integration test

**Files:**
- Create: `tests/plugins_manage.rs`

**Step 1: Write the test**

```rust
//! End-to-end: add marketplace → install → uninstall, with a local HTTP
//! server for metadata and a local bare git repo as the plugin source.

use std::process::Command;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

fn mk_plugin_repo(tmp: &std::path::Path) -> std::path::PathBuf {
    let work = tmp.join("work");
    std::fs::create_dir_all(&work).unwrap();
    Command::new("git").args(["init", "-q"]).current_dir(&work).status().unwrap();
    Command::new("git").args(["config", "user.email", "t@t"]).current_dir(&work).status().unwrap();
    Command::new("git").args(["config", "user.name", "t"]).current_dir(&work).status().unwrap();
    std::fs::write(work.join("SKILL.md"),
        "---\nname: web\ndescription: Web tools\n---\nbody").unwrap();
    // required plugin.json so the loader picks it up
    std::fs::create_dir_all(work.join(".synaps-plugin")).unwrap();
    std::fs::write(
        work.join(".synaps-plugin").join("plugin.json"),
        r#"{"name":"web"}"#,
    ).unwrap();
    // Move SKILL.md under a skills/ subdir as the loader expects.
    std::fs::create_dir_all(work.join("skills").join("search")).unwrap();
    std::fs::rename(work.join("SKILL.md"),
        work.join("skills").join("search").join("SKILL.md")).unwrap();
    Command::new("git").args(["add", "."]).current_dir(&work).status().unwrap();
    Command::new("git").args(["commit", "-q", "-m", "init"]).current_dir(&work).status().unwrap();
    let bare = tmp.join("bare.git");
    Command::new("git").args(["clone", "--bare", "-q",
        work.to_str().unwrap(), bare.to_str().unwrap()]).status().unwrap();
    bare
}

async fn serve_json_once(body: String) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(), body
        );
        sock.write_all(resp.as_bytes()).await.unwrap();
    });
    port
}

#[tokio::test]
async fn end_to_end_add_install_uninstall() {
    use synaps_cli::skills::{state::*, install, marketplace};

    let tmp = tempfile::tempdir().unwrap();
    let bare = mk_plugin_repo(tmp.path());
    let file_url = format!("file://{}", bare.display());

    // Serve a marketplace.json pointing at the bare repo.
    let body = format!(
        r#"{{"name":"mk","plugins":[{{"name":"web","source":"{}"}}]}}"#,
        file_url
    );
    let port = serve_json_once(body).await;
    let metadata_url = format!("http://127.0.0.1:{}/mk", port);

    // Step 1: fetch marketplace manifest.
    let manifest = marketplace::fetch_raw(&metadata_url).await.unwrap();
    let m: synaps_cli::skills::manifest::MarketplaceManifest =
        serde_json::from_str(&manifest).unwrap();

    let mut state = PluginsState::default();
    state.marketplaces.push(Marketplace {
        name: m.name.clone(),
        url: metadata_url.clone(),
        description: None,
        last_refreshed: Some("now".into()),
        cached_plugins: m.plugins.iter().map(|p| CachedPlugin {
            name: p.name.clone(),
            source: p.source.clone(),
            version: None,
            description: None,
        }).collect(),
    });

    // Step 2: install (TOFU bypassed by adding host directly; file:// has no host derivation).
    let dest = tmp.path().join("plugins").join("web");
    let sha = install::install_plugin(&file_url, &dest).unwrap();
    assert!(dest.join(".synaps-plugin").join("plugin.json").exists());
    state.installed.push(InstalledPlugin {
        name: "web".into(),
        marketplace: Some("mk".into()),
        source_url: file_url.clone(),
        installed_commit: sha,
        latest_commit: None,
        installed_at: "now".into(),
    });

    let state_path = tmp.path().join("plugins.json");
    state.save_to(&state_path).unwrap();
    let reloaded = PluginsState::load_from(&state_path).unwrap();
    assert_eq!(reloaded.marketplaces.len(), 1);
    assert_eq!(reloaded.installed.len(), 1);

    // Step 3: uninstall.
    install::uninstall_plugin(&dest).unwrap();
    assert!(!dest.exists());
}
```

**Step 2: Run**

Run: `cargo test --test plugins_manage`
Expected: 1 passing.

**Step 3: Commit**

```bash
git add tests/plugins_manage.rs
git commit -m "test: end-to-end add marketplace → install → uninstall"
```

---

## Task 18: Final build + clippy + tests sweep

**Files:**
- None (verification only)

**Steps:**

1. `cargo build --release` — expect clean build, 8 binaries.
2. `cargo test` — expect all tests pass (previously 145, now ~160).
3. `cargo clippy --all-targets -- -D warnings` — fix any new clippy warnings introduced. Pre-existing warnings (see prior merge) are out of scope.
4. Manually smoke test in the chatui: `cargo run --release --bin chatui`, then `/settings` → verify Plugins category appears, `/plugins` → verify the modal opens and nav works.

**Commit** (only if there are fixes):

```bash
git add -A
git commit -m "chore: polish — fix clippy warnings introduced by plugins UI"
```

---

## Execution notes

- **Tests that mutate `HOME` (Task 9)** can race with parallel test runs. If you see flakes, gate those tests with `serial_test` (add dev-dep) or move them to a dedicated integration test file that runs single-threaded.
- **UI code without unit tests (Tasks 14, 16)** is intentional — rendering and wiring are better verified end-to-end. The state machine tests cover the logic that matters.
- **Do not break `pi-skills` compatibility for *loading*** — the loader already handles `.synaps-plugin/` and relative-path marketplaces. The new install flow requires full URLs, but that's only for the new install-from-marketplace path, not for manually-dropped plugin dirs.
- If a task blocks on a compile error elsewhere (e.g., adding a new enum variant breaks a match), follow the chain and fix it in the same task — don't split the commit across tasks artificially.
