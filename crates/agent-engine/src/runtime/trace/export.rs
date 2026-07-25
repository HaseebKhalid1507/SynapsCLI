//! Trace export (Task 12): metadata export of persisted
//! `synaps-request-trace/1` records, explicit redacted content export, and
//! the recursive secret redaction they share.
//!
//! - **Metadata export** (`synaps trace export <id> --metadata-only`):
//!   reads the private trace JSONL, validates every line as a
//!   [`RequestTrace`], selects the records whose `turn_id` or `request_id`
//!   equals the given (validated, bounded) [`TraceId`], and writes them to
//!   a user-selected private file (`0600`, parent `0700`), refusing
//!   symlinks and pre-existing targets. No network, no content — the
//!   records are structurally metadata-only.
//! - **Content export** (`--include-content --allow-content-export`):
//!   fail-closed. It requires BOTH flags in the same invocation and an
//!   existing ephemeral capture bundle produced by `/trace next content`
//!   (see `trace::controls`). The bundle is re-redacted (defense in depth),
//!   written under its own clearly-labeled schema
//!   (`synaps-trace-content-export/1`, never `synaps-request-trace/1`),
//!   and the capture bundle is deleted (consumed). Expired bundles are
//!   deleted and refused.

use super::controls::{
    capture_path, unix_ms_now, ContentCaptureBundle, CAPTURE_TTL, CONTENT_CAPTURE_SCHEMA,
};
use super::types::{RequestTrace, TraceId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Schema tag of the content-export artifact. Deliberately distinct from
/// both the request-trace and capture schemas.
pub const CONTENT_EXPORT_SCHEMA: &str = "synaps-trace-content-export/1";

/// Typed export failure. Every variant is metadata-only (paths, counts).
#[derive(Debug)]
pub enum ExportError {
    /// The given ID is not a valid bounded `TraceId`.
    InvalidId(String),
    /// The trace log does not exist / is unreadable.
    TraceLogUnreadable(PathBuf, std::io::Error),
    /// A persisted line failed `RequestTrace` validation (fail closed:
    /// export refuses a log it cannot fully validate).
    InvalidRecord {
        line: usize,
        reason: String,
    },
    /// No record matched the requested turn/request ID.
    NotFound(String),
    /// Refusing to write through a symlink or over an existing file.
    UnsafeTarget(PathBuf, String),
    /// Content export invoked without the explicit runtime opt-in flag.
    ContentOptInMissing,
    /// No capture bundle exists for this ID (content export is fail-closed:
    /// it never reconstructs prompts from sessions).
    CaptureMissing(PathBuf),
    /// The capture bundle expired (it is deleted, not exported).
    CaptureExpired(PathBuf),
    /// The capture bundle is malformed or carries the wrong schema tag.
    CaptureInvalid(PathBuf, String),
    /// The capture recorded an over-budget body: nothing to export.
    CaptureOverBudget,
    /// A streaming read bound was exceeded (per-line or total bytes).
    BoundExceeded {
        what: &'static str,
        limit: u64,
    },
    Io(std::io::Error),
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExportError::InvalidId(reason) => write!(f, "invalid trace id: {reason}"),
            ExportError::TraceLogUnreadable(p, e) => {
                write!(f, "cannot read trace log {}: {e}", p.display())
            }
            ExportError::InvalidRecord { line, reason } => {
                write!(f, "trace log line {line} is not a valid record: {reason}")
            }
            ExportError::NotFound(id) => {
                write!(f, "no trace record matches turn/request id {id}")
            }
            ExportError::UnsafeTarget(p, why) => {
                write!(f, "refusing output target {}: {why}", p.display())
            }
            ExportError::ContentOptInMissing => write!(
                f,
                "content export requires the explicit --allow-content-export opt-in \
                 in the same invocation"
            ),
            ExportError::CaptureMissing(p) => write!(
                f,
                "no content capture bundle at {} — run `/trace next content` in a live \
                 session and retry the export before it expires; persisted traces contain \
                 no content and prompts are never reconstructed from sessions",
                p.display()
            ),
            ExportError::CaptureExpired(p) => write!(
                f,
                "content capture at {} expired and was deleted",
                p.display()
            ),
            ExportError::CaptureInvalid(p, reason) => {
                write!(f, "capture bundle {} is invalid: {reason}", p.display())
            }
            ExportError::CaptureOverBudget => write!(
                f,
                "the captured request exceeded the capture byte budget; no content was retained"
            ),
            ExportError::BoundExceeded { what, limit } => {
                write!(f, "trace export {what} exceeds the {limit}-byte bound")
            }
            ExportError::Io(e) => write!(f, "export io error: {e}"),
        }
    }
}

impl std::error::Error for ExportError {}

impl From<std::io::Error> for ExportError {
    fn from(e: std::io::Error) -> Self {
        ExportError::Io(e)
    }
}

/// Result of a metadata export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetadataExportStats {
    /// Records scanned in the trace log.
    pub scanned: usize,
    /// Records matching the ID and written to the output.
    pub exported: usize,
}

/// Conservative cap on bytes read from a single capture bundle. A valid
/// bundle body is bounded by `CAPTURE_MAX_BYTES` before pretty-printing;
/// anything beyond this cap is planted garbage, removable without reading.
pub(crate) const CAPTURE_BUNDLE_READ_CAP: u64 = 8 * 1024 * 1024;

/// Upper bound on directory entries examined by one capture sweep: bounded
/// work even against a maliciously stuffed directory.
const MAX_SWEEP_ENTRIES: usize = 4096;

/// Classified failure of the bounded regular-file read primitive.
#[derive(Debug)]
enum BoundedReadError {
    /// No entry at the path.
    NotFound,
    /// Symlink, FIFO, directory, device, or other non-regular entry —
    /// refused before any read (static reason string, never file content).
    NotRegular(&'static str),
    /// The (regular) file exceeds the byte cap.
    Oversized,
    /// Other I/O failure.
    Io(std::io::Error),
}

/// Safe bounded read of a regular file, closing the check/open TOCTOU:
/// open read-only with `O_NOFOLLOW|O_NONBLOCK` (Unix), `fstat` the opened
/// handle, require a regular file, enforce `cap` before allocating, and
/// read at most `cap + 1` bytes to detect concurrent growth. Symlinks,
/// FIFOs (no blocking open), directories, and devices are refused without
/// reading a single byte.
fn read_bounded_regular_file(path: &Path, cap: u64) -> Result<Vec<u8>, BoundedReadError> {
    use std::io::Read as _;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        // O_NOFOLLOW: a planted symlink fails the open (ELOOP), never
        // followed. O_NONBLOCK: opening a writer-less FIFO returns
        // immediately instead of blocking forever; harmless on regular
        // files, which is the only type accepted below.
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(not(unix))]
    if std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(BoundedReadError::NotRegular("symlink"));
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(BoundedReadError::NotFound)
        }
        #[cfg(unix)]
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
            return Err(BoundedReadError::NotRegular("symlink"));
        }
        Err(e) => return Err(BoundedReadError::Io(e)),
    };
    // fstat the opened handle — the type/size decision and the read use
    // the same file description, so a swap after open cannot bypass it.
    let meta = file.metadata().map_err(BoundedReadError::Io)?;
    if !meta.file_type().is_file() {
        return Err(BoundedReadError::NotRegular("not a regular file"));
    }
    if meta.len() > cap {
        return Err(BoundedReadError::Oversized);
    }
    let mut buf = Vec::with_capacity(meta.len().min(cap) as usize);
    file.take(cap + 1)
        .read_to_end(&mut buf)
        .map_err(BoundedReadError::Io)?;
    if buf.len() as u64 > cap {
        return Err(BoundedReadError::Oversized);
    }
    Ok(buf)
}

/// Static classification of a serde_json error: category and position
/// only — NEVER serde's own message, which can echo hostile input bytes.
fn classify_json_error(e: &serde_json::Error) -> String {
    let class = match e.classify() {
        serde_json::error::Category::Io => "io",
        serde_json::error::Category::Syntax => "syntax",
        serde_json::error::Category::Data => "schema",
        serde_json::error::Category::Eof => "eof",
    };
    format!("{class} error at line {} column {}", e.line(), e.column())
}

/// Create the output file privately: parent `0700`, file `0600`,
/// `O_CREAT|O_EXCL|O_NOFOLLOW` — a symlink or pre-existing file at the
/// target is refused, never followed or truncated.
fn create_private_new(path: &Path) -> Result<std::fs::File, ExportError> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        agent_core::core::private_fs::ensure_private_dir(parent)
            .map_err(|e| ExportError::UnsafeTarget(path.to_path_buf(), e.to_string()))?;
    }
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(ExportError::UnsafeTarget(
                path.to_path_buf(),
                "symlink planted at target".to_string(),
            ));
        }
        Ok(_) => {
            return Err(ExportError::UnsafeTarget(
                path.to_path_buf(),
                "target already exists".to_string(),
            ));
        }
        Err(_) => {}
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    options
        .open(path)
        .map_err(|e| ExportError::UnsafeTarget(path.to_path_buf(), e.to_string()))
}

/// Export all records whose `turn_id` or `request_id` equals `id` from the
/// trace JSONL at `trace_log` into a fresh private file at `output`.
///
/// F5 hardening: the log is streamed line by line under explicit per-line
/// (1 MiB) and total (64 MiB) byte bounds — no unbounded whole-file read —
/// and parse failures are reported by static category and position only,
/// never by echoing persisted bytes.
pub fn export_metadata(
    trace_log: &Path,
    id: &str,
    output: &Path,
) -> Result<MetadataExportStats, ExportError> {
    export_metadata_bounded(trace_log, id, output, 1024 * 1024, 64 * 1024 * 1024)
}

/// [`export_metadata`] with explicit per-line and total byte bounds.
fn export_metadata_bounded(
    trace_log: &Path,
    id: &str,
    output: &Path,
    line_cap: usize,
    total_cap: u64,
) -> Result<MetadataExportStats, ExportError> {
    use std::io::{BufRead as _, Read as _};
    let id = TraceId::new(id).map_err(ExportError::InvalidId)?;
    let file = std::fs::File::open(trace_log)
        .map_err(|e| ExportError::TraceLogUnreadable(trace_log.to_path_buf(), e))?;
    let mut reader = std::io::BufReader::new(file);

    let mut scanned = 0usize;
    let mut selected: Vec<String> = Vec::new();
    let mut total_read: u64 = 0;
    let mut buf: Vec<u8> = Vec::new();
    let mut line_no = 0usize;
    loop {
        line_no += 1;
        buf.clear();
        // Bounded line read: never buffer more than the cap (+1 to detect
        // the overflow) regardless of the log's contents.
        let read = (&mut reader)
            .take(line_cap as u64 + 1)
            .read_until(b'\n', &mut buf)?;
        if read == 0 {
            break;
        }
        if buf.len() > line_cap {
            return Err(ExportError::BoundExceeded {
                what: "log line",
                limit: line_cap as u64,
            });
        }
        total_read = total_read.saturating_add(read as u64);
        if total_read > total_cap {
            return Err(ExportError::BoundExceeded {
                what: "trace log",
                limit: total_cap,
            });
        }
        let line = std::str::from_utf8(&buf)
            .map_err(|_| ExportError::InvalidRecord {
                line: line_no,
                reason: "not valid UTF-8".to_string(),
            })?
            .trim_end_matches(['\n', '\r']);
        if line.trim().is_empty() {
            continue;
        }
        // Every line must validate as a RequestTrace (fail closed). Parse
        // failures are classified statically — hostile bytes never travel
        // through the error value.
        let record: RequestTrace =
            serde_json::from_str(line).map_err(|e| ExportError::InvalidRecord {
                line: line_no,
                reason: classify_json_error(&e),
            })?;
        scanned += 1;
        if record.turn_id == id || record.request_id == id {
            // Re-serialize the validated record (canonical field order) —
            // never copy the raw line into the export.
            let line = serde_json::to_string(&record).map_err(|e| ExportError::InvalidRecord {
                line: line_no,
                reason: classify_json_error(&e),
            })?;
            selected.push(line);
        }
    }
    if selected.is_empty() {
        return Err(ExportError::NotFound(id.as_str().to_string()));
    }

    let mut file = create_private_new(output)?;
    for line in &selected {
        file.write_all(line.as_bytes())?;
        file.write_all(b"\n")?;
    }
    file.flush()?;
    Ok(MetadataExportStats {
        scanned,
        exported: selected.len(),
    })
}

// --- Recursive redaction ---

/// Key substrings (lowercased comparison) whose values are always redacted.
const SECRET_KEY_MARKERS: &[&str] = &[
    "authorization",
    "api_key",
    "apikey",
    "api-key",
    "token",
    "secret",
    "password",
    "passwd",
    "credential",
    "cookie",
    "bearer",
    "private_key",
    "access_key",
    "session_key",
    "client_secret",
    "refresh",
];

/// Value prefixes that mark an obvious credential wherever it appears.
const SECRET_VALUE_PREFIXES: &[&str] = &[
    "sk-",
    "sk_",
    "sk-ant-",
    "xoxb-",
    "xoxp-",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "github_pat_",
    "glpat-",
    "npm_",
    "shpat_",
    "shpss_",
    "AKIA",
    "ASIA",
    "ya29.",
    "AIza",
    "Bearer ",
    "bearer ",
];

const REDACTED: &str = "[REDACTED]";

fn key_is_secret(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    SECRET_KEY_MARKERS.iter().any(|m| lower.contains(m))
}

/// Word-level credential prefixes for embedded-token scanning.
const SECRET_WORD_PREFIXES: &[&str] = &[
    "sk-",
    "sk_",
    "sk-ant-",
    "xoxb-",
    "xoxp-",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "github_pat_",
    "glpat-",
    "npm_",
    "shpat_",
    "shpss_",
    "AKIA",
    "ASIA",
    "ya29.",
    "AIza",
];

/// A token shaped like a JWT: three dot-separated base64url segments, the
/// first being a base64url-encoded JSON header (always starts with `eyJ`).
fn looks_like_jwt(token: &str) -> bool {
    if !token.starts_with("eyJ") {
        return false;
    }
    let mut segments = 0usize;
    for segment in token.split('.') {
        segments += 1;
        if segment.is_empty()
            || !segment
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '=')
        {
            return false;
        }
    }
    segments == 3
}

/// PEM-armored private key material anywhere in a value.
fn contains_private_key_block(s: &str) -> bool {
    s.contains("-----BEGIN") && s.contains("PRIVATE KEY")
}

/// A standalone value shaped like an obvious credential (JWT or known
/// secret prefix) — used for query/fragment parameter values whose key is
/// not secret-named.
fn value_is_secret_shaped(value: &str) -> bool {
    !value.is_empty()
        && (looks_like_jwt(value) || SECRET_VALUE_PREFIXES.iter().any(|p| value.starts_with(p)))
}

/// Scrub one `k=v&k2=v2` parameter list: values under secret-named keys and
/// credential-shaped values under benign keys are redacted; benign pairs
/// and non-pair segments are preserved verbatim.
fn scrub_param_pairs(params: &str, changed: &mut bool) -> String {
    params
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((key, value))
                if !value.is_empty() && (key_is_secret(key) || value_is_secret_shaped(value)) =>
            {
                *changed = true;
                format!("{key}={REDACTED}")
            }
            _ => pair.to_string(),
        })
        .collect::<Vec<String>>()
        .join("&")
}

/// Redact secret-bearing query parameters AND fragment parameters inside a
/// URL-shaped string: `?api_key=…&token=…` and `#access_token=…&state=…`
/// (OAuth implicit-flow style) → secret values replaced, keys and safe
/// params preserved. Applies to any string containing a `?key=value` query
/// or `#key=value` fragment, not only strict URLs — conservative by design.
fn scrub_url_query(s: &str) -> Option<String> {
    let (head, fragment) = match s.find('#') {
        Some(idx) => (&s[..idx], Some(&s[idx + 1..])),
        None => (s, None),
    };
    let (base, query) = match head.find('?') {
        Some(idx) => (&head[..idx + 1], Some(&head[idx + 1..])),
        None => (head, None),
    };
    let mut changed = false;
    let scrubbed_query = match query {
        Some(query) if query.contains('=') => Some(scrub_param_pairs(query, &mut changed)),
        other => other.map(str::to_string),
    };
    let scrubbed_fragment = match fragment {
        Some(fragment) if fragment.contains('=') => Some(scrub_param_pairs(fragment, &mut changed)),
        other => other.map(str::to_string),
    };
    changed.then(|| {
        let mut out = String::from(base);
        if let Some(query) = scrubbed_query {
            out.push_str(&query);
        }
        if let Some(fragment) = scrubbed_fragment {
            out.push('#');
            out.push_str(&fragment);
        }
        out
    })
}

fn scrub_string(s: &str) -> Option<String> {
    // PEM private key blocks: redact the whole value — partial scrubbing
    // of multi-line key material is not worth the risk.
    if contains_private_key_block(s) {
        return Some(REDACTED.to_string());
    }
    // Whole-value credential prefixes ("Bearer <x>", "sk-…", JWTs, …).
    for prefix in SECRET_VALUE_PREFIXES {
        if s.starts_with(prefix) {
            return Some(REDACTED.to_string());
        }
    }
    if looks_like_jwt(s) {
        return Some(REDACTED.to_string());
    }
    // URL query parameters carrying secret-named keys.
    let (mut current, mut changed) = match scrub_url_query(s) {
        Some(scrubbed) => (scrubbed, true),
        None => (s.to_string(), false),
    };
    // Embedded credential tokens inside longer text: redact just the token
    // (including the word after a "Bearer" marker and embedded JWTs).
    let mut out = String::new();
    let mut prev_bearer = false;
    let mut word_changed = false;
    for word in current.split_inclusive(char::is_whitespace) {
        let trimmed = word.trim_end();
        let ws = &word[trimmed.len()..];
        let is_secret = !trimmed.is_empty()
            && (prev_bearer
                || looks_like_jwt(trimmed)
                || SECRET_WORD_PREFIXES
                    .iter()
                    .any(|p| trimmed.starts_with(p) && trimmed.len() >= p.len() + 6));
        if is_secret {
            out.push_str(REDACTED);
            word_changed = true;
        } else {
            out.push_str(trimmed);
        }
        if !trimmed.is_empty() {
            prev_bearer = trimmed.eq_ignore_ascii_case("bearer");
        }
        out.push_str(ws);
    }
    if word_changed {
        current = out;
        changed = true;
    }
    changed.then_some(current)
}

/// Recursively redact credential-bearing keys and obvious secret-shaped
/// values in place. Returns the number of redactions applied. Applies to
/// nested objects and arrays at any depth.
pub fn redact_value(value: &mut Value) -> u64 {
    match value {
        Value::Object(map) => {
            let mut count = 0;
            for (key, entry) in map.iter_mut() {
                if key_is_secret(key) {
                    *entry = Value::String(REDACTED.to_string());
                    count += 1;
                } else {
                    count += redact_value(entry);
                }
            }
            count
        }
        Value::Array(items) => items.iter_mut().map(redact_value).sum(),
        Value::String(s) => {
            if let Some(scrubbed) = scrub_string(s) {
                *s = scrubbed;
                1
            } else {
                0
            }
        }
        _ => 0,
    }
}

// --- Content export ---

/// On-disk shape of the content export artifact. NOT a request trace.
#[derive(Debug, Serialize, Deserialize)]
pub struct ContentExport {
    /// Always [`CONTENT_EXPORT_SCHEMA`].
    pub schema: String,
    pub request_id: TraceId,
    pub captured_unix_ms: u64,
    pub exported_unix_ms: u64,
    /// Hard marker: the body passed recursive redaction (twice — at
    /// capture and again at export).
    pub redacted: bool,
    pub redactions_applied: u64,
    pub body: Value,
}

/// Export the ephemeral capture bundle for `id` into a fresh private file.
/// Fail-closed: requires `allow_content_export`, an existing unexpired
/// bundle, and re-redacts before writing. Consumes (deletes) the bundle.
pub fn export_content(
    capture_dir: &Path,
    id: &str,
    output: &Path,
    allow_content_export: bool,
) -> Result<(), ExportError> {
    if !allow_content_export {
        return Err(ExportError::ContentOptInMissing);
    }
    let id = TraceId::new(id).map_err(ExportError::InvalidId)?;
    let path = capture_path(capture_dir, &id);
    // Opportunistic retention sweep (B2): every export interaction removes
    // stale bundles left behind by exited sessions. The requested bundle is
    // excluded here so its own expiry is reported precisely below
    // (`CaptureExpired`, and deleted there) instead of a generic miss.
    let _ = sweep_expired_captures_except(capture_dir, Some(&path));
    // Bounded, symlink-refusing, type-checked read of the opened handle —
    // no check/open TOCTOU, no blocking on planted FIFOs.
    let raw = match read_bounded_regular_file(&path, CAPTURE_BUNDLE_READ_CAP) {
        Ok(raw) => raw,
        Err(BoundedReadError::NotFound) => return Err(ExportError::CaptureMissing(path)),
        Err(BoundedReadError::NotRegular(why)) => {
            return Err(ExportError::CaptureInvalid(path, why.to_string()));
        }
        Err(BoundedReadError::Oversized) => {
            return Err(ExportError::CaptureInvalid(
                path,
                format!("exceeds the {CAPTURE_BUNDLE_READ_CAP}-byte bundle read cap"),
            ));
        }
        Err(BoundedReadError::Io(e)) => return Err(ExportError::Io(e)),
    };
    let bundle: ContentCaptureBundle = serde_json::from_slice(&raw)
        .map_err(|e| ExportError::CaptureInvalid(path.clone(), classify_json_error(&e)))?;
    if bundle.schema != CONTENT_CAPTURE_SCHEMA {
        return Err(ExportError::CaptureInvalid(
            path,
            format!("unexpected schema tag {}", bundle.schema),
        ));
    }
    // The bundle must claim exactly the requested request ID (fix 3): a
    // bundle renamed or crafted to sit at another ID's path is refused —
    // the export never attributes content to a request it was not
    // captured for.
    if bundle.request_id != id {
        return Err(ExportError::CaptureInvalid(
            path,
            format!(
                "bundle request_id {} does not match requested id {}",
                bundle.request_id.as_str(),
                id.as_str()
            ),
        ));
    }
    let now = unix_ms_now();
    // Expiry: refuse and delete. Also treat a bundle claiming a lifetime
    // longer than the policy TTL as expired (clock tampering fail-closed).
    if now >= bundle.expires_unix_ms
        || bundle
            .expires_unix_ms
            .saturating_sub(bundle.created_unix_ms)
            > 2 * CAPTURE_TTL.as_millis() as u64
    {
        let _ = std::fs::remove_file(&path);
        return Err(ExportError::CaptureExpired(path));
    }
    if !bundle.redacted {
        let _ = std::fs::remove_file(&path);
        return Err(ExportError::CaptureInvalid(
            path,
            "bundle not marked redacted".to_string(),
        ));
    }
    let Some(mut body) = bundle.body else {
        let _ = std::fs::remove_file(&path);
        return Err(ExportError::CaptureOverBudget);
    };
    // Defense in depth: redact again at export time.
    let redactions = redact_value(&mut body);

    let export = ContentExport {
        schema: CONTENT_EXPORT_SCHEMA.to_string(),
        request_id: bundle.request_id,
        captured_unix_ms: bundle.created_unix_ms,
        exported_unix_ms: now,
        redacted: true,
        redactions_applied: redactions,
        body,
    };
    let mut file = create_private_new(output)?;
    let data = serde_json::to_vec_pretty(&export)
        .map_err(|e| ExportError::Io(std::io::Error::other(e)))?;
    file.write_all(&data)?;
    file.write_all(b"\n")?;
    file.flush()?;

    // Consume: the ephemeral capture never outlives its export.
    let _ = std::fs::remove_file(&path);
    Ok(())
}

/// Delete any expired capture bundles in `dir` (bounded retention sweep).
///
/// Expiry is *logical* (an expired bundle can never be exported — see
/// [`export_content`]); this sweep is the physical deletion invoked on
/// every trace interaction (new captures, exports, `/trace status`, CLI
/// trace subcommands) AND operationally at Runtime/session startup and in
/// both the sync and async shutdown epilogues — stale bundles do not wait
/// for the next explicit trace interaction.
///
/// Hardening: each entry is examined through the bounded regular-file
/// primitive — symlinks are never followed, FIFOs never block, directories
/// and devices are never read, and oversized or malformed files are
/// removed without unbounded reads. Work is bounded ([`MAX_SWEEP_ENTRIES`])
/// and confined to direct entries of `dir`; every failure is soft.
pub fn sweep_expired_captures(dir: &Path) -> u64 {
    sweep_expired_captures_except(dir, None)
}

/// Sweep, optionally keeping one path for the caller's own precise
/// expiry handling.
fn sweep_expired_captures_except(dir: &Path, keep: Option<&Path>) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let now = unix_ms_now();
    let mut removed = 0;
    for entry in entries.flatten().take(MAX_SWEEP_ENTRIES) {
        let path = entry.path();
        if keep.is_some_and(|k| k == path) {
            continue;
        }
        let expired = match read_bounded_regular_file(&path, CAPTURE_BUNDLE_READ_CAP) {
            Ok(raw) => serde_json::from_slice::<ContentCaptureBundle>(&raw)
                .map(|b| now >= b.expires_unix_ms)
                // Unparseable bundles are removed too: nothing else may
                // live here.
                .unwrap_or(true),
            // Vanished concurrently: nothing to remove.
            Err(BoundedReadError::NotFound) => continue,
            // Symlinks (removed as links, target untouched), FIFOs,
            // directories, devices, oversized or unreadable files: all
            // removable garbage — never read, never blocked on.
            Err(_) => true,
        };
        // `remove_file` never follows symlinks and fails softly on
        // directories — the sweep stays confined to `dir`'s own entries.
        if expired && std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::trace::{
        CollectingTraceSink, EndpointMeta, RequestAnatomy, TraceSchemaVersion, TransportKind,
        TransportOutcome,
    };
    use agent_core::TurnOutcome;

    fn sample_record(turn: &str, request: &str) -> RequestTrace {
        RequestTrace {
            schema: TraceSchemaVersion,
            session_id: TraceId::new("session-1").unwrap(),
            turn_id: TraceId::new(turn).unwrap(),
            request_id: TraceId::new(request).unwrap(),
            execution_events: Vec::new(),
            attempt: 1,
            model: agent_core::prompt::QualifiedModelId::parse("anthropic/claude-test").unwrap(),
            transport: TransportKind::AnthropicMessages,
            endpoint: EndpointMeta::new("api.anthropic.com", "/v1/messages").unwrap(),
            anatomy: RequestAnatomy::default(),
            wire: None,
            system_segments: Vec::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            cache: Default::default(),
            translation_losses: Vec::new(),
            outcome: TransportOutcome::unobserved(TurnOutcome::Completed),
        }
    }

    fn write_log(dir: &Path, records: &[RequestTrace]) -> PathBuf {
        let path = dir.join("request-trace.jsonl");
        let lines: Vec<String> = records
            .iter()
            .map(|r| serde_json::to_string(r).unwrap())
            .collect();
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        path
    }

    #[test]
    fn metadata_export_selects_exact_id_and_writes_private_file() {
        let _ = CollectingTraceSink::new(); // keep import used
        let dir = tempfile::tempdir().unwrap();
        let log = write_log(
            dir.path(),
            &[
                sample_record("turn-a", "req-a"),
                sample_record("turn-b", "req-b"),
                sample_record("turn-b", "req-c"),
            ],
        );
        let out = dir.path().join("out/export.jsonl");
        let stats = export_metadata(&log, "turn-b", &out).expect("export");
        assert_eq!(stats.scanned, 3);
        assert_eq!(stats.exported, 2);

        let data = std::fs::read_to_string(&out).unwrap();
        for line in data.lines() {
            let record: RequestTrace = serde_json::from_str(line).expect("schema-valid line");
            assert_eq!(record.turn_id.as_str(), "turn-b");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&out).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "export must be 0600");
            let parent_mode = std::fs::metadata(out.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(parent_mode, 0o700, "created parent must be 0700");
        }
    }

    #[test]
    fn metadata_export_matches_request_id_too() {
        let dir = tempfile::tempdir().unwrap();
        let log = write_log(dir.path(), &[sample_record("turn-a", "req-a")]);
        let out = dir.path().join("byreq.jsonl");
        let stats = export_metadata(&log, "req-a", &out).expect("export by request id");
        assert_eq!(stats.exported, 1);
    }

    #[test]
    fn metadata_export_refuses_invalid_line_missing_id_and_unsafe_targets() {
        let dir = tempfile::tempdir().unwrap();
        let log = write_log(dir.path(), &[sample_record("turn-a", "req-a")]);

        // Unknown ID.
        assert!(matches!(
            export_metadata(&log, "turn-zzz", &dir.path().join("x.jsonl")),
            Err(ExportError::NotFound(_))
        ));
        // Hostile ID.
        assert!(matches!(
            export_metadata(&log, "turn a\n", &dir.path().join("x.jsonl")),
            Err(ExportError::InvalidId(_))
        ));
        // Existing target refused.
        let existing = dir.path().join("existing.jsonl");
        std::fs::write(&existing, b"old").unwrap();
        assert!(matches!(
            export_metadata(&log, "turn-a", &existing),
            Err(ExportError::UnsafeTarget(..))
        ));
        // Symlink target refused.
        #[cfg(unix)]
        {
            let link = dir.path().join("link.jsonl");
            std::os::unix::fs::symlink(dir.path().join("elsewhere"), &link).unwrap();
            assert!(matches!(
                export_metadata(&log, "turn-a", &link),
                Err(ExportError::UnsafeTarget(..))
            ));
        }
        // A log with an invalid line fails closed.
        let bad = dir.path().join("bad.jsonl");
        std::fs::write(&bad, "{\"schema\":\"other/1\"}\n").unwrap();
        assert!(matches!(
            export_metadata(&bad, "turn-a", &dir.path().join("y.jsonl")),
            Err(ExportError::InvalidRecord { line: 1, .. })
        ));
    }

    #[test]
    fn redaction_is_recursive_over_nested_arrays_and_objects() {
        let mut value = serde_json::json!({
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "use Bearer abcdef123456 please"},
                    {"type": "text", "text": "harmless"}
                ]},
                {"nested": {"deep": {"api_key": "SENTINEL-KEY-1",
                                     "list": ["sk-SENTINELVALUE123456"]}}}
            ],
            "Authorization": "Bearer SENTINEL-TOKEN-2",
            "metadata": {"password": "SENTINEL-PASS-3"}
        });
        let count = redact_value(&mut value);
        let flat = value.to_string();
        assert!(count >= 4, "expected multiple redactions, got {count}");
        for sentinel in [
            "SENTINEL-KEY-1",
            "sk-SENTINELVALUE123456",
            "SENTINEL-TOKEN-2",
            "SENTINEL-PASS-3",
            "Bearer abcdef123456",
        ] {
            assert!(!flat.contains(sentinel), "sentinel survived: {sentinel}");
        }
        assert!(flat.contains("harmless"), "non-secret content preserved");
    }

    #[test]
    fn redaction_covers_jwts_urls_and_nested_structures() {
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
        let mut value = serde_json::json!({
            "messages": [
                {"role": "user", "content": format!("my session token is {jwt} thanks")},
                {"role": "user", "content":
                    "fetch https://api.example.com/v1/data?api_key=SENTINEL-QP-1&page=2&access_token=SENTINEL-QP-2#frag"},
                {"nested": [[{"deep": {"note": format!("anthropic key sk-ant-SENTINEL456789 here")}}]]},
            ],
            "auth_url": "https://x.test/cb?client_secret=SENTINEL-QP-3&state=ok",
            "pem": "-----BEGIN RSA PRIVATE KEY-----\nSENTINEL-PEM-4\n-----END RSA PRIVATE KEY-----",
            "raw_jwt": jwt,
        });
        let count = redact_value(&mut value);
        assert!(count >= 6, "expected >=6 redactions, got {count}");
        let flat = value.to_string();
        for sentinel in [
            jwt,
            "SENTINEL-QP-1",
            "SENTINEL-QP-2",
            "SENTINEL-QP-3",
            "sk-ant-SENTINEL456789",
            "SENTINEL-PEM-4",
        ] {
            assert!(!flat.contains(sentinel), "sentinel survived: {sentinel}");
        }
        // Non-secret URL structure survives: host, path, safe params.
        assert!(flat.contains("api.example.com/v1/data"), "url base kept");
        assert!(flat.contains("page=2"), "safe query param kept");
        assert!(flat.contains("state=ok"), "safe query param kept");
        assert!(flat.contains("thanks"), "surrounding text kept");
    }

    #[test]
    fn content_export_refuses_bundle_claiming_a_different_request_id() {
        let dir = tempfile::tempdir().unwrap();
        agent_core::core::private_fs::ensure_private_dir(dir.path()).unwrap();
        let claimed = TraceId::new("req-other").unwrap();
        let bundle = ContentCaptureBundle {
            schema: CONTENT_CAPTURE_SCHEMA.to_string(),
            request_id: claimed,
            created_unix_ms: unix_ms_now(),
            expires_unix_ms: unix_ms_now() + 60_000,
            redacted: true,
            over_budget: false,
            body: Some(serde_json::json!({"x": 1})),
        };
        // Plant the bundle at the path of a DIFFERENT id.
        let requested = TraceId::new("req-mine").unwrap();
        std::fs::write(
            capture_path(dir.path(), &requested),
            serde_json::to_vec(&bundle).unwrap(),
        )
        .unwrap();
        let out = dir.path().join("out.json");
        assert!(matches!(
            export_content(dir.path(), "req-mine", &out, true),
            Err(ExportError::CaptureInvalid(_, reason)) if reason.contains("does not match")
        ));
        assert!(!out.exists());
    }

    #[test]
    fn content_export_requires_opt_in_and_consumes_capture() {
        let dir = tempfile::tempdir().unwrap();
        let capture_dir = dir.path().join("cap");
        let id = TraceId::new("req-42").unwrap();
        let capture = crate::runtime::trace::controls::ContentCapture::new(capture_dir.clone());
        let body = serde_json::json!({
            "messages": [{"role": "user", "content": "hello sk-SENTINELSECRET9876543"}]
        });
        capture.capture(&id, serde_json::to_vec(&body).unwrap().as_slice());

        let out = dir.path().join("content.json");
        // Fail closed without the explicit opt-in.
        assert!(matches!(
            export_content(&capture_dir, "req-42", &out, false),
            Err(ExportError::ContentOptInMissing)
        ));
        assert!(!out.exists());

        export_content(&capture_dir, "req-42", &out, true).expect("content export");
        let data = std::fs::read_to_string(&out).unwrap();
        assert!(!data.contains("sk-SENTINELSECRET9876543"));
        let export: ContentExport = serde_json::from_str(&data).unwrap();
        assert_eq!(export.schema, CONTENT_EXPORT_SCHEMA);
        assert!(export.redacted);
        // The artifact must never parse as a RequestTrace.
        assert!(serde_json::from_str::<RequestTrace>(&data).is_err());
        // Consumed: the capture bundle is gone; a second export fails.
        assert!(matches!(
            export_content(&capture_dir, "req-42", &dir.path().join("again.json"), true),
            Err(ExportError::CaptureMissing(_))
        ));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&out).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn expired_capture_is_refused_and_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let id = TraceId::new("req-old").unwrap();
        let bundle = ContentCaptureBundle {
            schema: CONTENT_CAPTURE_SCHEMA.to_string(),
            request_id: id.clone(),
            created_unix_ms: 1000,
            expires_unix_ms: 2000, // long past
            redacted: true,
            over_budget: false,
            body: Some(serde_json::json!({"x": 1})),
        };
        agent_core::core::private_fs::ensure_private_dir(dir.path()).unwrap();
        let path = capture_path(dir.path(), &id);
        std::fs::write(&path, serde_json::to_vec(&bundle).unwrap()).unwrap();

        let out = dir.path().join("out.json");
        assert!(matches!(
            export_content(dir.path(), "req-old", &out, true),
            Err(ExportError::CaptureExpired(_))
        ));
        assert!(!path.exists(), "expired bundle must be deleted");
        assert!(!out.exists());
    }

    #[test]
    fn sweep_removes_expired_and_foreign_files() {
        let dir = tempfile::tempdir().unwrap();
        let id = TraceId::new("req-x").unwrap();
        let live = ContentCaptureBundle {
            schema: CONTENT_CAPTURE_SCHEMA.to_string(),
            request_id: id.clone(),
            created_unix_ms: unix_ms_now(),
            expires_unix_ms: unix_ms_now() + 60_000,
            redacted: true,
            over_budget: false,
            body: Some(serde_json::json!({})),
        };
        std::fs::write(
            capture_path(dir.path(), &id),
            serde_json::to_vec(&live).unwrap(),
        )
        .unwrap();
        std::fs::write(dir.path().join("garbage.json"), b"not a bundle").unwrap();
        let removed = sweep_expired_captures(dir.path());
        assert_eq!(removed, 1);
        assert!(capture_path(dir.path(), &id).exists());
    }

    fn live_bundle(id: &TraceId) -> ContentCaptureBundle {
        ContentCaptureBundle {
            schema: CONTENT_CAPTURE_SCHEMA.to_string(),
            request_id: id.clone(),
            created_unix_ms: unix_ms_now(),
            expires_unix_ms: unix_ms_now() + 60_000,
            redacted: true,
            over_budget: false,
            body: Some(serde_json::json!({})),
        }
    }

    /// Security regression (sweep hardening): the sweep must never follow a
    /// planted symlink — the link itself is removable garbage; the target
    /// outside the capture dir is untouched.
    #[cfg(unix)]
    #[test]
    fn sweep_removes_symlink_without_following_it() {
        let dir = tempfile::tempdir().unwrap();
        let cap_dir = dir.path().join("cap");
        std::fs::create_dir_all(&cap_dir).unwrap();
        // A live-looking bundle OUTSIDE the capture dir, targeted by a
        // symlink inside it. Reading through the link would see a live
        // bundle and keep the link forever.
        let id = TraceId::new("req-linked").unwrap();
        let target = dir.path().join("outside.json");
        std::fs::write(&target, serde_json::to_vec(&live_bundle(&id)).unwrap()).unwrap();
        let link = cap_dir.join("capture-req-linked.json");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let removed = sweep_expired_captures(&cap_dir);
        assert_eq!(removed, 1, "planted symlink must be swept");
        assert!(!link.exists(), "symlink must be removed");
        assert!(target.exists(), "symlink target must never be touched");
    }

    /// Security regression (sweep hardening): a planted FIFO must not block
    /// the sweep (the historical `read_to_string` open blocked forever
    /// waiting for a writer) and is removed without reading.
    #[cfg(unix)]
    #[test]
    fn sweep_does_not_block_on_planted_fifo_and_removes_it() {
        let dir = tempfile::tempdir().unwrap();
        let cap_dir = dir.path().join("cap");
        std::fs::create_dir_all(&cap_dir).unwrap();
        let fifo = cap_dir.join("capture-fifo.json");
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        // Keep a live bundle alongside to prove selectivity.
        let id = TraceId::new("req-live").unwrap();
        std::fs::write(
            capture_path(&cap_dir, &id),
            serde_json::to_vec(&live_bundle(&id)).unwrap(),
        )
        .unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let sweep_dir = cap_dir.clone();
        std::thread::spawn(move || {
            let _ = tx.send(sweep_expired_captures(&sweep_dir));
        });
        let removed = match rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(removed) => removed,
            Err(_) => {
                // Unblock the stuck reader thread before failing loudly.
                let _ = std::fs::OpenOptions::new().write(true).open(&fifo);
                panic!("sweep blocked on a planted FIFO");
            }
        };
        assert_eq!(removed, 1, "FIFO must be swept without blocking");
        assert!(!fifo.exists());
        assert!(capture_path(&cap_dir, &id).exists(), "live bundle survives");
    }

    /// Security regression (sweep hardening): oversized planted files are
    /// removed without an unbounded read/allocation; non-regular entries
    /// (a directory) never make the sweep read or panic.
    #[test]
    fn sweep_removes_oversized_file_without_unbounded_read() {
        let dir = tempfile::tempdir().unwrap();
        let cap_dir = dir.path().join("cap");
        std::fs::create_dir_all(&cap_dir).unwrap();
        // Sparse file far beyond the bundle read cap.
        let big = cap_dir.join("capture-big.json");
        let f = std::fs::File::create(&big).unwrap();
        f.set_len(CAPTURE_BUNDLE_READ_CAP + 1024 * 1024).unwrap();
        drop(f);
        // A directory entry: not removable via remove_file, but must be
        // skipped without reading or panicking.
        std::fs::create_dir(cap_dir.join("capture-subdir")).unwrap();
        let id = TraceId::new("req-live2").unwrap();
        std::fs::write(
            capture_path(&cap_dir, &id),
            serde_json::to_vec(&live_bundle(&id)).unwrap(),
        )
        .unwrap();

        let removed = sweep_expired_captures(&cap_dir);
        assert_eq!(removed, 1, "oversized file must be swept");
        assert!(!big.exists());
        assert!(capture_path(&cap_dir, &id).exists(), "live bundle survives");
    }

    /// Security regression (export hardening): a symlink planted at the
    /// capture path is refused via the O_NOFOLLOW open — never followed —
    /// and the out-of-dir target survives untouched.
    #[cfg(unix)]
    #[test]
    fn export_content_refuses_symlinked_capture_without_following() {
        let dir = tempfile::tempdir().unwrap();
        agent_core::core::private_fs::ensure_private_dir(dir.path()).unwrap();
        let id = TraceId::new("req-sym").unwrap();
        let target = dir.path().join("target-outside");
        std::fs::write(&target, serde_json::to_vec(&live_bundle(&id)).unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, capture_path(dir.path(), &id)).unwrap();
        let out = dir.path().join("out.json");
        assert!(matches!(
            export_content(dir.path(), "req-sym", &out, true),
            Err(ExportError::CaptureInvalid(..))
        ));
        assert!(!out.exists());
        assert!(target.exists(), "symlink target must never be consumed");
    }

    /// Redaction regression: URL fragments carry OAuth implicit-flow
    /// credentials (`#access_token=…`, `#id_token=…`) and must be scrubbed
    /// recursively, while benign fragment values survive.
    #[test]
    fn redaction_covers_url_fragments_recursively() {
        let mut value = serde_json::json!({
            "messages": [
                {"content": [{"text":
                    "see https://x.test/cb#access_token=SENTINEL-FRAG-1&state=keepme ok"}]},
                {"nested": {"deep": ["https://x.test/p?q=1#id_token=SENTINEL-FRAG-2"]}},
            ],
            "doc": "https://x.test/doc#section-2",
            "app": "https://x.test/app#page=intro&lang=en",
            "jwt_frag":
                "https://x.test/app#sess=eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.c2lnbmF0dXJl",
            "cred_frag": "https://x.test/app#v=AKIASENTINELFRAG34567",
        });
        let count = redact_value(&mut value);
        assert!(count >= 4, "expected >=4 fragment redactions, got {count}");
        let flat = value.to_string();
        for sentinel in [
            "SENTINEL-FRAG-1",
            "SENTINEL-FRAG-2",
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.c2lnbmF0dXJl",
            "AKIASENTINELFRAG34567",
        ] {
            assert!(!flat.contains(sentinel), "sentinel survived: {sentinel}");
        }
        assert!(flat.contains("state=keepme"), "benign fragment param kept");
        assert!(flat.contains("#section-2"), "benign fragment kept");
        assert!(flat.contains("page=intro"), "benign fragment param kept");
        assert!(flat.contains("lang=en"), "benign fragment param kept");
        assert!(
            flat.contains("https://x.test/cb#access_token="),
            "url shape kept"
        );
    }

    /// F5 regression: metadata export must never echo hostile persisted
    /// bytes back through its error values (Display or Debug).
    #[test]
    fn metadata_export_errors_never_echo_hostile_log_content() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("trace.jsonl");
        // Valid JSON, invalid record: serde's own message would echo the
        // string value verbatim.
        std::fs::write(&log, b"\"HOSTILE-SENTINEL-999\"\n").unwrap();
        let err = export_metadata(&log, "turn-a", &dir.path().join("o.jsonl"))
            .expect_err("invalid record must fail");
        let shown = format!("{err} / {err:?}");
        assert!(
            !shown.contains("HOSTILE-SENTINEL-999"),
            "hostile log content echoed in error: {shown}"
        );
        assert!(matches!(err, ExportError::InvalidRecord { line: 1, .. }));
    }

    /// F5 regression: metadata export streams with explicit per-line and
    /// total byte bounds instead of an unbounded whole-file read.
    #[test]
    fn metadata_export_enforces_line_and_total_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let record = sample_record("turn-a", "req-a");
        let line = serde_json::to_string(&record).unwrap();

        // Per-line bound: one line beyond the cap is refused.
        let log = dir.path().join("wide.jsonl");
        std::fs::write(&log, format!("{line}\n")).unwrap();
        let err = export_metadata_bounded(
            &log,
            "turn-a",
            &dir.path().join("o1.jsonl"),
            line.len() - 1,
            1024 * 1024,
        )
        .expect_err("over-cap line must fail");
        assert!(matches!(err, ExportError::BoundExceeded { .. }), "{err:?}");

        // Total bound: a log beyond the cap is refused.
        let log2 = dir.path().join("long.jsonl");
        std::fs::write(&log2, format!("{line}\n{line}\n")).unwrap();
        let err = export_metadata_bounded(
            &log2,
            "turn-a",
            &dir.path().join("o2.jsonl"),
            1024 * 1024,
            line.len() as u64 + 1,
        )
        .expect_err("over-cap log must fail");
        assert!(matches!(err, ExportError::BoundExceeded { .. }), "{err:?}");

        // Within bounds: exports normally.
        let stats = export_metadata_bounded(
            &log,
            "turn-a",
            &dir.path().join("o3.jsonl"),
            1024 * 1024,
            1024 * 1024,
        )
        .expect("in-bounds export");
        assert_eq!(stats.exported, 1);
    }
}
