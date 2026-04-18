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
    marketplace::{fetch_manifest, is_safe_plugin_name, is_trusted, normalize_marketplace_url, trust_host_for_source},
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

/// Add-marketplace flow: normalize, fetch manifest, push Marketplace, save.
/// On error, stash in editor's `error` field (if still in editor) or row_error.
pub(crate) async fn apply_add_marketplace(state: &mut PluginsModalState, url: String) {
    let normalized = match normalize_marketplace_url(&url) {
        Ok(u) => u,
        Err(e) => {
            set_editor_or_row_error(state, e);
            return;
        }
    };
    let manifest = match fetch_manifest(&normalized).await {
        Ok(m) => m,
        Err(e) => {
            set_editor_or_row_error(state, e);
            return;
        }
    };
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
        url: normalized,
        description: manifest.description.clone(),
        last_refreshed: Some(now_rfc3339()),
        cached_plugins,
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
    let Some(cached) = state
        .file
        .marketplaces
        .iter()
        .find(|m| m.name == marketplace_name)
        .and_then(|m| m.cached_plugins.iter().find(|p| p.name == plugin_name))
        .cloned()
    else {
        state.row_error = Some(format!("plugin '{}' not found in '{}'", plugin_name, marketplace_name));
        return;
    };
    let source = cached.source.clone();
    let host = match trust_host_for_source(&source) {
        Ok(h) => h,
        Err(e) => {
            state.row_error = Some(e);
            return;
        }
    };
    if !is_trusted(&source, &state.file.trusted_hosts) {
        state.mode = RightMode::TrustPrompt {
            plugin_name: plugin_name.clone(),
            host,
            pending_source: source,
        };
        return;
    }
    run_install_flow(state, plugin_name, source, Some(marketplace_name), registry, config).await;
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
    let marketplace_name = state
        .file
        .marketplaces
        .iter()
        .find(|m| m.cached_plugins.iter().any(|p| p.source == source && p.name == plugin_name))
        .map(|m| m.name.clone());
    run_install_flow(state, plugin_name, source, marketplace_name, registry, config).await;
}

async fn run_install_flow(
    state: &mut PluginsModalState,
    plugin_name: String,
    source_url: String,
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
    // Run install_plugin on a blocking thread (it spawns git).
    let src = source_url.clone();
    let dest_clone = dest.clone();
    let install_res = tokio::task::spawn_blocking(move || install::install_plugin(&src, &dest_clone)).await;
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
    let update_res = tokio::task::spawn_blocking(move || install::update_plugin(&dir)).await;
    let sha = match update_res {
        Ok(Ok(sha)) => sha,
        Ok(Err(e)) => {
            state.row_error = Some(e);
            return;
        }
        Err(e) => {
            state.row_error = Some(format!("update task join error: {}", e));
            return;
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

pub(crate) fn apply_remove_marketplace(state: &mut PluginsModalState, name: String) {
    state.file.marketplaces.retain(|m| m.name != name);
    if let Err(e) = commit_plugins_state(&state.file) {
        state.row_error = Some(format!("save failed: {}", e));
        return;
    }
    // If cursor now points past end, clamp.
    let n = state.left_rows().len();
    if state.selected_left >= n && n > 0 {
        state.selected_left = n - 1;
    }
    state.row_error = None;
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

