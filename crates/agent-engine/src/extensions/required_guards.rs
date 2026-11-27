//! Required guards. The config's `required_guards` names plugin ids
//! the engine must have loaded as guards. While any one of them is not
//! loaded, for ANY reason, every `before_tool_call` the hook bus emits is
//! refused before (and re-checked after) the hook chain, with the guard and
//! its cause named. There is no allow fallback: a guard that is not known
//! to be loaded and live is not loaded.
//!
//! One gate per process ([`RequiredGuards::process`]): every hook bus made
//! with [`HookBus::new`](crate::extensions::hooks::HookBus::new) checks it,
//! so the host's bus, a worker's bus and a process with no
//! extension host at all (`synaps agent`) refuse alike. A bus that does not
//! itself hold every required guard's `before_tool_call` handler (a
//! worker's view, a detached empty bus) refuses too: the guard would never
//! see the call. A worker reaches the guards through its route
//! ([`HookBus::worker_route`](crate::extensions::hooks::HookBus::worker_route)). The host's
//! extension manager marks each required guard as discovery and the owner's
//! loads, reloads and unloads reach it; a loaded guard's process is checked
//! live at every call ([`ExtensionHandler::guard_down`]).
//!
//! Restoring the guard (its folder, its manifest, its `disabled_plugins`
//! entry) and reloading it (`synaps daemon reload-extension <id>` or a
//! daemon reload) clears the refusal.
//!
//! A required guard that is loaded and live but gives no usable
//! answer to a `before_tool_call` (no answer within the 5 s hook timeout,
//! a JSON-RPC error, a failed call, an answer that is not a hook result it
//! may give) blocks that call, naming the guard and the cause
//! ([`NoAnswer`], [`no_answer_text`]), whatever its manifest's `on_failure`
//! says. Any other guard keeps its manifest's `on_failure`.

use std::sync::{Arc, RwLock};

use crate::extensions::runtime::ExtensionHandler;
use crate::session::wire::RequiredGuardHealth;

/// Why a required guard is not loaded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NotLoaded {
    /// This process loads no extensions (no extension host, e.g. `synaps agent`).
    NoExtensionHost,
    /// The engine was started with extensions off (`--no-extensions`).
    ExtensionsOff,
    /// Extension discovery has not reached it yet.
    Pending,
    /// No plugin folder of that name in any plugin root (the roots listed).
    Absent(String),
    /// Listed in `disabled_plugins`.
    Disabled,
    /// Only a folder with a name discovery skips (`example-guard.disabled`,
    /// `example-guard.bak`, `example-guard.backup.<ts>`, ...) is there.
    SkipRenamed(String),
    /// A plugin root other than the base dir's `plugins/` (a project's
    /// `.synaps/plugins/`) holds a folder of its name (the path given),
    /// which discovery would let override the base one. A required guard
    /// loads only from the base dir's `plugins/`, so neither loads.
    Shadowed(String),
    /// Its `plugin.json` is missing, unreadable, not JSON, or has no valid
    /// `extension` block.
    BadManifest(String),
    /// Its load or start failed (post-install setup failed included).
    FailedStart(String),
    /// It loaded, but its manifest subscribes no `before_tool_call` hook, so
    /// it guards no tool call.
    NoGuardHook,
    /// It was unloaded (or every extension was stopped).
    Unloaded,
    /// Its process is gone (exited, killed, stopped).
    ProcessGone(String),
    /// The post-chain check: it is loaded and live, but not the
    /// instance the pre-chain check saw (it was reloaded while the call was
    /// in the chain), so that instance's silence is no allow.
    ReloadedDuringCall,
    /// The configured name can never be a plugin folder name.
    InvalidName,
}

impl NotLoaded {
    /// The short cause code the refusal and the health row show.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NoExtensionHost => "no-extension-host",
            Self::ExtensionsOff => "extensions-off",
            Self::Pending => "pending",
            Self::Absent(_) => "absent",
            Self::Disabled => "disabled",
            Self::SkipRenamed(_) => "skip-renamed",
            Self::Shadowed(_) => "shadowed",
            Self::BadManifest(_) => "bad-manifest",
            Self::FailedStart(_) => "failed-start",
            Self::NoGuardHook => "no-guard-hook",
            Self::Unloaded => "unloaded",
            Self::ProcessGone(_) => "process-gone",
            Self::ReloadedDuringCall => "reloaded",
            Self::InvalidName => "invalid-name",
        }
    }

    /// The cause in words, for guard `name`.
    pub fn detail(&self, name: &str) -> String {
        match self {
            Self::NoExtensionHost => "this process loads no extensions".into(),
            Self::ExtensionsOff => "the engine was started with extensions off".into(),
            Self::Pending => "extension discovery has not loaded it yet".into(),
            Self::Absent(roots) => format!("no plugin folder named '{name}' in {roots}"),
            Self::Disabled => format!("'{name}' is listed in disabled_plugins"),
            Self::SkipRenamed(folder) => {
                format!("its folder is named '{folder}', a name discovery skips")
            }
            Self::Shadowed(dir) => format!(
                "'{dir}' would override it, and a required guard loads only from the base dir's plugins/"
            ),
            Self::BadManifest(why) | Self::FailedStart(why) | Self::ProcessGone(why) => {
                why.clone()
            }
            Self::NoGuardHook => "its manifest subscribes no before_tool_call hook".into(),
            Self::Unloaded => "it was unloaded".into(),
            Self::ReloadedDuringCall => "it was reloaded during the call".into(),
            Self::InvalidName => format!("'{name}' is not a plugin folder name"),
        }
    }

    /// `code: detail`.
    pub fn describe(&self, name: &str) -> String {
        format!("{}: {}", self.code(), self.detail(name))
    }
}

/// The text every refused tool call gets while required guard `name` is
/// not loaded for `cause`.
pub fn refusal_text(name: &str, cause: &NotLoaded) -> String {
    format!(
        "Refused: required guard '{name}' is not loaded ({}). No tool call runs while a required guard is not loaded; restore the guard, then reload it.",
        cause.describe(name)
    )
}

/// Why a required guard that is loaded and live gave no usable
/// answer to a `before_tool_call`. The call is then blocked, whatever the
/// guard's manifest says (`on_failure`): a required guard never fails open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NoAnswer {
    /// No answer within the 5 s hook timeout: hung, stopped (SIGSTOP),
    /// busy, or slow on this input.
    Timeout,
    /// It answered with a JSON-RPC error (its message).
    Error(String),
    /// Its call failed with no answer (a transport error, a failed respawn).
    CallFailed(String),
    /// Its answer is not a hook result it may give before a tool call (why).
    BadAnswer(String),
}

impl NoAnswer {
    /// The short cause code the refusal shows.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Error(_) | Self::CallFailed(_) => "error",
            Self::BadAnswer(_) => "bad-answer",
        }
    }

    /// The cause in words. A guard-supplied part is cut to
    /// [`NO_ANSWER_DETAIL_MAX`] chars, control chars as spaces.
    pub fn detail(&self) -> String {
        match self {
            Self::Timeout => {
                "it did not answer within 5 s (hung, stopped, busy, or slow on this input)".into()
            }
            Self::Error(message) => format!("it answered with an error: {}", bounded(message)),
            Self::CallFailed(error) => format!("its call failed: {}", bounded(error)),
            Self::BadAnswer(why) => {
                format!("its answer is not a usable hook result: {}", bounded(why))
            }
        }
    }
}

/// [`NoAnswer::detail`]'s cut for a guard-supplied part.
pub const NO_ANSWER_DETAIL_MAX: usize = 200;

fn bounded(text: &str) -> String {
    let mut chars = text.chars().map(|c| if c.is_control() { ' ' } else { c });
    let head: String = chars.by_ref().take(NO_ANSWER_DETAIL_MAX).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// The text a tool call gets when required guard `name` gave no
/// usable answer to it ([`NoAnswer`]).
pub fn no_answer_text(name: &str, why: &NoAnswer) -> String {
    format!(
        "Refused: required guard '{name}' gave no usable answer ({}: {}). No tool call runs without a required guard's answer, whatever its on_failure says.",
        why.code(),
        why.detail()
    )
}

/// Whether `name` can be a plugin folder name discovery agrees on: 1 to 128
/// ASCII letters, digits, `-`, `_` or `.`, not starting with `.`. Any other
/// configured name refuses every tool call ([`NotLoaded::InvalidName`]).
pub fn valid_guard_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// One required guard's state.
#[derive(Clone)]
enum Slot {
    Loaded(Arc<dyn ExtensionHandler>),
    NotLoaded(NotLoaded),
}

/// What the pre-chain check ([`RequiredGuards::check`]) passed: each
/// required guard's live instance, for the post-chain
/// [`RequiredGuards::recheck`].
pub struct GuardPass(Vec<(String, Arc<dyn ExtensionHandler>)>);

impl GuardPass {
    /// The required guards the chain must run, in config order.
    pub(crate) fn names(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(|(name, _)| name.as_str())
    }
}

/// The required guards of one process and the state of each, in config
/// order. Empty: no gate at all (no refusal, no health rows).
pub struct RequiredGuards {
    slots: RwLock<Vec<(String, Slot)>>,
}

impl std::fmt::Debug for RequiredGuards {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequiredGuards").field("names", &self.names()).finish()
    }
}

#[cfg(not(test))]
static PROCESS: std::sync::OnceLock<Arc<RequiredGuards>> = std::sync::OnceLock::new();

impl RequiredGuards {
    /// `names` required, each not loaded yet: this process has no extension
    /// host until one marks them ([`Self::host_booting`]).
    pub fn new(names: &[String]) -> Self {
        let gate = Self { slots: RwLock::new(Vec::new()) };
        gate.require(names, NotLoaded::NoExtensionHost);
        gate
    }

    /// No required guards: no gate.
    pub fn none() -> Self {
        Self::new(&[])
    }

    /// This process's gate, shared by every hook bus made with
    /// [`HookBus::new`](crate::extensions::hooks::HookBus::new): first read
    /// from the active config file (fresh, silent). The host adds its own
    /// config's list at boot ([`Self::host_booting`]); the list only grows.
    /// The engine's unit tests read no config here and share no gate: each
    /// call is a fresh empty gate, so one test (a booted host included) can
    /// never refuse another's tool calls.
    #[cfg(not(test))]
    pub fn process() -> Arc<Self> {
        Arc::clone(PROCESS.get_or_init(|| {
            let names = crate::config::read_required_guards();
            if !names.is_empty() {
                tracing::info!(required_guards = ?names, "required guards gate every tool call");
            }
            Arc::new(Self::new(&names))
        }))
    }

    /// Tests: a fresh empty gate per call (see the non-test doc).
    #[cfg(test)]
    pub fn process() -> Arc<Self> {
        Arc::new(Self::none())
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Vec<(String, Slot)>> {
        self.slots.read().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Vec<(String, Slot)>> {
        self.slots.write().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Add `names` not already required, each `initial` (or
    /// [`NotLoaded::InvalidName`]). Never removes one.
    pub fn require(&self, names: &[String], initial: NotLoaded) {
        let mut slots = self.write();
        for name in names {
            if slots.iter().any(|(n, _)| n == name) {
                continue;
            }
            let cause = if valid_guard_name(name) {
                initial.clone()
            } else {
                tracing::warn!(guard = %name, "required guard name is not a plugin folder name; every tool call is refused");
                NotLoaded::InvalidName
            };
            slots.push((name.clone(), Slot::NotLoaded(cause)));
        }
    }

    /// Host boot: require `names` too, and mark every guard not loaded yet
    /// as pending discovery, or as extensions off when the host loads none.
    pub fn host_booting(&self, names: &[String], no_extensions: bool) {
        let cause = if no_extensions { NotLoaded::ExtensionsOff } else { NotLoaded::Pending };
        self.require(names, cause.clone());
        for (_, slot) in self.write().iter_mut() {
            if matches!(slot, Slot::NotLoaded(c) if *c != NotLoaded::InvalidName) {
                *slot = Slot::NotLoaded(cause.clone());
            }
        }
    }

    /// No guard is required.
    pub fn is_empty(&self) -> bool {
        self.read().is_empty()
    }

    /// Some guard is required (`Pong.required_guards_configured`):
    /// any configured name, an invalid one included.
    pub fn configured(&self) -> bool {
        !self.is_empty()
    }

    /// The required guard names, in config order.
    pub fn names(&self) -> Vec<String> {
        self.read().iter().map(|(n, _)| n.clone()).collect()
    }

    /// `id` is a required guard.
    pub fn is_required(&self, id: &str) -> bool {
        self.read().iter().any(|(n, _)| n == id)
    }

    /// Required guard `id` loaded, live in `handler`. No-op for any other id
    /// (and for an invalid name, which stays refused).
    pub(crate) fn mark_loaded(&self, id: &str, handler: Arc<dyn ExtensionHandler>) {
        self.set(id, Slot::Loaded(handler));
    }

    /// Required guard `id` not loaded for `cause`. No-op for any other id.
    pub(crate) fn mark_not_loaded(&self, id: &str, cause: NotLoaded) {
        if self.is_required(id) {
            tracing::warn!(guard = %id, cause = %cause.describe(id), "required guard not loaded; every tool call is refused");
        }
        self.set(id, Slot::NotLoaded(cause));
    }

    fn set(&self, id: &str, value: Slot) {
        for (name, slot) in self.write().iter_mut() {
            if name == id && !matches!(slot, Slot::NotLoaded(NotLoaded::InvalidName)) {
                *slot = value.clone();
            }
        }
    }

    /// Every required guard's state now, a loaded one's process checked
    /// live: its live handler, or why it is not loaded.
    async fn live(&self) -> Vec<(String, Result<Arc<dyn ExtensionHandler>, NotLoaded>)> {
        let slots: Vec<(String, Slot)> = self.read().clone();
        let mut out = Vec::with_capacity(slots.len());
        for (name, slot) in slots {
            let state = match slot {
                Slot::NotLoaded(cause) => Err(cause),
                Slot::Loaded(handler) => match handler.guard_down().await {
                    Some(why) => Err(NotLoaded::ProcessGone(why)),
                    None => Ok(handler),
                },
            };
            out.push((name, state));
        }
        out
    }

    /// Every required guard's state now ([`Self::live`]): `None` when loaded.
    async fn states(&self) -> Vec<(String, Option<NotLoaded>)> {
        self.live()
            .await
            .into_iter()
            .map(|(name, state)| (name, state.err()))
            .collect()
    }

    /// The refusal for a tool call now: `None` only when every required
    /// guard is loaded and its process is live (or none is required).
    pub async fn refusal(&self) -> Option<String> {
        self.check().await.err()
    }

    /// The pre-chain check: the refusal now ([`Self::refusal`]), or the
    /// live instance of each required guard the chain runs with, for
    /// [`Self::recheck`].
    pub async fn check(&self) -> Result<GuardPass, String> {
        if self.is_empty() {
            return Ok(GuardPass(Vec::new()));
        }
        let mut seen = Vec::new();
        for (name, state) in self.live().await {
            match state {
                Ok(handler) => seen.push((name, handler)),
                Err(cause) => return Err(refusal_text(&name, &cause)),
            }
        }
        Ok(GuardPass(seen))
    }

    /// The post-chain check: the refusal now, also when a required
    /// guard is loaded and live but is not the instance `pass` saw before
    /// the chain (same instance by `Arc::ptr_eq`): it was reloaded during
    /// the call ([`NotLoaded::ReloadedDuringCall`]).
    pub async fn recheck(&self, pass: &GuardPass) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        self.live().await.into_iter().find_map(|(name, state)| {
            let cause = match state {
                Err(cause) => cause,
                Ok(now) => {
                    let same = pass
                        .0
                        .iter()
                        .any(|(seen, before)| *seen == name && Arc::ptr_eq(before, &now));
                    if same {
                        return None;
                    }
                    NotLoaded::ReloadedDuringCall
                }
            };
            Some(refusal_text(&name, &cause))
        })
    }

    /// One health row per required guard, in config order (none when no
    /// guard is required).
    pub async fn health(&self) -> Vec<RequiredGuardHealth> {
        if self.is_empty() {
            return Vec::new();
        }
        self.states()
            .await
            .into_iter()
            .map(|(name, cause)| RequiredGuardHealth {
                loaded: cause.is_none(),
                cause: cause.map(|c| c.describe(&name)),
                name,
            })
            .collect()
    }
}

/// `synaps daemon status`: whether the daemon requires any guard,
/// before its rows, so an empty list reads as none required, never as all
/// loaded: `required guards configured: no` or
/// `required guards configured: yes (2)`.
pub fn configured_line(configured: bool, rows: usize) -> String {
    if configured {
        format!("required guards configured: yes ({rows})")
    } else {
        "required guards configured: no".to_string()
    }
}

/// `synaps daemon status`: one line per required guard,
/// `required guard: example-guard loaded` or
/// `required guard: example-guard NOT LOADED (disabled: 'example-guard' is listed in disabled_plugins)`.
pub fn status_lines(rows: &[RequiredGuardHealth]) -> Vec<String> {
    rows.iter()
        .map(|row| match (&row.cause, row.loaded) {
            (None, true) => format!("required guard: {} loaded", row.name),
            (cause, _) => format!(
                "required guard: {} NOT LOADED ({})",
                row.name,
                cause.as_deref().unwrap_or("unknown")
            ),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[tokio::test]
    async fn no_required_guard_is_no_gate() {
        let gate = RequiredGuards::none();
        assert!(gate.is_empty());
        assert_eq!(gate.refusal().await, None);
        assert!(gate.health().await.is_empty());
        // The engine's own tests never read a config into the process gate.
        assert!(RequiredGuards::process().is_empty());
    }

    /// `configured` is any configured name (an invalid one too),
    /// and the status line says so either way.
    #[test]
    fn configured_is_any_name_and_its_line_says_so() {
        assert!(!RequiredGuards::none().configured());
        assert!(RequiredGuards::new(&names(&["example-guard"])).configured());
        assert!(RequiredGuards::new(&names(&["../x"])).configured());
        assert_eq!(configured_line(false, 0), "required guards configured: no");
        assert_eq!(configured_line(true, 0), "required guards configured: yes (0)");
        assert_eq!(configured_line(true, 1), "required guards configured: yes (1)");
    }

    #[tokio::test]
    async fn a_required_guard_never_marked_refuses_naming_the_cause() {
        let gate = RequiredGuards::new(&names(&["example-guard"]));
        assert_eq!(
            gate.refusal().await.as_deref(),
            Some(
                "Refused: required guard 'example-guard' is not loaded (no-extension-host: this process loads no extensions). No tool call runs while a required guard is not loaded; restore the guard, then reload it."
            )
        );
        gate.host_booting(&[], false);
        assert!(gate.refusal().await.unwrap().contains("(pending: extension discovery has not loaded it yet)"));
        gate.host_booting(&[], true);
        assert!(gate.refusal().await.unwrap().contains("(extensions-off: "));
    }

    #[tokio::test]
    async fn an_invalid_name_refuses_every_tool_call() {
        let gate = RequiredGuards::new(&names(&["../x", "example-guard"]));
        gate.mark_not_loaded("../x", NotLoaded::Unloaded);
        gate.host_booting(&[], false);
        assert!(gate.refusal().await.unwrap().contains("required guard '../x' is not loaded (invalid-name: "));
        for bad in ["", ".", "..", ".hidden", "a/b", "a\\b", "a:b", "a b", "jév"] {
            assert!(!valid_guard_name(bad), "{bad:?}");
        }
        for good in ["example-guard", "my-gate", "com.example.guard", "a_b9"] {
            assert!(valid_guard_name(good), "{good:?}");
        }
    }

    #[tokio::test]
    async fn the_list_only_grows_and_marks_touch_only_required_guards() {
        let gate = RequiredGuards::new(&names(&["example-guard"]));
        gate.require(&names(&["example-guard", "other"]), NotLoaded::Pending);
        assert_eq!(gate.names(), ["example-guard", "other"]);
        gate.mark_not_loaded("unrelated", NotLoaded::Disabled);
        assert!(!gate.is_required("unrelated"));
        let rows = gate.health().await;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].cause.as_deref(), Some("pending: extension discovery has not loaded it yet"));
    }

    #[test]
    fn status_lines_name_each_guard() {
        let rows = vec![
            RequiredGuardHealth { name: "example-guard".into(), loaded: true, cause: None },
            RequiredGuardHealth {
                name: "gate".into(),
                loaded: false,
                cause: Some("disabled: 'gate' is listed in disabled_plugins".into()),
            },
        ];
        assert_eq!(
            status_lines(&rows),
            [
                "required guard: example-guard loaded",
                "required guard: gate NOT LOADED (disabled: 'gate' is listed in disabled_plugins)",
            ]
        );
        assert!(status_lines(&[]).is_empty());
    }
}
