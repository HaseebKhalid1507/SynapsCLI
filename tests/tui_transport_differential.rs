//! PLAN-phase3 §5.1 layer 3 — the terminal differential against the
//! REFERENCE BINARY built at `f0ee1e62` (the in-process TUI before the port):
//! the only oracle independent of the actor's author.
//!
//! For each scenario, two tmux panes run identical `send-keys` scripts
//! against the same scripted provider stub: (R) the reference binary and
//! (L) this binary in-process. After each step the pane is captured,
//! normalised and diffed. The journals are diffed too.
//!
//! Phase 4 (B8): with `SYNAPS_TUI_E2E_SOCKET=1` a third pane (S) runs
//! `synaps --attach` against a private daemon spawned in the pane's HOME
//! on the same stub; L≡S is diffed with the socket normaliser (drops the
//! `attached to … as client #…` line and session ids). Printed as a second
//! table: `SOCKET DIFF: empty (N)`.
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
                json_delay: Duration::from_millis(1500),
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
                Wait(300),
                // The transcript is rebuilt after the swap, so the lines
                // pushed at dispatch are only visible DURING the summary.
                Capture("during_compact"),
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

/// Journal normalisation: session ids and RFC 3339 timestamps are allowed
/// to differ (§5.1) — wherever they appear (top level and inside the
/// compaction metadata block). Everything else must be byte-equal.
fn normalise_journal(dir: &Path) -> String {
    let id_re = regex::Regex::new(r"\d{8}-\d{6}-[0-9a-f]{4}").unwrap();
    let ts_re = regex::Regex::new(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?Z").unwrap();
    fn walk(v: &mut serde_json::Value, id_re: &regex::Regex, ts_re: &regex::Regex) {
        match v {
            serde_json::Value::String(s) => {
                let t = id_re.replace_all(s, "<id>");
                let t = ts_re.replace_all(&t, "<ts>");
                *s = t.into_owned();
            }
            serde_json::Value::Array(a) => a.iter_mut().for_each(|x| walk(x, id_re, ts_re)),
            serde_json::Value::Object(o) => o.values_mut().for_each(|x| walk(x, id_re, ts_re)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        let mut files: Vec<_> = rd.flatten().map(|e| e.path()).collect();
        files.sort();
        for f in files {
            let raw = std::fs::read_to_string(&f).unwrap_or_default();
            let mut v: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
            walk(&mut v, &id_re, &ts_re);
            out.push(serde_json::to_string_pretty(&v).unwrap_or_default());
        }
    }
    out.join("\n---\n")
}

struct Pane {
    name: String,
    home: tempfile::TempDir,
    /// The private daemon behind an S pane (killed on drop).
    daemon: Option<std::process::Child>,
}

impl Drop for Pane {
    fn drop(&mut self) {
        if let Some(mut d) = self.daemon.take() {
            let _ = d.kill();
            let _ = d.wait();
        }
    }
}

/// Socket-pane normalisation on top of [`normalise`] — the documented L≡S
/// drops (phase 4 §7.4 G9):
/// 1. the attach banner (`attached to <id> as client #N (Mirror)`), a
///    System card that exists only on S;
/// 2. blank rows — the banner card shifts the top-anchored transcript by
///    its rows while the input box/footer stay bottom-anchored, so the
///    empty region between them differs in height. Content rows, their
///    order and the chrome (header, box, footer) are all compared.
fn normalise_socket(frame: &str) -> String {
    let attached = regex::Regex::new(r"^\s*attached to <id> as client #\d+ \([A-Za-z]+\).*$").unwrap();
    normalise(frame)
        .lines()
        .filter(|l| !attached.is_match(l) && !l.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Journal diff for L≡S: the session `.json` files only. `index.jsonl`
/// (start/end lifecycle events) is dropped — S's `/quit` is a `Detach`, the
/// daemon session lives on, so its `end` event is written later (or never
/// in the test's window). Documented drop.
fn normalise_journal_socket(dir: &Path) -> String {
    let tmp = tempfile::TempDir::new().unwrap();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let name = e.file_name();
            if e.path().is_file() && name.to_string_lossy().ends_with(".json") {
                let _ = std::fs::copy(e.path(), tmp.path().join(name));
            }
        }
    }
    normalise_journal(tmp.path())
}

/// Copy a pane's sessions dir under `target/tui-e2e/<scenario>.<tag>.sessions/`
/// so a journal diff can be inspected after the temp HOME is gone.
fn keep_sessions(out_dir: &Path, scenario: &str, tag: &str, home: &Path) {
    let dst = out_dir.join(format!("{scenario}.{tag}.sessions"));
    let _ = std::fs::remove_dir_all(&dst);
    let _ = std::fs::create_dir_all(&dst);
    if let Ok(rd) = std::fs::read_dir(home.join(".synaps-cli/sessions")) {
        for e in rd.flatten() {
            if e.path().is_file() {
                let _ = std::fs::copy(e.path(), dst.join(e.file_name()));
            }
        }
    }
}

fn pane_env(home: &Path, base: &Path, base_url: &str) -> String {
    format!(
        "HOME={h} SYNAPS_BASE_DIR={b} SYNAPS_ANTHROPIC_BASE_URL={u} SYNAPS_NO_BOOT_FX=1 TERM=xterm-256color COLUMNS={c} LINES={r}",
        h = home.display(),
        b = base.display(),
        u = base_url,
        c = COLS,
        r = ROWS,
    )
}

fn prepare_home(extensions: bool) -> (tempfile::TempDir, PathBuf) {
    let home = tempfile::TempDir::new().unwrap();
    let base = home.path().join(".synaps-cli");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(base.join("config"), "theme = \"default\"\n").unwrap();
    std::fs::write(base.join("auth.json"), phase2::synthetic_auth_json()).unwrap();
    if extensions {
        plant_extension(&base);
    }
    (home, base)
}

/// S pane: a daemon (`synaps daemon --foreground`) in a private HOME on
/// `base_url`, then `synaps --attach` (creates the session) in tmux.
fn spawn_socket_pane(name: &str, bin: &Path, base_url: &str, extensions: bool) -> Pane {
    let (home, base) = prepare_home(extensions);
    let run = home.path().join("run");
    let mut daemon = Command::new(bin);
    daemon
        .args(["daemon", "--foreground"])
        .env("HOME", home.path())
        .env("SYNAPS_BASE_DIR", &base)
        .env("SYNAPS_ANTHROPIC_BASE_URL", base_url)
        .env("SYNAPS_DAEMON", "1")
        .env("SYNAPS_RUNTIME_DIR", &run)
        .env("SYNAPS_DAEMON_PARK_GRACE_SECS", "never")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let child = daemon.spawn().expect("spawn daemon");
    let sock = run.join("daemon.sock");
    for _ in 0..500 {
        if sock.exists() && run.join("daemon.json").exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(sock.exists(), "daemon socket never appeared at {}", sock.display());
    // The listener may exist a beat before the accept loop; give it a moment.
    std::thread::sleep(Duration::from_millis(150));
    let env = format!(
        "{} SYNAPS_DAEMON=1 SYNAPS_RUNTIME_DIR={}",
        pane_env(home.path(), &base, base_url),
        run.display()
    );
    let cmd = format!(
        "exec env {env} {} --attach {}",
        bin.display(),
        if extensions { "" } else { "--no-extensions" }
    );
    tmux(&["new", "-d", "-s", name, "-x", &COLS.to_string(), "-y", &ROWS.to_string(), &cmd]);
    wait_ready(name);
    Pane { name: name.to_string(), home, daemon: Some(child) }
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
    let (home, base) = prepare_home(extensions);
    let env = pane_env(home.path(), &base, base_url);
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
        daemon: None,
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
                // Raw captures; each diff applies its own normaliser.
                let frames = panes
                    .iter()
                    .map(|p| tmux(&["capture-pane", "-pt", &p.name]))
                    .collect();
                captures.push((label.to_string(), frames));
            }
        }
    }
}

fn diff_report(label: &str, a: &str, b: &str) -> Option<String> {
    diff_report_named(label, ("R", a), ("L", b))
}

fn diff_report_named(label: &str, (na, a): (&str, &str), (nb, b): (&str, &str)) -> Option<String> {
    if a == b {
        return None;
    }
    let mut out = format!("--- {label}: {na} vs {nb} ---\n");
    let la: Vec<&str> = a.lines().collect();
    let lb: Vec<&str> = b.lines().collect();
    for i in 0..la.len().max(lb.len()) {
        let x = la.get(i).copied().unwrap_or("<none>");
        let y = lb.get(i).copied().unwrap_or("<none>");
        if x != y {
            out.push_str(&format!("{i:3}| {na}: {x}\n   | {nb}: {y}\n"));
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
    let socket = std::env::var("SYNAPS_TUI_E2E_SOCKET").ok().as_deref() == Some("1");
    let mut failures = Vec::new();
    let mut socket_failures = Vec::new();
    let mut table: Vec<(&'static str, bool)> = Vec::new();
    let mut socket_table: Vec<(&'static str, bool)> = Vec::new();
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
        let s_pane = if socket {
            let (url_s, _h3, _b3) = spawn_stub(sc.script.clone()).await;
            // Keep the stub alive for the scenario.
            std::mem::forget((_h3, _b3));
            Some(spawn_socket_pane("s", &new_bin, &url_s, sc.extensions))
        } else {
            None
        };
        let mut captures = Vec::new();
        let mut panes: Vec<&Pane> = vec![&r, &l];
        if let Some(s) = &s_pane {
            panes.push(s);
        }
        drive(&panes, &sc.steps, &mut captures);
        // Quit all so the journals are flushed.
        for p in &panes {
            tmux(&["send-keys", "-t", &p.name, "-l", "/quit"]);
            tmux(&["send-keys", "-t", &p.name, "Enter"]);
        }
        std::thread::sleep(Duration::from_millis(1500));
        let before = failures.len();
        for (label, frames) in &captures {
            let fr = normalise(&frames[0]);
            let fl = normalise(&frames[1]);
            let _ = std::fs::write(out_dir.join(format!("{}.{label}.ref.txt", sc.name)), &fr);
            let _ = std::fs::write(out_dir.join(format!("{}.{label}.new.txt", sc.name)), &fl);
            if let Some(d) = diff_report(&format!("{}/{label}", sc.name), &fr, &fl) {
                failures.push(d);
            }
        }
        keep_sessions(&out_dir, sc.name, "ref", r.home.path());
        keep_sessions(&out_dir, sc.name, "new", l.home.path());
        let jr = normalise_journal(&r.home.path().join(".synaps-cli/sessions"));
        let jl = normalise_journal(&l.home.path().join(".synaps-cli/sessions"));
        if let Some(d) = diff_report(&format!("{}/journal", sc.name), &jr, &jl) {
            failures.push(d);
        }
        table.push((sc.name, failures.len() == before));
        if let Some(s) = &s_pane {
            let before = socket_failures.len();
            for (label, frames) in &captures {
                let fl = normalise_socket(&frames[1]);
                let fs = normalise_socket(&frames[2]);
                let _ = std::fs::write(out_dir.join(format!("{}.{label}.sock.txt", sc.name)), &fs);
                if let Some(d) = diff_report_named(&format!("{}/{label}", sc.name), ("L", &fl), ("S", &fs)) {
                    socket_failures.push(d);
                }
            }
            // The daemon writes S's journal; give the detach a beat, then stop it.
            std::thread::sleep(Duration::from_millis(500));
            keep_sessions(&out_dir, sc.name, "sock", s.home.path());
            let jl = normalise_journal_socket(&l.home.path().join(".synaps-cli/sessions"));
            let js = normalise_journal_socket(&s.home.path().join(".synaps-cli/sessions"));
            if let Some(d) = diff_report_named(&format!("{}/journal", sc.name), ("L", &jl), ("S", &js)) {
                socket_failures.push(d);
            }
            socket_table.push((sc.name, socket_failures.len() == before));
        }
        drop(s_pane);
        tmux(&["kill-server"]);
    }
    if socket {
        println!("scenario                   L≡S?");
        for (name, ok) in &socket_table {
            println!("{name:<26} {}", if *ok { "yes" } else { "NO" });
        }
        if socket_failures.is_empty() {
            println!("SOCKET DIFF: empty ({})", socket_table.len());
        } else {
            println!("SOCKET DIFF:\n{}", socket_failures.join("\n"));
        }
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
    assert!(socket_failures.is_empty(), "SOCKET DIFF not empty (see above)");
}
