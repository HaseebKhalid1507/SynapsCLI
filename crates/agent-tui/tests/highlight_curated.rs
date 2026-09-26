//! C1 golden (PLAN-phase4 §3 C1 / §8 C1): the curated syntect dump that
//! `build.rs` writes to `$OUT_DIR` must highlight byte-identically to syntect's
//! full default set for every language a coding agent renders. If a fixture
//! differs, the include closure in `build.rs` is incomplete.
//!
//! Tests the dump directly (same bytes `highlight.rs` embeds), so the private
//! `tui::highlight` module needs no test facade.

use syntect::easy::HighlightLines;
use syntect::highlighting::{Style, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

static CURATED_DUMP: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/curated_newlines.packdump"));

/// Mirror of `build.rs::CURATED` — every name must resolve in the dump.
const CURATED: &[&str] = &[
    "Plain Text",
    "Rust",
    "Python",
    "JavaScript",
    "JSON",
    "Go",
    "C",
    "C++",
    "Java",
    "Ruby",
    "PHP",
    "Bourne Again Shell (bash)",
    "Shell-Unix-Generic",
    "YAML",
    "Markdown",
    "HTML",
    "CSS",
    "XML",
    "SQL",
    "Makefile",
    "Diff",
];

/// Extensions the render paths look up (`find_syntax_by_extension` /
/// `find_syntax_by_token`) — must resolve to a non-plain grammar.
const EXTENSIONS: &[&str] = &[
    "rs", "py", "js", "json", "go", "c", "h", "cpp", "cc", "java", "rb", "php", "sh", "bash",
    "zsh", "fish", "yaml", "yml", "md", "html", "css", "xml", "sql", "mk", "diff", "patch",
];

fn curated() -> SyntaxSet {
    syntect::dumps::from_uncompressed_data(CURATED_DUMP).expect("curated dump decodes")
}

fn full() -> SyntaxSet {
    SyntaxSet::load_defaults_newlines()
}

fn highlight(ss: &SyntaxSet, token: &str, code: &str) -> Vec<Vec<(Style, String)>> {
    let ts = ThemeSet::load_defaults();
    let theme = &ts.themes["base16-ocean.dark"];
    let syntax = ss
        .find_syntax_by_token(token)
        .unwrap_or_else(|| ss.find_syntax_plain_text());
    let mut h = HighlightLines::new(syntax, theme);
    LinesWithEndings::from(code)
        .map(|line| {
            h.highlight_line(line, ss)
                .unwrap_or_default()
                .into_iter()
                .map(|(st, t)| (st, t.to_string()))
                .collect()
        })
        .collect()
}

#[test]
fn curated_dump_is_smaller_than_full() {
    let c = curated();
    let f = full();
    assert!(c.syntaxes().len() < f.syntaxes().len());
    assert!(CURATED_DUMP.len() < syntect::dumps::dump_binary(&f).len() * 3 / 4);
}

#[test]
fn every_curated_name_resolves() {
    let ss = curated();
    for name in CURATED {
        assert!(
            ss.find_syntax_by_name(name).is_some(),
            "missing grammar {name:?}"
        );
    }
    assert_eq!(ss.find_syntax_plain_text().name, "Plain Text");
}

#[test]
fn every_agent_extension_resolves_to_a_grammar() {
    let ss = curated();
    for ext in EXTENSIONS {
        let s = ss
            .find_syntax_by_extension(ext)
            .unwrap_or_else(|| panic!("extension {ext:?} did not resolve"));
        assert_ne!(s.name, "Plain Text", "extension {ext:?} fell to plain text");
    }
}

#[test]
fn unknown_language_falls_back_to_plain_no_panic() {
    let ss = curated();
    for token in [
        "brainfuck",
        "ts",
        "toml",
        "kotlin",
        "swift",
        "dockerfile",
        "",
    ] {
        let out = highlight(&ss, token, "let x = 1;\nfoo bar\n");
        assert_eq!(out.len(), 2);
        // Same fallback as the full set.
        assert_eq!(out, highlight(&full(), token, "let x = 1;\nfoo bar\n"));
    }
}

#[test]
fn golden_curated_equals_full_on_fixtures() {
    let c = curated();
    let f = full();
    let fixtures: &[(&str, &str)] = &[
        ("rs", include_str!("fixtures/highlight/sample.rs.txt")),
        ("py", include_str!("fixtures/highlight/sample.py.txt")),
        ("js", include_str!("fixtures/highlight/sample.js.txt")),
        ("go", include_str!("fixtures/highlight/sample.go.txt")),
        ("sh", include_str!("fixtures/highlight/sample.sh.txt")),
        ("json", include_str!("fixtures/highlight/sample.json.txt")),
        ("yaml", include_str!("fixtures/highlight/sample.yaml.txt")),
        ("md", include_str!("fixtures/highlight/sample.md.txt")),
        ("diff", include_str!("fixtures/highlight/sample.diff.txt")),
        ("sql", include_str!("fixtures/highlight/sample.sql.txt")),
        ("html", include_str!("fixtures/highlight/sample.html.txt")),
        ("c", include_str!("fixtures/highlight/sample.c.txt")),
        ("rb", include_str!("fixtures/highlight/sample.rb.txt")),
        ("php", include_str!("fixtures/highlight/sample.php.txt")),
    ];
    for (token, code) in fixtures {
        let a = highlight(&c, token, code);
        let b = highlight(&f, token, code);
        assert!(!a.is_empty(), "{token}: empty highlight");
        // Every fixture must actually exercise a grammar (not fall to plain).
        assert!(
            a.iter()
                .flatten()
                .map(|(s, _)| s.foreground)
                .collect::<std::collections::HashSet<_>>()
                .len()
                > 1,
            "{token}: fixture highlighted as a single colour"
        );
        assert_eq!(a, b, "{token}: curated vs full highlight differ");
    }
}
