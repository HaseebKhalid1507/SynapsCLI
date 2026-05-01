//! Shared validation helpers for extension capability identifiers.
//!
//! Centralizes the rules for capability IDs (tool names, provider IDs, model
//! IDs, plugin IDs) so tools, providers, and hooks share consistent
//! validation behavior. New capability authors should reuse these helpers
//! rather than re-deriving the rules inline.

/// Maximum length for any capability identifier segment.
pub const MAX_ID_LENGTH: usize = 64;

/// Reserved characters that must not appear in capability IDs.
/// Currently `:` (used as namespace separator).
pub const RESERVED_CHARS: &[char] = &[':'];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdValidationError {
    Empty,
    TooLong { len: usize, max: usize },
    ContainsReserved { ch: char },
    ContainsWhitespace,
}

impl std::fmt::Display for IdValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "must not be empty"),
            Self::TooLong { len, max } => write!(f, "must be at most {max} chars (got {len})"),
            Self::ContainsReserved { ch } => {
                write!(f, "must not contain reserved character '{}'", ch)
            }
            Self::ContainsWhitespace => write!(f, "must not contain whitespace"),
        }
    }
}

impl std::error::Error for IdValidationError {}

/// Validate a capability ID segment. Used for tool names, provider IDs, model IDs.
pub fn validate_id_segment(id: &str) -> Result<(), IdValidationError> {
    if id.is_empty() {
        return Err(IdValidationError::Empty);
    }
    if id.len() > MAX_ID_LENGTH {
        return Err(IdValidationError::TooLong {
            len: id.len(),
            max: MAX_ID_LENGTH,
        });
    }
    if let Some(ch) = id.chars().find(|c| RESERVED_CHARS.contains(c)) {
        return Err(IdValidationError::ContainsReserved { ch });
    }
    if id.chars().any(|c| c.is_whitespace()) {
        return Err(IdValidationError::ContainsWhitespace);
    }
    Ok(())
}

/// Build a context-prefixed error message for validation failures.
///
/// Example: `validation_error("provider", "my:provider", err)` →
/// `"invalid provider 'my:provider': must not contain reserved character ':'"`.
pub fn validation_error(kind: &str, id: &str, err: IdValidationError) -> String {
    format!("invalid {} '{}': {}", kind, id, err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_id_is_rejected() {
        assert_eq!(validate_id_segment(""), Err(IdValidationError::Empty));
    }

    #[test]
    fn single_char_id_is_accepted() {
        assert!(validate_id_segment("a").is_ok());
    }

    #[test]
    fn reasonable_id_is_accepted() {
        assert!(validate_id_segment("foo-bar_baz.123").is_ok());
    }

    #[test]
    fn over_max_length_is_rejected() {
        let id = "a".repeat(MAX_ID_LENGTH + 1);
        assert_eq!(
            validate_id_segment(&id),
            Err(IdValidationError::TooLong {
                len: MAX_ID_LENGTH + 1,
                max: MAX_ID_LENGTH,
            })
        );
    }

    #[test]
    fn at_max_length_is_accepted() {
        let id = "a".repeat(MAX_ID_LENGTH);
        assert!(validate_id_segment(&id).is_ok());
    }

    #[test]
    fn reserved_colon_is_rejected() {
        assert_eq!(
            validate_id_segment("foo:bar"),
            Err(IdValidationError::ContainsReserved { ch: ':' })
        );
    }

    #[test]
    fn space_is_rejected() {
        assert_eq!(
            validate_id_segment("foo bar"),
            Err(IdValidationError::ContainsWhitespace)
        );
    }

    #[test]
    fn tab_is_rejected() {
        assert_eq!(
            validate_id_segment("foo\tbar"),
            Err(IdValidationError::ContainsWhitespace)
        );
    }

    #[test]
    fn validation_error_formats_context_and_cause() {
        let msg = validation_error(
            "tool",
            "x:y",
            IdValidationError::ContainsReserved { ch: ':' },
        );
        assert!(msg.contains("invalid tool 'x:y'"), "msg={msg}");
        assert!(msg.contains("':'"), "msg={msg}");
    }

    #[test]
    fn empty_error_displays_human_readable() {
        let msg = format!("{}", IdValidationError::Empty);
        assert_eq!(msg, "must not be empty");
    }

    #[test]
    fn too_long_error_displays_lengths() {
        let msg = format!("{}", IdValidationError::TooLong { len: 65, max: 64 });
        assert!(msg.contains("65"));
        assert!(msg.contains("64"));
    }
}
