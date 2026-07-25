// ─────────────────────────────────────────────────────────────────────────────
// S1 — every provider family emits schema-valid records for
//      success/failure/retry/cancel fixtures (+ S2 and export-CLI halves).
// ─────────────────────────────────────────────────────────────────────────────

/// Anthropic family through the REAL `Runtime` (production dispatch),
/// persisted by the REAL bounded writer, read back through the strict
/// `synaps-request-trace/1` parser. Hosts the S2 exact-wire-digest assert
/// and the S8 export-CLI asserts (they need these genuine records).
///
/// RED at d20e03f: no records at all. GREEN: `b2e0f82` (attempt records),
/// `3e7378a` (persistence), `2c381b4` (export CLI).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn s1_anthropic_success_retry_failure_cancel_records_persist_and_validate() {
    let guard = HomeGuard::new();
    let mut rt = Runtime::new().await.expect("runtime");
    rt.set_model("claude-sonnet-4-5".to_string());
    rt.set_api_retries(1);
    rt.set_telemetry_level(TelemetryLevel::Basic);

    // Success.
    let (url, hits_ok, bodies_ok) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let ev = drive_runtime_turn(&rt, &format!("{S_PROMPT} success"), false).await;
    assert!(turn_completed(&ev), "success fixture turn must complete");
    assert_eq!(hits_ok.load(Ordering::SeqCst), 1);

    // Retry: one 500 (retry-after: 0), then success.
    let (url, hits_retry, _) = spawn_stub(Script::FailThen {
        fails: 1,
        status: 500,
        then: ANTHROPIC_SSE,
    })
    .await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let ev = drive_runtime_turn(&rt, "retry fixture", false).await;
    assert!(turn_completed(&ev), "retried turn must complete");
    assert_eq!(hits_retry.load(Ordering::SeqCst), 2, "one retry expected");

    // Terminal failure: every attempt 500, hostile provider echoing the
    // full request back. The failure must surface — but the surfaced
    // error/notice strings must carry NO provider-controlled echoed content.
    let (url, hits_fail, _) = spawn_stub(Script::AlwaysFailEcho(500)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let ev = drive_runtime_turn(&rt, &format!("{S_PROMPT} failure fixture"), false).await;
    assert!(
        !turn_completed(&ev),
        "exhausted retries must surface an error"
    );
    assert!(hits_fail.load(Ordering::SeqCst) >= 1);
    assert_surfaced_errors_sentinel_free(&ev);

    // Cancellation mid-stream.
    let (url, _, _) = spawn_stub(Script::Endless(ANTHROPIC_SSE_PREFIX)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let _ = drive_runtime_turn(&rt, "cancel fixture", true).await;

    rt.shutdown_observability_async(Duration::from_secs(10))
        .await;
    let records = read_traces(&guard.trace_log());
    for r in &records {
        assert_record_conformant(r);
        assert_eq!(r.transport, TransportKind::AnthropicMessages);
        assert_eq!(r.model.as_str(), "anthropic/claude-sonnet-4-5");
    }

    // Success record.
    let ok: Vec<_> = records.iter().filter(|r| is_completed(r)).collect();
    assert!(ok.len() >= 2, "success + retried-success records expected");
    let success = ok[0];
    assert_eq!(success.attempt, 1);
    let usage = success.outcome.usage.expect("provider-reported usage");
    assert_eq!(usage.input_tokens, Some(10));
    assert_eq!(usage.output_tokens, Some(1));

    // Retry: same request_id, attempt 1 (failed) + attempt 2 (completed).
    let retried_ok = ok
        .iter()
        .find(|r| r.attempt == 2)
        .expect("retried request must emit an attempt-2 record");
    assert_eq!(retried_ok.outcome.retries.len(), 1);
    let prior: Vec<_> = records
        .iter()
        .filter(|r| r.request_id == retried_ok.request_id && r.attempt == 1)
        .collect();
    assert_eq!(prior.len(), 1, "failed first attempt needs its own record");
    assert!(is_provider_failed(prior[0]));

    // Terminal failure record: ProviderFailed, no fabricated usage.
    let terminal_failed = records
        .iter()
        .filter(|r| is_provider_failed(r) && r.request_id != retried_ok.request_id)
        .find(|r| r.outcome.http_status == Some(500))
        .expect("terminal failure record carries the HTTP status");
    assert!(
        terminal_failed.outcome.usage.is_none(),
        "no fabricated usage"
    );

    // Cancellation record.
    assert!(
        records.iter().any(is_canceled),
        "cancellation must emit a Canceled record; got {records:#?}"
    );

    // ── S2: exact wire digest vs the bytes the loopback server received ──
    let sent = bodies_ok.lock().unwrap();
    assert_eq!(sent.len(), 1);
    let wire = success
        .wire
        .as_ref()
        .expect("local HTTP path must claim wire bytes");
    assert_eq!(
        wire.byte_len,
        sent[0].len() as u64,
        "exact wire byte length"
    );
    let key = load_or_create_digest_key_at(&guard.base_dir().join("trace/digest.key"))
        .expect("installation digest key");
    assert_eq!(
        wire.digest,
        keyed_digest(&key, DigestDomain::Wire, &sent[0]),
        "wire digest must be the keyed digest of the exact sent bytes"
    );
    drop(sent);

    // ── S8 export CLI: exactly one logical request incl. retries, private
    //    file, content export refused without the double opt-in ──
    let out_path = guard.home.path().join("export.jsonl");
    let out = run_export_cli(
        &guard,
        &[
            retried_ok.request_id.as_str(),
            "--metadata-only",
            "--output",
            out_path.to_str().unwrap(),
        ],
    );
    assert!(
        out.status.success(),
        "metadata export failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let exported = read_traces(&out_path);
    assert_eq!(
        exported.len(),
        2,
        "exactly the retried request's two attempts"
    );
    assert!(exported
        .iter()
        .all(|r| r.request_id == retried_ok.request_id));
    #[cfg(unix)]
    assert_eq!(mode_of(&out_path), 0o600, "metadata export must be private");
    let denied = run_export_cli(
        &guard,
        &[
            retried_ok.request_id.as_str(),
            "--include-content",
            "--output",
            guard.home.path().join("content.jsonl").to_str().unwrap(),
        ],
    );
    assert!(
        !denied.status.success(),
        "--include-content without --allow-content-export must fail closed"
    );
}

/// OpenAI Chat Completions: EVERY static registry provider ID table-driven
/// through the real `try_route` entry point against one loopback remote
/// broker (success), plus failure and cancel fixtures. Remote-broker
/// honesty: `wire` must be `None`.
///
/// Shared-transport equivalence (claim hygiene): every registry ID resolves
/// to `WireProtocol::OpenAiChatCompletions` and is served by the ONE shared
/// `stream::call_oai_stream_inner` implementation — registry entries differ
/// only in metadata (base URL / model list). Success is therefore driven
/// per registry ID (route resolution is per-ID), while failure and
/// cancellation are transport-level behaviors proven once on a
/// representative ID (`groq`); they are NOT independently re-proven per ID.
///
/// RED at d20e03f: `try_route` emitted nothing. GREEN: `6e1c3dc`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s1_openai_chat_matrix_all_provider_ids_success_failure_cancel() {
    let (endpoint, hits, _) = spawn_broker(BrokerScript::ProxySse(OAI_CHAT_SSE)).await;
    let source = remote(&endpoint);

    let keys: Vec<&str> = synaps_cli::runtime::openai::registry::providers()
        .iter()
        .map(|s| s.key)
        .collect();
    assert!(!keys.is_empty(), "provider registry must not be empty");
    for key in &keys {
        let tmp = tempfile::TempDir::new().unwrap();
        let (ctx, sink) = collecting_ctx(&tmp);
        let model = format!("{key}/fixture-model");
        let run = drive_try_route(
            &model,
            &source,
            &ctx,
            &sink,
            vec![user_msg(&format!("{S_PROMPT} hello"))],
            vec![],
            Some(format!("{S_SYSTEM} system")),
            false,
        )
        .await;
        let r = assert_remote_success(&run, "/chat/completions");
        assert_eq!(r.model.as_str(), model, "{key}");
    }
    assert_eq!(hits.load(Ordering::SeqCst), keys.len(), "loopback only");

    // Failure: broker rejects the stream. Cancellation mid-stream.
    assert_failure_run(
        &broker_route_run(
            "groq/fixture-model",
            BrokerScript::ProxyFailThen {
                fails: usize::MAX,
                status: 500,
                then: OAI_CHAT_SSE,
            },
            vec![user_msg("fail fixture")],
            vec![],
            None,
            false,
        )
        .await,
    );
    assert_cancel_run(
        &broker_route_run(
            "groq/fixture-model",
            BrokerScript::ProxyEndless("data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n"),
            vec![user_msg("cancel fixture")],
            vec![],
            None,
            true,
        )
        .await,
    );
}

/// OpenAI Responses wire (`xai-auth` Responses model, token vended by the
/// loopback broker, stream broker-proxied): success, failure, cancellation.
/// RED at d20e03f; GREEN `6e1c3dc`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s1_openai_responses_success_failure_cancel() {
    const MODEL: &str = "xai-auth/grok-4.3";

    let run = broker_route_run(
        MODEL,
        BrokerScript::ProxySse(OAI_RESPONSES_SSE),
        vec![user_msg(&format!("{S_PROMPT} hi"))],
        vec![],
        Some(S_SYSTEM.to_string()),
        false,
    )
    .await;
    assert_remote_success(&run, "/responses");

    assert_failure_run(
        &broker_route_run(
            MODEL,
            BrokerScript::ProxyFailThen {
                fails: usize::MAX,
                status: 500,
                then: OAI_RESPONSES_SSE,
            },
            vec![user_msg("fail")],
            vec![],
            None,
            false,
        )
        .await,
    );
    assert_cancel_run(
        &broker_route_run(
            MODEL,
            BrokerScript::ProxyEndless(
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"x\"}\n\n",
            ),
            vec![user_msg("cancel")],
            vec![],
            None,
            true,
        )
        .await,
    );
}

/// Gemini Code Assist wire: success, transport-internal retry (one record
/// per actual attempt), terminal failure, cancellation. RED at d20e03f;
/// GREEN `0d5c46a`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s1_gemini_success_failure_retry_cancel() {
    const MODEL: &str = "google-gemini/gemini-2.5-pro";

    let run = broker_route_run(
        MODEL,
        BrokerScript::ProxySse(GEMINI_SSE),
        vec![user_msg(&format!("{S_PROMPT} hi"))],
        vec![],
        Some(S_SYSTEM.to_string()),
        false,
    )
    .await;
    assert_remote_success(&run, "/v1internal:streamGenerateContent");

    // Transport-internal retry: broker fails once, then succeeds.
    let (endpoint, hits, _) = spawn_broker(BrokerScript::ProxyFailThen {
        fails: 1,
        status: 429,
        then: GEMINI_SSE,
    })
    .await;
    let tmp = tempfile::TempDir::new().unwrap();
    let (ctx, sink) = collecting_ctx(&tmp);
    let run = drive_try_route(
        MODEL,
        &remote(&endpoint),
        &ctx,
        &sink,
        vec![user_msg("retry")],
        vec![],
        None,
        false,
    )
    .await;
    run.result
        .as_ref()
        .expect("gemini retry fixture must recover");
    assert_eq!(hits.load(Ordering::SeqCst), 2, "one retry expected");
    assert_eq!(run.records.len(), 2, "one record per actual attempt");
    for r in &run.records {
        assert_record_conformant(r);
    }
    assert!(is_provider_failed(&run.records[0]));
    assert_eq!(run.records[1].attempt, 2);
    assert_eq!(run.records[1].outcome.retries.len(), 1);
    assert!(is_completed(&run.records[1]));
    assert_eq!(
        run.records[0].request_id, run.records[1].request_id,
        "attempts of one logical request share the request ID"
    );

    // Terminal failure and cancellation mid-stream.
    assert_failure_run(
        &broker_route_run(
            MODEL,
            BrokerScript::ProxyFailThen {
                fails: usize::MAX,
                status: 500,
                then: GEMINI_SSE,
            },
            vec![user_msg("fail")],
            vec![],
            None,
            false,
        )
        .await,
    );
    assert_cancel_run(
        &broker_route_run(
            MODEL,
            BrokerScript::ProxyEndless(
                "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"x\"}]}}]}}\n\n",
            ),
            vec![user_msg("cancel")],
            vec![],
            None,
            true,
        )
        .await,
    );
}

/// Cloud invoke through the remote broker driven through the REAL
/// `Runtime`, with ALL THREE cloud provider IDs (`azure-openai`,
/// `aws-bedrock`, `google-vertex`) success-driven through the same
/// `cloud_invoke_stream` → `/cloud/invoke` path, plus failure and
/// cancellation fixtures — `CloudProxy` records with honest `wire: None`.
///
/// Shared-transport equivalence (claim hygiene): every cloud ID dispatches
/// through the single `runtime::cloud_invoke::cloud_invoke_stream` branch
/// and the broker's one `/cloud/invoke` RPC; provider-specific hosts/auth
/// live behind the broker boundary. This transport defines NO
/// transport-internal retry loop (a retry would be a new logical request),
/// asserted below via `attempt == 1` / empty `retries` on every record.
///
/// RED at d20e03f; GREEN `0d5c46a`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn s1_cloud_invoke_success_failure_cancel_wire_none() {
    const CLOUD_MODELS: [&str; 3] = [
        "azure-openai/gpt-4o",
        "aws-bedrock/anthropic.claude-3-haiku",
        "google-vertex/gemini-2.5-pro",
    ];
    let guard = HomeGuard::new();
    std::env::set_var("SYNAPS_MACHINE_TOKEN", "machine-token-fixture");

    let mut rt = Runtime::new().await.expect("runtime");
    // Cloud routes are text-only (spec §5.5): expose no tools.
    rt.set_tools(synaps_cli::tools::ToolRegistry::empty());
    rt.set_telemetry_level(TelemetryLevel::Basic);

    // Success: every cloud provider ID through the same cloud_invoke path.
    for model in CLOUD_MODELS {
        let (endpoint, hits, _) = spawn_broker(BrokerScript::CloudLines(CLOUD_EVENTS)).await;
        std::env::set_var("SYNAPS_AUTH_ENDPOINT", &endpoint);
        rt.apply_auth_config(&synaps_cli::SynapsConfig::default());
        rt.set_model(model.to_string());
        let ev = drive_runtime_turn(&rt, &format!("{S_PROMPT} cloud"), false).await;
        assert!(turn_completed(&ev), "{model}: cloud success must complete");
        assert!(hits.load(Ordering::SeqCst) >= 1, "{model}: loopback hit");
    }

    // Failure.
    let (endpoint, _, _) = spawn_broker(BrokerScript::CloudFail).await;
    std::env::set_var("SYNAPS_AUTH_ENDPOINT", &endpoint);
    rt.apply_auth_config(&synaps_cli::SynapsConfig::default());
    rt.set_model("azure-openai/gpt-4o".to_string());
    let ev = drive_runtime_turn(&rt, "cloud failure", false).await;
    assert!(!turn_completed(&ev), "cloud failure must surface");

    // Cancellation.
    let (endpoint, _, _) = spawn_broker(BrokerScript::CloudEndless).await;
    std::env::set_var("SYNAPS_AUTH_ENDPOINT", &endpoint);
    rt.apply_auth_config(&synaps_cli::SynapsConfig::default());
    let _ = drive_runtime_turn(&rt, "cloud cancel", true).await;

    rt.shutdown_observability_async(Duration::from_secs(10))
        .await;
    let records = read_traces(&guard.trace_log());
    assert!(
        records.len() >= 5,
        "one record per cloud invocation: {records:#?}"
    );
    for r in &records {
        assert_record_conformant(r);
        assert_eq!(r.transport, TransportKind::CloudProxy);
        assert!(
            r.wire.is_none(),
            "CloudProxy must NOT claim wire bytes — serialized behind the broker"
        );
        // No transport-internal retry loop exists on the cloud path.
        assert_eq!(r.attempt, 1, "cloud transport defines no internal retry");
        assert!(r.outcome.retries.is_empty(), "{r:#?}");
    }
    for model in CLOUD_MODELS {
        assert!(
            records
                .iter()
                .any(|r| is_completed(r) && r.model.as_str() == model),
            "{model}: completed cloud record expected: {records:#?}"
        );
    }
    assert!(records.iter().any(is_provider_failed));
    assert!(records.iter().any(is_canceled), "{records:#?}");
}

/// Extension-hosted provider through the real routing gate and real python
/// sidecars. Coverage is explicit (claim hygiene): the SUCCESS record comes
/// from the repo streaming fixture; FAILURE and CANCELLATION are separately
/// driven through [`SCRIPTED_EXTENSION_PY`] — a real sidecar whose
/// `provider.stream` either returns a JSON-RPC error or stalls mid-stream —
/// so each terminal class is genuinely produced by the extension transport
/// (the success test alone proves nothing about failure/cancel). All
/// records: `Extension` transport, honest `wire: None`. A request rejected
/// by the gate (unknown provider) emits NO attempt record.
/// RED at d20e03f; GREEN `2831ec5`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn s1_extension_provider_success_failure_cancel_and_gate_honesty() {
    let _guard = HomeGuard::new(); // base-dir isolation for extension state
    let (manager, _plugin_dir) = load_extension_fixture("ph2-ext").await;

    // Success.
    let tmp = tempfile::TempDir::new().unwrap();
    let (ctx, sink) = collecting_ctx(&tmp);
    let run = drive_try_route(
        "ph2-ext:stream-echo:stream-echo-mini",
        &CredentialSource::Local,
        &ctx,
        &sink,
        vec![Arc::new(serde_json::json!({
            "role": "user",
            "content": [{"type": "text", "text": format!("{S_PROMPT} hi")}]
        }))],
        vec![],
        None,
        false,
    )
    .await;
    run.result.as_ref().expect("extension success fixture");
    assert_eq!(run.records.len(), 1);
    let r = &run.records[0];
    assert_record_conformant(r);
    assert_eq!(r.transport, TransportKind::Extension);
    assert!(is_completed(r));
    assert!(
        r.wire.is_none(),
        "extension transport must NOT claim wire bytes — the sidecar owns the wire"
    );

    // Gate honesty: an unregistered PROVIDER is rejected BEFORE any sidecar
    // attempt — no attempt happened, so no attempt record.
    let tmp = tempfile::TempDir::new().unwrap();
    let (ctx, sink) = collecting_ctx(&tmp);
    let run = drive_try_route(
        "ph2-ext:no-such-provider:some-model",
        &CredentialSource::Local,
        &ctx,
        &sink,
        vec![user_msg("fail")],
        vec![],
        None,
        false,
    )
    .await;
    assert!(
        run.result.is_err(),
        "unregistered extension provider must fail"
    );
    assert!(
        run.records.iter().all(|r| {
            assert_record_conformant(r);
            true
        }),
        "any emitted record must still be conformant"
    );

    manager.write().await.shutdown_all().await;
    synaps_cli::runtime::openai::clear_extension_manager_for_routing();

    // Failure: a real sidecar answers `provider.stream` with a JSON-RPC
    // error → one conformant `ProviderFailed` Extension record. A fresh
    // sidecar per terminal class keeps the transport state clean (the
    // production runtime restart-retries a failed call, which would leave
    // a dirty transport for the next fixture).
    let (manager, _plugin_dir) = load_scripted_extension_fixture("ph2-ext-fail").await;
    let tmp = tempfile::TempDir::new().unwrap();
    let (ctx, sink) = collecting_ctx(&tmp);
    let run = drive_try_route(
        "ph2-ext-fail:scripted:scripted-mini",
        &CredentialSource::Local,
        &ctx,
        &sink,
        vec![user_msg("PH2-FAIL please")],
        vec![],
        None,
        false,
    )
    .await;
    assert_failure_run(&run);
    assert_eq!(run.records.len(), 1);
    assert_eq!(run.records[0].transport, TransportKind::Extension);
    assert!(run.records[0].wire.is_none());
    manager.write().await.shutdown_all().await;
    synaps_cli::runtime::openai::clear_extension_manager_for_routing();

    // Cancellation: the sidecar streams one delta then stalls; the driver
    // cancels after the first text → one conformant `Canceled` record.
    let (manager, _plugin_dir) = load_scripted_extension_fixture("ph2-ext-cancel").await;
    let tmp = tempfile::TempDir::new().unwrap();
    let (ctx, sink) = collecting_ctx(&tmp);
    let run = drive_try_route(
        "ph2-ext-cancel:scripted:scripted-mini",
        &CredentialSource::Local,
        &ctx,
        &sink,
        vec![user_msg("PH2-STALL please")],
        vec![],
        None,
        true,
    )
    .await;
    assert_cancel_run(&run);
    assert_eq!(run.records.len(), 1);
    assert_eq!(run.records[0].transport, TransportKind::Extension);
    assert!(run.records[0].wire.is_none());

    manager.write().await.shutdown_all().await;
    synaps_cli::runtime::openai::clear_extension_manager_for_routing();
}

/// Strict-reader table test: one handcrafted record per `TransportKind`
/// (incl. `CloudProxy`, `Extension`, and the Responses tag Codex records
/// share) parses through the production `RequestTrace` reader, and the
/// reader FAILS CLOSED on a foreign schema tag and a malformed ID. This is
/// the "all provider IDs may table-test" surface for pinned-endpoint
/// families (Codex). RED at d20e03f (no reader); GREEN `073a7b7`.
#[test]
fn s1_transport_kind_table_strict_reader_accepts_all_and_fails_closed() {
    let kinds = [
        TransportKind::AnthropicMessages,
        TransportKind::OpenAiChatCompletions,
        TransportKind::OpenAiResponses,
        TransportKind::GeminiGenerateContent,
        TransportKind::VertexGenerateContent,
        TransportKind::CloudProxy,
        TransportKind::Extension,
    ];
    let record_with = |kind: TransportKind, session_id: &str| {
        serde_json::json!({
            "schema": "synaps-request-trace/1",
            "session_id": session_id,
            "turn_id": "turn-t13",
            "request_id": "req-t13",
            "attempt": 1,
            "model": "provider/fixture-model",
            "transport": serde_json::to_value(kind).unwrap(),
            "endpoint": {"host": "127.0.0.1", "path": "/fixture"},
            "anatomy": {
                "system_segment_count": 0, "message_count": 1,
                "block_count": 1, "tool_count": 0
            },
            "system_segments": [],
            "messages": [],
            "tools": [],
            "cache": {"boundaries": []},
            "translation_losses": [],
            "outcome": {"timings": {}, "retries": [], "terminal": {"kind": "completed"}}
        })
    };
    for kind in kinds {
        let parsed: RequestTrace = serde_json::from_value(record_with(kind, "session-t13"))
            .unwrap_or_else(|e| panic!("transport {kind:?} must be schema-valid: {e}"));
        assert_record_conformant(&parsed);
    }
    // Fail closed: a foreign schema tag on an otherwise-valid record, and a
    // malformed ID as the ONLY defect of an otherwise-valid record (so the
    // rejection is attributable to ID validation, not missing fields).
    let mut foreign = record_with(TransportKind::AnthropicMessages, "session-t13");
    foreign["schema"] = serde_json::json!("synaps-request-trace/2");
    assert!(
        serde_json::from_value::<RequestTrace>(foreign).is_err(),
        "foreign schema tag must fail closed"
    );
    let bad_id = record_with(TransportKind::AnthropicMessages, "has whitespace");
    assert!(
        serde_json::from_value::<RequestTrace>(bad_id).is_err(),
        "malformed session ID must fail closed even when every other field is valid"
    );
}
