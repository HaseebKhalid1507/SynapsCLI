//! PLAN-phase3 §3.2 / Appendix A guards for the TUI port: after A2 the TUI
//! never writes the journal and never owns a `Runtime` — the `SessionActor`
//! does (in-process via `LocalTransport`, over the socket via
//! `SocketTransport`). Presentation-only code is all that remains.

use std::path::Path;

fn tui_src() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("crates/agent-tui/src")
}

fn walk(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read_dir") {
        let p = entry.expect("entry").path();
        if p.is_dir() {
            walk(&p, out);
        } else if p.extension().is_some_and(|e| e == "rs") {
            out.push(p);
        }
    }
}

/// Lines that are code (not `//` comments) and match any needle.
fn offenders(needles: &[&str], skip_tests: bool) -> Vec<String> {
    let mut files = Vec::new();
    walk(&tui_src(), &mut files);
    let mut hits = Vec::new();
    for f in files {
        let rel = f.strip_prefix(tui_src()).unwrap().display().to_string();
        if skip_tests && rel.starts_with("tui/testing") {
            continue;
        }
        let src = std::fs::read_to_string(&f).unwrap();
        let mut in_tests = false;
        for (i, line) in src.lines().enumerate() {
            if line.trim_start().starts_with("#[cfg(test)]") {
                in_tests = true;
            }
            if skip_tests && in_tests {
                continue;
            }
            let code = line.split("//").next().unwrap_or("");
            if needles.iter().any(|n| code.contains(n)) {
                hits.push(format!("{rel}:{}: {}", i + 1, line.trim()));
            }
        }
    }
    hits
}

#[test]
fn tui_never_writes_the_journal() {
    let hits = offenders(
        &[
            "save_session",
            "Session::save",
            ".session.save(",
            "save_session_in_dir",
            "run_stream_with_messages",
            "event_queue()",
            "apply_compaction(",
            "compact_conversation(",
        ],
        false,
    );
    assert!(hits.is_empty(), "journal/turn-machine code in the TUI:\n{}", hits.join("\n"));
}

#[test]
fn tui_holds_no_runtime_outside_tests() {
    let hits = offenders(
        &["synaps_cli::Runtime ", "synaps_cli::Runtime,", "synaps_cli::Runtime)", "synaps_cli::Runtime>", "&Runtime ", "&Runtime)", "&Runtime,", "&mut Runtime", "Runtime::new("],
        true,
    );
    assert!(hits.is_empty(), "Runtime in TUI production code:\n{}", hits.join("\n"));
}
