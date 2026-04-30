//! Hook events — typed payloads for each extension point.
//!
//! Each [`HookKind`] maps to a discrete phase of SynapsCLI's execution loop.
//! [`HookEvent`] is the concrete payload dispatched to subscribers at that
//! phase; [`HookResult`] is what a handler returns to control execution flow.
//!
//! Permission enforcement lives in [`crate::extensions::permissions`]:
//! every `HookKind` declares a [`required_permission`][HookKind::required_permission]
//! so the runtime can gate subscriptions before any payload is delivered.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::extensions::permissions::Permission;

// ── HookKind ──────────────────────────────────────────────────────────────────

/// All hook event kinds in the phase-1 catalog.
///
/// Each variant identifies a well-defined extension point in the agent loop.
/// The set is intentionally closed; new kinds are added via a breaking version
/// bump so existing permission grants stay coherent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookKind {
    /// Fires immediately before a tool is invoked. Handlers may block the call.
    BeforeToolCall,
    /// Fires immediately after a tool returns. Handlers receive the output.
    AfterToolCall,
    /// Fires before an LLM message is sent. Handlers may inspect or block.
    BeforeMessage,
    /// Fires when a new session is created.
    OnSessionStart,
    /// Fires when a session is torn down.
    OnSessionEnd,
}

impl HookKind {
    /// Canonical string identifier for this kind, suitable for serialization
    /// keys, log output, and manifest declarations.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::BeforeToolCall => "before_tool_call",
            Self::AfterToolCall => "after_tool_call",
            Self::BeforeMessage => "before_message",
            Self::OnSessionStart => "on_session_start",
            Self::OnSessionEnd => "on_session_end",
        }
    }

    /// Parse from the canonical string representation.
    ///
    /// Returns `None` for unrecognised strings so callers can surface
    /// a manifest validation error rather than silently dropping hooks.
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "before_tool_call" => Some(Self::BeforeToolCall),
            "after_tool_call" => Some(Self::AfterToolCall),
            "before_message" => Some(Self::BeforeMessage),
            "on_session_start" => Some(Self::OnSessionStart),
            "on_session_end" => Some(Self::OnSessionEnd),
            _ => None,
        }
    }

    /// The [`Permission`] an extension must hold to subscribe to this hook.
    ///
    /// Called by the permission gate before delivering any event; if the
    /// extension's [`PermissionSet`][crate::extensions::permissions::PermissionSet]
    /// does not include this permission, `HookBus::subscribe()` returns an error.
    pub fn required_permission(&self) -> Permission {
        match self {
            Self::BeforeToolCall | Self::AfterToolCall => Permission::ToolsIntercept,
            Self::BeforeMessage => Permission::LlmContent,
            Self::OnSessionStart | Self::OnSessionEnd => Permission::SessionLifecycle,
        }
    }
}

// ── HookEvent ─────────────────────────────────────────────────────────────────

/// A hook event payload dispatched to extension handlers.
///
/// Fields are optional and populated only when relevant to the hook kind:
///
/// | Kind              | tool_name | tool_input | tool_output | message | session_id |
/// |-------------------|-----------|------------|-------------|---------|------------|
/// | `before_tool_call`| ✓         | ✓          |             |         |            |
/// | `after_tool_call` | ✓         | ✓          | ✓           |         |            |
/// | `before_message`  |           |            |             | ✓       |            |
/// | `on_session_start`|           |            |             |         | ✓          |
/// | `on_session_end`  |           |            |             |         | ✓          |
///
/// The `data` field is available on all events for extensions that need to
/// attach arbitrary structured context when constructing synthetic events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookEvent {
    /// Which hook fired.
    pub kind: HookKind,
    /// Tool name for tool-specific hooks; `None` for general hooks.
    /// This is the API-safe name (sanitized for the LLM).
    pub tool_name: Option<String>,
    /// Original runtime name of the tool (before API sanitization).
    /// Extension authors typically write runtime names in their manifests.
    #[serde(default)]
    pub tool_runtime_name: Option<String>,
    /// Tool input arguments for `before_tool_call` and `after_tool_call`.
    pub tool_input: Option<Value>,
    /// Tool output for `after_tool_call`.
    pub tool_output: Option<String>,
    /// LLM message content for `before_message`.
    pub message: Option<String>,
    /// Session identifier for session lifecycle hooks.
    pub session_id: Option<String>,
    /// Session message history for `on_session_end`.
    /// Contains the conversation transcript so extensions (like Stelline)
    /// can extract memories without reaching into runtime internals.
    #[serde(default)]
    pub transcript: Option<Vec<Value>>,
    /// Arbitrary extension-defined data, passed through without inspection.
    pub data: Value,
}

impl HookEvent {
    /// Construct a `before_tool_call` event.
    pub fn before_tool_call(tool_name: &str, input: Value) -> Self {
        Self {
            kind: HookKind::BeforeToolCall,
            tool_name: Some(tool_name.to_string()),
            tool_input: Some(input),
            tool_output: None,
            message: None,
            session_id: None,
            tool_runtime_name: None,
            transcript: None,
            data: Value::Null,
        }
    }

    /// Construct an `after_tool_call` event carrying both input and output.
    /// Output is truncated to MAX_HOOK_OUTPUT_SIZE to prevent sending
    /// megabytes of bash output over the JSON-RPC pipe.
    pub fn after_tool_call(tool_name: &str, input: Value, output: String) -> Self {
        const MAX_HOOK_OUTPUT: usize = 32 * 1024; // 32 KB
        let truncated_output = if output.len() > MAX_HOOK_OUTPUT {
            // Find the last valid UTF-8 char boundary at or before MAX_HOOK_OUTPUT
            let mut end = MAX_HOOK_OUTPUT;
            while end > 0 && !output.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}…[truncated, {} total bytes]", &output[..end], output.len())
        } else {
            output
        };
        Self {
            kind: HookKind::AfterToolCall,
            tool_name: Some(tool_name.to_string()),
            tool_input: Some(input),
            tool_output: Some(truncated_output),
            message: None,
            session_id: None,
            tool_runtime_name: None,
            transcript: None,
            data: Value::Null,
        }
    }

    /// Construct a `before_message` event.
    pub fn before_message(message: &str) -> Self {
        Self {
            kind: HookKind::BeforeMessage,
            tool_name: None,
            tool_input: None,
            tool_output: None,
            message: Some(message.to_string()),
            session_id: None,
            tool_runtime_name: None,
            transcript: None,
            data: Value::Null,
        }
    }

    /// Construct an `on_session_start` event.
    pub fn on_session_start(session_id: &str) -> Self {
        Self {
            kind: HookKind::OnSessionStart,
            tool_name: None,
            tool_input: None,
            tool_output: None,
            message: None,
            session_id: Some(session_id.to_string()),
            tool_runtime_name: None,
            transcript: None,
            data: Value::Null,
        }
    }

    /// Construct an `on_session_end` event.
    pub fn on_session_end(session_id: &str, transcript: Option<Vec<Value>>) -> Self {
        Self {
            kind: HookKind::OnSessionEnd,
            tool_name: None,
            tool_input: None,
            tool_output: None,
            message: None,
            session_id: Some(session_id.to_string()),
            tool_runtime_name: None,
            transcript,
            data: Value::Null,
        }
    }
}

// ── HookResult ────────────────────────────────────────────────────────────────

/// What an extension handler returns after processing a hook event.
///
/// The runtime resolves multiple handlers by precedence:
/// - Any `Block` from any handler prevents the operation.
///   and the runtime should re-read them before proceeding.
/// - `Continue` is the no-op default — processing continues normally.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum HookResult {
    /// Allow execution to proceed unchanged.
    Continue,
    /// Prevent the hooked operation. The `reason` is surfaced to the user.
    Block { reason: String },
    /// Inject context — the extension provides text to prepend to the
    /// system prompt or conversation. Used by before_message hooks.
    Inject { content: String },
    // NOTE: `Modify` was removed in the review pass. Process-based extensions
    // can't mutate events in-place (they get a serialized copy). If mutation
    // support is needed, add a `ModifyWith { fields: Value }` variant that
    // carries the modified data back.
}

impl Default for HookResult {
    fn default() -> Self {
        Self::Continue
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── HookKind ──────────────────────────────────────────────────────────────

    /// Every variant's as_str round-trips through from_str.
    #[test]
    fn hook_kind_as_str_roundtrip() {
        let all = [
            HookKind::BeforeToolCall,
            HookKind::AfterToolCall,
            HookKind::BeforeMessage,
            HookKind::OnSessionStart,
            HookKind::OnSessionEnd,
        ];
        for kind in all {
            let s = kind.as_str();
            assert_eq!(
                HookKind::from_str(s),
                Some(kind),
                "round-trip failed for {s}"
            );
        }
    }

    /// Unknown strings return None, not a panic or a default.
    #[test]
    fn hook_kind_from_str_unknown_returns_none() {
        assert_eq!(HookKind::from_str(""), None);
        assert_eq!(HookKind::from_str("BeforeToolCall"), None); // wrong case
        assert_eq!(HookKind::from_str("on_crash"), None);
    }

    /// Serde uses snake_case via the attribute — spot-check two variants.
    #[test]
    fn hook_kind_serde_snake_case() {
        let serialized = serde_json::to_string(&HookKind::BeforeToolCall).unwrap();
        assert_eq!(serialized, r#""before_tool_call""#);

        let back: HookKind = serde_json::from_str(r#""on_session_end""#).unwrap();
        assert_eq!(back, HookKind::OnSessionEnd);
    }

    /// Each kind maps to the expected permission.
    #[test]
    fn hook_kind_required_permission() {
        assert_eq!(
            HookKind::BeforeToolCall.required_permission(),
            Permission::ToolsIntercept
        );
        assert_eq!(
            HookKind::AfterToolCall.required_permission(),
            Permission::ToolsIntercept
        );
        assert_eq!(
            HookKind::BeforeMessage.required_permission(),
            Permission::LlmContent
        );
        assert_eq!(
            HookKind::OnSessionStart.required_permission(),
            Permission::SessionLifecycle
        );
        assert_eq!(
            HookKind::OnSessionEnd.required_permission(),
            Permission::SessionLifecycle
        );
    }

    // ── HookEvent constructors ────────────────────────────────────────────────

    #[test]
    fn hook_event_before_tool_call() {
        let input = json!({"path": "/tmp/foo"});
        let ev = HookEvent::before_tool_call("read_file", input.clone());

        assert_eq!(ev.kind, HookKind::BeforeToolCall);
        assert_eq!(ev.tool_name.as_deref(), Some("read_file"));
        assert_eq!(ev.tool_input.as_ref(), Some(&input));
        assert!(ev.tool_output.is_none());
        assert!(ev.message.is_none());
        assert!(ev.session_id.is_none());
        assert_eq!(ev.data, Value::Null);
    }

    #[test]
    fn hook_event_after_tool_call() {
        let input = json!({"query": "select 1"});
        let ev =
            HookEvent::after_tool_call("sql_query", input.clone(), "1 row".to_string());

        assert_eq!(ev.kind, HookKind::AfterToolCall);
        assert_eq!(ev.tool_name.as_deref(), Some("sql_query"));
        assert_eq!(ev.tool_input.as_ref(), Some(&input));
        assert_eq!(ev.tool_output.as_deref(), Some("1 row"));
        assert!(ev.message.is_none());
        assert!(ev.session_id.is_none());
    }

    #[test]
    fn hook_event_before_message() {
        let ev = HookEvent::before_message("Hello, LLM");

        assert_eq!(ev.kind, HookKind::BeforeMessage);
        assert!(ev.tool_name.is_none());
        assert!(ev.tool_input.is_none());
        assert!(ev.tool_output.is_none());
        assert_eq!(ev.message.as_deref(), Some("Hello, LLM"));
        assert!(ev.session_id.is_none());
    }

    #[test]
    fn hook_event_on_session_start() {
        let ev = HookEvent::on_session_start("sess-abc-123");

        assert_eq!(ev.kind, HookKind::OnSessionStart);
        assert_eq!(ev.session_id.as_deref(), Some("sess-abc-123"));
        assert!(ev.tool_name.is_none());
        assert!(ev.message.is_none());
    }

    #[test]
    fn hook_event_on_session_end() {
        let ev = HookEvent::on_session_end("sess-abc-123", None);

        assert_eq!(ev.kind, HookKind::OnSessionEnd);
        assert_eq!(ev.session_id.as_deref(), Some("sess-abc-123"));
        assert!(ev.tool_name.is_none());
        assert!(ev.message.is_none());
    }

    /// HookEvent is round-trippable through JSON without loss.
    #[test]
    fn hook_event_serde_roundtrip() {
        let ev = HookEvent::before_tool_call("bash", json!({"cmd": "ls"}));
        let json = serde_json::to_string(&ev).unwrap();
        let back: HookEvent = serde_json::from_str(&json).unwrap();

        assert_eq!(back.kind, ev.kind);
        assert_eq!(back.tool_name, ev.tool_name);
        assert_eq!(back.tool_input, ev.tool_input);
    }

    // ── HookResult ────────────────────────────────────────────────────────────

    /// Default is Continue.
    #[test]
    fn hook_result_default_is_continue() {
        assert!(matches!(HookResult::default(), HookResult::Continue));
    }

    /// Block carries its reason through serialization.
    #[test]
    fn hook_result_block_serde() {
        let r = HookResult::Block {
            reason: "denied by policy".to_string(),
        };
        let json = serde_json::to_string(&r).unwrap();
        // `tag = "action"` means the JSON object has {"action":"block","reason":"..."}
        assert!(json.contains(r#""action":"block""#));
        assert!(json.contains("denied by policy"));

        let back: HookResult = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, HookResult::Block { reason } if reason == "denied by policy"));
    }

    /// Continue serialises as {"action":"continue"}.
    #[test]
    fn hook_result_continue_serde() {
        let json = serde_json::to_string(&HookResult::Continue).unwrap();
        assert_eq!(json, r#"{"action":"continue"}"#);
    }
}

impl HookEvent {
    /// Set the runtime name for tool-related events.
    pub fn with_runtime_name(mut self, name: &str) -> Self {
        self.tool_runtime_name = Some(name.to_string());
        self
    }
}
