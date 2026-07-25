//! Tests for the `synaps-request-trace/1` envelope, key handling, and
//! validated metadata identifiers.

use super::*;
use agent_core::TurnOutcome;
use std::path::Path;

/// Sentinel that must never appear in serialized traces. Split at const
/// time so the test source itself can't produce a false positive.
fn sentinel() -> String {
    format!("{}{}", "RAW-CONTENT-", "SENTINEL-7f3a9c")
}

fn digest_of(key: &TraceDigestKey, domain: DigestDomain, s: &str) -> ComponentDigest {
    keyed_digest(key, domain, s.as_bytes())
}

fn test_key(dir: &Path, name: &str) -> TraceDigestKey {
    load_or_create_digest_key_at(&dir.join(name)).expect("key creation")
}

fn id(s: &str) -> TraceId {
    TraceId::new(s).expect("valid trace id")
}

/// Build a fully-populated envelope from sentinel "content" — every
/// content-derived value must reduce to counts/bytes/digests/bounded IDs.
fn sample_trace(key: &TraceDigestKey) -> RequestTrace {
    let system_text = format!("You are helpful. {}", sentinel());
    let tool_schema = format!("{{\"type\":\"object\",\"note\":\"{}\"}}", sentinel());
    let wire_body = format!("{{\"messages\":[\"{}\"]}}", sentinel());
    RequestTrace {
        schema: TraceSchemaVersion,
        session_id: id("sess-01"),
        turn_id: id("turn-01"),
        request_id: id("req-01"),
        execution_events: Vec::new(),
        attempt: 2,
        model: agent_core::prompt::QualifiedModelId::parse("anthropic/claude-sonnet-4-6").unwrap(),
        transport: TransportKind::AnthropicMessages,
        endpoint: EndpointMeta::new("api.anthropic.com", "/v1/messages").unwrap(),
        anatomy: RequestAnatomy {
            system_segment_count: 1,
            message_count: 2,
            block_count: 3,
            tool_count: 1,
        },
        wire: Some(WireMeta {
            byte_len: wire_body.len() as u64,
            digest: digest_of(key, DigestDomain::Wire, &wire_body),
        }),
        system_segments: vec![SystemSegmentMeta {
            kind: SystemSegmentKind::Primary,
            byte_len: system_text.len() as u64,
            digest: digest_of(key, DigestDomain::SystemSegment, &system_text),
        }],
        messages: vec![
            MessageMeta {
                role: MessageRole::User,
                blocks: vec![BlockMeta {
                    kind: BlockKind::Text,
                    byte_len: sentinel().len() as u64,
                }],
            },
            MessageMeta {
                role: MessageRole::Assistant,
                blocks: vec![
                    BlockMeta {
                        kind: BlockKind::Thinking,
                        byte_len: 128,
                    },
                    BlockMeta {
                        kind: BlockKind::ToolUse,
                        byte_len: 64,
                    },
                ],
            },
        ],
        tools: vec![ToolMeta {
            stable_id: id("tool.bash"),
            wire_name: WireName::new("bash").unwrap(),
            schema_byte_len: tool_schema.len() as u64,
            schema_digest: digest_of(key, DigestDomain::ToolSchema, &tool_schema),
        }],
        cache: CacheMeta {
            boundaries: vec![CacheBoundaryMeta {
                location: CacheBoundaryLocation::System,
                index: 0,
                ttl: CacheTtlClass::FiveMinutes,
            }],
            tools_prefix: Some(PrefixMeta {
                byte_len: tool_schema.len() as u64,
                digest: digest_of(key, DigestDomain::ToolsPrefix, &tool_schema),
            }),
            system_prefix: None,
            history_tail: None,
            delta: None,
        },
        translation_losses: vec![TranslationLoss {
            action: TranslationAction::Downgraded,
            element: TranslationElement::MessageBlock,
            element_id: Some(id("messages[1].blocks[0]")),
        }],
        outcome: TransportOutcome {
            timings: TimingStages {
                send_start_unix_ms: Some(1_760_000_000_000),
                headers_ms: Some(120),
                first_byte_ms: Some(140),
                first_model_event_ms: None,
                stream_end_ms: Some(2_400),
            },
            retries: vec![RetryMeta {
                attempt: 1,
                class: RetryClass::Overloaded,
                delay_ms: 500,
            }],
            provider_request_id: Some(id("req_abc123")),
            http_status: Some(200),
            stop_reason: Some(StopReason::EndTurn),
            usage: Some(UsageMeta {
                provenance: UsageProvenance::ProviderReported,
                input_tokens: Some(1200),
                output_tokens: Some(340),
                cache_read_tokens: None,
                cache_write_tokens: Some(900),
            }),
            terminal: TurnOutcome::Completed,
        },
    }
}

#[test]
fn envelope_serializes_deterministically_and_round_trips() {
    let dir = tempfile::TempDir::new().unwrap();
    let key = test_key(dir.path(), "k1");
    let trace = sample_trace(&key);

    let a = serde_json::to_string(&trace).unwrap();
    let b = serde_json::to_string(&trace).unwrap();
    assert_eq!(a, b, "serialization must be deterministic");

    // Version tag is present and exact.
    let v: serde_json::Value = serde_json::from_str(&a).unwrap();
    assert_eq!(v["schema"], TRACE_SCHEMA);
    // Required top-level fields exist.
    let required = "session_id turn_id request_id attempt model transport endpoint anatomy \
                    system_segments messages tools cache translation_losses outcome";
    for field in required.split_whitespace() {
        assert!(!v[field].is_null(), "missing required field {field}");
    }
    // Validated IDs serialize as plain JSON strings, not wrapper objects.
    assert!(v["session_id"].is_string());
    assert!(v["tools"][0]["stable_id"].is_string());
    assert!(v["tools"][0]["wire_name"].is_string());
    // Endpoint keeps the `{host, path}` shape.
    assert_eq!(v["endpoint"]["host"], "api.anthropic.com");
    assert_eq!(v["endpoint"]["path"], "/v1/messages");
    // Documented attempt/retries rule: attempt == retries.len() + 1.
    assert_eq!(
        trace.attempt as usize,
        trace.outcome.retries.len() + 1,
        "attempt must equal transport tries (retries + final)"
    );

    let back: RequestTrace = serde_json::from_str(&a).unwrap();
    assert_eq!(back, trace, "round trip must be lossless");
    assert_eq!(back.provider(), "anthropic");

    // Any other schema tag is rejected on read.
    let mut v = v;
    v["schema"] = serde_json::Value::String("synaps-request-trace/2".into());
    assert!(serde_json::from_value::<RequestTrace>(v).is_err());
}

#[test]
fn unknown_metrics_are_absent_not_zero_and_round_trip_as_none() {
    let outcome = TransportOutcome::unobserved(TurnOutcome::Canceled);
    let json = serde_json::to_value(&outcome).unwrap();

    // Absent — not serialized as 0 or null.
    let timings = json["timings"].as_object().unwrap();
    assert!(timings.is_empty(), "unobserved timings serialize empty");
    for field in ["provider_request_id", "http_status", "stop_reason", "usage"] {
        assert!(
            json.as_object().unwrap().get(field).is_none(),
            "{field} must be absent when unknown"
        );
    }

    // Absent keys round-trip to None.
    let back: TransportOutcome = serde_json::from_value(json).unwrap();
    assert_eq!(back, outcome);
    assert_eq!(back.timings.first_byte_ms, None);
    assert_eq!(back.usage, None);

    // Explicit nulls also deserialize to None (lenient read contract).
    let with_nulls = serde_json::json!({
        "timings": { "headers_ms": null },
        "retries": [],
        "http_status": null,
        "terminal": { "kind": "canceled" },
    });
    let back: TransportOutcome = serde_json::from_value(with_nulls).unwrap();
    assert_eq!(back, outcome);
}

#[test]
fn digests_are_keyed_and_deterministic() {
    let dir = tempfile::TempDir::new().unwrap();
    let key_a = test_key(dir.path(), "ka");
    let key_b = test_key(dir.path(), "kb");
    let input = b"identical component bytes";

    let d1 = keyed_digest(&key_a, DigestDomain::SystemSegment, input);
    let d2 = keyed_digest(&key_a, DigestDomain::SystemSegment, input);
    assert_eq!(d1, d2, "same key + same input => same digest");

    let d3 = keyed_digest(&key_b, DigestDomain::SystemSegment, input);
    assert_ne!(d1, d3, "different key => different digest");

    // Domain separation: same key + same bytes, different domain.
    let d4 = keyed_digest(&key_a, DigestDomain::ToolSchema, input);
    assert_ne!(d1, d4, "different domain => different digest");

    // Well-formed hex round-trips through the validated newtype.
    let s: String = d1.clone().into();
    assert_eq!(s.len(), 64);
    assert_eq!(ComponentDigest::try_from(s).unwrap(), d1);
    assert!(ComponentDigest::try_from("not-hex".to_string()).is_err());
    assert!(
        ComponentDigest::try_from("AB".repeat(32)).is_err(),
        "uppercase rejected"
    );
}

// --- B1: digest-key file hardening ---

#[cfg(unix)]
#[test]
#[serial_test::serial(umask)]
fn key_file_is_0600_and_parent_0700_under_permissive_umask() {
    use std::os::unix::fs::PermissionsExt as _;
    // Process-global umask: hold the shared serial key (see private_fs).
    let old = unsafe { libc::umask(0) };
    let result = std::panic::catch_unwind(|| {
        let dir = tempfile::TempDir::new().unwrap();
        let parent = dir.path().join("trace");
        let path = parent.join("digest.key");
        let _key = load_or_create_digest_key_at(&path).unwrap();
        let file_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        let dir_mode = std::fs::metadata(&parent).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600, "key file must be 0600");
        assert_eq!(dir_mode, 0o700, "key parent dir must be 0700");
    });
    unsafe { libc::umask(old) };
    result.unwrap();
}

#[cfg(unix)]
#[test]
fn symlink_at_key_target_fails_safely() {
    let dir = tempfile::TempDir::new().unwrap();
    let victim = dir.path().join("victim");
    std::fs::write(&victim, b"do not touch").unwrap();
    let path = dir.path().join("digest.key");
    std::os::unix::fs::symlink(&victim, &path).unwrap();

    let err = load_or_create_digest_key_at(&path).unwrap_err();
    assert!(
        matches!(err, TraceKeyError::SymlinkRefused(_)),
        "expected SymlinkRefused, got {err:?}"
    );
    // The symlink target is untouched.
    assert_eq!(std::fs::read(&victim).unwrap(), b"do not touch");
}

#[cfg(unix)]
#[test]
fn preexisting_broad_mode_key_is_repaired_to_0600() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("digest.key");
    std::fs::write(&path, [0x42u8; 32]).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

    let key = load_or_create_digest_key_at(&path).unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o600,
        "broad pre-existing key must be repaired to 0600"
    );
    // The existing key bytes are used, not regenerated.
    let d1 = keyed_digest(&key, DigestDomain::Wire, b"probe");
    let key2 = load_or_create_digest_key_at(&path).unwrap();
    assert_eq!(d1, keyed_digest(&key2, DigestDomain::Wire, b"probe"));
}

#[cfg(unix)]
#[test]
fn fifo_at_key_path_fails_promptly_with_typed_error() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("digest.key");
    let c_path = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);

    let start = std::time::Instant::now();
    let err = load_or_create_digest_key_at(&path).unwrap_err();
    assert!(
        start.elapsed() < std::time::Duration::from_secs(2),
        "FIFO at key path must fail promptly, never block"
    );
    assert!(
        matches!(err, TraceKeyError::NotRegularFile(_)),
        "expected NotRegularFile, got {err:?}"
    );
}

#[test]
fn directory_at_key_path_fails_with_typed_error() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("digest.key");
    std::fs::create_dir(&path).unwrap();

    let err = load_or_create_digest_key_at(&path).unwrap_err();
    assert!(
        matches!(err, TraceKeyError::NotRegularFile(_)),
        "expected NotRegularFile, got {err:?}"
    );
}

#[test]
fn wrong_size_and_oversized_key_files_are_corrupt() {
    for bytes in [16usize, 33, 4096] {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("digest.key");
        std::fs::write(&path, vec![0x41u8; bytes]).unwrap();
        let err = load_or_create_digest_key_at(&path).unwrap_err();
        assert!(
            matches!(err, TraceKeyError::Corrupt(_)),
            "{bytes}-byte key file must be Corrupt, got {err:?}"
        );
    }
}

#[test]
fn concurrent_key_creation_converges_on_one_key() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("trace").join("digest.key");
    let digests: Vec<ComponentDigest> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                s.spawn(move || {
                    let key = load_or_create_digest_key_at(&path).unwrap();
                    keyed_digest(&key, DigestDomain::Wire, b"probe")
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert!(
        digests.iter().all(|d| *d == digests[0]),
        "all concurrent loaders must observe the same key"
    );
    // And a fresh sequential load agrees too.
    let key = load_or_create_digest_key_at(&path).unwrap();
    assert_eq!(keyed_digest(&key, DigestDomain::Wire, b"probe"), digests[0]);
}

// --- B2: validated endpoint ---

#[test]
fn endpoint_rejects_query_userinfo_fragment_and_control() {
    // Query string smuggling an API key.
    assert!(EndpointMeta::new("api.example.com", "/v1/messages?api_key=sk-secret").is_err());
    assert!(EndpointMeta::new("api.example.com?x=1", "/v1").is_err());
    // Userinfo in host.
    assert!(EndpointMeta::new("user:pw@api.example.com", "/v1").is_err());
    assert!(EndpointMeta::new("token@api.example.com", "/v1").is_err());
    // Fragments.
    assert!(EndpointMeta::new("api.example.com", "/v1#frag").is_err());
    assert!(EndpointMeta::new("api.example.com#f", "/v1").is_err());
    // CRLF / control / whitespace (header-injection shapes).
    assert!(EndpointMeta::new("api.example.com\r\nX-Evil: 1", "/v1").is_err());
    assert!(EndpointMeta::new("api.example.com", "/v1\r\nX-Evil: 1").is_err());
    assert!(EndpointMeta::new("api.example.com", "/v1 messages").is_err());
    assert!(EndpointMeta::new("api example.com", "/v1").is_err());
    assert!(EndpointMeta::new("api.example.com", "/v1\0").is_err());
    // Empty / shape violations.
    assert!(EndpointMeta::new("", "/v1").is_err());
    assert!(EndpointMeta::new("api.example.com", "").is_err());
    assert!(EndpointMeta::new("api.example.com", "v1/messages").is_err());
    assert!(EndpointMeta::new("api.example.com/v1", "/x").is_err());
    // Bad port / stray colons.
    assert!(EndpointMeta::new("api.example.com:notaport", "/v1").is_err());
    assert!(EndpointMeta::new("api.example.com:80:80", "/v1").is_err());
    assert!(EndpointMeta::new("api.example.com:", "/v1").is_err());
}

#[test]
fn endpoint_accepts_normal_and_loopback_hosts() {
    let e = EndpointMeta::new("api.anthropic.com", "/v1/messages").unwrap();
    assert_eq!(e.host(), "api.anthropic.com");
    assert_eq!(e.path(), "/v1/messages");

    let e = EndpointMeta::new("127.0.0.1:8080", "/v1/messages").unwrap();
    assert_eq!(e.host(), "127.0.0.1:8080");

    let e = EndpointMeta::new("[::1]:8080", "/v1").unwrap();
    assert_eq!(e.host(), "[::1]:8080");
    assert!(EndpointMeta::new("[::1]", "/v1").is_ok());
}

#[test]
fn endpoint_serde_keeps_shape_and_validates_on_read() {
    let e = EndpointMeta::new("api.anthropic.com", "/v1/messages").unwrap();
    let v = serde_json::to_value(&e).unwrap();
    assert_eq!(
        v,
        serde_json::json!({"host": "api.anthropic.com", "path": "/v1/messages"})
    );
    let back: EndpointMeta = serde_json::from_value(v).unwrap();
    assert_eq!(back, e);

    // Hostile payloads are rejected at deserialization too.
    for bad in [
        serde_json::json!({"host": "api.example.com", "path": "/v1?api_key=sk"}),
        serde_json::json!({"host": "u@api.example.com", "path": "/v1"}),
        serde_json::json!({"host": "api.example.com", "path": "/v1\r\nX: 1"}),
        serde_json::json!({"host": {"h": "x"}, "path": "/v1"}),
        serde_json::json!("api.example.com/v1"),
    ] {
        assert!(
            serde_json::from_value::<EndpointMeta>(bad.clone()).is_err(),
            "must reject {bad}"
        );
    }
}

// --- B3: validated bounded IDs ---

#[test]
fn trace_ids_are_bounded_and_safe_alphabet() {
    // Realistic IDs and positional paths are accepted.
    for good in [
        "sess-01",
        "req_abc123",
        "tool.bash",
        "messages[3].blocks[1]",
        "anthropic/claude:x",
        "a",
    ] {
        assert!(TraceId::new(good).is_ok(), "must accept {good}");
    }
    assert!(TraceId::new("x".repeat(TRACE_ID_MAX_BYTES)).is_ok());

    // Content-shaped or unbounded values are rejected.
    for bad in [
        "".to_string(),
        "two words".to_string(),
        "line1\nline2".to_string(),
        "tab\there".to_string(),
        "quo\"te".to_string(),
        "back\\slash".to_string(),
        "sneaky'quote".to_string(),
        "ctrl\u{7}".to_string(),
        "unicode-é".to_string(),
        "x".repeat(TRACE_ID_MAX_BYTES + 1),
        "y".repeat(10 * 1024),
    ] {
        assert!(TraceId::new(bad.clone()).is_err(), "must reject {bad:?}");
    }
}

#[test]
fn wire_names_are_stricter_than_trace_ids() {
    for good in ["bash", "web_search", "run-tests", "T0"] {
        assert!(WireName::new(good).is_ok(), "must accept {good}");
    }
    for bad in [
        "".to_string(),
        "rm -rf /".to_string(),
        "tool.bash".to_string(), // dots reserved for stable IDs
        "a/b".to_string(),
        "name\n".to_string(),
        "x".repeat(WIRE_NAME_MAX_BYTES + 1),
    ] {
        assert!(WireName::new(bad.clone()).is_err(), "must reject {bad:?}");
    }
}

#[test]
fn hostile_id_payloads_are_rejected_recursively_on_deserialization() {
    let dir = tempfile::TempDir::new().unwrap();
    let key = test_key(dir.path(), "k1");
    let good = serde_json::to_value(sample_trace(&key)).unwrap();
    // Sanity: the untampered record parses.
    assert!(serde_json::from_value::<RequestTrace>(good.clone()).is_ok());

    let huge = "A".repeat(10 * 1024);
    let mutations: Vec<(&str, serde_json::Value)> = vec![
        ("/session_id", serde_json::json!(huge.clone())),
        ("/turn_id", serde_json::json!("two words")),
        ("/request_id", serde_json::json!("evil\"quote")),
        ("/tools/0/stable_id", serde_json::json!("line1\nline2")),
        ("/tools/0/wire_name", serde_json::json!("rm -rf /")),
        (
            "/translation_losses/0/element_id",
            serde_json::json!("prompt text leaked here"),
        ),
        (
            "/outcome/provider_request_id",
            serde_json::json!(huge.clone()),
        ),
    ];
    for (pointer, value) in mutations {
        let mut v = good.clone();
        *v.pointer_mut(pointer).unwrap() = value;
        assert!(
            serde_json::from_value::<RequestTrace>(v).is_err(),
            "tampered {pointer} must be rejected"
        );
    }
}

#[test]
fn no_raw_content_can_reach_a_serialized_trace() {
    let dir = tempfile::TempDir::new().unwrap();
    let key = test_key(dir.path(), "k1");
    // sample_trace derives every field from sentinel-bearing "content";
    // since no field accepts content, the sentinel cannot appear.
    let trace = sample_trace(&key);
    let json = serde_json::to_value(&trace).unwrap();

    fn scan(v: &serde_json::Value, needle: &str) {
        match v {
            serde_json::Value::String(s) => {
                assert!(!s.contains(needle), "sentinel leaked into trace: {s}")
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    scan(item, needle);
                }
            }
            serde_json::Value::Object(map) => {
                for (k, item) in map {
                    assert!(!k.contains(needle), "sentinel leaked into key: {k}");
                    scan(item, needle);
                }
            }
            _ => {}
        }
    }
    scan(&json, &sentinel());
}

#[test]
fn terminal_turn_outcome_round_trips_in_envelope() {
    let dir = tempfile::TempDir::new().unwrap();
    let key = test_key(dir.path(), "k1");
    let corr = "turn-9-1".to_string();
    let outcomes = [
        TurnOutcome::Completed,
        TurnOutcome::Canceled,
        TurnOutcome::ProviderFailed {
            code: "overloaded".into(),
            correlation_id: corr.clone(),
        },
        TurnOutcome::ToolFailed {
            tool_id: "toolu_x".into(),
            correlation_id: corr,
        },
        TurnOutcome::BudgetExceeded {
            dimension: agent_core::BudgetDimension::WallClock,
        },
        TurnOutcome::InterruptedAfterSideEffect {
            call_id: "toolu_y".into(),
        },
    ];
    for outcome in outcomes {
        let mut trace = sample_trace(&key);
        trace.outcome.terminal = outcome.clone();
        let json = serde_json::to_string(&trace).unwrap();
        let back: RequestTrace = serde_json::from_str(&json).unwrap();
        assert_eq!(back.outcome.terminal, outcome);
    }
}

/// Task 12 fix 4: forking an armed ephemeral context preserves the session
/// cache snapshot store, so `/context` sees the armed request's §6.6
/// diagnostics (and the session identity/counters stay shared).
#[test]
fn forked_context_shares_session_cache_snapshot_store() {
    let dir = tempfile::tempdir().unwrap();
    let key = test_key(dir.path(), "fork-key");
    let base = TraceContext::with_sink(CollectingTraceSink::new());
    let fork = base.fork_with_sink(CollectingTraceSink::new());
    let msgs: Vec<crate::SharedMessage> = vec![std::sync::Arc::new(serde_json::json!({
        "role": "user",
        "content": "hi",
    }))];
    fork.cache_snapshots()
        .compare_and_update(Some(&key), &[], Some("sys"), &msgs);
    assert!(
        base.cache_snapshots().last_activity().is_some(),
        "armed fork must feed the same /context diagnostics store"
    );
}

/// B1: the one-shot request gate is consumed by the first
/// `RequestTracer::begin`; that request's retry attempts all emit, and a
/// second logical request through the same context begins nothing.
#[test]
fn one_shot_gate_admits_one_tracer_with_all_its_attempts() {
    let sink = CollectingTraceSink::new();
    let ctx = TraceContext::with_sink(sink.clone()).with_one_shot_request_gate();
    let model = agent_core::prompt::QualifiedModelId::parse("anthropic/claude-test").unwrap();
    let endpoint = EndpointMeta::new("api.anthropic.com", "/v1/messages").unwrap();

    let mut first = RequestTracer::begin(
        &ctx,
        None,
        model.clone(),
        TransportKind::AnthropicMessages,
        endpoint.clone(),
        RequestStructure::default(),
    )
    .expect("first logical request wins the gate");
    // Retry attempt then final attempt: both emit.
    first.attempt_failed(
        AttemptClock::start(),
        RetryClass::RateLimited,
        std::time::Duration::from_millis(1),
        Some(429),
        None,
        "http_429",
    );
    first.finish(
        AttemptClock::start(),
        Some(200),
        None,
        Some(StopReason::EndTurn),
        None,
        TurnOutcome::Completed,
    );

    assert!(
        RequestTracer::begin(
            &ctx,
            None,
            model,
            TransportKind::AnthropicMessages,
            endpoint,
            RequestStructure::default(),
        )
        .is_none(),
        "second logical request must not trace"
    );
    let records = sink.records();
    assert_eq!(records.len(), 2, "both attempts of the first request emit");
    assert_eq!(records[0].request_id, records[1].request_id);
    assert!(!ctx.enabled(), "consumed gate disables the context");
}
