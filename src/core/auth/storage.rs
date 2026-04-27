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
pub fn save_provider_auth(provider: &str, creds: &OAuthCredentials) -> std::result::Result<(), String> {
    let path = crate::config::resolve_write_path("auth.json");
    save_provider_auth_at(&path, provider, creds)
}

/// Path-explicit variant of `save_provider_auth`. Splits out the I/O so
/// the corrupt-file fallback path can be unit-tested without touching the
/// user's real `~/.synaps-cli/auth.json`.
fn save_provider_auth_at(
    path: &std::path::Path,
    provider: &str,
    creds: &OAuthCredentials,
) -> std::result::Result<(), String> {
    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create {}: {}", parent.display(), e))?;
    }

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
                serde_json::Map::new()
            }
        }
    } else {
        serde_json::Map::new()
    };

    root.insert(
        provider.to_string(),
        serde_json::to_value(creds).map_err(|e| format!("Failed to serialize auth: {}", e))?,
    );

    let json = serde_json::to_string_pretty(&root)
        .map_err(|e| format!("Failed to serialize auth: {}", e))?;

    std::fs::write(path, &json)
        .map_err(|e| format!("Failed to write {}: {}", path.display(), e))?;

    // Set file permissions to 600 (owner read/write only)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(path, perms)
            .map_err(|e| format!("Failed to set permissions on {}: {}", path.display(), e))?;
    }

    Ok(())
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
        assert!(parsed.get("anthropic").is_some(), "must keep anthropic entry");
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
        let parsed: serde_json::Value = serde_json::from_str(&content)
            .expect("file must now contain valid JSON");
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
}
