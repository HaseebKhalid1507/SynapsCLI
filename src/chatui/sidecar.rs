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
use synaps_cli::sidecar::manager::{SidecarManager, SidecarError, SidecarLifecycleEvent};
use synaps_cli::sidecar::protocol::{InsertTextMode, SIDECAR_PROTOCOL_VERSION};
use synaps_cli::sidecar::spawn::SidecarSpawnArgs;

use super::app::{App, ChatMessage};

/// What the chatui currently shows for the sidecar indicator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SidecarUiStatus {
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

        let manager = SidecarManager::spawn(&sidecar.binary, &args, config)
        .await
        .map_err(|err: SidecarError| format!("failed to start sidecar: {}", err))?;

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
            // Sidecar handshake is informational; we already pressed.
        }
        SidecarLifecycleEvent::StateChanged { state, label } => {
            let is_inactive = matches!(state.as_str(), "idle" | "ready" | "stopped");
            if is_inactive {
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
            app.push_msg(ChatMessage::Error(format!(
                "sidecar error: {}",
                message
            )));
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
    let needs_leading_space = !app.input.is_empty()
        && app.cursor_byte_pos() > 0
        && !app
            .input
            .as_bytes()
            .get(app.cursor_byte_pos().saturating_sub(1))
            .copied()
            .map(|b| (b as char).is_whitespace())
            .unwrap_or(true);
    let to_insert = if needs_leading_space {
        format!(" {}", trimmed)
    } else {
        trimmed.to_string()
    };
    let byte_pos = app.cursor_byte_pos();
    app.input.insert_str(byte_pos, &to_insert);
    app.cursor_pos += to_insert.chars().count();
    app.invalidate();
}

#[cfg(test)]
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
        assert_eq!(app.input, "hello world");
        assert_eq!(app.cursor_pos, "hello world".chars().count());
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
        assert!(out_args.is_empty(), "missing default file must not produce args, got {out_args:?}");
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
        app.input = "first".to_string();
        app.cursor_pos = "first".chars().count();
        insert_text_into_input(&mut app, "second sentence");
        assert_eq!(app.input, "first second sentence");
        assert_eq!(app.cursor_pos, "first second sentence".chars().count());
    }

    #[test]
    fn insert_text_no_double_space_when_input_ends_with_space() {
        let mut app = fresh_app();
        app.input = "first ".to_string();
        app.cursor_pos = "first ".chars().count();
        insert_text_into_input(&mut app, "second");
        assert_eq!(app.input, "first second");
    }

    #[test]
    fn insert_text_trims_whitespace_from_payload() {
        let mut app = fresh_app();
        insert_text_into_input(&mut app, "  spaced text  ");
        assert_eq!(app.input, "spaced text");
    }

    #[test]
    fn insert_text_ignores_empty_or_whitespace_only() {
        let mut app = fresh_app();
        insert_text_into_input(&mut app, "");
        insert_text_into_input(&mut app, "   ");
        assert_eq!(app.input, "");
        assert_eq!(app.cursor_pos, 0);
    }

    #[test]
    fn insert_text_inserts_at_cursor_not_end() {
        let mut app = fresh_app();
        app.input = "hello world".to_string();
        // Place cursor between "hello" and " world" (after "hello")
        app.cursor_pos = 5;
        insert_text_into_input(&mut app, "beautiful");
        assert_eq!(app.input, "hello beautiful world");
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
