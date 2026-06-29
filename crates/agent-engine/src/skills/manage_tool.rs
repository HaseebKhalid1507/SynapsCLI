//! `skill_manage` tool — model-initiated skill authorship.
//!
//! Companion to `tool.rs::LoadSkillTool` (read path). This is the write
//! path: create / update / delete a loose skill under
//! `~/.synaps-cli/skills/<name>/`, atomically. Indexing is **passive**:
//! Axel's consolidation walks `~/.synaps-cli/skills/` as a registered
//! source root (RATIFIED DECISION #1), so this tool makes zero Axel RPCs.
//!
//! Zero-regression contract with `loader.rs::load_skill_file` (lines 41–73):
//! we only ever write `SKILL.md` files whose frontmatter contains exactly
//! `name` and `description`, so the loader's required-field checks pass
//! byte-identically.

use std::sync::Arc;

use serde_json::json;

use crate::{
    skills::{
        registry::CommandRegistry,
        sidecar::{Provenance, SkillMeta},
        writer,
    },
    RuntimeError, SynapsConfig, Tool, ToolContext,
};

pub struct SkillManageTool {
    registry: Arc<CommandRegistry>,
    config: Arc<SynapsConfig>,
}

impl SkillManageTool {
    pub fn new(registry: Arc<CommandRegistry>, config: Arc<SynapsConfig>) -> Self {
        Self { registry, config }
    }
}

#[async_trait::async_trait]
impl Tool for SkillManageTool {
    fn name(&self) -> &str {
        "skill_manage"
    }

    fn description(&self) -> &str {
        "Author or improve a skill. action ∈ {create, update, delete}. \
         Creates a new SKILL.md under ~/.synaps-cli/skills/<name>/ (create), \
         atomically rewrites it (update), or archives it (delete). \
         The skill becomes searchable in Axel on the next consolidation cycle."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["create", "update", "delete"]
                },
                "name": {
                    "type": "string",
                    "description": "Skill name; [a-z0-9][a-z0-9-]{0,63}"
                },
                "description": {
                    "type": "string",
                    "description": "≤200 chars; required on create, optional on update"
                },
                "body": {
                    "type": "string",
                    "description": "Markdown body (post-frontmatter); required on create/update"
                },
                "provenance": {
                    "type": "string",
                    "enum": ["user", "learn", "background_review"],
                    "default": "background_review"
                }
            },
            "required": ["action", "name"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: ToolContext,
    ) -> crate::Result<String> {
        let action = params["action"]
            .as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing 'action'".into()))?;

        let name = params["name"]
            .as_str()
            .ok_or_else(|| RuntimeError::Tool("Missing 'name'".into()))?;

        writer::validate_name(name)
            .map_err(|e| RuntimeError::Tool(format!("invalid name: {e}")))?;

        // M4: acquire the per-skill lock BEFORE ensure_writable / skill_exists
        // — closes the TOCTOU between the existence check and the write.
        let _guard = writer::lock_skill(name)
            .map_err(|e| RuntimeError::Tool(format!("lock: {e}")))?;

        // Reject plugin-owned skills (RATIFIED DECISION #4)
        writer::ensure_writable(&self.registry, name)
            .map_err(|e| RuntimeError::Tool(e.to_string()))?;

        let outcome = match action {
            "create" => {
                let desc = params["description"].as_str().ok_or_else(|| {
                    RuntimeError::Tool("create: missing description".into())
                })?;
                writer::validate_description(desc)
                    .map_err(|e| RuntimeError::Tool(format!("invalid description: {e}")))?;
                let body = params["body"]
                    .as_str()
                    .ok_or_else(|| RuntimeError::Tool("create: missing body".into()))?;

                if writer::skill_exists(name) {
                    return Err(RuntimeError::Tool(format!(
                        "skill '{name}' already exists; use action=update"
                    )));
                }

                let provenance = Provenance::parse(params["provenance"].as_str());
                writer::write_skill_md_atomic(name, desc, body)?;
                SkillMeta::create(name, provenance)?;
                "created"
            }

            "update" => {
                if !writer::skill_exists(name) {
                    return Err(RuntimeError::Tool(format!(
                        "skill '{name}' not found; use action=create to create it"
                    )));
                }

                let body = params["body"]
                    .as_str()
                    .ok_or_else(|| RuntimeError::Tool("update: missing body".into()))?;

                // M3: distinguish ABSENT (preserve existing) from EMPTY (reject).
                // `as_str()` returns Some("") for "description":"" — treat
                // empty as a validation error, missing key as "keep old".
                let desc = match params.get("description") {
                    None | Some(serde_json::Value::Null) => writer::read_description(name)?,
                    Some(serde_json::Value::String(s)) => {
                        writer::validate_description(s).map_err(|e| {
                            RuntimeError::Tool(format!("invalid description: {e}"))
                        })?;
                        s.clone()
                    }
                    Some(_) => {
                        return Err(RuntimeError::Tool(
                            "update: 'description' must be a string".into(),
                        ));
                    }
                };

                writer::write_skill_md_atomic(name, &desc, body)?;
                SkillMeta::touch(name)?;
                "updated"
            }

            "delete" => {
                writer::archive_skill(name)?;
                "deleted"
            }

            other => {
                return Err(RuntimeError::Tool(format!("unknown action '{other}'")));
            }
        };

        // Refresh registry so new/updated/deleted skill is immediately resolvable
        crate::skills::reload_registry(&self.registry, &self.config);

        Ok(format!("skill_manage: {action} {name} → {outcome}"))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::Arc;
    use serial_test::serial;
    use tempfile::TempDir;

    fn test_ctx() -> ToolContext {
        ToolContext {
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

    fn set_home(dir: &Path) -> String {
        let old = std::env::var("HOME").unwrap_or_default();
        unsafe { std::env::set_var("HOME", dir) };
        old
    }

    fn restore_home(old: &str) {
        if old.is_empty() {
            unsafe { std::env::remove_var("HOME") };
        } else {
            unsafe { std::env::set_var("HOME", old) };
        }
    }

    #[test]
    fn parameters_schema_is_well_formed() {
        let registry = Arc::new(CommandRegistry::new(&[], vec![]));
        let config = Arc::new(SynapsConfig::default());
        let tool = SkillManageTool::new(registry, config);
        let schema = tool.parameters();
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["action"].is_object());
        assert!(schema["properties"]["name"].is_object());
        assert_eq!(schema["required"], json!(["action", "name"]));
    }

    #[tokio::test]
    #[serial]
    async fn create_then_create_same_name_errors() {
        let tmp = TempDir::new().unwrap();
        let old = set_home(tmp.path());

        let registry = Arc::new(CommandRegistry::new(&[], vec![]));
        let config = Arc::new(SynapsConfig::default());
        let tool = SkillManageTool::new(registry, config);

        tool.execute(
            json!({"action": "create", "name": "dup", "description": "d", "body": "# B"}),
            test_ctx(),
        )
        .await
        .unwrap();

        let err = tool
            .execute(
                json!({"action": "create", "name": "dup", "description": "d", "body": "# B"}),
                test_ctx(),
            )
            .await
            .unwrap_err();

        restore_home(&old);
        assert!(
            format!("{err}").contains("already exists"),
            "got: {err}"
        );
    }

    #[tokio::test]
    #[serial]
    async fn update_on_missing_skill_errors() {
        let tmp = TempDir::new().unwrap();
        let old = set_home(tmp.path());

        let registry = Arc::new(CommandRegistry::new(&[], vec![]));
        let config = Arc::new(SynapsConfig::default());
        let tool = SkillManageTool::new(registry, config);

        let err = tool
            .execute(
                json!({"action": "update", "name": "no-such", "body": "# B"}),
                test_ctx(),
            )
            .await
            .unwrap_err();

        restore_home(&old);
        assert!(
            format!("{err}").contains("not found"),
            "got: {err}"
        );
    }

    #[tokio::test]
    #[serial]
    async fn delete_on_missing_skill_errors() {
        let tmp = TempDir::new().unwrap();
        let old = set_home(tmp.path());

        let registry = Arc::new(CommandRegistry::new(&[], vec![]));
        let config = Arc::new(SynapsConfig::default());
        let tool = SkillManageTool::new(registry, config);

        let err = tool
            .execute(
                json!({"action": "delete", "name": "ghost"}),
                test_ctx(),
            )
            .await
            .unwrap_err();

        restore_home(&old);
        assert!(
            format!("{err}").contains("not found"),
            "got: {err}"
        );
    }

    #[tokio::test]
    #[serial]
    async fn provenance_defaults_to_background_review() {
        let tmp = TempDir::new().unwrap();
        let old = set_home(tmp.path());

        let registry = Arc::new(CommandRegistry::new(&[], vec![]));
        let config = Arc::new(SynapsConfig::default());
        let tool = SkillManageTool::new(registry, config);

        tool.execute(
            json!({"action": "create", "name": "prov-test", "description": "d", "body": "# B"}),
            test_ctx(),
        )
        .await
        .unwrap();

        let meta = SkillMeta::read("prov-test").unwrap();
        restore_home(&old);

        assert_eq!(meta.provenance, Provenance::BackgroundReview);
    }

    #[tokio::test]
    #[serial]
    async fn plugin_owned_skill_rejected() {
        use crate::skills::LoadedSkill;
        use std::path::PathBuf;

        let tmp = TempDir::new().unwrap();
        let old = set_home(tmp.path());

        // Seed a plugin-owned skill in the registry
        let plugin_skill = LoadedSkill {
            name: "plugin-skill".to_string(),
            description: "d".to_string(),
            body: "b".to_string(),
            plugin: Some("my-plugin".to_string()),
            base_dir: PathBuf::from("/"),
            source_path: PathBuf::from("/plugins/my-plugin/skills/plugin-skill/SKILL.md"),
        };
        let registry = Arc::new(CommandRegistry::new(&[], vec![plugin_skill]));
        let config = Arc::new(SynapsConfig::default());
        let tool = SkillManageTool::new(registry, config);

        let err = tool
            .execute(
                json!({"action": "create", "name": "plugin-skill", "description": "d", "body": "b"}),
                test_ctx(),
            )
            .await
            .unwrap_err();

        restore_home(&old);
        assert!(
            format!("{err}").contains("plugin-owned"),
            "got: {err}"
        );
    }

    // --- B1: frontmatter injection refused at tool boundary -------------------

    #[tokio::test]
    #[serial]
    async fn create_rejects_injected_description() {
        let tmp = TempDir::new().unwrap();
        let old = set_home(tmp.path());

        let registry = Arc::new(CommandRegistry::new(&[], vec![]));
        let config = Arc::new(SynapsConfig::default());
        let tool = SkillManageTool::new(registry, config);

        let err = tool
            .execute(
                json!({
                    "action": "create",
                    "name": "inj",
                    "description": "evil\n---\nbody-pwn",
                    "body": "b"
                }),
                test_ctx(),
            )
            .await
            .unwrap_err();

        let on_disk = tmp.path().join(".synaps-cli/skills/inj/SKILL.md");
        let exists = on_disk.exists();
        restore_home(&old);

        assert!(format!("{err}").to_lowercase().contains("description"), "got: {err}");
        assert!(!exists, "no SKILL.md should be written on rejection");
    }

    // --- M3: update description handling --------------------------------------

    #[tokio::test]
    #[serial]
    async fn update_with_no_description_preserves_existing() {
        let tmp = TempDir::new().unwrap();
        let old = set_home(tmp.path());

        let registry = Arc::new(CommandRegistry::new(&[], vec![]));
        let config = Arc::new(SynapsConfig::default());
        let tool = SkillManageTool::new(registry, config);

        tool.execute(
            json!({"action": "create", "name": "preserve", "description": "Original", "body": "# b"}),
            test_ctx(),
        ).await.unwrap();

        tool.execute(
            json!({"action": "update", "name": "preserve", "body": "# b2"}),
            test_ctx(),
        ).await.unwrap();

        let desc = writer::read_description("preserve").unwrap();
        restore_home(&old);
        assert_eq!(desc, "Original");
    }

    #[tokio::test]
    #[serial]
    async fn update_with_empty_description_errors() {
        let tmp = TempDir::new().unwrap();
        let old = set_home(tmp.path());

        let registry = Arc::new(CommandRegistry::new(&[], vec![]));
        let config = Arc::new(SynapsConfig::default());
        let tool = SkillManageTool::new(registry, config);

        tool.execute(
            json!({"action": "create", "name": "empty-desc", "description": "Original", "body": "# b"}),
            test_ctx(),
        ).await.unwrap();

        let err = tool.execute(
            json!({"action": "update", "name": "empty-desc", "description": "", "body": "# b"}),
            test_ctx(),
        ).await.unwrap_err();

        // Description must still be Original (write was refused).
        let desc = writer::read_description("empty-desc").unwrap();
        restore_home(&old);
        assert!(format!("{err}").to_lowercase().contains("description"), "got: {err}");
        assert_eq!(desc, "Original", "failed update must not corrupt existing description");
    }
}
