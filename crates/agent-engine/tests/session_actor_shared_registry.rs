//! One host serves many chats (the daemon) and they share one tool registry.
//! What a tool does for a chat, and which tools a chat is offered, belong to
//! that chat: `context_checkpoint` writes into the calling chat's rollover
//! state, and a chat that may not use a tool (no context management, no
//! project forum) does not take it away from one that may.
//!
//! Drives the production actor → runtime → Anthropic transport against a
//! loopback stub, like the other `session_actor_*` tests.

mod session_actor_common;
use session_actor_common::*;

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_engine::session::{
    ClientKind, ClientMeta, ClientTransport, LocalTransport, SessionConfig, SessionEventWire,
};
use agent_engine::{EngineHost, Runtime};
use axum::response::IntoResponse;
use serde_json::{json, Value};
use serial_test::serial;
use tokio::sync::{mpsc, Semaphore};

/// What a chat's model does with a user message.
#[derive(Clone, Copy)]
enum Reply {
    /// Calls `context_checkpoint` with this phase and note, then answers.
    Checkpoint(&'static str, &'static str),
    /// Answers.
    Text,
}

/// One request as the provider received it.
struct Request {
    chat: &'static str,
    /// The names in its `tools` array.
    tools: Vec<String>,
    system: String,
    /// The `tool_result`s its last message carries.
    results: Vec<Value>,
}

/// The provider behind every chat of a test. A chat is known by the word its
/// user says; its answer to a user message waits for [`Provider::release`].
struct Provider {
    chats: HashMap<&'static str, (Reply, Semaphore)>,
    arrived: mpsc::UnboundedSender<&'static str>,
    requests: Mutex<Vec<Request>>,
}

impl Provider {
    /// Let `chat`'s model answer one user message.
    fn release(&self, chat: &str) {
        self.chats[chat].1.add_permits(1);
    }
    /// Whether any request of `chat` offered `tool`: in its tools array, or
    /// in the list of tools it may activate (progressive disclosure).
    fn offered(&self, chat: &str, tool: &str) -> bool {
        let listed = format!("builtin:{tool}");
        self.requests.lock().unwrap().iter().any(|r| {
            r.chat == chat && (r.tools.iter().any(|t| t == tool) || r.system.contains(&listed))
        })
    }
    fn requests_of(&self, chat: &str) -> usize {
        let requests = self.requests.lock().unwrap();
        requests.iter().filter(|r| r.chat == chat).count()
    }
    /// The tool results `chat` sent back, as JSON text.
    fn results_of(&self, chat: &str) -> String {
        let requests = self.requests.lock().unwrap();
        let results: Vec<&Value> = requests
            .iter()
            .filter(|r| r.chat == chat)
            .flat_map(|r| &r.results)
            .collect();
        json!(results).to_string()
    }
}

fn sse(body: String) -> axum::response::Response {
    (
        axum::http::StatusCode::OK,
        [("content-type", "text/event-stream")],
        body,
    )
        .into_response()
}

/// One assistant round that calls `context_checkpoint`.
fn checkpoint_sse(phase: &str, note: &str) -> String {
    let usage = json!({"input_tokens": 10, "output_tokens": 5,
        "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0});
    [
        json!({"type": "message_start", "message": {"id": "msg_cp", "type": "message",
            "role": "assistant", "content": [], "model": MODEL, "stop_reason": null,
            "stop_sequence": null, "usage": usage}}),
        json!({"type": "content_block_start", "index": 0, "content_block":
            {"type": "tool_use", "id": "toolu_cp", "name": "context_checkpoint"}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta",
            "partial_json": json!({"phase": phase, "note": note}).to_string()}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "tool_use",
            "stop_sequence": null}, "usage": usage}),
        json!({"type": "message_stop"}),
    ]
    .iter()
    .map(|event| format!("data: {event}\n\n"))
    .collect()
}

async fn answer(provider: Arc<Provider>, body: String) -> axum::response::Response {
    let request: Value = serde_json::from_str(&body).unwrap_or_default();
    let messages = request["messages"].to_string();
    let Some((&chat, (reply, gate))) = provider
        .chats
        .iter()
        .find(|(word, _)| messages.contains(**word))
    else {
        return sse(SSE_HI.to_string());
    };
    let results: Vec<Value> = request["messages"]
        .as_array()
        .and_then(|all| all.last())
        .and_then(|last| last["content"].as_array())
        .map(|blocks| {
            let results = blocks.iter().filter(|b| b["type"] == "tool_result");
            results.cloned().collect()
        })
        .unwrap_or_default();
    let answered = !results.is_empty();
    provider.requests.lock().unwrap().push(Request {
        chat,
        tools: request["tools"]
            .as_array()
            .map(|tools| {
                let names = tools.iter().filter_map(|t| t["name"].as_str());
                names.map(str::to_string).collect()
            })
            .unwrap_or_default(),
        system: request["system"].to_string(),
        results,
    });
    if answered {
        return sse(SSE_HI.to_string());
    }
    // A user message: the turn has started. Its answer waits its turn.
    let _ = provider.arrived.send(chat);
    gate.acquire().await.unwrap().forget();
    match reply {
        Reply::Checkpoint(phase, note) => sse(checkpoint_sse(phase, note)),
        Reply::Text => sse(SSE_HI.to_string()),
    }
}

/// A temp HOME with `config`, the provider, and a host as the daemon boots
/// one. The receiver yields a chat's word each time its turn reaches the
/// provider with a user message.
async fn setup(
    config: &str,
    chats: &[(&'static str, Reply)],
) -> (Home, Arc<EngineHost>, Arc<Provider>, mpsc::UnboundedReceiver<&'static str>) {
    let home = Home::new();
    std::env::remove_var("SYNAPS_PROJECT_ROOT");
    std::fs::write(home.dir.path().join(".synaps-cli").join("config"), config).unwrap();
    let (arrived, arrivals) = mpsc::unbounded_channel();
    let provider = Arc::new(Provider {
        chats: chats
            .iter()
            .map(|&(word, reply)| (word, (reply, Semaphore::new(0))))
            .collect(),
        arrived,
        requests: Mutex::default(),
    });
    let stub = Arc::clone(&provider);
    let app = axum::Router::new().fallback(move |body: String| answer(Arc::clone(&stub), body));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", format!("http://{addr}"));
    (home, host().await, provider, arrivals)
}

/// A live chat on the host, and its runtime (what its turns run on).
struct Chat {
    runtime: Runtime,
    t: LocalTransport,
}

impl Chat {
    /// A chat in `dir`; `context` turns its context management on.
    async fn start(host: &Arc<EngineHost>, dir: &Path, context: bool) -> Self {
        let config = SessionConfig {
            cwd: Some(dir.to_path_buf()),
            ..cfg()
        };
        let (handle, task) = host.create_session_unspawned(config).await.unwrap();
        let runtime = task.runtime_for_harness();
        if context {
            runtime.context_management_command("auto").unwrap();
        }
        tokio::spawn(task.run());
        let t = LocalTransport::attach(handle, ClientMeta::new(ClientKind::Tui))
            .await
            .unwrap()
            .0;
        Self { runtime, t }
    }
    async fn say(&mut self, text: &str) {
        self.t.send(submit(text)).await.unwrap();
    }
    async fn idle(&mut self) {
        until(&mut self.t, |e| matches!(e, SessionEventWire::Idle)).await;
    }
    /// The phase and note this chat's rollover state holds.
    fn checkpoint(&self) -> (&'static str, String) {
        self.runtime.context_checkpoint_for_harness()
    }
}

/// The next turn to reach the provider is `chat`'s.
async fn started(arrivals: &mut mpsc::UnboundedReceiver<&'static str>, chat: &str) {
    let next = tokio::time::timeout(Duration::from_secs(30), arrivals.recv()).await;
    assert_eq!(next.expect("a turn reaches the provider"), Some(chat));
}

/// A rollover state holding this phase and note.
fn holds(phase: &'static str, note: &str) -> (&'static str, String) {
    (phase, note.to_string())
}

/// A rollover state no checkpoint has reached.
fn untouched() -> (&'static str, String) {
    holds("unknown", "")
}

// Context management archives on Unix only (`/context auto` is refused
// elsewhere), so there is no second chat with it on to test against.
/// Two chats with context management, both mid-turn at once. Each reports
/// its own phase and note: each chat's rollover state holds what its own
/// model reported, and nothing of the other's.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn each_chats_checkpoint_goes_into_its_own_rollover_state() {
    let (_h, host, provider, mut arrivals) = setup(
        "",
        &[
            ("alpha", Reply::Checkpoint("plan", "alpha: half of the parser is written")),
            ("beta", Reply::Checkpoint("wrap_up", "beta: two tests left to fix")),
        ],
    )
    .await;
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = Chat::start(&host, dir_a.path(), true).await;
    let mut b = Chat::start(&host, dir_b.path(), true).await;

    // Both turns start before either model answers: alpha's, then beta's.
    a.say("alpha, carry on").await;
    started(&mut arrivals, "alpha").await;
    b.say("beta, carry on").await;
    started(&mut arrivals, "beta").await;

    // alpha's model reports first.
    provider.release("alpha");
    a.idle().await;
    assert_eq!(
        (a.checkpoint(), b.checkpoint()),
        (holds("plan", "alpha: half of the parser is written"), untouched()),
        "(alpha's state, beta's state) after alpha alone reported; alpha was answered: {}",
        provider.results_of("alpha")
    );

    provider.release("beta");
    b.idle().await;
    assert_eq!(
        (a.checkpoint(), b.checkpoint()),
        (
            holds("plan", "alpha: half of the parser is written"),
            holds("wrap_up", "beta: two tests left to fix")
        ),
        "(alpha's state, beta's state) after both reported; beta was answered: {}",
        provider.results_of("beta")
    );
    end(&mut a.t).await;
    end(&mut b.t).await;
}

/// One chat with context management and one without, on one host. The one
/// without takes a whole turn while the other is mid-turn: the first is
/// still offered `context_checkpoint` and its report still lands in its own
/// state; the second is never offered the tool, and if its model calls it
/// anyway it is refused and nothing is written anywhere.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_chat_without_context_management_takes_nothing_from_one_that_has_it() {
    let (_h, host, provider, mut arrivals) = setup(
        "",
        &[
            ("alpha", Reply::Checkpoint("plan", "alpha: half of the parser is written")),
            ("beta", Reply::Checkpoint("wrap_up", "beta: must go nowhere")),
        ],
    )
    .await;
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = Chat::start(&host, dir_a.path(), true).await;
    let mut b = Chat::start(&host, dir_b.path(), false).await;

    a.say("alpha, carry on").await;
    started(&mut arrivals, "alpha").await;
    // beta's whole turn runs while alpha's model is still thinking. Its
    // model calls a tool it was not offered.
    b.say("beta, carry on").await;
    started(&mut arrivals, "beta").await;
    provider.release("beta");
    b.idle().await;
    let refusal = provider.results_of("beta");
    assert!(
        refusal.contains("toolu_cp") && !refusal.contains("recorded"),
        "beta's call is refused: {refusal}"
    );
    assert_eq!((a.checkpoint(), b.checkpoint()), (untouched(), untouched()));

    provider.release("alpha");
    a.idle().await;
    assert_eq!(
        (a.checkpoint(), b.checkpoint()),
        (holds("plan", "alpha: half of the parser is written"), untouched()),
        "(alpha's state, beta's state); alpha was answered: {}",
        provider.results_of("alpha")
    );
    assert!(provider.offered("alpha", "context_checkpoint"));
    assert!(!provider.offered("beta", "context_checkpoint"));
    // Tools both chats may use are offered to both.
    for chat in ["alpha", "beta"] {
        assert!(provider.offered(chat, "read"), "{chat}");
    }
    end(&mut a.t).await;
    end(&mut b.t).await;
}

/// The project forum belongs to the Axel memory backend, and a chat whose
/// folder's project cannot be resolved has no forum. Such a chat takes a
/// turn first; a chat on the same host that has a forum is then still
/// offered the forum tools, and the first never is.
async fn the_forum_stays_with_the_chat_that_has_one(progressive: bool) {
    // Never started: nothing in these turns calls the memory service.
    let service = tempfile::tempdir().unwrap();
    let config = format!(
        "memory.backend = axel\nmemory.axel.executable = {}\nmemory.axel.brain = {}\n\
         progressive_tool_disclosure = {progressive}\n",
        service.path().join("no-such-service").display(),
        service.path().join("brain.r8").display(),
    );
    let (_h, host, provider, mut arrivals) =
        setup(&config, &[("alpha", Reply::Text), ("beta", Reply::Text)]).await;
    // alpha's folder is a project of its own. beta's has a `.git` that is no
    // repository, so its project is refused rather than guessed.
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    std::fs::write(dir_a.path().join(".synaps-project"), "").unwrap();
    std::fs::write(dir_b.path().join(".git"), "not a repository\n").unwrap();
    let mut a = Chat::start(&host, dir_a.path(), false).await;
    let mut b = Chat::start(&host, dir_b.path(), false).await;
    assert!(a.runtime.memory_backend_is_axel(), "alpha has a forum");
    assert!(!b.runtime.memory_backend_is_axel(), "beta has none");

    b.say("beta, hello").await;
    started(&mut arrivals, "beta").await;
    provider.release("beta");
    b.idle().await;
    a.say("alpha, hello").await;
    started(&mut arrivals, "alpha").await;
    provider.release("alpha");
    a.idle().await;

    assert!(provider.requests_of("alpha") > 0 && provider.requests_of("beta") > 0);
    for tool in ["forum_post", "forum_read", "forum_forget"] {
        assert!(provider.offered("alpha", tool), "alpha is offered {tool}");
        assert!(!provider.offered("beta", tool), "beta is not offered {tool}");
    }
    for chat in ["alpha", "beta"] {
        assert!(provider.offered(chat, "read"), "{chat}");
    }
    end(&mut a.t).await;
    end(&mut b.t).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_chat_with_no_forum_does_not_take_the_forum_tools_from_one_that_has_it() {
    the_forum_stays_with_the_chat_that_has_one(false).await;
}

/// The same under progressive disclosure, where the forum tools are not in
/// the tools array but in the list of tools the chat may activate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_chat_with_no_forum_does_not_take_the_forum_tools_from_one_that_has_it_progressive() {
    the_forum_stays_with_the_chat_that_has_one(true).await;
}
