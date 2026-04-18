//! Slash command registry: built-ins + dynamically registered skills.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use crate::skills::LoadedSkill;

#[derive(Debug, Clone)]
pub struct PluginSummary {
    pub name: String,
    pub skill_count: usize,
}

/// Resolution outcome for a typed slash command.
#[derive(Debug)]
pub enum Resolution {
    /// A built-in command (dispatched via existing handle_command).
    Builtin,
    /// A single unambiguous skill.
    Skill(Arc<LoadedSkill>),
    /// Multiple skills share this unqualified name; user must qualify.
    Ambiguous(Vec<String>), // list of plugin-qualified names
    /// No such command.
    Unknown,
}

struct Inner {
    skills: HashMap<String, Vec<Arc<LoadedSkill>>>, // unqualified name -> all matches
    qualified: HashMap<String, Arc<LoadedSkill>>,   // "plugin:skill" -> single
}

pub struct CommandRegistry {
    builtins: Vec<&'static str>,
    inner: RwLock<Inner>,
}

impl CommandRegistry {
    pub fn new(builtins: &[&'static str], skills: Vec<LoadedSkill>) -> Self {
        let r = CommandRegistry {
            builtins: builtins.to_vec(),
            inner: RwLock::new(Inner {
                skills: HashMap::new(),
                qualified: HashMap::new(),
            }),
        };
        r.rebuild_with(skills);
        r
    }

    /// Atomically replace the skill set. Built-ins are unchanged.
    pub fn rebuild_with(&self, skills: Vec<LoadedSkill>) {
        let builtins_set: std::collections::HashSet<&str> =
            self.builtins.iter().copied().collect();
        let mut new_skills: HashMap<String, Vec<Arc<LoadedSkill>>> = HashMap::new();
        let mut new_qualified: HashMap<String, Arc<LoadedSkill>> = HashMap::new();
        for s in skills {
            let arc = Arc::new(s);
            // Unqualified entry
            if builtins_set.contains(arc.name.as_str()) {
                tracing::warn!(
                    "skill '{}' shadowed by built-in; reachable only via qualified form '{}:{}'",
                    arc.name,
                    arc.plugin.as_deref().unwrap_or("?"),
                    arc.name
                );
            } else {
                new_skills.entry(arc.name.clone()).or_default().push(arc.clone());
            }
            // Qualified entry
            if let Some(ref p) = arc.plugin {
                let q = format!("{}:{}", p, arc.name);
                new_qualified.insert(q, arc.clone());
            }
        }
        let mut w = self.inner.write().unwrap();
        w.skills = new_skills;
        w.qualified = new_qualified;
    }

    pub fn resolve(&self, cmd: &str) -> Resolution {
        let r = self.inner.read().unwrap();
        if cmd.contains(':') {
            return match r.qualified.get(cmd) {
                Some(s) => Resolution::Skill(s.clone()),
                None => Resolution::Unknown,
            };
        }
        if self.builtins.contains(&cmd) {
            return Resolution::Builtin;
        }
        match r.skills.get(cmd) {
            Some(v) if v.len() == 1 => Resolution::Skill(v[0].clone()),
            Some(v) => Resolution::Ambiguous(
                v.iter()
                    .map(|s| format!("{}:{}", s.plugin.as_deref().unwrap_or("?"), s.name))
                    .collect(),
            ),
            None => Resolution::Unknown,
        }
    }

    /// All commands for autocomplete/help: builtins + unique unqualified skill names, sorted.
    pub fn all_commands(&self) -> Vec<String> {
        let r = self.inner.read().unwrap();
        let mut v: Vec<String> = self.builtins.iter().map(|s| s.to_string()).collect();
        v.extend(r.skills.keys().cloned());
        v.sort();
        v.dedup();
        v
    }

    pub fn plugins(&self) -> Vec<PluginSummary> {
        let r = self.inner.read().unwrap();
        let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
        for s in r.qualified.values() {
            if let Some(ref p) = s.plugin {
                let key = (p.clone(), s.name.clone());
                if seen.insert(key) {
                    *counts.entry(p.clone()).or_insert(0) += 1;
                }
            }
        }
        counts.into_iter()
            .map(|(name, skill_count)| PluginSummary { name, skill_count })
            .collect()
    }

    pub fn all_skills(&self) -> Vec<Arc<LoadedSkill>> {
        let r = self.inner.read().unwrap();
        let mut seen: std::collections::HashSet<(Option<String>, String)> =
            std::collections::HashSet::new();
        let mut out = Vec::new();
        for list in r.skills.values() {
            for s in list {
                let key = (s.plugin.clone(), s.name.clone());
                if seen.insert(key) { out.push(s.clone()); }
            }
        }
        for s in r.qualified.values() {
            let key = (s.plugin.clone(), s.name.clone());
            if seen.insert(key) { out.push(s.clone()); }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn mk(name: &str, plugin: Option<&str>) -> LoadedSkill {
        LoadedSkill {
            name: name.to_string(),
            description: String::new(),
            body: String::new(),
            plugin: plugin.map(str::to_string),
            base_dir: PathBuf::from("/"),
            source_path: PathBuf::from("/SKILL.md"),
        }
    }

    #[test]
    fn resolve_builtin() {
        let r = CommandRegistry::new(&["clear"], vec![]);
        assert!(matches!(r.resolve("clear"), Resolution::Builtin));
    }

    #[test]
    fn resolve_unknown() {
        let r = CommandRegistry::new(&["clear"], vec![]);
        assert!(matches!(r.resolve("xyz"), Resolution::Unknown));
    }

    #[test]
    fn resolve_unique_skill() {
        let r = CommandRegistry::new(&[], vec![mk("search", Some("p"))]);
        match r.resolve("search") {
            Resolution::Skill(s) => assert_eq!(s.name, "search"),
            _ => panic!(),
        }
    }

    #[test]
    fn resolve_ambiguous() {
        let r = CommandRegistry::new(&[], vec![
            mk("search", Some("p1")),
            mk("search", Some("p2")),
        ]);
        match r.resolve("search") {
            Resolution::Ambiguous(v) => {
                assert_eq!(v.len(), 2);
                assert!(v.iter().any(|s| s == "p1:search"));
                assert!(v.iter().any(|s| s == "p2:search"));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn resolve_qualified() {
        let r = CommandRegistry::new(&[], vec![
            mk("search", Some("p1")),
            mk("search", Some("p2")),
        ]);
        match r.resolve("p1:search") {
            Resolution::Skill(s) => assert_eq!(s.plugin.as_deref(), Some("p1")),
            _ => panic!(),
        }
    }

    #[test]
    fn builtin_shadows_skill_unqualified() {
        // Skill named "clear" should not win over the built-in.
        let r = CommandRegistry::new(&["clear"], vec![mk("clear", Some("p"))]);
        assert!(matches!(r.resolve("clear"), Resolution::Builtin));
        // Qualified form still works.
        match r.resolve("p:clear") {
            Resolution::Skill(s) => assert_eq!(s.name, "clear"),
            _ => panic!(),
        }
    }

    #[test]
    fn all_commands_sorted_and_deduped() {
        let r = CommandRegistry::new(&["clear", "model"], vec![
            mk("search", Some("p")),
            mk("help-me", None),
        ]);
        let cmds = r.all_commands();
        assert_eq!(cmds, vec!["clear", "help-me", "model", "search"]);
    }

    #[test]
    fn all_skills_dedups_plugin_skill() {
        let r = CommandRegistry::new(&[], vec![mk("search", Some("p"))]);
        let all = r.all_skills();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "search");
        assert_eq!(all[0].plugin.as_deref(), Some("p"));
    }

    #[test]
    fn all_skills_includes_shadowed_skill() {
        let r = CommandRegistry::new(&["clear"], vec![mk("clear", Some("p"))]);
        let all = r.all_skills();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "clear");
        assert_eq!(all[0].plugin.as_deref(), Some("p"));
    }

    #[test]
    fn resolve_qualified_unknown_returns_unknown() {
        let r = CommandRegistry::new(&[], vec![mk("search", Some("p1"))]);
        assert!(matches!(r.resolve("p1:nosuch"), Resolution::Unknown));
        assert!(matches!(r.resolve("nosuch:search"), Resolution::Unknown));
    }

    #[test]
    fn rebuild_replaces_skills() {
        let r = CommandRegistry::new(&["clear"], vec![mk("old", None)]);
        assert!(matches!(r.resolve("old"), Resolution::Skill(_)));
        assert!(matches!(r.resolve("new"), Resolution::Unknown));
        r.rebuild_with(vec![mk("new", None)]);
        assert!(matches!(r.resolve("old"), Resolution::Unknown));
        assert!(matches!(r.resolve("new"), Resolution::Skill(_)));
    }

    #[test]
    fn rebuild_visible_through_shared_arc() {
        let r = std::sync::Arc::new(CommandRegistry::new(&[], vec![mk("a", None)]));
        let r2 = r.clone();
        r.rebuild_with(vec![mk("b", None)]);
        assert!(matches!(r2.resolve("b"), Resolution::Skill(_)));
        assert!(matches!(r2.resolve("a"), Resolution::Unknown));
    }

    #[test]
    fn plugins_summary_groups_by_plugin_name() {
        let r = CommandRegistry::new(&[], vec![
            mk("a", Some("p1")),
            mk("b", Some("p1")),
            mk("c", Some("p2")),
            mk("loose", None),
        ]);
        let mut plugins = r.plugins();
        plugins.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(plugins.len(), 2);
        assert_eq!(plugins[0].name, "p1");
        assert_eq!(plugins[0].skill_count, 2);
        assert_eq!(plugins[1].name, "p2");
        assert_eq!(plugins[1].skill_count, 1);
    }
}
