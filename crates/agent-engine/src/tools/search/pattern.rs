//! Patterns for the search tools: grep's regex and its `include` file-name
//! glob, and find's name glob.

use globset::{GlobBuilder, GlobMatcher};
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use ignore::types::{Types, TypesBuilder};

/// Longest pattern accepted. Real patterns are far shorter; this bounds parse
/// work before the size limit applies.
pub(crate) const MAX_PATTERN_BYTES: usize = 4096;

/// Compiled-regex and lazy-DFA cache limits (grep-regex defaults to 100 MiB
/// and 1000 MiB). A pattern over this is an error, not unbounded memory.
pub(crate) const REGEX_SIZE_LIMIT: usize = 10 << 20;

const REGEX_HINT: &str = "Hint: the pattern is a Rust regex (ripgrep syntax). Escape \
     ( ) [ ] { } * + ? . | ^ $ \\ with a backslash to match them literally. Lookaround and \
     backreferences are not supported.";

/// grep's matcher: Rust regex syntax, one line at a time. Multi-line mode,
/// as in ripgrep: the searcher hands the matcher whole buffers, so `^` and
/// `$` must anchor at each line, not only at the buffer's ends. `$` also
/// matches before a `\r\n` line ending.
pub(crate) fn content_matcher(pattern: &str) -> Result<RegexMatcher, String> {
    if pattern.len() > MAX_PATTERN_BYTES {
        return Err(format!(
            "pattern is {} bytes, over the {MAX_PATTERN_BYTES}-byte limit",
            pattern.len()
        ));
    }
    RegexMatcherBuilder::new()
        .multi_line(true)
        .crlf(true)
        .size_limit(REGEX_SIZE_LIMIT)
        .dfa_size_limit(REGEX_SIZE_LIMIT)
        .build(pattern)
        .map_err(|err| format!("invalid regex: {err}\n{REGEX_HINT}"))
}

/// grep's `include`: a glob on the file name only, like `grep --include`
/// (`*.rs`, `*.{ts,tsx}`). Directories are never filtered, and an explicit
/// file path is searched whatever its name.
pub(crate) fn include_filter(glob: &str) -> Result<Types, String> {
    let mut builder = TypesBuilder::new();
    builder
        .add("include", glob)
        .map_err(|err| format!("invalid include glob: {err}"))?;
    builder.select("include");
    builder
        .build()
        .map_err(|err| format!("invalid include glob: {err}"))
}

/// find's `pattern`: a glob on the file name, as `find -name` took it
/// (`*.rs`, `Cargo.*`, `[!.]*`), plus braces (`*.{rs,toml}`). Case-sensitive;
/// `*` matches a leading `.`. A name never holds a path separator, so a
/// pattern with one (`src/*.rs`, `**/x`) could never match: that is an error
/// pointing at `path`, where `find -name` silently found nothing.
pub(crate) fn name_glob(glob: &str) -> Result<GlobMatcher, String> {
    if glob.len() > MAX_PATTERN_BYTES {
        return Err(format!(
            "pattern is {} bytes, over the {MAX_PATTERN_BYTES}-byte limit",
            glob.len()
        ));
    }
    if glob.chars().any(std::path::is_separator) {
        return Err(format!(
            "invalid pattern {glob:?}: it is matched against file names, which never contain \
             a path separator. Put the directory in `path` and the name glob in `pattern` \
             (path \"src\", pattern \"*.rs\" finds .rs files at any depth under src)."
        ));
    }
    GlobBuilder::new(glob)
        .build()
        .map(|glob| glob.compile_matcher())
        .map_err(|err| format!("invalid pattern: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use grep_matcher::Matcher;

    fn matches(pattern: &str, line: &str) -> bool {
        content_matcher(pattern)
            .unwrap()
            .is_match(line.as_bytes())
            .unwrap()
    }

    /// Rust regex: alternation, repetition, classes, word boundaries and
    /// inline flags work as written.
    #[test]
    fn dialect_is_rust_regex() {
        assert!(matches("alpha|beta", "x beta"));
        assert!(matches("ab+c", "abbbc"));
        assert!(matches("colou?r", "color"));
        assert!(matches("^x{2}$", "xx"));
        assert!(!matches("^x{2}$", "xxx"));
        // `^`/`$` anchor at each line (CRLF too), not only the ends.
        assert!(matches("^x$", "a\r\nx\r\nb"));
        assert!(!matches("^x$", "a\r\nxx\r\nb"));
        assert!(matches(r"\d+", "v42"));
        assert!(matches(r"\bfoo\b", "a foo b"));
        assert!(!matches(r"\bfoo\b", "foobar"));
        assert!(matches("(?i)hello", "HeLLo"));
        assert!(matches(r"foo\(", "foo("));
        // `\|` is a literal pipe, not GNU's BRE alternation.
        assert!(matches(r"a\|b", "a|b"));
        assert!(!matches(r"a\|b", "a"));
    }

    /// A pattern that doesn't parse is an error with the parser's message and
    /// a hint, never a literal search.
    #[test]
    fn invalid_regex_is_an_error_with_a_hint() {
        for bad in ["fn main() {", "*foo", "(a)\\1", "(?=x)", "a\nb", "[z-a]"] {
            let err = content_matcher(bad).unwrap_err();
            assert!(err.starts_with("invalid regex: "), "{bad:?}: {err}");
            assert!(
                err.contains("Hint: the pattern is a Rust regex"),
                "{bad:?}: {err}"
            );
        }
    }

    /// A pathological regex hits the size limit instead of using unbounded
    /// memory; an oversized pattern is refused before parsing.
    #[test]
    fn size_limits_refuse_pathological_patterns() {
        let err = content_matcher(r"\w{1000}").unwrap_err();
        assert!(err.contains("size limit"), "{err}");
        let err = content_matcher(&"a".repeat(MAX_PATTERN_BYTES + 1)).unwrap_err();
        assert!(err.contains("byte limit"), "{err}");
        assert!(content_matcher(&"a".repeat(MAX_PATTERN_BYTES)).is_ok());
    }

    /// find's glob works on names: fnmatch-like wildcards and classes,
    /// braces, case-sensitive, `*` matching a leading dot.
    #[test]
    fn name_glob_matches_file_names() {
        let is = |glob: &str, name: &str| name_glob(glob).unwrap().is_match(name);
        assert!(is("*.rs", "main.rs"));
        assert!(is("*.rs", ".hidden.rs"));
        assert!(!is("*.rs", "main.rs.bak"));
        assert!(is("Cargo.*", "Cargo.toml"));
        assert!(is("Cargo.*", "Cargo.lock"));
        assert!(!is("Cargo.*", "cargo.toml"));
        assert!(is("*.{rs,toml}", "a.toml"));
        assert!(is("?.txt", "a.txt"));
        assert!(!is("?.txt", "ab.txt"));
        assert!(is("[!.]*", "x"));
        assert!(!is("[!.]*", ".x"));
        assert!(is("*", "."));
        assert!(!is("", "a"));
    }

    /// A separator can't match a name: an error naming `path`, never a
    /// silent "No files found.".
    #[test]
    fn name_glob_with_a_separator_is_an_error() {
        for glob in ["src/*.rs", "**/*.rs", "/x", "a/"] {
            let err = name_glob(glob).unwrap_err();
            assert!(err.starts_with("invalid pattern "), "{glob}: {err}");
            assert!(err.contains("`path`"), "{glob}: {err}");
        }
        #[cfg(windows)]
        assert!(name_glob(r"src\*.rs").is_err());
    }

    #[test]
    fn bad_name_glob_is_an_error_with_the_parse_message() {
        let err = name_glob("[").unwrap_err();
        assert!(err.starts_with("invalid pattern: "), "{err}");
        assert!(err.contains("unclosed character class"), "{err}");
        let err = name_glob("{a,b").unwrap_err();
        assert!(err.starts_with("invalid pattern: "), "{err}");
        assert!(name_glob(&"a".repeat(MAX_PATTERN_BYTES + 1)).is_err());
    }

    #[test]
    fn bad_include_glob_is_an_error() {
        assert!(include_filter("*.rs").is_ok());
        assert!(include_filter("*.{rs,toml}").is_ok());
        let err = include_filter("[").unwrap_err();
        assert!(err.starts_with("invalid include glob"), "{err}");
    }
}
