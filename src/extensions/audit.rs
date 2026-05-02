//! Append-only provider invocation audit log.
//!
//! Records minimal metadata for each provider routing event without
//! storing prompts or tool payloads. File: `$SYNAPS_BASE_DIR/extensions/audit.jsonl`.
//!
//! Design constraints:
//!
//! - One JSON object per line; missing file is equivalent to no entries.
//! - Append-only: each new entry uses `O_APPEND` so concurrent appenders
//!   on Unix produce well-formed line records without locking.
//! - Malformed lines (e.g. partial write from a crash) are skipped on read
//!   with a `tracing::warn!` so a corrupt line cannot lock the user out.
//! - Never contains prompt text, tool inputs, tool outputs, or tokens.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ProviderAuditEntry {
    /// RFC3339 UTC timestamp.
    pub timestamp: String,
    pub plugin_id: String,
    pub provider_id: String,
    pub model_id: String,
    /// Whether Synaps exposed any tool schemas to this invocation.
    pub tools_exposed: bool,
    /// Number of tool calls the provider requested during this invocation.
    /// 0 if no tool-use loop ran.
    #[serde(default)]
    pub tools_requested: u32,
    /// Whether the invocation streamed (vs. provider.complete).
    #[serde(default)]
    pub streamed: bool,
    /// "ok" | "error" | "blocked" — high-level outcome.
    pub outcome: String,
    /// Optional short error class (e.g. "trust_disabled", "ipc_error", "timeout").
    /// MUST NOT contain prompt or tool content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_class: Option<String>,
}

/// Path to the audit file under the active base dir. Caller is responsible for
/// creating parent directories when writing.
pub fn audit_file_path() -> PathBuf {
    audit_file_path_for(&crate::config::base_dir())
}

/// Path to the audit file rooted at an explicit base dir (test helper / reuse).
pub(crate) fn audit_file_path_for(base: &Path) -> PathBuf {
    base.join("extensions").join("audit.jsonl")
}

/// Build a fresh entry with the current UTC timestamp.
#[allow(clippy::too_many_arguments)]
pub fn new_audit_entry(
    plugin_id: impl Into<String>,
    provider_id: impl Into<String>,
    model_id: impl Into<String>,
    tools_exposed: bool,
    tools_requested: u32,
    streamed: bool,
    outcome: impl Into<String>,
    error_class: Option<String>,
) -> ProviderAuditEntry {
    ProviderAuditEntry {
        timestamp: chrono::Utc::now().to_rfc3339(),
        plugin_id: plugin_id.into(),
        provider_id: provider_id.into(),
        model_id: model_id.into(),
        tools_exposed,
        tools_requested,
        streamed,
        outcome: outcome.into(),
        error_class,
    }
}

/// Append a single entry as one JSON line. Creates parent dirs and the file
/// if missing. Atomic per-line via `O_APPEND` on Unix.
pub fn append_audit_entry(entry: &ProviderAuditEntry) -> Result<(), String> {
    append_audit_entry_to(&crate::config::base_dir(), entry)
}

/// Append an entry under an explicit base dir.
pub(crate) fn append_audit_entry_to(
    base: &Path,
    entry: &ProviderAuditEntry,
) -> Result<(), String> {
    let path = audit_file_path_for(base);
    let parent = path
        .parent()
        .ok_or_else(|| format!("audit.jsonl path has no parent: {}", path.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("failed to create dir {}: {}", parent.display(), e))?;

    let mut line = serde_json::to_string(entry)
        .map_err(|e| format!("failed to serialize audit entry: {}", e))?;
    line.push('\n');

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("failed to open {}: {}", path.display(), e))?;
    file.write_all(line.as_bytes())
        .map_err(|e| format!("failed to append to {}: {}", path.display(), e))?;
    Ok(())
}

/// Read all entries (one per line). Missing file → empty Vec. Malformed
/// lines are skipped with a `tracing::warn!`.
pub fn read_audit_entries() -> Result<Vec<ProviderAuditEntry>, String> {
    read_audit_entries_from(&crate::config::base_dir())
}

/// Read entries under an explicit base dir.
pub(crate) fn read_audit_entries_from(
    base: &Path,
) -> Result<Vec<ProviderAuditEntry>, String> {
    let path = audit_file_path_for(base);
    let contents = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(format!(
                "failed to read audit.jsonl at {}: {}",
                path.display(),
                e
            ));
        }
    };
    let mut entries = Vec::new();
    for (idx, raw) in contents.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<ProviderAuditEntry>(line) {
            Ok(entry) => entries.push(entry),
            Err(e) => {
                tracing::warn!(
                    target: "synaps::extensions::audit",
                    "skipping malformed audit.jsonl line {} at {}: {}",
                    idx + 1,
                    path.display(),
                    e
                );
            }
        }
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample(plugin: &str, outcome: &str) -> ProviderAuditEntry {
        ProviderAuditEntry {
            timestamp: "2025-01-01T00:00:00Z".to_string(),
            plugin_id: plugin.to_string(),
            provider_id: "p".to_string(),
            model_id: "m".to_string(),
            tools_exposed: false,
            tools_requested: 0,
            streamed: false,
            outcome: outcome.to_string(),
            error_class: None,
        }
    }

    #[test]
    fn audit_file_path_is_under_extensions_dir() {
        let dir = TempDir::new().unwrap();
        let p = audit_file_path_for(dir.path());
        assert_eq!(p, dir.path().join("extensions").join("audit.jsonl"));
    }

    #[test]
    fn append_two_entries_then_read_returns_them_in_order() {
        let dir = TempDir::new().unwrap();
        let a = sample("plug-a", "ok");
        let b = sample("plug-b", "blocked");
        append_audit_entry_to(dir.path(), &a).unwrap();
        append_audit_entry_to(dir.path(), &b).unwrap();
        let entries = read_audit_entries_from(dir.path()).unwrap();
        assert_eq!(entries, vec![a, b]);
    }

    #[test]
    fn read_missing_file_returns_empty() {
        let dir = TempDir::new().unwrap();
        let entries = read_audit_entries_from(dir.path()).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn malformed_line_in_middle_is_skipped() {
        let dir = TempDir::new().unwrap();
        let a = sample("plug-a", "ok");
        let c = sample("plug-c", "error");
        append_audit_entry_to(dir.path(), &a).unwrap();
        // Inject a malformed line.
        let path = audit_file_path_for(dir.path());
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"{ this is not valid json\n").unwrap();
        drop(f);
        append_audit_entry_to(dir.path(), &c).unwrap();

        let entries = read_audit_entries_from(dir.path()).unwrap();
        assert_eq!(entries, vec![a, c]);
    }

    #[test]
    fn concurrent_appenders_produce_full_record_count() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().to_path_buf();
        let mut handles = Vec::new();
        for t in 0..4u32 {
            let base = base.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..10u32 {
                    let mut e = sample(&format!("plug-{t}"), "ok");
                    e.tools_requested = i;
                    append_audit_entry_to(&base, &e).expect("append");
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let entries = read_audit_entries_from(&base).unwrap();
        assert_eq!(entries.len(), 40);
    }

    #[test]
    fn new_audit_entry_produces_rfc3339_timestamp() {
        let e = new_audit_entry(
            "plug",
            "prov",
            "model",
            true,
            0,
            false,
            "ok",
            None,
        );
        // Year-4-digits + 'T' separator + tz suffix ('Z' or '+'/'-' offset).
        let ts = &e.timestamp;
        assert!(ts.len() >= 20, "timestamp too short: {ts}");
        assert!(
            ts.chars().take(4).all(|c| c.is_ascii_digit()),
            "expected 4-digit year: {ts}"
        );
        assert!(ts.contains('T'), "expected 'T' separator: {ts}");
        assert!(
            ts.ends_with('Z') || ts.contains('+') || ts[10..].contains('-'),
            "expected timezone suffix: {ts}"
        );
        // chrono should be able to round-trip its own RFC3339 output.
        chrono::DateTime::parse_from_rfc3339(ts)
            .unwrap_or_else(|err| panic!("parse_from_rfc3339({ts}) failed: {err}"));
    }

    #[test]
    fn round_trip_with_error_class_omitted_when_none() {
        let dir = TempDir::new().unwrap();
        let mut e = sample("plug", "ok");
        e.error_class = None;
        append_audit_entry_to(dir.path(), &e).unwrap();
        let raw = std::fs::read_to_string(audit_file_path_for(dir.path())).unwrap();
        assert!(
            !raw.contains("error_class"),
            "error_class should be skipped when None: {raw}"
        );
        let loaded = read_audit_entries_from(dir.path()).unwrap();
        assert_eq!(loaded, vec![e]);
    }
}
