//! `synaps auth list|use|remove|identify` — operator management of stored
//! OAuth account slots. Reads labels and non-secret metadata only; never
//! prints or copies token material.

use std::io::{self, IsTerminal, Write};

use synaps_cli::auth::{
    self, Account, AccountSelector, AccountSummary, CredentialRef, OAuthProviderId, SeatResolution,
};
use synaps_cli::config;

/// Options for `synaps auth list`.
#[derive(Debug, Clone, Default)]
pub struct ListOptions {
    pub provider: Option<String>,
    pub json: bool,
}

/// Options for `synaps auth identify`.
#[derive(Debug, Clone, Default)]
pub struct IdentifyOptions {
    pub provider: Option<String>,
    pub account: Option<String>,
    /// Resolve and report, but write nothing.
    pub dry_run: bool,
    /// Re-verify slots that already carry an `accountId`.
    pub force: bool,
    pub json: bool,
}

fn parse_provider(raw: &str) -> Result<OAuthProviderId, String> {
    auth::provider::parse_cli_provider(raw).map_err(|e| {
        format!(
            "{e}. OAuth providers: {}",
            auth::provider::DESCRIPTORS
                .iter()
                .map(|d| d.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })
}

fn format_expiry(expires: u64) -> String {
    if expires == 0 {
        return "unknown".into();
    }
    let now = synaps_cli::epoch_millis();
    if expires <= now {
        return "expired (refreshable)".into();
    }
    let mins = (expires - now) / 60_000;
    if mins < 60 {
        format!("in {mins}m")
    } else if mins < 24 * 60 {
        format!("in {}h{:02}m", mins / 60, mins % 60)
    } else {
        format!("in {}d", mins / (24 * 60))
    }
}

/// Where the listing came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Local,
    Remote,
}

impl Source {
    fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Remote => "remote",
        }
    }
}

/// Gather account rows: local storage (with policy annotation) or the
/// remote broker's `/capabilities` account rows when this client uses a
/// remote credential source.
async fn gather(
    provider: Option<OAuthProviderId>,
) -> Result<(Source, Vec<AccountSummary>, auth::AccountInventory), String> {
    let cfg = config::load_config();
    let source = cfg.auth.credential_source();
    if source.is_remote() {
        let broker = auth::broker_from_source(&source, &auth::TokenCache::new(), reqwest::Client::new());
        let caps = broker
            .capabilities()
            .await
            .map_err(|e| format!("remote broker capabilities failed: {e}"))?;
        let rows: Vec<AccountSummary> = caps
            .into_iter()
            .filter(|c| provider.map_or(true, |p| c.key == p.as_str()))
            .flat_map(|c| c.accounts)
            .collect();
        return Ok((Source::Remote, rows, auth::AccountInventory::default()));
    }
    let inventory = auth::list_accounts_detailed(provider)?;
    let policy = cfg.auth.account_policy().with_env_overlay();
    let mut rows = inventory.accounts.clone();
    for row in &mut rows {
        if let Ok(id) = row.provider.parse::<OAuthProviderId>() {
            row.selected = matches!(policy.selector(id), AccountSelector::Account(a) if a.label_str() == row.label);
        }
    }
    Ok((Source::Local, rows, inventory))
}

/// `synaps auth list [--provider <id>] [--json]`.
pub async fn list(opts: ListOptions) -> Result<(), String> {
    let provider = opts.provider.as_deref().map(parse_provider).transpose()?;
    let (source, rows, inventory) = gather(provider).await?;
    let cfg = config::load_config();
    let policy = cfg.auth.account_policy().with_env_overlay();

    if opts.json {
        let selectors: serde_json::Map<String, serde_json::Value> = auth::provider::DESCRIPTORS
            .iter()
            .filter(|d| provider.map_or(true, |p| p == d.id))
            .map(|d| {
                let value = match policy.selector(d.id) {
                    AccountSelector::Account(a) => serde_json::json!({"kind": "account", "account": a.label_str()}),
                    AccountSelector::Auto => serde_json::json!({"kind": "auto"}),
                    AccountSelector::Invalid { source, reason } => {
                        serde_json::json!({"kind": "invalid", "source": source, "reason": reason})
                    }
                };
                (d.id.as_str().to_string(), value)
            })
            .collect();
        let out = serde_json::json!({
            "schema_version": 1,
            "source": source.as_str(),
            "accounts": rows,
            "selectors": selectors,
            "malformed_keys": inventory.malformed_keys,
            "duplicate_identity_keys": inventory.duplicate_identity_keys,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&out).map_err(|e| e.to_string())?
        );
        return Ok(());
    }

    if rows.is_empty() {
        println!(
            "No OAuth accounts stored{} ({}). Run `synaps login --provider <id> [--account <label>]`.",
            provider
                .map(|p| format!(" for {p}"))
                .unwrap_or_default(),
            source.as_str()
        );
    } else {
        println!(
            "{:<16} {:<12} {:<30} {:<10} {:<22} SELECTED",
            "PROVIDER", "ACCOUNT", "IDENTITY", "ID", "EXPIRES"
        );
        for row in &rows {
            let mut flags = Vec::new();
            if row.selected {
                flags.push("*");
            }
            if row.cooldown_until.is_some() {
                flags.push("cooldown");
            }
            if inventory
                .duplicate_identity_keys
                .iter()
                .any(|k| CredentialRef::parse_storage_key(k).is_some_and(|c| {
                    c.provider.as_str() == row.provider && c.account.label_str() == row.label
                }))
            {
                flags.push("DUPLICATE-SEAT");
            }
            println!(
                "{:<16} {:<12} {:<30} {:<10} {:<22} {}",
                row.provider,
                row.label,
                row.identity.as_deref().unwrap_or("-"),
                row.account_id_prefix.as_deref().unwrap_or("-"),
                format_expiry(row.expires),
                flags.join(" ")
            );
        }
    }
    for d in auth::provider::DESCRIPTORS.iter() {
        if provider.is_some_and(|p| p != d.id) {
            continue;
        }
        match policy.selector(d.id) {
            AccountSelector::Auto => println!(
                "note: {} uses automatic capacity selection (auth.account.{} = auto)",
                d.id, d.id
            ),
            AccountSelector::Invalid { source, reason } => println!(
                "WARNING: {} selection is disabled (fail closed): {source}: {reason}",
                d.id
            ),
            AccountSelector::Account(_) => {}
        }
    }
    if !inventory.malformed_keys.is_empty() {
        println!(
            "WARNING: ignored malformed account entries in {}: {}",
            auth::auth_file_path().display(),
            inventory.malformed_keys.join(", ")
        );
    }
    if !inventory.duplicate_identity_keys.is_empty() {
        println!(
            "WARNING: these slots share one provider seat (two refresh owners): {} — remove all but one with `synaps auth remove`",
            inventory.duplicate_identity_keys.join(", ")
        );
    }
    let unverified = count_unverified_identities(&rows);
    if unverified > 0 {
        println!(
            "note: {unverified} slot(s) have unverified identity; run `synaps auth identify`{} to detect duplicate seats.",
            if source == Source::Remote {
                " on the broker host"
            } else {
                ""
            }
        );
    }
    Ok(())
}

/// Rows of a provider that CAN expose a seat identity but whose slot carries
/// none yet (a login from before identity acquisition, or one whose lookup
/// failed). These are exactly the slots `auth identify` can backfill.
fn count_unverified_identities(rows: &[AccountSummary]) -> usize {
    rows.iter()
        .filter(|row| {
            row.account_id_prefix.is_none()
                && row
                    .provider
                    .parse::<OAuthProviderId>()
                    .is_ok_and(auth::supports_seat_identity)
        })
        .count()
}

// ── identify ────────────────────────────────────────────────────────────────

/// Per-slot outcome of `auth identify`. Never carries token material.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
struct IdentifyRow {
    provider: String,
    label: String,
    /// `resolved` (fetched and written), `already` (slot had an id; not
    /// re-verified without --force), `skipped` (fetched, not written:
    /// --dry-run), `failed`, `unsupported`.
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    identity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    account_id_prefix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    seat_fingerprint: Option<String>,
    /// Secret-free reason for `failed`/`unsupported`/`skipped`.
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

impl IdentifyRow {
    fn new(row: &AccountSummary, status: &'static str) -> Self {
        Self {
            provider: row.provider.clone(),
            label: row.label.clone(),
            status,
            identity: row.identity.clone(),
            account_id_prefix: row.account_id_prefix.clone(),
            seat_fingerprint: row.seat_fingerprint.clone(),
            detail: None,
        }
    }

    fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// Replace the identity views with a freshly resolved seat (the summary
    /// helper derives prefix + fingerprint from the full id, which is then
    /// dropped — the row never keeps the full account id).
    fn with_seat(mut self, provider: OAuthProviderId, seat: &auth::ResolvedSeat) -> Self {
        let view = AccountSummary::new(provider, &Account::Default)
            .with_account_id(Some(&seat.account_id));
        self.identity = seat.identity.clone().or(self.identity);
        self.account_id_prefix = view.account_id_prefix;
        self.seat_fingerprint = view.seat_fingerprint;
        self
    }
}

/// One group of slots that hold the same provider seat.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
struct DuplicateGroup {
    provider: String,
    /// Storage keys (`anthropic`, `anthropic@claude1`, …), sorted.
    keys: Vec<String>,
    /// Display identity (email) or id prefix, whichever is known.
    #[serde(skip_serializing_if = "Option::is_none")]
    shown_as: Option<String>,
}

/// Group `duplicate_identity_keys` by seat using the listing rows' fingerprint
/// (the inventory reports a flat key list). Keys without a readable row are
/// still reported by `auth list`; they cannot be grouped here.
fn duplicate_groups(inventory: &auth::AccountInventory) -> Vec<DuplicateGroup> {
    use std::collections::BTreeMap;
    #[derive(Default)]
    struct Acc {
        keys: Vec<String>,
        email: Option<String>,
        prefix: Option<String>,
    }
    let mut groups: BTreeMap<(String, String), Acc> = BTreeMap::new();
    for key in &inventory.duplicate_identity_keys {
        let Some(cred) = CredentialRef::parse_storage_key(key) else {
            continue;
        };
        let Some(row) = inventory
            .accounts
            .iter()
            .find(|a| a.provider == cred.provider.as_str() && a.label == cred.account.label_str())
        else {
            continue;
        };
        let Some(fp) = row.seat_fingerprint.clone() else {
            continue;
        };
        let acc = groups.entry((row.provider.clone(), fp)).or_default();
        acc.keys.push(key.clone());
        if acc.email.is_none() {
            acc.email = row.identity.clone();
        }
        if acc.prefix.is_none() {
            acc.prefix = row.account_id_prefix.as_ref().map(|p| format!("{p}…"));
        }
    }
    groups
        .into_iter()
        .filter(|(_, acc)| acc.keys.len() > 1)
        .map(|((provider, _), mut acc)| {
            acc.keys.sort();
            DuplicateGroup {
                provider,
                keys: acc.keys,
                // Prefer an email from ANY member; fall back to the id prefix.
                shown_as: acc.email.or(acc.prefix),
            }
        })
        .collect()
}

fn print_duplicate_report(inventory: &auth::AccountInventory, groups: &[DuplicateGroup]) {
    for g in groups {
        let provider = &g.provider;
        println!(
            "⚠ DUPLICATE SEAT: {} share one {provider} account ({}). Keep one; remove the rest with: synaps auth remove --provider {provider} --account <label> -y",
            g.keys.join(", "),
            g.shown_as.as_deref().unwrap_or("unknown identity"),
        );
    }
    // Keys the inventory flagged but no readable row could group (malformed
    // token fields): still say so rather than hide them.
    let grouped: std::collections::BTreeSet<&String> =
        groups.iter().flat_map(|g| g.keys.iter()).collect();
    let ungrouped: Vec<&String> = inventory
        .duplicate_identity_keys
        .iter()
        .filter(|k| !grouped.contains(k))
        .collect();
    if !ungrouped.is_empty() {
        println!(
            "⚠ DUPLICATE SEAT (unreadable slot): {} — see `synaps auth list`",
            ungrouped
                .iter()
                .map(|k| k.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
}

/// `synaps auth identify [--provider <id>] [--account <label|default>] [--dry-run] [--force] [--json]`.
///
/// Backfills the provider seat identity (`accountId` + display `identity`)
/// for stored OAuth slots that predate identity acquisition, then reports
/// slots that share one seat. Local store only: it needs the refresh token
/// to mint an access token for the profile lookup. Errors (non-zero exit)
/// iff duplicates exist after processing, so scripts can gate on it.
pub async fn identify(opts: IdentifyOptions) -> Result<(), String> {
    let provider = opts.provider.as_deref().map(parse_provider).transpose()?;
    let account = opts
        .account
        .as_deref()
        .map(Account::parse)
        .transpose()
        .map_err(|e| format!("invalid --account: {e}"))?;
    let cfg = config::load_config();
    if cfg.auth.credential_source().is_remote() {
        return Err(
            "identify runs on the machine that stores auth.json (a remote credential \
                    source is configured here); run it on the broker host"
                .into(),
        );
    }
    let inventory = auth::list_accounts_detailed(provider)?;
    let targets: Vec<AccountSummary> = inventory
        .accounts
        .iter()
        .filter(|row| {
            account
                .as_ref()
                .map_or(true, |a| a.label_str() == row.label)
        })
        .cloned()
        .collect();
    if let (Some(a), true) = (&account, targets.is_empty()) {
        return Err(format!(
            "no credential stored for {}{}; run `synaps auth list`",
            provider
                .map(|p| format!("{p}@"))
                .unwrap_or_else(|| "<provider>@".into()),
            a
        ));
    }
    let client = auth::identity_http_client()?;

    let mut rows: Vec<IdentifyRow> = Vec::with_capacity(targets.len());
    for row in &targets {
        rows.push(identify_one(&client, row, &opts).await);
    }

    // Re-read the store: the duplicate scan must reflect what was written.
    let inventory = auth::list_accounts_detailed(provider)?;
    let groups = duplicate_groups(&inventory);

    if opts.json {
        let out = serde_json::json!({
            "schema_version": 1,
            "dry_run": opts.dry_run,
            "rows": rows,
            "duplicates": groups.iter().map(|g| g.keys.clone()).collect::<Vec<_>>(),
            "duplicate_groups": groups,
            "duplicate_identity_keys": inventory.duplicate_identity_keys,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&out).map_err(|e| e.to_string())?
        );
    } else {
        if rows.is_empty() {
            println!(
                "No OAuth accounts stored{}. Run `synaps login --provider <id> [--account <label>]`.",
                provider.map(|p| format!(" for {p}")).unwrap_or_default()
            );
        } else {
            println!(
                "{:<16} {:<12} {:<12} {:<30} {:<10} DETAIL",
                "PROVIDER", "ACCOUNT", "STATUS", "IDENTITY", "ID"
            );
            for r in &rows {
                println!(
                    "{:<16} {:<12} {:<12} {:<30} {:<10} {}",
                    r.provider,
                    r.label,
                    r.status,
                    r.identity.as_deref().unwrap_or("-"),
                    r.account_id_prefix.as_deref().unwrap_or("-"),
                    r.detail.as_deref().unwrap_or(""),
                );
            }
        }
        if opts.dry_run {
            println!(
                "note: --dry-run: nothing was written to {}",
                auth::auth_file_path().display()
            );
        }
        print_duplicate_report(&inventory, &groups);
    }
    if !inventory.duplicate_identity_keys.is_empty() {
        return Err(format!(
            "{} slot(s) share a provider seat with another slot",
            inventory.duplicate_identity_keys.len()
        ));
    }
    Ok(())
}

/// Resolve (and unless `--dry-run`, record) the seat for one stored slot.
async fn identify_one(
    client: &reqwest::Client,
    row: &AccountSummary,
    opts: &IdentifyOptions,
) -> IdentifyRow {
    let Some(cred) = row.credential_ref() else {
        return IdentifyRow::new(row, "failed").with_detail("unparseable provider/label");
    };
    if !auth::supports_seat_identity(cred.provider) {
        return IdentifyRow::new(row, "unsupported")
            .with_detail("provider exposes no trustworthy account identity");
    }
    if row.account_id_prefix.is_some() && !opts.force {
        return IdentifyRow::new(row, "already")
            .with_detail("identity stored; --force re-verifies");
    }
    // Same single-flight, CAS-persisted refresh path every runtime uses; a
    // rotated refresh token lands in the store before the profile lookup.
    let fresh = match auth::ensure_fresh_credential(client, &cred).await {
        Ok(creds) => creds,
        Err(e) => return IdentifyRow::new(row, "failed").with_detail(format!("token: {e}")),
    };
    match auth::resolve_seat(cred.provider, &fresh.access, client, None).await {
        SeatResolution::Resolved(seat) => {
            if opts.dry_run {
                return IdentifyRow::new(row, "skipped")
                    .with_seat(cred.provider, &seat)
                    .with_detail("dry-run: not written");
            }
            match auth::set_slot_identity(&cred, &seat.account_id, seat.identity.as_deref()) {
                Ok(()) => IdentifyRow::new(row, "resolved").with_seat(cred.provider, &seat),
                Err(e) => IdentifyRow::new(row, "failed")
                    .with_seat(cred.provider, &seat)
                    .with_detail(format!("store: {e}")),
            }
        }
        SeatResolution::Unsupported => {
            IdentifyRow::new(row, "unsupported").with_detail("token carries no account identity")
        }
        SeatResolution::Failed(msg) => IdentifyRow::new(row, "failed").with_detail(msg),
    }
}

/// `synaps auth use --provider <id> --account <label|default|auto>`.
///
/// Writes `auth.account.<provider>` to the active profile config. Never
/// touches auth.json. An explicit account must exist; `auto` is accepted
/// only when at least one account is stored.
pub async fn use_account(provider: String, account: String) -> Result<(), String> {
    let provider_id = parse_provider(&provider)?;
    let selector = AccountSelector::parse(&account)?;
    let (source, rows, _) = gather(Some(provider_id)).await?;
    match &selector {
        AccountSelector::Account(account) => {
            if !rows.iter().any(|r| r.label == account.label_str()) {
                return Err(format!(
                    "unknown account '{}' for provider '{}' ({} store). Nothing changed. \
                     Run `synaps auth list --provider {}` or `synaps login --provider {} --account {}`.",
                    account,
                    provider_id,
                    source.as_str(),
                    provider_id,
                    provider_id,
                    account
                ));
            }
        }
        AccountSelector::Auto => {
            if rows.is_empty() {
                return Err(format!(
                    "no accounts stored for provider '{provider_id}'; `auto` needs at least one login"
                ));
            }
            if !auth::usage::supports_usage(provider_id) {
                return Err(format!(
                    "provider '{provider_id}' has no usage adapter; automatic selection would never find proven capacity"
                ));
            }
        }
        AccountSelector::Invalid { .. } => unreachable!("parse never yields Invalid"),
    }
    let key = auth::account_config_key(provider_id);
    config::write_config_value(&key, &selector.as_config_value())
        .map_err(|e| format!("failed to write {key}: {e}"))?;
    let env = auth::account_env_var(provider_id);
    println!(
        "{key} = {} written to {}",
        selector.as_config_value(),
        config::resolve_write_path("config").display()
    );
    if std::env::var(&env).is_ok_and(|v| !v.trim().is_empty()) {
        println!("note: {env} is set in this environment and overrides the config value");
    }
    if matches!(selector, AccountSelector::Auto) {
        println!(
            "note: automatic selection uses fresh read-only usage; accounts without proven capacity are never selected"
        );
    }
    Ok(())
}

/// `synaps auth remove --provider <id> --account <label> [--yes]`.
///
/// Removes exactly one slot from the local store. The default slot always
/// requires `--yes`; named slots are confirmed interactively unless `--yes`.
pub fn remove(provider: String, account: String, yes: bool) -> Result<(), String> {
    let provider_id = parse_provider(&provider)?;
    let account = Account::parse(&account)?;
    let cred = CredentialRef::new(provider_id, account);
    if cred.account.is_default() && !yes {
        return Err(format!(
            "refusing to remove the default {} credential without --yes (this is the slot every \
             legacy caller uses)",
            provider_id
        ));
    }
    match auth::load_credential(&cred)? {
        Some(_) => {}
        None => {
            return Err(format!(
                "no credential stored for {cred}; nothing removed. Run `synaps auth list`."
            ))
        }
    }
    if !yes {
        if !(io::stdin().is_terminal() && io::stderr().is_terminal()) {
            return Err(format!(
                "refusing to remove {cred} non-interactively without --yes"
            ));
        }
        eprint!("Remove {cred} from {}? [y/N] ", auth::auth_file_path().display());
        let _ = io::stderr().flush();
        let mut line = String::new();
        io::stdin().read_line(&mut line).map_err(|e| e.to_string())?;
        if !matches!(line.trim(), "y" | "Y" | "yes") {
            return Err("cancelled; nothing removed".into());
        }
    }
    let removed = auth::remove_credential(&cred)?;
    if !removed {
        return Err(format!("{cred} was not present; nothing removed"));
    }
    println!("removed {cred} from {}", auth::auth_file_path().display());
    // A config selection pointing at the removed slot would now fail closed;
    // say so rather than silently redirecting anything.
    let cfg = config::load_config();
    if let AccountSelector::Account(selected) = cfg.auth.account_policy().selector(provider_id) {
        if selected == cred.account && !cred.account.is_default() {
            println!(
                "note: auth.account.{} still selects '{}'; requests for {} will fail until you run \
                 `synaps auth use --provider {} --account <label|default>`",
                provider_id, selected, provider_id, provider_id
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(
        provider: OAuthProviderId,
        label: &str,
        id: Option<&str>,
        identity: Option<&str>,
    ) -> AccountSummary {
        AccountSummary {
            identity: identity.map(str::to_string),
            ..AccountSummary::new(provider, &Account::parse(label).unwrap()).with_account_id(id)
        }
    }

    #[test]
    fn unverified_count_only_covers_providers_that_can_resolve_identity() {
        let rows = vec![
            row(OAuthProviderId::Anthropic, "default", None, None),
            row(
                OAuthProviderId::Anthropic,
                "claude1",
                Some("b8a1448d-1"),
                Some("a@example.com"),
            ),
            row(OAuthProviderId::OpenAiCodex, "astra1", None, None),
            // Kimi/xAI/Copilot/Gemini expose no identity: never counted.
            row(OAuthProviderId::KimiCode, "m27", None, None),
            row(OAuthProviderId::Xai, "grok1", None, None),
        ];
        assert_eq!(count_unverified_identities(&rows), 2);
        assert_eq!(count_unverified_identities(&[]), 0);
    }

    #[test]
    fn duplicate_groups_pair_keys_by_seat_fingerprint() {
        let inventory = auth::AccountInventory {
            accounts: vec![
                row(OAuthProviderId::Anthropic, "default", Some("seat-A"), None),
                row(
                    OAuthProviderId::Anthropic,
                    "claude1",
                    Some("seat-A"),
                    Some("jr@example.com"),
                ),
                row(OAuthProviderId::Anthropic, "claude2", Some("seat-B"), None),
                row(
                    OAuthProviderId::OpenAiCodex,
                    "astra1",
                    Some("seat-C"),
                    Some("c@example.com"),
                ),
                row(OAuthProviderId::OpenAiCodex, "astra2", Some("seat-C"), None),
                // Same raw id as the Anthropic seat: a different provider is a different seat.
                row(OAuthProviderId::OpenAiCodex, "astra3", Some("seat-A"), None),
            ],
            malformed_keys: vec![],
            duplicate_identity_keys: vec![
                "anthropic".into(),
                "anthropic@claude1".into(),
                "openai-codex@astra1".into(),
                "openai-codex@astra2".into(),
            ],
        };
        let groups = duplicate_groups(&inventory);
        assert_eq!(groups.len(), 2, "{groups:?}");
        let anthropic = groups.iter().find(|g| g.provider == "anthropic").unwrap();
        assert_eq!(
            anthropic.keys,
            vec!["anthropic".to_string(), "anthropic@claude1".to_string()]
        );
        assert_eq!(
            anthropic.shown_as.as_deref(),
            Some("jr@example.com"),
            "an email from any member is preferred over the id prefix"
        );
        let codex = groups
            .iter()
            .find(|g| g.provider == "openai-codex")
            .unwrap();
        assert_eq!(
            codex.keys,
            vec![
                "openai-codex@astra1".to_string(),
                "openai-codex@astra2".to_string()
            ]
        );
        let json = serde_json::to_string(&groups).unwrap();
        assert!(
            !json.contains("seat-A"),
            "full account id never leaves the listing: {json}"
        );
        // Nothing flagged → no groups.
        assert!(duplicate_groups(&auth::AccountInventory::default()).is_empty());
    }

    #[test]
    fn duplicate_groups_skip_keys_without_a_readable_row() {
        let inventory = auth::AccountInventory {
            accounts: vec![row(
                OAuthProviderId::Anthropic,
                "default",
                Some("seat-A"),
                None,
            )],
            malformed_keys: vec!["anthropic@broken".into()],
            duplicate_identity_keys: vec!["anthropic".into(), "anthropic@broken".into()],
        };
        // The unreadable sibling cannot be grouped; the flat list still says
        // both keys are flagged (printed by `print_duplicate_report`).
        assert!(duplicate_groups(&inventory).is_empty());
    }

    #[test]
    fn identify_row_with_seat_keeps_only_prefix_and_fingerprint() {
        let base = row(
            OAuthProviderId::Anthropic,
            "claude1",
            None,
            Some("stale@example.com"),
        );
        let seat = auth::ResolvedSeat {
            account_id: "b8a1448d-0000-4000-8000-00000000000a".into(),
            identity: Some("jr@example.com".into()),
        };
        let r = IdentifyRow::new(&base, "resolved").with_seat(OAuthProviderId::Anthropic, &seat);
        assert_eq!(r.identity.as_deref(), Some("jr@example.com"));
        assert_eq!(r.account_id_prefix.as_deref(), Some("b8a1448d"));
        assert_eq!(
            r.seat_fingerprint,
            auth::seat_fingerprint(OAuthProviderId::Anthropic, &seat.account_id)
        );
        let json = serde_json::to_string(&r).unwrap();
        assert!(
            !json.contains("b8a1448d-0000"),
            "full id must not be emitted: {json}"
        );
        assert!(!json.contains("detail"), "absent detail is omitted");
        // A seat without an email keeps the stored identity.
        let seat = auth::ResolvedSeat {
            account_id: "x".into(),
            identity: None,
        };
        let r = IdentifyRow::new(&base, "skipped")
            .with_seat(OAuthProviderId::Anthropic, &seat)
            .with_detail("dry-run: not written");
        assert_eq!(r.identity.as_deref(), Some("stale@example.com"));
        assert_eq!(r.detail.as_deref(), Some("dry-run: not written"));
    }
}
