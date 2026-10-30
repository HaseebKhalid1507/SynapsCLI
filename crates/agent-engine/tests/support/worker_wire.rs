//! A loopback Anthropic provider for subagent workers, and what every
//! worker request must look like under the locked-down config of
//! [`LOCKED_CONFIG`]. Crate-agnostic: compiled into the engine's unit tests
//! (no-host spawns, `#[path]` from `tools/subagent/mod.rs`) and into
//! `tests/worker_config_inherit.rs` (host spawns).
#![allow(dead_code)]

use std::io::Read;
use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::response::IntoResponse;
use serde_json::{json, Value};

/// The parent's config: three built-ins disabled, cache diagnostics on,
/// gzip forced on at level 9 (the loopback URL is not the Anthropic API, so
/// `auto` would never compress).
pub const LOCKED_CONFIG: &str = "disabled_tools = bash, ls, grep\n\
     cache_diagnostics = true\n\
     request_compression = on\n\
     request_compression_level = 9\n";

/// The disabled built-ins of [`LOCKED_CONFIG`].
pub const DISABLED: [&str; 3] = ["bash", "ls", "grep"];

/// A synthetic, unexpiring OAuth login (no network refresh).
pub const OAUTH_AUTH_JSON: &str = r#"{"anthropic": {"type": "oauth", "refresh": "synthetic", "access": "synthetic-access", "expires": 9999999999999}}"#;

/// One request as the provider saw it.
#[derive(Debug, Clone)]
pub struct Seen {
    pub beta: String,
    /// gzip header XFL byte (2 = level 9, 4 = level 1); `None` = plain.
    pub gzip_xfl: Option<u8>,
    pub body: Value,
    /// Answered 400 "diagnostics: Extra inputs are not permitted".
    pub rejected: bool,
}

impl Seen {
    pub fn tool_names(&self) -> Vec<String> {
        self.body["tools"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|t| t["name"].as_str().map(String::from))
            .collect()
    }

    /// `Some(id-or-null)` when the request carries the diagnostics object.
    pub fn previous_message_id(&self) -> Option<Value> {
        self.body
            .get("diagnostics")
            .map(|d| d["previous_message_id"].clone())
    }

    pub fn has_tool_result(&self) -> bool {
        self.body["messages"].to_string().contains("tool_result")
    }

    /// The body minus the diagnostics object: what the prompt cache keys on.
    pub fn without_diagnostics(&self) -> Value {
        let mut body = self.body.clone();
        body.as_object_mut().unwrap().remove("diagnostics");
        body
    }
}

fn sse(id: &str, block_start: Value, delta: Value, stop: &str) -> String {
    let usage = r#""usage":{"input_tokens":10,"output_tokens":5,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}"#;
    let mut out = format!(
        "data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"{id}\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-6\",\"stop_reason\":null,\"stop_sequence\":null,{usage}}}}}\n\n"
    );
    out += &format!(
        "data: {}\n\n",
        json!({"type":"content_block_start","index":0,"content_block":block_start})
    );
    out += &format!(
        "data: {}\n\n",
        json!({"type":"content_block_delta","index":0,"delta":delta})
    );
    out += "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n";
    out += &format!(
        "data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"{stop}\",\"stop_sequence\":null}},{usage}}}\n\n"
    );
    out + "data: {\"type\":\"message_stop\"}\n\n"
}

/// A provider that answers a worker's first request with a `bash` call
/// (`touch marker`) and anything carrying a tool result with "done". With
/// `reject_diagnostics`, every request carrying the diagnostics object is
/// refused the way the API refuses an unknown field.
pub async fn spawn_provider(
    marker: &Path,
    reject_diagnostics: bool,
) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
    let log = seen.clone();
    let call_bash = sse(
        "msg_bash",
        json!({"type":"tool_use","id":"toolu_bash","name":"bash","input":{}}),
        json!({"type":"input_json_delta","partial_json":
            json!({"command": format!("touch {}", marker.display())}).to_string()}),
        "tool_use",
    );
    let done = sse(
        "msg_done",
        json!({"type":"text","text":""}),
        json!({"type":"text_delta","text":"done"}),
        "end_turn",
    );
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(
            move |headers: axum::http::HeaderMap, raw: axum::body::Bytes| {
                let log = log.clone();
                let (call_bash, done) = (call_bash.clone(), done.clone());
                async move {
                    let gzip = headers
                        .get("content-encoding")
                        .is_some_and(|v| v == "gzip");
                    let bytes = if gzip {
                        let mut out = Vec::new();
                        flate2::read::GzDecoder::new(&raw[..])
                            .read_to_end(&mut out)
                            .unwrap();
                        out
                    } else {
                        raw.to_vec()
                    };
                    let mut seen = Seen {
                        beta: headers
                            .get("anthropic-beta")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or_default()
                            .to_string(),
                        gzip_xfl: gzip.then(|| raw[8]),
                        body: serde_json::from_slice(&bytes).unwrap(),
                        rejected: false,
                    };
                    if reject_diagnostics && seen.body.get("diagnostics").is_some() {
                        seen.rejected = true;
                        log.lock().unwrap().push(seen);
                        let refusal = r#"{"type":"error","error":{"type":"invalid_request_error","message":"diagnostics: Extra inputs are not permitted"}}"#;
                        return (
                            axum::http::StatusCode::BAD_REQUEST,
                            [("content-type", "application/json")],
                            refusal.to_string(),
                        )
                            .into_response();
                    }
                    let reply = if seen.has_tool_result() { done } else { call_bash };
                    log.lock().unwrap().push(seen);
                    ([("content-type", "text/event-stream")], reply).into_response()
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), seen)
}

/// The oneshot, start and resume workers' requests under [`LOCKED_CONFIG`]
/// with [`OAUTH_AUTH_JSON`] (`label` names the spawn branch).
pub fn assert_worker_requests(label: &str, runs: &[Vec<Seen>; 3], marker: &Path) {
    for (path, requests) in ["oneshot", "start", "resume"].iter().zip(runs) {
        let path = format!("{label}/{path}");
        assert!(!requests.is_empty(), "{path}: no request");
        for (i, r) in requests.iter().enumerate() {
            let names = r.tool_names();
            // G1: never offered...
            for name in DISABLED {
                assert!(
                    !names.iter().any(|n| n == name),
                    "{path}#{i}: {name} offered: {names:?}"
                );
            }
            assert!(names.iter().any(|n| n == "read"), "{path}#{i}: {names:?}");
            // G6: gzip at the configured level 9 (XFL 2), every request.
            assert_eq!(r.gzip_xfl, Some(2), "{path}#{i}: gzip level");
            // G5: the diagnostics object; on OAuth never the beta header.
            assert!(
                r.previous_message_id().is_some(),
                "{path}#{i}: no diagnostics"
            );
            assert!(
                !r.beta.contains("cache-diagnosis"),
                "{path}#{i}: {}",
                r.beta
            );
        }
        // G5: the worker's own chain, starting at null (no parent's or
        // earlier worker's id), then following its own responses.
        assert_eq!(
            requests[0].previous_message_id(),
            Some(Value::Null),
            "{path}"
        );
        if requests.len() > 1 {
            assert_eq!(
                requests[1].previous_message_id(),
                Some(json!("msg_bash")),
                "{path}"
            );
        }
    }
    // ...and never run: the worker's `bash` call is refused, not executed.
    let [oneshot, start, _] = runs;
    for (path, requests) in [("oneshot", oneshot), ("start", start)] {
        assert_eq!(
            requests.len(),
            2,
            "{label}/{path}: the bash call, then its refusal"
        );
        let result = requests[1].body["messages"].to_string();
        assert!(
            result.contains("toolu_bash") && result.contains("Unknown tool: bash"),
            "{label}/{path}: {result}"
        );
    }
    assert!(!marker.exists(), "{label}: a worker ran a disabled bash");
}

/// G5 fallback: a worker whose diagnostics object is refused resends the
/// same request without it, latches it off for the rest of its run, and
/// still completes.
pub fn assert_diagnostics_refusal_falls_back(requests: &[Seen]) {
    assert_eq!(
        requests.len(),
        3,
        "refused, resent, then the tool round: {requests:#?}"
    );
    assert!(requests[0].rejected && requests[0].previous_message_id().is_some());
    assert_eq!(
        requests[0].without_diagnostics(),
        requests[1].body,
        "the resend is the same request minus the object"
    );
    for r in &requests[1..] {
        assert!(
            !r.rejected && r.previous_message_id().is_none(),
            "latched off"
        );
    }
}
