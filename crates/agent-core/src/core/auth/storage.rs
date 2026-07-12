use std::path::PathBuf;

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
pub fn load_provider_auth(provider: &str) -> std::result::Result<Option<OAuthCredentials>, String> {
    let path = auth_file_path();
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&path)
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
fn save_provider_auth_at(
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

    Ok(())
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
}
