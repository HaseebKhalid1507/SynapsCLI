//! Structured per-request API telemetry.
//!
//! Opt-in via the `telemetry` config key (`off` | `basic` | `full`).
//! Writes one JSON record per API call to `~/.cache/synaps/api-log.jsonl`
//! (mode 0600, O_NOFOLLOW — same hardening as the legacy usage log).
//!
//! `basic` records timing, usage, an estimated `cost_usd` (from
//! [`agent_core::pricing`]), the thinking level of the request (`thinking`,
//! and `effort` when the request carried one), the persisted conversation
//! id (`session_id`, the `~/.synaps-cli/sessions/<id>.json` id) and the request-trace
//! correlation ids (`runtime_session_id` / `turn_id` / `trace_request_id`),
//! so a record joins both its session and its `request-trace.jsonl` line.
//! Worker records also carry the worker's identity (`worker_agent`,
//! `worker_handle`, `parent_session_id`, `parent_runtime_session_id`).
//! After an automatic context rollover a record names its context window
//! (`rollover_window`), and the window's first request says `rollover: true`.
//! `full` additionally records rate-limit headers: the per-minute limits of
//! an API key, or on a subscription login the usage windows
//! (`ratelimit.unified_utilization`: how much of the 5-hour and weekly
//! allowance is used, as of that response). The `cache_diag` verdict is
//! present at either level when `cache_diagnostics` is on.
//!
//! Optional fields are omitted when unknown — never written as `0`.
//!
//! Writes are best-effort and non-blocking: persistence goes through the
//! bounded background [`writer`] (Task 11, spec §6.5), so a slow or broken
//! log path can never delay or fail the request pipeline. Failures are
//! counted (never silent-and-invisible) with one warning per persistent
//! failure class.

use serde::Serialize;
use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub mod writer;

pub use writer::{
    default_telemetry_log_path, default_trace_log_path, ShutdownOutcome, TelemetryWriter,
    WriteGate, WriterOptions, WriterStats, WriterTraceSink, DEFAULT_MAX_FILE_BYTES,
    DEFAULT_MAX_GENERATIONS, DEFAULT_QUEUE_CAPACITY, DEFAULT_SHUTDOWN_FLUSH_TIMEOUT,
};

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

/// The parent session's telemetry, handed to the subagents it spawns.
///
/// Workers record at the parent's level and enqueue onto the parent's
/// session writer instead of starting their own: the writer owns its log
/// files from a single thread, so a writer per worker would race rotation.
/// See `Runtime::inherit_telemetry`.
#[derive(Debug, Clone, Default)]
pub struct WorkerTelemetry {
    pub level: TelemetryLevel,
    /// The parent's session writer. `None` when the parent records nothing.
    pub writer: Option<TelemetryWriter>,
    /// The parent's persisted conversation id (the
    /// `~/.synaps-cli/sessions/<id>.json` id), stamped on the worker's
    /// records as `parent_session_id`. `None` when the parent is unkeyed.
    pub parent_session_id: Option<String>,
    /// The parent's runtime trace session id (`session-<pid>-<ms>-<seq>`),
    /// stamped as `parent_runtime_session_id`.
    pub parent_runtime_session_id: Option<String>,
    /// Set per spawn by [`Self::for_worker`]: the agent name.
    pub agent: Option<String>,
    /// Set per spawn by [`Self::for_worker`]: the subagent handle (`sa_N`).
    pub handle: Option<String>,
}

impl WorkerTelemetry {
    /// This telemetry, labelled for one spawned worker.
    pub fn for_worker(mut self, agent: &str, handle: &str) -> Self {
        self.agent = Some(agent.to_string());
        self.handle = Some(handle.to_string());
        self
    }

    /// The parent's writer, re-stamped for this worker: every record the
    /// worker enqueues carries its identity and its parent's ids, and NOT
    /// the parent's own `session_id`.
    pub fn worker_writer(&self) -> Option<TelemetryWriter> {
        self.writer.as_ref().map(|w| {
            w.with_stamp(RecordStamp {
                session_id: None,
                worker_agent: self.agent.clone(),
                worker_handle: self.handle.clone(),
                parent_session_id: self.parent_session_id.clone(),
                parent_runtime_session_id: self.parent_runtime_session_id.clone(),
            })
        })
    }
}

/// Identity fields a [`TelemetryWriter`] handle stamps onto every record
/// enqueued through it: the persisted conversation id of the session (see
/// `Runtime::request_telemetry`) and, on worker handles, who the worker is
/// (see [`WorkerTelemetry::worker_writer`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecordStamp {
    pub session_id: Option<String>,
    pub worker_agent: Option<String>,
    pub worker_handle: Option<String>,
    pub parent_session_id: Option<String>,
    pub parent_runtime_session_id: Option<String>,
}

impl RecordStamp {
    /// Fill the fields a record does not already carry.
    pub fn apply(&self, record: &mut TelemetryRecord) {
        fn fill(slot: &mut Option<String>, value: &Option<String>) {
            if slot.is_none() {
                slot.clone_from(value);
            }
        }
        fill(&mut record.session_id, &self.session_id);
        fill(&mut record.worker_agent, &self.worker_agent);
        fill(&mut record.worker_handle, &self.worker_handle);
        fill(&mut record.parent_session_id, &self.parent_session_id);
        fill(
            &mut record.parent_runtime_session_id,
            &self.parent_runtime_session_id,
        );
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
    /// Estimated USD cost via [`agent_core::pricing`] (the single pricing
    /// source), billing 5m and 1h cache writes at their own rates. `None`
    /// when no usage was reported (all counts zero) — unknown, not free.
    pub fn cost_usd(&self, model: &str) -> Option<f64> {
        if self.input == 0 && self.output == 0 && self.cache_read == 0 && self.cache_write == 0 {
            return None;
        }
        Some(agent_core::pricing::calculate_cost_optional_split(
            model,
            self.input,
            self.output,
            self.cache_read,
            self.cache_write,
            self.cache_write_5m,
            self.cache_write_1h,
        ))
    }

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
    /// Subscription (OAuth) usage windows, from the
    /// `anthropic-ratelimit-unified-*` headers, when the response carries
    /// them (a subscription login has none of the per-minute limits above).
    ///
    /// `anthropic-ratelimit-unified-status`: `allowed`, `allowed_warning` or
    /// `rejected`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unified_status: Option<String>,
    /// Unix seconds at which the window the status speaks for resets
    /// (`anthropic-ratelimit-unified-reset`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unified_reset: Option<u64>,
    /// The window the status speaks for
    /// (`anthropic-ratelimit-unified-representative-claim`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unified_claim: Option<String>,
    /// Used share of every window the response reported
    /// (`anthropic-ratelimit-unified-<window>-utilization`), keyed by the
    /// provider's own window name (`5h`, `7d`, `7d_oi`, ...): `0.62` is 62%
    /// used. The window names are not a fixed list; whatever the provider
    /// sends is recorded.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub unified_utilization: BTreeMap<String, f64>,
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
            && self.unified_status.is_none()
            && self.unified_reset.is_none()
            && self.unified_claim.is_none()
            && self.unified_utilization.is_empty()
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
    /// The thinking level the request was made at, by its config name
    /// (`off`, `adaptive`, `low`, `medium`, `high`, `xhigh`, `max`, ...): the
    /// session's level, or on a worker record the worker's own.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    /// The `output_config.effort` value the request carried. Absent when it
    /// carried none: thinking off or adaptive, or a model that takes a token
    /// budget instead of an effort.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// 1-based attempt number that succeeded (1 = no retries).
    pub attempt: u32,
    /// Number of refusal retries consumed (0 = no refusals). Present only when > 0.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refusal_retries_used: Option<u32>,
    /// Milliseconds from request send to the first parsed model event — the
    /// request trace's `outcome.timings.first_model_event_ms`. Absent when no
    /// event arrived, never `0`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<u64>,
    /// Milliseconds from request send to stream close.
    pub total_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    pub usage: UsageRecord,
    /// Estimated USD cost of this call at API list prices
    /// ([`UsageRecord::cost_usd`]), also for OAuth/subscription auth.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ratelimit: Option<RateLimitRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_diag: Option<CacheDiagRecord>,
    pub context: ContextRecord,
    /// `true` only on prompt-cache keep-warm pings (task #444) so telemetry
    /// can separate pings from real turns. Absent on every turn record.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub keepwarm: bool,
    /// `true` when a subagent (worker runtime) made the request, so reports
    /// can split delegated traffic from the foreground session. Absent on
    /// foreground and internal records.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub worker: bool,
    /// Persisted conversation id (`20261028-045932-eb8e`): the id of
    /// `~/.synaps-cli/sessions/<id>.json` and `synaps daemon sessions`.
    /// Absent for unkeyed runtimes (workers, tests).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Runtime trace session id (`session-<pid>-<ms>-<seq>`): the `session_id` of
    /// this call's `request-trace.jsonl` record. Not the persisted id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_session_id: Option<String>,
    /// Request-trace turn id (`turn-<pid>-<n>`). Despite the name it is
    /// minted per logical request (same counter as `trace_request_id`), not
    /// per user turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    /// Request-trace request id (`req-<pid>-<n>`), shared by every attempt
    /// of one logical request. Not the provider's `request_id`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_request_id: Option<String>,
    /// Worker records: the agent name (`spike`), or for an inline subagent
    /// its sanitized label (model-chosen text, `[A-Za-z0-9_-]`, bounded).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_agent: Option<String>,
    /// Worker records: the subagent handle (`sa_N`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_handle: Option<String>,
    /// Worker records: the spawning session's persisted `session_id`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// Worker records: the spawning session's `runtime_session_id`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_runtime_session_id: Option<String>,
    /// The context window (2, 3, …) this request belongs to after an
    /// automatic context rollover, worker or foreground. Absent before the
    /// first rollover.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rollover_window: Option<u64>,
    /// `true` only on the first request of a rolled-over window: the request
    /// that writes the successor's prompt cache. Absent otherwise.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub rollover: bool,
}

/// What a turn request tells its telemetry record about context rollover
/// (see [`TelemetryRecord::rollover_window`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RolloverStamp {
    /// The context window the request belongs to (2 after one rollover).
    pub window: u64,
    /// The first request of that window.
    pub first_request: bool,
}

impl TelemetryRecord {
    /// Fill `runtime_session_id` / `turn_id` / `trace_request_id` from the
    /// request trace's correlation ids. Without a tracer, `session` alone
    /// still names the runtime session.
    pub fn set_correlation(
        &mut self,
        session: &crate::runtime::trace::TraceId,
        ids: Option<(
            &crate::runtime::trace::TraceId,
            &crate::runtime::trace::TraceId,
            &crate::runtime::trace::TraceId,
        )>,
    ) {
        let (session, turn, request) = match ids {
            Some((s, t, r)) => (s, Some(t), Some(r)),
            None => (session, None, None),
        };
        self.runtime_session_id = Some(session.as_str().to_string());
        self.turn_id = turn.map(|t| t.as_str().to_string());
        self.trace_request_id = request.map(|r| r.as_str().to_string());
    }

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
        unified_status: header_string(headers, "anthropic-ratelimit-unified-status"),
        unified_reset: header_u64(headers, "anthropic-ratelimit-unified-reset"),
        unified_claim: header_string(
            headers,
            "anthropic-ratelimit-unified-representative-claim",
        ),
        unified_utilization: unified_utilization_from_headers(headers),
    }
}

/// Most usage windows recorded from one response, and the longest window
/// name kept: a provider that sends more, or longer, is not trusted to bound
/// the record by itself.
const MAX_UNIFIED_WINDOWS: usize = 16;
const MAX_UNIFIED_WINDOW_NAME: usize = 32;

/// Every `anthropic-ratelimit-unified-<window>-utilization` header, as
/// `window -> used share`. A value that is not a finite, non-negative number
/// is skipped: unknown is not recorded as zero.
fn unified_utilization_from_headers(headers: &reqwest::header::HeaderMap) -> BTreeMap<String, f64> {
    const PREFIX: &str = "anthropic-ratelimit-unified-";
    const SUFFIX: &str = "-utilization";
    let mut windows = BTreeMap::new();
    for (name, value) in headers {
        let Some(window) = name
            .as_str()
            .strip_prefix(PREFIX)
            .and_then(|rest| rest.strip_suffix(SUFFIX))
        else {
            continue;
        };
        if window.is_empty() || window.len() > MAX_UNIFIED_WINDOW_NAME {
            continue;
        }
        let Some(used) = value.to_str().ok().and_then(|v| v.trim().parse::<f64>().ok()) else {
            continue;
        };
        if !used.is_finite() || used < 0.0 {
            continue;
        }
        if windows.len() >= MAX_UNIFIED_WINDOWS && !windows.contains_key(window) {
            continue;
        }
        windows.insert(window.to_string(), used);
    }
    windows
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
        assert!(!json.contains("thinking"), "unknown level is not written");
        assert!(!json.contains("effort"));
        assert!(!json.contains("ratelimit"));
        assert!(!json.contains("cache_diag"));
        assert!(!json.contains("keepwarm"), "turn records carry no keepwarm field");
        assert!(!json.contains("worker"), "foreground records carry no worker field");
        assert!(json.contains("\"model\":\"claude-sonnet-4-6\""));
    }

    #[test]
    fn rollover_fields_appear_only_after_a_rollover() {
        let plain = serde_json::to_string(&TelemetryRecord::default()).unwrap();
        assert!(!plain.contains("rollover"), "{plain}");
        let first = TelemetryRecord {
            rollover_window: Some(2),
            rollover: true,
            worker: true,
            ..Default::default()
        };
        let json = serde_json::to_string(&first).unwrap();
        assert!(json.contains("\"rollover_window\":2"), "{json}");
        assert!(json.contains("\"rollover\":true"), "{json}");
        let later = TelemetryRecord {
            rollover_window: Some(2),
            ..Default::default()
        };
        let json = serde_json::to_string(&later).unwrap();
        assert!(json.contains("\"rollover_window\":2"), "{json}");
        assert!(!json.contains("\"rollover\":"), "{json}");
    }

    #[test]
    fn keepwarm_record_is_marked() {
        let record = TelemetryRecord {
            ts: 1,
            model: "claude-opus-4-7".to_string(),
            keepwarm: true,
            ..Default::default()
        };
        let json = serde_json::to_string(&record).unwrap();
        assert!(json.contains("\"keepwarm\":true"), "{json}");
    }

    #[test]
    fn worker_record_is_marked() {
        let record = TelemetryRecord {
            ts: 1,
            model: "claude-opus-5-5".to_string(),
            worker: true,
            ..Default::default()
        };
        let json = serde_json::to_string(&record).unwrap();
        assert!(json.contains("\"worker\":true"), "{json}");
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
        headers.insert(
            "anthropic-ratelimit-requests-limit",
            "5000".parse().unwrap(),
        );
        headers.insert(
            "anthropic-ratelimit-requests-remaining",
            "4900".parse().unwrap(),
        );
        headers.insert(
            "anthropic-ratelimit-tokens-reset",
            "2026-06-11T01:46:00Z".parse().unwrap(),
        );
        let r = ratelimit_from_headers(&headers);
        assert_eq!(r.requests_limit, Some(5000));
        assert_eq!(r.requests_remaining, Some(4900));
        assert_eq!(r.tokens_reset.as_deref(), Some("2026-06-11T01:46:00Z"));
        assert_eq!(r.tokens_limit, None);
    }

    #[test]
    fn ratelimit_ignores_malformed_values() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "anthropic-ratelimit-requests-limit",
            "not-a-number".parse().unwrap(),
        );
        let r = ratelimit_from_headers(&headers);
        assert_eq!(r.requests_limit, None);
    }

    fn headers_of(pairs: &[(&'static str, &str)]) -> reqwest::header::HeaderMap {
        let mut headers = reqwest::header::HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(*name, value.parse().unwrap());
        }
        headers
    }

    /// The header set a subscription account sent with its weekly rejection
    /// on 2026-11-11: every window's used share is recorded under the
    /// provider's own name, with the status, its window and the reset.
    #[test]
    fn ratelimit_records_subscription_usage_windows() {
        let headers = headers_of(&[
            ("anthropic-ratelimit-unified-7d-reset", "1794808800"),
            ("anthropic-ratelimit-unified-7d-status", "allowed"),
            ("anthropic-ratelimit-unified-7d-utilization", "0.62"),
            ("anthropic-ratelimit-unified-7d_oi-status", "rejected"),
            ("anthropic-ratelimit-unified-7d_oi-surpassed-threshold", "1.0"),
            ("anthropic-ratelimit-unified-7d_oi-utilization", "1.0"),
            ("anthropic-ratelimit-unified-fallback-percentage", "0.5"),
            (
                "anthropic-ratelimit-unified-representative-claim",
                "seven_day_overage_included",
            ),
            ("anthropic-ratelimit-unified-reset", "1794808800"),
            ("anthropic-ratelimit-unified-status", "rejected"),
        ]);
        let r = ratelimit_from_headers(&headers);
        assert!(!r.is_empty());
        assert_eq!(r.unified_status.as_deref(), Some("rejected"));
        assert_eq!(r.unified_reset, Some(1_794_808_800));
        assert_eq!(r.unified_claim.as_deref(), Some("seven_day_overage_included"));
        assert_eq!(
            r.unified_utilization,
            BTreeMap::from([("7d".to_string(), 0.62), ("7d_oi".to_string(), 1.0)]),
            "utilization headers only: not the threshold, not the fallback share"
        );
        assert_eq!(r.requests_limit, None, "no per-minute limits on a subscription");
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "unified_status": "rejected",
                "unified_reset": 1_794_808_800u64,
                "unified_claim": "seven_day_overage_included",
                "unified_utilization": {"7d": 0.62, "7d_oi": 1.0}
            })
        );
    }

    /// A window whose value is not a usable number is left out (unknown is
    /// not zero), a nameless or oversized window name is dropped, and a
    /// record with nothing in it stays empty and serializes without the map.
    #[test]
    fn ratelimit_skips_unusable_usage_windows() {
        let long = format!(
            "anthropic-ratelimit-unified-{}-utilization",
            "w".repeat(MAX_UNIFIED_WINDOW_NAME + 1)
        );
        let mut headers = headers_of(&[
            ("anthropic-ratelimit-unified-5h-utilization", "0.25"),
            ("anthropic-ratelimit-unified-7d-utilization", "n/a"),
            ("anthropic-ratelimit-unified-1d-utilization", "NaN"),
            ("anthropic-ratelimit-unified-30d-utilization", "-0.1"),
            ("anthropic-ratelimit-unified--utilization", "0.5"),
        ]);
        headers.insert(
            reqwest::header::HeaderName::from_bytes(long.as_bytes()).unwrap(),
            "0.5".parse().unwrap(),
        );
        let r = ratelimit_from_headers(&headers);
        assert_eq!(
            r.unified_utilization,
            BTreeMap::from([("5h".to_string(), 0.25)])
        );

        let none = ratelimit_from_headers(&reqwest::header::HeaderMap::new());
        assert!(none.is_empty());
        assert_eq!(serde_json::to_value(&none).unwrap(), serde_json::json!({}));

        // More windows than the bound: the record stops growing.
        let mut many = reqwest::header::HeaderMap::new();
        for i in 0..(MAX_UNIFIED_WINDOWS + 5) {
            many.insert(
                reqwest::header::HeaderName::from_bytes(
                    format!("anthropic-ratelimit-unified-w{i}-utilization").as_bytes(),
                )
                .unwrap(),
                "0.1".parse().unwrap(),
            );
        }
        assert_eq!(
            ratelimit_from_headers(&many).unified_utilization.len(),
            MAX_UNIFIED_WINDOWS
        );
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
        assert!(
            d.as_secs() >= 28 && d.as_secs() <= 32,
            "unexpected delay: {:?}",
            d
        );
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
        let tokens_dt =
            chrono::DateTime::<chrono::Utc>::from_timestamp((now + 15) as i64, 0).unwrap();
        let requests_dt =
            chrono::DateTime::<chrono::Utc>::from_timestamp((now + 45) as i64, 0).unwrap();

        let mut h = reqwest::header::HeaderMap::new();
        h.insert(
            "anthropic-ratelimit-tokens-reset",
            tokens_dt.to_rfc3339().parse().unwrap(),
        );
        h.insert(
            "anthropic-ratelimit-requests-reset",
            requests_dt.to_rfc3339().parse().unwrap(),
        );
        let (d, from_hdr) = retry_delay_from_headers(&h, 1);
        assert!(
            d.as_secs() >= 13 && d.as_secs() <= 17,
            "should be ~15s, got {:?}",
            d
        );
        assert!(from_hdr);
    }
}

/// Headerless network retry delay. Bounded even for arbitrarily large config
/// values; matches the streaming error path's 64-second ceiling.
pub(super) fn network_retry_delay(attempt: u32) -> Duration {
    Duration::from_secs(1u64 << attempt.saturating_sub(1).min(6))
}

#[cfg(test)]
mod network_backoff_tests {
    #[test]
    fn configured_large_retry_counts_never_overflow() {
        let seconds: Vec<_> = [0, 1, 2, 3, 7, 8, u32::MAX]
            .into_iter()
            .map(|n| super::network_retry_delay(n).as_secs())
            .collect();
        assert_eq!(seconds, [1, 1, 2, 4, 64, 64, 64]);
    }
}
