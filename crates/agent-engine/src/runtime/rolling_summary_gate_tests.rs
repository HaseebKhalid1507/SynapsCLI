// The rolling summary's no-ledger gate (`docs/specs/subagent-context-rollover.md`
// §6, slice 6), included into `rolling_summary::transport_tests::warm_tests`.
// The summary stage books a v2 worker run's summaries in its request ledger;
// every session (and every worker while worker rollover is off) runs the
// same stage without one. Each case runs two real streamed turns, the second
// rolling over with a summary, against a loopback stand-in, and compares
// what the stand-in received (method, path, headers, the exact body bytes),
// both turns' event sequences and those events' client-wire lines with
// fixtures recorded on the code before the summary booking.
//
// Provenance: the fixtures were written at e240e3e4 (before the summary
// booking), twice, with identical results, by
// `SUMMARY_GATE_WRITE=1 cargo test -p synaps-engine summary_gate`, from this
// file unchanged (included there the same way). Write them again only when a
// request or event change is intended and reviewed. The only normalization
// is `gate_stable`'s.
//
// Provenance on the dev port: the fixtures are re-pinned (e68127e6) to pin
// A, 4dbbdcb2, written with `SUMMARY_GATE_WRITE=1` at a7e1f463 (the dev base
// 7fdd5a4d plus these gate files, no production change), twice, the second
// write identical to the first. The e240e3e4 recording is history.

/// One request as the gate's stand-in received it.
struct GateSeen {
    method: axum::http::Method,
    uri: axum::http::Uri,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
}

/// How the stand-in answers a screened (cold, non-streaming) summary.
#[derive(Clone, Copy)]
enum ColdAnswer {
    Written,
    ServerError,
}

/// The stand-in: conversation turns report a cache write unless `turn_usage`
/// says otherwise; the warm summary reads it; a cold summary gets `cold`.
async fn gate_stand_in(
    turn_usage: Value,
    cold: ColdAnswer,
) -> (Arc<Mutex<Vec<GateSeen>>>, crate::test_env::EnvVarGuard) {
    let seen: Arc<Mutex<Vec<GateSeen>>> = Arc::default();
    let record = Arc::clone(&seen);
    let app = Router::new().fallback(
        move |method: axum::http::Method,
              uri: axum::http::Uri,
              headers: axum::http::HeaderMap,
              body: axum::body::Bytes| {
            let record = Arc::clone(&record);
            let turn_usage = turn_usage.clone();
            async move {
                let text = String::from_utf8_lossy(&body).into_owned();
                record.lock().unwrap().push(GateSeen {
                    method,
                    uri,
                    headers,
                    body,
                });
                if !text.contains("\"stream\":true") {
                    return match cold {
                        ColdAnswer::Written => {
                            let reply = json!({"id":"s","type":"message","role":"assistant","model":"claude-sonnet-4-6","content":[{"type":"text","text":"## Window 1\nCOLD-SUMMARY"}],"stop_reason":"end_turn","usage":{"input_tokens":3_000,"output_tokens":120}});
                            (StatusCode::OK, [("content-type", "application/json")], reply.to_string())
                                .into_response()
                        }
                        ColdAnswer::ServerError => {
                            let reply = json!({"type":"error","error":{"type":"api_error","message":"internal"}});
                            (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                [("content-type", "application/json")],
                                reply.to_string(),
                            )
                                .into_response()
                        }
                    };
                }
                let reply = if text.contains(WARM_MARK) {
                    streamed(
                        "## Window 1\nWARM-SUMMARY",
                        json!({"input_tokens":60,"output_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":4_000}),
                        120,
                    )
                } else {
                    streamed("ok", turn_usage, 5)
                };
                (StatusCode::OK, [("content-type", "text/event-stream")], reply).into_response()
            }
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let guard =
        crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &format!("http://{addr}"));
    (seen, guard)
}

/// A conversation turn's usage with full cache evidence (a 4k cache write).
fn gate_cached_usage() -> Value {
    json!({"input_tokens":100,"output_tokens":0,"cache_creation_input_tokens":4_000,"cache_read_input_tokens":0})
}

/// A conversation turn's usage without the cache-read count: no warm record
/// evidence, so the summary goes cold.
fn gate_uncached_usage() -> Value {
    json!({"input_tokens":100,"output_tokens":0,"cache_creation_input_tokens":4_000})
}

const GATE_TARGET: u64 = 2_500;
const GATE_SUMMARY: u64 = 400;

/// The gate's runtime: [`runtime`] with its tools pinned to `read`, `ls`
/// and the two memory tools automatic context management requires, so the
/// fixtures follow the summary, not the tool catalog, and a small successor
/// target.
fn gate_runtime(summary_model: Option<&str>) -> crate::Runtime {
    let mut rt = runtime(Some(GATE_SUMMARY));
    rt.set_tools(crate::tools::ToolRegistry::from_tools_for_tests(vec![
        Arc::new(crate::tools::ReadTool),
        Arc::new(crate::tools::LsTool),
        Arc::new(crate::tools::memory::MemorySearchTool),
        Arc::new(crate::tools::memory::MemoryFetchTool),
    ]));
    rt.continuation.lock().unwrap().config.target_tokens = Some(GATE_TARGET);
    // Upstream's message times (3bc5a7bcf) stamp each new message with the
    // runtime's clock; a fixed clock keeps the pinned lines byte-stable.
    rt.set_message_clock(crate::runtime::message_time::MessageClock::new(|| {
        chrono::DateTime::from_timestamp(1_767_225_600, 0).unwrap()
    }));
    if let Some(model) = summary_model {
        rt.set_summary_model(Some(model.to_owned()));
    }
    rt
}

/// A small history whose old part the rollover drops.
fn gate_history() -> Vec<SharedMessage> {
    let mut m = vec![msg(json!({"role":"user","content":"charter: build the parser"}))];
    m.push(msg(json!({"role":"assistant","content":[{"type":"tool_use","id":"t_old","name":"read","input":{"path":"src/old.rs"}}]})));
    m.push(msg(json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"t_old","content":"OLD-FILE-BODY"}]})));
    m.extend((0..18).map(|i| {
        msg(json!({"role":"user","content":format!(
            "<event id=\"e{i}\" type=\"worker_report\">report {i}: {}</event>",
            "x".repeat(300)
        )}))
    }));
    m.push(msg(json!({"role":"assistant","content":[{"type":"tool_use","id":"t_new","name":"ls","input":{"path":"."}}]})));
    m.push(msg(json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"t_new","content":"listing"}]})));
    m.push(msg(json!({"role":"user","content":"next: continue the parser"})));
    m
}

/// One turn: every event as the runtime emitted it and its client-wire
/// line, appended to `events` and `wire`; context-head checkpoints are
/// acknowledged as a durable frontend would. Returns the turn's history.
async fn gate_turn(
    rt: &crate::Runtime,
    messages: Vec<SharedMessage>,
    events: &mut String,
    wire: &mut String,
) -> Vec<SharedMessage> {
    let mut stream = rt
        .run_stream_with_messages(messages, Default::default(), None, None, false)
        .await;
    let mut history = Vec::new();
    while let Some(event) = stream.next().await {
        events.push_str(&format!("{event:?}\n"));
        match &event {
            agent_core::StreamEvent::Session(agent_core::SessionEvent::MessageHistory(h)) => {
                history = h.clone();
            }
            agent_core::StreamEvent::Session(agent_core::SessionEvent::ContextHeadCheckpoint {
                receipt,
                ..
            }) => receipt.complete(Ok(())),
            _ => {}
        }
        match crate::session::wire::WireStreamEvent::for_client(event) {
            Some(line) => wire.push_str(&crate::session::wire::encode_line(&line).unwrap()),
            None => wire.push_str("(no client form)\n"),
        }
    }
    history
}

/// Turn 1 below the thresholds (its request is recorded); turn 2 re-sends
/// its history plus one message with the thresholds lowered, so it rolls
/// over. `between` runs between the turns.
async fn gate_turns(rt: &crate::Runtime, between: impl FnOnce(&crate::Runtime)) -> (String, String) {
    let (mut events, mut wire) = (String::new(), String::new());
    set_thresholds(rt, 300_000, 400_000);
    events.push_str("=== turn 1\n");
    wire.push_str("=== turn 1\n");
    let first = gate_turn(rt, gate_history(), &mut events, &mut wire).await;
    between(rt);
    // PARITY (upstream dfa68045c): the usage anchor measures turn 2 from
    // turn 1's provider-reported input (100 + a 4k cache write) plus what
    // was appended, about 4.1k, no longer from the larger estimate. The
    // rollover threshold moves below that so turn 2 still rolls over; the
    // pressure stays at 4k, since the successor (and so the summary cap)
    // is sized to 2/3 of it.
    set_thresholds(rt, 4_000, 4_050);
    let mut next = first;
    next.push(msg(json!({"role":"user","content":"carry on in the next window"})));
    events.push_str("=== turn 2\n");
    wire.push_str("=== turn 2\n");
    gate_turn(rt, next, &mut events, &mut wire).await;
    (events, wire)
}

/// Headers that differ between runs for reasons unrelated to the request:
/// the stand-in's ephemeral port.
const GATE_VOLATILE_HEADERS: &[&str] = &["host"];

fn gate_requests(requests: &[GateSeen]) -> String {
    let mut out = String::new();
    for (i, r) in requests.iter().enumerate() {
        out.push_str(&format!("=== request {i}\n{} {}\n", r.method, r.uri));
        let mut headers: Vec<(String, String)> = r
            .headers
            .iter()
            .filter(|(name, _)| !GATE_VOLATILE_HEADERS.contains(&name.as_str()))
            .map(|(name, value)| {
                (
                    name.as_str().to_owned(),
                    String::from_utf8_lossy(value.as_bytes()).into_owned(),
                )
            })
            .collect();
        headers.sort();
        for (name, value) in headers {
            out.push_str(&format!("{name}: {value}\n"));
        }
        out.push_str(&format!("--- body ({} bytes)\n", r.body.len()));
        out.push_str(&String::from_utf8_lossy(&r.body));
        out.push('\n');
    }
    out
}

/// `text` without what differs between runs for reasons unrelated to the
/// summary, each replaced by a placeholder of its kind: a turn error's
/// correlation ID (it carries the process ID and the process's turn count),
/// random UUIDs (a message's host turn tag, the ephemeral session ID) and a
/// window's random 32-hex archive ID (also as `ctx-<id>`). A body's byte
/// count is taken before this, so it stays exact.
fn gate_stable(text: &str) -> String {
    let text = regex::Regex::new(r"turn-\d+-\d+")
        .unwrap()
        .replace_all(text, "turn-<pid>-<n>");
    let text = regex::Regex::new(r"\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b")
        .unwrap()
        .replace_all(&text, "<uuid>");
    regex::Regex::new(r"\b[0-9a-f]{32}\b")
        .unwrap()
        .replace_all(&text, "<archive-id>")
        .into_owned()
}

/// Compare `actual` with the fixture `name`, or write it under
/// `SUMMARY_GATE_WRITE=1`.
fn gate_check(name: &str, actual: &str) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/summary_gate")
        .join(name);
    if std::env::var_os("SUMMARY_GATE_WRITE").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    if expected != actual {
        let line = expected
            .lines()
            .zip(actual.lines())
            .position(|(a, b)| a != b)
            .unwrap_or_else(|| expected.lines().count().min(actual.lines().count()));
        panic!(
            "{name} differs from the pre-booking fixture at line {}:\n  expected: {:?}\n  actual:   {:?}",
            line + 1,
            expected.lines().nth(line).map(|l| l.chars().take(400).collect::<String>()),
            actual.lines().nth(line).map(|l| l.chars().take(400).collect::<String>()),
        );
    }
}

/// Run `name`'s two turns and compare its requests, events and wire lines.
async fn summary_gate(
    name: &str,
    rt: crate::Runtime,
    seen: Arc<Mutex<Vec<GateSeen>>>,
    between: impl FnOnce(&crate::Runtime),
) {
    let (events, wire) = gate_turns(&rt, between).await;
    let requests = gate_requests(&seen.lock().unwrap());
    gate_check(&format!("{name}_requests.txt"), &gate_stable(&requests));
    gate_check(&format!("{name}_events.txt"), &gate_stable(&events));
    gate_check(&format!("{name}_wire.txt"), &gate_stable(&wire));
}

/// A cold summary written and reported, without a request ledger.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn summary_gate_cold_is_unchanged_without_a_ledger() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (seen, _g) = gate_stand_in(gate_uncached_usage(), ColdAnswer::Written).await;
    summary_gate("cold", gate_runtime(None), seen, |_| {}).await;
}

/// A cold summary the provider answers with a server error (its spend
/// unknown), without a request ledger.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn summary_gate_cold_unknown_is_unchanged_without_a_ledger() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (seen, _g) = gate_stand_in(gate_uncached_usage(), ColdAnswer::ServerError).await;
    summary_gate("cold_unknown", gate_runtime(None), seen, |_| {}).await;
}

/// A cold summary that is provably not sent (its endpoint refuses the
/// connection), without a request ledger.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn summary_gate_cold_not_sent_is_unchanged_without_a_ledger() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (seen, _g) = gate_stand_in(gate_uncached_usage(), ColdAnswer::Written).await;
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let _local = crate::test_env::EnvVarGuard::set(
        "LOCAL_ENDPOINT",
        &format!("http://127.0.0.1:{closed}/v1"),
    );
    summary_gate("cold_not_sent", gate_runtime(Some("local/test-model")), seen, |_| {}).await;
}

/// A warm summary written and reported, without a request ledger.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn summary_gate_warm_is_unchanged_without_a_ledger() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (seen, _g) = gate_stand_in(gate_cached_usage(), ColdAnswer::Written).await;
    summary_gate("warm", gate_runtime(None), seen, |_| {}).await;
}

/// A warm summary not sent (the rebuilt prefix no longer matches the
/// record), then its cold fallback, without a request ledger.
#[tokio::test]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn summary_gate_warm_unsent_then_cold_is_unchanged_without_a_ledger() {
    let _base = crate::test_env::BaseDirGuard::new();
    let (seen, _g) = gate_stand_in(gate_cached_usage(), ColdAnswer::Written).await;
    summary_gate("warm_unsent_cold", gate_runtime(None), seen, |rt| {
        rt.continuation
            .lock()
            .unwrap()
            .warm_record()
            .expect("recorded")
            .tap
            .built([0x5a; 32]);
    })
    .await;
}
