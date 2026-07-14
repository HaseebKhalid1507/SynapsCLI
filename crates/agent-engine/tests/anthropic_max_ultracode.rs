//! Offline, deterministic authorization harness for Anthropic special modes.
use agent_core::reasoning::ReasoningLevel;
use agent_engine::runtime::openai::catalog::{
    plan_anthropic_execution, plan_codex_execution, AnthropicExecutionMode, AnthropicPlanErrorCode,
    AnthropicPlanPrerequisites, AnthropicWireEffort, AnthropicWorkflowPlan, CodexExecutionMode,
    CodexMultiAgentMode, CodexRequestRole, CodexWireEffort, ExecutionRole,
};

const FABLE: &str = "anthropic/claude-fable-5";

fn installed() -> AnthropicPlanPrerequisites {
    AnthropicPlanPrerequisites::installed()
}

#[test]
fn exact_fable_authorizes_only_evidence_backed_special_modes() {
    let max = plan_anthropic_execution(
        FABLE,
        ReasoningLevel::Max,
        ExecutionRole::Foreground,
        installed(),
        None,
    )
    .unwrap();
    assert_eq!(max.mode, AnthropicExecutionMode::Max);
    assert_eq!(max.wire_effort, Some(AnthropicWireEffort::Max));
    assert_eq!(max.workflow, AnthropicWorkflowPlan::None);

    let ultra = plan_anthropic_execution(
        FABLE,
        ReasoningLevel::UltraCode,
        ExecutionRole::Foreground,
        installed(),
        None,
    )
    .unwrap();
    assert_eq!(ultra.mode, AnthropicExecutionMode::UltraCode);
    assert_eq!(ultra.wire_effort, Some(AnthropicWireEffort::XHigh));
    assert_eq!(ultra.workflow, AnthropicWorkflowPlan::Standing);

    let xhigh = plan_anthropic_execution(
        FABLE,
        ReasoningLevel::XHigh,
        ExecutionRole::Foreground,
        installed(),
        None,
    )
    .unwrap();
    assert_eq!(xhigh.wire_effort, Some(AnthropicWireEffort::XHigh));
    assert_eq!(xhigh.workflow, AnthropicWorkflowPlan::None);
}

#[test]
fn special_modes_fail_closed_for_every_authority_near_miss() {
    let cases = [
        (
            "anthropic/claude-fable-5-near",
            ExecutionRole::Foreground,
            installed(),
            None,
            AnthropicPlanErrorCode::CapabilityMetadataMissing,
        ),
        (
            "openai-codex/claude-fable-5",
            ExecutionRole::Foreground,
            installed(),
            None,
            AnthropicPlanErrorCode::InvalidProviderIdentity,
        ),
        (
            FABLE,
            ExecutionRole::Foreground,
            installed(),
            Some(false),
            AnthropicPlanErrorCode::UnsupportedReasoningLevel,
        ),
        (
            FABLE,
            ExecutionRole::Worker,
            installed(),
            None,
            AnthropicPlanErrorCode::UltraCodeRequiresForeground,
        ),
        (
            FABLE,
            ExecutionRole::Internal,
            installed(),
            None,
            AnthropicPlanErrorCode::UltraCodeRequiresForeground,
        ),
        (
            FABLE,
            ExecutionRole::Foreground,
            AnthropicPlanPrerequisites {
                orchestration_policy: false,
                builtin_lifecycle_tools: true,
            },
            None,
            AnthropicPlanErrorCode::UltraCodeRequiresOrchestration,
        ),
        (
            FABLE,
            ExecutionRole::Foreground,
            AnthropicPlanPrerequisites {
                orchestration_policy: true,
                builtin_lifecycle_tools: false,
            },
            None,
            AnthropicPlanErrorCode::UltraCodeRequiresLifecycleTools,
        ),
    ];
    for (model, role, prereqs, live, code) in cases {
        let error = plan_anthropic_execution(model, ReasoningLevel::UltraCode, role, prereqs, live)
            .unwrap_err();
        assert_eq!(error.code(), code);
        assert_eq!(error.to_string(), code.as_str());
        for secret in ["prompt", "token", "body"] {
            assert!(!error.to_string().contains(secret));
        }
    }
}

#[test]
fn codex_ultra_control_remains_max_and_proactive() {
    let plan = plan_codex_execution(
        "openai-codex/gpt-5.6-sol",
        ReasoningLevel::Ultra,
        CodexRequestRole::Foreground,
        None,
    )
    .unwrap();
    assert_eq!(plan.mode, CodexExecutionMode::Ultra);
    assert_eq!(plan.wire_effort, Some(CodexWireEffort::Max));
    assert_eq!(plan.multi_agent_mode, Some(CodexMultiAgentMode::Proactive));
}
