//! `memory_context` control tool (task A4, spec §7.2).
//!
//! Scope: only `disable`, `status`, and `recall_once` commit directly — they
//! are always locally safe/revocable (spec §7.2 rules). `enable` requires
//! deterministic host-owned proof of user intent (`ExplicitCommand` from the
//! `/memory` frontend command, task A5) that a model tool call cannot supply
//! through JSON parameters alone, so it fails with the typed
//! [`MemoryContextError::RequiresHostConfirmation`] refusal. `index_history`
//! returns the host-computed D1 disclosure preview, but a model call remains
//! only a proposal and cannot confirm or begin import.
//!
//! Boundary discipline: raw JSON arguments are parsed into the typed
//! [`MemoryContextRequest`] here at the boundary — malformed values, unknown
//! actions, schema-foreign properties, and action-inapplicable parameters all
//! fail closed with static messages that never echo raw model input.

use super::{Tool, ToolContext};
use crate::runtime::memory_context::{
    DurableStatus, MemoryContextCapability, MemoryContextError, MemoryContextMode,
    MemoryContextStatus, OneShotStatus,
};
use crate::runtime::memory_history::{
    propose_history_import, CanonicalHistoryMetadataIo, HistoryImportHostState,
};
use crate::{Result, RuntimeError};
use serde_json::{json, Value};

/// Control the continuous-memory context of the current session (spec §7.2).
pub struct MemoryContextTool;

/// The complete, closed set of schema properties (spec §7.2,
/// `additionalProperties: false`). Enforced manually because provider-side
/// schema validation is advisory, never trusted.
const SCHEMA_PROPERTIES: [&str; 4] = ["action", "mode", "capture_tools", "expires_minutes"];

/// Typed, parse-at-the-boundary form of one tool invocation. Only data that
/// survived validation exists past this point — no raw JSON leaks through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemoryContextRequest {
    /// Proposal for a durable session lease — always refused (host-only).
    Enable,
    /// Revoke this session's memory context (always locally allowed).
    Disable,
    /// Metadata-only status snapshot (never spawns a provider).
    Status,
    /// Grant a one-shot recall lease, optionally with a bounded expiry.
    RecallOnce { expires_minutes: Option<u32> },
    /// Proposal to index prior history — always refused (host-only:
    /// requires separate disclosure preview and consent, spec §7.2).
    IndexHistory,
}

impl MemoryContextRequest {
    /// Static action name for the typed response — never the raw input.
    fn action_name(self) -> &'static str {
        match self {
            MemoryContextRequest::Enable => "enable",
            MemoryContextRequest::Disable => "disable",
            MemoryContextRequest::Status => "status",
            MemoryContextRequest::RecallOnce { .. } => "recall_once",
            MemoryContextRequest::IndexHistory => "index_history",
        }
    }
}

/// Parse raw model JSON into a [`MemoryContextRequest`]. Every failure is a
/// static, content-free message: raw model input is never echoed back.
fn parse_request(params: &Value) -> std::result::Result<MemoryContextRequest, String> {
    let object = params
        .as_object()
        .ok_or_else(|| "memory_context parameters must be a JSON object".to_string())?;

    // additionalProperties: false — schema-foreign keys fail closed.
    if object
        .keys()
        .any(|key| !SCHEMA_PROPERTIES.contains(&key.as_str()))
    {
        return Err(
            "memory_context rejects unknown parameters (additionalProperties is false)".to_string(),
        );
    }

    // Validate every known property's type/range regardless of action.
    let mode_supplied = match object.get("mode") {
        None => false,
        Some(Value::String(mode))
            if matches!(
                mode.as_str(),
                "recall_each_prompt" | "capture_only" | "capture_and_recall"
            ) =>
        {
            true
        }
        Some(_) => return Err("malformed 'mode': not a value from the schema enum".to_string()),
    };
    let capture_tools_supplied = match object.get("capture_tools") {
        None => false,
        Some(Value::Bool(_)) => true,
        Some(_) => return Err("malformed 'capture_tools': expected a boolean".to_string()),
    };
    let expires_minutes = match object.get("expires_minutes") {
        None => None,
        Some(value) => {
            let minutes = value
                .as_u64()
                .filter(|minutes| (1..=1440).contains(minutes))
                .ok_or_else(|| {
                    "malformed 'expires_minutes': expected an integer in 1..=1440".to_string()
                })?;
            Some(u32::try_from(minutes).expect("bounded by 1440"))
        }
    };

    let action = object
        .get("action")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing or malformed 'action'".to_string())?;

    // Reject parameters inapplicable to the requested action — a fail-closed
    // strictness on top of the schema's flat property list.
    let reject_inapplicable = |action_name: &'static str| -> std::result::Result<(), String> {
        if mode_supplied {
            return Err(format!(
                "'mode' is not applicable to action '{action_name}'"
            ));
        }
        if capture_tools_supplied {
            return Err(format!(
                "'capture_tools' is not applicable to action '{action_name}'"
            ));
        }
        Ok(())
    };
    let reject_expiry = |action_name: &'static str| -> std::result::Result<(), String> {
        if expires_minutes.is_some() {
            return Err(format!(
                "'expires_minutes' is not applicable to action '{action_name}'"
            ));
        }
        Ok(())
    };

    match action {
        // `enable` accepts the full parameter surface (mode/capture_tools/
        // expires_minutes) per the schema; it is refused later regardless.
        "enable" => Ok(MemoryContextRequest::Enable),
        "disable" => {
            reject_inapplicable("disable")?;
            reject_expiry("disable")?;
            Ok(MemoryContextRequest::Disable)
        }
        "status" => {
            reject_inapplicable("status")?;
            reject_expiry("status")?;
            Ok(MemoryContextRequest::Status)
        }
        "recall_once" => {
            reject_inapplicable("recall_once")?;
            Ok(MemoryContextRequest::RecallOnce { expires_minutes })
        }
        "index_history" => {
            reject_inapplicable("index_history")?;
            reject_expiry("index_history")?;
            Ok(MemoryContextRequest::IndexHistory)
        }
        // Static message only — the raw action value is never echoed.
        _ => Err("unknown 'action': not a value from the schema enum".to_string()),
    }
}

/// Static mode label for the bounded response summary.
fn mode_label(mode: MemoryContextMode) -> &'static str {
    match mode {
        MemoryContextMode::Off => "off",
        MemoryContextMode::RecallOnce => "recall_once",
        MemoryContextMode::RecallEachPrompt => "recall_each_prompt",
        MemoryContextMode::CaptureOnly => "capture_only",
        MemoryContextMode::CaptureAndRecall => "capture_and_recall",
    }
}

/// Render the bounded JSON status summary (mode, project digest placeholder,
/// expiry). `None` means no capability is wired: memory is deterministically
/// `Off` — `Off` requires no infrastructure (task A4 scope).
fn render_summary(action: &'static str, status: Option<&MemoryContextStatus>) -> String {
    let (mode, one_shot, expires_at) = match status {
        None => ("off", "idle", Value::Null),
        Some(status) => {
            let (mode, expires_at) = match &status.durable {
                DurableStatus::Off => ("off", Value::Null),
                DurableStatus::Active {
                    mode, expires_at, ..
                } => (
                    mode_label(*mode),
                    expires_at
                        .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
                        .map_or(Value::Null, |since| json!(since.as_secs())),
                ),
            };
            let one_shot = match &status.one_shot {
                OneShotStatus::Idle => "idle",
                OneShotStatus::Pending { .. } => "pending",
                OneShotStatus::Consumed { .. } => "consumed",
            };
            (mode, one_shot, expires_at)
        }
    };
    json!({
        "action": action,
        "mode": mode,
        "one_shot_recall": one_shot,
        // Placeholder until task A5 wires the host-computed project digest.
        "project_digest": Value::Null,
        "expires_at": expires_at,
    })
    .to_string()
}

fn typed_failure(error: MemoryContextError) -> RuntimeError {
    RuntimeError::Tool(format!("memory_context denied: {error}"))
}

#[async_trait::async_trait]
impl Tool for MemoryContextTool {
    fn origin(&self) -> crate::tools::ToolOrigin {
        crate::tools::ToolOrigin::Builtin
    }

    fn name(&self) -> &str {
        "memory_context"
    }

    fn description(&self) -> &str {
        "Control the continuous-memory context of this session. 'status', 'disable', and 'recall_once' commit locally safe actions. 'enable' requires the deterministic /memory command. 'index_history' returns a host-computed metadata preview only; explicit frontend confirmation is still required and no import begins from a model tool call."
    }

    fn parameters(&self) -> Value {
        // Exact schema from spec §7.2.
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "action": {
                    "enum": ["enable", "disable", "status", "recall_once", "index_history"],
                    "description": "Memory-context action. index_history previews metadata only; import still requires explicit frontend confirmation."
                },
                "mode": {
                    "enum": ["recall_each_prompt", "capture_only", "capture_and_recall"],
                    "description": "Requested durable mode (enable proposals only)."
                },
                "capture_tools": {
                    "type": "boolean",
                    "description": "Whether tool activity is eligible for capture (enable proposals only)."
                },
                "expires_minutes": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 1440,
                    "description": "Bounded lease expiry in minutes."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<String> {
        let request = parse_request(&params).map_err(RuntimeError::Tool)?;
        let capability: Option<&MemoryContextCapability> = ctx.capabilities.memory_context.as_ref();

        match request {
            // `status` is deterministic even with no capability wired: memory
            // is `Off`, and `Off` requires no infrastructure.
            MemoryContextRequest::Status => Ok(render_summary(
                request.action_name(),
                capability.map(MemoryContextCapability::status).as_ref(),
            )),
            // `disable` is always locally allowed and idempotent; with no
            // capability there is nothing to revoke — already `Off`.
            MemoryContextRequest::Disable => Ok(render_summary(
                request.action_name(),
                capability.map(MemoryContextCapability::disable).as_ref(),
            )),
            MemoryContextRequest::RecallOnce { expires_minutes } => {
                let capability = capability
                    .ok_or_else(|| typed_failure(MemoryContextError::CapabilityUnavailable))?;
                let status = capability
                    .recall_once(expires_minutes)
                    .map_err(typed_failure)?;
                Ok(render_summary(request.action_name(), Some(&status)))
            }
            // A model may request the host-owned preview, but the request is
            // only a proposal: this branch never receives a confirmation proof
            // and cannot begin import.
            MemoryContextRequest::IndexHistory => {
                let host = HistoryImportHostState::from_current_host()
                    .map_err(|error| RuntimeError::Tool(error.to_string()))?;
                let mut io = CanonicalHistoryMetadataIo::new();
                let preview = propose_history_import(&host, &mut io)
                    .map_err(|error| RuntimeError::Tool(error.to_string()))?;
                Ok(preview.render())
            }
            // Durable enable needs deterministic host-owned proof
            // (ExplicitCommand from /memory, task A5). A tool call cannot carry
            // it — typed refusal, no lease installed, nothing mutated.
            MemoryContextRequest::Enable => {
                if capability.is_none() {
                    return Err(typed_failure(MemoryContextError::CapabilityUnavailable));
                }
                Err(typed_failure(MemoryContextError::RequiresHostConfirmation))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::memory_context::{
        CapturePolicy, ContextProviderId, MemoryContextLease, MemoryLeaseId, ProjectId,
        RecallPolicy, RequestId, SessionId, SessionMemoryState, UserIntentProof,
    };
    use crate::tools::test_helpers::create_tool_context;
    use std::sync::{Arc, Mutex};
    use std::time::SystemTime;

    fn test_state() -> Arc<Mutex<SessionMemoryState>> {
        Arc::new(Mutex::new(SessionMemoryState::new(
            SessionId::parse("sess-tool").expect("valid session id"),
        )))
    }

    fn capability_over(state: Arc<Mutex<SessionMemoryState>>) -> MemoryContextCapability {
        MemoryContextCapability::new(
            state,
            ProjectId::parse("proj-tool").expect("valid project id"),
            ContextProviderId::parse("axel-memory").expect("valid provider id"),
            UserIntentProof::ExplicitCommand {
                command_id: RequestId::parse("cmd-test").expect("valid request id"),
            },
        )
    }

    /// Tool context with the capability wired; also returns the capability
    /// so tests can observe the shared session state.
    fn wired_context() -> (ToolContext, MemoryContextCapability) {
        let capability = capability_over(test_state());
        let mut ctx = create_tool_context();
        ctx.capabilities.memory_context = Some(capability.clone());
        (ctx, capability)
    }

    fn assert_fully_off(capability: &MemoryContextCapability) {
        let status = capability.status();
        assert_eq!(status.durable, DurableStatus::Off);
        assert_eq!(status.one_shot, OneShotStatus::Idle);
    }

    fn parsed(output: &str) -> Value {
        serde_json::from_str(output).expect("tool output is valid JSON")
    }

    // ── forged enable / index_history ───────────────────────────────────────

    #[tokio::test]
    async fn forged_enable_is_denied_and_installs_no_lease() {
        let (ctx, capability) = wired_context();

        let error = MemoryContextTool
            .execute(
                json!({"action": "enable", "mode": "capture_and_recall", "capture_tools": true, "expires_minutes": 60}),
                ctx,
            )
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("host confirmation"), "{error}");
        assert_fully_off(&capability);
    }

    #[tokio::test]
    async fn model_index_history_returns_preview_but_cannot_confirm() {
        let (ctx, capability) = wired_context();

        let preview = MemoryContextTool
            .execute(json!({"action": "index_history"}), ctx)
            .await
            .expect("model proposal receives metadata-only preview");

        assert!(preview.contains("History import preview"), "{preview}");
        assert!(
            preview.contains("explicit confirmation required: true"),
            "{preview}"
        );
        assert!(preview.contains("no import has started"), "{preview}");
        assert_fully_off(&capability);
    }

    #[tokio::test]
    async fn enable_without_capability_fails_typed_unavailable() {
        let error = MemoryContextTool
            .execute(json!({"action": "enable"}), create_tool_context())
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("unavailable in this context"), "{error}");
    }

    // ── status ──────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn status_without_capability_returns_off_without_erroring() {
        let output = MemoryContextTool
            .execute(json!({"action": "status"}), create_tool_context())
            .await
            .expect("status must never error: Off requires no infrastructure");

        let output = parsed(&output);
        assert_eq!(output["action"], "status");
        assert_eq!(output["mode"], "off");
        assert_eq!(output["one_shot_recall"], "idle");
        assert_eq!(output["project_digest"], Value::Null);
        assert_eq!(output["expires_at"], Value::Null);
    }

    #[tokio::test]
    async fn status_with_capability_reports_current_state() {
        let (ctx, _capability) = wired_context();

        let output = MemoryContextTool
            .execute(json!({"action": "status"}), ctx)
            .await
            .unwrap();

        let output = parsed(&output);
        assert_eq!(output["mode"], "off");
        assert_eq!(output["one_shot_recall"], "idle");
    }

    // ── disable ─────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn disable_without_capability_is_an_idempotent_off_noop() {
        for _ in 0..2 {
            let output = MemoryContextTool
                .execute(json!({"action": "disable"}), create_tool_context())
                .await
                .expect("disable is always locally allowed");
            let output = parsed(&output);
            assert_eq!(output["action"], "disable");
            assert_eq!(output["mode"], "off");
            assert_eq!(output["one_shot_recall"], "idle");
        }
    }

    #[tokio::test]
    async fn disable_revokes_an_active_lease_and_stays_idempotent() {
        let state = test_state();
        let lease = MemoryContextLease::grant(
            MemoryLeaseId::parse("lease-durable").unwrap(),
            SessionId::parse("sess-tool").unwrap(),
            ProjectId::parse("proj-tool").unwrap(),
            ContextProviderId::parse("axel-memory").unwrap(),
            MemoryContextMode::CaptureAndRecall,
            CapturePolicy::default(),
            RecallPolicy::default(),
            UserIntentProof::ExplicitCommand {
                command_id: RequestId::parse("cmd-host").unwrap(),
            },
            SystemTime::now(),
            None,
        )
        .unwrap();
        state.lock().unwrap().install(lease).unwrap();

        let capability = capability_over(state);
        assert_eq!(
            capability.status().durable,
            DurableStatus::Active {
                mode: MemoryContextMode::CaptureAndRecall,
                lease_id: MemoryLeaseId::parse("lease-durable").unwrap(),
                expires_at: None,
            }
        );

        for _ in 0..2 {
            let mut ctx = create_tool_context();
            ctx.capabilities.memory_context = Some(capability.clone());
            let output = MemoryContextTool
                .execute(json!({"action": "disable"}), ctx)
                .await
                .unwrap();
            assert_eq!(parsed(&output)["mode"], "off");
            assert_eq!(capability.status().durable, DurableStatus::Off);
        }
    }

    // ── recall_once ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn recall_once_installs_exactly_one_pending_one_shot() {
        let (ctx, capability) = wired_context();

        let output = MemoryContextTool
            .execute(json!({"action": "recall_once"}), ctx)
            .await
            .unwrap();

        let output = parsed(&output);
        assert_eq!(output["action"], "recall_once");
        assert_eq!(
            output["mode"], "off",
            "one-shot never occupies the durable slot"
        );
        assert_eq!(output["one_shot_recall"], "pending");
        assert!(matches!(
            capability.status().one_shot,
            OneShotStatus::Pending { .. }
        ));

        // A second grant while one is pending fails typed and leaves the
        // original pending lease in place.
        let mut ctx = create_tool_context();
        ctx.capabilities.memory_context = Some(capability.clone());
        let error = MemoryContextTool
            .execute(json!({"action": "recall_once"}), ctx)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("already pending"), "{error}");
        assert!(matches!(
            capability.status().one_shot,
            OneShotStatus::Pending { .. }
        ));
    }

    #[tokio::test]
    async fn recall_once_accepts_a_bounded_expiry() {
        let (ctx, capability) = wired_context();

        let output = MemoryContextTool
            .execute(json!({"action": "recall_once", "expires_minutes": 5}), ctx)
            .await
            .unwrap();

        assert_eq!(parsed(&output)["one_shot_recall"], "pending");
        assert!(matches!(
            capability.status().one_shot,
            OneShotStatus::Pending { .. }
        ));
    }

    #[tokio::test]
    async fn recall_once_without_capability_fails_typed() {
        let error = MemoryContextTool
            .execute(json!({"action": "recall_once"}), create_tool_context())
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("unavailable in this context"), "{error}");
    }

    // ── fail-closed parsing ─────────────────────────────────────────────────

    #[tokio::test]
    async fn unknown_action_fails_closed_without_echoing_input() {
        let (ctx, capability) = wired_context();

        let error = MemoryContextTool
            .execute(json!({"action": "hijack_everything"}), ctx)
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("unknown 'action'"), "{error}");
        assert!(
            !error.contains("hijack_everything"),
            "raw model input must never be echoed back: {error}"
        );
        assert_fully_off(&capability);
    }

    #[tokio::test]
    async fn malformed_action_and_missing_action_fail_closed() {
        for params in [
            json!({}),
            json!({"action": 5}),
            json!({"action": null}),
            json!([]),
        ] {
            let (ctx, capability) = wired_context();
            let error = MemoryContextTool
                .execute(params, ctx)
                .await
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("'action'") || error.contains("JSON object"),
                "{error}"
            );
            assert_fully_off(&capability);
        }
    }

    #[tokio::test]
    async fn extra_properties_fail_closed() {
        let (ctx, capability) = wired_context();

        let error = MemoryContextTool
            .execute(json!({"action": "status", "surprise_grant": true}), ctx)
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("unknown parameters"), "{error}");
        assert!(
            !error.contains("surprise_grant"),
            "raw model input must never be echoed back: {error}"
        );
        assert_fully_off(&capability);
    }

    #[tokio::test]
    async fn inapplicable_and_out_of_range_parameters_fail_closed() {
        let cases = [
            json!({"action": "disable", "mode": "capture_only"}),
            json!({"action": "status", "expires_minutes": 5}),
            json!({"action": "recall_once", "capture_tools": true}),
            json!({"action": "recall_once", "mode": "recall_each_prompt"}),
            json!({"action": "recall_once", "expires_minutes": 0}),
            json!({"action": "recall_once", "expires_minutes": 1441}),
            json!({"action": "recall_once", "expires_minutes": "5"}),
            json!({"action": "recall_once", "expires_minutes": 5.5}),
            json!({"action": "enable", "mode": "off"}),
            json!({"action": "enable", "capture_tools": "yes"}),
        ];
        for params in cases {
            let (ctx, capability) = wired_context();
            MemoryContextTool
                .execute(params.clone(), ctx)
                .await
                .expect_err(&format!("must fail closed: {params}"));
            assert_fully_off(&capability);
        }
    }
}
