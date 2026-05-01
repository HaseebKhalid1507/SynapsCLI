//! Extension manager — discovers, starts, and manages extension lifecycles.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use super::config::{diagnose_extension_config, ExtensionConfigDiagnostics};
use super::hooks::HookBus;
use super::manifest::{ExtensionConfigEntry, ExtensionManifest};
use super::providers::{ProviderRegistry, RegisteredProvider, RegisteredProviderSummary};
use super::runtime::{ExtensionHandler, ExtensionHealth};
use super::runtime::process::ProcessExtension;
use serde_json::{Map, Value};

fn project_plugins_disabled() -> bool {
    std::env::var("SYNAPS_DISABLE_PROJECT_PLUGINS")
        .map(|value| {
            let normalized = value.trim().to_ascii_lowercase();
            matches!(normalized.as_str(), "1" | "true" | "yes" | "on")
        })
        .unwrap_or(false)
}

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
    /// Optional shared tool registry for extension-provided tools.
    tools: Option<Arc<tokio::sync::RwLock<crate::ToolRegistry>>>,
    /// Provider metadata registered by loaded extensions. Routing is not wired yet.
    providers: ProviderRegistry,
    /// Running extensions keyed by ID.
    extensions: HashMap<String, Arc<dyn ExtensionHandler>>,
    /// Declared manifest config entries per loaded extension, kept so we can
    /// produce diagnostics without re-reading the manifest.
    manifest_configs: HashMap<String, Vec<ExtensionConfigEntry>>,
}

impl ExtensionManager {
    /// Create a new manager with a shared hook bus.
    pub fn new(hook_bus: Arc<HookBus>) -> Self {
        Self {
            hook_bus,
            tools: None,
            providers: ProviderRegistry::new(),
            extensions: HashMap::new(),
            manifest_configs: HashMap::new(),
        }
    }

    /// Create a new manager with shared hook bus and tool registry.
    pub fn new_with_tools(
        hook_bus: Arc<HookBus>,
        tools: Arc<tokio::sync::RwLock<crate::ToolRegistry>>,
    ) -> Self {
        Self {
            hook_bus,
            tools: Some(tools),
            providers: ProviderRegistry::new(),
            extensions: HashMap::new(),
            manifest_configs: HashMap::new(),
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
        let config = Self::resolve_config(id, &manifest.config)?;
        self.load_with_cwd_and_config(id, manifest, cwd, config).await
    }

    async fn load_with_cwd_and_config(
        &mut self,
        id: &str,
        manifest: &ExtensionManifest,
        cwd: Option<std::path::PathBuf>,
        config: Value,
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
        let capabilities = match process.initialize(cwd.clone(), config.clone()).await {
            Ok(capabilities) => capabilities,
            Err(error) => {
                process.shutdown().await;
                return Err(error);
            }
        };
        let registered_tools = capabilities.tools;
        let registered_providers = capabilities.providers;
        let handler: Arc<dyn ExtensionHandler> = Arc::new(process);
        if !registered_tools.is_empty() && !permissions.has(crate::extensions::permissions::Permission::ToolsRegister) {
            handler.shutdown().await;
            return Err(format!(
                "Extension '{}' registered tools but lacks permission 'tools.register'",
                id
            ));
        }
        if !registered_providers.is_empty() && !permissions.has(crate::extensions::permissions::Permission::ProvidersRegister) {
            handler.shutdown().await;
            return Err(format!(
                "Extension '{}' registered providers but lacks permission 'providers.register'",
                id
            ));
        }
        if !registered_providers.is_empty() {
            let mut registered_ids = Vec::new();
            for provider in registered_providers {
                if let Err(error) = Self::validate_provider_config_requirements(id, &provider, &config) {
                    self.providers.unregister_plugin(id);
                    handler.shutdown().await;
                    return Err(error);
                }
                match self.providers.register_with_handler(id, provider, Some(handler.clone())) {
                    Ok(runtime_id) => registered_ids.push(runtime_id),
                    Err(error) => {
                        self.providers.unregister_plugin(id);
                        handler.shutdown().await;
                        return Err(error);
                    }
                }
            }
            tracing::info!(extension = %id, providers = ?registered_ids, "Extension provider metadata registered");
            // Warn for tool-use-capable providers so authors and users can audit them.
            for runtime_id in &registered_ids {
                if let Some(provider) = self.providers.get(runtime_id) {
                    let tool_use = provider.spec.models.iter().any(|m| {
                        m.capabilities
                            .get("tool_use")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false)
                    });
                    if tool_use {
                        tracing::warn!(
                            "Provider '{}' is tool-use capable: it can request Synaps tools through provider mediation. Use `/extensions trust disable {}` to block routing.",
                            runtime_id,
                            runtime_id,
                        );
                    }
                }
            }
        }
        if !registered_tools.is_empty() {
            let Some(tools) = &self.tools else {
                handler.shutdown().await;
                return Err(format!(
                    "Extension '{}' registered tools but no tool registry is available",
                    id
                ));
            };
            let mut registry = tools.write().await;
            for spec in registered_tools {
                registry.register(Arc::new(crate::tools::ExtensionTool::new(id, spec, handler.clone())));
            }
        }

        // Register hook subscriptions
        for (kind, tool_filter, matcher) in subscriptions {
            self.hook_bus
                .subscribe(kind, handler.clone(), tool_filter, matcher, permissions.clone())
                .await?;
        }

        self.extensions.insert(id.to_string(), handler);
        self.manifest_configs
            .insert(id.to_string(), manifest.config.clone());
        tracing::info!(extension = %id, hooks = manifest.hooks.len(), "Extension loaded");
        Ok(())
    }

    fn validate_provider_config_requirements(
        id: &str,
        provider: &crate::extensions::runtime::process::RegisteredProviderSpec,
        config: &Value,
    ) -> Result<(), String> {
        let Some(required) = provider
            .config_schema
            .as_ref()
            .and_then(|schema| schema.get("required"))
            .and_then(Value::as_array) else {
            return Ok(());
        };
        for key in required {
            let Some(key) = key.as_str() else {
                return Err(format!(
                    "Extension '{}' provider '{}' config_schema.required must contain only strings",
                    id, provider.id,
                ));
            };
            let present = config
                .as_object()
                .map(|map| map.contains_key(key))
                .unwrap_or(false);
            if !present {
                return Err(format!(
                    "Extension '{}' provider '{}' missing required provider config '{}'",
                    id, provider.id, key,
                ));
            }
        }
        Ok(())
    }

    fn resolve_config(id: &str, entries: &[ExtensionConfigEntry]) -> Result<Value, String> {
        let mut out = Map::new();
        for entry in entries {
            let key = entry.key.trim();
            if key.is_empty() {
                return Err(format!("Extension '{}' declares config with empty key", id));
            }
            if key.contains('.') || key.contains('/') || key.contains(' ') {
                return Err(format!(
                    "Extension '{}' config key '{}' must not contain dots, slashes, or spaces",
                    id, key,
                ));
            }
            let config_key = format!("extension.{}.{}", id, key);
            if let Ok(value) = std::env::var(format!("SYNAPS_EXTENSION_{}_{}", id.replace('-', "_").to_ascii_uppercase(), key.replace('-', "_").to_ascii_uppercase())) {
                out.insert(key.to_string(), Value::String(value));
                continue;
            }
            if let Some(secret_env) = &entry.secret_env {
                if let Ok(value) = std::env::var(secret_env) {
                    out.insert(key.to_string(), Value::String(value));
                    continue;
                }
            }
            if let Some(value) = crate::config::read_config_value(&config_key) {
                out.insert(key.to_string(), Value::String(value));
                continue;
            }
            if let Some(default) = &entry.default {
                out.insert(key.to_string(), default.clone());
                continue;
            }
            if entry.required {
                let hint = if let Some(secret_env) = &entry.secret_env {
                    format!("set environment variable '{}' or config key '{}'", secret_env, config_key)
                } else {
                    format!("set config key '{}'", config_key)
                };
                return Err(format!("Extension '{}' missing required config '{}': {}", id, key, hint));
            }
        }
        Ok(Value::Object(out))
    }

    /// Unload an extension — unsubscribe hooks and shut down the process.
    pub async fn unload(&mut self, id: &str) -> Result<(), String> {
        let handler = self
            .extensions
            .remove(id)
            .ok_or_else(|| format!("Extension '{}' not found", id))?;

        self.hook_bus.unsubscribe_all(id).await;
        self.providers.unregister_plugin(id);
        self.manifest_configs.remove(id);
        handler.shutdown().await;

        tracing::info!(extension = %id, "Extension unloaded");
        Ok(())
    }

    /// Reload one extension by unloading any existing instance first, then loading
    /// the supplied manifest. If the new load fails, the previous instance remains
    /// unloaded so duplicate handlers cannot survive a broken reload.
    pub async fn reload(
        &mut self,
        id: &str,
        manifest: &ExtensionManifest,
        cwd: Option<std::path::PathBuf>,
    ) -> Result<(), String> {
        if self.extensions.contains_key(id) {
            self.unload(id).await?;
        }
        self.load_with_cwd(id, manifest, cwd).await
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

    /// Return registered provider metadata sorted by runtime id.
    pub fn providers(&self) -> Vec<&RegisteredProvider> {
        self.providers.list()
    }

    /// Return registered provider metadata by runtime id.
    pub fn provider(&self, runtime_id: &str) -> Option<&RegisteredProvider> {
        self.providers.get(runtime_id)
    }

    /// Return provider status summaries sorted by provider runtime id.
    pub fn provider_summaries(&self) -> Vec<RegisteredProviderSummary> {
        self.providers.summaries()
    }

    /// Return runtime ids of registered providers that declare at least one
    /// tool-use-capable model. Sorted by runtime id.
    pub fn provider_tool_use_runtime_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .providers
            .list()
            .into_iter()
            .filter(|p| {
                p.spec.models.iter().any(|m| {
                    m.capabilities
                        .get("tool_use")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
                })
            })
            .map(|p| p.runtime_id.clone())
            .collect();
        ids.sort();
        ids
    }

    /// Return a `runtime_id -> enabled` map for every registered provider, computed
    /// from the persisted trust state. Providers without an entry default to
    /// enabled. If the trust state cannot be loaded, all providers are reported
    /// as enabled (fail-open default matches `load_trust_state().unwrap_or_default()`).
    pub fn provider_trust_view(&self) -> std::collections::BTreeMap<String, bool> {
        let trust = crate::extensions::trust::load_trust_state().unwrap_or_default();
        self.providers
            .list()
            .into_iter()
            .map(|p| {
                let enabled =
                    crate::extensions::trust::is_provider_enabled(&trust, &p.runtime_id);
                (p.runtime_id.clone(), enabled)
            })
            .collect()
    }

    /// Compute config diagnostics for a loaded extension by id.
    /// Returns `None` if the extension is not loaded.
    pub fn config_diagnostics(&self, id: &str) -> Option<ExtensionConfigDiagnostics> {
        let manifest_config = self.manifest_configs.get(id)?;

        // Collect provider required keys from registered providers' config_schema.
        let mut provider_required: Vec<(String, Vec<String>)> = Vec::new();
        for provider in self.providers.list() {
            if provider.plugin_id != id {
                continue;
            }
            let required: Vec<String> = provider
                .spec
                .config_schema
                .as_ref()
                .and_then(|schema| schema.get("required"))
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            provider_required.push((provider.provider_id.clone(), required));
        }
        provider_required.sort_by(|a, b| a.0.cmp(&b.0));

        let env_lookup = |name: &str| std::env::var(name).ok();
        let config_lookup = |key: &str| crate::config::read_config_value(key);

        Some(diagnose_extension_config(
            id,
            manifest_config,
            &provider_required,
            &env_lookup,
            &config_lookup,
        ))
    }

    /// Diagnostics for all loaded extensions, sorted alphabetically by id.
    pub fn all_config_diagnostics(&self) -> Vec<ExtensionConfigDiagnostics> {
        let mut ids: Vec<&String> = self.manifest_configs.keys().collect();
        ids.sort();
        ids.into_iter()
            .filter_map(|id| self.config_diagnostics(id))
            .collect()
    }

    /// Get the shared hook bus.
    pub fn hook_bus(&self) -> &Arc<HookBus> {
        &self.hook_bus
    }

    /// Get the shared tool registry, when this manager was constructed with one.
    pub fn tools_shared(&self) -> Option<Arc<tokio::sync::RwLock<crate::ToolRegistry>>> {
        self.tools.clone()
    }

    /// Discover and load all extensions from the user and project plugin directories.
    ///
    /// Scans `~/.synaps-cli/plugins/*/.synaps-plugin/plugin.json` and
    /// `./.synaps/plugins/*/.synaps-plugin/plugin.json` for manifests that contain
    /// an `extension` field. Project-local plugins override user plugins with the
    /// same directory name.
    pub async fn discover_and_load(&mut self) -> (Vec<String>, Vec<ExtensionLoadFailure>) {
        let mut plugin_roots = vec![crate::config::base_dir().join("plugins")];
        if !project_plugins_disabled() {
            if let Ok(cwd) = std::env::current_dir() {
                let project_plugins = cwd.join(".synaps").join("plugins");
                if project_plugins != plugin_roots[0] {
                    plugin_roots.push(project_plugins);
                }
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
    async fn reload_unsubscribes_old_handler_before_loading_new_one() {
        let bus = Arc::new(HookBus::new());
        let mut mgr = ExtensionManager::new(bus.clone());
        let manifest = ExtensionManifest {
            protocol_version: 1,
            runtime: crate::extensions::manifest::ExtensionRuntime::Process,
            command: "python3".to_string(),
            args: vec!["tests/fixtures/process_extension.py".to_string(), "normal".to_string(), "/tmp/synaps-reload-test.log".to_string()],
            permissions: vec!["tools.intercept".to_string()],
            hooks: vec![crate::extensions::manifest::HookSubscription {
                hook: "before_tool_call".to_string(),
                tool: Some("bash".to_string()),
                matcher: None,
            }],
            config: vec![],
        };

        mgr.load("reload-test", &manifest).await.unwrap();
        assert_eq!(bus.handler_count().await, 1);

        mgr.reload("reload-test", &manifest, None).await.unwrap();

        assert_eq!(mgr.count(), 1);
        assert_eq!(bus.handler_count().await, 1);
        mgr.shutdown_all().await;
    }

    #[tokio::test]
    async fn reload_failure_leaves_previous_instance_unloaded() {
        let bus = Arc::new(HookBus::new());
        let mut mgr = ExtensionManager::new(bus.clone());
        let good = ExtensionManifest {
            protocol_version: 1,
            runtime: crate::extensions::manifest::ExtensionRuntime::Process,
            command: "python3".to_string(),
            args: vec!["tests/fixtures/process_extension.py".to_string(), "normal".to_string(), "/tmp/synaps-reload-failure-test.log".to_string()],
            permissions: vec!["tools.intercept".to_string()],
            hooks: vec![crate::extensions::manifest::HookSubscription {
                hook: "before_tool_call".to_string(),
                tool: Some("bash".to_string()),
                matcher: None,
            }],
            config: vec![],
        };
        let bad = ExtensionManifest {
            command: "/definitely/not/a/real/extension-binary".to_string(),
            ..good.clone()
        };

        mgr.load("reload-failure-test", &good).await.unwrap();
        let err = mgr.reload("reload-failure-test", &bad, None).await.unwrap_err();

        assert!(err.contains("Failed to spawn extension"), "{err}");
        assert_eq!(mgr.count(), 0);
        assert_eq!(bus.handler_count().await, 0);
    }

    #[test]
    fn project_plugins_disable_env_parser_accepts_truthy_values() {
        for value in ["1", "true", "TRUE", "yes", "on"] {
            std::env::set_var("SYNAPS_DISABLE_PROJECT_PLUGINS", value);
            assert!(project_plugins_disabled());
        }
        for value in ["", "0", "false", "off", "no"] {
            std::env::set_var("SYNAPS_DISABLE_PROJECT_PLUGINS", value);
            assert!(!project_plugins_disabled());
        }
        std::env::remove_var("SYNAPS_DISABLE_PROJECT_PLUGINS");
    }

    #[tokio::test]
    async fn config_diagnostics_returns_none_for_unknown_extension() {
        let bus = Arc::new(HookBus::new());
        let mgr = ExtensionManager::new(bus);
        assert!(mgr.config_diagnostics("nope").is_none());
        assert!(mgr.all_config_diagnostics().is_empty());
    }

    #[tokio::test]
    async fn config_diagnostics_reports_loaded_manifest_entries() {
        let bus = Arc::new(HookBus::new());
        let mut mgr = ExtensionManager::new(bus);
        let manifest = ExtensionManifest {
            protocol_version: 1,
            runtime: crate::extensions::manifest::ExtensionRuntime::Process,
            command: "python3".to_string(),
            args: vec![
                "tests/fixtures/process_extension.py".to_string(),
                "normal".to_string(),
                "/tmp/synaps-config-diag-test.log".to_string(),
            ],
            permissions: vec!["tools.intercept".to_string()],
            hooks: vec![crate::extensions::manifest::HookSubscription {
                hook: "before_tool_call".to_string(),
                tool: Some("bash".to_string()),
                matcher: None,
            }],
            config: vec![crate::extensions::manifest::ExtensionConfigEntry {
                key: "region".to_string(),
                description: Some("AWS region".to_string()),
                required: false,
                default: Some(serde_json::Value::String("us-east-1".to_string())),
                secret_env: None,
            }],
        };

        mgr.load("config-diag-test", &manifest).await.unwrap();

        let diag = mgr
            .config_diagnostics("config-diag-test")
            .expect("diagnostics should be available for loaded extension");
        assert_eq!(diag.extension_id, "config-diag-test");
        assert_eq!(diag.entries.len(), 1);
        assert_eq!(diag.entries[0].key, "region");
        assert!(diag.entries[0].has_value);
        assert!(diag.provider_missing.is_empty());

        let all = mgr.all_config_diagnostics();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].extension_id, "config-diag-test");

        mgr.shutdown_all().await;
        // After shutdown, manifest config storage is cleared.
        assert!(mgr.config_diagnostics("config-diag-test").is_none());
    }

    #[tokio::test]
    async fn provider_trust_view_is_empty_for_no_providers() {
        let bus = Arc::new(HookBus::new());
        let mgr = ExtensionManager::new(bus);
        let view = mgr.provider_trust_view();
        assert!(view.is_empty());
    }

    #[tokio::test]
    async fn provider_tool_use_runtime_ids_lists_only_tool_use_capable() {
        use crate::extensions::runtime::process::{RegisteredProviderModelSpec, RegisteredProviderSpec};
        let bus = Arc::new(HookBus::new());
        let mut mgr = ExtensionManager::new(bus);
        // Tool-use capable provider.
        let tool_spec = RegisteredProviderSpec {
            id: "alpha".into(),
            display_name: "Alpha".into(),
            description: "tool-use".into(),
            models: vec![RegisteredProviderModelSpec {
                id: "m1".into(),
                display_name: None,
                capabilities: serde_json::json!({"tool_use": true}),
                context_window: None,
            }],
            config_schema: None,
        };
        // Plain provider, no tool_use.
        let plain_spec = RegisteredProviderSpec {
            id: "beta".into(),
            display_name: "Beta".into(),
            description: "plain".into(),
            models: vec![RegisteredProviderModelSpec {
                id: "m1".into(),
                display_name: None,
                capabilities: serde_json::json!({"streaming": true}),
                context_window: None,
            }],
            config_schema: None,
        };
        mgr.providers.register("plug", tool_spec).unwrap();
        mgr.providers.register("plug", plain_spec).unwrap();
        let ids = mgr.provider_tool_use_runtime_ids();
        assert_eq!(ids, vec!["plug:alpha".to_string()]);
    }
}
