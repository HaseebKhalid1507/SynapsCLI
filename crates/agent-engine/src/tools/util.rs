//! Shared utilities for tool implementations — path expansion, ANSI stripping, IDs.
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;

/// Global counter for unique subagent IDs across all dispatches
pub(crate) static NEXT_SUBAGENT_ID: AtomicU64 = AtomicU64::new(1);

/// Strip ANSI escape sequences from a string.
/// Handles CSI sequences (\x1b[...X), OSC sequences (\x1b]...\x07), and simple \x1b(X) escapes.
pub(crate) fn strip_ansi(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    // CSI: consume until a letter (0x40-0x7E)
                    while let Some(&c) = chars.peek() {
                        chars.next();
                        if c.is_ascii_alphabetic() || c == '~' || c == '@' {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    // OSC: consume until BEL (\x07) or ST (\x1b\\)
                    while let Some(&c) = chars.peek() {
                        chars.next();
                        if c == '\x07' {
                            break;
                        }
                        if c == '\x1b' {
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                Some(_) => {
                    chars.next();
                } // simple two-char escape
                None => {}
            }
        } else {
            result.push(ch);
        }
    }
    result
}

pub(crate) fn expand_path(path: &str) -> PathBuf {
    if path.starts_with("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(path.strip_prefix("~/").unwrap());
        }
    } else if path == "~" {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home);
        }
    }
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[cfg(unix)]
    #[test]
    fn canonical_path_key_resolves_directory_symlink_alias_for_existing_leaf() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::TempDir::new().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::write(real.join("file.txt"), "x").unwrap();
        let alias = tmp.path().join("alias");
        symlink(&real, &alias).unwrap();
        assert_eq!(
            canonical_path_key(real.join("file.txt").to_str().unwrap()),
            canonical_path_key(alias.join("file.txt").to_str().unwrap())
        );
    }

    #[cfg(unix)]
    #[test]
    fn canonical_path_key_resolves_directory_symlink_alias_for_missing_leaf() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::TempDir::new().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let alias = tmp.path().join("alias");
        symlink(&real, &alias).unwrap();
        assert_eq!(
            canonical_path_key(real.join("new/file.txt").to_str().unwrap()),
            canonical_path_key(alias.join("new/file.txt").to_str().unwrap())
        );
    }

    #[test]
    fn test_expand_path_home_prefix() {
        let home = env::var("HOME").expect("HOME env var should be set");
        let result = expand_path("~/foo");
        assert_eq!(result, PathBuf::from(home).join("foo"));
    }

    #[test]
    fn test_expand_path_tilde_alone() {
        let home = env::var("HOME").expect("HOME env var should be set");
        let result = expand_path("~");
        assert_eq!(result, PathBuf::from(home));
    }

    #[test]
    fn test_expand_path_absolute_unchanged() {
        let result = expand_path("/absolute/path");
        assert_eq!(result, PathBuf::from("/absolute/path"));
    }

    #[test]
    fn test_expand_path_relative_unchanged() {
        let result = expand_path("relative/path");
        assert_eq!(result, PathBuf::from("relative/path"));
    }
}

/// Task 24/CP-11: deterministic, symlink-aware concurrency identity for a
/// path-target mutating tool. The deepest existing ancestor is canonicalized
/// (resolving directory symlinks), then the lexically normalized unresolved
/// suffix is appended. This works for replacement targets and create targets
/// whose leaf/parents do not exist yet, performs no mutation, and returns
/// `None` on any resolution error so the scheduler conservatively places the
/// call in its serial lane.
///
/// TOCTOU limitation: this is scheduling identity, not a filesystem lock. A
/// symlink can be swapped after resolution; write/edit must retain their own
/// filesystem safety policy for the actual mutation.
pub(crate) fn canonical_path_key(raw: &str) -> Option<String> {
    if raw.trim().is_empty() {
        return None;
    }
    let expanded = expand_path(raw);
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir().ok()?.join(expanded)
    };
    let normalized = lexical_normalize(&absolute)?;

    let mut existing = normalized.as_path();
    let mut unresolved = Vec::new();
    while !existing.exists() {
        let leaf = existing.file_name()?.to_os_string();
        unresolved.push(leaf);
        existing = existing.parent()?;
    }
    let mut canonical = std::fs::canonicalize(existing).ok()?;
    for component in unresolved.into_iter().rev() {
        canonical.push(component);
    }
    Some(canonical.to_string_lossy().to_string())
}

fn lexical_normalize(path: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut normalized = std::path::PathBuf::new();
    for component in path.components() {
        use std::path::Component;
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    Some(normalized)
}
