//! PLAN-phase3 §5.1 layer 3 — the terminal differential against the
//! REFERENCE BINARY built at `f0ee1e62` (the in-process TUI before the port):
//! the only oracle independent of the actor's author.
//!
//! For each scenario, three tmux panes run identical `send-keys` scripts
//! against the same scripted provider stub: (R) the reference binary,
//! (L) this binary in-process, (S) `synaps daemon --foreground` + this
//! binary `--attach` (when `SYNAPS_TUI_E2E_SOCKET=1`). After each step the
//! pane is captured, normalised and diffed. The journals are diffed too.
//!
//! Ignored unless `SYNAPS_TUI_E2E=1`; needs `SYNAPS_REF_BIN=<path>` and
//! `tmux`. Run via `scripts/tui-e2e/differential.sh`.

#[path = "support/phase2/mod.rs"]
#[allow(dead_code)]
mod phase2;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use phase2::{spawn_stub, Script, ANTHROPIC_SSE, ANTHROPIC_SSE_PREFIX, ANTHROPIC_SSE_TOOL_USE};

const COLS: u16 = 100;
const ROWS: u16 = 30;
const TMUX: &str = "tuidiff";

enum Step {
    /// `tmux send-keys` literal text (no Enter).
    Type(&'static str),
    /// A tmux key name (`Enter`, `Escape`, …).
    Key(&'static str),
    /// Settle then capture (label for the diff).
    Capture(&'static str),
    Wait(u64),
}

struct Scenario {
    name: &'static str,
    script: Script,
    steps: Vec<Step>,
}

fn scenarios() -> Vec<Scenario> {
    use Step::*;
    vec![
        Scenario {
            name: "plain_turn",
            script: Script::Sse(ANTHROPIC_SSE),
            steps: vec![
                Type("hello"),
                Key("Enter"),
                Wait(1500),
                Capture("after_turn"),
            ],
        },
        Scenario {
            name: "tool_loop",
            script: Script::SeqSse(&[ANTHROPIC_SSE_TOOL_USE, ANTHROPIC_SSE]),
            steps: vec![
                Type("list it"),
                Key("Enter"),
                Wait(2500),
                Capture("after_tool_turn"),
            ],
        },
        Scenario {
            name: "abort_mid_stream",
            script: Script::Endless(ANTHROPIC_SSE_PREFIX),
            steps: vec![
                Type("go"),
                Key("Enter"),
                Wait(1200),
                Capture("streaming"),
                Key("Escape"),
                Wait(800),
                Capture("after_abort"),
            ],
        },
        Scenario {
            name: "steer_mid_stream",
            script: Script::Endless(ANTHROPIC_SSE_PREFIX),
            steps: vec![
                Type("go"),
                Key("Enter"),
                Wait(1200),
                Type("turn left"),
                Key("Enter"),
                Wait(600),
                Capture("after_steer"),
                Key("Escape"),
                Wait(800),
                Capture("after_abort"),
            ],
        },
        Scenario {
            name: "settings_model_change",
            script: Script::Sse(ANTHROPIC_SSE),
            steps: vec![
                Type("/model claude-opus-4-1"),
                Key("Enter"),
                Wait(600),
                Capture("after_model"),
                Type("/thinking high"),
                Key("Enter"),
                Wait(600),
                Capture("after_thinking"),
                Type("/context 1m"),
                Key("Enter"),
                Wait(600),
                Capture("after_context"),
            ],
        },
        Scenario {
            name: "clear",
            script: Script::Sse(ANTHROPIC_SSE),
            steps: vec![
                Type("hello"),
                Key("Enter"),
                Wait(1500),
                Type("/clear"),
                Key("Enter"),
                Wait(600),
                Capture("after_clear"),
            ],
        },
    ]
}

fn tmux(args: &[&str]) -> String {
    let out = Command::new("tmux")
        .arg("-L")
        .arg(TMUX)
        .args(args)
        .output()
        .expect("tmux");
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn wait_ready(pane: &str) {
    for _ in 0..500 {
        std::thread::sleep(Duration::from_millis(20));
        let cap = tmux(&["capture-pane", "-pt", pane]);
        if cap.contains('❯') || cap.lines().any(|l| l.trim_end().ends_with('>')) {
            return;
        }
    }
    panic!("pane {pane} never became ready:\n{}", tmux(&["capture-pane", "-pt", pane]));
}

/// Strip what is allowed to differ (§5.1): session ids, timings, spinner.
fn normalise(frame: &str) -> String {
    let id_re = regex::Regex::new(r"\d{8}-\d{6}-[0-9a-f]{4}").unwrap();
    let ms_re = regex::Regex::new(r"\b\d+(\.\d+)?\s?(ms|s)\b").unwrap();
    let sha_re = regex::Regex::new(r"\b[0-9a-f]{8}(-dirty)?\b").unwrap();
    let spinner = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏', '◐', '◓', '◑', '◒'];
    frame
        .lines()
        .map(|l| {
            let l = id_re.replace_all(l, "<id>");
            let l = ms_re.replace_all(&l, "<t>");
            let l = sha_re.replace_all(&l, "<sha>");
            let l: String = l.chars().map(|c| if spinner.contains(&c) { '·' } else { c }).collect();
            l.trim_end().to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn normalise_journal(dir: &Path) -> String {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        let mut files: Vec<_> = rd.flatten().map(|e| e.path()).collect();
        files.sort();
        for f in files {
            let raw = std::fs::read_to_string(&f).unwrap_or_default();
            let mut v: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
            if let Some(o) = v.as_object_mut() {
                for k in ["id", "created_at", "updated_at", "parent_session", "compacted_into"] {
                    o.remove(k);
                }
            }
            out.push(serde_json::to_string_pretty(&v).unwrap_or_default());
        }
    }
    out.join("\n---\n")
}

struct Pane {
    name: String,
    home: tempfile::TempDir,
}

fn spawn_pane(name: &str, bin: &Path, base_url: &str, extra: &[&str]) -> Pane {
    let home = tempfile::TempDir::new().unwrap();
    let base = home.path().join(".synaps-cli");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(base.join("config"), "theme = \"default\"\n").unwrap();
    std::fs::write(base.join("auth.json"), phase2::synthetic_auth_json()).unwrap();
    let env = format!(
        "HOME={h} SYNAPS_BASE_DIR={b} SYNAPS_ANTHROPIC_BASE_URL={u} SYNAPS_NO_BOOT_FX=1 TERM=xterm-256color COLUMNS={c} LINES={r}",
        h = home.path().display(),
        b = base.display(),
        u = base_url,
        c = COLS,
        r = ROWS,
    );
    let cmd = format!(
        "exec env {env} {} --no-extensions {}",
        bin.display(),
        extra.join(" ")
    );
    tmux(&[
        "new",
        "-d",
        "-s",
        name,
        "-x",
        &COLS.to_string(),
        "-y",
        &ROWS.to_string(),
        &cmd,
    ]);
    wait_ready(name);
    Pane {
        name: name.to_string(),
        home,
    }
}

fn drive(panes: &[&Pane], steps: &[Step], captures: &mut Vec<(String, Vec<String>)>) {
    for step in steps {
        match step {
            Step::Type(t) => {
                for p in panes {
                    tmux(&["send-keys", "-t", &p.name, "-l", t]);
                }
            }
            Step::Key(k) => {
                for p in panes {
                    tmux(&["send-keys", "-t", &p.name, k]);
                }
            }
            Step::Wait(ms) => std::thread::sleep(Duration::from_millis(*ms)),
            Step::Capture(label) => {
                std::thread::sleep(Duration::from_millis(300));
                let frames = panes
                    .iter()
                    .map(|p| normalise(&tmux(&["capture-pane", "-pt", &p.name])))
                    .collect();
                captures.push((label.to_string(), frames));
            }
        }
    }
}

fn diff_report(label: &str, a: &str, b: &str) -> Option<String> {
    if a == b {
        return None;
    }
    let mut out = format!("--- {label}: reference vs new ---\n");
    for (i, (la, lb)) in a.lines().zip(b.lines()).enumerate() {
        if la != lb {
            out.push_str(&format!("{i:3}| R: {la}\n   | L: {lb}\n"));
        }
    }
    Some(out)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "tmux differential; SYNAPS_TUI_E2E=1 SYNAPS_REF_BIN=<f0ee1e62 build>"]
async fn tui_reference_binary_differential() {
    if std::env::var("SYNAPS_TUI_E2E").ok().as_deref() != Some("1") {
        eprintln!("SYNAPS_TUI_E2E != 1; skipping");
        return;
    }
    let ref_bin = PathBuf::from(std::env::var("SYNAPS_REF_BIN").expect("SYNAPS_REF_BIN"));
    let new_bin = PathBuf::from(env!("CARGO_BIN_EXE_synaps"));
    let only = std::env::var("SYNAPS_TUI_E2E_ONLY").ok();
    let mut failures = Vec::new();
    let out_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/tui-e2e");
    let _ = std::fs::create_dir_all(&out_dir);

    for sc in scenarios() {
        if only.as_deref().is_some_and(|o| o != sc.name) {
            continue;
        }
        tmux(&["kill-server"]);
        // One stub per pane: `SeqSse` scripts count hits per stub, so the
        // two binaries must not share the arrival order.
        let (url_r, _h1, _b1) = spawn_stub(sc.script.clone()).await;
        let (url_l, _h2, _b2) = spawn_stub(sc.script.clone()).await;
        let r = spawn_pane("r", &ref_bin, &url_r, &[]);
        let l = spawn_pane("l", &new_bin, &url_l, &[]);
        let mut captures = Vec::new();
        drive(&[&r, &l], &sc.steps, &mut captures);
        // Quit both so the journals are flushed.
        for p in [&r, &l] {
            tmux(&["send-keys", "-t", &p.name, "-l", "/quit"]);
            tmux(&["send-keys", "-t", &p.name, "Enter"]);
        }
        std::thread::sleep(Duration::from_millis(1500));
        for (label, frames) in &captures {
            let file = out_dir.join(format!("{}.{label}", sc.name));
            let _ = std::fs::write(file.with_extension("ref.txt"), &frames[0]);
            let _ = std::fs::write(file.with_extension("new.txt"), &frames[1]);
            if let Some(d) = diff_report(&format!("{}/{label}", sc.name), &frames[0], &frames[1]) {
                failures.push(d);
            }
        }
        let jr = normalise_journal(&r.home.path().join(".synaps-cli/sessions"));
        let jl = normalise_journal(&l.home.path().join(".synaps-cli/sessions"));
        if let Some(d) = diff_report(&format!("{}/journal", sc.name), &jr, &jl) {
            failures.push(d);
        }
        tmux(&["kill-server"]);
    }
    if failures.is_empty() {
        println!("REFERENCE DIFF: empty");
    } else {
        panic!("REFERENCE DIFF:\n{}", failures.join("\n"));
    }
}
