//! PLAN-phase4 §7.3 — the growth proof for the thin client.
//!
//! One daemon on a scripted provider stub, one `--attach` TUI pane in tmux
//! with `SYNAPS_MEM_TRACE=1 SYNAPS_MEMPROF_PURGE=1`; 40 turns, each a `bash`
//! tool round producing ~30 KB of tool output (≈ 40 KB of history) and a
//! text reply. The client's idle purge fires after every turn and writes a
//! `purged` ladder line; RssAnon after turns 10/20/30/40 comes from those.
//!
//! Gates (G6): `RssAnon(40) − RssAnon(10) ≤ 1.5 MB`, `max ≤ 14 MB`.
//! With the history mirror on (`SYNAPS_CLIENT_HISTORY=full`, the default
//! until B7) this is expected to FAIL — that output is B's "before".
//!
//! Ignored unless `SYNAPS_TUI_E2E=1`; needs `tmux`. Turns are driven by
//! `send-keys` into the pane (deterministic: the same path the differential
//! uses) rather than `synaps send`, whose event injection does not by itself
//! start a turn.
//!
//! ```text
//! SYNAPS_TUI_E2E=1 cargo test --release --test client_bounded -- --ignored --nocapture
//! ```

#[path = "support/phase2/mod.rs"]
#[allow(dead_code)]
mod phase2;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use phase2::{spawn_stub, Script};

const TMUX: &str = "clientbounded";
const TURNS: usize = 40;
const SLOPE_LIMIT_KB: i64 = 1536;
const MAX_LIMIT_KB: u64 = 14 * 1024;

/// Anthropic SSE turn: `bash` with ~30 KB of base64 on stdout.
const SSE_TOOL_BASH_30K: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_cb\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
    "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,",
    "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_cb\",\"name\":\"bash\",\"input\":{}}}\n\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",",
    "\"partial_json\":\"{\\\"command\\\":\\\"head -c 30000 /dev/urandom | base64\\\"}\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",",
    "\"stop_sequence\":null},\"usage\":{\"input_tokens\":10,\"output_tokens\":5,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

/// The continuation: a short text turn.
const SSE_TEXT: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_cb2\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
    "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",",
    "\"stop_sequence\":null},\"usage\":{\"input_tokens\":10,\"output_tokens\":1,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

/// tool, text, tool, text, … — one pair per turn (the stub answers by
/// arrival order; the last body repeats).
static BODIES: [&str; TURNS * 2] = {
    let mut a = [SSE_TEXT; TURNS * 2];
    let mut i = 0;
    while i < TURNS * 2 {
        a[i] = SSE_TOOL_BASH_30K;
        i += 2;
    }
    a
};

fn tmux(args: &[&str]) -> String {
    let out = Command::new("tmux").arg("-L").arg(TMUX).args(args).output().expect("tmux");
    String::from_utf8_lossy(&out.stdout).to_string()
}

struct Daemon(Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn purged_lines(trace: &Path) -> Vec<u64> {
    std::fs::read_to_string(trace)
        .unwrap_or_default()
        .lines()
        .filter(|l| l.contains(" stage=purged "))
        .filter_map(|l| {
            l.split_whitespace()
                .find_map(|kv| kv.strip_prefix("rss_anon_kb="))
                .and_then(|v| v.parse().ok())
        })
        .collect()
}

fn pane_idle(pane: &str) -> bool {
    let cap = tmux(&["capture-pane", "-pt", pane]);
    cap.contains('❯') && !cap.contains("thinking") && !cap.contains("streaming")
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn client_rss_is_bounded_over_40_tool_turns() {
    if std::env::var("SYNAPS_TUI_E2E").as_deref() != Ok("1") {
        eprintln!("skipped: SYNAPS_TUI_E2E=1 not set");
        return;
    }
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_synaps"));
    let home = tempfile::TempDir::new().unwrap();
    let base = home.path().join(".synaps-cli");
    let run = base.join("run");
    std::fs::create_dir_all(&run).unwrap();
    std::fs::write(base.join("config"), "theme = \"default\"\n").unwrap();
    std::fs::write(base.join("auth.json"), phase2::synthetic_auth_json()).unwrap();
    let (url, _hits, _bodies) = spawn_stub(Script::SeqSse(&BODIES)).await;

    let daemon = Daemon(
        Command::new(&bin)
            .args(["daemon", "--foreground"])
            .env("HOME", home.path())
            .env("SYNAPS_BASE_DIR", &base)
            .env("SYNAPS_RUNTIME_DIR", &run)
            .env("SYNAPS_DAEMON", "1")
            .env("SYNAPS_ANTHROPIC_BASE_URL", &url)
            .env_remove("SYNAPS_DAEMON_RELOAD_STATE")
            .env_remove("SYNAPS_DAEMON_LOCK_FD")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn daemon"),
    );
    let sock = run.join("daemon.sock");
    let t0 = Instant::now();
    while !sock.exists() {
        assert!(t0.elapsed() < Duration::from_secs(20), "daemon socket never appeared");
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_millis(300));

    let trace = home.path().join("client.trace");
    tmux(&["kill-server"]);
    let env = format!(
        "HOME={h} SYNAPS_BASE_DIR={b} SYNAPS_RUNTIME_DIR={r} SYNAPS_DAEMON=1 SYNAPS_ANTHROPIC_BASE_URL={u} \
         SYNAPS_MEM_TRACE=1 SYNAPS_MEM_TRACE_FILE={t} SYNAPS_MEMPROF_PURGE=1 SYNAPS_NO_BOOT_FX=1 \
         TERM=xterm-256color",
        h = home.path().display(),
        b = base.display(),
        r = run.display(),
        u = url,
        t = trace.display(),
    );
    tmux(&[
        "new", "-d", "-s", "c", "-x", "120", "-y", "40",
        &format!("exec env {env} {} --attach", bin.display()),
    ]);
    let t0 = Instant::now();
    while !pane_idle("c") {
        assert!(
            t0.elapsed() < Duration::from_secs(20),
            "client never became ready:\n{}",
            tmux(&["capture-pane", "-pt", "c"])
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    let mut samples: Vec<(usize, u64)> = Vec::new();
    for turn in 1..=TURNS {
        let before = purged_lines(&trace).len();
        tmux(&["send-keys", "-t", "c", "-l", "go"]);
        tmux(&["send-keys", "-t", "c", "Enter"]);
        let t0 = Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(100));
            let n = purged_lines(&trace).len();
            if n > before && pane_idle("c") {
                // settle: no further purge lines for 500 ms
                std::thread::sleep(Duration::from_millis(500));
                if purged_lines(&trace).len() == n {
                    break;
                }
            }
            assert!(
                t0.elapsed() < Duration::from_secs(40),
                "turn {turn}: no purge after the turn\n{}",
                tmux(&["capture-pane", "-pt", "c"])
            );
        }
        if turn % 10 == 0 {
            let rss = *purged_lines(&trace).last().unwrap();
            eprintln!("turn {turn:>2}: RssAnon = {rss} kB ({:.1} MB)", rss as f64 / 1024.0);
            samples.push((turn, rss));
        }
    }
    tmux(&["send-keys", "-t", "c", "-l", "/quit"]);
    tmux(&["send-keys", "-t", "c", "Enter"]);
    std::thread::sleep(Duration::from_millis(500));
    tmux(&["kill-server"]);
    drop(daemon);

    let all = purged_lines(&trace);
    let max = all.iter().copied().max().unwrap_or(0);
    let r10 = samples.iter().find(|(t, _)| *t == 10).unwrap().1 as i64;
    let r40 = samples.iter().find(|(t, _)| *t == 40).unwrap().1 as i64;
    let slope = r40 - r10;
    eprintln!(
        "samples={samples:?} slope(40-10)={slope} kB ({:.2} MB) max={max} kB ({:.1} MB) history_mode={}",
        slope as f64 / 1024.0,
        max as f64 / 1024.0,
        std::env::var("SYNAPS_CLIENT_HISTORY").unwrap_or_else(|_| "default".into())
    );
    assert!(slope <= SLOPE_LIMIT_KB, "G6: RssAnon grew {slope} kB over turns 10→40 (limit {SLOPE_LIMIT_KB})");
    assert!(max <= MAX_LIMIT_KB, "G6: max RssAnon {max} kB over the run (limit {MAX_LIMIT_KB})");
}
