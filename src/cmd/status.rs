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
//! - `--json` prints a [`UsageReport`].
//! - The last known usage (local broker only). An Anthropic account whose
//!   saved reading is at most as old as the broker's own reuse age for it (10 min;
//!   60 s near its limit unless that reading shows extra usage off) is shown
//!   from that reading, with its age, and no usage request is sent for it.
//!   Only an account without such a reading is read, and never while a
//!   refresh hold or the usage-read pause is active. When that read gets a
//!   usage 429, or the pause or the account's held 429 holds it back (also
//!   for an account whose saved reading is recent enough to show), the row
//!   shows the last good reading bound to the same account, its age and
//!   the table's retry time, or the plain line
//!   "usage read rate-limited (HTTP 429)" without one; in `--json` the error
//!   carries the kind `usage_rate_limited`, an optional `retry_at_ms` and an
//!   optional `last_good`, and the row still counts as an error. Status only
//!   loads the saved readings, once per run: it never writes the file. A
//!   remote broker and other transport errors keep today's text.
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
    UsageReadFailure,
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
    /// Full windows, model availability, credits and adapter diagnostics.
    pub verbose: bool,
}

// ── Backend seam ─────────────────────────────────────────────────────────────

/// Everything the status command needs from the credential layer. The
/// production implementation wraps the credential broker; tests use a fake.
#[async_trait]
trait UsageBackend: Send + Sync {
    /// `local` or `remote`.
    fn source_label(&self) -> &str;
    /// Labels of the stored accounts for `provider` (non-secret listing).
    async fn list_accounts(&self, provider: OAuthProviderId) -> Result<Vec<String>, BrokerError>;
    /// Optional display metadata; unavailable metadata must not hide usage.
    async fn identities(&self, _provider: OAuthProviderId) -> Vec<(String, String)> {
        Vec::new()
    }
    /// The account the active policy selects when none is given explicitly.
    fn resolve_default(&self, provider: OAuthProviderId) -> Result<Account, String>;
    /// Typed read-only usage for exactly this credential.
    async fn fetch(&self, cred: &CredentialRef) -> Result<UsageSnapshot, FetchFailure>;
}

/// A failed usage read as a report row shows it.
#[derive(Debug, Clone)]
struct FetchFailure {
    error: UsageErrorSummary,
    /// The account's last good reading, for a read the usage endpoint
    /// rate-limited.
    last_good: Option<UsageSnapshot>,
}

impl From<UsageErrorSummary> for FetchFailure {
    fn from(error: UsageErrorSummary) -> Self {
        Self {
            error,
            last_good: None,
        }
    }
}

struct BrokerBackend {
    broker: Arc<dyn CredentialBroker>,
    source: &'static str,
}

impl BrokerBackend {
    fn from_config() -> Self {
        let config = synaps_cli::config::load_config();
        let source = config.auth.credential_source();
        let label = if source.is_remote() {
            "remote"
        } else {
            "local"
        };
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
        BrokerError::BrokerTooOld(_) => "broker_too_old",
        _ => "broker_error",
    };
    UsageErrorSummary::other(kind, e.to_string())
}

/// Kind of a report row whose usage read the usage endpoint rate-limited.
const RATE_LIMITED_KIND: &str = "usage_rate_limited";
/// Message of such a row.
const RATE_LIMITED_TEXT: &str = "usage read rate-limited (HTTP 429)";

/// A failed [`CredentialBroker::usage_or_last_known`] read as a report row.
/// A rate-limited read is its own kind, with the broker's retry time and
/// the account's last good reading; any other failure keeps today's row.
fn fetch_failure(failure: &UsageReadFailure) -> FetchFailure {
    match &failure.rate_limited {
        Some(limited) => FetchFailure {
            error: UsageErrorSummary {
                kind: RATE_LIMITED_KIND.into(),
                message: RATE_LIMITED_TEXT.into(),
                http_status: Some(429),
                retry_at_ms: Some(limited.retry_at_ms),
            },
            last_good: limited.last_good.clone(),
        },
        None => summary_from_broker_error(&failure.error).into(),
    }
}

#[async_trait]
impl UsageBackend for BrokerBackend {
    fn source_label(&self) -> &str {
        self.source
    }

    async fn list_accounts(&self, provider: OAuthProviderId) -> Result<Vec<String>, BrokerError> {
        self.broker
            .accounts(provider)
            .await
            .map(|rows| rows.into_iter().map(|r| r.label).collect())
    }

    async fn identities(&self, provider: OAuthProviderId) -> Vec<(String, String)> {
        self.broker
            .accounts(provider)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(|row| row.identity.map(|identity| (row.label, identity)))
            .collect()
    }

    fn resolve_default(&self, provider: OAuthProviderId) -> Result<Account, String> {
        match self.broker.account_selector(provider) {
            AccountSelector::Account(account) => Ok(account),
            AccountSelector::Auto => Err(format!(
                "{provider} is set to automatic account selection; pass --account <label> or --all"
            )),
            other => Err(format!(
                "invalid account selector for {provider}: {other:?}"
            )),
        }
    }

    async fn fetch(&self, cred: &CredentialRef) -> Result<UsageSnapshot, FetchFailure> {
        self.broker
            .usage_or_last_known(cred)
            .await
            .map_err(|failure| fetch_failure(&failure))
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
                Err(e) => {
                    // A 0.10.2 broker cannot list accounts, but still serves
                    // its default Anthropic slot through the legacy `/usage`.
                    if matches!(e, BrokerError::BrokerTooOld(_)) && p == OAuthProviderId::Anthropic {
                        selection.targets.push(CredentialRef::new(p, Account::Default));
                    }
                    selection.listing_failures.push(ListingFailure {
                        provider: p,
                        message: e.to_string(),
                    })
                }
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
    selection
        .targets
        .push(CredentialRef::new(provider, account));
    Ok(selection)
}

// ── Aggregation ──────────────────────────────────────────────────────────────

/// One account's read as its report row's outcome.
fn row_outcome(read: Result<UsageSnapshot, FetchFailure>) -> AccountUsageOutcome {
    match read {
        Ok(snapshot) => AccountUsageOutcome::ok(snapshot),
        Err(failure) => AccountUsageOutcome::Error {
            error: failure.error,
            last_good: failure.last_good.map(Box::new),
        },
    }
}

async fn build_report(backend: &dyn UsageBackend, selection: Selection) -> UsageReport {
    let mut report = UsageReport::new(backend.source_label(), synaps_cli::epoch_millis());
    for failure in selection.listing_failures {
        report.accounts.push(AccountUsageEntry {
            provider: failure.provider.as_str().to_string(),
            account: "*".into(),
            identity: None,
            outcome: AccountUsageOutcome::Error {
                error: UsageErrorSummary::other("account_listing", failure.message),
                last_good: None,
            },
        });
    }
    let mut identities = std::collections::BTreeMap::new();
    for cred in selection.targets {
        let outcome = row_outcome(backend.fetch(&cred).await);
        if let std::collections::btree_map::Entry::Vacant(entry) = identities.entry(cred.provider) {
            entry.insert(backend.identities(cred.provider).await);
        }
        let identity = identities[&cred.provider]
            .iter()
            .find(|(label, _)| label == cred.account.label_str())
            .map(|(_, identity)| identity.trim().to_string())
            .filter(|identity| !identity.is_empty());
        report.accounts.push(AccountUsageEntry {
            provider: cred.provider.as_str().to_string(),
            account: cred.account.label_str().to_string(),
            identity,
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

/// How long before `now_ms` the reading at `at_ms` was taken, in its two
/// largest units (`45s`, `13m`, `2h 5m`, `1d 3h`).
fn age_text(now_ms: u64, at_ms: u64) -> String {
    let secs = now_ms.saturating_sub(at_ms) / 1000;
    let (d, h, m) = (secs / 86_400, (secs % 86_400) / 3_600, (secs % 3_600) / 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m")
    } else {
        format!("{secs}s")
    }
}

/// `, retry in Ns` while the broker's next usage request for the account
/// (`retry_at_ms`) is still ahead of `now_ms`; nothing otherwise.
fn retry_text(now_ms: u64, retry_at_ms: Option<u64>) -> String {
    match retry_at_ms {
        Some(at) if at > now_ms => format!(", retry in {}s", (at - now_ms).div_ceil(1000)),
        _ => String::new(),
    }
}

fn bar(percent: f64) -> String {
    const WIDTH: usize = 30;
    let filled = ((percent / 100.0) * WIDTH as f64)
        .round()
        .clamp(0.0, WIDTH as f64) as usize;
    format!("{}{}", "█".repeat(filled), "░".repeat(WIDTH - filled))
}

fn window_title(w: &UsageWindow) -> String {
    let scope = match &w.scope {
        WindowScope::Account => String::new(),
        WindowScope::Model { model } => format!(" · model {model}"),
        // A feature window named after its feature (`extra_usage`) has one title.
        WindowScope::Feature { feature } if *feature == w.id => String::new(),
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
    // No gauge for an unknown reading: a row of '?' adds nothing to the
    // "unknown (reason)" text beside it.
    let gauge = match w.used_percent.valid() {
        Some(p) => format!("{} ", bar(p)),
        None => String::new(),
    };
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
    out.push_str(&format!("    {gauge}{percent_text}{flag}\n"));
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

fn render_verbose(report: &UsageReport, now_ms: u64) -> String {
    let mut out = String::new();
    out.push_str(&format!("\n  ⚡ Account Usage ({})\n\n", report.source));
    if report.accounts.is_empty() {
        out.push_str("  No connected accounts for the requested providers.\n");
        out.push_str("  Run `synaps login --provider <id> [--account <label>]` to connect one.\n");
        return out;
    }
    for entry in &report.accounts {
        out.push_str(&format!("  {} / {}\n", entry.provider, entry.account));
        out.push_str(&format!(
            "    email: {}\n",
            inline_text(entry.identity.as_deref().unwrap_or("unknown"))
        ));
        match &entry.outcome {
            AccountUsageOutcome::Ok { snapshot } => {
                if snapshot.observed_at < report.observed_at {
                    out.push_str(&format!(
                        "    read {} ago\n",
                        age_text(now_ms, snapshot.observed_at)
                    ));
                }
                render_snapshot(&mut out, snapshot, now_ms)
            }
            AccountUsageOutcome::Error { error, last_good } if error.kind == RATE_LIMITED_KIND => {
                let retry = retry_text(now_ms, error.retry_at_ms);
                match last_good {
                    Some(s) => {
                        out.push_str(&format!("    ✗ usage read rate-limited{retry}\n"));
                        out.push_str(&format!(
                            "    last good reading, read {} ago\n",
                            age_text(now_ms, s.observed_at)
                        ));
                        render_snapshot(&mut out, s, now_ms);
                    }
                    None => out.push_str(&format!("    ✗ {RATE_LIMITED_TEXT}{retry}\n")),
                }
            }
            AccountUsageOutcome::Error { error, .. } => {
                let status = error
                    .http_status
                    .map(|s| format!(" [HTTP {s}]"))
                    .unwrap_or_default();
                out.push_str(&format!(
                    "    ✗ {}{status}: {}\n",
                    error.kind, error.message
                ));
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

// Remove terminal controls from provider-supplied display metadata.
fn safe_text(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .collect()
}

fn inline_text(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

fn compact_window(w: &UsageWindow, now_ms: u64) -> String {
    let used = w
        .used_percent
        .valid()
        .map(|p| format!("{p:.0}% used"))
        .unwrap_or_else(|| "usage unknown".into());
    let reset = w
        .reset_at
        .map(|at| relative_time(now_ms, at))
        .unwrap_or_else(|| "reset unknown".into());
    let label = if w.label == "unknown" {
        &w.id
    } else {
        &w.label
    };
    let scope = match &w.scope {
        WindowScope::Model { model } => format!(" · {model}"),
        WindowScope::Feature { feature } if *feature == w.id => String::new(),
        WindowScope::Feature { feature } => format!(" · {feature}"),
        WindowScope::Account => String::new(),
    };
    let flag = if w.is_exhausted() {
        " · EXHAUSTED"
    } else {
        ""
    };
    format!("{label}{scope}: {used} · {reset}{flag}")
}

/// One reading's compact lines: its limits, its main windows, the models
/// it shows unavailable, and a provisional schema.
fn compact_snapshot(out: &mut String, s: &UsageSnapshot, now_ms: u64) {
    if s.limit_reached == Some(true) {
        out.push_str("      ! Account limit reached\n");
    }
    if s.spend_control_reached == Some(true) {
        out.push_str("      ! Spend control reached\n");
    }
    let mut shown = 0;
    for w in &s.windows {
        // Quiet auxiliary counters move to --verbose, not the
        // main inference windows (even when unknown or at 0%).
        let auxiliary = matches!(w.scope, WindowScope::Feature { .. })
            || (w.duration_secs.is_none() && w.reset_at.is_none());
        if auxiliary && !w.is_exhausted() {
            continue;
        }
        out.push_str(&format!(
            "      {}\n",
            inline_text(&compact_window(w, now_ms))
        ));
        shown += 1;
    }
    if shown == 0 {
        out.push_str("      Usage/reset unknown; see --verbose\n");
    }
    for model in &s.model_availability {
        if model.availability != Availability::Available {
            let state = if model.availability == Availability::Exhausted {
                "unavailable"
            } else {
                "availability unknown"
            };
            out.push_str(&format!(
                "      ! {}: {state}\n",
                inline_text(&model.model)
            ));
        }
    }
    if s.notes.iter().any(|n| n.contains("unverified")) {
        out.push_str("      ! Provider schema unverified; treat usage as provisional\n");
    }
}

/// Compact stacked rows remain readable in narrow terminals, without dropping
/// unknown/exhausted primary windows or hiding per-account failures.
fn render_text(report: &UsageReport, now_ms: u64) -> String {
    if report.accounts.is_empty() {
        return render_verbose(report, now_ms);
    }
    let mut out = format!(
        "\n  Account Usage · {} · {} accounts · {} errors\n",
        report.source,
        report.accounts.len(),
        report.error_count()
    );
    let mut groups = std::collections::BTreeMap::<&str, Vec<&AccountUsageEntry>>::new();
    for entry in &report.accounts {
        groups.entry(&entry.provider).or_default().push(entry);
    }
    let mut missing_identity = false;
    for (provider, entries) in groups {
        out.push_str(&format!("\n  {}\n", inline_text(provider)));
        for entry in entries {
            let identity = entry.identity.as_deref().unwrap_or("unknown");
            missing_identity |= entry.identity.is_none() && entry.account != "*";
            let plan = match &entry.outcome {
                AccountUsageOutcome::Ok { snapshot } => snapshot.plan.as_deref().unwrap_or(""),
                _ => "",
            };
            let plan = if plan.is_empty() {
                String::new()
            } else {
                format!(" · {}", inline_text(plan))
            };
            out.push_str(&format!(
                "    {} · email: {}{plan}\n",
                inline_text(&entry.account),
                inline_text(identity)
            ));
            match &entry.outcome {
                AccountUsageOutcome::Error { error, last_good } if error.kind == RATE_LIMITED_KIND => {
                    let retry = retry_text(now_ms, error.retry_at_ms);
                    match last_good {
                        Some(s) => {
                            out.push_str(&format!("      ! usage read rate-limited{retry}\n"));
                            out.push_str(&format!(
                                "      last good reading, read {} ago\n",
                                age_text(now_ms, s.observed_at)
                            ));
                            compact_snapshot(&mut out, s, now_ms);
                        }
                        None => out.push_str(&format!("      ! {RATE_LIMITED_TEXT}{retry}\n")),
                    }
                }
                AccountUsageOutcome::Error { error, .. } => {
                    out.push_str(&format!(
                        "      ! {}: {}\n",
                        inline_text(&error.kind),
                        inline_text(&error.message)
                    ));
                    if error.kind == "unauthorized" || error.kind == "credential" {
                        out.push_str(&format!(
                            "      Re-login: synaps login --provider {} --account {}\n",
                            inline_text(provider),
                            inline_text(&entry.account)
                        ));
                    }
                }
                AccountUsageOutcome::Ok { snapshot: s } => {
                    // A reading taken before this run began: a broker's
                    // recent one, shown instead of a request.
                    if s.observed_at < report.observed_at {
                        out.push_str(&format!(
                            "      read {} ago\n",
                            age_text(now_ms, s.observed_at)
                        ));
                    }
                    compact_snapshot(&mut out, s, now_ms);
                }
            }
        }
    }
    if missing_identity {
        out.push_str(
            "\n  Email unknown? Run `synaps auth identify --force` on the credential host.\n",
        );
    }
    out.push_str(
        "\n  Percentages are used capacity. --verbose: full details · --json: structured output\n",
    );
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
        let now = synaps_cli::epoch_millis();
        let text = if opts.verbose {
            render_verbose(&report, now)
        } else {
            render_text(&report, now)
        };
        print!("{}", safe_text(&text));
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
        accounts: BTreeMap<OAuthProviderId, Result<Vec<String>, BrokerError>>,
        defaults: BTreeMap<OAuthProviderId, Result<Account, String>>,
        results: BTreeMap<String, Result<UsageSnapshot, FetchFailure>>,
        identities: BTreeMap<OAuthProviderId, Vec<(String, String)>>,
    }

    impl FakeBackend {
        fn new() -> Self {
            Self {
                accounts: BTreeMap::new(),
                defaults: BTreeMap::new(),
                results: BTreeMap::new(),
                identities: BTreeMap::new(),
            }
        }
        fn with_accounts(mut self, p: OAuthProviderId, labels: &[&str]) -> Self {
            self.accounts
                .insert(p, Ok(labels.iter().map(|s| s.to_string()).collect()));
            self
        }
        fn with_listing_error(mut self, p: OAuthProviderId, msg: &str) -> Self {
            self.accounts.insert(p, Err(BrokerError::Credential(msg.into())));
            self
        }
        fn with_listing_broker_error(mut self, p: OAuthProviderId, e: BrokerError) -> Self {
            self.accounts.insert(p, Err(e));
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
            self.results.insert(key.into(), result.map_err(FetchFailure::from));
            self
        }
        fn with_failure(mut self, key: &str, failure: FetchFailure) -> Self {
            self.results.insert(key.into(), Err(failure));
            self
        }
    }

    #[async_trait]
    impl UsageBackend for FakeBackend {
        fn source_label(&self) -> &str {
            "local"
        }
        async fn list_accounts(&self, provider: OAuthProviderId) -> Result<Vec<String>, BrokerError> {
            self.accounts
                .get(&provider)
                .cloned()
                .unwrap_or_else(|| Ok(Vec::new()))
        }
        async fn identities(&self, provider: OAuthProviderId) -> Vec<(String, String)> {
            self.identities.get(&provider).cloned().unwrap_or_default()
        }
        fn resolve_default(&self, provider: OAuthProviderId) -> Result<Account, String> {
            self.defaults
                .get(&provider)
                .cloned()
                .unwrap_or(Ok(Account::Default))
        }
        async fn fetch(&self, cred: &CredentialRef) -> Result<UsageSnapshot, FetchFailure> {
            self.results
                .get(&cred.storage_key())
                .cloned()
                .unwrap_or_else(|| {
                    Err(UsageErrorSummary::other(
                        "unknown_account",
                        format!("no such account {}", cred.storage_key()),
                    )
                    .into())
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
            verbose: false,
        }
    }

    // ── provider parsing ─────────────────────────────────────────────

    #[test]
    fn provider_aliases_and_rejections() {
        assert_eq!(
            parse_status_provider("claude").unwrap(),
            OAuthProviderId::Anthropic
        );
        assert_eq!(
            parse_status_provider("Anthropic").unwrap(),
            OAuthProviderId::Anthropic
        );
        assert_eq!(
            parse_status_provider("codex").unwrap(),
            OAuthProviderId::OpenAiCodex
        );
        assert_eq!(
            parse_status_provider("openai-codex").unwrap(),
            OAuthProviderId::OpenAiCodex
        );
        assert_eq!(
            parse_status_provider("kimi-code").unwrap(),
            OAuthProviderId::KimiCode
        );
        assert_eq!(parse_status_provider("grok").unwrap(), OAuthProviderId::Xai);
        assert_eq!(
            parse_status_provider("xai-auth").unwrap(),
            OAuthProviderId::Xai
        );
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
        let sel = select_targets(&backend, &StatusOptions::default())
            .await
            .unwrap();
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
        let sel = select_targets(
            &backend,
            &opts(Some("codex"), Some("default"), false, false),
        )
        .await
        .unwrap();
        assert_eq!(sel.targets[0].storage_key(), "openai-codex");
        let err = select_targets(
            &backend,
            &opts(Some("codex"), Some("Bad Label!"), false, false),
        )
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
            .with_accounts(
                OAuthProviderId::OpenAiCodex,
                &["default", "astra2", "BAD LABEL"],
            )
            .with_listing_error(OAuthProviderId::KimiCode, "auth.json unreadable");
        let sel = select_targets(&backend, &opts(None, None, true, false))
            .await
            .unwrap();
        let keys: Vec<String> = sel.targets.iter().map(|c| c.storage_key()).collect();
        assert_eq!(
            keys,
            vec![
                "anthropic",
                "anthropic@work",
                "openai-codex",
                "openai-codex@astra2"
            ]
        );
        assert_eq!(sel.listing_failures.len(), 2);
        assert!(sel
            .listing_failures
            .iter()
            .any(|f| f.provider == OAuthProviderId::OpenAiCodex
                && f.message.contains("label rejected")));
        assert!(sel
            .listing_failures
            .iter()
            .any(|f| f.provider == OAuthProviderId::KimiCode && f.message.contains("unreadable")));
        // --all --provider restricts to one provider.
        let sel = select_targets(&backend, &opts(Some("claude"), None, true, false))
            .await
            .unwrap();
        assert_eq!(sel.targets.len(), 2);
        assert!(sel
            .targets
            .iter()
            .all(|c| c.provider == OAuthProviderId::Anthropic));
        assert!(sel.listing_failures.is_empty());
    }

    #[tokio::test]
    async fn all_against_an_old_broker_names_it_and_keeps_the_default_account() {
        let too_old = || BrokerError::BrokerTooOld("account listing".into());
        let backend = FakeBackend::new()
            .with_listing_broker_error(OAuthProviderId::Anthropic, too_old())
            .with_listing_broker_error(OAuthProviderId::OpenAiCodex, too_old())
            .with_result("anthropic", Ok(anthropic_snapshot("default")));
        let sel = select_targets(&backend, &opts(None, None, true, false))
            .await
            .unwrap();
        let keys: Vec<String> = sel.targets.iter().map(|c| c.storage_key()).collect();
        assert_eq!(keys, vec!["anthropic"], "default slot served via legacy /usage");
        assert!(sel
            .listing_failures
            .iter()
            .all(|f| f.message.contains("broker is older than this client")));
        let report = build_report(&backend, sel).await;
        assert_eq!(report.ok_count(), 1);
        let text = render_verbose(&report, T0);
        assert!(!text.contains("No connected accounts"), "{text}");
        assert!(text.contains("broker is older than this client"), "{text}");
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
                    retry_at_ms: None,
                }),
            );
        let sel = select_targets(&backend, &opts(None, None, true, true))
            .await
            .unwrap();
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
        assert_eq!(
            rows[1]["snapshot"]["windows"][0]["used_percent"]["state"],
            "valid"
        );
        assert_eq!(
            rows[2]["snapshot"]["model_availability"][0]["model"],
            "gpt-6-astra"
        );
        assert_eq!(
            rows[2]["snapshot"]["model_availability"][0]["availability"],
            "exhausted"
        );
        assert_eq!(rows[3]["account"], "astra2");
        assert_eq!(rows[3]["error"]["http_status"], 401);
        // Round-trips through the shared schema.
        let back: UsageReport = serde_json::from_value(v).unwrap();
        assert_eq!(back, report);

        let text = render_verbose(&report, T0);
        assert!(text.contains("⚡ Account Usage (local)"));
        assert!(text.contains("anthropic / default"));
        assert!(text.contains("five_hour (5h)"));
        assert!(text.contains("12%") || text.contains("13%"));
        assert!(
            text.contains("unknown (null)"),
            "unknown percent is labelled, not zeroed"
        );
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
                Err(UsageErrorSummary::other(
                    "transport",
                    "usage transport error: connect",
                )),
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
                Err(UsageErrorSummary::other(
                    "transport",
                    "usage transport error: connect",
                )),
            );
        run_with_backend(&backend, &opts(None, None, true, true))
            .await
            .expect("partial failure is not fatal");

        let empty = FakeBackend::new();
        run_with_backend(&empty, &opts(None, None, true, false))
            .await
            .expect("no connected accounts is not an error");
        let report = build_report(
            &empty,
            select_targets(&empty, &opts(None, None, true, false))
                .await
                .unwrap(),
        )
        .await;
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
            AccountUsageOutcome::Error { error, .. } => assert_eq!(error.kind, "unknown_account"),
            other => panic!("expected error row, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn identities_are_scoped_to_provider_and_slot_even_on_usage_failure() {
        let mut backend = FakeBackend::new()
            .with_accounts(OAuthProviderId::Anthropic, &["default", "work"])
            .with_accounts(OAuthProviderId::OpenAiCodex, &["default"])
            .with_result("anthropic", Ok(anthropic_snapshot("default")))
            .with_result("openai-codex", Ok(codex_snapshot("default")));
        backend.identities.insert(
            OAuthProviderId::Anthropic,
            vec![
                ("default".into(), "personal@example.com".into()),
                ("work".into(), "work@example.com".into()),
            ],
        );
        let sel = select_targets(&backend, &opts(None, None, true, false))
            .await
            .unwrap();
        let report = build_report(&backend, sel).await;
        assert_eq!(
            report.accounts[0].identity.as_deref(),
            Some("personal@example.com")
        );
        assert_eq!(
            report.accounts[1].identity.as_deref(),
            Some("work@example.com")
        );
        assert!(matches!(
            report.accounts[1].outcome,
            AccountUsageOutcome::Error { .. }
        ));
        assert_eq!(report.accounts[2].identity, None);
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["accounts"][1]["identity"], "work@example.com");
        assert!(json["accounts"][2].get("identity").is_none());
        let text = render_text(&report, T0);
        assert!(text.contains("work · email: work@example.com"));
        assert!(text.contains("default · email: unknown · pro"));
        assert!(text.contains("unknown_account"));
        assert!(text.contains("auth identify --force"));
    }

    #[test]
    fn compact_view_preserves_limits_unknowns_and_scoped_exhaustion() {
        let snapshot = parse_anthropic_usage(
            r#"{
            "five_hour": {"utilization": 0, "resets_at": null},
            "seven_day": {"utilization": null, "resets_at": null},
            "seven_day_sonnet": {"utilization": 100, "resets_at": "2025-09-24T00:00:00Z"},
            "extra_usage": {"utilization": 0, "is_enabled": false}
        }"#,
            "work",
            T0,
        )
        .unwrap();
        let mut report = UsageReport::new("remote", T0);
        report.accounts.push(AccountUsageEntry {
            provider: "anthropic".into(),
            account: "work".into(),
            identity: Some("person@example.com".into()),
            outcome: AccountUsageOutcome::ok(snapshot),
        });
        let text = render_text(&report, T0);
        assert!(text.contains("remote · 1 accounts · 0 errors"));
        assert!(text.contains("5h: 0% used · reset unknown"));
        assert!(text.contains("7d: usage unknown · reset unknown"));
        assert!(text.contains("sonnet: 100% used"));
        assert!(text.contains("EXHAUSTED"));
        assert!(!text.contains("extra_usage"));
        assert!(!text.contains("note:"));
        assert!(!text.contains("auth identify"));
        // extra_usage is off on this account: no row in either view.
        assert!(!render_verbose(&report, T0).contains("extra_usage"));
        if let AccountUsageOutcome::Ok { snapshot } = &mut report.accounts[0].outcome {
            snapshot.spend_control_reached = Some(true);
            snapshot.limit_reached = Some(true);
            snapshot
                .notes
                .push("grok billing schema unverified against a live account".into());
        }
        let text = render_text(&report, T0);
        assert!(text.contains("Spend control reached"));
        assert!(text.contains("Account limit reached"));
        assert!(text.contains("Provider schema unverified"));
    }

    /// A4: Fable's weekly limit, read from the `limits` list, is a row of its
    /// own (`7d · fable`), EXHAUSTED when spent, and Fable is listed as
    /// unavailable; the shared windows are unchanged and no note is added
    /// (the repeats of the shared windows are skipped quietly).
    #[test]
    fn compact_view_shows_fables_weekly_limit_from_the_limits_list() {
        let snapshot = parse_anthropic_usage(
            r#"{
            "five_hour": {"utilization": 20, "resets_at": null},
            "seven_day": {"utilization": 30, "resets_at": null},
            "limits": [
                {"kind": "session", "percent": 20},
                {"kind": "weekly_all", "percent": 30},
                {"kind": "weekly_scoped", "scope": {"model": {"display_name": "Fable"}},
                 "percent": 100, "resets_at": "2025-09-24T00:00:00Z"}
            ]
        }"#,
            "work",
            T0,
        )
        .unwrap();
        let mut report = UsageReport::new("local", T0);
        report.accounts.push(AccountUsageEntry {
            provider: "anthropic".into(),
            account: "work".into(),
            identity: Some("person@example.com".into()),
            outcome: AccountUsageOutcome::ok(snapshot),
        });
        let text = render_text(&report, T0);
        assert!(text.contains("5h: 20% used"), "{text}");
        assert!(text.contains("7d: 30% used"), "{text}");
        assert!(text.contains("7d · fable: 100% used"), "{text}");
        assert!(text.contains("EXHAUSTED"), "{text}");
        assert!(text.contains("! fable: unavailable"), "{text}");
        let verbose = render_verbose(&report, T0);
        assert!(!verbose.contains("note:"), "{verbose}");
    }

    /// An account without extra usage enabled shows no extra_usage
    /// line at all. An enabled one shows a single title, and an unknown
    /// reading draws no row of '?'.
    #[test]
    fn extra_usage_shows_only_when_enabled_with_one_title_and_unknown_draws_no_gauge() {
        let entry = |body: &str| AccountUsageEntry {
            provider: "anthropic".into(),
            account: "work".into(),
            identity: None,
            outcome: AccountUsageOutcome::ok(parse_anthropic_usage(body, "work", T0).unwrap()),
        };
        let mut report = UsageReport::new("local", T0);
        report.accounts.push(entry(
            r#"{"five_hour": {"utilization": 10, "resets_at": "2025-09-20T22:00:00Z"},
                "seven_day": {"utilization": null, "resets_at": null},
                "extra_usage": {"is_enabled": false, "monthly_limit": null, "used_credits": null, "utilization": null}}"#,
        ));
        let off = render_verbose(&report, T0);
        assert!(!off.contains("extra_usage"), "{off}");
        assert!(off.contains("\n    unknown (null)\n"), "{off}");
        assert!(!off.contains("??"), "{off}");
        assert!(!render_text(&report, T0).contains("extra_usage"));
        report.accounts[0] = entry(
            r#"{"five_hour": {"utilization": 10, "resets_at": "2025-09-20T22:00:00Z"},
                "extra_usage": {"is_enabled": true, "monthly_limit": 5000, "used_credits": 5000, "utilization": 100}}"#,
        );
        let on = render_verbose(&report, T0);
        assert!(on.contains("\n    extra_usage\n"), "{on}");
        assert!(!on.contains("extra_usage · extra_usage"), "{on}");
        assert!(on.contains("100%  EXHAUSTED"), "{on}");
        let compact = render_text(&report, T0);
        assert!(
            compact.contains("extra_usage: 100% used · reset unknown · EXHAUSTED"),
            "{compact}"
        );
        assert!(!compact.contains("extra_usage · extra_usage"), "{compact}");
    }

    #[test]
    fn metadata_cannot_inject_terminal_controls_or_rows() {
        assert_eq!(
            inline_text("a\nb\r\t\x1b[2J@example.com"),
            "ab[2J@example.com"
        );
        assert_eq!(safe_text("a\nb\x1b[2J"), "a\nb[2J");
    }

    #[test]
    fn text_helpers() {
        assert_eq!(relative_time(T0, T0 - 1), "reset passed");
        assert_eq!(relative_time(T0, T0 + 90 * 60 * 1000), "resets in 1h 30m");
        assert_eq!(relative_time(T0, T0 + 26 * 3600 * 1000), "resets in 1d 2h");
        assert_eq!(relative_time(T0, T0 + 5 * 60 * 1000), "resets in 5m");
        assert_eq!(absolute_time(T0), "2025-09-20 20:26 UTC");
        assert_eq!(bar(0.0).chars().filter(|c| *c == '█').count(), 0);
        assert_eq!(bar(50.0).chars().filter(|c| *c == '█').count(), 15);
        assert_eq!(bar(100.0).chars().filter(|c| *c == '█').count(), 30);
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

    #[test]
    fn an_old_broker_is_named_as_the_cause() {
        let s = summary_from_broker_error(&BrokerError::BrokerTooOld("per-account usage".into()));
        assert_eq!(s.kind, "broker_too_old");
        assert!(s.message.contains("broker is older than this client"), "{}", s.message);
    }

    // ── the last known usage ────────────────────────────────────────

    const MINUTE: u64 = 60 * 1000;

    /// A usage read the endpoint rate-limited, as `BrokerBackend` reports
    /// it: `usage`'s error plus the broker's retry time and last good
    /// reading.
    fn rate_limited(retry_at_ms: u64, last_good: Option<UsageSnapshot>) -> FetchFailure {
        let error = synaps_cli::auth::UsageError::RateLimited {
            retry_after_s: Some(0),
        }
        .into_broker_error();
        fetch_failure(&UsageReadFailure {
            error,
            rate_limited: Some(synaps_cli::auth::RateLimitedRead {
                retry_at_ms,
                last_good,
            }),
        })
    }

    /// Any other failed usage read (here a 503), as `BrokerBackend`
    /// reports it.
    fn upstream_503() -> FetchFailure {
        let error = synaps_cli::auth::UsageError::UpstreamStatus {
            status: 503,
            retry_after_s: None,
        }
        .into_broker_error();
        fetch_failure(&UsageReadFailure::plain(error))
    }

    /// A report made at `T0` with one Anthropic row per `(label, read)`.
    fn report_of(rows: Vec<(&str, Result<UsageSnapshot, FetchFailure>)>) -> UsageReport {
        let mut report = UsageReport::new("local", T0);
        for (label, read) in rows {
            report.accounts.push(AccountUsageEntry {
                provider: "anthropic".into(),
                account: label.into(),
                identity: None,
                outcome: row_outcome(read),
            });
        }
        report
    }

    /// A read the usage endpoint rate-limited shows the account's last good
    /// reading with its age, and when the next request goes out (the
    /// broker's time, not the endpoint's "retry after 0s"); without a last
    /// good reading one plain line. Neither shows the transport chain.
    /// Any other failed read keeps today's text.
    #[test]
    fn a_rate_limited_read_shows_the_last_good_reading_its_age_and_the_retry_time() {
        let now = T0 + 13 * MINUTE;
        let retry_at = now + 90_000;
        let report = report_of(vec![
            ("work", Err(rate_limited(retry_at, Some(anthropic_snapshot("work"))))),
            ("spare", Err(rate_limited(retry_at, None))),
            ("late", Err(rate_limited(now - 1_000, None))),
            ("down", Err(upstream_503())),
        ]);
        let text = render_text(&report, now);
        assert!(
            text.contains(
                "    work · email: unknown\n      ! usage read rate-limited, retry in 90s\n      last good reading, read 13m ago\n      5h: "
            ),
            "{text}"
        );
        assert!(text.contains("sonnet: 100% used"), "the last good windows: {text}");
        assert!(
            text.contains(
                "    spare · email: unknown\n      ! usage read rate-limited (HTTP 429), retry in 90s\n"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "    late · email: unknown\n      ! usage read rate-limited (HTTP 429)\n"
            ),
            "a retry time already past is not shown: {text}"
        );
        assert!(
            text.contains("    down · email: unknown\n      ! transport: broker transport error: usage:"),
            "{text}"
        );
        assert_eq!(text.matches("transport").count(), 2, "only the 503's row: {text}");
        assert!(text.contains("4 accounts · 4 errors"), "{text}");

        let verbose = render_verbose(&report, now);
        assert!(
            verbose.contains(
                "    ✗ usage read rate-limited, retry in 90s\n    last good reading, read 13m ago\n    plan:"
            ) || verbose.contains(
                "    ✗ usage read rate-limited, retry in 90s\n    last good reading, read 13m ago\n    five_hour"
            ),
            "{verbose}"
        );
        assert!(
            verbose.contains("    ✗ usage read rate-limited (HTTP 429), retry in 90s\n"),
            "{verbose}"
        );
        assert!(verbose.contains("    ✗ usage read rate-limited (HTTP 429)\n"), "{verbose}");
        assert!(verbose.contains("    ✗ transport: broker transport error: usage:"), "{verbose}");
        assert_eq!(verbose.matches("transport").count(), 2, "{verbose}");
    }

    /// `--json`: a rate-limited row is still an error row (and still counts
    /// as one: every account failing still fails the command), of kind
    /// `usage_rate_limited`, with the optional `retry_at_ms` and
    /// `last_good`; any other error row keeps today's bytes.
    #[tokio::test]
    async fn a_rate_limited_row_in_json_has_its_retry_time_and_last_good_and_stays_an_error() {
        let backend = FakeBackend::new()
            .with_accounts(OAuthProviderId::Anthropic, &["work", "down"])
            .with_failure(
                "anthropic@work",
                rate_limited(T0 + 90_000, Some(anthropic_snapshot("work"))),
            )
            .with_failure("anthropic@down", upstream_503());
        let sel = select_targets(&backend, &opts(None, None, true, true))
            .await
            .unwrap();
        let report = build_report(&backend, sel).await;
        assert_eq!((report.ok_count(), report.error_count()), (0, 2));
        let v = serde_json::to_value(&report).unwrap();
        let rows = v["accounts"].as_array().unwrap();
        assert_eq!(rows[0]["account"], "work");
        assert_eq!(rows[0]["status"], "error");
        assert_eq!(
            rows[0]["error"],
            serde_json::json!({
                "kind": "usage_rate_limited",
                "message": "usage read rate-limited (HTTP 429)",
                "http_status": 429,
                "retry_at_ms": T0 + 90_000,
            })
        );
        assert_eq!(rows[0]["last_good"]["observed_at"], T0);
        assert_eq!(rows[1]["account"], "down");
        let keys = |v: &serde_json::Value| {
            let mut keys: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
            keys.sort();
            keys
        };
        assert_eq!(keys(&rows[1]), ["account", "error", "provider", "status"]);
        assert_eq!(keys(&rows[1]["error"]), ["kind", "message"]);
        assert_eq!(rows[1]["error"]["kind"], "transport");
        let back: UsageReport = serde_json::from_value(v).unwrap();
        assert_eq!(back, report);
        let err = run_with_backend(&backend, &opts(None, None, true, true))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("all 2 requested"), "{err}");
    }

    /// A reading taken before this status run (a broker's recent reading,
    /// shown instead of a request) shows its age; one taken in the run
    /// shows none.
    #[test]
    fn a_reading_taken_before_the_run_shows_its_age_and_one_taken_in_it_does_not() {
        let mut saved = anthropic_snapshot("work");
        saved.observed_at = T0 - 5 * MINUTE;
        let report = report_of(vec![
            ("work", Ok(saved)),
            ("spare", Ok(anthropic_snapshot("spare"))),
        ]);
        let now = T0 + 2_000;
        let text = render_text(&report, now);
        assert!(
            text.contains("    work · email: unknown\n      read 5m ago\n      5h: "),
            "{text}"
        );
        assert_eq!(text.matches(" ago\n").count(), 1, "{text}");
        let verbose = render_verbose(&report, now);
        assert!(verbose.contains("    email: unknown\n    read 5m ago\n"), "{verbose}");
        assert_eq!(verbose.matches(" ago\n").count(), 1, "{verbose}");
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
