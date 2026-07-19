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
    }
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
}

impl HookBus {
    /// Create an empty HookBus with no handlers.
    pub fn new() -> Self {
        Self {
            handlers: RwLock::new(HashMap::new()),
        }
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
    /// If no handlers are registered for this hook, returns immediately
    /// (the no-extensions fast path).
    pub async fn emit(&self, event: &HookEvent) -> HookResult {
        // Snapshot the handler list and drop the lock immediately.
        // This prevents holding the RwLock across async handler calls
        // (which could block subscribe/unsubscribe for the entire
        // duration of IPC round-trips to extension processes).
        let registrations = {
            let handlers = self.handlers.read().await;
            match handlers.get(&event.kind) {
                Some(regs) if !regs.is_empty() => regs.clone(),
                _ => return HookResult::Continue, // fast path: no handlers
            }
        }; // lock dropped here

        // Collect injections from all handlers rather than returning on first
        let mut injections: Vec<String> = Vec::new();

        for reg in &registrations {
            // Tool-specific filter: skip handlers that don't match.
            // Check both API name and runtime name so MCP tools with
            // sanitized names (slashes→underscores) still match.
            if let Some(ref filter) = reg.tool_filter {
                let matches = match (&event.tool_name, &event.tool_runtime_name) {
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
                if !matcher.matches(event) {
                    continue;
                }
            }

            // Call handler with timeout
            let handler = reg.handler.clone();
            let event_clone = event.clone();
            let trace_enabled = extensions_trace_enabled();
            let started_at = trace_enabled.then(Instant::now);
            let result = tokio::time::timeout(HANDLER_TIMEOUT, handler.handle(&event_clone)).await;

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
                            hook = %event.kind.as_str(),
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
                            hook = %event.kind.as_str(),
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
                Ok(result) if !event.kind.allows_result(&result) => {
                    tracing::warn!(
                        hook = %event.kind.as_str(),
                        extension = %reg.handler.id(),
                        action = hook_result_action(&result),
                        "Extension returned action not allowed for hook — ignoring"
                    );
                    continue;
                }
                Ok(HookResult::Block { reason }) => {
                    tracing::info!(
                        hook = %event.kind.as_str(),
                        extension = %reg.handler.id(),
                        reason = %reason,
                        "Hook blocked by extension"
                    );
                    return HookResult::Block { reason };
                }
                Ok(HookResult::Continue) => {}
                Ok(HookResult::Inject { content }) => {
                    tracing::debug!(
                        hook = %event.kind.as_str(),
                        extension = %reg.handler.id(),
                        len = content.len(),
                        "Extension injected context"
                    );
                    // Accumulate — don't early-return. Multiple extensions can inject.
                    injections.push(content);
                }
                Ok(HookResult::Modify { input }) => {
                    tracing::info!(
                        hook = %event.kind.as_str(),
                        extension = %reg.handler.id(),
                        "Hook modified tool input by extension"
                    );
                    return HookResult::Modify { input };
                }
                Ok(HookResult::Replace { output }) => {
                    // Two-key gate: subscribing to after_tool_call needs
                    // `tools.intercept` (observe); rewriting the output
                    // additionally needs `tools.transform_output`. Without it,
                    // ignore this handler's Replace (fail-safe: original output
                    // preserved) and continue the chain.
                    if !reg.permissions.has(Permission::ToolsTransformOutput) {
                        tracing::warn!(
                            hook = %event.kind.as_str(),
                            extension = %reg.handler.id(),
                            "Extension returned Replace without tools.transform_output permission — ignoring transform"
                        );
                        continue;
                    }
                    let observed_len = event.tool_output.as_ref().map_or(0, String::len);
                    tracing::info!(
                        hook = %event.kind.as_str(),
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
                        event.kind.as_str(),
                        event.tool_name.clone(),
                        observed_len,
                        output.len(),
                    );
                    // First transform wins (mirrors Modify). Chaining is a future enhancement.
                    return HookResult::Replace { output };
                }
                Ok(HookResult::Confirm { message }) => {
                    tracing::info!(
                        hook = %event.kind.as_str(),
                        extension = %reg.handler.id(),
                        "Hook requested confirmation by extension"
                    );
                    return HookResult::Confirm { message };
                }
                Err(_timeout) => {
                    tracing::warn!(
                        hook = %event.kind.as_str(),
                        extension = %reg.handler.id(),
                        timeout_secs = HANDLER_TIMEOUT.as_secs(),
                        "Hook handler timed out — skipping"
                    );
                    // Fail-open: timeout = continue
                }
            }
        }

        // Merge accumulated injections from all handlers
        if !injections.is_empty() {
            HookResult::Inject {
                content: injections.join("\n\n"),
            }
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
        debug_assert!(
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
                .filter(|reg| reg.matcher.as_ref().map_or(true, |m| m.matches(event)))
                .map(|reg| {
                    let handler = reg.handler.clone();
                    let event_clone = event.clone();
                    async move {
                        tokio::time::timeout(HANDLER_TIMEOUT, handler.handle(&event_clone)).await
                    }
                })
                .collect();

        let results = join_all(futures).await;

        let mut injections: Vec<String> = Vec::new();
        for result in results {
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
                Ok(HookResult::Confirm { message }) => {
                    return HookResult::Confirm { message };
                }
                Err(_timeout) => {
                    tracing::warn!(
                        hook = %event.kind.as_str(),
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
        } else {
            HookResult::Continue
        }
    }

    /// Remove all handlers for a given extension ID.
    pub async fn unsubscribe_all(&self, extension_id: &str) {
        let mut handlers = self.handlers.write().await;
        for regs in handlers.values_mut() {
            regs.retain(|r| r.handler.id() != extension_id);
        }
    }

    /// Number of registered handlers across all hooks.
    pub async fn handler_count(&self) -> usize {
        let handlers = self.handlers.read().await;
        handlers.values().map(|v| v.len()).sum()
    }

    /// Check if any handlers are registered (for fast-path decisions).
    pub async fn is_empty(&self) -> bool {
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
    async fn after_tool_call_replace_substitutes_recorded_output() {
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
        )
        .await;

        assert_eq!(recorded, "COMPRESSED", "Replace output must reach history");
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
        )
        .await;

        assert_eq!(recorded, "RAW", "no Replace → original output preserved");
    }

    /// First Replace wins: once an earlier after_tool_call handler returns
    /// Replace, later handlers are never reached (mirrors block/modify
    /// chain-stop). Proves the "first transform wins" comment in emit().
    #[tokio::test]
    async fn replace_stops_chain_for_after_tool_call() {
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
        )
        .await;

        assert_eq!(handler.calls(), 1, "handler still runs (it's subscribed)");
        assert_eq!(
            recorded, "ORIGINAL",
            "Replace without tools.transform_output must be ignored — original preserved"
        );
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
            hook_result_action(&HookResult::Confirm {
                message: "Proceed?".into(),
            }),
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

    #[tokio::test]
    async fn confirm_stops_chain_for_before_tool_call() {
        let bus = HookBus::new();
        let confirmer = TestHandler::new(
            "confirmer",
            HookResult::Confirm {
                message: "Run this command?".into(),
            },
        );
        let after = TestHandler::new("after", HookResult::Continue);
        let perms = perms_with(&[Permission::ToolsIntercept]);

        bus.subscribe(
            HookKind::BeforeToolCall,
            confirmer.clone(),
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

        assert!(matches!(result, HookResult::Confirm { .. }));
        assert_eq!(confirmer.calls(), 1);
        assert_eq!(after.calls(), 0);
    }

    #[tokio::test]
    async fn confirm_is_ignored_for_non_tool_hooks() {
        let bus = HookBus::new();
        let confirmer = TestHandler::new(
            "confirmer",
            HookResult::Confirm {
                message: "Not allowed here".into(),
            },
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

    #[tokio::test]
    async fn modify_stops_chain() {
        let bus = HookBus::new();
        let modifier = TestHandler::new(
            "modifier",
            HookResult::Modify {
                input: serde_json::json!({"command": "echo safe"}),
            },
        );
        let after = TestHandler::new(
            "after",
            HookResult::Block {
                reason: "should not run".into(),
            },
        );
        let perms = perms_with(&[Permission::ToolsIntercept]);

        bus.subscribe(
            HookKind::BeforeToolCall,
            modifier.clone(),
            None,
            None,
            perms.clone(),
        )
        .await
        .unwrap();
        bus.subscribe(HookKind::BeforeToolCall, after.clone(), None, None, perms)
            .await
            .unwrap();

        let event = HookEvent::before_tool_call("bash", serde_json::json!({"command": "rm -rf /"}));
        let result = bus.emit(&event).await;

        assert!(
            matches!(result, HookResult::Modify { input } if input == serde_json::json!({"command": "echo safe"}))
        );
        assert_eq!(modifier.calls(), 1);
        assert_eq!(after.calls(), 0); // never reached
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

    #[derive(Debug)]
    struct RecordingHandler {
        id: String,
        recorded_len: Arc<AtomicUsize>,
        result: HookResult,
    }
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
                watcher_exit_path: None,
                tool_register_tx: None,
                session_manager: None,
                subagent_registry: None,
                event_queue: None,
                secret_prompt: None,
                orchestration: None,
                tool_activation: None,
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
    async fn final_output_truncated_to_max_tool_output_after_hook() {
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
    async fn emit_after_tool_call_truncates_char_boundary_safe() {
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
        )
        .await;
        assert!(result2.is_char_boundary(result2.len()));
    }
}
