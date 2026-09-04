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

/// Source-grep guard (risk §6.5): the daemon and socket transport never
/// trace an `Answer` body.
#[test]
fn daemon_never_traces_answer_bodies() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("crates/agent-engine/src");
    let files = [
        root.join("daemon/mod.rs"),
        root.join("daemon/conn.rs"),
        root.join("daemon/listener.rs"),
        root.join("daemon/lifecycle.rs"),
        root.join("daemon/registry.rs"),
        root.join("session/socket_transport.rs"),
    ];
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
