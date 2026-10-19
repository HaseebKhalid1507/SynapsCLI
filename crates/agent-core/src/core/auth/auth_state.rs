//! `synaps auth state`: what one broker remembers about its accounts (each
//! account's cooldown, its last usage read and last token refresh exchange,
//! and the last automatic selection), read from memory only.
//!
//! Building a report makes no network request and refreshes nothing; the
//! account list (labels, access-token expiry, whether an account id is
//! recorded) comes from the credential store. A report never carries token
//! material, emails, account ids or identity hashes. `clear` drops the
//! in-memory cooldowns, usage readings and stickiness of the matched
//! accounts; it never writes the credential store.

use serde::{Deserialize, Serialize};

use super::quota_policy::{ObservationFailure, SelectionFailure};

/// Version of [`AuthStateReport`]'s JSON shape (`synaps auth state --json`).
pub const AUTH_STATE_SCHEMA_VERSION: u32 = 1;

/// Which accounts to report on, and whether to clear their state first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthStateQuery {
    /// Canonical provider id (`anthropic`); `None` = every provider that has
    /// stored accounts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// One account label (`default` for the bare slot).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// Drop the matched accounts' in-memory state before reporting.
    #[serde(default)]
    pub clear: bool,
}

/// One broker's in-memory account state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthStateReport {
    pub schema_version: u32,
    /// Answered by the running daemon's broker (`true`), or built by the
    /// asking process, which has served no request (`false`).
    pub live: bool,
    /// `false`: this process has no local broker yet (no request used
    /// automatic selection or reported a limit), so nothing is recorded.
    pub broker_active: bool,
    pub generated_at_ms: u64,
    /// Credential store the accounts were listed from.
    pub store: String,
    pub accounts: Vec<AccountState>,
    /// Last automatic selection per provider and model, newest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub selections: Vec<SelectionState>,
    /// `provider:label` of every account whose state `clear` dropped.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cleared: Vec<String>,
}

/// How limits follow an account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityKind {
    /// The slot records the provider account id.
    Stable,
    /// No account id recorded: the seat identity derives from refresh
    /// material and changes when the token rotates (`synaps auth identify`
    /// records the id).
    Rotating,
}

impl IdentityKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Rotating => "rotating",
        }
    }
}

/// One stored account and what the broker remembers about it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountState {
    pub provider: String,
    pub label: String,
    pub identity: IdentityKind,
    /// Access-token expiry, epoch ms; `None` when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_expires_ms: Option<u64>,
    /// An active cooldown (a reported provider limit).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown: Option<CooldownState>,
    /// The last usage read of this account (cache hits are not reads).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_usage: Option<ObservedOutcome>,
    /// When a usage read of this account last succeeded, epoch ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_usage_ok_ms: Option<u64>,
    /// The last token refresh exchange for this account in this process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_refresh: Option<ObservedOutcome>,
}

/// A reported provider limit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CooldownState {
    pub until_ms: u64,
    /// The reporter's reason kind (e.g. `anthropic_quota_exhausted`).
    pub reason: String,
}

/// One observation: when it happened and, if it failed, why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedOutcome {
    pub at_ms: u64,
    /// `None`: it succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<ObservationFailure>,
}

/// The last automatic selection for one provider and model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionState {
    pub provider: String,
    /// `None`: any model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub at_ms: u64,
    /// The chosen account (also set when its token could not be vended).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chosen: Option<String>,
    /// Why no account was chosen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<SelectionFailure>,
    /// Why the chosen account's token could not be vended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vend_failure: Option<ObservationFailure>,
}

/// Keep only characters that are safe on one terminal line (a reason comes
/// from a client report, which may be another machine).
pub(crate) fn display_safe(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "_-.:/ ".contains(c) {
                c
            } else {
                '?'
            }
        })
        .collect()
}

fn span(ms: u64) -> String {
    let secs = ms / 1000;
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
        s => format!("{}d{:02}h", s / 86_400, (s % 86_400) / 3600),
    }
}

/// `12s ago` (or `in 3s` for a clock that moved backwards).
fn ago(now_ms: u64, at_ms: u64) -> String {
    if at_ms <= now_ms {
        format!("{} ago", span(now_ms - at_ms))
    } else {
        format!("in {}", span(at_ms - now_ms))
    }
}

fn utc(ms: u64) -> String {
    chrono::DateTime::from_timestamp_millis(ms as i64)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_else(|| ms.to_string())
}

fn outcome(o: &ObservedOutcome, now_ms: u64) -> String {
    match &o.failure {
        None => format!("ok, {}", ago(now_ms, o.at_ms)),
        Some(f) => format!("{f}, {}", ago(now_ms, o.at_ms)),
    }
}

/// Plain-text rendering, one block per account, then the last selections.
pub fn render_text(report: &AuthStateReport) -> String {
    use std::fmt::Write;
    let now = report.generated_at_ms;
    let mut out = String::new();
    let source = if report.live {
        "live: the running daemon's broker"
    } else {
        "NOT LIVE: built by this command, which has served no request"
    };
    let _ = writeln!(out, "auth state ({source})");
    let _ = writeln!(out, "store: {}", report.store);
    if !report.broker_active {
        let _ = writeln!(
            out,
            "no automatic selection or limit report has reached this broker yet; nothing recorded"
        );
    }
    if !report.cleared.is_empty() {
        let _ = writeln!(
            out,
            "cleared (in memory only; credentials untouched): {}",
            report.cleared.join(", ")
        );
    }
    if report.accounts.is_empty() {
        let _ = writeln!(out, "no stored accounts");
    }
    let mut provider = "";
    for account in &report.accounts {
        if account.provider != provider {
            provider = &account.provider;
            let _ = writeln!(out, "{provider}");
        }
        let expiry = match account.access_expires_ms {
            Some(t) if t > now => format!("token expires in {}", span(t - now)),
            Some(_) => "token expired (refreshed on next use)".to_string(),
            None => "token expiry unknown".to_string(),
        };
        let _ = writeln!(
            out,
            "  {}  identity {}  {expiry}",
            account.label,
            account.identity.as_str()
        );
        match &account.cooldown {
            Some(c) => {
                let _ = writeln!(
                    out,
                    "    cooldown  {} left (until {}): {}",
                    span(c.until_ms.saturating_sub(now)),
                    utc(c.until_ms),
                    display_safe(&c.reason)
                );
            }
            None => {
                let _ = writeln!(out, "    cooldown  none");
            }
        }
        match &account.last_usage {
            Some(u) => {
                let last_ok = match (u.failure.is_some(), account.last_usage_ok_ms) {
                    (true, Some(ok)) => format!(" (last ok {})", ago(now, ok)),
                    (true, None) => " (no successful read yet)".to_string(),
                    _ => String::new(),
                };
                let _ = writeln!(out, "    usage     {}{last_ok}", outcome(u, now));
            }
            None => {
                let _ = writeln!(out, "    usage     not read yet");
            }
        }
        match &account.last_refresh {
            Some(r) => {
                let _ = writeln!(out, "    refresh   {}", outcome(r, now));
            }
            None => {
                let _ = writeln!(out, "    refresh   none in this process");
            }
        }
    }
    if !report.selections.is_empty() {
        let _ = writeln!(out, "last automatic selections");
        for s in &report.selections {
            let model = s.model.as_deref().unwrap_or("any model");
            let what = match (&s.chosen, &s.failure, &s.vend_failure) {
                (Some(label), _, Some(f)) => format!("chose {label}; token not vended: {f}"),
                (Some(label), _, None) => format!("chose {label}"),
                (None, Some(f), _) => {
                    let accounts = f.accounts_field();
                    if accounts.is_empty() {
                        format!("failed: {}", f.kind)
                    } else {
                        format!("failed: {} ({accounts})", f.kind)
                    }
                }
                (None, None, _) => "failed".to_string(),
            };
            let _ = writeln!(
                out,
                "  {} {model}: {what}, {}",
                s.provider,
                ago(now, s.at_ms)
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::auth::quota_policy::{
        AccountFailure, AccountFailureKind, SelectionFailureKind,
    };

    const NOW: u64 = 1_800_000_000_000;

    fn report() -> AuthStateReport {
        AuthStateReport {
            schema_version: AUTH_STATE_SCHEMA_VERSION,
            live: true,
            broker_active: true,
            generated_at_ms: NOW,
            store: "/scratch/auth.json".into(),
            accounts: vec![
                AccountState {
                    provider: "anthropic".into(),
                    label: "claude5".into(),
                    identity: IdentityKind::Stable,
                    access_expires_ms: Some(NOW + 3 * 3600 * 1000),
                    cooldown: Some(CooldownState {
                        until_ms: NOW + 41 * 60 * 1000,
                        reason: "anthropic_quota_exhausted\u{1b}[2J".into(),
                    }),
                    last_usage: Some(ObservedOutcome {
                        at_ms: NOW - 12_000,
                        failure: Some(
                            ObservationFailure::new(AccountFailureKind::UsageRateLimited)
                                .with_status(429)
                                .with_retry_after(Some(120)),
                        ),
                    }),
                    last_usage_ok_ms: Some(NOW - 6 * 60 * 1000),
                    last_refresh: Some(ObservedOutcome {
                        at_ms: NOW - (2 * 3600 + 3 * 60) * 1000,
                        failure: None,
                    }),
                },
                AccountState {
                    provider: "anthropic".into(),
                    label: "claude6".into(),
                    identity: IdentityKind::Rotating,
                    access_expires_ms: None,
                    cooldown: None,
                    last_usage: None,
                    last_usage_ok_ms: None,
                    last_refresh: None,
                },
            ],
            selections: vec![SelectionState {
                provider: "anthropic".into(),
                model: Some("claude-opus-4-1".into()),
                at_ms: NOW - 3_000,
                chosen: None,
                failure: Some(SelectionFailure {
                    kind: SelectionFailureKind::UsageReadFailed,
                    accounts: vec![AccountFailure {
                        account: "claude5".into(),
                        kind: AccountFailureKind::UsageRateLimited,
                        http_status: Some(429),
                        retry_after_s: Some(120),
                        until_ms: None,
                    }],
                    earliest_reset_ms: None,
                }),
                vend_failure: None,
            }],
            cleared: vec![],
        }
    }

    #[test]
    fn text_names_each_kind_status_and_age() {
        let text = render_text(&report());
        for needle in [
            "auth state (live: the running daemon's broker)",
            "anthropic\n  claude5  identity stable  token expires in 3h00m",
            "cooldown  41m left (until ",
            "usage     usage_rate_limited, HTTP 429, retry after 120s, 12s ago (last ok 6m ago)",
            "refresh   ok, 2h03m ago",
            "claude6  identity rotating  token expiry unknown",
            "usage     not read yet",
            "anthropic claude-opus-4-1: failed: usage_read_failed (claude5:usage_rate_limited:429), 3s ago",
        ] {
            assert!(text.contains(needle), "{needle:?} missing from:\n{text}");
        }
        assert!(
            !text.contains('\u{1b}'),
            "terminal control from a reason:\n{text}"
        );
    }

    #[test]
    fn a_report_not_from_the_daemon_says_so() {
        let mut r = report();
        r.live = false;
        r.broker_active = false;
        let text = render_text(&r);
        assert!(text.starts_with("auth state (NOT LIVE"), "{text}");
        assert!(text.contains("nothing recorded"), "{text}");
    }

    #[test]
    fn json_round_trips_with_its_schema_version() {
        let r = report();
        let value = serde_json::to_value(&r).unwrap();
        assert_eq!(value["schema_version"], AUTH_STATE_SCHEMA_VERSION);
        assert_eq!(value["accounts"][0]["identity"], "stable");
        assert_eq!(
            value["accounts"][0]["last_usage"]["failure"],
            serde_json::json!({"kind": "usage_rate_limited", "http_status": 429, "retry_after_s": 120})
        );
        let back: AuthStateReport = serde_json::from_value(value).unwrap();
        assert_eq!(back, r);
    }
}
