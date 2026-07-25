//! bridge_client — best-effort UDS client for the bridge daemon's
//! `ControlSocket` (`heartbeat_emit` op).
//!
//! Wire format (newline-delimited JSON, one request -> one line response):
//!
//! Request:
//! ```jsonc
//! { "op": "heartbeat_emit",
//!   "component": "agent",
//!   "id": "<agent-name>",
//!   "healthy": true,
//!   "details": { /* arbitrary */ },
//!   "synaps_user_id": "local" }
//! ```
//!
//! Response:
//! ```jsonc
//! { "ok": true,  "ts": "2025-..." }
//! { "ok": false, "code": "E_FOO", "error": "..." }
//! ```
//!
//! All errors are explicit — callers (the watcher loop) MUST swallow them
//! at debug level so the bridge being offline never blocks supervision.

use std::path::Path;
use std::time::Duration;

use serde_json::{json, Value};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::time::timeout;

#[derive(Debug, Error)]
pub enum BridgeClientError {
    /// Socket file missing or refused the connection — bridge daemon not running.
    #[error("bridge socket unavailable: {0}")]
    Unavailable(String),
    /// Connect, write, or read exceeded `heartbeat_timeout_ms`.
    #[error("bridge call timed out")]
    Timeout,
    /// Underlying I/O failure (write, read, peer closed mid-frame).
    #[error("bridge io error: {0}")]
    Io(#[from] std::io::Error),
    /// Response was not valid JSON or was missing the `ok` field.
    #[error("bridge bad response: {0}")]
    BadResponse(String),
    /// Bridge replied `{ok:false, code, error}`.
    #[error("bridge rejected: code={code} error={error}")]
    Rejected { code: String, error: String },
}

/// Emit a single `heartbeat_emit` JSON-RPC call to the bridge daemon.
///
/// Best-effort: this function is intentionally **not retried**. Callers
/// (the watcher heartbeat loop) should `tokio::spawn` it and log any
/// error at debug level only.
///
/// `timeout` covers connect + write + read end-to-end.
pub async fn emit_heartbeat(
    uds_path: &Path,
    call_timeout: Duration,
    component: &str,
    id: &str,
    healthy: bool,
    details: Value,
    synaps_user_id: &str,
) -> Result<(), BridgeClientError> {
    let request = json!({
        "op": "heartbeat_emit",
        "component": component,
        "id": id,
        "healthy": healthy,
        "details": details,
        "synaps_user_id": synaps_user_id,
    });

    let mut payload = serde_json::to_vec(&request)
        .map_err(|e| BridgeClientError::BadResponse(format!("encode: {e}")))?;
    payload.push(b'\n');

    let fut = async move {
        let stream = UnixStream::connect(uds_path)
            .await
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => {
                    BridgeClientError::Unavailable(e.to_string())
                }
                _ => BridgeClientError::Io(e),
            })?;

        let (read_half, mut write_half) = stream.into_split();
        write_half.write_all(&payload).await?;
        write_half.flush().await?;
        // Half-close write side so the peer can detect end-of-request if it
        // chooses to wait for EOF; ControlSocket reads line-by-line so this
        // is harmless.
        drop(write_half);

        let mut reader = BufReader::new(read_half);
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Err(BridgeClientError::BadResponse(
                "empty response (peer closed)".to_string(),
            ));
        }

        let parsed: Value = serde_json::from_str(line.trim_end())
            .map_err(|e| BridgeClientError::BadResponse(format!("invalid json: {e}")))?;

        match parsed.get("ok").and_then(|v| v.as_bool()) {
            Some(true) => Ok(()),
            Some(false) => {
                let code = parsed
                    .get("code")
                    .and_then(|v| v.as_str())
                    .unwrap_or("E_UNKNOWN")
                    .to_string();
                let error = parsed
                    .get("error")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                Err(BridgeClientError::Rejected { code, error })
            }
            None => Err(BridgeClientError::BadResponse(format!(
                "missing 'ok' field: {line}"
            ))),
        }
    };

    match timeout(call_timeout, fut).await {
        Ok(res) => res,
        Err(_) => Err(BridgeClientError::Timeout),
    }
}

/// Resolve the synaps_user_id for a heartbeat: env `SYNAPS_USER_ID` or `"local"`.
pub fn resolve_synaps_user_id() -> String {
    std::env::var("SYNAPS_USER_ID")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "local".to_string())
}

/// High-level helper used by the watcher supervisor.
///
/// Reads the `BridgeConfig` and:
/// - returns `Ok(())` immediately when `heartbeat_mirror == false`
///   (without touching the socket at all)
/// - otherwise calls [`emit_heartbeat`] with the configured UDS path and timeout
///
/// Callers should `tokio::spawn` this and discard the result so the watcher
/// loop is never blocked or failed by bridge unavailability.
pub async fn mirror_heartbeat(
    bridge_cfg: &synaps_cli::config::BridgeConfig,
    agent_name: &str,
    healthy: bool,
    details: Value,
) -> Result<(), BridgeClientError> {
    if !bridge_cfg.heartbeat_mirror {
        return Ok(());
    }
    let uds = bridge_cfg.resolved_uds_path();
    let user_id = resolve_synaps_user_id();
    emit_heartbeat(
        &uds,
        Duration::from_millis(bridge_cfg.heartbeat_timeout_ms),
        "agent",
        agent_name,
        healthy,
        details,
        &user_id,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::io::AsyncReadExt;
    use tokio::net::UnixListener;
    use tokio::sync::Mutex;

    fn temp_sock(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "synaps-bridge-client-{}-{}",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("control.sock")
    }

    /// Spawn a fake bridge that responds with the supplied JSON line and
    /// records every received request line. Returns the listener task and
    /// the shared request log.
    fn spawn_fake_bridge(
        path: std::path::PathBuf,
        response: &'static str,
    ) -> (tokio::task::JoinHandle<()>, Arc<Mutex<Vec<String>>>) {
        let listener = UnixListener::bind(&path).expect("bind temp UDS");
        let recorded: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded_clone = recorded.clone();

        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let recorded = recorded_clone.clone();
                tokio::spawn(async move {
                    let (read_half, mut write_half) = stream.split();
                    let mut reader = BufReader::new(read_half);
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.is_ok() && !line.is_empty() {
                        recorded.lock().await.push(line.clone());
                        let _ = write_half.write_all(response.as_bytes()).await;
                        let _ = write_half.write_all(b"\n").await;
                        let _ = write_half.flush().await;
                    }
                    // Drain anything else then drop.
                    let mut sink = Vec::new();
                    let _ = reader.read_to_end(&mut sink).await;
                });
            }
        });

        (handle, recorded)
    }

    #[tokio::test]
    async fn emit_heartbeat_happy_path() {
        let path = temp_sock("happy");
        let (task, recorded) =
            spawn_fake_bridge(path.clone(), r#"{"ok":true,"ts":"2025-01-01T00:00:00Z"}"#);

        let res = emit_heartbeat(
            &path,
            Duration::from_secs(2),
            "agent",
            "research-bot",
            true,
            json!({"session_count": 42}),
            "local",
        )
        .await;
        assert!(res.is_ok(), "expected Ok, got: {res:?}");

        let lines = recorded.lock().await.clone();
        assert_eq!(lines.len(), 1, "expected exactly one request");
        let req: Value = serde_json::from_str(lines[0].trim_end()).unwrap();
        assert_eq!(req["op"], "heartbeat_emit");
        assert_eq!(req["component"], "agent");
        assert_eq!(req["id"], "research-bot");
        assert_eq!(req["healthy"], true);
        assert_eq!(req["synaps_user_id"], "local");
        assert_eq!(req["details"]["session_count"], 42);

        task.abort();
    }

    #[tokio::test]
    async fn emit_heartbeat_unavailable_when_socket_missing() {
        let path = std::env::temp_dir().join(format!(
            "synaps-bridge-client-missing-{}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);

        let res = emit_heartbeat(
            &path,
            Duration::from_secs(1),
            "agent",
            "x",
            true,
            json!({}),
            "local",
        )
        .await;
        match res {
            Err(BridgeClientError::Unavailable(_)) => {}
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn emit_heartbeat_rejected_when_bridge_says_no() {
        let path = temp_sock("rejected");
        let (task, _recorded) = spawn_fake_bridge(
            path.clone(),
            r#"{"ok":false,"code":"E_BAD","error":"nope"}"#,
        );

        let res = emit_heartbeat(
            &path,
            Duration::from_secs(2),
            "agent",
            "x",
            true,
            json!({}),
            "local",
        )
        .await;
        match res {
            Err(BridgeClientError::Rejected { code, error }) => {
                assert_eq!(code, "E_BAD");
                assert_eq!(error, "nope");
            }
            other => panic!("expected Rejected, got {other:?}"),
        }

        task.abort();
    }

    #[tokio::test]
    async fn emit_heartbeat_bad_response_when_not_json() {
        let path = temp_sock("badjson");
        let (task, _recorded) = spawn_fake_bridge(path.clone(), "not json at all");

        let res = emit_heartbeat(
            &path,
            Duration::from_secs(2),
            "agent",
            "x",
            true,
            json!({}),
            "local",
        )
        .await;
        match res {
            Err(BridgeClientError::BadResponse(_)) => {}
            other => panic!("expected BadResponse, got {other:?}"),
        }

        task.abort();
    }

    #[tokio::test]
    async fn emit_heartbeat_times_out_when_peer_silent() {
        let path = temp_sock("timeout");
        let listener = UnixListener::bind(&path).unwrap();
        // Accept and hold the connection without responding.
        let task = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                tokio::time::sleep(Duration::from_secs(5)).await;
                drop(stream);
            }
        });

        let res = emit_heartbeat(
            &path,
            Duration::from_millis(100),
            "agent",
            "x",
            true,
            json!({}),
            "local",
        )
        .await;
        match res {
            Err(BridgeClientError::Timeout) => {}
            other => panic!("expected Timeout, got {other:?}"),
        }
        task.abort();
    }

    #[tokio::test]
    async fn mirror_heartbeat_skips_when_disabled() {
        // Point at a non-existent socket; with mirror disabled this must NOT
        // attempt to connect and must return Ok(()).
        let bogus = std::path::PathBuf::from("/tmp/synaps-bridge-NEVER-EXISTS-disabled.sock");
        let cfg = synaps_cli::config::BridgeConfig {
            uds_path: Some(bogus),
            heartbeat_mirror: false,
            heartbeat_timeout_ms: 100,
        };
        let res = mirror_heartbeat(&cfg, "agent-x", true, json!({})).await;
        assert!(
            res.is_ok(),
            "disabled mirror must short-circuit Ok, got {res:?}"
        );
    }

    #[tokio::test]
    async fn mirror_heartbeat_emits_when_enabled() {
        let path = temp_sock("mirror-on");
        let (task, recorded) = spawn_fake_bridge(path.clone(), r#"{"ok":true}"#);

        let cfg = synaps_cli::config::BridgeConfig {
            uds_path: Some(path),
            heartbeat_mirror: true,
            heartbeat_timeout_ms: 1000,
        };
        let res = mirror_heartbeat(&cfg, "research-bot", true, json!({"session_count": 7})).await;
        assert!(res.is_ok(), "expected Ok, got {res:?}");

        let lines = recorded.lock().await.clone();
        assert_eq!(lines.len(), 1);
        let req: Value = serde_json::from_str(lines[0].trim_end()).unwrap();
        assert_eq!(req["op"], "heartbeat_emit");
        assert_eq!(req["component"], "agent");
        assert_eq!(req["id"], "research-bot");
        assert_eq!(req["healthy"], true);
        assert_eq!(req["details"]["session_count"], 7);

        task.abort();
    }

    #[tokio::test]
    async fn mirror_heartbeat_returns_unavailable_when_socket_missing() {
        let bogus = std::env::temp_dir().join(format!(
            "synaps-bridge-mirror-missing-{}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&bogus);
        let cfg = synaps_cli::config::BridgeConfig {
            uds_path: Some(bogus),
            heartbeat_mirror: true,
            heartbeat_timeout_ms: 200,
        };
        let res = mirror_heartbeat(&cfg, "x", true, json!({})).await;
        match res {
            Err(BridgeClientError::Unavailable(_)) => {}
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }
}
