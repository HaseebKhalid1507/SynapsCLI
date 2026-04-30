//! Extension manager — discovers, starts, and manages extension lifecycles.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use super::hooks::HookBus;
use super::manifest::ExtensionManifest;
use super::runtime::{ExtensionHandler, ExtensionHealth};
use super::runtime::process::ProcessExtension;

/// Actionable discovery/load failure for an installed plugin extension.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionLoadFailure {
    pub plugin: String,
    pub manifest_path: Option<PathBuf>,
    pub reason: String,
    pub hint: String,
}

impl ExtensionLoadFailure {
    fn new(
        plugin: impl Into<String>,
        manifest_path: Option<PathBuf>,
        reason: impl Into<String>,
        hint: impl Into<String>,
    ) -> Self {
        Self {
            plugin: plugin.into(),
            manifest_path,
            reason: reason.into(),
            hint: hint.into(),
        }
    }

    pub fn concise_message(&self) -> String {
        match &self.manifest_path {
            Some(path) => format!(
                "{} (manifest: {}; hint: {})",
                self.reason,
                path.display(),
                self.hint
            ),
            None => format!("{} (hint: {})", self.reason, self.hint),
        }
    }
}

/// Snapshot of a loaded extension's runtime status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionStatus {
    pub id: String,
    pub health: ExtensionHealth,
    pub restart_count: usize,
}

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
        self.load_with_cwd(id, manifest, None).await
    }

    /// Load and start an extension from its manifest with a process cwd.
    pub async fn load_with_cwd(
        &mut self,
        id: &str,
        manifest: &ExtensionManifest,
        cwd: Option<std::path::PathBuf>,
    ) -> Result<(), String> {
        // Don't load duplicates
        if self.extensions.contains_key(id) {
            return Err(format!("Extension '{}' is already loaded", id));
        }

        // Validate permissions and hook subscriptions before spawning the
        // extension process. This keeps malformed manifests from leaking child
        // processes when a later subscription step fails.
        let validated = manifest.validate(id)?;
        let permissions = validated.permissions;
        let subscriptions = validated.subscriptions;

        // Spawn the extension process only after the manifest is known-good.
        let process = ProcessExtension::spawn_with_cwd(id, &manifest.command, &manifest.args, cwd.clone()).await?;
        process.initialize(cwd.clone()).await?;
        let handler: Arc<dyn ExtensionHandler> = Arc::new(process);

        // Register hook subscriptions
        for (kind, tool_filter, matcher) in subscriptions {
            self.hook_bus
                .subscribe(kind, handler.clone(), tool_filter, matcher, permissions.clone())
                .await?;
        }

        self.extensions.insert(id.to_string(), handler);
        tracing::info!(extension = %id, hooks = manifest.hooks.len(), "Extension loaded");
        Ok(())
    }

    /// Unload an extension — unsubscribe hooks and shut down the process.
    pub async fn unload(&mut self, id: &str) -> Result<(), String> {
        let handler = self
            .extensions
            .remove(id)
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

    /// Return health snapshots for all loaded extensions, sorted by ID.
    pub async fn statuses(&self) -> Vec<ExtensionStatus> {
        let mut handlers: Vec<(String, Arc<dyn ExtensionHandler>)> = self
            .extensions
            .iter()
            .map(|(id, handler)| (id.clone(), handler.clone()))
            .collect();
        handlers.sort_by(|a, b| a.0.cmp(&b.0));

        let mut statuses = Vec::with_capacity(handlers.len());
        for (id, handler) in handlers {
            statuses.push(ExtensionStatus {
                id,
                health: handler.health().await,
                restart_count: handler.restart_count().await,
            });
        }
        statuses
    }

    /// Get the shared hook bus.
    pub fn hook_bus(&self) -> &Arc<HookBus> {
        &self.hook_bus
    }

    /// Discover and load all extensions from the user and project plugin directories.
    ///
    /// Scans `~/.synaps-cli/plugins/*/.synaps-plugin/plugin.json` and
    /// `./.synaps/plugins/*/.synaps-plugin/plugin.json` for manifests that contain
    /// an `extension` field. Project-local plugins override user plugins with the
    /// same directory name.
    pub async fn discover_and_load(&mut self) -> (Vec<String>, Vec<ExtensionLoadFailure>) {
        let mut plugin_roots = vec![crate::config::base_dir().join("plugins")];
        if let Ok(cwd) = std::env::current_dir() {
            let project_plugins = cwd.join(".synaps").join("plugins");
            if project_plugins != plugin_roots[0] {
                plugin_roots.push(project_plugins);
            }
        }

        let mut plugin_dirs: HashMap<String, PathBuf> = HashMap::new();
        let mut failed: Vec<ExtensionLoadFailure> = Vec::new();

        for plugins_dir in plugin_roots {
            if !plugins_dir.exists() {
                continue;
            }

            let entries = match std::fs::read_dir(&plugins_dir) {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(path = %plugins_dir.display(), error = %e, "Failed to read plugins directory");
                    failed.push(ExtensionLoadFailure::new(
                        "plugins",
                        Some(plugins_dir.clone()),
                        format!("Failed to read plugins directory: {e}"),
                        "Check directory permissions and retry",
                    ));
                    continue;
                }
            };

            for entry in entries.flatten() {
                let plugin_name = entry.file_name().to_string_lossy().to_string();
                plugin_dirs.insert(plugin_name, entry.path());
            }
        }

        let mut plugin_dirs: Vec<(String, PathBuf)> = plugin_dirs.into_iter().collect();
        plugin_dirs.sort_by(|a, b| a.0.cmp(&b.0));

        let mut loaded = Vec::new();
        for (plugin_name, plugin_dir) in plugin_dirs {
            let manifest_path = plugin_dir.join(".synaps-plugin").join("plugin.json");
            if !manifest_path.exists() {
                continue;
            }

            let content = match std::fs::read_to_string(&manifest_path) {
                Ok(c) => c,
                Err(e) => {
                    let reason = format!("Failed to read plugin manifest: {e}");
                    tracing::warn!(plugin = %plugin_name, manifest = %manifest_path.display(), error = %e, "Failed to read plugin manifest");
                    failed.push(ExtensionLoadFailure::new(
                        plugin_name,
                        Some(manifest_path),
                        reason,
                        "Check manifest file permissions, then run `plugin validate <plugin-dir>`",
                    ));
                    continue;
                }
            };

            let json: serde_json::Value = match serde_json::from_str(&content) {
                Ok(v) => v,
                Err(e) => {
                    let reason = format!("Invalid plugin manifest JSON: {e}");
                    tracing::warn!(plugin = %plugin_name, manifest = %manifest_path.display(), error = %e, "Invalid plugin manifest JSON");
                    failed.push(ExtensionLoadFailure::new(
                        plugin_name,
                        Some(manifest_path),
                        reason,
                        "Fix JSON syntax, then run `plugin validate <plugin-dir>`",
                    ));
                    continue;
                }
            };

            let ext_value = match json.get("extension") {
                Some(v) => v.clone(),
                None => continue,
            };

            let ext_manifest: ExtensionManifest = match serde_json::from_value(ext_value) {
                Ok(m) => m,
                Err(e) => {
                    let reason = format!("Failed to parse extension manifest: {e}");
                    tracing::warn!(plugin = %plugin_name, manifest = %manifest_path.display(), error = %e, "Failed to parse extension manifest");
                    failed.push(ExtensionLoadFailure::new(
                        plugin_name,
                        Some(manifest_path),
                        reason,
                        "Check the `extension` block shape against docs/extensions/contract.json, then run `plugin validate <plugin-dir>`",
                    ));
                    continue;
                }
            };

            let command = if std::path::Path::new(&ext_manifest.command).is_absolute() {
                ext_manifest.command.clone()
            } else if !ext_manifest.command.contains(std::path::MAIN_SEPARATOR) && !ext_manifest.command.contains('/') {
                ext_manifest.command.clone()
            } else {
                plugin_dir.join(&ext_manifest.command)
                    .to_string_lossy().to_string()
            };

            let args: Vec<String> = ext_manifest.args.iter().map(|arg| {
                let arg_path = plugin_dir.join(arg);
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

            match self.load_with_cwd(&plugin_name, &resolved, Some(plugin_dir.clone())).await {
                Ok(()) => {
                    tracing::info!(plugin = %plugin_name, path = %plugin_dir.display(), "Extension loaded from plugins/");
                    loaded.push(plugin_name);
                }
                Err(e) => {
                    tracing::warn!(plugin = %plugin_name, manifest = %manifest_path.display(), error = %e, "Failed to load extension");
                    failed.push(ExtensionLoadFailure::new(
                        plugin_name,
                        Some(manifest_path),
                        e,
                        "Run `plugin validate <plugin-dir>` and confirm the extension command is installed",
                    ));
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

    #[tokio::test]
    async fn statuses_report_loaded_extension_health() {
        let bus = Arc::new(HookBus::new());
        let mut mgr = ExtensionManager::new(bus);
        let manifest = ExtensionManifest {
            protocol_version: 1,
            runtime: crate::extensions::manifest::ExtensionRuntime::Process,
            command: "python3".to_string(),
            args: vec!["tests/fixtures/process_extension.py".to_string(), "normal".to_string(), "/tmp/synaps-status-test.log".to_string()],
            permissions: vec!["tools.intercept".to_string()],
            hooks: vec![crate::extensions::manifest::HookSubscription {
                hook: "before_tool_call".to_string(),
                tool: Some("bash".to_string()),
                matcher: None,
            }],
        };

        mgr.load("status-test", &manifest).await.unwrap();

        assert_eq!(
            mgr.statuses().await,
            vec![ExtensionStatus {
                id: "status-test".to_string(),
                health: ExtensionHealth::Healthy,
                restart_count: 0,
            }]
        );

        mgr.shutdown_all().await;
    }
}
