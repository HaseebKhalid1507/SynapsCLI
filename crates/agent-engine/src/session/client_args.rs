//! Client-side resolution of path-like `SessionConfig` arguments (T4, F26).
//!
//! When a thin client ships `--system ./prompt.md` to the daemon, the
//! *string* `./prompt.md` must be resolved against the **client's** cwd, not
//! the daemon's.  This module provides [`resolve_client_session_args`] which
//! rewrites `SessionConfig` in place before `Hello` is sent.
//!
//! ## Path-like heuristic for `--system`
//!
//! A value is treated as a file path when it:
//!   - starts with `/`, `./`, `../`, or `~`, **or**
//!   - ends with `.md` or `.txt` (case-insensitive)
//!
//! **and** contains no embedded newlines or runs of whitespace longer than a
//! single space (prose rarely looks like a path).
//!
//! When path-like AND the file is readable, the value is replaced with the
//! file's **contents**.  When path-like but missing/unreadable, an error is
//! returned so the client can exit non-zero with a helpful message.
//!
//! Non-path-like values pass through untouched (they are literal prompt text).
//!
//! ## `--prompt-manifest`
//!
//! Canonicalized against `client_cwd`.  Missing → error with the resolved path.

use std::path::{Path, PathBuf};

use super::types::SessionConfig;

/// Returns `true` when the value looks like a file path rather than prose.
///
/// Criteria (any of):
/// - starts with `/`, `./`, `../`, or `~`
/// - ends with `.md` or `.txt` (case-insensitive)
///
/// **and** contains no newlines and no multi-space runs (prose guard).
pub fn looks_path_like(val: &str) -> bool {
    // Prose guard: newlines or runs of 2+ spaces → not a path.
    if val.contains('\n') || val.contains("  ") {
        return false;
    }
    let trimmed = val.trim();
    if trimmed.is_empty() {
        return false;
    }

    let starts = trimmed.starts_with('/')
        || trimmed.starts_with("./")
        || trimmed.starts_with("../")
        || trimmed.starts_with('~');

    let lower = trimmed.to_ascii_lowercase();
    let ends = lower.ends_with(".md") || lower.ends_with(".txt");

    starts || ends
}

/// Resolve `client_cwd`-relative path to absolute, expanding `~` to the
/// user's home directory.
fn resolve_against(val: &str, client_cwd: &Path) -> PathBuf {
    let trimmed = val.trim();
    if trimmed.starts_with('~') {
        if let Some(home) = dirs::home_dir() {
            return home.join(trimmed.strip_prefix("~/").unwrap_or(&trimmed[1..]));
        }
    }
    let p = Path::new(trimmed);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        client_cwd.join(p)
    }
}

/// Resolve path-like arguments in `cfg` against `client_cwd`.
///
/// - `system`: path-like → read file contents; path-like but missing → Err.
/// - `prompt_manifest`: canonicalize; missing → Err.
///
/// Called from both thin clients (`tui/attach.rs`, `cmd/attach.rs`) **before**
/// sending `Hello`.  The in-process path (`resolve_system_prompt` in
/// `agent-core`) is unchanged.
pub fn resolve_client_session_args(
    cfg: &mut SessionConfig,
    client_cwd: &Path,
) -> Result<(), String> {
    // ── system ────────────────────────────────────────────────────────────
    if let Some(ref val) = cfg.system {
        if looks_path_like(val) {
            let abs = resolve_against(val, client_cwd);
            match std::fs::read_to_string(&abs) {
                Ok(contents) => {
                    cfg.system = Some(contents);
                }
                Err(_) => {
                    return Err(format!(
                        "--system {val}: no such file (resolved {})",
                        abs.display()
                    ));
                }
            }
        }
    }

    // ── prompt_manifest ──────────────────────────────────────────────────
    if let Some(ref val) = cfg.prompt_manifest {
        let abs = if val.is_absolute() {
            val.clone()
        } else {
            client_cwd.join(val)
        };
        if !abs.exists() {
            return Err(format!(
                "--prompt-manifest {}: no such file (resolved {})",
                val.display(),
                abs.display()
            ));
        }
        cfg.prompt_manifest = Some(abs);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_like_detection() {
        // Path-like
        assert!(looks_path_like("./prompt.md"));
        assert!(looks_path_like("../prompt.md"));
        assert!(looks_path_like("/tmp/prompt.md"));
        assert!(looks_path_like("~/prompts/system.md"));
        assert!(looks_path_like("notes.txt"));
        assert!(looks_path_like("NOTES.MD"));
        assert!(looks_path_like("./foo"));

        // Not path-like (prose)
        assert!(!looks_path_like("You are a helpful assistant"));
        assert!(!looks_path_like("Be concise and direct. Use tools."));
        assert!(!looks_path_like("multi\nline\nprompt"));
        assert!(!looks_path_like("has  double  spaces"));
        assert!(!looks_path_like(""));
    }

    #[test]
    fn relative_resolution_against_cwd() {
        let cwd = Path::new("/tmp/proj");
        let abs = resolve_against("./prompt.md", cwd);
        assert_eq!(abs, PathBuf::from("/tmp/proj/prompt.md"));
        let abs = resolve_against("../other/p.md", cwd);
        assert_eq!(abs, PathBuf::from("/tmp/proj/../other/p.md"));
    }

    #[test]
    fn absolute_path_unchanged() {
        let cwd = Path::new("/tmp/proj");
        let abs = resolve_against("/etc/prompt.md", cwd);
        assert_eq!(abs, PathBuf::from("/etc/prompt.md"));
    }

    #[test]
    fn system_content_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("p.md");
        std::fs::write(&file, "You are PROOF-T4").unwrap();

        let mut cfg = SessionConfig {
            system: Some("./p.md".into()),
            ..Default::default()
        };
        resolve_client_session_args(&mut cfg, dir.path()).unwrap();
        assert_eq!(cfg.system.as_deref(), Some("You are PROOF-T4"));
    }

    #[test]
    fn system_missing_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = SessionConfig {
            system: Some("./missing.md".into()),
            ..Default::default()
        };
        let err = resolve_client_session_args(&mut cfg, dir.path()).unwrap_err();
        assert!(err.contains("--system ./missing.md"), "err={err}");
        assert!(err.contains("no such file"), "err={err}");
        // The resolved path must contain the tempdir and filename.
        assert!(
            err.contains("missing.md"),
            "err={err}"
        );
    }

    #[test]
    fn system_prose_passes_through() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = SessionConfig {
            system: Some("You are a helpful assistant".into()),
            ..Default::default()
        };
        resolve_client_session_args(&mut cfg, dir.path()).unwrap();
        assert_eq!(
            cfg.system.as_deref(),
            Some("You are a helpful assistant")
        );
    }

    #[test]
    fn prompt_manifest_canonicalized() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("manifest.toml");
        std::fs::write(&file, "[prompt]").unwrap();

        let mut cfg = SessionConfig {
            prompt_manifest: Some(PathBuf::from("manifest.toml")),
            ..Default::default()
        };
        resolve_client_session_args(&mut cfg, dir.path()).unwrap();
        assert!(cfg.prompt_manifest.as_ref().unwrap().is_absolute());
    }

    #[test]
    fn prompt_manifest_missing_errors() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = SessionConfig {
            prompt_manifest: Some(PathBuf::from("nope.toml")),
            ..Default::default()
        };
        let err = resolve_client_session_args(&mut cfg, dir.path()).unwrap_err();
        assert!(err.contains("--prompt-manifest"), "err={err}");
        assert!(err.contains("no such file"), "err={err}");
    }
}
