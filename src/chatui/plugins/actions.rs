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
    reload_registry,
    registry::CommandRegistry,
    state::{InstalledPlugin, Marketplace, PluginsState},
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
            source: p.source.clone(),
            version: p.version.clone(),
            description: p.description.clone(),
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
        };
        return;
    }
    run_install_flow(
        state, plugin_name, effective_source, subdir, Some(marketplace_name),
        registry, config,
    ).await;
}

/// Trust the host, then install. Used for `TrustAndInstall` outcome.
pub(crate) async fn apply_trust_and_install(
    state: &mut PluginsModalState,
    plugin_name: String,
    host: String,
    source: String,
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
    // Run git on a blocking thread. Subdir case clones the marketplace
    // repo and snapshots <repo>/<subdir> into dest; non-subdir case is a
    // straight shallow clone.
    let src = source_url.clone();
    let dest_clone = dest.clone();
    let subdir_for_install = subdir.clone();
    let install_res = tokio::task::spawn_blocking(move || match subdir_for_install {
        Some(s) => install::install_plugin_from_subdir(&src, &s, &dest_clone),
        None => install::install_plugin(&src, &dest_clone),
    })
    .await;
    let sha = match install_res {
        Ok(Ok(sha)) => sha,
        Ok(Err(e)) => {
            state.row_error = Some(e);
            return;
        }
        Err(e) => {
            state.row_error = Some(format!("install task join error: {}", e));
            return;
        }
    };
    let plugin_name_for_msg = plugin_name.clone();
    state.file.installed.push(InstalledPlugin {
        name: plugin_name,
        marketplace: marketplace_name,
        source_url,
        installed_commit: sha,
        latest_commit: None,
        installed_at: now_rfc3339(),
        source_subdir: subdir,
    });
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
                source: p.source.clone(),
                version: p.version.clone(),
                description: p.description.clone(),
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

fn set_editor_or_row_error(state: &mut PluginsModalState, msg: String) {
    if let RightMode::AddMarketplaceEditor { error, .. } = &mut state.mode {
        *error = Some(msg);
    } else {
        state.row_error = Some(msg);
    }
}

