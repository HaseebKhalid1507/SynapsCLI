//! Per-provider trust state (enable/disable controls).
//!
//! Trust decisions are local and user-owned. State is persisted under
//! `$SYNAPS_BASE_DIR/extensions/trust.json`. Enabled-by-default semantics:
//! a provider with no entry is considered enabled. Users explicitly
//! disable providers they distrust.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ProviderTrustState {
    /// Map of `runtime_id` (`<plugin_id>:<provider_id>`) to disabled flag.
    /// Absence means trusted/enabled by default.
    #[serde(default)]
    pub disabled: BTreeMap<String, ProviderTrustEntry>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ProviderTrustEntry {
    pub disabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Path to the trust file under the active base dir. Caller is responsible for
/// creating parent directories when writing.
pub fn trust_file_path() -> PathBuf {
    trust_file_path_for(&crate::config::base_dir())
}

/// Path to the trust file rooted at an explicit base dir (test helper / reuse).
pub(crate) fn trust_file_path_for(base: &Path) -> PathBuf {
    base.join("extensions").join("trust.json")
}

/// Load the persisted state. Missing file → `Default::default()`. IO errors → Err.
/// Malformed JSON → Err with a descriptive message.
pub fn load_trust_state() -> Result<ProviderTrustState, String> {
    load_trust_state_from(&crate::config::base_dir())
}

/// Load state from an explicit base dir.
pub(crate) fn load_trust_state_from(base: &Path) -> Result<ProviderTrustState, String> {
    let path = trust_file_path_for(base);
    match std::fs::read_to_string(&path) {
        Ok(contents) => serde_json::from_str(&contents).map_err(|e| {
            format!("failed to parse trust.json at {}: {}", path.display(), e)
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ProviderTrustState::default()),
        Err(e) => Err(format!(
            "failed to read trust.json at {}: {}",
            path.display(),
            e
        )),
    }
}

/// Persist the state. Creates parent dirs if needed. Atomic via tempfile + rename.
pub fn save_trust_state(state: &ProviderTrustState) -> Result<(), String> {
    save_trust_state_to(&crate::config::base_dir(), state)
}

/// Persist state under an explicit base dir.
pub(crate) fn save_trust_state_to(base: &Path, state: &ProviderTrustState) -> Result<(), String> {
    let path = trust_file_path_for(base);
    let parent = path.parent().ok_or_else(|| {
        format!("trust.json path has no parent: {}", path.display())
    })?;
    std::fs::create_dir_all(parent).map_err(|e| {
        format!("failed to create dir {}: {}", parent.display(), e)
    })?;
    let serialized = serde_json::to_string_pretty(state)
        .map_err(|e| format!("failed to serialize trust state: {}", e))?;

    // Atomic write: write to a sibling temp file then rename over the target.
    let tmp_path = path.with_extension("json.tmp");
    std::fs::write(&tmp_path, serialized.as_bytes()).map_err(|e| {
        format!("failed to write {}: {}", tmp_path.display(), e)
    })?;
    std::fs::rename(&tmp_path, &path).map_err(|e| {
        format!(
            "failed to rename {} -> {}: {}",
            tmp_path.display(),
            path.display(),
            e
        )
    })?;
    Ok(())
}

/// Returns true if the runtime_id is permitted to be routed (i.e. NOT disabled).
/// Default: true (enabled when absent).
pub fn is_provider_enabled(state: &ProviderTrustState, runtime_id: &str) -> bool {
    match state.disabled.get(runtime_id) {
        Some(entry) => !entry.disabled,
        None => true,
    }
}

/// Record a disabled decision. Replaces any existing entry for the runtime_id.
pub fn disable_provider(
    state: &mut ProviderTrustState,
    runtime_id: &str,
    reason: Option<String>,
) {
    state.disabled.insert(
        runtime_id.to_string(),
        ProviderTrustEntry {
            disabled: true,
            reason,
        },
    );
}

/// Re-enable a previously disabled provider. Removes the entry.
pub fn enable_provider(state: &mut ProviderTrustState, runtime_id: &str) {
    state.disabled.remove(runtime_id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn enabled_by_default_when_entry_absent() {
        let state = ProviderTrustState::default();
        assert!(is_provider_enabled(&state, "plug:prov"));
    }

    #[test]
    fn disabled_entry_makes_provider_not_enabled() {
        let mut state = ProviderTrustState::default();
        state.disabled.insert(
            "plug:prov".to_string(),
            ProviderTrustEntry {
                disabled: true,
                reason: None,
            },
        );
        assert!(!is_provider_enabled(&state, "plug:prov"));
    }

    #[test]
    fn disable_then_check() {
        let mut state = ProviderTrustState::default();
        disable_provider(&mut state, "plug:prov", Some("untrusted".into()));
        assert!(!is_provider_enabled(&state, "plug:prov"));
        let entry = state.disabled.get("plug:prov").unwrap();
        assert!(entry.disabled);
        assert_eq!(entry.reason.as_deref(), Some("untrusted"));
    }

    #[test]
    fn enable_after_disable_removes_entry() {
        let mut state = ProviderTrustState::default();
        disable_provider(&mut state, "plug:prov", None);
        enable_provider(&mut state, "plug:prov");
        assert!(state.disabled.get("plug:prov").is_none());
        assert!(is_provider_enabled(&state, "plug:prov"));
    }

    #[test]
    fn load_from_missing_file_returns_default() {
        let dir = TempDir::new().unwrap();
        let state = load_trust_state_from(dir.path()).unwrap();
        assert_eq!(state, ProviderTrustState::default());
    }

    #[test]
    fn save_then_load_round_trip() {
        let dir = TempDir::new().unwrap();
        let mut state = ProviderTrustState::default();
        disable_provider(&mut state, "plug:prov", Some("nope".into()));
        disable_provider(&mut state, "other:thing", None);
        save_trust_state_to(dir.path(), &state).unwrap();
        let loaded = load_trust_state_from(dir.path()).unwrap();
        assert_eq!(loaded, state);
    }

    #[test]
    fn malformed_json_errors_with_context() {
        let dir = TempDir::new().unwrap();
        let path = trust_file_path_for(dir.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ this is not json").unwrap();
        let err = load_trust_state_from(dir.path()).unwrap_err();
        assert!(err.contains("trust.json"), "error should mention trust.json: {}", err);
    }

    #[test]
    fn save_is_atomic_replacement() {
        let dir = TempDir::new().unwrap();
        let mut s1 = ProviderTrustState::default();
        disable_provider(&mut s1, "a:b", None);
        save_trust_state_to(dir.path(), &s1).unwrap();

        let mut s2 = ProviderTrustState::default();
        disable_provider(&mut s2, "c:d", Some("reason".into()));
        disable_provider(&mut s2, "e:f", None);
        save_trust_state_to(dir.path(), &s2).unwrap();

        let loaded = load_trust_state_from(dir.path()).unwrap();
        assert_eq!(loaded, s2);
        // Ensure no stale temp file left behind.
        let tmp = trust_file_path_for(dir.path()).with_extension("json.tmp");
        assert!(!tmp.exists(), "temp file should not remain after rename");
    }
}
