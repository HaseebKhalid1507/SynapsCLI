//! `load_skill` tool — model-initiated skill activation.

use std::sync::Arc;
use serde_json::json;
use crate::skills::{LoadedSkill, registry::{CommandRegistry, Resolution}};

pub struct LoadSkillTool {
    registry: Arc<CommandRegistry>,
}

impl LoadSkillTool {
    pub fn new(registry: Arc<CommandRegistry>) -> Self {
        Self { registry }
    }

    /// Produce the tool-result body for a successfully loaded skill.
    /// Shared between user-initiated (slash) and model-initiated (tool) paths.
    pub fn format_body(skill: &LoadedSkill) -> String {
        format!(
            "# Skill: {} — {}\n\nFollow these guidelines for the rest of this conversation.\n\n{}",
            skill.name, skill.description, skill.body
        )
    }
}

#[async_trait::async_trait]
impl crate::Tool for LoadSkillTool {
    fn name(&self) -> &str { "load_skill" }

    fn description(&self) -> &str {
        "Load a skill to guide your behavior for the current conversation. \
         Skills provide structured guidelines, checklists, and best practices. \
         Call this when a task would benefit from a specific methodology."
    }

    fn parameters(&self) -> serde_json::Value {
        let list: Vec<String> = self.registry.all_skills().iter()
            .map(|s| {
                let qualified = match &s.plugin {
                    Some(p) => format!("{}:{} — {}", p, s.name, s.description),
                    None => format!("{} — {}", s.name, s.description),
                };
                qualified
            })
            .collect();
        json!({
            "type": "object",
            "properties": {
                "skill": {
                    "type": "string",
                    "description": format!("Name of the skill to load (bare or plugin:skill). Available:\n{}", list.join("\n"))
                }
            },
            "required": ["skill"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: crate::ToolContext,
    ) -> crate::Result<String> {
        let name = params["skill"].as_str()
            .ok_or_else(|| crate::RuntimeError::Tool("Missing 'skill' parameter".to_string()))?;

        match self.registry.resolve(name) {
            Resolution::Skill(s) => Ok(Self::format_body(&s)),
            Resolution::Ambiguous(opts) => Err(crate::RuntimeError::Tool(format!(
                "ambiguous skill '{}'; specify one of: {}", name, opts.join(", ")
            ))),
            Resolution::Builtin | Resolution::Unknown => Err(crate::RuntimeError::Tool(
                format!("unknown skill '{}'", name)
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_ctx() -> crate::ToolContext {
        crate::ToolContext {
            tx_delta: None,
            tx_events: None,
            watcher_exit_path: None,
            tool_register_tx: None,
            session_manager: None,
            max_tool_output: 30000,
            bash_timeout: 30,
            bash_max_timeout: 300,
            subagent_timeout: 300,
            subagent_registry: None,
            event_queue: None,
        }
    }

    fn mk(name: &str, plugin: Option<&str>) -> LoadedSkill {
        LoadedSkill {
            name: name.to_string(),
            description: format!("desc-{name}"),
            body: format!("body-{name}"),
            plugin: plugin.map(str::to_string),
            base_dir: PathBuf::from("/"),
            source_path: PathBuf::from("/SKILL.md"),
        }
    }

    #[test]
    fn format_body_includes_name_and_description() {
        let s = LoadedSkill {
            name: "x".into(),
            description: "y".into(),
            body: "z".into(),
            plugin: None,
            base_dir: PathBuf::from("/"),
            source_path: PathBuf::from("/SKILL.md"),
        };
        let out = LoadSkillTool::format_body(&s);
        assert!(out.contains("x"));
        assert!(out.contains("y"));
        assert!(out.contains("z"));
        assert!(out.contains("Follow these guidelines"));
    }

    #[tokio::test]
    async fn execute_returns_skill_body_on_unique_match() {
        use crate::Tool;
        let reg = Arc::new(crate::skills::registry::CommandRegistry::new(
            &[], vec![mk("search", Some("p1"))]
        ));
        let tool = LoadSkillTool::new(reg);
        let result = tool.execute(
            serde_json::json!({"skill": "search"}),
            test_ctx()
        ).await.unwrap();
        assert!(result.contains("# Skill: search"));
        assert!(result.contains("desc-search"));
        assert!(result.contains("body-search"));
    }

    #[tokio::test]
    async fn execute_errors_on_ambiguous() {
        use crate::Tool;
        let reg = Arc::new(crate::skills::registry::CommandRegistry::new(
            &[], vec![mk("search", Some("p1")), mk("search", Some("p2"))]
        ));
        let tool = LoadSkillTool::new(reg);
        let err = tool.execute(
            serde_json::json!({"skill": "search"}),
            test_ctx()
        ).await.unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("ambiguous"));
        assert!(msg.contains("p1:search"));
        assert!(msg.contains("p2:search"));
    }

    #[tokio::test]
    async fn execute_errors_on_unknown() {
        use crate::Tool;
        let reg = Arc::new(crate::skills::registry::CommandRegistry::new(&[], vec![]));
        let tool = LoadSkillTool::new(reg);
        let err = tool.execute(
            serde_json::json!({"skill": "nosuch"}),
            test_ctx()
        ).await.unwrap_err();
        assert!(format!("{err}").contains("unknown skill 'nosuch'"));
    }

    #[tokio::test]
    async fn execute_errors_on_builtin() {
        use crate::Tool;
        // A built-in is not a skill; load_skill should refuse to load it.
        let reg = Arc::new(crate::skills::registry::CommandRegistry::new(&["clear"], vec![]));
        let tool = LoadSkillTool::new(reg);
        let err = tool.execute(
            serde_json::json!({"skill": "clear"}),
            test_ctx()
        ).await.unwrap_err();
        assert!(format!("{err}").contains("unknown skill 'clear'"));
    }

    #[tokio::test]
    async fn execute_errors_on_missing_skill_param() {
        use crate::Tool;
        let reg = Arc::new(crate::skills::registry::CommandRegistry::new(&[], vec![]));
        let tool = LoadSkillTool::new(reg);
        let err = tool.execute(
            serde_json::json!({}),
            test_ctx()
        ).await.unwrap_err();
        assert!(format!("{err}").contains("Missing 'skill' parameter"));
    }

    #[test]
    fn parameters_schema_is_well_formed() {
        use crate::Tool;
        let reg = Arc::new(crate::skills::registry::CommandRegistry::new(&[], vec![]));
        let tool = LoadSkillTool::new(reg);
        let schema = tool.parameters();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["skill"]["type"], "string");
        assert_eq!(schema["required"], serde_json::json!(["skill"]));
    }
}
