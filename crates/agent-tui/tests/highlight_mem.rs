//! C1/C2 heap numbers (PLAN-phase4 §8.4). Ignored; run in release on bella:
//!
//! ```text
//! cargo test -p synaps-tui --release --test highlight_mem -- --ignored --nocapture
//! ```
//!
//! Uses jemalloc as the global allocator (like the `synaps` binary) so
//! `jemalloc_allocated_kb` is the live heap and `purge_arenas()` makes RssAnon
//! honest. Measures the curated dump vs `load_defaults_newlines()` in the same
//! process: load + first Rust highlight, more languages, then drop (= C2
//! eviction) — what comes back is what eviction buys.
#![cfg(all(unix, not(target_env = "musl")))]

use std::time::Instant;

use agent_core::core::memstat::{purge_arenas, self_snapshot};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

static CURATED_DUMP: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/curated_newlines.packdump"));

const FIXTURES: &[(&str, &str)] = &[
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
];

fn hl(ss: &SyntaxSet, theme: &Theme, token: &str, code: &str) -> usize {
    let syntax = ss
        .find_syntax_by_token(token)
        .unwrap_or_else(|| ss.find_syntax_plain_text());
    let mut h = HighlightLines::new(syntax, theme);
    LinesWithEndings::from(code)
        .map(|l| h.highlight_line(l, ss).map(|r| r.len()).unwrap_or(0))
        .sum()
}

fn snap() -> (u64, u64) {
    purge_arenas();
    let s = self_snapshot();
    (s.jemalloc_allocated_kb, s.rss_anon_kb)
}

fn theme() -> Theme {
    ThemeSet::load_defaults()
        .themes
        .remove("base16-ocean.dark")
        .unwrap()
}

fn run(label: &str, load: fn() -> SyntaxSet) {
    let (a0, r0) = snap();
    let t = Instant::now();
    let ss = load();
    let th = theme();
    let load_ms = t.elapsed().as_millis();
    let t = Instant::now();
    hl(&ss, &th, "rs", FIXTURES[0].1);
    let first_ms = t.elapsed().as_millis();
    let (a1, r1) = snap();
    for (tok, code) in FIXTURES {
        hl(&ss, &th, tok, code);
    }
    let (a2, r2) = snap();
    drop(ss);
    drop(th);
    let (a3, r3) = snap();
    eprintln!(
        "highlight_mem {label}: load_ms={load_ms} first_rust_hl_ms={first_ms} \
         alloc_kb: base={a0} +rust={} +10langs={} after_drop={:+} | \
         rss_anon_kb: base={r0} +rust={} +10langs={} after_drop={:+}",
        a1 as i64 - a0 as i64,
        a2 as i64 - a0 as i64,
        a3 as i64 - a2 as i64,
        r1 as i64 - r0 as i64,
        r2 as i64 - r0 as i64,
        r3 as i64 - r2 as i64,
    );
}

#[test]
#[ignore]
fn measure_curated_vs_full() {
    eprintln!(
        "highlight_mem dump bytes: curated={} full={}",
        CURATED_DUMP.len(),
        syntect::dumps::dump_binary(&SyntaxSet::load_defaults_newlines()).len()
    );
    for _ in 0..3 {
        run("curated", || {
            syntect::dumps::from_uncompressed_data(CURATED_DUMP).unwrap()
        });
        run("full   ", SyntaxSet::load_defaults_newlines);
    }
    // The pre-C3 shape: full set + the whole ThemeSet retained.
    let (a0, _) = snap();
    let ss = SyntaxSet::load_defaults_newlines();
    let ts = ThemeSet::load_defaults();
    hl(&ss, &ts.themes["base16-ocean.dark"], "rs", FIXTURES[0].1);
    let (a1, _) = snap();
    let ts_only = ThemeSet::load_defaults();
    let (a2, _) = snap();
    eprintln!(
        "highlight_mem pre-C3 (full set + ThemeSet) +rust alloc_kb={} ; ThemeSet alone alloc_kb={}",
        a1 as i64 - a0 as i64,
        a2 as i64 - a1 as i64
    );
    drop(ts_only);
}
