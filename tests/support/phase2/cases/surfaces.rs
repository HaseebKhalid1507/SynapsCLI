// ─────────────────────────────────────────────────────────────────────────────
// S7 — /context: content-free explanation incl. cache-change flagging.
// ─────────────────────────────────────────────────────────────────────────────

/// The engine surface behind `/context` names every section (model, system,
/// tools, history, skills, memories, cache, trace) and stays content-free
/// even when the system prompt and history contain sentinels. After two
/// traced turns whose tool set changed in between, the cache section must
/// flag the changed tools component. RED at d20e03f; GREEN `2c381b4`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn s7_context_report_is_content_free_and_names_every_section() {
    use synaps_cli::runtime::trace::SegmentChange;
    let guard = HomeGuard::new();
    let (url, _, _) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);

    let mut rt = Runtime::new().await.expect("runtime");
    rt.set_model("claude-sonnet-4-5".to_string());
    rt.set_system_prompt(format!("{S_SYSTEM} fixture prompt"));
    rt.set_telemetry_level(TelemetryLevel::Basic);

    let ev = drive_runtime_turn(&rt, &format!("{S_PROMPT} one"), false).await;
    assert!(turn_completed(&ev));
    // Intentional tool-set change between turns.
    rt.tools_shared()
        .try_write()
        .expect("registry free between turns")
        .disable(&["bash".to_string()]);
    let ev = drive_runtime_turn(&rt, "two", false).await;
    assert!(turn_completed(&ev));

    let history = vec![user_msg(&format!("{S_PROMPT} one")), user_msg("two")];
    let report = rt.context_report(Some(&history));
    let rendered = report.render();
    let lower = rendered.to_lowercase();
    for section in [
        "model",
        "system prompt",
        "tools",
        "history",
        "skills",
        "memories",
        "cache",
        "trace writer",
    ] {
        assert!(
            lower.contains(section),
            "/context must name '{section}':\n{rendered}"
        );
    }
    for s in all_sentinels() {
        assert!(
            !rendered.contains(s),
            "content leaked into /context:\n{rendered}"
        );
    }
    // The changed tools component is flagged, content-free.
    let activity = report
        .cache
        .as_ref()
        .expect("cache activity after traced turns");
    assert_eq!(
        activity.delta.tools,
        Some(SegmentChange::Changed),
        "changed tool set must be flagged:\n{rendered}"
    );
    assert!(
        lower.contains("cache: tools changed"),
        "/context must say which component changed:\n{rendered}"
    );

    rt.shutdown_observability_async(Duration::from_secs(10))
        .await;
    drop(guard);
}

/// Direct probe of the §6.6 delta engine: identical inputs are Unchanged; a
/// pure reorder of the same two tools is flagged as a changed tools prefix
/// with `tool_order_changed`. RED at d20e03f; GREEN `2c381b4`.
#[test]
fn s7_intentional_tool_order_change_is_flagged() {
    use synaps_cli::runtime::trace::{CacheSnapshotStore, SegmentChange};
    let tmp = tempfile::TempDir::new().unwrap();
    let key = load_or_create_digest_key_at(&tmp.path().join("digest.key")).unwrap();
    let store = CacheSnapshotStore::new();
    let tool_a = serde_json::json!({"name": "alpha", "input_schema": {"type": "object"}});
    let tool_b = serde_json::json!({"name": "beta", "input_schema": {"type": "object"}});
    let msgs = vec![user_msg("stable history")];

    let first = store.compare_and_update(
        Some(&key),
        &[tool_a.clone(), tool_b.clone()],
        Some("sys"),
        &msgs,
    );
    assert_eq!(first.delta.expect("delta").tools, Some(SegmentChange::New));
    let same = store.compare_and_update(
        Some(&key),
        &[tool_a.clone(), tool_b.clone()],
        Some("sys"),
        &msgs,
    );
    let same_delta = same.delta.expect("delta");
    assert_eq!(same_delta.tools, Some(SegmentChange::Unchanged));
    assert_eq!(same_delta.system, Some(SegmentChange::Unchanged));
    let swapped = store.compare_and_update(Some(&key), &[tool_b, tool_a], Some("sys"), &msgs);
    let delta = swapped.delta.expect("delta");
    assert_eq!(
        delta.tools,
        Some(SegmentChange::Changed),
        "an intentional tool-order change must be flagged as a prefix change"
    );
    assert!(delta.tool_order_changed, "delta must identify the reorder");
}

// ─────────────────────────────────────────────────────────────────────────────
// S8 — /trace next one-shot semantics (export-CLI half in s1_anthropic_…).
// ─────────────────────────────────────────────────────────────────────────────

/// With telemetry OFF, `/trace next` must persist exactly ONE logical
/// request — including every retry attempt of that request — and nothing
/// afterwards. RED at d20e03f; GREEN `2c381b4`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn s8_trace_next_one_shot_covers_exactly_one_logical_request_including_retries() {
    let guard = HomeGuard::new();
    let (url, hits, _) = spawn_stub(Script::FailThen {
        fails: 1,
        status: 500,
        then: ANTHROPIC_SSE,
    })
    .await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);

    let mut rt = Runtime::new().await.expect("runtime");
    rt.set_model("claude-sonnet-4-5".to_string());
    rt.set_api_retries(1);
    assert_eq!(
        rt.telemetry_level(),
        TelemetryLevel::Off,
        "default stays off"
    );

    rt.trace_arm_next(false);
    let ev = drive_runtime_turn(&rt, "armed retried turn", false).await;
    assert!(turn_completed(&ev));
    assert_eq!(hits.load(Ordering::SeqCst), 2, "one retry happened");

    // A second, unarmed turn must add nothing.
    let ev = drive_runtime_turn(&rt, "unarmed turn", false).await;
    assert!(turn_completed(&ev));
    rt.shutdown_observability_async(Duration::from_secs(10))
        .await;

    let records = read_traces(&guard.trace_log());
    assert_eq!(
        records.len(),
        2,
        "exactly the armed request's two attempts, nothing else: {records:#?}"
    );
    for r in &records {
        assert_record_conformant(r);
    }
    assert_eq!(records[0].request_id, records[1].request_id);
    assert!(is_provider_failed(&records[0]));
    assert!(is_completed(&records[1]));
    assert_eq!(records[1].attempt, 2);
}

/// Tool-loop one-shot lifetime: within ONE engine turn the tool loop issues
/// several logical requests through the SAME shared `ApiOptions.trace`
/// context (the exact `TraceContext` lifetime production tool loops use —
/// `StreamSession.options` is shared across loop iterations). An armed
/// `/trace next` must cover only the FIRST logical request; the tool-result
/// continuation request in the same shared context must NOT trace. Driven
/// through the real `Runtime` with a real local tool (`ls`, innocuous CWD
/// listing) and a loopback tool-use → continuation stub.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn s8_tool_loop_shared_trace_context_continuation_does_not_trace() {
    let guard = HomeGuard::new();
    let (url, hits, _) = spawn_stub(Script::SeqSse(&[ANTHROPIC_SSE_TOOL_USE, ANTHROPIC_SSE])).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);

    let mut rt = Runtime::new().await.expect("runtime");
    rt.set_model("claude-sonnet-4-5".to_string());
    assert_eq!(
        rt.telemetry_level(),
        TelemetryLevel::Off,
        "default stays off"
    );
    rt.trace_arm_next(false);
    let ev = drive_runtime_turn(&rt, "tool loop fixture", false).await;
    assert!(turn_completed(&ev), "tool-loop turn must complete");
    assert_eq!(
        hits.load(Ordering::SeqCst),
        2,
        "tool-use request + continuation request must both reach the stub"
    );
    // The tool genuinely ran: the final history carries its tool_result.
    let history = ev
        .iter()
        .rev()
        .find_map(|e| match e {
            StreamEvent::Session(SessionEvent::MessageHistory(h)) => Some(h.clone()),
            _ => None,
        })
        .expect("turn must surface message history");
    assert!(
        history
            .iter()
            .any(|m| m["content"].as_array().is_some_and(|blocks| blocks
                .iter()
                .any(|b| b["type"] == "tool_result" && b["tool_use_id"] == "toolu_ph2"))),
        "ls tool_result missing from history: {history:#?}"
    );

    rt.shutdown_observability_async(Duration::from_secs(10))
        .await;
    let records = read_traces(&guard.trace_log());
    assert_eq!(
        records.len(),
        1,
        "only the FIRST logical request of the shared-context tool loop may \
         trace; the continuation must not: {records:#?}"
    );
    let r = &records[0];
    assert_record_conformant(r);
    assert!(is_completed(r));
    assert_eq!(r.attempt, 1);
    assert_eq!(
        r.outcome.stop_reason,
        Some(synaps_cli::runtime::trace::StopReason::ToolUse),
        "the traced record must be the tool-use request, proving the \
         end_turn continuation is the untraced one: {r:#?}"
    );
}

/// Content export happy path (§6.1 double opt-in, genuine end-to-end): arm
/// `/trace next content`, run a real turn whose prompt embeds a
/// secret-shaped URL parameter, take the actual `request_id` from the
/// persisted trace record AND the capture bundle, then run the REAL
/// `synaps trace export --include-content --allow-content-export` binary.
/// The export must succeed, be private (0600), carry the
/// `synaps-trace-content-export/1` schema, preserve the benign prompt text,
/// redact the secret-shaped sentinel (marker present, sentinels absent),
/// and CONSUME the capture bundle (second export fails).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn s8_content_export_double_opt_in_succeeds_and_consumes_capture() {
    use synaps_cli::runtime::trace::{ContentExport, CONTENT_EXPORT_SCHEMA};

    const BENIGN: &str = "ph2-benign-content-marker";
    let guard = HomeGuard::new();
    let (url, _, _) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);

    let mut rt = Runtime::new().await.expect("runtime");
    rt.set_model("claude-sonnet-4-5".to_string());
    assert_eq!(rt.telemetry_level(), TelemetryLevel::Off);
    rt.trace_arm_next(true); // `/trace next content`
    let ev = drive_runtime_turn(
        &rt,
        &format!("{BENIGN} please call https://example.invalid/cb?api_key={S_NESTED}"),
        false,
    )
    .await;
    assert!(turn_completed(&ev));
    rt.shutdown_observability_async(Duration::from_secs(10))
        .await;

    // Actual request identity from the persisted trace record…
    let records = read_traces(&guard.trace_log());
    assert_eq!(records.len(), 1, "one armed logical request: {records:#?}");
    let request_id = records[0].request_id.clone();
    // …cross-checked against the capture bundle the armed turn wrote.
    let capture_dir = guard.base_dir().join("trace/capture");
    let bundles: Vec<_> = std::fs::read_dir(&capture_dir)
        .expect("capture dir exists after content-armed turn")
        .flatten()
        .map(|e| e.path())
        .collect();
    assert_eq!(bundles.len(), 1, "exactly one capture bundle: {bundles:#?}");
    let bundle: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&bundles[0]).unwrap()).unwrap();
    assert_eq!(
        bundle["request_id"].as_str(),
        Some(request_id.as_str()),
        "capture bundle must belong to the armed request"
    );

    // Real CLI, both flags → success.
    let out_path = guard.home.path().join("content-export.json");
    let out = run_export_cli(
        &guard,
        &[
            request_id.as_str(),
            "--include-content",
            "--allow-content-export",
            "--output",
            out_path.to_str().unwrap(),
        ],
    );
    assert!(
        out.status.success(),
        "double-opt-in content export must succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    #[cfg(unix)]
    assert_eq!(mode_of(&out_path), 0o600, "content export must be private");

    let raw = std::fs::read_to_string(&out_path).unwrap();
    let export: ContentExport =
        serde_json::from_str(&raw).expect("output must parse as a content export");
    assert_eq!(export.schema, CONTENT_EXPORT_SCHEMA);
    assert_eq!(export.request_id.as_str(), request_id.as_str());
    assert!(export.redacted, "hard redaction marker must be set");
    // `redactions_applied` counts the EXPORT-time defense-in-depth pass
    // only; the secret was already scrubbed at CAPTURE time, so the marker
    // + sentinel-absence asserts below are the meaningful redaction proof.
    let body = serde_json::to_string(&export.body).unwrap();
    assert!(
        body.contains(BENIGN),
        "genuine (benign) request content must be exported: {body}"
    );
    assert!(
        raw.contains("[REDACTED]"),
        "redaction marker must replace the secret value: {raw}"
    );
    for s in all_sentinels() {
        assert!(!raw.contains(s), "sentinel {s} leaked into content export");
    }

    // Consumed: bundle gone, second export fails closed.
    assert!(
        !bundles[0].exists(),
        "capture bundle must be consumed by the export"
    );
    let again = run_export_cli(
        &guard,
        &[
            request_id.as_str(),
            "--include-content",
            "--allow-content-export",
            "--output",
            guard
                .home
                .path()
                .join("content-export-2.json")
                .to_str()
                .unwrap(),
        ],
    );
    assert!(
        !again.status.success(),
        "a consumed capture must not be exportable twice"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// S9 — default workspace regression: telemetry off persists nothing.
// ─────────────────────────────────────────────────────────────────────────────

/// Default configuration (telemetry `off`, nothing armed): a successful
/// turn persists NO trace or telemetry file, and the only network operation
/// is the loopback stub hit (all provider key env vars removed by the
/// guard). GREEN across `073a7b7`…`2c381b4` — red here would mean Phase 2
/// changed default behavior.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn s9_default_telemetry_off_persists_nothing_and_touches_loopback_only() {
    let guard = HomeGuard::new();
    let (url, hits, _) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);

    let mut rt = Runtime::new().await.expect("runtime");
    rt.apply_config(&synaps_cli::SynapsConfig::default());
    rt.set_model("claude-sonnet-4-5".to_string());
    assert_eq!(rt.telemetry_level(), TelemetryLevel::Off);

    let ev = drive_runtime_turn(&rt, "default workspace turn", false).await;
    assert!(turn_completed(&ev), "default workspace turn must succeed");
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "exactly one loopback request"
    );
    rt.shutdown_observability_async(Duration::from_secs(5))
        .await;

    assert!(
        !guard.trace_log().exists(),
        "telemetry off must not create a trace log"
    );
    assert!(
        !guard
            .home
            .path()
            .join(".cache/synaps/api-log.jsonl")
            .exists(),
        "telemetry off must not create the telemetry log"
    );
}
