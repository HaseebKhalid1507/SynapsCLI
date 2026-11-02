//! A worker (subagent) runtime takes the configured tool limits and leaves
//! out the configured disabled tools, on the legacy spawn path (no host, or
//! `SYNAPS_SUBAGENT_FRESH_RUNTIME=1`); the host path is pinned in
//! `tests/engine_host.rs`. A worker runs in its caller's cwd and env.
//!
//! The bash tests run a real oneshot worker against a loopback Anthropic
//! stub: the worker's first turn calls `bash`, and the test reads the tool
//! result the worker sends back in its second request. What they assert is
//! the timeout the command ran under, as the result reports it (the
//! "[timeout clamped …]" line, or the timeout itself), not a wall clock.

use crate::test_env::{BaseDirGuard, EnvVarGuard, OfflineProviderEnv};

/// The configured limits (`bash_max_timeout`, `bash_timeout`,
/// `max_tool_output`, `subagent_timeout`) as a config file.
fn limits_config(cap: u64, default: u64, output: usize, subagent: u64) -> String {
    format!(
        "bash_max_timeout = {cap}\nbash_timeout = {default}\nmax_tool_output = {output}\nsubagent_timeout = {subagent}\n"
    )
}

#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn a_legacy_worker_runtime_takes_the_configured_tool_limits() {
    let base = BaseDirGuard::new();
    let _offline = OfflineProviderEnv::new();
    let _legacy = EnvVarGuard::set("SYNAPS_SUBAGENT_FRESH_RUNTIME", "1");
    for (cap, default, output, subagent) in [(0u64, 7u64, 12_345usize, 111u64), (2, 3, 4_321, 222)] {
        std::fs::write(
            base.path().join("config"),
            limits_config(cap, default, output, subagent),
        )
        .unwrap();
        let worker = super::spawn_runtime().await.unwrap();
        assert_eq!(worker.bash_max_timeout(), cap, "bash_max_timeout");
        assert_eq!(worker.bash_timeout(), default, "bash_timeout");
        assert_eq!(worker.max_tool_output(), output, "max_tool_output");
        assert_eq!(worker.subagent_timeout(), subagent, "subagent_timeout");
    }
}

/// The legacy worker leaves out every tool the operator disabled. The host
/// path is pinned in `tests/engine_host.rs`.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn a_legacy_worker_leaves_out_the_configured_disabled_tools() {
    let base = BaseDirGuard::new();
    let _offline = OfflineProviderEnv::new();
    let _legacy = EnvVarGuard::set("SYNAPS_SUBAGENT_FRESH_RUNTIME", "1");
    std::fs::write(base.path().join("config"), "disabled_tools = bash, ls\n").unwrap();
    let worker = super::spawn_runtime().await.unwrap();
    let reg = worker.tools_shared();
    let reg = reg.read().await;
    assert!(reg.get("ls").is_none(), "ls is disabled");
    assert!(reg.get("bash").is_none(), "bash is disabled");
    assert!(reg.get("read").is_some(), "read stays");
}

/// All three spawn paths put the worker in its caller's place (cwd and env
/// snapshot, upstream's `WorkerWorkspace`): captured from the caller's tool
/// context before the worker thread starts, handed to the worker policy on
/// the spawned runtime before its stream. The oneshot path is also pinned
/// by behaviour
/// (`worker_bash::a_worker_runs_in_its_callers_directory_and_env`).
#[test]
fn all_spawn_paths_put_the_worker_in_its_callers_place() {
    for (name, source) in [
        ("oneshot", include_str!("oneshot.rs")),
        ("start", include_str!("start.rs")),
        ("resume", include_str!("resume.rs")),
    ] {
        let capture = source
            .find("workspace = super::WorkerWorkspace::from_capabilities(&ctx.capabilities);")
            .unwrap_or_else(|| panic!("{name}: the caller's place is not captured"));
        let thread = source
            .find("std::thread::")
            .unwrap_or_else(|| panic!("{name}: no worker thread"));
        let spawn = source.find("super::spawn_runtime().await").unwrap();
        let policy = source
            .find("super::apply_subagent_runtime_policy(")
            .unwrap_or_else(|| panic!("{name}: no worker policy"));
        // Formatting-agnostic: the call may be wrapped across lines.
        let apply = policy
            + source[policy..policy + 400]
                .find("&workspace")
                .unwrap_or_else(|| panic!("{name}: the caller's place is not applied"));
        let stream = source.find("runtime.run_stream").unwrap();
        assert!(
            capture < thread && spawn < policy && apply < stream,
            "{name}"
        );
        assert_eq!(
            source.matches("WorkerWorkspace::from_capabilities(").count(),
            1,
            "{name}"
        );
    }
}

/// The worker's `bash` (compiled in with `command-exec` only).
#[cfg(feature = "command-exec")]
mod worker_bash {
    use std::sync::{Arc, Mutex};

    use serde_json::{json, Value};

    use crate::test_env::{BaseDirGuard, EnvVarGuard, OfflineProviderEnv};

    const AUTH_JSON: &str = "{\"anthropic\": {\"type\": \"oauth\", \"refresh\": \"r\", \"access\": \"synthetic-token\", \"expires\": 9999999999999}}";

    const SSE_DONE: &str = concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_wl2\",\"type\":\"message\",",
        "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-6\",\"stop_reason\":null,",
        "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,",
        "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,",
        "\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,",
        "\"delta\":{\"type\":\"text_delta\",\"text\":\"done\"}}\n\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",",
        "\"stop_sequence\":null},\"usage\":{\"input_tokens\":10,\"output_tokens\":1,",
        "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );

    /// A turn that calls `bash` with `input`.
    fn sse_bash(input: &Value) -> String {
        let partial = serde_json::to_string(&input.to_string()).unwrap();
        [
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_wl1\",\"type\":\"message\",\
             \"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-6\",\"stop_reason\":null,\
             \"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,\
             \"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n"
                .to_string(),
            "data: {\"type\":\"content_block_start\",\"index\":0,\
             \"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_wl\",\"name\":\"bash\",\"input\":{}}}\n\n"
                .to_string(),
            format!(
                "data: {{\"type\":\"content_block_delta\",\"index\":0,\
                 \"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":{partial}}}}}\n\n"
            ),
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n".to_string(),
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",\
             \"stop_sequence\":null},\"usage\":{\"input_tokens\":10,\"output_tokens\":5,\
             \"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}\n\n"
                .to_string(),
            "data: {\"type\":\"message_stop\"}\n\n".to_string(),
        ]
        .concat()
    }

    #[derive(Clone)]
    struct Stub {
        bodies: Arc<Mutex<Vec<Value>>>,
        first: Arc<String>,
    }

    /// Loopback `/v1/messages`: the first request gets `first`, every later one
    /// a plain "done". Records each request body.
    async fn stub(first: String) -> (String, Arc<Mutex<Vec<Value>>>) {
        use axum::extract::State;
        use axum::http::StatusCode;
        use axum::response::IntoResponse;
        let stub = Stub {
            bodies: Arc::default(),
            first: Arc::new(first),
        };
        let app = axum::Router::new()
            .route(
                "/v1/messages",
                axum::routing::post(|State(s): State<Stub>, body: String| async move {
                    let n = {
                        let mut bodies = s.bodies.lock().unwrap();
                        bodies.push(serde_json::from_str(&body).unwrap_or(Value::Null));
                        bodies.len()
                    };
                    let sse = if n == 1 {
                        s.first.as_str().to_string()
                    } else {
                        SSE_DONE.to_string()
                    };
                    (StatusCode::OK, [("content-type", "text/event-stream")], sse).into_response()
                }),
            )
            .layer(axum::extract::DefaultBodyLimit::max(64 * 1024 * 1024))
            .with_state(stub.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), stub.bodies)
    }

    /// The text of the `tool_result` block in a request body.
    fn tool_result_text(body: &Value) -> Option<String> {
        for message in body["messages"].as_array()? {
            for block in message["content"].as_array().into_iter().flatten() {
                if block["type"] == "tool_result" {
                    return Some(match &block["content"] {
                        Value::String(text) => text.clone(),
                        Value::Array(parts) => parts
                            .iter()
                            .filter_map(|part| part["text"].as_str())
                            .collect::<Vec<_>>()
                            .join(""),
                        other => other.to_string(),
                    });
                }
            }
        }
        None
    }

    /// Runs one oneshot worker under `config` whose first turn calls `bash`
    /// with `input`, and returns the tool result the worker sent back.
    async fn worker_bash_result(config: &str, input: Value) -> String {
        worker_bash_result_in(config, input, crate::tools::test_helpers::create_tool_context())
            .await
    }

    /// [`worker_bash_result`], called from the tool context `ctx` (the
    /// caller's capabilities, e.g. its cwd and env).
    async fn worker_bash_result_in(
        config: &str,
        input: Value,
        ctx: crate::tools::ToolContext,
    ) -> String {
        use crate::tools::Tool;
        let base = BaseDirGuard::new();
        std::fs::write(base.path().join("config"), config).unwrap();
        std::fs::write(base.path().join("auth.json"), AUTH_JSON).unwrap();
        let _offline = OfflineProviderEnv::new();
        let (url, bodies) = stub(sse_bash(&input)).await;
        let _url = EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
        let _legacy = EnvVarGuard::set("SYNAPS_SUBAGENT_FRESH_RUNTIME", "1");
        let out = super::super::oneshot::SubagentTool
            .execute(
                json!({
                    "agent": "",
                    "system_prompt": "You are a test worker.",
                    "task": "Run the command.",
                    "model": "anthropic/claude-sonnet-4-6",
                    "timeout": 60
                }),
                ctx,
            )
            .await;
        let bodies = bodies.lock().unwrap().clone();
        assert_eq!(
            bodies.len(),
            2,
            "the worker's turn, then its turn after the tool: {out:?}"
        );
        tool_result_text(&bodies[1]).unwrap_or_else(|| panic!("no tool_result in {}", bodies[1]))
    }

    /// The reported case: with no cap configured (0), a worker's long timeout
    /// is not lowered to 300 s. The same call under a 300 s cap shows the
    /// observation works.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial_test::serial(synaps_base_dir)]
    async fn a_worker_with_no_configured_cap_keeps_a_long_timeout() {
        let input = json!({"command": "echo ran", "timeout": 400});
        let uncapped = worker_bash_result("bash_max_timeout = 0\n", input.clone()).await;
        assert!(uncapped.contains("ran"), "{uncapped}");
        assert!(
            !uncapped.contains("clamped"),
            "no cap is configured, so 400 s stands: {uncapped}"
        );

        let capped = worker_bash_result("bash_max_timeout = 300\n", input).await;
        assert!(
            capped.contains("[timeout clamped from 400 s to 300 s by bash_max_timeout]"),
            "{capped}"
        );
    }

    /// A short real one: a configured cap of 2 s kills a worker's `sleep 3`
    /// that asked for 5 s, at the cap.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial_test::serial(synaps_base_dir)]
    async fn a_worker_command_is_stopped_at_the_configured_cap() {
        let result = worker_bash_result(
            "bash_max_timeout = 2\n",
            json!({"command": "sleep 3; echo finished", "timeout": 5}),
        )
        .await;
        assert!(result.contains("timed out after 2s"), "{result}");
        assert!(
            result.contains("[timeout clamped from 5 s to 2 s by bash_max_timeout]"),
            "{result}"
        );
        assert!(!result.contains("finished"), "{result}");
    }

    /// A worker's tools run in its caller's working directory and env
    /// snapshot (the caller's tool context), not in the daemon's own: the
    /// variable here is set for the caller only, never in this process.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial_test::serial(synaps_base_dir)]
    async fn a_worker_runs_in_its_callers_directory_and_env() {
        const VAR: &str = "SYNAPS_WL_CALLER_ONLY";
        assert!(std::env::var_os(VAR).is_none(), "{VAR} is the caller's alone");
        let place = tempfile::TempDir::new().unwrap();
        let dir = place.path().canonicalize().unwrap();
        let mut env: crate::session::types::SessionEnv =
            std::env::vars().filter(|(name, _)| name == "PATH").collect();
        env.push((VAR.to_string(), "from-the-caller".to_string()));
        let mut ctx = crate::tools::test_helpers::create_tool_context();
        ctx.capabilities.cwd = Some(dir.clone());
        ctx.capabilities.env = Some(env);

        let result = worker_bash_result_in(
            "",
            json!({"command": format!("pwd -P; echo \"var=${VAR}\"")}),
            ctx,
        )
        .await;
        assert!(
            result.contains(&format!("{}\n", dir.display())),
            "the worker's bash runs in the caller's directory: {result}"
        );
        assert!(
            result.contains("var=from-the-caller"),
            "the worker's bash sees the caller's env: {result}"
        );
    }

    /// The caller's env snapshot is the whole of a worker's env: a variable
    /// set in this process (the daemon's own env) but not in the caller's
    /// snapshot is absent from the worker's bash.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial_test::serial(synaps_base_dir)]
    async fn a_worker_does_not_see_the_daemons_own_env() {
        const VAR: &str = "SYNAPS_WL_DAEMON_ONLY";
        let _daemon = EnvVarGuard::set(VAR, "from-the-daemon");
        let env: crate::session::types::SessionEnv =
            std::env::vars().filter(|(name, _)| name == "PATH").collect();
        let mut ctx = crate::tools::test_helpers::create_tool_context();
        ctx.capabilities.env = Some(env);

        let result = worker_bash_result_in(
            "",
            json!({"command": format!("echo \"var=${{{VAR}:-unset}}\"")}),
            ctx,
        )
        .await;
        assert!(
            result.contains("var=unset") && !result.contains("from-the-daemon"),
            "the daemon's own variable stays out of the worker's bash: {result}"
        );
    }
}
