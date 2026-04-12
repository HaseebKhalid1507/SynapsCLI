use std::path::PathBuf;
use std::sync::OnceLock;

static PROFILE_NAME: OnceLock<Option<String>> = OnceLock::new();

/// Returns the active profile name, if any.
/// Reads from `SYNAPS_PROFILE` environment variable if not already set programmatically.
pub fn get_profile() -> Option<String> {
    PROFILE_NAME.get_or_init(|| std::env::var("SYNAPS_PROFILE").ok()).clone()
}

/// Sets the active profile name. Must be called before any `get_profile()` call
/// (i.e., before config resolution begins). Uses OnceLock — first write wins,
/// subsequent calls are no-ops. No env var mutation (unsafe under tokio).
pub fn set_profile(name: Option<String>) {
    let _ = PROFILE_NAME.set(name);
}

pub fn base_dir() -> PathBuf {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".synaps-cli")
}

/// Resolves a path for reading. Checks the profile folder first, then falls back to the default folder.
pub fn resolve_read_path(filename: &str) -> PathBuf {
    let base = base_dir();
    
    if let Some(profile) = get_profile() {
        let profile_path = base.join(&profile).join(filename);
        if profile_path.exists() {
            return profile_path;
        }
    }
    
    base.join(filename)
}

/// Resolves a path for reading with an extended arbitrary path tree.
pub fn resolve_read_path_extended(path: &str) -> PathBuf {
    let base = base_dir();
    
    if let Some(profile) = get_profile() {
        let profile_path = base.join(&profile).join(path);
        if profile_path.exists() {
            return profile_path;
        }
    }
    
    base.join(path)
}

/// Resolves a path for writing. Unconditionally writes to the profile folder if a profile is active.
pub fn resolve_write_path(filename: &str) -> PathBuf {
    let mut base = base_dir();
    
    if let Some(profile) = get_profile() {
        base.push(profile);
    }
    
    let _ = std::fs::create_dir_all(&base);
    base.join(filename)
}

/// Gets the absolute directory for the current profile (or root if default).
pub fn get_active_config_dir() -> PathBuf {
    let mut base = base_dir();
    if let Some(profile) = get_profile() {
        base.push(profile);
    }
    base
}
