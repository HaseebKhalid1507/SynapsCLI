//! Account-aware selection and bounded failover for the Codex transport (G5).
//!
//! Three concerns live here, deliberately separated so each is testable
//! without the others:
//!
//! 1. **Wire evidence** — pure classification of what the Codex backend
//!    said. A quota-exhausted account is recognized ONLY from provider-declared
//!    signals (`usage_limit_reached` body type, `x-codex-*-used-percent >= 100`
//!    headers, or the vetted in-stream quota error identifiers). A generic 429
//!    is a transient rate limit and is never treated as exhaustion. Provider
//!    bodies are read into a bounded buffer for classification and dropped:
//!    they can echo request content and are never logged, stored or surfaced.
//! 2. **Runtime gate** — the preconditions the runtime enforces before it even
//!    asks for a failover candidate: the selector must be `Auto` (an explicit
//!    account is never overridden), nothing may have been streamed on this
//!    turn (no cross-account replay), and at most
//!    [`MAX_CODEX_ACCOUNT_FAILOVERS`] switches per request. These are not the
//!    capacity policy — that stays in `auth::quota_policy` — they are the
//!    runtime's own safety invariants.
//! 3. **Router seam** — [`CodexAccountRouter`] is what the stream path talks
//!    to. [`BrokerCodexRouter`] adapts the `CredentialBroker` trait; tests use
//!    a scripted router. The seam returns one [`PinnedAccount`] per attempt so
//!    the bearer token and the `chatgpt-account-id` header are always derived
//!    from the same credential.

use async_trait::async_trait;
use reqwest::header::HeaderMap;
use serde_json::Value;
use std::sync::Arc;

use crate::auth::{
    AccountSelector, BrokerError, CredentialBroker, CredentialRef, OAuthProviderId, PinnedToken,
};

/// Upper bound on account switches per logical request. One is the whole
/// design: a second exhausted seat means the operator needs to know, not a
/// silent tour through every account.
pub(crate) const MAX_CODEX_ACCOUNT_FAILOVERS: u32 = 1;

/// Bytes of a 429 body read for classification. Matches the broker's
/// `MAX_UPSTREAM_ERROR_BYTES`; anything past the cap is discarded unread.
pub(crate) const QUOTA_PROBE_BODY_CAP: usize = 2 * 1024;

/// Which provider-declared signal proved exhaustion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QuotaSignal {
    /// HTTP 429 body: `error.type` / `error.code` == `usage_limit_reached`.
    UsageLimitReachedBody,
    /// HTTP 429 header: `x-codex-primary-used-percent >= 100`.
    PrimaryWindowHeader,
    /// HTTP 429 header: `x-codex-secondary-used-percent >= 100`.
    SecondaryWindowHeader,
    /// In-stream terminal failure with a vetted quota identifier.
    StreamQuotaEvent,
}

/// Recognized quota exhaustion for the account that made the request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CodexQuotaEvidence {
    pub(crate) signal: QuotaSignal,
    /// Provider-reported seconds until the exhausted window resets, when the
    /// response carried one (`error.resets_in_seconds` or
    /// `x-codex-*-reset-after-seconds`). `None` = unknown, never guessed.
    pub(crate) reset_after_secs: Option<u64>,
}

impl CodexQuotaEvidence {
    /// Absolute cooldown deadline (epoch ms) for the failed account, if the
    /// provider said when the window resets.
    pub(crate) fn cooldown_until_ms(&self, now_ms: u64) -> Option<u64> {
        self.reset_after_secs
            .map(|secs| now_ms.saturating_add(secs.saturating_mul(1000)))
    }
}

/// Classification of a Codex HTTP 429.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Codex429 {
    /// Provider-declared quota exhaustion for this account.
    QuotaExhausted(CodexQuotaEvidence),
    /// `usage_not_included`: this ChatGPT account has no Codex entitlement.
    /// Deterministic for the account — never retried, never a failover
    /// trigger (it is not a quota window that another seat can cover).
    UsageNotIncluded,
    /// Ordinary rate limit: transient, keeps the normal retry path, and is
    /// NOT evidence of weekly exhaustion.
    Generic,
}

/// Typed error carried out of the send loop when a 429 proved exhaustion.
/// The stream path downcasts to it; every other consumer sees the static
/// quota message via `Display`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CodexQuotaExhausted {
    pub(crate) evidence: CodexQuotaEvidence,
    /// Provider request id from validated headers, for the trace record.
    pub(crate) request_id: Option<crate::runtime::trace::TraceId>,
}

impl std::fmt::Display for CodexQuotaExhausted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Codex{}", super::stream::RESPONSES_QUOTA_SUFFIX)
    }
}

impl std::error::Error for CodexQuotaExhausted {}

/// Sanitized identifiers (already reduced by `sanitize_error_identifier`)
/// that mean "this account's usage allowance is spent". Vetted list — a
/// capacity-looking free-text message is never enough. `billing_error` is
/// deliberately absent: it is a payment problem, not a spent window, and is
/// not safe failover evidence.
const STREAM_QUOTA_IDENTIFIERS: &[&str] = &[
    "insufficient_quota",
    "quota_exceeded",
    "usage_limit_reached",
];

/// True when either sanitized identifier of an in-stream terminal failure is
/// a vetted quota identifier and the other does not contradict it.
pub(crate) fn stream_failure_is_quota(error_kind: &str, error_code: &str) -> bool {
    let quota = |v: &str| STREAM_QUOTA_IDENTIFIERS.contains(&v);
    let neutral = |v: &str| matches!(v, "absent" | "error");
    (quota(error_kind) && (quota(error_code) || neutral(error_code)))
        || (quota(error_code) && (quota(error_kind) || neutral(error_kind)))
}

fn header_f64(headers: &HeaderMap, name: &str) -> Option<f64> {
    headers
        .get(name)?
        .to_str()
        .ok()?
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
}

fn header_u64(headers: &HeaderMap, name: &str) -> Option<u64> {
    header_f64(headers, name).and_then(|v| {
        if v >= 0.0 {
            Some(v.round() as u64)
        } else {
            None
        }
    })
}

/// Identifier-shaped `error.type` / `error.code` values only (lowercase
/// snake_case, ≤ 64 bytes). Free text or secret-shaped values classify as
/// nothing. Mirrors `stream::sanitize_error_identifier` without exporting it.
fn body_identifier(v: Option<&Value>) -> Option<&str> {
    let s = v?.as_str()?;
    (!s.is_empty()
        && s.len() <= 64
        && s
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'))
    .then_some(s)
}

/// Classify a Codex HTTP 429 from its headers and a bounded body slice.
/// The body is consulted only to produce the enum — it is never retained.
pub(crate) fn classify_codex_429(headers: &HeaderMap, body: &[u8]) -> Codex429 {
    // Body evidence first: it is the signal codex-rs itself keys on.
    if let Ok(json) = serde_json::from_slice::<Value>(body) {
        let error = json.get("error").filter(|e| e.is_object());
        let kind = body_identifier(error.and_then(|e| e.get("type")));
        let code = body_identifier(error.and_then(|e| e.get("code")));
        let reset = error
            .and_then(|e| e.get("resets_in_seconds"))
            .and_then(Value::as_f64)
            .filter(|v| v.is_finite() && *v >= 0.0)
            .map(|v| v.round() as u64);
        if kind == Some("usage_limit_reached") || code == Some("usage_limit_reached") {
            return Codex429::QuotaExhausted(CodexQuotaEvidence {
                signal: QuotaSignal::UsageLimitReachedBody,
                reset_after_secs: reset.or_else(|| {
                    header_u64(headers, "x-codex-primary-reset-after-seconds")
                }),
            });
        }
        if kind == Some("usage_not_included") || code == Some("usage_not_included") {
            return Codex429::UsageNotIncluded;
        }
    }
    // Header evidence: the backend reports per-window utilization on every
    // response; a 429 with a window at/over 100% is provider-declared
    // exhaustion of that window.
    for (window, used, reset, signal) in [
        (
            "primary",
            "x-codex-primary-used-percent",
            "x-codex-primary-reset-after-seconds",
            QuotaSignal::PrimaryWindowHeader,
        ),
        (
            "secondary",
            "x-codex-secondary-used-percent",
            "x-codex-secondary-reset-after-seconds",
            QuotaSignal::SecondaryWindowHeader,
        ),
    ] {
        if header_f64(headers, used).is_some_and(|pct| pct >= 100.0) {
            tracing::debug!(window, "codex 429 carries an exhausted window header");
            return Codex429::QuotaExhausted(CodexQuotaEvidence {
                signal,
                reset_after_secs: header_u64(headers, reset),
            });
        }
    }
    Codex429::Generic
}

/// Read at most `cap` bytes of a response body for classification, then
/// drop the connection. Errors mid-read yield whatever was received.
pub(crate) async fn read_body_for_classification(resp: reqwest::Response, cap: usize) -> Vec<u8> {
    use futures::StreamExt;
    let mut out = Vec::with_capacity(cap.min(512));
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else { break };
        let room = cap.saturating_sub(out.len());
        if room == 0 {
            break;
        }
        out.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if out.len() >= cap {
            break;
        }
    }
    out
}

// ── Runtime gate ─────────────────────────────────────────────────────────────

/// Why the runtime refused to even look for a failover candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FailoverBlocked {
    /// The selector pinned an explicit account (default or named).
    ExplicitSelection,
    /// Text or tool events already reached the UI on this turn.
    OutputAlreadyStreamed,
    /// `MAX_CODEX_ACCOUNT_FAILOVERS` already spent.
    BudgetExhausted,
}

impl FailoverBlocked {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::ExplicitSelection => "explicit_selection",
            Self::OutputAlreadyStreamed => "output_already_streamed",
            Self::BudgetExhausted => "failover_budget_exhausted",
        }
    }
}

/// Runtime preconditions for a Codex account failover. Pure; evaluated
/// before any candidate lookup so an explicit account or a partially
/// streamed turn never triggers usage fetches or token vends.
pub(crate) fn failover_gate(
    auto_selected: bool,
    output_started: bool,
    failovers_so_far: u32,
) -> Result<(), FailoverBlocked> {
    if !auto_selected {
        return Err(FailoverBlocked::ExplicitSelection);
    }
    if output_started {
        return Err(FailoverBlocked::OutputAlreadyStreamed);
    }
    if failovers_so_far >= MAX_CODEX_ACCOUNT_FAILOVERS {
        return Err(FailoverBlocked::BudgetExhausted);
    }
    Ok(())
}

// ── Router seam ──────────────────────────────────────────────────────────────

/// One resolved account/token pair. The `chatgpt-account-id` header is
/// derived from `token` by the caller, so header and bearer cannot diverge.
#[derive(Clone)]
pub(crate) struct PinnedAccount {
    /// The credential the broker selected. Its storage key
    /// (`openai-codex` | `openai-codex@label`, via `Display`) is the identity
    /// used for distinctness checks, cooldown reports and log lines.
    pub(crate) credential: CredentialRef,
    /// Short-lived bearer. Never logged; `Debug` redacts.
    pub(crate) token: String,
    /// True when the broker's selector for the provider is `Auto` — the only
    /// mode in which the runtime may switch accounts.
    pub(crate) auto: bool,
}

impl PinnedAccount {
    pub(crate) fn from_pinned(pinned: PinnedToken, auto: bool) -> Self {
        Self {
            credential: pinned.credential,
            token: pinned.token.token,
            auto,
        }
    }

    /// `default` or the label — safe for notices.
    pub(crate) fn label(&self) -> &str {
        self.credential.account.label_str()
    }
}

impl std::fmt::Debug for PinnedAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PinnedAccount")
            .field("credential", &self.credential)
            .field("token", &"[REDACTED]")
            .field("auto", &self.auto)
            .finish()
    }
}

/// What the Codex stream path needs from account selection. Exactly one
/// implementation talks to the real broker; tests script the rest.
#[async_trait]
pub(crate) trait CodexAccountRouter: Send + Sync {
    /// Resolve the selected account and its token for `model`. One pair.
    async fn pin(&self, model: &str) -> Result<PinnedAccount, String>;

    /// After recognized exhaustion on `failed`, resolve a DISTINCT account
    /// with fresh, model-relevant capacity and its token. `Ok(None)` when no
    /// eligible seat exists. Called only after [`failover_gate`] passed.
    async fn failover(
        &self,
        model: &str,
        failed: &PinnedAccount,
        evidence: &CodexQuotaEvidence,
        failovers_so_far: u32,
    ) -> Result<Option<PinnedAccount>, String>;

    /// Best-effort cooldown report for `failed`. Must never fail the turn.
    async fn report_exhausted(&self, failed: &PinnedAccount, evidence: &CodexQuotaEvidence);
}

/// Cooldown reason reported to the broker for recognized exhaustion.
pub(crate) const COOLDOWN_REASON_QUOTA: &str = "codex_quota_exhausted";

/// Adapter over the credential broker. Vends through the broker boundary
/// only — no `auth.json` access here.
///
/// Selection policy lives in exactly one place: the broker's `Auto`
/// selector (`quota_policy::select` over fresh usage snapshots + cooldowns,
/// model-aware via `access_token_pinned_for`). Failover therefore does not
/// rebuild candidates in the runtime; it reports the exhausted seat's
/// cooldown and asks the broker to select again, then verifies the answer is
/// a DIFFERENT account. A broker that cannot honour the cooldown (default
/// no-op sink) answers with the same seat, which the runtime refuses —
/// bounded and safe, never a loop.
pub(crate) struct BrokerCodexRouter {
    broker: Arc<dyn CredentialBroker>,
}

impl BrokerCodexRouter {
    pub(crate) fn new(broker: Arc<dyn CredentialBroker>) -> Self {
        Self { broker }
    }
}

#[async_trait]
impl CodexAccountRouter for BrokerCodexRouter {
    async fn pin(&self, model: &str) -> Result<PinnedAccount, String> {
        let provider = OAuthProviderId::OpenAiCodex;
        let auto = matches!(self.broker.account_selector(provider), AccountSelector::Auto);
        let pinned = self
            .broker
            .access_token_pinned_for(provider, Some(model))
            .await
            .map_err(|e| e.to_string())?;
        Ok(PinnedAccount::from_pinned(pinned, auto))
    }

    async fn failover(
        &self,
        model: &str,
        failed: &PinnedAccount,
        _evidence: &CodexQuotaEvidence,
        _failovers_so_far: u32,
    ) -> Result<Option<PinnedAccount>, String> {
        let provider = OAuthProviderId::OpenAiCodex;
        // The gate already established `Auto`; re-check so a policy that
        // became explicit mid-request is honoured (never overridden).
        if !matches!(self.broker.account_selector(provider), AccountSelector::Auto) {
            return Ok(None);
        }
        match self.broker.access_token_pinned_for(provider, Some(model)).await {
            Ok(next) if next.credential != failed.credential => {
                Ok(Some(PinnedAccount::from_pinned(next, true)))
            }
            Ok(_same_seat) => Ok(None),
            Err(BrokerError::NoAccountAvailable { .. }) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }

    async fn report_exhausted(&self, failed: &PinnedAccount, evidence: &CodexQuotaEvidence) {
        let until_ms = evidence.cooldown_until_ms(crate::epoch_millis());
        if let Err(e) = self
            .broker
            .report_cooldown(&failed.credential, until_ms, COOLDOWN_REASON_QUOTA)
            .await
        {
            tracing::warn!(
                account = %failed.credential,
                error = %e,
                "codex cooldown report failed (best effort)"
            );
        }
        tracing::warn!(
            account = %failed.credential,
            signal = ?evidence.signal,
            reset_after_secs = ?evidence.reset_after_secs,
            "codex account quota exhausted"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue};

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn body_usage_limit_reached_is_exhaustion_with_reset_hint() {
        let body = br#"{"error":{"type":"usage_limit_reached","message":"ECHOED prompt","plan_type":"plus","resets_in_seconds":3600}}"#;
        let class = classify_codex_429(&HeaderMap::new(), body);
        assert_eq!(
            class,
            Codex429::QuotaExhausted(CodexQuotaEvidence {
                signal: QuotaSignal::UsageLimitReachedBody,
                reset_after_secs: Some(3600),
            })
        );
    }

    #[test]
    fn body_code_field_also_counts_and_header_reset_fills_missing_body_hint() {
        let body = br#"{"error":{"code":"usage_limit_reached","message":"x"}}"#;
        let h = headers(&[("x-codex-primary-reset-after-seconds", "120")]);
        let class = classify_codex_429(&h, body);
        assert_eq!(
            class,
            Codex429::QuotaExhausted(CodexQuotaEvidence {
                signal: QuotaSignal::UsageLimitReachedBody,
                reset_after_secs: Some(120),
            })
        );
    }

    #[test]
    fn usage_not_included_is_its_own_class() {
        let body = br#"{"error":{"type":"usage_not_included","message":"no codex"}}"#;
        assert_eq!(
            classify_codex_429(&HeaderMap::new(), body),
            Codex429::UsageNotIncluded
        );
    }

    #[test]
    fn generic_429_is_not_exhaustion() {
        for body in [
            br#"{"error":{"type":"rate_limit_exceeded","message":"slow down"}}"#.as_slice(),
            br#"{"error":{"message":"Too many requests"}}"#.as_slice(),
            b"not json at all".as_slice(),
            b"".as_slice(),
        ] {
            assert_eq!(
                classify_codex_429(&HeaderMap::new(), body),
                Codex429::Generic,
                "{:?}",
                String::from_utf8_lossy(body)
            );
        }
        // Partial utilization headers are informational, not exhaustion.
        let h = headers(&[
            ("x-codex-primary-used-percent", "87.5"),
            ("x-codex-secondary-used-percent", "12"),
        ]);
        assert_eq!(classify_codex_429(&h, b""), Codex429::Generic);
    }

    #[test]
    fn free_text_or_secret_shaped_identifiers_never_classify() {
        // Mixed case / spaces / long values are not identifier-shaped.
        for kind in [
            "Usage Limit Reached",
            "USAGE_LIMIT_REACHED",
            "sk-abcDEF123",
            &"a".repeat(65),
        ] {
            let body = serde_json::json!({"error": {"type": kind}}).to_string();
            assert_eq!(
                classify_codex_429(&HeaderMap::new(), body.as_bytes()),
                Codex429::Generic,
                "{kind}"
            );
        }
    }

    #[test]
    fn exhausted_window_headers_prove_exhaustion() {
        let h = headers(&[
            ("x-codex-primary-used-percent", "100"),
            ("x-codex-primary-reset-after-seconds", "86400"),
        ]);
        assert_eq!(
            classify_codex_429(&h, b"{}"),
            Codex429::QuotaExhausted(CodexQuotaEvidence {
                signal: QuotaSignal::PrimaryWindowHeader,
                reset_after_secs: Some(86400),
            })
        );
        let h = headers(&[
            ("x-codex-primary-used-percent", "40"),
            ("x-codex-secondary-used-percent", "100.0"),
        ]);
        assert_eq!(
            classify_codex_429(&h, b"garbage"),
            Codex429::QuotaExhausted(CodexQuotaEvidence {
                signal: QuotaSignal::SecondaryWindowHeader,
                reset_after_secs: None,
            })
        );
        // Non-numeric / negative header values are ignored.
        let h = headers(&[("x-codex-primary-used-percent", "lots")]);
        assert_eq!(classify_codex_429(&h, b""), Codex429::Generic);
    }

    #[test]
    fn body_evidence_outranks_headers() {
        let h = headers(&[("x-codex-primary-used-percent", "100")]);
        let body = br#"{"error":{"type":"usage_not_included"}}"#;
        assert_eq!(classify_codex_429(&h, body), Codex429::UsageNotIncluded);
    }

    #[test]
    fn cooldown_deadline_only_with_provider_hint() {
        let with = CodexQuotaEvidence {
            signal: QuotaSignal::UsageLimitReachedBody,
            reset_after_secs: Some(90),
        };
        assert_eq!(with.cooldown_until_ms(1_000), Some(91_000));
        let without = CodexQuotaEvidence {
            signal: QuotaSignal::StreamQuotaEvent,
            reset_after_secs: None,
        };
        assert_eq!(without.cooldown_until_ms(1_000), None);
    }

    #[test]
    fn stream_quota_identifiers_are_vetted_and_must_agree() {
        assert!(stream_failure_is_quota("insufficient_quota", "absent"));
        assert!(stream_failure_is_quota("absent", "quota_exceeded"));
        assert!(stream_failure_is_quota("error", "usage_limit_reached"));
        assert!(stream_failure_is_quota("quota_exceeded", "insufficient_quota"));
        // Contradicting identifiers are not quota.
        assert!(!stream_failure_is_quota("insufficient_quota", "server_error"));
        assert!(!stream_failure_is_quota("rate_limit_exceeded", "absent"));
        assert!(!stream_failure_is_quota("absent", "absent"));
        assert!(!stream_failure_is_quota("unsafe_or_freeform", "unsafe_or_freeform"));
        // A billing problem is not a spent window: never failover evidence.
        assert!(!stream_failure_is_quota("billing_error", "absent"));
        assert!(!stream_failure_is_quota("absent", "billing_error"));
    }

    #[test]
    fn gate_refuses_explicit_streamed_and_overspent() {
        assert_eq!(failover_gate(true, false, 0), Ok(()));
        assert_eq!(
            failover_gate(false, false, 0),
            Err(FailoverBlocked::ExplicitSelection)
        );
        assert_eq!(
            failover_gate(true, true, 0),
            Err(FailoverBlocked::OutputAlreadyStreamed)
        );
        assert_eq!(
            failover_gate(true, false, MAX_CODEX_ACCOUNT_FAILOVERS),
            Err(FailoverBlocked::BudgetExhausted)
        );
        // Explicit selection wins even over the other reasons: it is checked
        // first so an explicit account never has candidates looked up.
        assert_eq!(
            failover_gate(false, true, 5),
            Err(FailoverBlocked::ExplicitSelection)
        );
    }

    #[test]
    fn quota_error_display_is_the_static_quota_message() {
        let e = CodexQuotaExhausted {
            evidence: CodexQuotaEvidence {
                signal: QuotaSignal::UsageLimitReachedBody,
                reset_after_secs: Some(1),
            },
            request_id: None,
        };
        assert_eq!(
            e.to_string(),
            format!("Codex{}", super::super::stream::RESPONSES_QUOTA_SUFFIX)
        );
        let boxed: Box<dyn std::error::Error + Send + Sync> = Box::new(e);
        assert!(boxed.downcast_ref::<CodexQuotaExhausted>().is_some());
    }

    #[test]
    fn pinned_account_debug_redacts_token() {
        let p = PinnedAccount {
            credential: CredentialRef::new(
                OAuthProviderId::OpenAiCodex,
                crate::auth::Account::parse("astra2").unwrap(),
            ),
            token: "sk-SECRET".into(),
            auto: true,
        };
        let dbg = format!("{p:?}");
        assert!(dbg.contains("astra2"));
        assert!(!dbg.contains("SECRET"));
        assert_eq!(p.credential.storage_key(), "openai-codex@astra2");
        assert_eq!(p.label(), "astra2");
    }

    #[tokio::test]
    async fn bounded_body_read_stops_at_cap() {
        use axum::{routing::get, Router};
        let big = "x".repeat(10_000);
        let app = Router::new().route("/", get(move || async move { big.clone() }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let resp = reqwest::get(format!("http://{addr}/")).await.unwrap();
        let body = read_body_for_classification(resp, 100).await;
        assert_eq!(body.len(), 100);
    }
}
