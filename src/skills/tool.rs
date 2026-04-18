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
}
