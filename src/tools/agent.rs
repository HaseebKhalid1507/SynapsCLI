//! Agent prompt resolution — loads agent configs from ~/.synaps-cli/agents/.
//!
//! Resolution order for `resolve_agent_prompt(name)`:
//!   1. `name` contains `/` → treat as file path, read directly
//!   2. `name` contains `:` → `plugin:agent` namespaced lookup
//!      → search `~/.synaps-cli/plugins/<plugin>/skills/*/agents/<agent>.md`
//!   3. bare name → `~/.synaps-cli/agents/<name>.md`
use super::util::expand_path;

/// Resolve an agent name to a system prompt.
pub fn resolve_agent_prompt(name: &str) -> std::result::Result<String, String> {
    // 1. File path — name contains '/'
    if name.contains('/') {
        let path = expand_path(name);
        let content = std::fs::read_to_string(&path)
            .map_err(|e| format!("Failed to read agent file '{}': {}", path.display(), e))?;
        return Ok(strip_frontmatter(&content));
    }

    // 2. Namespaced — "plugin:agent" syntax
    if let Some((plugin, agent)) = name.split_once(':') {
        let plugins_dir = crate::config::base_dir().join("plugins");
        let plugin_dir = plugins_dir.join(plugin);
        if !plugin_dir.is_dir() {
            return Err(format!(
                "Plugin '{}' not found at {}",
                plugin,
                plugin_dir.display()
            ));
        }
        return resolve_namespaced_agent(agent, &plugin_dir);
    }

    // 3. Bare name — ~/.synaps-cli/agents/<name>.md
    let agents_dir = crate::config::base_dir().join("agents");
    let agent_path = agents_dir.join(format!("{}.md", name));

    if agent_path.exists() {
        let content = std::fs::read_to_string(&agent_path)
            .map_err(|e| format!("Failed to read agent '{}': {}", agent_path.display(), e))?;
        return Ok(strip_frontmatter(&content));
    }

    Err(format!(
        "Agent '{}' not found. Searched:\n  - {}\nCreate the file or pass a system_prompt directly.",
        name,
        agent_path.display()
    ))
}

/// Search `plugin_dir/skills/*/agents/<agent>.md` for a matching agent file.
fn resolve_namespaced_agent(
    agent: &str,
    plugin_dir: &std::path::Path,
) -> std::result::Result<String, String> {
    let skills_dir = plugin_dir.join("skills");
    let Ok(entries) = std::fs::read_dir(&skills_dir) else {
        return Err(format!(
            "No skills directory in plugin at {}",
            plugin_dir.display()
        ));
    };
    for entry in entries.flatten() {
        let agent_path = entry.path().join("agents").join(format!("{}.md", agent));
        if agent_path.exists() {
            let content = std::fs::read_to_string(&agent_path)
                .map_err(|e| format!("Failed to read agent '{}': {}", agent_path.display(), e))?;
            return Ok(strip_frontmatter(&content));
        }
    }
    Err(format!(
        "Agent '{}' not found in plugin at {}. Searched skills/*/agents/{}.md",
        agent,
        plugin_dir.display(),
        agent
    ))
}

pub(crate) fn strip_frontmatter(content: &str) -> String {
    if let Some(rest) = content.strip_prefix("---") {
        if let Some(end) = rest.find("\n---") {
            // skip past the "\n---" (4 bytes) to get the body
            return rest[end + 4..].trim().to_string();
        }
    }
    content.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_namespaced_agent_finds_plugin_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let agents_dir = tmp
            .path()
            .join("skills")
            .join("bbe")
            .join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("sage.md"),
            "---\nname: bbe-sage\ndescription: d\n---\nYou are sage.",
        )
        .unwrap();

        let result = resolve_namespaced_agent("sage", tmp.path());
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "You are sage.");
    }

    #[test]
    fn resolve_namespaced_agent_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("skills")).unwrap();

        let result = resolve_namespaced_agent("nonexistent", tmp.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[test]
    fn resolve_namespaced_agent_no_skills_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let result = resolve_namespaced_agent("sage", tmp.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("No skills directory"));
    }

    #[test]
    fn resolve_namespaced_agent_strips_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        let agents_dir = tmp.path().join("skills").join("s").join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("a.md"),
            "---\nname: x\ndescription: d\n---\nClean body",
        )
        .unwrap();

        let result = resolve_namespaced_agent("a", tmp.path()).unwrap();
        assert_eq!(result, "Clean body");
    }

    #[test]
    fn strip_frontmatter_removes_yaml_header() {
        let input = "---\nname: x\n---\nBody text";
        assert_eq!(strip_frontmatter(input), "Body text");
    }

    #[test]
    fn strip_frontmatter_passes_through_plain_text() {
        assert_eq!(strip_frontmatter("Just text"), "Just text");
    }
}
