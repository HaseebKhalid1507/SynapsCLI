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
use synaps_cli::sidecar::protocol::{
    SidecarConfig, SidecarProviderState, SidecarSessionMode, SIDECAR_PROTOCOL_VERSION,
};
use synaps_cli::sidecar::spawn::SidecarSpawnArgs;

use super::app::{App, ChatMessage};

/// What the chatui currently shows for the sidecar indicator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SidecarUiStatus {
    /// Manager is alive but not currently capturing audio.
    Idle,
    /// Sidecar reported `ListeningStarted`.
    Listening,
    /// Sidecar reported `TranscribingStarted`.
    Transcribing,
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
}

impl SidecarUiState {
    /// Discover a sidecar from loaded plugins and spawn its manager
    /// with a default dictation-mode handshake.
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

        if !sidecar.binary.is_file() {
            return Err(format!(
                "sidecar binary not found at {} — run the plugin's setup.sh first",
                sidecar.binary.display()
            ));
        }

        let (args, language) = build_spawn_args(&sidecar, spawn_args);

        let manager = SidecarManager::spawn(
            &sidecar.binary,
            &args,
            SidecarConfig {
                mode: SidecarSessionMode::Dictation,
                language: language.clone(),
                protocol_version: SIDECAR_PROTOCOL_VERSION,
            },
        )
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
        })
    }

    /// Render a human-readable status line for `/sidecar status`.
    pub fn status_line(&self) -> String {
        let state = match &self.status {
            SidecarUiStatus::Idle => "idle",
            SidecarUiStatus::Listening => "listening",
            SidecarUiStatus::Transcribing => "transcribing",
            SidecarUiStatus::Error(msg) => return format!("sidecar: error — {}", msg),
        };
        format!(
            "sidecar: {} ({}) — process: {} | backend: {}",
            state,
            self.sidecar.plugin_name,
            self.sidecar.binary.display(),
            self.compiled_backend.as_deref().unwrap_or("unknown")
        )
    }
}

/// Apply a [`SidecarLifecycleEvent`] to the chatui state.
///
/// Final transcripts are inserted at the cursor position (with a
/// leading space when the existing input doesn't already end in
/// whitespace), so the user can keep dictating into the same line.
pub(crate) fn handle_event(app: &mut App, event: SidecarLifecycleEvent) {
    let Some(v) = app.sidecar.as_mut() else {
        return;
    };
    match event {
        SidecarLifecycleEvent::Ready { .. } => {
            // Sidecar handshake is informational; we already pressed.
        }
        SidecarLifecycleEvent::StateChanged(state) => match state {
            SidecarProviderState::Listening => v.status = SidecarUiStatus::Listening,
            SidecarProviderState::Transcribing => v.status = SidecarUiStatus::Transcribing,
            SidecarProviderState::Ready | SidecarProviderState::Stopped => {
                // Only fall back to Idle when the user has actually
                // released. Otherwise the VAD is just between utterances
                // and the sidecar is still armed.
                if !v.armed {
                    v.status = SidecarUiStatus::Idle;
                }
            }
            SidecarProviderState::Error => {
                v.status = SidecarUiStatus::Error("sidecar reported error state".into())
            }
            SidecarProviderState::Speaking => {}
        },
        SidecarLifecycleEvent::ListeningStarted => {
            v.status = SidecarUiStatus::Listening;
        }
        SidecarLifecycleEvent::ListeningStopped => {
            // The STT provider emits ListeningStopped between VAD
            // utterances *and* on real shutdown. Only clear status if the
            // user has unarmed (toggled off).
            if !v.armed {
                v.status = SidecarUiStatus::Idle;
            }
        }
        SidecarLifecycleEvent::TranscribingStarted => {
            v.status = SidecarUiStatus::Transcribing;
        }
        SidecarLifecycleEvent::PartialTranscript(_) => {
            // Reserved for V5+ — drop for now.
        }
        SidecarLifecycleEvent::FinalTranscript(text) => {
            // Insert text but do NOT reset status: the VAD will keep
            // emitting more utterances until the user toggles off.
            let armed = v.armed;
            insert_transcript_into_input(app, &text);
            if !armed {
                // Re-borrow because insert_transcript_into_input took
                // a mutable borrow of `app`.
                if let Some(v) = app.sidecar.as_mut() {
                    v.status = SidecarUiStatus::Idle;
                }
            }
        }
        SidecarLifecycleEvent::Error(message) => {
            v.status = SidecarUiStatus::Error(message.clone());
            app.push_msg(ChatMessage::Error(format!(
                "sidecar error: {}",
                message
            )));
        }
        SidecarLifecycleEvent::Exited => {
            app.push_msg(ChatMessage::System("sidecar exited".to_string()));
            app.sidecar = None;
        }
    }
}

/// Insert a transcript at the current cursor position with sensible
/// whitespace handling. Pure function over `App` so it's unit-testable
/// without any sidecar plumbing.
pub(crate) fn insert_transcript_into_input(app: &mut App, transcript: &str) {
    let trimmed = transcript.trim();
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
    fn insert_transcript_into_empty_input() {
        let mut app = fresh_app();
        insert_transcript_into_input(&mut app, "hello world");
        assert_eq!(app.input, "hello world");
        assert_eq!(app.cursor_pos, "hello world".chars().count());
    }

    // ---- build_spawn_args tests ---------------------------------------

    fn discovered(default_model: Option<&str>) -> DiscoveredSidecar {
        use synaps_cli::skills::manifest::VoiceSidecarModel;
        DiscoveredSidecar {
            plugin_name: "anything".into(),
            plugin_root: std::path::PathBuf::from("/opt/anything"),
            binary: std::path::PathBuf::from("/opt/anything/bin/sidecar"),
            protocol_version: 1,
            setup_script: None,
            model: default_model.map(|p| VoiceSidecarModel {
                default_path: Some(p.to_string()),
                required_for_real_stt: false,
            }),
        }
    }

    #[test]
    fn build_spawn_args_uses_plugin_args_verbatim() {
        let sidecar = discovered(None);
        let args = SidecarSpawnArgs {
            args: vec!["--foo".into(), "bar".into()],
            language: Some("fr".into()),
        };
        let (out_args, lang) = build_spawn_args(&sidecar, Some(args));
        assert_eq!(out_args, vec!["--foo", "bar"]);
        assert_eq!(lang.as_deref(), Some("fr"));
    }

    #[test]
    fn build_spawn_args_treats_auto_language_as_none() {
        let sidecar = discovered(None);
        for sentinel in ["", " ", "?", "auto", "(auto)"] {
            let args = SidecarSpawnArgs {
                args: vec![],
                language: Some(sentinel.into()),
            };
            let (_, lang) = build_spawn_args(&sidecar, Some(args));
            assert_eq!(lang, None, "sentinel `{sentinel}` should map to None");
        }
    }

    #[test]
    fn build_spawn_args_appends_manifest_default_when_file_exists() {
        // Use Cargo.toml as a known-existing file so the file-existence
        // check passes deterministically across machines.
        let cargo_toml = std::env::current_dir().unwrap().join("Cargo.toml");
        let path_str = cargo_toml.to_string_lossy().into_owned();
        let sidecar = discovered(Some(&path_str));
        let (out_args, _) = build_spawn_args(&sidecar, None);
        assert_eq!(out_args.len(), 2);
        assert_eq!(out_args[0], "--model-path");
        assert_eq!(out_args[1], path_str);
    }

    #[test]
    fn build_spawn_args_skips_manifest_default_when_file_missing() {
        let sidecar = discovered(Some("/definitely/not/a/real/path/xyz.bin"));
        let (out_args, _) = build_spawn_args(&sidecar, None);
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
        let (out_args, _) = build_spawn_args(&sidecar, Some(plugin_args));
        // Only one --model-path, and it's the plugin's choice.
        let count = out_args.iter().filter(|a| *a == "--model-path").count();
        assert_eq!(count, 1);
        assert_eq!(out_args, vec!["--model-path", "/plugin/chosen.bin"]);
    }

    #[test]
    fn build_spawn_args_returns_empty_when_no_plugin_args_and_no_manifest_default() {
        let sidecar = discovered(None);
        let (out_args, lang) = build_spawn_args(&sidecar, None);
        assert!(out_args.is_empty());
        assert_eq!(lang, None);
    }

    #[test]
    fn build_spawn_args_with_none_spawn_args_falls_back_to_manifest() {
        let cargo_toml = std::env::current_dir().unwrap().join("Cargo.toml");
        let sidecar = discovered(Some(&cargo_toml.to_string_lossy()));
        let (out_args, lang) = build_spawn_args(&sidecar, None);
        assert_eq!(out_args[0], "--model-path");
        assert_eq!(lang, None);
    }

    // ---- existing insert_transcript tests -----------------------------

    #[test]
    fn insert_transcript_appends_with_leading_space() {
        let mut app = fresh_app();
        app.input = "first".to_string();
        app.cursor_pos = "first".chars().count();
        insert_transcript_into_input(&mut app, "second sentence");
        assert_eq!(app.input, "first second sentence");
        assert_eq!(app.cursor_pos, "first second sentence".chars().count());
    }

    #[test]
    fn insert_transcript_no_double_space_when_input_ends_with_space() {
        let mut app = fresh_app();
        app.input = "first ".to_string();
        app.cursor_pos = "first ".chars().count();
        insert_transcript_into_input(&mut app, "second");
        assert_eq!(app.input, "first second");
    }

    #[test]
    fn insert_transcript_trims_whitespace_from_payload() {
        let mut app = fresh_app();
        insert_transcript_into_input(&mut app, "  spaced text  ");
        assert_eq!(app.input, "spaced text");
    }

    #[test]
    fn insert_transcript_ignores_empty_or_whitespace_only() {
        let mut app = fresh_app();
        insert_transcript_into_input(&mut app, "");
        insert_transcript_into_input(&mut app, "   ");
        assert_eq!(app.input, "");
        assert_eq!(app.cursor_pos, 0);
    }

    #[test]
    fn insert_transcript_inserts_at_cursor_not_end() {
        let mut app = fresh_app();
        app.input = "hello world".to_string();
        // Place cursor between "hello" and " world" (after "hello")
        app.cursor_pos = 5;
        insert_transcript_into_input(&mut app, "beautiful");
        assert_eq!(app.input, "hello beautiful world");
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
/// Returns `(args, language)` ready for [`SidecarManager::spawn`] and the
/// [`SidecarConfig`] handshake.
///
/// Logic:
/// - If the plugin returned spawn args, take its `args` verbatim and its
///   `language` for the handshake.
/// - If the plugin's args don't already include `--model-path` and the
///   manifest declares a `default_path`, append `--model-path <expanded>`
///   when the file actually exists. Plugins that opt out of model-path
///   bootstrapping by including `--model-path` themselves (or by passing
///   an explicit empty list) keep full control.
/// - If `spawn_args` is `None`, only the manifest default applies.
///
/// This function is pure and unit-tested below.
fn build_spawn_args(
    sidecar: &DiscoveredSidecar,
    spawn_args: Option<SidecarSpawnArgs>,
) -> (Vec<String>, Option<String>) {
    let mut args: Vec<String> = Vec::new();
    let mut language: Option<String> = None;

    if let Some(plugin_args) = spawn_args {
        args.extend(plugin_args.args);
        language = plugin_args.language.and_then(|s| {
            let trimmed = s.trim().to_string();
            if trimmed.is_empty()
                || trimmed == "?"
                || trimmed == "auto"
                || trimmed == "(auto)"
            {
                None
            } else {
                Some(trimmed)
            }
        });
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

    (args, language)
}
