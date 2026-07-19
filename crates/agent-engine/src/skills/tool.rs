//! `load_skill` tool — model-initiated skill activation — plus the Task 17
//! `search_skills` bounded discovery tool (stable skill IDs + compact
//! descriptions only; no bodies, no source paths, no process/network).

use crate::skills::{
    looks_like_stable_skill_id,
    registry::{CommandRegistry, Resolution},
    stable_skill_id, LoadedSkill,
};
use serde_json::json;
use std::sync::Arc;

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
    fn name(&self) -> &str {
        "load_skill"
    }

    fn description(&self) -> &str {
        "Load a skill to guide your behavior for the current conversation. \
         Skills provide structured guidelines, checklists, and best practices. \
         Call this when a task would benefit from a specific methodology."
    }

    /// Compiled into this runtime: `skills::register` adds this tool from
    /// core code, so its catalog identity is `builtin:load_skill` with
    /// builtin-runtime provenance — never a conservative unknown.
    fn origin(&self) -> crate::tools::ToolOrigin {
        crate::tools::ToolOrigin::Builtin
    }

    fn parameters(&self) -> serde_json::Value {
        let list: Vec<String> = self
            .registry
            .all_skills()
            .iter()
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
        let name = params["skill"]
            .as_str()
            .ok_or_else(|| crate::RuntimeError::Tool("Missing 'skill' parameter".to_string()))?;

        // Stable-ID path first (Task 17): inputs spelled in the reserved
        // `skill:`/`skill.<plugin>:` namespace resolve by EXACT deterministic
        // id. Exactly one match loads that skill and nothing else; zero
        // matches fail typed; duplicate ids fail closed as ambiguous rather
        // than guessing. Legacy exact qualified (`plugin:skill`) and
        // unambiguous bare names keep resolving through the registry below.
        if looks_like_stable_skill_id(name) {
            let matches: Vec<Arc<LoadedSkill>> = self
                .registry
                .all_skills()
                .into_iter()
                .filter(|s| stable_skill_id(s) == name)
                .collect();
            return match matches.as_slice() {
                [] => Err(crate::RuntimeError::Tool(format!(
                    "unknown skill id '{}'",
                    crate::BoundedText::new(name, 160).text
                ))),
                [skill] => Ok(Self::format_body(skill)),
                _ => Err(crate::RuntimeError::Tool(format!(
                    "ambiguous skill id '{}'",
                    crate::BoundedText::new(name, 160).text
                ))),
            };
        }

        match self.registry.resolve(name) {
            Resolution::Skill(s) => Ok(Self::format_body(&s)),
            Resolution::Ambiguous(opts) => Err(crate::RuntimeError::Tool(format!(
                "ambiguous skill '{}'; specify one of: {}",
                name,
                opts.join(", ")
            ))),
            Resolution::PluginCommand(_) | Resolution::Builtin | Resolution::Unknown => Err(
                crate::RuntimeError::Tool(format!("unknown skill '{}'", name)),
            ),
        }
    }
}

// ── search_skills (Task 17) ─────────────────────────────────────────────────

/// Byte budget for one compact skill description in search results.
const SKILL_DESCRIPTION_MAX_BYTES: usize = 160;
/// Maximum number of skills returned by one search.
const SEARCH_SKILLS_MAX_RESULTS: usize = 16;
/// Serialized byte budget for the returned skill collection.
const SEARCH_SKILLS_MAX_RESULT_BYTES: usize = 8 * 1024;

/// Bounded, deterministic skill discovery: stable skill IDs plus compact
/// bounded descriptions ONLY. Never returns bodies or source paths, starts
/// no process, performs no network access, and reads only the in-memory
/// registry snapshot.
pub struct SearchSkillsTool {
    registry: Arc<CommandRegistry>,
}

impl SearchSkillsTool {
    pub fn new(registry: Arc<CommandRegistry>) -> Self {
        Self { registry }
    }
}

#[async_trait::async_trait]
impl crate::Tool for SearchSkillsTool {
    fn name(&self) -> &str {
        "search_skills"
    }

    fn description(&self) -> &str {
        "Search available skills by keyword. Returns bounded stable skill \
         ids with compact descriptions. Pass a returned id to load_skill to \
         load exactly that skill."
    }

    /// Compiled into this runtime and registered by `skills::register`.
    fn origin(&self) -> crate::tools::ToolOrigin {
        crate::tools::ToolOrigin::Builtin
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Case-insensitive keyword matched against skill ids, names, and descriptions."
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: crate::ToolContext,
    ) -> crate::Result<String> {
        let raw = params["query"].as_str().ok_or_else(|| {
            crate::RuntimeError::Tool("Missing 'query' parameter (string)".to_string())
        })?;
        // Reuse the discovery-query boundary parser: empty, oversized, and
        // control-character queries fail typed and bounded.
        let query = crate::tools::catalog::DiscoveryQuery::parse(raw)
            .map_err(|err| crate::RuntimeError::Tool(format!("invalid skill query: {err}")))?;

        // Deterministic candidate order: stable id, then plugin/name for
        // (unexpected) duplicate ids.
        let mut candidates: Vec<(String, Arc<LoadedSkill>)> = self
            .registry
            .all_skills()
            .into_iter()
            .map(|s| (stable_skill_id(&s), s))
            .collect();
        candidates.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then_with(|| a.1.plugin.cmp(&b.1.plugin))
                .then_with(|| a.1.name.cmp(&b.1.name))
        });

        let mut hits = Vec::new();
        // Serialized cost of `hits` as a compact JSON array: `[]` is 2
        // bytes; each entry adds its own bytes plus a separating comma.
        let mut used_bytes = 2usize;
        let mut truncated = false;
        for (id, skill) in candidates {
            let description =
                crate::BoundedText::new(&skill.description, SKILL_DESCRIPTION_MAX_BYTES).text;
            let haystack = format!(
                "{}\n{}\n{}",
                id.to_lowercase(),
                skill.name.to_lowercase(),
                description.to_lowercase()
            );
            if !haystack.contains(query.needle()) {
                continue;
            }
            let entry = json!({
                "id": id,
                "name": skill.name,
                "description": description,
            });
            let entry_bytes = serde_json::to_vec(&entry)
                .map_err(|err| {
                    crate::RuntimeError::Tool(format!("failed to serialize skill entry: {err}"))
                })?
                .len();
            let separator = usize::from(!hits.is_empty());
            let next_bytes = used_bytes + separator + entry_bytes;
            if hits.len() == SEARCH_SKILLS_MAX_RESULTS
                || next_bytes > SEARCH_SKILLS_MAX_RESULT_BYTES
            {
                truncated = true;
                break;
            }
            used_bytes = next_bytes;
            hits.push(entry);
        }

        let body = json!({"truncated": truncated, "skills": hits});
        serde_json::to_string(&body).map_err(|err| {
            crate::RuntimeError::Tool(format!("failed to serialize skill search results: {err}"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_ctx() -> crate::ToolContext {
        crate::ToolContext {
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
                max_tool_output: 30000,
                max_tool_buffer: 256 * 1024,
                bash_timeout: 30,
                bash_max_timeout: 300,
                subagent_timeout: 300,
            },
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
            &[],
            vec![mk("search", Some("p1"))],
        ));
        let tool = LoadSkillTool::new(reg);
        let result = tool
            .execute(serde_json::json!({"skill": "search"}), test_ctx())
            .await
            .unwrap();
        assert!(result.contains("# Skill: search"));
        assert!(result.contains("desc-search"));
        assert!(result.contains("body-search"));
    }

    #[tokio::test]
    async fn execute_errors_on_ambiguous() {
        use crate::Tool;
        let reg = Arc::new(crate::skills::registry::CommandRegistry::new(
            &[],
            vec![mk("search", Some("p1")), mk("search", Some("p2"))],
        ));
        let tool = LoadSkillTool::new(reg);
        let err = tool
            .execute(serde_json::json!({"skill": "search"}), test_ctx())
            .await
            .unwrap_err();
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
        let err = tool
            .execute(serde_json::json!({"skill": "nosuch"}), test_ctx())
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("unknown skill 'nosuch'"));
    }

    #[tokio::test]
    async fn execute_errors_on_builtin() {
        use crate::Tool;
        // A built-in is not a skill; load_skill should refuse to load it.
        let reg = Arc::new(crate::skills::registry::CommandRegistry::new(
            &["clear"],
            vec![],
        ));
        let tool = LoadSkillTool::new(reg);
        let err = tool
            .execute(serde_json::json!({"skill": "clear"}), test_ctx())
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("unknown skill 'clear'"));
    }

    #[tokio::test]
    async fn execute_errors_on_missing_skill_param() {
        use crate::Tool;
        let reg = Arc::new(crate::skills::registry::CommandRegistry::new(&[], vec![]));
        let tool = LoadSkillTool::new(reg);
        let err = tool
            .execute(serde_json::json!({}), test_ctx())
            .await
            .unwrap_err();
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

    /// Production-shaped: `skills::register` puts `load_skill` into the live
    /// tool registry. As a compiled-in capability it must catalog as
    /// `builtin:load_skill` with builtin-runtime provenance — never as an
    /// unverified unknown.
    #[test]
    fn load_skill_catalogs_as_builtin_capability() {
        use crate::tools::catalog::{CapabilitySource, ToolId, TrustProvenance};
        let reg = Arc::new(crate::skills::registry::CommandRegistry::new(
            &[],
            vec![mk("search", Some("p1"))],
        ));
        let mut tools = crate::tools::ToolRegistry::empty();
        tools.register(Arc::new(LoadSkillTool::new(reg)));

        let record = tools
            .catalog()
            .get(&ToolId::builtin("load_skill"))
            .expect("load_skill cataloged under the builtin namespace");
        assert_eq!(record.source(), &CapabilitySource::Builtin);
        assert_eq!(record.provenance(), &TrustProvenance::BuiltinRuntime);
        assert!(
            tools
                .catalog()
                .get(&ToolId::unclassified("load_skill"))
                .is_none(),
            "load_skill must not be cataloged as unknown"
        );
        // Registration/exposure behavior unchanged.
        assert!(tools.get("load_skill").is_some());
        assert_eq!(tools.tools_schema().len(), 1);
    }
}
