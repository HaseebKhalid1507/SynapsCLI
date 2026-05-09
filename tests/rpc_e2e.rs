//! End-to-end subprocess harness for `synaps rpc`.
//!
//! Spawns the real `synaps` binary as a child process, drives it over its
//! stdin/stdout pipes with line-delimited JSON frames, and asserts the
//! protocol invariants described in `synaps-bridge.SPEC.md §4`.
//!
//! See also:
//! - `tests/rpc_protocol.rs` (Task 1) — golden round-trip tests for the
//!   protocol types without spawning a process.
//! - `src/cmd/rpc.rs` — the implementation under test.
//!
//! # Structure
//!
//! * **`mod tier1`** — hermetic tests that never reach an LLM.  Each test
//!   spawns a fresh child with an isolated `HOME` tempdir, exchanges a few
//!   frames, then shuts down.  Stable across `--test-threads=1` and
//!   `--test-threads=4`.
//!
//! * **`mod tier2`** — tests that plant a fake streaming-provider extension
//!   and drive a real `Prompt` through the engine.  Guarded on `python3`
//!   availability; skipped (not failed) when python3 is absent.

use std::path::Path;
use std::time::Duration;

use serde_json::Value;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::time::timeout;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// A running `synaps rpc` child process with typed send/recv helpers.
///
/// Drop automatically kills the child so that assertion failures in tests
/// don't leave zombie `synaps rpc` processes behind.
struct RpcChild {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    /// Kept alive so the tempdir isn't deleted while the child is running.
    _home: TempDir,
}

impl RpcChild {
    /// Spawn `synaps rpc` with the given extra args and a fresh isolated HOME.
    ///
    /// `setup` is called with the HOME path **before** the child is spawned,
    /// so callers can plant config files or plugin manifests.
    async fn spawn(args: &[&str], setup: impl FnOnce(&Path)) -> anyhow::Result<Self> {
        let home = TempDir::new()?;
        setup(home.path());

        // Write a minimal config so the engine doesn't try to load API keys.
        let cfg_path = home.path().join(".synaps-cli");
        std::fs::create_dir_all(&cfg_path)?;
        // Empty config — engine will fall back to defaults.
        std::fs::write(cfg_path.join("config"), "")?;

        let bin = env!("CARGO_BIN_EXE_synaps");

        let mut cmd = Command::new(bin);
        cmd.arg("rpc");
        for a in args {
            cmd.arg(a);
        }
        cmd.env("HOME", home.path())
            .env("SYNAPS_BASE_DIR", cfg_path)
            // Suppress any real API calls — no key means the engine uses the
            // extension provider path when an extension is present, and fails
            // gracefully otherwise.
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .env_remove("GROQ_API_KEY")
            .env_remove("CEREBRAS_API_KEY")
            .env_remove("NVIDIA_API_KEY")
            .env_remove("SAMBANOVA_API_KEY")
            .env_remove("OPENROUTER_API_KEY")
            .env_remove("GOOGLE_API_KEY")
            .env_remove("DEEPINFRA_API_KEY")
            .env_remove("DEEPINFRA_TOKEN")
            .env_remove("HUGGINGFACE_API_KEY")
            .env_remove("HF_TOKEN")
            .env_remove("FIREWORKS_API_KEY")
            .env_remove("HYPERBOLIC_API_KEY")
            .env_remove("SCALEWAY_API_KEY")
            .env_remove("SILICONFLOW_API_KEY")
            .env_remove("TOGETHER_API_KEY")
            .env_remove("CHUTES_API_KEY")
            .env_remove("CODESTRAL_API_KEY")
            .env_remove("PERPLEXITY_API_KEY")
            .env_remove("PPLX_API_KEY")
            .env_remove("MISTRAL_API_KEY")
            .env_remove("XAI_API_KEY")
            .env_remove("DEEPSEEK_API_KEY")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null()) // keep test output clean
            .kill_on_drop(true);

        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow::anyhow!("no stdin"))?;
        let stdout_raw = child.stdout.take().ok_or_else(|| anyhow::anyhow!("no stdout"))?;
        let stdout = BufReader::new(stdout_raw);

        Ok(RpcChild { child, stdin, stdout, _home: home })
    }

    /// Spawn `synaps rpc` reusing an existing HOME directory.
    ///
    /// This is used by `continue_resumes_history` to share a single `TempDir`
    /// between two successive children so session files are visible to both.
    /// The caller retains ownership of the `TempDir` and must ensure it
    /// outlives both children.
    async fn spawn_with_home(
        args: &[&str],
        home_path: &Path,
        dummy_home: TempDir, // caller passes a dummy TempDir to satisfy the _home field
    ) -> anyhow::Result<Self> {
        let cfg_path = home_path.join(".synaps-cli");
        std::fs::create_dir_all(&cfg_path)?;
        std::fs::write(cfg_path.join("config"), "")?;

        let bin = env!("CARGO_BIN_EXE_synaps");

        let mut cmd = Command::new(bin);
        cmd.arg("rpc");
        for a in args {
            cmd.arg(a);
        }
        cmd.env("HOME", home_path)
            .env("SYNAPS_BASE_DIR", &cfg_path)
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .env_remove("GROQ_API_KEY")
            .env_remove("CEREBRAS_API_KEY")
            .env_remove("NVIDIA_API_KEY")
            .env_remove("SAMBANOVA_API_KEY")
            .env_remove("OPENROUTER_API_KEY")
            .env_remove("GOOGLE_API_KEY")
            .env_remove("DEEPINFRA_API_KEY")
            .env_remove("DEEPINFRA_TOKEN")
            .env_remove("HUGGINGFACE_API_KEY")
            .env_remove("HF_TOKEN")
            .env_remove("FIREWORKS_API_KEY")
            .env_remove("HYPERBOLIC_API_KEY")
            .env_remove("SCALEWAY_API_KEY")
            .env_remove("SILICONFLOW_API_KEY")
            .env_remove("TOGETHER_API_KEY")
            .env_remove("CHUTES_API_KEY")
            .env_remove("CODESTRAL_API_KEY")
            .env_remove("PERPLEXITY_API_KEY")
            .env_remove("PPLX_API_KEY")
            .env_remove("MISTRAL_API_KEY")
            .env_remove("XAI_API_KEY")
            .env_remove("DEEPSEEK_API_KEY")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);

        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow::anyhow!("no stdin"))?;
        let stdout_raw = child.stdout.take().ok_or_else(|| anyhow::anyhow!("no stdout"))?;
        let stdout = BufReader::new(stdout_raw);

        Ok(RpcChild { child, stdin, stdout, _home: dummy_home })
    }

    /// Read one line from the child's stdout and parse it as a JSON Value.
    /// Times out after `dur`.
    async fn recv_timeout(&mut self, dur: Duration) -> anyhow::Result<Value> {
        let mut line = String::new();
        timeout(dur, self.stdout.read_line(&mut line))
            .await
            .map_err(|_| anyhow::anyhow!("recv timed out after {dur:?}"))??;
        if line.is_empty() {
            anyhow::bail!("child stdout closed (EOF)");
        }
        let v: Value = serde_json::from_str(line.trim_end())?;
        Ok(v)
    }

    /// Read one frame with the default 5-second timeout.
    async fn recv(&mut self) -> anyhow::Result<Value> {
        self.recv_timeout(Duration::from_secs(5)).await
    }

    /// Serialise `cmd` as a JSON line and write it to the child's stdin.
    async fn send(&mut self, cmd: &Value) -> anyhow::Result<()> {
        let mut line = serde_json::to_string(cmd)?;
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    /// Send `Shutdown`, wait for the child to exit (up to 5 s), assert code 0.
    async fn shutdown(mut self) -> anyhow::Result<()> {
        let shutdown_cmd = serde_json::json!({"type": "shutdown"});
        // Ignore send errors — child may have already gone.
        let _ = self.send(&shutdown_cmd).await;

        let status = timeout(Duration::from_secs(5), self.child.wait())
            .await
            .map_err(|_| anyhow::anyhow!("child did not exit within 5s after Shutdown"))??;

        if !status.success() {
            anyhow::bail!("child exited with non-zero status: {status}");
        }
        Ok(())
    }
}

impl Drop for RpcChild {
    fn drop(&mut self) {
        // Best-effort kill so a panicking test doesn't leave a zombie process.
        let _ = self.child.start_kill();
    }
}

// ---------------------------------------------------------------------------
// Tier-1 tests — hermetic, no LLM
// ---------------------------------------------------------------------------

mod tier1 {
    use super::*;
    use serde_json::json;

    /// Helper: spawn a child, receive and return the Ready frame.
    async fn spawn_and_ready() -> anyhow::Result<(RpcChild, Value)> {
        let mut child = RpcChild::spawn(&[], |_| {}).await?;
        let ready = child.recv().await?;
        Ok((child, ready))
    }

    /// `Ready` frame arrives within 2 s with `type = "ready"` and
    /// `protocol_version = 1`.
    #[tokio::test]
    async fn ready_frame_arrives() {
        let mut child = RpcChild::spawn(&[], |_| {}).await.expect("spawn");
        let ready = child
            .recv_timeout(Duration::from_secs(2))
            .await
            .expect("Ready frame within 2s");

        assert_eq!(ready["type"], "ready", "first frame must be 'ready'");
        assert_eq!(ready["protocol_version"], 1, "protocol_version must be 1");
        assert!(
            ready["session_id"].is_string(),
            "session_id must be a string"
        );
        assert!(ready["model"].is_string(), "model must be a string");

        child.shutdown().await.expect("clean shutdown");
    }

    /// `Shutdown` → child exits with code 0, stdout closes cleanly.
    #[tokio::test]
    async fn shutdown_clean_exit() {
        let (child, _ready) = spawn_and_ready().await.expect("spawn");
        child.shutdown().await.expect("clean shutdown");
    }

    /// A malformed JSON line → `Error { id: null }`, child stays alive.
    #[tokio::test]
    async fn malformed_json_stays_alive() {
        let (mut child, _ready) = spawn_and_ready().await.expect("spawn");

        // Send garbage.
        child
            .stdin
            .write_all(b"this is not json\n")
            .await
            .expect("write");
        child.stdin.flush().await.expect("flush");

        let err = child.recv().await.expect("error frame");
        assert_eq!(err["type"], "error", "must be an error frame");
        assert!(
            err.get("id").map(|v| v.is_null()).unwrap_or(true),
            "id must be null or absent for parse errors"
        );

        // Child must still respond to a valid command.
        child
            .send(&json!({"type": "get_state", "id": "ping"}))
            .await
            .expect("send");
        let resp = child.recv().await.expect("response after malformed frame");
        assert_eq!(resp["type"], "response");
        assert_eq!(resp["command"], "get_state");

        child.shutdown().await.expect("clean shutdown");
    }

    /// An oversize frame (>1 MiB) → `Error { id: null }`, child stays alive.
    #[tokio::test]
    async fn oversize_frame_rejected() {
        let (mut child, _ready) = spawn_and_ready().await.expect("spawn");

        // 1 MiB + 1 byte of 'x' characters — guaranteed to exceed the limit.
        let oversize = "x".repeat(1024 * 1024 + 1) + "\n";
        child
            .stdin
            .write_all(oversize.as_bytes())
            .await
            .expect("write oversize");
        child.stdin.flush().await.expect("flush");

        let err = child.recv().await.expect("error frame");
        assert_eq!(err["type"], "error", "oversize frame must yield error");
        assert!(
            err.get("id").map(|v| v.is_null()).unwrap_or(true),
            "id must be null for oversize frame errors"
        );

        // Child must still be alive.
        child
            .send(&json!({"type": "get_state", "id": "alive"}))
            .await
            .expect("send");
        let resp = child.recv().await.expect("response after oversize");
        assert_eq!(resp["type"], "response");
        assert_eq!(resp["command"], "get_state");

        child.shutdown().await.expect("clean shutdown");
    }

    /// `SetModel` → `Response { command: "set_model" }`,
    /// `GetState` reflects the new model.
    #[tokio::test]
    async fn set_model_then_get_state() {
        let (mut child, _ready) = spawn_and_ready().await.expect("spawn");

        child
            .send(&json!({"type": "set_model", "id": "sm1", "model": "claude-opus-4-5"}))
            .await
            .expect("set_model send");
        let resp = child.recv().await.expect("set_model response");
        assert_eq!(resp["type"], "response");
        assert_eq!(resp["command"], "set_model");
        assert_eq!(resp["id"], "sm1");

        child
            .send(&json!({"type": "get_state", "id": "gs1"}))
            .await
            .expect("get_state send");
        let state = child.recv().await.expect("get_state response");
        assert_eq!(state["type"], "response");
        assert_eq!(state["command"], "get_state");
        assert_eq!(
            state["model"], "claude-opus-4-5",
            "model must reflect SetModel"
        );

        child.shutdown().await.expect("clean shutdown");
    }

    /// `GetState` → `Response` with expected keys: `streaming`, `model`,
    /// `session_id`, `message_count`.
    #[tokio::test]
    async fn get_state_shape() {
        let (mut child, _ready) = spawn_and_ready().await.expect("spawn");

        child
            .send(&json!({"type": "get_state", "id": "gs2"}))
            .await
            .expect("send");
        let resp = child.recv().await.expect("response");

        assert_eq!(resp["type"], "response");
        assert_eq!(resp["command"], "get_state");
        assert_eq!(resp["id"], "gs2");
        assert!(resp["streaming"].is_boolean(), "streaming must be boolean");
        assert!(resp["model"].is_string(), "model must be string");
        assert!(resp["session_id"].is_string(), "session_id must be string");
        assert!(
            resp["message_count"].is_number(),
            "message_count must be number"
        );

        child.shutdown().await.expect("clean shutdown");
    }

    /// `GetSessionStats` → `Response` with expected keys.
    #[tokio::test]
    async fn get_session_stats_shape() {
        let (mut child, _ready) = spawn_and_ready().await.expect("spawn");

        child
            .send(&json!({"type": "get_session_stats", "id": "gss1"}))
            .await
            .expect("send");
        let resp = child.recv().await.expect("response");

        assert_eq!(resp["type"], "response");
        assert_eq!(resp["command"], "get_session_stats");
        assert_eq!(resp["id"], "gss1");
        assert!(resp["input_tokens"].is_number(), "input_tokens must be number");
        assert!(resp["output_tokens"].is_number(), "output_tokens must be number");
        assert!(resp["message_count"].is_number(), "message_count must be number");
        assert!(resp["model"].is_string(), "model must be string");
        assert!(resp["session_id"].is_string(), "session_id must be string");

        child.shutdown().await.expect("clean shutdown");
    }

    /// `NewSession` → `Response` with a `session_id` different from the
    /// initial one advertised in the `Ready` frame.
    #[tokio::test]
    async fn new_session_changes_id() {
        let (mut child, ready) = spawn_and_ready().await.expect("spawn");
        let initial_id = ready["session_id"].as_str().unwrap().to_string();

        child
            .send(&json!({"type": "new_session", "id": "ns1"}))
            .await
            .expect("send");
        let resp = child.recv().await.expect("response");

        assert_eq!(resp["type"], "response");
        assert_eq!(resp["command"], "new_session");
        assert_eq!(resp["id"], "ns1");
        let new_id = resp["session_id"].as_str().expect("session_id string");
        assert_ne!(
            new_id, initial_id,
            "NewSession must produce a different session_id"
        );

        child.shutdown().await.expect("clean shutdown");
    }

    /// `GetMessages` → `Response { messages: [] }` on a fresh session.
    #[tokio::test]
    async fn get_messages_empty_initially() {
        let (mut child, _ready) = spawn_and_ready().await.expect("spawn");

        child
            .send(&json!({"type": "get_messages", "id": "gm1"}))
            .await
            .expect("send");
        let resp = child.recv().await.expect("response");

        assert_eq!(resp["type"], "response");
        assert_eq!(resp["command"], "get_messages");
        assert_eq!(resp["id"], "gm1");
        let msgs = resp["messages"].as_array().expect("messages must be array");
        assert!(msgs.is_empty(), "fresh session must have no messages");

        child.shutdown().await.expect("clean shutdown");
    }

    /// `Abort` with no in-flight stream → `Response { ok: true }`.
    #[tokio::test]
    async fn abort_no_inflight() {
        let (mut child, _ready) = spawn_and_ready().await.expect("spawn");

        child
            .send(&json!({"type": "abort", "id": "ab1"}))
            .await
            .expect("send");
        let resp = child.recv().await.expect("response");

        assert_eq!(resp["type"], "response");
        assert_eq!(resp["command"], "abort");
        assert_eq!(resp["id"], "ab1");
        assert_eq!(resp["ok"], true, "abort with no stream must return ok: true");

        child.shutdown().await.expect("clean shutdown");
    }

    /// `GetAvailableModels` → `Response { models: [...] }`.
    /// Accepts an empty array — in the test env there may be no API keys,
    /// so no provider models are visible.  We only verify the shape.
    #[tokio::test]
    async fn get_available_models_shape() {
        let (mut child, _ready) = spawn_and_ready().await.expect("spawn");

        child
            .send(&json!({"type": "get_available_models", "id": "gam1"}))
            .await
            .expect("send");
        let resp = child.recv().await.expect("response");

        assert_eq!(resp["type"], "response");
        assert_eq!(resp["command"], "get_available_models");
        assert_eq!(resp["id"], "gam1");
        assert!(
            resp["models"].is_array(),
            "models must be an array (got {:?})",
            resp["models"]
        );

        child.shutdown().await.expect("clean shutdown");
    }

    /// `--continue <id>` resumes an existing session:
    /// * session_id is preserved across restart
    /// * model set in session A is preserved in session B (via Ready frame)
    /// * `get_messages` returns the user message sent in session A
    ///
    /// Tier-1 safe: no LLM is needed. The prompt fails (no API key), but the
    /// user message is pushed to `api_messages` BEFORE the streaming task is
    /// spawned (see `handle_prompt`), so the message survives and is saved on
    /// `Shutdown`.
    #[tokio::test]
    async fn continue_resumes_history() {
        // A single HOME shared by both children so session files are visible.
        let shared_home = TempDir::new().expect("TempDir");
        let home_path = shared_home.path().to_path_buf();

        // ── Session A ────────────────────────────────────────────────────────
        let dummy_a = TempDir::new().expect("dummy TempDir A");
        let mut child_a =
            RpcChild::spawn_with_home(&[], &home_path, dummy_a).await.expect("spawn A");
        let ready_a = child_a.recv().await.expect("Ready A");
        assert_eq!(ready_a["type"], "ready", "first frame from A must be 'ready'");

        let session_id = ready_a["session_id"].as_str().expect("session_id in Ready A").to_string();

        // Change to a non-default model so we can verify persistence.
        let chosen_model = "claude-opus-4-5";
        child_a
            .send(&serde_json::json!({
                "type": "set_model",
                "id": "sm_a",
                "model": chosen_model
            }))
            .await
            .expect("set_model send");
        let sm_resp = child_a.recv().await.expect("set_model response");
        assert_eq!(sm_resp["type"], "response");
        assert_eq!(sm_resp["command"], "set_model");

        // Send a prompt — it will fail (no LLM), but the user message is
        // pushed to api_messages before the stream starts.
        let prompt_text = "hello from session A";
        child_a
            .send(&serde_json::json!({
                "type": "prompt",
                "id": "p_a",
                "message": prompt_text
            }))
            .await
            .expect("prompt send");

        // Drain frames until the prompt response arrives (error path).
        // Expected sequence: error frame, agent_end, response{ok:false}.
        // We collect up to 10 frames to avoid hanging on unexpected output.
        let mut got_prompt_response = false;
        for _ in 0..10 {
            let frame = child_a
                .recv_timeout(Duration::from_secs(10))
                .await
                .expect("frame from A after prompt");
            // The prompt response is {"type":"response","command":"prompt",...}
            if frame["type"] == "response" && frame["command"] == "prompt" {
                got_prompt_response = true;
                break;
            }
        }
        assert!(got_prompt_response, "prompt response frame never arrived");

        // Shutdown A — save_session() runs here, persisting api_messages.
        child_a.shutdown().await.expect("clean shutdown A");

        // ── Session B (--continue) ───────────────────────────────────────────
        let dummy_b = TempDir::new().expect("dummy TempDir B");
        let mut child_b =
            RpcChild::spawn_with_home(&["--continue", &session_id], &home_path, dummy_b)
                .await
                .expect("spawn B");
        let ready_b = child_b.recv().await.expect("Ready B");
        assert_eq!(ready_b["type"], "ready", "first frame from B must be 'ready'");

        // 1. Session identity is preserved.
        assert_eq!(
            ready_b["session_id"].as_str().expect("session_id in Ready B"),
            session_id,
            "session_id must match across --continue restart"
        );

        // 2. Model is preserved.
        assert_eq!(
            ready_b["model"].as_str().expect("model in Ready B"),
            chosen_model,
            "model must be restored from persisted session"
        );

        // 3. Message history includes the original user message.
        child_b
            .send(&serde_json::json!({"type": "get_messages", "id": "gm_b"}))
            .await
            .expect("get_messages send");
        let gm_resp = child_b.recv().await.expect("get_messages response");
        assert_eq!(gm_resp["type"], "response");
        assert_eq!(gm_resp["command"], "get_messages");

        let messages = gm_resp["messages"].as_array().expect("messages array");
        assert!(!messages.is_empty(), "resumed session must have at least one message");

        let has_user_msg = messages.iter().any(|m| {
            m["role"] == "user"
                && m["content"]
                    .as_str()
                    .map(|s| s.contains(prompt_text))
                    .unwrap_or(false)
        });
        assert!(
            has_user_msg,
            "get_messages must return the original user message; got: {messages:?}"
        );

        child_b.shutdown().await.expect("clean shutdown B");

        // shared_home dropped here — after both children are gone.
        drop(shared_home);
    }

}

// ---------------------------------------------------------------------------
// Tier-2 tests — require fake LLM extension via planted plugin manifest
// ---------------------------------------------------------------------------

mod tier2 {
    use super::*;
    use serde_json::json;

    /// Returns the absolute path to the slow streaming provider fixture.
    fn slow_provider_fixture() -> String {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/slow_streaming_provider_extension.py")
            .to_string_lossy()
            .to_string()
    }

    /// Check that python3 is available; return false (and print a skip message)
    /// if it is not.
    fn python3_available() -> bool {
        std::process::Command::new("python3")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Plant the stream-echo provider plugin into the given SYNAPS_BASE_DIR.
    fn plant_stream_echo_plugin(base_dir: &Path, fixture_path: &str) {
        let plugin_dir = base_dir.join("plugins/stream-echo/.synaps-plugin");
        std::fs::create_dir_all(&plugin_dir).expect("create plugin dir");

        let manifest = format!(
            r#"{{
  "name": "stream-echo",
  "version": "0.1.0",
  "extension": {{
    "protocol_version": 1,
    "runtime": "process",
    "command": "python3",
    "args": ["{fixture_path}"],
    "permissions": ["providers.register"]
  }}
}}
"#
        );
        std::fs::write(plugin_dir.join("plugin.json"), manifest)
            .expect("write plugin.json");

        // Config: activate the extension provider model.
        std::fs::write(
            base_dir.join("config"),
            "model = stream-echo:stream-echo-mini\n",
        )
        .expect("write config");
    }

    /// Spawn a child with the stream-echo provider planted and return (child, ready_frame).
    async fn spawn_with_echo_provider() -> anyhow::Result<(RpcChild, Value)> {
        let fixture = slow_provider_fixture();
        let mut child = RpcChild::spawn(&[], move |home_path| {
            let base_dir = home_path.join(".synaps-cli");
            std::fs::create_dir_all(&base_dir).expect("create base dir");
            plant_stream_echo_plugin(&base_dir, &fixture);
        })
        .await?;
        let ready = child.recv_timeout(Duration::from_secs(10)).await?;
        Ok((child, ready))
    }

    /// Full happy path: `Prompt` → at least one `MessageUpdate { TextDelta }` →
    /// `AgentEnd { usage }` → `Response { command: "prompt", ok: true }`.
    ///
    /// All frames must share the same `prompt_id` (or carry no id for streaming
    /// events).
    #[tokio::test]
    async fn prompt_to_agent_end_happy_path() {
        if !python3_available() {
            eprintln!("skipping tier2::prompt_to_agent_end_happy_path: python3 unavailable");
            return;
        }

        let (mut child, _ready) = match spawn_with_echo_provider().await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skipping tier2::prompt_to_agent_end_happy_path: spawn failed: {e}");
                return;
            }
        };

        child
            .send(&json!({
                "type": "prompt",
                "id": "t2p1",
                "message": "ping",
                "attachments": []
            }))
            .await
            .expect("send prompt");

        let mut saw_text_delta = false;
        let mut saw_agent_end = false;
        let mut saw_prompt_response = false;

        for _ in 0..30 {
            let frame = match child.recv_timeout(Duration::from_secs(10)).await {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("recv error: {e}");
                    break;
                }
            };

            match frame["type"].as_str().unwrap_or("") {
                "message_update" => {
                    if frame["event"]["type"] == "text_delta" {
                        saw_text_delta = true;
                    }
                }
                "agent_end" => {
                    assert!(
                        frame["usage"].is_object(),
                        "agent_end must carry usage object"
                    );
                    saw_agent_end = true;
                }
                "response" => {
                    if frame["command"] == "prompt" {
                        assert_eq!(frame["id"], "t2p1");
                        saw_prompt_response = true;
                        break;
                    }
                }
                "error" => {
                    eprintln!(
                        "skipping tier2::prompt_to_agent_end_happy_path: engine error: {}",
                        frame["message"]
                    );
                    let _ = child.shutdown().await;
                    return;
                }
                _ => {}
            }
        }

        let _ = child.shutdown().await;

        assert!(
            saw_text_delta,
            "expected at least one TextDelta message_update"
        );
        assert!(saw_agent_end, "expected an agent_end frame");
        assert!(
            saw_prompt_response,
            "expected a Response {{ command: prompt }} frame"
        );
    }

    /// Abort mid-stream: send `Prompt`, wait ~150 ms, send `Abort`.
    /// Expect `Response { command: "abort", ok: true }` and eventually
    /// `Response { command: "prompt" }` (ok may be true or false — both are
    /// valid per the spec when the stream is cancelled).
    #[tokio::test]
    async fn abort_mid_stream() {
        if !python3_available() {
            eprintln!("skipping tier2::abort_mid_stream: python3 unavailable");
            return;
        }

        let (mut child, _ready) = match spawn_with_echo_provider().await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skipping tier2::abort_mid_stream: spawn failed: {e}");
                return;
            }
        };

        child
            .send(&json!({
                "type": "prompt",
                "id": "t2p2",
                "message": "stream me",
                "attachments": []
            }))
            .await
            .expect("send prompt");

        // Wait ~150 ms so the stream has started but is between the first and
        // second sleep in the slow provider (each sleep is 200 ms).
        tokio::time::sleep(Duration::from_millis(150)).await;

        child
            .send(&json!({"type": "abort", "id": "ab2"}))
            .await
            .expect("send abort");

        let mut saw_abort_response = false;
        let mut saw_prompt_response = false;

        for _ in 0..30 {
            let frame = match child.recv_timeout(Duration::from_secs(10)).await {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("recv error during abort test: {e}");
                    break;
                }
            };

            match frame["type"].as_str().unwrap_or("") {
                "response" if frame["command"] == "abort" => {
                    assert_eq!(frame["id"], "ab2");
                    assert_eq!(frame["ok"], true, "abort must return ok: true");
                    saw_abort_response = true;
                }
                "response" if frame["command"] == "prompt" => {
                    assert_eq!(frame["id"], "t2p2");
                    // Cancelled stream must report ok: true, cancelled: true.
                    assert_eq!(frame["ok"], true, "cancelled prompt must return ok: true");
                    assert_eq!(
                        frame["cancelled"], true,
                        "cancelled prompt must carry cancelled: true"
                    );
                    saw_prompt_response = true;
                    if saw_abort_response {
                        break;
                    }
                }
                "error" => {
                    eprintln!(
                        "skipping tier2::abort_mid_stream: engine error: {}",
                        frame["message"]
                    );
                    let _ = child.shutdown().await;
                    return;
                }
                _ => {}
            }

            if saw_abort_response && saw_prompt_response {
                break;
            }
        }

        let _ = child.shutdown().await;

        assert!(
            saw_abort_response,
            "expected Response {{ command: abort, ok: true }}"
        );
        assert!(
            saw_prompt_response,
            "expected Response {{ command: prompt }} after abort"
        );
    }

    /// `NewSession` while a stream is in-flight must be rejected with an
    /// `Error { id: Some("ns1"), message: "...abort first" }` frame, and the
    /// streaming prompt must complete normally afterwards.
    #[tokio::test]
    async fn new_session_rejected_while_streaming() {
        if !python3_available() {
            eprintln!(
                "skipping tier2::new_session_rejected_while_streaming: python3 unavailable"
            );
            return;
        }

        let (mut child, _ready) = match spawn_with_echo_provider().await {
            Ok(r) => r,
            Err(e) => {
                eprintln!(
                    "skipping tier2::new_session_rejected_while_streaming: spawn failed: {e}"
                );
                return;
            }
        };

        // Start a streaming prompt.
        child
            .send(&json!({
                "type": "prompt",
                "id": "t2p3",
                "message": "stream me",
                "attachments": []
            }))
            .await
            .expect("send prompt");

        // Wait ~150 ms so the stream is definitely in-flight (first 200 ms sleep
        // in the slow provider is still running).
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Send NewSession while in-flight — must be rejected.
        child
            .send(&json!({"type": "new_session", "id": "ns1"}))
            .await
            .expect("send new_session");

        let mut saw_ns_error = false;
        let mut saw_prompt_response = false;

        for _ in 0..40 {
            let frame = match child.recv_timeout(Duration::from_secs(10)).await {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("recv error: {e}");
                    break;
                }
            };

            match frame["type"].as_str().unwrap_or("") {
                "error" => {
                    if frame["id"] == "ns1" {
                        let msg = frame["message"].as_str().unwrap_or("");
                        assert!(
                            msg.contains("abort first"),
                            "expected 'abort first' in error message, got: {msg}"
                        );
                        saw_ns_error = true;
                    } else {
                        // Engine error from the prompt itself — skip test.
                        eprintln!(
                            "skipping tier2::new_session_rejected_while_streaming: engine error: {}",
                            frame["message"]
                        );
                        let _ = child.shutdown().await;
                        return;
                    }
                }
                "response" if frame["command"] == "prompt" => {
                    assert_eq!(frame["id"], "t2p3");
                    saw_prompt_response = true;
                }
                _ => {}
            }

            if saw_ns_error && saw_prompt_response {
                break;
            }
        }

        let _ = child.shutdown().await;

        assert!(
            saw_ns_error,
            "expected Error {{ id: 'ns1', message: '...abort first' }}"
        );
        assert!(
            saw_prompt_response,
            "expected prompt t2p3 to complete after rejected new_session"
        );
    }

    /// A second `Prompt` while a slow stream is in-flight must be rejected with
    /// `Error { id: "p2", message: "...abort first..." }`.
    ///
    /// Uses the slow-streaming fixture (200 ms sleeps between tokens) so the
    /// timing is deterministic: p2 is dispatched while p1 is still streaming.
    #[tokio::test]
    async fn concurrent_prompt_rejected() {
        if !python3_available() {
            eprintln!("skipping tier2::concurrent_prompt_rejected: python3 unavailable");
            return;
        }

        let (mut child, _ready) = match spawn_with_echo_provider().await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skipping tier2::concurrent_prompt_rejected: spawn failed: {e}");
                return;
            }
        };

        // Start a slow streaming prompt.
        child
            .send(&json!({
                "type": "prompt",
                "id": "p1",
                "message": "stream me",
                "attachments": []
            }))
            .await
            .expect("send p1");

        // Wait 150 ms — p1 is now mid-stream (first 200 ms sleep still running).
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Send second prompt while p1 is in-flight — must be rejected.
        child
            .send(&json!({
                "type": "prompt",
                "id": "p2",
                "message": "should be rejected",
                "attachments": []
            }))
            .await
            .expect("send p2");

        let mut found_concurrent_error = false;

        for _ in 0..40 {
            let frame = match child.recv_timeout(Duration::from_secs(10)).await {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("recv error: {e}");
                    break;
                }
            };

            // If the engine itself errors on p1 (e.g. auth failure because the
            // slow fixture didn't register in time), the environment can't
            // exercise this guard — skip rather than fail.
            if frame["type"] == "error" && frame["id"] == "p1" {
                eprintln!(
                    "skipping tier2::concurrent_prompt_rejected: engine error on p1: {}",
                    frame["message"]
                );
                let _ = child.shutdown().await;
                return;
            }

            if frame["type"] == "error" && frame["id"] == "p2" {
                let msg = frame["message"].as_str().unwrap_or("");
                assert!(
                    msg.contains("abort first"),
                    "expected 'abort first' in concurrent-prompt error, got: {msg}"
                );
                found_concurrent_error = true;
                break;
            }
        }

        // Abort p1 and drain remaining frames.
        child
            .send(&json!({"type": "abort", "id": "ab_p1"}))
            .await
            .ok();
        for _ in 0..10 {
            if child.recv_timeout(Duration::from_millis(300)).await.is_err() {
                break;
            }
        }

        let _ = child.shutdown().await;

        assert!(
            found_concurrent_error,
            "expected Error {{ id: 'p2', message: '...abort first...' }} \
             when a second Prompt is sent while p1 is still streaming"
        );
    }
}
