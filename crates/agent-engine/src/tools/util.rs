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

/// Task 24: deterministic canonical concurrency key for a path-target
/// mutating tool. Lexical (no filesystem access, works for files that do
/// not exist yet): expands `~`, absolutizes against the cwd, and
/// normalizes `.`/`..` components, so two spellings of the same target
/// collide on one key and distinct targets never do.
pub(crate) fn canonical_path_key(raw: &str) -> Option<String> {
    if raw.trim().is_empty() {
        return None;
    }
    let expanded = expand_path(raw);
    let path = std::path::Path::new(&expanded);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    let mut normalized = std::path::PathBuf::new();
    for component in absolute.components() {
        use std::path::Component;
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    Some(normalized.to_string_lossy().to_string())
}
