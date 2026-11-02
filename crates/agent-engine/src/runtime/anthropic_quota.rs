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
/// How much of the five-hour / weekly window is spent, as a fraction (1.0 =
/// the limit). A window can be spent behind an `allowed` status while extra
/// usage pays for it.
const H_5H_UTILIZATION: &str = "anthropic-ratelimit-unified-5h-utilization";
const H_7D_UTILIZATION: &str = "anthropic-ratelimit-unified-7d-utilization";
/// `7d_oi`: Fable's weekly limit, a limit of Fable's requests only (9 of
/// 193 captured 429s carried it, all on Fable requests).
const H_7D_OI_STATUS: &str = "anthropic-ratelimit-unified-7d_oi-status";
const H_7D_OI_RESET: &str = "anthropic-ratelimit-unified-7d_oi-reset";
/// The window the provider names as the reason for the limit.
const H_REPRESENTATIVE_CLAIM: &str = "anthropic-ratelimit-unified-representative-claim";
/// The claims that name Fable's weekly bucket. Unverified: no 429 seen so
/// far named it; any other claim is a limit of the whole account.
const FABLE_WEEKLY_CLAIMS: [&str; 2] = ["seven_day_oi", "7d_oi"];
/// The longest claim a log line names as itself; a longer one is `other`.
const CLAIM_WORD_MAX: usize = 32;
/// The unrecognized-claim line's limiter admits at most one line per this
/// many milliseconds, process-wide, on its own monotonic clock (never the
/// response's time); the next one says how many were left out. A thread
/// that stalls between its admission and its write can write its line
/// late, so in rare cases two lines land close together; lines never
/// outnumber admissions, so a storm of such 429s can't flood the log.
const CLAIM_LINE_INTERVAL_MS: u64 = 60_000;
/// The signal of a 429 refused by Fable's weekly limit alone.
const FABLE_WEEKLY_LIMIT: AnthropicQuotaSignal = AnthropicQuotaSignal::ModelWindowRejected {
    window: "7d_oi",
    model: "fable",
};
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
    /// `anthropic-ratelimit-unified-<window>-status: rejected` for a window
    /// that limits one model's requests only (`7d_oi`: Fable's weekly
    /// limit), while the shared five-hour and weekly windows are allowed.
    ModelWindowRejected {
        window: &'static str,
        model: &'static str,
    },
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

    /// The model whose requests alone this limit stops (a token of the
    /// model id, `fable`), or `None` when it stops the whole account.
    pub(crate) fn model_scope(&self) -> Option<&'static str> {
        match self.signal {
            AnthropicQuotaSignal::ModelWindowRejected { model, .. } => Some(model),
            _ => None,
        }
    }

    /// This evidence for a request for `model`: a limit of another model's
    /// requests can't be what refused it, so it stops the whole account.
    pub(crate) fn for_request_model(self, model: &str) -> Self {
        match self.signal {
            AnthropicQuotaSignal::ModelWindowRejected {
                window,
                model: limited,
            } if !crate::auth::quota_policy::model_matches(
                limited,
                super::auth::quota_model_key(model),
            ) =>
            {
                Self {
                    signal: AnthropicQuotaSignal::WindowStatusRejected { window },
                    ..self
                }
            }
            _ => self,
        }
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

/// A representative claim as a log-safe word: lowercased, 1 to
/// [`CLAIM_WORD_MAX`] bytes of `[a-z0-9_]`, else `other`.
fn claim_word(claim: &str) -> String {
    let word = claim.to_ascii_lowercase();
    if !word.is_empty()
        && word.len() <= CLAIM_WORD_MAX
        && word
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        word
    } else {
        "other".to_string()
    }
}

/// One rule-0 input, read conservatively: absent; one value that is
/// visible ASCII (trimmed); or unusable: present but not visible ASCII, or
/// sent more than once. An unusable input is unknown, never absent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SoleHeader<'h> {
    Absent,
    Value(&'h str),
    Unusable,
}

fn sole_header<'h>(headers: &'h HeaderMap, name: &str) -> SoleHeader<'h> {
    let mut values = headers.get_all(name).iter();
    let Some(first) = values.next() else {
        return SoleHeader::Absent;
    };
    if values.next().is_some() {
        return SoleHeader::Unusable;
    }
    match first.to_str() {
        Ok(value) => SoleHeader::Value(value.trim()),
        Err(_) => SoleHeader::Unusable,
    }
}

/// Rate state of the unrecognized-claim line, process-wide: when it was
/// last logged, on the limiter's own clock ([`claim_line_clock_ms`]), and
/// how many were left out since.
struct ClaimLineState {
    last_ms: Option<u64>,
    suppressed: u64,
}

static CLAIM_LINE: std::sync::Mutex<ClaimLineState> = std::sync::Mutex::new(ClaimLineState {
    last_ms: None,
    suppressed: 0,
});

/// The clock the unrecognized-claim line's limiter reads: monotonic
/// milliseconds since its first read in this process (a test may set it).
/// The limiter reads it under its own lock, so admissions follow this
/// clock's order; the response's time is never used, so a wall clock
/// stepped back, or a caller that took its time before another one did,
/// can't get a line admitted early.
fn claim_line_clock_ms() -> u64 {
    #[cfg(test)]
    if let Some(ms) = *CLAIM_LINE_TEST_CLOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
    {
        return ms;
    }
    static ORIGIN: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let origin = *ORIGIN.get_or_init(std::time::Instant::now);
    u64::try_from(origin.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Whether the unrecognized-claim line may be logged now: `Some` with how
/// many were left out since the last admission, or `None` (counted) inside
/// [`CLAIM_LINE_INTERVAL_MS`] of it, on the limiter's own clock, read
/// under the lock. A reading earlier than the last line's (which a
/// monotonic clock never gives) counts as inside the interval. The line is
/// written after the lock is released, so a thread that stalls between its
/// admission and its write can write its line late, and in rare cases two
/// lines land close together; lines never outnumber admissions, so a storm
/// of such 429s can't flood the log.
fn admit_claim_line() -> Option<u64> {
    let mut state = CLAIM_LINE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let now_ms = claim_line_clock_ms();
    if let Some(last) = state.last_ms {
        if now_ms.saturating_sub(last) < CLAIM_LINE_INTERVAL_MS {
            state.suppressed = state.suppressed.saturating_add(1);
            return None;
        }
    }
    state.last_ms = Some(now_ms);
    Some(std::mem::take(&mut state.suppressed))
}

/// Test seam: forget the unrecognized-claim line's rate state and any
/// clock a test set, so a test that captures the line sees its own.
#[cfg(test)]
fn reset_claim_line_limiter() {
    let mut state = CLAIM_LINE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.last_ms = None;
    state.suppressed = 0;
    drop(state);
    *CLAIM_LINE_TEST_CLOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
}

/// A clock a test set for the limiter, in place of the monotonic one.
#[cfg(test)]
static CLAIM_LINE_TEST_CLOCK: std::sync::Mutex<Option<u64>> = std::sync::Mutex::new(None);

/// Test seam: the clock the unrecognized-claim line's limiter reads, in
/// milliseconds.
#[cfg(test)]
fn set_claim_line_clock(ms: u64) {
    *CLAIM_LINE_TEST_CLOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ms);
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
/// 0. Only Fable's weekly limit refused it. Each input is read once
///    ([`sole_header`]): absent, one visible-ASCII value (trimmed), or
///    unusable (not visible ASCII, or sent more than once), which fails its
///    condition. `…-unified-7d_oi-status == rejected`; the `…5h-status` and
///    `…7d-status` each exactly `allowed` or `allowed_warning` (any case);
///    the `…5h-utilization` and `…7d-utilization` both sent and in
///    [0, 1); and `…-representative-claim` absent or naming the 7d_oi
///    bucket (`seven_day_oi` / `7d_oi`, any case) ⇒ a limit of Fable's
///    requests ([`AnthropicQuotaSignal::ModelWindowRejected`]), whatever
///    the unified status says; reset from `…7d_oi-reset`, else
///    `retry-after`. The shape is unverified (the captured 429s had `7d_oi`
///    allowed); anything short of every condition falls through to today's
///    rules, and [`AnthropicQuotaEvidence::for_request_model`] makes it the
///    whole account's on a request for another model. Any other claim, or
///    an unusable one, with the rest met, leaves an INFO line naming the
///    claim as a plain word ([`claim_word`]) or `other`, and no other
///    header value; the limiter admits at most one such line per
///    [`CLAIM_LINE_INTERVAL_MS`] process-wide, on its own monotonic clock
///    (the next line carries how many were left out; a thread that stalls
///    between its admission and its write can write its line late, so two
///    lines can rarely land close together, but lines never outnumber
///    admissions). Rules 1-4 read a repeated header's first value, as
///    before.
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

    // Rule 0: every input sent once and readable; each shared window
    // reported with a status the provider sends for a window with room,
    // and its utilization known and below the limit.
    let fable_rejected = matches!(
        sole_header(headers, H_7D_OI_STATUS),
        SoleHeader::Value(v) if v.eq_ignore_ascii_case("rejected")
    );
    let allowed = |name: &str| {
        matches!(
            sole_header(headers, name),
            SoleHeader::Value(v)
                if v.eq_ignore_ascii_case("allowed") || v.eq_ignore_ascii_case("allowed_warning")
        )
    };
    let below_limit = |name: &str| match sole_header(headers, name) {
        SoleHeader::Value(v) => v.parse::<f64>().is_ok_and(|u| (0.0..1.0).contains(&u)),
        SoleHeader::Absent | SoleHeader::Unusable => false,
    };
    if fable_rejected
        && allowed(H_5H_STATUS)
        && allowed(H_7D_STATUS)
        && below_limit(H_5H_UTILIZATION)
        && below_limit(H_7D_UTILIZATION)
    {
        let claim = sole_header(headers, H_REPRESENTATIVE_CLAIM);
        let names_the_fable_bucket = match claim {
            SoleHeader::Absent => true,
            SoleHeader::Value(claim) => FABLE_WEEKLY_CLAIMS
                .iter()
                .any(|bucket| claim.eq_ignore_ascii_case(bucket)),
            SoleHeader::Unusable => false,
        };
        if names_the_fable_bucket {
            return Anthropic429::QuotaExhausted(AnthropicQuotaEvidence {
                signal: FABLE_WEEKLY_LIMIT,
                resets_at_ms: first_reset_ms(headers, &[H_7D_OI_RESET], now_ms),
            });
        }
        if let Some(suppressed) = admit_claim_line() {
            let word = match claim {
                SoleHeader::Value(claim) => claim_word(claim),
                SoleHeader::Absent | SoleHeader::Unusable => "other".to_string(),
            };
            tracing::info!(
                claim = %word,
                suppressed,
                "7d_oi rejected but claim not recognized; rule 0 not applied; today's rules decide"
            );
        }
    }
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

/// Backstop on account switches per logical request. The real bound is the
/// attempted set: a request uses each account at most once, so it stops
/// when no account it hasn't used has proven capacity. This limit only
/// stops a router that keeps naming accounts the request hasn't seen. (It
/// was 1, "one is the whole design": that ended turns on the second
/// exhausted account while other accounts still had capacity.)
pub(crate) const MAX_ANTHROPIC_ACCOUNT_FAILOVERS: u32 = 16;

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
    /// The request may be sent once only (a keep-warm ping or a rollover
    /// summary): a hop would be a second, speculative request at full
    /// cache-write price on another account's cold prompt cache, and would
    /// rebind the session's seat from a background request.
    pub(crate) single_attempt: bool,
}

/// Why the gate refused an account switch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FailoverRefusal {
    /// A keep-warm ping or a rollover summary: sent once, on its own seat.
    SingleAttempt,
    ExplicitSelection,
    OutputAlreadyStreamed,
    ToolActivity,
    BudgetExhausted,
    /// A cache keep-warm ping: it keeps the cache of the account it is on.
    KeepWarmPing,
}

impl FailoverRefusal {
    /// Static reason label for logs.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::SingleAttempt => "single_attempt",
            Self::ExplicitSelection => "explicit_selection",
            Self::OutputAlreadyStreamed => "output_already_streamed",
            Self::ToolActivity => "tool_activity",
            Self::BudgetExhausted => "failover_budget_exhausted",
            Self::KeepWarmPing => "keepwarm_ping",
        }
    }
}

/// Runtime preconditions for an Anthropic account failover. Pure; evaluated
/// before any candidate lookup so an explicit account or a partially
/// streamed request never triggers usage fetches or token vends.
pub(crate) fn failover_permitted(gate: &AnthropicFailoverGate) -> Result<(), FailoverRefusal> {
    if gate.single_attempt {
        return Err(FailoverRefusal::SingleAttempt);
    }
    if !gate.auto_selector {
        return Err(FailoverRefusal::ExplicitSelection);
    }
    if gate.output_started {
        return Err(FailoverRefusal::OutputAlreadyStreamed);
    }
    if gate.tool_activity {
        return Err(FailoverRefusal::ToolActivity);
    }
    if gate.failovers_so_far >= MAX_ANTHROPIC_ACCOUNT_FAILOVERS {
        return Err(FailoverRefusal::BudgetExhausted);
    }
    Ok(())
}

/// Why a recognized limit on the request's account did not switch it to
/// another account. Each reason has its own text: none of them may claim
/// that another account will serve.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NoHop {
    /// An explicit account, or a request bound to no account: the runtime
    /// never switches.
    Explicit,
    /// Output or a tool call already streamed on this request.
    OutputStreamed,
    /// [`MAX_ANTHROPIC_ACCOUNT_FAILOVERS`] switches were spent.
    Backstop,
    /// The broker named no account this request hasn't used.
    NoOtherAccount,
    /// Looking up another account failed.
    LookupFailed,
}

impl NoHop {
    pub(crate) fn from_refusal(refusal: FailoverRefusal) -> Self {
        match refusal {
            // A ping or a summary on its own seat: the neutral form, which
            // claims nothing about other accounts.
            FailoverRefusal::SingleAttempt
            | FailoverRefusal::ExplicitSelection
            | FailoverRefusal::KeepWarmPing => Self::Explicit,
            FailoverRefusal::OutputAlreadyStreamed | FailoverRefusal::ToolActivity => {
                Self::OutputStreamed
            }
            FailoverRefusal::BudgetExhausted => Self::Backstop,
        }
    }

    /// The reason in a notice; `None` for an explicit or unbound request,
    /// whose notice has its own form.
    fn reason(self) -> Option<&'static str> {
        match self {
            Self::Explicit => None,
            Self::OutputStreamed => {
                Some("output already streamed, so this request can't switch accounts")
            }
            Self::Backstop => Some("no more account switches on this request"),
            Self::NoOtherAccount => Some("no other account has proven capacity now"),
            Self::LookupFailed => Some("looking up another account failed"),
        }
    }

    /// The advice that ends the turn on a long reset.
    pub(crate) fn advice(self) -> String {
        const STATE: &str = "`synaps auth state --provider anthropic` shows each account's state.";
        match self {
            Self::Explicit => "Wait for the reset, or switch accounts with \
                `synaps auth use --provider anthropic --account <label|auto>`."
                .to_string(),
            Self::OutputStreamed => "Output had already streamed, so this request could not \
                switch accounts; send again to select an account for the next turn."
                .to_string(),
            Self::Backstop => format!(
                "Stopped after {MAX_ANTHROPIC_ACCOUNT_FAILOVERS} account switches on this \
                 request; {STATE}"
            ),
            Self::NoOtherAccount => format!("No other account has proven capacity now; {STATE}"),
            Self::LookupFailed => format!("Looking up another account failed; {STATE}"),
        }
    }
}

/// Every line the request loop shows about a recognized limit on one
/// account: the switch notice, the no-switch notice and the error that ends
/// the turn. The words say what the provider declared. A unified or
/// per-window `rejected` status "is at a usage limit"; without a reset time
/// the time shown is our own cooldown, and the text says so. A long wait on
/// a 429 with no usage-limit header "is rate-limited", not at a usage limit.
pub(crate) struct LimitText<'a> {
    pub(crate) label: &'a str,
    pub(crate) evidence: &'a AnthropicQuotaEvidence,
    pub(crate) now_ms: u64,
}

impl LimitText<'_> {
    /// When the account is used again: the provider's reset, else our
    /// cooldown.
    fn until(&self) -> String {
        let until_ms = self
            .evidence
            .cooldown_until_ms(self.now_ms)
            .unwrap_or(self.now_ms);
        describe_reset_utc(until_ms, self.now_ms)
    }

    fn reset_given(&self) -> bool {
        self.evidence.resets_at_ms.is_some_and(|t| t > self.now_ms)
    }

    fn rate_limited(&self) -> bool {
        self.evidence.signal == AnthropicQuotaSignal::LongResetWithRateLimitBody
    }

    /// "{label} is at a usage limit until …", and the other two forms.
    fn state(&self) -> String {
        let (label, until) = (self.label, self.until());
        if self.rate_limited() {
            format!("{label} is rate-limited until {until} (HTTP 429, no usage-limit header)")
        } else if self.reset_given() {
            format!("{label} is at a usage limit until {until}")
        } else {
            format!(
                "{label} is at a usage limit (reset time not given; not used again before {until})"
            )
        }
    }

    /// The notice for a switch to `next`.
    pub(crate) fn hop(&self, next: &str) -> String {
        format!("⚠ {} — switching to {next}", self.state())
    }

    /// The notice when the request did not switch accounts.
    pub(crate) fn no_hop(&self, why: NoHop) -> String {
        match why.reason() {
            Some(reason) => format!("⚠ {}; {reason}", self.state()),
            None => format!("⚠ account limit reached until {}", self.until()),
        }
    }

    /// The error that ends the turn on a long reset.
    pub(crate) fn terminal(&self, why: NoHop) -> String {
        let (label, until) = (self.label, self.until());
        let head = if self.rate_limited() {
            format!(
                "Anthropic rate limit on account '{label}' until {until} \
                 (HTTP 429 with a long wait, not a declared usage limit)."
            )
        } else if self.reset_given() {
            format!("Anthropic usage limit reached on account '{label}' — window resets at {until}.")
        } else {
            format!(
                "Anthropic usage limit reached on account '{label}' (reset time not given; \
                 not used again before {until})."
            )
        };
        format!("{head} {}", why.advice())
    }
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

    /// A request on `seat` sent at `sent_at_ms` (epoch ms) was accepted
    /// after this request reported `seat` exhausted (a short or unknown
    /// reset the ordinary retry outlived): drop the cooldown, so the next
    /// round does not move the turn off an account that serves. Must never
    /// fail the turn.
    async fn report_served(&self, seat: &CredentialRef, sent_at_ms: u64);

    /// After recognized exhaustion, ask the broker to select again for
    /// `model`, never answering with an account in `attempted`: every
    /// account this logical request already used, the exhausted one last.
    /// `Ok(None)` when no other eligible account exists (or the broker's
    /// answer was already attempted); `Err` for any other broker failure.
    /// Called only after [`failover_permitted`] passed.
    async fn failover(
        &self,
        model: &str,
        attempted: &[CredentialRef],
    ) -> Result<Option<PinnedToken>, String>;

    /// [`Self::failover`] that keeps why no account was found: `Ok(Err(f))`
    /// carries the broker's selection failure, when it gave one, for a
    /// capacity wait to plan from. A capacity wait's re-check calls it with
    /// nothing attempted. Default: [`Self::failover`], no failure.
    async fn failover_detailed(
        &self,
        model: &str,
        attempted: &[CredentialRef],
    ) -> Result<Result<PinnedToken, Option<crate::auth::SelectionFailure>>, String> {
        Ok(self.failover(model, attempted).await?.ok_or(None))
    }
}

/// Adapter over the credential broker. Vends through the broker boundary
/// only — no `auth.json` access here.
///
/// Selection policy lives in exactly one place: the broker's `Auto`
/// selector. Failover therefore does not rebuild candidates in the runtime;
/// it reports the exhausted seat's cooldown and asks the broker to select
/// again without the accounts the request already used, then verifies the
/// answer is none of them. A broker that cannot honour the cooldown or the
/// exclusion (default no-op sink, a remote broker) answers with an attempted
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
        // A limit of one model's requests (Fable's weekly limit) cools the
        // account for that model only; a broker that keeps no such
        // cooldowns cools the whole account (the trait's default).
        let reported = match evidence.model_scope() {
            Some(model) => {
                self.broker()
                    .report_model_cooldown(failed, model, until_ms, COOLDOWN_REASON_QUOTA)
                    .await
            }
            None => {
                self.broker()
                    .report_cooldown(failed, until_ms, COOLDOWN_REASON_QUOTA)
                    .await
            }
        };
        if let Err(e) = reported {
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

    async fn report_served(&self, seat: &CredentialRef, sent_at_ms: u64) {
        self.broker().clear_cooldown(seat, sent_at_ms);
    }

    async fn failover(
        &self,
        model: &str,
        attempted: &[CredentialRef],
    ) -> Result<Option<PinnedToken>, String> {
        Ok(self.failover_detailed(model, attempted).await?.ok())
    }

    async fn failover_detailed(
        &self,
        model: &str,
        attempted: &[CredentialRef],
    ) -> Result<Result<PinnedToken, Option<crate::auth::SelectionFailure>>, String> {
        let provider = OAuthProviderId::Anthropic;
        // The gate already established `Auto`; re-check so a policy that
        // became explicit mid-request is honoured (never overridden).
        if !matches!(self.broker().account_selector(provider), AccountSelector::Auto) {
            return Ok(Err(None));
        }
        match self
            .broker()
            .access_token_pinned_excluding(
                provider,
                Some(super::auth::quota_model_key(model)),
                attempted,
            )
            .await
        {
            Ok(next) if !attempted.contains(&next.credential) => Ok(Ok(next)),
            Ok(_attempted_seat) => Ok(Err(None)),
            Err(BrokerError::NoAccountAvailable { failure, .. }) => Ok(Err(failure)),
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
            single_attempt: false,
        };
        assert_eq!(failover_permitted(&ok), Ok(()));
        assert_eq!(
            failover_permitted(&AnthropicFailoverGate {
                single_attempt: true,
                ..ok
            }),
            Err(FailoverRefusal::SingleAttempt),
            "a keep-warm ping or rollover summary is sent once, on its own account"
        );
        assert_eq!(
            failover_permitted(&AnthropicFailoverGate {
                auto_selector: false,
                ..ok
            }),
            Err(FailoverRefusal::ExplicitSelection)
        );
        assert_eq!(
            failover_permitted(&AnthropicFailoverGate {
                output_started: true,
                ..ok
            }),
            Err(FailoverRefusal::OutputAlreadyStreamed)
        );
        assert_eq!(
            failover_permitted(&AnthropicFailoverGate {
                tool_activity: true,
                ..ok
            }),
            Err(FailoverRefusal::ToolActivity)
        );
        assert_eq!(
            failover_permitted(&AnthropicFailoverGate {
                failovers_so_far: MAX_ANTHROPIC_ACCOUNT_FAILOVERS,
                ..ok
            }),
            Err(FailoverRefusal::BudgetExhausted)
        );
        // Explicit selection is checked first: an explicit account never has
        // candidates looked up, whatever else is true.
        assert_eq!(
            failover_permitted(&AnthropicFailoverGate {
                auto_selector: false,
                output_started: true,
                tool_activity: true,
                failovers_so_far: 5,
                single_attempt: false,
            }),
            Err(FailoverRefusal::ExplicitSelection)
        );
        // The log labels are unchanged.
        assert_eq!(
            [
                FailoverRefusal::SingleAttempt,
                FailoverRefusal::ExplicitSelection,
                FailoverRefusal::OutputAlreadyStreamed,
                FailoverRefusal::ToolActivity,
                FailoverRefusal::BudgetExhausted,
            ]
            .map(FailoverRefusal::label),
            [
                "single_attempt",
                "explicit_selection",
                "output_already_streamed",
                "tool_activity",
                "failover_budget_exhausted",
            ]
        );
    }

    /// The limit texts say what the provider declared: a `rejected` status
    /// is a usage limit, with its reset or with our cooldown named as such;
    /// a long wait on a plain 429 is a rate limit. None says another account
    /// will serve, and every no-switch advice but the explicit one points at
    /// `auth state`.
    #[test]
    fn limit_texts_say_what_the_provider_declared() {
        let at = |signal, resets_at_ms| AnthropicQuotaEvidence {
            signal,
            resets_at_ms,
        };
        let unified = at(
            AnthropicQuotaSignal::UnifiedStatusRejected,
            Some(NOW_MS + 3600 * 1000),
        );
        let window = at(
            AnthropicQuotaSignal::WindowStatusRejected { window: "7d" },
            Some(NOW_MS + 3600 * 1000),
        );
        let no_reset = at(AnthropicQuotaSignal::UnifiedStatusRejected, None);
        let long_wait = at(
            AnthropicQuotaSignal::LongResetWithRateLimitBody,
            Some(NOW_MS + 3600 * 1000),
        );
        fn text(evidence: &AnthropicQuotaEvidence) -> LimitText<'_> {
            LimitText {
                label: "claude4",
                evidence,
                now_ms: NOW_MS,
            }
        }

        for evidence in [&unified, &window] {
            let t = text(evidence);
            assert_eq!(
                t.hop("claude5"),
                "⚠ claude4 is at a usage limit until 06:20 UTC — switching to claude5"
            );
            assert_eq!(
                t.no_hop(NoHop::NoOtherAccount),
                "⚠ claude4 is at a usage limit until 06:20 UTC; \
                 no other account has proven capacity now"
            );
            assert_eq!(
                t.terminal(NoHop::NoOtherAccount),
                "Anthropic usage limit reached on account 'claude4' — window resets at \
                 06:20 UTC. No other account has proven capacity now; \
                 `synaps auth state --provider anthropic` shows each account's state."
            );
        }

        let t = text(&no_reset);
        assert_eq!(
            t.hop("claude5"),
            "⚠ claude4 is at a usage limit (reset time not given; not used again before \
             05:35 UTC) — switching to claude5"
        );
        assert_eq!(
            t.no_hop(NoHop::LookupFailed),
            "⚠ claude4 is at a usage limit (reset time not given; not used again before \
             05:35 UTC); looking up another account failed"
        );

        let t = text(&long_wait);
        assert_eq!(
            t.hop("claude5"),
            "⚠ claude4 is rate-limited until 06:20 UTC (HTTP 429, no usage-limit header) \
             — switching to claude5"
        );
        assert_eq!(
            t.no_hop(NoHop::OutputStreamed),
            "⚠ claude4 is rate-limited until 06:20 UTC (HTTP 429, no usage-limit header); \
             output already streamed, so this request can't switch accounts"
        );
        assert_eq!(
            t.terminal(NoHop::Backstop),
            format!(
                "Anthropic rate limit on account 'claude4' until 06:20 UTC (HTTP 429 with a \
                 long wait, not a declared usage limit). Stopped after \
                 {MAX_ANTHROPIC_ACCOUNT_FAILOVERS} account switches on this request; \
                 `synaps auth state --provider anthropic` shows each account's state."
            )
        );
        assert!(!t.terminal(NoHop::Backstop).contains("usage limit reached"));

        // Explicit or unbound: the notice and advice are unchanged.
        let t = text(&unified);
        assert_eq!(
            t.no_hop(NoHop::Explicit),
            "⚠ account limit reached until 06:20 UTC"
        );
        assert!(t
            .terminal(NoHop::Explicit)
            .ends_with("Wait for the reset, or switch accounts with `synaps auth use --provider anthropic --account <label|auto>`."));

        for why in [
            NoHop::OutputStreamed,
            NoHop::Backstop,
            NoHop::NoOtherAccount,
            NoHop::LookupFailed,
        ] {
            let advice = why.advice();
            assert!(!advice.contains("another account will"), "{advice}");
            assert!(!advice.contains("next turn will"), "{advice}");
            if why != NoHop::OutputStreamed {
                assert!(
                    advice.ends_with(
                        "`synaps auth state --provider anthropic` shows each account's state."
                    ),
                    "{advice}"
                );
            }
        }
    }

    // ── A4: a limit of Fable's requests only (`7d_oi`) ───────────────────────

    const FABLE_OI: AnthropicQuotaSignal = AnthropicQuotaSignal::ModelWindowRejected {
        window: "7d_oi",
        model: "fable",
    };

    /// A 429 where only Fable's weekly limit (`7d_oi`) is spent and the
    /// shared windows are allowed, with their utilization below the limit
    /// (the shape is unverified: the 429s seen so far had 7d_oi allowed),
    /// with `changes` applied: a header set to a value, or removed (`None`).
    fn fable_only(changes: &[(&'static str, Option<&str>)]) -> HeaderMap {
        let (r5h, r7d, roi) = (
            epoch_secs_in(1800),
            epoch_secs_in(5 * 86400),
            epoch_secs_in(2 * 86400),
        );
        let base: [(&'static str, &str); 11] = [
            ("anthropic-ratelimit-unified-status", "rejected"),
            ("anthropic-ratelimit-unified-reset", &r7d),
            ("anthropic-ratelimit-unified-5h-status", "allowed"),
            ("anthropic-ratelimit-unified-5h-reset", &r5h),
            ("anthropic-ratelimit-unified-5h-utilization", "0.36"),
            ("anthropic-ratelimit-unified-7d-status", "allowed"),
            ("anthropic-ratelimit-unified-7d-reset", &r7d),
            ("anthropic-ratelimit-unified-7d-utilization", "0.5"),
            ("anthropic-ratelimit-unified-7d_oi-status", "rejected"),
            ("anthropic-ratelimit-unified-7d_oi-reset", &roi),
            ("anthropic-ratelimit-unified-7d_oi-utilization", "1.0"),
        ];
        let mut pairs: Vec<(&'static str, &str)> = base
            .into_iter()
            .filter(|(name, _)| !changes.iter().any(|(changed, _)| changed == name))
            .collect();
        pairs.extend(changes.iter().filter_map(|(name, value)| value.map(|v| (*name, v))));
        headers(&pairs)
    }

    /// The whole account, as today: the unified status rejected, until the
    /// unified reset (the shared weekly one in [`fable_only`]).
    fn whole_account() -> Anthropic429 {
        Anthropic429::QuotaExhausted(AnthropicQuotaEvidence {
            signal: AnthropicQuotaSignal::UnifiedStatusRejected,
            resets_at_ms: Some(NOW_MS + 5 * 86400 * 1000),
        })
    }

    /// Rule 0: only Fable's weekly limit refused the request (7d_oi
    /// rejected, the five-hour and weekly windows reported allowed, no claim
    /// naming either): a limit of Fable's requests, until 7d_oi's reset,
    /// whatever the unified status says.
    #[test]
    fn only_fables_weekly_limit_rejected_is_a_fable_limit_until_its_reset() {
        let fable = Anthropic429::QuotaExhausted(AnthropicQuotaEvidence {
            signal: FABLE_OI,
            resets_at_ms: Some(NOW_MS + 2 * 86400 * 1000),
        });
        let claim = "anthropic-ratelimit-unified-representative-claim";
        for changes in [
            vec![],
            vec![("anthropic-ratelimit-unified-status", None)],
            vec![("anthropic-ratelimit-unified-status", Some("allowed"))],
            vec![(claim, Some("seven_day_oi"))],
            vec![("anthropic-ratelimit-unified-5h-status", Some("allowed_warning"))],
        ] {
            let h = fable_only(&changes);
            assert_eq!(classify_anthropic_429(&h, "", NOW_MS), fable, "{changes:?}");
        }
        let Anthropic429::QuotaExhausted(evidence) = fable else {
            unreachable!()
        };
        assert_eq!(evidence.model_scope(), Some("fable"));
        assert_eq!(
            AnthropicQuotaEvidence {
                signal: AnthropicQuotaSignal::UnifiedStatusRejected,
                resets_at_ms: None,
            }
            .model_scope(),
            None
        );
    }

    /// The 10-12 and 10-14 captures: a Fable request refused by a SHARED
    /// window (7d_oi allowed) stops the whole account, as today; and so does
    /// a shared window spent together with Fable's.
    #[test]
    fn a_fable_request_refused_by_a_shared_window_stops_the_whole_account() {
        let claim = "anthropic-ratelimit-unified-representative-claim";
        let seven_day_capture = fable_only(&[
            (claim, Some("seven_day")),
            ("anthropic-ratelimit-unified-7d-status", Some("rejected")),
            ("anthropic-ratelimit-unified-7d_oi-status", Some("allowed")),
            ("anthropic-ratelimit-unified-7d_oi-utilization", Some("0.05")),
        ]);
        assert_eq!(classify_anthropic_429(&seven_day_capture, "", NOW_MS), whole_account());
        let five_hour_capture = fable_only(&[
            (claim, Some("five_hour")),
            ("anthropic-ratelimit-unified-5h-status", Some("rejected")),
            ("anthropic-ratelimit-unified-7d_oi-status", Some("allowed")),
        ]);
        assert_eq!(classify_anthropic_429(&five_hour_capture, "", NOW_MS), whole_account());
        for shared in [
            "anthropic-ratelimit-unified-5h-status",
            "anthropic-ratelimit-unified-7d-status",
        ] {
            let both = fable_only(&[(shared, Some("rejected"))]);
            assert_eq!(classify_anthropic_429(&both, "", NOW_MS), whole_account(), "{shared}");
        }
    }

    /// A claim naming a shared window wins over 7d_oi: the whole account.
    #[test]
    #[serial_test::serial(anthropic_429_log_capture)]
    fn a_claim_naming_a_shared_window_stops_the_whole_account() {
        for claim in ["five_hour", "seven_day"] {
            let h = fable_only(&[(
                "anthropic-ratelimit-unified-representative-claim",
                Some(claim),
            )]);
            assert_eq!(classify_anthropic_429(&h, "", NOW_MS), whole_account(), "{claim}");
        }
    }

    /// Rule 0 needs both shared windows reported, and allowed: with either
    /// status missing, today's rules apply (the unified status: the whole
    /// account; without one, an ordinary 429).
    #[test]
    fn a_fable_limit_needs_both_shared_windows_reported_allowed() {
        for missing in [
            "anthropic-ratelimit-unified-5h-status",
            "anthropic-ratelimit-unified-7d-status",
        ] {
            let h = fable_only(&[(missing, None)]);
            assert_eq!(classify_anthropic_429(&h, "", NOW_MS), whole_account(), "{missing}");
        }
        let h = fable_only(&[
            ("anthropic-ratelimit-unified-status", None),
            ("anthropic-ratelimit-unified-7d-status", None),
        ]);
        assert_eq!(classify_anthropic_429(&h, "", NOW_MS), Anthropic429::RateLimited);
    }

    const CLAIM: &str = "anthropic-ratelimit-unified-representative-claim";

    /// The fixed text of the line a 7d_oi rejection with any other claim
    /// leaves in the log.
    const UNRECOGNIZED_CLAIM_LINE: &str =
        "7d_oi rejected but claim not recognized; rule 0 not applied; today's rules decide";

    /// The lines of `logs` that are the unrecognized-claim line.
    fn claim_lines(logs: &str) -> Vec<&str> {
        logs.lines()
            .filter(|line| line.contains(UNRECOGNIZED_CLAIM_LINE))
            .collect()
    }

    /// Rule 0's result on [`fable_only`]: a limit of Fable's requests until
    /// the 7d_oi reset.
    fn fable_limit() -> Anthropic429 {
        Anthropic429::QuotaExhausted(AnthropicQuotaEvidence {
            signal: FABLE_OI,
            resets_at_ms: Some(NOW_MS + 2 * 86400 * 1000),
        })
    }

    /// `std::io::Write` sink appending to a shared buffer. Every test that
    /// can reach the unrecognized-claim line runs in the serial group of
    /// the failover tests that capture logs (`anthropic_429_log_capture`):
    /// tracing caches a callsite's interest globally, and a first hit on a
    /// thread with no subscriber can hide the line from a capture running
    /// at the same time (see the note on `anthropic_failover_tests`).
    struct SharedLog(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for SharedLog {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// [`classify_logged_at`] at [`NOW_MS`], with the line's rate state
    /// forgotten first.
    fn classify_logged(h: &HeaderMap) -> (Anthropic429, String) {
        reset_claim_line_limiter();
        classify_logged_at(h, NOW_MS)
    }

    /// Classify `h` at `now_ms`, and what the daemon's log file keeps of it:
    /// the log file's own filter (`with_crate_directives` over the ERROR
    /// default an unset `RUST_LOG` gives, as `init_logging` builds it).
    fn classify_logged_at(h: &HeaderMap, now_ms: u64) -> (Anthropic429, String) {
        use tracing_subscriber::filter::{EnvFilter, LevelFilter};
        let buf = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&buf);
        let unset = EnvFilter::builder()
            .with_default_directive(LevelFilter::ERROR.into())
            .parse_lossy("");
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(crate::logging::with_crate_directives(unset))
            .with_writer(move || SharedLog(Arc::clone(&sink)))
            .with_ansi(false)
            .finish();
        let out = tracing::subscriber::with_default(subscriber, || {
            classify_anthropic_429(h, "", now_ms)
        });
        let logs = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        (out, logs)
    }

    /// [`classify_logged_at`] at `now_ms`, the response's time, with the
    /// limiter's own clock set to `limiter_ms` first.
    fn classify_logged_on(h: &HeaderMap, now_ms: u64, limiter_ms: u64) -> (Anthropic429, String) {
        set_claim_line_clock(limiter_ms);
        classify_logged_at(h, now_ms)
    }

    /// Rule 0 needs each shared status to be one the provider sends for a
    /// window with room, `allowed` or `allowed_warning` (any case): any
    /// other value, or an empty one, is today's rejection of the whole
    /// account, with its reset.
    #[test]
    fn a_fable_limit_needs_a_known_allowed_status_on_both_shared_windows() {
        for shared in [
            "anthropic-ratelimit-unified-5h-status",
            "anthropic-ratelimit-unified-7d-status",
        ] {
            for value in ["garbage", "", "allowed-ish", "not_rejected", "warning"] {
                let h = fable_only(&[(shared, Some(value))]);
                assert_eq!(
                    classify_anthropic_429(&h, "", NOW_MS),
                    whole_account(),
                    "{shared}: {value:?}"
                );
            }
        }
        for value in ["allowed", "allowed_warning", "ALLOWED_WARNING", "Allowed"] {
            let h = fable_only(&[
                ("anthropic-ratelimit-unified-5h-status", Some(value)),
                ("anthropic-ratelimit-unified-7d-status", Some(value)),
            ]);
            assert_eq!(classify_anthropic_429(&h, "", NOW_MS), fable_limit(), "{value}");
        }
    }

    /// A shared window can be spent behind an allowed status (while extra
    /// usage pays for it): a five-hour or weekly utilization of 1.0 or
    /// more, or one that can't be read, is today's rejection of the whole
    /// account. From 0 to below 1.0, rule 0 holds.
    #[test]
    fn a_fable_limit_needs_both_shared_windows_below_their_limit() {
        for utilization in [
            "anthropic-ratelimit-unified-5h-utilization",
            "anthropic-ratelimit-unified-7d-utilization",
        ] {
            for value in ["1.0", "1", "1.5", "full", "NaN", ""] {
                let h = fable_only(&[(utilization, Some(value))]);
                assert_eq!(
                    classify_anthropic_429(&h, "", NOW_MS),
                    whole_account(),
                    "{utilization}: {value:?}"
                );
            }
            for value in ["0.99", "0", "0.36"] {
                let h = fable_only(&[(utilization, Some(value))]);
                assert_eq!(
                    classify_anthropic_429(&h, "", NOW_MS),
                    fable_limit(),
                    "{utilization}: {value}"
                );
            }
        }
    }

    /// Rule 0 takes no claim, or one naming the 7d_oi bucket
    /// (`seven_day_oi`, `7d_oi`, any case), and logs nothing more then.
    #[test]
    #[serial_test::serial(anthropic_429_log_capture)]
    fn a_fable_limit_takes_no_claim_or_one_naming_its_bucket() {
        for claim in [None, Some("seven_day_oi"), Some("7d_oi"), Some("SEVEN_DAY_OI")] {
            let (out, logs) = classify_logged(&fable_only(&[(CLAIM, claim)]));
            assert_eq!(out, fable_limit(), "{claim:?}");
            assert!(!logs.contains(UNRECOGNIZED_CLAIM_LINE), "{claim:?}: {logs}");
        }
    }

    /// Any other claim on a 7d_oi rejection (what one names there was never
    /// seen): rule 0 is not applied and today's rules decide (here the
    /// unified rejection: the whole account), with one INFO line in the
    /// daemon's log that names the claim only as a plain word (`[a-z0-9_]`,
    /// at most 32 bytes, lowercased), else `other`, and no header value;
    /// nothing was left out before it (`suppressed=0`). The 429s seen so
    /// far (7d_oi allowed, a shared window claimed) leave no such line.
    #[test]
    #[serial_test::serial(anthropic_429_log_capture)]
    fn a_fable_limit_with_any_other_claim_stops_the_whole_account_with_one_line() {
        let long = "a".repeat(33);
        for (claim, word) in [
            ("overage", "overage"),
            ("seven_day_opus", "seven_day_opus"),
            ("zork9", "zork9"),
            ("Overage", "overage"),
            ("five_hour", "five_hour"),
            ("weird value!", "other"),
            ("seven-day-oi", "other"),
            (long.as_str(), "other"),
            ("", "other"),
        ] {
            let (out, logs) = classify_logged(&fable_only(&[(CLAIM, Some(claim))]));
            assert_eq!(out, whole_account(), "{claim:?}");
            let lines: Vec<&str> = logs
                .lines()
                .filter(|line| line.contains(UNRECOGNIZED_CLAIM_LINE))
                .collect();
            assert_eq!(lines.len(), 1, "{claim:?}: {logs}");
            let line = lines[0];
            assert!(line.contains("INFO"), "{line}");
            assert!(
                line.ends_with(&format!("claim={word} suppressed=0")),
                "{claim:?}: {line}"
            );
            assert!(!line.contains("anthropic-ratelimit"), "{line}");
            if word == "other" && !claim.is_empty() {
                assert!(!line.contains(claim), "{line}");
            }
        }
        let capture = fable_only(&[
            (CLAIM, Some("seven_day")),
            ("anthropic-ratelimit-unified-7d-status", Some("rejected")),
            ("anthropic-ratelimit-unified-7d_oi-status", Some("allowed")),
        ]);
        let (out, logs) = classify_logged(&capture);
        assert_eq!(out, whole_account());
        assert!(!logs.contains(UNRECOGNIZED_CLAIM_LINE), "{logs}");
    }

    /// Every rule-0 input must be readable: a value that isn't visible
    /// ASCII (here byte 0xff) is unknown, never absent, so rule 0 is not
    /// applied and today's rules decide (the unified rejection here).
    #[test]
    #[serial_test::serial(anthropic_429_log_capture)]
    fn a_fable_limit_needs_every_input_readable() {
        for (name, bytes) in [
            ("anthropic-ratelimit-unified-7d_oi-status", &b"rejected\xff"[..]),
            ("anthropic-ratelimit-unified-5h-status", &b"allowed\xff"[..]),
            ("anthropic-ratelimit-unified-7d-status", &b"allowed\xff"[..]),
            ("anthropic-ratelimit-unified-5h-utilization", &b"0.36\xff"[..]),
            ("anthropic-ratelimit-unified-7d-utilization", &b"0.5\xff"[..]),
            (CLAIM, &b"seven_day_oi\xff"[..]),
        ] {
            let mut h = fable_only(&[]);
            h.insert(name, HeaderValue::from_bytes(bytes).unwrap());
            assert!(h.get(name).unwrap().to_str().is_err(), "{name}");
            assert_eq!(classify_anthropic_429(&h, "", NOW_MS), whole_account(), "{name}");
        }
    }

    /// Every rule-0 input must be sent once: a header repeated, with
    /// another value or the same one, is unknown, so rule 0 is not applied
    /// and today's rules decide (the unified rejection here; those rules
    /// read a repeated header's first value, as before).
    #[test]
    #[serial_test::serial(anthropic_429_log_capture)]
    fn a_fable_limit_needs_every_input_sent_once() {
        for (name, first, second) in [
            ("anthropic-ratelimit-unified-7d_oi-status", "rejected", "allowed"),
            ("anthropic-ratelimit-unified-5h-status", "allowed", "rejected"),
            ("anthropic-ratelimit-unified-7d-status", "allowed", "rejected"),
            ("anthropic-ratelimit-unified-5h-utilization", "0.36", "1.0"),
            ("anthropic-ratelimit-unified-7d-utilization", "0.5", "1.0"),
            (CLAIM, "seven_day_oi", "seven_day"),
            (CLAIM, "seven_day_oi", "seven_day_oi"),
        ] {
            let mut h = fable_only(&[(name, Some(first))]);
            h.append(name, HeaderValue::from_static(second));
            assert_eq!(h.get_all(name).iter().count(), 2, "{name}");
            assert_eq!(
                classify_anthropic_429(&h, "", NOW_MS),
                whole_account(),
                "{name}: {first}, then {second}"
            );
        }
    }

    /// A shared utilization that isn't sent, or is below 0, is unknown, not
    /// room: rule 0 is not applied and today's rules decide.
    #[test]
    fn a_fable_limit_needs_both_shared_utilizations_reported() {
        for utilization in [
            "anthropic-ratelimit-unified-5h-utilization",
            "anthropic-ratelimit-unified-7d-utilization",
        ] {
            for value in [None, Some("-0.1")] {
                let h = fable_only(&[(utilization, value)]);
                assert_eq!(
                    classify_anthropic_429(&h, "", NOW_MS),
                    whole_account(),
                    "{utilization}: {value:?}"
                );
            }
        }
    }

    /// The unrecognized-claim line is admitted process-wide once a minute:
    /// a second 429 inside 60 s leaves no line, and the next line, a minute
    /// after the last one, says how many were left out.
    #[test]
    #[serial_test::serial(anthropic_429_log_capture)]
    fn the_unrecognized_claim_line_is_limited_to_one_a_minute() {
        let h = fable_only(&[(CLAIM, Some("overage"))]);
        reset_claim_line_limiter();
        let (out, logs) = classify_logged_on(&h, NOW_MS, 0);
        assert_eq!(out, whole_account());
        let lines = claim_lines(&logs);
        assert_eq!(lines.len(), 1, "{logs}");
        assert!(lines[0].ends_with("claim=overage suppressed=0"), "{logs}");

        let (out, logs) = classify_logged_on(&h, NOW_MS + 59_999, 59_999);
        assert_eq!(out, whole_account(), "the classification is unchanged");
        assert!(claim_lines(&logs).is_empty(), "{logs}");

        let (_, logs) = classify_logged_on(&h, NOW_MS + 60_000, 60_000);
        let lines = claim_lines(&logs);
        assert_eq!(lines.len(), 1, "{logs}");
        assert!(lines[0].ends_with("claim=overage suppressed=1"), "{logs}");
    }

    /// The limiter keeps its own time, not the response's: a 429 whose
    /// time is earlier than the last line's (a clock stepped back, or a
    /// caller that took its time before another one did) or a minute later
    /// is counted, not logged, while the limiter's own clock is inside the
    /// minute; so is one when that clock reads earlier than its last line
    /// (which a monotonic clock never does). Only the limiter's own minute
    /// lets the next line out.
    #[test]
    #[serial_test::serial(anthropic_429_log_capture)]
    fn the_unrecognized_claim_line_keeps_its_own_time() {
        let h = fable_only(&[(CLAIM, Some("overage"))]);
        reset_claim_line_limiter();
        let (_, logs) = classify_logged_on(&h, NOW_MS, 10_000);
        assert_eq!(claim_lines(&logs).len(), 1, "{logs}");
        for (now_ms, limiter_ms, case) in [
            (NOW_MS - 1, 10_001, "a response 1 ms earlier"),
            (NOW_MS - 3_600_000, 10_002, "a response an hour earlier"),
            (NOW_MS + 60_000, 10_003, "a response a minute later"),
            (NOW_MS, 9_000, "the limiter's clock earlier than its last line"),
        ] {
            let (out, logs) = classify_logged_on(&h, now_ms, limiter_ms);
            assert!(
                matches!(
                    out,
                    Anthropic429::QuotaExhausted(AnthropicQuotaEvidence {
                        signal: AnthropicQuotaSignal::UnifiedStatusRejected,
                        ..
                    })
                ),
                "{case}: the classification is unchanged: {out:?}"
            );
            assert!(claim_lines(&logs).is_empty(), "{case}: {logs}");
        }
        let (_, logs) = classify_logged_on(&h, NOW_MS - 2, 70_000);
        let lines = claim_lines(&logs);
        assert_eq!(lines.len(), 1, "{logs}");
        assert!(lines[0].ends_with("claim=overage suppressed=4"), "{logs}");
    }

    /// Each line's count starts again from 0: after a line that says how
    /// many were left out, the next one counts only those left out since.
    #[test]
    #[serial_test::serial(anthropic_429_log_capture)]
    fn the_unrecognized_claim_lines_count_starts_again_after_each_line() {
        let h = fable_only(&[(CLAIM, Some("overage"))]);
        reset_claim_line_limiter();
        let mut lines = Vec::new();
        for offset in [0, 1_000, 2_000, 60_000, 61_000, 120_000] {
            let (_, logs) = classify_logged_on(&h, NOW_MS + offset, offset);
            for line in claim_lines(&logs) {
                let count = line.rsplit(' ').next().unwrap_or_default().to_string();
                lines.push((offset, count));
            }
        }
        let expected: Vec<(u64, String)> = [(0, 0), (60_000, 2), (120_000, 1)]
            .into_iter()
            .map(|(offset, n)| (offset, format!("suppressed={n}")))
            .collect();
        assert_eq!(lines, expected);
    }

    /// What the router does with the evidence of a Fable request refused
    /// with an unknown shared status or an unrecognized claim: it reports a
    /// whole-account cooldown, not one of Fable's requests only. (The local
    /// broker applies a whole-account cooldown to every model's requests:
    /// broker_capacity's `a_whole_account_limit_stops_opus_requests_too`.)
    #[tokio::test]
    #[serial_test::serial(anthropic_429_log_capture)]
    async fn unknown_headers_on_a_fable_request_cool_the_whole_account() {
        for change in [
            ("anthropic-ratelimit-unified-5h-status", Some("garbage")),
            (CLAIM, Some("overage")),
            ("anthropic-ratelimit-unified-5h-utilization", Some("1.0")),
        ] {
            let Anthropic429::QuotaExhausted(evidence) =
                classify_anthropic_429(&fable_only(&[change]), "", NOW_MS)
            else {
                panic!("{change:?}: a recognized limit");
            };
            let evidence = evidence.for_request_model("claude-fable-5-1");
            let broker = ScriptedBroker::new(AccountSelector::Auto, Some("claude2"));
            BrokerAnthropicRouter::new(broker.clone())
                .report_exhausted(&seat("claude4"), &evidence)
                .await;
            assert!(broker.model_cooldowns.lock().unwrap().is_empty(), "{change:?}");
            let cooldowns = broker.cooldowns.lock().unwrap().clone();
            assert_eq!(cooldowns.len(), 1, "{change:?}");
            assert_eq!(cooldowns[0].0, "anthropic@claude4", "{change:?}");
        }
    }

    /// A Fable limit on a request for another model can't be what refused
    /// it: it stops the whole account (the 7d_oi window named), with the
    /// same reset. A Fable request keeps it; other evidence is unchanged.
    #[test]
    fn a_fable_limit_on_another_models_request_stops_the_whole_account() {
        let fable = AnthropicQuotaEvidence {
            signal: FABLE_OI,
            resets_at_ms: Some(NOW_MS + 1000),
        };
        for model in ["claude-fable-5-1", "anthropic/claude-fable-5-1"] {
            assert_eq!(fable.for_request_model(model), fable, "{model}");
        }
        for model in ["claude-opus-5-5", "claude-haiku-4-5", "claude-fablelike-1"] {
            let seat = fable.for_request_model(model);
            assert_eq!(
                seat.signal,
                AnthropicQuotaSignal::WindowStatusRejected { window: "7d_oi" },
                "{model}"
            );
            assert_eq!(seat.resets_at_ms, fable.resets_at_ms, "{model}");
            assert_eq!(seat.model_scope(), None, "{model}");
        }
        let unified = AnthropicQuotaEvidence {
            signal: AnthropicQuotaSignal::UnifiedStatusRejected,
            resets_at_ms: None,
        };
        assert_eq!(unified.for_request_model("claude-opus-5-5"), unified);
    }

    // ── router over a scripted broker ───────────────────────────────────────

    struct ScriptedBroker {
        selector: Mutex<AccountSelector>,
        /// Seat the auto selector answers with; `None` = NoAccountAvailable.
        pick: Mutex<Option<&'static str>>,
        /// Force a non-availability broker error from the auto vend.
        vend_error: Mutex<Option<BrokerError>>,
        cooldowns: Mutex<Vec<(String, Option<u64>, String)>>,
        /// Cooldowns of one model's requests: the account, the model, until.
        model_cooldowns: Mutex<Vec<(String, String, Option<u64>)>>,
        auto_calls: Mutex<Vec<Option<String>>>,
        /// The `exclude` list of every excluding vend.
        excluded: Mutex<Vec<Vec<CredentialRef>>>,
    }

    impl ScriptedBroker {
        fn new(selector: AccountSelector, pick: Option<&'static str>) -> Arc<Self> {
            Arc::new(Self {
                selector: Mutex::new(selector),
                pick: Mutex::new(pick),
                vend_error: Mutex::new(None),
                cooldowns: Mutex::new(Vec::new()),
                model_cooldowns: Mutex::new(Vec::new()),
                auto_calls: Mutex::new(Vec::new()),
                excluded: Mutex::new(Vec::new()),
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
                    failure: None,
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
        /// Records the exclusion, then answers like the trait default:
        /// the scripted pick, refused when it is excluded.
        async fn access_token_pinned_excluding(
            &self,
            provider: OAuthProviderId,
            model: Option<&str>,
            exclude: &[CredentialRef],
        ) -> Result<PinnedToken, BrokerError> {
            self.excluded.lock().unwrap().push(exclude.to_vec());
            let pinned = self.access_token_pinned_for(provider, model).await?;
            if exclude.contains(&pinned.credential) {
                return Err(BrokerError::NoAccountAvailable {
                    provider: provider.as_str().into(),
                    reason: "scripted: already attempted".into(),
                    failure: None,
                });
            }
            Ok(pinned)
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
        async fn report_model_cooldown(
            &self,
            cred: &CredentialRef,
            model: &str,
            until_ms: Option<u64>,
            _reason: &str,
        ) -> Result<(), BrokerError> {
            self.model_cooldowns
                .lock()
                .unwrap()
                .push((cred.storage_key(), model.to_string(), until_ms));
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

    /// A broker that keeps no model cooldowns: the trait's defaults.
    struct SeatOnly(Arc<ScriptedBroker>);

    #[async_trait]
    impl CredentialBroker for SeatOnly {
        async fn access_token(&self, p: OAuthProviderId) -> Result<AccessToken, BrokerError> {
            self.0.access_token(p).await
        }
        async fn report_cooldown(
            &self,
            cred: &CredentialRef,
            until_ms: Option<u64>,
            reason: &str,
        ) -> Result<(), BrokerError> {
            self.0.report_cooldown(cred, until_ms, reason).await
        }
        async fn proxy(
            &self,
            r: crate::auth::ProxyRequest,
        ) -> Result<crate::auth::ProxyResponse, BrokerError> {
            self.0.proxy(r).await
        }
        async fn proxy_stream(
            &self,
            r: crate::auth::ProxyRequest,
        ) -> Result<crate::auth::ProxyByteStream, BrokerError> {
            self.0.proxy_stream(r).await
        }
        async fn anthropic_usage(&self) -> Result<serde_json::Value, BrokerError> {
            self.0.anthropic_usage().await
        }
        async fn capabilities(&self) -> Result<Vec<crate::auth::ProviderStatus>, BrokerError> {
            self.0.capabilities().await
        }
    }

    /// A4: a Fable limit is reported as a cooldown of Fable's requests only,
    /// until its reset; any other evidence still cools the whole account. A
    /// broker that keeps no model cooldowns (the trait's default) cools the
    /// whole account for a Fable limit too.
    #[tokio::test]
    async fn router_reports_a_fable_limit_as_a_cooldown_of_fables_requests() {
        let broker = ScriptedBroker::new(AccountSelector::Auto, Some("claude2"));
        let router = BrokerAnthropicRouter::new(broker.clone());
        let reset = crate::epoch_millis() + 2 * 3600 * 1000;
        let fable = AnthropicQuotaEvidence {
            signal: FABLE_OI,
            resets_at_ms: Some(reset),
        };
        router.report_exhausted(&seat("claude4"), &fable).await;
        assert_eq!(
            broker.model_cooldowns.lock().unwrap().clone(),
            vec![("anthropic@claude4".to_string(), "fable".to_string(), Some(reset))]
        );
        assert!(
            broker.cooldowns.lock().unwrap().is_empty(),
            "the whole account is not cooled"
        );
        router
            .report_exhausted(&seat("claude4"), &evidence(Some(reset)))
            .await;
        assert_eq!(broker.cooldowns.lock().unwrap().len(), 1);
        assert_eq!(broker.model_cooldowns.lock().unwrap().len(), 1);

        let seat_only = Arc::new(SeatOnly(ScriptedBroker::new(
            AccountSelector::Auto,
            Some("claude2"),
        )));
        BrokerAnthropicRouter::new(seat_only.clone())
            .report_exhausted(&seat("claude4"), &fable)
            .await;
        assert_eq!(
            seat_only.0.cooldowns.lock().unwrap().clone(),
            vec![(
                "anthropic@claude4".to_string(),
                Some(reset),
                COOLDOWN_REASON_QUOTA.to_string()
            )]
        );
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
            .failover("anthropic/claude-opus-4-7", &[seat("claude4")])
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
            .failover("claude-opus-4-7", &[seat("claude2")])
            .await
            .unwrap();
        assert!(same.is_none());
    }

    #[tokio::test]
    async fn router_failover_maps_no_account_to_none_and_other_errors_to_err() {
        let broker = ScriptedBroker::new(AccountSelector::Auto, None);
        let router = BrokerAnthropicRouter::new(broker.clone());
        assert!(router
            .failover("claude-opus-4-7", &[seat("claude4")])
            .await
            .unwrap()
            .is_none());
        *broker.vend_error.lock().unwrap() = Some(BrokerError::Transport("down".into()));
        let err = router
            .failover("claude-opus-4-7", &[seat("claude4")])
            .await
            .unwrap_err();
        assert!(err.contains("down"));
    }

    #[tokio::test]
    async fn router_failover_passes_every_attempted_account_and_never_answers_one() {
        let broker = ScriptedBroker::new(AccountSelector::Auto, Some("claude2"));
        let router = BrokerAnthropicRouter::new(broker.clone());
        let attempted = [seat("claude4"), seat("claude5")];
        let next = router
            .failover("claude-opus-4-7", &attempted)
            .await
            .unwrap()
            .expect("claude2 was not attempted");
        assert_eq!(next.credential, seat("claude2"));
        // The broker's answer is attempted ⇒ no failover.
        let attempted_too = [seat("claude4"), seat("claude2")];
        assert!(router
            .failover("claude-opus-4-7", &attempted_too)
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            broker.excluded.lock().unwrap().clone(),
            vec![attempted.to_vec(), attempted_too.to_vec()],
            "the broker is told every attempted account, in order"
        );
    }

    #[tokio::test]
    async fn router_failover_refuses_when_selector_became_explicit() {
        let broker = ScriptedBroker::new(AccountSelector::Auto, Some("claude2"));
        let router = BrokerAnthropicRouter::new(broker.clone());
        *broker.selector.lock().unwrap() = AccountSelector::Account(Account::Default);
        assert!(!router.auto_selected());
        assert!(router
            .failover("claude-opus-4-7", &[seat("claude4")])
            .await
            .unwrap()
            .is_none());
        // The broker was never asked to vend.
        assert!(broker.auto_calls.lock().unwrap().is_empty());
    }
}
