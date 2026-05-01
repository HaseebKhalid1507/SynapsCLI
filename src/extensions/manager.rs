//! Extension manager — discovers, starts, and manages extension lifecycles.

use std::collections::HashMap;
use std::sync::Arc;

use super::hooks::HookBus;
use super::hooks::events::HookKind;
use super::manifest::ExtensionManifest;
use super::permissions::PermissionSet;
use super::runtime::ExtensionHandler;
use super::runtime::process::ProcessExtension;

/// Manages the lifecycle of all loaded extensions.
pub struct ExtensionManager {
    /// The shared hook bus.
    hook_bus: Arc<HookBus>,
    /// Running extensions keyed by ID.
    extensions: HashMap<String, Arc<dyn ExtensionHandler>>,
}

impl ExtensionManager {
    /// Create a new manager with a shared hook bus.
    pub fn new(hook_bus: Arc<HookBus>) -> Self {
        Self {
            hook_bus,
            extensions: HashMap::new(),
        }
    }

    /// Load and start an extension from its manifest.
    pub async fn load(
        &mut self,
        id: &str,
        manifest: &ExtensionManifest,
    ) -> Result<(), String> {
        // Don't load duplicates
        if self.extensions.contains_key(id) {
            return Err(format!("Extension '{}' is already loaded", id));
        }

        // Spawn the extension process
        let handler: Arc<dyn ExtensionHandler> = Arc::new(
            ProcessExtension::spawn(id, &manifest.command, &manifest.args).await?
        );

        // Parse permissions
        let permissions = PermissionSet::from_strings(&manifest.permissions);

        // Register hook subscriptions
        for sub in &manifest.hooks {
            let kind = match HookKind::from_str(&sub.hook) {
                Some(k) => k,
                None => {
                    // Rollback: unsubscribe any registered hooks and shutdown process
                    self.hook_bus.unsubscribe_all(id).await;
                    handler.shutdown().await;
                    return Err(format!("Unknown hook kind: '{}' in extension '{}'", sub.hook, id));
                }
            };

            if let Err(e) = self.hook_bus
                .subscribe(kind, handler.clone(), sub.tool.clone(), permissions.clone())
                .await
            {
                // Rollback: unsubscribe any registered hooks and shutdown process
                self.hook_bus.unsubscribe_all(id).await;
                handler.shutdown().await;
                return Err(e);
            }
        }

        self.extensions.insert(id.to_string(), handler);
        tracing::info!(extension = %id, hooks = manifest.hooks.len(), "Extension loaded");
        Ok(())
    }

    /// Unload an extension — unsubscribe hooks and shut down the process.
    pub async fn unload(&mut self, id: &str) -> Result<(), String> {
        let handler = self.extensions.remove(id)
            .ok_or_else(|| format!("Extension '{}' not found", id))?;

        self.hook_bus.unsubscribe_all(id).await;
        handler.shutdown().await;

        tracing::info!(extension = %id, "Extension unloaded");
        Ok(())
    }

    /// Shut down all extensions gracefully.
    pub async fn shutdown_all(&mut self) {
        let ids: Vec<String> = self.extensions.keys().cloned().collect();
        for id in ids {
            let _ = self.unload(&id).await;
        }
    }

    /// List running extension IDs.
    pub fn list(&self) -> Vec<&str> {
        self.extensions.keys().map(|s| s.as_str()).collect()
    }

    /// Number of running extensions.
    pub fn count(&self) -> usize {
        self.extensions.len()
    }

    /// Get the shared hook bus.
    pub fn hook_bus(&self) -> &Arc<HookBus> {
        &self.hook_bus
    }

    /// Discover and load all extensions from the plugins directory.
    ///
    /// Scans `~/.synaps-cli/plugins/*/plugin.json` for manifests that
    /// contain an `extension` field. Loads each one via `self.load()`.
    pub async fn discover_and_load(&mut self) -> (Vec<String>, Vec<(String, String)>) {
        let plugins_dir = crate::config::base_dir().join("plugins");
        let mut loaded = Vec::new();
        let mut failed: Vec<(String, String)> = Vec::new();

        if !plugins_dir.exists() {
            return (loaded, failed);
        }

        let entries = match std::fs::read_dir(&plugins_dir) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to read plugins directory");
                return (loaded, failed);
            }
        };

        for entry in entries.flatten() {
            let manifest_path = entry.path().join(".synaps-plugin").join("plugin.json");
            if !manifest_path.exists() {
                continue;
            }

            let content = match std::fs::read_to_string(&manifest_path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            // Parse the full manifest to check for extension field
            let json: serde_json::Value = match serde_json::from_str(&content) {
                Ok(v) => v,
                Err(_) => continue,
            };

            // Only load plugins that have an "extension" field
            let ext_value = match json.get("extension") {
                Some(v) => v.clone(),
                None => continue,
            };

            let ext_manifest: ExtensionManifest = match serde_json::from_value(ext_value) {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!(
                        plugin = %entry.path().display(),
                        error = %e,
                        "Failed to parse extension manifest"
                    );
                    continue;
                }
            };

            let plugin_name = entry.file_name().to_string_lossy().to_string();
            let plugin_dir = entry.path();

            // Resolve command relative to plugin directory
            let command = if std::path::Path::new(&ext_manifest.command).is_absolute() {
                ext_manifest.command.clone()
            } else if !ext_manifest.command.contains(std::path::MAIN_SEPARATOR) && !ext_manifest.command.contains('/') {
                // No path separator = bare executable name (interpreter like python3, node, ruby)
                // Resolve via PATH, don't join with plugin directory
                ext_manifest.command.clone()
            } else {
                // Relative path — resolve and confine to plugin directory
                let resolved = plugin_dir.join(&ext_manifest.command);
                match resolved.canonicalize() {
                    Ok(canonical) if canonical.starts_with(&plugin_dir) => {
                        canonical.to_string_lossy().to_string()
                    }
                    Ok(canonical) => {
                        tracing::warn!(
                            "Extension '{}' command escapes plugin directory: {} → {}",
                            plugin_name, ext_manifest.command, canonical.display()
                        );
                        continue;
                    }
                    Err(_) => {
                        tracing::warn!(
                            "Extension '{}' command not found: {}",
                            plugin_name, ext_manifest.command
                        );
                        continue;
                    }
                }
            };

            // Resolve args relative to plugin directory
            let args: Vec<String> = ext_manifest.args.iter().map(|arg| {
                let arg_path = plugin_dir.join(arg);
                // Only resolve if path exists AND stays within the plugin directory
                // (prevents path traversal via ../../etc/passwd)
                if arg_path.exists() {
                    if let (Ok(canonical), Ok(plugin_canonical)) = (
                        arg_path.canonicalize(),
                        plugin_dir.canonicalize(),
                    ) {
                        if canonical.starts_with(&plugin_canonical) {
                            return canonical.to_string_lossy().to_string();
                        }
                    }
                }
                arg.clone()
            }).collect();

            let resolved = ExtensionManifest {
                command,
                args,
                ..ext_manifest
            };

            match self.load(&plugin_name, &resolved).await {
                Ok(()) => {
                    tracing::info!(plugin = %plugin_name, "Extension loaded from plugins/");
                    loaded.push(plugin_name);
                }
                Err(e) => {
                    tracing::warn!(plugin = %plugin_name, error = %e, "Failed to load extension");
                    failed.push((plugin_name, e));
                }
            }
        }

        (loaded, failed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn new_manager_has_no_extensions() {
        let bus = Arc::new(HookBus::new());
        let mgr = ExtensionManager::new(bus);
        assert_eq!(mgr.count(), 0);
        assert!(mgr.list().is_empty());
    }

    #[tokio::test]
    async fn unload_nonexistent_returns_error() {
        let bus = Arc::new(HookBus::new());
        let mut mgr = ExtensionManager::new(bus);
        let result = mgr.unload("nope").await;
        assert!(result.is_err());
    }
}
