//! PLAN-phase3 §5.1 layer 3 — the terminal differential against the
//! REFERENCE BINARY built at `f0ee1e62` (the in-process TUI before the port):
//! the only oracle independent of the actor's author.
//!
//! For each scenario, two tmux panes run identical `send-keys` scripts
//! against the same scripted provider stub: (R) the reference binary and
//! (L) this binary in-process. After each step the pane is captured,
//! normalised and diffed. The journals are diffed too.
//!
//! There is NO socket pane here: L≡S (`--attach` against a daemon) is
//! #111's gate and is not claimed by this file.
//!
//! Scenarios: plain_turn, tool_loop, abort_mid_stream, steer_mid_stream,
//! settings_model_change, clear, compaction, queued_during_compaction,
//! secret_prompt, extension_loaded. `queue_while_busy_then_autosend`
//! (the `Steered{delivered:false}` → auto-send-on-Done branch) is NOT
//! here: it needs the stream's steering receiver closed before `Done` is
//! processed, which neither binary reaches deterministically (same reason
//! as `session_actor_differential.rs` ext header #2).
//!
//! Ignored unless `SYNAPS_TUI_E2E=1`; needs `SYNAPS_REF_BIN=<path>` and
//! `tmux`. Run via `scripts/tui-e2e/differential.sh`.

#[path = "support/phase2/mod.rs"]
#[allow(dead_code)]
mod phase2;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use phase2::{
    spawn_stub, Script, ANTHROPIC_MESSAGES_JSON, ANTHROPIC_SSE, ANTHROPIC_SSE_PREFIX,
    ANTHROPIC_SSE_TOOL_USE,
};

/// Anthropic SSE turn calling `bash` with a command that prints a password
/// prompt on stderr and reads the answer from stdin — the bash tool's
/// `detect_password_prompt` raises a secret prompt (`Prompt` envelope →
/// secret-prompt pane on both binaries). Deterministic: the tool result is
/// `len=<answer length>`.
const ANTHROPIC_SSE_BASH_PASSWORD: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_04\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
    "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,",
    "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_pw\",\"name\":\"bash\",\"input\":{}}}\n\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",",
    "\"partial_json\":\"{\\\"command\\\":\\\"printf 'Password: ' >&2; read -r p; echo len=${#p}\\\"}\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",",
    "\"stop_sequence\":null},\"usage\":{\"input_tokens\":10,\"output_tokens\":5,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

/// The in-tree process-extension fixture (`crates/agent-tui/tests/fixtures/
/// interactive_command_extension.py`), planted as an installed plugin under
/// the pane's `SYNAPS_BASE_DIR/plugins/` so the loader arm, `ext_ready`
/// waiter and `on_session_start` timing are inside the oracle.
const EXTENSION_FIXTURE: &str = "crates/agent-tui/tests/fixtures/interactive_command_extension.py";

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
    /// Plant the extension fixture and run WITHOUT `--no-extensions`.
    extensions: bool,
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
            extensions: false,
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
            extensions: false,
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
            extensions: false,
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
            extensions: false,
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
            extensions: false,
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
            extensions: false,
        },
        // HIGH #1: `/compact` must push exactly the reference's lines
        // (disclosure + "compacting conversation..." then the ✓ line). The
        // summary request (non-streaming `call_api_simple`) is answered by
        // the same stub with a Messages JSON body.
        Scenario {
            name: "compaction",
            script: Script::SseOrJson {
                sse: ANTHROPIC_SSE,
                json: ANTHROPIC_MESSAGES_JSON,
                json_delay: Duration::from_millis(0),
            },
            steps: vec![
                Type("hello"),
                Key("Enter"),
                Wait(1500),
                Type("again"),
                Key("Enter"),
                Wait(1500),
                Type("/compact"),
                Key("Enter"),
                Wait(2500),
                Capture("after_compact"),
            ],
            extensions: false,
        },
        // Submit while the compaction summary is in flight → "queued: …"
        // then "queued message restored: …" after the swap (the reference's
        // `compact_task.is_some()` branch / the actor's `compact.is_some()`).
        // The summary reply is delayed 2 s to open the window.
        Scenario {
            name: "queued_during_compaction",
            script: Script::SseOrJson {
                sse: ANTHROPIC_SSE,
                json: ANTHROPIC_MESSAGES_JSON,
                json_delay: Duration::from_millis(2000),
            },
            steps: vec![
                Type("hello"),
                Key("Enter"),
                Wait(1500),
                Type("again"),
                Key("Enter"),
                Wait(1500),
                Type("/compact"),
                Key("Enter"),
                Wait(600),
                Type("later"),
                Key("Enter"),
                Wait(400),
                Capture("queued"),
                Wait(3000),
                Capture("after_compact"),
            ],
            extensions: false,
        },
        // `bash` prints `Password:` on stderr → secret prompt pane on both
        // binaries; the answer goes back to the tool's stdin (`PromptBridge`
        // on the new side, the inline pane on the reference).
        Scenario {
            name: "secret_prompt",
            script: Script::SeqSse(&[ANTHROPIC_SSE_BASH_PASSWORD, ANTHROPIC_SSE]),
            steps: vec![
                Type("do it"),
                Key("Enter"),
                Wait(2000),
                Capture("prompting"),
                Type("abc"),
                Key("Enter"),
                Wait(2500),
                Capture("after_answer"),
            ],
            extensions: false,
        },
        // Extension loader arm + `ext_ready` wait + on_session_start with a
        // real process extension loaded (no `--no-extensions`).
        Scenario {
            name: "extension_loaded",
            script: Script::Sse(ANTHROPIC_SSE),
            steps: vec![
                Wait(1500),
                Capture("booted"),
                Type("hello"),
                Key("Enter"),
                Wait(1500),
                Capture("after_turn"),
            ],
            extensions: true,
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

/// Strip what is allowed to differ (§5.1): session ids, timings, the build
/// sha in the welcome banner, spinner. Deliberately narrow — a bare 8-digit
/// number (token counts, cost cents) must survive so a real footer diff is
/// visible.
fn normalise(frame: &str) -> String {
    let id_re = regex::Regex::new(r"\d{8}-\d{6}-[0-9a-f]{4}").unwrap();
    let ms_re = regex::Regex::new(r"\b\d+(\.\d+)?\s?(ms|s)\b").unwrap();
    // The build sha (`GIT_HASH`, `git rev-parse --short`) is rendered
    // exactly once: `v<ver> · <sha>[-dirty] ` in the banner (draw.rs).
    let sha_re = regex::Regex::new(r"(?P<pre>· )[0-9a-f]{7,12}(-dirty)?\b").unwrap();
    let spinner = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏', '◐', '◓', '◑', '◒'];
    frame
        .lines()
        .map(|l| {
            let l = id_re.replace_all(l, "<id>");
            let l = ms_re.replace_all(&l, "<t>");
            let l = sha_re.replace_all(&l, "${pre}<sha>");
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

fn plant_extension(base: &Path) {
    let root = base.join("plugins").join("demo-plugin");
    std::fs::create_dir_all(root.join(".synaps-plugin")).unwrap();
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(EXTENSION_FIXTURE);
    std::fs::copy(&src, root.join("ext.py")).expect("extension fixture present");
    std::fs::write(
        root.join(".synaps-plugin").join("plugin.json"),
        serde_json::json!({
            "name": "demo-plugin",
            "version": "0.0.1",
            "extension": {
                "protocol_version": 1,
                "runtime": "process",
                "command": "python3",
                "args": ["ext.py"],
                "permissions": ["tools.register"],
            }
        })
        .to_string(),
    )
    .unwrap();
}

fn spawn_pane(name: &str, bin: &Path, base_url: &str, extensions: bool) -> Pane {
    let home = tempfile::TempDir::new().unwrap();
    let base = home.path().join(".synaps-cli");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(base.join("config"), "theme = \"default\"\n").unwrap();
    std::fs::write(base.join("auth.json"), phase2::synthetic_auth_json()).unwrap();
    if extensions {
        plant_extension(&base);
    }
    let env = format!(
        "HOME={h} SYNAPS_BASE_DIR={b} SYNAPS_ANTHROPIC_BASE_URL={u} SYNAPS_NO_BOOT_FX=1 TERM=xterm-256color COLUMNS={c} LINES={r}",
        h = home.path().display(),
        b = base.display(),
        u = base_url,
        c = COLS,
        r = ROWS,
    );
    let cmd = format!(
        "exec env {env} {} {}",
        bin.display(),
        if extensions { "" } else { "--no-extensions" }
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
    let mut table: Vec<(&'static str, bool)> = Vec::new();
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
        let r = spawn_pane("r", &ref_bin, &url_r, sc.extensions);
        let l = spawn_pane("l", &new_bin, &url_l, sc.extensions);
        let mut captures = Vec::new();
        drive(&[&r, &l], &sc.steps, &mut captures);
        // Quit both so the journals are flushed.
        for p in [&r, &l] {
            tmux(&["send-keys", "-t", &p.name, "-l", "/quit"]);
            tmux(&["send-keys", "-t", &p.name, "Enter"]);
        }
        std::thread::sleep(Duration::from_millis(1500));
        let before = failures.len();
        for (label, frames) in &captures {
            let _ = std::fs::write(
                out_dir.join(format!("{}.{label}.ref.txt", sc.name)),
                &frames[0],
            );
            let _ = std::fs::write(
                out_dir.join(format!("{}.{label}.new.txt", sc.name)),
                &frames[1],
            );
            if let Some(d) = diff_report(&format!("{}/{label}", sc.name), &frames[0], &frames[1]) {
                failures.push(d);
            }
        }
        let jr = normalise_journal(&r.home.path().join(".synaps-cli/sessions"));
        let jl = normalise_journal(&l.home.path().join(".synaps-cli/sessions"));
        if let Some(d) = diff_report(&format!("{}/journal", sc.name), &jr, &jl) {
            failures.push(d);
        }
        table.push((sc.name, failures.len() == before));
        tmux(&["kill-server"]);
    }
    println!("scenario                   diff empty?");
    for (name, ok) in &table {
        println!("{name:<26} {}", if *ok { "yes" } else { "NO" });
    }
    if failures.is_empty() {
        println!("REFERENCE DIFF: empty ({} scenarios)", table.len());
    } else {
        panic!("REFERENCE DIFF:\n{}", failures.join("\n"));
    }
}
