//! `synaps daemon reload` with a reactive worker running, against a REAL
//! daemon process and a scripted loopback provider.
//!
//! The reload `execv`s the daemon, and worker threads cannot survive that.
//! Before this fix the reload destroyed a running worker silently: no
//! archive, no wake for the waiting parent, the worker's in-flight shell
//! command left running unreaped in the worktree, and the handle counter
//! restarted at `sa_1`, so the model's remembered handles aliased new
//! workers. This test asserts that the reload now:
//!
//! 1. stops the worker before the exec, so its in-flight command's process
//!    group (the shell AND its background job) is killed, not orphaned,
//!    while a job the command deliberately detached (`setsid`) lives on;
//! 2. leaves a resumable archive (`status: cancelled`);
//! 3. wakes the parent in the new image ("stopped … by a daemon reload, not
//!    by the user"), and `subagent_status` on the OLD id answers from the
//!    archive (`stopped_by_reload: true`);
//! 4. hands the next worker a NEW id (`sa_2`, never `sa_1` again);
//! 5. never repeats the wake: a second reload, after the parent had it,
//!    stays silent about `sa_1`.
//!
//! Unix only (execv + flock).

#![cfg(unix)]

#[path = "support/phase2/mod.rs"]
mod phase2;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_engine::daemon::registry;
use agent_engine::session::socket_transport::SocketTransport;
use agent_engine::session::wire::*;
use agent_engine::session::*;
use axum::body::Bytes;
use axum::response::IntoResponse;
use phase2::*;
use serde_json::{json, Value};
use serial_test::serial;

const WORKER_MARK: &str = "RELOAD-E2E-WORKER-SLEEPS";
const WORKER2_MARK: &str = "RELOAD-E2E-WORKER-QUICK";

struct DaemonProc {
    child: Child,
    paths: registry::DaemonPaths,
}

impl Drop for DaemonProc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_synaps")
}

async fn spawn_daemon(guard: &HomeGuard, url: &str) -> DaemonProc {
    let run = guard.base_dir().join("run");
    std::fs::create_dir_all(&run).unwrap();
    let paths = registry::daemon_paths_in(&run, None);
    let child = Command::new(bin())
        .args(["daemon", "--foreground"])
        .env("HOME", guard.home.path())
        .env("SYNAPS_BASE_DIR", guard.base_dir())
        .env("SYNAPS_RUNTIME_DIR", &run)
        .env("SYNAPS_DAEMON", "1")
        .env("SYNAPS_ANTHROPIC_BASE_URL", url)
        .env("SYNAPS_NO_BOOT_FX", "1")
        .env_remove("SYNAPS_DAEMON_RELOAD_STATE")
        .env_remove("SYNAPS_DAEMON_LOCK_FD")
        .env_remove("SYNAPS_DAEMON_SUBAGENT_ID_FLOOR")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn daemon");
    let d = DaemonProc { child, paths };
    let t0 = std::time::Instant::now();
    while SocketTransport::ping(&d.paths.sock).await.is_err() {
        assert!(t0.elapsed() < Duration::from_secs(20), "daemon never answered");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    d
}

// ── scripted provider ───────────────────────────────────────────────────────

fn sse_frames(blocks: &str, stop: &str) -> String {
    format!(
        concat!(
            "data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_rw\",\"type\":\"message\",",
            "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
            "\"stop_sequence\":null,\"usage\":{{\"input_tokens\":10,\"output_tokens\":0,",
            "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}}}}\n\n",
            "{blocks}",
            "data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"{stop}\",",
            "\"stop_sequence\":null}},\"usage\":{{\"input_tokens\":10,\"output_tokens\":5,",
            "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}}\n\n",
            "data: {{\"type\":\"message_stop\"}}\n\n",
        ),
        blocks = blocks,
        stop = stop,
    )
}

fn sse_text(text: &str) -> String {
    let start = json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}});
    let delta = json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}});
    let stop = json!({"type":"content_block_stop","index":0});
    sse_frames(&format!("data: {start}\n\ndata: {delta}\n\ndata: {stop}\n\n"), "end_turn")
}

fn sse_tool(id: &str, name: &str, input: Value) -> String {
    let start = json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":id,"name":name,"input":{}}});
    let delta = json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":input.to_string()}});
    let stop = json!({"type":"content_block_stop","index":0});
    sse_frames(&format!("data: {start}\n\ndata: {delta}\n\ndata: {stop}\n\n"), "tool_use")
}

/// The id a tool result answered, if the last message is tool results.
fn last_tool_result(body: &Value) -> Option<(String, String)> {
    let last = body["messages"].as_array()?.last()?;
    last["content"].as_array()?.iter().find_map(|b| {
        (b["type"] == "tool_result").then(|| {
            (
                b["tool_use_id"].as_str().unwrap_or("").to_string(),
                b["content"].to_string(),
            )
        })
    })
}

fn handle_in(text: &str) -> Option<String> {
    let i = text.find("sa_")?;
    let digits: String = text[i + 3..].chars().take_while(|c| c.is_ascii_digit()).collect();
    (!digits.is_empty()).then(|| format!("sa_{digits}"))
}

/// Parent: start a sleeping worker, end the turn. On the reload wake, ask
/// `subagent_status` about the old id, then start a second (quick) worker.
/// Worker 1 runs one long shell command that records the pids of its shell,
/// a background `sleep` it waits on (same process group), and a job it
/// detached with `setsid` (own session: meant to outlive the command);
/// worker 2 answers at once.
fn respond(body: &Value, pids: &Path) -> String {
    let system = body["system"].to_string();
    let last = body["messages"].as_array().and_then(|m| m.last()).map(|m| m.to_string()).unwrap_or_default();
    if system.contains(WORKER_MARK) {
        if last_tool_result(body).is_some() {
            return sse_text("worker one done");
        }
        let d = pids.display();
        let cmd = format!(
            "echo $$ > {d}/shell.pid; \
             setsid sh -c 'echo $$ > {d}/detached.pid; exec sleep 3972' </dev/null >/dev/null 2>&1 & \
             sleep 3971 & echo $! > {d}/sleep.pid; wait $!"
        );
        return sse_tool("toolu_sleep", "bash", json!({"command": cmd, "timeout": 3600}));
    }
    if system.contains(WORKER2_MARK) {
        return sse_text("worker two done");
    }
    match last_tool_result(body) {
        Some((id, _)) if id == "toolu_start1" => sse_text("worker started; waiting"),
        Some((id, _)) if id == "toolu_status" => sse_tool(
            "toolu_start2",
            "subagent_start",
            json!({"system_prompt": WORKER2_MARK, "task": "answer at once", "name": "quick"}),
        ),
        Some((id, _)) if id == "toolu_start2" => sse_text("second worker started"),
        Some(_) => sse_text("ok"),
        None if last.contains("daemon reload") => {
            let old = handle_in(&last).unwrap_or_else(|| "sa_missing".into());
            sse_tool("toolu_status", "subagent_status", json!({"handle_id": old}))
        }
        None if last.contains("start the worker") => sse_tool(
            "toolu_start1",
            "subagent_start",
            json!({"system_prompt": WORKER_MARK, "task": "run the long command", "name": "sleeper", "timeout": 0}),
        ),
        None => sse_text("ack"),
    }
}

async fn spawn_scripted_provider(pids: PathBuf) -> (String, Arc<Mutex<Vec<Value>>>) {
    let bodies: Arc<Mutex<Vec<Value>>> = Arc::default();
    let seen = Arc::clone(&bodies);
    let app = axum::Router::new().fallback(move |body: Bytes| {
        let seen = Arc::clone(&seen);
        let pids = pids.clone();
        async move {
            let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let reply = respond(&v, &pids);
            seen.lock().unwrap().push(v);
            (
                axum::http::StatusCode::OK,
                [("content-type", "text/event-stream")],
                reply,
            )
                .into_response()
        }
    });
    (serve(app).await, bodies)
}

// ── helpers ─────────────────────────────────────────────────────────────────

/// Linux process state letter (`S`, `R`, `Z`…), `None` once fully gone.
fn proc_state(pid: u32) -> Option<char> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit(')').next()?.trim().chars().next()
}

fn alive(pid: u32) -> bool {
    matches!(proc_state(pid), Some(c) if c != 'Z' && c != 'X')
}

async fn wait_for<T>(what: &str, budget: Duration, mut f: impl FnMut() -> Option<T>) -> T {
    let t0 = std::time::Instant::now();
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(t0.elapsed() < budget, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn archives(base: &Path, id: &str) -> Vec<PathBuf> {
    let Ok(dirs) = std::fs::read_dir(base.join("subagent-history")) else {
        return Vec::new();
    };
    dirs.flatten()
        .map(|d| d.path().join(format!("{id}.json")))
        .filter(|p| p.exists())
        .collect()
}

/// A tool result's content parsed as the JSON object it carries.
fn tool_result_json(bodies: &[Value], tool_use_id: &str) -> Option<Value> {
    bodies.iter().find_map(|b| {
        let last = b["messages"].as_array()?.last()?;
        let content = last["content"].as_array()?.iter().find_map(|blk| {
            (blk["type"] == "tool_result" && blk["tool_use_id"] == tool_use_id)
                .then(|| blk["content"].clone())
        })?;
        let text = match &content {
            Value::String(s) => s.clone(),
            Value::Array(a) => a.iter().filter_map(|b| b["text"].as_str()).collect(),
            other => other.to_string(),
        };
        let start = text.find('{')?;
        serde_json::Deserializer::from_str(&text[start..]).into_iter::<Value>().next()?.ok()
    })
}

/// The parent's model calls (not the workers') whose newest message is the
/// reload wake for a stopped worker.
fn reload_wakes(bodies: &[Value]) -> Vec<String> {
    bodies
        .iter()
        .filter(|b| {
            let system = b["system"].to_string();
            !system.contains(WORKER_MARK) && !system.contains(WORKER2_MARK)
        })
        .filter_map(|b| b["messages"].as_array()?.last().map(|m| m.to_string()))
        .filter(|m| m.contains("by a daemon reload, not by the user"))
        .collect()
}

/// SIGKILLs a pid on drop: the detached job must not outlive the test.
struct KillOnDrop(Option<u32>);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
        }
    }
}

fn read_pid(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn tool_result_text(bodies: &[Value], tool_use_id: &str) -> Option<String> {
    bodies.iter().find_map(|b| {
        last_tool_result(b).and_then(|(id, content)| (id == tool_use_id).then_some(content))
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn reload_stops_running_workers_resumably_and_never_reuses_their_ids() {
    let guard = HomeGuard::new();
    let pids = guard.home.path().to_path_buf();
    let (url, bodies) = spawn_scripted_provider(pids.clone()).await;
    let d = spawn_daemon(&guard, &url).await;

    let conn = SocketTransport::connect(&d.paths.sock, Hello::new(ClientKind::Test)).await.unwrap();
    let (mut t, _snap) = SocketTransport::attach(
        conn,
        Attach::Create {
            config: SessionConfig {
                cwd: Some(guard.home.path().to_path_buf()),
                env: None,
                model_override: Some("claude-sonnet-4-5".into()),
                ..Default::default()
            },
            mode: AttachMode::Mirror,
        },
    )
    .await
    .unwrap();

    // Turn 1: the parent starts the worker and ends its turn (it now waits
    // for the worker's completion wake, like any orchestrating session).
    t.send_from_self(SessionCommand::Submit { text: "start the worker".into(), attachments: vec![] })
        .await
        .unwrap();
    loop {
        let env = tokio::time::timeout(Duration::from_secs(30), t.next_event()).await.expect("turn hung").expect("alive");
        if matches!(env.event, SessionEventWire::Idle) {
            break;
        }
    }
    let first = tool_result_text(&bodies.lock().unwrap(), "toolu_start1").expect("subagent_start answered");
    let old_id = handle_in(&first).expect("handle id in the start result");
    assert_eq!(old_id, "sa_1", "{first}");

    // The worker's shell command is running, with its background sleep and
    // the job it detached.
    let (shell, sleep, detached) = wait_for("the worker's processes", Duration::from_secs(30), || {
        Some((
            read_pid(&pids.join("shell.pid"))?,
            read_pid(&pids.join("sleep.pid"))?,
            read_pid(&pids.join("detached.pid"))?,
        ))
    })
    .await;
    let _reap_detached = KillOnDrop(Some(detached));
    for (what, pid) in [("shell", shell), ("background sleep", sleep), ("detached job", detached)] {
        assert!(alive(pid), "worker {what} {pid} should be running before the reload");
    }
    assert!(archives(&guard.base_dir(), &old_id).is_empty(), "no terminal archive yet");

    // Reload while the worker runs (the parent itself is idle).
    let gen = SocketTransport::reload(&d.paths.sock, true, None, None).await.expect("reload accepted");
    assert_eq!(gen, 2);
    while tokio::time::timeout(Duration::from_secs(20), t.next_event()).await.expect("announce").is_some() {}
    tokio::time::timeout(Duration::from_secs(20), t.reconnect(AttachMode::Mirror))
        .await
        .expect("reconnect budget")
        .expect("reconnected");

    // 1. The command's process group was killed before the exec — not left
    //    running in the worktree (a zombie at most: nobody reaps after exec).
    for (what, pid) in [("shell", shell), ("background sleep", sleep)] {
        assert!(!alive(pid), "worker {what} {pid} survived the reload (state {:?})", proc_state(pid));
    }
    // A job the command detached on purpose (own session) is not part of
    // the command and is left alone: callers rely on that.
    assert!(alive(detached), "the detached job {detached} was killed (state {:?})", proc_state(detached));

    // 2. A resumable archive, labelled cancelled.
    let paths = archives(&guard.base_dir(), &old_id);
    assert_eq!(paths.len(), 1, "one archive for {old_id}: {paths:?}");
    let archive: Value = serde_json::from_slice(&std::fs::read(&paths[0]).unwrap()).unwrap();
    assert_eq!(archive["status"], "cancelled");
    assert!(archive["history"].as_array().is_some_and(|h| !h.is_empty()), "history kept for resume");

    // 3. The new image woke the parent, which asked about the OLD id and
    //    got the archived answer; then 4. started a worker with a NEW id.
    let status = wait_for("subagent_status of the old id", Duration::from_secs(30), || {
        tool_result_json(&bodies.lock().unwrap(), "toolu_status")
    })
    .await;
    assert_eq!(status["stopped_by_reload"], json!(true), "{status}");
    assert_eq!(status["status"], "cancelled", "{status}");
    {
        let wakes = reload_wakes(&bodies.lock().unwrap());
        assert_eq!(wakes.len(), 1, "the parent was woken once about the reload: {wakes:?}");
        let wake = &wakes[0];
        assert!(wake.contains(&format!("({old_id})")), "{wake}");
        assert!(wake.contains("subagent_resume"), "{wake}");
    }
    let second = wait_for("the second worker's start", Duration::from_secs(30), || {
        tool_result_text(&bodies.lock().unwrap(), "toolu_start2")
    })
    .await;
    let new_id = handle_in(&second).expect("handle id in the second start result");
    assert_eq!(new_id, "sa_2", "ids continue across the reload: {second}");

    // 5. A second reload, after the parent got (and acted on) its wake: the
    //    old id still rides along as a record, but the parent is not told
    //    to resume it again — that would start a duplicate worker. Wait for
    //    the quick worker to finish first (a running one would rightly be
    //    stopped and announced); the drain waits for the parent's turn.
    wait_for("the second worker's archive", Duration::from_secs(30), || {
        (!archives(&guard.base_dir(), &new_id).is_empty()).then_some(())
    })
    .await;
    let gen = SocketTransport::reload(&d.paths.sock, false, Some(20), None).await.expect("second reload");
    assert_eq!(gen, 3);
    while tokio::time::timeout(Duration::from_secs(40), t.next_event()).await.expect("announce").is_some() {}
    tokio::time::timeout(Duration::from_secs(20), t.reconnect(AttachMode::Mirror))
        .await
        .expect("reconnect budget")
        .expect("reconnected");
    t.send_from_self(SessionCommand::Submit { text: "ping".into(), attachments: vec![] })
        .await
        .unwrap();
    let pinged = |bodies: &[Value]| {
        bodies.iter().any(|b| {
            b["messages"].as_array().and_then(|m| m.last()).is_some_and(|m| m.to_string().contains("ping"))
        })
    };
    wait_for("the ping turn", Duration::from_secs(30), || pinged(&bodies.lock().unwrap()).then_some(())).await;
    // A wake posted by the new image would start (or join) a turn at once.
    tokio::time::sleep(Duration::from_secs(2)).await;
    {
        let bodies = bodies.lock().unwrap();
        let wakes = reload_wakes(&bodies);
        assert_eq!(wakes.len(), 1, "no repeated reload wake after the second reload: {wakes:?}");
        // The quick worker's own completion reached the parent exactly once:
        // before the reload, or re-posted by the new image if it was still
        // queued — never both.
        let completions = bodies
            .iter()
            .filter(|b| {
                let system = b["system"].to_string();
                !system.contains(WORKER_MARK) && !system.contains(WORKER2_MARK)
            })
            .filter_map(|b| b["messages"].as_array()?.last().map(|m| m.to_string()))
            .filter(|m| m.contains("worker two done"))
            .count();
        assert_eq!(completions, 1, "sa_2's completion wake");
    }

    t.detach().await;
    SocketTransport::shutdown(&d.paths.sock, false).await.unwrap();
    let t0 = std::time::Instant::now();
    while registry::is_alive(&d.paths) && t0.elapsed() < Duration::from_secs(10) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
