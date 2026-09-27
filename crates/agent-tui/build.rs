//! Build script: capture the git commit the binary was built from, so the TUI
//! can display it next to the version. Resolved at compile time and baked in
//! via `GIT_HASH` — no runtime git shelling.
//!
//! IMPORTANT: only trust git when its repo root IS our own workspace root.
//! Source-tarball builds (AUR, Homebrew, crates.io) are often extracted inside
//! an UNRELATED git repo — e.g. the AUR package's own checkout — and a naive
//! `git rev-parse` there walks up and returns that repo's hash plus a spurious
//! `-dirty` (the extracted files look untracked). For any packaged build we
//! fall back to `release` instead of baking a misleading hash.
//!
//! Second job (PLAN-phase4 §3 C1): write a **curated** syntect `SyntaxSet`
//! dump to `$OUT_DIR/curated_newlines.packdump`. The default set ships 75
//! grammars; a coding agent renders a dozen. `CURATED` lists the keepers and
//! `curated_syntax_dump` pulls in the transitive closure of every grammar they
//! `include`/`embed` (linked `Direct` context ids in the dump), so nothing
//! dangles and colours are byte-identical to the full set
//! (`tests/highlight_curated.rs` is the golden). Runtime kill-switch:
//! `SYNAPS_TUI_SYNTECT=full`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

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
    let git_hash = synaps_repo_hash().unwrap_or_else(|| "release".to_string());
    println!("cargo:rustc-env=GIT_HASH={git_hash}");

    curated_syntax_dump();

    // Rebuild when HEAD moves or the index changes so the hash stays accurate
    // in-repo. Harmless (ignored) when these paths don't exist in a tarball.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");
}

/// Short commit (`+ "-dirty"`) ONLY when building inside the real synaps repo.
/// Returns `None` for any other situation (no git, or git resolves to a
/// different/unrelated repository).
fn synaps_repo_hash() -> Option<String> {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").ok()?; // .../crates/agent-tui
                                                              // Our workspace root is two levels up from this crate dir.
    let ws_root = Path::new(&manifest)
        .parent()?
        .parent()?
        .canonicalize()
        .ok()?;

    // Where does git think the repo root is, starting from here?
    let toplevel = Command::new("git")
        .args(["-C", &manifest, "rev-parse", "--show-toplevel"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| Path::new(s.trim()).canonicalize().ok())?;

    // If git's repo root isn't OUR workspace root, we're inside an unrelated
    // repo (tarball extracted in some package's git dir) — don't trust it.
    if toplevel != ws_root {
        return None;
    }

    let hash = Command::new("git")
        .args(["-C", &manifest, "rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())?;

    let dirty = Command::new("git")
        .args([
            "-C",
            &manifest,
            "status",
            "--porcelain",
            "--untracked-files=no",
        ])
        .output()
        .ok()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);

    Some(if dirty { format!("{hash}-dirty") } else { hash })
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
