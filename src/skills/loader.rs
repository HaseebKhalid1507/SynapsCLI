//! SKILL.md parsing, {baseDir}/${CLAUDE_PLUGIN_ROOT} substitution, and discovery.

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

/// Load a SKILL.md file into a `LoadedSkill`. Applies two substitutions:
/// - `{baseDir}` → the skill's own directory (native SynapsCLI form).
/// - `${CLAUDE_PLUGIN_ROOT}` / `$CLAUDE_PLUGIN_ROOT` → the plugin's root
///   directory (Claude Code compat), when the skill belongs to a plugin.
///
/// Returns None if required frontmatter is missing or body is empty.
pub fn load_skill_file(
    skill_md: &Path,
    plugin: Option<&str>,
    plugin_root: Option<&Path>,
) -> Option<LoadedSkill> {
    let content = std::fs::read_to_string(skill_md).ok()?;
    let (fields, body) = parse_frontmatter(&content);

    let name = fields.iter().find(|(k, _)| k == "name").map(|(_, v)| v.clone())?;
    let description = fields.iter().find(|(k, _)| k == "description").map(|(_, v)| v.clone())?;

    if body.is_empty() {
        return None;
    }

    let base_dir = skill_md.parent()?.canonicalize().ok()?;
    let mut body = body.replace("{baseDir}", base_dir.to_str()?);
    if let Some(root) = plugin_root.and_then(|p| p.canonicalize().ok()) {
        let root_str = root.to_str()?;
        // Replace braced form first so the plain-$ pass doesn't see a partial match.
        body = body.replace("${CLAUDE_PLUGIN_ROOT}", root_str);
        body = body.replace("$CLAUDE_PLUGIN_ROOT", root_str);
    }

    Some(LoadedSkill {
        name,
        description,
        body,
        plugin: plugin.map(str::to_string),
        base_dir,
        source_path: skill_md.canonicalize().ok()?,
    })
}

use crate::skills::{Plugin, manifest::{PluginManifest, MarketplaceManifest}};

/// The four default discovery roots, in priority order (local first, global second).
pub fn default_roots() -> Vec<PathBuf> {
    let mut roots = vec![
        PathBuf::from(".synaps-cli/plugins"),
        PathBuf::from(".synaps-cli/skills"),
    ];
    let home_plugins = crate::config::resolve_read_path_extended("plugins");
    let home_skills = crate::config::resolve_read_path_extended("skills");
    roots.push(home_plugins);
    roots.push(home_skills);
    roots
}

/// Walk the given roots and discover all plugins and skills.
/// Deduplicates on (plugin_name, skill_name); first occurrence wins.
pub fn load_all(roots: &[PathBuf]) -> (Vec<Plugin>, Vec<LoadedSkill>) {
    let mut plugins: Vec<Plugin> = Vec::new();
    let mut skills: Vec<LoadedSkill> = Vec::new();
    let mut seen: std::collections::HashSet<(Option<String>, String)> =
        std::collections::HashSet::new();

    for root in roots {
        walk_root(root, &mut plugins, &mut skills, &mut seen);
    }
    (plugins, skills)
}

/// Return the first existing path from a list of candidates, or None.
/// Used to accept both `.synaps-plugin/` (native) and `.claude-plugin/`
/// (Claude Code compat) manifest directories.
fn first_existing(candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates.iter().find(|p| p.exists()).cloned()
}

fn marketplace_json_for(root: &Path) -> Option<PathBuf> {
    first_existing(&[
        root.join(".synaps-plugin").join("marketplace.json"),
        root.join(".claude-plugin").join("marketplace.json"),
    ])
}

fn plugin_json_for(plugin_root: &Path) -> Option<PathBuf> {
    first_existing(&[
        plugin_root.join(".synaps-plugin").join("plugin.json"),
        plugin_root.join(".claude-plugin").join("plugin.json"),
    ])
}

fn walk_root(
    root: &Path,
    plugins: &mut Vec<Plugin>,
    skills: &mut Vec<LoadedSkill>,
    seen: &mut std::collections::HashSet<(Option<String>, String)>,
) {
    if !root.exists() { return; }

    // 1. Marketplace pass
    let marketplace_name = if let Some(marketplace_json) = marketplace_json_for(root) {
        match std::fs::read_to_string(&marketplace_json)
            .ok()
            .and_then(|c| serde_json::from_str::<MarketplaceManifest>(&c).ok())
        {
            Some(m) => {
                for entry in &m.plugins {
                    let plugin_root = root.join(&entry.source);
                    load_plugin(&plugin_root, Some(&m.name), plugins, skills, seen);
                }
                Some(m.name)
            }
            None => {
                tracing::warn!("failed to parse {}", marketplace_json.display());
                None
            }
        }
    } else {
        None
    };

    // 2. Plugin pass (subdirs with .synaps-plugin/plugin.json or .claude-plugin/plugin.json)
    //    Additionally, if a subdir contains a marketplace.json, treat it as a
    //    nested discovery root and recurse once. This supports the common
    //    "clone marketplace repo into plugins/" install pattern.
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() { continue; }
            if marketplace_json_for(&path).is_some() {
                walk_root(&path, plugins, skills, seen);
            } else if plugin_json_for(&path).is_some() {
                load_plugin(&path, marketplace_name.as_deref(), plugins, skills, seen);
            }
        }
    }

    // 3. Loose-skill pass — scan both root/ and root/skills/ for <name>/SKILL.md
    for loose_dir in [root.to_path_buf(), root.join("skills")] {
        if !loose_dir.is_dir() { continue; }
        if let Ok(entries) = std::fs::read_dir(&loose_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() { continue; }
                let skill_md = path.join("SKILL.md");
                if skill_md.exists() {
                    if let Some(s) = load_skill_file(&skill_md, None, None) {
                        let key = (None, s.name.clone());
                        if seen.insert(key) { skills.push(s); }
                    }
                }
            }
        }
    }
}

fn load_plugin(
    plugin_root: &Path,
    marketplace: Option<&str>,
    plugins: &mut Vec<Plugin>,
    skills: &mut Vec<LoadedSkill>,
    seen: &mut std::collections::HashSet<(Option<String>, String)>,
) {
    let Some(manifest_path) = plugin_json_for(plugin_root) else {
        tracing::warn!("no plugin.json under {}", plugin_root.display());
        return;
    };
    let Ok(content) = std::fs::read_to_string(&manifest_path) else {
        tracing::warn!("failed to read {}", manifest_path.display());
        return;
    };
    let Ok(m): Result<PluginManifest, _> = serde_json::from_str(&content) else {
        tracing::warn!("failed to parse {}", manifest_path.display());
        return;
    };

    let Ok(root_abs) = plugin_root.canonicalize() else { return; };
    if plugins.iter().any(|p| p.root == root_abs) {
        return;
    }
    plugins.push(Plugin {
        name: m.name.clone(),
        root: root_abs,
        marketplace: marketplace.map(str::to_string),
        version: m.version.clone(),
        description: m.description.clone(),
        manifest: Some(m.clone()),
    });

    let skills_dir = plugin_root.join("skills");
    if !skills_dir.is_dir() { return; }
    let Ok(entries) = std::fs::read_dir(&skills_dir) else { return; };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() { continue; }
        let skill_md = path.join("SKILL.md");
        if !skill_md.exists() { continue; }
        if let Some(s) = load_skill_file(&skill_md, Some(&m.name), Some(plugin_root)) {
            let key = (Some(m.name.clone()), s.name.clone());
            if seen.insert(key) { skills.push(s); }
        }
    }
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
        let s = load_skill_file(&path, Some("plugin-x"), None).unwrap();
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
        let s = load_skill_file(&path, None, None).unwrap();
        let expected = format!("Run {}/x.js", s.base_dir.to_str().unwrap());
        assert_eq!(s.body, expected);
    }

    #[test]
    fn load_skill_missing_frontmatter_returns_none() {
        let tmp = tempdir();
        let skill_dir = tmp.join("bad");
        let path = write_skill(&skill_dir, "no frontmatter here");
        assert!(load_skill_file(&path, None, None).is_none());
    }

    #[test]
    fn load_skill_missing_description_returns_none() {
        let tmp = tempdir();
        let skill_dir = tmp.join("bad2");
        let path = write_skill(&skill_dir, "---\nname: x\n---\nbody");
        assert!(load_skill_file(&path, None, None).is_none());
    }

    #[test]
    fn load_skill_missing_name_returns_none() {
        let tmp = tempdir();
        let skill_dir = tmp.join("bad3");
        let path = write_skill(&skill_dir, "---\ndescription: d\n---\nbody");
        assert!(load_skill_file(&path, None, None).is_none());
    }

    #[test]
    fn load_skill_empty_body_returns_none() {
        let tmp = tempdir();
        let skill_dir = tmp.join("empty-body");
        let path = write_skill(&skill_dir, "---\nname: x\ndescription: d\n---\n");
        assert!(load_skill_file(&path, None, None).is_none());
    }

    #[test]
    fn load_skill_unclosed_frontmatter_returns_none() {
        let tmp = tempdir();
        let skill_dir = tmp.join("unclosed");
        // No closing `---`; parse_frontmatter returns ([], full_text) so name/description missing → None.
        let path = write_skill(&skill_dir, "---\nname: x\ndescription: d\nbody without closing fence");
        assert!(load_skill_file(&path, None, None).is_none());
    }

    #[test]
    fn load_skill_basedir_multiple_occurrences() {
        let tmp = tempdir();
        let skill_dir = tmp.join("multi");
        let path = write_skill(
            &skill_dir,
            "---\nname: m\ndescription: d\n---\n{baseDir}/a and {baseDir}/b",
        );
        let s = load_skill_file(&path, None, None).unwrap();
        let bd = s.base_dir.to_str().unwrap();
        assert_eq!(s.body, format!("{}/a and {}/b", bd, bd));
    }

    #[test]
    fn load_skill_substitutes_claude_plugin_root_braced_and_plain() {
        // Regression: Claude-Code-style skills reference ${CLAUDE_PLUGIN_ROOT}
        // (and the bare $CLAUDE_PLUGIN_ROOT form) which must be substituted
        // to the plugin's canonical root before the body is handed to the model.
        let tmp = tempdir();
        let plugin_root = tmp.join("my-plugin");
        fs::create_dir_all(&plugin_root).unwrap();
        let skill_dir = plugin_root.join("skills").join("exa");
        let path = write_skill(
            &skill_dir,
            "---\nname: exa\ndescription: d\n---\nbash ${CLAUDE_PLUGIN_ROOT}/scripts/a.js then $CLAUDE_PLUGIN_ROOT/b.js",
        );
        let s = load_skill_file(&path, Some("my-plugin"), Some(&plugin_root)).unwrap();
        let root_abs = plugin_root.canonicalize().unwrap();
        let r = root_abs.to_str().unwrap();
        assert_eq!(s.body, format!("bash {}/scripts/a.js then {}/b.js", r, r));
    }

    #[test]
    fn load_skill_leaves_claude_plugin_root_alone_when_not_in_plugin() {
        // Loose skills (plugin_root = None) should not receive the substitution.
        let tmp = tempdir();
        let skill_dir = tmp.join("loose");
        let path = write_skill(
            &skill_dir,
            "---\nname: loose\ndescription: d\n---\n${CLAUDE_PLUGIN_ROOT}/x",
        );
        let s = load_skill_file(&path, None, None).unwrap();
        assert_eq!(s.body, "${CLAUDE_PLUGIN_ROOT}/x");
    }

    #[test]
    fn load_all_loose_skill() {
        let tmp = tempdir();
        let skill_dir = tmp.join("skills").join("loose");
        write_skill(&skill_dir, "---\nname: loose\ndescription: d\n---\nBody");

        let (plugins, skills) = load_all(std::slice::from_ref(&tmp));
        assert!(plugins.is_empty());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "loose");
        assert_eq!(skills[0].plugin, None);
    }

    #[test]
    fn load_all_plugin_skill() {
        let tmp = tempdir();
        let plugin_dir = tmp.join("my-plugin");
        fs::create_dir_all(plugin_dir.join(".synaps-plugin")).unwrap();
        fs::write(
            plugin_dir.join(".synaps-plugin").join("plugin.json"),
            r#"{"name":"my-plugin"}"#,
        ).unwrap();
        write_skill(&plugin_dir.join("skills").join("s1"),
            "---\nname: s1\ndescription: d\n---\nBody");

        let (plugins, skills) = load_all(std::slice::from_ref(&tmp));
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].name, "my-plugin");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].plugin.as_deref(), Some("my-plugin"));
    }

    #[test]
    fn load_all_marketplace() {
        let tmp = tempdir();
        // marketplace.json at root
        fs::create_dir_all(tmp.join(".synaps-plugin")).unwrap();
        fs::write(tmp.join(".synaps-plugin").join("marketplace.json"),
            r#"{"name":"pi-skills","plugins":[{"name":"web","source":"./web"}]}"#).unwrap();
        // plugin at ./web
        let plugin_dir = tmp.join("web");
        fs::create_dir_all(plugin_dir.join(".synaps-plugin")).unwrap();
        fs::write(plugin_dir.join(".synaps-plugin").join("plugin.json"),
            r#"{"name":"web"}"#).unwrap();
        write_skill(&plugin_dir.join("skills").join("search"),
            "---\nname: search\ndescription: d\n---\nBody");

        let (plugins, skills) = load_all(std::slice::from_ref(&tmp));
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].marketplace.as_deref(), Some("pi-skills"));
        assert_eq!(skills.len(), 1);
    }

    #[test]
    fn load_all_dedup_priority() {
        let tmp_local = tempdir();
        let tmp_global = tempdir();
        // same skill name in both
        write_skill(&tmp_local.join("skills").join("dup"),
            "---\nname: dup\ndescription: local\n---\nBody");
        write_skill(&tmp_global.join("skills").join("dup"),
            "---\nname: dup\ndescription: global\n---\nBody");

        let (_p, skills) = load_all(&[tmp_local, tmp_global]);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].description, "local"); // local wins
    }

    #[test]
    fn test_load_all_plugin_dedup_via_marketplace_and_subdir() {
        // Regression: when a plugin is discovered both through marketplace.json
        // and through the plugin-subdir walk, load_plugin's root-based dedup guard
        // must prevent a duplicate Plugin entry and duplicate skill registration.
        let root = tempdir();

        // marketplace.json at root pointing to ./web
        fs::create_dir_all(root.join(".synaps-plugin")).unwrap();
        fs::write(
            root.join(".synaps-plugin").join("marketplace.json"),
            r#"{"name":"mp","plugins":[{"name":"web","source":"./web"}]}"#,
        )
        .unwrap();

        // Plugin at ./web — also discoverable via the plugin-subdir pass
        let plugin_dir = root.join("web");
        fs::create_dir_all(plugin_dir.join(".synaps-plugin")).unwrap();
        fs::write(
            plugin_dir.join(".synaps-plugin").join("plugin.json"),
            r#"{"name":"web"}"#,
        )
        .unwrap();
        write_skill(
            &plugin_dir.join("skills").join("demo"),
            "---\nname: demo\ndescription: d\n---\nBody",
        );

        let (plugins, skills) = load_all(std::slice::from_ref(&root));

        // Exactly one plugin registered, not two.
        assert_eq!(plugins.len(), 1, "plugin should be deduplicated");
        assert_eq!(plugins[0].name, "web");
        assert_eq!(plugins[0].root, plugin_dir.canonicalize().unwrap());

        // Skill registered exactly once.
        assert_eq!(skills.len(), 1, "skill should be registered exactly once");
        assert_eq!(skills[0].name, "demo");
        assert_eq!(skills[0].plugin.as_deref(), Some("web"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn load_all_accepts_claude_plugin_marketplace_layout() {
        // Claude-Code-style: marketplace.json under .claude-plugin/, plugin.json
        // also under .claude-plugin/.
        let tmp = tempdir();
        fs::create_dir_all(tmp.join(".claude-plugin")).unwrap();
        fs::write(tmp.join(".claude-plugin").join("marketplace.json"),
            r#"{"name":"cc-mp","plugins":[{"name":"web","source":"./web"}]}"#).unwrap();
        let plugin_dir = tmp.join("web");
        fs::create_dir_all(plugin_dir.join(".claude-plugin")).unwrap();
        fs::write(plugin_dir.join(".claude-plugin").join("plugin.json"),
            r#"{"name":"web"}"#).unwrap();
        write_skill(&plugin_dir.join("skills").join("search"),
            "---\nname: search\ndescription: d\n---\nBody");

        let (plugins, skills) = load_all(std::slice::from_ref(&tmp));
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].marketplace.as_deref(), Some("cc-mp"));
        assert_eq!(plugins[0].name, "web");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "search");
    }

    #[test]
    fn load_all_prefers_synaps_plugin_over_claude_plugin() {
        // When both layouts are present, .synaps-plugin/ wins.
        let tmp = tempdir();
        let plugin_dir = tmp.join("dual");
        fs::create_dir_all(plugin_dir.join(".synaps-plugin")).unwrap();
        fs::create_dir_all(plugin_dir.join(".claude-plugin")).unwrap();
        fs::write(plugin_dir.join(".synaps-plugin").join("plugin.json"),
            r#"{"name":"native"}"#).unwrap();
        fs::write(plugin_dir.join(".claude-plugin").join("plugin.json"),
            r#"{"name":"claude"}"#).unwrap();
        write_skill(&plugin_dir.join("skills").join("s"),
            "---\nname: s\ndescription: d\n---\nBody");

        let (plugins, _skills) = load_all(std::slice::from_ref(&tmp));
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].name, "native", "synaps-plugin layout must win");
    }

    #[test]
    fn test_load_all_malformed_plugin_json_continues_walk() {
        // Regression: a malformed plugin.json should be skipped with a warning,
        // and the walk must continue so other valid plugins still register.
        let root = tempdir();

        // Broken plugin: invalid JSON in plugin.json
        let broken_dir = root.join("broken");
        fs::create_dir_all(broken_dir.join(".synaps-plugin")).unwrap();
        fs::write(
            broken_dir.join(".synaps-plugin").join("plugin.json"),
            "{ this is not valid json",
        )
        .unwrap();

        // Good plugin alongside it
        let good_dir = root.join("good");
        fs::create_dir_all(good_dir.join(".synaps-plugin")).unwrap();
        fs::write(
            good_dir.join(".synaps-plugin").join("plugin.json"),
            r#"{"name":"good"}"#,
        )
        .unwrap();
        write_skill(
            &good_dir.join("skills").join("hello"),
            "---\nname: hello\ndescription: d\n---\nBody",
        );

        let (plugins, skills) = load_all(std::slice::from_ref(&root));

        // Only the good plugin registered.
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].name, "good");

        // Its skill is present.
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "hello");
        assert_eq!(skills[0].plugin.as_deref(), Some("good"));

        let _ = fs::remove_dir_all(&root);
    }

    /// Create a unique tempdir under /tmp for tests.
    fn tempdir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "synaps-skills-test-{}", std::process::id()
        ));
        let unique = base.join(format!("{}-{}", crate::epoch_millis(), n));
        std::fs::create_dir_all(&unique).unwrap();
        unique
    }
}
