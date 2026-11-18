//! Read-only session worker catalog operation.
use super::super::{Tool, ToolContext};
use crate::{Result, RuntimeError};
use serde_json::{json, Value};

pub struct SubagentModelsTool;

/// What `subagent_models` reports for this policy and the user's
/// `subagent_model` (`configured`, already `None` when unset or `inherit`).
/// A valid configured default is trusted first, as a dispatch would, so it
/// is listed among `models`.
fn models_report(
    policy: &crate::orchestration::OrchestrationRuntime,
    configured: Option<&str>,
) -> Value {
    let configured = configured.map(|raw| (raw, policy.configured_default_model(raw)));
    let models = policy.effective_choices();
    let mut report = json!({
        "foreground_model": policy.foreground_model(),
        "models": models,
        "model_omission": "inherit_foreground"
    });
    if let Some((raw, canonical)) = configured {
        let shown = canonical
            .as_ref()
            .map_or_else(|_| raw.trim().to_owned(), |id| id.as_str().to_owned());
        let allowed = canonical.is_ok_and(|id| models.iter().any(|m| m == id.as_str()));
        report["model_omission"] = json!("configured_default");
        report["configured_default"] = json!(shown);
        report["configured_default_allowed"] = json!(allowed);
    }
    report
}

#[async_trait::async_trait]
impl Tool for SubagentModelsTool {
    fn origin(&self) -> crate::tools::ToolOrigin {
        crate::tools::ToolOrigin::Builtin
    }

    fn name(&self) -> &str {
        "subagent_models"
    }
    fn description(&self) -> &str {
        "List this session's exact trusted worker model choices. Omitting model on delegation uses the session default: config `subagent_model` when set, otherwise the foreground identity."
    }
    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}, "additionalProperties": false})
    }
    async fn execute(&self, _params: Value, ctx: ToolContext) -> Result<String> {
        let policy = ctx
            .capabilities
            .orchestration
            .as_ref()
            .ok_or_else(|| RuntimeError::Tool("delegation policy unavailable".into()))?;
        let config = super::spawn_config();
        Ok(models_report(policy, config.subagent_model.as_deref()).to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> crate::orchestration::OrchestrationRuntime {
        let foreground =
            agent_core::prompt::QualifiedModelId::parse("anthropic/claude-fable-5").unwrap();
        crate::orchestration::OrchestrationRuntime::baseline(foreground, 8, 64).unwrap()
    }

    #[test]
    fn unset_default_reports_foreground_inheritance_unchanged() {
        let report = models_report(&policy(), None);
        assert_eq!(
            report,
            json!({
                "foreground_model": "anthropic/claude-fable-5",
                "models": ["anthropic/claude-fable-5"],
                "model_omission": "inherit_foreground"
            })
        );
    }

    #[test]
    fn configured_default_is_reported_qualified_listed_and_allowed() {
        let policy = policy();
        let report = models_report(&policy, Some("claude-fable-5"));
        assert_eq!(report["model_omission"], "configured_default");
        assert_eq!(report["configured_default"], "anthropic/claude-fable-5");
        assert_eq!(report["configured_default_allowed"], true);
        assert_eq!(report["foreground_model"], "anthropic/claude-fable-5");

        let report = models_report(&policy, Some("openai-codex/gpt-5.6-sol"));
        assert_eq!(report["configured_default"], "openai-codex/gpt-5.6-sol");
        assert_eq!(report["configured_default_allowed"], true);
        assert!(report["models"]
            .as_array()
            .unwrap()
            .contains(&json!("openai-codex/gpt-5.6-sol")));
    }

    #[test]
    fn invented_configured_default_is_shown_not_listed_or_allowed() {
        let policy = policy();
        for (raw, shown) in [
            ("anthropic/claude-invented-99", "anthropic/claude-invented-99"),
            ("foo/", "foo/"),
        ] {
            let report = models_report(&policy, Some(raw));
            assert_eq!(report["model_omission"], "configured_default");
            assert_eq!(report["configured_default"], shown);
            assert_eq!(report["configured_default_allowed"], false);
            assert_eq!(report["models"], json!(["anthropic/claude-fable-5"]));
        }
    }

    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn tool_reads_subagent_model_from_config() {
        let base = crate::test_env::BaseDirGuard::new();
        let ctx = crate::tools::test_helpers::create_tool_context;
        let out = SubagentModelsTool.execute(json!({}), ctx()).await.unwrap();
        let report: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(report["model_omission"], "inherit_foreground");
        assert!(report.get("configured_default").is_none());

        std::fs::write(base.path().join("config"), "subagent_model = claude-opus-4-6\n").unwrap();
        let out = SubagentModelsTool.execute(json!({}), ctx()).await.unwrap();
        let report: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(report["model_omission"], "configured_default");
        assert_eq!(report["configured_default"], "anthropic/claude-opus-4-6");
        assert_eq!(report["configured_default_allowed"], true);
        assert!(report["models"].as_array().unwrap().contains(&json!("anthropic/claude-opus-4-6")));
    }
}
