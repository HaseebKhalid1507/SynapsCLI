use std::path::{Path, PathBuf};

use super::account::{Account, AccountSummary, CredentialRef};
use super::provider::OAuthProviderId;
use super::{AuthFile, OAuthCredentials};

/// Get the path to auth.json (~/.synaps-cli/auth.json).
pub fn auth_file_path() -> PathBuf {
    crate::config::resolve_read_path("auth.json")
}

/// Load credentials from auth.json.
pub fn load_auth() -> std::result::Result<Option<AuthFile>, String> {
    let path = auth_file_path();
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;
    let auth: AuthFile = serde_json::from_str(&content)
        .map_err(|e| format!("Failed to parse {}: {}", path.display(), e))?;
    Ok(Some(auth))
}

/// Load one provider's OAuth credential from auth.json.
///
/// `provider` is a storage key: the bare provider id for the default account
/// or `<provider>@<label>` for a named account (see [`CredentialRef`]).
pub fn load_provider_auth(provider: &str) -> std::result::Result<Option<OAuthCredentials>, String> {
    load_provider_auth_at(&auth_file_path(), provider)
}

pub(crate) fn load_provider_auth_at(
    path: &Path,
    provider: &str,
) -> std::result::Result<Option<OAuthCredentials>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;
    let value: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| format!("Failed to parse {}: {}", path.display(), e))?;
    let Some(raw) = value.get(provider) else {
        return Ok(None);
    };
    let creds: OAuthCredentials = serde_json::from_value(raw.clone())
        .map_err(|e| format!("Failed to parse {} credential: {}", provider, e))?;
    Ok(Some(creds))
}

// ── Account-addressed API (additive; bare keys are the `default` account) ────

/// Load the credential stored in one account slot.
pub fn load_credential(
    cred: &CredentialRef,
) -> std::result::Result<Option<OAuthCredentials>, String> {
    load_provider_auth(&cred.storage_key())
}

/// Persist a credential into exactly one account slot. Other slots (including
/// the provider's default slot) and any non-secret metadata already stored in
/// this slot are preserved. A corrupt store is an error — the account path
/// never resets the file (that would wipe every other slot); only the legacy
/// single-credential [`save_provider_auth`] keeps the recovery behaviour.
pub fn save_credential(
    cred: &CredentialRef,
    creds: &OAuthCredentials,
) -> std::result::Result<(), String> {
    let path = crate::config::resolve_write_path("auth.json");
    save_credential_at(&path, cred, creds)
}

pub(crate) fn save_credential_at(
    path: &Path,
    cred: &CredentialRef,
    creds: &OAuthCredentials,
) -> std::result::Result<(), String> {
    let encoded =
        serde_json::to_value(creds).map_err(|e| format!("Failed to serialize auth: {}", e))?;
    let fields = encoded
        .as_object()
        .cloned()
        .ok_or_else(|| "credential did not serialize to an object".to_string())?;
    let key = cred.storage_key();
    with_locked_root(path, false, |root| {
        let mut slot = root
            .remove(&key)
            .filter(serde_json::Value::is_object)
            .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
        if let Some(target) = slot.as_object_mut() {
            for (k, v) in &fields {
                target.insert(k.clone(), v.clone());
            }
        }
        root.insert(key.clone(), slot);
        Ok((true, ()))
    })
}

/// Remove one account slot. Returns `true` if a key was removed. Never leaves
/// a partial file and never touches any other key. A corrupt file is an error
/// (nothing is reset on a removal path).
///
/// Profiles: listings and loads read through [`auth_file_path`], which falls
/// back to the base `auth.json` when the active profile has none. Removal
/// refuses to act on such an *inherited* file rather than silently
/// "succeeding" against an empty profile file or deleting from the base
/// store on the profile's behalf.
pub fn remove_credential(cred: &CredentialRef) -> std::result::Result<bool, String> {
    let read_path = auth_file_path();
    let write_path = crate::config::resolve_write_path("auth.json");
    if read_path != write_path {
        return Err(format!(
            "{} is stored in {} which the active profile inherits; re-run without --profile \
             (or with the profile that owns that file) to remove it",
            cred,
            read_path.display()
        ));
    }
    remove_key_at(&write_path, &cred.storage_key())
}

pub(crate) fn remove_key_at(path: &Path, key: &str) -> std::result::Result<bool, String> {
    // NOTE: the per-credential refresh lock file (`<auth>.refresh.<key>.lock`)
    // is deliberately NOT unlinked here: unlinking a lock file another
    // process may hold defeats flock (a re-created file is a new inode).
    if !path.exists() {
        return Ok(false);
    }
    with_locked_root(path, false, |root| {
        let removed = root.remove(key).is_some();
        Ok((removed, removed))
    })
}

/// Non-secret metadata stored beside a credential (`label`, `identity`,
/// `addedAt`). Kept separate from [`OAuthCredentials`] so refresh writes,
/// which only carry token fields, never clobber it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccountMetadata {
    /// Display-only provider identity (e.g. an email claim). Never a secret.
    pub identity: Option<String>,
    /// Epoch seconds when the slot was created.
    pub added_at: Option<u64>,
}

/// Merge non-secret metadata into an account slot (which must already exist).
pub fn save_account_metadata(
    cred: &CredentialRef,
    metadata: &AccountMetadata,
) -> std::result::Result<(), String> {
    let path = crate::config::resolve_write_path("auth.json");
    save_account_metadata_at(&path, cred, metadata)
}

pub(crate) fn save_account_metadata_at(
    path: &Path,
    cred: &CredentialRef,
    metadata: &AccountMetadata,
) -> std::result::Result<(), String> {
    let key = cred.storage_key();
    with_locked_root(path, false, |root| {
        let Some(entry) = root.get_mut(&key).and_then(serde_json::Value::as_object_mut) else {
            return Err(format!("no credential stored for {key}"));
        };
        match &cred.account {
            Account::Default => {
                entry.remove("label");
            }
            Account::Named(label) => {
                entry.insert("label".into(), serde_json::Value::String(label.as_str().into()));
            }
        }
        match &metadata.identity {
            Some(identity) if !identity.trim().is_empty() => {
                entry.insert(
                    "identity".into(),
                    serde_json::Value::String(identity.trim().to_string()),
                );
            }
            _ => {}
        }
        if let Some(added_at) = metadata.added_at {
            entry.insert("addedAt".into(), serde_json::Value::from(added_at));
        }
        Ok((true, ()))
    })
}

/// Outcome of a compare-and-swap credential write (refresh rotation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CasOutcome {
    /// The stored refresh token matched; the rotated credential was written.
    Saved,
    /// The slot no longer exists (removed while the refresh was in flight).
    Removed,
    /// The slot now holds a different credential (re-login while the refresh
    /// was in flight). Nothing was written.
    Replaced,
}

/// Write a rotated credential only if the slot still holds the credential
/// the rotation started from (identified by its refresh token). Preserves
/// metadata and, when the rotated credential omits `accountId`, the stored
/// one. Used by the refresh path so a concurrent removal or re-login is never
/// overwritten by a stale rotation.
pub(crate) fn save_provider_auth_if_refresh_matches_at(
    path: &Path,
    key: &str,
    expected_refresh: &str,
    creds: &OAuthCredentials,
) -> std::result::Result<CasOutcome, String> {
    let encoded =
        serde_json::to_value(creds).map_err(|e| format!("Failed to serialize auth: {}", e))?;
    let fields = encoded
        .as_object()
        .cloned()
        .ok_or_else(|| "credential did not serialize to an object".to_string())?;
    if !path.exists() {
        return Ok(CasOutcome::Removed);
    }
    with_locked_root(path, false, |root| {
        let Some(entry) = root.get_mut(key).and_then(serde_json::Value::as_object_mut) else {
            return Ok((false, CasOutcome::Removed));
        };
        let stored_refresh = entry.get("refresh").and_then(|v| v.as_str()).unwrap_or("");
        if stored_refresh != expected_refresh {
            return Ok((false, CasOutcome::Replaced));
        }
        for (k, v) in &fields {
            entry.insert(k.clone(), v.clone());
        }
        Ok((true, CasOutcome::Saved))
    })
}

/// Listing of one provider's stored accounts plus any sibling keys that look
/// like accounts of this provider but fail validation (never loaded).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccountInventory {
    pub accounts: Vec<AccountSummary>,
    pub malformed_keys: Vec<String>,
    /// Storage keys of slots that share a provider `accountId` with another
    /// slot of the same provider (two refresh owners for one seat).
    pub duplicate_identity_keys: Vec<String>,
}

/// Enumerate stored OAuth accounts for `provider` (non-secret rows;
/// `selected` is left `false` for the caller to annotate).
pub fn list_accounts(provider: OAuthProviderId) -> std::result::Result<Vec<AccountSummary>, String> {
    Ok(list_accounts_detailed_at(&auth_file_path(), Some(provider))?.accounts)
}

/// Enumerate stored OAuth accounts for every provider.
pub fn list_all_accounts() -> std::result::Result<Vec<AccountSummary>, String> {
    Ok(list_accounts_detailed_at(&auth_file_path(), None)?.accounts)
}

/// Enumerate with malformed-key diagnostics (for `auth list`).
pub fn list_accounts_detailed(
    provider: Option<OAuthProviderId>,
) -> std::result::Result<AccountInventory, String> {
    list_accounts_detailed_at(&auth_file_path(), provider)
}

pub(crate) fn list_accounts_detailed_at(
    path: &Path,
    provider: Option<OAuthProviderId>,
) -> std::result::Result<AccountInventory, String> {
    let mut inventory = AccountInventory::default();
    if !path.exists() {
        return Ok(inventory);
    }
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;
    let root: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&content)
        .map_err(|e| format!("Failed to parse {}: {}", path.display(), e))?;
    for (key, entry) in &root {
        let belongs = match provider {
            Some(p) => CredentialRef::key_belongs_to(key, p),
            None => super::provider::DESCRIPTORS
                .iter()
                .any(|d| CredentialRef::key_belongs_to(key, d.id)),
        };
        if !belongs {
            continue;
        }
        let Some(cred) = CredentialRef::parse_storage_key(key) else {
            tracing::warn!(key_len = key.len(), "ignoring malformed account key in auth.json");
            inventory.malformed_keys.push(display_key(key));
            continue;
        };
        let Some(obj) = entry.as_object() else {
            inventory.malformed_keys.push(display_key(key));
            continue;
        };
        if obj.get("type").and_then(|t| t.as_str()) != Some("oauth") {
            // Bare provider keys may legitimately hold non-OAuth state; only
            // OAuth entries are accounts.
            continue;
        }
        // An OAuth entry is only "configured" when it actually carries token
        // material (same rule as `oauth_provider_logged_in`). Entries with
        // missing/non-string tokens are reported by KEY ONLY — never values.
        let token_present = |field: &str| {
            obj.get(field)
                .and_then(|v| v.as_str())
                .is_some_and(|v| !v.is_empty())
        };
        let tokens_typed = ["access", "refresh"]
            .iter()
            .all(|f| matches!(obj.get(*f), None | Some(serde_json::Value::String(_))));
        if !tokens_typed || !(token_present("access") || token_present("refresh")) {
            tracing::warn!(key_len = key.len(), "oauth entry without usable token fields");
            inventory.malformed_keys.push(display_key(key));
            continue;
        }
        inventory.accounts.push(AccountSummary {
            identity: obj
                .get("identity")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .map(|s| s.trim().to_string()),
            expires: obj.get("expires").and_then(|v| v.as_u64()).unwrap_or(0),
            added_at: obj.get("addedAt").and_then(|v| v.as_u64()),
            ..AccountSummary::new(cred.provider, &cred.account)
                .with_account_id(obj.get("accountId").and_then(|v| v.as_str()))
        });
    }
    inventory
        .accounts
        .sort_by(|a, b| (&a.provider, a.label != "default", &a.label).cmp(&(&b.provider, b.label != "default", &b.label)));
    // Duplicate seats: same provider + same full accountId in two slots.
    let mut seen: std::collections::BTreeMap<(String, String), Vec<String>> =
        std::collections::BTreeMap::new();
    for (key, entry) in &root {
        let Some(cred) = CredentialRef::parse_storage_key(key) else {
            continue;
        };
        if let Some(id) = entry
            .get("accountId")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if provider.map_or(true, |p| p == cred.provider) {
                seen.entry((cred.provider.as_str().to_string(), id.to_string()))
                    .or_default()
                    .push(key.clone());
            }
        }
    }
    for keys in seen.into_values() {
        if keys.len() > 1 {
            inventory.duplicate_identity_keys.extend(keys);
        }
    }
    inventory.duplicate_identity_keys.sort();
    Ok(inventory)
}

/// Bounded key text for diagnostics (keys are labels, never token material,
/// but a hand-edited file could put anything there).
fn display_key(key: &str) -> String {
    crate::truncate_str(key, 64).to_string()
}

/// True if auth.json holds at least one OAuth credential of ANY provider or
/// account. Used for broker startup/health so non-Anthropic-only
/// installations work.
pub fn any_oauth_credential_present() -> std::result::Result<bool, String> {
    any_oauth_credential_present_at(&auth_file_path())
}

pub(crate) fn any_oauth_credential_present_at(path: &Path) -> std::result::Result<bool, String> {
    Ok(!list_accounts_detailed_at(path, None)?.accounts.is_empty())
}

/// What the duplicate-identity guard could establish for a new login.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityEvidence {
    /// The provider exposes no trustworthy identity for this credential, so
    /// a duplicate seat under another label CANNOT be detected.
    NoEvidence,
    /// Identity known; no other slot of this provider holds the same seat.
    NoDuplicate,
    /// Another slot of this provider already holds this seat.
    Duplicate(Account),
}

/// Outcome of [`save_credential_unless_duplicate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginPersistOutcome {
    pub evidence: IdentityEvidence,
    /// False iff a duplicate was found (nothing was written).
    pub saved: bool,
}

/// Login persistence: duplicate scan and write happen inside ONE locked
/// read-modify-write of auth.json, so two concurrent logins (or a login
/// racing a remove/re-login) can never create two slots owning the same
/// seat. A duplicate is another slot of the same provider with the same
/// non-empty `accountId`, or with an identical refresh token.
///
/// Metadata is rewritten atomically with the credential: `label` follows the
/// slot, `identity` is set or CLEARED (a re-login with a different seat never
/// keeps the old identity), `addedAt` is set to `metadata.added_at`.
///
/// A corrupt store is an error (never reset: that would wipe every other
/// slot). With an active profile that has no own auth.json while the base
/// store exists, the write is REFUSED: creating a profile file would hide
/// every inherited account from that profile, and tokens are never copied.
pub fn save_credential_unless_duplicate(
    cred: &CredentialRef,
    creds: &OAuthCredentials,
    metadata: &AccountMetadata,
) -> std::result::Result<LoginPersistOutcome, String> {
    let read_path = auth_file_path();
    let write_path = crate::config::resolve_write_path("auth.json");
    if read_path != write_path && read_path.exists() {
        return Err(format!(
            "the active profile has no auth.json of its own and inherits {}; logging in here would \
             create a profile store that hides every inherited account (tokens are never copied). \
             Re-run without --profile to add the account to the shared store.",
            read_path.display()
        ));
    }
    save_credential_unless_duplicate_at(&write_path, cred, creds, metadata)
}

pub(crate) fn save_credential_unless_duplicate_at(
    path: &Path,
    cred: &CredentialRef,
    creds: &OAuthCredentials,
    metadata: &AccountMetadata,
) -> std::result::Result<LoginPersistOutcome, String> {
    let encoded =
        serde_json::to_value(creds).map_err(|e| format!("Failed to serialize auth: {}", e))?;
    let fields = encoded
        .as_object()
        .cloned()
        .ok_or_else(|| "credential did not serialize to an object".to_string())?;
    let key = cred.storage_key();
    let account_id = creds
        .account_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    with_locked_root(path, false, |root| {
        // Scan sibling slots of the same provider under the lock.
        let mut duplicate: Option<Account> = None;
        for (other_key, entry) in root.iter() {
            if other_key == &key {
                continue;
            }
            let Some(other) = CredentialRef::parse_storage_key(other_key) else {
                continue;
            };
            if other.provider != cred.provider {
                continue;
            }
            let stored_id = entry
                .get("accountId")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .unwrap_or("");
            let stored_refresh = entry.get("refresh").and_then(|v| v.as_str()).unwrap_or("");
            let same_seat = account_id.is_some_and(|id| id == stored_id)
                || (!creds.refresh.is_empty() && creds.refresh == stored_refresh);
            if same_seat {
                duplicate = Some(other.account);
                break;
            }
        }
        if let Some(account) = duplicate {
            return Ok((
                false,
                LoginPersistOutcome {
                    evidence: IdentityEvidence::Duplicate(account),
                    saved: false,
                },
            ));
        }
        let mut slot = root
            .remove(&key)
            .filter(serde_json::Value::is_object)
            .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
        if let Some(target) = slot.as_object_mut() {
            for (k, v) in &fields {
                target.insert(k.clone(), v.clone());
            }
            if account_id.is_none() {
                // The new credential carries no identity: never keep a stale
                // one from a previous login into this slot.
                target.remove("accountId");
            }
            match &cred.account {
                Account::Default => {
                    target.remove("label");
                }
                Account::Named(label) => {
                    target.insert("label".into(), serde_json::Value::String(label.as_str().into()));
                }
            }
            match metadata.identity.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                Some(identity) => {
                    target.insert("identity".into(), serde_json::Value::String(identity.into()));
                }
                None => {
                    target.remove("identity");
                }
            }
            match metadata.added_at {
                Some(added_at) => {
                    target.insert("addedAt".into(), serde_json::Value::from(added_at));
                }
                None => {
                    target.remove("addedAt");
                }
            }
        }
        root.insert(key.clone(), slot);
        Ok((
            true,
            LoginPersistOutcome {
                evidence: if account_id.is_some() {
                    IdentityEvidence::NoDuplicate
                } else {
                    IdentityEvidence::NoEvidence
                },
                saved: true,
            },
        ))
    })
}

/// Duplicate-identity guard for login: returns the account (of the same
/// provider, other than `except`) that already stores `account_id`.
pub fn find_duplicate_identity(
    provider: OAuthProviderId,
    account_id: &str,
    except: &Account,
) -> std::result::Result<Option<Account>, String> {
    find_duplicate_identity_at(&auth_file_path(), provider, account_id, except)
}

pub(crate) fn find_duplicate_identity_at(
    path: &Path,
    provider: OAuthProviderId,
    account_id: &str,
    except: &Account,
) -> std::result::Result<Option<Account>, String> {
    let account_id = account_id.trim();
    if account_id.is_empty() || !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;
    let root: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&content)
        .map_err(|e| format!("Failed to parse {}: {}", path.display(), e))?;
    for (key, entry) in &root {
        let Some(cred) = CredentialRef::parse_storage_key(key) else {
            continue;
        };
        if cred.provider != provider || &cred.account == except {
            continue;
        }
        let stored = entry
            .get("accountId")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or("");
        if !stored.is_empty() && stored == account_id {
            return Ok(Some(cred.account));
        }
    }
    Ok(None)
}

/// Save credentials to auth.json.
pub fn save_auth(creds: &OAuthCredentials) -> std::result::Result<(), String> {
    save_provider_auth("anthropic", creds)
}

/// Save one provider credential while preserving other auth.json entries.
pub fn save_provider_auth(
    provider: &str,
    creds: &OAuthCredentials,
) -> std::result::Result<(), String> {
    let path = crate::config::resolve_write_path("auth.json");
    save_provider_auth_at(&path, provider, creds)
}

/// Load an opaque broker-owned cloud provider state object.
pub fn load_cloud_state(provider: &str) -> std::result::Result<Option<serde_json::Value>, String> {
    let path = auth_file_path();
    if !path.exists() {
        return Ok(None);
    }
    let root: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&path)
            .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?,
    )
    .map_err(|e| format!("Failed to parse {}: {}", path.display(), e))?;
    Ok(root
        .get(provider)
        .and_then(|v| v.get("cloud_state"))
        .cloned())
}

/// Atomically persist opaque cloud state while preserving every unrelated field.
pub fn save_cloud_state(
    provider: &str,
    state: &serde_json::Value,
) -> std::result::Result<(), String> {
    let path = crate::config::resolve_write_path("auth.json");
    let mut fields = serde_json::Map::new();
    fields.insert("type".into(), serde_json::Value::String("cloud".into()));
    fields.insert("cloud_state".into(), state.clone());
    save_provider_fields_at(&path, provider, &fields)
}

/// Load one provider's broker-owned static API key from auth.json.
///
/// Static keys are persisted as `{"type":"api_key","key":"…"}` entries in the
/// same open JSON object as OAuth credentials, so unknown providers and
/// provider metadata round-trip through the exact same merge writer.
pub fn load_static_key(provider: &str) -> std::result::Result<Option<String>, String> {
    load_static_key_at(&auth_file_path(), provider)
}

pub(crate) fn load_static_key_at(
    path: &std::path::Path,
    provider: &str,
) -> std::result::Result<Option<String>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;
    let value: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| format!("Failed to parse {}: {}", path.display(), e))?;
    let Some(entry) = value.get(provider) else {
        return Ok(None);
    };
    if entry.get("type").and_then(|t| t.as_str()) != Some("api_key") {
        return Ok(None);
    }
    Ok(entry
        .get("key")
        .and_then(|k| k.as_str())
        .filter(|k| !k.is_empty())
        .map(str::to_string))
}

/// Persist a broker-owned static API key while preserving every other
/// auth.json entry and any provider metadata.
pub fn save_static_key(provider: &str, key: &str) -> std::result::Result<(), String> {
    let path = crate::config::resolve_write_path("auth.json");
    save_static_key_at(&path, provider, key)
}

pub(crate) fn save_static_key_at(
    path: &std::path::Path,
    provider: &str,
    key: &str,
) -> std::result::Result<(), String> {
    let mut fields = serde_json::Map::new();
    fields.insert(
        "type".to_string(),
        serde_json::Value::String("api_key".to_string()),
    );
    fields.insert(
        "key".to_string(),
        serde_json::Value::String(key.to_string()),
    );
    save_provider_fields_at(path, provider, &fields)
}

/// Path-explicit variant of `save_provider_auth`. Splits out the I/O so
/// the corrupt-file fallback path can be unit-tested without touching the
/// user's real `~/.synaps-cli/auth.json`.
pub(crate) fn save_provider_auth_at(
    path: &std::path::Path,
    provider: &str,
    creds: &OAuthCredentials,
) -> std::result::Result<(), String> {
    let encoded =
        serde_json::to_value(creds).map_err(|e| format!("Failed to serialize auth: {}", e))?;
    let fields = encoded
        .as_object()
        .ok_or_else(|| "credential did not serialize to an object".to_string())?;
    save_provider_fields_at(path, provider, fields)
}

/// Shared merge writer: lock, read-merge every provider entry, merge `fields`
/// into the one provider object (preserving unknown metadata), atomic rename.
fn save_provider_fields_at(
    path: &std::path::Path,
    provider: &str,
    fields: &serde_json::Map<String, serde_json::Value>,
) -> std::result::Result<(), String> {
    with_locked_root(path, true, |root| {
        // Merge known credential fields into an existing provider object rather
        // than replacing it. Providers may add metadata (including nested objects)
        // that must survive refresh/login writes.
        let mut provider_value = root
            .remove(provider)
            .filter(serde_json::Value::is_object)
            .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
        if let Some(target) = provider_value.as_object_mut() {
            for (key, value) in fields {
                target.insert(key.clone(), value.clone());
            }
        }
        root.insert(provider.to_string(), provider_value);
        Ok((true, ()))
    })
}

/// Locked read-modify-write over the auth.json root object.
///
/// Holds the exclusive `auth.json.lock` for the whole cycle, hands the parsed
/// root to `edit`, and — when `edit` returns `(true, _)` — writes the result
/// atomically (0600 tmp + rename). `recover_corrupt` selects the login-path
/// behaviour of resetting an unparseable file (with a backup) versus failing.
fn with_locked_root<T>(
    path: &std::path::Path,
    recover_corrupt: bool,
    edit: impl FnOnce(
        &mut serde_json::Map<String, serde_json::Value>,
    ) -> std::result::Result<(bool, T), String>,
) -> std::result::Result<T, String> {
    use fs4::fs_std::FileExt;

    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create {}: {}", parent.display(), e))?;
    }

    // Hold an exclusive lock for the entire read-modify-write cycle.
    // Without this, two concurrent `synaps login` processes can race:
    // both read the same file, each adds their provider, second write
    // silently drops the first's credential.
    let lock_path = path.with_extension("json.lock");
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&lock_path)
        .map_err(|e| format!("Failed to open lock file {}: {}", lock_path.display(), e))?;
    FileExt::lock_exclusive(&lock_file)
        .map_err(|e| format!("Failed to lock {}: {}", lock_path.display(), e))?;

    let mut root = if path.exists() {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;
        // Corrupt-file recovery: if the existing auth.json is not a JSON
        // object (truncated write, manual edit error, swap-file detritus),
        // log a warning and start fresh rather than refusing to save the
        // new credential. The alternative is permanently locking the user
        // out of `synaps login`.
        match serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&content) {
            Ok(map) => map,
            Err(e) if !recover_corrupt => {
                return Err(format!("Failed to parse {}: {}", path.display(), e));
            }
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "auth.json could not be parsed as a JSON object; replacing with a fresh structure"
                );
                // Back up the corrupt file so credentials are potentially recoverable.
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let backup = path.with_extension(format!("json.corrupt.{}", ts));
                match std::fs::copy(path, &backup) {
                    Ok(_) => {
                        eprintln!(
                            "[warn] auth.json was corrupt and has been reset. Backup saved to: {}",
                            backup.display()
                        );
                    }
                    Err(copy_err) => {
                        eprintln!(
                            "[warn] auth.json was corrupt and has been reset, but backup failed: {}",
                            copy_err
                        );
                    }
                }
                serde_json::Map::new()
            }
        }
    } else {
        serde_json::Map::new()
    };

    let (write, result) = edit(&mut root)?;
    if !write {
        return Ok(result);
    }

    let json = serde_json::to_string_pretty(&root)
        .map_err(|e| format!("Failed to serialize auth: {}", e))?;

    // Atomic write: write to .tmp then rename. rename(2) is atomic on POSIX.
    // This prevents a crash/kill between truncate and write from zeroing auth.json.
    // Create with restrictive permissions from the start so credentials are never
    // world-readable, even briefly.
    let tmp_path = path.with_extension("json.tmp");
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp_path)
            .map_err(|e| format!("Failed to create {}: {}", tmp_path.display(), e))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            file.set_permissions(perms).map_err(|e| {
                format!("Failed to set permissions on {}: {}", tmp_path.display(), e)
            })?;
        }

        file.write_all(json.as_bytes())
            .map_err(|e| format!("Failed to write {}: {}", tmp_path.display(), e))?;
        file.sync_all()
            .map_err(|e| format!("Failed to fsync {}: {}", tmp_path.display(), e))?;
    }

    std::fs::rename(&tmp_path, path)
        .map_err(|e| format!("Failed to atomically replace {}: {}", path.display(), e))?;

    Ok(result)
}

/// Test-only re-export of `save_provider_auth_at` so that `token::tests`
/// can exercise the same function `ensure_fresh_token` now delegates to,
/// without making the path-explicit variant part of the public API.
#[cfg(test)]
pub(super) fn save_provider_auth_at_test_hook(
    path: &std::path::Path,
    provider: &str,
    creds: &OAuthCredentials,
) -> std::result::Result<(), String> {
    save_provider_auth_at(path, provider, creds)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_creds() -> OAuthCredentials {
        OAuthCredentials {
            auth_type: "oauth".to_string(),
            refresh: "r".to_string(),
            access: "a".to_string(),
            expires: 1,
            account_id: None,
        }
    }

    #[test]
    fn save_preserves_unknown_nested_provider_metadata() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");
        std::fs::write(&path, r#"{"openai-codex":{"type":"oauth","refresh":"old","access":"old","expires":1,"metadata":{"tenant":"t1","flags":{"beta":true}}}}"#).unwrap();
        save_provider_auth_at(&path, "openai-codex", &fresh_creds()).unwrap();
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(value["openai-codex"]["metadata"]["tenant"], "t1");
        assert_eq!(value["openai-codex"]["metadata"]["flags"]["beta"], true);
        assert_eq!(value["openai-codex"]["access"], "a");
    }

    #[test]
    fn save_provider_auth_at_creates_file_when_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");
        save_provider_auth_at(&path, "openai-codex", &fresh_creds()).expect("save");
        assert!(path.exists());
        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(parsed.get("openai-codex").is_some());
    }

    #[test]
    fn save_provider_auth_at_preserves_other_providers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");
        std::fs::write(
            &path,
            r#"{"anthropic":{"type":"oauth","refresh":"r2","access":"a2","expires":2}}"#,
        )
        .unwrap();
        save_provider_auth_at(&path, "openai-codex", &fresh_creds()).expect("save");
        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(
            parsed.get("anthropic").is_some(),
            "must keep anthropic entry"
        );
        assert!(parsed.get("openai-codex").is_some());
    }

    #[test]
    fn save_provider_auth_at_recovers_from_corrupt_file() {
        // Pre-fix: a corrupt auth.json would lock the user out of
        // `synaps login` entirely because save_provider_auth would fail
        // to parse and bail. After fix: corrupt content is replaced with
        // a fresh structure containing the new credential.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");
        std::fs::write(&path, "this is not json {{{").unwrap();
        save_provider_auth_at(&path, "openai-codex", &fresh_creds())
            .expect("save must succeed even on corrupt input");
        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(&content).expect("file must now contain valid JSON");
        assert!(parsed.get("openai-codex").is_some());
        assert!(
            parsed.get("anthropic").is_none(),
            "corrupt fallback discards old (unrecoverable) entries"
        );
    }

    #[test]
    fn save_provider_auth_at_recovers_from_array_root() {
        // auth.json was a JSON array (perhaps from a botched migration).
        // Treat it as corrupt — same recovery as garbage input.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");
        std::fs::write(&path, "[1, 2, 3]").unwrap();
        save_provider_auth_at(&path, "openai-codex", &fresh_creds())
            .expect("save must succeed against non-object root");
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(parsed.is_object());
        assert!(parsed.get("openai-codex").is_some());
    }

    // ── Broker-owned static-key storage ──────────────────────────────────────

    /// Saving a static key must preserve existing OAuth entries (cross-provider
    /// isolation at the storage layer), and vice versa.
    #[test]
    fn static_key_save_preserves_oauth_entries_and_roundtrips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");
        save_provider_auth_at(&path, "anthropic", &fresh_creds()).expect("save oauth");
        save_static_key_at(&path, "groq", "gsk-secret-1").expect("save static key");

        assert_eq!(
            load_static_key_at(&path, "groq").unwrap().as_deref(),
            Some("gsk-secret-1")
        );
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            parsed["anthropic"]["refresh"], "r",
            "OAuth entry must survive"
        );
        assert_eq!(parsed["groq"]["type"], "api_key");
    }

    /// An OAuth save must not disturb a broker-owned static key.
    #[test]
    fn oauth_save_preserves_static_key_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");
        save_static_key_at(&path, "openrouter", "sk-or-1").expect("save static key");
        save_provider_auth_at(&path, "openai-codex", &fresh_creds()).expect("save oauth");
        assert_eq!(
            load_static_key_at(&path, "openrouter").unwrap().as_deref(),
            Some("sk-or-1")
        );
    }

    /// Static-key updates merge in place and keep unknown metadata on the entry.
    #[test]
    fn static_key_upsert_preserves_entry_metadata() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");
        std::fs::write(
            &path,
            r#"{"groq":{"type":"api_key","key":"old","note":{"source":"migrated"}}}"#,
        )
        .unwrap();
        save_static_key_at(&path, "groq", "new-key").expect("upsert");
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed["groq"]["key"], "new-key");
        assert_eq!(parsed["groq"]["note"]["source"], "migrated");
    }

    /// Loading a static key from an OAuth entry must return None — the two
    /// credential kinds never blur (cross-kind isolation).
    #[test]
    fn load_static_key_ignores_oauth_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");
        save_provider_auth_at(&path, "anthropic", &fresh_creds()).expect("save oauth");
        assert_eq!(load_static_key_at(&path, "anthropic").unwrap(), None);
        assert_eq!(load_static_key_at(&path, "missing").unwrap(), None);
    }

    // ── Bug #159 regression suite ────────────────────────────────────────────
    //
    // Repro: `synaps login --provider openai-codex` (saves "openai-codex"),
    // then `synaps login` (Anthropic OAuth, saves "anthropic") → the first
    // provider's credential was wiped because save_auth() rebuilt AuthFile
    // with only the new entry and did a whole-file overwrite.
    //
    // Root cause (pre-fix): src/core/auth/storage.rs @ 90f8f71 — save_auth()
    // constructed `AuthFile { anthropic: creds }` (no read of existing file)
    // then called `fs::write(path, json)`, discarding every other provider.
    //
    // All three multi-provider tests below would FAIL against that code.

    /// Primary repro (bug #159): save openai-codex, then save anthropic via
    /// the same write path `synaps login` (Anthropic OAuth) takes.
    /// openai-codex MUST survive the second login.
    #[test]
    fn bug159_anthropic_login_preserves_existing_openai_codex() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");

        // Step 1: user runs `synaps login --provider openai-codex`
        let codex_creds = OAuthCredentials {
            auth_type: "oauth".to_string(),
            refresh: "codex-refresh".to_string(),
            access: "codex-access".to_string(),
            expires: 9_999_999_999_000,
            account_id: Some("acct_codex_123".to_string()),
        };
        save_provider_auth_at(&path, "openai-codex", &codex_creds).expect("save openai-codex");

        // Step 2: user then runs `synaps login` (Anthropic Claude OAuth)
        let anthropic_creds = OAuthCredentials {
            auth_type: "oauth".to_string(),
            refresh: "anth-refresh".to_string(),
            access: "anth-access".to_string(),
            expires: 9_999_999_999_000,
            account_id: None,
        };
        save_provider_auth_at(&path, "anthropic", &anthropic_creds).expect("save anthropic");

        // Both providers must be present — bug #159 would wipe openai-codex here
        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();

        assert!(
            parsed.get("anthropic").is_some(),
            "anthropic credential must be present after login"
        );
        assert!(
            parsed.get("openai-codex").is_some(),
            "bug #159: openai-codex credential must NOT be wiped when logging in as anthropic"
        );
        assert_eq!(
            parsed["openai-codex"]["refresh"].as_str(),
            Some("codex-refresh"),
            "openai-codex refresh token must be unchanged"
        );
        assert_eq!(
            parsed["anthropic"]["refresh"].as_str(),
            Some("anth-refresh"),
            "anthropic refresh token must be correctly saved"
        );
    }

    /// Reverse of primary repro: anthropic first, then openai-codex login.
    /// anthropic MUST survive.
    #[test]
    fn bug159_openai_codex_login_preserves_existing_anthropic() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");

        // Step 1: anthropic already logged in
        let anthropic_creds = OAuthCredentials {
            auth_type: "oauth".to_string(),
            refresh: "anth-refresh-first".to_string(),
            access: "anth-access-first".to_string(),
            expires: 9_999_999_999_000,
            account_id: None,
        };
        save_provider_auth_at(&path, "anthropic", &anthropic_creds).expect("save anthropic");

        // Step 2: user runs `synaps login --provider openai-codex`
        let codex_creds = OAuthCredentials {
            auth_type: "oauth".to_string(),
            refresh: "codex-refresh-second".to_string(),
            access: "codex-access-second".to_string(),
            expires: 9_999_999_999_000,
            account_id: Some("acct_456".to_string()),
        };
        save_provider_auth_at(&path, "openai-codex", &codex_creds).expect("save openai-codex");

        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();

        assert!(
            parsed.get("anthropic").is_some(),
            "anthropic credential must survive openai-codex login"
        );
        assert!(
            parsed.get("openai-codex").is_some(),
            "openai-codex credential must be present"
        );
        assert_eq!(
            parsed["anthropic"]["refresh"].as_str(),
            Some("anth-refresh-first"),
            "anthropic refresh token must be unchanged"
        );
    }

    /// Three-provider scenario: saving a third provider preserves the first two.
    /// Guards against regressions if/when more OAuth providers are added.
    #[test]
    fn bug159_third_provider_login_preserves_both_existing_providers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");

        save_provider_auth_at(
            &path,
            "anthropic",
            &OAuthCredentials {
                auth_type: "oauth".to_string(),
                refresh: "anth-r".to_string(),
                access: "anth-a".to_string(),
                expires: 1,
                account_id: None,
            },
        )
        .expect("save anthropic");

        save_provider_auth_at(
            &path,
            "openai-codex",
            &OAuthCredentials {
                auth_type: "oauth".to_string(),
                refresh: "codex-r".to_string(),
                access: "codex-a".to_string(),
                expires: 1,
                account_id: Some("acct_codex".to_string()),
            },
        )
        .expect("save openai-codex");

        save_provider_auth_at(
            &path,
            "future-provider",
            &OAuthCredentials {
                auth_type: "oauth".to_string(),
                refresh: "future-r".to_string(),
                access: "future-a".to_string(),
                expires: 1,
                account_id: None,
            },
        )
        .expect("save future-provider");

        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();

        assert!(parsed.get("anthropic").is_some(), "anthropic must survive");
        assert!(
            parsed.get("openai-codex").is_some(),
            "openai-codex must survive"
        );
        assert!(
            parsed.get("future-provider").is_some(),
            "future-provider must be present"
        );
    }

    /// Upsert: saving the same provider twice updates the credential in place
    /// without duplicating or corrupting the entry.
    #[test]
    fn bug159_same_provider_login_upserts_in_place() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");

        save_provider_auth_at(
            &path,
            "anthropic",
            &OAuthCredentials {
                auth_type: "oauth".to_string(),
                refresh: "old-refresh".to_string(),
                access: "old-access".to_string(),
                expires: 1,
                account_id: None,
            },
        )
        .expect("initial save");

        save_provider_auth_at(
            &path,
            "anthropic",
            &OAuthCredentials {
                auth_type: "oauth".to_string(),
                refresh: "new-refresh".to_string(),
                access: "new-access".to_string(),
                expires: 2,
                account_id: None,
            },
        )
        .expect("upsert save");

        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        let obj = parsed.as_object().unwrap();

        assert_eq!(obj.len(), 1, "only one anthropic entry — no duplicates");
        assert_eq!(
            parsed["anthropic"]["refresh"].as_str(),
            Some("new-refresh"),
            "credential must be updated to latest value"
        );
    }

    // ── Multi-account slots (G1) ─────────────────────────────────────────────

    fn creds(refresh: &str, account_id: Option<&str>) -> OAuthCredentials {
        OAuthCredentials {
            auth_type: "oauth".to_string(),
            refresh: refresh.to_string(),
            access: format!("{refresh}-access"),
            expires: 5,
            account_id: account_id.map(str::to_string),
        }
    }

    fn named(provider: OAuthProviderId, label: &str) -> CredentialRef {
        CredentialRef::new(provider, Account::named(label).unwrap())
    }

    fn read_root(path: &Path) -> serde_json::Map<String, serde_json::Value> {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn named_slot_save_never_touches_default_slot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        // Legacy single-account file, written exactly as older versions do.
        std::fs::write(
            &path,
            r#"{"openai-codex":{"type":"oauth","refresh":"legacy-r","access":"legacy-a","expires":1,"accountId":"acct-legacy"}}"#,
        )
        .unwrap();
        let astra2 = named(OAuthProviderId::OpenAiCodex, "astra2");
        save_provider_auth_at(&path, &astra2.storage_key(), &creds("r2", Some("acct-2"))).unwrap();
        let root = read_root(&path);
        assert_eq!(root["openai-codex"]["refresh"], "legacy-r", "default slot untouched");
        assert_eq!(root["openai-codex"]["accountId"], "acct-legacy");
        assert_eq!(root["openai-codex@astra2"]["refresh"], "r2");
        assert_eq!(root.len(), 2);

        // Legacy readers keep resolving the bare key.
        let legacy = load_provider_auth_at(&path, "openai-codex").unwrap().unwrap();
        assert_eq!(legacy.refresh, "legacy-r");
        let slot = load_provider_auth_at(&path, &astra2.storage_key()).unwrap().unwrap();
        assert_eq!(slot.refresh, "r2");
        // The named slot is invisible under the bare key and vice versa.
        assert!(load_provider_auth_at(&path, "openai-codex@missing").unwrap().is_none());
    }

    #[test]
    fn list_accounts_reports_default_named_and_malformed_without_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        std::fs::write(
            &path,
            r#"{
              "openai-codex": {"type":"oauth","refresh":"R0","access":"A0","expires":10,"accountId":"2b2f0000-1111"},
              "openai-codex@astra2": {"type":"oauth","refresh":"R1","access":"A1","expires":20,"accountId":"7f530000-2222","label":"astra2","identity":"a@example.com","addedAt":99},
              "openai-codex@Bad Label": {"type":"oauth","refresh":"R2","access":"A2","expires":0},
              "openai-codex@default": {"type":"oauth","refresh":"R3","access":"A3","expires":0},
              "openai-codex@notoken": {"type":"oauth","expires":5},
              "openai-codex@badtype": {"type":"oauth","refresh":{"nested":"SECRET-VALUE"},"access":"A5","expires":5},
              "openai-codex@empty": {"type":"oauth","refresh":"","access":"","expires":5},
              "anthropic": {"type":"oauth","refresh":"R4","access":"A4","expires":30},
              "groq": {"type":"api_key","key":"gsk-secret"},
              "aws-bedrock": {"type":"cloud","cloud_state":{}}
            }"#,
        )
        .unwrap();
        let inv = list_accounts_detailed_at(&path, Some(OAuthProviderId::OpenAiCodex)).unwrap();
        assert_eq!(inv.accounts.len(), 2, "{:?}", inv.accounts);
        assert_eq!(inv.accounts[0].label, "default");
        assert_eq!(inv.accounts[0].account_id_prefix.as_deref(), Some("2b2f0000"));
        assert_eq!(
            inv.accounts[0].seat_fingerprint,
            super::super::account::seat_fingerprint(OAuthProviderId::OpenAiCodex, "2b2f0000-1111")
        );
        assert_ne!(inv.accounts[0].seat_fingerprint, inv.accounts[1].seat_fingerprint);
        assert_eq!(inv.accounts[0].expires, 10);
        assert_eq!(inv.accounts[1].label, "astra2");
        assert_eq!(inv.accounts[1].identity.as_deref(), Some("a@example.com"));
        assert_eq!(inv.accounts[1].added_at, Some(99));
        assert_eq!(inv.malformed_keys.len(), 5, "{:?}", inv.malformed_keys);
        for key in [
            "openai-codex@Bad Label",
            "openai-codex@default",
            "openai-codex@notoken",
            "openai-codex@badtype",
            "openai-codex@empty",
        ] {
            assert!(inv.malformed_keys.contains(&key.to_string()), "{key}");
        }
        let diag = format!("{:?}", inv);
        assert!(!diag.contains("SECRET-VALUE"), "diagnostics must not dump values");

        let all = list_accounts_detailed_at(&path, None).unwrap();
        assert_eq!(all.accounts.len(), 3);
        assert!(all.accounts.iter().all(|a| a.provider != "groq" && a.provider != "aws-bedrock"));
        let json = serde_json::to_string(&all.accounts).unwrap();
        for secret in ["R0", "R1", "R4", "A0", "A1", "A4", "A5", "gsk-secret", "SECRET-VALUE"] {
            assert!(!json.contains(secret), "listing leaked {secret}: {json}");
        }
        assert!(!json.contains("2b2f0000-1111"), "full account id must not be exposed");
        assert!(any_oauth_credential_present_at(&path).unwrap());
    }

    #[test]
    fn any_oauth_credential_present_accepts_non_anthropic_only_and_rejects_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        assert!(!any_oauth_credential_present_at(&path).unwrap(), "missing file");
        std::fs::write(&path, r#"{"groq":{"type":"api_key","key":"k"}}"#).unwrap();
        assert!(!any_oauth_credential_present_at(&path).unwrap(), "static keys are not OAuth");
        std::fs::write(
            &path,
            r#"{"kimi-code@m27":{"type":"oauth","refresh":"r","access":"a","expires":1}}"#,
        )
        .unwrap();
        assert!(any_oauth_credential_present_at(&path).unwrap(), "named Kimi-only install");
        std::fs::write(&path, "not json").unwrap();
        assert!(any_oauth_credential_present_at(&path).is_err());
    }

    #[test]
    fn remove_credential_removes_exactly_one_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        save_provider_auth_at(&path, "openai-codex", &creds("r0", None)).unwrap();
        save_provider_auth_at(&path, "openai-codex@a", &creds("ra", None)).unwrap();
        save_provider_auth_at(&path, "openai-codex@b", &creds("rb", None)).unwrap();
        assert!(remove_key_at(&path, "openai-codex@a").unwrap());
        let root = read_root(&path);
        assert_eq!(root.len(), 2);
        assert_eq!(root["openai-codex"]["refresh"], "r0");
        assert_eq!(root["openai-codex@b"]["refresh"], "rb");
        assert!(!remove_key_at(&path, "openai-codex@a").unwrap(), "idempotent");
        assert!(!path.with_extension("json.tmp").exists());
        // Removal never resets a corrupt file.
        std::fs::write(&path, "garbage{").unwrap();
        assert!(remove_key_at(&path, "openai-codex").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "garbage{");
    }

    #[test]
    fn metadata_round_trip_and_preserved_across_refresh_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let cred = named(OAuthProviderId::Anthropic, "work");
        save_provider_auth_at(&path, &cred.storage_key(), &creds("r1", None)).unwrap();
        save_account_metadata_at(
            &path,
            &cred,
            &AccountMetadata {
                identity: Some("me@example.com".into()),
                added_at: Some(1234),
            },
        )
        .unwrap();
        // A refresh-style write carries only token fields.
        save_provider_auth_at(&path, &cred.storage_key(), &creds("r2", None)).unwrap();
        let root = read_root(&path);
        assert_eq!(root["anthropic@work"]["label"], "work");
        assert_eq!(root["anthropic@work"]["identity"], "me@example.com");
        assert_eq!(root["anthropic@work"]["addedAt"], 1234);
        assert_eq!(root["anthropic@work"]["refresh"], "r2");
        // Metadata on a missing slot is an error, not a phantom entry.
        assert!(save_account_metadata_at(
            &path,
            &named(OAuthProviderId::Anthropic, "nope"),
            &AccountMetadata::default()
        )
        .is_err());
        assert!(read_root(&path).get("anthropic@nope").is_none());
    }

    #[test]
    fn cas_save_saved_removed_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let key = "openai-codex@astra2";
        save_provider_auth_at(&path, key, &creds("old", Some("acct-1"))).unwrap();

        // Rotated credential without accountId must keep the stored one.
        let rotated = creds("old-rotated", None);
        assert_eq!(
            save_provider_auth_if_refresh_matches_at(&path, key, "old", &rotated).unwrap(),
            CasOutcome::Saved
        );
        let root = read_root(&path);
        assert_eq!(root[key]["refresh"], "old-rotated");
        assert_eq!(root[key]["accountId"], "acct-1", "accountId preserved");

        // Re-login replaced the slot meanwhile: stale rotation must not win.
        save_provider_auth_at(&path, key, &creds("fresh-login", Some("acct-9"))).unwrap();
        assert_eq!(
            save_provider_auth_if_refresh_matches_at(&path, key, "old-rotated", &creds("stale", None))
                .unwrap(),
            CasOutcome::Replaced
        );
        assert_eq!(read_root(&path)[key]["refresh"], "fresh-login");

        // Slot removed meanwhile: nothing is resurrected.
        remove_key_at(&path, key).unwrap();
        assert_eq!(
            save_provider_auth_if_refresh_matches_at(&path, key, "fresh-login", &creds("x", None))
                .unwrap(),
            CasOutcome::Removed
        );
        assert!(read_root(&path).get(key).is_none());
        // Missing file: nothing is created either.
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            save_provider_auth_if_refresh_matches_at(&path, key, "x", &creds("x", None)).unwrap(),
            CasOutcome::Removed
        );
        assert!(!path.exists());
    }

    #[test]
    fn locked_login_persist_refuses_duplicate_seat_and_writes_metadata_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        save_provider_auth_at(&path, "openai-codex", &creds("r0", Some("acct-A"))).unwrap();
        let astra2 = named(OAuthProviderId::OpenAiCodex, "astra2");
        let meta = AccountMetadata {
            identity: Some("a@example.com".into()),
            added_at: Some(100),
        };
        // Same seat under a new label: refused, nothing written.
        let out = save_credential_unless_duplicate_at(&path, &astra2, &creds("rx", Some("acct-A")), &meta)
            .unwrap();
        assert_eq!(out.evidence, IdentityEvidence::Duplicate(Account::Default));
        assert!(!out.saved);
        assert!(read_root(&path).get("openai-codex@astra2").is_none());
        // Same refresh token (identical login) under a new label: refused too.
        let out = save_credential_unless_duplicate_at(&path, &astra2, &creds("r0", None), &meta).unwrap();
        assert_eq!(out.evidence, IdentityEvidence::Duplicate(Account::Default));
        assert!(!out.saved);
        // Distinct seat: saved with label/identity/addedAt.
        let out = save_credential_unless_duplicate_at(&path, &astra2, &creds("r2", Some("acct-B")), &meta)
            .unwrap();
        assert_eq!(out.evidence, IdentityEvidence::NoDuplicate);
        assert!(out.saved);
        let root = read_root(&path);
        assert_eq!(root["openai-codex@astra2"]["label"], "astra2");
        assert_eq!(root["openai-codex@astra2"]["identity"], "a@example.com");
        assert_eq!(root["openai-codex@astra2"]["addedAt"], 100);
        assert_eq!(root["openai-codex"]["refresh"], "r0", "default slot untouched");
        // Re-login into the same slot with a different seat and no identity:
        // stale identity/accountId are cleared, addedAt updated.
        let out = save_credential_unless_duplicate_at(
            &path,
            &astra2,
            &creds("r3", None),
            &AccountMetadata {
                identity: None,
                added_at: Some(200),
            },
        )
        .unwrap();
        assert_eq!(out.evidence, IdentityEvidence::NoEvidence);
        assert!(out.saved);
        let root = read_root(&path);
        assert!(root["openai-codex@astra2"].get("identity").is_none());
        assert!(root["openai-codex@astra2"].get("accountId").is_none());
        assert_eq!(root["openai-codex@astra2"]["addedAt"], 200);
        assert_eq!(root["openai-codex@astra2"]["refresh"], "r3");
        // Corrupt store: refused, never reset.
        std::fs::write(&path, "{{not json").unwrap();
        assert!(save_credential_unless_duplicate_at(&path, &astra2, &creds("r4", None), &meta).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{{not json");
        assert!(save_credential_at(&path, &astra2, &creds("r4", None)).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{{not json");
    }

    #[test]
    fn inventory_flags_duplicate_seats() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        save_provider_auth_at(&path, "openai-codex", &creds("r0", Some("acct-A"))).unwrap();
        save_provider_auth_at(&path, "openai-codex@b", &creds("rb", Some("acct-A"))).unwrap();
        save_provider_auth_at(&path, "openai-codex@c", &creds("rc", Some("acct-C"))).unwrap();
        let inv = list_accounts_detailed_at(&path, Some(OAuthProviderId::OpenAiCodex)).unwrap();
        assert_eq!(
            inv.duplicate_identity_keys,
            vec!["openai-codex".to_string(), "openai-codex@b".to_string()]
        );
    }

    #[test]
    fn duplicate_identity_detects_same_seat_under_other_label() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        save_provider_auth_at(&path, "openai-codex", &creds("r0", Some("acct-A"))).unwrap();
        save_provider_auth_at(&path, "openai-codex@b", &creds("rb", Some("acct-B"))).unwrap();
        save_provider_auth_at(&path, "anthropic@x", &creds("rx", Some("acct-A"))).unwrap();
        let dup = find_duplicate_identity_at(
            &path,
            OAuthProviderId::OpenAiCodex,
            "acct-A",
            &Account::named("new").unwrap(),
        )
        .unwrap();
        assert_eq!(dup, Some(Account::Default));
        // Re-login into the same slot is not a duplicate of itself.
        assert_eq!(
            find_duplicate_identity_at(&path, OAuthProviderId::OpenAiCodex, "acct-A", &Account::Default)
                .unwrap(),
            None
        );
        // Other providers never collide; unknown ids never match; empty never matches.
        assert_eq!(
            find_duplicate_identity_at(&path, OAuthProviderId::OpenAiCodex, "acct-Z", &Account::Default)
                .unwrap(),
            None
        );
        assert_eq!(
            find_duplicate_identity_at(&path, OAuthProviderId::OpenAiCodex, "", &Account::Default)
                .unwrap(),
            None
        );
    }
}
