//! A worker's (subagent's) tool calls run its caller's `before_tool_call`
//! hooks, and a hook's `Confirm` on them is answered by the caller's confirm
//! step: the caller's prompt, its auto-approve policy and its "Allow all"
//! latch. So a worker's call gets exactly the answer its caller would get
//! for the same call. The prompt handle reaches only that confirm step,
//! never the worker's tools. `subagent.tool_hooks = off` leaves a worker
//! with no hooks, as before.
//!
//! The bash tests run a real worker against a loopback Anthropic stub (the
//! harness of `worker_limits_tests.rs`): the worker's first turn calls
//! `bash`, and the test reads the tool result the worker sends back in its
//! second request. The caller's tool call runs inside [`CALLER_HOOKS`], as
//! the stream runs every tool call.
//!
//! [`CALLER_HOOKS`]: crate::tools::CALLER_HOOKS

/// All three spawn paths capture the caller's hook route from the calling
/// tool call (before the worker thread starts, where the stream's scope is)
/// and bind it on the worker's runtime after the worker policy (its place,
/// and its transform view or the `subagent_transform_hooks = off` detach,
/// which the bound route keeps), before its stream.
#[test]
fn all_spawn_paths_route_the_workers_hooks_to_its_caller() {
    for (name, source, id) in [
        ("oneshot", include_str!("oneshot.rs"), "&orchestration_id"),
        ("start", include_str!("start.rs"), "&handle_id"),
        ("resume", include_str!("resume.rs"), "&handle_id"),
    ] {
        let capture = source
            .find(&format!(
                "let worker_hooks = super::WorkerHooks::of(&ctx.capabilities, {id});"
            ))
            .unwrap_or_else(|| panic!("{name}: the caller's hook route is not captured"));
        let thread = source
            .find("std::thread::")
            .unwrap_or_else(|| panic!("{name}: no worker thread"));
        let place = source.find("super::apply_subagent_runtime_policy(").unwrap();
        let apply = source
            .find("worker_hooks.apply(&mut runtime, &config);")
            .unwrap_or_else(|| panic!("{name}: the caller's hook route is not applied"));
        let stream = source.find("runtime.run_stream").unwrap();
        assert!(
            capture < thread && place < apply && apply < stream,
            "{name}"
        );
        assert_eq!(source.matches("WorkerHooks::of(").count(), 1, "{name}");
        assert_eq!(source.matches("worker_hooks.apply(").count(), 1, "{name}");
    }
}

/// A worker takes its caller's memory-only globs (a data-handling policy,
/// so also with `subagent.tool_hooks = off`), never the caller's raw results.
#[tokio::test]
// It swaps the process's base dir (BaseDirGuard): keyed like every other
// base-dir test, or a concurrent one reads its config and auth from here.
#[serial_test::serial(synaps_base_dir)]
async fn memory_only_policy_is_inherited_by_a_worker_even_with_hooks_off() {
    use crate::tools::{CallerHooks, CALLER_HOOKS};
    let _base = crate::test_env::BaseDirGuard::new();
    let mut policy = crate::runtime::memory_only::Policy::default();
    policy.set_session(vec!["memfix:*".into()]);
    let hooks = CallerHooks {
        bus: std::sync::Arc::new(crate::extensions::hooks::HookBus::new()),
        auto_approve: false,
        memory_only: policy,
    };
    let ctx = crate::tools::test_helpers::create_tool_context();
    let worker = CALLER_HOOKS
        .scope(hooks, async { super::WorkerHooks::of(&ctx.capabilities, "sa_1") })
        .await;
    let mut config = crate::config::SynapsConfig::default();
    config.subagent.tool_hooks = false;
    let mut runtime = crate::Runtime::new().await.unwrap();
    assert!(runtime.memory_only_globs().is_empty());
    worker.apply(&mut runtime, &config);
    assert_eq!(runtime.memory_only_globs(), vec!["memfix:*"]);
    assert_eq!(runtime.memory_only_status().live, 0);
}

/// A worker's `bash` (compiled in with `command-exec` only).
#[cfg(feature = "command-exec")]
mod worker_bash {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use serde_json::{json, Value};

    use crate::extensions::hooks::events::{HookEvent, HookKind, HookResult};
    use crate::extensions::hooks::HookBus;
    use crate::extensions::permissions::{Permission, PermissionSet};
    use crate::test_env::{BaseDirGuard, EnvVarGuard, OfflineProviderEnv};
    use crate::tools::{
        CallerHooks, SecretPromptHandle, SecretPromptRequest, SessionConfirmLatch,
        SubagentRegistry, Tool, ToolContext, CALLER_HOOKS,
    };

    const AUTH_JSON: &str = "{\"anthropic\": {\"type\": \"oauth\", \"refresh\": \"r\", \"access\": \"synthetic-token\", \"expires\": 9999999999999}}";

    const SSE_DONE: &str = concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_wh2\",\"type\":\"message\",",
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
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_wh1\",\"type\":\"message\",\
             \"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-6\",\"stop_reason\":null,\
             \"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,\
             \"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n"
                .to_string(),
            "data: {\"type\":\"content_block_start\",\"index\":0,\
             \"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_wh\",\"name\":\"bash\",\"input\":{}}}\n\n"
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

    /// Loopback `/v1/messages`: the first request gets `first`, every later
    /// one a plain "done". Records each request body.
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

    /// The worker's command; only running it prints [`RAN`].
    const COMMAND: &str = "echo worker-$((40+2))";
    const RAN: &str = "worker-42";

    /// The caller's guard extension: records each event it sees and answers
    /// every one with `result`.
    struct Guard {
        seen: Arc<Mutex<Vec<HookEvent>>>,
        result: HookResult,
    }

    #[async_trait]
    impl crate::extensions::runtime::ExtensionHandler for Guard {
        fn id(&self) -> &str {
            "guard"
        }

        async fn handle(&self, event: &HookEvent) -> HookResult {
            self.seen.lock().unwrap().push(event.clone());
            self.result.clone()
        }

        async fn shutdown(&self) {}
    }

    fn confirm() -> HookResult {
        HookResult::Confirm {
            message: "guard wants a look".into(),
            sources: vec![],
            generations: vec![],
            tool: None,
            modified_input: None,
            modified_by: vec![],
        }
    }

    fn block() -> HookResult {
        HookResult::Block {
            reason: "guard says no".into(),
        }
    }

    /// The caller's hook bus, with the `before_tool_call` extension "guard"
    /// answering `result`, and what the guard saw.
    async fn caller_bus(result: HookResult) -> (Arc<HookBus>, Arc<Mutex<Vec<HookEvent>>>) {
        let bus = Arc::new(HookBus::new());
        let seen: Arc<Mutex<Vec<HookEvent>>> = Arc::default();
        let mut permissions = PermissionSet::new();
        permissions.grant(Permission::ToolsIntercept);
        bus.subscribe(
            HookKind::BeforeToolCall,
            Arc::new(Guard {
                seen: Arc::clone(&seen),
                result,
            }),
            None,
            None,
            permissions,
        )
        .await
        .unwrap();
        (bus, seen)
    }

    /// Every prompt request seen: (title, body).
    type Prompts = Arc<Mutex<Vec<(String, String)>>>;

    /// The caller's prompt: records each request and answers it `answer`
    /// (`None` = deny / cancel). Abort the task when done.
    fn caller_prompt(
        answer: Option<&'static str>,
    ) -> (SecretPromptHandle, Prompts, tokio::task::JoinHandle<()>) {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SecretPromptRequest>();
        let prompts = Prompts::default();
        let seen = Arc::clone(&prompts);
        let task = tokio::spawn(async move {
            while let Some(request) = rx.recv().await {
                seen.lock()
                    .unwrap()
                    .push((request.title.clone(), request.prompt.clone()));
                let _ = request.response_tx.send(answer.map(str::to_string));
            }
        });
        (SecretPromptHandle::new(tx), prompts, task)
    }

    /// A caller in conversation "sess-parent" with `prompt`, an unarmed
    /// "Allow all" latch, and the stream's hook route for its tool call.
    fn caller(
        bus: &Arc<HookBus>,
        prompt: Option<SecretPromptHandle>,
        auto_approve: bool,
    ) -> (ToolContext, CallerHooks) {
        let mut ctx = crate::tools::test_helpers::create_tool_context();
        ctx.capabilities.session_id = Some("sess-parent".into());
        ctx.capabilities.secret_prompt = prompt;
        ctx.capabilities.session_allow_all = Some(Arc::new(SessionConfirmLatch::new()));
        (
            ctx,
            CallerHooks {
                bus: Arc::clone(bus),
                auto_approve,
                memory_only: Default::default(),
            },
        )
    }

    #[derive(Clone, Copy, Debug)]
    enum Spawn {
        /// The blocking `subagent` tool: the caller's turn waits in it.
        Oneshot,
        /// `subagent_start`: the caller's tool call returns at once and the
        /// worker runs on; the test then waits for the worker thread.
        Start,
    }

    /// Runs one worker under `config`, started from the caller's tool call
    /// (`hooks` in scope, as the stream sets it), whose first turn calls
    /// `bash` with `command`. Returns the tool result the worker sent back
    /// and, for `subagent_start`, the handle id.
    async fn worker_bash(
        spawn: Spawn,
        config: &str,
        command: &str,
        mut ctx: ToolContext,
        hooks: CallerHooks,
    ) -> (String, Option<String>) {
        let base = BaseDirGuard::new();
        std::fs::write(base.path().join("config"), config).unwrap();
        std::fs::write(base.path().join("auth.json"), AUTH_JSON).unwrap();
        let _offline = OfflineProviderEnv::new();
        let (url, bodies) = stub(sse_bash(&json!({ "command": command }))).await;
        let _url = EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
        let _legacy = EnvVarGuard::set("SYNAPS_SUBAGENT_FRESH_RUNTIME", "1");
        let params = json!({
            "system_prompt": "You are a test worker.",
            "task": "Run the command.",
            "model": "anthropic/claude-sonnet-4-6",
            "timeout": 60
        });
        let (out, handle_id) = match spawn {
            Spawn::Oneshot => {
                let out = CALLER_HOOKS
                    .scope(hooks, super::super::oneshot::SubagentTool.execute(params, ctx))
                    .await;
                (format!("{out:?}"), None)
            }
            Spawn::Start => {
                let registry = Arc::new(Mutex::new(SubagentRegistry::new()));
                ctx.capabilities.subagent_registry = Some(Arc::clone(&registry));
                let out = CALLER_HOOKS
                    .scope(hooks, super::super::start::SubagentStartTool.execute(params, ctx))
                    .await
                    .expect("dispatch");
                // The caller's tool call has returned (its scope is gone);
                // the worker runs on and its confirm still reaches the caller.
                let handle_id = serde_json::from_str::<Value>(&out).unwrap()["handle_id"]
                    .as_str()
                    .unwrap()
                    .to_string();
                let thread = registry
                    .lock()
                    .unwrap()
                    .get_mut(&handle_id)
                    .and_then(|handle| handle.thread_handle.take())
                    .expect("the worker thread");
                tokio::task::spawn_blocking(move || thread.join())
                    .await
                    .unwrap()
                    .unwrap();
                (out, Some(handle_id))
            }
        };
        let bodies = bodies.lock().unwrap().clone();
        assert_eq!(
            bodies.len(),
            2,
            "{spawn:?}: the worker's turn, then its turn after the tool: {out}"
        );
        let result = tool_result_text(&bodies[1])
            .unwrap_or_else(|| panic!("no tool_result in {}", bodies[1]));
        (result, handle_id)
    }

    /// (1) A worker's call that the caller's guard wants confirmed is shown
    /// on the caller's prompt, while the caller's turn waits in `subagent`
    /// and after its `subagent_start` call has returned, and runs once the
    /// user allows it. The guard saw the call with the caller's conversation
    /// and the worker's id.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial_test::serial(synaps_base_dir)]
    async fn a_worker_confirm_is_answered_on_the_callers_prompt() {
        for spawn in [Spawn::Oneshot, Spawn::Start] {
            let (bus, seen) = caller_bus(confirm()).await;
            let (prompt, prompts, answering) = caller_prompt(Some("y"));
            let (ctx, hooks) = caller(&bus, Some(prompt), false);
            let (result, handle_id) = worker_bash(spawn, "", COMMAND, ctx, hooks).await;
            answering.abort();
            assert!(result.contains(RAN), "{spawn:?}: allowed, so it ran: {result}");

            let prompts = prompts.lock().unwrap();
            assert_eq!(prompts.len(), 1, "{spawn:?}: {prompts:?}");
            let (title, body) = &prompts[0];
            assert_eq!(title, crate::session::CONFIRM_PROMPT_TITLE, "{spawn:?}");
            assert!(
                body.contains("Extension: guard")
                    && body.contains("Tool: bash")
                    && body.contains("guard wants a look"),
                "{spawn:?}: {body}"
            );

            let seen = seen.lock().unwrap();
            assert_eq!(seen.len(), 1, "{spawn:?}: the caller's guard ran once");
            assert_eq!(seen[0].tool_name.as_deref(), Some("bash"), "{spawn:?}");
            assert_eq!(seen[0].session_id.as_deref(), Some("sess-parent"), "{spawn:?}");
            let id = seen[0].subagent_id.as_deref().unwrap_or_default();
            assert!(id.starts_with("sa_"), "{spawn:?}: subagent_id {id:?}");
            if let Some(handle_id) = handle_id {
                assert_eq!(id, handle_id, "the worker's id as its caller sees it");
            }
        }
    }

    /// A worker's call the user denies on the caller's prompt does not run;
    /// a caller with no prompt gets what it gets for its own call (fails
    /// closed).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial_test::serial(synaps_base_dir)]
    async fn a_worker_call_denied_on_the_callers_prompt_does_not_run() {
        let (bus, _seen) = caller_bus(confirm()).await;
        let (prompt, prompts, answering) = caller_prompt(None);
        let (ctx, hooks) = caller(&bus, Some(prompt), false);
        let (result, _) = worker_bash(Spawn::Oneshot, "", COMMAND, ctx, hooks).await;
        answering.abort();
        assert!(
            result.contains("Tool call confirmation denied") && !result.contains(RAN),
            "{result}"
        );
        assert_eq!(prompts.lock().unwrap().len(), 1);

        let (ctx, hooks) = caller(&bus, None, false);
        let (result, _) = worker_bash(Spawn::Oneshot, "", COMMAND, ctx, hooks).await;
        assert!(
            result.contains("no interactive prompt is available") && !result.contains(RAN),
            "{result}"
        );
    }

    /// (2) Where the caller's own call would go through without a prompt,
    /// so does the worker's: a caller latched on "Allow all" for the guard,
    /// and a caller that auto-approves confirms.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial_test::serial(synaps_base_dir)]
    async fn a_worker_call_goes_through_without_a_prompt_where_the_callers_would() {
        for auto_approve in [false, true] {
            let (bus, seen) = caller_bus(confirm()).await;
            let (prompt, prompts, answering) = caller_prompt(Some("y"));
            let (ctx, hooks) = caller(&bus, Some(prompt), auto_approve);
            if !auto_approve {
                let key = ("guard".to_string(), bus.extension_generation("guard"));
                assert!(ctx
                    .capabilities
                    .session_allow_all
                    .as_ref()
                    .unwrap()
                    .allow(&[key]));
            }
            let (result, _) = worker_bash(Spawn::Oneshot, "", COMMAND, ctx, hooks).await;
            answering.abort();
            assert!(result.contains(RAN), "auto_approve={auto_approve}: {result}");
            assert!(
                prompts.lock().unwrap().is_empty(),
                "auto_approve={auto_approve}: no prompt"
            );
            assert_eq!(seen.lock().unwrap().len(), 1, "the guard still ran");
        }
    }

    /// A command that asks for a password on stderr, then echoes the answer.
    const PASSWORD_PROBE: &str =
        "printf '[sudo] password for probe: ' >&2; read -r -t 5 answer; echo \"got=${answer:-none}\"";

    /// (3) The caller's prompt handle answers the worker's confirm only: a
    /// password prompt in a worker's command never reaches the caller's
    /// prompt, and nothing is fed to the command's stdin. Control: in the
    /// caller's own tool context sudo's password request does reach it.
    ///
    /// PARITY (upstream e8f30eecc): stderr text no longer raises a prompt
    /// anywhere; sudo asks only through the verified askpass broker. So the
    /// worker's probe is plain output (`got=none`), and the control asks
    /// through the test askpass broker with a fake sudo instead of stderr.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial_test::serial(synaps_base_dir)]
    async fn a_worker_tool_cannot_prompt_through_the_callers_handle() {
        let (bus, _seen) = caller_bus(confirm()).await;
        let (prompt, prompts, answering) = caller_prompt(Some("y"));
        let (ctx, hooks) = caller(&bus, Some(prompt.clone()), false);
        let (result, _) = worker_bash(Spawn::Oneshot, "", PASSWORD_PROBE, ctx, hooks).await;
        assert!(
            result.contains("got=none") && !result.contains("got=y"),
            "{result}"
        );
        let titles: Vec<String> = prompts.lock().unwrap().iter().map(|p| p.0.clone()).collect();
        assert_eq!(
            titles,
            vec![crate::session::CONFIRM_PROMPT_TITLE.to_string()],
            "only the guard's confirm reached the caller's prompt"
        );

        #[cfg(target_os = "linux")]
        {
            let fake = tempfile::tempdir().unwrap();
            let helper = {
                // Writes the fake sudo, then execs it.
                let _exec = crate::test_env::SCRIPT_EXEC_LOCK.lock();
                crate::tools::sudo_askpass::testing::write_fixtures(fake.path()).unwrap()
            };
            let mut own = crate::tools::test_helpers::create_tool_context();
            own.capabilities.secret_prompt = Some(prompt);
            let command = format!(
                "export PATH=\"{}:$PATH\"; FAKE_SUDO_ECHO=1 sudo echo ran; echo after:$?",
                fake.path().display()
            );
            let out = crate::tools::sudo_askpass::testing::run_bash_with_test_askpass(
                &command,
                Some(30),
                own,
                &helper,
            )
            .await
            .unwrap();
            answering.abort();
            // The answer went in as a secret: bash redacts it from the output.
            assert!(out.contains("sudo got: [redacted]"), "{out}");
            let prompts = prompts.lock().unwrap();
            assert_eq!(prompts.len(), 2, "{prompts:?}");
            assert!(prompts[1].0.contains("password required"), "{prompts:?}");
        }
        #[cfg(not(target_os = "linux"))]
        answering.abort();
    }

    /// (4) `subagent.tool_hooks = off` leaves a worker with no hooks (its
    /// call runs and the caller's guard never sees it); `on` routes it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial_test::serial(synaps_base_dir)]
    async fn subagent_tool_hooks_off_leaves_the_worker_without_hooks() {
        for (config, routed) in [
            ("subagent.tool_hooks = off\n", false),
            ("subagent.tool_hooks = on\n", true),
        ] {
            let (bus, seen) = caller_bus(block()).await;
            let (ctx, hooks) = caller(&bus, None, false);
            let (result, _) = worker_bash(Spawn::Oneshot, config, COMMAND, ctx, hooks).await;
            let seen = seen.lock().unwrap().len();
            if routed {
                assert!(
                    result.contains("Tool call blocked by extension: guard says no")
                        && !result.contains(RAN),
                    "{config}: {result}"
                );
                assert_eq!(seen, 1, "{config}");
            } else {
                assert!(result.contains(RAN), "{config}: {result}");
                assert_eq!(seen, 0, "{config}: the guard never saw it");
            }
        }
    }
}
