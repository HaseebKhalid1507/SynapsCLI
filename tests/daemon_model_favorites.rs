//! W24 — `model_favorites` end to end against a REAL in-process daemon: the
//! control query answers the model picker's Favorites view (the favorites of
//! every usable provider, in picker order) on a control connection and on an
//! attached one, and re-reads `favorite_models` from the config on every
//! query. The frames are raw JSON on purpose, so this file builds at a parent
//! without the frame (where the daemon answers "unknown variant").
//!
//! Starts a daemon: run it on the remote test host only. Unix only: it
//! connects with a Unix socket.
#![cfg(unix)]

#[path = "support/phase2/mod.rs"]
mod phase2;

use std::sync::Arc;
use std::time::Duration;

use agent_engine::daemon::{Daemon, DaemonOpts};
use agent_engine::session::wire::{encode_line, Attach, ClientFrame, Hello};
use agent_engine::session::{AttachMode, ClientKind, SessionConfig};
use agent_engine::{EngineHost, HostOpts};
use phase2::*;
use serde_json::{json, Value};
use serial_test::serial;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

async fn start(runtime_dir: &std::path::Path) -> Daemon {
    let host: Arc<EngineHost> = EngineHost::boot_and_install(HostOpts {
        profile: None,
        no_extensions: true,
        mcp_discovery: agent_engine::McpDiscovery::Inline,
    })
    .await
    .expect("host boot");
    Daemon::start(host, DaemonOpts { runtime_dir: Some(runtime_dir.to_path_buf()), ..Default::default() })
        .await
        .expect("daemon start")
}

struct Raw {
    lines: tokio::io::Lines<BufReader<OwnedReadHalf>>,
    w: OwnedWriteHalf,
}

impl Raw {
    /// Connect and say hello; returns the connection and the `Welcome`.
    async fn connect(sock: &std::path::Path) -> (Self, Value) {
        let stream = UnixStream::connect(sock).await.expect("connect");
        let (r, w) = stream.into_split();
        let mut raw = Raw { lines: BufReader::new(r).lines(), w };
        let hello = encode_line(&ClientFrame::Hello(Hello::new(ClientKind::Attach))).unwrap();
        raw.send_line(hello.trim_end()).await;
        let welcome = raw.next().await;
        assert_eq!(welcome["type"], "welcome", "{welcome}");
        (raw, welcome)
    }

    async fn send_line(&mut self, line: &str) {
        self.w.write_all(line.as_bytes()).await.unwrap();
        self.w.write_all(b"\n").await.unwrap();
    }

    async fn next(&mut self) -> Value {
        let line = tokio::time::timeout(Duration::from_secs(15), self.lines.next_line())
            .await
            .expect("a frame in time")
            .expect("read")
            .expect("connection open");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("not JSON ({e}): {line}"))
    }

    /// The next frame whose type is not `event` (an attached connection
    /// also carries the session's events).
    async fn next_reply(&mut self) -> Value {
        loop {
            let frame = self.next().await;
            if frame["type"] != "event" {
                return frame;
            }
        }
    }
}

/// Removes, for the test, every variable that can make a provider usable or
/// pick an account or a config outside the guard's scratch HOME: each static
/// provider's key variables, the Muse key, the local endpoint and key,
/// `SYNAPS_PROFILE` and every `SYNAPS_ACCOUNT_*`. Made before the guard and
/// the daemon, so no config is read with them; restores them on drop.
/// Values are never read out.
struct ProviderEnvScrub(Vec<(String, std::ffi::OsString)>);

impl ProviderEnvScrub {
    fn new() -> Self {
        let mut names: Vec<String> = agent_core::auth::static_providers::STATIC_PROVIDERS
            .iter()
            .flat_map(|spec| spec.env_vars.iter().map(|var| var.to_string()))
            .collect();
        names.extend(["META_API_KEY", "LOCAL_ENDPOINT", "LOCAL_API_KEY", "SYNAPS_PROFILE"].map(String::from));
        names.extend(std::env::vars_os().filter_map(|(k, _)| k.into_string().ok()).filter(|k| k.starts_with("SYNAPS_ACCOUNT_")));
        names.sort();
        names.dedup();
        let saved = names
            .into_iter()
            .filter_map(|name| std::env::var_os(&name).map(|value| (name, value)))
            .collect::<Vec<_>>();
        for (name, _) in &saved {
            std::env::remove_var(name);
        }
        Self(saved)
    }
}

impl Drop for ProviderEnvScrub {
    fn drop(&mut self) {
        for (name, value) in &self.0 {
            std::env::set_var(name, value);
        }
    }
}

fn write_config(guard: &HomeGuard, favorites: &str) {
    let config = format!("favorite_models = {favorites}\nprovider.local.models = llama3:8b, qwen3:8b\n");
    std::fs::write(guard.base_dir().join("config"), config).unwrap();
}

/// `{id, label, provider_key, provider_name}` of each row, in order.
fn rows(reply: &Value) -> Vec<(String, Value, String, String)> {
    reply["models"]
        .as_array()
        .unwrap_or_else(|| panic!("models array: {reply}"))
        .iter()
        .map(|m| {
            (
                m["id"].as_str().unwrap_or_else(|| panic!("id: {m}")).to_string(),
                m["label"].clone(),
                m["provider_key"].as_str().unwrap_or_else(|| panic!("provider_key: {m}")).to_string(),
                m["provider_name"].as_str().unwrap_or_else(|| panic!("provider_name: {m}")).to_string(),
            )
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn model_favorites_answers_the_pickers_favorites_view_on_both_connections() {
    // Declared first, so it is dropped (restored) after the guard.
    let _scrub = ProviderEnvScrub::new();
    let guard = HomeGuard::new();
    let (url, _hits, _) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let known = agent_engine::models::KNOWN_MODELS;
    let (k0, l0) = known[0];
    let (k1, l1) = known[1];
    // Anthropic is logged in (the guard's synthetic login), local has two
    // models, groq has no key (hidden), the plugin provider does not exist
    // (hidden). `claude/` and a bare `claude-` id are the picker's legacy
    // spellings of an Anthropic favorite.
    write_config(&guard, &format!("claude/{k1}, {k0}, local/qwen3:8b, groq/llama-3.3-70b-versatile, plug:prov:m"));
    let run = guard.base_dir().join("run");
    let d = start(&run).await;
    let sock = d.paths.sock.clone();

    // Control connection.
    let (mut c, welcome) = Raw::connect(&sock).await;
    c.send_line(&json!({"type": "model_favorites", "id": 7}).to_string()).await;
    let reply = c.next().await;
    assert_eq!(reply["type"], "model_favorites_result", "{reply}");
    assert_eq!(reply["id"], 7, "{reply}");
    assert_eq!(
        rows(&reply),
        vec![
            (format!("anthropic/{k0}"), json!(l0), "anthropic".into(), "Anthropic".into()),
            (format!("anthropic/{k1}"), json!(l1), "anthropic".into(), "Anthropic".into()),
            ("local/qwen3:8b".into(), json!("Qwen3"), "local".into(), "Local".into()),
        ],
        "{reply}"
    );
    // No extensions on this daemon: its plugin rows (none) are final.
    assert_eq!(reply["plugins_loaded"], true, "{reply}");
    assert_eq!(reply["live_providers"], json!([]), "{reply}");
    let features = welcome["features"].as_array().cloned().unwrap_or_default();
    assert!(features.contains(&json!("model_favorites")), "{welcome}");

    // The config is read again on every query.
    write_config(&guard, "local/qwen3:8b, local/llama3:8b");
    c.send_line(&json!({"type": "model_favorites", "id": 8}).to_string()).await;
    let reply = c.next().await;
    assert_eq!(reply["id"], 8, "{reply}");
    let ids: Vec<String> = rows(&reply).into_iter().map(|r| r.0).collect();
    assert_eq!(ids, ["local/llama3:8b", "local/qwen3:8b"], "the provider's own order: {reply}");

    // Attached connection: the same answer.
    let (mut a, _) = Raw::connect(&sock).await;
    let create = encode_line(&ClientFrame::Attach(Attach::Create {
        config: SessionConfig { cwd: Some(guard.home.path().to_path_buf()), persist: false, ..Default::default() },
        mode: AttachMode::Mirror,
    }))
    .unwrap();
    a.send_line(create.trim_end()).await;
    let attached = a.next_reply().await;
    assert_eq!(attached["type"], "attached", "{attached}");
    a.send_line(&json!({"type": "model_favorites", "id": 9}).to_string()).await;
    let reply = a.next_reply().await;
    assert_eq!(reply["type"], "model_favorites_result", "{reply}");
    assert_eq!(reply["id"], 9, "{reply}");
    let ids: Vec<String> = rows(&reply).into_iter().map(|r| r.0).collect();
    assert_eq!(ids, ["local/llama3:8b", "local/qwen3:8b"], "{reply}");

    drop(a);
    drop(c);
    d.shutdown_token().cancel();
    d.wait().await;
}
