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

use tokio::sync::RwLock;

use self::events::{HookEvent, HookKind, HookResult};
use crate::extensions::permissions::PermissionSet;

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
    }
}

/// A registered hook handler with its metadata.
#[derive(Clone)]
pub struct HandlerRegistration {
    /// The extension handler.
    pub handler: Arc<dyn crate::extensions::runtime::ExtensionHandler>,
    /// Optional tool name filter (None = all tools).
    pub tool_filter: Option<String>,
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

            // Call handler with timeout
            let handler = reg.handler.clone();
            let event_clone = event.clone();
            let trace_enabled = extensions_trace_enabled();
            let started_at = trace_enabled.then(Instant::now);
            let result = tokio::time::timeout(
                HANDLER_TIMEOUT,
                handler.handle(&event_clone),
            )
            .await;

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
                Ok(HookResult::Block { reason }) => {
                    if !event.kind.allows_result(&HookResult::Block { reason: reason.clone() }) {
                        tracing::warn!(
                            hook = %event.kind.as_str(),
                            extension = %reg.handler.id(),
                            action = "block",
                            "Extension returned action not allowed for hook — ignoring"
                        );
                        continue;
                    }
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
                    if !event.kind.allows_result(&HookResult::Inject { content: content.clone() }) {
                        tracing::warn!(
                            hook = %event.kind.as_str(),
                            extension = %reg.handler.id(),
                            action = "inject",
                            "Extension returned action not allowed for hook — ignoring"
                        );
                        continue;
                    }
                    tracing::debug!(
                        hook = %event.kind.as_str(),
                        extension = %reg.handler.id(),
                        len = content.len(),
                        "Extension injected context"
                    );
                    // Accumulate — don't early-return. Multiple extensions can inject.
                    injections.push(content);
                }
                Ok(HookResult::Confirm { message }) => {
                    if !event.kind.allows_result(&HookResult::Confirm { message: message.clone() }) {
                        tracing::warn!(
                            hook = %event.kind.as_str(),
                            extension = %reg.handler.id(),
                            action = "confirm",
                            "Extension returned action not allowed for hook — ignoring"
                        );
                        continue;
                    }
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

    #[test]
    fn trace_env_value_parser_accepts_common_truthy_values() {
        for value in ["1", "true", "TRUE", "yes", "on"] {
            std::env::set_var("SYNAPS_EXTENSIONS_TRACE", value);
            assert!(extensions_trace_enabled(), "{value} should enable trace mode");
        }

        for value in ["", "0", "false", "off", "no"] {
            std::env::set_var("SYNAPS_EXTENSIONS_TRACE", value);
            assert!(!extensions_trace_enabled(), "{value:?} should not enable trace mode");
        }
        std::env::remove_var("SYNAPS_EXTENSIONS_TRACE");
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

        bus.subscribe(HookKind::BeforeToolCall, handler.clone(), None, perms)
            .await
            .unwrap();

        let event = HookEvent::before_tool_call("bash", serde_json::json!({"command": "ls"}));
        bus.emit(&event).await;

        assert_eq!(handler.calls(), 1);
    }

    #[tokio::test]
    async fn confirm_stops_chain_for_before_tool_call() {
        let bus = HookBus::new();
        let confirmer = TestHandler::new("confirmer", HookResult::Confirm {
            message: "Run this command?".into(),
        });
        let after = TestHandler::new("after", HookResult::Continue);
        let perms = perms_with(&[Permission::ToolsIntercept]);

        bus.subscribe(HookKind::BeforeToolCall, confirmer.clone(), None, perms.clone())
            .await
            .unwrap();
        bus.subscribe(HookKind::BeforeToolCall, after.clone(), None, perms)
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
        let confirmer = TestHandler::new("confirmer", HookResult::Confirm {
            message: "Not allowed here".into(),
        });
        let perms = perms_with(&[Permission::LlmContent]);

        bus.subscribe(HookKind::BeforeMessage, confirmer.clone(), None, perms)
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
        let blocker = TestHandler::new("blocker", HookResult::Block {
            reason: "dangerous".into(),
        });
        let after = TestHandler::new("after", HookResult::Continue);
        let perms = perms_with(&[Permission::ToolsIntercept]);

        bus.subscribe(HookKind::BeforeToolCall, blocker.clone(), None, perms.clone())
            .await
            .unwrap();
        bus.subscribe(HookKind::BeforeToolCall, after.clone(), None, perms)
            .await
            .unwrap();

        let event = HookEvent::before_tool_call("bash", serde_json::json!({}));
        let result = bus.emit(&event).await;

        assert!(matches!(result, HookResult::Block { .. }));
        assert_eq!(blocker.calls(), 1);
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
            .subscribe(HookKind::BeforeToolCall, handler, None, perms)
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("lacks permission"));
    }

    #[tokio::test]
    async fn unsubscribe_removes_handlers() {
        let bus = HookBus::new();
        let handler = TestHandler::new("removable", HookResult::Continue);
        let perms = perms_with(&[Permission::ToolsIntercept]);

        bus.subscribe(HookKind::BeforeToolCall, handler.clone(), None, perms)
            .await
            .unwrap();
        assert_eq!(bus.handler_count().await, 1);

        bus.unsubscribe_all("removable").await;
        assert_eq!(bus.handler_count().await, 0);
    }

    #[tokio::test]
    async fn is_empty_reflects_state() {
        let bus = HookBus::new();
        assert!(bus.is_empty().await);

        let handler = TestHandler::new("ext", HookResult::Continue);
        let perms = perms_with(&[Permission::ToolsIntercept]);
        bus.subscribe(HookKind::BeforeToolCall, handler, None, perms)
            .await
            .unwrap();
        assert!(!bus.is_empty().await);
    }
}
