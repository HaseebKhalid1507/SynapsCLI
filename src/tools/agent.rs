//! Agent prompt resolution — loads agent configs from ~/.synaps-cli/agents/.
//!
//! Resolution order for `resolve_agent_prompt(name)`:
//!   1. `name` contains `/` → treat as file path, read directly
//!   2. `name` contains `:` → `plugin:agent` namespaced lookup
//!      → search `~/.synaps-cli/plugins/<plugin>/skills/*/agents/<agent>.md`
//!   3. bare name → `~/.synaps-cli/agents/<name>.md`
use super::util::expand_path;

/// Returns true if the name component is a safe identifier (no path separators,
/// no `..`, non-empty, only alphanumeric + hyphens + underscores).
fn is_valid_name(s: &str) -> bool {
    !s.is_empty()
        && !s.contains('/')
        && !s.contains('\\')
        && !s.contains("..")
        && s.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_')
}

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
        // Validate both sides are safe identifiers
        if !is_valid_name(plugin) || !is_valid_name(agent) {
            return Err(format!(
                "Invalid agent syntax '{}'. Expected 'plugin:agent' where both are \
                 identifiers (alphanumeric, hyphens, underscores).",
                name
            ));
        }
        let plugins_dir = crate::config::base_dir().join("plugins");
        let plugin_dir = plugins_dir.join(plugin);
        if !plugin_dir.is_dir() {
            return Err(format!(
                "Plugin '{}' not found at {}",
                plugin,
                plugin_dir.display()
            ));
        }
        // Verify resolved path is still under plugins_dir (path traversal guard)
        let canonical_plugins = plugins_dir.canonicalize().unwrap_or_else(|_| plugins_dir.clone());
        let canonical_plugin = plugin_dir.canonicalize().unwrap_or_else(|_| plugin_dir.clone());
        if !canonical_plugin.starts_with(&canonical_plugins) {
            return Err(format!("Invalid plugin name: '{}'", plugin));
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
        "Agent '{}' not found. Searched:\n  - {}\n\
         Create the file, pass a system_prompt directly, or use 'plugin:agent' syntax for plugin agents.",
        name,
        agent_path.display()
    ))
}

/// Search `plugin_dir/skills/*/agents/<agent>.md` for a matching agent file.
/// Errors if the agent is found in multiple skills (ambiguous).
fn resolve_namespaced_agent(
    agent: &str,
    plugin_dir: &std::path::Path,
) -> std::result::Result<String, String> {
    let skills_dir = plugin_dir.join("skills");
    let entries = std::fs::read_dir(&skills_dir).map_err(|e| {
        format!(
            "No skills directory in plugin at {}: {}",
            plugin_dir.display(),
            e
        )
    })?;

    let mut matches: Vec<std::path::PathBuf> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("Error reading skills dir: {}", e))?;
        let agent_path = entry.path().join("agents").join(format!("{}.md", agent));
        if agent_path.exists() {
            matches.push(agent_path);
        }
    }

    match matches.len() {
        0 => Err(format!(
            "Agent '{}' not found in plugin at {}. Searched skills/*/agents/{}.md",
            agent,
            plugin_dir.display(),
            agent
        )),
        1 => {
            let content = std::fs::read_to_string(&matches[0])
                .map_err(|e| format!("Failed to read agent '{}': {}", matches[0].display(), e))?;
            Ok(strip_frontmatter(&content))
        }
        n => Err(format!(
            "Ambiguous agent '{}': found in {} skills. Use the full path instead.\n  {}",
            agent,
            n,
            matches.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("\n  ")
        )),
    }
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
    fn resolve_namespaced_agent_ambiguous_errors() {
        let tmp = tempfile::tempdir().unwrap();
        // Create same agent in two different skills
        let skill1 = tmp.path().join("skills").join("skill-a").join("agents");
        let skill2 = tmp.path().join("skills").join("skill-b").join("agents");
        std::fs::create_dir_all(&skill1).unwrap();
        std::fs::create_dir_all(&skill2).unwrap();
        std::fs::write(skill1.join("sage.md"), "---\nname: sage\n---\nA").unwrap();
        std::fs::write(skill2.join("sage.md"), "---\nname: sage\n---\nB").unwrap();

        let result = resolve_namespaced_agent("sage", tmp.path());
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("Ambiguous"), "Expected ambiguity error, got: {}", err);
        assert!(err.contains("2 skills"), "Expected '2 skills' in error, got: {}", err);
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

    #[test]
    fn strip_frontmatter_unclosed_returns_raw() {
        // Unclosed frontmatter — no closing "---"
        let input = "---\nname: x\nno closing delimiter\nBody";
        assert_eq!(strip_frontmatter(input), input);
    }

    #[test]
    fn is_valid_name_rejects_traversal() {
        assert!(!is_valid_name("../../etc"));
        assert!(!is_valid_name("foo/bar"));
        assert!(!is_valid_name(""));
        assert!(!is_valid_name("foo..bar"));
        assert!(!is_valid_name("foo\\bar"));
    }

    #[test]
    fn is_valid_name_accepts_good_names() {
        assert!(is_valid_name("dev-tools"));
        assert!(is_valid_name("sage"));
        assert!(is_valid_name("my_agent_123"));
        assert!(is_valid_name("BBE"));
    }
}
