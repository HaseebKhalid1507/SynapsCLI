//! Chatui-side glue for the sidecar subsystem.
//!
//! Owns the `SidecarUiState` held on `App.sidecar` and provides helpers
//! the slash-command dispatcher and event loop call into. The actual
//! sidecar lifecycle lives in `crate::sidecar::manager::SidecarManager`.
//!
//! ## Phase 7 slice F — plugin self-config
//!
//! This module no longer reads any plugin-namespaced config keys
//! itself. All sidecar spawn arguments come from the plugin via the
//! `sidecar.spawn_args` RPC (see [`synaps_cli::sidecar::spawn`]).
//! Core does not know which plugin it is hosting; it just plumbs the
//! RPC result through to [`SidecarManager::spawn`].
//!
//! When the plugin doesn't implement the RPC (legacy/old builds), the
//! caller in `chatui/mod.rs` simply passes `None` and we fall back to
//! the manifest's `provides.sidecar.model.default_path` if any.

use synaps_cli::sidecar::discovery::{discover, DiscoveredSidecar};
use synaps_cli::sidecar::manager::{SidecarError, SidecarLifecycleEvent, SidecarManager};
use synaps_cli::sidecar::protocol::{InsertTextMode, SIDECAR_PROTOCOL_VERSION};
use synaps_cli::sidecar::spawn::SidecarSpawnArgs;

use super::app::{App, ChatMessage};

type ExtensionManager =
    std::sync::Arc<tokio::sync::RwLock<synaps_cli::extensions::manager::ExtensionManager>>;
type CommandRegistry = synaps_cli::skills::registry::CommandRegistry;
const STARTUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Own the task rather than detaching it: shutdown, cancellation and a dropped
/// completion all drop the manager, which kills the child and aborts readers.
pub(crate) struct SidecarStartup {
    task: tokio::task::JoinHandle<Result<SidecarUiState, String>>,
    sidecar: DiscoveredSidecar,
    label: String,
}

impl Drop for SidecarStartup {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl SidecarStartup {
    fn start(
        sidecar: DiscoveredSidecar,
        has_extension: bool,
        label: String,
        manager: ExtensionManager,
    ) -> Self {
        let discovered = sidecar.clone();
        let display_name = label.clone();
        let task = tokio::spawn(async move {
            tokio::time::timeout(STARTUP_TIMEOUT, async move {
                let (info, handler) = if has_extension {
                    // Only this background task may wait for extension loading.
                    // Clone the authorized handler, then release the manager
                    // lock BEFORE IPC/sidecar initialization.
                    let manager = manager.read().await;
                    (
                        manager.plugin_info(&discovered.plugin_name).cloned(),
                        Some(manager.user_action_handler(&discovered.plugin_name)?),
                    )
                } else {
                    (None, None)
                };
                let args = if let Some(handler) = handler {
                    match handler.sidecar_spawn_args().await {
                        Ok(args) => Some(args),
                        // Only legacy unsupported-method replies can fall back.
                        // A denied activation, crash or timeout must never start
                        // a sidecar with unintended manifest defaults.
                        Err(error) if spawn_args_unsupported(&error) => None,
                        Err(error) => return Err(error),
                    }
                } else {
                    None
                };
                let mut state = SidecarUiState::spawn_for(discovered, args, info.as_ref()).await?;
                state.set_display_name(Some(display_name));
                Ok(state)
            })
            .await
            .map_err(|_| "sidecar startup timed out after 30s; toggle to retry".to_string())?
        });
        Self {
            task,
            sidecar,
            label,
        }
    }
}

fn spawn_args_unsupported(error: &str) -> bool {
    matches!(
        error,
        "extension runtime does not support sidecar.spawn_args"
            | "Extension error: method not found"
            | "Extension error: unknown method"
            | "Extension error: unknown method: sidecar.spawn_args"
    )
}

fn loading_message(label: &str) -> String {
    format!("{label}: still loading — try the toggle again when ready")
}

/// Non-blocking half of the toggle path. All filesystem discovery came from
/// the filtered boot registry; lock waits, RPC and process startup run off-loop.
pub(crate) async fn toggle(
    app: &mut App,
    plugin_id: Option<String>,
    registry: &CommandRegistry,
    manager: &ExtensionManager,
) {
    if app.sidecars_disabled {
        app.push_msg(ChatMessage::System(
            "Sidecars are disabled (--no-extensions).".into(),
        ));
        return;
    }
    let all = registry.sidecars();
    let target = plugin_id.or_else(|| all.first().map(|s| s.plugin_name.clone()));
    let Some(pid) = target else {
        app.push_msg(ChatMessage::Error(
            "sidecar unavailable: no enabled plugin provides a sidecar binary".into(),
        ));
        return;
    };
    if let Some(pending) = app.sidecar_starts.get(&pid) {
        app.push_msg(ChatMessage::System(loading_message(&pending.label)));
        return;
    }
    // Do not operate stale instances after reload/disable.
    let Some(discovered) = all.into_iter().find(|s| s.plugin_name == pid) else {
        app.sidecars.remove(&pid);
        app.push_msg(ChatMessage::Error(format!(
            "sidecar plugin '{pid}' is not enabled or discoverable"
        )));
        return;
    };
    drain_events(app, &pid);
    if app.sidecars.get(&pid).is_some_and(|state| {
        state.sidecar != discovered || matches!(state.status, SidecarUiStatus::Error(_))
    }) {
        app.sidecars.remove(&pid);
    }
    if let Some(state) = app.sidecars.get_mut(&pid) {
        let label = state.display_name.clone().unwrap_or_else(|| pid.clone());
        if matches!(state.status, SidecarUiStatus::Loading) {
            app.push_msg(ChatMessage::System(loading_message(&label)));
            return;
        }
        if state.armed {
            state.armed = false;
            match state.manager.release().await {
                Ok(()) => app.push_msg(ChatMessage::System(format!(
                    "{label}: stopping — final transcript will be appended"
                ))),
                Err(error) => {
                    state.status = SidecarUiStatus::Error(error.to_string());
                    app.push_msg(ChatMessage::Error(format!(
                        "{label} release failed: {error}"
                    )));
                }
            }
        } else {
            match state.manager.press().await {
                Ok(()) => {
                    state.armed = true;
                    app.push_msg(ChatMessage::System(format!(
                        "{label} active — toggle again to stop"
                    )));
                }
                Err(error) => {
                    state.status = SidecarUiStatus::Error(error.to_string());
                    app.push_msg(ChatMessage::Error(format!("{label} press failed: {error}")));
                }
            }
        }
        return;
    }
    let label = super::loop_arms::pick_display_name_for_plugin(&pid, &registry.lifecycle_claims())
        .unwrap_or_else(|| pid.clone());
    // Startup discovery may not even have acquired the manager lock yet.
    // Never turn an early keypress into an unknown-extension failure (or an
    // activation queued behind discovery). Let the user retry after loading.
    if registry.sidecar_has_extension(&pid) && app.extension_loader_running {
        app.push_msg(ChatMessage::System(loading_message(&label)));
        return;
    }
    let startup = SidecarStartup::start(
        discovered,
        registry.sidecar_has_extension(&pid),
        label.clone(),
        manager.clone(),
    );
    app.sidecar_starts.insert(pid, startup);
    app.push_msg(ChatMessage::System(loading_message(&label)));
}

/// JoinHandle polling is cancellation-safe across tokio::select iterations.
/// No result channel/sender cycle or detached completion can outlive App.
pub(crate) async fn next_startup(
    starts: &mut std::collections::HashMap<String, SidecarStartup>,
) -> (String, Result<SidecarUiState, String>) {
    if starts.is_empty() {
        return std::future::pending().await;
    }
    let futures: Vec<_> = starts
        .iter_mut()
        .map(|(pid, start)| {
            let pid = pid.clone();
            Box::pin(async move {
                let result = (&mut start.task)
                    .await
                    .unwrap_or_else(|_| Err("sidecar startup task failed; toggle to retry".into()));
                (pid, result)
            })
        })
        .collect();
    futures::future::select_all(futures).await.0
}

pub(crate) fn finish_startup(
    app: &mut App,
    registry: &CommandRegistry,
    pid: String,
    result: Result<SidecarUiState, String>,
) {
    let Some(pending) = app.sidecar_starts.remove(&pid) else {
        return;
    };
    // A registry reload may disable/change the plugin while it initializes.
    if app.sidecars_disabled || !registry.sidecars().contains(&pending.sidecar) {
        app.push_msg(ChatMessage::System(format!(
            "{} startup cancelled: plugin changed or disabled",
            pending.label
        )));
        return; // result drops here, killing a late successful process
    }
    match result {
        Ok(state) => {
            debug_assert!(!state.armed);
            app.sidecars.insert(pid.clone(), state);
            drain_events(app, &pid);
            if app
                .sidecars
                .get(&pid)
                .is_some_and(|state| matches!(state.status, SidecarUiStatus::Idle))
            {
                app.push_msg(ChatMessage::System(format!(
                    "{} ready — toggle to activate",
                    pending.label
                )));
            }
        }
        Err(error) => app.push_msg(ChatMessage::Error(format!(
            "{} unavailable: {error}",
            pending.label
        ))),
    }
}

fn drain_events(app: &mut App, pid: &str) {
    // Bound work even for a continuously noisy child. The bounded channel has
    // 64 slots; if still full after draining, conservatively withhold activation.
    for _ in 0..64 {
        let Some(event) = app
            .sidecars
            .get_mut(pid)
            .and_then(|s| s.manager.try_next_event())
        else {
            return;
        };
        let failed = matches!(
            event,
            SidecarLifecycleEvent::Error(_) | SidecarLifecycleEvent::Exited
        );
        handle_event(app, pid, event);
        if failed || !app.sidecars.contains_key(pid) {
            return;
        }
    }
    if let Some(state) = app.sidecars.get_mut(pid) {
        state.status = SidecarUiStatus::Loading;
    }
}

pub(crate) fn retain_enabled(app: &mut App, registry: &CommandRegistry) {
    let enabled = registry.sidecars();
    app.sidecar_starts
        .retain(|_, pending| enabled.contains(&pending.sidecar));
    app.sidecars
        .retain(|_, state| enabled.contains(&state.sidecar));
}

pub(crate) fn status(app: &App, plugin_id: Option<&str>, registry: &CommandRegistry) -> String {
    if app.sidecars_disabled {
        return "Sidecars are disabled (--no-extensions).".into();
    }
    let mut lines = Vec::new();
    for sidecar in registry
        .sidecars()
        .into_iter()
        .filter(|s| plugin_id.map_or(true, |p| p == s.plugin_name))
    {
        let pid = &sidecar.plugin_name;
        lines.push(if let Some(pending) = app.sidecar_starts.get(pid) {
            loading_message(&pending.label)
        } else if let Some(state) = app.sidecars.get(pid) {
            state.status_line()
        } else {
            format!("{pid}: not yet started — toggle to load in the background")
        });
    }
    lines.sort();
    if lines.is_empty() {
        "sidecar: no enabled plugin provides the requested sidecar".into()
    } else {
        lines.join("\n")
    }
}

/// What the chatui currently shows for the sidecar indicator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SidecarUiStatus {
    /// Plugin initialization/reinitialization has not finished.
    Loading,
    /// Sidecar is not currently doing plugin-defined work.
    Idle,
    /// Sidecar is doing plugin-defined work and supplied a display label.
    Active { label: String },
    /// Sidecar reported an error; user should `/sidecar toggle` to retry.
    Error(String),
}

/// State held by the chatui while a sidecar plugin is enabled.
pub(crate) struct SidecarUiState {
    pub manager: SidecarManager,
    pub status: SidecarUiStatus,
    pub sidecar: DiscoveredSidecar,
    /// `true` once the user has issued `press()`. The sidecar is logically
    /// "armed" until the user toggles off — even when the VAD has just
    /// flushed an utterance and momentarily quiesced. Without this we'd
    /// flap back to `Idle` after every utterance and the next toggle would
    /// (incorrectly) issue another press.
    pub armed: bool,
    /// Cached sidecar build-info backend — populated lazily on first
    /// spawn via `discovery::read_build_info()`. `None` when the probe
    /// failed (e.g. older sidecar without `--print-build-info`).
    pub compiled_backend: Option<String>,
    /// Human-readable name from the plugin's lifecycle claim
    /// (`provides.sidecar.lifecycle.display_name`). Used to label the
    /// header pill, status line, and error/info messages. `None` when
    /// no plugin has claimed lifecycle for this sidecar (legacy
    /// fallback) — display strings then say "sidecar".
    pub display_name: Option<String>,
}

impl SidecarUiState {
    /// Discover a sidecar from loaded plugins and spawn its manager
    /// with a default protocol handshake.
    ///
    /// Returns `Err` with a user-facing message if no plugin provides
    /// a sidecar binary or the spawn itself fails.
    #[allow(dead_code)]
    pub async fn spawn_default() -> Result<Self, String> {
        Self::spawn_with(None, None).await
    }

    /// Same as [`Self::spawn_default`], but lets callers pass cached extension
    /// `info.get` metadata so build-info probing avoids the legacy sidecar shim
    /// when possible.
    #[allow(dead_code)]
    pub async fn spawn_default_with_plugin_info(
        plugin_info: Option<&synaps_cli::extensions::info::PluginInfo>,
    ) -> Result<Self, String> {
        Self::spawn_with(None, plugin_info).await
    }

    /// Discover a sidecar and spawn it using plugin-supplied
    /// [`SidecarSpawnArgs`] (typically obtained via the
    /// `sidecar.spawn_args` RPC by the caller).
    ///
    /// `spawn_args = None` means the plugin didn't provide overrides;
    /// in that case core falls back to the manifest's default model
    /// path (if any).
    pub async fn spawn_with(
        spawn_args: Option<SidecarSpawnArgs>,
        plugin_info: Option<&synaps_cli::extensions::info::PluginInfo>,
    ) -> Result<Self, String> {
        let sidecar = discover().ok_or_else(|| {
            "no plugin provides a sidecar binary; install a sidecar-providing plugin from synaps-skills"
                .to_string()
        })?;
        Self::spawn_for(sidecar, spawn_args, plugin_info).await
    }

    /// Spawn a [`SidecarUiState`] for a specific [`DiscoveredSidecar`]
    /// — used by the multi-sidecar host (Phase 8 8B) which discovers
    /// every sidecar and keys instances by plugin id.
    pub async fn spawn_for(
        sidecar: DiscoveredSidecar,
        spawn_args: Option<SidecarSpawnArgs>,
        plugin_info: Option<&synaps_cli::extensions::info::PluginInfo>,
    ) -> Result<Self, String> {
        if !sidecar.binary.is_file() {
            return Err(format!(
                "sidecar binary not found at {} — run the plugin's setup.sh first",
                sidecar.binary.display()
            ));
        }

        let args = build_spawn_args(&sidecar, spawn_args);
        let config = serde_json::json!({
            "protocol_version": SIDECAR_PROTOCOL_VERSION,
        });

        let mut manager = SidecarManager::spawn(&sidecar.binary, &args, config)
            .await
            .map_err(|err: SidecarError| format!("failed to start sidecar: {}", err))?;

        // Strong post-Init readiness is opt-in; legacy Hello-only sidecars
        // remain compatible. "ready_after_init" promises exactly a ready status
        // only AFTER Init has been processed and triggers can be accepted.
        if manager.ready_after_init() {
            tokio::time::timeout(STARTUP_TIMEOUT, async {
                loop {
                    match manager.next_event().await {
                        Some(SidecarLifecycleEvent::StateChanged { state, .. })
                            if state == "ready" =>
                        {
                            return Ok(())
                        }
                        Some(SidecarLifecycleEvent::Error(error)) => return Err(error),
                        Some(SidecarLifecycleEvent::Exited) | None => {
                            return Err("sidecar exited while loading".to_string())
                        }
                        _ => {} // progress only; idle/stopped are not initialization acknowledgments
                    }
                }
            })
            .await
            .map_err(|_| "sidecar did not report ready after Init within 30s".to_string())??;
        }

        // Read the sidecar's compiled backend straight from the cached
        // `info.get` response (Phase 5). Falls back to None when the plugin
        // hasn't advertised build info yet — the value is only used for the
        // human-readable status line.
        let compiled_backend = plugin_info
            .and_then(|info| info.build.as_ref())
            .map(|b| b.backend.clone());

        Ok(Self {
            manager,
            status: SidecarUiStatus::Idle,
            sidecar,
            armed: false,
            compiled_backend,
            display_name: None,
        })
    }

    /// Set the human-readable display name (from the plugin's
    /// `provides.sidecar.lifecycle.display_name`). Called by the
    /// chatui dispatcher after spawn when a lifecycle claim is known.
    #[allow(dead_code)]
    pub fn set_display_name(&mut self, name: Option<String>) {
        self.display_name = name;
    }

    /// Render a human-readable status line for `/sidecar status`.
    pub fn status_line(&self) -> String {
        format_status_line(
            self.display_name.as_deref(),
            &self.status,
            &self.sidecar.plugin_name,
            &self.sidecar.binary.display().to_string(),
            self.compiled_backend.as_deref(),
        )
    }
}

/// Pure helper backing [`SidecarUiState::status_line`]. Keeps the
/// formatting unit-testable without spawning a real sidecar process.
fn format_status_line(
    display_name: Option<&str>,
    status: &SidecarUiStatus,
    plugin_name: &str,
    binary_path: &str,
    backend: Option<&str>,
) -> String {
    let label = display_name.unwrap_or("sidecar");
    let state = match status {
        SidecarUiStatus::Loading => "loading".to_string(),
        SidecarUiStatus::Idle => "idle".to_string(),
        SidecarUiStatus::Active { label } => label.clone(),
        SidecarUiStatus::Error(msg) => return format!("{label}: error — {msg}"),
    };
    format!(
        "{}: {} ({}) — process: {} | backend: {}",
        label,
        state,
        plugin_name,
        binary_path,
        backend.unwrap_or("unknown"),
    )
}

/// Poll every live sidecar without holding a borrow across UI dispatch.
/// Channel closure is an exit, not an endlessly ready select branch.
pub(crate) async fn next_event(
    sidecars: &mut std::collections::HashMap<String, SidecarUiState>,
) -> (String, SidecarLifecycleEvent) {
    if sidecars.is_empty() {
        return std::future::pending().await;
    }
    let futures: Vec<_> = sidecars
        .iter_mut()
        .map(|(pid, state)| {
            let pid = pid.clone();
            Box::pin(async move {
                let event = state
                    .manager
                    .next_event()
                    .await
                    .unwrap_or(SidecarLifecycleEvent::Exited);
                (pid, event)
            })
        })
        .collect();
    futures::future::select_all(futures).await.0
}

/// Apply a [`SidecarLifecycleEvent`] to the chatui state.
///
/// InsertText payloads are inserted at the cursor position (with a
/// leading space when the existing input doesn't already end in
/// whitespace), so consecutive payloads compose naturally in one line.
pub(crate) fn handle_event(app: &mut App, plugin_id: &str, event: SidecarLifecycleEvent) {
    let Some(v) = app.sidecars.get_mut(plugin_id) else {
        return;
    };
    match event {
        SidecarLifecycleEvent::Ready { .. } => {
            // Duplicate Hello is informational; readiness follows Init status.
        }
        SidecarLifecycleEvent::StateChanged { state, label } => {
            let is_inactive = matches!(state.as_str(), "idle" | "ready" | "stopped");
            if matches!(state.as_str(), "loading" | "initializing") && !v.armed {
                v.status = SidecarUiStatus::Loading;
            } else if is_inactive {
                if !v.armed {
                    v.status = SidecarUiStatus::Idle;
                }
            } else {
                v.status = SidecarUiStatus::Active {
                    label: label.unwrap_or(state),
                };
            }
        }
        SidecarLifecycleEvent::InsertText { text, mode } => match mode {
            InsertTextMode::Append => {
                // Reserved for future live-preview support.
            }
            InsertTextMode::Final | InsertTextMode::Replace => {
                let armed = v.armed;
                insert_text_into_input(app, &text);
                if !armed {
                    if let Some(v) = app.sidecars.get_mut(plugin_id) {
                        v.status = SidecarUiStatus::Idle;
                    }
                }
            }
        },
        SidecarLifecycleEvent::Error(message) => {
            v.status = SidecarUiStatus::Error(message.clone());
            app.push_msg(ChatMessage::Error(format!("sidecar error: {}", message)));
        }
        SidecarLifecycleEvent::Exited => {
            let label = app
                .sidecars
                .get(plugin_id)
                .and_then(|s| s.display_name.clone())
                .unwrap_or_else(|| "sidecar".to_string());
            app.push_msg(ChatMessage::System(format!("{label} exited")));
            app.sidecars.remove(plugin_id);
        }
    }
}

/// Insert text at the current cursor position with sensible whitespace
/// handling. Pure function over `App` so it's unit-testable without any
/// sidecar plumbing.
pub(crate) fn insert_text_into_input(app: &mut App, text: &str) {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return;
    }
    // A leading space is needed only when the char immediately before the
    // cursor exists and is non-whitespace. col > 0 means there's a char on
    // this line before the cursor; at col 0 the preceding char (if any) is a
    // newline — whitespace — so no space is inserted.
    let needs_leading_space = {
        let (row, col) = app.editor.cursor();
        col > 0
            && app
                .editor
                .lines()
                .get(row)
                .and_then(|line| line.chars().nth(col - 1))
                .is_some_and(|c| !c.is_whitespace())
    };
    let to_insert = if needs_leading_space {
        format!(" {}", trimmed)
    } else {
        trimmed.to_string()
    };
    app.insert_at_cursor(&to_insert);
    app.invalidate();
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)] // test mod precedes helper items in this file
mod tests {
    use super::*;
    use synaps_cli::Session;

    fn fresh_app() -> App {
        App::new(Session::new("test", "medium", None))
    }

    #[test]
    fn insert_text_into_empty_input() {
        let mut app = fresh_app();
        insert_text_into_input(&mut app, "hello world");
        assert_eq!(app.input_text(), "hello world");
        assert_eq!(app.cursor_char_pos(), "hello world".chars().count());
    }

    #[cfg(unix)]
    mod async_startup {
        use super::*;
        use std::os::unix::fs::PermissionsExt;
        use std::{path::Path, sync::Arc, time::Duration};

        fn fixture() -> (tempfile::TempDir, synaps_cli::skills::Plugin) {
            let dir = tempfile::tempdir().unwrap();
            let bin = dir.path().join("sidecar.py");
            std::fs::write(
                &bin,
                r#"#!/usr/bin/env python3
import json, os, pathlib, sys, time
root = pathlib.Path(__file__).parent
(root / 'pid').write_text(str(os.getpid()))
with (root / 'spawns').open('a') as f: f.write('spawn\n')
while not (root / 'hello').exists(): time.sleep(.01)
print(json.dumps({'type':'hello','protocol_version':2,'extension':'test-sidecar','capabilities':['ready_after_init']}), flush=True)
for line in sys.stdin:
    msg = json.loads(line)
    with (root / 'commands').open('a') as f: f.write(line)
    if msg['type'] == 'init':
        while not (root / 'ready').exists(): time.sleep(.01)
        print(json.dumps({'type':'status','state':'ready'}), flush=True)
    elif msg['type'] == 'shutdown': break
"#,
            )
            .unwrap();
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700)).unwrap();
            let manifest = serde_json::from_value(serde_json::json!({
                "name": "test-sidecar",
                "provides": {"sidecar": {"command": "sidecar.py", "protocol_version": 2}}
            }))
            .unwrap();
            let plugin = synaps_cli::skills::Plugin {
                name: "test-sidecar".into(),
                root: dir.path().to_path_buf(),
                marketplace: None,
                version: None,
                description: None,
                extension: None,
                manifest: Some(manifest),
            };
            (dir, plugin)
        }

        fn registry(plugin: synaps_cli::skills::Plugin) -> CommandRegistry {
            CommandRegistry::new_with_plugins(&[], vec![], vec![plugin])
        }

        fn manager() -> ExtensionManager {
            let mut manager = synaps_cli::extensions::manager::ExtensionManager::new(Arc::new(
                synaps_cli::extensions::hooks::HookBus::new(),
            ));
            manager.bind_memory_backend(false);
            Arc::new(tokio::sync::RwLock::new(manager))
        }

        async fn wait_for(mut condition: impl FnMut() -> bool) {
            tokio::time::timeout(Duration::from_secs(5), async {
                while !condition() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("fixture condition timed out");
        }
        fn text(path: &Path) -> String {
            std::fs::read_to_string(path).unwrap_or_default()
        }

        async fn complete(app: &mut App, registry: &CommandRegistry) {
            let (pid, result) = tokio::time::timeout(
                Duration::from_secs(5),
                next_startup(&mut app.sidecar_starts),
            )
            .await
            .expect("startup should finish");
            finish_startup(app, registry, pid, result);
        }

        #[tokio::test]
        async fn slow_hello_and_init_do_not_block_or_queue_activation() {
            let (dir, plugin) = fixture();
            let registry = registry(plugin);
            let manager = manager();
            let mut app = fresh_app();
            // Even an unrelated extension load holding the write lock must not
            // stall this sidecar (no extension RPC required by this fixture).
            let _busy = manager.write().await;
            tokio::time::timeout(
                Duration::from_millis(500),
                toggle(&mut app, None, &registry, &manager),
            )
            .await
            .expect("toggle must return before Hello");
            assert!(status(&app, None, &registry).contains("still loading"));
            wait_for(|| dir.path().join("pid").exists()).await;
            toggle(&mut app, None, &registry, &manager).await;
            assert_eq!(app.sidecar_starts.len(), 1);
            assert_eq!(text(&dir.path().join("spawns")).lines().count(), 1);
            assert!(!app.sidecar_starts["test-sidecar"].task.is_finished());
            // UI edits work while the child is waiting for Hello.
            app.set_input_text("responsive");
            assert_eq!(app.input_text(), "responsive");
            std::fs::write(dir.path().join("hello"), "go").unwrap();
            wait_for(|| text(&dir.path().join("commands")).contains("init")).await;
            assert!(
                !app.sidecar_starts["test-sidecar"].task.is_finished(),
                "Hello alone is not ready"
            );
            toggle(&mut app, None, &registry, &manager).await;
            assert!(!text(&dir.path().join("commands")).contains("trigger"));
            std::fs::write(dir.path().join("ready"), "go").unwrap();
            complete(&mut app, &registry).await;
            assert!(app.sidecar_starts.is_empty());
            assert!(!app.sidecars["test-sidecar"].armed);
            assert!(!text(&dir.path().join("commands")).contains("trigger"));
            toggle(&mut app, None, &registry, &manager).await;
            assert!(app.sidecars["test-sidecar"].armed);
            wait_for(|| text(&dir.path().join("commands")).contains("press")).await;
            app.sidecars.clear();
        }

        #[tokio::test]
        async fn legacy_hello_only_completes_unarmed_without_post_init_status() {
            let (dir, plugin) = fixture();
            let bin = dir.path().join("sidecar.py");
            let source =
                text(&bin).replace("'capabilities':['ready_after_init']", "'capabilities':[]");
            std::fs::write(&bin, source).unwrap();
            std::fs::write(dir.path().join("hello"), "go").unwrap();
            // No ready gate: legacy child will never emit post-Init status.
            let registry = registry(plugin);
            let mut app = fresh_app();
            toggle(&mut app, None, &registry, &manager()).await;
            complete(&mut app, &registry).await;
            assert!(!app.sidecars["test-sidecar"].armed);
            assert!(!text(&dir.path().join("commands")).contains("trigger"));
        }

        #[tokio::test]
        async fn negotiated_readiness_error_never_publishes_or_triggers() {
            let (dir, plugin) = fixture();
            let bin = dir.path().join("sidecar.py");
            let source = text(&bin).replace(
                "{'type':'status','state':'ready'}",
                "{'type':'error','message':'initialization failed'}",
            );
            std::fs::write(&bin, source).unwrap();
            std::fs::write(dir.path().join("hello"), "go").unwrap();
            std::fs::write(dir.path().join("ready"), "go").unwrap();
            let registry = registry(plugin);
            let mut app = fresh_app();
            toggle(&mut app, None, &registry, &manager()).await;
            complete(&mut app, &registry).await;
            assert!(app.sidecar_starts.is_empty());
            assert!(app.sidecars.is_empty());
            assert!(!text(&dir.path().join("commands")).contains("trigger"));
        }

        #[tokio::test]
        async fn delayed_completion_drains_loading_before_allowing_a_trigger() {
            let (dir, plugin) = fixture();
            let bin = dir.path().join("sidecar.py");
            let source = text(&bin).replace(
                "print(json.dumps({'type':'status','state':'ready'}), flush=True)",
                "print(json.dumps({'type':'status','state':'ready'}), flush=True)\n        print(json.dumps({'type':'status','state':'loading'}), flush=True)\n        (root / 'loading-sent').write_text('yes')"
            );
            std::fs::write(&bin, source).unwrap();
            std::fs::write(dir.path().join("hello"), "go").unwrap();
            std::fs::write(dir.path().join("ready"), "go").unwrap();
            let registry = registry(plugin);
            let manager = manager();
            let mut app = fresh_app();
            toggle(&mut app, None, &registry, &manager).await;
            wait_for(|| dir.path().join("loading-sent").exists()).await;
            // Delay publishing until the reader has queued both states.
            tokio::time::sleep(Duration::from_millis(30)).await;
            complete(&mut app, &registry).await;
            assert_eq!(
                app.sidecars["test-sidecar"].status,
                SidecarUiStatus::Loading
            );
            toggle(&mut app, None, &registry, &manager).await;
            assert!(!app.sidecars["test-sidecar"].armed);
            assert!(!text(&dir.path().join("commands")).contains("trigger"));
        }

        #[tokio::test]
        async fn startup_timeout_clears_pending_instead_of_falling_back() {
            // Hold the manager lock through the entire bounded startup. Tokio
            // virtual time avoids adding 30 seconds to the test suite.
            let (dir, mut plugin) = fixture();
            plugin.extension = Some(
                serde_json::from_value(serde_json::json!({
                    "runtime":"process", "command":"unused"
                }))
                .unwrap(),
            );
            let registry = registry(plugin);
            let manager = manager();
            let _lock = manager.write().await;
            let mut app = fresh_app();
            tokio::time::pause();
            toggle(&mut app, None, &registry, &manager).await;
            tokio::task::yield_now().await;
            tokio::time::advance(STARTUP_TIMEOUT + Duration::from_secs(1)).await;
            complete(&mut app, &registry).await;
            tokio::time::resume();
            assert!(app.sidecar_starts.is_empty());
            assert!(app.sidecars.is_empty());
            assert!(!dir.path().join("pid").exists());
        }

        #[cfg(target_os = "linux")]
        #[tokio::test]
        async fn disable_cancels_both_loading_and_live_sidecars() {
            for live in [false, true] {
                let (dir, plugin) = fixture();
                let registry = registry(plugin);
                let mut app = fresh_app();
                toggle(&mut app, None, &registry, &manager()).await;
                wait_for(|| dir.path().join("pid").exists()).await;
                let pid = text(&dir.path().join("pid"));
                if live {
                    std::fs::write(dir.path().join("hello"), "go").unwrap();
                    std::fs::write(dir.path().join("ready"), "go").unwrap();
                    complete(&mut app, &registry).await;
                }
                registry.rebuild_with_plugins(vec![], vec![]);
                retain_enabled(&mut app, &registry);
                assert!(app.sidecars.is_empty());
                assert!(app.sidecar_starts.is_empty());
                wait_for(|| dead(&pid)).await;
            }
        }

        #[tokio::test]
        async fn extension_lock_wait_is_background_and_failed_load_is_retryable() {
            let (dir, mut plugin) = fixture();
            plugin.extension = Some(
                serde_json::from_value(serde_json::json!({
                    "runtime":"process", "command":"unused"
                }))
                .unwrap(),
            );
            let registry = registry(plugin);
            let manager = manager();
            let guard = manager.write().await;
            let mut app = fresh_app();
            tokio::time::timeout(
                Duration::from_millis(500),
                toggle(&mut app, None, &registry, &manager),
            )
            .await
            .expect("must not queue UI behind extension loading");
            tokio::task::yield_now().await;
            assert_eq!(app.sidecar_starts.len(), 1);
            assert!(!dir.path().join("pid").exists());
            drop(guard);
            // Extension wasn't registered, so no fallback child may start.
            complete(&mut app, &registry).await;
            assert!(app.sidecar_starts.is_empty());
            assert!(app.sidecars.is_empty());
            assert!(!dir.path().join("pid").exists());
            toggle(&mut app, None, &registry, &manager).await;
            assert_eq!(app.sidecar_starts.len(), 1);
            complete(&mut app, &registry).await;
        }

        #[tokio::test]
        async fn disabled_sidecars_and_registry_removal_never_start() {
            let (dir, plugin) = fixture();
            let registry = registry(plugin);
            let manager = manager();
            let mut app = fresh_app();
            app.sidecars_disabled = true;
            toggle(&mut app, None, &registry, &manager).await;
            assert!(app.sidecar_starts.is_empty());
            assert!(status(&app, None, &registry).contains("disabled"));
            app.sidecars_disabled = false;
            registry.rebuild_with_plugins(vec![], vec![]);
            toggle(&mut app, Some("test-sidecar".into()), &registry, &manager).await;
            assert!(app.sidecar_starts.is_empty());
            assert!(!dir.path().join("pid").exists());
        }

        #[tokio::test]
        async fn panic_clears_loading_and_allows_retry() {
            let (dir, plugin) = fixture();
            let registry = registry(plugin);
            let manager = manager();
            let mut app = fresh_app();
            app.sidecar_starts.insert(
                "test-sidecar".into(),
                SidecarStartup {
                    task: tokio::spawn(async { panic!("synthetic startup failure") }),
                    sidecar: registry.sidecars()[0].clone(),
                    label: "test".into(),
                },
            );
            complete(&mut app, &registry).await;
            assert!(app.sidecar_starts.is_empty());
            toggle(&mut app, None, &registry, &manager).await;
            assert_eq!(app.sidecar_starts.len(), 1);
            app.sidecar_starts.clear();
            drop(dir);
        }

        #[cfg(target_os = "linux")]
        fn dead(pid: &str) -> bool {
            let stat = text(&std::path::PathBuf::from(format!(
                "/proc/{}/stat",
                pid.trim()
            )));
            stat.is_empty()
                || stat
                    .split(')')
                    .nth(1)
                    .is_some_and(|s| s.trim_start().starts_with('Z'))
        }

        #[cfg(target_os = "linux")]
        #[tokio::test]
        async fn drop_loading_app_kills_child_and_stale_success_is_not_published() {
            for stale_success in [false, true] {
                let (dir, plugin) = fixture();
                let registry = registry(plugin);
                let manager = manager();
                let mut app = fresh_app();
                toggle(&mut app, None, &registry, &manager).await;
                wait_for(|| dir.path().join("pid").exists()).await;
                let pid = text(&dir.path().join("pid"));
                if stale_success {
                    std::fs::write(dir.path().join("hello"), "go").unwrap();
                    std::fs::write(dir.path().join("ready"), "go").unwrap();
                    let (key, result) = tokio::time::timeout(
                        Duration::from_secs(5),
                        next_startup(&mut app.sidecar_starts),
                    )
                    .await
                    .unwrap();
                    assert!(result.is_ok());
                    registry.rebuild_with_plugins(vec![], vec![]);
                    finish_startup(&mut app, &registry, key, result);
                    assert!(app.sidecars.is_empty());
                }
                drop(app);
                wait_for(|| dead(&pid)).await;
            }
        }
    }

    #[test]
    fn only_unsupported_spawn_args_can_use_defaults() {
        assert!(spawn_args_unsupported("Extension error: method not found"));
        assert!(spawn_args_unsupported(
            "extension runtime does not support sidecar.spawn_args"
        ));
        for error in [
            "activation denied",
            "sidecar.spawn_args timed out",
            "invalid response",
            "denied -32601",
        ] {
            assert!(!spawn_args_unsupported(error));
        }
    }

    // ---- build_spawn_args tests ---------------------------------------

    fn discovered(default_model: Option<&str>) -> DiscoveredSidecar {
        use synaps_cli::skills::manifest::SidecarModel;
        DiscoveredSidecar {
            plugin_name: "anything".into(),
            plugin_root: std::path::PathBuf::from("/opt/anything"),
            binary: std::path::PathBuf::from("/opt/anything/bin/sidecar"),
            protocol_version: 1,
            setup_script: None,
            model: default_model.map(|p| SidecarModel {
                default_path: Some(p.to_string()),
                required: false,
            }),
            lifecycle: None,
        }
    }

    #[test]
    fn build_spawn_args_uses_plugin_args_verbatim() {
        let sidecar = discovered(None);
        let args = SidecarSpawnArgs {
            args: vec!["--foo".into(), "bar".into()],
            language: Some("fr".into()),
        };
        let out_args = build_spawn_args(&sidecar, Some(args));
        assert_eq!(out_args, vec!["--foo", "bar"]);
    }

    #[test]
    fn build_spawn_args_appends_manifest_default_when_file_exists() {
        // Use Cargo.toml as a known-existing file so the file-existence
        // check passes deterministically across machines.
        let cargo_toml = std::env::current_dir().unwrap().join("Cargo.toml");
        let path_str = cargo_toml.to_string_lossy().into_owned();
        let sidecar = discovered(Some(&path_str));
        let out_args = build_spawn_args(&sidecar, None);
        assert_eq!(out_args.len(), 2);
        assert_eq!(out_args[0], "--model-path");
        assert_eq!(out_args[1], path_str);
    }

    #[test]
    fn build_spawn_args_skips_manifest_default_when_file_missing() {
        let sidecar = discovered(Some("/definitely/not/a/real/path/xyz.bin"));
        let out_args = build_spawn_args(&sidecar, None);
        assert!(
            out_args.is_empty(),
            "missing default file must not produce args, got {out_args:?}"
        );
    }

    #[test]
    fn build_spawn_args_does_not_double_up_model_path() {
        let cargo_toml = std::env::current_dir().unwrap().join("Cargo.toml");
        let sidecar = discovered(Some(&cargo_toml.to_string_lossy()));
        let plugin_args = SidecarSpawnArgs {
            args: vec!["--model-path".into(), "/plugin/chosen.bin".into()],
            language: None,
        };
        let out_args = build_spawn_args(&sidecar, Some(plugin_args));
        // Only one --model-path, and it's the plugin's choice.
        let count = out_args.iter().filter(|a| *a == "--model-path").count();
        assert_eq!(count, 1);
        assert_eq!(out_args, vec!["--model-path", "/plugin/chosen.bin"]);
    }

    #[test]
    fn build_spawn_args_returns_empty_when_no_plugin_args_and_no_manifest_default() {
        let sidecar = discovered(None);
        let out_args = build_spawn_args(&sidecar, None);
        assert!(out_args.is_empty());
    }

    #[test]
    fn build_spawn_args_with_none_spawn_args_falls_back_to_manifest() {
        let cargo_toml = std::env::current_dir().unwrap().join("Cargo.toml");
        let sidecar = discovered(Some(&cargo_toml.to_string_lossy()));
        let out_args = build_spawn_args(&sidecar, None);
        assert_eq!(out_args[0], "--model-path");
    }

    // ---- existing insert_text tests -----------------------------

    #[test]
    fn insert_text_appends_with_leading_space() {
        let mut app = fresh_app();
        app.set_input_text("first");
        insert_text_into_input(&mut app, "second sentence");
        assert_eq!(app.input_text(), "first second sentence");
        assert_eq!(
            app.cursor_char_pos(),
            "first second sentence".chars().count()
        );
    }

    #[test]
    fn insert_text_no_double_space_when_input_ends_with_space() {
        let mut app = fresh_app();
        app.set_input_text("first ");
        insert_text_into_input(&mut app, "second");
        assert_eq!(app.input_text(), "first second");
    }

    #[test]
    fn insert_text_trims_whitespace_from_payload() {
        let mut app = fresh_app();
        insert_text_into_input(&mut app, "  spaced text  ");
        assert_eq!(app.input_text(), "spaced text");
    }

    #[test]
    fn insert_text_ignores_empty_or_whitespace_only() {
        let mut app = fresh_app();
        insert_text_into_input(&mut app, "");
        insert_text_into_input(&mut app, "   ");
        assert_eq!(app.input_text(), "");
        assert_eq!(app.cursor_char_pos(), 0);
    }

    #[test]
    fn insert_text_inserts_at_cursor_not_end() {
        let mut app = fresh_app();
        app.set_input_text("hello world");
        // Place cursor between "hello" and " world" (after "hello")
        app.editor.move_cursor(tui_textarea::CursorMove::Jump(0, 5));
        insert_text_into_input(&mut app, "beautiful");
        assert_eq!(app.input_text(), "hello beautiful world");
    }

    // ---- status_line label tests --------------------------------------

    #[test]
    fn status_line_uses_display_name_when_set() {
        let line = format_status_line(
            Some("Sensor"),
            &SidecarUiStatus::Idle,
            "sample-sidecar",
            "/opt/sample-sidecar/bin/sidecar",
            Some("metal"),
        );
        assert!(line.starts_with("Sensor:"), "got: {line}");
    }

    #[test]
    fn status_line_falls_back_to_sidecar_when_no_display_name() {
        let line = format_status_line(
            None,
            &SidecarUiStatus::Idle,
            "sample-sidecar",
            "/opt/sample-sidecar/bin/sidecar",
            Some("metal"),
        );
        assert!(line.starts_with("sidecar:"), "got: {line}");
    }

    #[test]
    fn status_line_uses_display_name_for_error_state() {
        let line = format_status_line(
            Some("Sensor"),
            &SidecarUiStatus::Error("oops".into()),
            "sample-sidecar",
            "/opt/sample-sidecar/bin/sidecar",
            None,
        );
        assert_eq!(line, "Sensor: error — oops");
    }
}

fn expand_tilde(path: String) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            let mut full = std::path::PathBuf::from(home);
            full.push(rest);
            return full.to_string_lossy().into_owned();
        }
    }
    path
}

/// Combine plugin-supplied [`SidecarSpawnArgs`] with manifest defaults.
///
/// Returns command-line args ready for [`SidecarManager::spawn`]. Core does
/// not interpret sidecar-specific config; the plugin owns its CLI and Init
/// schemas.
///
/// Logic:
/// - If the plugin returned spawn args, take its `args` verbatim.
/// - If the plugin's args don't already include `--model-path` and the
///   manifest declares a `default_path`, append `--model-path <expanded>`
///   when the file actually exists. Plugins that opt out of model-path
///   bootstrapping by including `--model-path` themselves keep full control.
/// - If `spawn_args` is `None`, only the manifest default applies.
///
/// This function is pure and unit-tested below.
fn build_spawn_args(
    sidecar: &DiscoveredSidecar,
    spawn_args: Option<SidecarSpawnArgs>,
) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();

    if let Some(plugin_args) = spawn_args {
        args.extend(plugin_args.args);
    }

    let already_has_model_path = args.iter().any(|a| a == "--model-path");
    if !already_has_model_path {
        if let Some(default_path) = sidecar
            .model
            .as_ref()
            .and_then(|m| m.default_path.clone())
            .map(expand_tilde)
        {
            if std::path::Path::new(&default_path).is_file() {
                args.push("--model-path".to_string());
                args.push(default_path);
            }
        }
    }

    args
}
