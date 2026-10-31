//! HookBus — the central dispatcher for extension hooks.
//!
//! The HookBus holds registered handlers and dispatches typed events to them.
//! Without any handlers, `emit()` is a no-op fast path (<1µs).
//!
//! Tool-specific hooks filter by tool name before dispatching.

pub mod events;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::future::join_all;
use tokio::sync::RwLock;

use self::events::{HookEvent, HookKind, HookResult};
use crate::extensions::manifest::HookMatcher;
use crate::extensions::permissions::{Permission, PermissionSet};

/// Default timeout for a single hook handler call.
const HANDLER_TIMEOUT: Duration = Duration::from_secs(5);

fn extensions_trace_enabled() -> bool {
    std::env::var("SYNAPS_EXTENSIONS_TRACE")
        .map(|value| {
            let normalized = value.trim().to_ascii_lowercase();
            matches!(normalized.as_str(), "1" | "true" | "yes" | "on")
        })
        .unwrap_or(false)
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
    /// `Some(host)`: this bus is a subagent worker's view of `host`
    /// ([`Self::worker_transforms`]) and has no handlers of its own.
    transforms_of: Option<Arc<HookBus>>,
}

impl HookBus {
    /// Create an empty HookBus with no handlers.
    pub fn new() -> Self {
        Self {
            handlers: RwLock::new(HashMap::new()),
            session_injection: RwLock::new(HashMap::new()),
            generations: std::sync::Mutex::new(HashMap::new()),
            transforms_of: None,
        }
    }

    /// The hook bus a subagent worker runs with: a read-only view of
    /// `host`'s `after_tool_call` subscriptions from extensions that hold
    /// `tools.transform_output`, so output transforms (compression,
    /// redaction) see worker tool results too. Nothing else fires in a
    /// worker: no `before_tool_call` (no confirm prompt, block or modify),
    /// no message or session hooks, no observe-only `after_tool_call`.
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
        self.session_injection
            .write()
            .await
            .insert(session_id.to_string(), content);
    }

    /// Context injected at session start for `session_id`, if any. Read on
    /// every turn while composing the system prompt.
    pub async fn session_injection_for(&self, session_id: &str) -> Option<String> {
        self.session_injection.read().await.get(session_id).cloned()
    }

    /// Drop the injection recorded for `session_id` (session teardown).
    pub async fn clear_session_injection(&self, session_id: &str) {
        self.session_injection.write().await.remove(session_id);
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
    /// `handle_no_spawn`; every other kind returns `Continue` at once.
    pub async fn emit(&self, event: &HookEvent) -> HookResult {
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

        // Collect injections from all handlers rather than returning on first
        let mut injections: Vec<String> = Vec::new();
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

        for reg in &registrations {
            let ev: &HookEvent = modified_event.as_ref().unwrap_or(event);
            // Tool-specific filter: skip handlers that don't match.
            // Check both API name and runtime name so MCP tools with
            // sanitized names (slashes→underscores) still match.
            if let Some(ref filter) = reg.tool_filter {
                let matches = match (&ev.tool_name, &ev.tool_runtime_name) {
                    (Some(api), Some(runtime)) => filter == api || filter == runtime,
                    (Some(api), None) => filter == api,
                    (None, Some(runtime)) => filter == runtime,
                    (None, None) => false,
                };
                if !matches {
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
            let call = async {
                if no_spawn {
                    handler.handle_no_spawn(&event_clone).await
                } else {
                    handler.handle(&event_clone).await
                }
            };
            let result = tokio::time::timeout(HANDLER_TIMEOUT, call).await;

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
                Ok(HookResult::Inject { content }) => {
                    tracing::debug!(
                        hook = %ev.kind.as_str(),
                        extension = %reg.handler.id(),
                        len = content.len(),
                        "Extension injected context"
                    );
                    // Accumulate — don't early-return. Multiple extensions can inject.
                    injections.push(content);
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
        if !injections.is_empty() {
            HookResult::Inject {
                content: injections.join("\n\n"),
            }
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
                    if let Some(ref filter) = reg.tool_filter {
                        match (&event.tool_name, &event.tool_runtime_name) {
                            (Some(api), Some(runtime)) => filter == api || filter == runtime,
                            (Some(api), None) => filter == api,
                            (None, Some(runtime)) => filter == runtime,
                            (None, None) => false,
                        }
                    } else {
                        true
                    }
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

        let mut injections: Vec<String> = Vec::new();
        // First accepted report in completion order (registration order for
        // the collected results); advisory only, never stops the chain.
        let mut context_phase: Option<String> = None;
        for (reg, result) in results {
            match result {
                Ok(HookResult::Continue) => {}
                Ok(HookResult::Block { reason }) => {
                    return HookResult::Block { reason };
                }
                Ok(HookResult::Inject { content }) => {
                    injections.push(content);
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

        if !injections.is_empty() {
            HookResult::Inject {
                content: injections.join("\n\n"),
            }
        } else if let Some(phase) = context_phase {
            HookResult::ContextPhase { phase }
        } else {
            HookResult::Continue
        }
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
            hook_result_action(&HookResult::Inject {
                content: "context".into(),
            }),
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
                watcher_exit_path: None,
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
                memory_context: None,
                cwd: None,
                env: None,
                env_stripped: Vec::new(),
                env_warned: Default::default(),
                session_id: None,
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
        let inject = Probe::new("injector", HookResult::Inject { content: "ctx".into() });
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
}
