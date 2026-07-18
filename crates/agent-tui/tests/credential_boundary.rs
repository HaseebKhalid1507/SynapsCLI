//! Checkpoint-1 boundary ratchet: the TUI layer never reads credentials
//! directly. All credential use (tokens, static keys, the Anthropic usage
//! call) must go through the configured `CredentialBroker`.
//!
//! This is a source-level scan of `crates/agent-tui/src`: forbidden patterns
//! are direct credential-file reads, credential env vars, and hardcoded
//! provider auth endpoints. If one of these reappears, the broker boundary
//! has been bypassed — fix the code, don't widen this list.

use std::path::{Path, PathBuf};

const FORBIDDEN: &[(&str, &str)] = &[
    ("auth.json", "direct credential-file access"),
    ("ANTHROPIC_API_KEY", "credential env var read/guidance"),
    (
        "api.anthropic.com",
        "hardcoded provider endpoint (must be broker-pinned)",
    ),
    ("load_provider_auth", "broker-internal credential loader"),
    ("load_static_key", "broker-internal static key loader"),
    ("bearer_auth", "TUI must never attach a bearer credential"),
];

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("readable src dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn tui_sources_contain_no_direct_credential_access() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_sources(&src, &mut files);
    assert!(!files.is_empty(), "expected TUI sources under {src:?}");

    let mut violations = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).expect("readable source file");
        for (needle, why) in FORBIDDEN {
            for (idx, line) in text.lines().enumerate() {
                if line.contains(needle) {
                    violations.push(format!(
                        "{}:{}: `{}` ({why})",
                        file.display(),
                        idx + 1,
                        needle
                    ));
                }
            }
        }
    }
    assert!(
        violations.is_empty(),
        "TUI credential-boundary violations:\n{}",
        violations.join("\n")
    );
}
