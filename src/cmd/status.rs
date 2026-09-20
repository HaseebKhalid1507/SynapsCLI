//! `synaps status` — account usage and reset times for every connected OAuth
//! account (Claude, ChatGPT/Codex, Kimi Code, Grok), plus the unrelated
//! `synaps status --memory` process report at the bottom of this file.
//!
//! Usage path (goal plan G4):
//!
//! - Snapshots come from the credential broker's typed `usage(&CredentialRef)`
//!   operation. This process never receives an access token: the broker
//!   resolves the credential behind its boundary (local in-process, or the
//!   remote `synaps auth-broker` over machine-authenticated HTTP) and returns
//!   the normalized, secret-free [`UsageSnapshot`].
//! - Selection is explicit. `--account` requires `--provider`; an explicit
//!   account that does not exist is an error for that row — never a fallback
//!   to another slot. With no flags the legacy behaviour is kept: the
//!   policy-selected Claude account.
//! - `--all` enumerates every stored account of every usage-capable provider.
//!   Per-account failures are reported inline; they never discard the accounts
//!   that succeeded. The process exits non-zero only when *every* requested
//!   account failed (or the selection itself was invalid).
//! - `--json` prints a [`UsageReport`] (same schema the keeper consumes).
//!
//! The `UsageBackend` seam exists so the selection/aggregation/rendering logic
//! is unit-tested with a fake — no network, no `auth.json`.

use std::sync::Arc;

use async_trait::async_trait;
use synaps_cli::auth::provider::parse_cli_provider;
use synaps_cli::auth::usage::{
    supported_usage_providers, supports_usage, AccountUsageEntry, AccountUsageOutcome,
    Availability, UsageErrorSummary, UsageReport, UsageSnapshot, UsageWindow, UsedPercent,
    WindowScope,
};
use synaps_cli::auth::{
    Account, AccountSelector, BrokerError, CredentialBroker, CredentialRef, OAuthProviderId,
};

/// Flags for `synaps status` (usage mode). `Default` = legacy behaviour.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StatusOptions {
    /// Canonical OAuth provider id or CLI alias (`claude`, `codex`, `kimi-code`, `grok`).
    pub provider: Option<String>,
    /// Account label (or `default`). Requires `provider`.
    pub account: Option<String>,
    /// Every stored account of every usage-capable provider (or of `provider`).
    pub all: bool,
    pub json: bool,
}

// ── Backend seam ─────────────────────────────────────────────────────────────

/// Everything the status command needs from the credential layer. The
/// production implementation wraps the credential broker; tests use a fake.
#[async_trait]
trait UsageBackend: Send + Sync {
    /// `local` or `remote`.
    fn source_label(&self) -> &str;
    /// Labels of the stored accounts for `provider` (non-secret listing).
    async fn list_accounts(&self, provider: OAuthProviderId) -> Result<Vec<String>, String>;
    /// The account the active policy selects when none is given explicitly.
    fn resolve_default(&self, provider: OAuthProviderId) -> Result<Account, String>;
    /// Typed read-only usage for exactly this credential.
    async fn fetch(&self, cred: &CredentialRef) -> Result<UsageSnapshot, UsageErrorSummary>;
}

struct BrokerBackend {
    broker: Arc<dyn CredentialBroker>,
    source: &'static str,
}

impl BrokerBackend {
    fn from_config() -> Self {
        let config = synaps_cli::config::load_config();
        let source = config.auth.credential_source();
        let label = if source.is_remote() { "remote" } else { "local" };
        let cache = synaps_cli::auth::TokenCache::new();
        let http = reqwest::Client::new();
        let broker = synaps_cli::auth::broker_from_source(&source, &cache, http);
        Self {
            broker,
            source: label,
        }
    }
}

fn summary_from_broker_error(e: &BrokerError) -> UsageErrorSummary {
    // Wildcard kept deliberately so a new broker variant degrades to a
    // generic kind instead of breaking the status command's build.
    #[allow(unreachable_patterns)]
    let kind = match e {
        BrokerError::UnknownProvider(_) => "unknown_provider",
        BrokerError::RegistrationRequired { .. } => "registration_required",
        BrokerError::NotConfigured(_) => "not_configured",
        BrokerError::Unauthorized => "broker_unauthorized",
        BrokerError::Denied(_) => "denied",
        BrokerError::UnsupportedCapability { .. } => "unsupported",
        BrokerError::Transport(_) => "transport",
        BrokerError::Credential(_) => "credential",
        BrokerError::UnknownAccount { .. } => "unknown_account",
        BrokerError::UnsupportedAccount { .. } => "unsupported_account",
        BrokerError::InvalidAccount(_) => "invalid_account",
        BrokerError::NoAccountAvailable { .. } => "no_account_available",
        _ => "broker_error",
    };
    UsageErrorSummary::other(kind, e.to_string())
}

#[async_trait]
impl UsageBackend for BrokerBackend {
    fn source_label(&self) -> &str {
        self.source
    }

    async fn list_accounts(&self, provider: OAuthProviderId) -> Result<Vec<String>, String> {
        self.broker
            .accounts(provider)
            .await
            .map(|rows| rows.into_iter().map(|r| r.label).collect())
            .map_err(|e| e.to_string())
    }

    fn resolve_default(&self, provider: OAuthProviderId) -> Result<Account, String> {
        match self.broker.account_selector(provider) {
            AccountSelector::Account(account) => Ok(account),
            AccountSelector::Auto => Err(format!(
                "{provider} is set to automatic account selection; pass --account <label> or --all"
            )),
            other => Err(format!("invalid account selector for {provider}: {other:?}")),
        }
    }

    async fn fetch(&self, cred: &CredentialRef) -> Result<UsageSnapshot, UsageErrorSummary> {
        self.broker
            .usage(cred)
            .await
            .map_err(|e| summary_from_broker_error(&e))
    }
}

// ── Selection ────────────────────────────────────────────────────────────────

/// CLI provider parsing for status: the shared alias table plus the obvious
/// usage-only names. Providers without a usage adapter are rejected honestly.
fn parse_status_provider(raw: &str) -> Result<OAuthProviderId, String> {
    let id = match raw.trim().to_ascii_lowercase().as_str() {
        "codex" | "chatgpt" => OAuthProviderId::OpenAiCodex,
        "grok" | "grok-build" => OAuthProviderId::Xai,
        other => parse_cli_provider(other)?,
    };
    if !supports_usage(id) {
        return Err(format!(
            "no usage adapter for {id}; supported: {}",
            supported_usage_providers()
                .iter()
                .map(|p| p.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(id)
}

/// A selection failure that is scoped to one provider's listing (reported as
/// a row with account `*`) rather than to the whole command.
#[derive(Debug)]
struct ListingFailure {
    provider: OAuthProviderId,
    message: String,
}

struct Selection {
    targets: Vec<CredentialRef>,
    listing_failures: Vec<ListingFailure>,
}

/// Manual impl: `CredentialRef` has no `Debug` derive; render its storage key.
impl std::fmt::Debug for Selection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Selection")
            .field(
                "targets",
                &self
                    .targets
                    .iter()
                    .map(|c| c.storage_key())
                    .collect::<Vec<_>>(),
            )
            .field("listing_failures", &self.listing_failures)
            .finish()
    }
}

async fn select_targets(
    backend: &dyn UsageBackend,
    opts: &StatusOptions,
) -> Result<Selection, String> {
    let provider = opts
        .provider
        .as_deref()
        .map(parse_status_provider)
        .transpose()?;

    if opts.account.is_some() && provider.is_none() {
        return Err("--account requires --provider".into());
    }
    if opts.all && opts.account.is_some() {
        return Err("--all cannot be combined with --account".into());
    }

    let mut selection = Selection {
        targets: Vec::new(),
        listing_failures: Vec::new(),
    };

    if opts.all {
        let providers: Vec<OAuthProviderId> = match provider {
            Some(p) => vec![p],
            None => supported_usage_providers().to_vec(),
        };
        for p in providers {
            match backend.list_accounts(p).await {
                Ok(labels) => {
                    for label in labels {
                        match Account::parse(&label) {
                            Ok(account) => selection.targets.push(CredentialRef::new(p, account)),
                            Err(reason) => selection.listing_failures.push(ListingFailure {
                                provider: p,
                                message: format!("stored account label rejected: {reason}"),
                            }),
                        }
                    }
                }
                Err(message) => selection.listing_failures.push(ListingFailure {
                    provider: p,
                    message,
                }),
            }
        }
        return Ok(selection);
    }

    let provider = provider.unwrap_or(OAuthProviderId::Anthropic);
    let account = match opts.account.as_deref() {
        // Explicit: validated, never coerced to another slot.
        Some(label) => Account::parse(label).map_err(|e| format!("invalid --account: {e}"))?,
        None => backend.resolve_default(provider)?,
    };
    selection.targets.push(CredentialRef::new(provider, account));
    Ok(selection)
}

// ── Aggregation ──────────────────────────────────────────────────────────────

async fn build_report(backend: &dyn UsageBackend, selection: Selection) -> UsageReport {
    let mut report = UsageReport::new(backend.source_label(), synaps_cli::epoch_millis());
    for failure in selection.listing_failures {
        report.accounts.push(AccountUsageEntry {
            provider: failure.provider.as_str().to_string(),
            account: "*".into(),
            outcome: AccountUsageOutcome::Error {
                error: UsageErrorSummary::other("account_listing", failure.message),
            },
        });
    }
    for cred in selection.targets {
        let outcome = match backend.fetch(&cred).await {
            Ok(snapshot) => AccountUsageOutcome::ok(snapshot),
            Err(error) => AccountUsageOutcome::Error { error },
        };
        report.accounts.push(AccountUsageEntry {
            provider: cred.provider.as_str().to_string(),
            account: cred.account.label_str().to_string(),
            outcome,
        });
    }
    report
}

// ── Rendering ────────────────────────────────────────────────────────────────

fn relative_time(now_ms: u64, at_ms: u64) -> String {
    if at_ms <= now_ms {
        return "reset passed".into();
    }
    let secs = (at_ms - now_ms) / 1000;
    let (d, h, m) = (secs / 86_400, (secs % 86_400) / 3_600, (secs % 3_600) / 60);
    if d > 0 {
        format!("resets in {d}d {h}h")
    } else if h > 0 {
        format!("resets in {h}h {m}m")
    } else {
        format!("resets in {m}m")
    }
}

fn absolute_time(at_ms: u64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(at_ms as i64)
        .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| "?".into())
}

fn bar(percent: Option<f64>) -> String {
    const WIDTH: usize = 30;
    match percent {
        Some(p) => {
            let filled = ((p / 100.0) * WIDTH as f64).round().clamp(0.0, WIDTH as f64) as usize;
            format!("{}{}", "█".repeat(filled), "░".repeat(WIDTH - filled))
        }
        None => "?".repeat(WIDTH),
    }
}

fn window_title(w: &UsageWindow) -> String {
    let scope = match &w.scope {
        WindowScope::Account => String::new(),
        WindowScope::Model { model } => format!(" · model {model}"),
        WindowScope::Feature { feature } => format!(" · {feature}"),
    };
    let label = if w.label == w.id || w.label == "unknown" {
        w.id.clone()
    } else {
        format!("{} ({})", w.id, w.label)
    };
    format!("{label}{scope}")
}

fn render_window(out: &mut String, w: &UsageWindow, now_ms: u64) {
    let percent = w.used_percent.valid();
    let percent_text = match &w.used_percent {
        UsedPercent::Valid { percent } => format!("{percent:.0}%"),
        UsedPercent::Unknown { reason } => format!("unknown ({reason})"),
    };
    let reset = match w.reset_at {
        Some(at) => format!("{} · {}", relative_time(now_ms, at), absolute_time(at)),
        None => "reset unknown".into(),
    };
    let flag = if w.is_exhausted() { "  EXHAUSTED" } else { "" };
    out.push_str(&format!("    {}\n", window_title(w)));
    out.push_str(&format!("    {} {}{}\n", bar(percent), percent_text, flag));
    out.push_str(&format!("    {reset}\n"));
}

fn render_snapshot(out: &mut String, s: &UsageSnapshot, now_ms: u64) {
    if let Some(plan) = &s.plan {
        out.push_str(&format!("    plan: {plan}\n"));
    }
    if s.limit_reached == Some(true) {
        out.push_str("    ⚠ provider reports the account limit is reached\n");
    }
    if s.spend_control_reached == Some(true) {
        out.push_str("    ⚠ provider reports the spend control is reached\n");
    }
    if s.windows.is_empty() {
        out.push_str("    no usage windows reported\n");
    }
    for w in &s.windows {
        render_window(out, w, now_ms);
    }
    for m in &s.model_availability {
        let state = match m.availability {
            Availability::Available => "available".to_string(),
            Availability::Exhausted => match m.reset_at {
                Some(at) => format!("unavailable until {}", absolute_time(at)),
                None => "unavailable".into(),
            },
            Availability::Unknown => "availability unknown".into(),
        };
        let credits = if m.credits_would_enable == Some(true) {
            " (purchased credits would enable; not automatic)"
        } else {
            ""
        };
        out.push_str(&format!("    model {}: {state}{credits}\n", m.model));
    }
    if let Some(c) = &s.credits {
        let mut parts = Vec::new();
        if c.unlimited == Some(true) {
            parts.push("unlimited".to_string());
        }
        if let Some(r) = c.remaining {
            parts.push(format!("{r:.2} remaining"));
        }
        if let Some(t) = c.total {
            parts.push(format!("of {t:.2}"));
        }
        if let Some(u) = &c.unit {
            parts.push(u.clone());
        }
        if let Some(at) = c.renews_at {
            parts.push(format!("renews {}", absolute_time(at)));
        }
        if !parts.is_empty() {
            out.push_str(&format!("    credits: {}\n", parts.join(" ")));
        }
    }
    if let Some(b) = &s.banked_resets {
        let count = b
            .available_count
            .map(|n| n.to_string())
            .unwrap_or_else(|| "unknown".into());
        out.push_str(&format!("    banked resets available: {count}\n"));
        for c in &b.credits {
            let kind = c.reset_type.as_deref().unwrap_or("reset");
            let exp = c
                .expires_at
                .map(|at| format!(" expires {}", absolute_time(at)))
                .unwrap_or_default();
            out.push_str(&format!("      · {kind}{exp}\n"));
        }
        if let Some(e) = &b.inventory_error {
            out.push_str(&format!("      inventory unavailable ({e})\n"));
        }
    }
    for note in &s.notes {
        out.push_str(&format!("    note: {note}\n"));
    }
}

fn render_text(report: &UsageReport, now_ms: u64) -> String {
    let mut out = String::new();
    out.push_str(&format!("\n  ⚡ Account Usage ({})\n\n", report.source));
    if report.accounts.is_empty() {
        out.push_str("  No connected accounts for the requested providers.\n");
        out.push_str("  Run `synaps login --provider <id> [--account <label>]` to connect one.\n");
        return out;
    }
    for entry in &report.accounts {
        out.push_str(&format!("  {} / {}\n", entry.provider, entry.account));
        match &entry.outcome {
            AccountUsageOutcome::Ok { snapshot } => render_snapshot(&mut out, snapshot, now_ms),
            AccountUsageOutcome::Error { error } => {
                let status = error
                    .http_status
                    .map(|s| format!(" [HTTP {s}]"))
                    .unwrap_or_default();
                out.push_str(&format!("    ✗ {}{status}: {}\n", error.kind, error.message));
                if error.kind == "unauthorized" || error.kind == "credential" {
                    out.push_str(&format!(
                        "      run `synaps login --provider {} --account {}` to re-authenticate\n",
                        entry.provider, entry.account
                    ));
                }
            }
        }
        out.push('\n');
    }
    out
}

// ── Entry points ─────────────────────────────────────────────────────────────

async fn run_with_backend(
    backend: &dyn UsageBackend,
    opts: &StatusOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    let selection = select_targets(backend, opts).await?;
    let report = build_report(backend, selection).await;
    if opts.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", render_text(&report, synaps_cli::epoch_millis()));
    }
    if !report.accounts.is_empty() && report.ok_count() == 0 {
        return Err(format!(
            "usage unavailable for all {} requested account(s)",
            report.accounts.len()
        )
        .into());
    }
    Ok(())
}

/// `synaps status [--provider <id>] [--account <label>] [--all] [--json]`.
///
/// `StatusOptions::default()` is the legacy `synaps status`: the
/// policy-selected Claude account in the text layout.
pub async fn run_usage(opts: StatusOptions) -> Result<(), Box<dyn std::error::Error>> {
    let backend = BrokerBackend::from_config();
    run_with_backend(&backend, &opts).await
}

#[cfg(test)]
mod usage_tests {
    use super::*;
    use std::collections::BTreeMap;
    use synaps_cli::auth::usage::{parse_anthropic_usage, parse_codex_usage};

    const T0: u64 = 1_758_400_000_000;

    struct FakeBackend {
        accounts: BTreeMap<OAuthProviderId, Result<Vec<String>, String>>,
        defaults: BTreeMap<OAuthProviderId, Result<Account, String>>,
        results: BTreeMap<String, Result<UsageSnapshot, UsageErrorSummary>>,
    }

    impl FakeBackend {
        fn new() -> Self {
            Self {
                accounts: BTreeMap::new(),
                defaults: BTreeMap::new(),
                results: BTreeMap::new(),
            }
        }
        fn with_accounts(mut self, p: OAuthProviderId, labels: &[&str]) -> Self {
            self.accounts
                .insert(p, Ok(labels.iter().map(|s| s.to_string()).collect()));
            self
        }
        fn with_listing_error(mut self, p: OAuthProviderId, msg: &str) -> Self {
            self.accounts.insert(p, Err(msg.into()));
            self
        }
        fn with_default(mut self, p: OAuthProviderId, account: Result<Account, String>) -> Self {
            self.defaults.insert(p, account);
            self
        }
        fn with_result(
            mut self,
            key: &str,
            result: Result<UsageSnapshot, UsageErrorSummary>,
        ) -> Self {
            self.results.insert(key.into(), result);
            self
        }
    }

    #[async_trait]
    impl UsageBackend for FakeBackend {
        fn source_label(&self) -> &str {
            "local"
        }
        async fn list_accounts(&self, provider: OAuthProviderId) -> Result<Vec<String>, String> {
            self.accounts
                .get(&provider)
                .cloned()
                .unwrap_or_else(|| Ok(Vec::new()))
        }
        fn resolve_default(&self, provider: OAuthProviderId) -> Result<Account, String> {
            self.defaults
                .get(&provider)
                .cloned()
                .unwrap_or(Ok(Account::Default))
        }
        async fn fetch(&self, cred: &CredentialRef) -> Result<UsageSnapshot, UsageErrorSummary> {
            self.results
                .get(&cred.storage_key())
                .cloned()
                .unwrap_or_else(|| {
                    Err(UsageErrorSummary::other(
                        "unknown_account",
                        format!("no such account {}", cred.storage_key()),
                    ))
                })
        }
    }

    fn anthropic_snapshot(label: &str) -> UsageSnapshot {
        parse_anthropic_usage(
            r#"{"five_hour": {"utilization": 12.5, "resets_at": "2025-09-20T22:00:00Z"},
                "seven_day": {"utilization": null, "resets_at": null},
                "seven_day_sonnet": {"utilization": 100, "resets_at": "2025-09-24T00:00:00Z"}}"#,
            label,
            T0,
        )
        .unwrap()
    }

    fn codex_snapshot(label: &str) -> UsageSnapshot {
        parse_codex_usage(
            r#"{"plan_type": "pro",
                "rate_limit": {"limit_reached": false,
                    "primary_window": {"used_percent": 37, "limit_window_seconds": 604800, "reset_at": 1758500000},
                    "secondary_window": null},
                "model_usage": {"gpt-6-astra": {"available": false, "available_at": "2026-01-05T09:00:00Z", "credits_would_enable": true}},
                "rate_limit_reset_credits": {"available_count": 2},
                "credits": {"has_credits": true, "unlimited": false, "balance": "12.5"}}"#,
            label,
            T0,
            None,
        )
        .unwrap()
    }

    fn opts(provider: Option<&str>, account: Option<&str>, all: bool, json: bool) -> StatusOptions {
        StatusOptions {
            provider: provider.map(str::to_string),
            account: account.map(str::to_string),
            all,
            json,
        }
    }

    // ── provider parsing ─────────────────────────────────────────────

    #[test]
    fn provider_aliases_and_rejections() {
        assert_eq!(parse_status_provider("claude").unwrap(), OAuthProviderId::Anthropic);
        assert_eq!(parse_status_provider("Anthropic").unwrap(), OAuthProviderId::Anthropic);
        assert_eq!(parse_status_provider("codex").unwrap(), OAuthProviderId::OpenAiCodex);
        assert_eq!(parse_status_provider("openai-codex").unwrap(), OAuthProviderId::OpenAiCodex);
        assert_eq!(parse_status_provider("kimi-code").unwrap(), OAuthProviderId::KimiCode);
        assert_eq!(parse_status_provider("grok").unwrap(), OAuthProviderId::Xai);
        assert_eq!(parse_status_provider("xai-auth").unwrap(), OAuthProviderId::Xai);
        // Static-key collisions stay rejected like `synaps login`.
        assert!(parse_status_provider("kimi").is_err());
        assert!(parse_status_provider("nonsense").is_err());
        // Providers without a usage adapter are rejected honestly.
        let err = parse_status_provider("copilot").unwrap_err();
        assert!(err.contains("no usage adapter"), "{err}");
    }

    // ── selection ────────────────────────────────────────────────────

    #[tokio::test]
    async fn no_flags_selects_policy_default_anthropic() {
        let backend = FakeBackend::new().with_default(
            OAuthProviderId::Anthropic,
            Ok(Account::parse("work").unwrap()),
        );
        let sel = select_targets(&backend, &StatusOptions::default()).await.unwrap();
        assert_eq!(sel.targets.len(), 1);
        assert_eq!(sel.targets[0].provider, OAuthProviderId::Anthropic);
        assert_eq!(sel.targets[0].account.label_str(), "work");
        assert!(sel.listing_failures.is_empty());
    }

    #[tokio::test]
    async fn explicit_account_is_validated_and_never_coerced() {
        let backend = FakeBackend::new();
        let sel = select_targets(&backend, &opts(Some("codex"), Some("astra2"), false, false))
            .await
            .unwrap();
        assert_eq!(sel.targets[0].storage_key(), "openai-codex@astra2");
        let sel = select_targets(&backend, &opts(Some("codex"), Some("default"), false, false))
            .await
            .unwrap();
        assert_eq!(sel.targets[0].storage_key(), "openai-codex");
        let err = select_targets(&backend, &opts(Some("codex"), Some("Bad Label!"), false, false))
            .await
            .unwrap_err();
        assert!(err.starts_with("invalid --account"), "{err}");
        let err = select_targets(&backend, &opts(None, Some("astra2"), false, false))
            .await
            .unwrap_err();
        assert_eq!(err, "--account requires --provider");
        let err = select_targets(&backend, &opts(Some("codex"), Some("astra2"), true, false))
            .await
            .unwrap_err();
        assert_eq!(err, "--all cannot be combined with --account");
    }

    #[tokio::test]
    async fn auto_policy_without_explicit_account_is_an_error_not_a_guess() {
        let backend = FakeBackend::new().with_default(
            OAuthProviderId::OpenAiCodex,
            Err("openai-codex is set to automatic account selection; pass --account".into()),
        );
        let err = select_targets(&backend, &opts(Some("codex"), None, false, false))
            .await
            .unwrap_err();
        assert!(err.contains("automatic"), "{err}");
    }

    #[tokio::test]
    async fn all_enumerates_every_provider_and_isolates_listing_failures() {
        let backend = FakeBackend::new()
            .with_accounts(OAuthProviderId::Anthropic, &["default", "work"])
            .with_accounts(OAuthProviderId::OpenAiCodex, &["default", "astra2", "BAD LABEL"])
            .with_listing_error(OAuthProviderId::KimiCode, "auth.json unreadable");
        let sel = select_targets(&backend, &opts(None, None, true, false)).await.unwrap();
        let keys: Vec<String> = sel.targets.iter().map(|c| c.storage_key()).collect();
        assert_eq!(
            keys,
            vec!["anthropic", "anthropic@work", "openai-codex", "openai-codex@astra2"]
        );
        assert_eq!(sel.listing_failures.len(), 2);
        assert!(sel
            .listing_failures
            .iter()
            .any(|f| f.provider == OAuthProviderId::OpenAiCodex && f.message.contains("label rejected")));
        assert!(sel
            .listing_failures
            .iter()
            .any(|f| f.provider == OAuthProviderId::KimiCode && f.message.contains("unreadable")));
        // --all --provider restricts to one provider.
        let sel = select_targets(&backend, &opts(Some("claude"), None, true, false))
            .await
            .unwrap();
        assert_eq!(sel.targets.len(), 2);
        assert!(sel.targets.iter().all(|c| c.provider == OAuthProviderId::Anthropic));
        assert!(sel.listing_failures.is_empty());
    }

    // ── aggregation + rendering ─────────────────────────────────────

    #[tokio::test]
    async fn report_keeps_successes_alongside_partial_errors() {
        let backend = FakeBackend::new()
            .with_accounts(OAuthProviderId::Anthropic, &["default"])
            .with_accounts(OAuthProviderId::OpenAiCodex, &["default", "astra2"])
            .with_listing_error(OAuthProviderId::Xai, "broker returned HTTP 503")
            .with_result("anthropic", Ok(anthropic_snapshot("default")))
            .with_result("openai-codex", Ok(codex_snapshot("default")))
            .with_result(
                "openai-codex@astra2",
                Err(UsageErrorSummary {
                    kind: "unauthorized".into(),
                    message: "provider rejected the access token (HTTP 401)".into(),
                    http_status: Some(401),
                }),
            );
        let sel = select_targets(&backend, &opts(None, None, true, true)).await.unwrap();
        let report = build_report(&backend, sel).await;
        assert_eq!(report.source, "local");
        assert_eq!(report.accounts.len(), 4);
        assert_eq!(report.ok_count(), 2);
        assert_eq!(report.error_count(), 2);

        let v = serde_json::to_value(&report).unwrap();
        assert_eq!(v["schema_version"], 1);
        let rows = v["accounts"].as_array().unwrap();
        // Listing failure row first, tagged with account "*".
        assert_eq!(rows[0]["provider"], "xai-auth");
        assert_eq!(rows[0]["account"], "*");
        assert_eq!(rows[0]["status"], "error");
        assert_eq!(rows[0]["error"]["kind"], "account_listing");
        assert_eq!(rows[1]["provider"], "anthropic");
        assert_eq!(rows[1]["status"], "ok");
        assert_eq!(rows[1]["snapshot"]["windows"][0]["used_percent"]["state"], "valid");
        assert_eq!(rows[2]["snapshot"]["model_availability"][0]["model"], "gpt-6-astra");
        assert_eq!(rows[2]["snapshot"]["model_availability"][0]["availability"], "exhausted");
        assert_eq!(rows[3]["account"], "astra2");
        assert_eq!(rows[3]["error"]["http_status"], 401);
        // Round-trips through the shared schema.
        let back: UsageReport = serde_json::from_value(v).unwrap();
        assert_eq!(back, report);

        let text = render_text(&report, T0);
        assert!(text.contains("⚡ Account Usage (local)"));
        assert!(text.contains("anthropic / default"));
        assert!(text.contains("five_hour (5h)"));
        assert!(text.contains("12%") || text.contains("13%"));
        assert!(text.contains("unknown (null)"), "unknown percent is labelled, not zeroed");
        assert!(text.contains("reset unknown"));
        assert!(text.contains("EXHAUSTED"));
        assert!(text.contains("model gpt-6-astra: unavailable until 2026-01-05 09:00 UTC (purchased credits would enable; not automatic)"));
        assert!(text.contains("banked resets available: 2"));
        assert!(text.contains("credits: 12.50 remaining credits"));
        assert!(text.contains("openai-codex / astra2"));
        assert!(text.contains("✗ unauthorized [HTTP 401]"));
        assert!(text.contains("synaps login --provider openai-codex --account astra2"));
        assert!(text.contains("xai-auth / *"));
        assert!(text.contains("account_listing"));
    }

    #[tokio::test]
    async fn run_exit_status_semantics() {
        // All failed → Err (exit 1); partial → Ok; none connected → Ok with message.
        let backend = FakeBackend::new()
            .with_accounts(OAuthProviderId::Anthropic, &["default"])
            .with_result(
                "anthropic",
                Err(UsageErrorSummary::other("transport", "usage transport error: connect")),
            );
        let err = run_with_backend(&backend, &opts(None, None, true, true))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("all 1 requested account"));

        let backend = FakeBackend::new()
            .with_accounts(OAuthProviderId::Anthropic, &["default", "work"])
            .with_result("anthropic", Ok(anthropic_snapshot("default")))
            .with_result(
                "anthropic@work",
                Err(UsageErrorSummary::other("transport", "usage transport error: connect")),
            );
        run_with_backend(&backend, &opts(None, None, true, true))
            .await
            .expect("partial failure is not fatal");

        let empty = FakeBackend::new();
        run_with_backend(&empty, &opts(None, None, true, false))
            .await
            .expect("no connected accounts is not an error");
        let report = build_report(&empty, select_targets(&empty, &opts(None, None, true, false)).await.unwrap()).await;
        assert!(render_text(&report, T0).contains("No connected accounts"));

        // Explicit missing account: error row, no fallback to default.
        let backend = FakeBackend::new().with_result("openai-codex", Ok(codex_snapshot("default")));
        let sel = select_targets(&backend, &opts(Some("codex"), Some("ghost"), false, false))
            .await
            .unwrap();
        let report = build_report(&backend, sel).await;
        assert_eq!(report.ok_count(), 0);
        assert_eq!(report.accounts[0].account, "ghost");
        match &report.accounts[0].outcome {
            AccountUsageOutcome::Error { error } => assert_eq!(error.kind, "unknown_account"),
            other => panic!("expected error row, got {other:?}"),
        }
    }

    #[test]
    fn text_helpers() {
        assert_eq!(relative_time(T0, T0 - 1), "reset passed");
        assert_eq!(relative_time(T0, T0 + 90 * 60 * 1000), "resets in 1h 30m");
        assert_eq!(relative_time(T0, T0 + 26 * 3600 * 1000), "resets in 1d 2h");
        assert_eq!(relative_time(T0, T0 + 5 * 60 * 1000), "resets in 5m");
        assert_eq!(absolute_time(T0), "2025-09-20 20:26 UTC");
        assert_eq!(bar(Some(0.0)).chars().filter(|c| *c == '█').count(), 0);
        assert_eq!(bar(Some(50.0)).chars().filter(|c| *c == '█').count(), 15);
        assert_eq!(bar(Some(100.0)).chars().filter(|c| *c == '█').count(), 30);
        assert_eq!(bar(None), "?".repeat(30));
    }

    #[test]
    fn broker_error_mapping_is_secret_free() {
        let s = summary_from_broker_error(&BrokerError::UnknownAccount {
            provider: "openai-codex".into(),
            label: "ghost".into(),
        });
        assert_eq!(s.kind, "unknown_account");
        assert!(s.message.contains("ghost"));
        let s = summary_from_broker_error(&BrokerError::Credential("refresh failed".into()));
        assert_eq!(s.kind, "credential");
        assert_eq!(s.http_status, None);
    }
}

// ═══ `synaps status --memory` (§3.7) ═══════════════════════════════════════

use synaps_cli::core::memstat::{self, MemTotals, ProcMem, ProcRole};

#[derive(serde::Serialize)]
struct SessionMem {
    session_id: Option<String>,
    name: Option<String>,
    root_pid: u32,
    procs: Vec<ProcMem>,
    totals: MemTotals,
}

#[derive(serde::Serialize)]
struct MemoryReport {
    sessions: Vec<SessionMem>,
    totals: MemTotals,
}

fn collect(pid: Option<u32>) -> Result<MemoryReport, Box<dyn std::error::Error>> {
    let mut sessions = Vec::new();
    match pid {
        Some(p) => {
            let procs = memstat::tree(p)?;
            let totals = MemTotals::of(&procs);
            sessions.push(SessionMem {
                session_id: None,
                name: None,
                root_pid: p,
                procs,
                totals,
            });
        }
        None => {
            let mut regs = synaps_cli::events::registry::list_active_sessions();
            regs.sort_by_key(|a| a.started_at);
            for reg in regs {
                let Ok(procs) = memstat::tree(reg.pid) else {
                    continue;
                };
                let totals = MemTotals::of(&procs);
                sessions.push(SessionMem {
                    session_id: Some(reg.session_id),
                    name: reg.name,
                    root_pid: reg.pid,
                    procs,
                    totals,
                });
            }
        }
    }
    let all: Vec<ProcMem> = sessions
        .iter()
        .flat_map(|s| s.procs.iter().cloned())
        .collect();
    let totals = MemTotals::of(&all);
    Ok(MemoryReport { sessions, totals })
}

fn mb(kb: u64) -> f64 {
    kb as f64 / 1024.0
}

fn role_label(role: &ProcRole) -> String {
    match role {
        ProcRole::Engine => "engine".into(),
        ProcRole::ExtensionSidecar { name } => format!("ext:{name}"),
        ProcRole::McpServer { name } => format!("mcp:{name}"),
        ProcRole::Shell => "shell".into(),
        ProcRole::Other => "other".into(),
    }
}

fn print_table(report: &MemoryReport) {
    if report.sessions.is_empty() {
        println!("No live sessions (registry empty) — pass --pid N to inspect a process tree.");
        return;
    }
    println!(
        "{:<8} {:<22} {:>8} {:>8} {:>8} {:>8} {:>4}  CMD",
        "PID", "ROLE", "RSS MB", "PSS MB", "USS MB", "ANON MB", "THR"
    );
    for s in &report.sessions {
        let label = match (&s.name, &s.session_id) {
            (Some(n), Some(id)) => format!("{n} ({id})"),
            (None, Some(id)) => id.clone(),
            _ => format!("pid {}", s.root_pid),
        };
        println!("── session {label}");
        for p in &s.procs {
            let cmd: String = p.cmd.chars().take(60).collect();
            println!(
                "{:<8} {:<22} {:>8.1} {:>8.1} {:>8.1} {:>8.1} {:>4}  {}",
                p.pid,
                role_label(&p.role),
                mb(p.rss_kb),
                mb(p.pss_kb),
                mb(p.uss_kb),
                mb(p.anon_kb),
                p.threads,
                cmd
            );
        }
        let t = &s.totals;
        println!(
            "{:<8} {:<22} {:>8.1} {:>8.1} {:>8.1} {:>8.1} {:>4}",
            "",
            format!("subtotal ({} procs)", t.procs),
            mb(t.rss_kb),
            mb(t.pss_kb),
            mb(t.uss_kb),
            mb(t.anon_kb),
            t.threads
        );
    }
    let t = &report.totals;
    println!(
        "{:<8} {:<22} {:>8.1} {:>8.1} {:>8.1} {:>8.1} {:>4}",
        "",
        format!(
            "TOTAL ({} sessions, {} procs)",
            report.sessions.len(),
            t.procs
        ),
        mb(t.rss_kb),
        mb(t.pss_kb),
        mb(t.uss_kb),
        mb(t.anon_kb),
        t.threads
    );
}

/// `synaps status --memory [--json] [--pid N]`.
pub fn run_memory(json: bool, pid: Option<u32>) -> Result<(), Box<dyn std::error::Error>> {
    let report = collect(pid)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_table(&report);
    }
    Ok(())
}
