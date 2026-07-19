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
