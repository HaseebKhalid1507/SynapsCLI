//! `synaps auth list|use|remove` — operator management of stored OAuth
//! account slots. Reads labels and non-secret metadata only; never prints
//! or copies token material.

use std::io::{self, IsTerminal, Write};

use synaps_cli::auth::{
    self, Account, AccountSelector, AccountSummary, CredentialRef, OAuthProviderId,
};
use synaps_cli::config;

/// Options for `synaps auth list`.
#[derive(Debug, Clone, Default)]
pub struct ListOptions {
    pub provider: Option<String>,
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
    Ok(())
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
