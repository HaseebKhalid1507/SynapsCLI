//! Plugin-namespaced configuration store for extensions.
//!
//! Values are stored as simple `key = value` lines under
//! `~/.synaps-cli/plugins/<plugin-id>/config` (or the active test/profile base).
//! This is intentionally separate from the global Synaps config so rich plugins
//! can own their settings without colonizing the core keyspace.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

/// A single plugin-config change event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginConfigChange {
    pub key: String,
    pub value: Option<String>,
}

/// Compute the on-disk config path for one plugin id.
pub fn plugin_config_path(plugin_id: &str) -> PathBuf {
    crate::config::base_dir().join("plugins").join(plugin_id).join("config")
}

/// Read one plugin-owned config value.
pub fn read_plugin_config(plugin_id: &str, key: &str) -> Option<String> {
    read_plugin_config_from(&plugin_config_path(plugin_id), key)
}

/// Write one plugin-owned config value, preserving comments and unrelated keys.
pub fn write_plugin_config(plugin_id: &str, key: &str, value: &str) -> std::io::Result<()> {
    write_plugin_config_to(&plugin_config_path(plugin_id), key, value)
}

/// Read one plugin-owned config value from an explicit path (testable core).
pub fn read_plugin_config_from(path: &Path, key: &str) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let key_trimmed = key.trim();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        if k.trim() == key_trimmed {
            return Some(v.trim().to_string());
        }
    }
    None
}

/// Write one plugin-owned config value to an explicit path (testable core).
pub fn write_plugin_config_to(path: &Path, key: &str, value: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let key_trimmed = key.trim();
    let replacement = format!("{} = {}", key_trimmed, value.trim());

    let mut found = false;
    let mut new_lines: Vec<String> = existing
        .lines()
        .map(|line| {
            if found {
                return line.to_string();
            }
            let t = line.trim_start();
            if t.starts_with('#') || t.is_empty() {
                return line.to_string();
            }
            if let Some((k, _)) = t.split_once('=') {
                if k.trim() == key_trimmed {
                    found = true;
                    return replacement.clone();
                }
            }
            line.to_string()
        })
        .collect();

    if !found {
        new_lines.push(replacement);
    }
    let mut out = new_lines.join("\n");
    if !out.ends_with('\n') {
        out.push('\n');
    }

    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, out)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Subscribe to changes for selected keys in a plugin config file.
///
/// The returned watch receiver emits `Some(change)` whenever the config file is
/// created or modified and one of the watched keys changes value. An empty
/// `keys` list watches every parsed key.
pub fn subscribe_changes(
    plugin_id: &str,
    keys: Vec<String>,
) -> notify::Result<watch::Receiver<Option<PluginConfigChange>>> {
    subscribe_changes_at(plugin_config_path(plugin_id), keys)
}

/// Testable implementation of [`subscribe_changes`] for an explicit path.
pub fn subscribe_changes_at(
    path: PathBuf,
    keys: Vec<String>,
) -> notify::Result<watch::Receiver<Option<PluginConfigChange>>> {
    let (watch_tx, watch_rx) = watch::channel(None);
    let (notify_tx, notify_rx) = mpsc::channel();
    let watch_path = path.clone();
    let parent = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let watched: Vec<String> = keys.into_iter().map(|k| k.trim().to_string()).collect();

    std::fs::create_dir_all(&parent).map_err(notify::Error::io)?;

    let mut watcher = RecommendedWatcher::new(
        move |res| {
            let _ = notify_tx.send(res);
        },
        notify::Config::default().with_poll_interval(Duration::from_millis(50)),
    )?;
    watcher.watch(&parent, RecursiveMode::NonRecursive)?;

    std::thread::spawn(move || {
        let _keep_watcher_alive = watcher;
        let mut previous = parse_config_file(&watch_path);
        while let Ok(event) = notify_rx.recv() {
            let Ok(event) = event else { continue };
            if !matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_)) {
                continue;
            }
            if !event.paths.iter().any(|p| p == &watch_path) {
                continue;
            }
            let current = parse_config_file(&watch_path);
            for (key, value) in &current {
                if !watched.is_empty() && !watched.iter().any(|k| k == key) {
                    continue;
                }
                if previous.get(key) != Some(value) {
                    let _ = watch_tx.send(Some(PluginConfigChange {
                        key: key.clone(),
                        value: Some(value.clone()),
                    }));
                    break;
                }
            }
            previous = current;
        }
    });

    Ok(watch_rx)
}

fn parse_config_file(path: &Path) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    let Ok(content) = std::fs::read_to_string(path) else {
        return out;
    };
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        out.insert(k.trim().to_string(), v.trim().to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_plugin_config_reads_exact_key_from_plugin_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        std::fs::write(&path, "# comment\nmodel_path = /tmp/a.bin\nbackend = cpu\n").unwrap();

        assert_eq!(
            read_plugin_config_from(&path, "model_path"),
            Some("/tmp/a.bin".to_string())
        );
        assert_eq!(read_plugin_config_from(&path, "missing"), None);
    }

    #[test]
    fn write_plugin_config_replaces_existing_key_and_preserves_comments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        std::fs::write(&path, "# keep\nbackend = auto\nmodel_path = old\n").unwrap();

        write_plugin_config_to(&path, "backend", "cpu").unwrap();

        let content = std::fs::read_to_string(path).unwrap();
        assert!(content.contains("# keep"));
        assert!(content.contains("backend = cpu"));
        assert!(content.contains("model_path = old"));
    }

    #[test]
    fn write_plugin_config_creates_parent_directory_and_appends_missing_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plugins").join("capture").join("config");

        write_plugin_config_to(&path, "language", "auto").unwrap();

        assert_eq!(read_plugin_config_from(&path, "language"), Some("auto".to_string()));
    }

    #[tokio::test]
    async fn subscribe_changes_notifies_for_watched_key_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        std::fs::write(&path, "backend = auto\nignored = old\n").unwrap();

        let mut rx = subscribe_changes_at(path.clone(), vec!["backend".to_string()]).unwrap();
        write_plugin_config_to(&path, "ignored", "new").unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(rx.borrow().is_none());

        write_plugin_config_to(&path, "backend", "cpu").unwrap();
        tokio::time::timeout(Duration::from_secs(2), rx.changed()).await.unwrap().unwrap();
        assert_eq!(
            rx.borrow().clone(),
            Some(PluginConfigChange { key: "backend".to_string(), value: Some("cpu".to_string()) })
        );
    }
}
