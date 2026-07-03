//! Structured per-request API telemetry.
//!
//! Opt-in via the `telemetry` config key (`off` | `basic` | `full`).
//! Writes one JSON record per API call to `~/.cache/synaps/api-log.jsonl`
//! (mode 0600, O_NOFOLLOW — same hardening as the legacy usage log).
//!
//! `basic` records timing + usage + cost. `full` additionally records
//! rate-limit headers and cache-diagnostics results when available.
//!
//! Writes are best-effort: a broken log path must never break the request
//! pipeline. All errors are silently dropped (matching `log_usage`).

use serde::Serialize;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Telemetry verbosity level, parsed from the `telemetry` config key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TelemetryLevel {
    #[default]
    Off,
    Basic,
    Full,
}

impl TelemetryLevel {
    pub fn from_str_key(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "basic" | "on" | "1" | "true" => Self::Basic,
            "full" => Self::Full,
            _ => Self::Off,
        }
    }

    pub fn enabled(&self) -> bool {
        !matches!(self, Self::Off)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Basic => "basic",
            Self::Full => "full",
        }
    }
}

/// Token usage for one API call, including the cache-creation TTL breakdown.
#[derive(Debug, Clone, Default, Serialize)]
pub struct UsageRecord {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// Cache writes with 5-minute TTL (from `usage.cache_creation.ephemeral_5m_input_tokens`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write_5m: Option<u64>,
    /// Cache writes with 1-hour TTL (from `usage.cache_creation.ephemeral_1h_input_tokens`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write_1h: Option<u64>,
    /// Cache hit percentage: cache_read / (input + cache_read + cache_write) * 100.
    pub hit_pct: f64,
}

impl UsageRecord {
    pub fn compute_hit_pct(&mut self) {
        let total = self.input + self.cache_read + self.cache_write;
        self.hit_pct = if total > 0 {
            (self.cache_read as f64 / total as f64 * 1000.0).round() / 10.0
        } else {
            0.0
        };
    }
}

/// Rate-limit headroom captured from `anthropic-ratelimit-*` response headers.
/// Only recorded at `full` level.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RateLimitRecord {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requests_limit: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requests_remaining: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_limit: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_remaining: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens_remaining: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens_remaining: Option<u64>,
    /// RFC 3339 timestamp when the most restrictive token limit resets.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_reset: Option<String>,
}

impl RateLimitRecord {
    pub fn is_empty(&self) -> bool {
        self.requests_limit.is_none()
            && self.requests_remaining.is_none()
            && self.tokens_limit.is_none()
            && self.tokens_remaining.is_none()
            && self.input_tokens_remaining.is_none()
            && self.output_tokens_remaining.is_none()
            && self.tokens_reset.is_none()
    }
}

/// Cache-diagnostics result (beta `cache-diagnosis-2026-04-07`).
/// Only present when the user opted in via `cache_diagnostics = true`.
#[derive(Debug, Clone, Serialize)]
pub struct CacheDiagRecord {
    /// `cache_miss_reason.type` — e.g. "system_changed", "tools_changed".
    pub miss_reason: String,
    /// `cache_missed_input_tokens` — estimated tokens lost after divergence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub missed_tokens: Option<u64>,
}

/// Request-shape context: what we sent, for correlating cache behavior.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ContextRecord {
    pub messages: usize,
    pub tools: usize,
    pub system_bytes: usize,
    /// Indices of user messages carrying a conversational cache_control marker.
    pub breakpoints: Vec<usize>,
}

/// One JSONL record per API call.
#[derive(Debug, Clone, Default, Serialize)]
pub struct TelemetryRecord {
    /// Unix epoch milliseconds at request completion.
    pub ts: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// Anthropic message id (`msg_...`) from message_start.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub msg_id: Option<String>,
    pub model: String,
    /// 1-based attempt number that succeeded (1 = no retries).
    pub attempt: u32,
    /// Number of refusal retries consumed (0 = no refusals). Present only when > 0.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refusal_retries_used: Option<u32>,
    /// Milliseconds from request send to first SSE byte.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<u64>,
    /// Milliseconds from request send to stream close.
    pub total_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    pub usage: UsageRecord,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ratelimit: Option<RateLimitRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_diag: Option<CacheDiagRecord>,
    pub context: ContextRecord,
}

impl TelemetryRecord {
    pub fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

/// Parse a `u64` rate-limit header value.
fn header_u64(headers: &reqwest::header::HeaderMap, name: &str) -> Option<u64> {
    headers.get(name)?.to_str().ok()?.parse().ok()
}

/// Parse a string rate-limit header value.
fn header_string(headers: &reqwest::header::HeaderMap, name: &str) -> Option<String> {
    Some(headers.get(name)?.to_str().ok()?.to_string())
}

/// Extract rate-limit headroom from response headers.
pub fn ratelimit_from_headers(headers: &reqwest::header::HeaderMap) -> RateLimitRecord {
    RateLimitRecord {
        requests_limit: header_u64(headers, "anthropic-ratelimit-requests-limit"),
        requests_remaining: header_u64(headers, "anthropic-ratelimit-requests-remaining"),
        tokens_limit: header_u64(headers, "anthropic-ratelimit-tokens-limit"),
        tokens_remaining: header_u64(headers, "anthropic-ratelimit-tokens-remaining"),
        input_tokens_remaining: header_u64(headers, "anthropic-ratelimit-input-tokens-remaining"),
        output_tokens_remaining: header_u64(headers, "anthropic-ratelimit-output-tokens-remaining"),
        tokens_reset: header_string(headers, "anthropic-ratelimit-tokens-reset"),
    }
}

/// Extract the `request-id` response header.
pub fn request_id_from_headers(headers: &reqwest::header::HeaderMap) -> Option<String> {
    header_string(headers, "request-id")
}

/// Maximum delay we'll honour from any rate-limit header. A pathological reset
/// timestamp far in the future would otherwise stall the turn indefinitely.
pub const RETRY_DELAY_CAP: Duration = Duration::from_secs(60);

/// Compute how long to wait before the next retry, consulting response headers.
///
/// Priority order:
///   1. `retry-after` (integer seconds, or an HTTP-date — integer is what
///      Anthropic sends in practice, HTTP-date is handled as a best-effort
///      fallback).
///   2. `anthropic-ratelimit-tokens-reset` / `anthropic-ratelimit-requests-reset`
///      (RFC 3339 UTC timestamp) — take the *minimum* of whichever is present.
///   3. Classic exponential back-off: `1s * 2^(attempt-1)` (1 s, 2 s, 4 s …).
///
/// The returned duration is capped at [`RETRY_DELAY_CAP`] so a far-future
/// reset timestamp never hangs the turn forever. `attempt` is 1-based (first
/// retry = 1).
///
/// Returns `(delay, from_header)` — the bool is `true` when a header was
/// used (callers can emit a more informative notice message).
pub fn retry_delay_from_headers(
    headers: &reqwest::header::HeaderMap,
    attempt: u32,
) -> (Duration, bool) {
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // ── 1. retry-after (integer seconds preferred) ──────────────────────────
    if let Some(ra) = header_string(headers, "retry-after") {
        let ra = ra.trim();
        // Integer form: "30"
        if let Ok(secs) = ra.parse::<u64>() {
            let d = Duration::from_secs(secs).min(RETRY_DELAY_CAP);
            return (d, true);
        }
        // HTTP-date form: "Wed, 11 Jun 2025 01:46:00 GMT" — parse via chrono.
        if let Ok(dt) = chrono::DateTime::parse_from_rfc2822(ra) {
            let reset_secs = dt.timestamp().max(0) as u64;
            let wait = reset_secs.saturating_sub(now_secs);
            let d = Duration::from_secs(wait).min(RETRY_DELAY_CAP);
            return (d, true);
        }
    }

    // ── 2. anthropic-ratelimit-*-reset (RFC 3339) ───────────────────────────
    let mut min_wait: Option<u64> = None;
    for name in &[
        "anthropic-ratelimit-tokens-reset",
        "anthropic-ratelimit-requests-reset",
    ] {
        if let Some(ts) = header_string(headers, name) {
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts.trim()) {
                let reset_secs = dt.timestamp().max(0) as u64;
                let wait = reset_secs.saturating_sub(now_secs);
                min_wait = Some(min_wait.map_or(wait, |prev| prev.min(wait)));
            }
        }
    }
    if let Some(wait) = min_wait {
        let d = Duration::from_secs(wait).min(RETRY_DELAY_CAP);
        return (d, true);
    }

    // ── 3. Exponential back-off fallback ────────────────────────────────────
    let d = Duration::from_millis(1000 * 2u64.pow(attempt.saturating_sub(1)));
    (d, false)
}

/// Default telemetry log path: `~/.cache/synaps/api-log.jsonl`.
fn default_log_path() -> Option<std::path::PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(std::path::PathBuf::from(home).join(".cache/synaps/api-log.jsonl"))
}

/// Append a record to the telemetry log. Best-effort — all errors are
/// silently dropped so a broken log path never breaks the request pipeline.
///
/// File is created 0600 with O_NOFOLLOW (CWE-59 hardening, matching
/// `HelperMethods::log_usage`).
pub fn write_record(record: &TelemetryRecord) {
    let Some(path) = default_log_path() else { return };
    let Ok(line) = serde_json::to_string(record) else { return };

    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    // Size-capped rotation: at >50MB, rename to <path>.1 (clobbering any old
    // .1) before appending. One generation is enough — this is a diagnostic
    // log, not an audit trail. Errors silently dropped (best-effort contract).
    const MAX_BYTES: u64 = 50 * 1024 * 1024;
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > MAX_BYTES {
            let mut rotated = path.as_os_str().to_owned();
            rotated.push(".1");
            let _ = std::fs::rename(&path, std::path::PathBuf::from(rotated));
        }
    }

    use std::os::unix::fs::OpenOptionsExt;

    let result = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path);
    if let Ok(mut f) = result {
        use std::io::Write;
        let _ = writeln!(f, "{}", line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_parses_known_values() {
        assert_eq!(TelemetryLevel::from_str_key("off"), TelemetryLevel::Off);
        assert_eq!(TelemetryLevel::from_str_key("basic"), TelemetryLevel::Basic);
        assert_eq!(TelemetryLevel::from_str_key("full"), TelemetryLevel::Full);
        assert_eq!(TelemetryLevel::from_str_key("FULL"), TelemetryLevel::Full);
        assert_eq!(TelemetryLevel::from_str_key("true"), TelemetryLevel::Basic);
        assert_eq!(TelemetryLevel::from_str_key("garbage"), TelemetryLevel::Off);
        assert_eq!(TelemetryLevel::from_str_key(""), TelemetryLevel::Off);
    }

    #[test]
    fn level_enabled() {
        assert!(!TelemetryLevel::Off.enabled());
        assert!(TelemetryLevel::Basic.enabled());
        assert!(TelemetryLevel::Full.enabled());
    }

    #[test]
    fn hit_pct_computation() {
        let mut u = UsageRecord {
            input: 100,
            cache_read: 800,
            cache_write: 100,
            ..Default::default()
        };
        u.compute_hit_pct();
        assert_eq!(u.hit_pct, 80.0);
    }

    #[test]
    fn hit_pct_zero_total() {
        let mut u = UsageRecord::default();
        u.compute_hit_pct();
        assert_eq!(u.hit_pct, 0.0);
    }

    #[test]
    fn hit_pct_rounds_to_one_decimal() {
        let mut u = UsageRecord {
            input: 1,
            cache_read: 2,
            cache_write: 0,
            ..Default::default()
        };
        u.compute_hit_pct();
        assert_eq!(u.hit_pct, 66.7);
    }

    #[test]
    fn record_serializes_skipping_none_fields() {
        let record = TelemetryRecord {
            ts: 1,
            model: "claude-sonnet-4-6".to_string(),
            attempt: 1,
            total_ms: 100,
            ..Default::default()
        };
        let json = serde_json::to_string(&record).unwrap();
        assert!(!json.contains("request_id"));
        assert!(!json.contains("ratelimit"));
        assert!(!json.contains("cache_diag"));
        assert!(json.contains("\"model\":\"claude-sonnet-4-6\""));
    }

    #[test]
    fn ratelimit_empty_detection() {
        assert!(RateLimitRecord::default().is_empty());
        let r = RateLimitRecord {
            requests_remaining: Some(10),
            ..Default::default()
        };
        assert!(!r.is_empty());
    }

    #[test]
    fn ratelimit_parses_headers() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("anthropic-ratelimit-requests-limit", "5000".parse().unwrap());
        headers.insert("anthropic-ratelimit-requests-remaining", "4900".parse().unwrap());
        headers.insert("anthropic-ratelimit-tokens-reset", "2026-06-11T01:46:00Z".parse().unwrap());
        let r = ratelimit_from_headers(&headers);
        assert_eq!(r.requests_limit, Some(5000));
        assert_eq!(r.requests_remaining, Some(4900));
        assert_eq!(r.tokens_reset.as_deref(), Some("2026-06-11T01:46:00Z"));
        assert_eq!(r.tokens_limit, None);
    }

    #[test]
    fn ratelimit_ignores_malformed_values() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("anthropic-ratelimit-requests-limit", "not-a-number".parse().unwrap());
        let r = ratelimit_from_headers(&headers);
        assert_eq!(r.requests_limit, None);
    }
}

#[cfg(test)]
mod retry_delay_tests {
    use super::*;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[test]
    fn integer_retry_after() {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert("retry-after", "45".parse().unwrap());
        let (d, from_hdr) = retry_delay_from_headers(&h, 1);
        assert_eq!(d, Duration::from_secs(45));
        assert!(from_hdr);
    }

    #[test]
    fn integer_retry_after_capped() {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert("retry-after", "300".parse().unwrap()); // 5 min — beyond cap
        let (d, from_hdr) = retry_delay_from_headers(&h, 1);
        assert_eq!(d, RETRY_DELAY_CAP);
        assert!(from_hdr);
    }

    #[test]
    fn rfc3339_reset_future() {
        let future_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 30;
        let dt = chrono::DateTime::<chrono::Utc>::from_timestamp(future_secs as i64, 0).unwrap();
        let ts = dt.to_rfc3339();

        let mut h = reqwest::header::HeaderMap::new();
        h.insert("anthropic-ratelimit-tokens-reset", ts.parse().unwrap());
        let (d, from_hdr) = retry_delay_from_headers(&h, 1);
        assert!(d.as_secs() >= 28 && d.as_secs() <= 32, "unexpected delay: {:?}", d);
        assert!(from_hdr);
    }

    #[test]
    fn rfc3339_reset_beyond_cap() {
        let future_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 600;
        let dt = chrono::DateTime::<chrono::Utc>::from_timestamp(future_secs as i64, 0).unwrap();
        let ts = dt.to_rfc3339();

        let mut h = reqwest::header::HeaderMap::new();
        h.insert("anthropic-ratelimit-tokens-reset", ts.parse().unwrap());
        let (d, from_hdr) = retry_delay_from_headers(&h, 1);
        assert_eq!(d, RETRY_DELAY_CAP);
        assert!(from_hdr);
    }

    #[test]
    fn no_headers_exponential_fallback() {
        let h = reqwest::header::HeaderMap::new();
        let (d1, hdr1) = retry_delay_from_headers(&h, 1);
        let (d2, hdr2) = retry_delay_from_headers(&h, 2);
        let (d3, hdr3) = retry_delay_from_headers(&h, 3);
        assert_eq!(d1, Duration::from_secs(1));
        assert_eq!(d2, Duration::from_secs(2));
        assert_eq!(d3, Duration::from_secs(4));
        assert!(!hdr1 && !hdr2 && !hdr3);
    }

    #[test]
    fn prefers_retry_after_over_rfc3339() {
        let future_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 30;
        let dt = chrono::DateTime::<chrono::Utc>::from_timestamp(future_secs as i64, 0).unwrap();
        let ts = dt.to_rfc3339();

        let mut h = reqwest::header::HeaderMap::new();
        h.insert("retry-after", "10".parse().unwrap());
        h.insert("anthropic-ratelimit-tokens-reset", ts.parse().unwrap());
        let (d, from_hdr) = retry_delay_from_headers(&h, 1);
        assert_eq!(d, Duration::from_secs(10));
        assert!(from_hdr);
    }

    #[test]
    fn min_of_multiple_ratelimit_reset_headers() {
        // tokens-reset is sooner; requests-reset is later — should pick tokens.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let tokens_dt = chrono::DateTime::<chrono::Utc>::from_timestamp((now + 15) as i64, 0).unwrap();
        let requests_dt = chrono::DateTime::<chrono::Utc>::from_timestamp((now + 45) as i64, 0).unwrap();

        let mut h = reqwest::header::HeaderMap::new();
        h.insert("anthropic-ratelimit-tokens-reset", tokens_dt.to_rfc3339().parse().unwrap());
        h.insert("anthropic-ratelimit-requests-reset", requests_dt.to_rfc3339().parse().unwrap());
        let (d, from_hdr) = retry_delay_from_headers(&h, 1);
        assert!(d.as_secs() >= 13 && d.as_secs() <= 17, "should be ~15s, got {:?}", d);
        assert!(from_hdr);
    }
}
