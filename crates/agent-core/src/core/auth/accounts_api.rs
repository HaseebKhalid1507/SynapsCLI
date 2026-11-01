//! `pria.synaps.accounts/1`: the machine-readable accounts contract behind
//! `synaps auth capabilities|list|order|use|remove --json`.
//!
//! Replies are typed structs serialized compact, in declaration order, so
//! the bytes are stable and pinned by `tests/fixtures/accounts_v1/`. Error
//! replies carry a closed code per verb and never any prose.
//!
//! The `revision` is computed, never stored: the first 64 bits of a sha256
//! over every OAuth slot (storage key, seat fingerprint, added_at) and the
//! config that steers selection (`auth.account.*`, `auth.order.*`,
//! `auth.auto.strategy*`). Writers compare it under their locks:
//!
//! - `order`: config lock, then the auth.json lock; the revision is computed
//!   from the locked root plus a config read made under both, and the config
//!   write happens before either is released.
//! - `remove`: the same two locks; the check and the slot removal are one
//!   locked auth.json edit.
//! - `use`: the config lock plus an unlocked (atomic-rename) snapshot of
//!   auth.json; its enum has no `store_busy`, so it never waits on the store.
//!
//! Lock order is always config first, then auth.json; no other path takes
//! them the other way round.

use std::collections::BTreeMap;

use serde::Serialize;

use super::account::{Account, AccountSelector, AccountSummary, CredentialRef};
use super::auto_selection::AutoSelectionPolicy;
use super::provider::OAuthProviderId;
use super::quota_policy::Strategy;
use super::storage::{self, AccountInventory, StoreError};
use crate::config::{self, AuthConfig, ConfigWriteGuard};

/// `accounts_api` level advertised by `auth capabilities`.
pub const ACCOUNTS_API: u32 = 1;
/// `"v"` of every new reply.
pub const WIRE_VERSION: u32 = 1;
/// Providers `auth connect` supports (phase 1).
pub const CONNECT_PROVIDERS: [&str; 2] = ["anthropic", "openai-codex"];
pub const CONNECT_FLOWS: [&str; 1] = ["browser"];
pub const VERBS: [&str; 6] = ["capabilities", "list", "state", "order", "remove", "connect"];

const REVISION_DOMAIN: &[u8] = b"pria.synaps.accounts/1 revision\n";

// ── Wire types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Capabilities {
    pub v: u32,
    pub accounts_api: u32,
    pub connect: ConnectCapabilities,
    pub verbs: Vec<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConnectCapabilities {
    pub providers: Vec<&'static str>,
    pub flows: Vec<&'static str>,
}

pub fn capabilities() -> Capabilities {
    Capabilities {
        v: WIRE_VERSION,
        accounts_api: ACCOUNTS_API,
        connect: ConnectCapabilities {
            providers: CONNECT_PROVIDERS.to_vec(),
            flows: CONNECT_FLOWS.to_vec(),
        },
        verbs: VERBS.to_vec(),
    }
}

/// Closed error codes. Each verb uses a subset (see the contract, §2/§4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Conflict,
    OrderMismatch,
    NotFound,
    UnknownProvider,
    BadRequest,
    StoreBusy,
    ConfigUnwritable,
    RefusedProfile,
    StoreUnreadable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ErrorReply {
    pub v: u32,
    pub ok: bool,
    pub error: ErrorCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
}

impl ErrorReply {
    pub fn new(error: ErrorCode, revision: Option<String>) -> Self {
        Self {
            v: WIRE_VERSION,
            ok: false,
            error,
            revision,
        }
    }
}

/// Why a verb did not succeed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    /// A code of the verb's closed enum; `revision` only on `conflict`.
    Refused {
        code: ErrorCode,
        revision: Option<String>,
    },
    /// Nothing in the verb's enum fits (an unparseable store, an I/O error):
    /// the CLI prints no JSON, the text goes to stderr, exit 1.
    Failed(String),
}

impl ApiError {
    pub fn code(code: ErrorCode) -> Self {
        Self::Refused {
            code,
            revision: None,
        }
    }

    pub fn conflict(revision: String) -> Self {
        Self::Refused {
            code: ErrorCode::Conflict,
            revision: Some(revision),
        }
    }

    /// The JSON reply, or `None` for [`ApiError::Failed`].
    pub fn reply(&self) -> Option<ErrorReply> {
        match self {
            Self::Refused { code, revision } => Some(ErrorReply::new(*code, revision.clone())),
            Self::Failed(_) => None,
        }
    }

    fn from_store(err: StoreError) -> Self {
        match err {
            StoreError::Busy(_) => Self::code(ErrorCode::StoreBusy),
            other => Self::Failed(String::from(other)),
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused { code, .. } => {
                let code = serde_json::to_value(code).unwrap_or_default();
                write!(f, "{}", code.as_str().unwrap_or("refused"))
            }
            Self::Failed(message) => f.write_str(message),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OrderReply {
    pub v: u32,
    pub ok: bool,
    pub provider: String,
    pub order: Vec<String>,
    pub strategy: Strategy,
    pub revision: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RemovedSlot {
    pub provider: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RemoveReply {
    pub v: u32,
    pub ok: bool,
    pub removed: RemovedSlot,
    pub revision: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SelectorReply {
    Auto,
    Account { account: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UseReply {
    pub v: u32,
    pub ok: bool,
    pub provider: String,
    pub selector: SelectorReply,
    pub revision: String,
}

/// One compact JSON document (no trailing newline).
pub fn to_line<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("accounts wire types always serialize")
}

// ── Revision ────────────────────────────────────────────────────────────────

/// One OAuth slot as the revision sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevisionSlot {
    pub key: String,
    pub seat_fingerprint: Option<String>,
    pub added_at: Option<u64>,
}

impl RevisionSlot {
    pub fn from_summary(row: &AccountSummary) -> Option<Self> {
        Some(Self {
            key: row.credential_ref()?.storage_key(),
            seat_fingerprint: row.seat_fingerprint.clone(),
            added_at: row.added_at,
        })
    }
}

/// The slots of listed rows (every provider's, for a store-wide revision).
pub fn revision_slots(rows: &[AccountSummary]) -> Vec<RevisionSlot> {
    rows.iter().filter_map(RevisionSlot::from_summary).collect()
}

/// `revision` (contract §0): 16 lowercase hex characters, the first 64 bits
/// of sha256 over a domain tag and the compact JSON of
/// `[[ [key, fingerprint|null, added_at|null], ... sorted by key ],
///   {auth.account.*}, {auth.order.*}, {auth.auto.strategy and strategy.*}]`
/// (config values raw, maps in key order). The global `auth.auto.strategy`
/// is included because it changes the effective strategy of every provider
/// without a per-provider override.
pub fn revision(slots: &[RevisionSlot], auth: &AuthConfig) -> String {
    use sha2::{Digest, Sha256};
    let mut slots: Vec<&RevisionSlot> = slots.iter().collect();
    slots.sort_by(|a, b| a.key.cmp(&b.key));
    let slots: Vec<serde_json::Value> = slots
        .iter()
        .map(|s| serde_json::json!([s.key, s.seat_fingerprint, s.added_at]))
        .collect();
    let strategy: BTreeMap<&String, &String> = auth
        .auto
        .iter()
        .filter(|(key, _)| key.as_str() == "strategy" || key.starts_with("strategy."))
        .collect();
    let canonical = serde_json::json!([slots, auth.accounts, auth.order, strategy]).to_string();
    let mut hasher = Sha256::new();
    hasher.update(REVISION_DOMAIN);
    hasher.update(canonical.as_bytes());
    hasher.finalize()[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The store-wide revision of listed rows.
pub fn revision_of_rows(rows: &[AccountSummary], auth: &AuthConfig) -> String {
    revision(&revision_slots(rows), auth)
}

/// Exactly 16 lowercase hex characters.
pub fn is_revision(raw: &str) -> bool {
    raw.len() == 16 && raw.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

// ── Order and strategy ──────────────────────────────────────────────────────

/// `auth.order.<provider>` parsed leniently: comma separated, trimmed,
/// blanks dropped, duplicates keep their first occurrence. Unknown labels
/// stay here and are dropped by [`effective_order`].
pub fn parse_order(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for label in raw.split(',').map(str::trim).filter(|l| !l.is_empty()) {
        if !out.iter().any(|seen| seen == label) {
            out.push(label.to_string());
        }
    }
    out
}

/// Configured labels that exist in `listed`, then the rest of `listed` in
/// its own order (the store listing: default first, then alphabetical).
pub fn effective_order(configured: &[String], listed: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(listed.len());
    for label in configured.iter().chain(listed) {
        if listed.contains(label) && !out.contains(label) {
            out.push(label.clone());
        }
    }
    out
}

/// The configured (parsed) order for `provider`, as the broker reads it.
pub fn configured_order(auth: &AuthConfig, provider: OAuthProviderId) -> Vec<String> {
    AutoSelectionPolicy::default()
        .with_order_config(&auth.order)
        .order
        .remove(&provider)
        .unwrap_or_default()
}

/// The strategy the broker applies to `provider` under this config.
pub fn effective_strategy(auth: &AuthConfig, provider: OAuthProviderId) -> Strategy {
    AutoSelectionPolicy::from_config_map(&auth.auto)
        .0
        .strategy_for(provider)
}

/// Labels of `provider` among listed rows, in listing order.
pub fn labels_of(rows: &[AccountSummary], provider: OAuthProviderId) -> Vec<String> {
    rows.iter()
        .filter(|r| r.provider == provider.as_str())
        .map(|r| r.label.clone())
        .collect()
}

/// The `order` and `strategy` maps of `auth list --json`: one entry per
/// provider that has at least one listed row.
pub fn list_order_and_strategy(
    rows: &[AccountSummary],
    auth: &AuthConfig,
) -> (BTreeMap<String, Vec<String>>, BTreeMap<String, Strategy>) {
    let mut order = BTreeMap::new();
    let mut strategy = BTreeMap::new();
    for descriptor in super::provider::DESCRIPTORS.iter() {
        let listed = labels_of(rows, descriptor.id);
        if listed.is_empty() {
            continue;
        }
        let key = descriptor.id.as_str().to_string();
        order.insert(
            key.clone(),
            effective_order(&configured_order(auth, descriptor.id), &listed),
        );
        strategy.insert(key, effective_strategy(auth, descriptor.id));
    }
    (order, strategy)
}

// ── Local store reads ───────────────────────────────────────────────────────

/// The local listing for `auth list`: the (provider-filtered) inventory and
/// the store-wide revision, from ONE unlocked read of auth.json.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalListing {
    pub inventory: AccountInventory,
    pub revision: String,
}

pub fn local_listing(
    provider: Option<OAuthProviderId>,
    auth: &AuthConfig,
) -> Result<LocalListing, String> {
    let root = storage::read_root_at(&storage::auth_file_path())?;
    let all = storage::inventory_from_root(&root, None);
    let revision = revision_of_rows(&all.accounts, auth);
    let inventory = match provider {
        None => all,
        Some(p) => storage::inventory_from_root(&root, Some(p)),
    };
    Ok(LocalListing {
        inventory,
        revision,
    })
}

fn rows_of(root: &serde_json::Map<String, serde_json::Value>) -> Vec<AccountSummary> {
    storage::inventory_from_root(root, None).accounts
}

// ── Argument checks shared by the verbs ─────────────────────────────────────

/// Canonical provider id (CLI aliases accepted), else `unknown_provider`.
pub fn parse_provider(raw: &str) -> Result<OAuthProviderId, ApiError> {
    super::provider::parse_cli_provider(raw).map_err(|_| ApiError::code(ErrorCode::UnknownProvider))
}

/// A slot label (`default` or the label grammar), else `bad_request`.
pub fn parse_account(raw: &str) -> Result<Account, ApiError> {
    Account::parse(raw).map_err(|_| ApiError::code(ErrorCode::BadRequest))
}

/// `--expect` / `--expect-rev` must look like a revision, else `bad_request`.
pub fn check_expect(expect: Option<&str>) -> Result<(), ApiError> {
    match expect {
        Some(rev) if !is_revision(rev) => Err(ApiError::code(ErrorCode::BadRequest)),
        _ => Ok(()),
    }
}

/// `--set L1,L2,...`: every item must be a slot label, else `bad_request`.
pub fn parse_set(raw: &str) -> Result<Vec<String>, ApiError> {
    raw.split(',')
        .map(|item| parse_account(item).map(|a| a.label_str().to_string()))
        .collect()
}

fn matches_expect(expect: Option<&str>, current: &str) -> Result<(), ApiError> {
    match expect {
        Some(rev) if rev != current => Err(ApiError::conflict(current.to_string())),
        _ => Ok(()),
    }
}

/// True when `set` names exactly `labels`, each once.
fn is_permutation(set: &[String], labels: &[String]) -> bool {
    let mut a: Vec<&String> = set.iter().collect();
    let mut b: Vec<&String> = labels.iter().collect();
    a.sort();
    b.sort();
    a == b
}

fn order_reply(
    provider: OAuthProviderId,
    rows: &[AccountSummary],
    auth: &AuthConfig,
) -> OrderReply {
    OrderReply {
        v: WIRE_VERSION,
        ok: true,
        provider: provider.as_str().to_string(),
        order: effective_order(&configured_order(auth, provider), &labels_of(rows, provider)),
        strategy: effective_strategy(auth, provider),
        revision: revision_of_rows(rows, auth),
    }
}

// ── Verbs ───────────────────────────────────────────────────────────────────

/// `auth order --provider P --json [--expect REV]` (read). Unlocked, like
/// `list`; a given `--expect` that differs answers `conflict`.
pub fn order_read(provider: OAuthProviderId, expect: Option<&str>) -> Result<OrderReply, ApiError> {
    check_expect(expect)?;
    let auth = config::read_auth_config();
    let root = storage::read_root_at(&storage::auth_file_path()).map_err(ApiError::Failed)?;
    let reply = order_reply(provider, &rows_of(&root), &auth);
    matches_expect(expect, &reply.revision)?;
    Ok(reply)
}

/// What an `auth order` write changes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OrderWrite {
    /// `--set`, already through [`parse_set`].
    pub set: Option<Vec<String>>,
    /// `--strict` (`preference_order`) or `--tie-break` (`soonest_reset`).
    pub strategy: Option<Strategy>,
}

/// `auth order ... --set/--strict/--tie-break [--expect REV]`. Under the
/// config lock and then the auth.json lock: compute the revision from the
/// locked root and a config read, compare, check the permutation, write
/// every config key in ONE atomic replace, then answer with the new
/// revision. auth.json itself is never written.
pub fn order_write(
    provider: OAuthProviderId,
    change: &OrderWrite,
    expect: Option<&str>,
) -> Result<OrderReply, ApiError> {
    check_expect(expect)?;
    let guard = ConfigWriteGuard::acquire().map_err(|_| ApiError::code(ErrorCode::ConfigUnwritable))?;
    let path = storage::auth_file_path();
    let outcome = storage::with_locked_root_typed(&path, false, storage::STORE_LOCK_WAIT, |root| {
        // auth.json is only read here: the edit never asks for a write.
        Ok((false, decide_order_write(&guard, &rows_of(root), provider, change, expect)))
    });
    outcome.map_err(ApiError::from_store)?
}

/// The body of [`order_write`], run with both locks held.
fn decide_order_write(
    guard: &ConfigWriteGuard,
    rows: &[AccountSummary],
    provider: OAuthProviderId,
    change: &OrderWrite,
    expect: Option<&str>,
) -> Result<OrderReply, ApiError> {
    matches_expect(expect, &revision_of_rows(rows, &guard.read_auth()))?;
    if let Some(set) = &change.set {
        if !is_permutation(set, &labels_of(rows, provider)) {
            return Err(ApiError::code(ErrorCode::OrderMismatch));
        }
    }
    let order_key = format!("auth.order.{}", provider.as_str());
    let order_value = change.set.as_ref().map(|set| set.join(","));
    let strategy_key = format!("auth.auto.strategy.{}", provider.as_str());
    let mut pairs: Vec<(&str, &str)> = Vec::new();
    if let Some(value) = &order_value {
        pairs.push((order_key.as_str(), value.as_str()));
    }
    if let Some(strategy) = change.strategy {
        pairs.push((strategy_key.as_str(), strategy_config_value(strategy)));
    }
    if !pairs.is_empty() {
        guard
            .write_values(&pairs)
            .map_err(|_| ApiError::code(ErrorCode::ConfigUnwritable))?;
    }
    Ok(order_reply(provider, rows, &guard.read_auth()))
}

/// The config value `--strict` / `--tie-break` writes.
pub fn strategy_config_value(strategy: Strategy) -> &'static str {
    match strategy {
        Strategy::PreferenceOrder => "preference_order",
        Strategy::LowestUtilization => "lowest_utilization",
        Strategy::SoonestReset => "soonest_reset",
    }
}

/// `auth remove --provider P --account L -y --json [--expect-rev REV]`.
///
/// Refuses an inherited store (`refused_profile`, as `remove_credential`).
/// Under the config lock and then the auth.json lock: compare the revision,
/// require an OAuth slot at the key (`not_found`), remove it and write, in
/// one locked edit; a refusal writes nothing. The parked-refresh sidecar is
/// removed after, like the plain remove.
pub fn remove(cred: &CredentialRef, expect: Option<&str>) -> Result<RemoveReply, ApiError> {
    check_expect(expect)?;
    let read_path = storage::auth_file_path();
    let write_path = config::resolve_write_path("auth.json");
    if read_path != write_path {
        return Err(ApiError::code(ErrorCode::RefusedProfile));
    }
    // Held so the config half of the revision cannot move mid-check.
    let guard = ConfigWriteGuard::acquire()
        .map_err(|e| ApiError::Failed(format!("failed to lock the config: {e}")))?;
    let key = cred.storage_key();
    let outcome = storage::with_locked_root_typed(&write_path, false, storage::STORE_LOCK_WAIT, |root| {
        let auth = guard.read_auth();
        if let Err(refused) = matches_expect(expect, &revision_of_rows(&rows_of(root), &auth)) {
            return Ok((false, Err(refused)));
        }
        let is_oauth_slot = root
            .get(&key)
            .and_then(|entry| entry.get("type"))
            .and_then(|t| t.as_str())
            == Some("oauth");
        if !is_oauth_slot {
            return Ok((false, Err(ApiError::code(ErrorCode::NotFound))));
        }
        root.remove(&key);
        Ok((true, Ok(revision_of_rows(&rows_of(root), &auth))))
    });
    let revision = outcome.map_err(ApiError::from_store)??;
    drop(guard);
    let _ = std::fs::remove_file(storage::refresh_pending_path(&write_path, &key));
    Ok(RemoveReply {
        v: WIRE_VERSION,
        ok: true,
        removed: RemovedSlot {
            provider: cred.provider.as_str().to_string(),
            label: cred.account.label_str().to_string(),
        },
        revision,
    })
}

/// `auth use --provider P --account L|default|auto --json [--expect REV]`.
///
/// Under the config lock with an unlocked snapshot of auth.json: compare the
/// revision, validate like the plain `use` (`not_found` for a missing slot
/// or `auto` with no slot, `bad_request` for `auto` without a usage
/// adapter), write `auth.account.<P>`, answer with the new revision. A
/// key with more than one config line is `config_unwritable`, nothing written.
pub fn use_selector(
    provider: OAuthProviderId,
    selector: &AccountSelector,
    expect: Option<&str>,
) -> Result<UseReply, ApiError> {
    check_expect(expect)?;
    let guard = ConfigWriteGuard::acquire().map_err(|_| ApiError::code(ErrorCode::ConfigUnwritable))?;
    let root = storage::read_root_at(&storage::auth_file_path()).map_err(ApiError::Failed)?;
    let rows = rows_of(&root);
    matches_expect(expect, &revision_of_rows(&rows, &guard.read_auth()))?;
    let labels = labels_of(&rows, provider);
    let reply_selector = match selector {
        AccountSelector::Account(account) => {
            if !labels.iter().any(|l| l == account.label_str()) {
                return Err(ApiError::code(ErrorCode::NotFound));
            }
            SelectorReply::Account {
                account: account.label_str().to_string(),
            }
        }
        AccountSelector::Auto => {
            if labels.is_empty() {
                return Err(ApiError::code(ErrorCode::NotFound));
            }
            if !super::usage::supports_usage(provider) {
                return Err(ApiError::code(ErrorCode::BadRequest));
            }
            SelectorReply::Auto
        }
        AccountSelector::Invalid { .. } => return Err(ApiError::code(ErrorCode::BadRequest)),
    };
    let key = super::account::account_config_key(provider);
    let value = selector.as_config_value();
    guard
        .write_values_no_duplicates(&[(key.as_str(), value.as_str())])
        .map_err(|_| ApiError::code(ErrorCode::ConfigUnwritable))?;
    let revision = revision_of_rows(&rows, &guard.read_auth());
    Ok(UseReply {
        v: WIRE_VERSION,
        ok: true,
        provider: provider.as_str().to_string(),
        selector: reply_selector,
        revision,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    const CAPABILITIES: &str = r#"{"v":1,"accounts_api":1,"connect":{"providers":["anthropic","openai-codex"],"flows":["browser"]},"verbs":["capabilities","list","state","order","remove","connect"]}"#;

    #[test]
    fn capabilities_bytes_are_the_contract() {
        assert_eq!(to_line(&capabilities()), CAPABILITIES);
        let fixture = include_str!("../../../../../tests/fixtures/accounts_v1/capabilities.json");
        assert_eq!(fixture, format!("{CAPABILITIES}\n"));
    }

    #[test]
    fn error_and_reply_shapes() {
        assert_eq!(
            to_line(&ApiError::conflict("0123456789abcdef".into()).reply().unwrap()),
            r#"{"v":1,"ok":false,"error":"conflict","revision":"0123456789abcdef"}"#
        );
        for (code, text) in [
            (ErrorCode::OrderMismatch, "order_mismatch"),
            (ErrorCode::NotFound, "not_found"),
            (ErrorCode::UnknownProvider, "unknown_provider"),
            (ErrorCode::BadRequest, "bad_request"),
            (ErrorCode::StoreBusy, "store_busy"),
            (ErrorCode::ConfigUnwritable, "config_unwritable"),
            (ErrorCode::RefusedProfile, "refused_profile"),
            (ErrorCode::StoreUnreadable, "store_unreadable"),
        ] {
            assert_eq!(
                to_line(&ApiError::code(code).reply().unwrap()),
                format!(r#"{{"v":1,"ok":false,"error":"{text}"}}"#)
            );
            assert_eq!(ApiError::code(code).to_string(), text);
        }
        assert_eq!(ApiError::Failed("x".into()).reply(), None);
        let selector = |s: SelectorReply| {
            to_line(&UseReply {
                v: 1,
                ok: true,
                provider: "anthropic".into(),
                selector: s,
                revision: "0123456789abcdef".into(),
            })
        };
        assert_eq!(
            selector(SelectorReply::Auto),
            r#"{"v":1,"ok":true,"provider":"anthropic","selector":{"kind":"auto"},"revision":"0123456789abcdef"}"#
        );
        assert_eq!(
            selector(SelectorReply::Account {
                account: "claude2".into()
            }),
            r#"{"v":1,"ok":true,"provider":"anthropic","selector":{"kind":"account","account":"claude2"},"revision":"0123456789abcdef"}"#
        );
    }

    fn slot(key: &str, fp: Option<&str>, added_at: Option<u64>) -> RevisionSlot {
        RevisionSlot {
            key: key.into(),
            seat_fingerprint: fp.map(str::to_string),
            added_at,
        }
    }

    #[test]
    fn revision_changes_on_each_input_and_is_stable_otherwise() {
        let slots = vec![
            slot("anthropic", Some("f1"), Some(10)),
            slot("anthropic@claude2", None, Some(20)),
        ];
        let auth = AuthConfig::default();
        let base = revision(&slots, &auth);
        assert!(is_revision(&base), "{base}");
        // Stable: same inputs, any slot order, unrelated config.
        let mut reversed = slots.clone();
        reversed.reverse();
        assert_eq!(revision(&reversed, &auth), base);
        let mut unrelated = AuthConfig::default();
        unrelated.auto.insert("sticky".into(), "false".into());
        unrelated.auto.insert("urgent_horizon_hours".into(), "4".into());
        unrelated.remote_endpoint = Some("https://x".into());
        assert_eq!(revision(&slots, &unrelated), base);

        let mut seen = std::collections::BTreeSet::from([base.clone()]);
        let mut changed = |rev: String, what: &str| {
            assert!(is_revision(&rev), "{rev}");
            assert!(seen.insert(rev), "revision did not change on {what}");
        };
        // Add, remove, re-login to another seat, re-login at another time.
        let mut added = slots.clone();
        added.push(slot("openai-codex", None, None));
        changed(revision(&added, &auth), "add");
        changed(revision(&slots[..1], &auth), "remove");
        let mut seat = slots.clone();
        seat[0].seat_fingerprint = Some("f2".into());
        changed(revision(&seat, &auth), "seat");
        let mut no_seat = slots.clone();
        no_seat[0].seat_fingerprint = None;
        changed(revision(&no_seat, &auth), "seat cleared");
        let mut login = slots.clone();
        login[1].added_at = Some(21);
        changed(revision(&login, &auth), "added_at");
        // Selector, order, strategy (per provider and global).
        let mut selector = AuthConfig::default();
        selector.accounts.insert("anthropic".into(), "auto".into());
        changed(revision(&slots, &selector), "selector");
        let mut order = AuthConfig::default();
        order.order.insert("anthropic".into(), "claude2,default".into());
        changed(revision(&slots, &order), "order");
        let mut strategy = AuthConfig::default();
        strategy
            .auto
            .insert("strategy.anthropic".into(), "preference_order".into());
        changed(revision(&slots, &strategy), "strategy override");
        let mut global = AuthConfig::default();
        global.auto.insert("strategy".into(), "lowest_utilization".into());
        changed(revision(&slots, &global), "global strategy");
    }

    #[test]
    fn order_parsing_is_lenient_and_effective_order_fills_the_rest() {
        assert_eq!(parse_order(" b, a ,,b, zz "), vec!["b", "a", "zz"]);
        assert!(parse_order("").is_empty());
        let listed: Vec<String> = ["default", "a", "b"].map(String::from).to_vec();
        assert_eq!(
            effective_order(&parse_order("b,zz,b"), &listed),
            vec!["b", "default", "a"]
        );
        assert_eq!(effective_order(&[], &listed), listed);
        assert!(effective_order(&parse_order("a"), &[]).is_empty());
        assert!(is_revision("0123456789abcdef"));
        for bad in ["0123456789ABCDEF", "0123456789abcde", "0123456789abcdefa", "0123456789abcdeg"] {
            assert!(!is_revision(bad), "{bad}");
        }
        assert_eq!(parse_set("claude2, default").unwrap(), vec!["claude2", "default"]);
        for bad in ["", "a,,b", "Bad", "auto", "a,"] {
            assert_eq!(
                parse_set(bad),
                Err(ApiError::code(ErrorCode::BadRequest)),
                "{bad:?}"
            );
        }
    }

    // ── CAS against a temp base dir ─────────────────────────────────────────

    /// HOME and SYNAPS_BASE_DIR pinned to a fresh temp dir for one test.
    struct Sandbox {
        dir: tempfile::TempDir,
        saved: Vec<(&'static str, Option<String>)>,
    }

    impl Sandbox {
        fn new(auth_json: &str, config: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let base = dir.path().join(".synaps-cli");
            std::fs::create_dir_all(&base).unwrap();
            std::fs::write(base.join("auth.json"), auth_json).unwrap();
            std::fs::write(base.join("config"), config).unwrap();
            let saved = ["HOME", "SYNAPS_BASE_DIR", "SYNAPS_CONFIG_LOCK"]
                .into_iter()
                .map(|k| (k, std::env::var(k).ok()))
                .collect();
            std::env::set_var("HOME", dir.path());
            std::env::set_var("SYNAPS_BASE_DIR", &base);
            std::env::remove_var("SYNAPS_CONFIG_LOCK");
            Self { dir, saved }
        }

        fn base(&self) -> std::path::PathBuf {
            self.dir.path().join(".synaps-cli")
        }

        fn auth_bytes(&self) -> Vec<u8> {
            std::fs::read(self.base().join("auth.json")).unwrap()
        }

        fn config_text(&self) -> String {
            std::fs::read_to_string(self.base().join("config")).unwrap()
        }

        fn revision(&self) -> String {
            local_listing(None, &config::read_auth_config()).unwrap().revision
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            for (key, value) in &self.saved {
                match value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    const STORE: &str = r#"{
  "anthropic": {"type": "oauth", "access": "a0", "refresh": "r0", "expires": 1, "accountId": "seat-0", "addedAt": 100},
  "anthropic@claude2": {"type": "oauth", "access": "a2", "refresh": "r2", "expires": 2, "addedAt": 200},
  "anthropic@claude3": {"type": "oauth", "access": "a3", "refresh": "r3", "expires": 3, "accountId": "seat-3", "addedAt": 300},
  "muse": {"type": "oauth", "access": "m", "refresh": "m", "expires": 4}
}"#;

    fn anthropic() -> OAuthProviderId {
        OAuthProviderId::Anthropic
    }

    #[test]
    #[serial]
    fn order_read_reports_effective_order_strategy_and_revision() {
        let sb = Sandbox::new(STORE, "auth.order.anthropic = claude3, ghost\n");
        let reply = order_read(anthropic(), None).unwrap();
        assert_eq!(reply.order, vec!["claude3", "default", "claude2"]);
        assert_eq!(reply.strategy, Strategy::SoonestReset);
        assert_eq!(reply.revision, sb.revision());
        assert_eq!(order_read(anthropic(), Some(&reply.revision)).unwrap(), reply);
        assert_eq!(
            order_read(anthropic(), Some("0000000000000000")),
            Err(ApiError::conflict(reply.revision.clone()))
        );
        assert_eq!(
            order_read(anthropic(), Some("nope")),
            Err(ApiError::code(ErrorCode::BadRequest))
        );
        let muse = order_read(OAuthProviderId::Muse, None).unwrap();
        assert_eq!(muse.order, vec!["default"]);
        assert_eq!(muse.revision, reply.revision, "the revision is store-wide");
    }

    #[test]
    #[serial]
    fn order_write_checks_expect_and_permutation_and_writes_atomically() {
        let sb = Sandbox::new(STORE, "model = m\n");
        let rev = sb.revision();
        let set = |labels: &str| OrderWrite {
            set: Some(parse_set(labels).unwrap()),
            strategy: None,
        };
        // Stale --expect: nothing written, current revision returned.
        assert_eq!(
            order_write(anthropic(), &set("claude2,default,claude3"), Some("0000000000000000")),
            Err(ApiError::conflict(rev.clone()))
        );
        // Not a permutation: missing, extra, duplicate.
        for bad in ["claude2,default", "claude2,default,claude3,ghost", "claude2,claude2,default"] {
            assert_eq!(
                order_write(anthropic(), &set(bad), Some(&rev)),
                Err(ApiError::code(ErrorCode::OrderMismatch)),
                "{bad}"
            );
        }
        assert_eq!(sb.config_text(), "model = m\n");

        let reply = order_write(
            anthropic(),
            &OrderWrite {
                set: Some(parse_set("claude2,default,claude3").unwrap()),
                strategy: Some(Strategy::PreferenceOrder),
            },
            Some(&rev),
        )
        .unwrap();
        assert_eq!(reply.order, vec!["claude2", "default", "claude3"]);
        assert_eq!(reply.strategy, Strategy::PreferenceOrder);
        assert_ne!(reply.revision, rev);
        assert_eq!(reply.revision, sb.revision());
        assert_eq!(
            sb.config_text(),
            "model = m\nauth.order.anthropic = claude2,default,claude3\n\
             auth.auto.strategy.anthropic = preference_order\n"
        );
        // The old revision is now stale.
        assert_eq!(
            order_write(anthropic(), &set("default,claude2,claude3"), Some(&rev)),
            Err(ApiError::conflict(reply.revision.clone()))
        );
        // --tie-break alone flips the strategy back and keeps the order.
        let tie = order_write(
            anthropic(),
            &OrderWrite {
                set: None,
                strategy: Some(Strategy::SoonestReset),
            },
            Some(&reply.revision),
        )
        .unwrap();
        assert_eq!(tie.order, reply.order);
        assert_eq!(tie.strategy, Strategy::SoonestReset);
        assert_ne!(tie.revision, reply.revision);
        assert!(sb
            .config_text()
            .contains("auth.auto.strategy.anthropic = soonest_reset\n"));
        // The broker's policy reads the same order.
        let cfg = config::load_config();
        let policy = AutoSelectionPolicy::from_config_map(&cfg.auth.auto)
            .0
            .with_order_config(&cfg.auth.order);
        assert_eq!(
            policy.preference_for(anthropic(), &labels_of(&rows_of(&storage::read_root_at(&sb.base().join("auth.json")).unwrap()), anthropic())),
            vec!["claude2", "default", "claude3"]
        );
        // auth.json is never written by order.
        assert_eq!(sb.auth_bytes(), STORE.as_bytes());
    }

    #[test]
    #[serial]
    fn remove_conflict_and_not_found_leave_the_store_byte_identical() {
        let sb = Sandbox::new(STORE, "");
        let rev = sb.revision();
        let claude2 = CredentialRef::new(anthropic(), Account::parse("claude2").unwrap());
        assert_eq!(
            remove(&claude2, Some("0000000000000000")),
            Err(ApiError::conflict(rev.clone()))
        );
        assert_eq!(sb.auth_bytes(), STORE.as_bytes());
        let ghost = CredentialRef::new(anthropic(), Account::parse("ghost").unwrap());
        assert_eq!(remove(&ghost, Some(&rev)), Err(ApiError::code(ErrorCode::NotFound)));
        assert_eq!(sb.auth_bytes(), STORE.as_bytes());
        assert_eq!(remove(&claude2, Some("bad")), Err(ApiError::code(ErrorCode::BadRequest)));

        let sidecar = storage::refresh_pending_path(&sb.base().join("auth.json"), "anthropic@claude2");
        std::fs::write(&sidecar, "parked").unwrap();
        let reply = remove(&claude2, Some(&rev)).unwrap();
        assert_eq!(
            reply.removed,
            RemovedSlot {
                provider: "anthropic".into(),
                label: "claude2".into()
            }
        );
        assert_eq!(reply.revision, sb.revision());
        assert_ne!(reply.revision, rev);
        assert!(!sidecar.exists());
        let root = storage::read_root_at(&sb.base().join("auth.json")).unwrap();
        assert!(!root.contains_key("anthropic@claude2"));
        assert!(root.contains_key("anthropic@claude3") && root.contains_key("muse"));
        // Without --expect-rev the removal is unconditional; a second one is not_found.
        assert_eq!(remove(&claude2, None), Err(ApiError::code(ErrorCode::NotFound)));
    }

    #[test]
    #[serial]
    fn use_checks_expect_then_validates_then_writes() {
        let sb = Sandbox::new(STORE, "auth.account.anthropic = default\n");
        let rev = sb.revision();
        let claude3 = AccountSelector::parse("claude3").unwrap();
        assert_eq!(
            use_selector(anthropic(), &claude3, Some("0000000000000000")),
            Err(ApiError::conflict(rev.clone()))
        );
        assert_eq!(
            use_selector(anthropic(), &AccountSelector::parse("ghost").unwrap(), Some(&rev)),
            Err(ApiError::code(ErrorCode::NotFound))
        );
        assert_eq!(
            use_selector(OAuthProviderId::OpenAiCodex, &AccountSelector::Auto, None),
            Err(ApiError::code(ErrorCode::NotFound))
        );
        assert_eq!(
            use_selector(OAuthProviderId::Muse, &AccountSelector::Auto, None),
            Err(ApiError::code(ErrorCode::BadRequest)),
            "auto needs a usage adapter"
        );
        assert_eq!(sb.config_text(), "auth.account.anthropic = default\n");

        let reply = use_selector(anthropic(), &claude3, Some(&rev)).unwrap();
        assert_eq!(
            reply.selector,
            SelectorReply::Account {
                account: "claude3".into()
            }
        );
        assert_eq!(sb.config_text(), "auth.account.anthropic = claude3\n");
        assert_eq!(reply.revision, sb.revision());
        assert_ne!(reply.revision, rev);
        let auto = use_selector(anthropic(), &AccountSelector::Auto, Some(&reply.revision)).unwrap();
        assert_eq!(auto.selector, SelectorReply::Auto);
        assert_eq!(sb.config_text(), "auth.account.anthropic = auto\n");
        assert_eq!(
            use_selector(anthropic(), &claude3, Some(&reply.revision)),
            Err(ApiError::conflict(auto.revision.clone()))
        );
        assert_eq!(sb.auth_bytes(), STORE.as_bytes());
    }

    /// A key with two config lines (a write replaces the first, the parser
    /// reads the last) is refused in either order, with nothing written.
    #[test]
    #[serial]
    fn use_refuses_a_key_with_two_lines_and_writes_nothing() {
        let claude3 = AccountSelector::parse("claude3").unwrap();
        for config in [
            "auth.account.anthropic = claude2\nauth.account.anthropic =\n",
            "auth.account.anthropic =\n# x\nauth.account.anthropic = claude2\n",
        ] {
            let sb = Sandbox::new(STORE, config);
            assert_eq!(
                use_selector(anthropic(), &claude3, None),
                Err(ApiError::code(ErrorCode::ConfigUnwritable))
            );
            assert_eq!(sb.config_text(), config);
            assert_eq!(sb.auth_bytes(), STORE.as_bytes());
        }
    }

    #[test]
    #[serial]
    fn list_extras_cover_every_provider_with_a_slot() {
        let _sb = Sandbox::new(
            STORE,
            "auth.order.anthropic = claude2\nauth.auto.strategy.anthropic = preference_order\nauth.auto.strategy = lowest_utilization\n",
        );
        let auth = config::read_auth_config();
        let listing = local_listing(None, &auth).unwrap();
        let (order, strategy) = list_order_and_strategy(&listing.inventory.accounts, &auth);
        assert_eq!(
            order,
            BTreeMap::from([
                ("anthropic".to_string(), vec!["claude2".to_string(), "default".into(), "claude3".into()]),
                ("muse".to_string(), vec!["default".to_string()]),
            ])
        );
        assert_eq!(
            strategy,
            BTreeMap::from([
                ("anthropic".to_string(), Strategy::PreferenceOrder),
                ("muse".to_string(), Strategy::LowestUtilization),
            ])
        );
        // A provider filter narrows the rows, never the revision.
        let only_muse = local_listing(Some(OAuthProviderId::Muse), &auth).unwrap();
        assert_eq!(only_muse.inventory.accounts.len(), 1);
        assert_eq!(only_muse.revision, listing.revision);
    }
}
