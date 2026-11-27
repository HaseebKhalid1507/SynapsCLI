//! HookBus — the central dispatcher for extension hooks.
//!
//! The HookBus holds registered handlers and dispatches typed events to them.
//! Without any handlers, `emit()` is a no-op fast path (<1µs).
//!
//! Tool-specific hooks filter by tool name before dispatching.
//!
//! Notify-only kinds ([`HookKind::is_notification`]) go out through
//! [`HookBus::notify`]: the caller does not wait, and no handler can delay,
//! fail or change what the caller is doing.

pub mod events;
#[cfg(test)]
mod worker_route_tests;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::future::join_all;
use futures::FutureExt;
use tokio::sync::RwLock;

use self::events::{HookEvent, HookFailurePolicy, HookKind, HookResult};
use crate::extensions::manifest::HookMatcher;
use crate::extensions::permissions::{Permission, PermissionSet};
use crate::extensions::required_guards::{no_answer_text, GuardPass, NoAnswer, RequiredGuards};

/// Default timeout for a single hook handler call.
const HANDLER_TIMEOUT: Duration = Duration::from_secs(5);

/// Most [`HookBus::notify`] deliveries in flight on one bus. A notify-only
/// event that arrives while this many are still being delivered is dropped,
/// never queued, so handlers that are slow while events keep coming cost at
/// most this many tasks, each ended by [`HANDLER_TIMEOUT`].
const NOTIFY_MAX_IN_FLIGHT: usize = 64;

fn extensions_trace_enabled() -> bool {
    std::env::var("SYNAPS_EXTENSIONS_TRACE")
        .map(|value| {
            let normalized = value.trim().to_ascii_lowercase();
            matches!(normalized.as_str(), "1" | "true" | "yes" | "on")
        })
        .unwrap_or(false)
}

/// Injections gathered from one emit, in handler order (#414). `content`
/// joins every extension's `content` the way it always did (what
/// `on_session_start` and older consumers use); `parts` keeps each
/// extension's parts with the host-known source stamped on, so the runtime
/// can save, show and mark them per extension.
#[derive(Default)]
struct Injections {
    contents: Vec<String>,
    parts: Vec<events::InjectPart>,
}

impl Injections {
    fn push(&mut self, source: &str, result: &HookResult) {
        if let HookResult::Inject { content, .. } = result {
            self.contents.push(content.clone());
        }
        // Any `source` on the wire is ignored (`#[serde(skip)]`); this is
        // the only place one is set.
        self.parts
            .extend(result.inject_parts().into_iter().map(|p| p.from_source(source)));
    }

    fn finish(self) -> Option<HookResult> {
        (!self.contents.is_empty()).then(|| HookResult::Inject {
            content: self.contents.join("\n\n"),
            forget: false,
            parts: self.parts,
        })
    }
}

fn hook_result_action(result: &HookResult) -> &'static str {
    match result {
        HookResult::Continue => "continue",
        HookResult::Block { .. } => "block",
        HookResult::Inject { .. } => "inject",
        HookResult::Confirm { .. } => "confirm",
        HookResult::Modify { .. } => "modify",
        HookResult::Replace { .. } => "replace",
        HookResult::ContextPhase { .. } => "context_phase",
    }
}

/// Two-key gate for an advisory `context_phase` report: subscribing to
/// `on_message_complete` needs `privacy.llm_content`; reporting a phase to
/// the host's context management additionally needs `session.lifecycle`.
/// Returns `Some(phase)` when the report is accepted, `None` (after a
/// warning) when it must be ignored and treated as `continue`.
fn accept_context_phase(
    reg: &HandlerRegistration,
    kind: HookKind,
    phase: String,
) -> Option<String> {
    if !reg.permissions.has(Permission::SessionLifecycle) {
        tracing::warn!(
            hook = %kind.as_str(),
            extension = %reg.handler.id(),
            "Extension returned context_phase without session.lifecycle permission — ignoring report"
        );
        return None;
    }
    tracing::info!(
        hook = %kind.as_str(),
        extension = %reg.handler.id(),
        phase = %crate::runtime::continuation::phase_for_log(&phase),
        "Extension reported advisory context phase"
    );
    Some(phase)
}

/// Whether a handler's `tool` filter covers `ev`. A filter matches the
/// tool's API name or runtime name (MCP tools with sanitized names still
/// match). Before a call, a filter for `edit` or `write` also covers the
/// builtin `multi_edit`, which changes files the same way: a guard written
/// before `multi_edit` existed must still block or confirm it. Output
/// transforms (`after_tool_call`) stay exact; their rewrite is per-format.
fn tool_filter_matches(filter: &str, ev: &HookEvent) -> bool {
    let (api, runtime) = (ev.tool_name.as_deref(), ev.tool_runtime_name.as_deref());
    if api == Some(filter) || runtime == Some(filter) {
        return true;
    }
    ev.kind == HookKind::BeforeToolCall
        && matches!(filter, "edit" | "write")
        && runtime.or(api) == Some("multi_edit")
}

/// A registered hook handler with its metadata.
#[derive(Clone)]
pub struct HandlerRegistration {
    /// The extension handler.
    pub handler: Arc<dyn crate::extensions::runtime::ExtensionHandler>,
    /// Optional tool name filter (None = all tools).
    pub tool_filter: Option<String>,
    /// Optional matcher for event payloads.
    pub matcher: Option<HookMatcher>,
    /// Permissions granted to this handler's extension.
    pub permissions: PermissionSet,
    /// What the subscription answers when the handler does not.
    pub on_failure: HookFailurePolicy,
}

impl std::fmt::Debug for HookBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HookBus").finish_non_exhaustive()
    }
}

/// The central hook dispatcher.
///
/// Thread-safe: uses `RwLock` so multiple concurrent emitters can read
/// the handler list, and registration takes a write lock only briefly.
pub struct HookBus {
    handlers: RwLock<HashMap<HookKind, Vec<HandlerRegistration>>>,
    /// Per-extension-id load generation, bumped by [`Self::unsubscribe_all`]
    /// (unload / reload / reinstall). Stamped on every `Confirm` so the
    /// "Allow all" latch keys on (id, generation): a different plugin later
    /// installed under the same id does not inherit an earlier consent.
    generations: std::sync::Mutex<HashMap<String, u64>>,
    /// Context injected by `on_session_start` handlers, held for the lifetime
    /// of the session.
    ///
    /// `before_message` injection is per-turn and re-applied on every request.
    /// Session-start injection is the once-per-session equivalent: an
    /// extension that wants to hand the model a session handoff should not
    /// have to fake it by tracking "have I injected yet?" across every
    /// message, and should not pay to re-send it each turn.
    ///
    /// Keyed by conversation session id: one process may host N sessions
    /// (Phase 2 daemon), and each must read only its own injection.
    session_injection: RwLock<HashMap<String, String>>,
    /// Advanced under `session_injection`'s write guard by every set and
    /// clear ([`Self::injection_generation`]): a lock-free change counter.
    injection_gen: std::sync::atomic::AtomicU64,
    /// `Some(host)`: this bus is a subagent worker's view of `host`
    /// ([`Self::worker_transforms`]) and has no handlers of its own.
    transforms_of: Option<Arc<HookBus>>,
    /// Set on a worker's (subagent's) bus only: where its `before_tool_call`
    /// events go. `None` everywhere else. Independent of `transforms_of`:
    /// a worker bus may carry both ([`Self::with_worker_transforms_of`]).
    worker_route: Option<WorkerRoute>,
    /// The required guards every `before_tool_call` on this bus is checked
    /// against, before and after its chain. The process's gate
    /// ([`RequiredGuards::process`]) for every bus made with [`Self::new`];
    /// a worker view or route shares its host's or caller's.
    required: Arc<RequiredGuards>,
    /// One permit per [`Self::notify`] delivery in flight
    /// ([`NOTIFY_MAX_IN_FLIGHT`]).
    notify_slots: Arc<tokio::sync::Semaphore>,
}

/// A worker's route to its caller's `before_tool_call` hooks (see
/// [`HookBus::worker_route`]).
struct WorkerRoute {
    /// The calling session's bus (the host's, in a daemon).
    caller: Arc<HookBus>,
    /// The caller's tokio runtime. A worker runs on its own current-thread
    /// runtime, which is dropped when the worker ends. An extension process
    /// started or restarted from inside it would tie its pipes and reader
    /// task to that runtime and lose them with the worker, so the caller's
    /// handlers always run here.
    host: tokio::runtime::Handle,
    /// The conversation that started the worker (`None` when its caller
    /// has none).
    session_id: Option<String>,
    /// The worker's id as its caller sees it (`sa_N`).
    subagent_id: String,
}

impl WorkerRoute {
    /// Run the caller's `before_tool_call` chain for one of the worker's
    /// calls, stamped with the caller's conversation. A chain that cannot
    /// run (the caller's runtime is gone, or a handler panicked) blocks the
    /// call: the guard is not skipped.
    async fn before_tool_call(&self, event: &HookEvent) -> HookResult {
        let mut event = event.clone();
        if event.session_id.is_none() {
            event = event.with_session(self.session_id.as_deref());
        }
        event.subagent_id = Some(self.subagent_id.clone());
        let caller = Arc::clone(&self.caller);
        match self
            .host
            .spawn(async move { caller.emit_to_own_handlers(&event).await })
            .await
        {
            Ok(result) => result,
            Err(error) => {
                tracing::warn!(
                    subagent_id = %self.subagent_id,
                    error = %error,
                    "before_tool_call hooks could not run for a subagent tool call; blocking it"
                );
                HookResult::block(format!(
                    "the before_tool_call hooks could not run for this subagent tool call ({error})"
                ))
            }
        }
    }
}

impl HookBus {
    /// Create an empty HookBus with no handlers.
    pub fn new() -> Self {
        Self {
            handlers: RwLock::new(HashMap::new()),
            session_injection: RwLock::new(HashMap::new()),
            injection_gen: std::sync::atomic::AtomicU64::new(0),
            generations: std::sync::Mutex::new(HashMap::new()),
            transforms_of: None,
            worker_route: None,
            required: RequiredGuards::process(),
            notify_slots: Arc::new(tokio::sync::Semaphore::new(NOTIFY_MAX_IN_FLIGHT)),
        }
    }

    /// This bus checked against `gate` instead of the process's (tests).
    #[cfg(test)]
    pub(crate) fn with_required_guards(mut self, gate: Arc<RequiredGuards>) -> Self {
        self.required = gate;
        self
    }

    /// The required guards this bus checks every `before_tool_call` against:
    /// the extension manager marks them, the daemon's `Pong` reports them.
    pub fn required_guards(&self) -> &Arc<RequiredGuards> {
        &self.required
    }

    /// The hook bus a subagent worker runs with: a read-only view of
    /// `host`'s `after_tool_call` subscriptions from extensions that hold
    /// `tools.transform_output`, so output transforms (compression,
    /// redaction) see worker tool results too. Nothing else fires on the
    /// view itself: no `before_tool_call` (no confirm prompt, block or
    /// modify), no message or session hooks, no observe-only
    /// `after_tool_call`. A worker's `before_tool_call` reaches its
    /// caller's chain only through a [`Self::worker_route`] layered on
    /// this view ([`Self::with_worker_transforms_of`]).
    ///
    /// Live, not a snapshot: every [`Self::emit`] reads `host`'s current
    /// handler list, so an extension loaded, reloaded or unloaded while a
    /// worker runs applies from that worker's next tool call (a snapshot
    /// would keep calling a reloaded extension's stopped instance).
    /// Dispatch is otherwise [`Self::emit`]'s: chain order, the permission
    /// matrix, the 5 s timeout, first `replace` wins. Handlers are called
    /// through [`ExtensionHandler::handle_no_spawn`]: a worker never starts
    /// or restarts an extension process, and one that is not running
    /// passes the output through.
    ///
    /// [`ExtensionHandler::handle_no_spawn`]: crate::extensions::runtime::ExtensionHandler::handle_no_spawn
    pub fn worker_transforms(host: &Arc<HookBus>) -> Arc<HookBus> {
        // A view of a view reads the same host.
        let host = host.transforms_of.clone().unwrap_or_else(|| Arc::clone(host));
        Arc::new(Self {
            required: Arc::clone(&host.required),
            transforms_of: Some(host),
            ..Self::new()
        })
    }

    /// Whether this is a worker's view ([`Self::worker_transforms`]).
    pub fn is_worker_view(&self) -> bool {
        self.transforms_of.is_some()
    }

    /// Snapshot of what a worker view dispatches for `kind`: `None` for any
    /// kind but `after_tool_call`, or when no handler holds
    /// `tools.transform_output`.
    async fn worker_transform_registrations(
        &self,
        kind: HookKind,
    ) -> Option<Vec<HandlerRegistration>> {
        if kind != HookKind::AfterToolCall {
            return None;
        }
        let handlers = self.handlers.read().await;
        let regs: Vec<HandlerRegistration> = handlers
            .get(&kind)?
            .iter()
            .filter(|reg| reg.permissions.has(Permission::ToolsTransformOutput))
            .cloned()
            .collect();
        (!regs.is_empty()).then_some(regs)
    }

    /// A worker's (subagent's) bus. It has no handlers of its own: each
    /// `before_tool_call` the worker emits runs `caller`'s chain instead, on
    /// `host` (the caller's runtime), with `session_id` set to the caller's
    /// conversation when the event has none (subject to
    /// `SYNAPS_HOOK_SESSION_ID=0`). The result, a `Confirm` included, is the
    /// caller's chain's; who answers a `Confirm` is the worker runtime's
    /// confirm route. Every other hook kind finds no handlers here, as on an
    /// empty bus, unless the host's output transforms are layered on
    /// ([`Self::with_worker_transforms_of`]).
    pub fn worker_route(
        caller: Arc<HookBus>,
        host: tokio::runtime::Handle,
        session_id: Option<String>,
        subagent_id: String,
    ) -> Self {
        // A worker's worker runs the chain its caller runs, not the (empty)
        // handler list of its caller's own route.
        let (caller, host, session_id) = match &caller.worker_route {
            Some(up) => (
                Arc::clone(&up.caller),
                up.host.clone(),
                up.session_id.clone().or(session_id),
            ),
            None => (caller, host, session_id),
        };
        Self {
            required: Arc::clone(&caller.required),
            worker_route: Some(WorkerRoute {
                caller,
                host,
                session_id,
                subagent_id,
            }),
            ..Self::new()
        }
    }

    /// Whether this is a worker's bus routed to a caller's hooks.
    pub fn is_worker_route(&self) -> bool {
        self.worker_route.is_some()
    }

    /// This worker route ([`Self::worker_route`]) plus the host output
    /// transforms `view` dispatches, so one worker bus runs both its
    /// caller's `before_tool_call` chain and the host's `after_tool_call`
    /// transforms (`subagent_transform_hooks`).
    ///
    /// Only ever narrows: `view` contributes its host only when it is a
    /// worker view ([`Self::worker_transforms`]). A plain bus (the
    /// detached `subagent_transform_hooks = false` bus, or a full host bus)
    /// contributes nothing, so this never hands a worker a full host bus.
    pub fn with_worker_transforms_of(mut self, view: &HookBus) -> Self {
        self.transforms_of = view.transforms_of.clone();
        self
    }

    /// Current load generation of `extension_id` (0 until first unloaded).
    pub fn extension_generation(&self, extension_id: &str) -> u64 {
        self.generations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(extension_id)
            .copied()
            .unwrap_or(0)
    }

    /// Record context injected by an `on_session_start` handler for
    /// `session_id`.
    ///
    /// Called once per session, by whoever emits the session-start event,
    /// after extensions have loaded. Later calls for the same id replace the
    /// stored value.
    pub async fn set_session_injection_for(&self, session_id: &str, content: String) {
        let mut map = self.session_injection.write().await;
        map.insert(session_id.to_string(), content);
        self.injection_gen
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    }

    /// The injection change counter, read without the lock (a `CacheExpiry`
    /// capture), compared again after the reads it guards.
    pub fn injection_generation(&self) -> u64 {
        self.injection_gen
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Context injected at session start for `session_id`, if any. Read on
    /// every turn while composing the system prompt.
    pub async fn session_injection_for(&self, session_id: &str) -> Option<String> {
        self.session_injection.read().await.get(session_id).cloned()
    }

    /// Drop the injection recorded for `session_id` (session teardown).
    pub async fn clear_session_injection(&self, session_id: &str) {
        let mut map = self.session_injection.write().await;
        map.remove(session_id);
        self.injection_gen
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    }

    /// Test seam: hold the injection's write lock, as a set or a clear
    /// holds it, until the guard drops.
    #[cfg(any(test, feature = "testing"))]
    pub async fn hold_session_injection(
        &self,
    ) -> tokio::sync::RwLockWriteGuard<'_, HashMap<String, String>> {
        self.session_injection.write().await
    }

    /// Compatibility shim: writes under the empty key.
    #[deprecated(note = "use set_session_injection_for")]
    pub async fn set_session_injection(&self, content: String) {
        self.set_session_injection_for("", content).await;
    }

    /// Compatibility shim: reads the empty key.
    #[deprecated(note = "use session_injection_for")]
    pub async fn session_injection(&self) -> Option<String> {
        self.session_injection_for("").await
    }

    /// Register a handler for a specific hook kind.
    ///
    /// Returns an error if the handler's permissions don't allow
    /// subscribing to this hook kind.
    pub async fn subscribe(
        &self,
        kind: HookKind,
        handler: Arc<dyn crate::extensions::runtime::ExtensionHandler>,
        tool_filter: Option<String>,
        matcher: Option<HookMatcher>,
        permissions: PermissionSet,
    ) -> Result<(), String> {
        self.subscribe_with_failure_policy(
            kind,
            handler,
            tool_filter,
            matcher,
            permissions,
            HookFailurePolicy::Continue,
        )
        .await
    }

    /// [`Self::subscribe`] with the manifest's `on_failure`. `Block` is
    /// refused for any hook but `before_tool_call`.
    pub async fn subscribe_with_failure_policy(
        &self,
        kind: HookKind,
        handler: Arc<dyn crate::extensions::runtime::ExtensionHandler>,
        tool_filter: Option<String>,
        matcher: Option<HookMatcher>,
        permissions: PermissionSet,
        on_failure: HookFailurePolicy,
    ) -> Result<(), String> {
        if on_failure == HookFailurePolicy::Block && kind != HookKind::BeforeToolCall {
            return Err(format!(
                "Extension '{}' hook '{}' cannot use on_failure \"block\": only before_tool_call can block a call its extension did not answer",
                handler.id(),
                kind.as_str(),
            ));
        }
        if self.is_worker_view() {
            return Err(format!(
                "Extension '{}' cannot subscribe to a worker's hook view; subscribe to the host bus",
                handler.id(),
            ));
        }
        // Permission check
        if !permissions.allows_hook(kind) {
            return Err(format!(
                "Extension '{}' lacks permission '{}' required for hook '{}'",
                handler.id(),
                kind.required_permission().as_str(),
                kind.as_str(),
            ));
        }

        let reg = HandlerRegistration {
            handler,
            tool_filter,
            matcher,
            permissions,
            on_failure,
        };

        let mut handlers = self.handlers.write().await;
        handlers.entry(kind).or_default().push(reg);
        Ok(())
    }

    /// Emit a hook event to all registered handlers.
    ///
    /// Returns the first `Block` result if any handler blocks, otherwise
    /// returns `Continue`. Handlers are called in registration order.
    ///
    /// For `before_tool_call` only `Block` ends the chain (#148 I1):
    /// - `Modify` rewrites the input and the chain continues; every later
    ///   handler sees (and vets) the modified input, and the final input is
    ///   returned as `Modify` unless something blocks or confirms.
    /// - `Confirm` is recorded and the chain continues, so a later `Block`
    ///   wins even when the confirming extension is already latched.
    ///   Confirms are merged (all sources, all messages) and returned last,
    ///   carrying any input modified BEFORE the first Confirm.
    /// - A `Modify` after a pending `Confirm` is ignored (logged at warn):
    ///   the user confirms the input as it stood when they were asked.
    ///
    /// If no handlers are registered for this hook, returns immediately
    /// (the no-extensions fast path).
    ///
    /// On a worker view ([`Self::worker_transforms`]) only the host's
    /// `after_tool_call` transform handlers run, each through
    /// `handle_no_spawn`; every other kind returns `Continue` at once. On
    /// a worker route ([`Self::worker_route`]) a `before_tool_call` runs the
    /// caller's chain instead; the other kinds dispatch as above (a route
    /// with a view layered on runs both).
    pub async fn emit(&self, event: &HookEvent) -> HookResult {
        if let Some(route) = &self.worker_route {
            if event.kind == HookKind::BeforeToolCall {
                return route.before_tool_call(event).await;
            }
        }
        self.emit_to_own_handlers(event).await
    }

    /// [`Self::emit`] on this bus's own handlers, never through a worker
    /// route. A worker route runs its caller's chain with it (not with
    /// `emit`, so the spawned future does not contain itself).
    ///
    /// A `before_tool_call` is refused while a required guard is not loaded
    /// ([`RequiredGuards::check`]), checked before the chain (so the call
    /// reaches no hook and never runs) and again after it unless the chain
    /// blocked ([`RequiredGuards::recheck`]: a guard that died, or was
    /// reloaded, while the call was in its chain cannot have its silence
    /// read as an allow). No allow fallback: the refusal is a `Block` on
    /// every bus, a worker's included.
    ///
    /// A `before_tool_call` chain runs the required guards last, in
    /// registration order among themselves, so a guard vets the input as
    /// every other extension's `modify` left it. A required guard's `modify`
    /// after another required guard has answered blocks the call: that
    /// guard would never see the input it ran.
    ///
    /// In the chain, a required guard's own handler is called with
    /// [`ExtensionHandler::handle_required`](crate::extensions::runtime::ExtensionHandler::handle_required)
    /// whatever its manifest's `on_failure` says, and no usable answer
    /// (the 5 s timeout, an error, a failed call, an answer it may not
    /// give before a call, or a `modify` the bus cannot apply after an
    /// earlier Confirm) blocks the call, naming the guard and the cause
    /// ([`no_answer_text`]). Every other handler keeps its `on_failure`.
    async fn emit_to_own_handlers(&self, event: &HookEvent) -> HookResult {
        if event.kind != HookKind::BeforeToolCall || self.required.is_empty() {
            return self.dispatch_own_handlers(event).await;
        }
        let pass = match self.required.check().await {
            Ok(pass) => pass,
            Err(refusal) => return HookResult::block(refusal),
        };
        // A loaded guard this bus does not hold would never see the call
        // (a worker's view, a detached empty bus): that is not an allow.
        if let Some(name) = self.guard_not_on_this_bus(&pass).await {
            return HookResult::block(format!(
                "required guard '{name}' is not subscribed to before_tool_call on this \
                 hook bus (no route to it), so it cannot vet this tool call"
            ));
        }
        let result = self.dispatch_own_handlers(event).await;
        if matches!(result, HookResult::Block { .. }) {
            return result;
        }
        match self.required.recheck(&pass).await {
            Some(refusal) => HookResult::block(refusal),
            None => result,
        }
    }

    /// The first required guard in `pass` with no `before_tool_call`
    /// registration on this bus.
    async fn guard_not_on_this_bus(&self, pass: &GuardPass) -> Option<String> {
        let mut names = pass.names().peekable();
        names.peek()?;
        let handlers = self.handlers.read().await;
        let regs = handlers.get(&HookKind::BeforeToolCall);
        names
            .find(|name| {
                !regs.is_some_and(|regs| regs.iter().any(|reg| reg.handler.id() == *name))
            })
            .map(str::to_string)
    }

    /// [`Self::emit_to_own_handlers`]'s dispatch, after the required-guard
    /// check.
    async fn dispatch_own_handlers(&self, event: &HookEvent) -> HookResult {
        // Snapshot the handler list and drop the lock immediately.
        // This prevents holding the RwLock across async handler calls
        // (which could block subscribe/unsubscribe for the entire
        // duration of IPC round-trips to extension processes).
        let no_spawn = self.is_worker_view();
        let registrations = match &self.transforms_of {
            Some(host) => match host.worker_transform_registrations(event.kind).await {
                Some(regs) => regs,
                None => return HookResult::Continue,
            },
            None => {
                let handlers = self.handlers.read().await;
                match handlers.get(&event.kind) {
                    Some(regs) if !regs.is_empty() => regs.clone(),
                    _ => return HookResult::Continue, // fast path: no handlers
                }
            }
        }; // lock dropped here
        let mut registrations = registrations;
        // Required guards run last, so each vets the input as every other
        // extension left it: an extension later in the chain could rewrite
        // what a guard had already approved. (Stable: registration order
        // holds among the guards, and among everything else.)
        if event.kind == HookKind::BeforeToolCall {
            registrations.sort_by_key(|reg| self.required.is_required(reg.handler.id()));
        }

        // Collect injections from all handlers rather than returning on first
        let mut injections = Injections::default();
        // First valid advisory context-phase report wins; the chain continues
        // so later observers still see the event.
        let mut context_phase: Option<String> = None;
        // (source extension id, message) for every Confirm, in chain order.
        let mut confirms: Vec<(String, String)> = Vec::new();
        // (id, load generation) per distinct confirming extension.
        let mut source_keys: Vec<(String, u64)> = Vec::new();

        // Input rewritten by an earlier Modify (#148 I1 / prA I1). Later
        // handlers see — and vet — the MODIFIED input, so a Modify can no
        // longer smuggle unchecked input past a later Block or Confirm.
        let mut modified_event: Option<HookEvent> = None;
        // Who rewrote it (chain order, de-duplicated).
        let mut modified_by: Vec<String> = Vec::new();
        // The first required guard that has answered in this chain.
        let mut guard_answered: Option<String> = None;

        for reg in &registrations {
            let ev: &HookEvent = modified_event.as_ref().unwrap_or(event);
            // Tool-specific filter: skip handlers that don't match.
            // Check both API name and runtime name so MCP tools with
            // sanitized names (slashes→underscores) still match.
            if let Some(ref filter) = reg.tool_filter {
                if !tool_filter_matches(filter, ev) {
                    continue;
                }
            }

            if let Some(ref matcher) = reg.matcher {
                if !matcher.matches(ev) {
                    continue;
                }
            }

            // Call handler with timeout
            let handler = reg.handler.clone();
            let event_clone = ev.clone();
            let trace_enabled = extensions_trace_enabled();
            let started_at = trace_enabled.then(Instant::now);
            // A required guard's `before_tool_call` never fails open,
            // whatever its manifest's `on_failure` says: no answer in time,
            // an error, a failed call or an answer it may not give blocks
            // the call, naming the guard and the cause.
            let required = ev.kind == HookKind::BeforeToolCall
                && self.required.is_required(reg.handler.id());
            let call = async {
                if required {
                    handler
                        .handle_required(&event_clone, no_spawn)
                        .await
                        .unwrap_or_else(|why| {
                            HookResult::block(no_answer_text(handler.id(), &why))
                        })
                } else if reg.on_failure == HookFailurePolicy::Block {
                    handler.handle_fail_closed(&event_clone, no_spawn).await
                } else if no_spawn {
                    handler.handle_no_spawn(&event_clone).await
                } else {
                    handler.handle(&event_clone).await
                }
            };
            let result = tokio::time::timeout(HANDLER_TIMEOUT, call).await;
            let answered_before = guard_answered.clone();
            if required && result.is_ok() {
                guard_answered.get_or_insert_with(|| reg.handler.id().to_string());
            }

            if trace_enabled {
                let health = reg.handler.health().await;
                let health = health.as_str();
                let restart_count = reg.handler.restart_count().await;
                let duration_ms = started_at
                    .map(|start| start.elapsed().as_millis() as u64)
                    .unwrap_or(0);
                match &result {
                    Ok(hook_result) => {
                        let action = hook_result_action(hook_result);
                        tracing::info!(
                            extension_trace = true,
                            hook = %ev.kind.as_str(),
                            extension = %reg.handler.id(),
                            action = action,
                            duration_ms = duration_ms,
                            health = health,
                            restart_count = restart_count,
                            "Extension hook trace"
                        );
                    }
                    Err(_) => {
                        tracing::warn!(
                            extension_trace = true,
                            hook = %ev.kind.as_str(),
                            extension = %reg.handler.id(),
                            action = "timeout",
                            duration_ms = duration_ms,
                            timeout_secs = HANDLER_TIMEOUT.as_secs(),
                            health = health,
                            restart_count = restart_count,
                            "Extension hook trace"
                        );
                    }
                }
            }

            match result {
                Ok(result) if required && !ev.kind.allows_result(&result) => {
                    let action = hook_result_action(&result);
                    let why = NoAnswer::BadAnswer(format!(
                        "'{action}' is not an answer to a before_tool_call"
                    ));
                    tracing::warn!(
                        hook = %ev.kind.as_str(),
                        extension = %reg.handler.id(),
                        action,
                        "Required guard returned an action not allowed for the hook — blocking the call"
                    );
                    return HookResult::block(no_answer_text(reg.handler.id(), &why));
                }
                Ok(result) if !ev.kind.allows_result(&result) => {
                    tracing::warn!(
                        hook = %ev.kind.as_str(),
                        extension = %reg.handler.id(),
                        action = hook_result_action(&result),
                        "Extension returned action not allowed for hook — ignoring"
                    );
                    continue;
                }
                Ok(HookResult::Block { reason }) => {
                    tracing::info!(
                        hook = %ev.kind.as_str(),
                        extension = %reg.handler.id(),
                        reason = %reason,
                        "Hook blocked by extension"
                    );
                    return HookResult::Block { reason };
                }
                Ok(HookResult::Continue) => {}
                Ok(result @ HookResult::Inject { .. }) => {
                    tracing::debug!(
                        hook = %ev.kind.as_str(),
                        extension = %reg.handler.id(),
                        "Extension injected context"
                    );
                    // Accumulate — don't early-return. Multiple extensions can inject.
                    injections.push(reg.handler.id(), &result);
                }
                Ok(HookResult::Modify { .. }) if required && answered_before.is_some() => {
                    // Another required guard has already vetted the input
                    // as it stood: this rewrite would run an input that
                    // guard never saw.
                    let why = NoAnswer::BadAnswer(format!(
                        "'modify' cannot apply here: required guard '{}' had already vetted the input as it was",
                        answered_before.unwrap_or_default()
                    ));
                    tracing::warn!(
                        hook = %ev.kind.as_str(),
                        extension = %reg.handler.id(),
                        "Required guard's Modify comes after another guard vetted the input — blocking the call"
                    );
                    return HookResult::block(no_answer_text(reg.handler.id(), &why));
                }
                Ok(HookResult::Modify { .. }) if required && !confirms.is_empty() => {
                    // A required guard's rewrite the bus may not apply
                    // (after an earlier Confirm) would let the input it
                    // rewrote run as it was; it blocks, naming the guard,
                    // like any answer the bus cannot use from it.
                    let confirming: Vec<&str> = confirms.iter().map(|(id, _)| id.as_str()).collect();
                    let why = NoAnswer::BadAnswer(format!(
                        "'modify' cannot apply here: an earlier extension ({}) asked to confirm the input as it was",
                        confirming.join(", ")
                    ));
                    tracing::warn!(
                        hook = %ev.kind.as_str(),
                        extension = %reg.handler.id(),
                        "Required guard's Modify cannot apply here — blocking the call"
                    );
                    return HookResult::block(no_answer_text(reg.handler.id(), &why));
                }
                Ok(HookResult::Modify { input }) => {
                    if !confirms.is_empty() {
                        // The user is asked about the input as it stood at
                        // the first Confirm; a rewrite after that point
                        // would run something they never saw.
                        tracing::warn!(
                            hook = %ev.kind.as_str(),
                            extension = %reg.handler.id(),
                            confirming = ?confirms.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
                            "Extension Modify after a pending Confirm IGNORED: the user confirms the input as it was when the first Confirm was raised"
                        );
                        continue;
                    }
                    tracing::info!(
                        hook = %ev.kind.as_str(),
                        extension = %reg.handler.id(),
                        "Hook modified tool input by extension; later handlers vet the modified input"
                    );
                    let mut next = ev.clone();
                    next.tool_input = Some(input);
                    modified_event = Some(next);
                    let id = reg.handler.id().to_string();
                    if !modified_by.contains(&id) {
                        modified_by.push(id);
                    }
                }
                Ok(HookResult::Replace { output }) => {
                    // Two-key gate: subscribing to after_tool_call needs
                    // `tools.intercept` (observe); rewriting the output
                    // additionally needs `tools.transform_output`. Without it,
                    // ignore this handler's Replace (fail-safe: original output
                    // preserved) and continue the chain.
                    if !reg.permissions.has(Permission::ToolsTransformOutput) {
                        tracing::warn!(
                            hook = %ev.kind.as_str(),
                            extension = %reg.handler.id(),
                            "Extension returned Replace without tools.transform_output permission — ignoring transform"
                        );
                        continue;
                    }
                    let observed_len = ev.tool_output.as_ref().map_or(0, String::len);
                    tracing::info!(
                        hook = %ev.kind.as_str(),
                        extension = %reg.handler.id(),
                        observed_len,
                        replacement_len = output.len(),
                        "Hook replaced tool output by extension"
                    );
                    // Persistent audit trail — who rewrote which tool's output
                    // and when, plus sizes (never content). Best-effort: a
                    // failed audit write must not break the tool flow.
                    let _ = crate::extensions::audit::record_tool_output_replace(
                        reg.handler.id(),
                        ev.kind.as_str(),
                        ev.tool_name.clone(),
                        observed_len,
                        output.len(),
                    );
                    // First transform wins (mirrors Modify). Chaining is a future enhancement.
                    return HookResult::Replace { output };
                }
                Ok(HookResult::Confirm { message, .. }) => {
                    tracing::info!(
                        hook = %ev.kind.as_str(),
                        extension = %reg.handler.id(),
                        "Hook requested confirmation by extension"
                    );
                    // Host-stamped attribution: the "Allow all" latch and
                    // the dialog key on the extension that actually asked.
                    // Keep going: a later Block must still win.
                    confirms.push((reg.handler.id().to_string(), message));
                    let id = reg.handler.id();
                    if !source_keys.iter().any(|(k, _)| k == id) {
                        source_keys.push((id.to_string(), self.extension_generation(id)));
                    }
                }
                Ok(HookResult::ContextPhase { phase }) => {
                    // Advisory only — never stops the chain. Two-key gated;
                    // an ignored report is a plain continue.
                    if let Some(phase) = accept_context_phase(reg, ev.kind, phase) {
                        if context_phase.is_none() {
                            context_phase = Some(phase);
                        } else {
                            tracing::debug!(
                                hook = %ev.kind.as_str(),
                                extension = %reg.handler.id(),
                                "Extension context_phase superseded by an earlier report — ignoring"
                            );
                        }
                    }
                }
                Err(_timeout) if required => {
                    tracing::warn!(
                        hook = %ev.kind.as_str(),
                        extension = %reg.handler.id(),
                        timeout_secs = HANDLER_TIMEOUT.as_secs(),
                        "Required guard timed out — blocking the call"
                    );
                    return HookResult::block(no_answer_text(reg.handler.id(), &NoAnswer::Timeout));
                }
                Err(_timeout) if reg.on_failure == HookFailurePolicy::Block => {
                    tracing::warn!(
                        hook = %ev.kind.as_str(),
                        extension = %reg.handler.id(),
                        timeout_secs = HANDLER_TIMEOUT.as_secs(),
                        "Guard hook handler timed out — blocking the call"
                    );
                    return reg.on_failure.timed_out(reg.handler.id());
                }
                Err(_timeout) => {
                    tracing::warn!(
                        hook = %ev.kind.as_str(),
                        extension = %reg.handler.id(),
                        timeout_secs = HANDLER_TIMEOUT.as_secs(),
                        "Hook handler timed out — skipping"
                    );
                    // Fail-open: timeout = continue
                }
            }
        }

        // No Block anywhere in the chain: resolve the Confirm(s) last.
        if !confirms.is_empty() {
            let tool = event
                .tool_runtime_name
                .clone()
                .or_else(|| event.tool_name.clone());
            let message = if confirms.len() == 1 {
                confirms[0].1.clone()
            } else {
                confirms
                    .iter()
                    .map(|(id, m)| format!("[{id}] {m}"))
                    .collect::<Vec<_>>()
                    .join("\n\n")
            };
            let sources = source_keys.iter().map(|(id, _)| id.clone()).collect();
            let generations = source_keys.iter().map(|(_, g)| *g).collect();
            return HookResult::Confirm {
                message,
                sources,
                generations,
                tool,
                modified_input: modified_event.and_then(|e| e.tool_input),
                modified_by,
            };
        }
        if let Some(input) = modified_event.and_then(|e| e.tool_input) {
            return HookResult::Modify { input };
        }

        // Merge accumulated injections from all handlers. An advisory phase
        // report is returned only when nothing else would be (Block/Inject
        // take precedence; on_message_complete permits neither).
        if let Some(inject) = injections.finish() {
            inject
        } else if let Some(phase) = context_phase {
            HookResult::ContextPhase { phase }
        } else {
            HookResult::Continue
        }
    }

    /// Emit a hook event to all registered handlers **concurrently**.
    ///
    /// All handlers race under a single shared timeout (`per_handler_timeout`).
    /// Results are collected and the first `Block` wins; injections are merged.
    ///
    /// **When to use this over `emit()`:**
    ///
    /// Only safe for hook kinds whose handlers are order-independent — i.e.
    /// where no handler's result depends on another's execution.  Currently
    /// that applies to:
    ///   - `on_session_end`: only `Continue` is a valid result; handlers are
    ///     fire-and-forget notification calls (deck, d20, jawz-widget,
    ///     synaps-tasks all write to their own stores independently).
    ///
    /// The notify-only kinds do not come through here: [`notify`](Self::notify)
    /// delivers to each handler on its own.
    ///
    /// **Do NOT use for** `before_tool_call` / `after_tool_call` / `before_message`
    /// hooks where `Block` / `Modify` / `Replace` / `Inject` semantics require a
    /// defined winner when two handlers disagree — `join_all` completion order is
    /// nondeterministic, so the "winner" would be unstable. Those hooks must use
    /// the sequential [`emit`](Self::emit), which enforces first-wins ordering.
    ///
    /// With N extensions and a 5 s per-handler timeout, serial emit takes up
    /// to N×5 s; concurrent emit collapses that to a single 5 s window
    /// regardless of N — critical for teardown budgets.
    pub async fn emit_concurrent(&self, event: &HookEvent) -> HookResult {
        // H4: real assert, not debug_assert — this invariant protects against
        // nondeterministic Block/Modify/Replace results in release builds.
        assert!(
            !matches!(
                event.kind,
                HookKind::BeforeToolCall | HookKind::AfterToolCall | HookKind::BeforeMessage
            ),
            "emit_concurrent must not be used for transform-capable hooks \
             ({:?}); their Block/Modify/Replace/Inject results need the \
             deterministic first-wins ordering of the sequential emit()",
            event.kind,
        );
        // Snapshot handler list (same as emit()).
        let registrations = {
            let handlers = self.handlers.read().await;
            match handlers.get(&event.kind) {
                Some(regs) if !regs.is_empty() => regs.clone(),
                _ => return HookResult::Continue, // fast path: no handlers
            }
        };

        // Dispatch all handlers simultaneously.
        let futures: Vec<_> =
            registrations
                .iter()
                .filter(|reg| {
                    // Apply tool filter before spawning.
                    reg.tool_filter
                        .as_deref()
                        .is_none_or(|filter| tool_filter_matches(filter, event))
                })
                .filter(|reg| reg.matcher.as_ref().is_none_or(|m| m.matches(event)))
                .map(|reg| {
                    let handler = reg.handler.clone();
                    let event_clone = event.clone();
                    async move {
                        (
                            reg,
                            tokio::time::timeout(HANDLER_TIMEOUT, handler.handle(&event_clone))
                                .await,
                        )
                    }
                })
                .collect();

        let results = join_all(futures).await;

        let mut injections = Injections::default();
        // First accepted report in completion order (registration order for
        // the collected results); advisory only, never stops the chain.
        let mut context_phase: Option<String> = None;
        for (reg, result) in results {
            match result {
                Ok(HookResult::Continue) => {}
                Ok(HookResult::Block { reason }) => {
                    return HookResult::Block { reason };
                }
                Ok(result @ HookResult::Inject { .. }) => {
                    injections.push(reg.handler.id(), &result);
                }
                Ok(HookResult::Modify { input }) => {
                    return HookResult::Modify { input };
                }
                Ok(HookResult::Replace { output }) => {
                    return HookResult::Replace { output };
                }
                Ok(HookResult::Confirm { message, .. }) => {
                    return HookResult::Confirm {
                        message,
                        sources: vec![reg.handler.id().to_string()],
                        generations: vec![self.extension_generation(reg.handler.id())],
                        tool: event.tool_runtime_name.clone().or_else(|| event.tool_name.clone()),
                        modified_input: None,
                        modified_by: vec![],
                    };
                }
                Ok(result @ HookResult::ContextPhase { .. }) => {
                    if !event.kind.allows_result(&result) {
                        tracing::warn!(
                            hook = %event.kind.as_str(),
                            extension = %reg.handler.id(),
                            action = hook_result_action(&result),
                            "Extension returned action not allowed for hook — ignoring"
                        );
                        continue;
                    }
                    if let HookResult::ContextPhase { phase } = result {
                        if let Some(phase) = accept_context_phase(reg, event.kind, phase) {
                            context_phase.get_or_insert(phase);
                        }
                    }
                }
                Err(_timeout) => {
                    tracing::warn!(
                        hook = %event.kind.as_str(),
                        extension = %reg.handler.id(),
                        timeout_secs = HANDLER_TIMEOUT.as_secs(),
                        "Hook handler timed out in concurrent emit — skipping"
                    );
                }
            }
        }

        if let Some(inject) = injections.finish() {
            inject
        } else if let Some(phase) = context_phase {
            HookResult::ContextPhase { phase }
        } else {
            HookResult::Continue
        }
    }

    /// Deliver a notify-only event ([`HookKind::is_notification`]) without
    /// waiting for anyone. Returns at once and cannot fail: the caller's
    /// work is already done and no handler can delay, fail or change it.
    ///
    /// The handlers run concurrently on one detached task, each under
    /// [`HANDLER_TIMEOUT`], so the task ends within that time of the event
    /// being ready and holds nothing of the caller's. Every result, error,
    /// timeout and panic is ignored, and costs the other handlers nothing
    /// ([`Self::notify_with`]). Delivery is best effort and unordered; the
    /// event is dropped when
    ///
    /// - nobody subscribes to the kind (then nothing is spawned at all),
    /// - this is a worker's view ([`Self::worker_transforms`]),
    /// - [`NOTIFY_MAX_IN_FLIGHT`] deliveries are still in flight (logged),
    /// - there is no Tokio runtime to deliver on, or it shuts down first.
    ///
    /// A handler that is stopped is not started again by a late delivery
    /// (`ProcessExtension::shutdown` is terminal).
    pub fn notify(self: &Arc<Self>, event: HookEvent) {
        self.notify_with(event.kind, async move { Some(event) });
    }

    /// [`Self::notify`] for events that cost something to build (a file
    /// lookup): `events` runs on the detached task, inside the same in-flight
    /// limit, and is not polled at all when the delivery would be dropped.
    /// The build is not under the handler timeout, so it must end on its own.
    ///
    /// What it yields is `None`, `Some(event)` or a `Vec` of events. Several
    /// are one delivery, on the one task: every handler is sent them in that
    /// order, the next once it has answered the one before. So a caller with
    /// many events to send takes one in-flight slot, not one each, and
    /// cannot have its later events dropped because its earlier ones are
    /// still in flight. An event of another kind than `kind` is not sent.
    ///
    /// Each handler gets the sequence on its own. One that does not answer
    /// an event within [`HANDLER_TIMEOUT`] is sent no more of this delivery
    /// (logged once, with the number not sent): the request that timed out
    /// is still in its input, and each further one would keep it busy, and
    /// its other hooks waiting, that much longer. Nobody else's events are
    /// held up or dropped by it, and the task ends within one timeout of
    /// the slowest answer. A handler that panics on an event loses that
    /// event and nothing more: it is sent the next, and the other handlers
    /// are not touched. (That is where a panic unwinds. The release profile
    /// aborts on any panic, here as anywhere.)
    pub fn notify_with<F, I>(self: &Arc<Self>, kind: HookKind, events: F)
    where
        F: std::future::Future<Output = I> + Send + 'static,
        I: IntoIterator<Item = HookEvent> + Send,
        I::IntoIter: Send,
    {
        if !kind.is_notification() {
            // A hook whose answer matters must be awaited (`emit`).
            tracing::error!(hook = %kind.as_str(), "notify used for a hook that is not notify-only; nothing sent");
            return;
        }
        if self.is_worker_view() || !self.may_have_subscriber(kind) {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let Ok(slot) = Arc::clone(&self.notify_slots).try_acquire_owned() else {
            tracing::warn!(
                hook = %kind.as_str(),
                in_flight = NOTIFY_MAX_IN_FLIGHT,
                "Notify-only hook event dropped: earlier deliveries are still in flight"
            );
            return;
        };
        let bus = Arc::clone(self);
        runtime.spawn(async move {
            let _slot = slot;
            let events: Vec<HookEvent> =
                events.await.into_iter().filter(|event| event.kind == kind).collect();
            bus.deliver_notifications(kind, &events).await;
        });
    }

    /// One [`Self::notify_with`] delivery: `events`, in order, to every
    /// handler of `kind`, the handlers independent of each other.
    async fn deliver_notifications(&self, kind: HookKind, events: &[HookEvent]) {
        let registrations = {
            let handlers = self.handlers.read().await;
            handlers.get(&kind).cloned().unwrap_or_default()
        }; // lock dropped here, as in `emit`
        let sequences = registrations.iter().map(|reg| async move {
            let wanted = |event: &HookEvent| {
                reg.tool_filter
                    .as_deref()
                    .is_none_or(|filter| tool_filter_matches(filter, event))
                    && reg.matcher.as_ref().is_none_or(|m| m.matches(event))
            };
            for (sent, event) in events.iter().enumerate() {
                if !wanted(event) {
                    continue;
                }
                // The answer is discarded. A panic is caught here, so it ends
                // this one call and not the task every handler shares.
                let call = tokio::time::timeout(HANDLER_TIMEOUT, reg.handler.handle(event));
                match std::panic::AssertUnwindSafe(call).catch_unwind().await {
                    Ok(Ok(_answer)) => {}
                    Ok(Err(_timeout)) => {
                        // The host has given up on this call; the handler
                        // has not. Sending it the rest would only add to
                        // what it is behind by, and to the time its other
                        // hooks time out in.
                        let not_sent =
                            events[sent + 1..].iter().filter(|event| wanted(event)).count();
                        tracing::warn!(
                            hook = %kind.as_str(),
                            extension = %reg.handler.id(),
                            timeout_secs = HANDLER_TIMEOUT.as_secs(),
                            not_sent,
                            "Notify-only hook handler timed out — the later events of this delivery are not sent to it"
                        );
                        break;
                    }
                    Err(_panic) => tracing::error!(
                        hook = %kind.as_str(),
                        extension = %reg.handler.id(),
                        "Notify-only hook handler panicked — event skipped for it"
                    ),
                }
            }
        });
        join_all(sequences).await;
    }

    /// Whether `kind` may have a subscriber, without waiting: `true` while
    /// the handler list is being changed (the delivery task then looks).
    fn may_have_subscriber(&self, kind: HookKind) -> bool {
        match self.handlers.try_read() {
            Ok(handlers) => handlers.get(&kind).is_some_and(|regs| !regs.is_empty()),
            Err(_) => true,
        }
    }

    /// [`Self::notify`] deliveries still in flight.
    #[cfg(test)]
    pub(crate) fn notifies_in_flight(&self) -> usize {
        NOTIFY_MAX_IN_FLIGHT - self.notify_slots.available_permits()
    }

    /// Remove all handlers for a given extension ID.
    pub async fn unsubscribe_all(&self, extension_id: &str) {
        *self
            .generations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(extension_id.to_string())
            .or_insert(0) += 1;
        let mut handlers = self.handlers.write().await;
        for regs in handlers.values_mut() {
            regs.retain(|r| r.handler.id() != extension_id);
        }
    }

    /// Number of registered handlers across all hooks. On a worker view:
    /// the host transform handlers it would dispatch.
    pub async fn handler_count(&self) -> usize {
        if let Some(host) = &self.transforms_of {
            return host
                .worker_transform_registrations(HookKind::AfterToolCall)
                .await
                .map_or(0, |regs| regs.len());
        }
        let handlers = self.handlers.read().await;
        handlers.values().map(|v| v.len()).sum()
    }

    /// Whether any handler is registered for `kind` (its matchers and tool
    /// filters aside): whether an `emit` of it can reach an extension.
    pub async fn has_handlers(&self, kind: HookKind) -> bool {
        match &self.transforms_of {
            Some(host) => host
                .worker_transform_registrations(kind)
                .await
                .is_some_and(|regs| !regs.is_empty()),
            None => self.handlers.read().await.get(&kind).is_some_and(|regs| !regs.is_empty()),
        }
    }

    /// Check if any handlers are registered (for fast-path decisions).
    pub async fn is_empty(&self) -> bool {
        if self.is_worker_view() {
            return self.handler_count().await == 0;
        }
        let handlers = self.handlers.read().await;
        handlers.values().all(|v| v.is_empty())
    }

    /// Return all (kind, tool_filter) pairs subscribed by the given extension id.
    /// Sorted by kind name, then by tool_filter (None first), for stable output.
    pub async fn subscriptions_for(&self, extension_id: &str) -> Vec<(HookKind, Option<String>)> {
        let handlers = self.handlers.read().await;
        let mut out: Vec<(HookKind, Option<String>)> = Vec::new();
        for (kind, regs) in handlers.iter() {
            for reg in regs {
                if reg.handler.id() == extension_id {
                    out.push((*kind, reg.tool_filter.clone()));
                }
            }
        }
        out.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()).then_with(|| a.1.cmp(&b.1)));
        out
    }
}

impl Default for HookBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::hooks::events::HookEvent;
    use crate::extensions::permissions::Permission;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Test handler that counts calls and returns a configurable result.
    struct TestHandler {
        id: String,
        call_count: AtomicUsize,
        result: HookResult,
    }

    impl TestHandler {
        fn new(id: &str, result: HookResult) -> Arc<Self> {
            Arc::new(Self {
                id: id.to_string(),
                call_count: AtomicUsize::new(0),
                result,
            })
        }

        fn calls(&self) -> usize {
            self.call_count.load(Ordering::Relaxed)
        }
    }

    #[async_trait]
    impl crate::extensions::runtime::ExtensionHandler for TestHandler {
        fn id(&self) -> &str {
            &self.id
        }

        async fn handle(&self, _event: &HookEvent) -> HookResult {
            self.call_count.fetch_add(1, Ordering::Relaxed);
            self.result.clone()
        }

        async fn shutdown(&self) {}
    }

    fn perms_with(perms: &[Permission]) -> PermissionSet {
        let mut set = PermissionSet::new();
        for p in perms {
            set.grant(*p);
        }
        set
    }

    /// emit_after_tool_call returns the substituted output when an
    /// after_tool_call handler returns Replace — this is the seam that lets an
    /// extension compress/redact tool output before it enters history.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn after_tool_call_replace_substitutes_recorded_output() {
        // #186: Replace writes the intercept audit — isolate the base dir.
        let base = crate::test_env::BaseDirGuard::new();
        let bus = std::sync::Arc::new(HookBus::new());
        let handler = TestHandler::new(
            "compressor",
            HookResult::Replace {
                output: "COMPRESSED".into(),
            },
        );
        bus.subscribe(
            HookKind::AfterToolCall,
            handler,
            None,
            None,
            perms_with(&[Permission::ToolsIntercept, Permission::ToolsTransformOutput]),
        )
        .await
        .unwrap();

        let recorded = crate::runtime::emit_after_tool_call(
            &bus,
            "bash",
            None,
            serde_json::json!({"command": "cat huge.log"}),
            "RAW 10k lines".to_string(),
            30_000,
            None,
            None,
        )
        .await;

        assert_eq!(recorded, "COMPRESSED", "Replace output must reach history");
        let audit = std::fs::read_to_string(
            crate::extensions::audit::intercept_file_path_for(base.path()),
        )
        .expect("intercept audit must land in the isolated base dir");
        assert!(audit.contains("\"plugin_id\":\"compressor\""));
    }

    /// With no transform handler, the original output is preserved unchanged.
    #[tokio::test]
    async fn after_tool_call_without_replace_keeps_original_output() {
        let bus = std::sync::Arc::new(HookBus::new());
        let recorded = crate::runtime::emit_after_tool_call(
            &bus,
            "bash",
            None,
            serde_json::json!({}),
            "RAW".to_string(),
            30_000,
            None,
            None,
        )
        .await;

        assert_eq!(recorded, "RAW", "no Replace → original output preserved");
    }

    /// First Replace wins: once an earlier after_tool_call handler returns
    /// Replace, later handlers are never reached (mirrors block/modify
    /// chain-stop). Proves the "first transform wins" comment in emit().
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn replace_stops_chain_for_after_tool_call() {
        // #186: Replace writes the intercept audit — isolate the base dir.
        let _base = crate::test_env::BaseDirGuard::new();
        let bus = HookBus::new();
        let first = TestHandler::new(
            "first",
            HookResult::Replace {
                output: "FIRST".into(),
            },
        );
        let second = TestHandler::new(
            "second",
            HookResult::Replace {
                output: "SECOND".into(),
            },
        );
        let perms = perms_with(&[Permission::ToolsIntercept, Permission::ToolsTransformOutput]);

        bus.subscribe(
            HookKind::AfterToolCall,
            first.clone(),
            None,
            None,
            perms.clone(),
        )
        .await
        .unwrap();
        bus.subscribe(HookKind::AfterToolCall, second.clone(), None, None, perms)
            .await
            .unwrap();

        let event = HookEvent::after_tool_call("bash", serde_json::json!({}), "RAW".to_string());
        let result = bus.emit(&event).await;

        assert_eq!(
            result,
            HookResult::Replace {
                output: "FIRST".into()
            }
        );
        assert_eq!(first.calls(), 1);
        assert_eq!(second.calls(), 0); // never reached — first transform wins
    }

    /// Two-key gate: an extension may subscribe to after_tool_call with only
    /// `tools.intercept` (observe), but its Replace is IGNORED without the
    /// additional `tools.transform_output` — the original output is preserved.
    #[tokio::test]
    async fn replace_without_transform_permission_is_ignored() {
        let bus = std::sync::Arc::new(HookBus::new());
        let handler = TestHandler::new(
            "observer-only",
            HookResult::Replace {
                output: "SNEAKY".into(),
            },
        );
        // Only the observe key — NOT tools.transform_output.
        bus.subscribe(
            HookKind::AfterToolCall,
            handler.clone(),
            None,
            None,
            perms_with(&[Permission::ToolsIntercept]),
        )
        .await
        .unwrap();

        let recorded = crate::runtime::emit_after_tool_call(
            &bus,
            "bash",
            None,
            serde_json::json!({}),
            "ORIGINAL".to_string(),
            30_000,
            None,
            None,
        )
        .await;

        assert_eq!(handler.calls(), 1, "handler still runs (it's subscribed)");
        assert_eq!(
            recorded, "ORIGINAL",
            "Replace without tools.transform_output must be ignored — original preserved"
        );
    }

    // ── context_phase (advisory work-phase report) ───────────────────────────

    fn phase(p: &str) -> HookResult {
        HookResult::ContextPhase { phase: p.into() }
    }

    fn message_complete_event() -> HookEvent {
        HookEvent::on_message_complete(
            "done",
            serde_json::json!({"content_block_count": 1, "has_tool_use": false}),
        )
    }

    /// Two-key gate, positive side: `privacy.llm_content` (subscribe) plus
    /// `session.lifecycle` (report) => the advisory report is returned.
    #[tokio::test]
    async fn context_phase_accepted_with_session_lifecycle() {
        let bus = HookBus::new();
        let reporter = TestHandler::new("reporter", phase("new_task"));
        bus.subscribe(
            HookKind::OnMessageComplete,
            reporter.clone(),
            None,
            None,
            perms_with(&[Permission::LlmContent, Permission::SessionLifecycle]),
        )
        .await
        .unwrap();

        let result = bus.emit(&message_complete_event()).await;
        assert_eq!(result, phase("new_task"));
        assert_eq!(reporter.calls(), 1);
    }

    /// Two-key gate, negative side: the observe key alone lets the handler
    /// run, but its report is ignored (treated as continue).
    #[tokio::test]
    async fn context_phase_ignored_without_session_lifecycle() {
        let bus = HookBus::new();
        let reporter = TestHandler::new("observer-only", phase("new_task"));
        bus.subscribe(
            HookKind::OnMessageComplete,
            reporter.clone(),
            None,
            None,
            perms_with(&[Permission::LlmContent]),
        )
        .await
        .unwrap();

        let result = bus.emit(&message_complete_event()).await;
        assert_eq!(result, HookResult::Continue);
        assert_eq!(reporter.calls(), 1, "handler still runs (it's subscribed)");
    }

    /// A report does not stop the chain: later observers still run, and the
    /// first VALID report wins over later ones (an unprivileged earlier
    /// report does not claim the slot).
    #[tokio::test]
    async fn context_phase_does_not_stop_chain_and_first_valid_wins() {
        let bus = HookBus::new();
        let unprivileged = TestHandler::new("unprivileged", phase("plan"));
        let first = TestHandler::new("first", phase("new_task"));
        let second = TestHandler::new("second", phase("wrap_up"));
        let observer = TestHandler::new("observer", HookResult::Continue);
        let both = || perms_with(&[Permission::LlmContent, Permission::SessionLifecycle]);

        bus.subscribe(
            HookKind::OnMessageComplete,
            unprivileged.clone(),
            None,
            None,
            perms_with(&[Permission::LlmContent]),
        )
        .await
        .unwrap();
        for handler in [first.clone(), second.clone()] {
            bus.subscribe(HookKind::OnMessageComplete, handler, None, None, both())
                .await
                .unwrap();
        }
        bus.subscribe(
            HookKind::OnMessageComplete,
            observer.clone(),
            None,
            None,
            perms_with(&[Permission::LlmContent]),
        )
        .await
        .unwrap();

        let result = bus.emit(&message_complete_event()).await;
        assert_eq!(result, phase("new_task"), "first valid report wins");
        assert_eq!(unprivileged.calls(), 1);
        assert_eq!(first.calls(), 1);
        assert_eq!(second.calls(), 1, "later reporter still runs");
        assert_eq!(observer.calls(), 1, "later observer still runs");
    }

    /// A Block from an on_message_complete handler is still ignored, and a
    /// valid phase report from another handler is still returned.
    #[tokio::test]
    async fn context_phase_survives_ignored_block_on_message_complete() {
        let bus = HookBus::new();
        let blocker = TestHandler::new(
            "blocker",
            HookResult::Block {
                reason: "nope".into(),
            },
        );
        let reporter = TestHandler::new("reporter", phase("execute"));
        bus.subscribe(
            HookKind::OnMessageComplete,
            blocker.clone(),
            None,
            None,
            perms_with(&[Permission::LlmContent, Permission::SessionLifecycle]),
        )
        .await
        .unwrap();
        bus.subscribe(
            HookKind::OnMessageComplete,
            reporter.clone(),
            None,
            None,
            perms_with(&[Permission::LlmContent, Permission::SessionLifecycle]),
        )
        .await
        .unwrap();

        let result = bus.emit(&message_complete_event()).await;
        assert_eq!(result, phase("execute"));
        assert_eq!(blocker.calls(), 1);
        assert_eq!(reporter.calls(), 1);
    }

    /// context_phase is not allowed on any other hook kind, even with both
    /// permissions — sequential and concurrent paths alike.
    #[tokio::test]
    async fn context_phase_not_allowed_on_other_hooks() {
        let bus = HookBus::new();
        let all_perms = perms_with(&[
            Permission::ToolsIntercept,
            Permission::LlmContent,
            Permission::SessionLifecycle,
        ]);
        let handlers: Vec<(HookKind, HookEvent, Arc<TestHandler>)> = vec![
            (
                HookKind::BeforeToolCall,
                HookEvent::before_tool_call("bash", serde_json::json!({})),
                TestHandler::new("btc", phase("new_task")),
            ),
            (
                HookKind::AfterToolCall,
                HookEvent::after_tool_call("bash", serde_json::json!({}), "o".into()),
                TestHandler::new("atc", phase("new_task")),
            ),
            (
                HookKind::BeforeMessage,
                HookEvent::before_message("hi"),
                TestHandler::new("bm", phase("new_task")),
            ),
            (
                HookKind::OnCompaction,
                HookEvent::on_compaction("a", "b", "s", 1, serde_json::json!({})),
                TestHandler::new("oc", phase("new_task")),
            ),
            (
                HookKind::OnSessionStart,
                HookEvent::on_session_start("s"),
                TestHandler::new("oss", phase("new_task")),
            ),
            (
                HookKind::OnSessionEnd,
                HookEvent::on_session_end("s", None),
                TestHandler::new("ose", phase("new_task")),
            ),
        ];
        for (kind, _, handler) in &handlers {
            bus.subscribe(*kind, handler.clone(), None, None, all_perms.clone())
                .await
                .unwrap();
        }
        for (kind, event, handler) in &handlers {
            let result = bus.emit(event).await;
            assert_eq!(result, HookResult::Continue, "{}", kind.as_str());
            assert_eq!(handler.calls(), 1, "{}", kind.as_str());
        }
        // Concurrent path (only legal for non-transform hooks).
        let end = HookEvent::on_session_end("s", None);
        assert_eq!(bus.emit_concurrent(&end).await, HookResult::Continue);
    }

    /// The concurrent emit path applies the same two-key gate and first-wins
    /// aggregation for on_message_complete.
    #[tokio::test]
    async fn context_phase_concurrent_emit_two_key_gate() {
        let bus = HookBus::new();
        let unprivileged = TestHandler::new("unprivileged", phase("plan"));
        let reporter = TestHandler::new("reporter", phase("wrap_up"));
        bus.subscribe(
            HookKind::OnMessageComplete,
            unprivileged.clone(),
            None,
            None,
            perms_with(&[Permission::LlmContent]),
        )
        .await
        .unwrap();
        bus.subscribe(
            HookKind::OnMessageComplete,
            reporter.clone(),
            None,
            None,
            perms_with(&[Permission::LlmContent, Permission::SessionLifecycle]),
        )
        .await
        .unwrap();

        let result = bus.emit_concurrent(&message_complete_event()).await;
        assert_eq!(result, phase("wrap_up"));
        assert_eq!(unprivileged.calls(), 1);
        assert_eq!(reporter.calls(), 1);
    }

    #[test]
    fn trace_env_value_parser_accepts_common_truthy_values() {
        for value in ["1", "true", "TRUE", "yes", "on"] {
            std::env::set_var("SYNAPS_EXTENSIONS_TRACE", value);
            assert!(
                extensions_trace_enabled(),
                "{value} should enable trace mode"
            );
        }

        for value in ["", "0", "false", "off", "no"] {
            std::env::set_var("SYNAPS_EXTENSIONS_TRACE", value);
            assert!(
                !extensions_trace_enabled(),
                "{value:?} should not enable trace mode"
            );
        }
        std::env::remove_var("SYNAPS_EXTENSIONS_TRACE");
    }

    #[tokio::test]
    async fn matcher_skips_handler_when_input_does_not_contain_value() {
        let bus = HookBus::new();
        let handler = TestHandler::new(
            "matcher",
            HookResult::Block {
                reason: "matched".into(),
            },
        );
        let mut perms = PermissionSet::new();
        perms.grant(Permission::ToolsIntercept);
        bus.subscribe(
            HookKind::BeforeToolCall,
            handler.clone(),
            None,
            Some(HookMatcher {
                input_contains: Some("danger".to_string()),
                input_equals: None,
            }),
            perms,
        )
        .await
        .unwrap();

        let safe = HookEvent::before_tool_call("bash", serde_json::json!({"command": "echo safe"}));
        assert!(matches!(bus.emit(&safe).await, HookResult::Continue));

        let danger =
            HookEvent::before_tool_call("bash", serde_json::json!({"command": "echo danger"}));
        assert!(matches!(bus.emit(&danger).await, HookResult::Block { .. }));
    }

    #[test]
    fn hook_result_action_names_are_stable_for_trace_logs() {
        assert_eq!(hook_result_action(&HookResult::Continue), "continue");
        assert_eq!(
            hook_result_action(&HookResult::Block {
                reason: "stop".into(),
            }),
            "block"
        );
        assert_eq!(
            hook_result_action(&HookResult::inject("context")),
            "inject"
        );
        assert_eq!(
            hook_result_action(&HookResult::confirm("Proceed?")),
            "confirm"
        );
        assert_eq!(
            hook_result_action(&HookResult::Modify {
                input: serde_json::json!({"command": "echo safe"}),
            }),
            "modify"
        );
    }

    #[tokio::test]
    async fn empty_bus_returns_continue() {
        let bus = HookBus::new();
        let event = HookEvent::before_tool_call("bash", serde_json::json!({}));
        let result = bus.emit(&event).await;
        assert!(matches!(result, HookResult::Continue));
    }

    #[tokio::test]
    async fn handler_receives_events() {
        let bus = HookBus::new();
        let handler = TestHandler::new("test-ext", HookResult::Continue);
        let perms = perms_with(&[Permission::ToolsIntercept]);

        bus.subscribe(HookKind::BeforeToolCall, handler.clone(), None, None, perms)
            .await
            .unwrap();

        let event = HookEvent::before_tool_call("bash", serde_json::json!({"command": "ls"}));
        bus.emit(&event).await;

        assert_eq!(handler.calls(), 1);
    }

    async fn bus_with(handlers: Vec<Arc<TestHandler>>) -> HookBus {
        let bus = HookBus::new();
        let perms = perms_with(&[Permission::ToolsIntercept]);
        for h in handlers {
            bus.subscribe(HookKind::BeforeToolCall, h, None, None, perms.clone())
                .await
                .unwrap();
        }
        bus
    }

    /// #148 I1: a Confirm no longer stops the chain. Later handlers run, and
    /// with nothing else to say the Confirm is returned (resolved last).
    #[tokio::test]
    async fn confirm_does_not_stop_chain_for_before_tool_call() {
        let confirmer = TestHandler::new("confirmer", HookResult::confirm("Run this command?"));
        let after = TestHandler::new("after", HookResult::Continue);
        let bus = bus_with(vec![confirmer.clone(), after.clone()]).await;

        let event = HookEvent::before_tool_call("bash", serde_json::json!({}));
        let result = bus.emit(&event).await;

        assert!(matches!(result, HookResult::Confirm { .. }));
        assert_eq!(confirmer.calls(), 1);
        assert_eq!(after.calls(), 1, "later handlers are still evaluated");
    }

    /// A later extension's Block wins over an earlier Confirm.
    #[tokio::test]
    async fn later_block_wins_over_earlier_confirm() {
        let confirmer = TestHandler::new("confirmer", HookResult::confirm("Run?"));
        let blocker = TestHandler::new(
            "policy",
            HookResult::Block {
                reason: "policy veto".into(),
            },
        );
        let bus = bus_with(vec![confirmer, blocker]).await;
        let event = HookEvent::before_tool_call("bash", serde_json::json!({}));
        assert!(matches!(
            bus.emit(&event).await,
            HookResult::Block { reason } if reason == "policy veto"
        ));
    }

    /// Two extensions confirming the same call: one prompt carrying both
    /// sources (so "Allow all" from one cannot silently skip the other).
    #[tokio::test]
    async fn confirms_from_several_extensions_merge_sources() {
        let a = TestHandler::new("ext-a", HookResult::confirm("A asks"));
        let b = TestHandler::new("ext-b", HookResult::confirm("B asks"));
        let bus = bus_with(vec![a, b]).await;
        let event = HookEvent::before_tool_call("bash", serde_json::json!({}));
        match bus.emit(&event).await {
            HookResult::Confirm {
                message, sources, ..
            } => {
                assert_eq!(sources, vec!["ext-a".to_string(), "ext-b".to_string()]);
                assert!(message.contains("A asks") && message.contains("B asks"), "{message}");
                assert!(message.contains("[ext-a]") && message.contains("[ext-b]"), "{message}");
            }
            other => panic!("expected merged Confirm, got {other:?}"),
        }
    }

    /// With the confirming extension latched ("Allow all"), a later
    /// extension's Block is still evaluated and still blocks.
    #[tokio::test]
    async fn armed_latch_never_skips_a_later_block() {
        let confirmer = TestHandler::new("jev", HookResult::confirm("Run?"));
        let blocker = TestHandler::new(
            "policy",
            HookResult::Block {
                reason: "policy veto".into(),
            },
        );
        let bus = bus_with(vec![confirmer, blocker.clone()]).await;
        let latch = crate::tools::SessionConfirmLatch::new();
        latch.allow(&[("jev".to_string(), 0)]);
        let event = HookEvent::before_tool_call("bash", serde_json::json!({}));
        let decision = crate::runtime::resolve_before_tool_call_decision(
            serde_json::json!({}),
            bus.emit(&event).await,
            None,
            false,
            Some(&latch),
        )
        .await;
        assert_eq!(blocker.calls(), 1);
        assert!(matches!(
            decision,
            crate::runtime::BeforeToolCallDecision::Block { reason } if reason == "policy veto"
        ));
    }

    /// Records the `tool_input` it was shown, then returns `result`.
    struct InputSeen {
        id: String,
        seen: std::sync::Mutex<Option<serde_json::Value>>,
        result: HookResult,
    }
    #[async_trait]
    impl crate::extensions::runtime::ExtensionHandler for InputSeen {
        fn id(&self) -> &str {
            &self.id
        }
        async fn handle(&self, event: &HookEvent) -> HookResult {
            *self.seen.lock().unwrap() = event.tool_input.clone();
            self.result.clone()
        }
        async fn shutdown(&self) {}
    }
    fn input_seen(id: &str, result: HookResult) -> Arc<InputSeen> {
        Arc::new(InputSeen {
            id: id.into(),
            seen: std::sync::Mutex::new(None),
            result,
        })
    }
    fn modify(cmd: &str) -> HookResult {
        HookResult::Modify {
            input: serde_json::json!({ "command": cmd }),
        }
    }
    async fn bus_of(handlers: Vec<Arc<dyn crate::extensions::runtime::ExtensionHandler>>) -> HookBus {
        let bus = HookBus::new();
        let perms = perms_with(&[Permission::ToolsIntercept]);
        for h in handlers {
            bus.subscribe(HookKind::BeforeToolCall, h, None, None, perms.clone())
                .await
                .unwrap();
        }
        bus
    }

    /// prA I1: a Modify no longer ends the chain. A later Block sees the
    /// MODIFIED input and still wins.
    #[tokio::test]
    async fn modify_then_block_blocks_and_blocker_vets_modified_input() {
        let blocker = input_seen(
            "policy",
            HookResult::Block {
                reason: "veto".into(),
            },
        );
        let bus = bus_of(vec![input_seen("rewriter", modify("rm -rf /")), blocker.clone()]).await;
        let event = HookEvent::before_tool_call("bash", serde_json::json!({"command": "ls"}));
        assert!(matches!(bus.emit(&event).await, HookResult::Block { reason } if reason == "veto"));
        assert_eq!(
            blocker.seen.lock().unwrap().clone(),
            Some(serde_json::json!({"command": "rm -rf /"})),
            "the blocker must vet the rewritten input, not the original"
        );
    }

    /// Modify then Confirm: the user is still asked, and approval runs the
    /// modified input.
    #[tokio::test]
    async fn modify_then_confirm_prompts_with_modified_input() {
        let confirmer = input_seen("jev", HookResult::confirm("ok?"));
        let bus = bus_of(vec![input_seen("rewriter", modify("echo safe")), confirmer.clone()]).await;
        let event = HookEvent::before_tool_call("bash", serde_json::json!({"command": "ls"}));
        let result = bus.emit(&event).await;
        assert_eq!(
            confirmer.seen.lock().unwrap().clone(),
            Some(serde_json::json!({"command": "echo safe"}))
        );
        match &result {
            HookResult::Confirm {
                sources,
                modified_input,
                ..
            } => {
                assert_eq!(sources, &vec!["jev".to_string()]);
                assert_eq!(
                    modified_input,
                    &Some(serde_json::json!({"command": "echo safe"}))
                );
            }
            other => panic!("expected Confirm, got {other:?}"),
        }
        // Approved (here: auto-approve) → the MODIFIED input runs.
        let decision = crate::runtime::resolve_before_tool_call_decision(
            serde_json::json!({"command": "ls"}),
            result,
            None,
            true,
            None,
        )
        .await;
        assert!(matches!(
            decision,
            crate::runtime::BeforeToolCallDecision::Continue { input }
                if input == serde_json::json!({"command": "echo safe"})
        ));
    }

    /// Two modifiers compose in order; with nothing else, the final input is
    /// returned as a Modify.
    #[tokio::test]
    async fn modifiers_compose_and_final_input_is_returned() {
        let second = input_seen("second", modify("two"));
        let bus = bus_of(vec![input_seen("first", modify("one")), second.clone()]).await;
        let event = HookEvent::before_tool_call("bash", serde_json::json!({"command": "zero"}));
        assert!(matches!(
            bus.emit(&event).await,
            HookResult::Modify { input } if input == serde_json::json!({"command": "two"})
        ));
        assert_eq!(
            second.seen.lock().unwrap().clone(),
            Some(serde_json::json!({"command": "one"}))
        );
    }

    /// Unloading / reinstalling an extension under the same id bumps its
    /// load generation, so a latch armed for the old instance doesn't match.
    #[tokio::test]
    async fn reinstall_under_same_id_gets_a_new_confirm_generation() {
        let bus = bus_with(vec![TestHandler::new("jev", HookResult::confirm("ok?"))]).await;
        let event = HookEvent::before_tool_call("bash", serde_json::json!({}));
        let gens = |r: HookResult| match r {
            HookResult::Confirm { generations, .. } => generations,
            other => panic!("{other:?}"),
        };
        assert_eq!(gens(bus.emit(&event).await), vec![0]);
        bus.unsubscribe_all("jev").await;
        bus.subscribe(
            HookKind::BeforeToolCall,
            TestHandler::new("jev", HookResult::confirm("ok?")),
            None,
            None,
            perms_with(&[Permission::ToolsIntercept]),
        )
        .await
        .unwrap();
        let latch = crate::tools::SessionConfirmLatch::new();
        latch.allow(&[("jev".to_string(), 0)]);
        let result = bus.emit(&event).await;
        let decision = crate::runtime::resolve_before_tool_call_decision(
            serde_json::json!({}),
            result.clone(),
            None,
            false,
            Some(&latch),
        )
        .await;
        assert_eq!(gens(result), vec![1]);
        assert!(
            matches!(decision, crate::runtime::BeforeToolCallDecision::Block { .. }),
            "old consent must not carry over to the reinstalled extension"
        );
    }

    /// A Modify after a pending Confirm is not applied (the user is asked to
    /// confirm the original call) — same outcome as before, when the chain
    /// stopped at the Confirm.
    #[tokio::test]
    async fn modify_after_confirm_is_ignored() {
        let confirmer = TestHandler::new("confirmer", HookResult::confirm("Run?"));
        let modifier = TestHandler::new(
            "modifier",
            HookResult::Modify {
                input: serde_json::json!({"command": "echo other"}),
            },
        );
        let bus = bus_with(vec![confirmer, modifier]).await;
        let event = HookEvent::before_tool_call("bash", serde_json::json!({}));
        assert!(matches!(
            bus.emit(&event).await,
            HookResult::Confirm { modified_input: None, .. }
        ), "Confirm→Modify: the modify is ignored (logged), the original input is confirmed");
    }

    #[tokio::test]
    async fn confirm_is_stamped_with_source_extension_and_tool() {
        let bus = HookBus::new();
        let confirmer = TestHandler::new("jev", HookResult::confirm("Run ls?"));
        let perms = perms_with(&[Permission::ToolsIntercept]);
        bus.subscribe(HookKind::BeforeToolCall, confirmer, None, None, perms)
            .await
            .unwrap();
        let event = HookEvent::before_tool_call("bash", serde_json::json!({}));
        match bus.emit(&event).await {
            HookResult::Confirm {
                message,
                sources,
                tool,
                ..
            } => {
                assert_eq!(message, "Run ls?");
                assert_eq!(sources, vec!["jev".to_string()]);
                assert_eq!(tool.as_deref(), Some("bash"));
            }
            other => panic!("expected Confirm, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn confirm_is_ignored_for_non_tool_hooks() {
        let bus = HookBus::new();
        let confirmer = TestHandler::new(
            "confirmer",
            HookResult::confirm("Not allowed here"),
        );
        let perms = perms_with(&[Permission::LlmContent]);

        bus.subscribe(
            HookKind::BeforeMessage,
            confirmer.clone(),
            None,
            None,
            perms,
        )
        .await
        .unwrap();

        let event = HookEvent::before_message("hello");
        let result = bus.emit(&event).await;

        assert!(matches!(result, HookResult::Continue));
        assert_eq!(confirmer.calls(), 1);
    }

    #[tokio::test]
    async fn block_stops_chain() {
        let bus = HookBus::new();
        let blocker = TestHandler::new(
            "blocker",
            HookResult::Block {
                reason: "dangerous".into(),
            },
        );
        let after = TestHandler::new("after", HookResult::Continue);
        let perms = perms_with(&[Permission::ToolsIntercept]);

        bus.subscribe(
            HookKind::BeforeToolCall,
            blocker.clone(),
            None,
            None,
            perms.clone(),
        )
        .await
        .unwrap();
        bus.subscribe(HookKind::BeforeToolCall, after.clone(), None, None, perms)
            .await
            .unwrap();

        let event = HookEvent::before_tool_call("bash", serde_json::json!({}));
        let result = bus.emit(&event).await;

        assert!(matches!(result, HookResult::Block { .. }));
        assert_eq!(blocker.calls(), 1);
        assert_eq!(after.calls(), 0); // never reached
    }

    /// prA I1 (was `modify_stops_chain`, which pinned the bypass): a later
    /// Block still runs after a Modify, and wins.
    #[tokio::test]
    async fn modify_does_not_stop_chain_later_block_wins() {
        let modifier = TestHandler::new(
            "modifier",
            HookResult::Modify {
                input: serde_json::json!({"command": "echo safe"}),
            },
        );
        let after = TestHandler::new(
            "after",
            HookResult::Block {
                reason: "vetoed".into(),
            },
        );
        let bus = bus_with(vec![modifier.clone(), after.clone()]).await;
        let event = HookEvent::before_tool_call("bash", serde_json::json!({"command": "rm -rf /"}));
        let result = bus.emit(&event).await;
        assert!(matches!(result, HookResult::Block { reason } if reason == "vetoed"));
        assert_eq!(modifier.calls(), 1);
        assert_eq!(after.calls(), 1);
    }

    #[tokio::test]
    async fn tool_filter_only_matches_specified_tool() {
        let bus = HookBus::new();
        let handler = TestHandler::new("bash-only", HookResult::Continue);
        let perms = perms_with(&[Permission::ToolsIntercept]);

        bus.subscribe(
            HookKind::AfterToolCall,
            handler.clone(),
            Some("bash".into()),
            None,
            perms,
        )
        .await
        .unwrap();

        // Should NOT fire for 'read' tool
        let event = HookEvent::after_tool_call("read", serde_json::json!({}), "content".into());
        bus.emit(&event).await;
        assert_eq!(handler.calls(), 0);

        // SHOULD fire for 'bash' tool
        let event = HookEvent::after_tool_call("bash", serde_json::json!({}), "output".into());
        bus.emit(&event).await;
        assert_eq!(handler.calls(), 1);
    }

    /// A guard written for `edit`/`write` (from before `multi_edit`
    /// existed) still sees `multi_edit` before the call, so it can block or
    /// confirm it. Output transforms stay exact.
    #[tokio::test]
    async fn edit_and_write_filters_also_guard_multi_edit_before_the_call() {
        for filter in ["edit", "write"] {
            let bus = HookBus::new();
            let guard = TestHandler::new(
                "guard",
                HookResult::Block {
                    reason: "guarded".into(),
                },
            );
            let after = TestHandler::new("after", HookResult::Continue);
            let perms = perms_with(&[Permission::ToolsIntercept]);
            bus.subscribe(HookKind::BeforeToolCall, guard.clone(), Some(filter.into()), None, perms.clone())
                .await
                .unwrap();
            bus.subscribe(HookKind::AfterToolCall, after.clone(), Some(filter.into()), None, perms)
                .await
                .unwrap();
            let input = serde_json::json!({"edits": [{"path": "/etc/hosts", "op": "append", "text": "x"}]});
            let blocked = bus.emit(&HookEvent::before_tool_call("multi_edit", input.clone())).await;
            assert!(matches!(blocked, HookResult::Block { reason } if reason == "guarded"), "{filter}");
            assert_eq!(guard.calls(), 1, "{filter}");
            let other = bus.emit(&HookEvent::before_tool_call("read", serde_json::json!({}))).await;
            assert!(matches!(other, HookResult::Continue), "{filter}: only the file-changing tool");
            bus.emit(&HookEvent::after_tool_call("multi_edit", input, "ok".into())).await;
            assert_eq!(after.calls(), 0, "{filter}: after_tool_call filters stay exact");
        }
    }

    #[tokio::test]
    async fn permission_denied_rejects_subscribe() {
        let bus = HookBus::new();
        let handler = TestHandler::new("no-perms", HookResult::Continue);
        let perms = PermissionSet::new(); // empty — no permissions

        let result = bus
            .subscribe(HookKind::BeforeToolCall, handler, None, None, perms)
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("lacks permission"));
    }

    #[tokio::test]
    async fn unsubscribe_removes_handlers() {
        let bus = HookBus::new();
        let handler = TestHandler::new("removable", HookResult::Continue);
        let perms = perms_with(&[Permission::ToolsIntercept]);

        bus.subscribe(HookKind::BeforeToolCall, handler.clone(), None, None, perms)
            .await
            .unwrap();
        assert_eq!(bus.handler_count().await, 1);

        bus.unsubscribe_all("removable").await;
        assert_eq!(bus.handler_count().await, 0);
    }

    #[tokio::test]
    async fn subscriptions_for_lists_only_matching_extension() {
        let bus = HookBus::new();
        let alpha = TestHandler::new("alpha", HookResult::Continue);
        let beta = TestHandler::new("beta", HookResult::Continue);
        let perms = perms_with(&[Permission::ToolsIntercept]);

        bus.subscribe(
            HookKind::BeforeToolCall,
            alpha.clone(),
            Some("bash".into()),
            None,
            perms.clone(),
        )
        .await
        .unwrap();
        bus.subscribe(
            HookKind::AfterToolCall,
            alpha.clone(),
            None,
            None,
            perms.clone(),
        )
        .await
        .unwrap();
        bus.subscribe(HookKind::BeforeToolCall, beta.clone(), None, None, perms)
            .await
            .unwrap();

        let alpha_subs = bus.subscriptions_for("alpha").await;
        assert_eq!(alpha_subs.len(), 2);
        // sorted by kind name then by tool_filter (None first)
        assert_eq!(alpha_subs[0].0, HookKind::AfterToolCall);
        assert_eq!(alpha_subs[0].1, None);
        assert_eq!(alpha_subs[1].0, HookKind::BeforeToolCall);
        assert_eq!(alpha_subs[1].1, Some("bash".to_string()));

        let beta_subs = bus.subscriptions_for("beta").await;
        assert_eq!(beta_subs, vec![(HookKind::BeforeToolCall, None)]);

        let none_subs = bus.subscriptions_for("ghost").await;
        assert!(none_subs.is_empty());
    }

    #[tokio::test]
    async fn is_empty_reflects_state() {
        let bus = HookBus::new();
        assert!(bus.is_empty().await);

        let handler = TestHandler::new("ext", HookResult::Continue);
        let perms = perms_with(&[Permission::ToolsIntercept]);
        bus.subscribe(HookKind::BeforeToolCall, handler, None, None, perms)
            .await
            .unwrap();
        assert!(!bus.is_empty().await);
    }

    // ────────────────────────────────────────────────────────────────────────
    // compress-then-truncate ordering (Synaps Fork 1) — RED stub
    // ────────────────────────────────────────────────────────────────────────

    #[cfg(feature = "command-exec")]
    #[derive(Debug)]
    struct RecordingHandler {
        id: String,
        recorded_len: Arc<AtomicUsize>,
        result: HookResult,
    }
    #[cfg(feature = "command-exec")]
    impl RecordingHandler {
        fn new(id: &str, result: HookResult) -> (Arc<Self>, Arc<AtomicUsize>) {
            let recorded_len = Arc::new(AtomicUsize::new(0));
            let h = Arc::new(Self {
                id: id.to_string(),
                recorded_len: recorded_len.clone(),
                result,
            });
            (h, recorded_len)
        }
    }
    #[cfg(feature = "command-exec")]
    #[async_trait]
    impl crate::extensions::runtime::ExtensionHandler for RecordingHandler {
        fn id(&self) -> &str {
            &self.id
        }
        async fn handle(&self, event: &HookEvent) -> HookResult {
            if let Some(out) = event.tool_output.as_ref() {
                self.recorded_len.store(out.len(), Ordering::Relaxed);
            }
            self.result.clone()
        }
        async fn shutdown(&self) {}
    }

    /// Test A (RED stub, current signature) — proves the bug.
    #[cfg(feature = "command-exec")]
    #[tokio::test]
    async fn after_tool_call_transform_receives_full_output_not_pre_truncated() {
        use crate::tools::Tool;
        let bus = std::sync::Arc::new(HookBus::new());
        let (handler, recorded_len) = RecordingHandler::new("len-recorder", HookResult::Continue);
        bus.subscribe(
            HookKind::AfterToolCall,
            handler,
            None,
            None,
            perms_with(&[Permission::ToolsIntercept]),
        )
        .await
        .unwrap();

        let ctx = crate::tools::ToolContext {
            channels: crate::tools::ToolChannels {
                tx_delta: None,
                tx_events: None,
            },
            capabilities: crate::tools::ToolCapabilities {
                session_allow_all: None,
                launch_cancel: None,
                memory_backend: None,
                telemetry: None,
                tool_register_tx: None,
                session_manager: None,
                subagent_registry: None,
                event_queue: None,
                delegation_parent: None,
                codex_parent_plan: None,
                secret_prompt: None,
                orchestration: None,
                tool_activation: None,
                mcp_leases: None,
                extension_leases: None,
                cwd: None,
                env: None,
                env_stripped: Vec::new(),
                env_warned: Default::default(),
                session_id: None,
                hook_bus: None,
                continuation: None,
            },
            limits: crate::tools::ToolLimits {
                max_tool_output: 30_000,
                max_tool_buffer: 256 * 1024,
                bash_timeout: 30,
                bash_max_timeout: 300,
                subagent_timeout: 300,
            },
        };

        let bash = crate::tools::BashTool;
        let output = bash
            .execute(
                serde_json::json!({"command": "yes hello | head -c 150000"}),
                ctx,
            )
            .await
            .expect("bash should succeed");

        let _ = crate::runtime::emit_after_tool_call(
            &bus,
            "bash",
            None,
            serde_json::json!({"command": "yes hello | head -c 150000"}),
            output,
            30_000,
            None,
            None,
        )
        .await;

        let len = recorded_len.load(Ordering::Relaxed);
        assert!(
            len > 100_000,
            "after_tool_call handler was starved: received {len} bytes — \
             expected the full ~150KB buffered output (max_tool_buffer=256KB)"
        );
    }

    /// Test B: when a transform Replaces with a >max_tool_output string, the
    /// FINAL emit_after_tool_call return is ≤ max_tool_output (+marker overhead).
    /// Proves compress-then-truncate ordering at the new site.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn final_output_truncated_to_max_tool_output_after_hook() {
        // #186: Replace writes the intercept audit — isolate the base dir.
        let _base = crate::test_env::BaseDirGuard::new();
        let bus = std::sync::Arc::new(HookBus::new());
        let big: String = "x".repeat(100_000);
        let handler = TestHandler::new("bloater", HookResult::Replace { output: big });
        bus.subscribe(
            HookKind::AfterToolCall,
            handler,
            None,
            None,
            perms_with(&[Permission::ToolsIntercept, Permission::ToolsTransformOutput]),
        )
        .await
        .unwrap();

        let result = crate::runtime::emit_after_tool_call(
            &bus,
            "bash",
            None,
            serde_json::json!({}),
            "tiny original".to_string(),
            30_000,
            None,
            None,
        )
        .await;

        assert!(
            result.len() <= 30_000 + 200,
            "post-hook output should be capped at max_tool_output (got {} bytes)",
            result.len()
        );
    }

    /// Test C: with NO transform, the final output is byte-identical to the
    /// legacy truncate_tool_result(...) result — behavior preservation invariant.
    #[tokio::test]
    async fn no_transform_extension_preserves_legacy_truncation() {
        let bus = std::sync::Arc::new(HookBus::new());
        let huge: String = "y".repeat(80_000);

        let result = crate::runtime::emit_after_tool_call(
            &bus,
            "bash",
            None,
            serde_json::json!({}),
            huge.clone(),
            30_000,
            None,
            None,
        )
        .await;

        let legacy = crate::runtime::helpers::HelperMethods::truncate_tool_result(&huge, 30_000);
        assert_eq!(
            result, legacy,
            "no-extension path must be byte-identical to legacy truncation"
        );
    }

    /// Test D: emit_after_tool_call unit-level — Continue and Replace branches
    /// both truncate to max_tool_output, and truncation is char-boundary safe.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn emit_after_tool_call_truncates_char_boundary_safe() {
        // #186: Replace writes the intercept audit — isolate the base dir.
        let _base = crate::test_env::BaseDirGuard::new();
        // Continue path with multibyte content.
        let bus = std::sync::Arc::new(HookBus::new());
        let multibyte: String = "héllo🌟".repeat(5_000);
        assert!(multibyte.len() > 30_000);

        let result = crate::runtime::emit_after_tool_call(
            &bus,
            "bash",
            None,
            serde_json::json!({}),
            multibyte,
            30_000,
            None,
            None,
        )
        .await;
        assert!(result.is_char_boundary(result.len()));

        // Replace path with multibyte content.
        let bus2 = std::sync::Arc::new(HookBus::new());
        let big_multi = "✨".repeat(20_000);
        assert!(big_multi.len() > 30_000);
        let handler = TestHandler::new("mb-replacer", HookResult::Replace { output: big_multi });
        bus2.subscribe(
            HookKind::AfterToolCall,
            handler,
            None,
            None,
            perms_with(&[Permission::ToolsIntercept, Permission::ToolsTransformOutput]),
        )
        .await
        .unwrap();

        let result2 = crate::runtime::emit_after_tool_call(
            &bus2,
            "bash",
            None,
            serde_json::json!({}),
            "orig".to_string(),
            30_000,
            None,
            None,
        )
        .await;
        assert!(result2.is_char_boundary(result2.len()));
    }

    #[tokio::test]
    async fn keyed_injection_isolated_per_session() {
        let bus = HookBus::new();
        bus.set_session_injection_for("s1", "one".into()).await;
        bus.set_session_injection_for("s2", "two".into()).await;
        assert_eq!(bus.session_injection_for("s1").await.as_deref(), Some("one"));
        assert_eq!(bus.session_injection_for("s2").await.as_deref(), Some("two"));
        assert_eq!(bus.session_injection_for("s3").await, None);
    }

    #[tokio::test]
    async fn clear_removes_only_that_session() {
        let bus = HookBus::new();
        bus.set_session_injection_for("s1", "one".into()).await;
        bus.set_session_injection_for("s2", "two".into()).await;
        bus.clear_session_injection("s1").await;
        assert_eq!(bus.session_injection_for("s1").await, None);
        assert_eq!(bus.session_injection_for("s2").await.as_deref(), Some("two"));
    }

    #[tokio::test]
    #[allow(deprecated)]
    async fn deprecated_shims_use_empty_key() {
        let bus = HookBus::new();
        bus.set_session_injection("legacy".into()).await;
        assert_eq!(bus.session_injection().await.as_deref(), Some("legacy"));
        assert_eq!(bus.session_injection_for("").await.as_deref(), Some("legacy"));
        assert_eq!(bus.session_injection_for("s1").await, None);
    }
}

/// Subagent workers' hook view ([`HookBus::worker_transforms`]): only the
/// host's `after_tool_call` output transforms, live, dispatched without
/// ever (re)starting an extension.
#[cfg(test)]
mod worker_view_tests {
    use super::*;
    use crate::extensions::permissions::Permission;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Counts `handle` / `handle_no_spawn` calls separately. `result` is
    /// what both return; `tag_output` instead replaces with
    /// `"<tag>:<tool_output>"` (per-call, so concurrent callers can tell
    /// their replacements apart). `delay` sleeps first.
    struct Probe {
        id: String,
        result: HookResult,
        tag_output: Option<String>,
        delay: Duration,
        handled: AtomicUsize,
        handled_no_spawn: AtomicUsize,
    }

    impl Probe {
        fn build(id: &str, result: HookResult, tag: Option<&str>, delay: Duration) -> Arc<Self> {
            Arc::new(Self {
                id: id.into(),
                result,
                tag_output: tag.map(str::to_string),
                delay,
                handled: AtomicUsize::new(0),
                handled_no_spawn: AtomicUsize::new(0),
            })
        }

        fn new(id: &str, result: HookResult) -> Arc<Self> {
            Self::build(id, result, None, Duration::ZERO)
        }

        fn tagging(id: &str, tag: &str, delay: Duration) -> Arc<Self> {
            Self::build(id, HookResult::Continue, Some(tag), delay)
        }

        fn calls(&self) -> (usize, usize) {
            (
                self.handled.load(Ordering::SeqCst),
                self.handled_no_spawn.load(Ordering::SeqCst),
            )
        }

        async fn answer(&self, event: &HookEvent) -> HookResult {
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            match &self.tag_output {
                Some(tag) => HookResult::Replace {
                    output: format!("{tag}:{}", event.tool_output.as_deref().unwrap_or("")),
                },
                None => self.result.clone(),
            }
        }
    }

    #[async_trait]
    impl crate::extensions::runtime::ExtensionHandler for Probe {
        fn id(&self) -> &str {
            &self.id
        }
        async fn handle(&self, event: &HookEvent) -> HookResult {
            self.handled.fetch_add(1, Ordering::SeqCst);
            self.answer(event).await
        }
        async fn handle_no_spawn(&self, event: &HookEvent) -> HookResult {
            self.handled_no_spawn.fetch_add(1, Ordering::SeqCst);
            self.answer(event).await
        }
        async fn shutdown(&self) {}
    }

    fn perms(perms: &[Permission]) -> PermissionSet {
        let mut set = PermissionSet::new();
        for p in perms {
            set.grant(*p);
        }
        set
    }

    fn transform() -> PermissionSet {
        perms(&[Permission::ToolsIntercept, Permission::ToolsTransformOutput])
    }

    async fn after(bus: &Arc<HookBus>, output: &str) -> String {
        crate::runtime::emit_after_tool_call(
            bus,
            "bash",
            None,
            serde_json::json!({"command": "cat big.log"}),
            output.to_string(),
            30_000,
            None,
            None,
        )
        .await
    }

    /// The host bus a worker views: a confirm and a block on
    /// before_tool_call, an inject on before_message, an observe-only
    /// after_tool_call, and one real output transform.
    async fn host_with_every_kind() -> (Arc<HookBus>, Vec<Arc<Probe>>, Arc<Probe>) {
        let host = Arc::new(HookBus::new());
        let confirm = Probe::new(
            "confirmer",
            HookResult::Confirm {
                message: "really?".into(),
                sources: vec![],
                generations: vec![],
                tool: None,
                modified_input: None,
                modified_by: vec![],
            },
        );
        let block = Probe::new("blocker", HookResult::Block { reason: "no".into() });
        let inject = Probe::new("injector", HookResult::inject("ctx"));
        let observer = Probe::new("observer", HookResult::Replace { output: "SNEAKY".into() });
        let crush = Probe::new("crush", HookResult::Replace { output: "CRUSHED".into() });
        let intercept = perms(&[Permission::ToolsIntercept]);
        host.subscribe(HookKind::BeforeToolCall, confirm.clone(), None, None, intercept.clone())
            .await
            .unwrap();
        host.subscribe(HookKind::BeforeToolCall, block.clone(), None, None, transform())
            .await
            .unwrap();
        host.subscribe(
            HookKind::BeforeMessage,
            inject.clone(),
            None,
            None,
            perms(&[Permission::LlmContent, Permission::ToolsTransformOutput]),
        )
        .await
        .unwrap();
        // Observe-only first, so on the host it is reached before `crush`.
        host.subscribe(HookKind::AfterToolCall, observer.clone(), None, None, intercept)
            .await
            .unwrap();
        host.subscribe(HookKind::AfterToolCall, crush.clone(), None, None, transform())
            .await
            .unwrap();
        (host, vec![confirm, block, inject, observer], crush)
    }

    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn a_worker_runs_only_the_hosts_after_tool_call_transforms() {
        let _base = crate::test_env::BaseDirGuard::new();
        let (host, others, crush) = host_with_every_kind().await;
        let worker = HookBus::worker_transforms(&host);
        assert!(worker.is_worker_view() && !host.is_worker_view());

        // The worker's bash result is replaced by the transform...
        assert_eq!(after(&worker, "RAW 10k lines").await, "CRUSHED");
        assert_eq!(crush.calls(), (0, 1), "through handle_no_spawn");
        // ...and nothing else fires: no confirm prompt, no block, no inject.
        let before = crate::runtime::emit_before_tool_call(
            &worker,
            "bash",
            None,
            serde_json::json!({"command": "rm -rf /"}),
            None,
            None,
        )
        .await;
        assert_eq!(before, HookResult::Continue);
        let decision = crate::runtime::resolve_before_tool_call_decision(
            serde_json::json!({"command": "rm -rf /"}),
            before,
            None,
            false,
            None,
        )
        .await;
        assert!(matches!(decision, crate::runtime::BeforeToolCallDecision::Continue { .. }));
        for kind in [HookKind::BeforeMessage, HookKind::OnMessageComplete, HookKind::OnSessionStart] {
            let mut event = HookEvent::after_tool_call("bash", serde_json::json!({}), "x".into());
            event.kind = kind;
            assert_eq!(worker.emit(&event).await, HookResult::Continue, "{kind:?}");
        }
        for probe in &others {
            assert_eq!(probe.calls(), (0, 0), "{} never reached from a worker", probe.id);
        }
        assert_eq!(worker.handler_count().await, 1, "the one transform");
        assert!(!worker.is_empty().await);

        // The foreground (host bus) is unchanged: observer first, then crush.
        assert_eq!(after(&host, "RAW").await, "CRUSHED");
        assert_eq!(others[3].calls(), (1, 0));
        assert_eq!(crush.calls(), (1, 1));
    }

    /// Two-key gate on the worker path: an after_tool_call subscriber
    /// without `tools.transform_output` is not even dispatched, so it can
    /// neither replace nor observe a worker's output.
    #[tokio::test]
    async fn an_extension_without_transform_output_cannot_replace_a_workers_output() {
        let host = Arc::new(HookBus::new());
        let observer = Probe::new("observer", HookResult::Replace { output: "SNEAKY".into() });
        host.subscribe(
            HookKind::AfterToolCall,
            observer.clone(),
            None,
            None,
            perms(&[Permission::ToolsIntercept]),
        )
        .await
        .unwrap();
        let worker = HookBus::worker_transforms(&host);
        assert_eq!(after(&worker, "RAW").await, "RAW");
        assert_eq!(observer.calls(), (0, 0));
        assert!(worker.is_empty().await);
        // The foreground still ignores its Replace (the existing gate).
        assert_eq!(after(&host, "RAW").await, "RAW");
        assert_eq!(observer.calls(), (1, 0));
    }

    /// Fail-safe: a transform that times out (5 s) or answers `Continue`
    /// (what a crashed or not-running extension answers) leaves the
    /// original output, and the chain moves on to the next transform.
    #[tokio::test(start_paused = true)]
    #[serial_test::serial(synaps_base_dir)]
    async fn a_timed_out_or_crashed_transform_leaves_the_original_output() {
        let _base = crate::test_env::BaseDirGuard::new();
        let host = Arc::new(HookBus::new());
        let slow = Probe::tagging("slow", "SLOW", Duration::from_secs(60));
        host.subscribe(HookKind::AfterToolCall, slow.clone(), None, None, transform())
            .await
            .unwrap();
        let worker = HookBus::worker_transforms(&host);
        let started = tokio::time::Instant::now();
        assert_eq!(after(&worker, "RAW").await, "RAW", "timed out → original");
        let waited = started.elapsed();
        assert!(
            waited >= HANDLER_TIMEOUT && waited < HANDLER_TIMEOUT + Duration::from_secs(1),
            "bounded by the hook timeout: {waited:?}"
        );

        let crashed = Probe::new("crashed", HookResult::Continue);
        let next = Probe::tagging("next", "NEXT", Duration::ZERO);
        let host = Arc::new(HookBus::new());
        host.subscribe(HookKind::AfterToolCall, crashed.clone(), None, None, transform())
            .await
            .unwrap();
        let worker = HookBus::worker_transforms(&host);
        assert_eq!(after(&worker, "RAW").await, "RAW", "crashed → original");
        host.subscribe(HookKind::AfterToolCall, next.clone(), None, None, transform())
            .await
            .unwrap();
        assert_eq!(after(&worker, "RAW").await, "NEXT:RAW", "the chain continues");
    }

    /// Live view, not a snapshot: a worker created before an extension
    /// loads, reloads or unloads sees each change on its next call, and
    /// never calls a stopped instance.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn a_worker_sees_extension_loads_reloads_and_unloads_live() {
        let _base = crate::test_env::BaseDirGuard::new();
        let host = Arc::new(HookBus::new());
        let worker = HookBus::worker_transforms(&host);
        assert_eq!(after(&worker, "RAW").await, "RAW", "nothing loaded yet");

        let v1 = Probe::tagging("crush", "V1", Duration::ZERO);
        host.subscribe(HookKind::AfterToolCall, v1.clone(), None, None, transform())
            .await
            .unwrap();
        assert_eq!(after(&worker, "RAW").await, "V1:RAW", "loaded after the worker started");

        // Reload = unsubscribe_all + subscribe the new instance.
        host.unsubscribe_all("crush").await;
        assert_eq!(after(&worker, "RAW").await, "RAW", "mid-reload: pass-through");
        let v2 = Probe::tagging("crush", "V2", Duration::ZERO);
        host.subscribe(HookKind::AfterToolCall, v2.clone(), None, None, transform())
            .await
            .unwrap();
        assert_eq!(after(&worker, "RAW").await, "V2:RAW");
        assert_eq!(v1.calls(), (0, 1), "the stopped instance is never called again");

        host.unsubscribe_all("crush").await;
        assert_eq!(after(&worker, "RAW").await, "RAW", "unloaded");
    }

    /// Parallel workers, each on its own OS thread and current-thread
    /// runtime (as subagents run), share one handler and each get the
    /// replacement of their own output.
    #[test]
    #[serial_test::serial(synaps_base_dir)]
    fn parallel_workers_each_get_their_own_replacement() {
        let _base = crate::test_env::BaseDirGuard::new();
        let crush = Probe::tagging("crush", "CRUSHED", Duration::from_millis(2));
        let host = Arc::new(HookBus::new());
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(host.subscribe(HookKind::AfterToolCall, crush.clone(), None, None, transform()))
            .unwrap();
        let threads: Vec<_> = (0..8)
            .map(|w| {
                let worker = HookBus::worker_transforms(&host);
                std::thread::spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    rt.block_on(async {
                        for call in 0..10 {
                            let output = format!("worker {w} call {call}");
                            assert_eq!(after(&worker, &output).await, format!("CRUSHED:{output}"));
                        }
                    });
                })
            })
            .collect();
        for t in threads {
            t.join().expect("worker thread");
        }
        assert_eq!(crush.calls(), (0, 80));
    }

    #[tokio::test]
    async fn a_view_is_read_only_and_a_view_of_a_view_reads_the_host() {
        let host = Arc::new(HookBus::new());
        let worker = HookBus::worker_transforms(&host);
        let probe = Probe::tagging("crush", "C", Duration::ZERO);
        let err = worker
            .subscribe(HookKind::AfterToolCall, probe.clone(), None, None, transform())
            .await
            .unwrap_err();
        assert!(err.contains("worker"), "{err}");
        let nested = HookBus::worker_transforms(&worker);
        assert!(nested.is_worker_view());
        assert!(Arc::ptr_eq(nested.transforms_of.as_ref().unwrap(), &host));
        assert_eq!(nested.handler_count().await, 0);
    }

    /// #414: two extensions inject; the merged result carries each one's
    /// parts in handler order with the host-known id stamped on (never the
    /// wire's), and `content` still joins every `content` as before.
    #[tokio::test]
    async fn before_message_injections_keep_parts_and_host_stamped_sources() {
        let bus = HookBus::new();
        let a: HookResult = serde_json::from_value(serde_json::json!({
            "action": "inject", "content": "fallback-a",
            "parts": [{"content": "A1", "source": "b"}, {"content": "A2", "forget": true}]
        }))
        .unwrap();
        let b: HookResult = serde_json::from_value(
            serde_json::json!({"action": "inject", "content": "B", "forget": true}),
        )
        .unwrap();
        for (id, result) in [("ext-a", a), ("ext-b", b)] {
            bus.subscribe(
                HookKind::BeforeMessage,
                Probe::new(id, result),
                None,
                None,
                perms(&[Permission::LlmContent]),
            )
            .await
            .unwrap();
        }
        let merged = bus.emit(&HookEvent::before_message("hi")).await;
        let HookResult::Inject { content, parts, .. } = &merged else {
            panic!("expected Inject, got {merged:?}");
        };
        assert_eq!(content, "fallback-a\n\nB");
        assert_eq!(
            parts,
            &vec![
                events::InjectPart::saved("A1").from_source("ext-a"),
                events::InjectPart::forget("A2").from_source("ext-a"),
                events::InjectPart::forget("B").from_source("ext-b"),
            ]
        );
        assert_eq!(merged.inject_parts(), *parts);
    }
}

/// A hook subscriber for tests of code that sends notify-only events.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Records the events it is sent and counts the calls it was entered
    /// for and the ones it answered. `delay` is slept first
    /// (`Duration::MAX`: never answers); its first `panics` calls panic
    /// instead of answering.
    pub(crate) struct Listener {
        pub(crate) id: String,
        delay: Duration,
        panics: usize,
        result: HookResult,
        seen: std::sync::Mutex<Vec<HookEvent>>,
        answered: AtomicUsize,
    }

    impl Listener {
        fn build(id: &str, delay: Duration, panics: usize, result: HookResult) -> Arc<Self> {
            Arc::new(Self {
                id: id.into(),
                delay,
                panics,
                result,
                seen: Default::default(),
                answered: AtomicUsize::new(0),
            })
        }
        pub(crate) fn answering(id: &str, result: HookResult) -> Arc<Self> {
            Self::build(id, Duration::ZERO, 0, result)
        }
        pub(crate) fn prompt(id: &str) -> Arc<Self> {
            Self::answering(id, HookResult::Continue)
        }
        pub(crate) fn slow(id: &str, delay: Duration) -> Arc<Self> {
            Self::build(id, delay, 0, HookResult::Continue)
        }
        pub(crate) fn hanging(id: &str) -> Arc<Self> {
            Self::slow(id, Duration::MAX)
        }
        pub(crate) fn panicking(id: &str) -> Arc<Self> {
            Self::build(id, Duration::ZERO, usize::MAX, HookResult::Continue)
        }
        /// Panics on the first event it is sent and answers every later one.
        pub(crate) fn panicking_once(id: &str) -> Arc<Self> {
            Self::build(id, Duration::ZERO, 1, HookResult::Continue)
        }

        /// Subscribe to `kind` on `bus`, holding only the permission that
        /// kind requires.
        pub(crate) async fn subscribe(self: &Arc<Self>, bus: &HookBus, kind: HookKind) {
            let mut permissions = PermissionSet::new();
            permissions.grant(kind.required_permission());
            bus.subscribe(kind, self.clone(), None, None, permissions)
                .await
                .unwrap();
        }

        /// Every event this listener was entered for, in arrival order.
        pub(crate) fn events(&self) -> Vec<HookEvent> {
            self.seen.lock().unwrap().clone()
        }

        /// (entered, answered)
        pub(crate) fn calls(&self) -> (usize, usize) {
            (self.seen.lock().unwrap().len(), self.answered.load(Ordering::SeqCst))
        }

        /// The first `count` events, once that many have arrived. Deliveries
        /// are detached, so a test has to wait for them; 30 s without them
        /// is a failure.
        pub(crate) async fn wait_for(&self, count: usize) -> Vec<HookEvent> {
            let waited = tokio::time::timeout(Duration::from_secs(30), async {
                while self.seen.lock().unwrap().len() < count {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await;
            let events = self.events();
            assert!(waited.is_ok(), "{}: expected {count} events, got {events:?}", self.id);
            events
        }
    }

    #[async_trait]
    impl crate::extensions::runtime::ExtensionHandler for Listener {
        fn id(&self) -> &str {
            &self.id
        }
        async fn handle(&self, event: &HookEvent) -> HookResult {
            let call = {
                let mut seen = self.seen.lock().unwrap();
                seen.push(event.clone());
                seen.len()
            };
            if self.delay == Duration::MAX {
                std::future::pending::<()>().await;
            }
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            if call <= self.panics {
                panic!("notify test: this handler panics on purpose");
            }
            self.answered.fetch_add(1, Ordering::SeqCst);
            self.result.clone()
        }
        async fn shutdown(&self) {}
    }
}

/// [`HookBus::notify`]: fire and forget. The caller never waits, every
/// answer is discarded, and the detached delivery is bounded in time
/// ([`HANDLER_TIMEOUT`]) and in number ([`NOTIFY_MAX_IN_FLIGHT`]). Time is
/// Tokio's paused clock, so "did not wait" is exact and cannot flake.
#[cfg(test)]
mod notify_tests {
    use super::test_support::Listener;
    use super::*;
    use crate::extensions::permissions::Permission;
    use std::sync::atomic::{AtomicBool, Ordering};

    const KIND: HookKind = HookKind::OnContextArchived;

    fn event() -> HookEvent {
        HookEvent::on_context_archived("chat", &"a".repeat(32), None, "p0", 1, 1, 2)
    }

    async fn bus_with(listeners: &[&Arc<Listener>]) -> Arc<HookBus> {
        let bus = Arc::new(HookBus::new());
        for listener in listeners {
            listener.subscribe(&bus, KIND).await;
        }
        bus
    }

    /// Let the detached deliveries run, without reaching any handler timer.
    async fn settle() {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    #[tokio::test(start_paused = true)]
    async fn notify_returns_before_a_hanging_handler() {
        let slow = Listener::slow("slow", Duration::from_secs(60));
        let bus = bus_with(&[&slow]).await;
        let (virtual_start, real_start) = (tokio::time::Instant::now(), Instant::now());
        bus.notify(event());
        // Not one tick of the paused clock: nothing was awaited. The real
        // bound is loose on purpose; waiting would cost the 5 s timeout.
        assert_eq!(virtual_start.elapsed(), Duration::ZERO);
        assert!(real_start.elapsed() < Duration::from_secs(1));
        assert_eq!(slow.calls(), (0, 0), "the handler runs on the detached task");
        assert_eq!(bus.notifies_in_flight(), 1);

        // The delivery is still waiting just before the handler timeout...
        tokio::time::sleep(HANDLER_TIMEOUT - Duration::from_millis(1)).await;
        assert_eq!(slow.calls(), (1, 0));
        assert_eq!(bus.notifies_in_flight(), 1);
        // ...and gone just after it: the task ended, the handler never answered.
        tokio::time::sleep(Duration::from_millis(2)).await;
        assert_eq!(bus.notifies_in_flight(), 0);
        assert_eq!(slow.calls(), (1, 0));
    }

    /// The panicking handler is first in line: its panic used to end the
    /// task before the healthy one was called at all.
    #[tokio::test(start_paused = true)]
    async fn notify_survives_a_panicking_handler() {
        let (bad, good) = (Listener::panicking("bad"), Listener::prompt("good"));
        let bus = bus_with(&[&bad, &good]).await;
        bus.notify(event());
        settle().await;
        assert_eq!(bad.calls(), (1, 0));
        assert_eq!(good.calls(), (1, 1), "the healthy listener got the event");
        assert_eq!(bus.notifies_in_flight(), 0, "the panic released its slot");
        // The bus still delivers, here and on its awaited paths.
        bus.unsubscribe_all("bad").await;
        bus.notify(event());
        settle().await;
        assert!(good.calls().1 >= 1, "{:?}", good.calls());
        assert_eq!(bus.emit(&HookEvent::before_message("hi")).await, HookResult::Continue);
        assert_eq!(bus.notifies_in_flight(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn notify_ignores_block_and_inject_results() {
        let listeners = [
            Listener::answering("block", HookResult::Block { reason: "no".into() }),
            Listener::answering("inject", HookResult::inject("ctx")),
            Listener::answering("replace", HookResult::Replace { output: "x".into() }),
            Listener::answering("confirm", HookResult::confirm("sure?")),
            Listener::answering("phase", HookResult::ContextPhase { phase: "new_task".into() }),
            Listener::prompt("plain"),
        ];
        let bus = bus_with(&listeners.iter().collect::<Vec<_>>()).await;
        bus.notify(event());
        settle().await;
        // Nobody's answer stopped the others, and none had any effect.
        for listener in &listeners {
            assert_eq!(listener.calls(), (1, 1), "{}", listener.id);
        }
        assert_eq!(bus.session_injection_for("chat").await, None);
        assert_eq!(bus.notifies_in_flight(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn notify_with_no_subscribers_is_a_no_op() {
        // A subscriber to another kind only.
        let other = Listener::prompt("other");
        let bus = Arc::new(HookBus::new());
        other.subscribe(&bus, HookKind::BeforeMessage).await;
        bus.notify(event());
        assert_eq!(bus.notifies_in_flight(), 0, "no task was spawned");
        // An event that costs something to build is not even built.
        let built = Arc::new(AtomicBool::new(false));
        let flag = built.clone();
        bus.notify_with(KIND, async move {
            flag.store(true, Ordering::SeqCst);
            Some(event())
        });
        assert_eq!(bus.notifies_in_flight(), 0);
        settle().await;
        assert!(!built.load(Ordering::SeqCst));
        assert_eq!(other.calls(), (0, 0));
    }

    #[tokio::test(start_paused = true)]
    async fn notify_on_worker_view_sends_nothing() {
        let listener = Listener::prompt("listener");
        let host = bus_with(&[&listener]).await;
        let worker = HookBus::worker_transforms(&host);
        worker.notify(event());
        settle().await;
        assert_eq!(listener.calls(), (0, 0));
        assert_eq!((worker.notifies_in_flight(), host.notifies_in_flight()), (0, 0));
        // The same event on the host bus is delivered.
        host.notify(event());
        settle().await;
        assert_eq!(listener.calls(), (1, 1));
    }

    #[tokio::test(start_paused = true)]
    async fn notify_reaches_every_subscriber_concurrently() {
        // The slow one is first in line; `emit` would make the fast one wait.
        let slow = Listener::slow("slow", Duration::from_secs(3));
        let fast = Listener::prompt("fast");
        let bus = bus_with(&[&slow, &fast]).await;
        bus.notify(event());
        settle().await;
        assert_eq!(fast.calls(), (1, 1), "answered without waiting for the slow one");
        assert_eq!(slow.calls(), (1, 0));
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(slow.calls(), (1, 1));
        assert_eq!(bus.notifies_in_flight(), 0);
    }

    /// Handlers that never answer while events keep coming: at most
    /// `NOTIFY_MAX_IN_FLIGHT` deliveries exist, the rest are dropped rather
    /// than queued, and the bus delivers again once the timeout clears them.
    #[tokio::test(start_paused = true)]
    async fn notify_drops_events_beyond_the_in_flight_limit() {
        let stuck = Listener::hanging("stuck");
        let bus = bus_with(&[&stuck]).await;
        for _ in 0..NOTIFY_MAX_IN_FLIGHT + 10 {
            bus.notify(event());
        }
        assert_eq!(bus.notifies_in_flight(), NOTIFY_MAX_IN_FLIGHT);
        settle().await;
        assert_eq!(stuck.calls(), (NOTIFY_MAX_IN_FLIGHT, 0), "the extra ten were dropped");
        tokio::time::sleep(HANDLER_TIMEOUT).await;
        assert_eq!(bus.notifies_in_flight(), 0, "every delivery ended at the timeout");
        bus.notify(event());
        settle().await;
        assert_eq!(stuck.calls(), (NOTIFY_MAX_IN_FLIGHT + 1, 0));
    }

    /// An event built on the detached task: the caller does not wait for the
    /// build either, and a build that yields nothing sends nothing.
    #[tokio::test(start_paused = true)]
    async fn notify_with_builds_the_event_off_the_callers_path() {
        let listener = Listener::prompt("listener");
        let bus = bus_with(&[&listener]).await;
        let started = tokio::time::Instant::now();
        bus.notify_with(KIND, async {
            tokio::time::sleep(Duration::from_secs(10)).await;
            Some(event())
        });
        assert_eq!(started.elapsed(), Duration::ZERO);
        settle().await;
        assert_eq!(listener.calls(), (0, 0), "still being built");
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert_eq!(listener.calls(), (1, 1));

        bus.notify_with(KIND, async { None });
        // An event of another kind than the one announced is not sent.
        bus.notify_with(KIND, async { Some(HookEvent::on_session_start("chat")) });
        settle().await;
        assert_eq!(listener.calls(), (1, 1));
        assert_eq!(bus.notifies_in_flight(), 0);
    }

    /// Several events from one build are one delivery: one slot, sent to a
    /// handler in order. More of them than the in-flight limit lose none.
    #[tokio::test(start_paused = true)]
    async fn notify_with_sends_several_events_in_order_as_one_delivery() {
        let numbered = |n: usize| part(n, NOTIFY_MAX_IN_FLIGHT + 6);
        let listener = Listener::prompt("listener");
        let bus = bus_with(&[&listener]).await;
        let count = NOTIFY_MAX_IN_FLIGHT + 6;
        bus.notify_with(KIND, async move {
            let mut events: Vec<_> = (1..=count).map(numbered).collect();
            // One of another kind in the middle is skipped, not a stop.
            events.insert(3, HookEvent::on_session_start("chat"));
            events
        });
        assert_eq!(bus.notifies_in_flight(), 1);
        settle().await;
        assert_eq!(parts_seen(&listener), (1..=count as u64).collect::<Vec<_>>());
        assert_eq!(bus.notifies_in_flight(), 0);

        // A handler that never answers: the delivery ends at the first
        // event's timeout, and the second is not sent.
        let hung = Listener::hanging("hung");
        let bus = bus_with(&[&hung]).await;
        bus.notify_with(KIND, async move { vec![numbered(1), numbered(2)] });
        settle().await;
        assert_eq!(parts_seen(&hung), [1]);
        assert_eq!(bus.notifies_in_flight(), 1);
        tokio::time::sleep(HANDLER_TIMEOUT).await;
        settle().await;
        assert_eq!(bus.notifies_in_flight(), 0);
        assert_eq!(hung.calls(), (1, 0));
    }

    fn part(n: usize, of: usize) -> HookEvent {
        event().with_part(n, of, 0, 0)
    }

    fn parts_seen(listener: &Listener) -> Vec<u64> {
        let events = listener.events();
        events.iter().map(|event| event.data["part"].as_u64().unwrap()).collect()
    }

    /// Two handlers, the first of which never answers. The healthy one used
    /// to get each later part only when the stuck one's 5 s for the part
    /// before had run out, so the last of `n` parts up to 5·(n-1) s late. It
    /// now has them all at once, in order.
    ///
    /// The stuck one is sent the first part and no other. It used to be sent
    /// the next at each timeout, so an extension that was only slow had `n`
    /// requests in its input after 5·n s and was busy for `n` times its own
    /// time, with every other hook sent to it (a `before_tool_call`) timing
    /// out meanwhile. Its backlog is now one event whatever `n` is, and the
    /// delivery's slot is free after one timeout.
    #[tokio::test(start_paused = true)]
    async fn a_stuck_handler_does_not_delay_another_handlers_parts() {
        const PARTS: usize = 4;
        let (stuck, healthy) = (Listener::hanging("stuck"), Listener::prompt("healthy"));
        let bus = bus_with(&[&stuck, &healthy]).await;
        bus.notify_with(KIND, async { (1..=PARTS).map(|n| part(n, PARTS)).collect::<Vec<_>>() });
        settle().await;
        assert_eq!(parts_seen(&healthy), [1, 2, 3, 4], "none waited for the stuck handler");
        assert_eq!(healthy.calls(), (PARTS, PARTS));
        assert_eq!(parts_seen(&stuck), [1]);
        assert_eq!(bus.notifies_in_flight(), 1);

        // The stuck handler's sequence ends at its first timeout, and the
        // delivery with it: parts 2 to 4 are never sent to it.
        tokio::time::sleep(HANDLER_TIMEOUT).await;
        settle().await;
        assert_eq!(bus.notifies_in_flight(), 0, "one timeout, not one per part");
        assert_eq!(parts_seen(&stuck), [1]);
        tokio::time::sleep(HANDLER_TIMEOUT * PARTS as u32).await;
        assert_eq!((stuck.calls(), healthy.calls()), ((1, 0), (PARTS, PARTS)));

        // A handler that is slow but answers in time is sent every part.
        let slow = Listener::slow("slow", HANDLER_TIMEOUT - Duration::from_millis(1));
        let bus = bus_with(&[&slow]).await;
        bus.notify_with(KIND, async { (1..=PARTS).map(|n| part(n, PARTS)).collect::<Vec<_>>() });
        tokio::time::sleep(HANDLER_TIMEOUT * PARTS as u32).await;
        assert_eq!(slow.calls(), (PARTS, PARTS));
        assert_eq!(bus.notifies_in_flight(), 0);
    }

    /// A handler that panics on part 1 of several. It used to end the
    /// delivery task: no handler got parts 2 and 3, and the one behind it in
    /// line did not get part 1 either. The panic now costs that handler that
    /// event.
    #[tokio::test(start_paused = true)]
    async fn a_panic_on_the_first_part_loses_only_that_part_for_that_handler() {
        let (bad, good) = (Listener::panicking_once("bad"), Listener::prompt("good"));
        let bus = bus_with(&[&bad, &good]).await;
        bus.notify_with(KIND, async { (1..=3).map(|n| part(n, 3)).collect::<Vec<_>>() });
        settle().await;
        assert_eq!(parts_seen(&good), [1, 2, 3]);
        assert_eq!(good.calls(), (3, 3));
        assert_eq!(parts_seen(&bad), [1, 2, 3], "it is sent the parts after the panic");
        assert_eq!(bad.calls(), (3, 2), "and answers them");
        assert_eq!(bus.notifies_in_flight(), 0);

        // A handler that panics on every part costs the other nothing.
        let (bad, good) = (Listener::panicking("bad"), Listener::prompt("good"));
        let bus = bus_with(&[&bad, &good]).await;
        bus.notify_with(KIND, async { (1..=3).map(|n| part(n, 3)).collect::<Vec<_>>() });
        settle().await;
        assert_eq!((parts_seen(&good), good.calls()), (vec![1, 2, 3], (3, 3)));
        assert_eq!(bad.calls(), (3, 0));
        assert_eq!(bus.notifies_in_flight(), 0);
    }

    /// A hook whose answer matters cannot be sent this way.
    #[tokio::test(start_paused = true)]
    async fn notify_refuses_a_hook_that_is_not_notify_only() {
        let listener = Listener::answering("guard", HookResult::Block { reason: "no".into() });
        let bus = Arc::new(HookBus::new());
        listener.subscribe(&bus, HookKind::BeforeToolCall).await;
        bus.notify(HookEvent::before_tool_call("bash", serde_json::json!({})));
        settle().await;
        assert_eq!(listener.calls(), (0, 0));
        assert_eq!(bus.notifies_in_flight(), 0);
    }

    /// No runtime to deliver on (a synchronous caller): nothing is sent and
    /// nothing panics.
    #[test]
    fn notify_outside_a_runtime_sends_nothing() {
        let listener = Listener::prompt("listener");
        let bus = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(bus_with(&[&listener]));
        bus.notify(event());
        assert_eq!(bus.notifies_in_flight(), 0);
        assert_eq!(listener.calls(), (0, 0));
    }

    /// The path leads to conversation content: `privacy.llm_content`.
    #[tokio::test]
    async fn on_context_archived_needs_llm_content_to_subscribe() {
        let bus = HookBus::new();
        let mut perms = PermissionSet::new();
        perms.grant(Permission::SessionLifecycle);
        let err = bus
            .subscribe(KIND, Listener::prompt("nosy"), None, None, perms.clone())
            .await
            .unwrap_err();
        assert!(err.contains("privacy.llm_content"), "{err}");
        perms.grant(Permission::LlmContent);
        bus.subscribe(KIND, Listener::prompt("ok"), None, None, perms)
            .await
            .unwrap();
    }

    /// Ids only, no content: `session.lifecycle`, and nothing more.
    #[tokio::test(start_paused = true)]
    async fn on_memory_forgotten_needs_session_lifecycle_and_is_delivered_by_notify() {
        let forgotten = HookKind::OnMemoryForgotten;
        let bus = Arc::new(HookBus::new());
        let mut perms = PermissionSet::new();
        perms.grant(Permission::LlmContent);
        let err = bus
            .subscribe(forgotten, Listener::prompt("nosy"), None, None, perms)
            .await
            .unwrap_err();
        assert!(err.contains("session.lifecycle"), "{err}");

        let listener = Listener::prompt("indexer");
        listener.subscribe(&bus, forgotten).await;
        bus.notify(HookEvent::on_memory_forgotten(Some("chat"), "mem-1", "note", "p0", "repository"));
        settle().await;
        assert_eq!(listener.calls(), (1, 1));
        assert_eq!(listener.events()[0].data["id"], "mem-1");
    }
}
