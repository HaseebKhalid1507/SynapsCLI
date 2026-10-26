//! Single source of truth for every tweakable setting.
//!
//! One macro invocation generates both the UI schema (`ALL_SETTINGS`) and the
//! apply dispatch (`apply_setting_dispatch`). Add a setting here and both
//! sides stay in sync — drift is impossible.
//!
//! Runtime-backed keys produce a [`SettingApply::Session`] (the
//! `SessionSetting` the caller sends as `Set{id, ..}`); local keys apply
//! synchronously and yield [`SettingApply::Local`].

use super::schema::{Category, EditorKind, SettingDef};
use agent_engine::session::SessionSetting;

/// What a settings apply resolves to (PLAN-phase3 §3.2 settings/defs).
pub(crate) enum SettingApply {
    /// Send this to the session; the reply decides the config write.
    Session(SessionSetting),
    /// Applied locally (theme, keybinds, …) or a no-op; `Err` = rejected
    /// (the caller must NOT write the value to the config file).
    Local(Result<(), String>),
}

/// Cycler rows whose Enter also opens a free-text editor, for values the
/// parser accepts beyond the preset list (e.g. `cache_keepwarm = 6h`).
pub(crate) const TYPED_CYCLERS: &[&str] = &["cache_keepwarm"];

/// Config keys the parser accepts that deliberately have NO /settings row,
/// with the reason. Every key in `config::KNOWN_CONFIG_KEYS` and
/// `config::FAMILY_CONFIG_KEYS` must be a row in `ALL_SETTINGS` or listed
/// here (enforced by `every_config_key_has_a_row_or_an_exclusion`), so a
/// new config key cannot silently skip the settings UI. Keep this tight.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const EXCLUDED_CONFIG_KEYS: &[(&str, &str)] = &[
    // Dead config: parsed by `load_config` but read by nothing (the shell
    // tool is built from `ShellConfig::default()`; no runtime consumer of
    // the continuous-memory policy fields). No row until a PR wires the key
    // up — that PR must move it back to a row in `define_settings!`.
    ("shell.default_cols", NOT_READ_YET),
    ("shell.default_rows", NOT_READ_YET),
    ("shell.idle_timeout", NOT_READ_YET),
    ("shell.max_output", NOT_READ_YET),
    ("shell.max_sessions", NOT_READ_YET),
    ("shell.readiness_strategy", NOT_READ_YET),
    ("shell.readiness_timeout_ms", NOT_READ_YET),
    ("shell.max_readiness_timeout_ms", NOT_READ_YET),
    ("memory.capture_user", NOT_READ_YET),
    ("memory.capture_assistant", NOT_READ_YET),
    ("memory.capture_tools", NOT_READ_YET),
    ("memory.recall_max_records", NOT_READ_YET),
    ("memory.recall_max_tokens", NOT_READ_YET),
    ("memory.recall_timeout_ms", NOT_READ_YET),
    ("memory.auto_consolidate", NOT_READ_YET),
    ("memory.local_embeddings", NOT_READ_YET),
    ("memory.gliner", NOT_READ_YET),
    ("server.token", "secret: never echoed in a TUI"),
    ("auth.machine_token", "secret: never echoed in a TUI"),
    ("auth.remote_endpoint", "credential-broker wiring set by deployment, not a preference"),
    ("server.allowed_origins", "WebSocket server mode only"),
    ("server.auto_approve_confirms", "security-sensitive WebSocket server-mode policy"),
    ("server.max_message_size", "WebSocket server mode only"),
    ("bridge.uds_path", "internal sidecar plumbing"),
    ("bridge.heartbeat_mirror", "internal sidecar plumbing"),
    ("bridge.heartbeat_timeout_ms", "internal sidecar plumbing"),
    ("memory.default_mode_confirmed", "operator consent marker: must be set by hand"),
    ("subagent_max_concurrent", "legacy flat alias of subagent.max_concurrent (edit that row)"),
    ("subagent_max_total", "legacy flat alias of subagent.max_total (edit that row)"),
    ("favorite_models", "owned by the model picker (/models favorites)"),
    ("disabled_plugins", "owned by the Plugins tab"),
    ("provider.*", "owned by the Providers tab (API keys)"),
    ("keybind.*", "keybind family (keybind registry), not a single setting"),
];

/// Reason for dead-config exclusions (see `EXCLUDED_CONFIG_KEYS`).
#[cfg_attr(not(test), allow(dead_code))]
const NOT_READ_YET: &str =
    "parsed but not read by the runtime yet (no-op); wiring it up must restore its settings row";

/// Config-only rows: validate with the real config parser over the whole
/// file as it would look after the write, so /settings can never persist a
/// value `load_config` rejects (including cross-key rules). `Err` carries
/// the parser's own message and the caller skips the write.
fn config_edit(key: &str, value: &str) -> SettingApply {
    SettingApply::Local(synaps_cli::config::validate_config_edit(key, value))
}

/// `on` / `off` only (what the cycler emits), then the parser check.
fn on_off(key: &str, value: &str) -> SettingApply {
    match value {
        "on" | "off" => config_edit(key, value),
        _ => SettingApply::Local(Err("expected on or off".to_string())),
    }
}

/// A non-negative whole number, then the parser check (ranges, widths).
fn whole_number(key: &str, value: &str) -> SettingApply {
    match value.trim().parse::<u64>() {
        Ok(_) => config_edit(key, value),
        Err(_) => SettingApply::Local(Err("expected a whole number".to_string())),
    }
}

/// A per-turn count limit: a positive whole number or `unlimited`. A new `0`
/// is refused: it still means "none allowed" in config, but is deprecated.
fn turn_limit(key: &str, value: &str) -> SettingApply {
    let v = value.trim();
    if v.eq_ignore_ascii_case("unlimited") {
        return config_edit(key, v);
    }
    match v.parse::<u64>() {
        Ok(0) => SettingApply::Local(Err(
            "0 (none allowed) is deprecated; enter unlimited for no limit, or a positive number"
                .to_string(),
        )),
        Ok(_) => config_edit(key, value),
        Err(_) => SettingApply::Local(Err("expected a positive whole number or unlimited".to_string())),
    }
}

/// Free text that must not be blank (the parser would store it verbatim).
fn non_empty_text(key: &str, value: &str) -> SettingApply {
    if value.trim().is_empty() {
        SettingApply::Local(Err("must not be empty".to_string()))
    } else {
        config_edit(key, value)
    }
}

/// A finite, non-negative dollar amount. Stricter than the parser, which
/// also takes `inf`/`NaN`/negatives as an f64.
fn budget_usd(key: &str, value: &str) -> SettingApply {
    match value.trim().parse::<f64>() {
        Ok(usd) if usd.is_finite() && usd >= 0.0 => config_edit(key, value),
        _ => SettingApply::Local(Err("expected a dollar amount, e.g. 2.50".to_string())),
    }
}

macro_rules! define_settings {
    ($(
        $key:literal, $label:expr, $category:ident, $editor:expr, $help:expr,
            $apply:expr;
    )*) => {
        pub(crate) const ALL_SETTINGS: &[SettingDef] = &[
            $(
                SettingDef {
                    key: $key,
                    label: $label,
                    category: Category::$category,
                    editor: $editor,
                    help: $help,
                },
            )*
        ];

        /// Resolve a setting by key/value: a `SessionSetting` to send, or the
        /// local outcome. Handlers that do not perform validation always
        /// yield `Local(Ok(()))`. On `Local(Err)` the caller must NOT write
        /// the value to the config file.
        pub(crate) fn apply_setting_dispatch(
            key: &str,
            value: &str,
            app: &mut crate::tui::app::App,
        ) -> SettingApply {
            match key {
                $(
                    $key => {
                        let handler: fn(&mut crate::tui::app::App, &str) -> SettingApply = $apply;
                        handler(app, value)
                    }
                )*
                _ => SettingApply::Local(Ok(()))
            }
        }
    };
}

define_settings! {
    "model", "Model", Model, EditorKind::ModelPicker,
        "Which Claude model to use.",
        |_app, value| SettingApply::Session(SessionSetting::Model { model: value.to_string() });

    "thinking", "Thinking", Model,
        EditorKind::DynamicCycler,
        "Thinking depth — controls effort on adaptive models, budget on legacy.",
        |app, value| {
            use agent_core::reasoning::ReasoningLevel;
            if let Some(level) = ReasoningLevel::parse(value) {
                // Validate against capability cache/static before sending
                // (client-local pre-check; the actor re-validates). On Err:
                // return the error so apply_setting can skip config write.
                if let Err(e) = synaps_cli::runtime::openai::catalog::validation::validate_reasoning_mutation(
                    &app.session.model, level,
                ) {
                    return SettingApply::Local(Err(e));
                }
                SettingApply::Session(SessionSetting::ReasoningLevel { level })
            } else {
                // Unknown string — ignore silently (cycler only emits valid strings).
                SettingApply::Local(Ok(()))
            }
        };

    "reasoning_type", "Reasoning", Model,
        EditorKind::Display,
        "How the active model expresses reasoning depth (derived from exact model capabilities; read-only).",
        |_app, _value| { /* display-only: no editor emits Apply for this key */ SettingApply::Local(Ok(())) };

    "context_window", "Context window", Model,
        EditorKind::Cycler(&["200k", "1m", "auto"]),
        "Override context window limit (auto = model default).",
        |_app, value| {
            let window = match value {
                "200k" | "200K" => Some(200_000u64),
                "1m" | "1M" => Some(1_000_000u64),
                "auto" => None,
                _ => return SettingApply::Local(Ok(())),
            };
            // The bar denominator (`last_turn_context_window`) follows the
            // reply's view in apply_setting().
            SettingApply::Session(SessionSetting::ContextWindow { tokens: window })
        };

    "compaction_model", "Compaction model", Model,
        EditorKind::ModelPicker,
        "Model used for /compact (default: claude-sonnet-4-6).",
        |_app, value| {
            let model = if value.is_empty() || value == "auto" || value == "default" {
                None
            } else {
                Some(value.to_string())
            };
            SettingApply::Session(SessionSetting::CompactionModel { model })
        };

    "api_retries", "API retries", Agent, EditorKind::Text { numeric: true },
        "Retries on transient API errors.",
        |_app, value| match value.parse::<u32>() {
            Ok(n) => SettingApply::Session(SessionSetting::ApiRetries { n }),
            Err(_) => SettingApply::Local(Ok(())),
        };

    "subagent_timeout", "Subagent timeout", Agent, EditorKind::Text { numeric: true },
        "Seconds before a dispatched subagent is canceled.",
        |_app, value| match value.parse::<u64>() {
            Ok(secs) => SettingApply::Session(SessionSetting::SubagentTimeout { secs }),
            Err(_) => SettingApply::Local(Ok(())),
        };

    "subagent_cold_resume_compact_tokens", "Cold resume compact (tokens)", Agent,
        EditorKind::Text { numeric: true },
        "subagent_resume of a worker whose prompt cache expired replays a compact context instead of the full transcript once its history reaches this many tokens (default 100000; 0 = always replay in full). Applies from the next resume.",
        |_app, value| whole_number("subagent_cold_resume_compact_tokens", value);

    "max_tool_output", "Max tool output", ToolLimits, EditorKind::Text { numeric: true },
        "Bytes to capture from a tool before truncating.",
        |_app, value| match value.parse::<usize>() {
            Ok(bytes) => SettingApply::Session(SessionSetting::MaxToolOutput { bytes }),
            Err(_) => SettingApply::Local(Ok(())),
        };

    "bash_timeout", "Bash timeout", ToolLimits, EditorKind::Text { numeric: true },
        "Default seconds allowed for a bash command.",
        |_app, value| match value.parse::<u64>() {
            Ok(secs) => SettingApply::Session(SessionSetting::BashTimeout { secs }),
            Err(_) => SettingApply::Local(Ok(())),
        };

    "bash_max_timeout", "Bash max timeout", ToolLimits, EditorKind::Text { numeric: true },
        "Legacy setting retained for config compatibility; requested bash timeouts are no longer clamped.",
        |_app, value| match value.parse::<u64>() {
            Ok(secs) => SettingApply::Session(SessionSetting::BashMaxTimeout { secs }),
            Err(_) => SettingApply::Local(Ok(())),
        };

    "theme", "Theme", Appearance, EditorKind::ThemePicker,
        "Color theme (restart required).",
        |_app, _value| { /* handled after write_config_value in apply_setting() */ SettingApply::Local(Ok(())) };

    "tui_background_opaque", "Background", Appearance, EditorKind::Cycler(&["opaque", "invisible"]),
        "Opaque paints Synaps' theme background; invisible uses your terminal background.",
        |_app, value| {
            match value {
                "opaque" => super::super::theme::set_background_opaque(true),
                "invisible" => super::super::theme::set_background_opaque(false),
                _ => return SettingApply::Local(Err("expected opaque or invisible".to_string())),
            }
            SettingApply::Local(Ok(()))
        };

    "tui_streaming_glow", "Streaming glow", Appearance, EditorKind::Cycler(&["on", "off"]),
        "A soft glow in the context bar's colour sweeps across the prompt while a reply streams.",
        |_app, value| {
            match value {
                "on" => super::super::neon_prompt::set_streaming_glow(true),
                "off" => super::super::neon_prompt::set_streaming_glow(false),
                _ => return SettingApply::Local(Err("expected on or off".to_string())),
            }
            SettingApply::Local(Ok(()))
        };

    "theme_transition", "Theme transition", Appearance, EditorKind::Cycler(&["on", "off"]),
        "Animated cross-fade on theme changes (on = 350ms). Off = instant snap. Integer ms (0-2000) accepted in the config file.",
        |_app, value| {
            match synaps_cli::config::ThemeTransitionMode::parse(value) {
                Some(mode) => {
                    super::super::theme::transition::set_transition_mode(mode);
                    SettingApply::Local(Ok(()))
                }
                None => SettingApply::Local(Err("expected on, off, or milliseconds (0-2000)".to_string())),
            }
        };

    "sidecar_toggle_key", "Sidecar toggle key", Sidecar,
        EditorKind::Cycler(&["F8", "F2", "F12", "C-V", "C-G"]),
        "Keybind that toggles the active sidecar plugin. Takes effect immediately.",
        |app, value| {
            if let Some(kb) = app.keybinds.as_ref() {
                match kb.write() {
                    Ok(mut g) => {
                        if let Err(e) = g.set_slash_command_key("sidecar toggle", value) {
                            tracing::warn!("sidecar_toggle_key apply failed: {}", e);
                        }
                    }
                    Err(_) => tracing::warn!("sidecar_toggle_key apply: registry poisoned"),
                }
            }
            SettingApply::Local(Ok(()))
        };

    "startup.quick_start", "Quick start", Startup, EditorKind::Cycler(&["on", "off"]),
        "Start without waiting for extensions to load (faster startup). The first message still waits for extensions to start before it is sent (up to the extensions-ready wait, 30 s by default, plus the extensions' session-start hooks). Takes effect next launch.",
        |_app, value| match value {
            "on" | "off" => SettingApply::Local(Ok(())),
            _ => SettingApply::Local(Err("expected on or off".to_string())),
        };

    "startup.extensions_ready_timeout_secs", "Extensions-ready wait", Startup,
        EditorKind::Text { numeric: true },
        "Max seconds to wait for extension discovery: at startup when Quick Start is off, before the first message is sent when it is on. Takes effect next launch.",
        |_app, value| match value.parse::<u64>() {
            Ok(_) => SettingApply::Local(Ok(())),
            Err(_) => SettingApply::Local(Err("expected a number of seconds".to_string())),
        };

    "daemon.idle_exit_secs", "Daemon idle timeout", Daemon,
        EditorKind::Text { numeric: true },
        "Seconds the daemon stays alive after the last client disconnects (higher = stays warm, fewer cold starts). 0 = never idle-exit. Env SYNAPS_DAEMON_IDLE_EXIT_SECS overrides.",
        |_app, value| match value.parse::<u64>() {
            Ok(_) => SettingApply::Local(Ok(())),
            Err(_) => SettingApply::Local(Err("expected a number of seconds".to_string())),
        };

    "daemon.prompt_abandon_secs", "Prompt-abandon timeout", Daemon,
        EditorKind::Text { numeric: true },
        "Seconds a pending confirmation on a detached session survives before it is auto-denied. 0 = disabled (survives forever). Env SYNAPS_DAEMON_PROMPT_ABANDON_SECS overrides.",
        |_app, value| match value.parse::<u64>() {
            Ok(_) => SettingApply::Local(Ok(())),
            Err(_) => SettingApply::Local(Err("expected a number of seconds".to_string())),
        };

    "daemon.parked_evict_secs", "Parked eviction", Daemon,
        EditorKind::Text { numeric: true },
        "Seconds a parked session row lingers in the daemon before eviction (its state stays on disk; --continue rebuilds it). 0 = keep forever. Env SYNAPS_DAEMON_PARKED_EVICT_SECS overrides.",
        |_app, value| match value.parse::<u64>() {
            Ok(_) => SettingApply::Local(Ok(())),
            Err(_) => SettingApply::Local(Err("expected a number of seconds".to_string())),
        };

    "client_input", "Client input", Daemon, EditorKind::Cycler(&["shared", "exclusive"]),
        "Who may type in a daemon session with several clients attached (tabs, a dashboard, a bridge): shared (default) lets every client except --observe ones send input; exclusive lets only the input owner, and the others /takeover. The daemon reads it when it starts: `synaps daemon reload` applies it.",
        |_app, value| config_edit("client_input", value);

    // ── Context & cache ──────────────────────────────────────────────────
    // Config-only rows below validate through the real config parser
    // (`config_edit`), so /settings can never write a value `load_config`
    // rejects; `Local(Err)` skips the write.

    "cache_ttl", "Cache TTL", Context, EditorKind::Cycler(&["5m", "1h", "hybrid"]),
        "Prompt-cache lifetime for Anthropic requests: 5m (default), 1h on every marker, or hybrid (1h on the stable prefix, 5m on the message tail). Takes effect next launch.",
        |_app, value| config_edit("cache_ttl", value);

    "cache_diagnostics", "Cache diagnostics", Context, EditorKind::Cycler(&["on", "off"]),
        "Opt into the cache-diagnosis beta (reports why a prompt-cache read missed). Takes effect next launch.",
        |_app, value| on_off("cache_diagnostics", value);

    "cache_keepwarm", "Cache keep-warm", Context,
        EditorKind::Cycler(&["off", "auto", "1h", "2h", "4h", "8h", "12h", "24h", "48h"]),
        "Keep the prompt cache warm while idle: off, auto (stops once pings would cost more than the rewrite they prevent), or stop after <N>h (1–48). ←/→ cycles presets; Enter types any <N>h. New sessions after next launch; /keepwarm changes the current session.",
        |_app, value| config_edit("cache_keepwarm", value);

    "cache_keepwarm_auto_max_hours", "Keep-warm auto max (h)", Context,
        EditorKind::Text { numeric: true },
        "Longest an auto keep-warm idle period may run, in hours (1–48, default 24). Takes effect next launch.",
        |_app, value| whole_number("cache_keepwarm_auto_max_hours", value);

    "context_management.mode", "Context management", Context,
        EditorKind::Cycler(&["off", "auto"]),
        "Automatic context management: auto plans a wrap-up and hands over to a fresh context before the window fills; off disables it (default). Takes effect next launch.",
        |_app, value| config_edit("context_management.mode", value);

    "context_management.pressure_tokens", "Pressure threshold", Context,
        EditorKind::Text { numeric: false },
        "Token count where context pressure starts (auto = window-aware default: 140k on 200k, 250k on 1m). Must stay below the rollover threshold. Takes effect next launch.",
        |_app, value| config_edit("context_management.pressure_tokens", value);

    "context_management.rollover_tokens", "Rollover threshold", Context,
        EditorKind::Text { numeric: false },
        "Token count where a context rollover becomes due (auto = window-aware default: 180k on 200k, 400k on 1m). Takes effect next launch.",
        |_app, value| config_edit("context_management.rollover_tokens", value);

    "context_management.target_tokens", "Rollover target", Context,
        EditorKind::Text { numeric: false },
        "Context size a rollover aims for, as reported by the provider when known (auto = min(100k, 2/3 of pressure)). Must stay below the pressure threshold. Takes effect next launch.",
        |_app, value| config_edit("context_management.target_tokens", value);

    "context_management.reserve_tokens", "Reserve tokens", Context,
        EditorKind::Text { numeric: true },
        "Minimum free tokens kept before any model round (1–1000000, default 16000). Takes effect next launch.",
        |_app, value| whole_number("context_management.reserve_tokens", value);

    "context_management.finish_rounds", "Finish rounds", Context,
        EditorKind::Text { numeric: true },
        "Extra plan/wrap-up rounds allowed after a rollover becomes due (0–8, default 2; 0 = none). Takes effect next launch.",
        |_app, value| whole_number("context_management.finish_rounds", value);

    "compaction_mode", "Compaction mode", Context, EditorKind::Cycler(&["remote", "local"]),
        "Where /compact summarises: remote (the configured provider, default) or local (no network request is built). Takes effect next launch.",
        |_app, value| config_edit("compaction_mode", value);

    "compaction_exclude", "Compaction exclude", Context,
        EditorKind::Text { numeric: false },
        "Content kept out of remote compaction, comma-separated: user_text, assistant_text, thinking, tool_calls, tool_results, file_paths, event_data. Empty = none. Takes effect next launch.",
        |_app, value| config_edit("compaction_exclude", value);

    "refusal_retries", "Refusal retries", Context, EditorKind::Text { numeric: true },
        "Retries when the model stops with a refusal (default 2). Takes effect next launch.",
        |_app, value| match value.trim().parse::<u32>() {
            Ok(_) => config_edit("refusal_retries", value),
            Err(_) => SettingApply::Local(Err("expected a whole number".to_string())),
        };

    "telemetry", "Telemetry", Context, EditorKind::Cycler(&["off", "basic", "full"]),
        "Per-request API log in ~/.cache/synaps/api-log.jsonl (stays on this machine): off (default), basic (timing, usage, cost) or full (adds rate-limit headers and cache diagnostics). Takes effect next launch.",
        |_app, value| match value {
            "off" | "basic" | "full" => config_edit("telemetry", value),
            _ => SettingApply::Local(Err("expected off, basic or full".to_string())),
        };

    // ── Agent ────────────────────────────────────────────────────────────

    "subagent.max_concurrent", "Subagents at once", Agent, EditorKind::Text { numeric: true },
        "Workers running at the same time across the whole delegation tree (at least 1, default 16; never above the total). Takes effect next launch.",
        |_app, value| whole_number("subagent.max_concurrent", value);

    "subagent.max_total", "Subagents in total", Agent, EditorKind::Text { numeric: true },
        "Workers dispatched and not yet reconciled across the delegation tree (at least 1, default 64). Takes effect next launch.",
        |_app, value| whole_number("subagent.max_total", value);

    "subagent.max_depth", "Subagent depth", Agent, EditorKind::Text { numeric: true },
        "How deep workers may nest: 1 = only the foreground delegates (1–65535, default 4). Takes effect next launch.",
        |_app, value| whole_number("subagent.max_depth", value);

    "session_persistence", "Session persistence", Agent, EditorKind::Cycler(&["json", "journal"]),
        "How sessions are saved: json (full rewrite, default) or journal (append-only deltas with periodic snapshots). Applies from the next session save.",
        |_app, value| config_edit("session_persistence", value);

    "agent_name", "Agent name", Agent, EditorKind::Text { numeric: false },
        "Name the agent goes by. Takes effect next launch.",
        |_app, value| non_empty_text("agent_name", value);

    "identity", "Identity", Agent, EditorKind::Text { numeric: false },
        "Identity preamble at the top of the system prompt (default: the SynapsCLI identity). Remove the line from the config to restore the default. Takes effect next launch.",
        |_app, value| non_empty_text("identity", value);

    "events.auto_turn", "Auto turns", Agent, EditorKind::Cycler(&["on", "off"]),
        "Start a model turn automatically when runtime events arrive while idle (server, RPC and daemon sessions). Takes effect next launch.",
        |_app, value| on_off("events.auto_turn", value);

    "events.auto_turn_cap", "Auto-turn cap", Agent, EditorKind::Text { numeric: true },
        "Consecutive automatic turns before the session waits for real input (default 5; 0 = unlimited). Takes effect next launch.",
        |_app, value| whole_number("events.auto_turn_cap", value);

    // ── Turn budgets ─────────────────────────────────────────────────────

    "turn_budget.foreground.max_cost_usd", "Foreground cost (USD)", Budgets,
        EditorKind::Text { numeric: false },
        "US dollars one turn may spend before it stops (decimal, e.g. 2.50) — turns you type or queue, plugin commands and session-driver turns, in the TUI, daemon sessions, synaps chat and rpc (event-started turns also apply the event budget). Unset = the role's built-in default (remove the line from the config to restore it). Takes effect next launch.",
        |_app, value| budget_usd("turn_budget.foreground.max_cost_usd", value);

    "turn_budget.foreground.max_elapsed_secs", "Foreground time (s)", Budgets,
        EditorKind::Text { numeric: true },
        "Wall-clock seconds one turn may run; 0 = no limit (the default) — turns you type or queue, plugin commands and session-driver turns, in the TUI, daemon sessions, synaps chat and rpc (event-started turns also apply the event budget). Unset = the role's built-in default (remove the line from the config to restore it). Takes effect next launch.",
        |_app, value| whole_number("turn_budget.foreground.max_elapsed_secs", value);

    "turn_budget.foreground.max_tool_calls", "Foreground tool calls", Budgets,
        EditorKind::Text { numeric: false },
        "Tool calls allowed in one turn; a positive number, or unlimited (default: no limit) — turns you type or queue, plugin commands and session-driver turns, in the TUI, daemon sessions, synaps chat and rpc (event-started turns also apply the event budget). Unset = the role's built-in default (remove the line from the config to restore it). Takes effect next launch.",
        |_app, value| turn_limit("turn_budget.foreground.max_tool_calls", value);

    "turn_budget.foreground.max_provider_rounds", "Foreground rounds", Budgets,
        EditorKind::Text { numeric: false },
        "Model round-trips allowed in one turn before the allowance runs out; a positive number, or unlimited (default: no limit) — turns you type or queue, plugin commands and session-driver turns, in the TUI, daemon sessions, synaps chat and rpc (event-started turns also apply the event budget). Unset = the role's built-in default (remove the line from the config to restore it). Takes effect next launch.",
        |_app, value| turn_limit("turn_budget.foreground.max_provider_rounds", value);

    "turn_budget.foreground.max_round_renewals", "Foreground renewals", Budgets,
        EditorKind::Text { numeric: true },
        "Automatic renewals of the round allowance after it runs out mid-turn (only with a round limit set); 0 = hard stop at the first exhaustion — turns you type or queue, plugin commands and session-driver turns, in the TUI, daemon sessions, synaps chat and rpc (event-started turns also apply the event budget). Unset = the role's built-in default (remove the line from the config to restore it). Takes effect next launch.",
        |_app, value| whole_number("turn_budget.foreground.max_round_renewals", value);

    "turn_budget.foreground.max_context_tokens", "Foreground ctx tokens", Budgets,
        EditorKind::Text { numeric: true },
        "Context tokens one turn may use — turns you type or queue, plugin commands and session-driver turns, in the TUI, daemon sessions, synaps chat and rpc (event-started turns also apply the event budget). Unset = the role's built-in default (remove the line from the config to restore it). Takes effect next launch.",
        |_app, value| whole_number("turn_budget.foreground.max_context_tokens", value);

    "turn_budget.foreground.max_accumulated_tool_result_bytes", "Foreground tool bytes", Budgets,
        EditorKind::Text { numeric: false },
        "Total bytes of tool results one turn may accumulate; a positive number, or unlimited (default: no limit) — turns you type or queue, plugin commands and session-driver turns, in the TUI, daemon sessions, synaps chat and rpc (event-started turns also apply the event budget). Unset = the role's built-in default (remove the line from the config to restore it). Takes effect next launch.",
        |_app, value| turn_limit("turn_budget.foreground.max_accumulated_tool_result_bytes", value);

    "turn_budget.autonomous.max_cost_usd", "Autonomous cost (USD)", Budgets,
        EditorKind::Text { numeric: false },
        "US dollars one turn may spend before it stops (decimal, e.g. 2.50) — watcher agent turns (synaps agent) only. Unset = the role's built-in default (remove the line from the config to restore it). Takes effect next launch.",
        |_app, value| budget_usd("turn_budget.autonomous.max_cost_usd", value);

    "turn_budget.autonomous.max_elapsed_secs", "Autonomous time (s)", Budgets,
        EditorKind::Text { numeric: true },
        "Wall-clock seconds one turn may run; 0 = no limit (the default) — watcher agent turns (synaps agent) only. Unset = the role's built-in default (remove the line from the config to restore it). Takes effect next launch.",
        |_app, value| whole_number("turn_budget.autonomous.max_elapsed_secs", value);

    "turn_budget.autonomous.max_tool_calls", "Autonomous tool calls", Budgets,
        EditorKind::Text { numeric: false },
        "Tool calls allowed in one turn; a positive number, or unlimited (default: 96) — watcher agent turns (synaps agent) only. Unset = the role's built-in default (remove the line from the config to restore it). Takes effect next launch.",
        |_app, value| turn_limit("turn_budget.autonomous.max_tool_calls", value);

    "turn_budget.autonomous.max_provider_rounds", "Autonomous rounds", Budgets,
        EditorKind::Text { numeric: false },
        "Model round-trips allowed in one turn before the allowance runs out; a positive number, or unlimited (default: 24) — watcher agent turns (synaps agent) only. Unset = the role's built-in default (remove the line from the config to restore it). Takes effect next launch.",
        |_app, value| turn_limit("turn_budget.autonomous.max_provider_rounds", value);

    "turn_budget.autonomous.max_round_renewals", "Autonomous renewals", Budgets,
        EditorKind::Text { numeric: true },
        "Automatic renewals of the round allowance after it runs out mid-turn (only with a round limit set); 0 = hard stop at the first exhaustion — watcher agent turns (synaps agent) only. Unset = the role's built-in default (remove the line from the config to restore it). Takes effect next launch.",
        |_app, value| whole_number("turn_budget.autonomous.max_round_renewals", value);

    "turn_budget.autonomous.max_context_tokens", "Autonomous ctx tokens", Budgets,
        EditorKind::Text { numeric: true },
        "Context tokens one turn may use — watcher agent turns (synaps agent) only. Unset = the role's built-in default (remove the line from the config to restore it). Takes effect next launch.",
        |_app, value| whole_number("turn_budget.autonomous.max_context_tokens", value);

    "turn_budget.autonomous.max_accumulated_tool_result_bytes", "Autonomous tool bytes", Budgets,
        EditorKind::Text { numeric: false },
        "Total bytes of tool results one turn may accumulate; a positive number, or unlimited (default: 8 MiB) — watcher agent turns (synaps agent) only. Unset = the role's built-in default (remove the line from the config to restore it). Takes effect next launch.",
        |_app, value| turn_limit("turn_budget.autonomous.max_accumulated_tool_result_bytes", value);

    "turn_budget.event.max_cost_usd", "Event cost (USD)", Budgets,
        EditorKind::Text { numeric: false },
        "US dollars one turn may spend before it stops (decimal, e.g. 2.50) — turns a session starts by itself when events arrive (inbox messages, subagent completions); combined with the session budget, the stricter limit wins. Unset = the role's built-in default (remove the line from the config to restore it). Takes effect next launch.",
        |_app, value| budget_usd("turn_budget.event.max_cost_usd", value);

    "turn_budget.event.max_elapsed_secs", "Event time (s)", Budgets,
        EditorKind::Text { numeric: true },
        "Wall-clock seconds one turn may run; 0 = no limit (the default) — turns a session starts by itself when events arrive (inbox messages, subagent completions); combined with the session budget, the stricter limit wins. Unset = the role's built-in default (remove the line from the config to restore it). Takes effect next launch.",
        |_app, value| whole_number("turn_budget.event.max_elapsed_secs", value);

    "turn_budget.event.max_tool_calls", "Event tool calls", Budgets,
        EditorKind::Text { numeric: false },
        "Tool calls allowed in one turn; a positive number, or unlimited (default: 96) — turns a session starts by itself when events arrive (inbox messages, subagent completions); combined with the session budget, the stricter limit wins. Unset = the role's built-in default (remove the line from the config to restore it). Takes effect next launch.",
        |_app, value| turn_limit("turn_budget.event.max_tool_calls", value);

    "turn_budget.event.max_provider_rounds", "Event rounds", Budgets,
        EditorKind::Text { numeric: false },
        "Model round-trips allowed in one turn before the allowance runs out; a positive number, or unlimited (default: 24) — turns a session starts by itself when events arrive (inbox messages, subagent completions); combined with the session budget, the stricter limit wins. Unset = the role's built-in default (remove the line from the config to restore it). Takes effect next launch.",
        |_app, value| turn_limit("turn_budget.event.max_provider_rounds", value);

    "turn_budget.event.max_round_renewals", "Event renewals", Budgets,
        EditorKind::Text { numeric: true },
        "Automatic renewals of the round allowance after it runs out mid-turn (only with a round limit set); 0 = hard stop at the first exhaustion — turns a session starts by itself when events arrive (inbox messages, subagent completions); combined with the session budget, the stricter limit wins. Unset = the role's built-in default (remove the line from the config to restore it). Takes effect next launch.",
        |_app, value| whole_number("turn_budget.event.max_round_renewals", value);

    "turn_budget.event.max_context_tokens", "Event ctx tokens", Budgets,
        EditorKind::Text { numeric: true },
        "Context tokens one turn may use — turns a session starts by itself when events arrive (inbox messages, subagent completions); combined with the session budget, the stricter limit wins. Unset = the role's built-in default (remove the line from the config to restore it). Takes effect next launch.",
        |_app, value| whole_number("turn_budget.event.max_context_tokens", value);

    "turn_budget.event.max_accumulated_tool_result_bytes", "Event tool bytes", Budgets,
        EditorKind::Text { numeric: false },
        "Total bytes of tool results one turn may accumulate; a positive number, or unlimited (default: 8 MiB) — turns a session starts by itself when events arrive (inbox messages, subagent completions); combined with the session budget, the stricter limit wins. Unset = the role's built-in default (remove the line from the config to restore it). Takes effect next launch.",
        |_app, value| turn_limit("turn_budget.event.max_accumulated_tool_result_bytes", value);

    "turn_budget.worker.max_cost_usd", "Worker cost (USD)", Budgets,
        EditorKind::Text { numeric: false },
        "US dollars one turn may spend before it stops (decimal, e.g. 2.50) — subagent workers; each start or resume gets a fresh allowance. Unset = the role's built-in default (remove the line from the config to restore it). Takes effect next launch.",
        |_app, value| budget_usd("turn_budget.worker.max_cost_usd", value);

    "turn_budget.worker.max_elapsed_secs", "Worker time (s)", Budgets,
        EditorKind::Text { numeric: true },
        "Wall-clock seconds one turn may run; 0 = no limit (the default) — subagent workers; each start or resume gets a fresh allowance. Unset = the role's built-in default (remove the line from the config to restore it). Takes effect next launch.",
        |_app, value| whole_number("turn_budget.worker.max_elapsed_secs", value);

    "turn_budget.worker.max_tool_calls", "Worker tool calls", Budgets,
        EditorKind::Text { numeric: false },
        "Tool calls allowed in one turn; a positive number, or unlimited (default: no limit) — subagent workers; each start or resume gets a fresh allowance. Unset = the role's built-in default (remove the line from the config to restore it). Takes effect next launch.",
        |_app, value| turn_limit("turn_budget.worker.max_tool_calls", value);

    "turn_budget.worker.max_provider_rounds", "Worker rounds", Budgets,
        EditorKind::Text { numeric: false },
        "Model round-trips allowed in one turn before the allowance runs out; a positive number, or unlimited (default: 64 per allowance) — subagent workers; each start or resume gets a fresh allowance. Unset = the role's built-in default (remove the line from the config to restore it). Takes effect next launch.",
        |_app, value| turn_limit("turn_budget.worker.max_provider_rounds", value);

    "turn_budget.worker.max_round_renewals", "Worker renewals", Budgets,
        EditorKind::Text { numeric: true },
        "Automatic renewals of the round allowance after it runs out mid-turn (only with a round limit set); 0 = hard stop at the first exhaustion — subagent workers; each start or resume gets a fresh allowance. Unset = the role's built-in default (remove the line from the config to restore it). Takes effect next launch.",
        |_app, value| whole_number("turn_budget.worker.max_round_renewals", value);

    "turn_budget.worker.max_context_tokens", "Worker ctx tokens", Budgets,
        EditorKind::Text { numeric: true },
        "Context tokens one turn may use — subagent workers; each start or resume gets a fresh allowance. Unset = the role's built-in default (remove the line from the config to restore it). Takes effect next launch.",
        |_app, value| whole_number("turn_budget.worker.max_context_tokens", value);

    "turn_budget.worker.max_accumulated_tool_result_bytes", "Worker tool bytes", Budgets,
        EditorKind::Text { numeric: false },
        "Total bytes of tool results one turn may accumulate; a positive number, or unlimited (default: no limit) — subagent workers; each start or resume gets a fresh allowance. Unset = the role's built-in default (remove the line from the config to restore it). Takes effect next launch.",
        |_app, value| turn_limit("turn_budget.worker.max_accumulated_tool_result_bytes", value);
    // ── Tool limits ──────────────────────────────────────────────────────

    "progressive_tool_disclosure", "Progressive tools", ToolLimits,
        EditorKind::Cycler(&["on", "off"]),
        "Start each session with a small core tool set and activate the rest on demand (off = send every tool schema, default). Takes effect next launch.",
        |_app, value| on_off("progressive_tool_disclosure", value);

    "tools.deferred_loading", "Deferred tool loading", ToolLimits,
        EditorKind::Cycler(&["on", "off"]),
        "With progressive tools on the native Anthropic transport: send activatable tools up front as deferred so an activation never changes the cached prompt (default on). Takes effect next launch.",
        |_app, value| on_off("tools.deferred_loading", value);

    "tools.activation_confirm", "Tool activation", ToolLimits,
        EditorKind::Cycler(&["auto", "prompt", "deny"]),
        "When the model asks to activate tools: auto allows (default), prompt asks you, deny refuses. Takes effect next launch.",
        |_app, value| config_edit("tools.activation_confirm", value);

    "process_scopes", "Process scopes", ToolLimits, EditorKind::Cycler(&["auto", "off"]),
        "Run the daemon, bash commands and shells in their own systemd scopes so the OOM killer stops one runaway command, not the terminal (Linux; default auto). Applies to commands started afterwards; the daemon's own scope changes next launch. Env SYNAPS_PROCESS_SCOPES overrides.",
        |_app, value| config_edit("process_scopes", value);

    "disabled_tools", "Disabled tools", ToolLimits, EditorKind::Text { numeric: false },
        "Built-in tools never offered to the model, comma-separated runtime names (e.g. bash, ls). Empty = none. Takes effect next launch.",
        |_app, value| config_edit("disabled_tools", value);

    "disabled_skills", "Disabled skills", ToolLimits, EditorKind::Text { numeric: false },
        "Skills never loaded, comma-separated names. Empty = none. Takes effect next launch.",
        |_app, value| config_edit("disabled_skills", value);

    // ── Appearance ───────────────────────────────────────────────────────

    "max_fps", "Max FPS", Appearance, EditorKind::Text { numeric: true },
        "Redraw cap while streaming, in frames per second (1–1000, default 60). Input always redraws immediately. Takes effect next launch.",
        |_app, value| whole_number("max_fps", value);

    "scroll_lines", "Scroll lines", Appearance, EditorKind::Text { numeric: true },
        "Lines scrolled per mouse-wheel event (1–20, default 3). Takes effect next launch.",
        |_app, value| whole_number("scroll_lines", value);

    // ── Memory ───────────────────────────────────────────────────────────

    "memory.default_mode", "Default memory mode", Memory,
        EditorKind::Cycler(&["off", "recall_once", "recall_each_prompt", "capture_only", "capture_and_recall"]),
        "Memory mode every new session starts in (default off). A non-off mode needs memory.default_mode_confirmed = true in the config, added by hand; until then this setting is refused. /memory off still turns it off for that session. Takes effect next launch.",
        |_app, value| config_edit("memory.default_mode", value);

    "memory.backend", "Memory backend", Memory, EditorKind::Cycler(&["legacy", "axel"]),
        "Note storage: legacy (default) or axel (needs the Axel executable below). Selecting a backend never enables capture or recall. Restart required.",
        |_app, value| config_edit("memory.backend", value);

    "memory.user_scope", "User-wide notes", Memory, EditorKind::Cycler(&["true", "false"]),
        "Allow user-wide notes in addition to project notes (default false). Never grants capture or history consent. Restart required.",
        |_app, value| config_edit("memory.user_scope", value);

    "memory.axel.executable", "Axel executable", Memory, EditorKind::Text { numeric: false },
        "Absolute path to the Axel memory service executable (never looked up on PATH). Restart required.",
        |_app, value| config_edit("memory.axel.executable", value);

    "memory.axel.brain", "Axel brain", Memory, EditorKind::Text { numeric: false },
        "Absolute path to a shared Axel brain file (unset = the private per-user default). Restart required.",
        |_app, value| config_edit("memory.axel.brain", value);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_transition_setting_is_appearance_cycler() {
        let def = ALL_SETTINGS
            .iter()
            .find(|d| d.key == "theme_transition")
            .expect("theme_transition setting should be defined");
        assert_eq!(def.category, Category::Appearance);
        match def.editor {
            EditorKind::Cycler(opts) => assert_eq!(opts, &["on", "off"]),
            _ => panic!("theme_transition editor should be a Cycler"),
        }
    }

    #[test]
    fn streaming_glow_setting_is_an_appearance_cycler() {
        let def = ALL_SETTINGS
            .iter()
            .find(|d| d.key == "tui_streaming_glow")
            .expect("tui_streaming_glow setting should be defined");
        assert_eq!(def.category, Category::Appearance);
        match def.editor {
            EditorKind::Cycler(opts) => assert_eq!(opts, &["on", "off"]),
            _ => panic!("tui_streaming_glow editor should be a Cycler"),
        }
    }

    #[test]
    fn sidecar_toggle_key_setting_is_in_sidecar_category() {
        let def = ALL_SETTINGS
            .iter()
            .find(|d| d.key == "sidecar_toggle_key")
            .expect("sidecar_toggle_key setting should be defined");
        assert_eq!(def.category, Category::Sidecar);
    }

    #[test]
    fn quick_start_setting_is_in_startup_category() {
        let def = ALL_SETTINGS
            .iter()
            .find(|d| d.key == "startup.quick_start")
            .expect("startup.quick_start setting should be defined");
        assert_eq!(def.category, Category::Startup);
        match def.editor {
            EditorKind::Cycler(opts) => assert_eq!(opts, &["on", "off"]),
            _ => panic!("startup.quick_start editor should be a Cycler"),
        }
    }

    #[test]
    fn daemon_settings_are_in_daemon_category() {
        for key in [
            "daemon.idle_exit_secs",
            "daemon.prompt_abandon_secs",
            "daemon.parked_evict_secs",
        ] {
            let def = ALL_SETTINGS
                .iter()
                .find(|d| d.key == key)
                .unwrap_or_else(|| panic!("{key} setting should be defined"));
            assert_eq!(def.category, Category::Daemon, "{key} should be in Daemon");
            assert!(
                matches!(def.editor, EditorKind::Text { numeric: true }),
                "{key} should be a numeric Text editor"
            );
        }
    }

    #[test]
    fn extensions_ready_timeout_is_in_startup_category() {
        let def = ALL_SETTINGS
            .iter()
            .find(|d| d.key == "startup.extensions_ready_timeout_secs")
            .expect("startup.extensions_ready_timeout_secs should be defined");
        assert_eq!(def.category, Category::Startup);
        assert!(matches!(def.editor, EditorKind::Text { numeric: true }));
    }

    #[test]
    fn sidecar_toggle_key_static_setting_still_defined_for_backward_compat() {
        // Phase 8 slice 8A.4: even after `visible_categories(claims)`
        // hides the global Sidecar page when a plugin claims its own
        // settings_category, the static def stays in ALL_SETTINGS so
        // legacy users without claimed plugins keep a working toggle
        // and config round-trips remain stable.
        let def = ALL_SETTINGS
            .iter()
            .find(|d| d.key == "sidecar_toggle_key")
            .expect("sidecar_toggle_key setting must remain in ALL_SETTINGS for back-compat");
        assert_eq!(def.category, Category::Sidecar);
        match def.editor {
            EditorKind::Cycler(opts) => {
                assert!(opts.contains(&"F8"));
                assert!(opts.contains(&"C-V"));
            }
            _ => panic!("expected Cycler editor for sidecar_toggle_key"),
        }
    }

    fn app_with_model(model: &str) -> crate::tui::app::App {
        crate::tui::app::App::new(synaps_cli::Session::new(model, "medium", None))
    }

    // Rejected thinking never becomes a `Set` (the config write is skipped).
    #[test]
    fn thinking_dispatch_rejects_unsupported_level_returns_err_no_set() {
        let mut app = app_with_model("openai-codex/gpt-5.6-luna"); // luna: no ultra
        match apply_setting_dispatch("thinking", "ultra", &mut app) {
            SettingApply::Local(Err(_)) => {}
            _ => panic!("ultra must be rejected for luna before any Set"),
        }
    }

    #[test]
    fn thinking_dispatch_accepts_valid_level_yields_set() {
        let mut app = app_with_model("openai-codex/gpt-5.6-sol"); // sol: supports ultra
        match apply_setting_dispatch("thinking", "ultra", &mut app) {
            SettingApply::Session(SessionSetting::ReasoningLevel { level }) => {
                assert_eq!(level, agent_core::reasoning::ReasoningLevel::Ultra)
            }
            _ => panic!("ultra must be accepted for sol"),
        }
    }

    #[test]
    fn thinking_dispatch_rejects_off_on_xai_45_no_set() {
        let mut app = app_with_model("xai-auth/grok-4.5");
        // Off must be rejected (reasoning cannot be disabled), never omitted.
        assert!(matches!(
            apply_setting_dispatch("thinking", "off", &mut app),
            SettingApply::Local(Err(_))
        ));
        // xhigh is not documented for grok-4.5 either.
        assert!(matches!(
            apply_setting_dispatch("thinking", "xhigh", &mut app),
            SettingApply::Local(Err(_))
        ));
        // A documented effort is accepted.
        assert!(matches!(
            apply_setting_dispatch("thinking", "low", &mut app),
            SettingApply::Session(SessionSetting::ReasoningLevel {
                level: agent_core::reasoning::ReasoningLevel::Low
            })
        ));
    }

    #[test]
    fn runtime_keys_yield_the_expected_session_setting() {
        let mut app = app_with_model("m");
        assert!(matches!(
            apply_setting_dispatch("model", "claude-opus-4-7", &mut app),
            SettingApply::Session(SessionSetting::Model { model }) if model == "claude-opus-4-7"
        ));
        assert!(matches!(
            apply_setting_dispatch("api_retries", "5", &mut app),
            SettingApply::Session(SessionSetting::ApiRetries { n: 5 })
        ));
        assert!(matches!(
            apply_setting_dispatch("bash_timeout", "30", &mut app),
            SettingApply::Session(SessionSetting::BashTimeout { secs: 30 })
        ));
        assert!(matches!(
            apply_setting_dispatch("context_window", "1m", &mut app),
            SettingApply::Session(SessionSetting::ContextWindow { tokens: Some(1_000_000) })
        ));
        assert!(matches!(
            apply_setting_dispatch("compaction_model", "auto", &mut app),
            SettingApply::Session(SessionSetting::CompactionModel { model: None })
        ));
        // Unparseable numerics and unknown keys are local no-ops.
        assert!(matches!(
            apply_setting_dispatch("api_retries", "x", &mut app),
            SettingApply::Local(Ok(()))
        ));
        assert!(matches!(
            apply_setting_dispatch("unknown_key_xyz", "val", &mut app),
            SettingApply::Local(Ok(()))
        ));
    }

    // ── Settings coverage (every config key editable or excluded) ─────────

    /// Points SYNAPS_BASE_DIR at a temp dir holding `config` for the test's
    /// lifetime; restores the previous value on drop. Hold
    /// `CONFIG_ENV_TEST_LOCK` while it lives.
    struct TempConfig {
        dir: tempfile::TempDir,
        old: Option<String>,
    }

    impl TempConfig {
        fn new(content: &str) -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            std::fs::write(dir.path().join("config"), content).expect("write config");
            let old = std::env::var("SYNAPS_BASE_DIR").ok();
            synaps_cli::config::set_base_dir_for_tests(dir.path().to_path_buf());
            Self { dir, old }
        }

        fn write(&self, content: &str) {
            std::fs::write(self.dir.path().join("config"), content).expect("write config");
        }
    }

    impl Drop for TempConfig {
        fn drop(&mut self) {
            match self.old.take() {
                Some(v) => std::env::set_var("SYNAPS_BASE_DIR", v),
                None => std::env::remove_var("SYNAPS_BASE_DIR"),
            }
        }
    }

    fn config_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::tui::CONFIG_ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Drift guard: every option a Cycler row can write is accepted by
    /// `load_config` with no warning (and no stderr-only family diagnostic).
    /// Catches cyclers like the old `context_window = auto`, which the parser
    /// ignored with a warning.
    #[test]
    fn every_cycler_option_is_accepted_by_load_config() {
        let _lock = config_lock();
        let temp = TempConfig::new("");
        let mut bad = Vec::new();
        for def in ALL_SETTINGS {
            let EditorKind::Cycler(options) = def.editor else {
                continue;
            };
            for option in options.iter() {
                // A non-off default mode is valid only alongside the
                // operator's hand-set consent latch (spec §12).
                let latch = if def.key == "memory.default_mode" {
                    "memory.default_mode_confirmed = true\n"
                } else {
                    ""
                };
                let content = format!("{latch}{} = {option}\n", def.key);
                temp.write(&content);
                let config = synaps_cli::config::load_config();
                if !config.warnings.is_empty() {
                    bad.push(format!("{} = {option}: {:?}", def.key, config.warnings));
                }
                let problems = synaps_cli::config::config_content_problems(&content);
                if !problems.is_empty() {
                    bad.push(format!("{} = {option}: {problems:?}", def.key));
                }
            }
        }
        drop(temp);
        assert!(bad.is_empty(), "cycler options the parser rejects: {bad:#?}");
    }

    /// Coverage guard: every key the config parser accepts — the top-level
    /// `KNOWN_CONFIG_KEYS` and the explicit family list
    /// `FAMILY_CONFIG_KEYS` — has a /settings row or an `EXCLUDED_CONFIG_KEYS`
    /// entry. A new config key without either fails here.
    #[test]
    fn every_config_key_has_a_row_or_an_exclusion() {
        let rows: Vec<&str> = ALL_SETTINGS.iter().map(|d| d.key).collect();
        let excluded: Vec<&str> = EXCLUDED_CONFIG_KEYS.iter().map(|(k, _)| *k).collect();
        let parser_keys: Vec<&str> = synaps_cli::config::KNOWN_CONFIG_KEYS
            .iter()
            .chain(synaps_cli::config::FAMILY_CONFIG_KEYS.iter())
            .copied()
            .collect();
        let missing: Vec<&str> = parser_keys
            .iter()
            .copied()
            .filter(|k| !rows.contains(k) && !excluded.contains(k))
            .collect();
        assert!(
            missing.is_empty(),
            "config keys with neither a /settings row nor an EXCLUDED_CONFIG_KEYS entry: {missing:?}"
        );
        // EXCLUDED stays tight: only real parser keys, never also a row, and
        // every exclusion says why.
        for (key, reason) in EXCLUDED_CONFIG_KEYS {
            assert!(parser_keys.contains(key), "excluded key {key} is not a parser key");
            assert!(!rows.contains(key), "{key} is both a row and excluded");
            assert!(!reason.trim().is_empty(), "{key} needs a reason");
        }
        // And the UI never writes a key the parser does not know.
        for def in ALL_SETTINGS {
            if matches!(def.editor, EditorKind::Display) {
                continue;
            }
            assert!(
                parser_keys.contains(&def.key),
                "settings row {} is not a config parser key",
                def.key
            );
        }
    }

    /// Every row shows a real current value: snapshot-backed rows are the
    /// original fixed set; every other row is config-backed and resolves in
    /// `config_row_display` (never the "?" fallback), and with an empty
    /// config the value shown is one the parser accepts.
    #[test]
    fn every_config_backed_row_displays_a_parseable_default() {
        use super::super::draw::{config_row_display, DEFAULT_DISPLAY, NONE_DISPLAY};
        const SNAPSHOT_ROWS: &[&str] = &[
            "model",
            "thinking",
            "reasoning_type",
            "context_window",
            "compaction_model",
            "api_retries",
            "subagent_timeout",
            "max_tool_output",
            "bash_timeout",
            "bash_max_timeout",
            "theme",
            "tui_background_opaque",
            "tui_streaming_glow",
            "theme_transition",
            "sidecar_toggle_key",
            "startup.quick_start",
            "startup.extensions_ready_timeout_secs",
            "daemon.idle_exit_secs",
            "daemon.prompt_abandon_secs",
            "daemon.parked_evict_secs",
        ];
        let _lock = config_lock();
        let temp = TempConfig::new("");
        let mut bad = Vec::new();
        for def in ALL_SETTINGS {
            if SNAPSHOT_ROWS.contains(&def.key) {
                continue;
            }
            let Some(shown) = config_row_display(def.key) else {
                bad.push(format!("{}: no current-value display", def.key));
                continue;
            };
            if let EditorKind::Cycler(options) = def.editor {
                if !options.contains(&shown.as_str()) {
                    bad.push(format!("{}: default {shown:?} not in {options:?}", def.key));
                }
            }
            if shown == NONE_DISPLAY || shown == DEFAULT_DISPLAY {
                continue;
            }
            let problems =
                synaps_cli::config::config_content_problems(&format!("{} = {shown}\n", def.key));
            if !problems.is_empty() {
                bad.push(format!("{} = {shown}: {problems:?}", def.key));
            }
        }
        drop(temp);
        assert!(bad.is_empty(), "{bad:#?}");
    }

    /// The display follows the configured value (normalised spellings).
    #[test]
    fn config_row_display_reads_the_configured_value() {
        use super::super::draw::config_row_display;
        let _lock = config_lock();
        let temp = TempConfig::new(concat!(
            "cache_ttl = 60m\n",
            "cache_keepwarm = 6H\n",
            "compaction_mode = local_only\n",
            "process_scopes = no\n",
            "disabled_tools = bash,  ls\n",
            "turn_budget.worker.max_cost_usd = 2.5\n",
        ));
        assert_eq!(config_row_display("cache_ttl").as_deref(), Some("1h"));
        assert_eq!(config_row_display("cache_keepwarm").as_deref(), Some("6h"));
        assert_eq!(config_row_display("compaction_mode").as_deref(), Some("local"));
        assert_eq!(config_row_display("process_scopes").as_deref(), Some("off"));
        assert_eq!(config_row_display("disabled_tools").as_deref(), Some("bash, ls"));
        assert_eq!(
            config_row_display("turn_budget.worker.max_cost_usd").as_deref(),
            Some("2.5")
        );
        drop(temp);
    }

    #[test]
    fn typed_cyclers_are_cycler_rows() {
        for key in TYPED_CYCLERS {
            let def = ALL_SETTINGS
                .iter()
                .find(|d| d.key == *key)
                .unwrap_or_else(|| panic!("{key} must be a settings row"));
            assert!(matches!(def.editor, EditorKind::Cycler(_)), "{key} must be a Cycler");
        }
    }

    fn is_ok(apply: SettingApply) -> bool {
        matches!(apply, SettingApply::Local(Ok(())))
    }

    fn is_err(apply: SettingApply) -> bool {
        matches!(apply, SettingApply::Local(Err(_)))
    }

    /// Config-only rows validate through the real parser: invalid input is
    /// `Local(Err)` (no write), valid input `Local(Ok)` (caller writes).
    #[test]
    fn config_row_validators_accept_valid_and_reject_invalid() {
        let _lock = config_lock();
        let temp = TempConfig::new("");
        let mut app = app_with_model("m");
        {
            let mut d = |key: &str, value: &str| apply_setting_dispatch(key, value, &mut app);

            assert!(is_ok(d("cache_ttl", "hybrid")));
            assert!(is_err(d("cache_ttl", "2h")));
            assert!(is_ok(d("cache_keepwarm", "6h")));
            assert!(is_err(d("cache_keepwarm", "49h")));
            assert!(is_ok(d("cache_keepwarm_auto_max_hours", "48")));
            assert!(is_err(d("cache_keepwarm_auto_max_hours", "49")));
            assert!(is_ok(d("context_management.pressure_tokens", "auto")));
            assert!(is_ok(d("context_management.pressure_tokens", "120000")));
            assert!(is_err(d("context_management.pressure_tokens", "lots")));
            assert!(is_ok(d("context_management.target_tokens", "auto")));
            assert!(is_ok(d("context_management.target_tokens", "90000")));
            assert!(is_err(d("context_management.target_tokens", "0")));
            assert!(is_err(d("context_management.target_tokens", "lots")));
            assert!(is_err(d("context_management.finish_rounds", "9")));
            assert!(is_ok(d("compaction_exclude", "thinking, tool_results")));
            assert!(is_err(d("compaction_exclude", "thinking, secrets")));
            assert!(is_ok(d("refusal_retries", "4")));
            assert!(is_err(d("refusal_retries", "-1")));
            assert!(is_err(d("telemetry", "loud")));
            assert!(is_ok(d("subagent.max_depth", "3")));
            assert!(is_err(d("subagent.max_depth", "0")));
            assert!(is_err(d("subagent.max_depth", "70000")));
            assert!(is_err(d("identity", "   ")));
            assert!(is_ok(d("events.auto_turn_cap", "0")));
            assert!(is_ok(d("turn_budget.worker.max_cost_usd", "2.50")));
            assert!(is_err(d("turn_budget.worker.max_cost_usd", "-1")));
            assert!(is_err(d("turn_budget.worker.max_cost_usd", "NaN")));
            assert!(is_err(d("turn_budget.foreground.max_tool_calls", "many")));
            assert!(is_err(d("turn_budget.foreground.max_tool_calls", "0")));
            assert!(is_ok(d("turn_budget.foreground.max_tool_calls", "unlimited")));
            assert!(is_err(d("turn_budget.worker.max_provider_rounds", "0")));
            assert!(is_ok(d("turn_budget.worker.max_provider_rounds", "unlimited")));
            assert!(is_ok(d("turn_budget.worker.max_provider_rounds", "128")));
            assert!(is_err(d("turn_budget.autonomous.max_accumulated_tool_result_bytes", "0")));
            assert!(is_ok(d("turn_budget.autonomous.max_accumulated_tool_result_bytes", "unlimited")));
            assert!(is_ok(d("max_fps", "144")));
            assert!(is_err(d("max_fps", "0")));
            assert!(is_err(d("scroll_lines", "21")));
            assert!(is_ok(d("memory.axel.brain", "/srv/axel/brain.r8")));
            assert!(is_err(d("memory.axel.brain", "brain.r8")));
            assert!(is_err(d("memory.user_scope", "yes")));
            assert!(is_err(d("memory.backend", "sqlite")));
        }

        // Cross-key rules see the rest of the file.
        temp.write("subagent.max_total = 4\n");
        assert!(is_err(apply_setting_dispatch("subagent.max_concurrent", "5", &mut app)));
        assert!(is_ok(apply_setting_dispatch("subagent.max_concurrent", "4", &mut app)));
        // Validation never writes.
        assert_eq!(
            synaps_cli::config::read_config_value("subagent.max_concurrent"),
            None
        );

        // A non-off default memory mode needs the hand-set consent latch.
        temp.write("");
        assert!(is_ok(apply_setting_dispatch(
            "memory.default_mode",
            "off",
            &mut app
        )));
        assert!(is_err(apply_setting_dispatch(
            "memory.default_mode",
            "capture_and_recall",
            &mut app
        )));
        assert!(is_err(apply_setting_dispatch(
            "memory.default_mode",
            "everything",
            &mut app
        )));
        temp.write("memory.default_mode_confirmed = true\n");
        assert!(is_ok(apply_setting_dispatch(
            "memory.default_mode",
            "capture_and_recall",
            &mut app
        )));
        drop(temp);
    }

    /// The default-mode row shows the effective mode: off until the
    /// operator's consent latch is set alongside it.
    #[test]
    fn memory_default_mode_display_requires_the_consent_latch() {
        use super::super::draw::config_row_display;
        let _lock = config_lock();
        let temp = TempConfig::new("memory.default_mode = capture_and_recall\n");
        assert_eq!(
            config_row_display("memory.default_mode").as_deref(),
            Some("off")
        );
        temp.write(
            "memory.default_mode = capture_and_recall\nmemory.default_mode_confirmed = true\n",
        );
        assert_eq!(
            config_row_display("memory.default_mode").as_deref(),
            Some("capture_and_recall")
        );
        temp.write("memory.default_mode = bogus\nmemory.default_mode_confirmed = true\n");
        assert_eq!(
            config_row_display("memory.default_mode").as_deref(),
            Some("off")
        );
        drop(temp);
    }
}
