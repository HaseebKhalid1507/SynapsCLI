use std::path::PathBuf;

/// A skill loaded from a markdown file.
#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub content: String,
}

/// Parse YAML frontmatter from a markdown file.
/// Returns (frontmatter_fields, body_content).
fn parse_frontmatter(text: &str) -> (Vec<(String, String)>, String) {
    if !text.starts_with("---") {
        return (vec![], text.to_string());
    }

    if let Some(end) = text[3..].find("\n---") {
        let frontmatter_str = &text[3..3 + end];
        let body = text[3 + end + 4..].trim().to_string();

        let fields: Vec<(String, String)> = frontmatter_str
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                if line.is_empty() { return None; }
                let (key, val) = line.split_once(':')?;
                Some((key.trim().to_string(), val.trim().trim_matches('"').to_string()))
            })
            .collect();

        (fields, body)
    } else {
        (vec![], text.to_string())
    }
}

/// Load a single skill from a markdown file.
fn load_skill(path: &PathBuf) -> Option<Skill> {
    let content = std::fs::read_to_string(path).ok()?;
    let (fields, body) = parse_frontmatter(&content);

    let file_stem = path.file_stem()?.to_str()?.to_string();

    let name = fields.iter()
        .find(|(k, _)| k == "name")
        .map(|(_, v)| v.clone())
        .unwrap_or(file_stem);

    let description = fields.iter()
        .find(|(k, _)| k == "description")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();

    if body.is_empty() {
        return None;
    }

    Some(Skill { name, description, content: body })
}

/// Scan a directory for skill files (.md).
fn scan_skills_dir(dir: &PathBuf) -> Vec<Skill> {
    let mut skills = Vec::new();

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return skills,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("md") {
            if let Some(skill) = load_skill(&path) {
                skills.push(skill);
            }
        }
    }

    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

/// Load all skills from the configured directories.
/// Search order:
///   1. Project-local: .synaps-cli/skills/
///   2. Global: ~/.synaps-cli/skills/
///   3. Profile: ~/.synaps-cli/<profile>/skills/
///
/// If `filter` is provided (from config `skills = rust, security`),
/// only skills with matching names are loaded.
pub fn load_skills(filter: Option<&[String]>) -> Vec<Skill> {
    let mut all_skills = Vec::new();
    let mut seen_names = std::collections::HashSet::new();

    // Project-local skills (highest priority)
    let local_dir = PathBuf::from(".synaps-cli/skills");
    for skill in scan_skills_dir(&local_dir) {
        if !seen_names.contains(&skill.name) {
            seen_names.insert(skill.name.clone());
            all_skills.push(skill);
        }
    }

    // Global/profile skills
    let global_dir = crate::config::resolve_read_path("skills");
    for skill in scan_skills_dir(&global_dir) {
        if !seen_names.contains(&skill.name) {
            seen_names.insert(skill.name.clone());
            all_skills.push(skill);
        }
    }

    // Apply filter if specified
    if let Some(names) = filter {
        let filter_set: std::collections::HashSet<&str> = names.iter().map(|s| s.as_str()).collect();
        all_skills.retain(|s| filter_set.contains(s.name.as_str()));
    }

    all_skills
}

/// Format skills into a block that gets appended to the system prompt.
pub fn format_skills_for_prompt(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return String::new();
    }

    let mut output = String::from("\n\n# Active Skills\n\n");

    for skill in skills {
        output.push_str(&format!("## {}", skill.name));
        if !skill.description.is_empty() {
            output.push_str(&format!(" — {}", skill.description));
        }
        output.push('\n');
        output.push_str(&skill.content);
        output.push_str("\n\n");
    }

    output
}

/// Parse the `skills` config value (comma-separated names).
pub fn parse_skills_config(val: &str) -> Vec<String> {
    val.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

// ── On-Demand Skill Loading ─────────────────────────────────────────────

use std::sync::Arc;
use serde_json::json;

/// A tool that loads skills on demand. Returns skill content as the tool result.
/// The model reads the content and follows it for the rest of the conversation.
pub struct LoadSkillTool {
    skills: Vec<Skill>,
}

#[async_trait::async_trait]
impl crate::Tool for LoadSkillTool {
    fn name(&self) -> &str { "load_skill" }

    fn description(&self) -> &str {
        "Load a skill to guide your behavior for the current conversation. Skills provide structured guidelines, checklists, and best practices. Call this when a task would benefit from a specific methodology."
    }

    fn parameters(&self) -> serde_json::Value {
        let skill_list: Vec<String> = self.skills.iter()
            .map(|s| format!("{} — {}", s.name, s.description))
            .collect();
        json!({
            "type": "object",
            "properties": {
                "skill": {
                    "type": "string",
                    "description": format!("Name of the skill to load. Available:\n{}", skill_list.join("\n"))
                }
            },
            "required": ["skill"]
        })
    }

    async fn execute(&self, params: serde_json::Value, _ctx: crate::ToolContext) -> crate::Result<String> {
        let skill_name = params["skill"].as_str()
            .ok_or_else(|| crate::RuntimeError::Tool("Missing 'skill' parameter".to_string()))?;

        let skill = self.skills.iter()
            .find(|s| s.name == skill_name)
            .ok_or_else(|| {
                let available: Vec<&str> = self.skills.iter().map(|s| s.name.as_str()).collect();
                crate::RuntimeError::Tool(format!(
                    "Unknown skill '{}'. Available: {}", skill_name, available.join(", ")
                ))
            })?;

        Ok(format!(
            "# Skill: {} — {}\n\nFollow these guidelines for the rest of this conversation.\n\n{}",
            skill.name, skill.description, skill.content
        ))
    }
}

/// Register the load_skill tool with all available skills.
/// Returns the number of available skills.
pub async fn setup_skill_tool(registry: &Arc<tokio::sync::RwLock<crate::ToolRegistry>>) -> usize {
    let skills = load_skills(None);
    if skills.is_empty() {
        return 0;
    }

    let count = skills.len();
    let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
    tracing::info!(skills = ?names, "Skills available for on-demand loading");

    let tool = LoadSkillTool { skills };
    registry.write().await.register(Arc::new(tool));

    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_frontmatter_valid() {
        let text = "---\nname: test\ndescription: Test skill\n---\nThis is the content";
        let (fields, body) = parse_frontmatter(text);
        
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0], ("name".to_string(), "test".to_string()));
        assert_eq!(fields[1], ("description".to_string(), "Test skill".to_string()));
        assert_eq!(body, "This is the content");
    }

    #[test]
    fn test_parse_frontmatter_no_frontmatter() {
        let text = "This is just regular content";
        let (fields, body) = parse_frontmatter(text);
        
        assert!(fields.is_empty());
        assert_eq!(body, "This is just regular content");
    }

    #[test]
    fn test_parse_frontmatter_unclosed() {
        let text = "---\nname: test\ndescription: Test skill\nThis is content without closing";
        let (fields, body) = parse_frontmatter(text);
        
        assert!(fields.is_empty());
        assert_eq!(body, text);
    }

    #[test]
    fn test_parse_frontmatter_empty() {
        let text = "---\n---\nThis is the content";
        let (fields, body) = parse_frontmatter(text);
        
        assert!(fields.is_empty());
        assert_eq!(body, "This is the content");
    }

    #[test]
    fn test_parse_skills_config_multiple() {
        let result = parse_skills_config("rust, security");
        assert_eq!(result, vec!["rust".to_string(), "security".to_string()]);
    }

    #[test]
    fn test_parse_skills_config_empty() {
        let result = parse_skills_config("");
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_skills_config_single() {
        let result = parse_skills_config("rust");
        assert_eq!(result, vec!["rust".to_string()]);
    }

    #[test]
    fn test_parse_skills_config_whitespace() {
        let result = parse_skills_config("  rust  ,   security   ");
        assert_eq!(result, vec!["rust".to_string(), "security".to_string()]);
    }

    #[test]
    fn test_format_skills_for_prompt_empty() {
        let result = format_skills_for_prompt(&[]);
        assert_eq!(result, "");
    }

    #[test]
    fn test_format_skills_for_prompt_single() {
        let skill = Skill {
            name: "rust".to_string(),
            description: "Rust programming".to_string(),
            content: "Use safe Rust practices".to_string(),
        };
        let result = format_skills_for_prompt(&[skill]);
        
        assert!(result.contains("rust"));
        assert!(result.contains("Rust programming"));
        assert!(result.contains("Use safe Rust practices"));
        assert!(result.starts_with("\n\n# Active Skills\n\n"));
    }
}
