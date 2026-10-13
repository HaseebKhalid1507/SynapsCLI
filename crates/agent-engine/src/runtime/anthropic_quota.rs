//! Window-exhaustion recognition and bounded account failover for the
//! Anthropic Messages transport (plan `auto-chooser-soonest-reset`, G3).
//!
//! Three concerns live here, deliberately separated so each is testable
//! without the others (mirrors `openai/account_routing.rs`):
//!
//! 1. **Wire evidence** — pure classification of what Anthropic said on a
//!    429. A window-exhausted seat is recognized ONLY from provider-declared
//!    signals: the `anthropic-ratelimit-unified-status: rejected` header
//!    (or a per-window `…-unified-5h-status` / `…-unified-7d-status`), or —
//!    secondary — a `rate_limit_error` body TOGETHER with a reset/`retry-after`
//!    at least [`LONG_RESET_MIN_SECS`] away. Anything else is an ordinary 429
//!    and keeps the existing retry/backoff. Response bodies are consulted
//!    from the caller's bounded buffer for classification only: they can echo
//!    the whole prompt and are never logged, stored or surfaced.
//! 2. **Runtime gate** — the preconditions the request loop enforces before
//!    it even asks for a failover candidate: the selector must be `Auto` (an
//!    explicit account is never overridden), nothing may have been streamed
//!    on this request (no cross-account replay), no tool block may have been
//!    produced, and at most [`MAX_ANTHROPIC_ACCOUNT_FAILOVERS`] switches per
//!    request. These are the runtime's own safety invariants, not the
//!    capacity policy (that stays in `auth::quota_policy` behind the broker).
//! 3. **Router seam** — [`AnthropicAccountRouter`] is what the request loop
//!    talks to. [`BrokerAnthropicRouter`] adapts the `CredentialBroker`
//!    trait (report the cooldown, ask the broker to select again, REQUIRE a
//!    different seat); tests script the rest. The seam returns one
//!    `PinnedToken` per hop so the credential the loop binds and the bearer
//!    it sends are always the same vend.
//!
//! The hop is an explicit, gated exception to the mid-turn seat pin (commit
//! 95ab4299): the loop re-binds `AuthState` to the new seat, so the rest of
//! the turn stays on it and the next turn boundary re-selects as usual.

use async_trait::async_trait;
use reqwest::header::HeaderMap;
use serde_json::Value;
use std::sync::{Arc, OnceLock};

use crate::auth::{
    broker_from_source, AccountSelector, BrokerError, CredentialBroker, CredentialRef,
    CredentialSource, OAuthProviderId, PinnedToken, TokenCache,
};

// ── Wire evidence ────────────────────────────────────────────────────────────

/// A `rate_limit_error` body only counts as exhaustion when the provider
/// also says the wait is at least this long (seconds). Shorter waits are
/// ordinary throttling and keep the existing backoff.
pub(crate) const LONG_RESET_MIN_SECS: u64 = 300;

/// Cooldown reason reported to the broker for recognized exhaustion.
pub(crate) const COOLDOWN_REASON_QUOTA: &str = "anthropic_quota_exhausted";

/// Bytes of a 429 body handed to the classifier. Matches the broker's
/// `MAX_UPSTREAM_ERROR_BYTES`; the loop truncates on a char boundary.
pub(crate) const QUOTA_PROBE_BODY_CAP: usize = 2 * 1024;

/// Reset instants further out than this are treated as unparsable (a
/// pathological header must never park a seat for weeks).
pub(crate) const MAX_RESET_HORIZON_SECS: u64 = 8 * 24 * 60 * 60;

/// Cooldown applied when exhaustion was declared without a usable reset.
pub(crate) const DEFAULT_COOLDOWN_SECS: u64 = 15 * 60;

/// Integer reset values below this are relative seconds, not an epoch (an
/// epoch this small is 2001 or earlier and would be discarded as past).
const MIN_PLAUSIBLE_EPOCH_SECS: u64 = 1_000_000_000;

const H_UNIFIED_STATUS: &str = "anthropic-ratelimit-unified-status";
const H_UNIFIED_RESET: &str = "anthropic-ratelimit-unified-reset";
const H_5H_STATUS: &str = "anthropic-ratelimit-unified-5h-status";
const H_5H_RESET: &str = "anthropic-ratelimit-unified-5h-reset";
const H_7D_STATUS: &str = "anthropic-ratelimit-unified-7d-status";
const H_7D_RESET: &str = "anthropic-ratelimit-unified-7d-reset";
const H_RETRY_AFTER: &str = "retry-after";
const RATELIMIT_PREFIX: &str = "anthropic-ratelimit-";

/// Which provider-declared signal proved exhaustion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AnthropicQuotaSignal {
    /// `anthropic-ratelimit-unified-status: rejected`.
    UnifiedStatusRejected,
    /// `anthropic-ratelimit-unified-<window>-status: rejected`.
    WindowStatusRejected { window: &'static str },
    /// `error.type == "rate_limit_error"` body plus a reset/retry-after at
    /// least [`LONG_RESET_MIN_SECS`] away.
    LongResetWithRateLimitBody,
}

/// Recognized window exhaustion for the seat that made the request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AnthropicQuotaEvidence {
    pub(crate) signal: AnthropicQuotaSignal,
    /// Provider-reported reset instant (epoch ms) when the response carried
    /// one that is in the future and within [`MAX_RESET_HORIZON_SECS`].
    /// `None` = unknown, never guessed.
    pub(crate) resets_at_ms: Option<u64>,
}

impl AnthropicQuotaEvidence {
    /// Cooldown deadline (epoch ms) for the exhausted seat: the reported
    /// reset, else `now + DEFAULT_COOLDOWN_SECS`; never past
    /// `now + MAX_RESET_HORIZON_SECS`.
    pub(crate) fn cooldown_until_ms(&self, now_ms: u64) -> Option<u64> {
        let cap = now_ms.saturating_add(MAX_RESET_HORIZON_SECS * 1000);
        let until = self
            .resets_at_ms
            .filter(|t| *t > now_ms)
            .unwrap_or_else(|| now_ms.saturating_add(DEFAULT_COOLDOWN_SECS * 1000));
        Some(until.min(cap))
    }

    /// True when the reset is far enough out that spending the 429 retry
    /// budget on it is pointless (each retry sleeps at most a minute).
    pub(crate) fn reset_is_long(&self, now_ms: u64) -> bool {
        self.resets_at_ms
            .is_some_and(|t| t > now_ms.saturating_add(LONG_RESET_MIN_SECS * 1000))
    }
}

/// Classification of an Anthropic HTTP 429.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Anthropic429 {
    /// Provider-declared exhaustion of a usage window for this seat.
    QuotaExhausted(AnthropicQuotaEvidence),
    /// Ordinary rate limit: transient, keeps the normal retry path.
    RateLimited,
}

fn header_str<'h>(headers: &'h HeaderMap, name: &str) -> Option<&'h str> {
    headers.get(name)?.to_str().ok().map(str::trim)
}

fn header_status_is(headers: &HeaderMap, name: &str, expected: &str) -> bool {
    header_str(headers, name).is_some_and(|v| v.eq_ignore_ascii_case(expected))
}

/// Parse one reset-shaped header value into an epoch-ms instant:
/// epoch seconds (integer or float), relative seconds (small integers),
/// RFC 3339, or an HTTP-date (`retry-after` form). Values in the past or
/// beyond [`MAX_RESET_HORIZON_SECS`] yield `None`.
fn parse_reset_instant_ms(raw: &str, now_ms: u64) -> Option<u64> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let candidate_ms = if let Ok(n) = raw.parse::<u64>() {
        if n < MIN_PLAUSIBLE_EPOCH_SECS {
            now_ms.saturating_add(n.saturating_mul(1000))
        } else {
            n.saturating_mul(1000)
        }
    } else if let Ok(f) = raw.parse::<f64>() {
        if !f.is_finite() || f < 0.0 {
            return None;
        }
        if f < MIN_PLAUSIBLE_EPOCH_SECS as f64 {
            now_ms.saturating_add((f * 1000.0).round() as u64)
        } else {
            (f * 1000.0).round() as u64
        }
    } else if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(raw) {
        u64::try_from(dt.timestamp_millis()).ok()?
    } else if let Ok(dt) = chrono::DateTime::parse_from_rfc2822(raw) {
        u64::try_from(dt.timestamp_millis()).ok()?
    } else {
        return None;
    };
    let horizon = now_ms.saturating_add(MAX_RESET_HORIZON_SECS * 1000);
    (candidate_ms > now_ms && candidate_ms <= horizon).then_some(candidate_ms)
}

/// First usable reset instant among `names`, then `retry-after`.
fn first_reset_ms(headers: &HeaderMap, names: &[&str], now_ms: u64) -> Option<u64> {
    names
        .iter()
        .chain(std::iter::once(&H_RETRY_AFTER))
        .filter_map(|name| header_str(headers, name))
        .find_map(|raw| parse_reset_instant_ms(raw, now_ms))
}

/// The wait the runtime would actually honour: `retry-after` when present,
/// else the EARLIEST future `anthropic-ratelimit-*-reset` instant.
fn effective_reset_ms(headers: &HeaderMap, now_ms: u64) -> Option<u64> {
    if let Some(raw) = header_str(headers, H_RETRY_AFTER) {
        return parse_reset_instant_ms(raw, now_ms);
    }
    headers
        .iter()
        .filter(|(name, _)| {
            let n = name.as_str();
            n.starts_with(RATELIMIT_PREFIX) && n.ends_with("-reset")
        })
        .filter_map(|(_, v)| v.to_str().ok())
        .filter_map(|raw| parse_reset_instant_ms(raw, now_ms))
        .min()
}

/// Identifier-shaped `error.type` only (lowercase snake_case, ≤ 64 bytes).
fn body_error_type(body_prefix: &str) -> Option<String> {
    let json: Value = serde_json::from_str(body_prefix).ok()?;
    let s = json.get("error")?.get("type")?.as_str()?;
    (!s.is_empty()
        && s.len() <= 64
        && s
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'))
    .then(|| s.to_string())
}

/// Classify an Anthropic HTTP 429 from its headers and a bounded body
/// prefix. Fails closed: anything not matching the table is `RateLimited`.
///
/// Decision table (first match wins):
/// 1. `anthropic-ratelimit-unified-status == rejected` (case-insensitive,
///    trimmed) ⇒ exhausted; reset from `…-unified-reset`, else the rejected
///    window's reset, else `retry-after`.
/// 2. `…-unified-5h-status` / `…-unified-7d-status == rejected` ⇒ exhausted
///    with that window's `-reset` (else `retry-after`).
/// 3. Body parses as JSON with `error.type == "rate_limit_error"` AND the
///    effective wait (`retry-after`, else the earliest
///    `anthropic-ratelimit-*-reset`) is ≥ [`LONG_RESET_MIN_SECS`] ⇒ exhausted.
///    A present, non-`rejected` unified status vetoes this rule: the
///    provider explicitly said the usage window is not the cause.
/// 4. Otherwise ⇒ `RateLimited`.
pub(crate) fn classify_anthropic_429(
    headers: &HeaderMap,
    body_prefix: &str,
    now_ms: u64,
) -> Anthropic429 {
    let five_h_rejected = header_status_is(headers, H_5H_STATUS, "rejected");
    let seven_d_rejected = header_status_is(headers, H_7D_STATUS, "rejected");

    if header_status_is(headers, H_UNIFIED_STATUS, "rejected") {
        let mut names: Vec<&str> = vec![H_UNIFIED_RESET];
        if five_h_rejected {
            names.push(H_5H_RESET);
        }
        if seven_d_rejected {
            names.push(H_7D_RESET);
        }
        return Anthropic429::QuotaExhausted(AnthropicQuotaEvidence {
            signal: AnthropicQuotaSignal::UnifiedStatusRejected,
            resets_at_ms: first_reset_ms(headers, &names, now_ms),
        });
    }
    if five_h_rejected {
        return Anthropic429::QuotaExhausted(AnthropicQuotaEvidence {
            signal: AnthropicQuotaSignal::WindowStatusRejected { window: "5h" },
            resets_at_ms: first_reset_ms(headers, &[H_5H_RESET], now_ms),
        });
    }
    if seven_d_rejected {
        return Anthropic429::QuotaExhausted(AnthropicQuotaEvidence {
            signal: AnthropicQuotaSignal::WindowStatusRejected { window: "7d" },
            resets_at_ms: first_reset_ms(headers, &[H_7D_RESET], now_ms),
        });
    }
    // Secondary evidence: the provider declared a rate-limit error AND a
    // wait long enough that it cannot be ordinary throttling. A present
    // unified status that is not `rejected` contradicts exhaustion.
    let unified_says_ok = header_str(headers, H_UNIFIED_STATUS).is_some();
    if !unified_says_ok && body_error_type(body_prefix).as_deref() == Some("rate_limit_error") {
        if let Some(reset_ms) = effective_reset_ms(headers, now_ms) {
            if reset_ms >= now_ms.saturating_add(LONG_RESET_MIN_SECS * 1000) {
                return Anthropic429::QuotaExhausted(AnthropicQuotaEvidence {
                    signal: AnthropicQuotaSignal::LongResetWithRateLimitBody,
                    resets_at_ms: Some(reset_ms),
                });
            }
        }
    }
    Anthropic429::RateLimited
}

/// Truncate `body` to at most `cap` bytes on a char boundary. The result is
/// for classification only and must never reach a log line.
pub(crate) fn body_prefix(body: &str, cap: usize) -> &str {
    crate::truncate_str(body, cap)
}

/// `name=value` pairs for every `anthropic-ratelimit-*` header plus
/// `retry-after`, sorted, values truncated to 64 chars. Headers only —
/// never the body. Emitted once per turn so the live schema can be
/// confirmed from `debug.log`.
pub(crate) fn ratelimit_header_summary(headers: &HeaderMap) -> String {
    let mut pairs: Vec<String> = headers
        .iter()
        .filter(|(name, _)| {
            let n = name.as_str();
            n.starts_with(RATELIMIT_PREFIX) || n == H_RETRY_AFTER
        })
        .map(|(name, value)| {
            let v = value
                .to_str()
                .map(|s| crate::truncate_str(s.trim(), 64).to_string())
                .unwrap_or_else(|_| "<non-utf8>".to_string());
            format!("{}={}", name.as_str(), v)
        })
        .collect();
    pairs.sort();
    pairs.join(" ")
}

/// Human label for a reset instant: `HH:MM UTC` when it falls within the
/// next 24 h, otherwise `Mon DD HH:MM UTC`.
pub(crate) fn describe_reset_utc(until_ms: u64, now_ms: u64) -> String {
    let Some(dt) = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(until_ms as i64) else {
        return "the next window".to_string();
    };
    if until_ms.saturating_sub(now_ms) < 24 * 60 * 60 * 1000 {
        dt.format("%H:%M UTC").to_string()
    } else {
        dt.format("%b %d %H:%M UTC").to_string()
    }
}

// ── Runtime gate ─────────────────────────────────────────────────────────────

/// Upper bound on account switches per logical request. One is the whole
/// design: a second exhausted seat means the operator needs to know, not a
/// silent tour through every account.
pub(crate) const MAX_ANTHROPIC_ACCOUNT_FAILOVERS: u32 = 1;

/// What the gate judges. Built at the 429 site from request-level state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AnthropicFailoverGate {
    /// The broker's selector for Anthropic is `Auto`.
    pub(crate) auto_selector: bool,
    /// Any SSE frame was parsed on an earlier attempt of this request.
    pub(crate) output_started: bool,
    /// A `tool_use` block was produced on an earlier attempt of this request.
    pub(crate) tool_activity: bool,
    /// Account switches already spent on this request.
    pub(crate) failovers_so_far: u32,
}

/// Runtime preconditions for an Anthropic account failover. Pure; evaluated
/// before any candidate lookup so an explicit account or a partially
/// streamed request never triggers usage fetches or token vends. The error
/// is a static reason label for logs.
pub(crate) fn failover_permitted(gate: &AnthropicFailoverGate) -> Result<(), &'static str> {
    if !gate.auto_selector {
        return Err("explicit_selection");
    }
    if gate.output_started {
        return Err("output_already_streamed");
    }
    if gate.tool_activity {
        return Err("tool_activity");
    }
    if gate.failovers_so_far >= MAX_ANTHROPIC_ACCOUNT_FAILOVERS {
        return Err("failover_budget_exhausted");
    }
    Ok(())
}

// ── Router seam ──────────────────────────────────────────────────────────────

/// What the Anthropic request loop needs from account selection. Exactly
/// one implementation talks to the real broker; tests script the rest.
#[async_trait]
pub(crate) trait AnthropicAccountRouter: Send + Sync {
    /// True when the broker's selector for Anthropic is `Auto` — the only
    /// mode in which the runtime may switch accounts.
    fn auto_selected(&self) -> bool;

    /// Best-effort cooldown report for `failed`. Must never fail the turn.
    /// Always called on recognized exhaustion, even when the gate then
    /// refuses to switch, so the next turn boundary re-selects away.
    async fn report_exhausted(&self, failed: &CredentialRef, evidence: &AnthropicQuotaEvidence);

    /// After recognized exhaustion on `failed`, ask the broker to select
    /// again for `model`. `Ok(None)` when the answer is the same seat or no
    /// eligible seat exists; `Err` for any other broker failure. Called only
    /// after [`failover_permitted`] passed.
    async fn failover(
        &self,
        model: &str,
        failed: &CredentialRef,
    ) -> Result<Option<PinnedToken>, String>;
}

/// Adapter over the credential broker. Vends through the broker boundary
/// only — no `auth.json` access here.
///
/// Selection policy lives in exactly one place: the broker's `Auto`
/// selector. Failover therefore does not rebuild candidates in the runtime;
/// it reports the exhausted seat's cooldown and asks the broker to select
/// again, then verifies the answer is a DIFFERENT account. A broker that
/// cannot honour the cooldown (default no-op sink) answers with the same
/// seat, which the runtime refuses — bounded and safe, never a loop.
///
/// The broker is built lazily from the request's credential source so the
/// hot path pays nothing until the first recognized exhaustion; report and
/// re-vend then hit the SAME instance (an in-process broker keeps cooldowns
/// in memory).
pub(crate) struct BrokerAnthropicRouter {
    broker: OnceLock<Arc<dyn CredentialBroker>>,
    lazy: Option<(CredentialSource, TokenCache, reqwest::Client)>,
}

impl BrokerAnthropicRouter {
    /// Adapt an already-built broker (tests, or a caller holding the
    /// process-wide broker).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn new(broker: Arc<dyn CredentialBroker>) -> Self {
        let cell = OnceLock::new();
        let _ = cell.set(broker);
        Self {
            broker: cell,
            lazy: None,
        }
    }

    /// Defer broker construction to first use.
    pub(crate) fn lazy(source: &CredentialSource, cache: &TokenCache, client: reqwest::Client) -> Self {
        Self {
            broker: OnceLock::new(),
            lazy: Some((source.clone(), cache.clone(), client)),
        }
    }

    fn broker(&self) -> &Arc<dyn CredentialBroker> {
        self.broker.get_or_init(|| {
            let (source, cache, client) = self
                .lazy
                .as_ref()
                .expect("router constructed with neither a broker nor a source");
            broker_from_source(source, cache, client.clone())
        })
    }
}

#[async_trait]
impl AnthropicAccountRouter for BrokerAnthropicRouter {
    fn auto_selected(&self) -> bool {
        matches!(
            self.broker().account_selector(OAuthProviderId::Anthropic),
            AccountSelector::Auto
        )
    }

    async fn report_exhausted(&self, failed: &CredentialRef, evidence: &AnthropicQuotaEvidence) {
        let now_ms = crate::epoch_millis();
        let until_ms = evidence.cooldown_until_ms(now_ms);
        if let Err(e) = self
            .broker()
            .report_cooldown(failed, until_ms, COOLDOWN_REASON_QUOTA)
            .await
        {
            tracing::warn!(
                account = %failed,
                error = %e,
                "anthropic cooldown report failed (best effort)"
            );
        }
        tracing::warn!(
            account = %failed,
            signal = ?evidence.signal,
            resets_at_ms = ?evidence.resets_at_ms,
            until_ms = ?until_ms,
            "anthropic usage window exhausted"
        );
    }

    async fn failover(
        &self,
        model: &str,
        failed: &CredentialRef,
    ) -> Result<Option<PinnedToken>, String> {
        let provider = OAuthProviderId::Anthropic;
        // The gate already established `Auto`; re-check so a policy that
        // became explicit mid-request is honoured (never overridden).
        if !matches!(self.broker().account_selector(provider), AccountSelector::Auto) {
            return Ok(None);
        }
        match self
            .broker()
            .access_token_pinned_for(provider, Some(super::auth::quota_model_key(model)))
            .await
        {
            Ok(next) if next.credential != *failed => Ok(Some(next)),
            Ok(_same_seat) => Ok(None),
            Err(BrokerError::NoAccountAvailable { .. }) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{AccessToken, Account};
    use reqwest::header::{HeaderMap, HeaderValue};
    use std::sync::Mutex;

    const NOW_MS: u64 = 1_758_000_000_000; // 2025-09-16T05:20:00Z

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    fn epoch_secs_in(secs: u64) -> String {
        (NOW_MS / 1000 + secs).to_string()
    }

    const RATE_LIMIT_BODY: &str =
        r#"{"type":"error","error":{"type":"rate_limit_error","message":"ECHOED prompt text"}}"#;

    // ── classifier ──────────────────────────────────────────────────────────

    #[test]
    fn rejected_unified_header_is_exhaustion_with_epoch_seconds_reset() {
        let h = headers(&[
            ("anthropic-ratelimit-unified-status", "rejected"),
            ("anthropic-ratelimit-unified-reset", &epoch_secs_in(3 * 3600)),
        ]);
        assert_eq!(
            classify_anthropic_429(&h, "", NOW_MS),
            Anthropic429::QuotaExhausted(AnthropicQuotaEvidence {
                signal: AnthropicQuotaSignal::UnifiedStatusRejected,
                resets_at_ms: Some(NOW_MS + 3 * 3600 * 1000),
            })
        );
        // Case and whitespace on the status value are tolerated.
        let h = headers(&[("anthropic-ratelimit-unified-status", "  REJECTED ")]);
        assert!(matches!(
            classify_anthropic_429(&h, "", NOW_MS),
            Anthropic429::QuotaExhausted(AnthropicQuotaEvidence {
                signal: AnthropicQuotaSignal::UnifiedStatusRejected,
                resets_at_ms: None,
            })
        ));
    }

    #[test]
    fn rejected_unified_header_accepts_rfc3339_and_relative_resets() {
        let h = headers(&[
            ("anthropic-ratelimit-unified-status", "rejected"),
            ("anthropic-ratelimit-unified-reset", "2025-09-16T08:20:00Z"),
        ]);
        assert_eq!(
            classify_anthropic_429(&h, "", NOW_MS),
            Anthropic429::QuotaExhausted(AnthropicQuotaEvidence {
                signal: AnthropicQuotaSignal::UnifiedStatusRejected,
                resets_at_ms: Some(NOW_MS + 3 * 3600 * 1000),
            })
        );
        // A small integer cannot be an epoch: relative seconds from now.
        let h = headers(&[
            ("anthropic-ratelimit-unified-status", "rejected"),
            ("anthropic-ratelimit-unified-reset", "7200"),
        ]);
        assert_eq!(
            classify_anthropic_429(&h, "", NOW_MS),
            Anthropic429::QuotaExhausted(AnthropicQuotaEvidence {
                signal: AnthropicQuotaSignal::UnifiedStatusRejected,
                resets_at_ms: Some(NOW_MS + 7200 * 1000),
            })
        );
    }

    #[test]
    fn allowed_warning_with_short_retry_after_is_rate_limited() {
        let h = headers(&[
            ("anthropic-ratelimit-unified-status", "allowed_warning"),
            ("retry-after", "5"),
        ]);
        assert_eq!(
            classify_anthropic_429(&h, RATE_LIMIT_BODY, NOW_MS),
            Anthropic429::RateLimited
        );
    }

    #[test]
    fn non_rejected_unified_status_vetoes_the_body_rule() {
        let h = headers(&[
            ("anthropic-ratelimit-unified-status", "allowed_warning"),
            ("retry-after", "3600"),
        ]);
        assert_eq!(
            classify_anthropic_429(&h, RATE_LIMIT_BODY, NOW_MS),
            Anthropic429::RateLimited
        );
    }

    #[test]
    fn five_hour_window_rejected_is_exhaustion_with_that_reset() {
        let h = headers(&[
            ("anthropic-ratelimit-unified-status", "allowed"),
            ("anthropic-ratelimit-unified-5h-status", "rejected"),
            ("anthropic-ratelimit-unified-5h-reset", &epoch_secs_in(1800)),
            ("anthropic-ratelimit-unified-7d-status", "allowed"),
            ("anthropic-ratelimit-unified-7d-reset", &epoch_secs_in(5 * 86400)),
        ]);
        assert_eq!(
            classify_anthropic_429(&h, "", NOW_MS),
            Anthropic429::QuotaExhausted(AnthropicQuotaEvidence {
                signal: AnthropicQuotaSignal::WindowStatusRejected { window: "5h" },
                resets_at_ms: Some(NOW_MS + 1800 * 1000),
            })
        );
        let h = headers(&[
            ("anthropic-ratelimit-unified-7d-status", "rejected"),
            ("anthropic-ratelimit-unified-7d-reset", &epoch_secs_in(5 * 86400)),
        ]);
        assert_eq!(
            classify_anthropic_429(&h, "", NOW_MS),
            Anthropic429::QuotaExhausted(AnthropicQuotaEvidence {
                signal: AnthropicQuotaSignal::WindowStatusRejected { window: "7d" },
                resets_at_ms: Some(NOW_MS + 5 * 86400 * 1000),
            })
        );
    }

    #[test]
    fn unified_rejected_without_unified_reset_falls_back_to_window_then_retry_after() {
        let h = headers(&[
            ("anthropic-ratelimit-unified-status", "rejected"),
            ("anthropic-ratelimit-unified-7d-status", "rejected"),
            ("anthropic-ratelimit-unified-7d-reset", &epoch_secs_in(86400)),
            ("retry-after", "60"),
        ]);
        assert_eq!(
            classify_anthropic_429(&h, "", NOW_MS),
            Anthropic429::QuotaExhausted(AnthropicQuotaEvidence {
                signal: AnthropicQuotaSignal::UnifiedStatusRejected,
                resets_at_ms: Some(NOW_MS + 86400 * 1000),
            })
        );
        let h = headers(&[
            ("anthropic-ratelimit-unified-status", "rejected"),
            ("retry-after", "600"),
        ]);
        assert_eq!(
            classify_anthropic_429(&h, "", NOW_MS),
            Anthropic429::QuotaExhausted(AnthropicQuotaEvidence {
                signal: AnthropicQuotaSignal::UnifiedStatusRejected,
                resets_at_ms: Some(NOW_MS + 600 * 1000),
            })
        );
    }

    #[test]
    fn rate_limit_body_with_long_retry_after_is_exhaustion() {
        let h = headers(&[("retry-after", "3600")]);
        assert_eq!(
            classify_anthropic_429(&h, RATE_LIMIT_BODY, NOW_MS),
            Anthropic429::QuotaExhausted(AnthropicQuotaEvidence {
                signal: AnthropicQuotaSignal::LongResetWithRateLimitBody,
                resets_at_ms: Some(NOW_MS + 3600 * 1000),
            })
        );
        // HTTP-date retry-after and RFC 3339 reset headers count too.
        let h = headers(&[("retry-after", "Tue, 16 Sep 2025 06:20:00 GMT")]);
        assert_eq!(
            classify_anthropic_429(&h, RATE_LIMIT_BODY, NOW_MS),
            Anthropic429::QuotaExhausted(AnthropicQuotaEvidence {
                signal: AnthropicQuotaSignal::LongResetWithRateLimitBody,
                resets_at_ms: Some(NOW_MS + 3600 * 1000),
            })
        );
        let h = headers(&[("anthropic-ratelimit-tokens-reset", "2025-09-16T06:20:00Z")]);
        assert_eq!(
            classify_anthropic_429(&h, RATE_LIMIT_BODY, NOW_MS),
            Anthropic429::QuotaExhausted(AnthropicQuotaEvidence {
                signal: AnthropicQuotaSignal::LongResetWithRateLimitBody,
                resets_at_ms: Some(NOW_MS + 3600 * 1000),
            })
        );
    }

    #[test]
    fn rate_limit_body_with_short_retry_after_is_rate_limited() {
        let h = headers(&[("retry-after", "10")]);
        assert_eq!(
            classify_anthropic_429(&h, RATE_LIMIT_BODY, NOW_MS),
            Anthropic429::RateLimited
        );
        // Exactly the threshold counts; one second under does not.
        let h = headers(&[("retry-after", &LONG_RESET_MIN_SECS.to_string())]);
        assert!(matches!(
            classify_anthropic_429(&h, RATE_LIMIT_BODY, NOW_MS),
            Anthropic429::QuotaExhausted(_)
        ));
        let h = headers(&[("retry-after", &(LONG_RESET_MIN_SECS - 1).to_string())]);
        assert_eq!(
            classify_anthropic_429(&h, RATE_LIMIT_BODY, NOW_MS),
            Anthropic429::RateLimited
        );
    }

    #[test]
    fn earliest_reset_governs_the_body_rule() {
        // Tokens bucket refills in 10 s: the request can succeed soon, so
        // the far requests-reset is not exhaustion evidence.
        let h = headers(&[
            ("anthropic-ratelimit-tokens-reset", "2025-09-16T05:20:10Z"),
            ("anthropic-ratelimit-requests-reset", "2025-09-16T09:20:00Z"),
        ]);
        assert_eq!(
            classify_anthropic_429(&h, RATE_LIMIT_BODY, NOW_MS),
            Anthropic429::RateLimited
        );
        // `retry-after` outranks the reset headers when present.
        let h = headers(&[
            ("retry-after", "900"),
            ("anthropic-ratelimit-tokens-reset", "2025-09-16T05:20:10Z"),
        ]);
        assert!(matches!(
            classify_anthropic_429(&h, RATE_LIMIT_BODY, NOW_MS),
            Anthropic429::QuotaExhausted(_)
        ));
    }

    #[test]
    fn garbage_or_other_bodies_without_signals_are_rate_limited() {
        for body in [
            "",
            "not json",
            r#"{"error":{"type":"overloaded_error"}}"#,
            r#"{"error":{"type":"Rate Limit Error"}}"#,
            r#"{"error":{"message":"rate_limit_error"}}"#,
            "{",
        ] {
            assert_eq!(
                classify_anthropic_429(&HeaderMap::new(), body, NOW_MS),
                Anthropic429::RateLimited,
                "{body:?}"
            );
            // Even with a long retry-after: no rate_limit_error type, no exhaustion.
            let h = headers(&[("retry-after", "3600")]);
            assert_eq!(
                classify_anthropic_429(&h, body, NOW_MS),
                Anthropic429::RateLimited,
                "{body:?}"
            );
        }
        // Long retry-after alone, body says rate_limit_error but unified
        // headers are informational (`allowed`) → the provider disagrees.
        let h = headers(&[
            ("anthropic-ratelimit-unified-status", "allowed"),
            ("retry-after", "3600"),
        ]);
        assert_eq!(
            classify_anthropic_429(&h, RATE_LIMIT_BODY, NOW_MS),
            Anthropic429::RateLimited
        );
    }

    #[test]
    fn reset_in_the_past_or_too_far_out_is_dropped() {
        let h = headers(&[
            ("anthropic-ratelimit-unified-status", "rejected"),
            ("anthropic-ratelimit-unified-reset", &(NOW_MS / 1000 - 60).to_string()),
        ]);
        assert_eq!(
            classify_anthropic_429(&h, "", NOW_MS),
            Anthropic429::QuotaExhausted(AnthropicQuotaEvidence {
                signal: AnthropicQuotaSignal::UnifiedStatusRejected,
                resets_at_ms: None,
            })
        );
        let h = headers(&[
            ("anthropic-ratelimit-unified-status", "rejected"),
            (
                "anthropic-ratelimit-unified-reset",
                &epoch_secs_in(MAX_RESET_HORIZON_SECS + 1),
            ),
        ]);
        assert_eq!(
            classify_anthropic_429(&h, "", NOW_MS),
            Anthropic429::QuotaExhausted(AnthropicQuotaEvidence {
                signal: AnthropicQuotaSignal::UnifiedStatusRejected,
                resets_at_ms: None,
            })
        );
        // Unparsable values are ignored, not errors.
        let h = headers(&[
            ("anthropic-ratelimit-unified-status", "rejected"),
            ("anthropic-ratelimit-unified-reset", "soon"),
        ]);
        assert!(matches!(
            classify_anthropic_429(&h, "", NOW_MS),
            Anthropic429::QuotaExhausted(AnthropicQuotaEvidence {
                resets_at_ms: None,
                ..
            })
        ));
        // A past retry-after cannot satisfy the body rule either.
        let h = headers(&[("retry-after", "Tue, 16 Sep 2025 04:20:00 GMT")]);
        assert_eq!(
            classify_anthropic_429(&h, RATE_LIMIT_BODY, NOW_MS),
            Anthropic429::RateLimited
        );
    }

    #[test]
    fn cooldown_deadline_defaults_and_caps() {
        let with = AnthropicQuotaEvidence {
            signal: AnthropicQuotaSignal::UnifiedStatusRejected,
            resets_at_ms: Some(NOW_MS + 90_000),
        };
        assert_eq!(with.cooldown_until_ms(NOW_MS), Some(NOW_MS + 90_000));
        let without = AnthropicQuotaEvidence {
            signal: AnthropicQuotaSignal::UnifiedStatusRejected,
            resets_at_ms: None,
        };
        assert_eq!(
            without.cooldown_until_ms(NOW_MS),
            Some(NOW_MS + DEFAULT_COOLDOWN_SECS * 1000)
        );
        let past = AnthropicQuotaEvidence {
            signal: AnthropicQuotaSignal::UnifiedStatusRejected,
            resets_at_ms: Some(NOW_MS - 1),
        };
        assert_eq!(
            past.cooldown_until_ms(NOW_MS),
            Some(NOW_MS + DEFAULT_COOLDOWN_SECS * 1000)
        );
        let far = AnthropicQuotaEvidence {
            signal: AnthropicQuotaSignal::UnifiedStatusRejected,
            resets_at_ms: Some(NOW_MS + 30 * 86400 * 1000),
        };
        assert_eq!(
            far.cooldown_until_ms(NOW_MS),
            Some(NOW_MS + MAX_RESET_HORIZON_SECS * 1000)
        );
        assert!(with.reset_is_long(NOW_MS - LONG_RESET_MIN_SECS * 1000));
        assert!(!with.reset_is_long(NOW_MS));
        assert!(!without.reset_is_long(NOW_MS));
    }

    #[test]
    fn header_summary_lists_only_ratelimit_headers_sorted_and_truncated() {
        let long = "v".repeat(100);
        let h = headers(&[
            ("retry-after", "30"),
            ("anthropic-ratelimit-unified-status", "rejected"),
            ("anthropic-ratelimit-tokens-reset", &long),
            ("content-type", "application/json"),
            ("request-id", "req_123"),
            ("authorization", "Bearer sk-SECRET"),
        ]);
        let summary = ratelimit_header_summary(&h);
        assert_eq!(
            summary,
            format!(
                "anthropic-ratelimit-tokens-reset={} anthropic-ratelimit-unified-status=rejected retry-after=30",
                "v".repeat(64)
            )
        );
        assert!(!summary.contains("SECRET"));
        assert!(!summary.contains("request-id"));
        assert!(!summary.contains("content-type"));
        assert_eq!(ratelimit_header_summary(&HeaderMap::new()), "");
    }

    #[test]
    fn body_prefix_is_bounded_on_a_char_boundary() {
        let body = format!("{}é", "a".repeat(QUOTA_PROBE_BODY_CAP - 1));
        let p = body_prefix(&body, QUOTA_PROBE_BODY_CAP);
        assert_eq!(p.len(), QUOTA_PROBE_BODY_CAP - 1);
        assert_eq!(body_prefix("short", QUOTA_PROBE_BODY_CAP), "short");
    }

    #[test]
    fn reset_description_is_clock_or_date() {
        assert_eq!(describe_reset_utc(NOW_MS + 3600 * 1000, NOW_MS), "06:20 UTC");
        assert_eq!(
            describe_reset_utc(NOW_MS + 3 * 86400 * 1000, NOW_MS),
            "Sep 19 05:20 UTC"
        );
    }

    // ── gate ────────────────────────────────────────────────────────────────

    #[test]
    fn gate_refuses_explicit_streamed_tool_activity_and_overspent() {
        let ok = AnthropicFailoverGate {
            auto_selector: true,
            output_started: false,
            tool_activity: false,
            failovers_so_far: 0,
        };
        assert_eq!(failover_permitted(&ok), Ok(()));
        assert_eq!(
            failover_permitted(&AnthropicFailoverGate {
                auto_selector: false,
                ..ok
            }),
            Err("explicit_selection")
        );
        assert_eq!(
            failover_permitted(&AnthropicFailoverGate {
                output_started: true,
                ..ok
            }),
            Err("output_already_streamed")
        );
        assert_eq!(
            failover_permitted(&AnthropicFailoverGate {
                tool_activity: true,
                ..ok
            }),
            Err("tool_activity")
        );
        assert_eq!(
            failover_permitted(&AnthropicFailoverGate {
                failovers_so_far: MAX_ANTHROPIC_ACCOUNT_FAILOVERS,
                ..ok
            }),
            Err("failover_budget_exhausted")
        );
        // Explicit selection is checked first: an explicit account never has
        // candidates looked up, whatever else is true.
        assert_eq!(
            failover_permitted(&AnthropicFailoverGate {
                auto_selector: false,
                output_started: true,
                tool_activity: true,
                failovers_so_far: 5,
            }),
            Err("explicit_selection")
        );
    }

    // ── router over a scripted broker ───────────────────────────────────────

    struct ScriptedBroker {
        selector: Mutex<AccountSelector>,
        /// Seat the auto selector answers with; `None` = NoAccountAvailable.
        pick: Mutex<Option<&'static str>>,
        /// Force a non-availability broker error from the auto vend.
        vend_error: Mutex<Option<BrokerError>>,
        cooldowns: Mutex<Vec<(String, Option<u64>, String)>>,
        auto_calls: Mutex<Vec<Option<String>>>,
    }

    impl ScriptedBroker {
        fn new(selector: AccountSelector, pick: Option<&'static str>) -> Arc<Self> {
            Arc::new(Self {
                selector: Mutex::new(selector),
                pick: Mutex::new(pick),
                vend_error: Mutex::new(None),
                cooldowns: Mutex::new(Vec::new()),
                auto_calls: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl CredentialBroker for ScriptedBroker {
        async fn access_token(&self, _p: OAuthProviderId) -> Result<AccessToken, BrokerError> {
            Err(BrokerError::Denied("legacy path must not be used".into()))
        }
        fn account_selector(&self, _p: OAuthProviderId) -> AccountSelector {
            self.selector.lock().unwrap().clone()
        }
        async fn access_token_pinned_for(
            &self,
            provider: OAuthProviderId,
            model: Option<&str>,
        ) -> Result<PinnedToken, BrokerError> {
            self.auto_calls
                .lock()
                .unwrap()
                .push(model.map(str::to_string));
            if let Some(e) = self.vend_error.lock().unwrap().take() {
                return Err(e);
            }
            let Some(label) = *self.pick.lock().unwrap() else {
                return Err(BrokerError::NoAccountAvailable {
                    provider: provider.as_str().into(),
                    reason: "scripted".into(),
                });
            };
            Ok(PinnedToken {
                credential: CredentialRef::new(provider, Account::parse(label).unwrap()),
                token: AccessToken {
                    token: format!("tok-{label}"),
                    expires: u64::MAX,
                },
            })
        }
        async fn report_cooldown(
            &self,
            cred: &CredentialRef,
            until_ms: Option<u64>,
            reason: &str,
        ) -> Result<(), BrokerError> {
            self.cooldowns
                .lock()
                .unwrap()
                .push((cred.storage_key(), until_ms, reason.to_string()));
            Ok(())
        }
        async fn proxy(
            &self,
            _r: crate::auth::ProxyRequest,
        ) -> Result<crate::auth::ProxyResponse, BrokerError> {
            Err(BrokerError::Denied("not implemented in stub".into()))
        }
        async fn proxy_stream(
            &self,
            _r: crate::auth::ProxyRequest,
        ) -> Result<crate::auth::ProxyByteStream, BrokerError> {
            Err(BrokerError::Denied("not implemented in stub".into()))
        }
        async fn anthropic_usage(&self) -> Result<serde_json::Value, BrokerError> {
            Err(BrokerError::Denied("not implemented in stub".into()))
        }
        async fn capabilities(&self) -> Result<Vec<crate::auth::ProviderStatus>, BrokerError> {
            Ok(vec![])
        }
    }

    fn seat(label: &str) -> CredentialRef {
        CredentialRef::new(OAuthProviderId::Anthropic, Account::parse(label).unwrap())
    }

    fn evidence(reset_ms: Option<u64>) -> AnthropicQuotaEvidence {
        AnthropicQuotaEvidence {
            signal: AnthropicQuotaSignal::UnifiedStatusRejected,
            resets_at_ms: reset_ms,
        }
    }

    #[tokio::test]
    async fn router_reports_cooldown_until_reset_with_the_quota_reason() {
        let broker = ScriptedBroker::new(AccountSelector::Auto, Some("claude2"));
        let router = BrokerAnthropicRouter::new(broker.clone());
        let reset = crate::epoch_millis() + 2 * 3600 * 1000;
        router
            .report_exhausted(&seat("claude4"), &evidence(Some(reset)))
            .await;
        let reports = broker.cooldowns.lock().unwrap().clone();
        assert_eq!(
            reports,
            vec![(
                "anthropic@claude4".to_string(),
                Some(reset),
                COOLDOWN_REASON_QUOTA.to_string()
            )]
        );
        // No reset: the short default deadline, never `None`.
        router
            .report_exhausted(&seat("claude4"), &evidence(None))
            .await;
        let reports = broker.cooldowns.lock().unwrap().clone();
        let until = reports[1].1.expect("default cooldown deadline");
        let now = crate::epoch_millis();
        assert!(until > now && until <= now + DEFAULT_COOLDOWN_SECS * 1000 + 1000);
    }

    #[tokio::test]
    async fn router_failover_requires_a_different_seat() {
        let broker = ScriptedBroker::new(AccountSelector::Auto, Some("claude2"));
        let router = BrokerAnthropicRouter::new(broker.clone());
        assert!(router.auto_selected());
        let next = router
            .failover("anthropic/claude-opus-4-7", &seat("claude4"))
            .await
            .unwrap()
            .expect("distinct seat");
        assert_eq!(next.credential, seat("claude2"));
        assert_eq!(next.token.token, "tok-claude2");
        // The model key handed to the selector is the bare canonical id.
        assert_eq!(
            broker.auto_calls.lock().unwrap().clone(),
            vec![Some("claude-opus-4-7".to_string())]
        );
        // Same seat back ⇒ no failover (a no-op cooldown sink can never loop).
        let same = router
            .failover("claude-opus-4-7", &seat("claude2"))
            .await
            .unwrap();
        assert!(same.is_none());
    }

    #[tokio::test]
    async fn router_failover_maps_no_account_to_none_and_other_errors_to_err() {
        let broker = ScriptedBroker::new(AccountSelector::Auto, None);
        let router = BrokerAnthropicRouter::new(broker.clone());
        assert!(router
            .failover("claude-opus-4-7", &seat("claude4"))
            .await
            .unwrap()
            .is_none());
        *broker.vend_error.lock().unwrap() = Some(BrokerError::Transport("down".into()));
        let err = router
            .failover("claude-opus-4-7", &seat("claude4"))
            .await
            .unwrap_err();
        assert!(err.contains("down"));
    }

    #[tokio::test]
    async fn router_failover_refuses_when_selector_became_explicit() {
        let broker = ScriptedBroker::new(AccountSelector::Auto, Some("claude2"));
        let router = BrokerAnthropicRouter::new(broker.clone());
        *broker.selector.lock().unwrap() = AccountSelector::Account(Account::Default);
        assert!(!router.auto_selected());
        assert!(router
            .failover("claude-opus-4-7", &seat("claude4"))
            .await
            .unwrap()
            .is_none());
        // The broker was never asked to vend.
        assert!(broker.auto_calls.lock().unwrap().is_empty());
    }
}
