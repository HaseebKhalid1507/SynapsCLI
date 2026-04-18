//! SKILL.md parsing, {baseDir} substitution, and filesystem discovery.

use std::path::{Path, PathBuf};
use crate::skills::LoadedSkill;

/// Parse YAML frontmatter from a markdown file.
/// Returns (frontmatter_fields, body).
pub(super) fn parse_frontmatter(text: &str) -> (Vec<(String, String)>, String) {
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
                let (k, v) = line.split_once(':')?;
                Some((k.trim().to_string(), v.trim().trim_matches('"').to_string()))
            })
            .collect();
        (fields, body)
    } else {
        (vec![], text.to_string())
    }
}

/// Load a SKILL.md file into a `LoadedSkill`. Applies `{baseDir}` substitution.
/// Returns None if required frontmatter is missing or body is empty.
pub fn load_skill_file(skill_md: &Path, plugin: Option<&str>) -> Option<LoadedSkill> {
    let content = std::fs::read_to_string(skill_md).ok()?;
    let (fields, body) = parse_frontmatter(&content);

    let name = fields.iter().find(|(k, _)| k == "name").map(|(_, v)| v.clone())?;
    let description = fields.iter().find(|(k, _)| k == "description").map(|(_, v)| v.clone())?;

    if body.is_empty() {
        return None;
    }

    let base_dir = skill_md.parent()?.canonicalize().ok()?;
    let body = body.replace("{baseDir}", base_dir.to_str()?);

    Some(LoadedSkill {
        name,
        description,
        body,
        plugin: plugin.map(str::to_string),
        base_dir,
        source_path: skill_md.canonicalize().ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn frontmatter_valid() {
        let t = "---\nname: x\ndescription: y\n---\nBody text";
        let (fields, body) = parse_frontmatter(t);
        assert_eq!(fields.len(), 2);
        assert_eq!(body, "Body text");
    }

    #[test]
    fn frontmatter_absent() {
        let t = "Just body";
        let (fields, body) = parse_frontmatter(t);
        assert!(fields.is_empty());
        assert_eq!(body, "Just body");
    }

    fn write_skill(dir: &Path, content: &str) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join("SKILL.md");
        fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn load_skill_basic() {
        let tmp = tempdir();
        let skill_dir = tmp.join("my-skill");
        let path = write_skill(&skill_dir, "---\nname: my-skill\ndescription: desc\n---\nBody");
        let s = load_skill_file(&path, Some("plugin-x")).unwrap();
        assert_eq!(s.name, "my-skill");
        assert_eq!(s.description, "desc");
        assert_eq!(s.body, "Body");
        assert_eq!(s.plugin.as_deref(), Some("plugin-x"));
        assert!(s.base_dir.is_absolute());
    }

    #[test]
    fn load_skill_basedir_substitution() {
        let tmp = tempdir();
        let skill_dir = tmp.join("skill");
        let path = write_skill(&skill_dir, "---\nname: s\ndescription: d\n---\nRun {baseDir}/x.js");
        let s = load_skill_file(&path, None).unwrap();
        let expected = format!("Run {}/x.js", s.base_dir.to_str().unwrap());
        assert_eq!(s.body, expected);
    }

    #[test]
    fn load_skill_missing_frontmatter_returns_none() {
        let tmp = tempdir();
        let skill_dir = tmp.join("bad");
        let path = write_skill(&skill_dir, "no frontmatter here");
        assert!(load_skill_file(&path, None).is_none());
    }

    #[test]
    fn load_skill_missing_description_returns_none() {
        let tmp = tempdir();
        let skill_dir = tmp.join("bad2");
        let path = write_skill(&skill_dir, "---\nname: x\n---\nbody");
        assert!(load_skill_file(&path, None).is_none());
    }

    #[test]
    fn load_skill_missing_name_returns_none() {
        let tmp = tempdir();
        let skill_dir = tmp.join("bad3");
        let path = write_skill(&skill_dir, "---\ndescription: d\n---\nbody");
        assert!(load_skill_file(&path, None).is_none());
    }

    #[test]
    fn load_skill_empty_body_returns_none() {
        let tmp = tempdir();
        let skill_dir = tmp.join("empty-body");
        let path = write_skill(&skill_dir, "---\nname: x\ndescription: d\n---\n");
        assert!(load_skill_file(&path, None).is_none());
    }

    #[test]
    fn load_skill_unclosed_frontmatter_returns_none() {
        let tmp = tempdir();
        let skill_dir = tmp.join("unclosed");
        // No closing `---`; parse_frontmatter returns ([], full_text) so name/description missing → None.
        let path = write_skill(&skill_dir, "---\nname: x\ndescription: d\nbody without closing fence");
        assert!(load_skill_file(&path, None).is_none());
    }

    #[test]
    fn load_skill_basedir_multiple_occurrences() {
        let tmp = tempdir();
        let skill_dir = tmp.join("multi");
        let path = write_skill(
            &skill_dir,
            "---\nname: m\ndescription: d\n---\n{baseDir}/a and {baseDir}/b",
        );
        let s = load_skill_file(&path, None).unwrap();
        let bd = s.base_dir.to_str().unwrap();
        assert_eq!(s.body, format!("{}/a and {}/b", bd, bd));
    }

    /// Create a unique tempdir under /tmp for tests.
    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "synaps-skills-test-{}", std::process::id()
        ));
        let unique = base.join(format!("{}", crate::epoch_millis()));
        std::fs::create_dir_all(&unique).unwrap();
        unique
    }
}
