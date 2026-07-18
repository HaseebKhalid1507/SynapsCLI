//! Session-scoped worker-model authorization.
use super::super::{Tool, ToolContext};
use crate::{Result, RuntimeError};
use serde_json::{json, Value};

pub struct SubagentModelAuthorizeTool;

#[async_trait::async_trait]
impl Tool for SubagentModelAuthorizeTool {
    fn name(&self) -> &str {
        "subagent_model_authorize"
    }

    fn description(&self) -> &str {
        "Request interactive user confirmation to authorize one exact qualified model for subagent use in this session. Validate the exact runtime model locally, then show the user the identity before granting it. The grant is session-only: it does not change the foreground model or persist favorites."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "model": {
                    "type": "string",
                    "maxLength": 256,
                    "description": "Exact runtime-qualified model identity, for example anthropic/claude-sonnet-4-6."
                }
            },
            "required": ["model"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<String> {
        let model = params["model"]
            .as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing 'model' parameter".into()))?;
        if model.len() > 256 {
            return Err(RuntimeError::Tool(
                "worker-model authorization denied: model identity exceeds 256 bytes".into(),
            ));
        }
        let model = crate::orchestration::validate_user_authorizable_model(model)
            .map_err(RuntimeError::Tool)?;
        let policy = ctx
            .capabilities
            .orchestration
            .as_ref()
            .ok_or_else(|| RuntimeError::Tool("delegation policy unavailable".into()))?;
        if policy
            .effective_choices()
            .iter()
            .any(|choice| choice == model.as_str())
        {
            return Ok(json!({
                "authorized_model": model.as_str(),
                "already_authorized": true,
                "scope": "session",
                "persisted": false,
                "foreground_model": policy.foreground_model(),
                "models": policy.effective_choices()
            })
            .to_string());
        }
        let prompt = ctx.capabilities.secret_prompt.as_ref().ok_or_else(|| {
            RuntimeError::Tool(
                "worker-model authorization denied: interactive confirmation is unavailable".into(),
            )
        })?;
        let response = prompt
            .prompt(
                "Authorize worker model".to_string(),
                format!(
                    "Allow exact worker model '{}' for this session only? This will not change the foreground model or persist a favorite.\n\nType 'yes' or 'y' to allow.",
                    model.as_str()
                ),
            )
            .await;
        let confirmed = response.as_deref().map(str::trim).is_some_and(|answer| {
            answer.eq_ignore_ascii_case("yes") || answer.eq_ignore_ascii_case("y")
        });
        if !confirmed {
            return Err(RuntimeError::Tool(format!(
                "worker-model authorization confirmation denied for '{}'",
                model.as_str()
            )));
        }
        policy
            .grant_worker_model(model.as_str())
            .map_err(RuntimeError::Tool)?;

        Ok(json!({
            "authorized_model": model.as_str(),
            "scope": "session",
            "persisted": false,
            "foreground_model": policy.foreground_model(),
            "models": policy.effective_choices()
        })
        .to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_helpers::create_tool_context;

    fn context_without_sonnet() -> ToolContext {
        let mut ctx = create_tool_context();
        let foreground = agent_core::prompt::QualifiedModelId::parse("anthropic/claude-opus-4-6")
            .expect("test foreground is qualified");
        ctx.capabilities.orchestration = Some(std::sync::Arc::new(
            crate::orchestration::OrchestrationRuntime::baseline(foreground, 8, 64)
                .expect("test foreground is routable"),
        ));
        ctx
    }

    fn install_confirmation(
        ctx: &mut ToolContext,
        response: Option<&'static str>,
    ) -> tokio::task::JoinHandle<()> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        ctx.capabilities.secret_prompt = Some(crate::tools::SecretPromptHandle::new(tx));
        tokio::spawn(async move {
            let request = rx.recv().await.expect("authorization confirmation request");
            assert_eq!(request.title, "Authorize worker model");
            assert!(request.prompt.contains("anthropic/claude-sonnet-4-6"));
            assert!(request.prompt.contains("session"));
            let _ = request.response_tx.send(response.map(str::to_string));
        })
    }

    #[tokio::test]
    async fn confirmed_known_model_is_added_for_this_session() {
        let mut ctx = context_without_sonnet();
        let policy = ctx.capabilities.orchestration.clone().unwrap();
        assert!(!policy
            .effective_choices()
            .contains(&"anthropic/claude-sonnet-4-6".to_owned()));
        let responder = install_confirmation(&mut ctx, Some("yes"));

        let output = SubagentModelAuthorizeTool
            .execute(json!({"model": "anthropic/claude-sonnet-4-6"}), ctx)
            .await
            .unwrap();

        responder.await.unwrap();
        let output: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(output["authorized_model"], "anthropic/claude-sonnet-4-6");
        assert_eq!(output["scope"], "session");
        assert_eq!(output["persisted"], false);
        assert!(policy
            .effective_choices()
            .contains(&"anthropic/claude-sonnet-4-6".to_owned()));
        policy
            .resolve_and_authorize("sa_user_requested", Some("anthropic/claude-sonnet-4-6"))
            .expect("newly authorized exact model must dispatch");
    }

    #[tokio::test]
    async fn already_authorized_model_is_idempotent_without_prompting() {
        let ctx = create_tool_context();
        let policy = ctx.capabilities.orchestration.clone().unwrap();
        let before = policy.effective_choices();

        let output = SubagentModelAuthorizeTool
            .execute(json!({"model": "anthropic/claude-sonnet-4-6"}), ctx)
            .await
            .unwrap();

        let output: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(output["authorized_model"], "anthropic/claude-sonnet-4-6");
        assert_eq!(output["already_authorized"], true);
        assert_eq!(policy.effective_choices(), before);
    }

    #[tokio::test]
    async fn missing_interactive_confirmation_fails_without_mutating_policy() {
        let ctx = context_without_sonnet();
        let policy = ctx.capabilities.orchestration.clone().unwrap();
        let before = policy.effective_choices();

        let error = SubagentModelAuthorizeTool
            .execute(json!({"model": "anthropic/claude-sonnet-4-6"}), ctx)
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("interactive confirmation"), "{error}");
        assert_eq!(policy.effective_choices(), before);
    }

    #[tokio::test]
    async fn denied_confirmation_fails_without_mutating_policy() {
        let mut ctx = context_without_sonnet();
        let policy = ctx.capabilities.orchestration.clone().unwrap();
        let before = policy.effective_choices();
        let responder = install_confirmation(&mut ctx, Some("no"));

        let error = SubagentModelAuthorizeTool
            .execute(json!({"model": "anthropic/claude-sonnet-4-6"}), ctx)
            .await
            .unwrap_err()
            .to_string();

        responder.await.unwrap();
        assert!(error.contains("confirmation denied"), "{error}");
        assert_eq!(policy.effective_choices(), before);
    }

    #[tokio::test]
    async fn canceled_confirmation_fails_without_mutating_policy() {
        let mut ctx = context_without_sonnet();
        let policy = ctx.capabilities.orchestration.clone().unwrap();
        let before = policy.effective_choices();
        let responder = install_confirmation(&mut ctx, None);

        let error = SubagentModelAuthorizeTool
            .execute(json!({"model": "anthropic/claude-sonnet-4-6"}), ctx)
            .await
            .unwrap_err()
            .to_string();

        responder.await.unwrap();
        assert!(error.contains("confirmation denied"), "{error}");
        assert_eq!(policy.effective_choices(), before);
    }

    #[tokio::test]
    async fn authorization_is_exact_and_does_not_grant_sibling_models() {
        let mut ctx = context_without_sonnet();
        let policy = ctx.capabilities.orchestration.clone().unwrap();
        let responder = install_confirmation(&mut ctx, Some("y"));

        SubagentModelAuthorizeTool
            .execute(json!({"model": "anthropic/claude-sonnet-4-6"}), ctx)
            .await
            .unwrap();

        responder.await.unwrap();
        assert!(policy.preflight("anthropic/claude-sonnet-4-6").is_ok());
        assert!(policy.preflight("anthropic/claude-fable-5").is_err());
    }

    #[tokio::test]
    async fn authorization_is_not_present_in_a_fresh_session() {
        let mut ctx = context_without_sonnet();
        let policy = ctx.capabilities.orchestration.clone().unwrap();
        let responder = install_confirmation(&mut ctx, Some("yes"));

        SubagentModelAuthorizeTool
            .execute(json!({"model": "anthropic/claude-sonnet-4-6"}), ctx)
            .await
            .unwrap();
        responder.await.unwrap();
        assert!(policy.preflight("anthropic/claude-sonnet-4-6").is_ok());

        let fresh = context_without_sonnet();
        let fresh_policy = fresh.capabilities.orchestration.unwrap();
        assert!(fresh_policy
            .preflight("anthropic/claude-sonnet-4-6")
            .is_err());
    }

    #[tokio::test]
    async fn oversized_model_identity_fails_before_policy_mutation() {
        let ctx = context_without_sonnet();
        let policy = ctx.capabilities.orchestration.clone().unwrap();
        let before = policy.effective_choices();

        let error = SubagentModelAuthorizeTool
            .execute(
                json!({"model": format!("anthropic/{}", "x".repeat(300))}),
                ctx,
            )
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("256 bytes"), "{error}");
        assert_eq!(policy.effective_choices(), before);
    }

    #[tokio::test]
    async fn malformed_model_fails_closed_without_requesting_confirmation() {
        let ctx = context_without_sonnet();
        let policy = ctx.capabilities.orchestration.clone().unwrap();
        let before = policy.effective_choices();

        let error = SubagentModelAuthorizeTool
            .execute(json!({"model": "claude-sonnet-4-6"}), ctx)
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("invalid qualified model"), "{error}");
        assert_eq!(policy.effective_choices(), before);
    }

    #[tokio::test]
    async fn invented_model_fails_closed_without_requesting_confirmation() {
        let ctx = context_without_sonnet();
        let policy = ctx.capabilities.orchestration.clone().unwrap();
        let before = policy.effective_choices();

        let error = SubagentModelAuthorizeTool
            .execute(json!({"model": "anthropic/claude-invented-99"}), ctx)
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("known routable model"), "{error}");
        assert_eq!(policy.effective_choices(), before);
    }
}
