//! `synaps-engine` (where the daemon lives) must have no dependency path to
//! `synaps-tui` (S289 D-1 amended; PLAN-phase2 §5.3). Uses `cargo metadata`
//! so the check is about the graph, not about what happened to link.

use std::collections::{HashMap, HashSet, VecDeque};
use std::process::Command;

#[test]
fn daemon_no_tui_dep() {
    let out = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("cargo metadata");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let meta: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let pkgs = meta["packages"].as_array().unwrap();
    // name → set of dependency package names (workspace members only)
    let mut deps: HashMap<String, HashSet<String>> = HashMap::new();
    for p in pkgs {
        let name = p["name"].as_str().unwrap().to_string();
        let set: HashSet<String> = p["dependencies"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|d| d["kind"].is_null()) // normal deps only (dev-deps excluded)
            .map(|d| d["name"].as_str().unwrap().to_string())
            .collect();
        deps.insert(name, set);
    }
    assert!(deps.contains_key("synaps-engine"), "workspace has synaps-engine");
    assert!(deps.contains_key("synaps-tui"), "workspace has synaps-tui");
    // BFS from synaps-engine over normal deps
    let mut seen = HashSet::new();
    let mut q = VecDeque::from(["synaps-engine".to_string()]);
    while let Some(n) = q.pop_front() {
        if !seen.insert(n.clone()) {
            continue;
        }
        if let Some(ds) = deps.get(&n) {
            for d in ds {
                q.push_back(d.clone());
            }
        }
    }
    assert!(!seen.contains("synaps-tui"), "synaps-engine reaches synaps-tui: {seen:?}");
}

fn rs_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            out.extend(rs_files(&p));
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
    out.sort();
    out
}

/// Source-grep guard (risk §6.5, PLAN-phase3 C2): nothing under
/// `crates/agent-engine/src/{session,daemon}/**` traces an `Answer` body or
/// a prompt value.
#[test]
fn daemon_never_traces_answer_bodies() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("crates/agent-engine/src");
    let mut files = rs_files(&root.join("daemon"));
    files.extend(rs_files(&root.join("session")));
    assert!(files.len() >= 12, "expected the daemon + session trees: {files:?}");
    for f in files {
        let src = std::fs::read_to_string(&f).unwrap();
        for (i, line) in src.lines().enumerate() {
            if line.contains("tracing::") {
                let l = line.to_ascii_lowercase();
                assert!(!l.contains("answer") && !l.contains("value"), "{}:{}: {line}", f.display(), i + 1);
            }
        }
    }
}

/// PLAN-phase3 §2.2 / Appendix A: `SessionCommand` has a MANUAL `Debug`
/// (bodies redacted, no lengths) and no derive; `ClientFrame` likewise; no
/// `chars().count()` in either Debug impl (a length is a side channel).
#[test]
fn session_command_debug_is_manual_and_lengthless() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("crates/agent-engine/src/session");
    let types = std::fs::read_to_string(root.join("types.rs")).unwrap();
    let wire = std::fs::read_to_string(root.join("wire.rs")).unwrap();

    assert_eq!(
        types.matches("impl std::fmt::Debug for SessionCommand").count(),
        1,
        "SessionCommand needs exactly one manual Debug impl"
    );
    // No `#[derive(..Debug..)]` directly above `pub enum SessionCommand`.
    let lines: Vec<&str> = types.lines().collect();
    let at = lines
        .iter()
        .position(|l| l.starts_with("pub enum SessionCommand"))
        .expect("pub enum SessionCommand");
    let mut k = at;
    while k > 0 {
        k -= 1;
        let l = lines[k].trim();
        if l.starts_with("#[") {
            assert!(!l.contains("derive(") || !l.contains("Debug"), "types.rs:{}: {l}", k + 1);
            continue;
        }
        if l.starts_with("///") || l.starts_with("//") {
            continue;
        }
        break;
    }
    assert_eq!(types.matches("impl std::fmt::Debug for ClientFrame").count(), 0);
    assert_eq!(wire.matches("impl std::fmt::Debug for ClientFrame").count(), 1);

    for (name, src) in [("types.rs", &types), ("wire.rs", &wire)] {
        for (i, line) in src.lines().enumerate() {
            assert!(!line.contains("chars().count()"), "{name}:{}: {line}", i + 1);
        }
    }
    // The redaction marker exists and the redacted variants carry no length.
    let dbg_start = types.find("impl std::fmt::Debug for SessionCommand").unwrap();
    let dbg = &types[dbg_start..];
    let dbg_end = dbg.find("\n}\n").map(|e| e + 3).unwrap_or(dbg.len());
    let dbg = &dbg[..dbg_end];
    assert!(dbg.contains("<redacted>"), "SessionCommand Debug redacts bodies");
    // `attachments.len()` is a count of attachments (P3-0 design), not a
    // body length; any other `.len()` inside the impl is a side channel.
    for (i, line) in dbg.lines().enumerate() {
        if line.contains(".len()") {
            assert!(line.contains("attachments.len()"), "SessionCommand Debug line {}: {line}", i + 1);
        }
    }
}
