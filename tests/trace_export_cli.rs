//! Task 12 integration tests for `synaps trace export`.
//!
//! Spawns the real `synaps` binary with an isolated HOME and asserts:
//! - metadata export selects exactly the requested turn/request ID, writes
//!   a schema-valid JSONL file with mode `0600`, and refuses unsafe targets;
//! - content export fails closed without `--allow-content-export` and
//!   without a capture bundle;
//! - a seeded (redacted) capture bundle exports under the content-export
//!   schema, sentinel secrets never surface, and the bundle is consumed.

use std::path::Path;
use std::process::Command;

fn run_trace(home: &Path, args: &[&str]) -> std::process::Output {
    let bin = env!("CARGO_BIN_EXE_synaps");
    Command::new(bin)
        .arg("trace")
        .args(args)
        .env("HOME", home)
        .env_remove("ANTHROPIC_API_KEY")
        .output()
        .expect("failed to spawn synaps binary")
}

/// A minimal schema-valid `synaps-request-trace/1` record.
fn sample_record(turn: &str, request: &str) -> serde_json::Value {
    serde_json::json!({
        "schema": "synaps-request-trace/1",
        "session_id": "session-t12",
        "turn_id": turn,
        "request_id": request,
        "attempt": 1,
        "model": "anthropic/claude-test",
        "transport": "anthropic_messages",
        "endpoint": {"host": "api.anthropic.com", "path": "/v1/messages"},
        "anatomy": {
            "system_segment_count": 0, "message_count": 0,
            "block_count": 0, "tool_count": 0
        },
        "system_segments": [],
        "messages": [],
        "tools": [],
        "cache": {"boundaries": []},
        "translation_losses": [],
        "outcome": {
            "timings": {},
            "retries": [],
            "terminal": {"kind": "completed"}
        }
    })
}

fn seed_trace_log(home: &Path, records: &[serde_json::Value]) {
    let dir = home.join(".cache/synaps");
    std::fs::create_dir_all(&dir).unwrap();
    let lines: Vec<String> = records.iter().map(|r| r.to_string()).collect();
    std::fs::write(dir.join("request-trace.jsonl"), lines.join("\n") + "\n").unwrap();
}

/// The terminal outcome tag must match the engine's `TurnOutcome` serde
/// shape; probe it once so the fixture stays honest.
fn terminal_shape_is_valid(home: &Path) -> bool {
    let out = run_trace(
        home,
        &[
            "export",
            "turn-probe",
            "--metadata-only",
            "--output",
            home.join("probe.jsonl").to_str().unwrap(),
        ],
    );
    // NotFound (id absent) means every line VALIDATED; an InvalidRecord
    // error would mean the fixture shape is wrong.
    String::from_utf8_lossy(&out.stderr).contains("no trace record matches")
}

#[test]
fn metadata_export_writes_private_schema_valid_selection() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    seed_trace_log(
        home,
        &[
            sample_record("turn-1", "req-1"),
            sample_record("turn-2", "req-2"),
            sample_record("turn-2", "req-3"),
        ],
    );
    assert!(
        terminal_shape_is_valid(home),
        "fixture records must validate as RequestTrace"
    );

    let out_path = home.join("export/turn2.jsonl");
    let out = run_trace(
        home,
        &[
            "export",
            "turn-2",
            "--metadata-only",
            "--output",
            out_path.to_str().unwrap(),
        ],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let data = std::fs::read_to_string(&out_path).unwrap();
    let lines: Vec<&str> = data.lines().collect();
    assert_eq!(lines.len(), 2, "exactly the two turn-2 records");
    for line in lines {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["schema"], "synaps-request-trace/1");
        assert_eq!(v["turn_id"], "turn-2");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&out_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "export file must be 0600");
    }

    // Existing target refused on a second run.
    let again = run_trace(
        home,
        &[
            "export",
            "turn-2",
            "--metadata-only",
            "--output",
            out_path.to_str().unwrap(),
        ],
    );
    assert!(!again.status.success(), "must refuse an existing target");
}

#[test]
fn content_export_fails_closed_without_opt_in_or_capture() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    seed_trace_log(home, &[sample_record("turn-1", "req-1")]);

    // --include-content without --allow-content-export: refused, no output.
    let out_path = home.join("content.json");
    let out = run_trace(
        home,
        &[
            "export",
            "req-1",
            "--include-content",
            "--output",
            out_path.to_str().unwrap(),
        ],
    );
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--allow-content-export"));
    assert!(!out_path.exists());

    // Both flags but no capture bundle: fail closed with guidance.
    let out = run_trace(
        home,
        &[
            "export",
            "req-1",
            "--include-content",
            "--allow-content-export",
            "--output",
            out_path.to_str().unwrap(),
        ],
    );
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("/trace next content"));
    assert!(!out_path.exists());
}

#[test]
fn content_export_consumes_capture_and_redacts_sentinels() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let secret = "sk-CLI-SENTINEL-0123456789abcdef";

    // Seed a capture bundle the way `/trace next content` writes it
    // (already-redacted body; here we plant a *raw* sentinel to prove the
    // export-time re-redaction pass also fires).
    let capture_dir = home.join(".synaps-cli/trace/capture");
    std::fs::create_dir_all(&capture_dir).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let bundle = serde_json::json!({
        "schema": "synaps-trace-content-capture/1",
        "request_id": "req-cap",
        "created_unix_ms": now,
        "expires_unix_ms": now + 60_000,
        "redacted": true,
        "over_budget": false,
        "body": {
            "messages": [{"role": "user", "content": format!("please use {secret}")}],
            "api_key": secret
        }
    });
    std::fs::write(
        capture_dir.join("capture-req-cap.json"),
        serde_json::to_vec(&bundle).unwrap(),
    )
    .unwrap();

    let out_path = home.join("content.json");
    let out = run_trace(
        home,
        &[
            "export",
            "req-cap",
            "--include-content",
            "--allow-content-export",
            "--output",
            out_path.to_str().unwrap(),
        ],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Warning surfaced on stderr.
    assert!(String::from_utf8_lossy(&out.stderr).contains("WARNING"));

    let data = std::fs::read_to_string(&out_path).unwrap();
    assert!(!data.contains(secret), "raw sentinel secret exported");
    let export: serde_json::Value = serde_json::from_str(&data).unwrap();
    assert_eq!(export["schema"], "synaps-trace-content-export/1");
    assert_eq!(export["redacted"], true);

    // Capture consumed: second export fails.
    assert!(!capture_dir.join("capture-req-cap.json").exists());
    let again = run_trace(
        home,
        &[
            "export",
            "req-cap",
            "--include-content",
            "--allow-content-export",
            "--output",
            home.join("again.json").to_str().unwrap(),
        ],
    );
    assert!(!again.status.success());
}
