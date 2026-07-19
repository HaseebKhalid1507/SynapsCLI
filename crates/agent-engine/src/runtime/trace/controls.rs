//! Explicit trace controls (Task 12, spec §5.1 raw-capture clause + §6.1
//! surfaces): `/trace next`, `/trace next content`, `/trace status`.
//!
//! ## One-shot semantics
//!
//! Arming (`/trace next`) enables tracing for **exactly the next logical
//! outgoing provider request** — even when the `telemetry` config level is
//! Off — and then auto-disarms. The armed ephemeral context carries a
//! one-shot request gate consumed inside `RequestTracer::begin`: the first
//! request wins, all of its retry attempts emit, and subsequent tool-loop
//! requests through the same context are disabled. It never silently
//! enables indefinite persistence: when telemetry is Off the one-shot
//! record rides an ephemeral writer whose handle the runtime retains (in
//! its one-shot state, off the request path) until it is replaced by a
//! re-arm or drained by the `shutdown_observability*` exit epilogue — so
//! the armed record flushes even when the session exits immediately after
//! the request. The existing Basic/Full config rule is unchanged — arming
//! on top of an already-enabled session only adds the (optional) content
//! capture.
//!
//! ## Content capture (explicit, ephemeral, redacted)
//!
//! Persisted traces contain no content, so `synaps trace export
//! --include-content` needs an explicit capture: `/trace next content` arms
//! a **one-request, bounded, redacted** capture of the request body.
//! Security decisions, deliberately conservative:
//!
//! - capture is redacted **at capture time** (recursive credential-key /
//!   secret-pattern scrub, see `trace::export::redact_value`) — an
//!   unredacted body never touches disk;
//! - the bundle is written with the Phase 1 private-fs helpers (`0600`
//!   file, `0700` parent, symlink-refusing, atomic) under
//!   `<synaps base dir>/trace/capture/`;
//! - the bundle carries its own schema tag (`synaps-trace-content-capture/1`)
//!   so it can never masquerade as a `synaps-request-trace/1` record;
//! - capture is bounded ([`CAPTURE_MAX_BYTES`]) and expires
//!   ([`CAPTURE_TTL`]): `trace export --include-content` consumes (deletes)
//!   it, and an expired bundle is refused and deleted instead of exported;
//!
//!   **Expiry semantics (documented guarantee):** expiry is *logical* —
//!   an expired bundle can never be exported (the export path checks the
//!   embedded `expires_unix_ms` and deletes stale bundles). Physical
//!   deletion is *opportunistic*: no background process exists after the
//!   CLI exits, so stale bundles are swept on the next trace interaction
//!   (every new capture, every `synaps trace export` invocation). A bundle
//!   may therefore sit encrypted-at-rest-equivalent (private `0600` file,
//!   already redacted) on disk past its TTL until the next interaction —
//!   but it is unreadable through any supported export path from the
//!   moment it expires;
//! - only the request **body** is captured — headers, cookies, and
//!   credentials never enter the capture path by construction (they are
//!   attached by the HTTP client after the body is built).
//!
//! The capture file is how the arm crosses the process boundary to a later
//! `synaps trace export` invocation without weakening privacy: the on-disk
//! artifact is already redacted, private, bounded, and short-lived.

use super::types::TraceId;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Schema tag for the ephemeral content-capture bundle. Distinct from the
/// request-trace schema by design.
pub const CONTENT_CAPTURE_SCHEMA: &str = "synaps-trace-content-capture/1";

/// Upper bound on captured (post-redaction) body bytes.
pub const CAPTURE_MAX_BYTES: usize = 1024 * 1024;

/// Retention window for a capture bundle.
pub const CAPTURE_TTL: Duration = Duration::from_secs(15 * 60);

/// Default capture directory: `<synaps base dir>/trace/capture/`.
pub fn default_capture_dir() -> PathBuf {
    agent_core::core::config::base_dir()
        .join("trace")
        .join("capture")
}

/// Current arm state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceArm {
    Off,
    /// Trace the next request (metadata only), then auto-disarm.
    NextMetadata,
    /// Trace the next request AND write a redacted content-capture bundle.
    NextWithContent,
}

/// Session trace controls: one-shot arm consumed by the next request.
#[derive(Debug, Default)]
pub struct TraceControls {
    state: Mutex<Option<ArmKind>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArmKind {
    Metadata,
    Content,
}

impl TraceControls {
    pub fn new() -> Self {
        Self::default()
    }

    /// Arm for exactly the next request. Re-arming replaces the pending arm.
    pub fn arm_next(&self, with_content: bool) {
        *self.state.lock().expect("trace controls poisoned") = Some(if with_content {
            ArmKind::Content
        } else {
            ArmKind::Metadata
        });
    }

    pub fn disarm(&self) {
        *self.state.lock().expect("trace controls poisoned") = None;
    }

    /// Non-consuming view for `/trace status`.
    pub fn peek(&self) -> TraceArm {
        match *self.state.lock().expect("trace controls poisoned") {
            None => TraceArm::Off,
            Some(ArmKind::Metadata) => TraceArm::NextMetadata,
            Some(ArmKind::Content) => TraceArm::NextWithContent,
        }
    }

    /// Consume the pending arm (auto-disable). Returns `Some(with_content)`
    /// exactly once per arming.
    pub fn consume(&self) -> Option<bool> {
        self.state
            .lock()
            .expect("trace controls poisoned")
            .take()
            .map(|kind| kind == ArmKind::Content)
    }
}

// --- Ephemeral content capture ---

/// On-disk shape of the capture bundle. Clearly labeled non-request-trace
/// schema; `redacted` is a hard marker checked again on export.
#[derive(Debug, Serialize, Deserialize)]
pub struct ContentCaptureBundle {
    /// Always [`CONTENT_CAPTURE_SCHEMA`].
    pub schema: String,
    pub request_id: TraceId,
    pub created_unix_ms: u64,
    pub expires_unix_ms: u64,
    /// Always true: the body was recursively redacted before writing.
    pub redacted: bool,
    /// True when the raw body exceeded [`CAPTURE_MAX_BYTES`] and was
    /// therefore NOT captured (fail closed, never truncated mid-secret).
    pub over_budget: bool,
    /// Redacted request body (request content fields only — never headers
    /// or credentials). `None` iff `over_budget`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<serde_json::Value>,
}

pub(crate) fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// One-request content capture arm, attached to a `TraceContext` when the
/// user ran `/trace next content`. Fires at most once.
#[derive(Debug)]
pub struct ContentCapture {
    dir: PathBuf,
    consumed: AtomicBool,
}

impl ContentCapture {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            consumed: AtomicBool::new(false),
        }
    }

    /// Capture one request body: parse, redact recursively, bound, write a
    /// private bundle. Never fails the request — any error is logged as
    /// metadata and the capture is simply lost. Fires at most once.
    pub fn capture(&self, request_id: &TraceId, body_bytes: &[u8]) {
        if self.consumed.swap(true, Ordering::SeqCst) {
            return;
        }
        // Opportunistic retention sweep (see module docs): any bundle from
        // an earlier session that outlived its TTL is removed before a new
        // one is written — expiry is enforced logically at export, deleted
        // physically on the next trace interaction.
        let _ = super::export::sweep_expired_captures(&self.dir);
        let over_budget = body_bytes.len() > CAPTURE_MAX_BYTES;
        let body = if over_budget {
            None
        } else {
            match serde_json::from_slice::<serde_json::Value>(body_bytes) {
                Ok(mut value) => {
                    super::export::redact_value(&mut value);
                    Some(value)
                }
                Err(_) => {
                    tracing::warn!(
                        byte_len = body_bytes.len(),
                        "trace content capture skipped: body is not JSON"
                    );
                    return;
                }
            }
        };
        let now = unix_ms_now();
        let bundle = ContentCaptureBundle {
            schema: CONTENT_CAPTURE_SCHEMA.to_string(),
            request_id: request_id.clone(),
            created_unix_ms: now,
            expires_unix_ms: now + CAPTURE_TTL.as_millis() as u64,
            redacted: true,
            over_budget,
            body,
        };
        if let Err(err) = write_bundle(&self.dir, &bundle) {
            // Metadata-only: path/IO reason, never body content.
            tracing::warn!(reason = %err, "trace content capture write failed");
        }
    }

    /// Consume the arm on a provider path that cannot capture a request
    /// body (no exact serialized body exists in this process). Returns
    /// `true` exactly once so the caller can surface the failure; never
    /// writes anything.
    pub fn mark_unsupported(&self) -> bool {
        !self.consumed.swap(true, Ordering::SeqCst)
    }
}

/// Path of the capture bundle for a request ID.
pub fn capture_path(dir: &Path, request_id: &TraceId) -> PathBuf {
    // TraceId alphabet includes '/' and ':'; flatten to a safe file name.
    let name: String = request_id
        .as_str()
        .chars()
        .map(|c| if c == '/' || c == ':' { '_' } else { c })
        .collect();
    dir.join(format!("capture-{name}.json"))
}

fn write_bundle(dir: &Path, bundle: &ContentCaptureBundle) -> Result<(), String> {
    agent_core::core::private_fs::ensure_private_dir(dir).map_err(|e| e.to_string())?;
    let path = capture_path(dir, &bundle.request_id);
    let data = serde_json::to_vec_pretty(bundle).map_err(|e| e.to_string())?;
    agent_core::core::private_fs::write_atomic_private(&path, &data).map_err(|e| e.to_string())
}

// --- `/trace status` report ---

/// Metadata-only status for `/trace status`: mode, persistence path,
/// counters. Never secrets, never content.
#[derive(Debug, Clone)]
pub struct TraceStatusReport {
    /// Session persistence via the telemetry Basic/Full rule.
    pub persistent_enabled: bool,
    pub arm: TraceArm,
    /// Where trace records are persisted when enabled.
    pub trace_path: Option<PathBuf>,
    pub writer_stats: Option<crate::runtime::telemetry::WriterStats>,
    pub degraded_records: u64,
}

impl TraceStatusReport {
    pub fn render(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        let mode = match (self.persistent_enabled, self.arm) {
            (true, TraceArm::NextWithContent) => "enabled (next request adds content capture)",
            (true, _) => "enabled (telemetry basic/full)",
            (false, TraceArm::NextMetadata) => "armed for next request (metadata only)",
            (false, TraceArm::NextWithContent) => {
                "armed for next request (metadata + redacted content capture)"
            }
            (false, TraceArm::Off) => "disabled",
        };
        let _ = writeln!(out, "trace: {mode}");
        match &self.trace_path {
            Some(path) => {
                let _ = writeln!(out, "  persistence: {}", path.display());
            }
            None => {
                let _ = writeln!(out, "  persistence: (unresolved)");
            }
        }
        match self.writer_stats {
            Some(stats) => {
                let _ = writeln!(
                    out,
                    "  writer: {} enqueued, {} written, {} dropped; {} degraded records",
                    stats.enqueued, stats.written, stats.dropped, self.degraded_records,
                );
            }
            None => {
                let _ = writeln!(
                    out,
                    "  writer: not running; {} degraded records",
                    self.degraded_records
                );
            }
        }
        out.trim_end().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arm_is_consumed_exactly_once() {
        let controls = TraceControls::new();
        assert_eq!(controls.peek(), TraceArm::Off);
        assert!(controls.consume().is_none());

        controls.arm_next(false);
        assert_eq!(controls.peek(), TraceArm::NextMetadata);
        assert_eq!(controls.consume(), Some(false));
        assert_eq!(controls.peek(), TraceArm::Off);
        assert!(controls.consume().is_none(), "second consume must be empty");

        controls.arm_next(true);
        assert_eq!(controls.peek(), TraceArm::NextWithContent);
        assert_eq!(controls.consume(), Some(true));
        assert!(controls.consume().is_none());
    }

    #[test]
    fn content_capture_fires_once_writes_private_redacted_bundle() {
        let dir = tempfile::tempdir().expect("tempdir");
        let capture = ContentCapture::new(dir.path().join("cap"));
        let id = TraceId::new("req-1-0").unwrap();
        let secret = "sk-SENTINEL1234567890abcd";
        let body = serde_json::json!({
            "model": "m",
            "messages": [{"role": "user", "content": format!("my key is {secret}")}],
            "api_key": "topsecret-value"
        });
        capture.capture(&id, serde_json::to_vec(&body).unwrap().as_slice());

        let path = capture_path(&dir.path().join("cap"), &id);
        let data = std::fs::read_to_string(&path).expect("bundle exists");
        assert!(!data.contains(secret), "raw sentinel secret persisted");
        assert!(!data.contains("topsecret-value"));
        assert!(data.contains(CONTENT_CAPTURE_SCHEMA));
        let bundle: ContentCaptureBundle = serde_json::from_str(&data).unwrap();
        assert!(bundle.redacted);
        assert!(!bundle.over_budget);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        // Second capture on the same arm is a no-op.
        std::fs::remove_file(&path).unwrap();
        capture.capture(&id, b"{}");
        assert!(!path.exists(), "capture fired twice");
    }

    #[test]
    fn over_budget_body_is_refused_not_truncated() {
        let dir = tempfile::tempdir().expect("tempdir");
        let capture = ContentCapture::new(dir.path().to_path_buf());
        let id = TraceId::new("req-big").unwrap();
        let big = serde_json::json!({"content": "x".repeat(CAPTURE_MAX_BYTES + 10)});
        capture.capture(&id, serde_json::to_vec(&big).unwrap().as_slice());
        let path = capture_path(dir.path(), &id);
        let bundle: ContentCaptureBundle =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(bundle.over_budget);
        assert!(bundle.body.is_none());
    }

    #[test]
    fn new_capture_sweeps_a_previous_expired_bundle() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cap_dir = dir.path().join("cap");
        agent_core::core::private_fs::ensure_private_dir(&cap_dir).unwrap();

        // Capture A: expired long ago (as if left behind by a dead process).
        let stale_id = TraceId::new("req-stale").unwrap();
        let stale = ContentCaptureBundle {
            schema: CONTENT_CAPTURE_SCHEMA.to_string(),
            request_id: stale_id.clone(),
            created_unix_ms: 1_000,
            expires_unix_ms: 2_000,
            redacted: true,
            over_budget: false,
            body: Some(serde_json::json!({"old": true})),
        };
        let stale_path = capture_path(&cap_dir, &stale_id);
        std::fs::write(&stale_path, serde_json::to_vec(&stale).unwrap()).unwrap();

        // Capture B removes A on its way in (opportunistic sweep).
        let capture = ContentCapture::new(cap_dir.clone());
        let fresh_id = TraceId::new("req-fresh").unwrap();
        capture.capture(&fresh_id, b"{\"x\":1}");

        assert!(
            !stale_path.exists(),
            "expired capture A must be swept when capture B is written"
        );
        assert!(capture_path(&cap_dir, &fresh_id).exists());
    }

    #[test]
    fn mark_unsupported_consumes_the_arm_exactly_once_without_artifact() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cap_dir = dir.path().join("cap");
        let capture = ContentCapture::new(cap_dir.clone());
        assert!(capture.mark_unsupported(), "first consume reports true");
        assert!(!capture.mark_unsupported(), "second consume is a no-op");
        // A later capture attempt on the consumed arm writes nothing.
        let id = TraceId::new("req-after").unwrap();
        capture.capture(&id, b"{}");
        assert!(!capture_path(&cap_dir, &id).exists());
    }

    #[test]
    fn status_render_mentions_mode_and_no_secrets() {
        let report = TraceStatusReport {
            persistent_enabled: false,
            arm: TraceArm::NextMetadata,
            trace_path: Some(PathBuf::from("/tmp/request-trace.jsonl")),
            writer_stats: None,
            degraded_records: 2,
        };
        let text = report.render();
        assert!(text.contains("armed for next request (metadata only)"));
        assert!(text.contains("request-trace.jsonl"));
        assert!(text.contains("2 degraded"));
    }
}
