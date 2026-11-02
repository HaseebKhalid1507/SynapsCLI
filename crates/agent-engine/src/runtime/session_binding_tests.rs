//! Daemon shape: every foreground runtime of one host shares ONE tool
//! registry (`HostParts::tools`). A `context_checkpoint` call must record
//! into the calling session's own continuation state, whichever session
//! started a turn last, and a session's next rollover successor must carry
//! only its own working note. Loopback inference only.

use crate::SharedMessage;
use agent_core::config::ContextManagementMode;
use agent_core::core::context_policy::WorkPhase;
use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::post, Router};
use futures::StreamExt;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

const TAGS: [&str; 2] = ["SESSION-A", "SESSION-B"];

/// What the next turn of each session answers with.
#[derive(Clone, Default)]
struct Plan {
    /// Tag -> the (tool name, input) calls its first round returns, in
    /// order (ids `toolu_cp`, `toolu_2`, ...). A tag without an entry
    /// answers with plain text.
    calls: HashMap<&'static str, Vec<(&'static str, Value)>>,
    /// `(held, until)`: the held session's first-round response waits until
    /// the `until` session's first request has arrived, i.e. until that
    /// session has finished its turn-start setup.
    hold: Option<(&'static str, &'static str)>,
}

#[derive(Clone)]
struct Mock {
    plan: Arc<Mutex<Plan>>,
    arrived: Arc<tokio::sync::watch::Sender<HashSet<&'static str>>>,
    bodies: Arc<Mutex<Vec<(&'static str, String)>>>,
}

impl Mock {
    fn step(
        &self,
        checkpoints: &[(&'static str, &'static str, &'static str)],
        hold: Option<(&'static str, &'static str)>,
    ) {
        let calls = checkpoints
            .iter()
            .map(|(tag, phase, note)| {
                (*tag, "context_checkpoint", json!({"phase": phase, "note": note}))
            })
            .collect::<Vec<_>>();
        self.step_calls(&calls, hold);
    }

    fn step_calls(
        &self,
        calls: &[(&'static str, &'static str, Value)],
        hold: Option<(&'static str, &'static str)>,
    ) {
        let mut planned: HashMap<&'static str, Vec<(&'static str, Value)>> = HashMap::new();
        for (tag, name, input) in calls {
            planned.entry(*tag).or_default().push((*name, input.clone()));
        }
        *self.plan.lock().unwrap() = Plan {
            calls: planned,
            hold,
        };
        self.arrived.send_replace(HashSet::new());
    }

    /// Resolves once `tag`'s first request of the current step has arrived,
    /// i.e. once that session has finished its turn-start setup.
    async fn arrival(&self, tag: &'static str) {
        let mut rx = self.arrived.subscribe();
        tokio::time::timeout(
            std::time::Duration::from_secs(20),
            rx.wait_for(|set| set.contains(tag)),
        )
        .await
        .expect("the session reaches its first request")
        .unwrap();
    }

    fn bodies_of(&self, tag: &str) -> Vec<String> {
        self.bodies
            .lock()
            .unwrap()
            .iter()
            .filter(|(t, _)| *t == tag)
            .map(|(_, b)| b.clone())
            .collect()
    }
}

fn sse(events: Vec<Value>) -> String {
    events.iter().map(|e| format!("data: {e}\n\n")).collect()
}

fn message_start() -> Value {
    json!({"type":"message_start","message":{"id":"msg_b","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-6","stop_reason":null,"stop_sequence":null}})
}

fn text_sse() -> String {
    sse(vec![
        message_start(),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null}}),
        json!({"type":"message_stop"}),
    ])
}

fn call_id(index: usize) -> String {
    if index == 0 {
        "toolu_cp".to_string()
    } else {
        format!("toolu_{}", index + 1)
    }
}

fn tool_sse(calls: &[(&str, Value)]) -> String {
    let mut events = vec![message_start()];
    for (index, (name, input)) in calls.iter().enumerate() {
        events.push(json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":call_id(index),"name":name,"input":{}}}));
        events.push(json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":input.to_string()}}));
        events.push(json!({"type":"content_block_stop","index":index}));
    }
    events.push(json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null}}));
    events.push(json!({"type":"message_stop"}));
    sse(events)
}

/// The non-streaming (`run_single`) form of a response.
fn message_json(calls: Option<&[(&str, Value)]>) -> Value {
    let (content, stop) = match calls {
        Some(calls) => (
            calls
                .iter()
                .enumerate()
                .map(|(index, (name, input))| {
                    json!({"type":"tool_use","id":call_id(index),"name":name,"input":input})
                })
                .collect::<Vec<_>>(),
            "tool_use",
        ),
        None => (vec![json!({"type":"text","text":"ok"})], "end_turn"),
    };
    json!({"id":"msg_b","type":"message","role":"assistant","content":content,
        "model":"claude-sonnet-4-6","stop_reason":stop,"stop_sequence":null,
        "usage":{"input_tokens":10,"output_tokens":5}})
}

async fn respond(State(m): State<Mock>, body: String) -> axum::response::Response {
    let tag = TAGS
        .into_iter()
        .find(|t| body.contains(t))
        .expect("every request carries its session tag");
    m.bodies.lock().unwrap().push((tag, body.clone()));
    let first_round = !body.contains("\"tool_result\"");
    let (call, hold) = {
        let plan = m.plan.lock().unwrap();
        (plan.calls.get(tag).cloned(), plan.hold)
    };
    if first_round {
        m.arrived.send_modify(|set| {
            set.insert(tag);
        });
    }
    let calls = call.filter(|_| first_round);
    if calls.is_some() {
        if let Some((held, until)) = hold {
            if held == tag {
                m.arrival(until).await;
            }
        }
    }
    let streaming = serde_json::from_str::<Value>(&body)
        .map(|parsed| parsed["stream"] == true)
        .unwrap_or(false);
    if !streaming {
        return (StatusCode::OK, axum::Json(message_json(calls.as_deref()))).into_response();
    }
    let sse = match &calls {
        Some(calls) => tool_sse(calls),
        None => text_sse(),
    };
    (
        StatusCode::OK,
        [("content-type", "text/event-stream")],
        sse,
    )
        .into_response()
}

async fn spawn() -> (String, Mock) {
    let mock = Mock {
        plan: Arc::default(),
        arrived: Arc::new(tokio::sync::watch::channel(HashSet::new()).0),
        bodies: Arc::default(),
    };
    let app = Router::new()
        .route("/v1/messages", post(respond))
        .with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), mock)
}

fn runtime(mode: ContextManagementMode, progressive: bool) -> crate::Runtime {
    let mut rt = crate::Runtime::new_headless();
    rt.set_model("claude-sonnet-4-6".to_string());
    rt.set_api_retries(1);
    rt.set_context_window(Some(500_000));
    rt.progressive_tool_disclosure = progressive;
    rt.continuation.lock().unwrap().config.mode = mode;
    rt
}

/// Two foreground runtimes of one host: one shared tool registry and hook
/// bus (`HostParts`), separate sessions and continuation states.
fn host_pair(
    a_mode: ContextManagementMode,
    b_mode: ContextManagementMode,
    progressive: bool,
) -> (crate::Runtime, crate::Runtime) {
    let a = runtime(a_mode, progressive);
    let mut b = runtime(b_mode, progressive);
    b.tools = Arc::clone(&a.tools);
    b.hook_bus = Arc::clone(&a.hook_bus);
    (a, b)
}

#[derive(Default)]
struct Turn {
    history: Vec<SharedMessage>,
    heads: Vec<Vec<SharedMessage>>,
    errors: Vec<String>,
}

async fn turn(rt: &crate::Runtime, messages: Vec<SharedMessage>) -> Turn {
    let mut stream = rt
        .run_stream_with_messages(messages, Default::default(), None, None, false)
        .await;
    let mut out = Turn::default();
    while let Some(event) =
        tokio::time::timeout(std::time::Duration::from_secs(30), stream.next())
            .await
            .expect("turn makes progress")
    {
        match event {
            agent_core::StreamEvent::Session(agent_core::SessionEvent::Error(e)) => {
                out.errors.push(e.message)
            }
            agent_core::StreamEvent::Session(agent_core::SessionEvent::MessageHistory(h)) => {
                out.history = h
            }
            agent_core::StreamEvent::Session(agent_core::SessionEvent::ContextHeadCheckpoint {
                messages,
                receipt,
                ..
            }) => {
                out.heads.push(messages);
                receipt.complete(Ok(()));
            }
            _ => {}
        }
    }
    out
}

fn tagged(tag: &str, step: usize) -> Vec<SharedMessage> {
    vec![Arc::new(
        json!({"role":"user","content":format!("{tag}: step {step}")}),
    )]
}

fn note(rt: &crate::Runtime) -> String {
    rt.continuation.lock().unwrap().note.clone()
}

fn phase(rt: &crate::Runtime) -> WorkPhase {
    rt.continuation.lock().unwrap().policy.phase()
}

/// The planned call's tool result in a turn's published history.
fn checkpoint_result(history: &[SharedMessage]) -> Value {
    history
        .iter()
        .filter_map(|m| m["content"].as_array())
        .flatten()
        .find(|b| b["type"] == "tool_result" && b["tool_use_id"] == "toolu_cp")
        .cloned()
        .expect("the planned call's tool result in history")
}

/// Whether a `context_checkpoint` result is the tool's own success receipt
/// (an unknown-tool or refused call is not, flagged or not).
fn recorded(result: &Value) -> bool {
    result["is_error"] != true
        && result["content"]
            .as_str()
            .is_some_and(|text| text.starts_with("phase="))
}

fn assert_recorded(t: &Turn, label: &str) {
    assert!(t.errors.is_empty(), "{label}: turn errors {:?}", t.errors);
    let result = checkpoint_result(&t.history);
    assert!(recorded(&result), "{label}: {result}");
}

/// Low soft thresholds so the big history forces a rollover at the first
/// round of the turn.
fn low_thresholds(rt: &crate::Runtime) {
    let mut s = rt.continuation.lock().unwrap();
    s.config.pressure_tokens = Some(60_000);
    s.config.rollover_tokens = Some(80_000);
    s.config.target_tokens = Some(25_000);
}

fn big_history(tag: &str) -> Vec<SharedMessage> {
    let mut m = vec![Arc::new(
        json!({"role":"user","content":format!("{tag}: charter")}),
    )];
    m.extend((0..260).map(|i| {
        Arc::new(json!({"role":"user","content":format!(
            "<event id=\"e{i}\" type=\"worker_report\" severity=\"low\" source=\"worker-{}\">report {i}: {}</event>",
            i % 4,
            "x".repeat(1_100)
        )})) as SharedMessage
    }));
    m.push(Arc::new(json!({"role":"user","content":"next"})));
    m
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(synaps_base_dir)]
async fn checkpoints_on_a_shared_registry_record_into_the_calling_sessions_own_state() {
    for progressive in [false, true] {
        let _base = crate::test_env::BaseDirGuard::new();
        let (url, mock) = spawn().await;
        let _url = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
        let auto = ContextManagementMode::Auto;
        let (a, b) = host_pair(auto, auto, progressive);

        // A starts its turn first; its call executes after B's turn start.
        mock.step(
            &[
                ("SESSION-A", "plan", "NOTE-A-FIRST"),
                ("SESSION-B", "execute", "NOTE-B-FIRST"),
            ],
            Some(("SESSION-A", "SESSION-B")),
        );
        let (ta, tb) = tokio::join!(turn(&a, tagged("SESSION-A", 1)), turn(&b, tagged("SESSION-B", 1)));
        assert_recorded(&ta, "A1");
        assert_recorded(&tb, "B1");
        assert_eq!(note(&a), "NOTE-A-FIRST", "progressive={progressive}");
        assert_eq!(phase(&a), WorkPhase::Plan, "progressive={progressive}");
        assert_eq!(note(&b), "NOTE-B-FIRST", "progressive={progressive}");
        assert_eq!(phase(&b), WorkPhase::Execute, "progressive={progressive}");

        // B starts first now; its call executes after A's turn start.
        mock.step(
            &[
                ("SESSION-A", "execute", "NOTE-A-SECOND"),
                ("SESSION-B", "new_task", "NOTE-B-SECOND"),
            ],
            Some(("SESSION-B", "SESSION-A")),
        );
        let (tb, ta) = tokio::join!(turn(&b, tagged("SESSION-B", 2)), turn(&a, tagged("SESSION-A", 2)));
        assert_recorded(&ta, "A2");
        assert_recorded(&tb, "B2");
        assert_eq!(note(&a), "NOTE-A-SECOND", "progressive={progressive}");
        assert_eq!(phase(&a), WorkPhase::Execute, "progressive={progressive}");
        assert_eq!(note(&b), "NOTE-B-SECOND", "progressive={progressive}");
        assert_eq!(phase(&b), WorkPhase::NewTask, "progressive={progressive}");

        // Each session's next rollover successor carries only its own note.
        // (Final phases avoid plan/wrap_up: those admit a bounded finish
        // round before rolling over.)
        mock.step(&[], None);
        for (rt, tag, own, other) in [
            (&a, "SESSION-A", "NOTE-A-SECOND", "NOTE-B-"),
            (&b, "SESSION-B", "NOTE-B-SECOND", "NOTE-A-"),
        ] {
            low_thresholds(rt);
            let out = turn(rt, big_history(tag)).await;
            assert!(out.errors.is_empty(), "{tag}: {:?}", out.errors);
            assert_eq!(out.heads.len(), 1, "{tag}: exactly one rollover");
            let head = serde_json::to_string(&out.heads[0]).unwrap();
            assert!(head.contains("Working note"), "{tag}: successor head");
            assert!(head.contains(own), "{tag}: own note in successor");
            assert!(!head.contains(other), "{tag}: other session's note leaked");
            let sent = mock.bodies_of(tag);
            let last = sent.last().expect("request after rollover");
            assert!(last.contains(own), "{tag}: own note on the wire");
            for body in &sent {
                assert!(!body.contains(other), "{tag}: other session's note on the wire");
            }
        }
    }
}

/// An off session's turn start must not take the tool away from an auto
/// session of the same host: the auto session's next call still records
/// into its own state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(synaps_base_dir)]
async fn an_off_sessions_turn_leaves_the_auto_session_its_checkpoint() {
    for progressive in [false, true] {
        let _base = crate::test_env::BaseDirGuard::new();
        let (url, mock) = spawn().await;
        let _url = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
        let (a, b) = host_pair(
            ContextManagementMode::Auto,
            ContextManagementMode::Off,
            progressive,
        );
        mock.step(&[("SESSION-A", "plan", "NOTE-A-ONLY")], None);
        let ta = turn(&a, tagged("SESSION-A", 1)).await;
        assert_recorded(&ta, "A1");
        let tb = turn(&b, tagged("SESSION-B", 1)).await;
        assert!(tb.errors.is_empty(), "off session: {:?}", tb.errors);
        mock.step(&[("SESSION-A", "execute", "NOTE-A-AGAIN")], None);
        let ta = turn(&a, tagged("SESSION-A", 2)).await;
        assert_recorded(&ta, "A2");
        assert_eq!(note(&a), "NOTE-A-AGAIN", "progressive={progressive}");
        assert_eq!(phase(&a), WorkPhase::Execute, "progressive={progressive}");
        assert!(note(&b).is_empty(), "off session state untouched");
        assert_eq!(phase(&b), WorkPhase::Unknown, "off session state untouched");
    }
}

/// Once registered, repeated turns of sessions sharing the registry do not
/// advance the catalog generation (no per-turn rebuild of other sessions'
/// tool sets).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(synaps_base_dir)]
async fn repeated_turns_leave_the_shared_catalog_generation_unchanged() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (url, mock) = spawn().await;
    let _url = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let auto = ContextManagementMode::Auto;
    let (a, b) = host_pair(auto, auto, false);
    mock.step(
        &[
            ("SESSION-A", "plan", "NOTE-A"),
            ("SESSION-B", "plan", "NOTE-B"),
        ],
        None,
    );
    assert_recorded(&turn(&a, tagged("SESSION-A", 1)).await, "A1");
    let generation = a.tools.read().await.catalog().generation();
    for step in 2..5 {
        assert_recorded(&turn(&b, tagged("SESSION-B", step)).await, "B");
        assert_recorded(&turn(&a, tagged("SESSION-A", step)).await, "A");
        assert_eq!(
            a.tools.read().await.catalog().generation(),
            generation,
            "turn {step} advanced the shared catalog generation"
        );
    }
}

/// An extension's advisory phase report reaches only the session whose
/// hook emission it answers, even though the hook bus is host-wide.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(synaps_base_dir)]
async fn a_hook_phase_report_reaches_only_the_emitting_session() {
    use crate::extensions::hooks::events::{HookEvent, HookKind, HookResult};
    use crate::extensions::permissions::{Permission, PermissionSet};
    struct WrapUp;
    #[async_trait::async_trait]
    impl crate::extensions::runtime::ExtensionHandler for WrapUp {
        fn id(&self) -> &str {
            "wrap-up-hook"
        }
        async fn handle(&self, _event: &HookEvent) -> HookResult {
            HookResult::ContextPhase {
                phase: "wrap_up".into(),
            }
        }
        async fn shutdown(&self) {}
    }
    let _base = crate::test_env::BaseDirGuard::new();
    let (url, mock) = spawn().await;
    let _url = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let auto = ContextManagementMode::Auto;
    let (a, b) = host_pair(auto, auto, false);
    let mut perms = PermissionSet::new();
    perms.grant(Permission::LlmContent);
    perms.grant(Permission::SessionLifecycle);
    a.hook_bus
        .subscribe(HookKind::OnMessageComplete, Arc::new(WrapUp), None, None, perms)
        .await
        .unwrap();
    mock.step(&[], None);
    let ta = turn(&a, tagged("SESSION-A", 1)).await;
    assert!(ta.errors.is_empty(), "{:?}", ta.errors);
    assert_eq!(phase(&a), WorkPhase::WrapUp);
    assert_eq!(phase(&b), WorkPhase::Unknown, "other session untouched");
}

/// Tool calls made inside an extension-provider route (the provider's own
/// tool loop) see the calling session's memory binding, never a binding
/// resolved from process configuration and cwd.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(synaps_base_dir, extension_routing)]
async fn extension_provider_tool_calls_carry_the_sessions_own_memory_binding() {
    use crate::extensions::hooks::events::{HookEvent, HookResult};
    use crate::extensions::runtime::process::{
        ProviderCompleteParams, ProviderCompleteResult, RegisteredProviderModelSpec,
        RegisteredProviderSpec,
    };
    type Seen = Arc<Mutex<Vec<Option<crate::memory_backend::MemoryBinding>>>>;

    struct ProbeThenText;
    #[async_trait::async_trait]
    impl crate::extensions::runtime::ExtensionHandler for ProbeThenText {
        fn id(&self) -> &str {
            "probe-provider"
        }
        async fn handle(&self, _event: &HookEvent) -> HookResult {
            HookResult::Continue
        }
        async fn shutdown(&self) {}
        async fn provider_complete(
            &self,
            params: ProviderCompleteParams,
        ) -> Result<ProviderCompleteResult, String> {
            let answered = params.messages.iter().any(|m| {
                m["content"]
                    .as_array()
                    .is_some_and(|blocks| blocks.iter().any(|b| b["type"] == "tool_result"))
            });
            Ok(if answered {
                ProviderCompleteResult {
                    content: vec![json!({"type":"text","text":"done"})],
                    stop_reason: Some("end_turn".into()),
                    usage: None,
                }
            } else {
                ProviderCompleteResult {
                    content: vec![json!({"type":"tool_use","id":"call-probe","name":"binding_probe","input":{}})],
                    stop_reason: Some("tool_use".into()),
                    usage: None,
                }
            })
        }
    }

    struct BindingProbe(Seen);
    #[async_trait::async_trait]
    impl crate::tools::Tool for BindingProbe {
        fn name(&self) -> &str {
            "binding_probe"
        }
        fn description(&self) -> &str {
            "records the memory binding its context carries"
        }
        fn parameters(&self) -> Value {
            json!({"type":"object"})
        }
        fn origin(&self) -> crate::tools::ToolOrigin {
            crate::tools::ToolOrigin::Builtin
        }
        async fn execute(&self, _params: Value, ctx: crate::tools::ToolContext) -> crate::Result<String> {
            self.0.lock().unwrap().push(ctx.capabilities.memory_backend.clone());
            Ok("seen".into())
        }
    }

    let _base = crate::test_env::BaseDirGuard::new();
    let mut rt = runtime(ContextManagementMode::Off, false);
    rt.set_model("plug:prov:model-x".to_string());
    assert_eq!(rt.model(), "plug:prov:model-x");
    let seen: Seen = Arc::default();
    rt.tools
        .write()
        .await
        .register(Arc::new(BindingProbe(Arc::clone(&seen))));
    let mut manager = crate::extensions::manager::ExtensionManager::new_with_tools(
        Arc::clone(&rt.hook_bus),
        Arc::clone(&rt.tools),
    );
    manager
        .register_provider_handler_for_tests(
            "plug",
            RegisteredProviderSpec {
                id: "prov".into(),
                display_name: "Probe".into(),
                description: "probe provider".into(),
                models: vec![RegisteredProviderModelSpec {
                    id: "model-x".into(),
                    display_name: None,
                    capabilities: json!({"streaming": false, "tool_use": true}),
                    context_window: None,
                }],
                config_schema: None,
            },
            Arc::new(ProbeThenText),
        )
        .unwrap();
    crate::runtime::openai::set_extension_manager_for_routing(Arc::new(
        tokio::sync::RwLock::new(manager),
    ));
    let out = turn(
        &rt,
        vec![Arc::new(json!({"role":"user","content":"SESSION-A: use the probe"}))],
    )
    .await;
    crate::runtime::openai::clear_extension_manager_for_routing();
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "the provider's tool call ran once");
    let binding = seen[0]
        .as_ref()
        .expect("the session's binding, not a process-config fallback");
    assert!(binding.same_binding_for_tests(&rt.memory_backend_for_test()));
}

// ── Per-session visibility on a shared registry ─────────────────────────────

/// Whether a request body offers `name` to the model in any form: a tools
/// entry (loaded or deferred) or a line of the activatable-tool catalog in
/// the system prompt. (The history is not searched: it may hold the model's
/// own call naming the tool.)
fn offers(body: &str, name: &str) -> bool {
    let parsed: Value = serde_json::from_str(body).expect("request body is JSON");
    let in_tools = parsed["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|t| t["name"] == name);
    let id = crate::tools::catalog::ToolId::builtin(name);
    in_tools || parsed["system"].to_string().contains(id.as_str())
}

/// An Axel binding whose service must never start in these tests.
fn axel_config(base: &crate::test_env::BaseDirGuard) -> agent_core::config::MemoryBackendConfig {
    agent_core::config::MemoryBackendConfig {
        kind: agent_core::config::MemoryBackendKind::Axel,
        executable: Some(base.path().join("must-not-launch")),
        brain: Some(base.path().join("must-not-create.r8")),
        user_scope: false,
    }
}

/// Whether an `activate_tools` result is its success receipt.
fn activated(result: &Value) -> bool {
    result["is_error"] != true && result["content"].to_string().contains("activated")
}

/// An off session's turn start neither removes nor disables
/// `context_checkpoint` for the host: an auto session's call made after the
/// off session's turn start still records into its own state. Only the auto
/// session is offered the tool.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(synaps_base_dir)]
async fn an_off_sessions_turn_start_leaves_an_auto_session_its_checkpoint_mid_turn() {
    for progressive in [false, true] {
        let _base = crate::test_env::BaseDirGuard::new();
        let (url, mock) = spawn().await;
        let _url = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
        let (a, b) = host_pair(
            ContextManagementMode::Auto,
            ContextManagementMode::Off,
            progressive,
        );
        mock.step(&[("SESSION-A", "plan", "NOTE-A-FIRST")], None);
        assert_recorded(&turn(&a, tagged("SESSION-A", 1)).await, "A1");

        // A's turn start, then B's whole turn start, then A's call.
        mock.step(
            &[("SESSION-A", "execute", "NOTE-A-MID")],
            Some(("SESSION-A", "SESSION-B")),
        );
        let (ta, tb) = tokio::join!(turn(&a, tagged("SESSION-A", 2)), async {
            mock.arrival("SESSION-A").await;
            turn(&b, tagged("SESSION-B", 1)).await
        });
        assert!(tb.errors.is_empty(), "off session: {:?}", tb.errors);
        assert_recorded(&ta, "A2");
        assert_eq!(note(&a), "NOTE-A-MID", "progressive={progressive}");
        assert_eq!(phase(&a), WorkPhase::Execute, "progressive={progressive}");
        assert!(note(&b).is_empty(), "off session state untouched");
        assert_eq!(phase(&b), WorkPhase::Unknown, "off session state untouched");
        assert!(
            a.tools.read().await.get("context_checkpoint").is_some(),
            "the host keeps the tool registered"
        );
        let a_bodies = mock.bodies_of("SESSION-A");
        assert!(!a_bodies.is_empty());
        for body in &a_bodies {
            assert!(offers(body, "context_checkpoint"), "auto session, progressive={progressive}");
        }
        let b_bodies = mock.bodies_of("SESSION-B");
        assert!(!b_bodies.is_empty());
        for body in &b_bodies {
            assert!(
                !offers(body, "context_checkpoint"),
                "off session must not be offered the tool, progressive={progressive}"
            );
        }
    }
}

/// A context-off session that calls `context_checkpoint` anyway (it is
/// registered for the host) is refused and records into no state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(synaps_base_dir)]
async fn an_off_sessions_call_to_the_withheld_checkpoint_is_refused() {
    for progressive in [false, true] {
        let _base = crate::test_env::BaseDirGuard::new();
        let (url, mock) = spawn().await;
        let _url = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
        let (a, b) = host_pair(
            ContextManagementMode::Auto,
            ContextManagementMode::Off,
            progressive,
        );
        mock.step(&[("SESSION-A", "plan", "NOTE-A-ONLY")], None);
        assert_recorded(&turn(&a, tagged("SESSION-A", 1)).await, "A1");
        mock.step(&[("SESSION-B", "wrap_up", "STRAY-B")], None);
        let tb = turn(&b, tagged("SESSION-B", 1)).await;
        assert!(tb.errors.is_empty(), "{:?}", tb.errors);
        let result = checkpoint_result(&tb.history);
        assert!(!recorded(&result), "progressive={progressive}: {result}");
        assert!(note(&b).is_empty());
        assert_eq!(phase(&b), WorkPhase::Unknown);
        assert_eq!(note(&a), "NOTE-A-ONLY");
        assert_eq!(phase(&a), WorkPhase::Plan);
    }
}

/// forum_* visibility follows each session's own backend: a legacy
/// session's turn start does not hide the forum from an Axel session of the
/// same host, and the legacy session is never offered it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(synaps_base_dir)]
async fn forum_tools_follow_each_sessions_own_backend() {
    for progressive in [false, true] {
        let base = crate::test_env::BaseDirGuard::new();
        let (url, mock) = spawn().await;
        let _url = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
        let (mut a, b) = host_pair(
            ContextManagementMode::Off,
            ContextManagementMode::Off,
            progressive,
        );
        a.apply_memory_backend_config(&axel_config(&base));
        assert!(a.memory_backend_for_test().is_axel(), "fixture: an Axel-bound session");
        assert!(!b.memory_backend_for_test().is_axel(), "fixture: a legacy session");
        let forum_read = crate::tools::catalog::ToolId::builtin("forum_read");
        let activate = json!({"tools": [forum_read.as_str()]});

        // The legacy session's turn start comes first.
        if progressive {
            mock.step_calls(&[("SESSION-B", "activate_tools", activate.clone())], None);
        } else {
            mock.step(&[], None);
        }
        let tb = turn(&b, tagged("SESSION-B", 1)).await;
        assert!(tb.errors.is_empty(), "{:?}", tb.errors);
        if progressive {
            let result = checkpoint_result(&tb.history);
            assert!(!activated(&result), "legacy activation refused: {result}");
        }
        if progressive {
            mock.step_calls(&[("SESSION-A", "activate_tools", activate.clone())], None);
        }
        let ta = turn(&a, tagged("SESSION-A", 1)).await;
        assert!(ta.errors.is_empty(), "{:?}", ta.errors);
        let a_bodies = mock.bodies_of("SESSION-A");
        if progressive {
            let result = checkpoint_result(&ta.history);
            assert!(activated(&result), "Axel activation: {result}");
            assert!(offers(a_bodies.last().unwrap(), "forum_read"));
        } else {
            for body in &a_bodies {
                for name in ["forum_post", "forum_read", "forum_forget"] {
                    assert!(offers(body, name), "Axel session offered {name}");
                }
            }
        }
        for body in &mock.bodies_of("SESSION-B") {
            for name in ["forum_post", "forum_read", "forum_forget"] {
                assert!(!offers(body, name), "legacy session offered {name}, progressive={progressive}");
            }
        }
        assert!(!base.path().join("must-not-create.r8").exists());
    }
}

/// Held schedule: the Axel session's turn waits on inference while the
/// legacy session's turn starts; the Axel session's forum call made after
/// that still resolves to the forum tool (activation with progressive
/// disclosure, a direct call without it).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(synaps_base_dir)]
async fn a_legacy_turn_start_leaves_an_axel_session_its_forum_mid_turn() {
    for progressive in [false, true] {
        let base = crate::test_env::BaseDirGuard::new();
        let (url, mock) = spawn().await;
        let _url = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
        let (mut a, b) = host_pair(
            ContextManagementMode::Off,
            ContextManagementMode::Off,
            progressive,
        );
        a.apply_memory_backend_config(&axel_config(&base));
        assert!(a.memory_backend_for_test().is_axel(), "fixture: an Axel-bound session");
        let forum_read = crate::tools::catalog::ToolId::builtin("forum_read");
        let (name, input) = if progressive {
            ("activate_tools", json!({"tools": [forum_read.as_str()]}))
        } else {
            ("forum_read", json!({}))
        };
        // A's turn start, then B's whole turn start, then A's call.
        mock.step_calls(&[("SESSION-A", name, input)], Some(("SESSION-A", "SESSION-B")));
        let (ta, tb) = tokio::join!(turn(&a, tagged("SESSION-A", 1)), async {
            mock.arrival("SESSION-A").await;
            turn(&b, tagged("SESSION-B", 1)).await
        });
        assert!(tb.errors.is_empty(), "legacy session: {:?}", tb.errors);
        assert!(ta.errors.is_empty(), "Axel session: {:?}", ta.errors);
        let result = checkpoint_result(&ta.history);
        if progressive {
            assert!(activated(&result), "Axel activation after the legacy turn start: {result}");
        } else {
            let text = result["content"].to_string();
            assert!(
                !text.contains("Unknown tool") && !text.contains("Tool call denied"),
                "the forum tool itself must answer: {result}"
            );
        }
        assert!(a.tools.read().await.get("forum_read").is_some(), "the host keeps the forum");
        assert!(!base.path().join("must-not-create.r8").exists());
    }
}

/// Alternating context-off and auto turns on one host leave the shared
/// catalog generation alone: an off session's turn start no longer removes
/// the tool (and the auto session no longer re-adds it).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(synaps_base_dir)]
async fn alternating_off_and_auto_turns_leave_the_catalog_generation_unchanged() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (url, mock) = spawn().await;
    let _url = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let (a, b) = host_pair(ContextManagementMode::Auto, ContextManagementMode::Off, false);
    mock.step(&[("SESSION-A", "plan", "NOTE-A")], None);
    assert_recorded(&turn(&a, tagged("SESSION-A", 1)).await, "A1");
    let generation = a.tools.read().await.catalog().generation();
    for step in 2..5 {
        let tb = turn(&b, tagged("SESSION-B", step)).await;
        assert!(tb.errors.is_empty(), "{:?}", tb.errors);
        assert_recorded(&turn(&a, tagged("SESSION-A", step)).await, "A");
        assert_eq!(
            a.tools.read().await.catalog().generation(),
            generation,
            "turn {step} advanced the shared catalog generation"
        );
    }
}

// ── Dispatch lanes and registration ─────────────────────────────────────────

/// The tool result for call `id` in a turn's published history.
fn result_of(history: &[SharedMessage], id: &str) -> Value {
    history
        .iter()
        .filter_map(|m| m["content"].as_array())
        .flatten()
        .find(|b| b["type"] == "tool_result" && b["tool_use_id"] == id)
        .cloned()
        .unwrap_or_else(|| panic!("tool result {id} in history"))
}

/// Two sessions' batches at once, through the parallel dispatch lane: the
/// auto session's batch holding `context_checkpoint` is refused whole (the
/// host assesses rollover before more work), the off session's harmless
/// call runs while its withheld checkpoint is refused, and no batch records
/// into any state. (A context-managed session never executes the
/// checkpoint in this lane; its binding there is covered by inspection.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(synaps_base_dir)]
async fn checkpoint_batches_on_a_shared_registry_record_into_no_state() {
    for progressive in [false, true] {
        let _base = crate::test_env::BaseDirGuard::new();
        let (url, mock) = spawn().await;
        let _url = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
        let (a, b) = host_pair(
            ContextManagementMode::Auto,
            ContextManagementMode::Off,
            progressive,
        );
        mock.step(&[("SESSION-A", "plan", "NOTE-A-ONLY")], None);
        assert_recorded(&turn(&a, tagged("SESSION-A", 1)).await, "A1");
        let harmless = ("ls", json!({"path": "."}));
        mock.step_calls(
            &[
                ("SESSION-A", "context_checkpoint", json!({"phase":"wrap_up","note":"STRAY-A"})),
                ("SESSION-A", harmless.0, harmless.1.clone()),
                ("SESSION-B", "context_checkpoint", json!({"phase":"wrap_up","note":"STRAY-B"})),
                ("SESSION-B", harmless.0, harmless.1.clone()),
            ],
            None,
        );
        let (ta, tb) = tokio::join!(
            turn(&a, tagged("SESSION-A", 2)),
            turn(&b, tagged("SESSION-B", 1))
        );
        assert!(ta.errors.is_empty(), "{:?}", ta.errors);
        assert!(tb.errors.is_empty(), "{:?}", tb.errors);
        for id in ["toolu_cp", "toolu_2"] {
            let refused = result_of(&ta.history, id);
            assert_eq!(refused["is_error"], true, "auto batch refused whole: {refused}");
            assert!(refused["content"].to_string().contains("must be called alone"), "{refused}");
        }
        assert!(!recorded(&result_of(&tb.history, "toolu_cp")), "off checkpoint refused");
        assert_ne!(result_of(&tb.history, "toolu_2")["is_error"], true, "harmless call ran");
        assert_eq!(note(&a), "NOTE-A-ONLY", "progressive={progressive}");
        assert_eq!(phase(&a), WorkPhase::Plan, "progressive={progressive}");
        assert!(note(&b).is_empty(), "progressive={progressive}");
        assert_eq!(phase(&b), WorkPhase::Unknown, "progressive={progressive}");
    }
}

/// The non-streaming tool loop (`run_single`) on a registry another session
/// registered the tool into records the checkpoint into its own state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(synaps_base_dir)]
async fn run_single_checkpoints_record_into_the_calling_sessions_own_state() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (url, mock) = spawn().await;
    let _url = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let auto = ContextManagementMode::Auto;
    let (a, b) = host_pair(auto, auto, false);
    // A's stream turn registers the tool; run_single never registers it.
    mock.step(&[("SESSION-A", "plan", "NOTE-A")], None);
    assert_recorded(&turn(&a, tagged("SESSION-A", 1)).await, "A1");
    mock.step(&[("SESSION-B", "execute", "NOTE-B-SINGLE")], None);
    let out = b.run_single("SESSION-B: one shot").await.expect("run_single");
    assert_eq!(out, "ok");
    assert_eq!(note(&b), "NOTE-B-SINGLE");
    assert_eq!(phase(&b), WorkPhase::Execute);
    assert_eq!(note(&a), "NOTE-A");
    assert_eq!(phase(&a), WorkPhase::Plan);
    let sent = mock.bodies_of("SESSION-B");
    let result = sent.last().expect("follow-up request");
    assert!(result.contains("phase=execute recorded"), "{result}");
}

/// Turn-start registration: the builtin is registered when absent, replaces
/// a non-builtin tool shadowing its name (whose handler then never runs),
/// and an already registered builtin is left alone (same instance, same
/// catalog generation).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(synaps_base_dir)]
async fn turn_start_registers_the_builtin_checkpoint_only_when_needed() {
    struct Shadow(Arc<std::sync::atomic::AtomicUsize>);
    #[async_trait::async_trait]
    impl crate::tools::Tool for Shadow {
        fn name(&self) -> &str {
            "context_checkpoint"
        }
        fn description(&self) -> &str {
            "a non-builtin tool shadowing the checkpoint name"
        }
        fn parameters(&self) -> Value {
            json!({"type":"object"})
        }
        fn origin(&self) -> crate::tools::ToolOrigin {
            crate::tools::ToolOrigin::Extension {
                extension_id: "shadow".into(),
            }
        }
        async fn execute(&self, _params: Value, _ctx: crate::tools::ToolContext) -> crate::Result<String> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok("phase=plan recorded by the shadow".into())
        }
    }

    let _base = crate::test_env::BaseDirGuard::new();
    let (url, mock) = spawn().await;
    let _url = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let (a, _b) = host_pair(ContextManagementMode::Auto, ContextManagementMode::Auto, false);
    let builtin = |rt: &crate::Runtime| {
        let registry = rt.tools.try_read().expect("registry idle");
        registry
            .get("context_checkpoint")
            .map(|tool| (Arc::clone(tool), registry.catalog().generation()))
    };
    assert!(builtin(&a).is_none(), "absent at construction");

    mock.step(&[("SESSION-A", "plan", "NOTE-1")], None);
    assert_recorded(&turn(&a, tagged("SESSION-A", 1)).await, "absent");
    let (first, generation) = builtin(&a).expect("registered when absent");
    assert_eq!(first.origin(), crate::tools::ToolOrigin::Builtin);
    assert_eq!(note(&a), "NOTE-1");

    mock.step(&[("SESSION-A", "execute", "NOTE-2")], None);
    assert_recorded(&turn(&a, tagged("SESSION-A", 2)).await, "present");
    let (again, unchanged) = builtin(&a).unwrap();
    assert!(Arc::ptr_eq(&first, &again), "an already registered builtin is left alone");
    assert_eq!(unchanged, generation);
    assert_eq!(note(&a), "NOTE-2");

    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    a.tools
        .write()
        .await
        .register(Arc::new(Shadow(Arc::clone(&calls))));
    mock.step(&[("SESSION-A", "plan", "NOTE-3")], None);
    assert_recorded(&turn(&a, tagged("SESSION-A", 3)).await, "shadowed");
    let (replaced, _) = builtin(&a).unwrap();
    assert_eq!(replaced.origin(), crate::tools::ToolOrigin::Builtin, "shadow replaced");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0, "the shadow never ran");
    assert_eq!(note(&a), "NOTE-3");
    assert_eq!(phase(&a), WorkPhase::Plan);
}

/// Records, per call, whether its `ToolContext` carried no worker context
/// policy.
struct WorkerContextProbe(Arc<Mutex<Vec<bool>>>);

#[async_trait::async_trait]
impl crate::tools::Tool for WorkerContextProbe {
    fn name(&self) -> &str {
        "worker_context_probe"
    }
    fn description(&self) -> &str {
        "records whether a worker context policy reached the call"
    }
    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    fn origin(&self) -> crate::tools::ToolOrigin {
        crate::tools::ToolOrigin::Builtin
    }
    fn effect(&self) -> crate::tools::catalog::ToolEffect {
        crate::tools::catalog::ToolEffect::ReadOnly
    }
    async fn execute(&self, _params: Value, ctx: crate::ToolContext) -> crate::Result<String> {
        self.0
            .lock()
            .unwrap()
            .push(ctx.capabilities.worker_context.is_none());
        Ok("probed".to_string())
    }
}

/// Slice 9: with worker rollover built on, a runtime's turn snapshots its
/// live worker context policy for its tool calls: a tool's context carries
/// one (strict r1 B-SF kept the dark path from taking any).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(synaps_base_dir)]
async fn a_turn_hands_its_tools_the_worker_context() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (url, mock) = spawn().await;
    let _url = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let rt = runtime(ContextManagementMode::Auto, false);
    let seen = Arc::new(Mutex::new(Vec::new()));
    rt.tools
        .write()
        .await
        .register(Arc::new(WorkerContextProbe(seen.clone())));
    mock.step_calls(&[("SESSION-A", "worker_context_probe", json!({}))], None);
    let out = turn(&rt, tagged("SESSION-A", 1)).await;
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert_eq!(*seen.lock().unwrap(), vec![false], "one call, with a policy");
}
