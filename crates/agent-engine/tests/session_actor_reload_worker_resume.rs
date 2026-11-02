//! After a daemon reload the new image resumes each worker the reload
//! stopped itself, from its archive, and tells the parent once that it did
//! ("resumed by the daemon as <new>"). A worker it may not resume keeps
//! today's wake ("subagent_resume … continues it"), with the reason. The
//! session actor gets the carried workers as the reload's rehydrate sends
//! them (`SessionCommand::AdoptWorkers`).
//!
//! In-process actor, scripted loopback Anthropic SSE stub, isolated HOME.
//! Old handle ids are far apart per test: the id counter is per process.

mod session_actor_common;
use session_actor_common::*;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_engine::runtime::subagent::{CarriedWorker, SubagentArchiveMeta, SubagentRegistry};
use agent_engine::session::wire::CHECKPOINT_QUERY_ID;
use agent_engine::session::{
    CheckpointReason, ClientKind, ClientMeta, ClientTransport, Envelope, LocalTransport,
    SessionCommand, SessionEventWire, WorkerResume,
};
use axum::response::IntoResponse;
use futures::StreamExt;
use serde_json::{json, Value};
use serial_test::serial;

/// In every worker's system prompt (the archives' meta).
const WORKER_MARK: &str = "RELOAD-RESUME-WORKER";
/// The daemon's resume turn, word for word.
const DAEMON_WORDS: &str = "[The daemon reloaded and stopped you mid-run; this resume was started \
     by the daemon. Check your last tool call before trusting it, then continue.]";
/// A parent prompt the stub answers with a `subagent_resume` of sa_4921.
const RESUME_BY_HAND: &str = "resume sa_4921 by hand";
/// A parent prompt whose reply never ends (a reload cuts it).
const HANG_TURN: &str = "a turn that never finishes";
/// In the message that resumes the turn a reload cut (`resume_text`).
const CUT_TURN_RESUME: &str = "the daemon reloaded mid-turn";

type Bodies = Arc<Mutex<Vec<Value>>>;

fn is_worker(body: &Value) -> bool {
    body["system"].to_string().contains(WORKER_MARK)
}

/// Every string in `v`, joined (a message's text, unescaped).
fn texts(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(items) => items.iter().map(texts).collect::<Vec<_>>().join("\n"),
        Value::Object(map) => map.values().map(texts).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

fn last_message(body: &Value) -> Value {
    body["messages"].as_array().and_then(|m| m.last()).cloned().unwrap_or(Value::Null)
}

/// The text of a request's last message's last block.
fn last_text(body: &Value) -> String {
    let last = last_message(body);
    match &last["content"] {
        Value::Array(blocks) => blocks.last().and_then(|b| b["text"].as_str()).unwrap_or("").to_string(),
        Value::String(s) => s.clone(),
        _ => String::new(),
    }
}

fn sse_tool(id: &str, name: &str, input: Value) -> String {
    let start = json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":id,"name":name,"input":{}}});
    let delta = json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":input.to_string()}});
    let stop = json!({"type":"content_block_stop","index":0});
    format!(
        concat!(
            "data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_rr\",\"type\":\"message\",",
            "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
            "\"stop_sequence\":null,\"usage\":{{\"input_tokens\":10,\"output_tokens\":0,",
            "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}}}}\n\n",
            "data: {start}\n\ndata: {delta}\n\ndata: {stop}\n\n",
            "data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"tool_use\",",
            "\"stop_sequence\":null}},\"usage\":{{\"input_tokens\":10,\"output_tokens\":5,",
            "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}}\n\n",
            "data: {{\"type\":\"message_stop\"}}\n\n",
        ),
        start = start,
        delta = delta,
        stop = stop,
    )
}

fn endless() -> axum::response::Response {
    let stream = futures::stream::once(async {
        Ok::<_, std::io::Error>(axum::body::Bytes::from(SSE_PREFIX))
    })
    .chain(futures::stream::unfold((), |()| async {
        tokio::time::sleep(Duration::from_millis(200)).await;
        Some((Ok(axum::body::Bytes::from(": keep-alive\n\n")), ()))
    }));
    axum::response::Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .body(axum::body::Body::from_stream(stream))
        .unwrap()
}

/// Every request recorded. Workers answer `SSE_HI`, or never finish while
/// `hold` is set; the parent answers a `subagent_resume` call for
/// [`RESUME_BY_HAND`], never finishes [`HANG_TURN`], answers the resume of
/// a cut turn after 2 s (as a model takes a while), else `SSE_HI`.
async fn scripted_stub() -> (String, Bodies, Arc<AtomicBool>) {
    let seen: Bodies = Arc::default();
    let hold = Arc::new(AtomicBool::new(false));
    let (seen_c, hold_c) = (Arc::clone(&seen), Arc::clone(&hold));
    let app = axum::Router::new().fallback(move |body: axum::body::Bytes| {
        let (seen, hold) = (Arc::clone(&seen_c), Arc::clone(&hold_c));
        async move {
            let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            seen.lock().unwrap().push(v.clone());
            let sse = |body: String| {
                (axum::http::StatusCode::OK, [("content-type", "text/event-stream")], body)
                    .into_response()
            };
            if is_worker(&v) {
                if hold.load(Ordering::SeqCst) {
                    return endless();
                }
                return sse(SSE_HI.to_string());
            }
            let last = texts(&last_message(&v));
            if last.contains(RESUME_BY_HAND) {
                return sse(sse_tool(
                    "toolu_resume",
                    "subagent_resume",
                    json!({"handle_id": "sa_4921", "instructions": "go on"}),
                ));
            }
            if last.contains(HANG_TURN) {
                return endless();
            }
            if last.contains(CUT_TURN_RESUME) {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            sse(SSE_HI.to_string())
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), seen, hold)
}

struct Fixture {
    handle: agent_engine::session::SessionHandle,
    client: LocalTransport,
    registry: Arc<Mutex<SubagentRegistry>>,
    bodies: Bodies,
    hold: Arc<AtomicBool>,
}

/// A session with one finished turn in its history.
async fn fixture() -> Fixture {
    let (url, bodies, hold) = scripted_stub().await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let (handle, task) = host.create_session_unspawned(cfg()).await.unwrap();
    let registry = task.subagent_registry();
    tokio::spawn(task.run());
    let (mut client, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Test))
        .await
        .unwrap();
    client.send(submit("hello")).await.unwrap();
    until(&mut client, |e| matches!(e, SessionEventWire::Idle)).await;
    Fixture { handle, client, registry, bodies, hold }
}

/// Worker `id`'s v1 archive, as its finalizer writes it at a reload's stop:
/// its task, then its first answer. `filler` pads the task (size). The
/// model is qualified, as a launch records it.
fn archive(id: &str, filler: usize) -> std::path::PathBuf {
    let meta = SubagentArchiveMeta {
        agent_name: "digger".into(),
        model: format!("anthropic/{MODEL}"),
        system_prompt: WORKER_MARK.into(),
        timeout_secs: 600,
        thinking: None,
        watch_usage: None,
        cwd: None,
    };
    let history = [
        json!({"role": "user", "content": format!("dig the tunnel {}", "x".repeat(filler))}),
        json!({"role": "assistant", "content": [{"type": "text", "text": "digging"}]}),
    ];
    agent_engine::tools::write_test_archive(id, &meta, "cancelled", &history).unwrap()
}

/// As the reload record carries a worker it stopped.
fn stopped(id: &str, archive_path: Option<std::path::PathBuf>) -> CarriedWorker {
    CarriedWorker {
        id: id.into(),
        subagent_id: id.trim_start_matches("sa_").parse().unwrap(),
        agent: "digger".into(),
        status: "cancelled".into(),
        archive_path,
        stopped_by_reload: true,
        wake: true,
        elapsed_secs: 42,
        preview: "digging".into(),
        ..Default::default()
    }
}

async fn adopt(f: &Fixture, workers: Vec<CarriedWorker>, resume: WorkerResume) {
    f.handle.send(SessionCommand::AdoptWorkers { workers, resume }).await.unwrap();
}

fn started(seen: &[Envelope]) -> usize {
    seen.iter().filter(|e| matches!(e.event, SessionEventWire::TurnStarted { .. })).count()
}

fn notices(seen: &[Envelope]) -> Vec<String> {
    seen.iter()
        .filter_map(|e| match &e.event {
            SessionEventWire::SystemNotice(n) => Some(n.clone()),
            _ => None,
        })
        .collect()
}

/// One turn, start to idle.
async fn one_turn(f: &mut Fixture) -> Vec<Envelope> {
    let mut seen = until(&mut f.client, |e| matches!(e, SessionEventWire::TurnStarted { .. })).await;
    seen.extend(until(&mut f.client, |e| matches!(e, SessionEventWire::Idle)).await);
    seen
}

fn worker_bodies(bodies: &Bodies) -> Vec<Value> {
    bodies.lock().unwrap().iter().filter(|b| is_worker(b)).cloned().collect()
}

/// The parent's wakes naming worker `id`, in its latest request (which
/// repeats the history, so each wake once).
fn parent_wakes(bodies: &Bodies, id: &str) -> Vec<String> {
    let bodies = bodies.lock().unwrap();
    let Some(last) = bodies.iter().rfind(|b| !is_worker(b)) else {
        return Vec::new();
    };
    let text: String = last["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "user")
        .map(|m| texts(&m["content"]))
        .collect::<Vec<_>>()
        .join("\n");
    text.split("Subagent '")
        .skip(1)
        .filter(|wake| wake.contains(&format!("({id})")))
        .map(str::to_string)
        .collect()
}

/// Waits (up to 15 s) for `probe`, keeping the client drained.
async fn poll<T>(what: &str, f: &mut Fixture, mut probe: impl FnMut(&Fixture) -> Option<T>) -> T {
    for _ in 0..60 {
        if let Some(v) = probe(f) {
            return v;
        }
        drain_for(&mut f.client, Duration::from_millis(250)).await;
    }
    panic!("timed out waiting for {what}");
}

fn tool_names(body: &Value) -> Vec<String> {
    let mut names: Vec<String> = body["tools"]
        .as_array()
        .map(|t| t.iter().filter_map(|t| t["name"].as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    names.sort();
    names
}

fn new_of(f: &Fixture, old: &str) -> Option<String> {
    f.registry.lock().unwrap().resumed_as(old).flatten()
}

/// The reload stopped two workers; the new image resumes both itself. Each
/// continues its archived conversation with the daemon's words; the parent
/// gets ONE turn for the batch, naming old and new handles; a repeated
/// `AdoptWorkers` (the same record restored twice) launches and wakes
/// nothing again. Mutants: `plan_daemon_resumes` leaves every worker (no
/// "resumed as"); `adopted_already` forgets `daemon_resumes` (a second
/// launch and turn); `prior_run_as` ignores `by_daemon` (the orchestrator's
/// note in place of the daemon's words).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn the_daemon_resumes_reload_stopped_workers_and_wakes_the_parent_once() {
    let _h = Home::new();
    let mut f = fixture().await;
    f.hold.store(true, Ordering::SeqCst);
    let workers = vec![
        stopped("sa_900", Some(archive("sa_900", 0))),
        stopped("sa_901", Some(archive("sa_901", 0))),
    ];
    adopt(&f, workers.clone(), WorkerResume::Auto).await;
    let seen = one_turn(&mut f).await;
    assert_eq!(started(&seen), 1, "{seen:#?}");

    let new0 = new_of(&f, "sa_900").expect("sa_900 resumed by the daemon");
    let new1 = new_of(&f, "sa_901").expect("sa_901 resumed by the daemon");
    assert!(new0 != new1 && new0 != "sa_900" && new1 != "sa_901", "{new0} {new1}");
    for (old, new) in [("sa_900", &new0), ("sa_901", &new1)] {
        let wakes = parent_wakes(&f.bodies, old);
        assert_eq!(wakes.len(), 1, "{old}'s news, once: {wakes:#?}");
        assert!(
            wakes[0].contains(&format!(
                "was stopped by a daemon reload and resumed by the daemon as {new}; its completion will arrive as {new}."
            )),
            "{wakes:#?}"
        );
        assert!(!wakes[0].contains("subagent_resume with handle_id"), "{wakes:#?}");
    }
    let notices = notices(&seen);
    assert!(
        notices.iter().any(|n| n.contains(&format!("the daemon resumed workers sa_900 as {new0}, sa_901 as {new1}"))),
        "{notices:?}"
    );

    // Each worker continues its own conversation with the daemon's words.
    let workers_seen = poll("both workers' requests", &mut f, |f| {
        let w = worker_bodies(&f.bodies);
        (w.len() >= 2).then_some(w)
    })
    .await;
    for body in &workers_seen {
        assert!(texts(&body["messages"][0]).contains("dig the tunnel"), "{body}");
        assert_eq!(last_text(body), DAEMON_WORDS, "{body}");
    }

    // Nothing more: no second turn, and the same record again resumes nothing.
    adopt(&f, workers, WorkerResume::Auto).await;
    let later = drain_for(&mut f.client, Duration::from_millis(1500)).await;
    assert_eq!(started(&later), 0, "{later:#?}");
    assert_eq!(worker_bodies(&f.bodies).len(), 2, "one launch per worker");
    assert_eq!(new_of(&f, "sa_900"), Some(new0));
    end(&mut f.client).await;
}

/// The resumed run's completion reaches the parent as the new handle's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_daemon_resumed_workers_completion_arrives_as_its_new_handle() {
    let _h = Home::new();
    let mut f = fixture().await;
    adopt(&f, vec![stopped("sa_1905", Some(archive("sa_1905", 0)))], WorkerResume::Auto).await;
    let new = poll("the daemon's resume", &mut f, |f| new_of(f, "sa_1905")).await;
    let done = format!("Subagent 'digger' ({new}) finished with status 'completed'");
    poll("the new handle's completion", &mut f, |f| {
        f.bodies
            .lock()
            .unwrap()
            .iter()
            .filter(|b| !is_worker(b))
            .any(|b| texts(&b["messages"]).contains(&done))
            .then_some(())
    })
    .await;
    end(&mut f.client).await;
}

/// A worker the daemon may not resume keeps today's wake, saying why: its
/// resume is over the context limit, its run left no archive, or its
/// archive can't be read (a failed launch). One turn for the batch, no
/// worker started. Mutants: the plan resumes a worker with no archive (its
/// launch fails with another reason); a failed launch's wake drops the
/// reason (`build_reload_carry_event` in place of the left event).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn workers_the_daemon_may_not_resume_keep_todays_wake_with_the_reason() {
    let h = Home::new();
    std::fs::write(
        h.dir.path().join(".synaps-cli/config"),
        "subagent.resume_refuse_tokens = 50000\n",
    )
    .unwrap();
    let mut f = fixture().await;
    let big = archive("sa_2910", 400_000);
    let missing = big.with_file_name("sa_2912.json");
    assert!(!missing.exists());
    adopt(
        &f,
        vec![
            stopped("sa_2910", Some(big)),
            stopped("sa_2911", None),
            stopped("sa_2912", Some(missing)),
        ],
        WorkerResume::Auto,
    )
    .await;
    let seen = one_turn(&mut f).await;
    assert_eq!(started(&seen), 1, "{seen:#?}");
    for (id, reason) in [
        ("sa_2910", "Not resumed automatically: subagent_resume refused (context limit)"),
        ("sa_2911", "Not resumed automatically: its conversation was not archived."),
        ("sa_2912", "Not resumed automatically: Cannot resume 'sa_2912'"),
    ] {
        let wakes = parent_wakes(&f.bodies, id);
        assert_eq!(wakes.len(), 1, "{id}: {wakes:#?}");
        assert!(wakes[0].contains("by a daemon reload, not by the user"), "{wakes:#?}");
        assert!(wakes[0].contains(reason), "{id}: {wakes:#?}");
        assert_eq!(new_of(&f, id), None, "{id} is the model's to resume");
    }
    assert!(worker_bodies(&f.bodies).is_empty(), "no worker started");
    let notices = notices(&seen);
    assert!(
        notices.iter().any(|n| n.contains("sa_2910, sa_2911, sa_2912 stopped by the reload; subagent_resume")),
        "{notices:?}"
    );
    end(&mut f.client).await;
}

/// When the reload's rehydrate says not to (a parked session), or says
/// nothing (a crash or stop restore), the worker is left to the model:
/// today's wake, with the reason when there is one. Mutant: the actor
/// treats `Off` as `Auto`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_parked_session_or_a_restore_leaves_the_workers_to_the_model() {
    let _h = Home::new();
    let mut f = fixture().await;
    adopt(
        &f,
        vec![stopped("sa_5914", Some(archive("sa_5914", 0)))],
        WorkerResume::Off("its session was parked"),
    )
    .await;
    one_turn(&mut f).await;
    let wakes = parent_wakes(&f.bodies, "sa_5914");
    assert_eq!(wakes.len(), 1, "{wakes:#?}");
    assert!(wakes[0].contains("subagent_resume with handle_id \"sa_5914\""), "{wakes:#?}");
    assert!(wakes[0].contains("Not resumed automatically: its session was parked."), "{wakes:#?}");

    adopt(&f, vec![stopped("sa_5915", Some(archive("sa_5915", 0)))], WorkerResume::Never).await;
    one_turn(&mut f).await;
    let wakes = parent_wakes(&f.bodies, "sa_5915");
    assert_eq!(wakes.len(), 1, "{wakes:#?}");
    assert!(wakes[0].contains("subagent_resume with handle_id \"sa_5915\""), "{wakes:#?}");
    assert!(!wakes[0].contains("Not resumed automatically"), "{wakes:#?}");
    assert!(worker_bodies(&f.bodies).is_empty(), "no worker started");
    assert_eq!(new_of(&f, "sa_5914"), None);
    end(&mut f.client).await;
}

/// The reload cut the parent's turn: the rehydrate resumes that turn
/// (`ResumeAfterReload`), then hands over the workers. Their news lands
/// while the resumed turn's request is out and rides into that turn (the
/// engine takes steered events before it finishes): one turn in all. Mutant: the resumed wakes are pushed from a
/// task spawned after the launches (they land after the resumed turn and
/// start a second one).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_cut_turns_resume_and_the_daemons_worker_news_make_one_turn() {
    let _h = Home::new();
    let mut f = fixture().await;
    f.hold.store(true, Ordering::SeqCst);
    f.client.send(submit(HANG_TURN)).await.unwrap();
    until(&mut f.client, |e| {
        matches!(
            e,
            SessionEventWire::Stream(agent_engine::StreamEvent::Llm(agent_engine::LlmEvent::Text(_)))
        )
    })
    .await;
    f.handle
        .send(SessionCommand::Checkpoint {
            reason: CheckpointReason::Reload,
            deadline: None,
            skip_quiesce_grace: false,
        })
        .await
        .unwrap();
    until(&mut f.client, |e| {
        matches!(e, SessionEventWire::QueryResult { id, .. } if *id == CHECKPOINT_QUERY_ID)
    })
    .await;
    // No exec here: lift the fence the way a failed exec does.
    f.handle.send(SessionCommand::ReloadAborted).await.unwrap();
    let quiet = drain_for(&mut f.client, Duration::from_millis(500)).await;
    assert_eq!(started(&quiet), 0, "{quiet:#?}");

    // The rehydrate's two commands, in its order.
    f.handle
        .send(SessionCommand::ResumeAfterReload {
            generation: 5,
            reloaded_at: chrono::Utc::now(),
            turn_cut: Some(true),
            cut_tail: None,
            stopped: false,
            reply: None,
        })
        .await
        .unwrap();
    adopt(&f, vec![stopped("sa_3930", Some(archive("sa_3930", 0)))], WorkerResume::Auto).await;
    let seen = drain_for(&mut f.client, Duration::from_secs(4)).await;
    assert_eq!(started(&seen), 1, "{seen:#?}");
    let new = new_of(&f, "sa_3930").expect("sa_3930 resumed by the daemon");
    let news = format!("resumed by the daemon as {new}");
    assert!(
        seen.iter().any(|e| matches!(&e.event, SessionEventWire::External(ev) if ev.content.text.contains(&news))),
        "{seen:#?}"
    );
    end(&mut f.client).await;
}

/// A daemon resume needs nothing only a turn holds: its worker gets the
/// same tools as a worker resumed by hand from a turn (the worker
/// registry; never the turn's activations or MCP/extension leases).
/// Mutant: `resume_for_daemon` starts the worker on an empty registry
/// (`worker_tools: Some(&empty)`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_daemon_resumed_worker_has_the_tools_a_resume_by_hand_gets() {
    let _h = Home::new();
    let mut f = fixture().await;
    // sa_4921: an earlier worker's archive (no wake owed), resumed by hand.
    let by_hand = CarriedWorker {
        stopped_by_reload: false,
        wake: false,
        ..stopped("sa_4921", Some(archive("sa_4921", 0)))
    };
    adopt(
        &f,
        vec![stopped("sa_4920", Some(archive("sa_4920", 0))), by_hand],
        WorkerResume::Auto,
    )
    .await;
    one_turn(&mut f).await;
    assert!(new_of(&f, "sa_4920").is_some());
    assert_eq!(new_of(&f, "sa_4921"), None);
    f.client.send(submit(RESUME_BY_HAND)).await.unwrap();
    let (daemon, hand) = poll("both resumed workers' requests", &mut f, |f| {
        let w = worker_bodies(&f.bodies);
        let daemon = w.iter().find(|b| last_text(b) == DAEMON_WORDS)?.clone();
        let hand = w.iter().find(|b| last_text(b).ends_with("go on"))?.clone();
        Some((daemon, hand))
    })
    .await;
    let names = tool_names(&daemon);
    assert!(!names.is_empty(), "{daemon}");
    assert_eq!(names, tool_names(&hand));
    end(&mut f.client).await;
}

/// The cap: after one reload the daemon resumes at most sixteen workers per
/// session, oldest handle first (whatever order the record hands them
/// over in); the next is left to the model with today's wake and the cap
/// as its reason. Mutant: the cap back at 8.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn the_daemon_resumes_sixteen_workers_and_leaves_the_seventeenth_to_the_model() {
    let _h = Home::new();
    let mut f = fixture().await;
    f.hold.store(true, Ordering::SeqCst);
    let ids: Vec<String> = (6000..6017).map(|n| format!("sa_{n}")).collect();
    // Handed over newest first.
    let workers = ids.iter().rev().map(|id| stopped(id, Some(archive(id, 0)))).collect();
    adopt(&f, workers, WorkerResume::Auto).await;
    let seen = one_turn(&mut f).await;
    assert_eq!(started(&seen), 1, "{seen:#?}");
    for id in &ids[..16] {
        assert!(new_of(&f, id).is_some(), "{id} resumed by the daemon");
    }
    assert_eq!(new_of(&f, "sa_6016"), None, "the seventeenth is past the cap");
    let wakes = parent_wakes(&f.bodies, "sa_6016");
    assert_eq!(wakes.len(), 1, "{wakes:#?}");
    assert!(wakes[0].contains("subagent_resume with handle_id \"sa_6016\""), "{wakes:#?}");
    assert!(
        wakes[0].contains(
            "Not resumed automatically: the daemon resumes at most 16 workers per session after a reload."
        ),
        "{wakes:#?}"
    );
    end(&mut f.client).await;
}

/// A worker the daemon tried and could not resume takes no slot: the next
/// one in line gets the try. The two oldest are refused by the resume size
/// rule; the sixteen after them are all resumed, and none is left by the
/// cap. Mutant: a refused or failed launch still counts toward the cap.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn workers_the_daemon_could_not_resume_take_no_slot() {
    let h = Home::new();
    std::fs::write(
        h.dir.path().join(".synaps-cli/config"),
        "subagent.resume_refuse_tokens = 50000\n",
    )
    .unwrap();
    let mut f = fixture().await;
    f.hold.store(true, Ordering::SeqCst);
    let ids: Vec<String> = (7000..7018).map(|n| format!("sa_{n}")).collect();
    let workers = ids
        .iter()
        .enumerate()
        .map(|(i, id)| stopped(id, Some(archive(id, if i < 2 { 400_000 } else { 0 }))))
        .collect();
    adopt(&f, workers, WorkerResume::Auto).await;
    let seen = one_turn(&mut f).await;
    assert_eq!(started(&seen), 1, "{seen:#?}");
    for id in &ids[..2] {
        assert_eq!(new_of(&f, id), None, "{id} is the model's to resume");
        let wakes = parent_wakes(&f.bodies, id);
        assert_eq!(wakes.len(), 1, "{id}: {wakes:#?}");
        assert!(
            wakes[0].contains("Not resumed automatically: subagent_resume refused (context limit)"),
            "{id}: {wakes:#?}"
        );
    }
    for id in &ids[2..] {
        assert!(new_of(&f, id).is_some(), "{id} resumed by the daemon");
        let wakes = parent_wakes(&f.bodies, id);
        assert_eq!(wakes.len(), 1, "{id}: {wakes:#?}");
        assert!(!wakes[0].contains("workers per session after a reload"), "{id}: {wakes:#?}");
    }
    end(&mut f.client).await;
}

/// Train30 item 4b: a worker the reload stopped whose final record was
/// lost (its write failed in the drain) is left to the model. The adopting
/// image marks its head, the crash checkpoint, cancelled (no inventory sees
/// a "running" head no registry owns), and the parent's wake says the final
/// was lost and gives the head's path to resume from.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_worker_whose_final_was_lost_is_left_with_its_head_marked_cancelled() {
    let _h = Home::new();
    let mut f = fixture().await;
    let history = [
        json!({"role": "user", "content": "dig the tunnel"}),
        json!({"role": "assistant", "content": [{"type": "text", "text": "digging"}]}),
    ];
    let head = agent_engine::tools::write_test_crash_head("sa_8930", &history).unwrap();
    let status = |path: &std::path::Path| -> String {
        let v: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        v["status"].as_str().unwrap().to_string()
    };
    assert_eq!(status(&head), "running");
    adopt(
        &f,
        vec![CarriedWorker { crash_head: Some(head.clone()), ..stopped("sa_8930", None) }],
        WorkerResume::Auto,
    )
    .await;
    let seen = one_turn(&mut f).await;
    assert_eq!(started(&seen), 1, "{seen:#?}");
    assert_eq!(status(&head), "cancelled", "the adopting image marked the crash checkpoint");
    let wakes = parent_wakes(&f.bodies, "sa_8930");
    assert_eq!(wakes.len(), 1, "{wakes:#?}");
    assert!(wakes[0].contains("Its final record was lost"), "{wakes:#?}");
    assert!(
        wakes[0].contains(&format!("subagent_resume with archive_path \"{}\"", head.display())),
        "{wakes:#?}"
    );
    assert!(
        wakes[0].contains("Not resumed automatically: its final record was lost."),
        "{wakes:#?}"
    );
    assert_eq!(new_of(&f, "sa_8930"), None, "the model's to resume");
    assert!(worker_bodies(&f.bodies).is_empty(), "no worker started");
    end(&mut f.client).await;
}
