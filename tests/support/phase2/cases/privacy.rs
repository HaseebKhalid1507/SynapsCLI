// ─────────────────────────────────────────────────────────────────────────────
// S3 — timing buckets (S2 lives in s1_anthropic_…).
// ─────────────────────────────────────────────────────────────────────────────

/// Independently delays response headers, then the first body byte (an SSE
/// comment — not a model event), then the first model event, with every SSE
/// frame fragmented into ≤7-byte chunks. The four buckets must be observed,
/// ordered, and DISTINCT with tolerant lower bounds — a header delay must
/// not leak into the first-byte or model-event bucket.
///
/// RED at d20e03f: no timing observation existed. GREEN: `b2e0f82`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn s3_timing_buckets_headers_first_byte_model_event_are_ordered_and_distinct() {
    const HEADER_MS: u64 = 300;
    const FIRST_BYTE_MS: u64 = 250;
    const EVENT_MS: u64 = 250;

    let guard = HomeGuard::new();
    let (url, _, _) = spawn_stub(Script::Timed {
        header_delay: Duration::from_millis(HEADER_MS),
        first_byte_delay: Duration::from_millis(FIRST_BYTE_MS),
        event_delay: Duration::from_millis(EVENT_MS),
        body: ANTHROPIC_SSE,
    })
    .await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);

    let mut rt = Runtime::new().await.expect("runtime");
    rt.set_model("claude-sonnet-4-5".to_string());
    rt.set_telemetry_level(TelemetryLevel::Basic);
    let ev = drive_runtime_turn(&rt, "timing fixture", false).await;
    assert!(
        turn_completed(&ev),
        "fragmented SSE must still decode fully"
    );
    rt.shutdown_observability_async(Duration::from_secs(10))
        .await;

    let records = read_traces(&guard.trace_log());
    assert_eq!(records.len(), 1);
    let t = &records[0].outcome.timings;
    assert!(t.send_start_unix_ms.is_some(), "send start observed");
    let headers = t.headers_ms.expect("headers bucket observed");
    let first_byte = t.first_byte_ms.expect("first-byte bucket observed");
    let first_event = t
        .first_model_event_ms
        .expect("first-model-event bucket observed");
    let end = t.stream_end_ms.expect("stream-end bucket observed");

    // Tolerant lower bounds: each stage must absorb its own injected delay.
    assert!(
        headers >= HEADER_MS - 50,
        "headers_ms={headers} < header delay"
    );
    assert!(
        first_byte >= headers + FIRST_BYTE_MS - 50,
        "first_byte_ms={first_byte} must include the body delay beyond headers_ms={headers}"
    );
    assert!(
        first_event >= first_byte + EVENT_MS - 50,
        "first_model_event_ms={first_event} must include the event delay beyond first_byte_ms={first_byte}"
    );
    assert!(end >= first_event, "stream_end after first model event");
    // Distinct buckets: collapsing headers/first-byte/first-event into one
    // observation fails here.
    assert!(headers < first_byte && first_byte < first_event);
}

// ─────────────────────────────────────────────────────────────────────────────
// S4 — translation losses explicit, or semantics preserved.
// ─────────────────────────────────────────────────────────────────────────────

/// (a) A clean text-only Gemini fixture reports ZERO losses and its wire
/// body (captured at the loopback broker) preserves the normalized text and
/// system prompt. (b) A message the Gemini translator must drop is reported
/// explicitly. (c) A dotted tool name the OpenAI wire must rename is
/// reported explicitly. RED at d20e03f; GREEN `1de6426` + `6e1c3dc`/`0d5c46a`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s4_translation_losses_explicit_or_semantics_preserved() {
    const MODEL: &str = "google-gemini/gemini-2.5-pro";

    // (a) clean fixture → no losses, semantics preserved on the wire.
    let (endpoint, _, bodies) = spawn_broker(BrokerScript::ProxySse(GEMINI_SSE)).await;
    let tmp = tempfile::TempDir::new().unwrap();
    let (ctx, sink) = collecting_ctx(&tmp);
    let run = drive_try_route(
        MODEL,
        &remote(&endpoint),
        &ctx,
        &sink,
        vec![user_msg("normalized-text-survives")],
        vec![],
        Some("normalized-system-survives".to_string()),
        false,
    )
    .await;
    run.result.as_ref().expect("clean gemini fixture");
    assert_eq!(run.records.len(), 1);
    assert!(
        run.records[0].translation_losses.is_empty(),
        "clean fixture must report zero losses: {:#?}",
        run.records[0].translation_losses
    );
    let wire_body = String::from_utf8(bodies.lock().unwrap()[0].clone()).unwrap();
    assert!(
        wire_body.contains("normalized-text-survives"),
        "user text must survive translation onto the wire"
    );
    assert!(
        wire_body.contains("normalized-system-survives"),
        "system prompt must survive translation onto the wire"
    );

    // (b) dropped message → explicit loss.
    let run = broker_route_run(
        MODEL,
        BrokerScript::ProxySse(GEMINI_SSE),
        vec![
            Arc::new(serde_json::json!({"role": "tool", "content": "dropped-by-gemini"})),
            user_msg("kept"),
        ],
        vec![],
        None,
        false,
    )
    .await;
    run.result.as_ref().expect("gemini drop fixture");
    assert!(
        !run.records[0].translation_losses.is_empty(),
        "a dropped message must be reported, never silent"
    );

    // (c) renamed tool on the OpenAI wire → explicit rewrite.
    let run = broker_route_run(
        "groq/fixture-model",
        BrokerScript::ProxySse(OAI_CHAT_SSE),
        vec![user_msg("hi")],
        vec![serde_json::json!({
            "name": "my.dotted.tool",
            "description": "fixture",
            "input_schema": {"type": "object", "properties": {}}
        })],
        None,
        false,
    )
    .await;
    run.result.as_ref().expect("renamed-tool fixture");
    assert!(
        !run.records[0].translation_losses.is_empty(),
        "a renamed tool must be reported, never silent"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// S5 — trace secret exfiltration probe.
// ─────────────────────────────────────────────────────────────────────────────

/// Plants sentinels in the prompt, system prompt, tool args + result blocks
/// (pre-seeded history), the bearer credential, and a hostile provider
/// error that echoes the full request. Runs failing AND content-armed
/// successful turns, then scans EVERY persisted byte under HOME for any
/// sentinel. The armed capture bundle must exist and carry redaction
/// markers instead of secrets.
///
/// RED at d20e03f: `runtime/api.rs` traced the full payload; no capture
/// redaction existed. GREEN: Phase 1 (`ad8b0cb`, `97374e6`) + `2c381b4`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn s5_trace_secret_exfiltration_probe() {
    let guard = HomeGuard::new();
    let mut rt = Runtime::new().await.expect("runtime");
    rt.set_model("claude-sonnet-4-5".to_string());
    rt.set_api_retries(1);
    rt.set_system_prompt(format!("{S_SYSTEM} you are a fixture"));
    rt.set_telemetry_level(TelemetryLevel::Full);

    let history = vec![
        user_msg(&format!("{S_PROMPT} question")),
        Arc::new(serde_json::json!({
            "role": "assistant",
            "content": [{"type": "tool_use", "id": "toolu_01", "name": "probe_tool",
                         "input": {"arg": S_TOOL_ARGS}}]
        })),
        Arc::new(serde_json::json!({
            "role": "user",
            "content": [{"type": "tool_result", "tool_use_id": "toolu_01",
                         "content": S_TOOL_RESULT, "is_error": true}]
        })),
        user_msg("continue"),
    ];

    // Hostile echoing provider failure (provider-controlled error text).
    // The surfaced terminal-error / notice strings must exist and carry
    // NONE of the sentinels the provider echoes back.
    let (url, _, _) = spawn_stub(Script::AlwaysFailEcho(500)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let hostile_events = drive_runtime_history_turn(&rt, history).await;
    assert_surfaced_errors_sentinel_free(&hostile_events);

    // Content-armed successful turn: nested-secret capture must be redacted.
    let (url, _, _) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    rt.trace_arm_next(true);
    drive_runtime_history_turn(
        &rt,
        vec![Arc::new(serde_json::json!({
            "role": "user",
            "content": [{"type": "text",
                         "text": format!("api_key={S_NESTED} password={S_NESTED}")}]
        }))],
    )
    .await;
    rt.shutdown_observability_async(Duration::from_secs(10))
        .await;

    // Metadata log parses strictly and is sentinel-free.
    let records = read_traces(&guard.trace_log());
    assert!(!records.is_empty());
    for r in &records {
        assert_record_conformant(r);
    }

    // Whole-tree scan. auth.json legitimately holds the credential sentinel
    // (it IS the credential store); session transcripts legitimately hold
    // the user's own prompt; the trace/capture bundle is the EXPLICIT
    // double-opt-in content path (checked separately below). Everything
    // else — metadata logs, key material, tracing logs — must be clean.
    let leaks = scan_tree_for_sentinels(guard.home.path(), &|p: &std::path::Path| {
        p.ends_with("auth.json")
            || p.components()
                .any(|c| c.as_os_str() == "sessions" || c.as_os_str() == "capture")
    });
    assert!(
        leaks.is_empty(),
        "sentinel leaked into persisted state:\n{leaks:#?}"
    );

    // The armed capture bundle exists, is credential-redacted (the bearer
    // token never appears — headers/credentials are structurally excluded
    // and secret-shaped fields are scrubbed to markers), and is private.
    let capture_dir = guard.base_dir().join("trace/capture");
    let bundles: Vec<_> = std::fs::read_dir(&capture_dir)
        .map(|it| it.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    assert!(
        !bundles.is_empty(),
        "content-armed turn must write a capture bundle under {capture_dir:?}"
    );
    let bundle = std::fs::read_to_string(&bundles[0]).unwrap();
    assert!(
        bundle.to_lowercase().contains("redact"),
        "capture bundle must carry explicit redaction markers"
    );
    assert!(
        !bundle.contains(S_TOKEN),
        "the bearer credential must never reach a capture bundle"
    );
    #[cfg(unix)]
    assert_eq!(
        mode_of(&bundles[0]),
        0o600,
        "capture bundle must be private"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// S6 — slow/broken storage never delays or fails a model turn.
// ─────────────────────────────────────────────────────────────────────────────

/// A writer with an artificial 400 ms per-record delay and a 1-slot queue:
/// model turns finish at wire speed; later records overflow into the
/// dropped counter with exactly one overflow warning.
/// RED at d20e03f…`6e1c3dc` (no writer); GREEN `3e7378a`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s6_slow_storage_never_delays_turn_and_overflow_is_counted() {
    let tmp = tempfile::TempDir::new().unwrap();
    let writer = TelemetryWriter::new(WriterOptions {
        telemetry_path: None,
        trace_path: Some(tmp.path().join("trace.jsonl")),
        capacity: 1,
        max_file_bytes: synaps_cli::runtime::telemetry::DEFAULT_MAX_FILE_BYTES,
        write_delay: Some(Duration::from_millis(400)),
    });
    let ctx = TraceContext::with_sink(Arc::new(WriterTraceSink::new(writer.clone())))
        .with_key_path(tmp.path().join("digest.key"));
    let (endpoint, _, _) = spawn_broker(BrokerScript::ProxySse(OAI_CHAT_SSE)).await;
    let source = remote(&endpoint);
    let sink_unused = CollectingTraceSink::new(); // driver needs a sink handle

    let started = Instant::now();
    for i in 0..6 {
        let run = drive_try_route(
            "groq/fixture-model",
            &source,
            &ctx,
            &sink_unused,
            vec![user_msg(&format!("slow-storage turn {i}"))],
            vec![],
            None,
            false,
        )
        .await;
        run.result
            .as_ref()
            .expect("turn must succeed regardless of storage");
    }
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "6 loopback turns took {elapsed:?} — trace storage is delaying the request path"
    );
    let stats = writer.stats();
    assert!(
        stats.dropped > 0,
        "overflow must be counted, not silent: {stats:?}"
    );
    assert_eq!(
        stats.overflow_warnings, 1,
        "exactly one overflow warning: {stats:?}"
    );
    writer.shutdown(Duration::from_secs(5));
}

/// A writer whose trace path is unwritable (a directory occupies it): turns
/// still succeed, I/O failures are counted, exactly one I/O warning.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s6_broken_storage_never_fails_turn_and_warns_once() {
    let tmp = tempfile::TempDir::new().unwrap();
    let broken = tmp.path().join("trace.jsonl");
    std::fs::create_dir_all(&broken).unwrap(); // a directory: open() must fail
    let writer = TelemetryWriter::new(WriterOptions {
        telemetry_path: None,
        trace_path: Some(broken),
        capacity: 64,
        max_file_bytes: synaps_cli::runtime::telemetry::DEFAULT_MAX_FILE_BYTES,
        write_delay: None,
    });
    let ctx = TraceContext::with_sink(Arc::new(WriterTraceSink::new(writer.clone())))
        .with_key_path(tmp.path().join("digest.key"));
    let (endpoint, _, _) = spawn_broker(BrokerScript::ProxySse(OAI_CHAT_SSE)).await;
    let sink_unused = CollectingTraceSink::new();

    for i in 0..3 {
        let run = drive_try_route(
            "groq/fixture-model",
            &remote(&endpoint),
            &ctx,
            &sink_unused,
            vec![user_msg(&format!("broken-storage turn {i}"))],
            vec![],
            None,
            false,
        )
        .await;
        run.result
            .as_ref()
            .expect("turn must succeed with broken trace storage");
    }
    writer.shutdown(Duration::from_secs(5));
    let stats = writer.stats();
    assert!(
        stats.io_failures >= 1,
        "broken storage must be counted: {stats:?}"
    );
    assert_eq!(
        stats.io_warnings, 1,
        "one warning per failure class: {stats:?}"
    );
}

/// Direct bounded-shutdown proof IN THIS HARNESS (not delegated to the
/// in-crate writer test): a slow writer (300 ms per record — controlled
/// writer delay) with far more queued trace records than its deadline can
/// drain must return from `shutdown` at the deadline, not after the queue:
/// elapsed stays near the 200 ms deadline and the outcome honestly reports
/// the timeout while the detached worker keeps draining in the background.
#[test]
fn s6_trace_writer_shutdown_deadline_is_bounded_under_slow_storage() {
    let tmp = tempfile::TempDir::new().unwrap();
    let writer = TelemetryWriter::new(WriterOptions {
        telemetry_path: None,
        trace_path: Some(tmp.path().join("trace.jsonl")),
        capacity: 32,
        max_file_bytes: synaps_cli::runtime::telemetry::DEFAULT_MAX_FILE_BYTES,
        write_delay: Some(Duration::from_millis(300)),
    });
    let sink = WriterTraceSink::new(writer.clone());
    for n in 0..10 {
        synaps_cli::runtime::trace::TraceSink::emit(&sink, handcrafted_trace_record(n));
    }
    let started = Instant::now();
    let outcome = writer.shutdown(Duration::from_millis(200));
    let elapsed = started.elapsed();
    assert!(
        !outcome.is_flushed(),
        "10 × 300 ms of queued work cannot flush inside 200 ms — a flushed \
         outcome would mean the deadline was not exercised: {outcome:?}"
    );
    assert!(
        elapsed < Duration::from_millis(1000),
        "shutdown must return at its deadline, not drain the queue: {elapsed:?}"
    );
}
