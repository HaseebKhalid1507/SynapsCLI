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

/// Expand `raw` (tilde-aware) and, when `cwd` is `Some`, anchor a relative
/// path to it. `None` returns the expanded path untouched — still relative,
/// resolved by the OS against the process cwd exactly as before (§3.4).
pub(crate) fn resolve_path_in(raw: &str, cwd: Option<&std::path::Path>) -> PathBuf {
    let expanded = expand_path(raw);
    match cwd {
        Some(base) if expanded.is_relative() => base.join(expanded),
        _ => expanded,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn resolve_path_in_anchors_relative_to_cwd() {
        let base = std::path::Path::new("/tmp/synaps-cwd-test");
        assert_eq!(
            resolve_path_in("rel/file.txt", Some(base)),
            base.join("rel/file.txt")
        );
        assert_eq!(
            resolve_path_in("/abs/file.txt", Some(base)),
            PathBuf::from("/abs/file.txt")
        );
    }

    #[test]
    fn resolve_path_in_none_is_byte_identical_to_expand_path() {
        for raw in ["rel/file.txt", "./x", "/abs", "~/foo", "~"] {
            assert_eq!(resolve_path_in(raw, None), expand_path(raw), "{raw}");
        }
    }

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

    /// Kernel semantics: in `a/link/../x`, the symlink `link` is resolved
    /// BEFORE `..` applies — `..` walks up from the symlink's TARGET, not
    /// from its lexical parent. Same actual target ⇒ same key; the lexical
    /// misread (`a/x`) must NOT share the key.
    #[cfg(unix)]
    #[test]
    fn canonical_path_key_resolves_symlink_before_parent_traversal() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::TempDir::new().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();
        // a/link -> b, so a/link/.. is tmp (parent of b), NOT a.
        symlink(&b, a.join("link")).unwrap();

        let through_link = canonical_path_key(a.join("link/../x.txt").to_str().unwrap());
        assert_eq!(
            through_link,
            canonical_path_key(tmp.path().join("x.txt").to_str().unwrap()),
            "same actual kernel target must share one serialization key"
        );
        assert_ne!(
            through_link,
            canonical_path_key(a.join("x.txt").to_str().unwrap()),
            "distinct actual targets must not share a key via lexical .. collapse"
        );
    }

    /// Deeper variant: the symlink points into a NESTED directory, so the
    /// kernel parent after traversal is the nested target's parent.
    #[cfg(unix)]
    #[test]
    fn canonical_path_key_symlink_parent_traversal_lands_in_target_parent() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::TempDir::new().unwrap();
        let nest = tmp.path().join("deep/nest");
        std::fs::create_dir_all(&nest).unwrap();
        let link = tmp.path().join("l");
        symlink(&nest, &link).unwrap();
        assert_eq!(
            canonical_path_key(link.join("../f.txt").to_str().unwrap()),
            canonical_path_key(tmp.path().join("deep/f.txt").to_str().unwrap()),
            "l/../f must key to the symlink target's parent, deep/f"
        );
    }

    /// `..` inside the NONEXISTENT suffix pops suffix components only; a
    /// `..` that empties the suffix applies to the canonical resolved
    /// prefix (which contains no symlinks, so popping is kernel-correct).
    #[cfg(unix)]
    #[test]
    fn canonical_path_key_parent_traversal_after_missing_components() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::TempDir::new().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        assert_eq!(
            canonical_path_key(real.join("m1/m2/../f.txt").to_str().unwrap()),
            canonical_path_key(real.join("m1/f.txt").to_str().unwrap()),
            ".. inside the missing suffix pops the missing component"
        );
        // Suffix fully popped, then `..` walks the canonical prefix: with
        // b_link -> real, `b_link/miss/../x` keys to real/x (create-parents
        // semantics: mkdir(miss) then kernel-resolves miss/.. back to real).
        let b_link = tmp.path().join("b_link");
        symlink(&real, &b_link).unwrap();
        assert_eq!(
            canonical_path_key(b_link.join("miss/../x.txt").to_str().unwrap()),
            canonical_path_key(real.join("x.txt").to_str().unwrap()),
        );
    }

    /// A dangling symlink component cannot be canonicalized without guessing
    /// kernel behavior — the key must be None so the scheduler falls back to
    /// the single conservative global mutation lane.
    #[cfg(unix)]
    #[test]
    fn canonical_path_key_dangling_symlink_falls_back_to_conservative_lane() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::TempDir::new().unwrap();
        let dangling = tmp.path().join("dangling");
        symlink(tmp.path().join("void"), &dangling).unwrap();
        assert_eq!(
            canonical_path_key(dangling.join("f.txt").to_str().unwrap()),
            None,
            "unresolvable symlink component must yield the conservative lane"
        );
    }

    /// Kernel semantics at the root: `/..` is `/`, never an error.
    #[test]
    fn canonical_path_key_root_parent_stays_at_root() {
        let tmp = tempfile::TempDir::new().unwrap();
        let under_root = format!("/..{}", tmp.path().join("r.txt").display());
        assert_eq!(
            canonical_path_key(&under_root),
            canonical_path_key(tmp.path().join("r.txt").to_str().unwrap()),
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
/// path-target mutating tool, preserving KERNEL resolution order: each
/// component is resolved incrementally, so a directory symlink is resolved
/// BEFORE any later `..` applies (`a/link/../x` keys under the parent of
/// `link`'s TARGET, never the lexical parent `a`). Components past the
/// deepest existing prefix (create targets) are appended unresolved; a `..`
/// inside that suffix pops suffix components only (nothing there exists yet,
/// so no symlink can be traversed), and a `..` that empties the suffix walks
/// the canonical — symlink-free — resolved prefix, which is kernel-correct.
/// Any resolution ambiguity (dangling/unreadable symlink, permission error)
/// yields `None`, which the write/edit tools map to the single conservative
/// global mutation lane (`ConcurrencyKey::Serialize`) rather than risking
/// concurrent writes to one actual target.
///
/// TOCTOU limitation (documented residual, Moderate): this is scheduling
/// identity, not a filesystem lock. A symlink can be swapped after
/// resolution; write/edit must retain their own filesystem safety policy
/// for the actual mutation.
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

    use std::path::Component;
    // `resolved` is always canonical (every symlink resolved) and existing;
    // `suffix` holds the not-yet-existing tail in order.
    let mut resolved = std::path::PathBuf::new();
    let mut suffix: Vec<std::ffi::OsString> = Vec::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => resolved.push(prefix.as_os_str()),
            Component::RootDir => resolved.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if suffix.pop().is_some() {
                    continue;
                }
                // Kernel semantics on the canonical prefix: `/..` is `/`;
                // otherwise pop one real (symlink-free) directory.
                let is_root = resolved.parent().is_none();
                if !is_root && !resolved.pop() {
                    return None;
                }
            }
            Component::Normal(name) => {
                if !suffix.is_empty() {
                    suffix.push(name.to_os_string());
                    continue;
                }
                let candidate = resolved.join(name);
                match std::fs::symlink_metadata(&candidate) {
                    Ok(_) => {
                        // Exists (possibly a symlink): resolve it NOW so any
                        // later `..` walks the actual target's parent.
                        resolved = std::fs::canonicalize(&candidate).ok()?;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        suffix.push(name.to_os_string());
                    }
                    // Permission or other inspection failure: ambiguous —
                    // fall back to the conservative global mutation lane.
                    Err(_) => return None,
                }
            }
        }
    }
    for component in suffix {
        resolved.push(component);
    }
    Some(resolved.to_string_lossy().to_string())
}
