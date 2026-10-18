//! Build script. The build id the header shows is not made here: it is
//! the engine's `SYNAPS_BUILD_ID` (agent-engine/build.rs), the same one the
//! version-skew check compares (04-tui M8).
//!
//! Its job (PLAN-phase4 §3 C1): write a **curated** syntect `SyntaxSet`
//! dump to `$OUT_DIR/curated_newlines.packdump`. The default set ships 75
//! grammars; a coding agent renders a dozen. `CURATED` lists the keepers and
//! `curated_syntax_dump` pulls in the transitive closure of every grammar they
//! `include`/`embed` (linked `Direct` context ids in the dump), so nothing
//! dangles and colours are byte-identical to the full set
//! (`tests/highlight_curated.rs` is the golden). Runtime kill-switch:
//! `SYNAPS_TUI_SYNTECT=full`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use syntect::parsing::{SyntaxDefinition, SyntaxSet, SyntaxSetBuilder};

/// Grammars kept in the curated dump, by syntect `name`. Only names that exist
/// in syntect's default newlines set belong here (the test asserts each one
/// resolves). Not in syntect's defaults at all — they fall back to Plain Text
/// in both curated and full sets: TypeScript/TSX, TOML, Kotlin, Swift,
/// Dockerfile, fish (uses the bash grammar via the `fish` extension), INI.
/// Adding a language is a one-line change.
pub const CURATED: &[&str] = &[
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

fn main() {
    curated_syntax_dump();
}

/// Build `$OUT_DIR/curated_newlines.packdump` (uncompressed outer layer, like
/// syntect's own asset — per-syntax contexts are already deflated).
fn curated_syntax_dump() {
    println!("cargo:rerun-if-env-changed=SYNAPS_TUI_SYNTECT_REPORT");
    let out = std::env::var("OUT_DIR").expect("OUT_DIR");
    let path = Path::new(&out).join("curated_newlines.packdump");

    let full = SyntaxSet::load_defaults_newlines();
    let full_unlinked = full.find_unlinked_contexts();
    let full_len = syntect::dumps::dump_binary(&full).len();
    let defs: Vec<SyntaxDefinition> = full.into_builder().syntaxes().to_vec();

    // Every linked reference is a `Direct(ContextId { syntax_index, .. })`
    // whose fields are crate-private; go through serde_json to read and
    // rewrite them. `syntax_index` is the only key of that name in the tree.
    let mut values: Vec<serde_json::Value> = defs
        .iter()
        .map(|d| serde_json::to_value(d).expect("SyntaxDefinition → json"))
        .collect();

    let mut keep: BTreeSet<usize> = BTreeSet::new();
    let mut work: Vec<usize> = Vec::new();
    for name in CURATED {
        let idx = defs
            .iter()
            .position(|d| d.name == *name)
            .unwrap_or_else(|| panic!("CURATED grammar {name:?} is not in syntect's default set"));
        if keep.insert(idx) {
            work.push(idx);
        }
    }
    while let Some(idx) = work.pop() {
        let mut refs = BTreeSet::new();
        collect_syntax_indices(&values[idx], &mut refs);
        for r in refs {
            if keep.insert(r) {
                work.push(r);
            }
        }
    }

    let remap: BTreeMap<usize, usize> = keep.iter().enumerate().map(|(n, &o)| (o, n)).collect();
    let mut builder = SyntaxSetBuilder::new();
    for &old in &keep {
        let mut v = std::mem::take(&mut values[old]);
        remap_syntax_indices(&mut v, &remap);
        let def: SyntaxDefinition = serde_json::from_value(v).expect("json → SyntaxDefinition");
        builder.add(def);
    }
    let curated = builder.build();

    // Closure sanity: the curated set may not introduce dangling references
    // the full set did not already have.
    let new_unlinked: Vec<_> = curated
        .find_unlinked_contexts()
        .difference(&full_unlinked)
        .cloned()
        .collect();
    assert!(
        new_unlinked.is_empty(),
        "curated syntect set has dangling references (include closure incomplete): {new_unlinked:?}"
    );

    syntect::dumps::dump_to_uncompressed_file(&curated, &path).expect("write curated packdump");

    if std::env::var_os("SYNAPS_TUI_SYNTECT_REPORT").is_some() {
        let curated_len = syntect::dumps::dump_binary(&curated).len();
        let names: Vec<&str> = curated.syntaxes().iter().map(|s| s.name.as_str()).collect();
        println!(
            "cargo:warning=syntect curated dump: {} syntaxes / {} bytes vs full {} syntaxes / {} bytes ({:.0}%): {}",
            curated.syntaxes().len(),
            curated_len,
            defs.len(),
            full_len,
            100.0 * curated_len as f64 / full_len as f64,
            names.join(", ")
        );
    }
}

fn collect_syntax_indices(v: &serde_json::Value, out: &mut BTreeSet<usize>) {
    match v {
        serde_json::Value::Object(m) => {
            if let Some(i) = m.get("syntax_index").and_then(|i| i.as_u64()) {
                out.insert(i as usize);
            }
            m.values().for_each(|x| collect_syntax_indices(x, out));
        }
        serde_json::Value::Array(a) => a.iter().for_each(|x| collect_syntax_indices(x, out)),
        _ => {}
    }
}

fn remap_syntax_indices(v: &mut serde_json::Value, remap: &BTreeMap<usize, usize>) {
    match v {
        serde_json::Value::Object(m) => {
            if let Some(i) = m.get("syntax_index").and_then(|i| i.as_u64()) {
                let new = remap[&(i as usize)];
                m.insert("syntax_index".into(), serde_json::Value::from(new as u64));
            }
            m.values_mut().for_each(|x| remap_syntax_indices(x, remap));
        }
        serde_json::Value::Array(a) => a.iter_mut().for_each(|x| remap_syntax_indices(x, remap)),
        _ => {}
    }
}
