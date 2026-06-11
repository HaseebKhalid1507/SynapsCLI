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
use std::time::{SystemTime, UNIX_EPOCH};

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

    use std::os::unix::fs::OpenOptionsExt;
    #[cfg(target_os = "linux")]
    const O_NOFOLLOW_FLAG: i32 = 0o400000;
    #[cfg(target_os = "macos")]
    const O_NOFOLLOW_FLAG: i32 = 0x0100;
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    const O_NOFOLLOW_FLAG: i32 = 0;

    let result = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(O_NOFOLLOW_FLAG)
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
