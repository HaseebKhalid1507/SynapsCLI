use crate::core::shell_config::ShellConfig;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::OnceLock;

static PROFILE_NAME: OnceLock<Option<String>> = OnceLock::new();
static PROVIDER_KEYS: OnceLock<BTreeMap<String, String>> = OnceLock::new();
static IDENTITY: OnceLock<String> = OnceLock::new();

pub const DEFAULT_IDENTITY: &str =
    "You are an AI assistant running in SynapsCLI, an open-source agent runtime.";

thread_local! {
    /// [`with_identity_for_tests`]'s pin; `None` everywhere else.
    static IDENTITY_OVERRIDE: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// Returns the configured identity string for the system prompt preamble.
/// Falls back to `DEFAULT_IDENTITY` (the SynapsCLI identity above) if not set
/// in config. Initialized by `load_config()` — safe to call anytime after boot.
pub fn get_identity() -> String {
    if let Some(pinned) = IDENTITY_OVERRIDE.with(|o| o.borrow().clone()) {
        return pinned;
    }
    IDENTITY
        .get()
        .cloned()
        .unwrap_or_else(|| DEFAULT_IDENTITY.to_string())
}

/// Run `f` with [`get_identity`] pinned to `identity` on this thread.
///
/// `IDENTITY` is a process-wide `OnceLock` latched by whichever test first
/// calls `load_config()`, against whatever config is ambient then (on a
/// developer machine, the real `~/.synaps-cli/config`). A test that asserts
/// on the identity pins it here instead, so the result does not depend on
/// test order or on HOME. Thread-scoped, so concurrent tests don't see each
/// other's pin; code `f` hands to another thread does not see it either.
/// Restored on unwind.
#[doc(hidden)]
pub fn with_identity_for_tests<R>(identity: &str, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<String>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let prev = self.0.take();
            IDENTITY_OVERRIDE.with(|o| *o.borrow_mut() = prev);
        }
    }
    let prev = IDENTITY_OVERRIDE.with(|o| o.borrow_mut().replace(identity.to_string()));
    let _restore = Restore(prev);
    f()
}

/// Provider API keys parsed from `provider.<name> = ...` lines in config.
/// Empty if `load_config()` hasn't been called. The registry falls back to
/// env vars, so e.g. `GROQ_API_KEY` works even with an empty map.
pub fn get_provider_keys() -> BTreeMap<String, String> {
    PROVIDER_KEYS.get().cloned().unwrap_or_default()
}

/// Returns the active profile name, if any.
/// Reads from `SYNAPS_PROFILE` environment variable if not already set programmatically.
pub fn get_profile() -> Option<String> {
    PROFILE_NAME
        .get_or_init(|| std::env::var("SYNAPS_PROFILE").ok())
        .clone()
}

/// Sets the active profile name. Must be called before any `get_profile()` call
/// (i.e., before config resolution begins). Uses OnceLock — first write wins,
/// subsequent calls are no-ops. No env var mutation (unsafe under tokio).
pub fn set_profile(name: Option<String>) {
    let _ = PROFILE_NAME.set(name);
}

pub fn base_dir() -> PathBuf {
    if let Ok(path) = std::env::var("SYNAPS_BASE_DIR") {
        return PathBuf::from(path);
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".synaps-cli")
}

/// Overrides the Synaps base directory. Intended for tests and embedded harnesses.
#[doc(hidden)]
pub fn set_base_dir_for_tests(path: PathBuf) {
    std::env::set_var("SYNAPS_BASE_DIR", path);
}

/// Resolves a path for reading. Checks the profile folder first, then falls back to the default folder.
pub fn resolve_read_path(filename: &str) -> PathBuf {
    let base = base_dir();

    if let Some(profile) = get_profile() {
        let profile_path = base.join(&profile).join(filename);
        if profile_path.exists() {
            return profile_path;
        }
    }

    base.join(filename)
}

/// Resolves a path for reading with an extended arbitrary path tree.
pub fn resolve_read_path_extended(path: &str) -> PathBuf {
    let base = base_dir();

    if let Some(profile) = get_profile() {
        let profile_path = base.join(&profile).join(path);
        if profile_path.exists() {
            return profile_path;
        }
    }

    base.join(path)
}

/// Resolves a path for writing. Unconditionally writes to the profile folder if a profile is active.
pub fn resolve_write_path(filename: &str) -> PathBuf {
    let mut base = base_dir();

    if let Some(profile) = get_profile() {
        base.push(profile);
    }

    let _ = std::fs::create_dir_all(&base);
    base.join(filename)
}

/// Gets the absolute directory for the current profile (or root if default).
pub fn get_active_config_dir() -> PathBuf {
    let mut base = base_dir();
    if let Some(profile) = get_profile() {
        base.push(profile);
    }
    base
}

/// Automatic context management; independent of compaction disclosure.
///
/// On by default on Unix: a loaded config (or no config file at all)
/// resolves an unset `context_management.mode` to [`Self::CONFIG_DEFAULT`]
/// (`auto` on Unix, where the context archive is supported; `off`
/// elsewhere); `context_management.mode = off` opts out. The enum's `Default` (`Off`) is
/// the dark state of a runtime that has applied no config, and the fallback
/// for an invalid `context_management` block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ContextManagementMode {
    #[default]
    Off,
    Auto,
}

impl ContextManagementMode {
    /// What an unset `context_management.mode` resolves to: `auto` on Unix,
    /// `off` where the context archive is unsupported (`/context auto` is
    /// refused there too).
    pub const CONFIG_DEFAULT: Self = if cfg!(unix) { Self::Auto } else { Self::Off };

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" => Some(Self::Off),
            "auto" => Some(Self::Auto),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Auto => "auto",
        }
    }
}

/// Task-aware soft context bands, parsed from `context_management.*` keys.
///
/// Missing thresholds use window-aware defaults (200k: 140k/180k;
/// 1m: 250k/400k). A single override adjusts the other default as necessary
/// to keep pressure below rollover. These settings never override the host's
/// hard capacity or authorize tools, disclosure, or a session transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextManagementConfig {
    pub mode: ContextManagementMode,
    /// Optional absolute token threshold; `auto` in config clears the override.
    pub pressure_tokens: Option<u64>,
    /// Optional absolute token threshold; `auto` in config clears the override.
    pub rollover_tokens: Option<u64>,
    /// Minimum remaining headroom before any model round. Default: 16,000.
    pub reserve_tokens: u64,
    /// Maximum extra Plan/WrapUp rounds after rollover becomes due. Default: 2.
    /// Zero disables extensions; phase reports cannot renew this allowance.
    pub finish_rounds: u32,
    /// Footprint a rollover successor aims for (system prompt + tool schemas +
    /// retained messages). `None`/`auto` = min(100,000, 2/3 of pressure).
    /// An explicit value must be positive and below the pressure threshold.
    pub target_tokens: Option<u64>,
    /// Size cap of the rolling summary of dropped history carried by each
    /// successor; it counts inside `target_tokens`. `None`/`auto` =
    /// min(15,000, 15% of the target); `0` disables the summary. An explicit
    /// value must be below the effective target.
    pub summary_tokens: Option<u64>,
    /// Bound on one rollover summary request, in seconds (1–600). On timeout
    /// the rollover proceeds without a new summary.
    pub summary_timeout_secs: u64,
}

impl Default for ContextManagementConfig {
    fn default() -> Self {
        Self {
            mode: ContextManagementMode::Off,
            pressure_tokens: None,
            rollover_tokens: None,
            reserve_tokens: 16_000,
            finish_rounds: 2,
            target_tokens: None,
            summary_tokens: None,
            summary_timeout_secs: Self::DEFAULT_SUMMARY_TIMEOUT_SECS,
        }
    }
}

impl ContextManagementConfig {
    /// The settings an unset `context_management` block resolves to: every
    /// default, with [`ContextManagementMode::CONFIG_DEFAULT`] as the mode.
    pub fn config_default() -> Self {
        Self {
            mode: ContextManagementMode::CONFIG_DEFAULT,
            ..Self::default()
        }
    }

    pub const MAX_FINISH_ROUNDS: u32 = 8;
    pub const MAX_RESERVE_TOKENS: u64 = 1_000_000;
    /// A warm summary at xhigh/max effort may write cap + 32k output tokens;
    /// ~110 tokens/s was observed live, so 120 s ended long ones early.
    pub const DEFAULT_SUMMARY_TIMEOUT_SECS: u64 = 300;
    pub const MAX_SUMMARY_TIMEOUT_SECS: u64 = 600;

    /// Validate independently of a model selection. Also call
    /// `validate_for_window` when the effective context window is known.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.pressure_tokens == Some(0) {
            return Err("pressure_tokens must be positive");
        }
        if self.rollover_tokens.is_some_and(|v| v < 2) {
            return Err("rollover_tokens must be at least 2");
        }
        if let (Some(pressure), Some(rollover)) = (self.pressure_tokens, self.rollover_tokens) {
            if pressure >= rollover {
                return Err("pressure_tokens must be below rollover_tokens");
            }
        }
        if self.target_tokens == Some(0) {
            return Err("target_tokens must be positive");
        }
        if let (Some(target), Some(pressure)) = (self.target_tokens, self.pressure_tokens) {
            if target >= pressure {
                return Err("target_tokens must be below pressure_tokens");
            }
        }
        if !(1..=Self::MAX_RESERVE_TOKENS).contains(&self.reserve_tokens) {
            return Err("reserve_tokens must be between 1 and 1000000");
        }
        if self.finish_rounds > Self::MAX_FINISH_ROUNDS {
            return Err("finish_rounds must be between 0 and 8");
        }
        if let (Some(summary @ 1..), Some(target)) = (self.summary_tokens, self.target_tokens) {
            if summary >= target {
                return Err("summary_tokens must be below target_tokens");
            }
        }
        if !(1..=Self::MAX_SUMMARY_TIMEOUT_SECS).contains(&self.summary_timeout_secs) {
            return Err("summary_timeout_secs must be between 1 and 600");
        }
        Ok(())
    }

    /// Threshold overrides must fit the effective window. The hard budget
    /// remains authoritative even when an override fits this validation.
    pub fn validate_for_window(&self, context_window_tokens: u64) -> Result<(), &'static str> {
        self.validate()?;
        if context_window_tokens < 2 {
            return Err("context window must contain at least two tokens");
        }
        if self
            .pressure_tokens
            .is_some_and(|v| v >= context_window_tokens)
        {
            return Err("pressure_tokens must be below the context window");
        }
        if self
            .rollover_tokens
            .is_some_and(|v| v > context_window_tokens)
        {
            return Err("rollover_tokens must not exceed the context window");
        }
        if let Some(target) = self.target_tokens {
            let (pressure, _) =
                crate::core::context_policy::ContextThresholds::bands(self, context_window_tokens);
            if target >= pressure {
                return Err("target_tokens must be below pressure_tokens");
            }
        }
        if let Some(summary @ 1..) = self.summary_tokens {
            let (pressure, _) =
                crate::core::context_policy::ContextThresholds::bands(self, context_window_tokens);
            let target = crate::core::context_policy::ContextThresholds {
                pressure_tokens: pressure,
                rollover_tokens: pressure.saturating_add(1),
            }
            .target_tokens(self);
            if summary >= target {
                return Err("summary_tokens must be below target_tokens");
            }
        }
        Ok(())
    }

    /// These settings for `context_window_tokens`: unchanged when they fit
    /// it (`Ok((self, None))`); otherwise with the threshold overrides
    /// (pressure, rollover, target and summary size) reset to the window's
    /// defaults, and the reason they did not fit. The reserve, finish rounds
    /// and summary timeout stay. `Err` when even the defaults don't fit.
    pub fn fitted_to_window(
        &self,
        context_window_tokens: u64,
    ) -> Result<(Self, Option<&'static str>), &'static str> {
        let Err(why) = self.validate_for_window(context_window_tokens) else {
            return Ok((*self, None));
        };
        let reset = Self {
            pressure_tokens: None,
            rollover_tokens: None,
            target_tokens: None,
            summary_tokens: None,
            ..*self
        };
        reset.validate_for_window(context_window_tokens)?;
        Ok((reset, Some(why)))
    }

    /// The settings to run on `window` when the model's ceiling clamped an
    /// explicit `context_window` of `configured` tokens down to `window`
    /// (`models::effective_context_window`): thresholds that fit the
    /// configured window but not `window` are reset to `window`'s defaults
    /// ([`Self::fitted_to_window`]), with the reason. `None` when there is
    /// nothing to reset: no window was clamped, the thresholds fit, or they
    /// did not fit the configured window either (an invalid policy stays
    /// invalid and still fails closed).
    pub fn reset_for_clamped_window(
        &self,
        configured: Option<u64>,
        window: u64,
    ) -> Option<(Self, &'static str)> {
        let configured = configured.filter(|&configured| configured > window)?;
        self.validate_for_window(configured).ok()?;
        match self.fitted_to_window(window) {
            Ok((reset, Some(why))) => Some((reset, why)),
            Ok((_, None)) | Err(_) => None,
        }
    }
}

/// Parse fields first; validate the complete policy after all lines so threshold
/// ordering and an explicit context window do not depend on config key order.
/// `context_management.summary_model`: `auto` (any case) is `None`; any
/// other value must be a single model id (no whitespace or control
/// characters).
pub fn parse_summary_model(value: &str) -> Result<Option<String>, &'static str> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("auto") {
        return Ok(None);
    }
    if value.is_empty() || value.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("expected auto or a single model id");
    }
    Ok(Some(value.to_string()))
}

fn parse_context_management_config_key(
    config: &mut ContextManagementConfig,
    key: &str,
    value: &str,
) -> Result<(), &'static str> {
    match key {
        "context_management.mode" => {
            config.mode = ContextManagementMode::parse(value).ok_or("expected off or auto")?;
        }
        "context_management.pressure_tokens"
        | "context_management.rollover_tokens"
        | "context_management.target_tokens"
        | "context_management.summary_tokens" => {
            let tokens = if value.eq_ignore_ascii_case("auto") {
                None
            } else {
                Some(
                    value
                        .parse::<u64>()
                        .map_err(|_| "expected auto or an unsigned token count")?,
                )
            };
            match key {
                "context_management.pressure_tokens" => config.pressure_tokens = tokens,
                "context_management.rollover_tokens" => config.rollover_tokens = tokens,
                "context_management.summary_tokens" => config.summary_tokens = tokens,
                _ => config.target_tokens = tokens,
            }
        }
        "context_management.reserve_tokens" => {
            config.reserve_tokens = value
                .parse()
                .map_err(|_| "expected an unsigned token count")?;
        }
        "context_management.finish_rounds" => {
            config.finish_rounds = value
                .parse()
                .map_err(|_| "expected an unsigned round count")?;
        }
        "context_management.summary_timeout_secs" => {
            config.summary_timeout_secs = value
                .parse()
                .map_err(|_| "expected an unsigned number of seconds")?;
        }
        _ => return Err("unknown context management key"),
    }
    Ok(())
}

/// Startup / first-turn behaviour (`startup.*` keys). Boot-time only — these
/// take effect at the next launch, never mid-session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartupConfig {
    /// Create the session without waiting for extensions. When true the
    /// attach path sets `await_extensions=false`: the session (and the TUI)
    /// comes up at once, and `on_session_start` runs in the background. The
    /// first turn starts at once too, but its model request waits until the
    /// hook has run: for extension discovery (up to
    /// `extensions_ready_timeout_secs`, default 30 s), then for the hook
    /// (each handler up to 5 s). Clients are told while it waits. Default: true.
    pub quick_start: bool,
    /// Max seconds to wait for extension discovery before a session's
    /// `on_session_start`: at create when Quick Start is off, before the
    /// first turn's request when it is on. Default: 30. It does not bound
    /// the `on_session_start` call itself, which then waits its turn at the
    /// host's one-at-a-time gate.
    pub extensions_ready_timeout_secs: u64,
}

impl Default for StartupConfig {
    fn default() -> Self {
        Self {
            quick_start: true,
            extensions_ready_timeout_secs: 30,
        }
    }
}

/// Daemon lifetime knobs (`daemon.*` keys). Config-file fallbacks; the matching
/// `SYNAPS_DAEMON_*` env vars still WIN over these when set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DaemonConfig {
    /// `daemon.reload_resume` (on/off, default on): after a daemon reload,
    /// every session whose turn the reload cut gets one queued message that
    /// starts a new turn ("resume: ..."). Off is the kill switch: cut
    /// sessions stay idle, as before.
    pub reload_resume: bool,
    /// `daemon.crash_restore` (on/off, default on): the daemon keeps a crash
    /// record of its sessions (`daemon.live.json` in its runtime dir), and a
    /// fresh start after a daemon that died (or stopped cleanly) restores
    /// every recorded session from disk right after it listens (listed as
    /// restoring until each is back), resuming once each turn the death or
    /// stop cut. Off is the kill switch: no record is kept or read, and a
    /// fresh start serves only what clients re-attach.
    pub crash_restore: bool,
    /// Seconds an auto-spawned daemon stays alive after the last client
    /// disconnects. `0` = never idle-exit. Env `SYNAPS_DAEMON_IDLE_EXIT_SECS`
    /// overrides. Default: 10.
    pub idle_exit_secs: u64,
    /// Seconds a pending host confirmation on a detached session survives
    /// before it is fail-closed answered `None`. `0` = disabled (survives
    /// forever). Env `SYNAPS_DAEMON_PROMPT_ABANDON_SECS` overrides. Default: 3600.
    pub prompt_abandon_secs: u64,
    /// Seconds a PARKED session row lingers before eviction. `0` = keep
    /// forever. Env `SYNAPS_DAEMON_PARKED_EVICT_SECS` overrides. Default: 3600.
    pub parked_evict_secs: u64,
    /// File-creation mask of the daemon process (`daemon.umask`), and so of
    /// everything it starts. Set once at daemon start, from the config the
    /// daemon booted with (`agent_engine::daemon::umask`). Default: inherit.
    pub umask: DaemonUmask,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            reload_resume: true,
            crash_restore: true,
            idle_exit_secs: 10,
            prompt_abandon_secs: 3600,
            parked_evict_secs: 3600,
            umask: DaemonUmask::Inherit,
        }
    }
}

/// `daemon.umask`: keep the mask the daemon was started with, or set one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DaemonUmask {
    /// The mask the daemon was started with (default).
    #[default]
    Inherit,
    /// These permission bits (at most `0o777`).
    Mode(u32),
}

impl DaemonUmask {
    /// `inherit` (any case) or an octal mode of 1-4 digits up to `0777`,
    /// optionally in one pair of matching quotes (`"022"`, `'0022'`).
    pub fn parse(value: &str) -> Result<Self, &'static str> {
        const EXPECTED: &str = "expected inherit or an octal mode from 000 to 0777 (e.g. 022); inheriting";
        let value = value.trim();
        let value = ['"', '\'']
            .iter()
            .find_map(|q| value.strip_prefix(*q)?.strip_suffix(*q))
            .unwrap_or(value);
        if value.eq_ignore_ascii_case("inherit") {
            return Ok(Self::Inherit);
        }
        if value.is_empty() || value.len() > 4 || !value.bytes().all(|b| (b'0'..=b'7').contains(&b)) {
            return Err(EXPECTED);
        }
        match u32::from_str_radix(value, 8) {
            Ok(mode) if mode <= 0o777 => Ok(Self::Mode(mode)),
            _ => Err(EXPECTED),
        }
    }
}

impl SynapsConfig {
    /// The parser's warning for a malformed `daemon.umask` value (the daemon
    /// then inherits), if any: the last one, as the last line wins.
    pub fn daemon_umask_warning(&self) -> Option<&str> {
        self.warnings.iter().rfind(|w| w.starts_with("daemon.umask ")).map(String::as_str)
    }
}

/// Subagent fan-out limits (`subagent.*` keys). They apply to the whole
/// delegation tree of one session: workers started by workers count against
/// the same limits as workers started by the foreground. A prompt manifest
/// with an explicit delegation policy sets its own concurrency and total
/// limits; `max_depth` applies either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubagentLimits {
    /// Workers running at the same time. Default: 16.
    pub max_concurrent: usize,
    /// Workers dispatched and not yet collected with `reconciled=true`
    /// (running, finished, or collected without reconciling). Default: 64.
    pub max_total: usize,
    /// How deep workers may nest: 1 = only the foreground delegates,
    /// 2 = its workers may delegate once more, and so on. Default: 4.
    pub max_depth: u16,
    /// `subagent.context_warn_tokens`: a worker start, resume or oneshot
    /// whose first request is at least this many provider tokens is
    /// admitted with a warning the caller sees. `None` (`off`) disables
    /// the warning. Default: 150,000. A context limit, not a spend cap.
    pub context_warn_tokens: Option<u64>,
    /// `subagent.resume_refuse_tokens`: `subagent_resume` refuses a resume
    /// whose first request would be above this many provider tokens; the
    /// caller starts a fresh worker instead. `None` (`off`) disables the
    /// refusal. Default: 250,000. A context limit, not a spend cap.
    pub resume_refuse_tokens: Option<u64>,
    /// `subagent.tool_hooks` (on | off): whether a worker's tool calls run
    /// its caller's `before_tool_call` hooks (a guard extension's checks
    /// and confirms) as the caller's own calls do. `off` gives workers an
    /// empty hook bus again (no hook runs for their calls). Read at each
    /// worker spawn. Default: on.
    pub tool_hooks: bool,
    /// `subagent.context_rollover`: the operator's opt-out of worker
    /// context rollover. Default: inherit.
    pub context_rollover: ContextRollover,
    /// `subagent.context_target_tokens`: the successor footprint a worker's
    /// rollover aims for. It replaces the inherited
    /// `context_management.target_tokens` for workers only; main sessions
    /// never read it. `None` (`auto`, the default) is the inherited policy
    /// unchanged. Checked against each worker's own window: a value not
    /// below its pressure threshold, or with a summary that does not fit
    /// below it, falls back to the target the worker has without the key
    /// (the inherited one, or auto) with one notice. The summary keeps
    /// its auto size (15% of the target, at most 15,000) unless
    /// `context_management.summary_tokens` is set: 48,000 gives 7,200.
    pub context_target_tokens: Option<u64>,
}

impl Default for SubagentLimits {
    fn default() -> Self {
        Self {
            max_concurrent: 16,
            max_total: 64,
            max_depth: 4,
            context_warn_tokens: Some(150_000),
            resume_refuse_tokens: Some(250_000),
            tool_hooks: true,
            context_rollover: ContextRollover::Inherit,
            context_target_tokens: None,
        }
    }
}

/// Whether workers may have context rollover
/// (`docs/specs/subagent-context-rollover.md` §2): opt-out only, by the
/// operator (`subagent.context_rollover`) or the caller (the worker tools'
/// `context_rollover` parameter). Off from either wins.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ContextRollover {
    /// A worker follows its parent's live context management.
    #[default]
    Inherit,
    /// The worker runs without context rollover.
    Off,
}

impl ContextRollover {
    /// `inherit` or `off`, exactly; anything else is `None`.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "inherit" => Some(Self::Inherit),
            "off" => Some(Self::Off),
            _ => None,
        }
    }
}

/// Parse `subagent.*` keys. Unknown keys and invalid values return an error
/// (caller warns and keeps the default).
fn parse_subagent_config_key(
    config: &mut SubagentLimits,
    key: &str,
    value: &str,
) -> Result<(), &'static str> {
    let positive = || match value.trim().parse::<usize>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err("expected a whole number of at least 1"),
    };
    match key {
        "subagent.max_concurrent" => config.max_concurrent = positive()?,
        "subagent.max_total" => config.max_total = positive()?,
        "subagent.max_depth" => {
            config.max_depth =
                u16::try_from(positive()?).map_err(|_| "expected a whole number from 1 to 65535")?
        }
        "subagent.context_warn_tokens" => config.context_warn_tokens = tokens_or_off(value)?,
        "subagent.resume_refuse_tokens" => config.resume_refuse_tokens = tokens_or_off(value)?,
        "subagent.tool_hooks" => {
            config.tool_hooks = match value.trim() {
                "on" | "true" | "1" | "yes" => true,
                "off" | "false" | "0" | "no" => false,
                _ => return Err("expected on or off"),
            }
        }
        "subagent.context_rollover" => match ContextRollover::parse(value.trim()) {
            Some(choice) => config.context_rollover = choice,
            None => {
                // An opt-out: a malformed value is off, never `inherit`.
                config.context_rollover = ContextRollover::Off;
                return Err("expected inherit or off; worker context rollover is off");
            }
        },
        "subagent.context_target_tokens" => {
            let value = value.trim();
            // A malformed value is `auto`, never an earlier line's.
            config.context_target_tokens = None;
            if !value.eq_ignore_ascii_case("auto") {
                match value.parse::<u64>() {
                    Ok(n) if n > 0 => config.context_target_tokens = Some(n),
                    _ => {
                        return Err(
                            "expected a whole number of tokens of at least 1, or auto; using auto",
                        )
                    }
                }
            }
        }
        _ => return Err("unknown subagent key"),
    }
    Ok(())
}

/// A token line: a whole number of at least 1, or `off` (`None`).
fn tokens_or_off(value: &str) -> Result<Option<u64>, &'static str> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("off") {
        return Ok(None);
    }
    match value.parse::<u64>() {
        Ok(n) if n > 0 => Ok(Some(n)),
        _ => Err("expected a whole number of tokens of at least 1, or off"),
    }
}

/// Parse `startup.*` keys. Unknown keys return an error (caller warns).
fn parse_startup_config_key(
    config: &mut StartupConfig,
    key: &str,
    value: &str,
) -> Result<(), &'static str> {
    match key {
        "startup.quick_start" => {
            config.quick_start = match value.trim() {
                "true" | "1" | "on" | "yes" => true,
                "false" | "0" | "off" | "no" => false,
                _ => return Err("expected on or off"),
            };
        }
        "startup.extensions_ready_timeout_secs" => {
            config.extensions_ready_timeout_secs =
                value.trim().parse().map_err(|_| "expected an unsigned second count")?;
        }
        _ => return Err("unknown startup key"),
    }
    Ok(())
}

/// Parse `daemon.*` keys. Unknown keys return an error (caller warns).
fn parse_daemon_config_key(
    config: &mut DaemonConfig,
    key: &str,
    value: &str,
) -> Result<(), &'static str> {
    let secs = || value.trim().parse::<u64>().map_err(|_| "expected an unsigned second count");
    match key {
        "daemon.reload_resume" => {
            config.reload_resume = match value.trim() {
                "true" | "1" | "on" | "yes" => true,
                "false" | "0" | "off" | "no" => false,
                _ => return Err("expected on or off"),
            };
        }
        "daemon.crash_restore" => {
            config.crash_restore = match value.trim() {
                "true" | "1" | "on" | "yes" => true,
                "false" | "0" | "off" | "no" => false,
                _ => return Err("expected on or off"),
            };
        }
        "daemon.idle_exit_secs" => config.idle_exit_secs = secs()?,
        "daemon.prompt_abandon_secs" => config.prompt_abandon_secs = secs()?,
        "daemon.parked_evict_secs" => config.parked_evict_secs = secs()?,
        // A malformed value inherits, also over an earlier valid line.
        "daemon.umask" => match DaemonUmask::parse(value) {
            Ok(umask) => config.umask = umask,
            Err(reason) => {
                config.umask = DaemonUmask::Inherit;
                return Err(reason);
            }
        },
        _ => return Err("unknown daemon key"),
    }
    Ok(())
}

/// Server security configuration parsed from `server.*` keys.
#[derive(Debug, Clone, Default)]
pub struct ServerConfig {
    /// Comma-separated list of allowed Origin headers. Empty = allow all (localhost-only protection).
    pub allowed_origins: Vec<String>,
    /// Pre-shared authentication token. If set, clients must provide it on WebSocket upgrade
    /// via `?token=X` query param or `Authorization: Bearer X` header. If None, auto-generated on boot.
    pub token: Option<String>,
    /// When true, `HookResult::Confirm` is auto-approved without prompting (useful for headless/agent mode).
    pub auto_approve_confirms: bool,
    /// Maximum inbound message size in bytes. Defaults to context_window * 4 (rough token→byte estimate).
    /// None means no artificial cap.
    pub max_message_size: Option<usize>,
}

/// Bridge daemon configuration parsed from `bridge.*` keys.
///
/// Controls best-effort mirroring of watcher heartbeats over the bridge
/// daemon's UDS `ControlSocket` (`heartbeat_emit` op). All keys are optional
/// and the feature is OFF by default.
#[derive(Debug, Clone)]
pub struct BridgeConfig {
    /// Path to the bridge daemon's UDS control socket.
    /// When `None`, defaults to `base_dir().join("bridge/control.sock")`.
    pub uds_path: Option<PathBuf>,
    /// When true, every watcher heartbeat tick mirrors a `heartbeat_emit`
    /// JSON-RPC call over the UDS socket. Errors are logged at debug only.
    pub heartbeat_mirror: bool,
    /// Per-call timeout in milliseconds (covers connect + write + read).
    pub heartbeat_timeout_ms: u64,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            uds_path: None,
            heartbeat_mirror: false,
            heartbeat_timeout_ms: 250,
        }
    }
}

impl BridgeConfig {
    /// Resolve the UDS path, falling back to the default under `base_dir()`.
    pub fn resolved_uds_path(&self) -> PathBuf {
        self.uds_path
            .clone()
            .unwrap_or_else(|| base_dir().join("bridge/control.sock"))
    }
}

/// Auth configuration parsed from `auth.*` keys. Controls whether this client
/// resolves provider tokens from the local `auth.json` (default) or from a
/// remote credential broker. See task #157 / `auth::credential_source`.
#[derive(Debug, Clone, Default)]
pub struct AuthConfig {
    /// Broker base URL. When set (here, or via `SYNAPS_AUTH_ENDPOINT`), the
    /// client fetches short-lived access tokens from the broker instead of
    /// reading/refreshing the local `auth.json`.
    pub remote_endpoint: Option<String>,
    /// Per-machine bearer presented to the broker (or `SYNAPS_MACHINE_TOKEN`).
    /// This is the machine's own identity, never the provider credential.
    pub machine_token: Option<String>,
    /// Selected OAuth account per provider from `auth.account.<provider> =
    /// <label|default|auto>`. Keys are the raw provider ids as written;
    /// validation (and fail-closed handling of bad labels) happens in
    /// [`AccountPolicy::from_config_map`](crate::core::auth::AccountPolicy::from_config_map).
    pub accounts: BTreeMap<String, String>,
    /// Raw automatic chooser settings, relative to auth.auto.
    pub auto: BTreeMap<String, String>,
    /// Raw try order per provider from `auth.order.<provider> = L1,L2,...`.
    /// Parsed leniently by [`crate::core::auth::accounts_api::parse_order`].
    pub order: BTreeMap<String, String>,
}

impl AuthConfig {
    /// Account policy from config only (no env overlay). Invalid entries fail
    /// closed for their provider; see `AccountPolicy::from_config_map`.
    pub fn account_policy(&self) -> crate::core::auth::AccountPolicy {
        crate::core::auth::AccountPolicy::from_config_map(&self.accounts).0
    }

    /// Effective selector for one provider (env > config > default).
    pub fn account_selector(
        &self,
        provider: crate::core::auth::OAuthProviderId,
    ) -> crate::core::auth::AccountSelector {
        self.account_policy().with_env_overlay().selector(provider)
    }

    /// Resolve the credential source. Environment variables take precedence over
    /// config-file values: `SYNAPS_AUTH_ENDPOINT` / `SYNAPS_MACHINE_TOKEN`.
    /// Returns `Remote` iff an endpoint is set (env or config), else `Local`.
    ///
    /// The machine token here is only ever the EXPLICIT one (env or config).
    /// When none is configured it stays empty, and that emptiness is the
    /// provenance record: the broker client ([`crate::core::auth::MachineToken`])
    /// then uses — and may re-read after a 401 — the local broker's
    /// auto-generated `<base_dir>/auth-broker.token`, but only for a loopback
    /// endpoint (#147 C1, prA N1). An explicit token is never replaced by,
    /// or sent alongside, the file token.
    pub fn credential_source(&self) -> crate::core::auth::CredentialSource {
        let endpoint = std::env::var("SYNAPS_AUTH_ENDPOINT")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| self.remote_endpoint.clone());
        let machine_token = std::env::var("SYNAPS_MACHINE_TOKEN")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| self.machine_token.clone().filter(|s| !s.trim().is_empty()));
        crate::core::auth::CredentialSource::from_parts(endpoint, machine_token)
    }
}

/// Upper bound for the `<N>h` form of `cache_keepwarm`. The ski-rental bound
/// already caps an abandoned session's spend at about one prefix rewrite; the
/// hour cap is an extra, user-chosen wall on top of it.
pub const MAX_CACHE_KEEPWARM_HOURS: u32 = 48;

/// Default for `cache_keepwarm_auto_max_hours`: the longest an `auto` idle
/// period may run. Without it the ski-rental bound alone lets a 0.05×
/// cache-read model (Opus 5.5, Sonnet 5.5) ping ~39 times over ~36h and a
/// 0.025× model (Fable 5.1, Mythos 5.1) ~79 times over ~72h under a 1h TTL,
/// keeping an abandoned session unparked for days. 24h: a user who steps
/// away for a working day or overnight still comes back to a warm cache; a
/// session left over a weekend parks. (Owner decision, task #444.)
pub const DEFAULT_CACHE_KEEPWARM_AUTO_MAX_HOURS: u32 = 24;

/// Default for `subagent_cold_resume_compact_tokens`. Below ~100k tokens a
/// cold full replay costs well under $1 on the priciest model and keeps every
/// detail; above it (measured resumes reached 250k–715k tokens, up to ~$3.50
/// each) the compact context is the cheaper trade.
pub const DEFAULT_SUBAGENT_COLD_RESUME_COMPACT_TOKENS: u64 = 100_000;

/// Default for `subagent_rollover_tokens`: the footprint at which a worker's
/// context rolls over (to a successor of about 100k on a 1m window). Workers
/// are long 5-minute-TTL loops with no human pauses, so every round re-reads
/// the whole context; a 100k -> 200k cycle repays the ~100k successor write
/// (about 17 rounds on Opus 5.5) well before the next rollover.
pub const DEFAULT_SUBAGENT_ROLLOVER_TOKENS: u64 = 200_000;

/// Smallest accepted `subagent_rollover_tokens`. Below it the derived
/// successor target is smaller than a typical brief plus last round, so
/// rollovers could not shrink the context and would only churn the cache.
pub const MIN_SUBAGENT_ROLLOVER_TOKENS: u64 = 20_000;

/// Prompt-cache keep-warm policy (`cache_keepwarm`, task #444). See
/// docs/specs/cache-ttl-spec.md §7.
///
/// - `Off` (default): never ping.
/// - `Auto`: ping while the measured ping spend of the current idle period
///   stays below the cost of the cache rewrite it prevents (ski-rental rule:
///   at most 2× the optimal spend, at most ~one rewrite on an abandoned
///   session), and for at most `cache_keepwarm_auto_max_hours` (default
///   [`DEFAULT_CACHE_KEEPWARM_AUTO_MAX_HOURS`]) after the session went idle.
/// - `Hours(n)`: the same bound, and additionally stop `n` hours after the
///   session went idle — whichever comes first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheKeepWarm {
    #[default]
    Off,
    Auto,
    Hours(u32),
}

impl CacheKeepWarm {
    /// Parse `off` | `auto` | `<N>h` (case-insensitive; `0h` = off).
    /// `None` for anything else, including hours above
    /// [`MAX_CACHE_KEEPWARM_HOURS`].
    pub fn parse(val: &str) -> Option<CacheKeepWarm> {
        let v = val.trim().to_ascii_lowercase();
        match v.as_str() {
            "off" | "false" | "no" | "0" => return Some(CacheKeepWarm::Off),
            "auto" | "on" => return Some(CacheKeepWarm::Auto),
            _ => {}
        }
        let digits = v.strip_suffix('h')?;
        match digits.parse::<u32>().ok()? {
            0 => Some(CacheKeepWarm::Off),
            h if h <= MAX_CACHE_KEEPWARM_HOURS => Some(CacheKeepWarm::Hours(h)),
            _ => None,
        }
    }

    pub fn is_off(self) -> bool {
        self == CacheKeepWarm::Off
    }
}

impl std::fmt::Display for CacheKeepWarm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CacheKeepWarm::Off => f.write_str("off"),
            CacheKeepWarm::Auto => f.write_str("auto"),
            CacheKeepWarm::Hours(h) => write!(f, "{h}h"),
        }
    }
}

/// Who may drive a daemon session with several clients attached
/// (`client_input`; spec `multi-client-sessions.md` §6). Every client is
/// the daemon's own user.
///
/// - `Shared`: every drive client (not `--observe`) may send input; the
///   one that did last is the *active* client.
/// - `Exclusive` (default): only the input owner may; the others are
///   read-only until they `/takeover` (the behaviour before
///   `client_input`). PARITY: 790d R6, until JR answers E4 (upstream:
///   `Shared` is the default).
///
/// On the wire a missing value means `Exclusive`: an older daemon enforces
/// the owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientInput {
    Shared,
    // PARITY: 790d R6, until JR answers E4 (upstream: `#[default]` on Shared).
    #[default]
    Exclusive,
}

impl ClientInput {
    /// `shared` | `exclusive` (case-insensitive).
    pub fn parse(val: &str) -> Option<ClientInput> {
        match val.trim().to_ascii_lowercase().as_str() {
            "shared" => Some(ClientInput::Shared),
            "exclusive" => Some(ClientInput::Exclusive),
            _ => None,
        }
    }

    /// The wire's default: what a snapshot from an older daemon means.
    pub fn wire_default() -> ClientInput {
        ClientInput::Exclusive
    }
}

/// What an Anthropic thinking block carries back (`thinking_display`), sent
/// as the request's `thinking.display` on the foreground session's requests.
///
/// - `Summarized` (default): the model's thinking summary streams into the
///   block. The API holds the answer back until that summary is written,
///   then flushes the text it generated meanwhile in one burst.
/// - `Omitted`: the block's `thinking` text is empty; its `signature` still
///   carries the full reasoning, so history round-trips it unchanged. The
///   answer starts streaming sooner and evenly (measured on Opus 5.5:
///   first text 1.8–2.1 s vs 2.9–4.8 s at `high`, 7.9–9.0 s vs 11–12 s at
///   `xhigh`), and the TUI shows only a timed `thought` header.
///
/// Workers and internal requests (compaction) always send `Summarized`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThinkingDisplay {
    #[default]
    Summarized,
    Omitted,
}

impl ThinkingDisplay {
    /// `summarized` | `omitted` (case-insensitive).
    pub fn parse(val: &str) -> Option<ThinkingDisplay> {
        match val.trim().to_ascii_lowercase().as_str() {
            "summarized" => Some(ThinkingDisplay::Summarized),
            "omitted" => Some(ThinkingDisplay::Omitted),
            _ => None,
        }
    }

    /// The wire value of `thinking.display`.
    pub fn as_str(self) -> &'static str {
        match self {
            ThinkingDisplay::Summarized => "summarized",
            ThinkingDisplay::Omitted => "omitted",
        }
    }
}

impl std::fmt::Display for ThinkingDisplay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::fmt::Display for ClientInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ClientInput::Shared => "shared",
            ClientInput::Exclusive => "exclusive",
        })
    }
}

/// Prompt-cache TTL strategy for Anthropic requests.
///
/// Controls the `cache_control` value emitted at every cache marker site:
/// - `FiveMinutes` (default): bare `{"type": "ephemeral"}` — byte-identical
///   to historical payloads; the default path can never invalidate existing
///   cached prefixes.
/// - `OneHour`: `{"type": "ephemeral", "ttl": "1h"}` on all markers.
/// - `Hybrid`: 1h on the stable prefix (tools + system, written rarely),
///   bare 5m on the message-tail marker (written every turn).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheTtl {
    #[default]
    FiveMinutes,
    OneHour,
    Hybrid,
}

impl CacheTtl {
    /// Parse a config value (case-insensitive). Returns `None` for unknown
    /// values so the caller can warn and fall back to the default.
    pub fn parse(val: &str) -> Option<CacheTtl> {
        match val.to_ascii_lowercase().as_str() {
            "5m" | "5min" | "default" => Some(CacheTtl::FiveMinutes),
            "1h" | "60m" | "1hr" => Some(CacheTtl::OneHour),
            "hybrid" => Some(CacheTtl::Hybrid),
            _ => None,
        }
    }

    /// Canonical config spelling (`5m` | `1h` | `hybrid`).
    pub fn as_str(self) -> &'static str {
        match self {
            CacheTtl::FiveMinutes => "5m",
            CacheTtl::OneHour => "1h",
            CacheTtl::Hybrid => "hybrid",
        }
    }
}

impl std::fmt::Display for CacheTtl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The `cache_ttl` config value: either a fixed [`CacheTtl`] or `auto`.
///
/// `Auto` (default) resolves per credential *kind* at request time:
/// - Anthropic OAuth / subscription credentials → [`CacheTtl::OneHour`]
///   (what Claude Code uses on subscriptions; a subscription pays no
///   per-token premium for 1h writes, and idle gaps over five minutes
///   otherwise lose the conversation cache every turn).
/// - Anthropic API key → [`CacheTtl::FiveMinutes`] (1h writes bill at 2×
///   input versus 1.25× for 5m, so API-key users keep the cheaper write).
/// - Other providers ignore `cache_ttl` entirely.
///
/// Resolution depends only on the credential kind, never on which account
/// or token is in use, so account failover between two subscription seats
/// can never flip the TTL (a TTL change rewrites the cache).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheTtlSetting {
    #[default]
    Auto,
    Fixed(CacheTtl),
}

impl CacheTtlSetting {
    /// `auto` or any [`CacheTtl::parse`] spelling (case-insensitive).
    /// `None` for unknown values.
    pub fn parse(val: &str) -> Option<CacheTtlSetting> {
        let v = val.trim();
        if v.eq_ignore_ascii_case("auto") {
            return Some(CacheTtlSetting::Auto);
        }
        CacheTtl::parse(v).map(CacheTtlSetting::Fixed)
    }

    /// Canonical config spelling (`auto` | `5m` | `1h` | `hybrid`).
    pub fn as_str(self) -> &'static str {
        match self {
            CacheTtlSetting::Auto => "auto",
            CacheTtlSetting::Fixed(ttl) => ttl.as_str(),
        }
    }

    /// The wire policy for a request carried on Anthropic credentials of
    /// the given kind (`oauth` = subscription / OAuth access token, else an
    /// API key). Explicit values always win.
    pub fn resolve(self, oauth: bool) -> CacheTtl {
        match self {
            CacheTtlSetting::Fixed(ttl) => ttl,
            CacheTtlSetting::Auto if oauth => CacheTtl::OneHour,
            CacheTtlSetting::Auto => CacheTtl::FiveMinutes,
        }
    }

    /// [`Self::resolve`] keyed by the runtime's `auth_type` string. Only the
    /// exact `"oauth"` kind counts as a subscription credential.
    pub fn resolve_for_auth_type(self, auth_type: &str) -> CacheTtl {
        self.resolve(auth_type == "oauth")
    }
}

impl From<CacheTtl> for CacheTtlSetting {
    fn from(ttl: CacheTtl) -> Self {
        CacheTtlSetting::Fixed(ttl)
    }
}

impl std::fmt::Display for CacheTtlSetting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// gzip `Content-Encoding` for Anthropic request bodies
/// (`request_compression`). Every request re-uploads the whole
/// conversation, and the JSON compresses about 2x.
/// - `Auto` (default): bodies of 16 KiB or more sent straight to
///   api.anthropic.com; skipped behind an HTTPS proxy, with a custom CA
///   bundle or a custom base URL, where a middlebox or gateway may not
///   handle a compressed body.
/// - `On`: those too, for a proxy or gateway known to pass gzip request
///   bodies. A refused body is still resent uncompressed.
/// - `Off`: never.
///
/// `SYNAPS_GZIP_REQUEST_BODIES=0` turns it off whatever this says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RequestCompressionMode {
    #[default]
    Auto,
    On,
    Off,
}

impl RequestCompressionMode {
    /// `auto` | `on` | `off` (case-insensitive).
    pub fn parse(val: &str) -> Option<RequestCompressionMode> {
        match val.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(RequestCompressionMode::Auto),
            "on" => Some(RequestCompressionMode::On),
            "off" => Some(RequestCompressionMode::Off),
            _ => None,
        }
    }
}

impl std::fmt::Display for RequestCompressionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            RequestCompressionMode::Auto => "auto",
            RequestCompressionMode::On => "on",
            RequestCompressionMode::Off => "off",
        })
    }
}

/// Default gzip level for request bodies (`request_compression_level`):
/// fast enough to pay off up to roughly gigabit uplinks.
pub const DEFAULT_REQUEST_COMPRESSION_LEVEL: u32 = 1;
/// Highest gzip level. Levels 3-6 pay off on slow uplinks, where the extra
/// CPU time is tens of milliseconds and the upload saved is seconds; above
/// 6 the body barely shrinks further.
pub const MAX_REQUEST_COMPRESSION_LEVEL: u32 = 9;

/// Default for `tool_output_spill_bytes`: a tool result bigger than this is
/// stored whole in a spill file and model history gets a head + tail
/// preview pointing at it. 12 KiB is about 3k tokens. Below it the preview
/// (head + tail + pointer, ~7.3 KiB) saves too little to be worth a second
/// look when the model does need the middle; above it every result saves at
/// least ~4.5 KiB on every later request, and the 22-26 KB results that
/// were re-read hundreds of times shrink by ~70%.
pub const DEFAULT_TOOL_OUTPUT_SPILL_BYTES: usize = 12 * 1024;
/// Default for `tool_output_spill_head_bytes`: the start of a virtualised
/// result kept in history (the command echo, headers, the first errors).
pub const DEFAULT_TOOL_OUTPUT_SPILL_HEAD_BYTES: usize = 4 * 1024;
/// Default for `tool_output_spill_tail_bytes`: the end of a virtualised
/// result kept in history (exit status, summaries, the last errors).
pub const DEFAULT_TOOL_OUTPUT_SPILL_TAIL_BYTES: usize = 3 * 1024;

/// `tools.activation_confirm` — host policy for model-initiated
/// `activate_tools` (progressive tool disclosure).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ActivationConfirm {
    /// Authorize model-initiated activation without prompting (default).
    #[default]
    Auto,
    /// Ask the host: the y/n "Confirm tool activation" dialog; only an
    /// explicit y/yes authorizes (fail-closed).
    Prompt,
    /// Never authorize model-initiated activation; no prompt is raised.
    Deny,
}

impl ActivationConfirm {
    pub fn parse(val: &str) -> Option<Self> {
        match val.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "prompt" => Some(Self::Prompt),
            "deny" => Some(Self::Deny),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Prompt => "prompt",
            Self::Deny => "deny",
        }
    }
}

/// Animated theme-transition mode, parsed from `theme_transition`:
/// - `"on"` (default): cross-fade theme changes over the default 350 ms.
/// - `"off"`: instant snap — accessibility, and mercy on tmux-over-ssh.
/// - integer milliseconds: cross-fade over that duration (clamped to
///   0–2000 ms; `0` behaves like `off`).
///
/// Purely a TUI presentation knob: it changes *how* a new palette is
/// applied, never *which* palette wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemeTransitionMode {
    #[default]
    On,
    Off,
    Ms(u64),
}

impl ThemeTransitionMode {
    /// Default cross-fade duration for `On` (and the fallback when a live
    /// palette update carries no advisory duration of its own).
    pub const DEFAULT_MS: u64 = 350;
    /// Duration ceiling — both this knob and MXC wire `fade_ms` clamp here.
    pub const MAX_MS: u64 = 2000;

    /// Parse a config value (case-insensitive). Returns `None` for unknown
    /// values so the caller can warn and fall back to the default.
    pub fn parse(val: &str) -> Option<ThemeTransitionMode> {
        match val.to_ascii_lowercase().as_str() {
            "on" | "true" | "yes" => Some(ThemeTransitionMode::On),
            "off" | "false" | "no" => Some(ThemeTransitionMode::Off),
            other => other
                .parse::<u64>()
                .ok()
                .map(|ms| ThemeTransitionMode::Ms(ms.min(Self::MAX_MS))),
        }
    }

    /// Effective default duration in milliseconds; `0` means "snap".
    pub fn duration_ms(self) -> u64 {
        match self {
            ThemeTransitionMode::On => Self::DEFAULT_MS,
            ThemeTransitionMode::Off => 0,
            ThemeTransitionMode::Ms(ms) => ms.min(Self::MAX_MS),
        }
    }
}

/// Runtime event routing configuration parsed from `events.*` keys.
///
/// Controls how runtime events (from the `EventQueue`) are delivered in
/// server and RPC modes.
#[derive(Debug, Clone)]
pub struct EventsConfig {
    /// When `true` (default), the server/RPC/daemon session automatically
    /// triggers a model turn when runtime events arrive while idle.  Set
    /// `events.auto_turn = false` (or `0` / `no` / `off`) to opt out.
    /// Unrecognised values fail safe to `false` with a warning.
    /// The consecutive-turn cap (`auto_turn_cap`) still applies regardless.
    pub auto_turn: bool,
    /// Maximum number of consecutive auto-triggered model turns before the
    /// engine parks and waits for real user input.  Set via
    /// `events.auto_turn_cap = N`.  Default **5**.
    ///
    /// `0` (or `unlimited` / `inf` / `infinite`) disables the cap entirely —
    /// "to infinity and beyond".  Unparseable values warn and keep the default.
    pub auto_turn_cap: u32,
}

/// Default for `events.auto_turn_cap` — mirrors `engine::reactor::AUTO_TURN_CAP`.
pub const DEFAULT_AUTO_TURN_CAP: u32 = 5;

impl Default for EventsConfig {
    fn default() -> Self {
        Self {
            auto_turn: true,
            auto_turn_cap: DEFAULT_AUTO_TURN_CAP,
        }
    }
}

/// Parse a `events.auto_turn_cap` value.  Returns `None` on garbage.
///
/// Accepts a non-negative integer (`0` = unlimited) or one of the aliases
/// `unlimited` / `inf` / `infinite` / `infinity` / `none` (all → `0`).
pub fn parse_auto_turn_cap(val: &str) -> Option<u32> {
    let normalised = val.trim().to_lowercase();
    match normalised.as_str() {
        "unlimited" | "inf" | "infinite" | "infinity" | "none" => Some(0),
        n => n.parse::<u32>().ok(),
    }
}

/// Parse `events.*` configuration keys. `Err` carries the diagnostic the
/// caller prints (stderr, unchanged); the value side effects are the same
/// either way (an unrecognised `auto_turn` still fails safe to `false`).
fn parse_events_config_key(cfg: &mut EventsConfig, key: &str, val: &str) -> Result<(), String> {
    if key == "events.auto_turn" {
        let normalised = val.trim().to_lowercase();
        match normalised.as_str() {
            // Explicit true values.
            "true" | "1" | "yes" | "on" => cfg.auto_turn = true,
            // Explicit false values.
            "false" | "0" | "no" | "off" => cfg.auto_turn = false,
            // Unrecognised: fail safe to false and warn so the user knows.
            other => {
                cfg.auto_turn = false;
                return Err(format!(
                    "warning: config: unrecognised value for events.auto_turn = {:?}; \
                     expected true/false/yes/no/on/off/1/0 — defaulting to false",
                    other
                ));
            }
        }
    } else if key == "events.auto_turn_cap" {
        match parse_auto_turn_cap(val) {
            Some(cap) => cfg.auto_turn_cap = cap,
            None => {
                return Err(format!(
                    "warning: config: unrecognised value for events.auto_turn_cap = {:?}; \
                     expected a non-negative integer or unlimited/inf/infinite (0 = unlimited) \
                     — keeping default {}",
                    val.trim(),
                    DEFAULT_AUTO_TURN_CAP
                ));
            }
        }
    } // unknown events.* keys ignored
    Ok(())
}

/// `wait` tool settings, parsed from `wait.*` keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitConfig {
    /// The longest `timeout_secs` one inline wait may ask for
    /// (`wait.max_inline_secs`, 1 to 86400). An operational bound on one
    /// blocking tool call, not a spend cap.
    pub max_inline_secs: u64,
}

/// Default for `wait.max_inline_secs`.
pub const DEFAULT_WAIT_MAX_INLINE_SECS: u64 = 3600;
/// Largest accepted `wait.max_inline_secs` (one day).
pub const MAX_WAIT_MAX_INLINE_SECS: u64 = 86_400;

impl Default for WaitConfig {
    fn default() -> Self {
        Self {
            max_inline_secs: DEFAULT_WAIT_MAX_INLINE_SECS,
        }
    }
}

/// Parse `wait.*` keys. `Err` carries the diagnostic; the value is kept.
fn parse_wait_config_key(cfg: &mut WaitConfig, key: &str, val: &str) -> Result<(), String> {
    if key == "wait.max_inline_secs" {
        match val.trim().parse::<u64>() {
            Ok(secs) if (1..=MAX_WAIT_MAX_INLINE_SECS).contains(&secs) => {
                cfg.max_inline_secs = secs;
            }
            _ => {
                return Err(format!(
                    "warning: config: wait.max_inline_secs = {:?} is not a whole number of \
                     seconds from 1 to {MAX_WAIT_MAX_INLINE_SECS} — keeping {}",
                    val.trim(),
                    cfg.max_inline_secs
                ));
            }
        }
    } // unknown wait.* keys ignored
    Ok(())
}

/// Whether polling workers on large models are refused (`subagent.watch.admission`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchAdmission {
    /// Refuse a polling task on a model outside `small_models` (default).
    Refuse,
    /// The operator's kill switch: admission allows every dispatch.
    Off,
}

/// Small-watcher admission and the watch-worker lease, from
/// `subagent.watch.*`. All operational bounds, none a spend cap: the cost
/// lease is unset unless the operator sets it.
#[derive(Debug, Clone, PartialEq)]
pub struct WatchConfig {
    /// Models a polling worker may run on, as a watch worker under the
    /// lease (`subagent.watch.small_models`, comma-separated).
    pub small_models: Vec<String>,
    pub admission: WatchAdmission,
    /// A watch worker's wall-clock lease, across resumes (`lease_secs`).
    pub lease_secs: u64,
    /// A watch worker's provider rounds, across resumes (`lease_rounds`).
    pub lease_rounds: u32,
    /// A watch worker's cost lease in USD (`lease_cost_usd`); unset by default.
    pub lease_cost_usd: Option<f64>,
    /// The advisor's budget per ambiguous dispatch (`advice_timeout_ms`;
    /// 0 asks no advisor).
    pub advice_timeout_ms: u64,
    /// Advice below this confidence allows the dispatch (`advice_min_confidence`).
    pub advice_min_confidence: f64,
}

/// The small model a refusal suggests by default.
pub const DEFAULT_WATCH_SMALL_MODEL: &str = "muse/muse-wonder-1.0";
pub const DEFAULT_WATCH_LEASE_SECS: u64 = 7200;
pub const DEFAULT_WATCH_LEASE_ROUNDS: u32 = 40;
pub const DEFAULT_WATCH_ADVICE_TIMEOUT_MS: u64 = 1000;
pub const DEFAULT_WATCH_ADVICE_MIN_CONFIDENCE: f64 = 0.95;

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            small_models: vec![DEFAULT_WATCH_SMALL_MODEL.to_owned()],
            admission: WatchAdmission::Refuse,
            lease_secs: DEFAULT_WATCH_LEASE_SECS,
            lease_rounds: DEFAULT_WATCH_LEASE_ROUNDS,
            lease_cost_usd: None,
            advice_timeout_ms: DEFAULT_WATCH_ADVICE_TIMEOUT_MS,
            advice_min_confidence: DEFAULT_WATCH_ADVICE_MIN_CONFIDENCE,
        }
    }
}

/// Parse `subagent.watch.*` keys. `Err` carries the diagnostic; the value
/// is kept.
fn parse_watch_config_key(cfg: &mut WatchConfig, key: &str, val: &str) -> Result<(), String> {
    let v = val.trim();
    let bad = |what: &str, kept: String| {
        Err(format!(
            "warning: config: {key} = {v:?} is not {what} — keeping {kept}"
        ))
    };
    match key {
        "subagent.watch.small_models" => {
            cfg.small_models = v
                .split(',')
                .map(str::trim)
                .filter(|m| !m.is_empty())
                .map(str::to_owned)
                .collect();
        }
        "subagent.watch.admission" => match v {
            "refuse" => cfg.admission = WatchAdmission::Refuse,
            "off" => cfg.admission = WatchAdmission::Off,
            _ => {
                return bad(
                    "refuse or off",
                    format!("{:?}", cfg.admission).to_lowercase(),
                )
            }
        },
        "subagent.watch.lease_secs" => match v.parse::<u64>() {
            Ok(n) if (60..=86_400).contains(&n) => cfg.lease_secs = n,
            _ => {
                return bad(
                    "a whole number of seconds from 60 to 86400",
                    cfg.lease_secs.to_string(),
                )
            }
        },
        "subagent.watch.lease_rounds" => match v.parse::<u32>() {
            Ok(n) if (1..=10_000).contains(&n) => cfg.lease_rounds = n,
            _ => {
                return bad(
                    "a whole number from 1 to 10000",
                    cfg.lease_rounds.to_string(),
                )
            }
        },
        "subagent.watch.lease_cost_usd" => match v {
            "" | "none" | "unset" => cfg.lease_cost_usd = None,
            _ => match v.parse::<f64>() {
                Ok(usd) if usd.is_finite() && usd > 0.0 => cfg.lease_cost_usd = Some(usd),
                _ => {
                    return bad(
                        "a positive amount in USD, or none",
                        format!("{:?}", cfg.lease_cost_usd),
                    )
                }
            },
        },
        "subagent.watch.advice_timeout_ms" => match v.parse::<u64>() {
            Ok(n) if n <= 10_000 => cfg.advice_timeout_ms = n,
            _ => {
                return bad(
                    "a whole number of milliseconds from 0 to 10000",
                    cfg.advice_timeout_ms.to_string(),
                )
            }
        },
        "subagent.watch.advice_min_confidence" => match v.parse::<f64>() {
            Ok(c) if c > 0.0 && c <= 1.0 => cfg.advice_min_confidence = c,
            _ => {
                return bad(
                    "a number above 0 and at most 1",
                    cfg.advice_min_confidence.to_string(),
                )
            }
        },
        _ => {} // unknown subagent.watch.* keys ignored, as wait.* does
    }
    Ok(())
}

/// A configured per-turn count limit: a number, or `unlimited`.
///
/// Used for `turn_budget.<role>.max_provider_rounds`, `max_tool_calls` and
/// `max_accumulated_tool_result_bytes`. `Max(0)` means "none allowed" (the
/// 0.10.2 meaning of `0`); the parser still accepts `0` for one release, with
/// a deprecation warning, and will reject it later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnLimit<T> {
    Max(T),
    Unlimited,
}

impl<T> TurnLimit<T> {
    /// The limit, or `None` for no limit.
    pub fn limit(self) -> Option<T> {
        match self {
            TurnLimit::Max(v) => Some(v),
            TurnLimit::Unlimited => None,
        }
    }

    /// Convert the limit value, keeping `Unlimited`.
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> TurnLimit<U> {
        match self {
            TurnLimit::Max(v) => TurnLimit::Max(f(v)),
            TurnLimit::Unlimited => TurnLimit::Unlimited,
        }
    }
}

/// Typed per-role turn-budget overrides (Task 23, spec §8.1). Every field
/// is optional: unset fields keep the role's compiled default. Values are
/// parsed from `turn_budget.<role>.<field>` config keys.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TurnBudgetOverrides {
    /// Provider rounds per turn: N or `unlimited`. Unset = the role default
    /// (foreground unlimited, autonomous 24, worker 64 per allowance). `0` =
    /// none allowed (deprecated).
    pub max_provider_rounds: Option<TurnLimit<u32>>,
    /// Tool calls per turn: N or `unlimited`. Unset = the role default
    /// (autonomous 96, otherwise unlimited). `0` = none allowed (deprecated).
    pub max_tool_calls: Option<TurnLimit<u32>>,
    /// Wall-clock limit for one turn, in seconds. Unset = no limit (the
    /// default for every role); `0` also means no limit.
    pub max_elapsed_secs: Option<u64>,
    /// Accumulated tool-result bytes per turn: N or `unlimited`. Unset = the
    /// role default (autonomous 8 MiB, otherwise unlimited). `0` = none
    /// allowed (deprecated).
    pub max_accumulated_tool_result_bytes: Option<TurnLimit<usize>>,
    pub max_context_tokens: Option<u64>,
    pub max_cost_usd: Option<f64>,
    /// Bounded auto-renewals of the provider-round allowance after it is
    /// exhausted mid-turn (spec §8.1). Each renewal resets the round counter;
    /// round renewals never reset wall-clock (durable context successors do). `0`
    /// disables graceful continuation (hard stop at the first exhaustion).
    pub max_round_renewals: Option<u32>,
}

/// Per-role turn budgets (foreground, autonomous/watcher, worker).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TurnBudgetsConfig {
    pub foreground: TurnBudgetOverrides,
    pub autonomous: TurnBudgetOverrides,
    /// Turns a session starts by itself because events arrived.
    pub event: TurnBudgetOverrides,
    pub worker: TurnBudgetOverrides,
}

/// Parse `turn_budget.<role>.<field>` keys. Invalid values warn and keep
/// the default (parity with shell.* parsing). `Err` carries the stderr
/// diagnostic. `Ok(Some(warning))`: applied, with a deprecation warning for
/// `config.warnings` (shown at TUI startup, in watcher logs and on stderr
/// for headless hosts).
fn parse_turn_budget_config_key(
    budgets: &mut TurnBudgetsConfig,
    key: &str,
    val: &str,
) -> Result<Option<String>, String> {
    let Some(rest) = key.strip_prefix("turn_budget.") else {
        return Ok(None);
    };
    let Some((role, field)) = rest.split_once('.') else {
        return Err(format!(
            "Warning: invalid turn_budget key '{key}' (expected turn_budget.<role>.<field>)"
        ));
    };
    let overrides = match role {
        "foreground" => &mut budgets.foreground,
        "autonomous" => &mut budgets.autonomous,
        "event" => &mut budgets.event,
        "worker" => &mut budgets.worker,
        other => {
            return Err(format!(
                "Warning: unknown turn_budget role '{other}' (expected foreground|autonomous|event|worker)"
            ));
        }
    };
    macro_rules! set {
        ($slot:expr, $ty:ty) => {
            match val.parse::<$ty>() {
                Ok(parsed) => $slot = Some(parsed),
                Err(_) => {
                    return Err(format!(
                        "Warning: invalid value for {key}: '{val}', using default"
                    ))
                }
            }
        };
    }
    // Count limits: a number or `unlimited`. `0` keeps its 0.10.2 meaning
    // ("none allowed") for one release, with a deprecation warning.
    let mut deprecation = None;
    macro_rules! set_limit {
        ($slot:expr, $ty:ty) => {
            if val.trim_matches('"').eq_ignore_ascii_case("unlimited") {
                $slot = Some(TurnLimit::Unlimited);
            } else {
                match val.parse::<$ty>() {
                    Ok(0) => {
                        $slot = Some(TurnLimit::Max(0));
                        deprecation = Some(format!(
                            "{key} = 0 means \"none allowed\". `0` will be rejected in a \
                             future release; use `unlimited` for no limit, or a positive number"
                        ));
                    }
                    Ok(parsed) => $slot = Some(TurnLimit::Max(parsed)),
                    Err(_) => {
                        return Err(format!(
                            "Warning: invalid value for {key}: '{val}' (expected a positive \
                             number or unlimited), using default"
                        ))
                    }
                }
            }
        };
    }
    match field {
        "max_provider_rounds" => set_limit!(overrides.max_provider_rounds, u32),
        "max_tool_calls" => set_limit!(overrides.max_tool_calls, u32),
        "max_elapsed_secs" => set!(overrides.max_elapsed_secs, u64),
        "max_accumulated_tool_result_bytes" => {
            set_limit!(overrides.max_accumulated_tool_result_bytes, usize)
        }
        "max_context_tokens" => set!(overrides.max_context_tokens, u64),
        "max_cost_usd" => set!(overrides.max_cost_usd, f64),
        "max_round_renewals" => set!(overrides.max_round_renewals, u32),
        other => {
            return Err(format!("Warning: unknown turn_budget field '{other}'"))
        }
    }
    Ok(deprecation)
}

/// Continuous-memory settings surface (Task A2, spec §12). Every field is
/// fail-closed: unknown or invalid values warn and keep the compiled default.
///
/// This struct contains continuous-memory policy; backend selection lives in
/// the separate `MemoryBackendConfig`.
/// Secret handling, retention, and project-scope rules are deliberately NOT
/// fields here — they are not configurable at all (spec §12: "Secret,
/// retention, and project rules are not configurable to fail open").
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryConfig {
    /// Durable default memory mode. "off" unless the operator explicitly
    /// consents via `memory.default_mode_confirmed = true` in the same
    /// config parse pass (spec §12 operator-consent rule). Known values:
    /// off | recall_once | recall_each_prompt | capture_only |
    /// capture_and_recall (spec §6.1).
    pub default_mode: String,
    /// Operator-consent latch for `default_mode` (spec §12). Must be set to
    /// `true` in the same parse pass for a non-"off" `default_mode` to take
    /// effect; otherwise `default_mode` reverts to "off" with a warning.
    pub default_mode_confirmed: bool,
    /// Max records surfaced per recall (spec §10.3 budget).
    pub recall_max_records: u32,
    /// Max tokens surfaced per recall (spec §10.3 budget).
    pub recall_max_tokens: u32,
    /// Recall time budget in milliseconds (spec §16).
    pub recall_timeout_ms: u64,
    /// Tool-result capture policy: off | summary_only (spec §11).
    pub capture_tools: String,
    /// Capture assistant turns (spec §8.2).
    pub capture_assistant: bool,
    /// Capture user turns (spec §8.2).
    pub capture_user: bool,
    /// Background consolidation: off | on (spec §11.3, explicit opt-in).
    pub auto_consolidate: String,
    /// Local embedding models: off | on (spec §11.2, never implicit).
    pub local_embeddings: String,
    /// GLiNER entity extraction: off | on (spec §11.2, never implicit).
    pub gliner: String,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            default_mode: "off".to_string(),
            default_mode_confirmed: false,
            recall_max_records: 8,
            recall_max_tokens: 4096,
            recall_timeout_ms: 150,
            capture_tools: "summary_only".to_string(),
            capture_assistant: true,
            capture_user: true,
            auto_consolidate: "off".to_string(),
            local_embeddings: "off".to_string(),
            gliner: "off".to_string(),
        }
    }
}

/// Memory mode strings accepted by `memory.default_mode` (spec §6.1,
/// snake_case of `MemoryContextMode`). Anything else fails closed to the
/// current value — `default_mode` can never silently become a non-off mode
/// through an unrecognised string.
const KNOWN_MEMORY_MODES: &[&str] = &[
    "off",
    "recall_once",
    "recall_each_prompt",
    "capture_only",
    "capture_and_recall",
];

/// Parse `memory.<field>` keys (Task A2, spec §12). Invalid values warn and
/// keep the default (parity with turn_budget.* parsing) — never fail open.
/// `Err` carries the diagnostic the caller prints.
fn parse_memory_config_key(memory: &mut MemoryConfig, key: &str, val: &str) -> Result<(), String> {
    let Some(field) = key.strip_prefix("memory.") else {
        return Ok(());
    };
    /// Strict boolean: recognised forms parse; anything else warns and
    /// keeps the current (default) value.
    fn parse_bool(slot: &mut bool, key: &str, val: &str) -> Result<(), String> {
        match val {
            "true" | "1" | "yes" | "on" => *slot = true,
            "false" | "0" | "no" | "off" => *slot = false,
            other => {
                return Err(format!(
                    "Warning: invalid value for {key}: '{other}', expected true/false — using default"
                ))
            }
        }
        Ok(())
    }
    /// Closed string-enum: value must be in `allowed`, else warn and keep
    /// the current (default) value.
    fn parse_enum(slot: &mut String, key: &str, val: &str, allowed: &[&str]) -> Result<(), String> {
        if allowed.contains(&val) {
            *slot = val.to_string();
            Ok(())
        } else {
            Err(format!(
                "Warning: invalid value for {key}: '{val}', expected one of {} — using default",
                allowed.join("|")
            ))
        }
    }
    macro_rules! set_num {
        ($slot:expr, $ty:ty) => {
            match val.parse::<$ty>() {
                Ok(parsed) => {
                    $slot = parsed;
                    Ok(())
                }
                Err(_) => Err(format!("Warning: invalid value for {key}: '{val}', using default")),
            }
        };
    }
    match field {
        "default_mode" => parse_enum(&mut memory.default_mode, key, val, KNOWN_MEMORY_MODES),
        "default_mode_confirmed" => parse_bool(&mut memory.default_mode_confirmed, key, val),
        "recall_max_records" => set_num!(memory.recall_max_records, u32),
        "recall_max_tokens" => set_num!(memory.recall_max_tokens, u32),
        "recall_timeout_ms" => set_num!(memory.recall_timeout_ms, u64),
        "capture_tools" => parse_enum(
            &mut memory.capture_tools,
            key,
            val,
            &["off", "summary_only"],
        ),
        "capture_assistant" => parse_bool(&mut memory.capture_assistant, key, val),
        "capture_user" => parse_bool(&mut memory.capture_user, key, val),
        "auto_consolidate" => parse_enum(&mut memory.auto_consolidate, key, val, &["off", "on"]),
        "local_embeddings" => parse_enum(&mut memory.local_embeddings, key, val, &["off", "on"]),
        "gliner" => parse_enum(&mut memory.gliner, key, val, &["off", "on"]),
        other => Err(format!("Warning: unknown memory field '{other}'")),
    }
}

/// Host-owned note storage selection, independent of continuous-memory consent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MemoryBackendKind {
    #[default]
    Legacy,
    Axel,
    /// Invalid configuration must never select legacy storage implicitly.
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MemoryBackendConfig {
    pub kind: MemoryBackendKind,
    /// Explicit absolute executable path; never resolved through PATH.
    pub executable: Option<PathBuf>,
    /// Optional absolute shared Axel brain path; absent uses the private user default.
    pub brain: Option<PathBuf>,
    /// Explicit host opt-in to user-wide notes. Never grants capture/history consent.
    pub user_scope: bool,
}

/// Parsed configuration from the config file.
#[derive(Debug, Clone)]
pub struct SynapsConfig {
    pub model: Option<String>,
    pub thinking_budget: Option<u32>,
    /// Named reasoning level, parsed from the same `thinking = …` key.
    /// When `Some`, this is the authoritative level. When `None`, fall back
    /// to `thinking_budget` for legacy numeric-only values.
    pub thinking_level: Option<crate::core::reasoning::ReasoningLevel>,
    pub context_window: Option<u64>, // override auto-detected context window (tokens)
    pub context_management: ContextManagementConfig,
    /// First-turn / cold-start behaviour (`startup.*`).
    pub startup: StartupConfig,
    /// Daemon lifetime knobs (`daemon.*`); env vars still win.
    pub daemon: DaemonConfig,
    /// Subagent fan-out limits (`subagent.*`).
    pub subagent: SubagentLimits,
    pub compaction_model: Option<String>, // model used for /compact (default: claude-sonnet-4-6)
    /// `context_management.summary_model`: the model that writes the rolling
    /// summary at rollover. `None` = `auto` (the compaction model when its
    /// route has an enforceable output ceiling, otherwise the session's own
    /// model when its route has one, otherwise no summary).
    pub summary_model: Option<String>,
    /// `context_management.summary_model` was set to something that is not
    /// `auto` or a single model id. The rolling summary is then off (with a
    /// notice), never silently `auto`: a malformed attempt to pin a model
    /// must not send the summary to a model the user did not choose.
    pub summary_model_invalid: bool,
    /// Where compaction summarization runs (spec §9.4): remote provider or
    /// local-only (zero network construction).
    pub compaction_mode: crate::core::compaction::CompactionMode,
    /// Content classes excluded from remote compaction disclosure.
    pub compaction_exclude: Vec<crate::core::compaction::ContentClass>,
    pub max_tool_output: usize,  // default 30000
    /// Tool output virtualisation: a text tool result bigger than this many
    /// bytes is stored whole in a spill file (`<base>/spill/<session>/`) and
    /// model history gets a head + tail preview with a pointer to it. `0`
    /// turns it off: results go into history exactly as before (cut at
    /// `max_tool_output`). (`tool_output_spill_bytes`, default
    /// [`DEFAULT_TOOL_OUTPUT_SPILL_BYTES`].)
    pub tool_output_spill_bytes: usize,
    /// Bytes from the start of a virtualised result kept in history
    /// (`tool_output_spill_head_bytes`, default
    /// [`DEFAULT_TOOL_OUTPUT_SPILL_HEAD_BYTES`]).
    pub tool_output_spill_head_bytes: usize,
    /// Bytes from the end of a virtualised result kept in history
    /// (`tool_output_spill_tail_bytes`, default
    /// [`DEFAULT_TOOL_OUTPUT_SPILL_TAIL_BYTES`]).
    pub tool_output_spill_tail_bytes: usize,
    /// Read dedupe: a `read` whose result would be byte-identical to an
    /// earlier `read` of the same file/range still in history is replaced by
    /// a short stub naming that earlier call, and `write`/`edit` results say
    /// there is no need to re-read to verify. Off = legacy tool-result bytes.
    /// (`read_dedupe`, default on).
    pub read_dedupe: bool,
    pub bash_timeout: u64,       // default 30
    pub bash_max_timeout: u64,   // default 300
    pub subagent_timeout: u64,   // default 300
    /// `subagent_resume` of a worker whose prompt cache has expired replays
    /// a compact context instead of its whole transcript once the history is
    /// at least this many (estimated) tokens: a cold replay rewrites every
    /// token at the cache-write price. `0` disables compaction
    /// (`subagent_cold_resume_compact_tokens`, default
    /// [`DEFAULT_SUBAGENT_COLD_RESUME_COMPACT_TOKENS`]).
    pub subagent_cold_resume_compact_tokens: u64,
    /// Automatic context rollover for subagent workers
    /// (`subagent_context_management = auto | off`, default `auto`). Workers
    /// evict older rounds into their archive directory instead of growing
    /// until the window is full. Independent of the foreground
    /// `context_management.mode`.
    pub subagent_context_management: ContextManagementMode,
    /// Estimated footprint (tokens) at which a worker's context rolls over
    /// (`subagent_rollover_tokens`, at least [`MIN_SUBAGENT_ROLLOVER_TOKENS`],
    /// default [`DEFAULT_SUBAGENT_ROLLOVER_TOKENS`]). The window's own
    /// rollover band wins when it is lower; the successor target is derived
    /// like the foreground's (min(100k, 2/3 of pressure)).
    pub subagent_rollover_tokens: u64,
    /// Subagent workers run the host's `after_tool_call` output transforms
    /// (extensions holding `tools.transform_output`: compression,
    /// redaction) on their own tool results, as the foreground does. Off =
    /// worker tool results reach history untransformed, as before.
    /// (`subagent_transform_hooks`, default on).
    pub subagent_transform_hooks: bool,
    /// Worker default, independent of foreground `thinking`; per-call overrides win.
    pub subagent_thinking: crate::core::reasoning::ReasoningLevel,
    pub api_retries: u32,        // default 3
    pub refusal_retries: u32,    // default 2 — retries on stop_reason=refusal
    pub telemetry: String,       // off | basic | full (default off)
    /// Rotated generations kept for `api-log.jsonl` and
    /// `request-trace.jsonl` (`telemetry_log_generations`, 1–
    /// [`MAX_TELEMETRY_LOG_GENERATIONS`], default 1: just `<log>.1`).
    pub telemetry_log_generations: u32,
    pub cache_diagnostics: bool, // opt into cache-diagnosis beta (default false)
    /// Prompt-cache TTL strategy: "auto" (default: 1h on Anthropic OAuth /
    /// subscription credentials, 5m on an API key) | "5m" | "1h" | "hybrid".
    pub cache_ttl: CacheTtlSetting,
    /// Prompt-cache keep-warm while idle (`cache_keepwarm`): `off` (default)
    /// | `auto` | `<N>h`. See [`CacheKeepWarm`] and
    /// docs/specs/cache-ttl-spec.md §7.
    pub cache_keepwarm: CacheKeepWarm,
    /// Longest an `auto` keep-warm idle period may run, in hours
    /// (`cache_keepwarm_auto_max_hours`, 1–48, default
    /// [`DEFAULT_CACHE_KEEPWARM_AUTO_MAX_HOURS`]). An explicit `<N>h` mode
    /// is its own window and ignores this.
    pub cache_keepwarm_auto_max_hours: u32,
    /// Let keep-warm run for daemon sessions driven only by non-interactive
    /// clients (`cache_keepwarm_headless`, default `false`): `synaps chat`,
    /// rpc/server clients over the daemon, piped `synaps attach`. Off, only a
    /// session a TUI or interactive `attach` has seen qualifies.
    pub cache_keepwarm_headless: bool,
    /// Keep the prompt cache warm while a tool round runs
    /// (`cache_keepwarm_tools`, default `true`): once a round's tools have
    /// run for the TTL minus a safety margin (4.5 min on 5m, 55 min on 1h),
    /// re-send that round's request with a minimal output cap so the next
    /// request reads the prefix instead of rewriting it. Foreground and
    /// workers alike; independent of the idle `cache_keepwarm`.
    pub cache_keepwarm_tools: bool,
    /// gzip for Anthropic request bodies (`request_compression`): `auto`
    /// (default) | `on` | `off`. See [`RequestCompressionMode`].
    pub request_compression: RequestCompressionMode,
    /// gzip level 1–9 (`request_compression_level`, default
    /// [`DEFAULT_REQUEST_COMPRESSION_LEVEL`]). Higher shrinks more and costs
    /// more CPU: raise it on a slow uplink.
    pub request_compression_level: u32,
    /// Who may drive a daemon session with several clients attached
    /// (`client_input`): `shared` | `exclusive` (default; 790d R6). The daemon reads
    /// it at start, for every session it creates or restores. See
    /// [`ClientInput`].
    pub client_input: ClientInput,
    /// `thinking_display`: `summarized` (default) | `omitted`. The
    /// foreground session's `thinking.display`; see [`ThinkingDisplay`].
    pub thinking_display: ThinkingDisplay,
    /// Max TUI redraw rate in frames/sec — caps streaming redraws (e.g. 60,
    /// 144, 240). User input always redraws immediately regardless. Default
    /// 60. Range 1–1000. The frame budget is `1000 / max_fps` ms.
    pub max_fps: u32,
    /// Lines to scroll per mouse-wheel event. Different terminal emulators
    /// emit 1–9+ scroll events per physical notch; set this to compensate.
    /// Default: 3. Range 1–20.
    pub scroll_lines: Option<u16>,
    pub theme: Option<String>,
    /// Animated theme transitions: `"on"` (350 ms cross-fade, default),
    /// `"off"` (instant snap), or integer milliseconds (0–2000).
    pub theme_transition: ThemeTransitionMode,
    /// Whether the TUI paints its own opaque background. `false` preserves the terminal background.
    pub tui_background_opaque: bool,
    /// Whether the prompt shows a glow sweeping across it while a turn streams. Default off.
    pub tui_streaming_glow: bool,
    /// Whether the TUI reveals a burst of streamed text (a provider flushing
    /// a backlog at once) over a few frames instead of in one. Default on.
    pub tui_smooth_streaming: bool,
    /// Run the daemon, bash tool commands and PTY shells each in their own
    /// systemd scope (Linux, when a systemd user manager is reachable), so the
    /// out-of-memory killer can stop one runaway command instead of the whole
    /// terminal scope with the daemon in it. `process_scopes = off` disables;
    /// env `SYNAPS_PROCESS_SCOPES` overrides. Default on.
    pub process_scopes: bool,
    /// The systemd slice for every scope Synaps makes for a tool command
    /// (bash tool commands, PTY shells): `tool_scope_slice = synaps_tools.slice`
    /// passes `--slice=synaps_tools.slice` to `systemd-run`, so an operator
    /// can cap tool work on that slice apart from the desktop's
    /// `app.slice`. Unset or empty (the default) leaves systemd's placement
    /// as it is. Read with the config each spawn loads. Synaps sets no limits
    /// itself. Use a top-level (dash-free) name: `CPUWeight` only weighs a
    /// slice against its siblings (`docs/daemon-mode.md`).
    pub tool_scope_slice: Option<String>,
    /// The systemd slice for the daemon's own scope, applied when the daemon
    /// starts (a reload re-execs in the same scope). Separate from
    /// [`Self::tool_scope_slice`], so tools can be capped without the daemon.
    /// Unset or empty: no `--slice`.
    pub daemon_scope_slice: Option<String>,
    pub agent_name: Option<String>,
    pub identity: Option<String>,
    pub disabled_plugins: Vec<String>,
    pub favorite_models: Vec<String>,
    pub disabled_skills: Vec<String>,
    /// `skills.extra_root`: one more skill discovery root, walked after the
    /// project roots and before the home roots. One absolute path on one
    /// line (inner spaces are part of it, `~` is not expanded); any other
    /// value is ignored with a warning. See [`check_skills_extra_root`].
    pub skills_extra_root: Option<PathBuf>,
    /// Opt-in progressive tool disclosure (Task 18). When false, providers
    /// receive the existing full tool schema byte-for-byte. When true, each
    /// stream starts with the small essential local core plus discovery and
    /// authorization gateways; exact activations are added per session.
    pub progressive_tool_disclosure: bool,
    /// `capacity_wait` (default on): a turn whose automatic Anthropic account
    /// selection finds no capacity for a reason that ends by itself (every
    /// account at a usage limit that resets within 5 h 15 min, or capacity
    /// unproven for up to 30 min) waits in the turn and then carries on.
    /// `off` ends the turn at once with the selection's error, as before.
    pub capacity_wait: bool,
    /// `capacity_resume` (default on): a daemon session whose turn ended
    /// because no Anthropic account had capacity, while a limited account
    /// comes back at a known time (within 8 days), continues by itself ONCE
    /// at that time: one new turn asking it to carry on (a resumed turn that
    /// stops again arms again for a later reset, three times at most). Needs
    /// the daemon's crash record (`daemon.crash_restore`, default on): with
    /// it off nothing is armed. Independent of `daemon.reload_resume` and of
    /// `capacity_wait`. `off`: no automatic resume, an armed one never fires.
    pub capacity_resume: bool,
    /// `client_tools` (default on): a session's input owner may register
    /// client tools over the wire (docs/client-tools.md). `off`: every
    /// `register_client_tools` is refused `feature_off`; read at each one.
    pub client_tools: bool,
    /// Host confirmation policy for MODEL-INITIATED `activate_tools`
    /// (`tools.activation_confirm`). `Auto` (default) authorizes without
    /// prompting; `Prompt` asks the host (y/n confirm dialog); `Deny` never
    /// authorizes (locked-down hosts). `server.auto_approve_confirms = true`
    /// still authorizes regardless of this key.
    pub tools_activation_confirm: ActivationConfirm,
    /// `tools.deferred_loading` (default on). With progressive disclosure on
    /// the native Anthropic transport, every activatable tool is sent from the
    /// first request with `defer_loading: true` and `activate_tools` answers
    /// with `tool_reference` blocks, so an activation never changes the cached
    /// prompt prefix. `off` restores the per-activation tools-array change.
    pub tools_deferred_loading: bool,
    /// Opt-in session persistence strategy (Task 35, spec §9.8). `Json`
    /// (default) is the unchanged legacy full-rewrite path; `Journal` adds
    /// an append-only delta journal with periodic atomic snapshots. See
    /// docs/decisions/T35-session-journal-opt-in.md.
    pub session_persistence: crate::core::session_journal::SessionPersistence,
    /// Built-in tools to disable by runtime name (e.g. "bash", "ls"). Removed
    /// from the registry at boot so they're never offered to the model.
    pub disabled_tools: Vec<String>,
    /// Memory-only tool globs (`*`, `?`): results of matching extension or
    /// MCP tools are kept in RAM only, never in session records. Matched
    /// against the runtime name (`plugin:tool`, `ext__server__tool`) or the
    /// wire name (`plugin_tool`). See docs/memory-only-results.md.
    pub memory_only_tools: Vec<String>,
    pub shell: ShellConfig,
    pub server: ServerConfig,
    pub bridge: BridgeConfig,
    pub auth: AuthConfig,
    pub events: EventsConfig,
    /// `wait` tool bounds (`wait.*`).
    pub wait: WaitConfig,
    /// Small-watcher admission and the watch-worker lease (`subagent.watch.*`).
    pub watch: WatchConfig,
    pub provider_keys: BTreeMap<String, String>,
    pub keybinds: std::collections::HashMap<String, String>,
    /// Typed per-role turn budgets (Task 23, spec §8.1).
    pub turn_budgets: TurnBudgetsConfig,
    /// Continuous-memory settings surface (Task A2, spec §12).
    pub memory: MemoryConfig,
    /// Host-owned backend; selecting it does not enable recall or capture.
    pub memory_backend: MemoryBackendConfig,
    /// Non-fatal problems found while parsing the config file (unknown keys,
    /// unparseable values). Surfaced once at startup — never block boot.
    pub warnings: Vec<String>,
    /// The configured `(subagent.context_warn_tokens,
    /// subagent.resume_refuse_tokens)` when the parser clamped the warn line
    /// down to the refuse line. Subagent admission re-reads the config on
    /// every call and logs it from there.
    pub subagent_context_clamp: Option<(u64, u64)>,
}

impl Default for SynapsConfig {
    fn default() -> Self {
        Self {
            model: None,
            thinking_budget: None,
            thinking_level: None,
            context_window: None,
            context_management: ContextManagementConfig::config_default(),
            startup: StartupConfig::default(),
            daemon: DaemonConfig::default(),
            subagent: SubagentLimits::default(),
            compaction_model: None,
            summary_model: None,
            summary_model_invalid: false,
            compaction_mode: crate::core::compaction::CompactionMode::default(),
            compaction_exclude: Vec::new(),
            max_tool_output: 30000,
            tool_output_spill_bytes: DEFAULT_TOOL_OUTPUT_SPILL_BYTES,
            tool_output_spill_head_bytes: DEFAULT_TOOL_OUTPUT_SPILL_HEAD_BYTES,
            tool_output_spill_tail_bytes: DEFAULT_TOOL_OUTPUT_SPILL_TAIL_BYTES,
            read_dedupe: true,
            bash_timeout: 30,
            bash_max_timeout: 300,
            subagent_timeout: 300,
            subagent_cold_resume_compact_tokens: DEFAULT_SUBAGENT_COLD_RESUME_COMPACT_TOKENS,
            subagent_context_management: ContextManagementMode::Auto,
            subagent_rollover_tokens: DEFAULT_SUBAGENT_ROLLOVER_TOKENS,
            subagent_transform_hooks: true,
            subagent_thinking: crate::core::reasoning::ReasoningLevel::XHigh,
            api_retries: 3,
            refusal_retries: 2,
            telemetry: "off".to_string(),
            telemetry_log_generations: 1,
            cache_diagnostics: false,
            cache_ttl: CacheTtlSetting::default(),
            cache_keepwarm: CacheKeepWarm::Off,
            cache_keepwarm_auto_max_hours: DEFAULT_CACHE_KEEPWARM_AUTO_MAX_HOURS,
            cache_keepwarm_headless: false,
            cache_keepwarm_tools: true,
            request_compression: RequestCompressionMode::Auto,
            request_compression_level: DEFAULT_REQUEST_COMPRESSION_LEVEL,
            client_input: ClientInput::Exclusive, // PARITY: 790d R6, until JR answers E4 (upstream: Shared)
            thinking_display: ThinkingDisplay::Summarized,
            max_fps: 60,
            scroll_lines: None,
            theme: None,
            theme_transition: ThemeTransitionMode::default(),
            tui_background_opaque: true,
            tui_streaming_glow: false,
            tui_smooth_streaming: true,
            process_scopes: true,
            tool_scope_slice: None,
            daemon_scope_slice: None,
            agent_name: None,
            identity: None,
            disabled_plugins: Vec::new(),
            favorite_models: Vec::new(),
            disabled_skills: Vec::new(),
            skills_extra_root: None,
            progressive_tool_disclosure: false,
            capacity_wait: true,
            capacity_resume: true,
            client_tools: true,
            tools_activation_confirm: ActivationConfirm::default(),
            tools_deferred_loading: true,
            session_persistence: crate::core::session_journal::SessionPersistence::default(),
            disabled_tools: Vec::new(),
            memory_only_tools: Vec::new(),
            shell: ShellConfig::default(),
            server: ServerConfig::default(),
            bridge: BridgeConfig::default(),
            auth: AuthConfig::default(),
            events: EventsConfig::default(),
            wait: WaitConfig::default(),
            watch: WatchConfig::default(),
            provider_keys: BTreeMap::new(),
            keybinds: std::collections::HashMap::new(),
            turn_budgets: TurnBudgetsConfig::default(),
            memory: MemoryConfig::default(),
            memory_backend: MemoryBackendConfig::default(),
            warnings: Vec::new(),
            subagent_context_clamp: None,
        }
    }
}

/// Why `name` can't be a `tool_scope_slice` / `daemon_scope_slice` value, or
/// `None` if it can. A slice unit name: at most 255 bytes, ending in
/// `.slice`, only `[A-Za-z0-9:_.-]` (so no `/`), not starting with `-` and
/// not the root slice `-.slice`; as in systemd's own check, the part before
/// `.slice` is not empty, has no `--` and doesn't end in `-` (a `-` separates
/// the parent slices: `synaps-tools.slice` nests under `synaps.slice`). The
/// config file is not TOML, so a quoted value keeps its quotes and gets its
/// own hint. An empty value never reaches this check: the parser takes it
/// as "unset".
pub fn scope_slice_problem(name: &str) -> Option<&'static str> {
    if name.len() > 255 {
        return Some("longer than 255 bytes");
    }
    if name.starts_with(['"', '\'']) || name.ends_with(['"', '\'']) {
        return Some("write the name without quotes (the config file is not TOML)");
    }
    if !name.ends_with(".slice") {
        return Some("expected a slice unit name ending in .slice");
    }
    if !name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'_' | b'.' | b'-')) {
        return Some("only letters, digits, ':', '_', '.' and '-' are allowed");
    }
    if name == "-.slice" {
        return Some("the root slice -.slice is not allowed");
    }
    if name.starts_with('-') {
        return Some("must not start with '-'");
    }
    let stem = &name[..name.len() - ".slice".len()];
    if stem.is_empty() {
        return Some("needs a name before .slice");
    }
    if stem.contains("--") || stem.ends_with('-') {
        return Some("'-' separates parent slices: no '--' and no trailing '-'");
    }
    None
}

/// Upper bound for `telemetry_log_generations` (each generation of each log
/// is up to 50 MB).
pub const MAX_TELEMETRY_LOG_GENERATIONS: u32 = 100;

/// Known top-level config keys — used for unknown-key warnings + did-you-mean.
/// Public so the /settings coverage test can require a settings row (or an
/// explicit exclusion) for every key here.
pub const KNOWN_CONFIG_KEYS: &[&str] = &[
    "model",
    "thinking",
    "compaction_model",
    "compaction_mode",
    "compaction_exclude",
    "context_window",
    "context_management.mode",
    "context_management.pressure_tokens",
    "context_management.rollover_tokens",
    "context_management.reserve_tokens",
    "context_management.finish_rounds",
    "context_management.target_tokens",
    "context_management.summary_tokens",
    "context_management.summary_timeout_secs",
    "context_management.summary_model",
    "startup.quick_start",
    "startup.extensions_ready_timeout_secs",
    "daemon.reload_resume",
    "daemon.crash_restore",
    "daemon.idle_exit_secs",
    "daemon.prompt_abandon_secs",
    "daemon.parked_evict_secs",
    "daemon.umask",
    "max_tool_output",
    "tool_output_spill_bytes",
    "tool_output_spill_head_bytes",
    "tool_output_spill_tail_bytes",
    "read_dedupe",
    "bash_timeout",
    "bash_max_timeout",
    "subagent_timeout",
    "subagent_cold_resume_compact_tokens",
    "subagent_context_management",
    "subagent_rollover_tokens",
    "subagent_transform_hooks",
    "subagent_thinking",
    "subagent.max_concurrent",
    "subagent.max_total",
    "subagent.max_depth",
    "subagent.context_warn_tokens",
    "subagent.resume_refuse_tokens",
    "subagent.tool_hooks",
    "subagent.context_rollover",
    // Worker successor footprint (`<tokens>` | `auto`, default auto): a
    // worker's rollover target instead of `context_management.target_tokens`.
    "subagent.context_target_tokens",
    // Legacy flat spellings of `subagent.max_concurrent` / `subagent.max_total`.
    "subagent_max_concurrent",
    "subagent_max_total",
    "api_retries",
    "refusal_retries",
    "telemetry",
    "telemetry_log_generations",
    "cache_diagnostics",
    "cache_ttl",
    "cache_keepwarm",
    "cache_keepwarm_auto_max_hours",
    "cache_keepwarm_headless",
    "cache_keepwarm_tools",
    "request_compression",
    "request_compression_level",
    "client_input",
    "thinking_display",
    "max_fps",
    "scroll_lines",
    "theme",
    "theme_transition",
    "tui_background_opaque",
    "tui_streaming_glow",
    "tui_smooth_streaming",
    "process_scopes",
    "tool_scope_slice",
    "daemon_scope_slice",
    "agent_name",
    "identity",
    "disabled_plugins",
    "favorite_models",
    "disabled_skills",
    "skills.extra_root",
    "disabled_tools",
    "memory_only_tools",
    "progressive_tool_disclosure",
    "capacity_wait",
    "capacity_resume",
    "client_tools",
    "tools.activation_confirm",
    "tools.deferred_loading",
    "session_persistence",
    "sidecar_toggle_key",
];

/// Every key accepted by the namespaced family parsers that is NOT in
/// [`KNOWN_CONFIG_KEYS`] (`shell.*`, `events.*`, `turn_budget.*`,
/// `memory.*`, `server.*`, `bridge.*`, `auth.*`). Open-ended families whose
/// suffix is user-chosen are listed as `prefix.*`. Keep this in sync when a
/// parser learns a key: the /settings coverage test requires each entry to
/// have a settings row or an explicit exclusion.
pub const FAMILY_CONFIG_KEYS: &[&str] = &[
    // parse_shell_config_key
    "shell.max_sessions",
    "shell.idle_timeout",
    "shell.readiness_timeout_ms",
    "shell.max_readiness_timeout_ms",
    "shell.default_rows",
    "shell.default_cols",
    "shell.readiness_strategy",
    "shell.max_output",
    // parse_events_config_key
    "events.auto_turn",
    "events.auto_turn_cap",
    // parse_wait_config_key
    "wait.max_inline_secs",
    // parse_watch_config_key
    "subagent.watch.small_models",
    "subagent.watch.admission",
    "subagent.watch.lease_secs",
    "subagent.watch.lease_rounds",
    "subagent.watch.lease_cost_usd",
    "subagent.watch.advice_timeout_ms",
    "subagent.watch.advice_min_confidence",
    // parse_turn_budget_config_key: turn_budget.<role>.<field>
    "turn_budget.foreground.max_provider_rounds",
    "turn_budget.foreground.max_tool_calls",
    "turn_budget.foreground.max_elapsed_secs",
    "turn_budget.foreground.max_accumulated_tool_result_bytes",
    "turn_budget.foreground.max_context_tokens",
    "turn_budget.foreground.max_cost_usd",
    "turn_budget.foreground.max_round_renewals",
    "turn_budget.autonomous.max_provider_rounds",
    "turn_budget.autonomous.max_tool_calls",
    "turn_budget.autonomous.max_elapsed_secs",
    "turn_budget.autonomous.max_accumulated_tool_result_bytes",
    "turn_budget.autonomous.max_context_tokens",
    "turn_budget.autonomous.max_cost_usd",
    "turn_budget.autonomous.max_round_renewals",
    "turn_budget.event.max_provider_rounds",
    "turn_budget.event.max_tool_calls",
    "turn_budget.event.max_elapsed_secs",
    "turn_budget.event.max_accumulated_tool_result_bytes",
    "turn_budget.event.max_context_tokens",
    "turn_budget.event.max_cost_usd",
    "turn_budget.event.max_round_renewals",
    "turn_budget.worker.max_provider_rounds",
    "turn_budget.worker.max_tool_calls",
    "turn_budget.worker.max_elapsed_secs",
    "turn_budget.worker.max_accumulated_tool_result_bytes",
    "turn_budget.worker.max_context_tokens",
    "turn_budget.worker.max_cost_usd",
    "turn_budget.worker.max_round_renewals",
    // memory backend (top-level arms of apply_config_content)
    "memory.backend",
    "memory.user_scope",
    "memory.axel.executable",
    "memory.axel.brain",
    // parse_memory_config_key
    "memory.default_mode",
    "memory.default_mode_confirmed",
    "memory.recall_max_records",
    "memory.recall_max_tokens",
    "memory.recall_timeout_ms",
    "memory.capture_tools",
    "memory.capture_assistant",
    "memory.capture_user",
    "memory.auto_consolidate",
    "memory.local_embeddings",
    "memory.gliner",
    // parse_server_config_key
    "server.allowed_origins",
    "server.token",
    "server.auto_approve_confirms",
    "server.max_message_size",
    // parse_bridge_config_key
    "bridge.uds_path",
    "bridge.heartbeat_mirror",
    "bridge.heartbeat_timeout_ms",
    // parse_auth_config_key
    "auth.remote_endpoint",
    "auth.machine_token",
    // open-ended families
    "provider.*",
    "keybind.*",
];

/// Simple Levenshtein distance for did-you-mean suggestions.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Closest known key within edit distance 2, for typo suggestions.
fn did_you_mean(key: &str) -> Option<&'static str> {
    KNOWN_CONFIG_KEYS
        .iter()
        .map(|k| (*k, levenshtein(key, k)))
        .filter(|(_, d)| *d <= 2)
        .min_by_key(|(_, d)| *d)
        .map(|(k, _)| k)
}

// parse_thinking_budget replaced by ThinkingSpec::parse in apply_config_content.

fn parse_comma_list(val: &str) -> Vec<String> {
    val.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn write_comma_list_locked(key: &str, values: &[String]) -> std::io::Result<()> {
    write_config_value_locked(key, &values.join(", "))
}

/// Parse shell.* configuration keys and update the ShellConfig. `Err`
/// carries the diagnostic the caller prints.
fn parse_shell_config_key(shell_config: &mut ShellConfig, key: &str, val: &str) -> Result<(), String> {
    let invalid = || format!("Warning: invalid value for {key}: '{val}', using default");
    match key {
        "shell.max_sessions" => {
            shell_config.max_sessions = val.parse::<usize>().map_err(|_| invalid())?;
        }
        "shell.idle_timeout" => {
            let timeout = val.parse::<u64>().map_err(|_| invalid())?;
            shell_config.idle_timeout = std::time::Duration::from_secs(timeout);
        }
        "shell.readiness_timeout_ms" => {
            shell_config.readiness_timeout_ms = val.parse::<u64>().map_err(|_| invalid())?;
        }
        "shell.max_readiness_timeout_ms" => {
            shell_config.max_readiness_timeout_ms = val.parse::<u64>().map_err(|_| invalid())?;
        }
        "shell.default_rows" => {
            shell_config.default_rows = val.parse::<u16>().map_err(|_| invalid())?;
        }
        "shell.default_cols" => {
            shell_config.default_cols = val.parse::<u16>().map_err(|_| invalid())?;
        }
        "shell.readiness_strategy" => {
            let val_lower = val.to_lowercase();
            match val_lower.as_str() {
                "timeout" | "prompt" | "hybrid" => {
                    shell_config.readiness_strategy = val.to_string();
                }
                _ => return Err(invalid()),
            }
        }
        "shell.max_output" => {
            shell_config.max_output = val.parse::<usize>().map_err(|_| invalid())?;
        }
        _ => {
            // Unknown shell.* keys are preserved (not rejected)
        }
    }
    Ok(())
}

/// Parse server.* configuration keys and update the ServerConfig.
#[allow(clippy::collapsible_match)]
fn parse_server_config_key(server_config: &mut ServerConfig, key: &str, val: &str) {
    match key {
        "server.allowed_origins" => {
            server_config.allowed_origins = parse_comma_list(val);
        }
        "server.token" => {
            if !val.is_empty() {
                server_config.token = Some(val.to_string());
            }
        }
        "server.auto_approve_confirms" => {
            server_config.auto_approve_confirms = matches!(val, "true" | "1" | "yes");
        }
        "server.max_message_size" => {
            if let Ok(size) = val.parse::<usize>() {
                server_config.max_message_size = Some(size);
            } else {
                let msg = format!("Warning: invalid value for server.max_message_size: '{val}', ignored");
                if first_report(&msg) {
                    tracing::warn!(value = %val, "config: invalid server.max_message_size, ignored");
                    crate::eprintln_lossy!("{msg}");
                }
            }
        }
        _ => {
            // Unknown server.* keys preserved (not rejected)
        }
    }
}

/// Parse bridge.* configuration keys and update the BridgeConfig.
fn parse_bridge_config_key(bridge_config: &mut BridgeConfig, key: &str, val: &str) {
    match key {
        "bridge.uds_path" => {
            if val.is_empty() {
                bridge_config.uds_path = None;
            } else {
                bridge_config.uds_path = Some(PathBuf::from(val));
            }
        }
        "bridge.heartbeat_mirror" => {
            bridge_config.heartbeat_mirror = matches!(val, "true" | "1" | "yes");
        }
        "bridge.heartbeat_timeout_ms" => {
            if let Ok(ms) = val.parse::<u64>() {
                bridge_config.heartbeat_timeout_ms = ms;
            } else {
                let msg = format!("Warning: invalid value for bridge.heartbeat_timeout_ms: '{val}', using default");
                if first_report(&msg) {
                    tracing::warn!(value = %val, "config: invalid bridge.heartbeat_timeout_ms, using default");
                    crate::eprintln_lossy!("{msg}");
                }
            }
        }
        _ => {
            // Unknown bridge.* keys preserved (not rejected)
        }
    }
}

/// Warning for an `auth.*` key no parser reads (a typo would otherwise
/// silently leave traffic on the default account), with a did-you-mean.
fn unknown_auth_key_warning(key: &str) -> Option<String> {
    const EXACT: [&str; 2] = ["auth.remote_endpoint", "auth.machine_token"];
    const FAMILIES: [&str; 3] = ["auth.account", "auth.auto", "auth.order"];
    const PLACEHOLDERS: [&str; 3] = ["<provider>", "<key>", "<provider>"];
    if EXACT.contains(&key) {
        return None;
    }
    for (family, placeholder) in FAMILIES.iter().zip(PLACEHOLDERS) {
        match key.strip_prefix(family) {
            // `auth.account` or `auth.account.` (no provider): name the shape.
            Some("") | Some(".") => {
                return Some(format!(
                    "unknown key '{key}' (did you mean '{family}.{placeholder}'?)"
                ))
            }
            Some(rest) if rest.starts_with('.') && !rest[1..].trim().is_empty() => return None,
            _ => {}
        }
    }
    // A family typo keeps its suffix: `auth.acount.x` → `auth.account.x`.
    let (head, rest) = match key.match_indices('.').nth(1) {
        Some((i, _)) => key.split_at(i),
        None => (key, ""),
    };
    let suggestion = EXACT
        .iter()
        .map(|k| (levenshtein(key, k), k.to_string()))
        .chain(
            FAMILIES
                .iter()
                .filter(|_| !rest.is_empty())
                .map(|f| (levenshtein(head, f), format!("{f}{rest}"))),
        )
        .filter(|(d, _)| *d <= 2)
        .min_by_key(|(d, _)| *d)
        .map(|(_, k)| k);
    Some(match suggestion {
        Some(s) => format!("unknown key '{key}' (did you mean '{s}'?)"),
        None => format!("unknown key '{key}' — ignored"),
    })
}

/// Parse auth.* configuration keys and update the AuthConfig.
fn parse_auth_config_key(auth_config: &mut AuthConfig, key: &str, val: &str) {
    let v = val.trim();
    match key {
        "auth.remote_endpoint" => {
            auth_config.remote_endpoint = if v.is_empty() {
                None
            } else {
                Some(v.to_string())
            };
        }
        "auth.machine_token" => {
            auth_config.machine_token = if v.is_empty() {
                None
            } else {
                Some(v.to_string())
            };
        }
        _ => {
            if let Some(provider) = key.strip_prefix("auth.account.") {
                let provider = provider.trim();
                if !provider.is_empty() {
                    // Raw value; validated (fail closed) by AccountPolicy.
                    auth_config
                        .accounts
                        .insert(provider.to_string(), v.to_string());
                }
            }
            if let Some(key) = key.strip_prefix("auth.auto.").filter(|k| !k.trim().is_empty()) {
                auth_config.auto.insert(key.to_string(), v.to_string());
            }
            if let Some(provider) = key.strip_prefix("auth.order.") {
                let provider = provider.trim();
                if !provider.is_empty() {
                    // Raw value; unknown and duplicate labels are dropped
                    // where it is used (`accounts_api::effective_order`).
                    auth_config.order.insert(provider.to_string(), v.to_string());
                }
            }
            // Other unknown auth.* keys preserved (not rejected)
        }
    }
}

/// Parse config from a raw string — useful for tests and embedded harnesses.
/// Does NOT write to `PROVIDER_KEYS` or `IDENTITY` OnceLocks.
pub fn load_config_from_str(content: &str) -> SynapsConfig {
    let mut config = SynapsConfig::default();
    apply_config_content(&mut config, content);
    config
}

/// Every problem the parser reports for `content`: the `warnings` surfaced
/// at startup plus the stderr-only diagnostics of the `shell.*`, `events.*`,
/// `turn_budget.*` and `memory.*` families. Pure — prints nothing and
/// touches no process state.
pub fn config_content_problems(content: &str) -> Vec<String> {
    let mut config = SynapsConfig::default();
    let mut problems = Vec::new();
    apply_config_content_with(&mut config, content, &mut |diagnostic: String| problems.push(diagnostic));
    problems.extend(config.warnings);
    problems
}

/// Check a `key = value` edit before it is written (the /settings modal).
///
/// Runs the real parser over the config file as it would look after
/// [`write_config_value`] and returns `Err` with the parser's own messages
/// for any problem the edit introduces — invalid values, and cross-key
/// rules such as `context_management` threshold ordering, `subagent`
/// concurrency vs total, or `memory.default_mode` without operator consent.
/// Problems already present before the edit are not blamed on it.
pub fn validate_config_edit(key: &str, value: &str) -> Result<(), String> {
    if key.contains(['=', '\n', '\r'])
        || value.contains(['\n', '\r'])
    {
        return Err("value must be a single line".to_string());
    }
    // The file the edit lands in (profile-aware, like `write_config_value`),
    // computed without `resolve_write_path`'s directory creation.
    let target = get_active_config_dir().join("config");
    let existing = std::fs::read_to_string(&target).unwrap_or_default();
    let before = config_content_problems(&existing);
    let after = config_content_problems(&replace_config_line(&existing, key, value));
    let introduced: Vec<String> = after.into_iter().filter(|p| !before.contains(p)).collect();
    if introduced.is_empty() {
        Ok(())
    } else {
        Err(introduced.join("; "))
    }
}

/// Parse the config file at ~/.synaps-cli/config (or profile variant).
/// Returns default config if file doesn't exist or can't be read.
pub fn load_config() -> SynapsConfig {
    let path = resolve_read_path("config");
    let mut config = SynapsConfig::default();

    let Ok(content) = std::fs::read_to_string(&path) else {
        return config;
    };

    apply_config_content(&mut config, &content);

    // Publish provider keys to the process-wide cache for the API router.
    // First writer wins (OnceLock) — subsequent load_config calls are no-ops.
    let _ = PROVIDER_KEYS.set(config.provider_keys.clone());

    // Publish identity to the process-wide cache for API system prompt preamble.
    let identity_val = config
        .identity
        .clone()
        .unwrap_or_else(|| DEFAULT_IDENTITY.to_string());
    let _ = IDENTITY.set(identity_val);

    config
}

/// Apply key=value config lines from `content` into `config`.
/// Shared by `load_config` (file path) and `load_config_from_str` (test helper).
fn apply_config_content(config: &mut SynapsConfig, content: &str) {
    // The shell/events/turn_budget/memory families report to stderr (not
    // `config.warnings`), now also to the log. Never `eprintln!`: this runs
    // on every `load_config()`, in a daemon long after its starter (the
    // reader of its stderr pipe) may have gone, and that panic aborted the
    // daemon (a `turn_budget.event.*` line an older binary rejected).
    apply_config_content_once(config, content, &mut |diagnostic: &str| {
        tracing::warn!(%diagnostic, "config diagnostic");
        crate::eprintln_lossy!("{diagnostic}")
    });
}

/// [`apply_config_content_with`], reporting each distinct family diagnostic
/// to `report` only the first time this process sees it ([`first_report`]).
fn apply_config_content_once(
    config: &mut SynapsConfig,
    content: &str,
    report: &mut dyn FnMut(&str),
) {
    apply_config_content_with(config, content, &mut |diagnostic: String| {
        if first_report(&diagnostic) {
            report(&diagnostic);
        }
    });
}

/// Distinct config diagnostics remembered per process, at most this many.
const MAX_REMEMBERED_REPORTS: usize = 1024;

/// Whether this process has not reported `message` yet (and remember it).
///
/// `load_config()` runs on hot paths (session saves, auth selection, timers),
/// and one bad line would otherwise add the same lines to `synaps.log` and to
/// the never-rotated daemon stderr file on every call. A changed config gives
/// new messages, and those are reported. Past [`MAX_REMEMBERED_REPORTS`]
/// distinct messages, new ones are reported every time (not remembered).
fn first_report(message: &str) -> bool {
    use std::collections::HashSet;
    use std::hash::{BuildHasher, BuildHasherDefault, DefaultHasher};
    use std::sync::Mutex;
    static SEEN: OnceLock<Mutex<HashSet<u64>>> = OnceLock::new();
    let hash = BuildHasherDefault::<DefaultHasher>::default().hash_one(message);
    let mut seen = SEEN
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if seen.contains(&hash) {
        return false;
    }
    if seen.len() < MAX_REMEMBERED_REPORTS {
        seen.insert(hash);
    }
    true
}

/// [`apply_config_content`] with the stderr-only family diagnostics routed
/// to `family_diagnostic` instead, so [`config_content_problems`] can see
/// every value the parser rejects.
fn apply_config_content_with(
    config: &mut SynapsConfig,
    content: &str,
    family_diagnostic: &mut dyn FnMut(String),
) {
    let mut invalid_context_management = false;
    let mut invalid_memory_backend = false;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, val)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let val = val.trim();
        match key {
            "model" => config.model = Some(val.to_string()),
            "thinking" => {
                use crate::core::reasoning::ThinkingSpec;
                match ThinkingSpec::parse(val) {
                    Some(ThinkingSpec::Named(level)) => {
                        config.thinking_level = Some(level);
                        config.thinking_budget = level.to_legacy_budget();
                    }
                    Some(ThinkingSpec::Budget(budget)) => {
                        // Preserve exact legacy budgets; do not make the derived
                        // bucket authoritative over the user's token count.
                        config.thinking_level = None;
                        config.thinking_budget = Some(budget);
                    }
                    None => {
                        config.warnings.push(format!("thinking = {val} — expected off|adaptive|low|medium|high|xhigh|max|ultra|ultracode or a token count; thinking disabled"));
                    }
                }
            }
            "compaction_model" => config.compaction_model = Some(val.to_string()),
            "compaction_mode" => match val {
                "remote" => {
                    config.compaction_mode = crate::core::compaction::CompactionMode::Remote
                }
                "local" | "local_only" | "local-only" => {
                    config.compaction_mode = crate::core::compaction::CompactionMode::LocalOnly
                }
                _ => {
                    config.warnings.push(format!(
                        "compaction_mode = {val} — expected remote or local; ignored"
                    ));
                }
            },
            "compaction_exclude" => {
                let mut classes = Vec::new();
                for part in val.split(',').map(str::trim).filter(|p| !p.is_empty()) {
                    match crate::core::compaction::ContentClass::parse(part) {
                        Some(class) => {
                            if !classes.contains(&class) {
                                classes.push(class);
                            }
                        }
                        None => config.warnings.push(format!(
                            "compaction_exclude — unknown content class '{part}'; \
                             expected one of: user_text, assistant_text, thinking, \
                             tool_calls, tool_results, file_paths, event_data"
                        )),
                    }
                }
                config.compaction_exclude = classes;
            }
            // `auto` = model default (what /settings and `/context auto`
            // write): clears any override, no warning.
            "context_window" if val.eq_ignore_ascii_case("auto") => config.context_window = None,
            "context_window" => {
                let parsed = match val {
                    "200k" | "200K" => Some(200_000),
                    "1m" | "1M" => Some(1_000_000),
                    _ => val.parse::<u64>().ok(),
                };
                if parsed.is_none() {
                    config.warnings.push(format!(
                        "context_window = {val} — expected 200k, 1m, auto, or a token count; ignored"
                    ));
                }
                config.context_window = parsed;
            }
            "max_tool_output" => match val.parse::<usize>() {
                Ok(size) => config.max_tool_output = size,
                Err(_) => config.warnings.push(format!(
                    "max_tool_output = {val} — not a number; using {}",
                    config.max_tool_output
                )),
            },
            "tool_output_spill_bytes" => match val.parse::<usize>() {
                Ok(bytes) => config.tool_output_spill_bytes = bytes,
                Err(_) => config.warnings.push(format!(
                    "tool_output_spill_bytes = {val} — expected a byte count (0 turns virtualisation off); using {DEFAULT_TOOL_OUTPUT_SPILL_BYTES}"
                )),
            },
            "tool_output_spill_head_bytes" => match val.parse::<usize>() {
                Ok(bytes) => config.tool_output_spill_head_bytes = bytes,
                Err(_) => config.warnings.push(format!(
                    "tool_output_spill_head_bytes = {val} — expected a byte count; using {DEFAULT_TOOL_OUTPUT_SPILL_HEAD_BYTES}"
                )),
            },
            "tool_output_spill_tail_bytes" => match val.parse::<usize>() {
                Ok(bytes) => config.tool_output_spill_tail_bytes = bytes,
                Err(_) => config.warnings.push(format!(
                    "tool_output_spill_tail_bytes = {val} — expected a byte count; using {DEFAULT_TOOL_OUTPUT_SPILL_TAIL_BYTES}"
                )),
            },
            "bash_timeout" => match val.parse::<u64>() {
                Ok(t) if t >= 1 => config.bash_timeout = t,
                Ok(_) => config.warnings.push(format!(
                    "bash_timeout = {val} — below minimum (1s); using {}",
                    config.bash_timeout
                )),
                Err(_) => config.warnings.push(format!(
                    "bash_timeout = {val} — not a number; using {}",
                    config.bash_timeout
                )),
            },
            "bash_max_timeout" => {
                if let Ok(timeout) = val.parse::<u64>() {
                    config.bash_max_timeout = timeout;
                }
            }
            "subagent_thinking" => {
                match crate::core::reasoning::parse_subagent_thinking(val) {
                    Some(level) => config.subagent_thinking = level,
                    None => config.warnings.push(format!(
                        "subagent_thinking = {val} — expected low|medium|high|xhigh|max; keeping {}",
                        config.subagent_thinking
                    )),
                }
            }
            "subagent_timeout" => {
                if let Ok(timeout) = val.parse::<u64>() {
                    config.subagent_timeout = timeout;
                }
            }
            "subagent_cold_resume_compact_tokens" => match val.parse::<u64>() {
                Ok(tokens) => config.subagent_cold_resume_compact_tokens = tokens,
                Err(_) => config.warnings.push(format!(
                    "subagent_cold_resume_compact_tokens = {val} — expected a token count (0 disables); using {DEFAULT_SUBAGENT_COLD_RESUME_COMPACT_TOKENS}"
                )),
            },
            "subagent_context_management" => match ContextManagementMode::parse(val) {
                Some(mode) => config.subagent_context_management = mode,
                None => config.warnings.push(format!(
                    "subagent_context_management = {val} — expected auto or off; using auto"
                )),
            },
            "subagent_rollover_tokens" => match val.parse::<u64>() {
                Ok(tokens) if tokens >= MIN_SUBAGENT_ROLLOVER_TOKENS => {
                    config.subagent_rollover_tokens = tokens
                }
                _ => config.warnings.push(format!(
                    "subagent_rollover_tokens = {val} — expected a token count of at least {MIN_SUBAGENT_ROLLOVER_TOKENS}; using {DEFAULT_SUBAGENT_ROLLOVER_TOKENS}"
                )),
            },
            "subagent_transform_hooks" => match val {
                "true" | "1" | "on" | "yes" => config.subagent_transform_hooks = true,
                "false" | "0" | "off" | "no" => config.subagent_transform_hooks = false,
                _ => config.warnings.push(format!(
                    "subagent_transform_hooks = {val} — expected on or off; using on"
                )),
            },
            // Legacy flat spellings (pre-`subagent.*`): same parser, same limits.
            "subagent_max_concurrent" | "subagent_max_total" => {
                let dotted = key.replacen("subagent_", "subagent.", 1);
                if let Err(reason) =
                    parse_subagent_config_key(&mut config.subagent, &dotted, val)
                {
                    config.warnings.push(format!("{key} — {reason}"));
                }
            }
            "api_retries" => {
                if let Ok(retries) = val.parse::<u32>() {
                    config.api_retries = retries;
                }
            }
            "refusal_retries" => {
                if let Ok(retries) = val.parse::<u32>() {
                    config.refusal_retries = retries;
                }
            }
            "telemetry" => config.telemetry = val.to_string(),
            "telemetry_log_generations" => match val.parse::<u32>() {
                Ok(n) if (1..=MAX_TELEMETRY_LOG_GENERATIONS).contains(&n) => {
                    config.telemetry_log_generations = n;
                }
                _ => config.warnings.push(format!(
                    "telemetry_log_generations = {val} — expected 1–{MAX_TELEMETRY_LOG_GENERATIONS}; line ignored"
                )),
            },
            "cache_diagnostics" => {
                config.cache_diagnostics = matches!(val, "true" | "1" | "on" | "yes");
            }
            "cache_ttl" => match CacheTtlSetting::parse(val) {
                Some(ttl) => config.cache_ttl = ttl,
                None => config.warnings.push(format!(
                    "cache_ttl = {val} — expected auto, 5m, 1h, or hybrid; using auto"
                )),
            },
            "cache_keepwarm" => match CacheKeepWarm::parse(val) {
                Some(mode) => config.cache_keepwarm = mode,
                None => config.warnings.push(format!(
                    "cache_keepwarm = {val} — expected off, auto, or 1h–{MAX_CACHE_KEEPWARM_HOURS}h; keep-warm stays off"
                )),
            },
            "thinking_display" => match ThinkingDisplay::parse(val) {
                Some(display) => config.thinking_display = display,
                None => config.warnings.push(format!(
                    "thinking_display = {val} — expected summarized or omitted; using summarized"
                )),
            },
            "client_input" => match ClientInput::parse(val) {
                Some(mode) => config.client_input = mode,
                None => config.warnings.push(format!(
                    "client_input = {val} — expected shared or exclusive; using {}",
                    config.client_input
                )),
            },
            "cache_keepwarm_auto_max_hours" => {
                match val.trim().trim_end_matches(['h', 'H']).parse::<u32>() {
                    Ok(h) if (1..=MAX_CACHE_KEEPWARM_HOURS).contains(&h) => {
                        config.cache_keepwarm_auto_max_hours = h
                    }
                    _ => config.warnings.push(format!(
                        "cache_keepwarm_auto_max_hours = {val} — expected 1–{MAX_CACHE_KEEPWARM_HOURS}; using {DEFAULT_CACHE_KEEPWARM_AUTO_MAX_HOURS}"
                    )),
                }
            }
            "cache_keepwarm_headless" => match val.trim().to_ascii_lowercase().as_str() {
                "true" | "1" | "on" | "yes" => config.cache_keepwarm_headless = true,
                "false" | "0" | "off" | "no" => config.cache_keepwarm_headless = false,
                _ => config.warnings.push(format!(
                    "cache_keepwarm_headless = {val} — expected true or false; keeping false"
                )),
            },
            "cache_keepwarm_tools" => match val.trim().to_ascii_lowercase().as_str() {
                "true" | "1" | "on" | "yes" => config.cache_keepwarm_tools = true,
                "false" | "0" | "off" | "no" => config.cache_keepwarm_tools = false,
                _ => config.warnings.push(format!(
                    "cache_keepwarm_tools = {val} — expected on or off; keeping on"
                )),
            },
            "request_compression" => match RequestCompressionMode::parse(val) {
                Some(mode) => config.request_compression = mode,
                None => config.warnings.push(format!(
                    "request_compression = {val} — expected auto, on, or off; using auto"
                )),
            },
            "request_compression_level" => match val.trim().parse::<u32>() {
                Ok(n) if (1..=MAX_REQUEST_COMPRESSION_LEVEL).contains(&n) => {
                    config.request_compression_level = n
                }
                _ => config.warnings.push(format!(
                    "request_compression_level = {val} — expected 1–{MAX_REQUEST_COMPRESSION_LEVEL}; using {DEFAULT_REQUEST_COMPRESSION_LEVEL}"
                )),
            },
            "max_fps" => match val.parse::<u32>() {
                Ok(fps) if (1..=1000).contains(&fps) => config.max_fps = fps,
                Ok(_) => config.warnings.push(format!(
                    "max_fps = {val} — expected 1–1000; using {}",
                    config.max_fps
                )),
                Err(_) => config.warnings.push(format!(
                    "max_fps = {val} — not a number; using {}",
                    config.max_fps
                )),
            },
            "scroll_lines" => match val.parse::<u16>() {
                Ok(n) if (1..=20).contains(&n) => config.scroll_lines = Some(n),
                Ok(_) => config
                    .warnings
                    .push(format!("scroll_lines = {val} — expected 1–20; ignoring")),
                Err(_) => config
                    .warnings
                    .push(format!("scroll_lines = {val} — not a number; ignoring")),
            },
            "theme" => config.theme = Some(val.to_string()),
            "theme_transition" => match ThemeTransitionMode::parse(val) {
                Some(mode) => config.theme_transition = mode,
                None => config.warnings.push(format!(
                    "theme_transition = {val} — expected on, off, or milliseconds (0–2000); using on"
                )),
            },
            "tui_background_opaque" => match val {
                "true" | "1" | "on" | "yes" | "opaque" => config.tui_background_opaque = true,
                "false" | "0" | "off" | "no" | "invisible" => config.tui_background_opaque = false,
                _ => config.warnings.push(format!(
                    "tui_background_opaque = {val} — expected opaque or invisible; using {}",
                    if config.tui_background_opaque {
                        "opaque"
                    } else {
                        "invisible"
                    }
                )),
            },
            "read_dedupe" => match val {
                "true" | "1" | "on" | "yes" => config.read_dedupe = true,
                "false" | "0" | "off" | "no" => config.read_dedupe = false,
                _ => config.warnings.push(format!(
                    "read_dedupe = {val} — expected on or off; using on"
                )),
            },
            "tui_streaming_glow" => match val {
                "true" | "1" | "on" | "yes" => config.tui_streaming_glow = true,
                "false" | "0" | "off" | "no" => config.tui_streaming_glow = false,
                _ => config.warnings.push(format!(
                    "tui_streaming_glow = {val} — expected on or off; using off"
                )),
            },
            "tui_smooth_streaming" => match val {
                "true" | "1" | "on" | "yes" => config.tui_smooth_streaming = true,
                "false" | "0" | "off" | "no" => config.tui_smooth_streaming = false,
                _ => config.warnings.push(format!(
                    "tui_smooth_streaming = {val} — expected on or off; using on"
                )),
            },
            "process_scopes" => match val {
                "true" | "1" | "on" | "yes" | "auto" => config.process_scopes = true,
                "false" | "0" | "off" | "no" => config.process_scopes = false,
                _ => config.warnings.push(format!(
                    "process_scopes = {val} — expected auto or off; keeping the previous value (default auto)"
                )),
            },
            "tool_scope_slice" | "daemon_scope_slice" => {
                let field = if key == "tool_scope_slice" {
                    &mut config.tool_scope_slice
                } else {
                    &mut config.daemon_scope_slice
                };
                // `key =` with nothing after it unsets the key (no warning),
                // so a later line can undo an earlier one.
                if val.is_empty() {
                    *field = None;
                } else if let Some(why) = scope_slice_problem(val) {
                    config.warnings.push(format!(
                        "{key} = {val} — {why}; keeping the previous value (default: no slice, systemd's default placement)"
                    ));
                } else {
                    *field = Some(val.to_string());
                }
            }
            // Read directly by the keybind registry (`read_config_value`);
            // recognised here so the /settings row never trips the
            // unknown-key warning.
            "sidecar_toggle_key" => {}
            "agent_name" => config.agent_name = Some(val.to_string()),
            "identity" => config.identity = Some(val.to_string()),
            "disabled_plugins" => {
                config.disabled_plugins = parse_comma_list(val);
            }
            "favorite_models" => {
                config.favorite_models = parse_comma_list(val);
            }
            "disabled_skills" => {
                config.disabled_skills = parse_comma_list(val);
            }
            // An empty value unsets it; a bad one keeps the previous value.
            "skills.extra_root" if val.is_empty() => config.skills_extra_root = None,
            "skills.extra_root" => match check_skills_extra_root(val) {
                Ok(path) => config.skills_extra_root = Some(path),
                Err(why) => config
                    .warnings
                    .push(format!("skills.extra_root — {why}; ignoring it")),
            },
            "progressive_tool_disclosure" => {
                config.progressive_tool_disclosure = matches!(val, "true" | "1" | "on" | "yes");
            }
            "capacity_wait" => match val {
                "on" | "true" | "1" | "yes" => config.capacity_wait = true,
                "off" | "false" | "0" | "no" => config.capacity_wait = false,
                _ => config.warnings.push(format!(
                    "capacity_wait = {val} — expected on or off; keeping the default (on)"
                )),
            },
            "capacity_resume" => match val {
                "on" | "true" | "1" | "yes" => config.capacity_resume = true,
                "off" | "false" | "0" | "no" => config.capacity_resume = false,
                _ => config.warnings.push(format!(
                    "capacity_resume = {val} — expected on or off; keeping the default (on)"
                )),
            },
            "client_tools" => match val {
                "on" | "true" | "1" | "yes" => config.client_tools = true,
                "off" | "false" | "0" | "no" => config.client_tools = false,
                _ => config.warnings.push(format!(
                    "client_tools = {val} — expected on or off; keeping the default (on)"
                )),
            },
            "tools.deferred_loading" => match val {
                "on" | "true" | "1" | "yes" => config.tools_deferred_loading = true,
                "off" | "false" | "0" | "no" => config.tools_deferred_loading = false,
                _ => config.warnings.push(format!(
                    "tools.deferred_loading = {val} — expected on or off; keeping the default (on)"
                )),
            },
            "tools.activation_confirm" => match ActivationConfirm::parse(val) {
                Some(mode) => config.tools_activation_confirm = mode,
                None => config.warnings.push(format!(
                    "tools.activation_confirm = {val} — expected auto, prompt or deny; \
                     keeping the default (auto)"
                )),
            },
            "session_persistence" => {
                match crate::core::session_journal::SessionPersistence::parse(val) {
                    Some(mode) => config.session_persistence = mode,
                    None => config.warnings.push(format!(
                        "session_persistence = {val} — expected json or journal; \
                         keeping the default (json)"
                    )),
                }
            }
            "disabled_tools" => {
                config.disabled_tools = parse_comma_list(val);
            }
            "memory_only_tools" => {
                config.memory_only_tools = parse_comma_list(val);
            }
            "memory.backend" => {
                config.memory_backend.kind = match val {
                    "legacy" => MemoryBackendKind::Legacy,
                    "axel" => MemoryBackendKind::Axel,
                    _ => {
                        invalid_memory_backend = true;
                        config.warnings.push(format!(
                            "memory.backend = {val} — expected legacy or axel; memory backend unavailable"
                        ));
                        MemoryBackendKind::Unavailable
                    }
                };
            }
            "memory.user_scope" => match val {
                "true" => config.memory_backend.user_scope = true,
                "false" => config.memory_backend.user_scope = false,
                _ => {
                    invalid_memory_backend = true;
                    config.warnings.push("memory.user_scope — expected true or false; memory backend unavailable".into());
                }
            },
            "memory.axel.executable" | "memory.axel.brain" => {
                let path = PathBuf::from(val);
                if !path.is_absolute() {
                    invalid_memory_backend = true;
                    config.warnings.push(format!(
                        "{key} — expected an absolute path; memory backend unavailable"
                    ));
                }
                // Retain invalid paths too: direct config consumers must not
                // mistake an invalid explicit path for an absent default.
                if key == "memory.axel.executable" {
                    config.memory_backend.executable = Some(path);
                } else {
                    config.memory_backend.brain = Some(path);
                }
            }
            _ => {
                // Handle namespaced keys
                if key.starts_with("shell.") {
                    if let Err(diagnostic) = parse_shell_config_key(&mut config.shell, key, val) {
                        family_diagnostic(diagnostic);
                    }
                } else if key.starts_with("server.") {
                    parse_server_config_key(&mut config.server, key, val);
                } else if key.starts_with("bridge.") {
                    parse_bridge_config_key(&mut config.bridge, key, val);
                } else if key.starts_with("auth.") {
                    parse_auth_config_key(&mut config.auth, key, val);
                    if let Some(key) = key.strip_prefix("auth.auto.").filter(|k| !k.trim().is_empty()) {
                        let (_, warnings) = crate::auth::AutoSelectionPolicy::from_config_map(
                            &BTreeMap::from([(key.to_string(), val.trim().to_string())]));
                        config.warnings.extend(warnings);
                    }
                    if let Some(provider) =
                        key.strip_prefix("auth.account.").filter(|p| !p.trim().is_empty())
                    {
                        let mut one = BTreeMap::new();
                        one.insert(provider.trim().to_string(), val.trim().to_string());
                        let (_, warnings) =
                            crate::core::auth::AccountPolicy::from_config_map(&one);
                        config.warnings.extend(warnings);
                    }
                    if let Some(warning) = unknown_auth_key_warning(key) {
                        config.warnings.push(warning);
                    }
                } else if key.starts_with("events.") {
                    if let Err(diagnostic) = parse_events_config_key(&mut config.events, key, val)
                    {
                        family_diagnostic(diagnostic);
                    }
                } else if key.starts_with("wait.") {
                    if let Err(diagnostic) = parse_wait_config_key(&mut config.wait, key, val) {
                        family_diagnostic(diagnostic);
                    }
                } else if key.starts_with("turn_budget.") {
                    match parse_turn_budget_config_key(&mut config.turn_budgets, key, val) {
                        Ok(None) => {}
                        Ok(Some(warning)) => config.warnings.push(warning),
                        Err(diagnostic) => family_diagnostic(diagnostic),
                    }
                } else if key.starts_with("memory.") {
                    if let Err(diagnostic) = parse_memory_config_key(&mut config.memory, key, val)
                    {
                        family_diagnostic(diagnostic);
                    }
                } else if key.starts_with("startup.") {
                    if let Err(reason) =
                        parse_startup_config_key(&mut config.startup, key, val)
                    {
                        config.warnings.push(format!("{key} — {reason}"));
                    }
                } else if key.starts_with("daemon.") {
                    if let Err(reason) =
                        parse_daemon_config_key(&mut config.daemon, key, val)
                    {
                        config.warnings.push(format!("{key} — {reason}"));
                    }
                } else if key.starts_with("subagent.watch.") {
                    if let Err(diagnostic) = parse_watch_config_key(&mut config.watch, key, val) {
                        family_diagnostic(diagnostic);
                    }
                } else if key.starts_with("subagent.") {
                    if let Err(reason) =
                        parse_subagent_config_key(&mut config.subagent, key, val)
                    {
                        config.warnings.push(format!("{key} — {reason}"));
                    }
                } else if key == "context_management.summary_model" {
                    // A model choice, not a threshold: an invalid value
                    // turns the summary off (never silently `auto`) without
                    // disabling context management.
                    match parse_summary_model(val) {
                        Ok(model) => {
                            config.summary_model = model;
                            config.summary_model_invalid = false;
                        }
                        Err(reason) => {
                            config.summary_model = None;
                            config.summary_model_invalid = true;
                            config.warnings.push(format!(
                                "{key} — {reason}; the rolling summary is off until it is fixed"
                            ));
                        }
                    }
                } else if key.starts_with("context_management.") {
                    if let Err(reason) = parse_context_management_config_key(
                        &mut config.context_management, key, val,
                    ) {
                        invalid_context_management = true;
                        config.warnings.push(format!("{key} — {reason}"));
                    }
                } else if let Some(provider_key) = key.strip_prefix("provider.") {
                    config
                        .provider_keys
                        .insert(provider_key.to_string(), val.to_string());
                } else if let Some(keybind_key) = key.strip_prefix("keybind.") {
                    config
                        .keybinds
                        .insert(keybind_key.to_string(), val.to_string());
                } else if key.contains('.') {
                    // Dotted keys are namespaced (plugin/extension config, e.g.
                    // `knowledge.jawz_notes`). Plugins define their own keys —
                    // not ours to police. Silently preserved.
                } else {
                    // Unknown top-level key — warn with a did-you-mean if close.
                    match did_you_mean(key) {
                        Some(suggestion) => config.warnings.push(format!(
                            "unknown key '{key}' (did you mean '{suggestion}'?)"
                        )),
                        None => config
                            .warnings
                            .push(format!("unknown key '{key}' — ignored")),
                    }
                }
            }
        }
    }

    // Never leave a partially parsed automatic policy enabled. Syntax errors
    // invalidate this whole parse pass, even if a later duplicate is valid.
    let context_validation = match config.context_window {
        Some(window) => config.context_management.validate_for_window(window),
        None => config.context_management.validate(),
    };
    if let Err(reason) = context_validation {
        invalid_context_management = true;
        config
            .warnings
            .push(format!("context_management — {reason}"));
    }
    // The delegation policy refuses concurrent > total, which would take
    // subagents away entirely. Keep the stricter number for both. Checked
    // after the loop so key order never matters.
    if config.subagent.max_concurrent > config.subagent.max_total {
        config.warnings.push(format!(
            "subagent.max_concurrent ({}) is above subagent.max_total ({}); using {} for both",
            config.subagent.max_concurrent, config.subagent.max_total, config.subagent.max_total
        ));
        config.subagent.max_concurrent = config.subagent.max_total;
    }
    // A warn line above the refuse line would let a resume be refused
    // without ever having been warned about; keep the stricter number.
    if let (Some(warn), Some(refuse)) = (
        config.subagent.context_warn_tokens,
        config.subagent.resume_refuse_tokens,
    ) {
        if warn > refuse {
            config.warnings.push(format!(
                "subagent.context_warn_tokens ({warn}) is above subagent.resume_refuse_tokens ({refuse}); using {refuse} for both"
            ));
            config.subagent.context_warn_tokens = Some(refuse);
            config.subagent_context_clamp = Some((warn, refuse));
        }
    }
    if invalid_context_management {
        config.context_management = ContextManagementConfig::default();
        config.warnings.push(
            "invalid context_management configuration — context management is off".to_string(),
        );
    }

    // Any invalid backend setting poisons this parse pass, regardless of key
    // order or later duplicate selectors. Never silently route to legacy.
    if invalid_memory_backend {
        config.memory_backend.kind = MemoryBackendKind::Unavailable;
    }

    // Fail-closed operator-consent rule (Task A2, spec §12): a non-"off"
    // `memory.default_mode` takes effect only when the operator also set
    // `memory.default_mode_confirmed = true` in this same parse pass.
    // Checked after the loop so key order never matters.
    if config.memory.default_mode != "off" && !config.memory.default_mode_confirmed {
        config.warnings.push(format!(
            "memory.default_mode = {} requires memory.default_mode_confirmed = true \
             in the same config; reverting to off",
            config.memory.default_mode
        ));
        config.memory.default_mode = "off".to_string();
    }

    // Derive max_message_size from context_window if not explicitly set.
    // Rough estimate: 1 token ≈ 4 bytes. Context window in tokens → bytes.
    if config.server.max_message_size.is_none() {
        if let Some(ctx_tokens) = config.context_window {
            config.server.max_message_size = Some((ctx_tokens as usize) * 4);
        }
    }
}

/// Read a single config value by exact key from the active config file.
pub fn read_config_value(key: &str) -> Option<String> {
    let path = resolve_read_path("config");
    let content = std::fs::read_to_string(&path).ok()?;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        if k.trim() == key.trim() {
            return Some(v.trim().to_string());
        }
    }
    None
}

/// Exclusive advisory lock on `<config>.lock`, held around every
/// read-modify-write of the config file (same pattern as `auth.json`).
/// Two writers (`/settings` in two processes, a `/model` favorite toggle
/// racing a settings edit) no longer lose each other's updates.
///
/// Held for the RMW only (< 1 ms); readers (`load_config`) never take it —
/// they already tolerate the atomic rename. `SYNAPS_CONFIG_LOCK=0` bypasses.
///
/// NOT reentrant: flock on a fresh fd of the same file from the same process
/// conflicts on Linux. Callers that already hold the guard must use the
/// `_locked` inner fns.
struct ConfigLock {
    _file: Option<std::fs::File>,
}

impl ConfigLock {
    fn acquire() -> std::io::Result<Self> {
        if std::env::var("SYNAPS_CONFIG_LOCK").as_deref() == Ok("0") {
            return Ok(Self { _file: None });
        }
        use fs4::fs_std::FileExt;
        let lock_path = resolve_write_path("config").with_extension("lock");
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut open = std::fs::OpenOptions::new();
        open.create(true).write(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            open.mode(0o600);
        }
        let file = open.open(&lock_path)?;
        FileExt::lock_exclusive(&file)?;
        Ok(Self { _file: Some(file) })
    }
}

/// The `auth.*` settings of the active config file, parsed without any
/// diagnostics output. A missing or unreadable file reads as empty, exactly
/// like [`load_config`].
pub fn read_auth_config() -> AuthConfig {
    let content = std::fs::read_to_string(resolve_read_path("config")).unwrap_or_default();
    let mut config = SynapsConfig::default();
    apply_config_content_with(&mut config, &content, &mut |_diagnostic: String| {});
    config.auth
}

/// Check one `skills.extra_root` value (already trimmed, as the parser
/// trims): a single absolute path. Inner spaces are kept as they are
/// ("…/Application Support/…"); a control character, a leading `~` (never
/// expanded), quotes or a relative path are refused. The reason never
/// echoes the value.
pub fn check_skills_extra_root(val: &str) -> Result<PathBuf, &'static str> {
    if val.is_empty() {
        return Err("empty value");
    }
    if val.chars().any(char::is_control) {
        return Err("the path has a control character");
    }
    if val.starts_with('~') {
        return Err("~ is not expanded; write the absolute path");
    }
    if val.starts_with('"') || val.starts_with('\'') {
        return Err("write the path without quotes; spaces in it are kept");
    }
    let path = std::path::Path::new(val);
    if !path.is_absolute() {
        return Err("not an absolute path");
    }
    Ok(path.to_path_buf())
}

/// The config keys one skill discovery pass reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillsKeys {
    pub extra_root: Option<PathBuf>,
    pub disabled_plugins: Vec<String>,
    pub disabled_skills: Vec<String>,
}

impl SynapsConfig {
    /// This snapshot's [`SkillsKeys`].
    pub fn skills_keys(&self) -> SkillsKeys {
        SkillsKeys {
            extra_root: self.skills_extra_root.clone(),
            disabled_plugins: self.disabled_plugins.clone(),
            disabled_skills: self.disabled_skills.clone(),
        }
    }
}

/// The [`SkillsKeys`] of the active config file, read fresh now and parsed
/// without any diagnostics output (`skills.reload`); the caller's own
/// config snapshot is untouched. A missing or unreadable file reads as
/// empty, exactly like [`load_config`].
pub fn read_skills_keys() -> SkillsKeys {
    let content = std::fs::read_to_string(resolve_read_path("config")).unwrap_or_default();
    let mut config = SynapsConfig::default();
    apply_config_content_with(&mut config, &content, &mut |_diagnostic: String| {});
    config.skills_keys()
}

/// The config write lock held across a caller's compare-and-set (the
/// accounts verbs' `--expect`): read the current values, decide, and write
/// with no other locked writer in between. Writes are the same atomic
/// replace as [`write_config_value`]. Not reentrant (see [`ConfigLock`]):
/// never call [`write_config_value`] while holding it.
pub struct ConfigWriteGuard {
    _lock: ConfigLock,
}

impl ConfigWriteGuard {
    /// Blocks until the lock is free (holders keep it for one RMW).
    pub fn acquire() -> std::io::Result<Self> {
        Ok(Self {
            _lock: ConfigLock::acquire()?,
        })
    }

    /// [`read_auth_config`], made while the lock is held.
    pub fn read_auth(&self) -> AuthConfig {
        read_auth_config()
    }

    /// Write every pair in one atomic replace.
    pub fn write_values(&self, pairs: &[(&str, &str)]) -> std::io::Result<()> {
        write_config_values_locked(pairs)
    }

    /// [`Self::write_values`], refused with nothing written when a key has
    /// more than one line (see [`write_config_value_no_duplicates`]).
    pub fn write_values_no_duplicates(&self, pairs: &[(&str, &str)]) -> std::io::Result<()> {
        write_config_values_locked_with(pairs, true)
    }
}

/// Write a single `key = value` pair to `~/.synaps-cli/config` (or profile config).
/// Replaces the first existing line that matches the key, or appends if absent.
/// Preserves comments and unknown keys. Writes atomically via temp file + rename.
pub fn write_config_value(key: &str, value: &str) -> std::io::Result<()> {
    let _guard = ConfigLock::acquire()?;
    write_config_value_locked(key, value)
}

/// [`write_config_value`], but refused with nothing written (an
/// `InvalidData` error naming the key) when the file has more than one
/// `key = …` line: a write replaces the first, while the parser reads the
/// last, so the new value would not take effect (`synaps auth use`).
pub fn write_config_value_no_duplicates(key: &str, value: &str) -> std::io::Result<()> {
    let _guard = ConfigLock::acquire()?;
    write_config_values_locked_with(&[(key, value)], true)
}

/// The RMW body; caller holds [`ConfigLock`].
fn write_config_value_locked(key: &str, value: &str) -> std::io::Result<()> {
    write_config_values_locked(&[(key, value)])
}

/// Several `key = value` pairs in ONE atomic replace; caller holds
/// [`ConfigLock`]. Each pair lands exactly as [`write_config_value`] would
/// write it alone.
fn write_config_values_locked(pairs: &[(&str, &str)]) -> std::io::Result<()> {
    write_config_values_locked_with(pairs, false)
}

/// [`write_config_values_locked`]; with `refuse_duplicates`, a key that has
/// more than one line refuses the whole write.
fn write_config_values_locked_with(
    pairs: &[(&str, &str)],
    refuse_duplicates: bool,
) -> std::io::Result<()> {
    let path = resolve_write_path("config");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let duplicate = pairs
        .iter()
        .find(|(key, _)| refuse_duplicates && config_key_lines(&existing, key.trim()) > 1);
    if let Some((key, _)) = duplicate {
        let message = format!(
            "{} has more than one line in {}; keep one and retry",
            key.trim(),
            path.display()
        );
        let kind = std::io::ErrorKind::InvalidData;
        return Err(std::io::Error::new(kind, message));
    }
    let out = pairs
        .iter()
        .fold(existing, |text, (key, value)| replace_config_line(&text, key, value));
    publish_config_locked(&path, &out)
}

/// Atomically replace the config file at `path` with `contents`: an
/// owner-only temp file beside it, renamed over it. Caller holds
/// [`ConfigLock`].
fn publish_config_locked(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, contents)?;
    // Config may contain API keys — restrict to owner-only
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)
}

/// The trimmed value of `line` when it is a `key = value` line for `key`;
/// comments and blank lines never are. The one matching rule shared by
/// [`replace_config_line`] and [`write_config_values_checked`].
fn config_line_value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let t = line.trim_start();
    if t.starts_with('#') {
        return None;
    }
    let (k, v) = t.split_once('=')?;
    (k.trim() == key).then_some(v.trim())
}

/// How many lines of `content` are `key = …` lines for `key`, by
/// [`config_line_value`]. The parser takes the last of them and a write
/// replaces the first, so more than one is refused by the checked writes.
fn config_key_lines(content: &str, key: &str) -> usize {
    content
        .lines()
        .filter(|line| config_line_value(line, key).is_some())
        .count()
}

/// `existing` with the first `key = …` line replaced by `key = value` (or
/// the pair appended), comments and unknown keys preserved — exactly what
/// [`write_config_value`] writes.
fn replace_config_line(existing: &str, key: &str, value: &str) -> String {
    let key_trimmed = key.trim();
    let replacement = format!("{} = {}", key_trimmed, value);

    let mut found = false;
    let mut new_lines: Vec<String> = existing
        .lines()
        .map(|line| {
            if !found && config_line_value(line, key_trimmed).is_some() {
                found = true;
                return replacement.clone();
            }
            line.to_string()
        })
        .collect();

    if !found {
        new_lines.push(replacement);
    }

    let mut out = new_lines.join("\n");
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Keys that `synaps config set` ([`write_config_values_checked`]) may write
/// in any mode (the `--if-unset`-only keys are [`IF_UNSET_ONLY_CONFIG_KEYS`]).
///
/// An explicit allowlist, not "any known key": each entry becomes a public,
/// scriptable write surface (installers set these unattended), so growing
/// this list is a deliberate product decision, never a convenience.
pub const SETTABLE_CONFIG_KEYS: &[&str] = &["memory.backend", "memory.axel.executable"];

/// Keys that `synaps config set --if-unset` may write, and no other mode:
/// the setup doctor fills them in when the user has not chosen, and never
/// replaces a choice. A plain set ([`ConfigSetMode::Overwrite`]) or a
/// compare-and-set ([`ConfigSetMode::Expect`]) of one is refused as not
/// settable. Values get the normal parser check. Besides these, the
/// `auth.account.<provider>` family takes `--if-unset` with the value
/// [`IF_UNSET_ACCOUNT_VALUE`] for a known provider, and nothing else (see
/// [`config_set_allows`]).
pub const IF_UNSET_ONLY_CONFIG_KEYS: &[&str] = &["context_management.mode", "skills.extra_root"];

/// The only value `config set --if-unset` may write to
/// `auth.account.<provider>`: never a specific account, never a clear.
pub const IF_UNSET_ACCOUNT_VALUE: &str = crate::core::auth::AUTO_ACCOUNT_NAME;

/// Whether `synaps config set` in `mode` may write `value` to `key`:
/// [`SETTABLE_CONFIG_KEYS`] in any mode; under [`ConfigSetMode::IfUnset`]
/// only, [`IF_UNSET_ONLY_CONFIG_KEYS`] and `auth.account.<provider> = auto`
/// for a provider synaps knows.
pub fn config_set_allows(key: &str, value: &str, mode: &ConfigSetMode) -> bool {
    if SETTABLE_CONFIG_KEYS.contains(&key) {
        return true;
    }
    if !matches!(mode, ConfigSetMode::IfUnset) {
        return false;
    }
    if IF_UNSET_ONLY_CONFIG_KEYS.contains(&key) {
        return true;
    }
    use crate::core::auth::OAuthProviderId;
    let known = |provider: &str| provider.parse::<OAuthProviderId>().is_ok();
    key.strip_prefix("auth.account.").is_some_and(known) && value.trim() == IF_UNSET_ACCOUNT_VALUE
}

/// How [`write_config_values_checked`] treats keys that already have a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigSetMode {
    /// Write every pair, replacing current values.
    Overwrite,
    /// Write absent keys; a key set to a different value, or to an empty
    /// one, refuses the call.
    IfUnset,
    /// Compare-and-set: each `(key, value)` must match the current value (an
    /// empty value means "must be unset"), or the call is refused.
    Expect(Vec<(String, String)>),
}

/// Overall result of [`write_config_values_checked`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSetResult {
    /// The config file was replaced, in one atomic write.
    Written,
    /// Every key already had its value; nothing was written.
    Unchanged,
    /// A key is not settable or a check failed; nothing was written.
    Refused,
}

impl ConfigSetResult {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Written => "written",
            Self::Unchanged => "unchanged",
            Self::Refused => "refused",
        }
    }
}

/// Per-key status in a [`ConfigSetOutcome`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigKeyStatus {
    /// The new value was written.
    Written,
    /// Not written: the key already had the value, was only an expectation,
    /// or the call was refused because of another key.
    Unchanged,
    /// [`ConfigSetMode::IfUnset`]: the key is set to a different value.
    Conflict,
    /// [`ConfigSetMode::IfUnset`]: the key's one line has an empty value
    /// (`key =`). The parser reads that as unset, but it is the user's line,
    /// so it is refused like a [`Self::Conflict`] and named apart from one.
    Empty,
    /// The key has more than one line in the file, in any mode: the parser
    /// takes the last and a write replaces the first, so neither the check
    /// nor the write would mean what it says.
    Duplicate,
    /// [`ConfigSetMode::Expect`]: the key does not have the expected value.
    ExpectFailed,
    /// [`config_set_allows`] refuses the key (with its value, in this mode).
    NotSettable,
    /// The `expect_path` given to [`write_config_values_checked`] is not the
    /// config file's canonical path; no key was checked or written.
    PathMismatch,
}

impl ConfigKeyStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Written => "written",
            Self::Unchanged => "unchanged",
            Self::Conflict => "conflict",
            Self::Empty => "empty",
            Self::Duplicate => "duplicate",
            Self::ExpectFailed => "expect_failed",
            Self::NotSettable => "not_settable",
            Self::PathMismatch => "path_mismatch",
        }
    }
}

/// What [`write_config_values_checked`] did. Never holds a config value (the
/// file may contain secrets), so it is safe to print in full.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigSetOutcome {
    pub result: ConfigSetResult,
    /// Canonical absolute path of the config file — the one `load_config`
    /// reads, which is also the one written.
    pub path: PathBuf,
    /// Every key the call names (pairs first, then expect-only keys).
    pub keys: Vec<(String, ConfigKeyStatus)>,
}

/// Why [`write_config_values_checked`] could not decide.
#[derive(Debug)]
pub enum ConfigSetError {
    /// Malformed, empty or duplicate input, a value the config parser
    /// rejects, or a profile whose config file does not exist yet.
    Invalid(String),
    /// Reading or writing the config file failed.
    Io(std::io::Error),
}

impl std::fmt::Display for ConfigSetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(message) => f.write_str(message),
            Self::Io(err) => write!(f, "config file: {err}"),
        }
    }
}

impl std::error::Error for ConfigSetError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Invalid(_) => None,
            Self::Io(err) => Some(err),
        }
    }
}

impl From<std::io::Error> for ConfigSetError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

/// Set several config keys as one atomic step — the engine of
/// `synaps config set`.
///
/// Under a single [`ConfigLock`] hold: one read of the config file, every
/// check, then one temp-file + rename write (owner-only) or no write at all.
/// With `expect_path`, the canonical config path must equal it exactly (no
/// canonicalization of `expect_path`) before anything else is decided. A key
/// that [`config_set_allows`] rejects refuses the call before the file is
/// read.
/// A value the real parser would warn about is rejected the way
/// [`validate_config_edit`] rejects it, whatever the file holds now. A key is
/// present by the rule [`replace_config_line`] uses (a non-comment
/// `key = value` line); values compare trimmed. A key with more than one
/// such line refuses the call in every mode ([`ConfigKeyStatus::Duplicate`]),
/// and under `--if-unset` a line with an empty value is
/// [`ConfigKeyStatus::Empty`], not a conflict. A missing base dir is made
/// `0700` ([`create_config_dir`]).
///
/// `Err(Invalid)` is bad input; a policy or state refusal is `Ok` with
/// [`ConfigSetResult::Refused`] and a reason per key.
pub fn write_config_values_checked(
    pairs: &[(String, String)],
    mode: &ConfigSetMode,
    expect_path: Option<&std::path::Path>,
) -> Result<ConfigSetOutcome, ConfigSetError> {
    let expects: &[(String, String)] = match mode {
        ConfigSetMode::Expect(expects) => expects.as_slice(),
        ConfigSetMode::Overwrite | ConfigSetMode::IfUnset => &[],
    };
    check_config_set_request(pairs, expects)?;
    let path = config_set_target()?;

    let mut keys: Vec<(String, ConfigKeyStatus)> = Vec::new();
    for (key, _) in pairs.iter().chain(expects) {
        if !keys.iter().any(|(k, _)| k == key) {
            keys.push((key.clone(), ConfigKeyStatus::Unchanged));
        }
    }
    let refused = |keys: Vec<(String, ConfigKeyStatus)>| ConfigSetOutcome {
        result: ConfigSetResult::Refused,
        path: canonical_config_path(&path),
        keys,
    };

    // A fresh install: make the base dir 0700 here, before the lock step
    // would make it with the umask's mode.
    if let Some(dir) = path.parent() {
        create_config_dir(dir)?;
    }
    let _guard = ConfigLock::acquire()?;
    // Read and replace the file itself, never a symlink to it: the path
    // reported (and compared by --expect-path) is then the file that changes,
    // and a symlinked config stays a symlink.
    let path = canonical_config_path(&path);
    // Checked under the lock, so the file confirmed here is the file written.
    if let Some(expected) = expect_path {
        if canonical_config_path(&path).as_os_str() != expected.as_os_str() {
            for (_, status) in &mut keys {
                *status = ConfigKeyStatus::PathMismatch;
            }
            return Ok(refused(keys));
        }
    }
    let mut not_settable = false;
    for (key, status) in &mut keys {
        let value = pairs
            .iter()
            .find(|(k, _)| k == key)
            .map_or("", |(_, v)| v.as_str());
        if !config_set_allows(key, value, mode) {
            *status = ConfigKeyStatus::NotSettable;
            not_settable = true;
        }
    }
    if not_settable {
        return Ok(refused(keys));
    }

    let existing = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        // Unreadable or not UTF-8: never replace what could not be read.
        Err(err) => return Err(err.into()),
    };
    // Values are compared and written trimmed, as the parser reads them.
    let apply = |subset: &[&(String, String)]| {
        let mut text = existing.clone();
        for (key, value) in subset {
            text = replace_config_line(&text, key, value.trim());
        }
        text
    };

    // Judge every value, not only the ones that would change, so a bad
    // value is an error whatever the file holds now.
    let all: Vec<&(String, String)> = pairs.iter().collect();
    let before = config_content_problems(&existing);
    let introduced: Vec<String> = config_content_problems(&apply(&all))
        .into_iter()
        .filter(|problem| !before.contains(problem))
        .collect();
    if !introduced.is_empty() {
        return Err(ConfigSetError::Invalid(format!(
            "rejected by the config parser: {}",
            introduced.join("; ")
        )));
    }

    let current = |key: &str| existing.lines().find_map(|line| config_line_value(line, key));
    let duplicate = |key: &str| config_key_lines(&existing, key) > 1;
    for (key, status) in &mut keys {
        if duplicate(key) {
            *status = ConfigKeyStatus::Duplicate;
        }
    }
    let if_unset = matches!(mode, ConfigSetMode::IfUnset);
    let mut changed: Vec<&(String, String)> = Vec::new();
    for pair in pairs.iter().filter(|(key, _)| !duplicate(key)) {
        let (key, value) = (pair.0.as_str(), pair.1.trim());
        match current(key) {
            Some(now) if now == value => {}
            Some("") if if_unset => set_key_status(&mut keys, key, ConfigKeyStatus::Empty),
            Some(_) if if_unset => set_key_status(&mut keys, key, ConfigKeyStatus::Conflict),
            _ => changed.push(pair),
        }
    }
    for (key, want) in expects.iter().filter(|(key, _)| !duplicate(key)) {
        let (now, want) = (current(key), want.trim());
        // "Unset" means no line at all: `key =` with nothing after is set.
        let met = if want.is_empty() {
            now.is_none()
        } else {
            now == Some(want)
        };
        if !met {
            set_key_status(&mut keys, key, ConfigKeyStatus::ExpectFailed);
        }
    }

    let result = if keys.iter().any(|(_, status)| *status != ConfigKeyStatus::Unchanged) {
        ConfigSetResult::Refused
    } else if changed.is_empty() {
        ConfigSetResult::Unchanged
    } else {
        // Made above; only missing if it was removed since.
        if let Some(dir) = path.parent() {
            create_config_dir(dir)?;
        }
        publish_config_locked(&path, &apply(&changed))?;
        for (key, _) in &changed {
            set_key_status(&mut keys, key, ConfigKeyStatus::Written);
        }
        ConfigSetResult::Written
    };
    Ok(ConfigSetOutcome {
        result,
        path: canonical_config_path(&path),
        keys,
    })
}

/// Record `status` for `key` in a [`ConfigSetOutcome`] key list.
fn set_key_status(keys: &mut [(String, ConfigKeyStatus)], key: &str, status: ConfigKeyStatus) {
    if let Some((_, slot)) = keys.iter_mut().find(|(k, _)| k == key) {
        *slot = status;
    }
}

/// Input checks of [`write_config_values_checked`]; they touch no file.
fn check_config_set_request(
    pairs: &[(String, String)],
    expects: &[(String, String)],
) -> Result<(), ConfigSetError> {
    let invalid = |message: String| Err(ConfigSetError::Invalid(message));
    if pairs.is_empty() {
        return invalid("nothing to set".to_string());
    }
    let key_char = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_.-".contains(&b);
    for (list, is_expect) in [(pairs, false), (expects, true)] {
        for (key, value) in list {
            if key.is_empty() || !key.bytes().all(key_char) {
                return invalid(format!("invalid key {key:?}: allowed are a-z 0-9 _ . -"));
            }
            // Before any trimming: a trailing newline is a control character too.
            if value.chars().any(char::is_control) {
                return invalid(format!(
                    "value for {key} must be one line with no control characters"
                ));
            }
            // Clearing an account choice is a refusal (not settable), not
            // bad input.
            if !is_expect && value.trim().is_empty() && !key.starts_with("auth.account.") {
                return invalid(format!("empty value for {key}"));
            }
            if list.iter().filter(|(k, _)| k == key).count() > 1 {
                return invalid(format!("{key} is given more than once"));
            }
        }
    }
    Ok(())
}

/// The config file [`write_config_values_checked`] reads and writes: the
/// write path of [`write_config_value`], which must also be the file
/// [`load_config`] reads. They differ only under a profile whose config does
/// not exist yet — checks would then run against another file than the one
/// written, so that is refused.
fn config_set_target() -> Result<PathBuf, ConfigSetError> {
    // `resolve_write_path` without its directory creation.
    let target = get_active_config_dir().join("config");
    let read = resolve_read_path("config");
    if read != target {
        return Err(ConfigSetError::Invalid(format!(
            "{} does not exist yet (this profile reads {}); create it first",
            target.display(),
            read.display()
        )));
    }
    Ok(target)
}

/// Make the config file's directory (the base dir: under a profile it
/// exists already, see [`config_set_target`]) when it is missing, `0700`
/// set explicitly, so the umask does not decide it. Missing parents get
/// what `create_dir_all` gives them (the umask's mode), as before. An
/// existing directory is left as it is, mode and all; so is one another
/// process makes in between.
fn create_config_dir(dir: &std::path::Path) -> std::io::Result<()> {
    if dir.as_os_str().is_empty() || dir.is_dir() {
        return Ok(());
    }
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        match std::fs::DirBuilder::new().mode(0o700).create(dir) {
            // Made here, so its mode is ours to set (mkdir's is masked).
            Ok(()) => std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?,
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err),
        }
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(dir)?;
    Ok(())
}

/// The canonical absolute path of the config file [`load_config`] reads now
/// (the active profile's when it exists, else the base dir's), in the same
/// form `synaps config set --json` reports as "path", so the two compare
/// exactly. The daemon records it at boot for WELCOME's `config_path`.
pub fn canonical_config_read_path() -> PathBuf {
    canonical_config_path(&resolve_read_path("config"))
}

/// `path` made canonical; when the file does not exist yet, its directory.
fn canonical_config_path(path: &std::path::Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }
    if let (Some(dir), Some(name)) = (path.parent(), path.file_name()) {
        if let Ok(dir) = std::fs::canonicalize(dir) {
            return dir.join(name);
        }
    }
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Add a favorite model id (`provider/model`) to config, preserving sort/dedup.
pub fn add_favorite_model(id: &str) -> std::io::Result<()> {
    let trimmed = id.trim();
    if trimmed.is_empty() {
        return Ok(());
    }
    let _guard = ConfigLock::acquire()?;
    let mut values = load_config().favorite_models;
    if !values.iter().any(|v| v == trimmed) {
        values.push(trimmed.to_string());
        values.sort();
    }
    write_comma_list_locked("favorite_models", &values)
}

/// Remove a favorite model id (`provider/model`) from config.
pub fn remove_favorite_model(id: &str) -> std::io::Result<()> {
    let _guard = ConfigLock::acquire()?;
    let mut values = load_config().favorite_models;
    values.retain(|v| v != id.trim());
    write_comma_list_locked("favorite_models", &values)
}

/// Return whether a model id is marked as favorite.
pub fn is_favorite_model(id: &str) -> bool {
    load_config().favorite_models.iter().any(|v| v == id.trim())
}

/// Whether a key matches the secret denylist (case-insensitive).
/// Used by the journal save path to ensure secret values never reach disk,
/// and by the client env capture to strip secrets before `Hello`.
pub fn is_secret_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    upper.ends_with("_KEY")
        || upper.ends_with("_TOKEN")
        || upper.ends_with("_CREDENTIALS")
        || upper.ends_with("_API_KEY")
        || upper.contains("SECRET")
        || upper.contains("PASSWORD")
        || upper.contains("PASSWD")
        || upper.ends_with("_PAT")
        || upper.ends_with("_DSN")
}

/// Whether an env VALUE carries a credential regardless of its key name:
/// any URL with userinfo (`scheme://user:pass@host`, `scheme://key@host`),
/// anywhere in the value — `DATABASE_URL`, `HTTPS_PROXY`, a Sentry DSN or a
/// JDBC option string all qualify. The key-name denylist alone missed these,
/// so their credentials reached the daemon and the session files on disk.
/// A username-only URL (`ssh://git@host/…`) is treated as a secret too:
/// rare in an environment, and failing safe beats a guess.
pub fn is_secret_env_value(value: &str) -> bool {
    let mut rest = value;
    while let Some(i) = rest.find("://") {
        let scheme_ok = rest[..i]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric());
        let after = &rest[i + 3..];
        // The authority ends at the path, query or fragment, or at a list /
        // option separator when several URLs share one value.
        let end = after
            .find(|c: char| {
                matches!(c, '/' | '?' | '#' | ',' | ';' | '"' | '\'') || c.is_whitespace()
            })
            .unwrap_or(after.len());
        if scheme_ok && after[..end].rfind('@').is_some_and(|at| at > 0) {
            return true;
        }
        rest = after;
    }
    false
}

/// Whether an env pair must never reach the daemon or disk: a secret key
/// name OR a value carrying credentials (see [`is_secret_env_value`]).
pub fn is_secret_env(key: &str, value: &str) -> bool {
    is_secret_key(key) || is_secret_env_value(value)
}

/// Working-habit steering shared by both stock prompts. It keeps output,
/// re-reads and file sizes down; those tokens otherwise ride along on every
/// later request. Phrased without hard tool names: any tool can be disabled.
macro_rules! default_prompt_steering {
    ($output_filters:literal) => {
        concat!(
            " Change existing files with targeted edits rather than rewriting them whole. \
            A file you just read or wrote is current in your context; re-read it only if it may have changed. \
            Keep files, docs and comments as lean as the task allows. \
            Keep command output small (quiet flags, ",
            $output_filters,
            ") and read large outputs selectively."
        )
    };
}

/// Stock system prompt on Windows, used only when no `--system` value and no
/// `system.md` exist. Custom prompts never include it.
pub const DEFAULT_PROMPT_WINDOWS: &str = concat!(
    "You are a helpful AI agent running in a terminal on Windows. \
    You have access to powershell, bash, read, and write tools. \
    Prefer the powershell tool for system commands and filesystem operations — it is native \
    to Windows and avoids quoting/path translation issues. \
    Use bash only when the task specifically requires WSL or a Unix environment. \
    Be concise and direct. Use tools when the user asks you to interact with the filesystem or run commands.",
    default_prompt_steering!("`Select-Object -Last N`, `Select-String`")
);

/// Stock system prompt on every other platform; see [`DEFAULT_PROMPT_WINDOWS`].
pub const DEFAULT_PROMPT_UNIX: &str = concat!(
    "You are a helpful AI agent running in a terminal. \
    You have access to bash, read, and write tools. \
    Be concise and direct. Use tools when the user asks you to interact with the filesystem or run commands.",
    default_prompt_steering!("`| tail -n`, `grep`, `head`")
);

/// The stock system prompt for this platform.
#[cfg(windows)]
pub const DEFAULT_PROMPT: &str = DEFAULT_PROMPT_WINDOWS;
/// The stock system prompt for this platform.
#[cfg(not(windows))]
pub const DEFAULT_PROMPT: &str = DEFAULT_PROMPT_UNIX;

/// Resolve the system prompt from CLI flag, config file, or default.
/// Priority: explicit value > ~/.synaps-cli/system.md > built-in default.
pub fn resolve_system_prompt(explicit: Option<&str>) -> String {
    if let Some(val) = explicit {
        let path = std::path::Path::new(val);
        if path.exists() && path.is_file() {
            return std::fs::read_to_string(path).unwrap_or_else(|_| val.to_string());
        }
        return val.to_string();
    }

    let system_path = resolve_read_path("system.md");
    if system_path.exists() {
        return std::fs::read_to_string(&system_path).unwrap_or_default();
    }

    DEFAULT_PROMPT.to_string()
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_family_diagnostic_is_reported_once_per_process_until_the_line_changes() {
        // Role names unique to this test: the remembered set is process-wide.
        let bad = "turn_budget.once_per_process_a.max_tool_calls = 1\n";
        let mut reports: Vec<String> = Vec::new();
        for _ in 0..2 {
            let mut config = super::SynapsConfig::default();
            super::apply_config_content_once(&mut config, bad, &mut |d: &str| reports.push(d.to_string()));
        }
        assert_eq!(reports.len(), 1, "two loads of the same bad line: {reports:?}");
        assert!(reports[0].contains("unknown turn_budget role 'once_per_process_a'"), "{reports:?}");

        // A changed line is a new message: reported, once.
        let changed = "turn_budget.once_per_process_b.max_tool_calls = 1\n";
        for _ in 0..2 {
            let mut config = super::SynapsConfig::default();
            super::apply_config_content_once(&mut config, changed, &mut |d: &str| reports.push(d.to_string()));
        }
        assert_eq!(reports.len(), 2, "{reports:?}");
        assert!(reports[1].contains("'once_per_process_b'"), "{reports:?}");
    }

    #[test]
    fn first_report_remembers_a_message_and_tells_others_apart() {
        assert!(super::first_report("first_report test: message one"));
        assert!(!super::first_report("first_report test: message one"));
        assert!(super::first_report("first_report test: message two"));
    }
    #[test]
    fn memory_only_tools_parses_a_comma_list_of_globs() {
        assert!(super::load_config_from_str("").memory_only_tools.is_empty());
        let config = super::load_config_from_str("memory_only_tools = pria-vault:*, ext__kag__*\n");
        assert_eq!(config.memory_only_tools, vec!["pria-vault:*", "ext__kag__*"]);
        assert!(config.warnings.is_empty(), "a known key: {:?}", config.warnings);
    }
    #[test]
    fn capacity_wait_defaults_on_and_parses() {
        let default = super::load_config_from_str("");
        assert!(default.capacity_wait);
        assert!(default.warnings.is_empty(), "{:?}", default.warnings);
        let off = super::load_config_from_str("capacity_wait = false\n");
        assert!(!off.capacity_wait);
        assert!(off.warnings.is_empty(), "a known key: {:?}", off.warnings);
        assert!(!super::load_config_from_str("capacity_wait = off\n").capacity_wait);
        assert!(super::load_config_from_str("capacity_wait = on\n").capacity_wait);
        let bad = super::load_config_from_str("capacity_wait = maybe\n");
        assert!(bad.capacity_wait, "invalid value keeps the default");
        assert!(bad.warnings.iter().any(|w| w.contains("capacity_wait")));
    }
    #[test]
    fn capacity_resume_defaults_on_and_parses_apart_from_capacity_wait() {
        let default = super::load_config_from_str("");
        assert!(default.capacity_resume);
        let off = super::load_config_from_str("capacity_resume = off\n");
        assert!(!off.capacity_resume);
        assert!(off.capacity_wait, "the other switch is untouched");
        assert!(off.warnings.is_empty(), "a known key: {:?}", off.warnings);
        let wait_off = super::load_config_from_str("capacity_wait = off\n");
        assert!(wait_off.capacity_resume, "independent of capacity_wait");
        assert!(!super::load_config_from_str("capacity_resume = false\n").capacity_resume);
        assert!(super::load_config_from_str("capacity_resume = on\n").capacity_resume);
        let bad = super::load_config_from_str("capacity_resume = maybe\n");
        assert!(bad.capacity_resume, "invalid value keeps the default");
        assert!(bad.warnings.iter().any(|w| w.contains("capacity_resume")));
    }

    #[test]
    fn tools_deferred_loading_defaults_on_and_parses() {
        assert!(super::load_config_from_str("").tools_deferred_loading);
        assert!(
            !super::load_config_from_str("tools.deferred_loading = off\n").tools_deferred_loading
        );
        let bad = super::load_config_from_str("tools.deferred_loading = maybe\n");
        assert!(
            bad.tools_deferred_loading,
            "invalid value keeps the default"
        );
        assert!(bad
            .warnings
            .iter()
            .any(|w| w.contains("tools.deferred_loading")));
    }

    #[test]
    fn secret_env_value_detects_credential_urls() {
        use super::is_secret_env_value as v;
        // user:password in the authority
        assert!(v("postgres://app:hunter2@db.internal:5432/prod"));
        assert!(v("redis://:pw@cache:6379/0"));
        assert!(v("http://user:pass@proxy.corp:3128"));
        assert!(v(
            "mongodb+srv://u:p@cluster0.example.net/db?retryWrites=true"
        ));
        // a key as the username, no password (Sentry DSN)
        assert!(v("https://abc123def456@o1.ingest.sentry.io/42"));
        // anywhere in the value, not only at the start; lists
        assert!(v("-Dspring.datasource.url=jdbc:postgresql://u:p@h/db"));
        assert!(v("redis://h1:6379,redis://u:p@h2:6379"));
        // no userinfo → not a secret
        assert!(!v("https://api.example.com/v1/items?q=a@b"));
        assert!(!v("http://host/path/with@sign"));
        assert!(!v("https://@host/empty-userinfo"));
        assert!(!v("git@github.com:org/repo.git"));
        assert!(!v("mailto:someone@example.com"));
        assert!(!v("someone@example.com"));
        assert!(!v("/usr/local/bin:/usr/bin"));
        assert!(!v(""));
    }

    #[test]
    fn secret_key_covers_pat_and_dsn() {
        assert!(super::is_secret_key("GITHUB_PAT"));
        assert!(super::is_secret_key("gh_pat"));
        assert!(super::is_secret_key("SENTRY_DSN"));
        assert!(!super::is_secret_key("PATH"));
        assert!(!super::is_secret_key("PATTERN_DIR"));
    }

    #[test]
    fn secret_env_is_key_or_value() {
        assert!(super::is_secret_env(
            "DATABASE_URL",
            "postgres://app:hunter2@db/prod"
        ));
        assert!(super::is_secret_env("GH_TOKEN", "anything"));
        assert!(!super::is_secret_env("DATABASE_URL", "postgres://db/prod"));
        assert!(!super::is_secret_env("HOME", "/home/u"));
    }

    #[test]
    fn daemon_reload_resume_is_on_by_default_and_off_is_the_kill_switch() {
        assert!(super::load_config_from_str("").daemon.reload_resume);
        let c = super::load_config_from_str("daemon.reload_resume = off\n");
        assert!(!c.daemon.reload_resume);
        assert!(c.warnings.is_empty(), "warnings: {:?}", c.warnings);
        assert!(super::load_config_from_str("daemon.reload_resume = on\n").daemon.reload_resume);
        // A malformed value warns and keeps the default (on).
        let c = super::load_config_from_str("daemon.reload_resume = maybe\n");
        assert!(c.daemon.reload_resume);
        assert!(
            c.warnings.iter().any(|w| w.starts_with("daemon.reload_resume")),
            "warnings: {:?}",
            c.warnings
        );
    }

    #[test]
    fn daemon_crash_restore_is_on_by_default_and_off_is_the_kill_switch() {
        assert!(super::load_config_from_str("").daemon.crash_restore);
        let c = super::load_config_from_str("daemon.crash_restore = off\n");
        assert!(!c.daemon.crash_restore);
        assert!(c.warnings.is_empty(), "warnings: {:?}", c.warnings);
        assert!(super::load_config_from_str("daemon.crash_restore = on\n").daemon.crash_restore);
        // A malformed value warns and keeps the default (on).
        let c = super::load_config_from_str("daemon.crash_restore = maybe\n");
        assert!(c.daemon.crash_restore);
        assert!(
            c.warnings.iter().any(|w| w.starts_with("daemon.crash_restore")),
            "warnings: {:?}",
            c.warnings
        );
    }
    #[test]
    fn startup_and_daemon_defaults() {
        let c = super::load_config_from_str("");
        assert!(c.startup.quick_start, "quick_start defaults ON");
        assert_eq!(c.startup.extensions_ready_timeout_secs, 30);
        assert_eq!(c.daemon.idle_exit_secs, 10);
        assert_eq!(c.daemon.prompt_abandon_secs, 3600);
        assert_eq!(c.daemon.parked_evict_secs, 3600);
        // No warnings for the absent keys.
        assert!(c.warnings.is_empty(), "warnings: {:?}", c.warnings);
    }

    #[test]
    fn startup_and_daemon_parse_all_keys() {
        let c = super::load_config_from_str(concat!(
            "startup.quick_start = off\n",
            "startup.extensions_ready_timeout_secs = 45\n",
            "daemon.idle_exit_secs = 20\n",
            "daemon.prompt_abandon_secs = 0\n",
            "daemon.parked_evict_secs = 120\n",
        ));
        assert!(!c.startup.quick_start);
        assert_eq!(c.startup.extensions_ready_timeout_secs, 45);
        assert_eq!(c.daemon.idle_exit_secs, 20);
        assert_eq!(c.daemon.prompt_abandon_secs, 0);
        assert_eq!(c.daemon.parked_evict_secs, 120);
        assert!(
            !c.warnings.iter().any(|w| w.contains("unknown")),
            "no unknown-key warning: {:?}",
            c.warnings
        );
    }

    #[test]
    fn subagent_limits_default_to_16_64_4() {
        let c = super::load_config_from_str("");
        assert_eq!(c.subagent, super::SubagentLimits::default());
        assert_eq!(c.subagent.max_concurrent, 16);
        assert_eq!(c.subagent.max_total, 64);
        assert_eq!(c.subagent.max_depth, 4);
        assert!(c.warnings.is_empty(), "warnings: {:?}", c.warnings);
    }

    #[test]
    fn subagent_tool_hooks_defaults_on_and_parses_on_off() {
        assert!(super::load_config_from_str("").subagent.tool_hooks);
        for (value, expected) in [("off", false), ("on", true), ("false", false), ("true", true)] {
            let c = super::load_config_from_str(&format!("subagent.tool_hooks = {value}\n"));
            assert_eq!(c.subagent.tool_hooks, expected, "{value}");
            assert!(c.warnings.is_empty(), "{value}: {:?}", c.warnings);
        }
        let bad = super::load_config_from_str("subagent.tool_hooks = maybe\n");
        assert!(bad.subagent.tool_hooks, "an invalid value keeps the default");
        assert!(
            bad.warnings.iter().any(|w| w.contains("subagent.tool_hooks")),
            "{:?}",
            bad.warnings
        );
        assert!(super::KNOWN_CONFIG_KEYS.contains(&"subagent.tool_hooks"));
    }

    #[test]
    fn subagent_limits_parse_and_are_known_keys() {
        let c = super::load_config_from_str(concat!(
            "subagent.max_concurrent = 24\n",
            "subagent.max_total = 100\n",
            "subagent.max_depth = 2\n",
            "subagent_timeout = 600\n",
        ));
        assert_eq!(c.subagent.max_concurrent, 24);
        assert_eq!(c.subagent.max_total, 100);
        assert_eq!(c.subagent.max_depth, 2);
        assert_eq!(c.subagent_timeout, 600, "the old flat key is unaffected");
        assert!(c.warnings.is_empty(), "warnings: {:?}", c.warnings);
        for k in [
            "subagent.max_concurrent",
            "subagent.max_total",
            "subagent.max_depth",
        ] {
            assert!(
                super::KNOWN_CONFIG_KEYS.contains(&k),
                "{k} not in KNOWN_CONFIG_KEYS"
            );
        }
    }

    #[test]
    fn subagent_cold_resume_compact_tokens_parses_defaults_and_warns() {
        let c = super::load_config_from_str("");
        assert_eq!(c.subagent_cold_resume_compact_tokens, 100_000);
        assert_eq!(
            super::SynapsConfig::default().subagent_cold_resume_compact_tokens,
            super::DEFAULT_SUBAGENT_COLD_RESUME_COMPACT_TOKENS
        );
        let c = super::load_config_from_str("subagent_cold_resume_compact_tokens = 250000\n");
        assert_eq!(c.subagent_cold_resume_compact_tokens, 250_000);
        assert!(c.warnings.is_empty(), "warnings: {:?}", c.warnings);
        let c = super::load_config_from_str("subagent_cold_resume_compact_tokens = 0\n");
        assert_eq!(c.subagent_cold_resume_compact_tokens, 0, "0 disables");
        assert!(c.warnings.is_empty(), "warnings: {:?}", c.warnings);
        let c = super::load_config_from_str("subagent_cold_resume_compact_tokens = lots\n");
        assert_eq!(c.subagent_cold_resume_compact_tokens, 100_000);
        assert!(
            c.warnings
                .iter()
                .any(|w| w.contains("subagent_cold_resume_compact_tokens")),
            "warnings: {:?}",
            c.warnings
        );
        assert!(super::KNOWN_CONFIG_KEYS.contains(&"subagent_cold_resume_compact_tokens"));
    }

    #[test]
    fn subagent_transform_hooks_key_parses_defaults_on_and_warns() {
        assert!(super::load_config_from_str("").subagent_transform_hooks, "default on");
        assert!(super::SynapsConfig::default().subagent_transform_hooks);
        for (line, want) in [
            ("subagent_transform_hooks = off\n", false),
            ("subagent_transform_hooks = false\n", false),
            ("subagent_transform_hooks = on\n", true),
            ("subagent_transform_hooks = true\n", true),
        ] {
            let c = super::load_config_from_str(line);
            assert_eq!(c.subagent_transform_hooks, want, "{line}");
            assert!(c.warnings.is_empty(), "{line}: {:?}", c.warnings);
        }
        let c = super::load_config_from_str("subagent_transform_hooks = maybe\n");
        assert!(c.subagent_transform_hooks, "bad value keeps the default");
        assert!(c.warnings.iter().any(|w| w.contains("subagent_transform_hooks")));
    }

    #[test]
    fn subagent_rollover_keys_parse_default_and_warn() {
        use super::ContextManagementMode;
        let c = super::load_config_from_str("");
        assert_eq!(c.subagent_context_management, ContextManagementMode::Auto);
        assert_eq!(c.subagent_rollover_tokens, 200_000);
        let d = super::SynapsConfig::default();
        assert_eq!(d.subagent_context_management, ContextManagementMode::Auto);
        assert_eq!(d.subagent_rollover_tokens, super::DEFAULT_SUBAGENT_ROLLOVER_TOKENS);

        let c = super::load_config_from_str(
            "subagent_context_management = OFF\nsubagent_rollover_tokens = 150000\n",
        );
        assert_eq!(c.subagent_context_management, ContextManagementMode::Off);
        assert_eq!(c.subagent_rollover_tokens, 150_000);
        assert!(c.warnings.is_empty(), "warnings: {:?}", c.warnings);
        let c = super::load_config_from_str("subagent_rollover_tokens = 20000\n");
        assert_eq!(c.subagent_rollover_tokens, 20_000, "the floor itself is accepted");
        assert!(c.warnings.is_empty(), "warnings: {:?}", c.warnings);

        for (line, key) in [
            ("subagent_context_management = sometimes", "subagent_context_management"),
            ("subagent_rollover_tokens = lots", "subagent_rollover_tokens"),
            ("subagent_rollover_tokens = 19999", "subagent_rollover_tokens"),
            ("subagent_rollover_tokens = 0", "subagent_rollover_tokens"),
        ] {
            let c = super::load_config_from_str(line);
            assert_eq!(c.subagent_context_management, ContextManagementMode::Auto, "{line}");
            assert_eq!(c.subagent_rollover_tokens, 200_000, "{line}");
            assert!(
                c.warnings.iter().any(|w| w.contains(key)),
                "{line}: warnings {:?}",
                c.warnings
            );
        }
        for key in ["subagent_context_management", "subagent_rollover_tokens"] {
            assert!(super::KNOWN_CONFIG_KEYS.contains(&key), "{key}");
        }
    }

    #[test]
    fn legacy_flat_subagent_limit_keys_alias_the_dotted_keys() {
        let c = super::load_config_from_str(concat!(
            "subagent_max_concurrent = 256\n",
            "subagent_max_total = 1024\n",
        ));
        assert_eq!(c.subagent.max_concurrent, 256);
        assert_eq!(c.subagent.max_total, 1024);
        assert!(c.warnings.is_empty(), "warnings: {:?}", c.warnings);
        let c = super::load_config_from_str("subagent_max_concurrent = 0\n");
        assert_eq!(c.subagent, super::SubagentLimits::default());
        assert!(c.warnings.iter().any(|w| w.starts_with("subagent_max_concurrent")));
    }

    #[test]
    fn subagent_limits_reject_zero_and_junk_and_keep_defaults() {
        let c = super::load_config_from_str(concat!(
            "subagent.max_concurrent = 0\n",
            "subagent.max_total = lots\n",
            "subagent.max_depth = 70000\n",
            "subagent.max_children = 4\n",
        ));
        assert_eq!(c.subagent, super::SubagentLimits::default());
        for key in [
            "subagent.max_concurrent",
            "subagent.max_total",
            "subagent.max_depth",
            "subagent.max_children",
        ] {
            assert!(
                c.warnings.iter().any(|w| w.starts_with(key)),
                "expected a warning for {key}: {:?}",
                c.warnings
            );
        }
    }

    #[test]
    fn subagent_concurrency_above_total_uses_the_stricter_number() {
        // Either key order: the check runs after the whole file is parsed.
        for text in [
            "subagent.max_total = 10\nsubagent.max_concurrent = 32\n",
            "subagent.max_concurrent = 32\nsubagent.max_total = 10\n",
        ] {
            let c = super::load_config_from_str(text);
            assert_eq!(c.subagent.max_concurrent, 10);
            assert_eq!(c.subagent.max_total, 10);
            assert!(
                c.warnings
                    .iter()
                    .any(|w| w.contains("subagent.max_concurrent (32)")),
                "{:?}",
                c.warnings
            );
        }
        // Lowering only the total below the default concurrency also clamps.
        let c = super::load_config_from_str("subagent.max_total = 5\n");
        assert_eq!(c.subagent.max_concurrent, 5);
    }

    #[test]
    fn subagent_context_limits_default_to_150k_warn_and_250k_refuse() {
        let c = super::load_config_from_str("");
        assert_eq!(c.subagent.context_warn_tokens, Some(150_000));
        assert_eq!(c.subagent.resume_refuse_tokens, Some(250_000));
    }

    #[test]
    fn subagent_context_limits_parse_off_and_are_known_keys() {
        let c = super::load_config_from_str(concat!(
            "subagent.context_warn_tokens = 120000\n",
            "subagent.resume_refuse_tokens = 400000\n",
        ));
        assert_eq!(c.subagent.context_warn_tokens, Some(120_000));
        assert_eq!(c.subagent.resume_refuse_tokens, Some(400_000));
        assert!(c.warnings.is_empty(), "warnings: {:?}", c.warnings);
        let off = super::load_config_from_str(concat!(
            "subagent.context_warn_tokens = off\n",
            "subagent.resume_refuse_tokens = OFF\n",
        ));
        assert_eq!(off.subagent.context_warn_tokens, None);
        assert_eq!(off.subagent.resume_refuse_tokens, None);
        assert!(off.warnings.is_empty(), "warnings: {:?}", off.warnings);
        for k in [
            "subagent.context_warn_tokens",
            "subagent.resume_refuse_tokens",
        ] {
            assert!(
                super::KNOWN_CONFIG_KEYS.contains(&k),
                "{k} not in KNOWN_CONFIG_KEYS"
            );
        }
    }

    #[test]
    fn subagent_context_limits_reject_junk_and_keep_defaults() {
        let c = super::load_config_from_str(concat!(
            "subagent.context_warn_tokens = 0\n",
            "subagent.resume_refuse_tokens = lots\n",
        ));
        assert_eq!(c.subagent.context_warn_tokens, Some(150_000));
        assert_eq!(c.subagent.resume_refuse_tokens, Some(250_000));
        for key in [
            "subagent.context_warn_tokens",
            "subagent.resume_refuse_tokens",
        ] {
            assert!(
                c.warnings.iter().any(|w| w.starts_with(key)),
                "expected a warning for {key}: {:?}",
                c.warnings
            );
        }
    }

    #[test]
    fn subagent_warn_line_above_the_refuse_line_is_clamped() {
        // Either key order: the check runs after the whole file is parsed.
        for text in [
            "subagent.resume_refuse_tokens = 100000\nsubagent.context_warn_tokens = 180000\n",
            "subagent.context_warn_tokens = 180000\nsubagent.resume_refuse_tokens = 100000\n",
        ] {
            let c = super::load_config_from_str(text);
            assert_eq!(c.subagent.context_warn_tokens, Some(100_000));
            assert_eq!(c.subagent.resume_refuse_tokens, Some(100_000));
            assert_eq!(c.subagent_context_clamp, Some((180_000, 100_000)));
            assert!(
                c.warnings
                    .iter()
                    .any(|w| w.contains("subagent.context_warn_tokens (180000)")),
                "{:?}",
                c.warnings
            );
        }
        // Lowering only the refuse line below the default warn line clamps.
        let c = super::load_config_from_str("subagent.resume_refuse_tokens = 90000\n");
        assert_eq!(c.subagent.context_warn_tokens, Some(90_000));
        assert_eq!(c.subagent_context_clamp, Some((150_000, 90_000)));
        // A line that is off is never clamped against.
        let c = super::load_config_from_str(concat!(
            "subagent.resume_refuse_tokens = off\n",
            "subagent.context_warn_tokens = 900000\n",
        ));
        assert_eq!(c.subagent.context_warn_tokens, Some(900_000));
        assert_eq!(c.subagent.resume_refuse_tokens, None);
        assert_eq!(c.subagent_context_clamp, None);
        assert!(c.warnings.is_empty(), "{:?}", c.warnings);
    }

    #[test]
    fn subagent_context_rollover_is_inherit_unless_set_off() {
        use super::ContextRollover::{Inherit, Off};
        let c = super::load_config_from_str("");
        assert_eq!(c.subagent.context_rollover, Inherit);
        for (text, want) in [
            ("subagent.context_rollover = inherit\n", Inherit),
            ("subagent.context_rollover = off\n", Off),
            ("subagent.context_rollover =   off  \n", Off),
        ] {
            let c = super::load_config_from_str(text);
            assert_eq!(c.subagent.context_rollover, want, "{text:?}");
            assert!(c.warnings.is_empty(), "{text:?}: {:?}", c.warnings);
        }
        assert!(super::KNOWN_CONFIG_KEYS.contains(&"subagent.context_rollover"));
    }

    /// Pin d (config side): `subagent.context_target_tokens` is unset
    /// (`auto`) by default; a positive count sets it and `auto` clears it;
    /// `0`, a negative number and junk warn like the sibling keys and leave
    /// `auto`, never an earlier line's value. A known key, but not one
    /// `config set` may write.
    #[test]
    fn subagent_context_target_tokens_parses_and_rejects_zero_negative_and_junk() {
        assert_eq!(
            super::load_config_from_str("")
                .subagent
                .context_target_tokens,
            None
        );
        for (text, want) in [
            ("subagent.context_target_tokens = 48000\n", Some(48_000)),
            ("subagent.context_target_tokens =  45000 \n", Some(45_000)),
            ("subagent.context_target_tokens = auto\n", None),
            (
                "subagent.context_target_tokens = 48000\nsubagent.context_target_tokens = AUTO\n",
                None,
            ),
        ] {
            let c = super::load_config_from_str(text);
            assert_eq!(c.subagent.context_target_tokens, want, "{text:?}");
            assert!(c.warnings.is_empty(), "{text:?}: {:?}", c.warnings);
        }
        for value in ["0", "-48000", "48k", "lots", ""] {
            let c = super::load_config_from_str(&format!(
                "subagent.context_target_tokens = 50000\nsubagent.context_target_tokens = {value}\n"
            ));
            assert_eq!(c.subagent.context_target_tokens, None, "{value:?} is auto");
            assert_eq!(
                c.warnings
                    .iter()
                    .filter(|w| w.starts_with("subagent.context_target_tokens"))
                    .count(),
                1,
                "{value:?}: {:?}",
                c.warnings
            );
        }
        assert!(super::KNOWN_CONFIG_KEYS.contains(&"subagent.context_target_tokens"));
        assert!(!super::SETTABLE_CONFIG_KEYS.contains(&"subagent.context_target_tokens"));
    }

    /// A malformed opt-out is never read as "inherit": worker rollover is
    /// off, with a warning, and the other subagent keys are untouched.
    #[test]
    fn a_malformed_subagent_context_rollover_is_off_with_a_warning() {
        for value in ["of", "on", "auto", "OFF", "inherit off"] {
            let c = super::load_config_from_str(&format!(
                "subagent.context_rollover = {value}\nsubagent.max_total = 9\n"
            ));
            assert_eq!(
                c.subagent.context_rollover,
                super::ContextRollover::Off,
                "{value:?}"
            );
            assert!(
                c.warnings
                    .iter()
                    .any(|w| w.starts_with("subagent.context_rollover")),
                "{value:?}: {:?}",
                c.warnings
            );
            assert_eq!(c.subagent.max_total, 9, "{value:?}");
        }
    }

    #[test]
    fn process_scopes_defaults_on_and_parses_auto_and_off() {
        assert!(super::load_config_from_str("").process_scopes);
        assert!(super::load_config_from_str("process_scopes = auto\n").process_scopes);
        let off = super::load_config_from_str("process_scopes = off\n");
        assert!(!off.process_scopes);
        assert!(off.warnings.is_empty(), "{:?}", off.warnings);
        let bad = super::load_config_from_str("process_scopes = sometimes\n");
        assert!(bad.process_scopes, "an invalid value keeps the default");
        assert!(bad.warnings.iter().any(|w| w.starts_with("process_scopes")));
        assert!(super::KNOWN_CONFIG_KEYS.contains(&"process_scopes"));
    }

    #[test]
    fn an_invalid_repeated_process_scopes_keeps_the_previous_value() {
        let c = super::load_config_from_str("process_scopes = off\nprocess_scopes = sometimes\n");
        assert!(
            c.warnings.iter().any(|w| w.starts_with("process_scopes = sometimes") && w.contains("keeping the previous value")),
            "{:?}",
            c.warnings
        );
        assert!(!c.process_scopes, "the earlier valid value is kept");
    }

    #[test]
    fn an_invalid_repeated_scope_slice_keeps_the_previous_value() {
        for key in ["tool_scope_slice", "daemon_scope_slice"] {
            let c = super::load_config_from_str(&format!("{key} = a.slice\n{key} = worker\n"));
            assert!(
                c.warnings.iter().any(|w| w.starts_with(&format!("{key} = worker")) && w.contains("keeping the previous value")),
                "{key}: {:?}",
                c.warnings
            );
            let kept = if key == "tool_scope_slice" { &c.tool_scope_slice } else { &c.daemon_scope_slice };
            assert_eq!(kept.as_deref(), Some("a.slice"), "{key}: the earlier valid value is kept");
        }
    }

    #[test]
    fn scope_slices_default_unset_and_parse_valid_names() {
        let c = super::load_config_from_str("");
        assert_eq!((c.tool_scope_slice, c.daemon_scope_slice), (None, None));
        let c = super::load_config_from_str(
            "tool_scope_slice = synaps_tools.slice\ndaemon_scope_slice = synaps:daemon_1.slice\n",
        );
        assert!(c.warnings.is_empty(), "{:?}", c.warnings);
        assert_eq!(c.tool_scope_slice.as_deref(), Some("synaps_tools.slice"));
        assert_eq!(c.daemon_scope_slice.as_deref(), Some("synaps:daemon_1.slice"));
        // A nested name is valid too (the docs say why a flat one is better).
        let c = super::load_config_from_str("tool_scope_slice = synaps-tools.slice\n");
        assert_eq!((c.warnings.len(), c.tool_scope_slice.as_deref()), (0, Some("synaps-tools.slice")));
        // Each key sets only its own field.
        let c = super::load_config_from_str("tool_scope_slice = a.slice\n");
        assert_eq!((c.tool_scope_slice.as_deref(), c.daemon_scope_slice.as_deref()), (Some("a.slice"), None));
        let c = super::load_config_from_str("daemon_scope_slice = d.slice\n");
        assert_eq!((c.tool_scope_slice.as_deref(), c.daemon_scope_slice.as_deref()), (None, Some("d.slice")));
        let longest = format!("{}.slice", "a".repeat(255 - ".slice".len()));
        assert_eq!(super::scope_slice_problem(&longest), None);
        for k in ["tool_scope_slice", "daemon_scope_slice"] {
            assert!(super::KNOWN_CONFIG_KEYS.contains(&k), "{k} not in KNOWN_CONFIG_KEYS");
        }
    }

    #[test]
    fn an_invalid_scope_slice_warns_and_keeps_the_default() {
        let too_long = format!("{}.slice", "a".repeat(256 - ".slice".len()));
        // Every invalid value the function or the parser lets through.
        let mut accepted: Vec<String> = Vec::new();
        for bad in [
            "worker",            // no .slice suffix
            "worker.service",    // another unit type
            "a/b.slice",         // a path
            "work er.slice",     // a space
            "work@er.slice",     // outside [A-Za-z0-9:_.-]
            "-.slice",           // the root slice
            "-worker.slice",     // a leading '-'
            "--worker.slice",
            ".slice",            // no name
            "worker--agent.slice",
            "worker-.slice",
            too_long.as_str(),   // 256 bytes
            "wörker.slice",      // non-ASCII
            "worker\0.slice",    // NUL
            "\"worker-agent.slice\"", // quoted: the file is not TOML
            "'worker-agent.slice'",
            "5",                 // other types: a number, a boolean
            "true",
        ] {
            if super::scope_slice_problem(bad).is_none() {
                accepted.push(bad.to_string());
            }
            for key in ["tool_scope_slice", "daemon_scope_slice"] {
                let c = super::load_config_from_str(&format!("{key} = {bad}\n"));
                let unset = (c.tool_scope_slice.as_deref(), c.daemon_scope_slice.as_deref()) == (None, None);
                if !unset || !c.warnings.iter().any(|w| w.starts_with(&format!("{key} = "))) {
                    accepted.push(format!("{key} = {bad}"));
                }
            }
        }
        assert!(accepted.is_empty(), "accepted: {accepted:?}");
    }

    #[test]
    fn an_empty_scope_slice_unsets_the_key_without_a_warning() {
        for key in ["tool_scope_slice", "daemon_scope_slice"] {
            for empty in [format!("{key} ="), format!("{key} =   "), format!("  {key}=")] {
                // Alone: unset, silently.
                let c = super::load_config_from_str(&format!("{empty}\n"));
                assert!(c.warnings.is_empty(), "{empty:?}: {:?}", c.warnings);
                assert_eq!((c.tool_scope_slice.as_deref(), c.daemon_scope_slice.as_deref()), (None, None), "{empty:?}");
                // After a valid line: that line is undone, the other key kept.
                let c = super::load_config_from_str(&format!(
                    "tool_scope_slice = synaps_tools.slice\ndaemon_scope_slice = synaps_daemon.slice\n{empty}\n"
                ));
                assert!(c.warnings.is_empty(), "{empty:?}: {:?}", c.warnings);
                let (unset, kept) = if key == "tool_scope_slice" {
                    (&c.tool_scope_slice, c.daemon_scope_slice.as_deref())
                } else {
                    (&c.daemon_scope_slice, c.tool_scope_slice.as_deref())
                };
                assert_eq!(unset, &None, "{empty:?}");
                assert!(kept.is_some_and(|k| k.starts_with("synaps_")), "{empty:?}: {kept:?}");
            }
        }
    }

    #[test]
    fn a_quoted_scope_slice_says_to_drop_the_quotes() {
        for quoted in ["\"worker-agent.slice\"", "'worker-agent.slice'", "\"worker-agent.slice", "worker-agent.slice'"] {
            let why = super::scope_slice_problem(quoted);
            assert!(why.is_some_and(|w| w.contains("without quotes")), "{quoted}: {why:?}");
        }
    }

    #[test]
    fn startup_quick_start_accepts_on() {
        let c = super::load_config_from_str("startup.quick_start = on\n");
        assert!(c.startup.quick_start);
    }

    #[test]
    fn startup_and_daemon_keys_are_known() {
        for k in [
            "startup.quick_start",
            "startup.extensions_ready_timeout_secs",
            "daemon.idle_exit_secs",
            "daemon.prompt_abandon_secs",
            "daemon.parked_evict_secs",
        ] {
            assert!(super::KNOWN_CONFIG_KEYS.contains(&k), "{k} not in KNOWN_CONFIG_KEYS");
        }
    }

    #[test]
    fn startup_daemon_bad_values_warn_and_keep_defaults() {
        let c = super::load_config_from_str(concat!(
            "startup.quick_start = maybe\n",
            "daemon.idle_exit_secs = soon\n",
        ));
        assert!(c.startup.quick_start, "invalid bool keeps default");
        assert_eq!(c.daemon.idle_exit_secs, 10, "invalid u64 keeps default");
        assert!(c.warnings.iter().any(|w| w.contains("startup.quick_start")));
        assert!(c.warnings.iter().any(|w| w.contains("daemon.idle_exit_secs")));
    }

    #[test]
    fn daemon_umask_is_a_known_key() {
        assert!(super::KNOWN_CONFIG_KEYS.contains(&"daemon.umask"));
    }

    /// `daemon.umask`: `inherit` or an octal mode of 1-4 digits up to 0777,
    /// optionally in matching quotes (an installer may quote it).
    #[test]
    fn daemon_umask_accepts_inherit_and_octal_modes() {
        for v in ["inherit", "INHERIT", "022", "0022", "\"022\"", "'0027'", "0", "0777", "7"] {
            let problems = super::config_content_problems(&format!("daemon.umask = {v}\n"));
            assert!(problems.is_empty(), "daemon.umask = {v}: {problems:?}");
        }
    }

    #[test]
    fn daemon_umask_rejects_malformed_values() {
        for v in [
            "", "0778", "1777", "8", "00022", "abc", "0o22", "0x12", "-022", "+022", "22a",
            "\"022'", "\"\"", "u=rwx", "0 22",
        ] {
            let problems = super::config_content_problems(&format!("daemon.umask = {v}\n"));
            assert!(
                problems.iter().any(|p| p.starts_with("daemon.umask")),
                "daemon.umask = {v} must warn: {problems:?}"
            );
        }
    }

    #[test]
    fn daemon_umask_parses_to_a_mode_and_a_malformed_value_inherits() {
        use super::DaemonUmask::{Inherit, Mode};
        assert_eq!(super::load_config_from_str("").daemon.umask, Inherit);
        for (v, want) in [
            ("inherit", Inherit),
            ("022", Mode(0o022)),
            ("\"0022\"", Mode(0o022)),
            ("'027'", Mode(0o027)),
            ("0", Mode(0)),
            ("0777", Mode(0o777)),
        ] {
            let c = super::load_config_from_str(&format!("daemon.umask = {v}\n"));
            assert_eq!(c.daemon.umask, want, "{v}");
        }
        // A malformed later line inherits, also over an earlier valid one.
        let c = super::load_config_from_str("daemon.umask = 022\ndaemon.umask = 0778\n");
        assert_eq!(c.daemon.umask, Inherit);
        let c = super::load_config_from_str("model = x\ndaemon.umask = 0778\n");
        assert_eq!(c.daemon.umask, Inherit);
        assert!(c.daemon_umask_warning().is_some_and(|w| w.starts_with("daemon.umask — ")));
        // Other keys' problems are not the umask's.
        let c = super::load_config_from_str("daemon.umask = 022\nthinking = bogus\n");
        assert_eq!((c.daemon.umask, c.daemon_umask_warning()), (Mode(0o022), None));
        assert!(!c.warnings.is_empty(), "the thinking warning is there");
    }

    #[test]
    fn context_management_defaults_auto_and_parses_all_keys() {
        use super::{ContextManagementConfig, ContextManagementMode};
        // Unset (or no config file at all): on where the archive is
        // supported (Unix), with every other default.
        let unset = super::load_config_from_str("").context_management;
        let expected = if cfg!(unix) { ContextManagementMode::Auto } else { ContextManagementMode::Off };
        assert_eq!(unset.mode, expected);
        assert_eq!(ContextManagementMode::CONFIG_DEFAULT, expected);
        assert_eq!(unset, ContextManagementConfig::config_default());
        assert_eq!(super::SynapsConfig::default().context_management, unset);
        assert_eq!(
            ContextManagementConfig { mode: ContextManagementMode::Off, ..unset },
            ContextManagementConfig::default(),
            "only the mode differs from the dark runtime default"
        );
        // An explicit `off` still opts out, without a warning.
        let off = super::load_config_from_str("context_management.mode = off\n");
        assert_eq!(off.context_management.mode, ContextManagementMode::Off);
        assert!(off.warnings.is_empty(), "{:?}", off.warnings);
        let parsed = super::load_config_from_str(
            "context_management.mode = AUTO\n\
             context_management.pressure_tokens = 250000\n\
             context_management.rollover_tokens = 400000\n\
             context_management.reserve_tokens = 24000\n\
             context_management.finish_rounds = 3\n\
             context_window = 1m\n",
        );
        assert_eq!(
            parsed.context_management,
            ContextManagementConfig {
                mode: ContextManagementMode::Auto,
                pressure_tokens: Some(250_000),
                rollover_tokens: Some(400_000),
                reserve_tokens: 24_000,
                finish_rounds: 3,
                target_tokens: None,
                summary_tokens: None,
                summary_timeout_secs: ContextManagementConfig::DEFAULT_SUMMARY_TIMEOUT_SECS,
            }
        );
        assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
        assert!(parsed.provider_keys.is_empty());
        assert!(super::load_config_from_str("context_management.mode = off")
            .warnings
            .is_empty());
    }

    #[test]
    fn context_management_auto_clears_overrides_and_allows_bounded_extremes() {
        for rounds in [0, super::ContextManagementConfig::MAX_FINISH_ROUNDS] {
            let parsed = super::load_config_from_str(&format!(
                "context_management.mode = auto\n\
                 context_management.pressure_tokens = 300000\n\
                 context_management.rollover_tokens = 450000\n\
                 context_management.pressure_tokens = auto\n\
                 context_management.rollover_tokens = AUTO\n\
                 context_management.finish_rounds = {rounds}\n"
            ));
            assert_eq!(parsed.context_management.pressure_tokens, None);
            assert_eq!(parsed.context_management.rollover_tokens, None);
            assert_eq!(parsed.context_management.finish_rounds, rounds);
            assert!(parsed.warnings.is_empty());
        }
    }

    #[test]
    fn context_management_invalid_values_reset_entire_policy() {
        for (key, value) in [
            ("mode", "on"),
            ("pressure_tokens", "0"),
            ("pressure_tokens", "-1"),
            ("pressure_tokens", "250k"),
            ("pressure_tokens", "18446744073709551616"),
            ("rollover_tokens", "1"),
            ("rollover_tokens", "nope"),
            ("reserve_tokens", "0"),
            ("reserve_tokens", "1000001"),
            ("reserve_tokens", "auto"),
            ("finish_rounds", "9"),
            ("finish_rounds", "-1"),
            ("finish_rounds", "4294967296"),
            ("finish_rounds", ""),
            ("mod", "auto"),
        ] {
            let parsed = super::load_config_from_str(&format!(
                "context_management.mode = auto\ncontext_management.{key} = {value}\n"
            ));
            assert_eq!(
                parsed.context_management,
                super::ContextManagementConfig::default(),
                "{key}={value}"
            );
            assert!(!parsed.warnings.is_empty(), "{key}={value}");
            assert!(parsed.provider_keys.is_empty());
        }
        let duplicate = super::load_config_from_str(
            "context_management.mode = auto\ncontext_management.finish_rounds = bad\ncontext_management.finish_rounds = 2"
        );
        assert_eq!(
            duplicate.context_management,
            super::ContextManagementConfig::default()
        );
    }

    #[test]
    fn context_management_target_tokens_parses_and_validates() {
        assert_eq!(
            super::load_config_from_str("")
                .context_management
                .target_tokens,
            None
        );
        let parsed = super::load_config_from_str(
            "context_management.mode = auto\ncontext_management.target_tokens = 120000\ncontext_window = 1m\n",
        );
        assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
        assert_eq!(parsed.context_management.target_tokens, Some(120_000));
        let auto = super::load_config_from_str(
            "context_management.target_tokens = 5000\ncontext_management.target_tokens = auto\n",
        );
        assert_eq!(auto.context_management.target_tokens, None);
        let summary = super::load_config_from_str(
            "context_management.mode = auto\ncontext_management.summary_tokens = 20000\ncontext_management.summary_timeout_secs = 30\ncontext_window = 1m\n",
        );
        assert!(summary.warnings.is_empty(), "{:?}", summary.warnings);
        assert_eq!(summary.context_management.summary_tokens, Some(20_000));
        assert_eq!(summary.context_management.summary_timeout_secs, 30);
        let defaults = super::load_config_from_str("").context_management;
        assert_eq!(defaults.summary_tokens, None, "auto by default");
        assert_eq!(super::load_config_from_str("").summary_model, None, "auto");
        for (line, model) in [
            ("context_management.summary_model = auto", None),
            ("context_management.summary_model = AUTO", None),
            (
                "context_management.summary_model = groq/llama-3.3-70b-versatile",
                Some("groq/llama-3.3-70b-versatile"),
            ),
            (
                "context_management.summary_model = claude-haiku-4-5",
                Some("claude-haiku-4-5"),
            ),
        ] {
            let parsed =
                super::load_config_from_str(&format!("context_management.mode = auto\n{line}\n"));
            assert!(parsed.warnings.is_empty(), "{line}: {:?}", parsed.warnings);
            assert_eq!(parsed.summary_model.as_deref(), model, "{line}");
            assert!(!parsed.summary_model_invalid, "{line}");
            assert_eq!(
                parsed.context_management.mode,
                super::ContextManagementMode::Auto
            );
        }
        for bad in ["", "two words", "tab\there"] {
            let parsed = super::load_config_from_str(&format!(
                "context_management.mode = auto\ncontext_management.summary_model = {bad}\n"
            ));
            assert_eq!(parsed.summary_model, None, "{bad:?} names no model");
            assert!(
                parsed.summary_model_invalid,
                "{bad:?} turns the summary off, never auto"
            );
            assert!(
                parsed
                    .warnings
                    .iter()
                    .any(|w| w.contains("context_management.summary_model")),
                "{bad:?}: {:?}",
                parsed.warnings
            );
            assert_eq!(
                parsed.context_management.mode,
                super::ContextManagementMode::Auto,
                "an invalid summary model does not disable context management"
            );
        }
        // Warm summaries at xhigh/max effort run to cap + 32k output tokens;
        // ~110 tokens/s was observed, so 120 s ends long ones early.
        assert_eq!(defaults.summary_timeout_secs, 300);
        // An explicit key still wins, including the old default.
        let explicit = super::load_config_from_str(
            "context_management.mode = auto\ncontext_management.summary_timeout_secs = 120\ncontext_window = 1m\n",
        );
        assert!(explicit.warnings.is_empty(), "{:?}", explicit.warnings);
        assert_eq!(explicit.context_management.summary_timeout_secs, 120);
        let max = super::load_config_from_str(
            "context_management.mode = auto\ncontext_management.summary_timeout_secs = 600\ncontext_window = 1m\n",
        );
        assert_eq!(max.context_management.summary_timeout_secs, 600);
        for fields in [
            "target_tokens = 0",
            "target_tokens = nope",
            "target_tokens = 250000",
            "pressure_tokens = 200000\ncontext_management.target_tokens = 200000",
            "summary_tokens = nope",
            "summary_tokens = 100000",
            "target_tokens = 50000\ncontext_management.summary_tokens = 50000",
            "summary_timeout_secs = 0",
            "summary_timeout_secs = 601",
            "summary_timeout_secs = soon",
        ] {
            let parsed = super::load_config_from_str(&format!(
                "context_management.mode = auto\ncontext_management.{fields}\ncontext_window = 1m\n"
            ));
            assert_eq!(
                parsed.context_management,
                super::ContextManagementConfig::default(),
                "{fields}"
            );
            assert!(!parsed.warnings.is_empty(), "{fields}");
        }
    }

    #[test]
    fn context_management_validates_final_threshold_order_and_window() {
        for fields in [
            "pressure_tokens = 400000\ncontext_management.rollover_tokens = 400000",
            "pressure_tokens = 500000\ncontext_management.rollover_tokens = 400000",
            "pressure_tokens = 1000000",
            "rollover_tokens = 1000001",
        ] {
            for window_first in [false, true] {
                let policy =
                    format!("context_management.mode = auto\ncontext_management.{fields}\n");
                let content = if window_first {
                    format!("context_window = 1m\n{policy}")
                } else {
                    format!("{policy}context_window = 1m\n")
                };
                let parsed = super::load_config_from_str(&content);
                assert_eq!(
                    parsed.context_management,
                    super::ContextManagementConfig::default()
                );
                assert!(!parsed.warnings.is_empty());
            }
        }
        // Temporarily reversed overrides are valid once the whole file is parsed.
        let parsed = super::load_config_from_str(
            "context_management.mode = auto\n\
             context_management.rollover_tokens = 100000\n\
             context_management.pressure_tokens = 300000\n\
             context_management.rollover_tokens = 450000\ncontext_window = 1m",
        );
        assert!(parsed.warnings.is_empty());
        assert_eq!(parsed.context_management.rollover_tokens, Some(450_000));
    }

    #[test]
    fn context_management_parsed_finish_override_bounds_rollover_extension() {
        use crate::core::context_policy::{
            assess_context, ContextAction, ContextBudget, ContextState, WorkPhase,
        };
        let config = super::load_config_from_str(
            "context_management.mode = auto\n\
             context_management.finish_rounds = 1\ncontext_window = 1m",
        );
        let budget = ContextBudget {
            context_window_tokens: 1_000_000,
            used_tokens: 400_000,
            hard_remaining_tokens: 600_000,
            required_next_round_tokens: 8_000,
        };
        let mut state = ContextState::default();
        state.report_phase(WorkPhase::Plan);
        let first = assess_context(&config.context_management, &state, budget);
        assert_eq!(
            first.action,
            ContextAction::FinishBounded {
                rounds_remaining: 0
            }
        );
        state = first.next_state;
        state.report_phase(WorkPhase::WrapUp);
        assert_eq!(
            assess_context(&config.context_management, &state, budget).action,
            ContextAction::Rollover
        );
    }

    /// `context_window = 1m` with thresholds set for 1M, on a model whose
    /// ceiling clamps the window to 200k: the thresholds that don't fit run
    /// as unset (200k's defaults) instead of failing every assessment closed.
    #[test]
    fn context_window_clamp_resets_thresholds_that_do_not_fit_the_clamped_window() {
        use crate::core::context_policy::{
            assess_context, ContextAction, ContextBudget, ContextReason, ContextState,
        };
        let config = super::load_config_from_str(
            "context_management.mode = auto\n\
             context_management.pressure_tokens = 300000\n\
             context_management.rollover_tokens = 450000\n\
             context_management.reserve_tokens = 20000\n\
             context_window = 1m",
        );
        assert!(config.warnings.is_empty(), "{:?}", config.warnings);
        let policy = config.context_management;
        let budget = ContextBudget {
            context_window_tokens: 200_000,
            used_tokens: 50_000,
            hard_remaining_tokens: 150_000,
            required_next_round_tokens: 8_000,
        };
        // As configured, the policy can't resolve for 200k: a hard stop.
        let raw = assess_context(&policy, &ContextState::default(), budget);
        assert_eq!(raw.action, ContextAction::HardStop);
        assert_eq!(raw.reason, ContextReason::InvalidConfig);

        let (reset, why) = policy
            .reset_for_clamped_window(Some(1_000_000), 200_000)
            .expect("thresholds reset for the clamped window");
        assert_eq!(why, "pressure_tokens must be below the context window");
        assert_eq!(
            reset,
            super::ContextManagementConfig {
                pressure_tokens: None,
                rollover_tokens: None,
                ..policy
            }
        );
        assert_eq!(reset.reserve_tokens, 20_000);
        let assessed = assess_context(&reset, &ContextState::default(), budget);
        assert_eq!(assessed.action, ContextAction::Continue);
        let thresholds = assessed.thresholds.expect("200k defaults");
        assert_eq!(
            (thresholds.pressure_tokens, thresholds.rollover_tokens),
            (140_000, 180_000)
        );

        // Nothing to reset: no clamp, thresholds that fit, or a policy that
        // did not fit the configured window either.
        assert_eq!(policy.reset_for_clamped_window(None, 200_000), None);
        assert_eq!(
            policy.reset_for_clamped_window(Some(1_000_000), 1_000_000),
            None
        );
        assert_eq!(
            policy.reset_for_clamped_window(Some(400_000), 200_000),
            None,
            "rollover 450k never fit a 400k window"
        );
        let fits = super::ContextManagementConfig {
            pressure_tokens: Some(100_000),
            rollover_tokens: Some(150_000),
            ..policy
        };
        assert_eq!(
            fits.reset_for_clamped_window(Some(1_000_000), 200_000),
            None
        );
    }

    #[test]
    fn context_management_keys_are_known_for_suggestions() {
        for key in [
            "mode",
            "pressure_tokens",
            "rollover_tokens",
            "reserve_tokens",
            "finish_rounds",
        ] {
            let full = format!("context_management.{key}");
            assert!(super::KNOWN_CONFIG_KEYS.contains(&full.as_str()));
            assert_eq!(
                super::did_you_mean(&format!("{full}x")),
                Some(full.as_str())
            );
        }
    }

    #[test]
    fn context_management_parsed_policy_rolls_over_before_executing_350k_plan() {
        use crate::core::context_policy::{
            assess_context, ContextAction, ContextBudget, ContextState, WorkPhase,
        };
        let config =
            super::load_config_from_str("context_management.mode = auto\ncontext_window = 1m");
        let budget = ContextBudget {
            context_window_tokens: config.context_window.unwrap(),
            used_tokens: 350_000,
            hard_remaining_tokens: 650_000,
            required_next_round_tokens: 8_000,
        };
        let mut state = ContextState::default();
        state.report_phase(WorkPhase::Plan);
        let plan = assess_context(&config.context_management, &state, budget);
        assert_eq!(plan.action, ContextAction::Advisory);
        state = plan.next_state;
        state.report_phase(WorkPhase::Execute);
        assert_eq!(
            assess_context(&config.context_management, &state, budget).action,
            ContextAction::Rollover
        );
    }

    #[test]
    fn thinking_display_defaults_summarized_and_parses() {
        use super::ThinkingDisplay;
        assert_eq!(super::load_config_from_str("").thinking_display, ThinkingDisplay::Summarized);
        assert_eq!(
            super::load_config_from_str("thinking_display = omitted\n").thinking_display,
            ThinkingDisplay::Omitted
        );
        assert_eq!(
            super::load_config_from_str("thinking_display = Summarized\n").thinking_display,
            ThinkingDisplay::Summarized
        );
        let invalid = super::load_config_from_str("thinking_display = raw\n");
        assert_eq!(invalid.thinking_display, ThinkingDisplay::Summarized, "invalid keeps the default");
        assert!(invalid.warnings.iter().any(|w| w.contains("thinking_display")));
        assert_eq!(ThinkingDisplay::Omitted.as_str(), "omitted");
    }

    #[test]
    fn tui_smooth_streaming_defaults_on_and_parses() {
        assert!(super::load_config_from_str("").tui_smooth_streaming);
        assert!(!super::load_config_from_str("tui_smooth_streaming = off\n").tui_smooth_streaming);
        assert!(super::load_config_from_str("tui_smooth_streaming = on\n").tui_smooth_streaming);
        let invalid = super::load_config_from_str("tui_smooth_streaming = jerky\n");
        assert!(invalid.tui_smooth_streaming, "invalid keeps the default (on)");
        assert!(invalid
            .warnings
            .iter()
            .any(|w| w.contains("tui_smooth_streaming")));
    }

    #[test]
    fn tui_streaming_glow_defaults_off_and_parses() {
        assert!(!super::load_config_from_str("").tui_streaming_glow);
        assert!(super::load_config_from_str("tui_streaming_glow = on\n").tui_streaming_glow);
        assert!(!super::load_config_from_str("tui_streaming_glow = off\n").tui_streaming_glow);
        let invalid = super::load_config_from_str("tui_streaming_glow = sparkly\n");
        assert!(
            !invalid.tui_streaming_glow,
            "invalid keeps the default (off)"
        );
        assert!(invalid
            .warnings
            .iter()
            .any(|w| w.contains("tui_streaming_glow")));
    }

    #[test]
    fn tui_background_opaque_defaults_and_parses_supported_values() {
        assert!(super::load_config_from_str("").tui_background_opaque);
        assert!(
            super::load_config_from_str("tui_background_opaque = opaque\n").tui_background_opaque
        );
        assert!(
            !super::load_config_from_str("tui_background_opaque = invisible\n")
                .tui_background_opaque
        );
        let invalid = super::load_config_from_str("tui_background_opaque = translucent\n");
        assert!(invalid.tui_background_opaque);
        assert!(invalid
            .warnings
            .iter()
            .any(|w| w.contains("tui_background_opaque")));
    }

    #[test]
    fn turn_budget_keys_parse_typed_per_role() {
        let config = super::load_config_from_str(
            "turn_budget.worker.max_provider_rounds = 3\n\
             turn_budget.worker.max_cost_usd = 0.25\n\
             turn_budget.autonomous.max_elapsed_secs = 90\n\
             turn_budget.foreground.max_tool_calls = 7\n\
             turn_budget.foreground.max_round_renewals = 12\n\
             turn_budget.worker.max_provider_rounds_bogus = 1\n\
             turn_budget.nosuchrole.max_tool_calls = 1\n",
        );
        assert_eq!(
            config.turn_budgets.worker.max_provider_rounds,
            Some(super::TurnLimit::Max(3))
        );
        assert_eq!(config.turn_budgets.worker.max_cost_usd, Some(0.25));
        assert_eq!(config.turn_budgets.autonomous.max_elapsed_secs, Some(90));
        assert_eq!(
            config.turn_budgets.foreground.max_tool_calls,
            Some(super::TurnLimit::Max(7))
        );
        assert_eq!(config.turn_budgets.foreground.max_round_renewals, Some(12));
        // Unknown fields/roles warn and change nothing.
        assert_eq!(config.turn_budgets.worker.max_tool_calls, None);
        // Invalid values keep the default (None).
        let bad = super::load_config_from_str("turn_budget.worker.max_provider_rounds = nope\n");
        assert_eq!(bad.turn_budgets.worker.max_provider_rounds, None);
    }

    #[test]
    fn turn_budget_count_limits_accept_unlimited() {
        let config = super::load_config_from_str(
            "turn_budget.autonomous.max_provider_rounds = unlimited\n\
             turn_budget.autonomous.max_tool_calls = \"unlimited\"\n\
             turn_budget.worker.max_accumulated_tool_result_bytes = Unlimited\n",
        );
        assert!(config.warnings.is_empty(), "{:?}", config.warnings);
        let unlimited = Some(super::TurnLimit::Unlimited);
        assert_eq!(config.turn_budgets.autonomous.max_provider_rounds, unlimited);
        assert_eq!(config.turn_budgets.autonomous.max_tool_calls, unlimited);
        assert_eq!(
            config.turn_budgets.worker.max_accumulated_tool_result_bytes,
            Some(super::TurnLimit::Unlimited)
        );
        let bad = super::load_config_from_str("turn_budget.worker.max_tool_calls = lots\n");
        assert_eq!(bad.turn_budgets.worker.max_tool_calls, None);
    }

    #[test]
    fn turn_budget_zero_count_limits_keep_none_allowed_with_a_deprecation_warning() {
        // 0.10.2 read `0` as "none allowed". For one release it keeps that
        // meaning in every role (also where the default is unlimited), and
        // says it will be rejected later.
        for role in ["foreground", "autonomous", "event", "worker"] {
            for field in [
                "max_provider_rounds",
                "max_tool_calls",
                "max_accumulated_tool_result_bytes",
            ] {
                let key = format!("turn_budget.{role}.{field}");
                let content = format!("{key} = 0\n");
                let config = super::load_config_from_str(&content);
                let overrides = match role {
                    "foreground" => &config.turn_budgets.foreground,
                    "autonomous" => &config.turn_budgets.autonomous,
                    "event" => &config.turn_budgets.event,
                    _ => &config.turn_budgets.worker,
                };
                let parsed = match field {
                    "max_provider_rounds" => overrides.max_provider_rounds.map(|l| l.map(|v| v as usize)),
                    "max_tool_calls" => overrides.max_tool_calls.map(|l| l.map(|v| v as usize)),
                    _ => overrides.max_accumulated_tool_result_bytes,
                };
                assert_eq!(parsed, Some(super::TurnLimit::Max(0)), "{key}");
                assert!(
                    config.warnings.iter().any(|w| w.contains(&key)
                        && w.contains("none allowed")
                        && w.contains(
                            "`0` will be rejected in a future release; use `unlimited` for no limit, or a positive number"
                        )),
                    "{key}: {:?}",
                    config.warnings
                );
                assert!(!super::config_content_problems(&content).is_empty());
            }
        }
    }

    #[test]
    fn memory_keys_all_parse_with_consent() {
        let config = super::load_config_from_str(
            "memory.default_mode = capture_and_recall\n\
             memory.default_mode_confirmed = true\n\
             memory.recall_max_records = 12\n\
             memory.recall_max_tokens = 2048\n\
             memory.recall_timeout_ms = 250\n\
             memory.capture_tools = off\n\
             memory.capture_assistant = false\n\
             memory.capture_user = false\n\
             memory.auto_consolidate = on\n\
             memory.local_embeddings = on\n\
             memory.gliner = on\n",
        );
        assert_eq!(config.memory.default_mode, "capture_and_recall");
        assert!(config.memory.default_mode_confirmed);
        assert_eq!(config.memory.recall_max_records, 12);
        assert_eq!(config.memory.recall_max_tokens, 2048);
        assert_eq!(config.memory.recall_timeout_ms, 250);
        assert_eq!(config.memory.capture_tools, "off");
        assert!(!config.memory.capture_assistant);
        assert!(!config.memory.capture_user);
        assert_eq!(config.memory.auto_consolidate, "on");
        assert_eq!(config.memory.local_embeddings, "on");
        assert_eq!(config.memory.gliner, "on");
    }

    #[test]
    fn memory_defaults_match_spec_12() {
        let config = super::load_config_from_str("");
        assert_eq!(config.memory, super::MemoryConfig::default());
        assert_eq!(config.memory.default_mode, "off");
        assert!(!config.memory.default_mode_confirmed);
        assert_eq!(config.memory.recall_max_records, 8);
        assert_eq!(config.memory.recall_max_tokens, 4096);
        assert_eq!(config.memory.recall_timeout_ms, 150);
        assert_eq!(config.memory.capture_tools, "summary_only");
        assert!(config.memory.capture_assistant);
        assert!(config.memory.capture_user);
        assert_eq!(config.memory.auto_consolidate, "off");
        assert_eq!(config.memory.local_embeddings, "off");
        assert_eq!(config.memory.gliner, "off");
    }

    #[test]
    fn memory_unknown_values_fail_closed_to_defaults() {
        let config = super::load_config_from_str(
            "memory.default_mode = recall_everything_forever\n\
             memory.default_mode_confirmed = true\n\
             memory.recall_max_records = lots\n\
             memory.recall_max_tokens = -5\n\
             memory.recall_timeout_ms = fast\n\
             memory.capture_tools = full\n\
             memory.capture_assistant = maybe\n\
             memory.capture_user = sometimes\n\
             memory.auto_consolidate = aggressive\n\
             memory.local_embeddings = auto\n\
             memory.gliner = download\n\
             memory.nosuchfield = 1\n",
        );
        // Unrecognised mode string must stay "off" — never a non-off mode.
        assert_eq!(config.memory.default_mode, "off");
        assert_eq!(config.memory.recall_max_records, 8);
        assert_eq!(config.memory.recall_max_tokens, 4096);
        assert_eq!(config.memory.recall_timeout_ms, 150);
        assert_eq!(config.memory.capture_tools, "summary_only");
        assert!(config.memory.capture_assistant);
        assert!(config.memory.capture_user);
        assert_eq!(config.memory.auto_consolidate, "off");
        assert_eq!(config.memory.local_embeddings, "off");
        assert_eq!(config.memory.gliner, "off");
    }

    #[test]
    fn memory_default_mode_without_consent_reverts_to_off() {
        let config = super::load_config_from_str("memory.default_mode = recall_each_prompt\n");
        assert_eq!(config.memory.default_mode, "off");
        assert!(
            config
                .warnings
                .iter()
                .any(|w| w.contains("memory.default_mode_confirmed")),
            "reversion must surface a warning; got: {:?}",
            config.warnings
        );

        // Explicit false consent is not consent.
        let denied = super::load_config_from_str(
            "memory.default_mode = capture_only\n\
             memory.default_mode_confirmed = false\n",
        );
        assert_eq!(denied.memory.default_mode, "off");

        // Garbage consent value fails closed — not consent.
        let garbage = super::load_config_from_str(
            "memory.default_mode = capture_only\n\
             memory.default_mode_confirmed = definitely\n",
        );
        assert_eq!(garbage.memory.default_mode, "off");
    }

    #[test]
    fn memory_consent_is_order_independent() {
        // Consent key before the mode key.
        let before = super::load_config_from_str(
            "memory.default_mode_confirmed = true\n\
             memory.default_mode = recall_once\n",
        );
        assert_eq!(before.memory.default_mode, "recall_once");
        // Consent key after the mode key.
        let after = super::load_config_from_str(
            "memory.default_mode = recall_once\n\
             memory.default_mode_confirmed = true\n",
        );
        assert_eq!(after.memory.default_mode, "recall_once");
    }

    #[test]
    fn memory_backend_defaults_are_independent_of_consent() {
        let config = super::load_config_from_str("");
        assert_eq!(config.memory_backend, super::MemoryBackendConfig::default());
        assert_eq!(config.memory_backend.kind, super::MemoryBackendKind::Legacy);
        let axel = super::load_config_from_str("memory.backend = axel\n");
        assert_eq!(axel.memory_backend.kind, super::MemoryBackendKind::Axel);
        assert_eq!(axel.memory, config.memory);
        assert!(axel.warnings.is_empty());
    }

    #[test]
    fn user_wide_notes_require_explicit_valid_opt_in() {
        let default = super::load_config_from_str("memory.backend = axel\n");
        assert!(!default.memory_backend.user_scope);
        let enabled =
            super::load_config_from_str("memory.backend = axel\nmemory.user_scope = true\n");
        assert!(enabled.memory_backend.user_scope);
        assert_eq!(enabled.memory, default.memory);
        for bad in ["yes", "", "1", "TRUE"] {
            let config = super::load_config_from_str(&format!(
                "memory.user_scope = {bad}\nmemory.backend = axel\n"
            ));
            assert_eq!(
                config.memory_backend.kind,
                super::MemoryBackendKind::Unavailable
            );
        }
    }

    #[test]
    fn memory_backend_invalid_selector_fails_closed() {
        for content in [
            "memory.backend = typo\n",
            "memory.backend = \n",
            "memory.backend = unavailable\n",
            "memory.backend = axel\nmemory.backend = typo\n",
            "memory.backend = typo\nmemory.backend = legacy\n",
        ] {
            let config = super::load_config_from_str(content);
            assert_eq!(
                config.memory_backend.kind,
                super::MemoryBackendKind::Unavailable
            );
            assert!(config
                .warnings
                .iter()
                .any(|w| w.contains("memory backend unavailable")));
        }
    }

    #[test]
    fn memory_backend_absolute_paths_are_preserved() {
        let root = std::env::current_dir().unwrap();
        let executable = root.join("fixture bin/axel");
        let brain = root.join("fixture brain");
        let config = super::load_config_from_str(&format!(
            "memory.axel.brain = {}\nmemory.backend = axel\nmemory.axel.executable = {}\n",
            brain.display(),
            executable.display()
        ));
        assert_eq!(config.memory_backend.kind, super::MemoryBackendKind::Axel);
        assert_eq!(config.memory_backend.executable, Some(executable));
        assert_eq!(config.memory_backend.brain, Some(brain));
        assert!(config.warnings.is_empty());
    }

    #[test]
    fn memory_backend_relative_or_empty_paths_fail_closed_in_any_order() {
        for key in ["memory.axel.executable", "memory.axel.brain"] {
            for path in ["relative/path", "~/brain", ""] {
                for content in [
                    format!("memory.backend = axel\n{key} = {path}\n"),
                    format!("{key} = {path}\nmemory.backend = legacy\n"),
                ] {
                    let config = super::load_config_from_str(&content);
                    assert_eq!(
                        config.memory_backend.kind,
                        super::MemoryBackendKind::Unavailable
                    );
                    assert!(config.warnings.iter().any(|w| w.contains("absolute path")));
                }
            }
        }
    }

    #[test]
    fn memory_config_surface_is_closed_by_construction() {
        // Exhaustive struct literal — no `..Default::default()` spread. This
        // fails to COMPILE if any field is added to or removed from
        // MemoryConfig, pinning the exact memory.* config surface. Secret,
        // retention, and project-scope semantics have no field here, so no
        // `memory.secret*` / `memory.retention*` / `memory.project*` config
        // key can exist (spec §12: those rules are not configurable to fail
        // open). Asserted by construction, not by test disproof.
        let _closed_surface = super::MemoryConfig {
            default_mode: "off".to_string(),
            default_mode_confirmed: false,
            recall_max_records: 8,
            recall_max_tokens: 4096,
            recall_timeout_ms: 150,
            capture_tools: "summary_only".to_string(),
            capture_assistant: true,
            capture_user: true,
            auto_consolidate: "off".to_string(),
            local_embeddings: "off".to_string(),
            gliner: "off".to_string(),
        };
    }

    use super::*;
    use serial_test::serial;

    #[test]
    fn test_levenshtein_basics() {
        assert_eq!(levenshtein("model", "model"), 0);
        assert_eq!(levenshtein("modle", "model"), 2);
        assert_eq!(levenshtein("them", "theme"), 1);
    }

    #[test]
    fn test_did_you_mean_close_typos() {
        assert_eq!(did_you_mean("modle"), Some("model"));
        assert_eq!(did_you_mean("them"), Some("theme"));
        assert_eq!(did_you_mean("thinkng"), Some("thinking"));
        assert_eq!(did_you_mean("completely_unrelated_key"), None);
    }

    #[test]
    #[serial]
    fn test_config_warnings_unknown_key_and_bad_values() {
        let home = std::env::temp_dir().join(format!("synaps-warn-test-{}", std::process::id()));
        let dir = home.join(".synaps-cli");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config"), "modle = claude-opus-4-6\nthinking = hgih\nbash_timeout = 0\nknowledge.jawz_notes = ~/Jawz/notes\ncustom.plugin.key = 42\n").unwrap();

        with_home(&home, || {
            let config = load_config();
            // Dotted (namespaced) keys must NOT warn — plugins own those.
            assert_eq!(config.warnings.len(), 3, "warnings: {:?}", config.warnings);
            assert!(
                !config.warnings.iter().any(|w| w.contains("knowledge")),
                "{:?}",
                config.warnings
            );
            assert!(
                config
                    .warnings
                    .iter()
                    .any(|w| w.contains("did you mean 'model'")),
                "{:?}",
                config.warnings
            );
            assert!(
                config.warnings.iter().any(|w| w.contains("thinking")),
                "{:?}",
                config.warnings
            );
            assert!(
                config.warnings.iter().any(|w| w.contains("below minimum")),
                "{:?}",
                config.warnings
            );
            // Bad values fall back to defaults
            assert_eq!(config.bash_timeout, 30);
            assert_eq!(config.thinking_budget, None);
        });
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn test_client_input_is_exclusive_by_default_and_parses() {
        // PARITY: the default is Exclusive (790d R6, until JR answers E4).
        assert_eq!(SynapsConfig::default().client_input, ClientInput::Exclusive);
        assert_eq!(load_config_from_str("").client_input, ClientInput::Exclusive);
        assert_eq!(load_config_from_str("client_input = exclusive\n").client_input, ClientInput::Exclusive);
        assert_eq!(load_config_from_str("client_input = Shared\n").client_input, ClientInput::Shared);
        let c = load_config_from_str("client_input = mine\n");
        assert_eq!(c.client_input, SynapsConfig::default().client_input);
        assert!(c.warnings.iter().any(|w| w.contains("client_input")), "{:?}", c.warnings);
        assert_eq!(ClientInput::wire_default(), ClientInput::Exclusive, "an older daemon enforces the owner");
        assert_eq!(ClientInput::default(), ClientInput::Exclusive, "the enum agrees with the config");
    }

    // ── cache_keepwarm (task #444) ──

    #[test]
    fn test_cache_keepwarm_is_off_by_default() {
        assert_eq!(SynapsConfig::default().cache_keepwarm, CacheKeepWarm::Off);
        assert_eq!(load_config_from_str("").cache_keepwarm, CacheKeepWarm::Off);
    }

    #[test]
    fn test_cache_keepwarm_parse_table() {
        for (raw, want) in [
            ("off", Some(CacheKeepWarm::Off)),
            ("OFF", Some(CacheKeepWarm::Off)),
            ("0h", Some(CacheKeepWarm::Off)),
            ("auto", Some(CacheKeepWarm::Auto)),
            (" Auto ", Some(CacheKeepWarm::Auto)),
            ("1h", Some(CacheKeepWarm::Hours(1))),
            ("12H", Some(CacheKeepWarm::Hours(12))),
            ("48h", Some(CacheKeepWarm::Hours(MAX_CACHE_KEEPWARM_HOURS))),
            ("49h", None),
            ("12", None),
            ("h", None),
            ("-1h", None),
            ("lots", None),
        ] {
            assert_eq!(CacheKeepWarm::parse(raw), want, "{raw:?}");
        }
        for mode in [CacheKeepWarm::Off, CacheKeepWarm::Auto, CacheKeepWarm::Hours(6)] {
            assert_eq!(CacheKeepWarm::parse(&mode.to_string()), Some(mode));
        }
    }

    #[test]
    fn test_telemetry_log_generations_config_key() {
        let c = load_config_from_str("telemetry = basic\n");
        assert_eq!(c.telemetry_log_generations, 1, "default: one generation");

        let c = load_config_from_str("telemetry_log_generations = 7\n");
        assert_eq!(c.telemetry_log_generations, 7);
        assert!(c.warnings.is_empty(), "warnings: {:?}", c.warnings);

        for bad in ["0", "101", "-1", "lots"] {
            let c = load_config_from_str(&format!("telemetry_log_generations = {bad}\n"));
            assert_eq!(c.telemetry_log_generations, 1, "{bad}");
            assert!(
                c.warnings
                    .iter()
                    .any(|w| w.contains("telemetry_log_generations")),
                "{bad}: {:?}",
                c.warnings
            );
        }
    }

    #[test]
    fn test_cache_keepwarm_headless_key() {
        assert!(!SynapsConfig::default().cache_keepwarm_headless, "default off");
        assert!(!load_config_from_str("").cache_keepwarm_headless);
        for on in ["true", "1", "on", "yes", "TRUE"] {
            let c = load_config_from_str(&format!("cache_keepwarm_headless = {on}\n"));
            assert!(c.cache_keepwarm_headless, "{on}");
            assert!(c.warnings.is_empty(), "{on}: {:?}", c.warnings);
        }
        for off in ["false", "0", "off", "no"] {
            let c = load_config_from_str(&format!("cache_keepwarm_headless = {off}\n"));
            assert!(!c.cache_keepwarm_headless, "{off}");
        }
        let c = load_config_from_str("cache_keepwarm_headless = maybe\n");
        assert!(!c.cache_keepwarm_headless, "garbage keeps it off");
        assert!(c.warnings.iter().any(|w| w.contains("cache_keepwarm_headless")));
    }

    #[test]
    fn test_cache_keepwarm_tools_key() {
        assert!(SynapsConfig::default().cache_keepwarm_tools, "default on");
        assert!(load_config_from_str("").cache_keepwarm_tools);
        for off in ["false", "0", "off", "no", "OFF"] {
            let c = load_config_from_str(&format!("cache_keepwarm_tools = {off}\n"));
            assert!(!c.cache_keepwarm_tools, "{off}");
            assert!(c.warnings.is_empty(), "{off}: {:?}", c.warnings);
        }
        for on in ["true", "1", "on", "yes"] {
            assert!(load_config_from_str(&format!("cache_keepwarm_tools = {on}\n")).cache_keepwarm_tools);
        }
        let c = load_config_from_str("cache_keepwarm_tools = maybe\n");
        assert!(c.cache_keepwarm_tools, "garbage keeps it on");
        assert!(c.warnings.iter().any(|w| w.contains("cache_keepwarm_tools")));
    }

    #[test]
    fn test_cache_keepwarm_config_key() {
        let c = load_config_from_str("cache_keepwarm = auto\ncache_ttl = 5m\n");
        assert_eq!(c.cache_keepwarm, CacheKeepWarm::Auto);
        assert_eq!(c.cache_ttl, CacheTtlSetting::Fixed(CacheTtl::FiveMinutes));
        assert!(c.warnings.is_empty(), "warnings: {:?}", c.warnings);

        let c = load_config_from_str("cache_keepwarm = 6h\n");
        assert_eq!(c.cache_keepwarm, CacheKeepWarm::Hours(6));

        assert_eq!(c.cache_keepwarm_auto_max_hours, 24, "default auto cap");

        let c = load_config_from_str("cache_keepwarm_auto_max_hours = 8\n");
        assert_eq!(c.cache_keepwarm_auto_max_hours, 8);
        assert!(c.warnings.is_empty(), "warnings: {:?}", c.warnings);
        let c = load_config_from_str("cache_keepwarm_auto_max_hours = 12h\n");
        assert_eq!(c.cache_keepwarm_auto_max_hours, 12);
        for bad in ["0", "49", "lots"] {
            let c = load_config_from_str(&format!("cache_keepwarm_auto_max_hours = {bad}\n"));
            assert_eq!(c.cache_keepwarm_auto_max_hours, 24, "{bad}");
            assert!(
                c.warnings.iter().any(|w| w.contains("auto_max_hours")),
                "{bad}"
            );
        }

        let c = load_config_from_str("cache_keepwarm = forever\n");
        assert_eq!(c.cache_keepwarm, CacheKeepWarm::Off, "garbage keeps keep-warm off");
        assert!(c.warnings.iter().any(|w| w.contains("cache_keepwarm")));
    }

    #[test]
    fn request_compression_keys_parse_and_warn() {
        let c = load_config_from_str("");
        assert_eq!(c.request_compression, RequestCompressionMode::Auto);
        assert_eq!(c.request_compression_level, DEFAULT_REQUEST_COMPRESSION_LEVEL);

        for (val, want) in [
            ("auto", RequestCompressionMode::Auto),
            ("ON", RequestCompressionMode::On),
            (" off ", RequestCompressionMode::Off),
        ] {
            let c = load_config_from_str(&format!("request_compression = {val}\n"));
            assert_eq!(c.request_compression, want, "{val:?}");
            assert!(c.warnings.is_empty(), "{val:?}: {:?}", c.warnings);
            assert_eq!(RequestCompressionMode::parse(&want.to_string()), Some(want));
        }
        let c = load_config_from_str("request_compression = brotli\n");
        assert_eq!(c.request_compression, RequestCompressionMode::Auto);
        assert!(c.warnings.iter().any(|w| w.contains("request_compression")));

        for level in 1..=MAX_REQUEST_COMPRESSION_LEVEL {
            let c = load_config_from_str(&format!("request_compression_level = {level}\n"));
            assert_eq!(c.request_compression_level, level);
            assert!(c.warnings.is_empty());
        }
        for bad in ["0", "10", "max", "-1"] {
            let c = load_config_from_str(&format!("request_compression_level = {bad}\n"));
            assert_eq!(c.request_compression_level, DEFAULT_REQUEST_COMPRESSION_LEVEL, "{bad}");
            assert!(
                c.warnings.iter().any(|w| w.contains("request_compression_level")),
                "{bad}"
            );
        }
    }

    // ── cache_ttl parse table (spec §3.1) ──

    #[test]
    fn test_cache_ttl_parse_table() {
        // 5m aliases
        assert_eq!(CacheTtl::parse("5m"), Some(CacheTtl::FiveMinutes));
        assert_eq!(CacheTtl::parse("5min"), Some(CacheTtl::FiveMinutes));
        assert_eq!(CacheTtl::parse("default"), Some(CacheTtl::FiveMinutes));
        // 1h aliases
        assert_eq!(CacheTtl::parse("1h"), Some(CacheTtl::OneHour));
        assert_eq!(CacheTtl::parse("60m"), Some(CacheTtl::OneHour));
        assert_eq!(CacheTtl::parse("1hr"), Some(CacheTtl::OneHour));
        // hybrid
        assert_eq!(CacheTtl::parse("hybrid"), Some(CacheTtl::Hybrid));
        // case-insensitive
        assert_eq!(CacheTtl::parse("1H"), Some(CacheTtl::OneHour));
        assert_eq!(CacheTtl::parse("HYBRID"), Some(CacheTtl::Hybrid));
        assert_eq!(CacheTtl::parse("Default"), Some(CacheTtl::FiveMinutes));
        // garbage → None (caller warns + defaults)
        assert_eq!(CacheTtl::parse("2h"), None);
        assert_eq!(CacheTtl::parse(""), None);
        assert_eq!(CacheTtl::parse("forever"), None);
    }

    #[test]
    fn test_cache_ttl_default_is_auto() {
        // The wire policy's own default stays the historical bare 5m marker;
        // the *config* default is `auto` (resolved per credential kind).
        assert_eq!(CacheTtl::default(), CacheTtl::FiveMinutes);
        assert_eq!(CacheTtlSetting::default(), CacheTtlSetting::Auto);
        assert_eq!(SynapsConfig::default().cache_ttl, CacheTtlSetting::Auto);
        // An empty config file leaves it at auto.
        assert_eq!(load_config_from_str("").cache_ttl, CacheTtlSetting::Auto);
    }

    #[test]
    fn test_cache_ttl_setting_parse_and_round_trip() {
        assert_eq!(CacheTtlSetting::parse("auto"), Some(CacheTtlSetting::Auto));
        assert_eq!(CacheTtlSetting::parse(" AUTO "), Some(CacheTtlSetting::Auto));
        assert_eq!(
            CacheTtlSetting::parse("5m"),
            Some(CacheTtlSetting::Fixed(CacheTtl::FiveMinutes))
        );
        assert_eq!(
            CacheTtlSetting::parse("1hr"),
            Some(CacheTtlSetting::Fixed(CacheTtl::OneHour))
        );
        assert_eq!(
            CacheTtlSetting::parse("hybrid"),
            Some(CacheTtlSetting::Fixed(CacheTtl::Hybrid))
        );
        for bad in ["", "2h", "forever", "automatic", "on"] {
            assert_eq!(CacheTtlSetting::parse(bad), None, "{bad:?}");
        }
        // as_str → parse is the identity for every value, and the written
        // config line reloads to the same setting.
        for setting in [
            CacheTtlSetting::Auto,
            CacheTtlSetting::Fixed(CacheTtl::FiveMinutes),
            CacheTtlSetting::Fixed(CacheTtl::OneHour),
            CacheTtlSetting::Fixed(CacheTtl::Hybrid),
        ] {
            assert_eq!(CacheTtlSetting::parse(setting.as_str()), Some(setting));
            assert_eq!(setting.to_string(), setting.as_str());
            let cfg = load_config_from_str(&format!("cache_ttl = {setting}\n"));
            assert_eq!(cfg.cache_ttl, setting);
            assert!(cfg.warnings.is_empty(), "{setting}: {:?}", cfg.warnings);
        }
    }

    #[test]
    fn test_cache_ttl_setting_resolution() {
        // auto: subscription (OAuth) → 1h, API key → 5m.
        assert_eq!(CacheTtlSetting::Auto.resolve(true), CacheTtl::OneHour);
        assert_eq!(CacheTtlSetting::Auto.resolve(false), CacheTtl::FiveMinutes);
        assert_eq!(
            CacheTtlSetting::Auto.resolve_for_auth_type("oauth"),
            CacheTtl::OneHour
        );
        for not_oauth in ["api_key", "none", "", "OAuth"] {
            assert_eq!(
                CacheTtlSetting::Auto.resolve_for_auth_type(not_oauth),
                CacheTtl::FiveMinutes,
                "{not_oauth:?}"
            );
        }
        // Explicit values win over the credential kind, both ways.
        for ttl in [CacheTtl::FiveMinutes, CacheTtl::OneHour, CacheTtl::Hybrid] {
            for oauth in [true, false] {
                assert_eq!(CacheTtlSetting::Fixed(ttl).resolve(oauth), ttl);
            }
        }
    }

    #[test]
    fn test_max_fps_default_is_60() {
        assert_eq!(SynapsConfig::default().max_fps, 60);
    }

    // ── theme_transition parse table (animated theme transitions) ──

    #[test]
    fn test_theme_transition_parse_table() {
        use ThemeTransitionMode as M;
        assert_eq!(M::parse("on"), Some(M::On));
        assert_eq!(M::parse("ON"), Some(M::On));
        assert_eq!(M::parse("off"), Some(M::Off));
        assert_eq!(M::parse("Off"), Some(M::Off));
        // integer ms, clamped to 0..=2000
        assert_eq!(M::parse("500"), Some(M::Ms(500)));
        assert_eq!(M::parse("0"), Some(M::Ms(0)));
        assert_eq!(M::parse("2000"), Some(M::Ms(2000)));
        assert_eq!(M::parse("99999"), Some(M::Ms(2000)), "clamped to MAX_MS");
        // garbage → None (caller warns + keeps default)
        assert_eq!(M::parse("fast"), None);
        assert_eq!(M::parse(""), None);
        assert_eq!(M::parse("-5"), None);
        assert_eq!(M::parse("1.5"), None);
    }

    #[test]
    fn test_theme_transition_duration_ms() {
        use ThemeTransitionMode as M;
        assert_eq!(M::On.duration_ms(), M::DEFAULT_MS);
        assert_eq!(M::DEFAULT_MS, 350);
        assert_eq!(M::Off.duration_ms(), 0);
        assert_eq!(M::Ms(120).duration_ms(), 120);
        assert_eq!(M::Ms(0).duration_ms(), 0, "0 ms behaves like off");
        assert_eq!(M::Ms(u64::MAX).duration_ms(), M::MAX_MS);
    }

    #[test]
    fn test_theme_transition_config_key_parses_and_defaults() {
        use ThemeTransitionMode as M;
        assert_eq!(SynapsConfig::default().theme_transition, M::On);
        assert_eq!(
            super::load_config_from_str("theme_transition = off\n").theme_transition,
            M::Off
        );
        assert_eq!(
            super::load_config_from_str("theme_transition = 750\n").theme_transition,
            M::Ms(750)
        );
        let bad = super::load_config_from_str("theme_transition = warp\n");
        assert_eq!(bad.theme_transition, M::On, "bad value keeps default");
        assert!(
            bad.warnings.iter().any(|w| w.contains("theme_transition")),
            "{:?}",
            bad.warnings
        );
    }

    #[test]
    #[serial]
    fn test_max_fps_config_parse_and_validation() {
        let home = std::env::temp_dir().join(format!("synaps-maxfps-test-{}", std::process::id()));
        let dir = home.join(".synaps-cli");
        std::fs::create_dir_all(&dir).unwrap();

        // Valid high-refresh value parses, no warning.
        std::fs::write(dir.join("config"), "max_fps = 144\n").unwrap();
        with_home(&home, || {
            let config = load_config();
            assert_eq!(config.max_fps, 144);
            assert!(
                config.warnings.is_empty(),
                "warnings: {:?}",
                config.warnings
            );
        });

        // Out-of-range (0) → default 60 + boot warning (never blocks boot).
        std::fs::write(dir.join("config"), "max_fps = 0\n").unwrap();
        with_home(&home, || {
            let config = load_config();
            assert_eq!(config.max_fps, 60);
            assert!(
                config.warnings.iter().any(|w| w.contains("max_fps")),
                "warnings: {:?}",
                config.warnings
            );
        });

        // Non-numeric → default 60 + warning.
        std::fs::write(dir.join("config"), "max_fps = fast\n").unwrap();
        with_home(&home, || {
            let config = load_config();
            assert_eq!(config.max_fps, 60);
            assert!(
                config.warnings.iter().any(|w| w.contains("max_fps")),
                "warnings: {:?}",
                config.warnings
            );
        });

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    #[serial]
    fn test_cache_ttl_config_parse_and_garbage_warning() {
        let home =
            std::env::temp_dir().join(format!("synaps-cachettl-test-{}", std::process::id()));
        let dir = home.join(".synaps-cli");
        std::fs::create_dir_all(&dir).unwrap();

        // Valid value parses, no warning.
        std::fs::write(dir.join("config"), "cache_ttl = hybrid\n").unwrap();
        with_home(&home, || {
            let config = load_config();
            assert_eq!(config.cache_ttl, CacheTtlSetting::Fixed(CacheTtl::Hybrid));
            assert!(
                config.warnings.is_empty(),
                "warnings: {:?}",
                config.warnings
            );
        });

        // Garbage value → 5m default + boot warning (never blocks boot).
        std::fs::write(dir.join("config"), "cache_ttl = 2h\n").unwrap();
        with_home(&home, || {
            let config = load_config();
            assert_eq!(config.cache_ttl, CacheTtlSetting::Auto);
            assert!(
                config.warnings.iter().any(|w| w.contains("cache_ttl")),
                "warnings: {:?}",
                config.warnings
            );
        });

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn subagent_thinking_config_is_independent_and_validated() {
        use crate::core::reasoning::ReasoningLevel;
        let cfg = load_config_from_str("thinking = low\n");
        assert_eq!(cfg.subagent_thinking, ReasoningLevel::XHigh);
        for value in ["low", "medium", "high", "xhigh", "max"] {
            let cfg = load_config_from_str(&format!("thinking = high\nsubagent_thinking = {value}\n"));
            assert_eq!(cfg.subagent_thinking.as_str(), value);
            assert_eq!(cfg.thinking_level, Some(ReasoningLevel::High));
            assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);
        }
        for value in ["typo", "", "32768", "off", "ultra"] {
            let cfg = load_config_from_str(&format!("subagent_thinking = {value}\n"));
            assert_eq!(cfg.subagent_thinking, ReasoningLevel::XHigh);
            assert!(!cfg.warnings.is_empty(), "{value:?}");
        }
    }

    #[test]
    fn test_parse_thinking_budget() {
        use crate::core::reasoning::ThinkingSpec;
        // ThinkingSpec::parse replaced parse_thinking_budget; verify budget values.
        assert_eq!(ThinkingSpec::parse("low").unwrap().to_budget(), Some(2048));
        assert_eq!(
            ThinkingSpec::parse("medium").unwrap().to_budget(),
            Some(4096)
        );
        assert_eq!(
            ThinkingSpec::parse("high").unwrap().to_budget(),
            Some(16384)
        );
        assert_eq!(
            ThinkingSpec::parse("xhigh").unwrap().to_budget(),
            Some(32768)
        );
        assert_eq!(ThinkingSpec::parse("8192").unwrap().to_budget(), Some(8192));
        let config = load_config_from_str("thinking = 8192\n");
        assert_eq!(config.thinking_level, None);
        assert_eq!(config.thinking_budget, Some(8192));
        assert_eq!(ThinkingSpec::parse("invalid"), None);
    }

    #[test]
    fn test_base_dir() {
        let path = base_dir();
        assert!(path.to_string_lossy().ends_with(".synaps-cli"));
    }

    #[test]
    fn test_resolve_system_prompt_explicit() {
        let result = resolve_system_prompt(Some("test prompt"));
        assert_eq!(result, "test prompt");
    }

    #[test]
    #[serial]
    fn test_resolve_system_prompt_none() {
        // Empty base dir: no system.md, so the stock default applies.
        let home = std::env::temp_dir().join(format!("synaps-prompt-none-{}", std::process::id()));
        std::fs::create_dir_all(home.join(".synaps-cli")).unwrap();
        with_home(&home, || {
            assert_eq!(resolve_system_prompt(None), DEFAULT_PROMPT);
        });
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn default_prompts_carry_working_habit_steering() {
        for (name, prompt) in [
            ("unix", DEFAULT_PROMPT_UNIX),
            ("windows", DEFAULT_PROMPT_WINDOWS),
        ] {
            for needle in [
                "helpful AI agent running in a terminal",
                "Be concise and direct.",
                "targeted edits rather than rewriting them whole",
                "re-read it only if it may have changed",
                "as lean as the task allows",
                "Keep command output small",
                "read large outputs selectively",
            ] {
                assert!(prompt.contains(needle), "{name} prompt lacks {needle:?}");
            }
            assert!(!prompt.contains("  "), "{name} prompt has a double space");
            assert!(prompt.ends_with("selectively."), "{name}: {prompt}");
        }
        assert!(DEFAULT_PROMPT_UNIX.contains("`| tail -n`"));
        assert!(DEFAULT_PROMPT_WINDOWS.contains("on Windows"));
        assert!(DEFAULT_PROMPT_WINDOWS.contains("`Select-Object -Last N`"));
        assert!(!DEFAULT_PROMPT_WINDOWS.contains("tail -n"));
    }

    /// The stock prompt sits in the cached prefix of every stock request:
    /// keep it from silently ballooning.
    #[test]
    fn default_prompts_stay_small() {
        assert!(DEFAULT_PROMPT_UNIX.len() <= 640, "{}", DEFAULT_PROMPT_UNIX.len());
        assert!(DEFAULT_PROMPT_WINDOWS.len() <= 900, "{}", DEFAULT_PROMPT_WINDOWS.len());
    }

    /// Explicit `--system` text and files are used byte for byte; the stock
    /// steering never leaks into them.
    #[test]
    fn explicit_system_prompt_is_byte_identical() {
        let text = "  Custom prompt.\n\nKeep trailing whitespace.  \n";
        assert_eq!(resolve_system_prompt(Some(text)), text);

        let dir = std::env::temp_dir().join(format!("synaps-prompt-file-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("system.txt");
        std::fs::write(&file, text).unwrap();
        assert_eq!(resolve_system_prompt(Some(file.to_str().unwrap())), text);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A user `system.md` (base dir or active profile) replaces the stock
    /// default entirely, byte for byte.
    #[test]
    #[serial]
    fn system_md_prompt_is_byte_identical() {
        let text = "You are my agent.\nNo extra steering.\n";
        let home = std::env::temp_dir().join(format!("synaps-prompt-md-{}", std::process::id()));
        let dir = home.join(".synaps-cli");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("system.md"), text).unwrap();
        with_home(&home, || {
            assert_eq!(resolve_system_prompt(None), text);
            // An explicit value still wins over system.md.
            assert_eq!(resolve_system_prompt(Some("explicit")), "explicit");
        });
        let _ = std::fs::remove_dir_all(&home);
    }

    // Note: test_load_config_nonexistent_file removed — HOME env var mutation
    // is not thread-safe and races with shell config tests. Coverage provided
    // by shell::config::tests::test_shell_config_from_file.

    #[test]
    fn test_synaps_config_default() {
        let config = SynapsConfig::default();
        assert_eq!(config.model, None);
        assert_eq!(config.thinking_budget, None);
        assert_eq!(config.max_tool_output, 30000);
        assert_eq!(config.bash_timeout, 30);
        assert_eq!(config.bash_max_timeout, 300);
        assert_eq!(config.subagent_timeout, 300);
        assert_eq!(config.api_retries, 3);
        assert_eq!(config.theme, None);
        assert!(config.disabled_plugins.is_empty());
        assert!(config.favorite_models.is_empty());
        assert!(config.disabled_skills.is_empty());
        assert!(!config.progressive_tool_disclosure);
        assert_eq!(config.tools_activation_confirm, ActivationConfirm::Auto);
        assert_eq!(config.shell.max_sessions, 5);
        assert_eq!(config.shell.idle_timeout.as_secs(), 600);
        // Server config defaults
        assert!(config.server.allowed_origins.is_empty());
        assert_eq!(config.server.token, None);
        assert!(!config.server.auto_approve_confirms);
        assert_eq!(config.server.max_message_size, None);
        // Bridge config defaults
        assert!(config.bridge.uds_path.is_none());
        assert!(!config.bridge.heartbeat_mirror);
        assert_eq!(config.bridge.heartbeat_timeout_ms, 250);
    }

    #[test]
    fn read_dedupe_key_parses_defaults_on_and_warns() {
        assert!(super::load_config_from_str("").read_dedupe, "default on");
        for (line, want) in [
            ("read_dedupe = off\n", false),
            ("read_dedupe = false\n", false),
            ("read_dedupe = on\n", true),
            ("read_dedupe = true\n", true),
        ] {
            let c = super::load_config_from_str(line);
            assert_eq!(c.read_dedupe, want, "{line}");
            assert!(c.warnings.is_empty(), "{line}: {:?}", c.warnings);
        }
        let c = super::load_config_from_str("read_dedupe = maybe\n");
        assert!(c.read_dedupe, "bad value keeps the default");
        assert!(c.warnings.iter().any(|w| w.contains("read_dedupe")));
    }

    #[test]
    fn tool_output_spill_keys_parse_defaults_and_warn() {
        let c = super::load_config_from_str("");
        assert_eq!(c.tool_output_spill_bytes, 12 * 1024);
        assert_eq!(c.tool_output_spill_head_bytes, 4 * 1024);
        assert_eq!(c.tool_output_spill_tail_bytes, 3 * 1024);
        let d = super::SynapsConfig::default();
        assert_eq!(d.tool_output_spill_bytes, super::DEFAULT_TOOL_OUTPUT_SPILL_BYTES);
        assert_eq!(d.tool_output_spill_head_bytes, super::DEFAULT_TOOL_OUTPUT_SPILL_HEAD_BYTES);
        assert_eq!(d.tool_output_spill_tail_bytes, super::DEFAULT_TOOL_OUTPUT_SPILL_TAIL_BYTES);

        let c = super::load_config_from_str(concat!(
            "tool_output_spill_bytes = 20000\n",
            "tool_output_spill_head_bytes = 6000\n",
            "tool_output_spill_tail_bytes = 0\n",
        ));
        assert_eq!(c.tool_output_spill_bytes, 20_000);
        assert_eq!(c.tool_output_spill_head_bytes, 6_000);
        assert_eq!(c.tool_output_spill_tail_bytes, 0, "a head-only preview is allowed");
        assert!(c.warnings.is_empty(), "warnings: {:?}", c.warnings);

        let c = super::load_config_from_str("tool_output_spill_bytes = 0\n");
        assert_eq!(c.tool_output_spill_bytes, 0, "0 turns virtualisation off");
        assert!(c.warnings.is_empty(), "warnings: {:?}", c.warnings);

        for key in [
            "tool_output_spill_bytes",
            "tool_output_spill_head_bytes",
            "tool_output_spill_tail_bytes",
        ] {
            let c = super::load_config_from_str(&format!("{key} = lots\n"));
            assert_eq!(c.tool_output_spill_bytes, super::DEFAULT_TOOL_OUTPUT_SPILL_BYTES);
            assert_eq!(c.tool_output_spill_head_bytes, super::DEFAULT_TOOL_OUTPUT_SPILL_HEAD_BYTES);
            assert_eq!(c.tool_output_spill_tail_bytes, super::DEFAULT_TOOL_OUTPUT_SPILL_TAIL_BYTES);
            assert!(
                c.warnings.iter().any(|w| w.contains(key)),
                "{key}: warnings: {:?}",
                c.warnings
            );
            assert!(super::KNOWN_CONFIG_KEYS.contains(&key));
        }
    }

    #[test]
    #[serial]
    fn test_load_config_tools_activation_confirm_modes() {
        for (raw, expected, warns) in [
            ("prompt", ActivationConfirm::Prompt, false),
            ("deny", ActivationConfirm::Deny, false),
            ("AUTO", ActivationConfirm::Auto, false),
            ("garbage", ActivationConfirm::Auto, true),
        ] {
            let home = make_test_home(&format!("activation-confirm-{raw}"));
            let cfg = home.join(".synaps-cli/config");
            std::fs::write(&cfg, format!("tools.activation_confirm = {raw}\n")).unwrap();
            with_home(&home, || {
                let config = load_config();
                assert_eq!(config.tools_activation_confirm, expected, "{raw}");
                assert_eq!(
                    config.warnings.iter().any(|w| w.contains("tools.activation_confirm")),
                    warns,
                    "{raw}: {:?}",
                    config.warnings
                );
            });
            let _ = std::fs::remove_dir_all(&home);
        }
    }

    #[test]
    #[serial]
    fn test_load_config_progressive_tool_disclosure_is_opt_in() {
        let home = make_test_home("progressive-tool-disclosure");
        let cfg = home.join(".synaps-cli/config");
        std::fs::write(&cfg, "progressive_tool_disclosure = true\n").unwrap();

        with_home(&home, || {
            let config = load_config();
            assert!(config.progressive_tool_disclosure);
            assert!(config.warnings.is_empty());
        });

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    #[serial]
    fn test_load_config_bridge_keys() {
        let home = make_test_home("bridge-keys");
        let cfg = home.join(".synaps-cli/config");
        std::fs::write(
            &cfg,
            "\
bridge.uds_path = /tmp/some/control.sock\n\
bridge.heartbeat_mirror = true\n\
bridge.heartbeat_timeout_ms = 750\n\
",
        )
        .unwrap();

        with_home(&home, || {
            let config = load_config();
            assert_eq!(
                config.bridge.uds_path,
                Some(std::path::PathBuf::from("/tmp/some/control.sock")),
            );
            assert!(config.bridge.heartbeat_mirror);
            assert_eq!(config.bridge.heartbeat_timeout_ms, 750);
            assert_eq!(
                config.bridge.resolved_uds_path(),
                std::path::PathBuf::from("/tmp/some/control.sock"),
            );
        });

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn test_bridge_config_defaults() {
        let cfg = BridgeConfig::default();
        assert!(cfg.uds_path.is_none());
        assert!(!cfg.heartbeat_mirror);
        assert_eq!(cfg.heartbeat_timeout_ms, 250);
        // resolved path falls under base_dir()/bridge/control.sock
        let resolved = cfg.resolved_uds_path();
        assert!(resolved.ends_with("bridge/control.sock"));
    }

    #[test]
    #[serial]
    fn test_bridge_heartbeat_mirror_defaults_off_when_unset() {
        let home = make_test_home("bridge-default-off");
        let cfg = home.join(".synaps-cli/config");
        std::fs::write(&cfg, "model = claude-sonnet-4-6\n").unwrap();

        with_home(&home, || {
            let config = load_config();
            assert!(!config.bridge.heartbeat_mirror);
            assert!(config.bridge.uds_path.is_none());
            assert_eq!(config.bridge.heartbeat_timeout_ms, 250);
        });

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn compaction_disclosure_keys_parse_with_typed_warnings() {
        let config = load_config_from_str(
            "compaction_mode = local\ncompaction_exclude = thinking, tool_results, bogus\n",
        );
        assert_eq!(
            config.compaction_mode,
            crate::core::compaction::CompactionMode::LocalOnly
        );
        assert_eq!(
            config.compaction_exclude,
            vec![
                crate::core::compaction::ContentClass::Thinking,
                crate::core::compaction::ContentClass::ToolResults,
            ]
        );
        assert!(
            config.warnings.iter().any(|w| w.contains("bogus")),
            "unknown class must warn: {:?}",
            config.warnings
        );

        let defaults = load_config_from_str("");
        assert_eq!(
            defaults.compaction_mode,
            crate::core::compaction::CompactionMode::Remote
        );
        assert!(defaults.compaction_exclude.is_empty());

        let bad_mode = load_config_from_str("compaction_mode = cloud\n");
        assert_eq!(
            bad_mode.compaction_mode,
            crate::core::compaction::CompactionMode::Remote
        );
        assert!(bad_mode
            .warnings
            .iter()
            .any(|w| w.contains("compaction_mode")));
    }

    #[test]
    #[serial]
    fn test_load_config_server_keys() {
        let home = make_test_home("server-keys");
        let cfg = home.join(".synaps-cli/config");
        std::fs::write(
            &cfg,
            "\
server.allowed_origins = http://localhost:3000, http://localhost:5193\n\
server.token = my-secret-token\n\
server.auto_approve_confirms = true\n\
server.max_message_size = 65536\n\
context_window = 200k\n\
",
        )
        .unwrap();

        with_home(&home, || {
            let config = load_config();
            assert_eq!(
                config.server.allowed_origins,
                vec![
                    "http://localhost:3000".to_string(),
                    "http://localhost:5193".to_string(),
                ]
            );
            assert_eq!(config.server.token, Some("my-secret-token".to_string()));
            assert!(config.server.auto_approve_confirms);
            // Explicit max_message_size takes precedence over context_window derivation
            assert_eq!(config.server.max_message_size, Some(65536));
        });

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    #[serial]
    fn test_server_max_message_size_derived_from_context_window() {
        let home = make_test_home("server-derive");
        let cfg = home.join(".synaps-cli/config");
        std::fs::write(&cfg, "context_window = 200k\n").unwrap();

        with_home(&home, || {
            let config = load_config();
            // 200_000 tokens * 4 bytes/token = 800_000 bytes
            assert_eq!(config.server.max_message_size, Some(800_000));
        });

        let _ = std::fs::remove_dir_all(&home);
    }

    fn make_test_home(subdir: &str) -> std::path::PathBuf {
        let dir = std::path::PathBuf::from(format!("/tmp/synaps-write-test-{}", subdir));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".synaps-cli")).unwrap();
        dir
    }

    fn with_home<F: FnOnce()>(home: &std::path::Path, f: F) {
        // `base_dir()` reads SYNAPS_BASE_DIR before HOME: pin it too, or a value
        // another test left set redirects the write away from `home`.
        let original = std::env::var("HOME").ok();
        let original_base = std::env::var("SYNAPS_BASE_DIR").ok();
        std::env::set_var("HOME", home);
        std::env::set_var("SYNAPS_BASE_DIR", home.join(".synaps-cli"));
        f();
        if let Some(h) = original {
            std::env::set_var("HOME", h);
        } else {
            std::env::remove_var("HOME");
        }
        if let Some(b) = original_base {
            std::env::set_var("SYNAPS_BASE_DIR", b);
        } else {
            std::env::remove_var("SYNAPS_BASE_DIR");
        }
    }

    #[test]
    #[serial]
    fn write_config_value_replaces_existing_key() {
        let home = make_test_home("replace");
        let cfg = home.join(".synaps-cli/config");
        std::fs::write(&cfg, "model = claude-opus-4-6\nthinking = low\n").unwrap();

        with_home(&home, || {
            write_config_value("model", "claude-sonnet-4-6").unwrap();
        });

        let contents = std::fs::read_to_string(&cfg).unwrap();
        assert!(contents.contains("model = claude-sonnet-4-6"));
        assert!(contents.contains("thinking = low"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    #[serial]
    fn write_config_value_appends_when_missing() {
        let home = make_test_home("append");
        let cfg = home.join(".synaps-cli/config");
        std::fs::write(&cfg, "model = claude-opus-4-6\n").unwrap();

        with_home(&home, || {
            write_config_value("theme", "dracula").unwrap();
        });

        let contents = std::fs::read_to_string(&cfg).unwrap();
        assert!(contents.contains("model = claude-opus-4-6"));
        assert!(contents.contains("theme = dracula"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    #[serial]
    fn write_config_value_concurrent_writers_lose_nothing() {
        let home = make_test_home("concurrent");
        let cfg = home.join(".synaps-cli/config");
        std::fs::write(&cfg, "# keep\n").unwrap();

        // HOME is process-global; set it for the whole test, not per thread.
        with_home(&home, || {
            let a = std::thread::spawn(|| {
                for i in 0..200 {
                    write_config_value("a_key", &i.to_string()).unwrap();
                }
            });
            let b = std::thread::spawn(|| {
                for i in 0..200 {
                    write_config_value("b_key", &i.to_string()).unwrap();
                }
            });
            a.join().unwrap();
            b.join().unwrap();
        });

        let contents = std::fs::read_to_string(&cfg).unwrap();
        assert!(contents.contains("# keep"), "{contents}");
        assert!(contents.contains("a_key = 199"), "{contents}");
        assert!(contents.contains("b_key = 199"), "{contents}");
        assert_eq!(contents.matches("a_key").count(), 1);
        assert_eq!(contents.matches("b_key").count(), 1);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let lock = home.join(".synaps-cli/config.lock");
            let mode = std::fs::metadata(&lock).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    #[serial]
    fn config_lock_kill_switch_still_writes() {
        let home = make_test_home("lock-off");
        let cfg = home.join(".synaps-cli/config");
        std::env::set_var("SYNAPS_CONFIG_LOCK", "0");
        with_home(&home, || {
            write_config_value("model", "claude-sonnet-4-6").unwrap();
        });
        std::env::remove_var("SYNAPS_CONFIG_LOCK");
        let contents = std::fs::read_to_string(&cfg).unwrap();
        assert!(contents.contains("model = claude-sonnet-4-6"));
        assert!(!home.join(".synaps-cli/config.lock").exists());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    #[serial]
    fn add_favorite_model_end_to_end_does_not_deadlock() {
        let home = make_test_home("favorite-lock");
        let cfg = home.join(".synaps-cli/config");
        with_home(&home, || {
            add_favorite_model("anthropic/claude-sonnet-4-6").unwrap();
            add_favorite_model("openai/gpt-5").unwrap();
            remove_favorite_model("anthropic/claude-sonnet-4-6").unwrap();
        });
        let contents = std::fs::read_to_string(&cfg).unwrap();
        assert!(contents.contains("favorite_models = openai/gpt-5"), "{contents}");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    #[serial]
    fn write_config_value_preserves_comments() {
        let home = make_test_home("comments");
        let cfg = home.join(".synaps-cli/config");
        std::fs::write(&cfg, "# user comment\nmodel = claude-opus-4-6\n# another\n").unwrap();

        with_home(&home, || {
            write_config_value("model", "claude-sonnet-4-6").unwrap();
        });

        let contents = std::fs::read_to_string(&cfg).unwrap();
        assert!(contents.contains("# user comment"));
        assert!(contents.contains("# another"));
        assert!(contents.contains("model = claude-sonnet-4-6"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    #[serial]
    fn write_config_value_preserves_unknown_keys() {
        let home = make_test_home("unknown");
        let cfg = home.join(".synaps-cli/config");
        std::fs::write(&cfg, "custom_thing = 42\nmodel = claude-opus-4-6\n").unwrap();

        with_home(&home, || {
            write_config_value("model", "claude-sonnet-4-6").unwrap();
        });

        let contents = std::fs::read_to_string(&cfg).unwrap();
        assert!(contents.contains("custom_thing = 42"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    #[serial]
    fn write_config_value_creates_file_if_absent() {
        let home = make_test_home("create");
        let cfg = home.join(".synaps-cli/config");
        assert!(!cfg.exists());

        with_home(&home, || {
            write_config_value("model", "claude-sonnet-4-6").unwrap();
        });

        let contents = std::fs::read_to_string(&cfg).unwrap();
        assert!(contents.contains("model = claude-sonnet-4-6"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    #[serial]
    fn load_config_parses_theme_key() {
        let dir = std::path::PathBuf::from("/tmp/synaps-config-test-theme/.synaps-cli");
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("config"), "theme = dracula\n").unwrap();

        let original_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", "/tmp/synaps-config-test-theme");

        let config = load_config();

        if let Some(home) = original_home {
            std::env::set_var("HOME", home);
        } else {
            std::env::remove_var("HOME");
        }
        let _ = std::fs::remove_dir_all("/tmp/synaps-config-test-theme");

        assert_eq!(config.theme.as_deref(), Some("dracula"));
    }

    #[test]
    #[serial]
    fn test_load_config_disable_lists() {
        let test_dir =
            std::path::PathBuf::from("/tmp/synaps-config-test-disable-lists/.synaps-cli");
        let _ = std::fs::create_dir_all(&test_dir);
        let config_path = test_dir.join("config");

        let config_content = r#"
# Test config with disable lists
favorite_models = claude/claude-opus-4-7, groq/llama-3.3-70b-versatile

disabled_plugins = foo, bar
disabled_skills = baz, plug:qual
"#;
        std::fs::write(&config_path, config_content).unwrap();

        let original_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", "/tmp/synaps-config-test-disable-lists");

        let config = load_config();

        if let Some(home) = original_home {
            std::env::set_var("HOME", home);
        } else {
            std::env::remove_var("HOME");
        }

        let _ = std::fs::remove_dir_all("/tmp/synaps-config-test-disable-lists");

        assert_eq!(
            config.disabled_plugins,
            vec!["foo".to_string(), "bar".to_string()]
        );
        assert_eq!(
            config.favorite_models,
            vec![
                "claude/claude-opus-4-7".to_string(),
                "groq/llama-3.3-70b-versatile".to_string(),
            ]
        );
        assert_eq!(
            config.disabled_skills,
            vec!["baz".to_string(), "plug:qual".to_string()]
        );
    }

    #[test]
    #[serial]
    fn favorite_model_helpers_round_trip_through_config_file() {
        let home = make_test_home("favorite-models");
        let cfg = home.join(".synaps-cli/config");
        std::fs::write(&cfg, "model = claude-opus-4-7\n").unwrap();

        with_home(&home, || {
            add_favorite_model("groq/llama-3.3-70b-versatile").unwrap();
            add_favorite_model("claude/claude-opus-4-7").unwrap();
            add_favorite_model("groq/llama-3.3-70b-versatile").unwrap();
            assert!(is_favorite_model("groq/llama-3.3-70b-versatile"));
            remove_favorite_model("groq/llama-3.3-70b-versatile").unwrap();
            assert!(!is_favorite_model("groq/llama-3.3-70b-versatile"));
            assert!(is_favorite_model("claude/claude-opus-4-7"));
        });

        let contents = std::fs::read_to_string(&cfg).unwrap();
        assert!(contents.contains("model = claude-opus-4-7"));
        assert!(contents.contains("favorite_models = claude/claude-opus-4-7"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    #[serial]
    fn test_load_config_new_keys() {
        // Create a temporary config directory with the new keys
        let test_dir = std::path::PathBuf::from("/tmp/synaps-config-test-new-keys/.synaps-cli");
        let _ = std::fs::create_dir_all(&test_dir);
        let config_path = test_dir.join("config");

        let config_content = r#"
# Test config with new keys
model = claude-haiku
thinking = medium
max_tool_output = 50000
bash_timeout = 45
bash_max_timeout = 600
subagent_timeout = 120
api_retries = 5
"#;
        std::fs::write(&config_path, config_content).unwrap();

        // Temporarily override the config path for this test
        let original_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", "/tmp/synaps-config-test-new-keys");

        let config = load_config();

        // Restore original HOME
        if let Some(home) = original_home {
            std::env::set_var("HOME", home);
        } else {
            std::env::remove_var("HOME");
        }

        // Cleanup
        let _ = std::fs::remove_dir_all("/tmp/synaps-config-test-new-keys");

        assert_eq!(config.model, Some("claude-haiku".to_string()));
        assert_eq!(config.thinking_budget, Some(4096)); // medium = 4096
        assert_eq!(config.max_tool_output, 50000);
        assert_eq!(config.bash_timeout, 45);
        assert_eq!(config.bash_max_timeout, 600);
        assert_eq!(config.subagent_timeout, 120);
        assert_eq!(config.api_retries, 5);
    }

    // ── auth.* config + credential source (#157) ──

    // ── events.auto_turn parser ──────────────────────────────────────────────

    #[test]
    fn events_auto_turn_explicit_true_values() {
        for val in &["true", "TRUE", "1", "yes", "YES", "on", "ON"] {
            let cfg = load_config_from_str(&format!("events.auto_turn = {val}"));
            assert!(
                cfg.events.auto_turn,
                "expected true for events.auto_turn = {val}"
            );
        }
    }

    #[test]
    fn events_auto_turn_explicit_false_values() {
        for val in &["false", "FALSE", "0", "no", "NO", "off", "OFF"] {
            let cfg = load_config_from_str(&format!("events.auto_turn = {val}"));
            assert!(
                !cfg.events.auto_turn,
                "expected false for events.auto_turn = {val}"
            );
        }
    }

    #[test]
    fn events_auto_turn_typo_fails_safe_false() {
        // Unrecognised value should fail safe to false (with a warning on stderr).
        let cfg = load_config_from_str("events.auto_turn = fales");
        assert!(
            !cfg.events.auto_turn,
            "typo 'fales' must fail safe to false"
        );
    }

    // ── events.auto_turn_cap parser ──────────────────────────────────────────

    #[test]
    fn events_auto_turn_cap_default_is_five() {
        let cfg = load_config_from_str("");
        assert_eq!(cfg.events.auto_turn_cap, 5);
        assert_eq!(DEFAULT_AUTO_TURN_CAP, 5);
    }

    #[test]
    fn events_auto_turn_cap_zero_is_unlimited() {
        let cfg = load_config_from_str("events.auto_turn_cap = 0");
        assert_eq!(cfg.events.auto_turn_cap, 0);
    }

    #[test]
    fn events_auto_turn_cap_unlimited_aliases() {
        for val in ["unlimited", "inf", "infinite", "Infinity", "UNLIMITED", " inf "] {
            let cfg = load_config_from_str(&format!("events.auto_turn_cap = {val}"));
            assert_eq!(
                cfg.events.auto_turn_cap, 0,
                "expected 0 for events.auto_turn_cap = {val:?}"
            );
        }
    }

    #[test]
    fn events_auto_turn_cap_explicit_number() {
        let cfg = load_config_from_str("events.auto_turn_cap = 12");
        assert_eq!(cfg.events.auto_turn_cap, 12);
    }

    #[test]
    fn watch_keys_default_parse_and_report_bad_values() {
        let d = load_config_from_str("").watch;
        assert_eq!(d.small_models, vec!["muse/muse-wonder-1.0".to_owned()]);
        assert_eq!(d.admission, WatchAdmission::Refuse);
        assert_eq!((d.lease_secs, d.lease_rounds), (7200, 40));
        assert_eq!(d.lease_cost_usd, None, "no default spend cap");
        assert_eq!(d.advice_timeout_ms, 1000);
        assert_eq!(d.advice_min_confidence, 0.95);
        let content = concat!(
            "subagent.watch.small_models = muse/muse-wonder-1.0, local/tiny\n",
            "subagent.watch.admission = off\n",
            "subagent.watch.lease_secs = 600\n",
            "subagent.watch.lease_rounds = 12\n",
            "subagent.watch.lease_cost_usd = 0.50\n",
            "subagent.watch.advice_timeout_ms = 0\n",
            "subagent.watch.advice_min_confidence = 0.9\n",
        );
        let c = load_config_from_str(content);
        assert!(
            config_content_problems(content).is_empty(),
            "{:?}",
            config_content_problems(content)
        );
        assert_eq!(
            c.watch.small_models,
            vec!["muse/muse-wonder-1.0".to_owned(), "local/tiny".to_owned()]
        );
        assert_eq!(c.watch.admission, WatchAdmission::Off);
        assert_eq!((c.watch.lease_secs, c.watch.lease_rounds), (600, 12));
        assert_eq!(c.watch.lease_cost_usd, Some(0.5));
        assert_eq!(c.watch.advice_timeout_ms, 0);
        assert_eq!(c.watch.advice_min_confidence, 0.9);
        assert_eq!(
            c.subagent,
            SubagentLimits::default(),
            "not the subagent.* family"
        );
        for bad in [
            "subagent.watch.admission = maybe",
            "subagent.watch.lease_secs = 5",
            "subagent.watch.lease_rounds = 0",
            "subagent.watch.lease_cost_usd = -1",
            "subagent.watch.advice_timeout_ms = 60000",
            "subagent.watch.advice_min_confidence = 1.5",
        ] {
            assert_eq!(
                load_config_from_str(bad).watch,
                WatchConfig::default(),
                "{bad}"
            );
            assert!(
                !config_content_problems(bad).is_empty(),
                "{bad} is reported"
            );
        }
        for k in FAMILY_CONFIG_KEYS
            .iter()
            .filter(|k| k.starts_with("subagent.watch."))
        {
            assert!(!KNOWN_CONFIG_KEYS.contains(k), "{k}");
        }
    }

    #[test]
    fn wait_max_inline_secs_defaults_accepts_its_range_and_reports_the_rest() {
        assert_eq!(load_config_from_str("").wait.max_inline_secs, 3600);
        for (val, want) in [("1", 1), ("600", 600), ("86400", 86_400)] {
            let cfg = load_config_from_str(&format!("wait.max_inline_secs = {val}"));
            assert_eq!(cfg.wait.max_inline_secs, want, "{val}");
        }
        for bad in ["0", "86401", "-1", "an hour"] {
            let content = format!("wait.max_inline_secs = {bad}");
            assert_eq!(load_config_from_str(&content).wait.max_inline_secs, 3600, "{bad:?}");
            assert!(!config_content_problems(&content).is_empty(), "{bad:?} is reported");
        }
        assert!(config_content_problems("wait.max_inline_secs = 7200").is_empty());
    }

    #[test]
    fn events_auto_turn_cap_garbage_keeps_default() {
        for val in ["banana", "-3", "5.5", "", "true"] {
            let cfg = load_config_from_str(&format!("events.auto_turn_cap = {val}"));
            assert_eq!(
                cfg.events.auto_turn_cap, 5,
                "garbage {val:?} must keep default 5"
            );
        }
    }

    #[test]
    fn events_auto_turn_cap_independent_of_auto_turn() {
        let cfg = load_config_from_str("events.auto_turn = false\nevents.auto_turn_cap = 0");
        assert!(!cfg.events.auto_turn);
        assert_eq!(cfg.events.auto_turn_cap, 0);
    }

    #[test]
    fn events_auto_turn_default_is_true() {
        let cfg = load_config_from_str("model = claude-haiku");
        assert!(cfg.events.auto_turn, "default must be true when key absent");
    }

    #[test]
    #[serial]
    fn test_auth_config_parses_endpoint_and_token() {
        let home = std::env::temp_dir().join(format!("synaps-auth-test-{}", std::process::id()));
        let dir = home.join(".synaps-cli");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config"),
            "auth.remote_endpoint = https://jade.jade:8181\nauth.machine_token = machine-abc\n",
        )
        .unwrap();
        with_home(&home, || {
            let config = load_config();
            assert_eq!(
                config.auth.remote_endpoint.as_deref(),
                Some("https://jade.jade:8181")
            );
            assert_eq!(config.auth.machine_token.as_deref(), Some("machine-abc"));
            // auth.* are namespaced -> must NOT warn as unknown keys
            assert!(
                !config.warnings.iter().any(|w| w.contains("auth.")),
                "{:?}",
                config.warnings
            );
        });
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    #[serial]
    fn test_credential_source_local_by_default() {
        std::env::remove_var("SYNAPS_AUTH_ENDPOINT");
        std::env::remove_var("SYNAPS_MACHINE_TOKEN");
        assert!(!AuthConfig::default().credential_source().is_remote());
    }

    #[test]
    #[serial]
    fn test_credential_source_remote_from_config() {
        std::env::remove_var("SYNAPS_AUTH_ENDPOINT");
        std::env::remove_var("SYNAPS_MACHINE_TOKEN");
        let auth = AuthConfig {
            remote_endpoint: Some("https://b".into()),
            machine_token: Some("m".into()),
            accounts: BTreeMap::new(),
            auto: BTreeMap::new(),
            order: BTreeMap::new(),
        };
        assert_eq!(
            auth.credential_source(),
            crate::core::auth::CredentialSource::Remote {
                endpoint: "https://b".into(),
                machine_token: "m".into()
            }
        );
    }

    #[test]
    fn auth_account_keys_parse_and_invalid_fail_closed_with_warning() {
        let config = load_config_from_str(
            "auth.account.openai-codex = astra2\nauth.account.anthropic = auto\nauth.account.kimi-code = Bad Label\nauth.account.groq = x\n",
        );
        assert_eq!(
            config.auth.accounts.get("openai-codex").map(String::as_str),
            Some("astra2")
        );
        assert_eq!(
            config.auth.accounts.get("anthropic").map(String::as_str),
            Some("auto")
        );
        let policy = config.auth.account_policy();
        use crate::core::auth::{AccountSelector, OAuthProviderId};
        assert!(matches!(
            policy.selector(OAuthProviderId::OpenAiCodex),
            AccountSelector::Account(_)
        ));
        assert_eq!(
            policy.selector(OAuthProviderId::Anthropic),
            AccountSelector::Auto
        );
        assert!(
            matches!(
                policy.selector(OAuthProviderId::KimiCode),
                AccountSelector::Invalid { .. }
            ),
            "malformed label must fail closed"
        );
        assert!(
            config
                .warnings
                .iter()
                .any(|w| w.contains("auth.account.kimi-code")),
            "{:?}",
            config.warnings
        );
        assert!(
            config
                .warnings
                .iter()
                .any(|w| w.contains("auth.account.groq")),
            "{:?}",
            config.warnings
        );
        assert!(
            !config
                .warnings
                .iter()
                .any(|w| w.contains("auth.account.openai-codex")),
            "{:?}",
            config.warnings
        );
    }

    #[test]
    #[serial]
    fn test_credential_source_env_overrides_config() {
        let auth = AuthConfig {
            remote_endpoint: Some("https://config-host".into()),
            machine_token: Some("config-tok".into()),
            accounts: BTreeMap::new(),
            auto: BTreeMap::new(),
            order: BTreeMap::new(),
        };
        std::env::set_var("SYNAPS_AUTH_ENDPOINT", "https://env-host");
        std::env::set_var("SYNAPS_MACHINE_TOKEN", "env-tok");
        let src = auth.credential_source();
        std::env::remove_var("SYNAPS_AUTH_ENDPOINT");
        std::env::remove_var("SYNAPS_MACHINE_TOKEN");
        assert_eq!(
            src,
            crate::core::auth::CredentialSource::Remote {
                endpoint: "https://env-host".into(),
                machine_token: "env-tok".into()
            }
        );
    }
    /// #147 C1 / prA N1: only an explicit token travels in the source; the
    /// local file is resolved by the client, for loopback + none configured.
    #[test]
    #[serial]
    fn test_credential_source_records_token_provenance_and_file_is_resolved_lazily() {
        use crate::core::auth::MachineToken;
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("SYNAPS_BASE_DIR");
        std::env::set_var("SYNAPS_BASE_DIR", dir.path());
        std::env::remove_var("SYNAPS_AUTH_ENDPOINT");
        std::env::remove_var("SYNAPS_MACHINE_TOKEN");
        std::fs::write(dir.path().join("auth-broker.token"), "local-file-tok\n").unwrap();
        let source = |endpoint: &str, token: Option<&str>| {
            AuthConfig {
                remote_endpoint: Some(endpoint.into()),
                machine_token: token.map(str::to_string),
                accounts: BTreeMap::new(),
                auto: BTreeMap::new(),
                order: BTreeMap::new(),
            }
            .credential_source()
        };
        let remote_tok = |s: crate::core::auth::CredentialSource| match s {
            crate::core::auth::CredentialSource::Remote { machine_token, .. } => machine_token,
            other => panic!("{other:?}"),
        };
        // The source carries only the explicit token: empty = "none configured".
        let loopback = remote_tok(source("http://127.0.0.1:8181", None));
        let explicit = remote_tok(source("http://127.0.0.1:8181", Some("cfg-tok")));
        let remote = remote_tok(source("https://jade.jade:8181", None));
        assert_eq!(loopback, "");
        assert_eq!(explicit, "cfg-tok");
        assert_eq!(remote, "");
        // The client resolves the file only for "none configured" + loopback.
        let auto = MachineToken::for_endpoint("http://localhost:8181", &loopback);
        let fixed = MachineToken::for_endpoint("http://127.0.0.1:8181", &explicit);
        let far = MachineToken::for_endpoint("https://jade.jade:8181", &remote);
        match prev {
            Some(v) => std::env::set_var("SYNAPS_BASE_DIR", v),
            None => std::env::remove_var("SYNAPS_BASE_DIR"),
        }
        assert_eq!(auto.get(), "local-file-tok");
        assert_eq!(fixed.get(), "cfg-tok", "explicit token is never replaced");
        assert_eq!(far.get(), "", "never read for a non-loopback broker");
    }

    #[test]
    fn config_parses_ultracode_as_distinct_canonical_level() {
        let cfg = load_config_from_str("thinking = ultracode\n");
        assert_eq!(
            cfg.thinking_level,
            Some(crate::reasoning::ReasoningLevel::UltraCode)
        );
        assert_eq!(cfg.thinking_budget, None);
        for other in [
            crate::reasoning::ReasoningLevel::Ultra,
            crate::reasoning::ReasoningLevel::Max,
            crate::reasoning::ReasoningLevel::XHigh,
        ] {
            assert_ne!(cfg.thinking_level, Some(other));
        }
    }

    // ── /settings coverage support ────────────────────────────────────────

    /// `context_window = auto` is what the /settings cycler and `/context
    /// auto` write: it means "model default" (no override) and must not warn.
    #[test]
    fn context_window_auto_is_model_default_without_warning() {
        for spelling in ["auto", "AUTO", "Auto"] {
            let cfg = load_config_from_str(&format!("context_window = {spelling}\n"));
            assert_eq!(cfg.context_window, None, "{spelling}");
            assert!(cfg.warnings.is_empty(), "{spelling}: {:?}", cfg.warnings);
        }
        // `auto` after an explicit window clears it.
        let cfg = load_config_from_str("context_window = 1m\ncontext_window = auto\n");
        assert_eq!(cfg.context_window, None);
        assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);
        // Existing spellings are unchanged; garbage still warns.
        assert_eq!(
            load_config_from_str("context_window = 200k\n").context_window,
            Some(200_000)
        );
        let bad = load_config_from_str("context_window = huge\n");
        assert_eq!(bad.context_window, None);
        assert!(bad.warnings.iter().any(|w| w.contains("context_window")));
    }

    /// The /settings sidecar row writes `sidecar_toggle_key`; it is read by
    /// the keybind registry and must not trip the unknown-key warning.
    #[test]
    fn sidecar_toggle_key_is_a_known_key() {
        let cfg = load_config_from_str("sidecar_toggle_key = F2\n");
        assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);
        assert!(KNOWN_CONFIG_KEYS.contains(&"sidecar_toggle_key"));
        assert!(KNOWN_CONFIG_KEYS.contains(&"compaction_mode"));
        assert!(KNOWN_CONFIG_KEYS.contains(&"compaction_exclude"));
    }

    /// `config_content_problems` sees the stderr-only family diagnostics
    /// (shell/events/turn_budget/memory) as well as `warnings`, and nothing
    /// for valid values.
    #[test]
    fn config_content_problems_covers_family_diagnostics() {
        for bad in [
            "shell.max_sessions = lots",
            "shell.readiness_strategy = eventually",
            "events.auto_turn = maybe",
            "events.auto_turn_cap = many",
            "turn_budget.worker.max_cost_usd = cheap",
            "turn_budget.nobody.max_tool_calls = 3",
            "memory.capture_tools = everything",
            "memory.recall_max_records = -1",
            "memory.user_scope = yes",
            "cache_ttl = forever",
        ] {
            let problems = config_content_problems(bad);
            assert!(!problems.is_empty(), "{bad} should be reported");
        }
        for good in [
            "shell.max_sessions = 8",
            "shell.readiness_strategy = prompt",
            "events.auto_turn = off",
            "events.auto_turn_cap = unlimited",
            "turn_budget.worker.max_cost_usd = 2.5",
            "memory.capture_tools = off",
            "memory.recall_max_records = 4",
            "memory.user_scope = true",
            "cache_ttl = hybrid",
            "cache_ttl = auto",
        ] {
            let problems = config_content_problems(good);
            assert!(problems.is_empty(), "{good}: {problems:?}");
        }
        // The family values still land exactly as before.
        let cfg = load_config_from_str("shell.max_sessions = 8\nevents.auto_turn = maybe\n");
        assert_eq!(cfg.shell.max_sessions, 8);
        assert!(!cfg.events.auto_turn, "unrecognised auto_turn still fails safe to false");
    }

    #[test]
    fn bare_or_empty_auth_family_keys_warn() {
        let cfg = load_config_from_str(
            "auth.account = work\nauth.account. = work\nauth.auto = on\nauth.auto. = 1\n",
        );
        let w = &cfg.warnings;
        for key in ["auth.account", "auth.account."] {
            assert!(
                w.contains(&format!("unknown key '{key}' (did you mean 'auth.account.<provider>'?)")),
                "{key}: {w:?}"
            );
        }
        for key in ["auth.auto", "auth.auto."] {
            assert!(
                w.contains(&format!("unknown key '{key}' (did you mean 'auth.auto.<key>'?)")),
                "{key}: {w:?}"
            );
        }
        assert!(cfg.auth.accounts.is_empty() && cfg.auth.auto.is_empty());
    }

    #[test]
    fn unknown_auth_keys_warn_with_a_suggestion() {
        let cfg = load_config_from_str(
            "auth.acount.anthropic = work\nauth.remote_endpont = x\nauth.bogus = 1\n\
             auth.account.anthropic = work\nauth.machine_token = t\n",
        );
        let w = &cfg.warnings;
        assert!(
            w.contains(&"unknown key 'auth.acount.anthropic' (did you mean 'auth.account.anthropic'?)".into()),
            "{w:?}"
        );
        assert!(
            w.contains(&"unknown key 'auth.remote_endpont' (did you mean 'auth.remote_endpoint'?)".into()),
            "{w:?}"
        );
        assert!(w.contains(&"unknown key 'auth.bogus' — ignored".into()), "{w:?}");
        assert_eq!(w.iter().filter(|w| w.contains("auth.")).count(), 3, "{w:?}");
        assert_eq!(cfg.auth.accounts.get("anthropic").map(String::as_str), Some("work"));
        assert!(!cfg.auth.accounts.contains_key("acount.anthropic"));
    }

    /// An absolute path on this platform: `/opt/axel` has no drive on Windows.
    #[cfg(windows)]
    const ABSOLUTE_SAMPLE_PATH: &str = r"C:\opt\axel";
    #[cfg(not(windows))]
    const ABSOLUTE_SAMPLE_PATH: &str = "/opt/axel";

    /// Every listed family key is accepted by its parser for some value
    /// (catches a typo in `FAMILY_CONFIG_KEYS`).
    #[test]
    fn family_config_keys_are_real_parser_keys() {
        let sample = |key: &str| -> &'static str {
            match key {
                "shell.readiness_strategy" => "hybrid",
                "memory.backend" => "legacy",
                "memory.user_scope" | "memory.default_mode_confirmed" => "true",
                "memory.default_mode" => "off",
                "memory.capture_tools" => "summary_only",
                "memory.capture_assistant" | "memory.capture_user" => "on",
                "memory.auto_consolidate" | "memory.local_embeddings" | "memory.gliner" => "off",
                "memory.axel.executable" | "memory.axel.brain" => ABSOLUTE_SAMPLE_PATH,
                "events.auto_turn" => "on",
                "server.allowed_origins" | "server.token" => "x",
                "server.auto_approve_confirms" | "bridge.heartbeat_mirror" => "false",
                "bridge.uds_path" => "/tmp/x.sock",
                "auth.remote_endpoint" | "auth.machine_token" => "x",
                "subagent.watch.admission" => "refuse",
                "subagent.watch.lease_secs" => "600",
                "subagent.watch.advice_min_confidence" => "0.5",
                _ => "7",
            }
        };
        for key in FAMILY_CONFIG_KEYS.iter().filter(|k| !k.ends_with(".*")) {
            assert!(!KNOWN_CONFIG_KEYS.contains(key), "{key} is listed twice");
            let content = format!("{key} = {}\n", sample(key));
            let problems = config_content_problems(&content);
            assert!(problems.is_empty(), "{key}: {problems:?}");
        }
    }

    #[test]
    #[serial]
    fn validate_config_edit_uses_the_real_parser_and_whole_file() {
        let home = make_test_home("validate-edit");
        let cfg = home.join(".synaps-cli/config");
        with_home(&home, || {
            // Single-key value checks.
            assert!(validate_config_edit("cache_ttl", "1h").is_ok());
            assert!(validate_config_edit("cache_ttl", "auto").is_ok());
            assert!(validate_config_edit("cache_ttl", "2h").is_err());
            assert!(validate_config_edit("shell.max_output", "big").is_err());
            assert!(validate_config_edit("memory.axel.brain", "relative/brain").is_err());
            assert!(validate_config_edit("memory.axel.brain", ABSOLUTE_SAMPLE_PATH).is_ok());
            assert!(validate_config_edit("model", "a\nb").is_err());

            // Cross-key rules see the rest of the file.
            std::fs::write(&cfg, "context_management.rollover_tokens = 100000\n").unwrap();
            assert!(validate_config_edit("context_management.pressure_tokens", "90000").is_ok());
            assert!(validate_config_edit("context_management.pressure_tokens", "150000").is_err());
            std::fs::write(&cfg, "subagent.max_total = 8\n").unwrap();
            assert!(validate_config_edit("subagent.max_concurrent", "8").is_ok());
            assert!(validate_config_edit("subagent.max_concurrent", "9").is_err());
            std::fs::write(&cfg, "").unwrap();
            assert!(validate_config_edit("memory.default_mode", "recall_once").is_err());
            assert!(validate_config_edit("memory.default_mode", "off").is_ok());
            std::fs::write(&cfg, "memory.default_mode_confirmed = true\n").unwrap();
            assert!(validate_config_edit("memory.default_mode", "recall_once").is_ok());

            // A problem already in the file is not blamed on an unrelated edit.
            std::fs::write(&cfg, "cache_ttl = forever\n").unwrap();
            assert!(validate_config_edit("max_fps", "120").is_ok());
            // Validation never writes.
            assert_eq!(std::fs::read_to_string(&cfg).unwrap(), "cache_ttl = forever\n");
        });
        let _ = std::fs::remove_dir_all(&home);
    }

    // ── write_config_values_checked (`synaps config set`) ──

    fn owned_pairs(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn config_set(
        pairs: &[(&str, &str)],
        mode: &ConfigSetMode,
    ) -> Result<ConfigSetOutcome, ConfigSetError> {
        write_config_values_checked(&owned_pairs(pairs), mode, None)
    }

    fn expect(pairs: &[(&str, &str)]) -> ConfigSetMode {
        ConfigSetMode::Expect(owned_pairs(pairs))
    }

    /// The desktop installer's pair.
    fn installer_pairs() -> [(&'static str, &'static str); 2] {
        [("memory.backend", "axel"), ("memory.axel.executable", ABSOLUTE_SAMPLE_PATH)]
    }

    /// `key=status` per key, in outcome order.
    fn key_statuses(outcome: &ConfigSetOutcome) -> Vec<String> {
        outcome.keys.iter().map(|(key, status)| format!("{key}={}", status.as_str())).collect()
    }

    #[test]
    #[serial]
    fn config_set_writes_every_pair() {
        let home = make_test_home("set-written");
        let cfg = home.join(".synaps-cli/config");
        std::fs::write(&cfg, "model = claude-opus-4-6\nmemory.backend = legacy\n").unwrap();
        with_home(&home, || {
            let outcome = config_set(&installer_pairs(), &ConfigSetMode::Overwrite).unwrap();
            assert_eq!(outcome.result, ConfigSetResult::Written);
            assert_eq!(
                key_statuses(&outcome),
                ["memory.backend=written", "memory.axel.executable=written"]
            );
        });
        let contents = std::fs::read_to_string(&cfg).unwrap();
        assert_eq!(
            contents,
            format!(
                "model = claude-opus-4-6\nmemory.backend = axel\n\
                 memory.axel.executable = {ABSOLUTE_SAMPLE_PATH}\n"
            )
        );
        assert!(config_content_problems(&contents).is_empty());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    #[serial]
    fn config_set_unchanged_when_every_value_is_already_set() {
        let home = make_test_home("set-unchanged");
        let cfg = home.join(".synaps-cli/config");
        // Spaced unlike a write would space it: values compare trimmed, and
        // an unchanged call must not rewrite the file.
        let original =
            format!("memory.backend=axel\nmemory.axel.executable =  {ABSOLUTE_SAMPLE_PATH}  \n");
        std::fs::write(&cfg, &original).unwrap();
        with_home(&home, || {
            for mode in [ConfigSetMode::Overwrite, ConfigSetMode::IfUnset] {
                let outcome = config_set(&installer_pairs(), &mode).unwrap();
                assert_eq!(outcome.result, ConfigSetResult::Unchanged, "{mode:?}");
                assert_eq!(
                    key_statuses(&outcome),
                    ["memory.backend=unchanged", "memory.axel.executable=unchanged"]
                );
            }
            let padded = config_set(&[("memory.backend", " axel ")], &ConfigSetMode::Overwrite);
            assert_eq!(padded.unwrap().result, ConfigSetResult::Unchanged);
        });
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), original);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    #[serial]
    fn config_set_if_unset_conflict_refuses_the_whole_call() {
        let home = make_test_home("set-conflict");
        let cfg = home.join(".synaps-cli/config");
        let original = "# set by hand\nmemory.backend = legacy\n";
        std::fs::write(&cfg, original).unwrap();
        with_home(&home, || {
            let outcome = config_set(&installer_pairs(), &ConfigSetMode::IfUnset).unwrap();
            assert_eq!(outcome.result, ConfigSetResult::Refused);
            // The absent key is not written either.
            assert_eq!(
                key_statuses(&outcome),
                ["memory.backend=conflict", "memory.axel.executable=unchanged"]
            );
        });
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), original);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    #[serial]
    fn config_set_expect_met_writes() {
        let home = make_test_home("set-expect-met");
        let cfg = home.join(".synaps-cli/config");
        std::fs::write(&cfg, "memory.backend = legacy\n").unwrap();
        with_home(&home, || {
            // Compare-and-set on the written key plus an expect-only key.
            let mode = expect(&[("memory.backend", "legacy"), ("memory.axel.executable", "")]);
            let outcome = config_set(&[("memory.backend", "axel")], &mode).unwrap();
            assert_eq!(outcome.result, ConfigSetResult::Written);
            assert_eq!(
                key_statuses(&outcome),
                ["memory.backend=written", "memory.axel.executable=unchanged"]
            );
        });
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), "memory.backend = axel\n");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    #[serial]
    fn config_set_expect_failed_refuses() {
        let home = make_test_home("set-expect-failed");
        let cfg = home.join(".synaps-cli/config");
        let original = "memory.backend = legacy\n";
        std::fs::write(&cfg, original).unwrap();
        with_home(&home, || {
            let pairs = [("memory.axel.executable", ABSOLUTE_SAMPLE_PATH)];
            let outcome = config_set(&pairs, &expect(&[("memory.backend", "axel")])).unwrap();
            assert_eq!(outcome.result, ConfigSetResult::Refused);
            assert_eq!(
                key_statuses(&outcome),
                ["memory.axel.executable=unchanged", "memory.backend=expect_failed"]
            );
        });
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), original);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    #[serial]
    fn config_set_expect_empty_means_unset() {
        let home = make_test_home("set-expect-unset");
        let cfg = home.join(".synaps-cli/config");
        with_home(&home, || {
            let mode = expect(&[("memory.backend", "")]);
            let pairs = [("memory.backend", "axel")];
            // No file at all: unset.
            assert_eq!(config_set(&pairs, &mode).unwrap().result, ConfigSetResult::Written);
            // Now it is set.
            let again = config_set(&pairs, &mode).unwrap();
            assert_eq!(again.result, ConfigSetResult::Refused);
            assert_eq!(key_statuses(&again), ["memory.backend=expect_failed"]);
            // A `key =` line with nothing after it is set too.
            std::fs::write(&cfg, "memory.backend =\n").unwrap();
            let empty = config_set(&pairs, &mode).unwrap();
            assert_eq!(empty.result, ConfigSetResult::Refused);
        });
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), "memory.backend =\n");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    #[serial]
    fn config_set_refuses_keys_outside_the_allowlist() {
        let home = make_test_home("set-not-settable");
        let base = home.join(".synaps-cli");
        with_home(&home, || {
            // A known key, an unknown key, and an allowlisted one.
            let pairs = [("memory.backend", "axel"), ("model", "claude-opus-4-6"), ("nope", "1")];
            let outcome = config_set(&pairs, &ConfigSetMode::Overwrite).unwrap();
            assert_eq!(outcome.result, ConfigSetResult::Refused);
            assert_eq!(
                key_statuses(&outcome),
                ["memory.backend=unchanged", "model=not_settable", "nope=not_settable"]
            );
            // An expectation is a read: still only allowlisted keys.
            let mode = expect(&[("provider.anthropic", "")]);
            let outcome = config_set(&installer_pairs(), &mode).unwrap();
            assert_eq!(outcome.result, ConfigSetResult::Refused);
            assert!(key_statuses(&outcome).contains(&"provider.anthropic=not_settable".into()));
        });
        // Refused before the read: no config or temp file was created.
        assert!(!base.join("config").exists());
        assert!(!base.join("config.tmp").exists());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    #[serial]
    fn config_set_rejects_malformed_input() {
        let home = make_test_home("set-malformed");
        let cfg = home.join(".synaps-cli/config");
        let original = "memory.backend = legacy\n";
        std::fs::write(&cfg, original).unwrap();
        with_home(&home, || {
            let bad_pairs: &[&[(&str, &str)]] = &[
                &[],
                &[("memory.backend", "axel\n")],
                &[("memory.backend", "ax\rel")],
                &[("memory.axel.executable", "/opt/a\tb")],
                &[("memory.backend", "axel\u{1b}[0m")],
                &[("memory.backend", "")],
                &[("memory.backend", "   ")],
                &[("Memory.backend", "axel")],
                &[("memory backend", "axel")],
                &[("", "axel")],
            ];
            for pairs in bad_pairs {
                let result = config_set(pairs, &ConfigSetMode::Overwrite);
                assert!(matches!(result, Err(ConfigSetError::Invalid(_))), "{pairs:?}");
            }
            let bad_expects =
                [expect(&[("memory.backend", "legacy\n")]), expect(&[("MEMORY", "")])];
            for mode in &bad_expects {
                let result = config_set(&installer_pairs(), mode);
                assert!(matches!(result, Err(ConfigSetError::Invalid(_))), "{mode:?}");
            }
        });
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), original);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    #[serial]
    fn config_set_rejects_duplicate_keys() {
        let home = make_test_home("set-duplicate");
        let cfg = home.join(".synaps-cli/config");
        with_home(&home, || {
            let same = [("memory.backend", "axel"), ("memory.backend", "axel")];
            let differ = [("memory.backend", "axel"), ("memory.backend", "legacy")];
            for pairs in [same, differ] {
                let result = config_set(&pairs, &ConfigSetMode::Overwrite);
                assert!(matches!(result, Err(ConfigSetError::Invalid(_))), "{pairs:?}");
            }
            let mode = expect(&[("memory.backend", ""), ("memory.backend", "legacy")]);
            let result = config_set(&[("memory.backend", "axel")], &mode);
            assert!(matches!(result, Err(ConfigSetError::Invalid(_))));
        });
        assert!(!cfg.exists());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    #[serial]
    fn config_set_rejects_values_the_parser_rejects() {
        let home = make_test_home("set-parser");
        let cfg = home.join(".synaps-cli/config");
        let original = "memory.backend = legacy\n";
        std::fs::write(&cfg, original).unwrap();
        with_home(&home, || {
            let message = |pairs: &[(&str, &str)], mode: &ConfigSetMode| {
                match config_set(pairs, mode) {
                    Err(ConfigSetError::Invalid(message)) => message,
                    other => panic!("{pairs:?}: expected Invalid, got {other:?}"),
                }
            };
            let backend = message(&[("memory.backend", "bogus")], &ConfigSetMode::Overwrite);
            assert!(backend.contains("expected legacy or axel"), "{backend}");
            let relative = [("memory.axel.executable", "relative/synaps-axel")];
            let path = message(&relative, &ConfigSetMode::Overwrite);
            assert!(path.contains("absolute path"), "{path}");
            // A bad value is an error even where the call would be refused.
            message(&[("memory.backend", "bogus")], &ConfigSetMode::IfUnset);
        });
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), original);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    #[serial]
    fn config_set_unreadable_config_is_an_io_error_and_left_alone() {
        let home = make_test_home("set-unreadable");
        let cfg = home.join(".synaps-cli/config");
        let original: &[u8] = b"model = \xff\xfe\n";
        std::fs::write(&cfg, original).unwrap();
        with_home(&home, || {
            let result = config_set(&installer_pairs(), &ConfigSetMode::Overwrite);
            assert!(matches!(result, Err(ConfigSetError::Io(_))), "{result:?}");
        });
        assert_eq!(std::fs::read(&cfg).unwrap(), original);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    #[serial]
    fn config_set_preserves_comments_and_other_lines() {
        let home = make_test_home("set-preserve");
        let cfg = home.join(".synaps-cli/config");
        // A commented-out key is not "set".
        let original = "# Synaps config\n\nmodel = claude-opus-4-6\n# memory.backend = legacy\n\
                        myplugin.colour = teal\n  # indented comment\n";
        std::fs::write(&cfg, original).unwrap();
        with_home(&home, || {
            let outcome = config_set(&installer_pairs(), &ConfigSetMode::IfUnset).unwrap();
            assert_eq!(outcome.result, ConfigSetResult::Written);
        });
        assert_eq!(
            std::fs::read_to_string(&cfg).unwrap(),
            format!(
                "{original}memory.backend = axel\n\
                 memory.axel.executable = {ABSOLUTE_SAMPLE_PATH}\n"
            )
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[cfg(unix)]
    #[test]
    #[serial]
    fn config_set_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let home = make_test_home("set-mode");
        let cfg = home.join(".synaps-cli/config");
        std::fs::write(&cfg, "model = claude-opus-4-6\n").unwrap();
        std::fs::set_permissions(&cfg, std::fs::Permissions::from_mode(0o644)).unwrap();
        with_home(&home, || {
            let outcome = config_set(&installer_pairs(), &ConfigSetMode::Overwrite).unwrap();
            assert_eq!(outcome.result, ConfigSetResult::Written);
        });
        let mode = std::fs::metadata(&cfg).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    #[serial]
    fn config_set_outcome_never_carries_current_values() {
        let home = make_test_home("set-no-echo");
        let cfg = home.join(".synaps-cli/config");
        let original =
            "provider.anthropic = sk-ant-SECRET-other\nmemory.backend = SECRET-hand-set\n";
        std::fs::write(&cfg, original).unwrap();
        let mut printed = Vec::new();
        with_home(&home, || {
            let runs = [
                (ConfigSetMode::IfUnset, ConfigSetResult::Refused),
                (expect(&[("memory.backend", "legacy")]), ConfigSetResult::Refused),
                (ConfigSetMode::Overwrite, ConfigSetResult::Written),
            ];
            for (mode, result) in runs {
                let outcome = config_set(&installer_pairs(), &mode).unwrap();
                assert_eq!(outcome.result, result, "{mode:?}");
                printed.push(format!("{outcome:?}"));
            }
            // A rejected value names itself, never the value it would replace.
            std::fs::write(&cfg, original).unwrap();
            let err = config_set(&[("memory.backend", "bogus")], &ConfigSetMode::IfUnset);
            let err = err.unwrap_err().to_string();
            assert!(err.contains("bogus"), "{err}");
            printed.push(err);
        });
        for text in &printed {
            assert!(!text.contains("SECRET"), "{text}");
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    #[serial]
    fn config_set_expect_path_met_writes() {
        let home = make_test_home("set-expect-path-met");
        let cfg = home.join(".synaps-cli/config");
        // The canonical form, as the outcome reports it (on macOS /tmp is
        // /private/tmp).
        let canonical = std::fs::canonicalize(home.join(".synaps-cli")).unwrap().join("config");
        with_home(&home, || {
            let pairs = owned_pairs(&installer_pairs());
            let mode = ConfigSetMode::IfUnset;
            let outcome =
                write_config_values_checked(&pairs, &mode, Some(canonical.as_path())).unwrap();
            assert_eq!(outcome.result, ConfigSetResult::Written);
            assert_eq!(outcome.path, canonical);
            assert_eq!(
                key_statuses(&outcome),
                ["memory.backend=written", "memory.axel.executable=written"]
            );
        });
        assert!(cfg.is_file());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    #[serial]
    fn config_set_expect_path_mismatch_refuses() {
        let home = make_test_home("set-expect-path-mismatch");
        let base = home.join(".synaps-cli");
        let cfg = base.join("config");
        let original = "memory.backend = legacy\n";
        std::fs::write(&cfg, original).unwrap();
        let canonical = std::fs::canonicalize(&cfg).unwrap();
        // Another file, and this file spelled non-canonically: the expected
        // path is compared exactly as given, never resolved.
        let others = [canonical.with_file_name("other-config"), base.join(".").join("config")];
        with_home(&home, || {
            let pairs = owned_pairs(&installer_pairs());
            for other in &others {
                let mode = ConfigSetMode::Overwrite;
                let outcome =
                    write_config_values_checked(&pairs, &mode, Some(other.as_path())).unwrap();
                assert_eq!(outcome.result, ConfigSetResult::Refused, "{other:?}");
                assert_eq!(outcome.path, canonical);
                assert_eq!(
                    key_statuses(&outcome),
                    ["memory.backend=path_mismatch", "memory.axel.executable=path_mismatch"]
                );
            }
        });
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), original);
        assert!(!base.join("config.tmp").exists());
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The reported path is the real file `load_config` reads, even when the
    /// base dir is reached through a symlink.
    #[cfg(unix)]
    #[test]
    #[serial]
    fn config_set_path_is_the_canonical_read_path() {
        let home = make_test_home("set-canonical");
        let real = home.join("real-base");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::remove_dir_all(home.join(".synaps-cli")).unwrap();
        std::os::unix::fs::symlink(&real, home.join(".synaps-cli")).unwrap();
        let expected = std::fs::canonicalize(&real).unwrap().join("config");
        with_home(&home, || {
            // Before the file exists (a refusal) and after it is written.
            let refused = config_set(&[("model", "x")], &ConfigSetMode::Overwrite).unwrap();
            assert_eq!(refused.path, expected);
            let written = config_set(&installer_pairs(), &ConfigSetMode::IfUnset).unwrap();
            assert_eq!(written.result, ConfigSetResult::Written);
            assert_eq!(written.path, expected);
            let read = std::fs::canonicalize(resolve_read_path("config")).unwrap();
            assert_eq!(written.path, read);
        });
        assert!(real.join("config").is_file());
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A config file that is a symlink is written through: the link stays a
    /// link, its target changes, and the reported path is the target.
    #[cfg(unix)]
    #[test]
    #[serial]
    fn config_set_writes_through_a_symlinked_config_file() {
        let home = make_test_home("set-symlink-file");
        let target = home.join("dotfiles-config");
        std::fs::write(&target, "model = keep-me\n").unwrap();
        let link = home.join(".synaps-cli").join("config");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let expected = std::fs::canonicalize(&target).unwrap();
        with_home(&home, || {
            let written = config_set(&installer_pairs(), &ConfigSetMode::IfUnset).unwrap();
            assert_eq!(written.result, ConfigSetResult::Written);
            assert_eq!(written.path, expected);
        });
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        let content = std::fs::read_to_string(&target).unwrap();
        assert!(content.contains("model = keep-me"), "{content}");
        assert!(content.contains("memory.backend = axel"), "{content}");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Every allowlisted key is one the parser knows, so its value is checked.
    #[test]
    fn settable_config_keys_are_known_parser_keys() {
        for key in SETTABLE_CONFIG_KEYS.iter().chain(IF_UNSET_ONLY_CONFIG_KEYS) {
            assert!(
                KNOWN_CONFIG_KEYS.contains(key) || FAMILY_CONFIG_KEYS.contains(key),
                "{key}"
            );
        }
    }

    /// `skills.extra_root`: one absolute path, kept whole with its inner
    /// spaces; a relative path, `~`, quotes or a control character warn
    /// (without echoing the value) and the line is ignored.
    #[test]
    #[cfg(unix)]
    fn skills_extra_root_takes_one_absolute_path() {
        let mac = "/Users/me/Library/Application Support/pria.desktop.dev/skills/current";
        let config = load_config_from_str(&format!("skills.extra_root = {mac}\n"));
        assert_eq!(
            config.skills_extra_root.as_deref(),
            Some(std::path::Path::new(mac))
        );
        assert!(config.warnings.is_empty(), "{:?}", config.warnings);
        assert_eq!(config.skills_keys().extra_root, config.skills_extra_root);
        for bad in [
            "relative/skills",
            "~/skills",
            "\"/quoted path\"",
            "/a\u{7}b",
        ] {
            let config = load_config_from_str(&format!(
                "skills.extra_root = /kept\nskills.extra_root = {bad}\n"
            ));
            assert_eq!(
                config.skills_extra_root.as_deref(),
                Some(std::path::Path::new("/kept")),
                "{bad:?}"
            );
            assert_eq!(config.warnings.len(), 1, "{bad:?}: {:?}", config.warnings);
            assert!(
                config.warnings[0].starts_with("skills.extra_root — "),
                "{:?}",
                config.warnings
            );
            assert!(!config.warnings[0].contains(bad), "{:?}", config.warnings);
        }
        let unset = load_config_from_str("skills.extra_root = /kept\nskills.extra_root =\n");
        assert_eq!(unset.skills_extra_root, None);
        assert!(unset.warnings.is_empty(), "{:?}", unset.warnings);
        assert!(KNOWN_CONFIG_KEYS.contains(&"skills.extra_root"));
    }

    /// `read_skills_keys` reads the file as it is now: the three skill keys.
    #[test]
    #[serial]
    #[cfg(unix)]
    fn read_skills_keys_reads_the_file_now() {
        let home = make_test_home("skills-keys");
        with_home(&home, || {
            assert_eq!(read_skills_keys(), SkillsKeys::default());
            std::fs::write(
                home.join(".synaps-cli/config"),
                "skills.extra_root = /x y/z\ndisabled_plugins = a, b\ndisabled_skills = p:s\nmodel = m\n",
            )
            .unwrap();
            let keys = read_skills_keys();
            assert_eq!(
                keys.extra_root.as_deref(),
                Some(std::path::Path::new("/x y/z"))
            );
            assert_eq!(keys.disabled_plugins, ["a", "b"]);
            assert_eq!(keys.disabled_skills, ["p:s"]);
        });
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The `--if-unset`-only keys: a plain set or a compare-and-set of each
    /// is refused; `auth.account.<provider>` takes only `auto` for a known
    /// provider; a refusal or a bad value writes nothing; and `--if-unset`
    /// never replaces a value the user chose.
    #[test]
    #[serial]
    #[cfg(unix)]
    fn config_set_if_unset_only_keys_refuse_every_other_write() {
        let home = make_test_home("set-if-unset-only");
        let cfg = home.join(".synaps-cli/config");
        let extra = "/Users/me/Library/Application Support/pria.desktop.dev/skills/current";
        let three = [
            ("context_management.mode", "auto"),
            ("skills.extra_root", extra),
            ("auth.account.anthropic", "auto"),
        ];
        let if_unset = ConfigSetMode::IfUnset;
        let not_settable = |pairs: &[(&str, &str)], mode: &ConfigSetMode| {
            let outcome = config_set(pairs, mode).unwrap();
            assert_eq!(
                outcome.result,
                ConfigSetResult::Refused,
                "{pairs:?} {mode:?}"
            );
            let want = format!("{}=not_settable", pairs[0].0);
            assert!(key_statuses(&outcome).contains(&want), "{pairs:?} {mode:?}");
        };
        with_home(&home, || {
            // A plain set, or a compare-and-set, of each of the three.
            for pair in three {
                not_settable(&[pair], &ConfigSetMode::Overwrite);
                not_settable(&[pair], &expect(&[(pair.0, "")]));
            }
            // Under auth.account.*, only `auto` for a known provider: never a
            // specific account, a clear, or an unknown provider.
            for pair in [
                ("auth.account.anthropic", "work"),
                ("auth.account.anthropic", "default"),
                ("auth.account.anthropic", "AUTO"),
                ("auth.account.anthropic", ""),
                ("auth.account.openai", "auto"),
                ("auth.account.", "auto"),
            ] {
                not_settable(&[pair], &if_unset);
            }
            not_settable(
                &[
                    ("auth.account.anthropic", "work"),
                    ("memory.backend", "axel"),
                ],
                &if_unset,
            );
            assert!(!cfg.exists(), "a refusal writes nothing");
            // Bad values are the parser's (usage) errors.
            for pair in [
                ("context_management.mode", "on"),
                ("skills.extra_root", "rel/skills"),
            ] {
                let result = config_set(&[pair], &if_unset);
                assert!(
                    matches!(result, Err(ConfigSetError::Invalid(_))),
                    "{pair:?}"
                );
            }
            assert!(!cfg.exists());

            let outcome = config_set(&three, &if_unset).unwrap();
            assert_eq!(outcome.result, ConfigSetResult::Written);
            let written = std::fs::read_to_string(&cfg).unwrap();
            assert_eq!(
                written,
                format!(
                    "context_management.mode = auto\nskills.extra_root = {extra}\n\
                     auth.account.anthropic = auto\n"
                )
            );
            assert!(config_content_problems(&written).is_empty());
            let outcome = config_set(&three, &if_unset).unwrap();
            assert_eq!(outcome.result, ConfigSetResult::Unchanged);

            // The user's own choices stay: --if-unset is a conflict.
            let user = "context_management.mode = off\nauth.account.anthropic = work\n";
            std::fs::write(&cfg, user).unwrap();
            for pair in [three[0], three[2]] {
                let outcome = config_set(&[pair], &if_unset).unwrap();
                assert_eq!(key_statuses(&outcome), [format!("{}=conflict", pair.0)]);
            }
            assert_eq!(std::fs::read_to_string(&cfg).unwrap(), user);
        });
        let _ = std::fs::remove_dir_all(&home);
    }

    /// `--if-unset` names a key whose one line is `key =` `empty`, apart
    /// from a conflict: still refused, nothing written.
    #[test]
    #[serial]
    #[cfg(unix)]
    fn config_set_if_unset_names_an_explicit_empty_line_empty() {
        let home = make_test_home("set-empty");
        let cfg = home.join(".synaps-cli/config");
        let extra = "/Users/me/Library/Application Support/pria.desktop.dev/skills/current";
        with_home(&home, || {
            for (key, value) in [
                ("skills.extra_root", extra),
                ("auth.account.anthropic", "auto"),
            ] {
                for line in [format!("{key} ="), format!("  {key}=   ")] {
                    let original = format!("# mine\n{line}\nmodel = claude-opus-4-6\n");
                    std::fs::write(&cfg, &original).unwrap();
                    let pairs = [(key, value), ("memory.backend", "axel")];
                    let outcome = config_set(&pairs, &ConfigSetMode::IfUnset).unwrap();
                    assert_eq!(outcome.result, ConfigSetResult::Refused, "{line:?}");
                    let want = [format!("{key}=empty"), "memory.backend=unchanged".into()];
                    assert_eq!(key_statuses(&outcome), want, "{line:?}");
                    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), original);
                }
            }
            // A real value is still a conflict, and an empty line under
            // --expect is still set (expect_failed), not empty.
            std::fs::write(&cfg, "skills.extra_root = /other\nmemory.backend =\n").unwrap();
            let outcome = config_set(&[("skills.extra_root", extra)], &ConfigSetMode::IfUnset);
            assert_eq!(
                key_statuses(&outcome.unwrap()),
                ["skills.extra_root=conflict"]
            );
            let mode = expect(&[("memory.backend", "")]);
            let outcome = config_set(&[("memory.backend", "axel")], &mode).unwrap();
            assert_eq!(key_statuses(&outcome), ["memory.backend=expect_failed"]);
        });
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A key with two lines (the parser reads the last, a write would replace
    /// the first) is `duplicate` in every mode, in either order, and the call
    /// writes nothing.
    #[test]
    #[serial]
    #[cfg(unix)]
    fn config_set_refuses_a_key_with_two_lines_in_every_mode() {
        let home = make_test_home("set-duplicate-lines");
        let cfg = home.join(".synaps-cli/config");
        let extra = "/Users/me/Library/Application Support/pria.desktop.dev/skills/current";
        let both_orders = |key: &str, value: &str| {
            [
                format!("{key} = {value}\n# between\n{key} =\n"),
                format!("{key} =\n# between\n {key}={value}\n"),
            ]
        };
        let calls: Vec<(&str, &str, ConfigSetMode)> = vec![
            ("memory.backend", "axel", ConfigSetMode::Overwrite),
            ("memory.backend", "axel", ConfigSetMode::IfUnset),
            (
                "memory.backend",
                "axel",
                expect(&[("memory.backend", "axel")]),
            ),
            (
                "memory.backend",
                "legacy",
                expect(&[("memory.backend", "")]),
            ),
            ("skills.extra_root", extra, ConfigSetMode::IfUnset),
            ("auth.account.anthropic", "auto", ConfigSetMode::IfUnset),
        ];
        with_home(&home, || {
            for (key, value, mode) in &calls {
                for original in both_orders(key, value) {
                    std::fs::write(&cfg, &original).unwrap();
                    let pairs = [
                        (*key, *value),
                        ("memory.axel.executable", ABSOLUTE_SAMPLE_PATH),
                    ];
                    let outcome = config_set(&pairs, mode).unwrap();
                    assert_eq!(
                        outcome.result,
                        ConfigSetResult::Refused,
                        "{original:?} {mode:?}"
                    );
                    let want = [
                        format!("{key}=duplicate"),
                        "memory.axel.executable=unchanged".into(),
                    ];
                    assert_eq!(key_statuses(&outcome), want, "{original:?} {mode:?}");
                    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), original);
                }
            }
            // An expect-only key with two lines is refused the same way.
            let original = "memory.axel.executable = /a\nmemory.axel.executable = /b\n";
            std::fs::write(&cfg, original).unwrap();
            let mode = expect(&[("memory.axel.executable", "/b")]);
            let outcome = config_set(&[("memory.backend", "axel")], &mode).unwrap();
            assert_eq!(
                key_statuses(&outcome),
                [
                    "memory.backend=unchanged",
                    "memory.axel.executable=duplicate"
                ]
            );
            assert_eq!(std::fs::read_to_string(&cfg).unwrap(), original);
        });
        let _ = std::fs::remove_dir_all(&home);
    }

    /// `auth use`'s write: a key with two lines is refused with nothing
    /// written, in either order; other keys' duplicates do not matter.
    #[test]
    #[serial]
    fn write_config_value_no_duplicates_refuses_a_key_with_two_lines() {
        let home = make_test_home("write-no-duplicates");
        let cfg = home.join(".synaps-cli/config");
        let key = "auth.account.anthropic";
        with_home(&home, || {
            for original in [
                format!("{key} = work\n{key} =\n"),
                format!("{key} =\n# x\n{key} = work\n"),
            ] {
                std::fs::write(&cfg, &original).unwrap();
                let err = write_config_value_no_duplicates(key, "auto").unwrap_err();
                assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
                let message = err.to_string();
                assert!(message.starts_with(&format!("{key} has more than one line in ")));
                assert!(!message.contains("work"), "{message}");
                assert_eq!(std::fs::read_to_string(&cfg).unwrap(), original);
            }
            let original = "model = a\nmodel = b\nauth.account.anthropic = work\n";
            std::fs::write(&cfg, original).unwrap();
            write_config_value_no_duplicates(key, "auto").unwrap();
            let written = std::fs::read_to_string(&cfg).unwrap();
            assert_eq!(
                written,
                "model = a\nmodel = b\nauth.account.anthropic = auto\n"
            );
        });
        let _ = std::fs::remove_dir_all(&home);
    }

    /// `config set`'s base dir: a missing one is made 0700 (and its missing
    /// parent too, as `create_dir_all` makes it); an existing one keeps its
    /// mode, whatever it is.
    #[test]
    #[cfg(unix)]
    fn create_config_dir_makes_a_missing_dir_0700_and_leaves_an_existing_one() {
        use std::os::unix::fs::PermissionsExt;
        let mode =
            |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        let root = tempfile::tempdir().unwrap();
        let base = root.path().join("parent").join(".synaps-cli");
        create_config_dir(&base).unwrap();
        assert_eq!(mode(&base), 0o700);
        assert!(root.path().join("parent").is_dir());
        for existing in [0o755, 0o750, 0o711] {
            let permissions = std::fs::Permissions::from_mode(existing);
            std::fs::set_permissions(&base, permissions).unwrap();
            create_config_dir(&base).unwrap();
            assert_eq!(mode(&base), existing);
        }
    }

    /// A fresh install's base dir, made by `config set` itself, is 0700
    /// with the config lock on or off, and the reply's path is the file
    /// written.
    #[test]
    #[serial]
    #[cfg(unix)]
    fn config_set_makes_a_fresh_base_dir_0700_with_the_lock_on_or_off() {
        use std::os::unix::fs::PermissionsExt;
        let home = make_test_home("set-fresh-base-dir");
        let base = home.join(".synaps-cli");
        let pairs = [("context_management.mode".to_string(), "auto".to_string())];
        with_home(&home, || {
            for lock_off in [false, true] {
                std::fs::remove_dir_all(&base).unwrap();
                if lock_off {
                    std::env::set_var("SYNAPS_CONFIG_LOCK", "0");
                }
                let outcome = write_config_values_checked(&pairs, &ConfigSetMode::IfUnset, None);
                std::env::remove_var("SYNAPS_CONFIG_LOCK");
                let outcome = outcome.unwrap();
                assert_eq!(outcome.result, ConfigSetResult::Written, "{lock_off}");
                let config = std::fs::canonicalize(base.join("config")).unwrap();
                assert_eq!(outcome.path, config);
                let mode = std::fs::metadata(&base).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o700, "lock off: {lock_off}");
            }
        });
        let _ = std::fs::remove_dir_all(&home);
    }
}
