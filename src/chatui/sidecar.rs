//! Chatui-side glue for the sidecar subsystem.
//!
//! Owns the `SidecarUiState` held on `App.sidecar` and provides helpers
//! the slash-command dispatcher and event loop call into. The actual
//! sidecar lifecycle lives in `crate::sidecar::manager::SidecarManager`.
//!
//! NOTE (Phase 7 deferred): the bootstrap code here still reads
//! `local-voice` plugin-namespace config keys directly. That's a known
//! leakage; the next slice will replace it with an RPC asking the
//! plugin to self-configure, removing all plugin-name knowledge from
//! core.

use synaps_cli::sidecar::discovery::{discover, DiscoveredSidecar};
use synaps_cli::sidecar::manager::{SidecarManager, SidecarError, SidecarLifecycleEvent};
use synaps_cli::sidecar::protocol::{
    SidecarConfig, SidecarProviderState, SidecarSessionMode, SIDECAR_PROTOCOL_VERSION,
};

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
        Self::spawn_default_with_plugin_info(None).await
    }

    /// Same as [`Self::spawn_default`], but lets callers pass cached extension
    /// `info.get` metadata so build-info probing avoids the legacy sidecar shim
    /// when possible.
    pub async fn spawn_default_with_plugin_info(
        plugin_info: Option<&synaps_cli::extensions::info::PluginInfo>,
    ) -> Result<Self, String> {
        let sidecar = discover().ok_or_else(|| {
            "no plugin provides a voice sidecar; install the local-voice plugin from synaps-skills"
                .to_string()
        })?;

        if !sidecar.binary.is_file() {
            return Err(format!(
                "sidecar binary not found at {} — run the plugin's setup.sh first",
                sidecar.binary.display()
            ));
        }

        // Read voice language. Source of truth: the local-voice plugin's
        // own config namespace (`local-voice.language`); legacy global
        // `voice_language` keys are still read for one-release back-compat.
        let language = read_local_voice_setting("language", "voice_language")
            .map(|s| s.trim().to_string())
            .filter(|s| {
                !s.is_empty()
                    && s != "?"
                    && s != "auto"
                    && s != "(auto)"
            });

        // Build sidecar args: prefer `local-voice.model_path` from the plugin
        // config (legacy fallback `voice_stt_model_path` global key), else
        // fall back to the manifest's `provides.voice_sidecar.model.default_path`.
        // Tilde-expand and verify the file exists before passing it.
        let mut args: Vec<String> = Vec::new();
        let model_override = read_local_voice_setting("model_path", "voice_stt_model_path")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let model_default = sidecar
            .model
            .as_ref()
            .and_then(|m| m.default_path.clone());
        let resolved_model = model_override.or(model_default).map(expand_tilde);
        if let Some(path) = resolved_model {
            if std::path::Path::new(&path).is_file() {
                args.push("--model-path".to_string());
                args.push(path);
            }
        }
        if let Some(lang) = language.as_deref() {
            args.push("--language".to_string());
            args.push(lang.to_string());
        }

        let manager = SidecarManager::spawn(
            &sidecar.binary,
            &args,
            SidecarConfig {
                mode: SidecarSessionMode::Dictation,
                language,
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

/// Resolve a local-voice setting value: prefer the plugin's own namespaced
/// config (`~/.synaps-cli/plugins/local-voice/config`), fall back to the
/// legacy global key one release for back-compat. Logs a deprecation hint
/// when the legacy key is used.
fn read_local_voice_setting(plugin_key: &str, legacy_global_key: &str) -> Option<String> {
    if let Some(v) = synaps_cli::extensions::config_store::read_plugin_config(
        "local-voice",
        plugin_key,
    ) {
        return Some(v);
    }
    if let Some(v) = synaps_cli::config::read_config_value(legacy_global_key) {
        tracing::warn!(
            "voice: legacy global config key `{}` is deprecated; \
             move it under `~/.synaps-cli/plugins/local-voice/config` as `{} = ...`",
            legacy_global_key,
            plugin_key
        );
        return Some(v);
    }
    None
}
