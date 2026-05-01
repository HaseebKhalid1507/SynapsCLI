//! Side-effect handlers for `/plugins` modal outcomes.
//!
//! Called from the async main loop. Each `apply_*` function mutates the
//! modal state (`PluginsModalState`) to reflect success/failure, and
//! performs filesystem/network/registry side-effects. On failure, the
//! error is surfaced via `state.row_error` (or the editor's `error`
//! field for `AddMarketplace`).

use std::path::PathBuf;
use std::sync::Arc;

use synaps_cli::skills::{
    install,
    marketplace::{
        derive_git_clone_url, fetch_manifest, fetch_marketplace, is_safe_plugin_name, is_trusted,
        trust_host_for_source,
    },
    plugin_index::PluginIndexEntry,
    reload_registry,
    registry::CommandRegistry,
    state::{CachedPluginIndexMetadata, InstalledPlugin, Marketplace, PluginsState},
};

use super::state::{PluginsModalState, RightMode};

/// Persist plugins state to its canonical path. Helper used by every mutator.
fn commit_plugins_state(file: &PluginsState) -> std::io::Result<()> {
    file.save_to(&PluginsState::default_path())
}

/// Resolve `~/.synaps-cli/plugins/<name>` (profile-aware).
///
/// Defense-in-depth: rejects unsafe names via [`is_safe_plugin_name`]. This
/// should never fire in practice because `validate_manifest` already rejects
/// such names upstream during marketplace fetch.
fn install_dir_for(name: &str) -> Result<PathBuf, String> {
    if !is_safe_plugin_name(name) {
        return Err("refused to install plugin with unsafe name".into());
    }
    Ok(synaps_cli::config::resolve_write_path("plugins").join(name))
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn cached_index_metadata(entry: &PluginIndexEntry) -> CachedPluginIndexMetadata {
    CachedPluginIndexMetadata {
        repository: entry.repository.clone(),
        subdir: entry.subdir.clone(),
        checksum_algorithm: entry.checksum.algorithm.clone(),
        checksum_value: entry.checksum.value.clone(),
        compatibility_synaps: entry.compatibility.synaps.clone(),
        compatibility_extension_protocol: entry.compatibility.extension_protocol.clone(),
        has_extension: entry.capabilities.has_extension,
        skills: entry.capabilities.skills.clone(),
        permissions: entry.capabilities.permissions.clone(),
        hooks: entry.capabilities.hooks.clone(),
        commands: entry.capabilities.commands.clone(),
        trust_publisher: entry.trust.as_ref().and_then(|t| t.publisher.clone()),
        trust_homepage: entry.trust.as_ref().and_then(|t| t.homepage.clone()),
    }
}

/// Resolve a cached plugin's `source` into the concrete info needed to
/// install: the clone URL, and the optional repo subdir when the plugin
/// lives inside the marketplace repo (Claude-Code-style `./<name>` sources).
///
/// Invariants:
/// - For absolute https sources, returns the source unchanged, subdir=None.
/// - For `./<name>` sources, returns the marketplace's repo_url (falling
///   back to one derived from the marketplace URL if not stored), and
///   subdir=Some(name).
fn resolve_install_target(
    cached_source: &str,
    marketplace: &Marketplace,
) -> Result<(String, Option<String>), String> {
    let s = cached_source.trim();
    if let Some(subdir) = s.strip_prefix("./") {
        if !is_safe_plugin_name(subdir) {
            return Err(format!("refusing unsafe relative source '{}'", s));
        }
        let clone_url = marketplace
            .repo_url
            .clone()
            .or_else(|| derive_git_clone_url(&marketplace.url).ok())
            .ok_or_else(|| {
                format!(
                    "marketplace '{}' has a repo-relative source but no clone URL is known",
                    marketplace.name
                )
            })?;
        Ok((clone_url, Some(subdir.to_string())))
    } else {
        Ok((s.to_string(), None))
    }
}

/// Add-marketplace flow: normalize (probing both `.synaps-plugin/` and
/// `.claude-plugin/` for GitHub URLs), fetch manifest, push Marketplace, save.
/// On error, stash in editor's `error` field (if still in editor) or row_error.
pub(crate) async fn apply_add_marketplace(state: &mut PluginsModalState, url: String) {
    let (manifest, used_url) = match fetch_marketplace(&url).await {
        Ok(v) => v,
        Err(e) => {
            set_editor_or_row_error(state, e);
            return;
        }
    };
    // Derive git clone URL so plugins with "./subdir" sources can clone the
    // marketplace repo later. Only GitHub URLs have a known scheme for this;
    // non-GitHub marketplaces simply won't support relative sources.
    let repo_url = derive_git_clone_url(&url).ok();
    let cached_plugins = manifest
        .plugins
        .iter()
        .map(|p| synaps_cli::skills::state::CachedPlugin {
            name: p.name.clone(),
            source: p.source.clone().unwrap_or_else(|| p.index.as_ref().map(|idx| idx.repository.clone()).unwrap_or_default()),
            version: p.version.clone(),
            description: p.description.clone(),
            index: p.index.as_ref().map(cached_index_metadata),
        })
        .collect();
    let new_m = Marketplace {
        name: manifest.name.clone(),
        url: used_url,
        description: manifest.description.clone(),
        last_refreshed: Some(now_rfc3339()),
        cached_plugins,
        repo_url,
    };
    // Replace existing marketplace with same name, or push.
    if let Some(slot) = state.file.marketplaces.iter_mut().find(|m| m.name == new_m.name) {
        *slot = new_m;
    } else {
        state.file.marketplaces.push(new_m);
    }
    if let Err(e) = commit_plugins_state(&state.file) {
        set_editor_or_row_error(state, format!("save failed: {}", e));
        return;
    }
    state.mode = RightMode::List;
    state.row_error = None;
}

/// Install-or-trust-prompt flow. If host not trusted, switches state to
/// TrustPrompt and returns without side-effects. Otherwise runs install flow.
pub(crate) async fn apply_install(
    state: &mut PluginsModalState,
    marketplace_name: String,
    plugin_name: String,
    registry: &Arc<CommandRegistry>,
    config: &synaps_cli::SynapsConfig,
) {
    let Some(marketplace) = state
        .file
        .marketplaces
        .iter()
        .find(|m| m.name == marketplace_name)
        .cloned()
    else {
        state.row_error = Some(format!("marketplace '{}' not found", marketplace_name));
        return;
    };
    let Some(cached) = marketplace
        .cached_plugins
        .iter()
        .find(|p| p.name == plugin_name)
        .cloned()
    else {
        state.row_error = Some(format!("plugin '{}' not found in '{}'", plugin_name, marketplace_name));
        return;
    };
    let summary = permission_summary_for_plugin_name(&plugin_name)
        .unwrap_or_else(|| vec![format!("source: {}", cached.source)]);
    let (effective_source, subdir) = match resolve_install_target(&cached.source, &marketplace) {
        Ok(v) => v,
        Err(e) => {
            state.row_error = Some(e);
            return;
        }
    };
    let host = match trust_host_for_source(&effective_source) {
        Ok(h) => h,
        Err(e) => {
            state.row_error = Some(e);
            return;
        }
    };
    if !is_trusted(&effective_source, &state.file.trusted_hosts) {
        state.mode = RightMode::TrustPrompt {
            plugin_name: plugin_name.clone(),
            host,
            pending_source: effective_source,
            summary,
        };
        return;
    }
    run_install_flow(
        state, plugin_name, effective_source, subdir, Some(marketplace_name),
        registry, config,
    ).await;
}

pub(crate) async fn apply_install_from_index_entry(
    state: &mut PluginsModalState,
    marketplace_name: String,
    entry: PluginIndexEntry,
    registry: &Arc<CommandRegistry>,
    config: &synaps_cli::SynapsConfig,
) {
    let summary = vec![
        format!("index plugin: {} {}", entry.id, entry.version),
        format!("repository: {}", entry.repository),
        format!("checksum: {}:{}", entry.checksum.algorithm, entry.checksum.value),
        format!("executable extension: {}", if entry.capabilities.has_extension { "yes" } else { "no" }),
        format!("permissions: {}", if entry.capabilities.permissions.is_empty() { "none".to_string() } else { entry.capabilities.permissions.join(", ") }),
        format!("hooks: {}", if entry.capabilities.hooks.is_empty() { "none".to_string() } else { entry.capabilities.hooks.join(", ") }),
        "fetched plugin manifest will be re-inspected before final install".to_string(),
    ];
    let host = match trust_host_for_source(&entry.repository) {
        Ok(h) => h,
        Err(e) => {
            state.row_error = Some(e);
            return;
        }
    };
    if !is_trusted(&entry.repository, &state.file.trusted_hosts) {
        state.mode = RightMode::TrustPrompt {
            plugin_name: entry.id,
            host,
            pending_source: entry.repository,
            summary,
        };
        return;
    }
    let install_source = state
        .file
        .marketplaces
        .iter()
        .find(|m| m.name == marketplace_name)
        .and_then(|m| m.cached_plugins.iter().find(|p| p.name == entry.id))
        .map(|p| p.source.clone())
        .unwrap_or_else(|| entry.repository.clone());
    run_install_flow(
        state,
        entry.id,
        install_source,
        entry.subdir,
        Some(marketplace_name),
        registry,
        config,
    ).await;
}

/// Trust the host, then install. Used for `TrustAndInstall` outcome.
pub(crate) async fn apply_trust_and_install(
    state: &mut PluginsModalState,
    plugin_name: String,
    host: String,
    source: String,
    _summary: Vec<String>,
    registry: &Arc<CommandRegistry>,
    config: &synaps_cli::SynapsConfig,
) {
    if !state.file.trusted_hosts.iter().any(|h| h == &host) {
        state.file.trusted_hosts.push(host);
    }
    // Find marketplace owning this source (if any) so installed entry links back.
    // Also resolve subdir for Claude-Code-style plugins.
    let mut marketplace_name: Option<String> = None;
    let mut subdir: Option<String> = None;
    for m in &state.file.marketplaces {
        for p in &m.cached_plugins {
            if p.name != plugin_name { continue; }
            marketplace_name = Some(m.name.clone());
            if let Some(rest) = p.source.strip_prefix("./") {
                subdir = Some(rest.to_string());
            }
            break;
        }
    }
    run_install_flow(state, plugin_name, source, subdir, marketplace_name, registry, config).await;
}

/// Clone/snapshot into a temporary sibling directory. Returns (HEAD sha, temp dir path).
fn install_plugin_to_temp(
    plugin_name: &str,
    source_url: &str,
    subdir: Option<String>,
    final_dest: &std::path::Path,
) -> Result<(String, PathBuf), String> {
    let parent = final_dest.parent().ok_or_else(|| "dest has no parent directory".to_string())?;
    std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {}", parent.display(), e))?;
    let tmp = parent.join(format!(".{}-pending-install", plugin_name));
    let _ = std::fs::remove_dir_all(&tmp);
    let sha = match subdir {
        Some(s) => install::install_plugin_from_subdir(source_url, &s, &tmp),
        None => install::install_plugin(source_url, &tmp),
    }?;
    Ok((sha, tmp))
}

fn finalize_pending_install(temp_dir: &std::path::Path, final_dir: &std::path::Path) -> Result<(), String> {
    if final_dir.exists() {
        let _ = std::fs::remove_dir_all(temp_dir);
        return Err(format!("{} already exists on disk; uninstall first", final_dir.display()));
    }
    std::fs::rename(temp_dir, final_dir)
        .map_err(|e| format!("finalize install {} -> {}: {}", temp_dir.display(), final_dir.display(), e))
}

fn cancel_pending_temp(temp_dir: &std::path::Path) {
    let _ = std::fs::remove_dir_all(temp_dir);
}

fn summarize_plugin_dir(path: &std::path::Path) -> Vec<String> {
    let Some(parent) = path.parent() else {
        return vec!["plugin manifest could not be inspected before install".to_string()];
    };
    let Ok(path_abs) = path.canonicalize() else {
        return vec!["plugin manifest could not be inspected before install".to_string()];
    };
    let (plugins, _) = synaps_cli::skills::loader::load_all(&[parent.to_path_buf()]);
    if let Some(plugin) = plugins.into_iter().find(|plugin| plugin.root == path_abs) {
        synaps_cli::skills::trust::summarize_plugin_permissions(&plugin).lines()
    } else {
        vec!["plugin manifest could not be inspected before install".to_string()]
    }
}

fn record_installed_plugin(
    state: &mut PluginsModalState,
    plugin_name: String,
    marketplace_name: Option<String>,
    source_url: String,
    installed_commit: String,
    source_subdir: Option<String>,
) {
    state.file.installed.push(InstalledPlugin {
        name: plugin_name,
        marketplace: marketplace_name,
        source_url,
        installed_commit,
        latest_commit: None,
        installed_at: now_rfc3339(),
        source_subdir,
    });
}

async fn run_install_flow(
    state: &mut PluginsModalState,
    plugin_name: String,
    source_url: String,
    subdir: Option<String>,
    marketplace_name: Option<String>,
    registry: &Arc<CommandRegistry>,
    config: &synaps_cli::SynapsConfig,
) {
    let dest = match install_dir_for(&plugin_name) {
        Ok(d) => d,
        Err(e) => {
            state.row_error = Some(e);
            return;
        }
    };
    // Run git on a blocking thread into a temp directory first. If the plugin has
    // an executable extension, show the inspected permissions before finalizing.
    let src = source_url.clone();
    let dest_clone = dest.clone();
    let subdir_for_install = subdir.clone();
    let plugin_name_for_task = plugin_name.clone();
    let install_res = tokio::task::spawn_blocking(move || {
        install_plugin_to_temp(&plugin_name_for_task, &src, subdir_for_install, &dest_clone)
    })
    .await;
    let (sha, temp_dir) = match install_res {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            state.row_error = Some(e);
            return;
        }
        Err(e) => {
            state.row_error = Some(format!("install task join error: {}", e));
            return;
        }
    };
    let summary = summarize_plugin_dir(&temp_dir);
    let has_executable_extension = summary.iter().any(|line| line == "executable extension: yes");
    if has_executable_extension {
        state.mode = RightMode::PendingInstallConfirm {
            plugin_name,
            source_url,
            subdir,
            marketplace_name,
            summary,
            installed_commit: sha,
            temp_dir,
            final_dir: dest,
        };
        state.row_error = None;
        return;
    }
    if let Err(e) = finalize_pending_install(&temp_dir, &dest) {
        state.row_error = Some(e);
        return;
    }
    let plugin_name_for_msg = plugin_name.clone();
    record_installed_plugin(
        state,
        plugin_name,
        marketplace_name,
        source_url,
        sha,
        subdir,
    );
    if let Err(e) = commit_plugins_state(&state.file) {
        state.row_error = Some(format!(
            "installed '{}' but failed to save state: {}. Restart may lose this install.",
            plugin_name_for_msg, e
        ));
        return;
    }
    reload_registry(registry, config);
    state.mode = RightMode::List;
    state.row_error = None;
}

pub(crate) async fn apply_confirm_pending_install(
    state: &mut PluginsModalState,
    registry: &Arc<CommandRegistry>,
    config: &synaps_cli::SynapsConfig,
) {
    let RightMode::PendingInstallConfirm {
        plugin_name,
        source_url,
        subdir,
        marketplace_name,
        installed_commit,
        temp_dir,
        final_dir,
        ..
    } = std::mem::replace(&mut state.mode, RightMode::List) else {
        return;
    };

    let final_dir_for_task = final_dir.clone();
    let temp_dir_for_task = temp_dir.clone();
    let finalize_res = tokio::task::spawn_blocking(move || {
        finalize_pending_install(&temp_dir_for_task, &final_dir_for_task)
    }).await;
    match finalize_res {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            state.row_error = Some(e);
            return;
        }
        Err(e) => {
            state.row_error = Some(format!("install finalize task join error: {}", e));
            return;
        }
    }

    record_installed_plugin(
        state,
        plugin_name.clone(),
        marketplace_name,
        source_url,
        installed_commit,
        subdir,
    );
    if let Err(e) = commit_plugins_state(&state.file) {
        state.row_error = Some(format!(
            "installed '{}' but failed to save state: {}. Restart may lose this install.",
            plugin_name, e
        ));
        return;
    }
    reload_registry(registry, config);
    state.row_error = None;
}

pub(crate) fn apply_cancel_pending_install(state: &mut PluginsModalState) {
    let old = std::mem::replace(&mut state.mode, RightMode::List);
    if let RightMode::PendingInstallConfirm { temp_dir, .. } = old {
        cancel_pending_temp(&temp_dir);
    }
    state.row_error = None;
}

pub(crate) async fn apply_uninstall(
    state: &mut PluginsModalState,
    name: String,
    registry: &Arc<CommandRegistry>,
    config: &synaps_cli::SynapsConfig,
) {
    let dir = match install_dir_for(&name) {
        Ok(d) => d,
        Err(e) => {
            state.row_error = Some(e);
            return;
        }
    };
    let uninstall_res = tokio::task::spawn_blocking(move || install::uninstall_plugin(&dir)).await;
    match uninstall_res {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            state.row_error = Some(e);
            return;
        }
        Err(e) => {
            state.row_error = Some(format!("uninstall task join error: {}", e));
            return;
        }
    }
    state.file.installed.retain(|p| p.name != name);
    if let Err(e) = commit_plugins_state(&state.file) {
        state.row_error = Some(format!(
            "uninstalled '{}' but failed to save state: {}. State may be stale.",
            name, e
        ));
        return;
    }
    reload_registry(registry, config);
    state.row_error = None;
}

pub(crate) async fn apply_update(
    state: &mut PluginsModalState,
    name: String,
    registry: &Arc<CommandRegistry>,
    config: &synaps_cli::SynapsConfig,
) {
    let dir = match install_dir_for(&name) {
        Ok(d) => d,
        Err(e) => {
            state.row_error = Some(e);
            return;
        }
    };
    // Plugins installed from a marketplace subdir have no `.git` on disk
    // (they're snapshots), so `git pull` can't update them. Instead, remove
    // the snapshot and re-clone+snapshot from the marketplace repo.
    let installed = state.file.installed.iter().find(|p| p.name == name).cloned();
    let Some(installed) = installed else {
        state.row_error = Some(format!("plugin '{}' is not installed", name));
        return;
    };
    let sha = if let Some(subdir) = installed.source_subdir.clone() {
        let source = installed.source_url.clone();
        let dir_for_task = dir.clone();
        let task = tokio::task::spawn_blocking(move || {
            install::uninstall_plugin(&dir_for_task)
                .and_then(|_| install::install_plugin_from_subdir(&source, &subdir, &dir_for_task))
        }).await;
        match task {
            Ok(Ok(sha)) => sha,
            Ok(Err(e)) => { state.row_error = Some(e); return; }
            Err(e) => { state.row_error = Some(format!("update task join error: {}", e)); return; }
        }
    } else {
        let update_res = tokio::task::spawn_blocking(move || install::update_plugin(&dir)).await;
        match update_res {
            Ok(Ok(sha)) => sha,
            Ok(Err(e)) => { state.row_error = Some(e); return; }
            Err(e) => { state.row_error = Some(format!("update task join error: {}", e)); return; }
        }
    };
    if let Some(p) = state.file.installed.iter_mut().find(|p| p.name == name) {
        p.installed_commit = sha;
    }
    if let Err(e) = commit_plugins_state(&state.file) {
        state.row_error = Some(format!(
            "updated '{}' but failed to save state: {}. State may be stale.",
            name, e
        ));
        return;
    }
    reload_registry(registry, config);
    state.row_error = None;
}

pub(crate) async fn apply_refresh_marketplace(state: &mut PluginsModalState, name: String) {
    let Some(url) = state
        .file
        .marketplaces
        .iter()
        .find(|m| m.name == name)
        .map(|m| m.url.clone())
    else {
        state.row_error = Some(format!("marketplace '{}' not found", name));
        return;
    };
    let manifest = match fetch_manifest(&url).await {
        Ok(m) => m,
        Err(e) => {
            state.row_error = Some(e);
            return;
        }
    };
    // Collect fresh sources for ls-remote (only for installed plugins from this marketplace).
    let installed_sources: Vec<(String, String)> = state
        .file
        .installed
        .iter()
        .filter(|p| p.marketplace.as_deref() == Some(name.as_str()))
        .map(|p| (p.name.clone(), p.source_url.clone()))
        .collect();
    let ls_results = tokio::task::spawn_blocking(move || {
        installed_sources
            .into_iter()
            .map(|(n, s)| (n, install::ls_remote_head(&s)))
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();

    // Apply to state.
    if let Some(m) = state.file.marketplaces.iter_mut().find(|m| m.name == name) {
        m.cached_plugins = manifest
            .plugins
            .iter()
            .map(|p| synaps_cli::skills::state::CachedPlugin {
                name: p.name.clone(),
                source: p.source.clone().unwrap_or_else(|| p.index.as_ref().map(|idx| idx.repository.clone()).unwrap_or_default()),
                version: p.version.clone(),
                description: p.description.clone(),
                index: p.index.as_ref().map(cached_index_metadata),
            })
            .collect();
        m.last_refreshed = Some(now_rfc3339());
    }
    let mut failed: Vec<String> = Vec::new();
    for (plugin_name, res) in ls_results {
        match res {
            Ok(sha) => {
                if let Some(p) = state.file.installed.iter_mut().find(|p| p.name == plugin_name) {
                    p.latest_commit = Some(sha);
                }
            }
            Err(_) => {
                failed.push(plugin_name);
            }
        }
    }
    if let Err(e) = commit_plugins_state(&state.file) {
        state.row_error = Some(format!("save failed: {}", e));
        return;
    }
    if !failed.is_empty() {
        state.row_error = Some(format!(
            "refreshed '{}', but could not check updates for: {}",
            name,
            failed.join(", ")
        ));
    } else {
        state.row_error = None;
    }
    state.mode = RightMode::List;
}

pub(crate) async fn apply_remove_marketplace(
    state: &mut PluginsModalState,
    name: String,
    registry: &Arc<CommandRegistry>,
    config: &synaps_cli::SynapsConfig,
) {
    // Cascade uninstall: anything installed from this marketplace goes with it.
    let to_uninstall: Vec<String> = state
        .file
        .installed
        .iter()
        .filter(|p| p.marketplace.as_deref() == Some(name.as_str()))
        .map(|p| p.name.clone())
        .collect();

    let mut failed: Vec<String> = Vec::new();
    for plugin_name in &to_uninstall {
        let dir = match install_dir_for(plugin_name) {
            Ok(d) => d,
            Err(e) => { failed.push(format!("{}: {}", plugin_name, e)); continue; }
        };
        let res = tokio::task::spawn_blocking(move || install::uninstall_plugin(&dir)).await;
        match res {
            Ok(Ok(())) => {}
            Ok(Err(e)) => failed.push(format!("{}: {}", plugin_name, e)),
            Err(e) => failed.push(format!("{}: join error: {}", plugin_name, e)),
        }
    }

    state.file.installed.retain(|p| p.marketplace.as_deref() != Some(name.as_str()));
    state.file.marketplaces.retain(|m| m.name != name);

    if let Err(e) = commit_plugins_state(&state.file) {
        state.row_error = Some(format!("save failed: {}", e));
        return;
    }
    reload_registry(registry, config);

    let n = state.left_rows().len();
    if state.selected_left >= n && n > 0 {
        state.selected_left = n - 1;
    }

    state.row_error = if failed.is_empty() {
        None
    } else {
        Some(format!(
            "removed marketplace '{}', but failed to fully uninstall: {}",
            name,
            failed.join("; ")
        ))
    };
}

/// Core config mutation for toggling a plugin enabled/disabled.
///
/// Shared between the `/plugins` modal (`apply_toggle_plugin`) and the
/// settings modal's TogglePlugin handler. On success: updates `config`
/// and reloads the registry. On failure: returns the error string
/// without mutating `config`.
pub(crate) fn toggle_plugin_config(
    name: &str,
    enabled: bool,
    config: &mut synaps_cli::SynapsConfig,
    registry: &Arc<CommandRegistry>,
) -> Result<(), String> {
    let mut new_disabled = config.disabled_plugins.clone();
    if enabled {
        new_disabled.retain(|p| p != name);
    } else if !new_disabled.iter().any(|p| p == name) {
        new_disabled.push(name.to_string());
    }
    let csv = new_disabled.join(", ");
    synaps_cli::config::write_config_value("disabled_plugins", &csv)
        .map_err(|e| e.to_string())?;
    config.disabled_plugins = new_disabled;
    reload_registry(registry, config);
    Ok(())
}

pub(crate) fn apply_toggle_plugin(
    state: &mut PluginsModalState,
    name: String,
    enabled: bool,
    registry: &Arc<CommandRegistry>,
    config: &mut synaps_cli::SynapsConfig,
) {
    match toggle_plugin_config(&name, enabled, config, registry) {
        Ok(()) => {
            state.row_error = None;
        }
        Err(e) => {
            state.row_error = Some(e);
        }
    }
}

fn permission_summary_for_plugin_name(name: &str) -> Option<Vec<String>> {
    let roots = synaps_cli::skills::loader::default_roots();
    let (plugins, _) = synaps_cli::skills::loader::load_all(&roots);
    plugins
        .into_iter()
        .find(|plugin| plugin.name == name)
        .map(|plugin| synaps_cli::skills::trust::summarize_plugin_permissions(&plugin).lines())
}

pub(crate) fn confirm_enable_plugin(
    state: &mut PluginsModalState,
    name: String,
) {
    let summary = permission_summary_for_plugin_name(&name).unwrap_or_else(|| {
        vec!["plugin manifest not found; enabling will reload available plugin content".to_string()]
    });
    state.mode = RightMode::Confirm {
        prompt: format!("Enable plugin '{}'? y/n", name),
        on_yes: super::state::ConfirmAction::EnablePlugin(name),
        summary,
    };
}

fn set_editor_or_row_error(state: &mut PluginsModalState, msg: String) {
    if let RightMode::AddMarketplaceEditor { error, .. } = &mut state.mode {
        *error = Some(msg);
    } else {
        state.row_error = Some(msg);
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::{Arc, Mutex};
    use synaps_cli::skills::registry::CommandRegistry;
    use synaps_cli::skills::state::{CachedPlugin, Marketplace, PluginsState};
    use synaps_cli::skills::plugin_index::{
        PluginIndexCapabilities, PluginIndexChecksum, PluginIndexCompatibility, PluginIndexEntry,
    };

    static BASE_DIR_TEST_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        old_base_dir: Option<String>,
        old_git_config_global: Option<String>,
    }

    impl EnvGuard {
        fn set_base_dir(path: &Path, git_config_global: &Path) -> Self {
            let old_base_dir = std::env::var("SYNAPS_BASE_DIR").ok();
            let old_git_config_global = std::env::var("GIT_CONFIG_GLOBAL").ok();
            synaps_cli::config::set_base_dir_for_tests(path.to_path_buf());
            std::env::set_var("GIT_CONFIG_GLOBAL", git_config_global);
            Self { old_base_dir, old_git_config_global }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(old) = &self.old_base_dir {
                std::env::set_var("SYNAPS_BASE_DIR", old);
            } else {
                std::env::remove_var("SYNAPS_BASE_DIR");
            }
            if let Some(old) = &self.old_git_config_global {
                std::env::set_var("GIT_CONFIG_GLOBAL", old);
            } else {
                std::env::remove_var("GIT_CONFIG_GLOBAL");
            }
        }
    }

    fn git(args: &[&str], cwd: &Path) {
        let status = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .status()
            .unwrap();
        assert!(status.success(), "git {:?} failed", args);
    }

    fn fixture_plugin_repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("work");
        fs::create_dir_all(work.join(".synaps-plugin")).unwrap();
        fs::write(
            work.join(".synaps-plugin/plugin.json"),
            r#"{
  "name": "policy-test",
  "description": "test plugin with an executable extension",
  "extension": {
    "protocol_version": 1,
    "runtime": "process",
    "command": "python3",
    "args": ["extension.py"],
    "permissions": ["tools.intercept"],
    "hooks": [{"hook": "before_tool_call", "tool": "bash"}]
  }
}"#,
        )
        .unwrap();
        fs::write(work.join("extension.py"), "#!/usr/bin/env python3\n").unwrap();

        git(&["init", "-q"], &work);
        git(&["config", "user.email", "t@t"], &work);
        git(&["config", "user.name", "t"], &work);
        git(&["add", "."], &work);
        git(&["commit", "-q", "-m", "init"], &work);

        let bare = dir.path().join("plugin.git");
        let work_s = work.to_string_lossy().to_string();
        let bare_s = bare.to_string_lossy().to_string();
        git(&["clone", "--bare", "-q", &work_s, &bare_s], dir.path());
        (dir, bare)
    }

    fn state_for_marketplace(source_url: String) -> PluginsModalState {
        PluginsModalState::new(PluginsState {
            marketplaces: vec![Marketplace {
                name: "local-market".into(),
                url: "https://example.invalid/marketplace.json".into(),
                description: None,
                last_refreshed: None,
                cached_plugins: vec![CachedPlugin {
                    name: "policy-test".into(),
                    source: source_url,
                    version: None,
                    description: Some("fixture".into()),
                    index: None,
                }],
                repo_url: None,
            }],
            installed: vec![],
            trusted_hosts: vec!["example.invalid/owner".into()],
        })
    }

    fn index_entry(repository: String) -> PluginIndexEntry {
        PluginIndexEntry {
            id: "policy-test".into(),
            name: "policy-test".into(),
            version: "0.1.0".into(),
            description: "fixture".into(),
            repository,
            subdir: None,
            license: None,
            categories: vec![],
            keywords: vec![],
            checksum: PluginIndexChecksum {
                algorithm: "sha256".into(),
                value: "abc123".into(),
            },
            compatibility: PluginIndexCompatibility {
                synaps: Some(">=0.1.0".into()),
                extension_protocol: Some("1".into()),
            },
            capabilities: PluginIndexCapabilities {
                skills: vec![],
                has_extension: true,
                permissions: vec!["tools.intercept".into()],
                hooks: vec!["before_tool_call".into()],
                commands: vec![],
            },
            trust: None,
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn install_from_index_entry_uses_pending_install_and_reinspects_manifest() {
        let _guard = BASE_DIR_TEST_LOCK.lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        let _env = EnvGuard::set_base_dir(home.path(), &home.path().join("gitconfig"));
        let (_repo_tmp, bare) = fixture_plugin_repo();
        let source = format!("file://{}", bare.display());
        let repository = "https://example.invalid/owner/policy-test.git".to_string();
        let mut state = PluginsModalState::new(PluginsState {
            marketplaces: vec![Marketplace {
                name: "local-index".into(),
                url: "file:///tmp/plugin-index.json".into(),
                description: None,
                last_refreshed: None,
                cached_plugins: vec![CachedPlugin {
                    name: "policy-test".into(),
                    source: source.clone(),
                    version: Some("0.1.0".into()),
                    description: Some("fixture".into()),
                    index: None,
                }],
                repo_url: None,
            }],
            installed: vec![],
            trusted_hosts: vec!["example.invalid/owner".into()],
        });
        let registry = Arc::new(CommandRegistry::new(&[], vec![]));
        let config = synaps_cli::SynapsConfig::default();

        apply_install_from_index_entry(
            &mut state,
            "local-index".into(),
            PluginIndexEntry {
                repository: repository.clone(),
                ..index_entry(source.clone())
            },
            &registry,
            &config,
        )
        .await;

        let RightMode::PendingInstallConfirm {
            source_url,
            marketplace_name,
            summary,
            temp_dir,
            final_dir,
            ..
        } = &state.mode else {
            panic!("expected pending install confirmation, got {:?}", state.mode);
        };
        assert_eq!(source_url, &source);
        assert_eq!(marketplace_name.as_deref(), Some("local-index"));
        assert!(summary.iter().any(|line| line == "executable extension: yes"));
        assert!(temp_dir.exists());
        assert!(!final_dir.exists());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pending_extension_install_can_be_cancelled_without_touching_real_state() {
        let _guard = BASE_DIR_TEST_LOCK.lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        let _env = EnvGuard::set_base_dir(home.path(), &home.path().join("gitconfig"));
        let (_repo_tmp, bare) = fixture_plugin_repo();
        let source = format!("file://{}", bare.display());
        let mut state = state_for_marketplace(source);
        let registry = Arc::new(CommandRegistry::new(&[], vec![]));
        let config = synaps_cli::SynapsConfig::default();

        run_install_flow(
            &mut state,
            "policy-test".into(),
            format!("file://{}", bare.display()),
            None,
            Some("local-market".into()),
            &registry,
            &config,
        )
        .await;

        let (temp_dir, final_dir) = match &state.mode {
            RightMode::PendingInstallConfirm { temp_dir, final_dir, summary, .. } => {
                assert!(summary.iter().any(|line| line == "executable extension: yes"));
                (temp_dir.clone(), final_dir.clone())
            }
            other => panic!("expected pending install confirmation, got {other:?}"),
        };
        assert!(temp_dir.exists());
        assert!(temp_dir.ends_with(".policy-test-pending-install"));
        assert!(!final_dir.exists());
        assert!(state.file.installed.is_empty());

        apply_cancel_pending_install(&mut state);

        assert!(matches!(state.mode, RightMode::List));
        assert!(!temp_dir.exists());
        assert!(!final_dir.exists());
        assert!(state.file.installed.is_empty());
        assert!(!home.path().join("plugins.json").exists());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pending_extension_install_confirm_moves_temp_and_records_plugin() {
        let _guard = BASE_DIR_TEST_LOCK.lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        let _env = EnvGuard::set_base_dir(home.path(), &home.path().join("gitconfig"));
        let (_repo_tmp, bare) = fixture_plugin_repo();
        let source = format!("file://{}", bare.display());
        let mut state = state_for_marketplace(source.clone());
        let registry = Arc::new(CommandRegistry::new(&[], vec![]));
        let config = synaps_cli::SynapsConfig::default();

        run_install_flow(
            &mut state,
            "policy-test".into(),
            source.clone(),
            None,
            Some("local-market".into()),
            &registry,
            &config,
        )
        .await;

        let (temp_dir, final_dir) = match &state.mode {
            RightMode::PendingInstallConfirm { temp_dir, final_dir, .. } => {
                (temp_dir.clone(), final_dir.clone())
            }
            other => panic!("expected pending install confirmation, got {other:?}"),
        };
        assert!(temp_dir.exists());
        assert!(!final_dir.exists());

        apply_confirm_pending_install(&mut state, &registry, &config).await;

        assert!(matches!(state.mode, RightMode::List));
        assert!(!temp_dir.exists());
        assert!(final_dir.join(".synaps-plugin/plugin.json").exists());
        assert_eq!(state.file.installed.len(), 1);
        let installed = &state.file.installed[0];
        assert_eq!(installed.name, "policy-test");
        assert_eq!(installed.marketplace.as_deref(), Some("local-market"));
        assert_eq!(installed.source_url, source);
        assert_eq!(installed.installed_commit.len(), 40);

        let saved = PluginsState::load_from(&home.path().join("plugins.json")).unwrap();
        assert_eq!(saved.installed.len(), 1);
        assert_eq!(saved.installed[0].name, "policy-test");
    }
}
