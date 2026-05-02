//! Slash command registry: built-ins + dynamically registered skills.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use crate::skills::{LoadedSkill, Plugin};

#[derive(Debug, Clone)]
pub struct PluginSummary {
    pub name: String,
    pub skill_count: usize,
}

#[derive(Clone, Debug)]
pub enum RegisteredPluginCommandBackend {
    Shell { command: String, args: Vec<String> },
    ExtensionTool { tool: String, input: serde_json::Value },
    SkillPrompt { skill: String, prompt: String },
    Interactive { plugin_extension_id: String },
}

/// Editor kind for a plugin-declared settings field, normalised from the
/// manifest into a form the settings UI can consume directly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PluginSettingsEditor {
    Text { numeric: bool },
    Cycler { options: Vec<String> },
    Picker,
    /// Editor body rendered by the plugin via the
    /// `settings.editor.*` JSON-RPC contract.
    Custom,
}

#[derive(Clone, Debug)]
pub struct PluginSettingsField {
    pub key: String,
    pub label: String,
    pub editor: PluginSettingsEditor,
    pub help: Option<String>,
    pub default: Option<serde_json::Value>,
}

/// A plugin-declared category contributed to the `/settings` modal.
///
/// The owning plugin is recorded in `plugin` so that the settings UI can
/// scope reads/writes to that plugin's config namespace and dispatch
/// custom-editor JSON-RPC events to the right extension.
#[derive(Clone, Debug)]
pub struct PluginSettingsCategory {
    pub plugin: String,
    pub id: String,
    pub label: String,
    pub fields: Vec<PluginSettingsField>,
}

/// Resolution outcome for a typed slash command.
#[derive(Clone, Debug)]
pub struct RegisteredPluginCommand {
    pub plugin: String,
    pub name: String,
    pub description: Option<String>,
    pub backend: RegisteredPluginCommandBackend,
    pub plugin_root: std::path::PathBuf,
}

pub enum Resolution {
    /// A built-in command (dispatched via existing handle_command).
    Builtin,
    /// A single unambiguous skill.
    Skill(Arc<LoadedSkill>),
    /// A plugin command from plugin.json `commands`.
    PluginCommand(Arc<RegisteredPluginCommand>),
    /// Multiple skills share this unqualified name; user must qualify.
    Ambiguous(Vec<String>), // list of plugin-qualified names
    /// No such command.
    Unknown,
}

struct Inner {
    skills: HashMap<String, Vec<Arc<LoadedSkill>>>, // unqualified name -> all matches
    qualified: HashMap<String, Arc<LoadedSkill>>,   // "plugin:skill" -> single
    plugin_commands: HashMap<String, Arc<RegisteredPluginCommand>>, // "plugin:cmd" -> single
    /// Plugin-declared settings categories, in plugin discovery order.
    /// Path B Phase 4. The settings UI snapshots this on each open.
    plugin_settings_categories: Vec<PluginSettingsCategory>,
}

pub struct CommandRegistry {
    builtins: Vec<&'static str>,
    inner: RwLock<Inner>,
}

impl CommandRegistry {
    pub fn new(builtins: &[&'static str], skills: Vec<LoadedSkill>) -> Self {
        Self::new_with_plugins(builtins, skills, vec![])
    }

    pub fn new_with_plugins(builtins: &[&'static str], skills: Vec<LoadedSkill>, plugins: Vec<Plugin>) -> Self {
        let r = CommandRegistry {
            builtins: builtins.to_vec(),
            inner: RwLock::new(Inner {
                skills: HashMap::new(),
                qualified: HashMap::new(),
                plugin_commands: HashMap::new(),
                plugin_settings_categories: Vec::new(),
            }),
        };
        r.rebuild_with_plugins(skills, plugins);
        r
    }

    /// Atomically replace the skill set. Built-ins are unchanged.
    pub fn rebuild_with(&self, skills: Vec<LoadedSkill>) {
        self.rebuild_with_plugins(skills, vec![]);
    }

    /// Atomically replace the skill and plugin-command set. Built-ins are unchanged.
    pub fn rebuild_with_plugins(&self, skills: Vec<LoadedSkill>, plugins: Vec<Plugin>) {
        let builtins_set: std::collections::HashSet<&str> =
            self.builtins.iter().copied().collect();
        let mut new_skills: HashMap<String, Vec<Arc<LoadedSkill>>> = HashMap::new();
        let mut new_qualified: HashMap<String, Arc<LoadedSkill>> = HashMap::new();
        let mut new_plugin_commands: HashMap<String, Arc<RegisteredPluginCommand>> = HashMap::new();
        let mut new_plugin_settings_categories: Vec<PluginSettingsCategory> = Vec::new();
        for plugin in plugins {
            if let Some(manifest) = plugin.manifest {
                if let Some(ref settings) = manifest.settings {
                    for cat in &settings.categories {
                        let fields = cat
                            .fields
                            .iter()
                            .map(|f| PluginSettingsField {
                                key: f.key.clone(),
                                label: f.label.clone(),
                                editor: match f.editor {
                                    crate::skills::manifest::ManifestEditorKind::Text => {
                                        PluginSettingsEditor::Text { numeric: f.numeric }
                                    }
                                    crate::skills::manifest::ManifestEditorKind::Cycler => {
                                        PluginSettingsEditor::Cycler {
                                            options: f.options.clone(),
                                        }
                                    }
                                    crate::skills::manifest::ManifestEditorKind::Picker => {
                                        PluginSettingsEditor::Picker
                                    }
                                    crate::skills::manifest::ManifestEditorKind::Custom => {
                                        PluginSettingsEditor::Custom
                                    }
                                },
                                help: f.help.clone(),
                                default: f.default.clone(),
                            })
                            .collect();
                        new_plugin_settings_categories.push(PluginSettingsCategory {
                            plugin: manifest.name.clone(),
                            id: cat.id.clone(),
                            label: cat.label.clone(),
                            fields,
                        });
                    }
                }
                for cmd in manifest.commands {
                    let (name, description, backend) = match cmd {
                        crate::skills::manifest::ManifestCommand::Shell(cmd) => (
                            cmd.name,
                            cmd.description,
                            RegisteredPluginCommandBackend::Shell { command: cmd.command, args: cmd.args },
                        ),
                        crate::skills::manifest::ManifestCommand::ExtensionTool(cmd) => (
                            cmd.name,
                            cmd.description,
                            RegisteredPluginCommandBackend::ExtensionTool { tool: cmd.tool, input: cmd.input },
                        ),
                        crate::skills::manifest::ManifestCommand::SkillPrompt(cmd) => (
                            cmd.name,
                            cmd.description,
                            RegisteredPluginCommandBackend::SkillPrompt { skill: cmd.skill, prompt: cmd.prompt },
                        ),
                        crate::skills::manifest::ManifestCommand::Interactive(cmd) => {
                            if !cmd.interactive {
                                continue;
                            }
                            (
                                cmd.name,
                                cmd.description,
                                RegisteredPluginCommandBackend::Interactive {
                                    plugin_extension_id: manifest
                                        .extension
                                        .as_ref()
                                        .map(|_| plugin.name.clone())
                                        .unwrap_or_else(|| plugin.name.clone()),
                                },
                            )
                        },
                    };
                    let q = format!("{}:{}", manifest.name, name);
                    new_plugin_commands.insert(q, Arc::new(RegisteredPluginCommand {
                        plugin: manifest.name.clone(),
                        name,
                        description,
                        backend,
                        plugin_root: plugin.root.clone(),
                    }));
                }
            }
        }
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
        w.plugin_commands = new_plugin_commands;
        w.plugin_settings_categories = new_plugin_settings_categories;
    }

    pub fn resolve(&self, cmd: &str) -> Resolution {
        let r = self.inner.read().unwrap();
        if cmd.contains(':') {
            if let Some(c) = r.plugin_commands.get(cmd) {
                return Resolution::PluginCommand(c.clone());
            }
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
        v.extend(r.plugin_commands.keys().cloned());
        v.sort();
        v.dedup();
        v
    }

    pub fn plugins(&self) -> Vec<PluginSummary> {
        let r = self.inner.read().unwrap();
        let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
        for c in r.plugin_commands.values() {
            let key = (c.plugin.clone(), c.name.clone());
            if seen.insert(key) {
                *counts.entry(c.plugin.clone()).or_insert(0) += 0;
            }
        }
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

    /// Snapshot of every plugin-declared settings category, preserving
    /// plugin discovery order. The settings UI calls this on each open
    /// to merge plugin categories with the built-in ones.
    pub fn plugin_settings_categories(&self) -> Vec<PluginSettingsCategory> {
        self.inner.read().unwrap().plugin_settings_categories.clone()
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
    use crate::skills::manifest::{ManifestCommand, ManifestShellCommand};

    fn mk_cmd(plugin: &str, name: &str, root: PathBuf) -> Plugin {
        Plugin {
            name: plugin.to_string(),
            root,
            marketplace: None,
            version: None,
            description: None,
            extension: None,
            manifest: Some(crate::skills::manifest::PluginManifest {
                name: plugin.to_string(),
                version: None,
                description: None,
                keybinds: vec![],
                compatibility: None,
                commands: vec![ManifestCommand::Shell(ManifestShellCommand {
                    name: name.to_string(),
                    description: Some("desc".to_string()),
                    command: "printf".to_string(),
                    args: vec!["hi".to_string()],
                })],
                extension: None,
                provides: None,
                settings: None,
            }),
        }
    }


    fn mk_interactive_cmd(plugin: &str, name: &str, root: PathBuf) -> Plugin {
        Plugin {
            name: plugin.to_string(),
            root,
            marketplace: None,
            version: None,
            description: None,
            extension: None,
            manifest: Some(crate::skills::manifest::PluginManifest {
                name: plugin.to_string(),
                version: None,
                description: None,
                keybinds: vec![],
                compatibility: None,
                commands: vec![ManifestCommand::Interactive(crate::skills::manifest::ManifestInteractiveCommand {
                    name: name.to_string(),
                    description: Some("interactive desc".to_string()),
                    interactive: true,
                    subcommands: vec!["help".to_string()],
                })],
                extension: None,
                provides: None,
                settings: None,
            }),
        }
    }

    #[test]
    fn registers_interactive_plugin_command_backend() {
        let reg = CommandRegistry::new_with_plugins(
            &[],
            vec![],
            vec![mk_interactive_cmd("demo-plugin", "demo", PathBuf::from("/tmp/demo"))],
        );

        match reg.resolve("demo-plugin:demo") {
            Resolution::PluginCommand(cmd) => match &cmd.backend {
                RegisteredPluginCommandBackend::Interactive { plugin_extension_id } => {
                    assert_eq!(plugin_extension_id, "demo-plugin");
                    assert_eq!(cmd.name, "demo");
                }
                other => panic!("expected interactive backend, got {other:?}"),
            },
            _ => panic!("expected plugin command resolution"),
        }
    }

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
    fn resolve_qualified_plugin_command() {
        let r = CommandRegistry::new_with_plugins(&[], vec![], vec![mk_cmd("p", "hello", PathBuf::from("/tmp/p"))]);
        match r.resolve("p:hello") {
            Resolution::PluginCommand(cmd) => {
                assert_eq!(cmd.plugin, "p");
                assert_eq!(cmd.name, "hello");
                assert!(matches!(
                    &cmd.backend,
                    RegisteredPluginCommandBackend::Shell { command, .. } if command == "printf"
                ));
                assert_eq!(cmd.plugin_root, PathBuf::from("/tmp/p"));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn all_commands_includes_qualified_plugin_commands() {
        let r = CommandRegistry::new_with_plugins(&["help"], vec![], vec![mk_cmd("p", "hello", PathBuf::from("/tmp/p"))]);
        let cmds = r.all_commands();
        assert!(cmds.contains(&"help".to_string()));
        assert!(cmds.contains(&"p:hello".to_string()));
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

    fn mk_plugin_with_settings(plugin: &str, root: PathBuf) -> Plugin {
        use crate::skills::manifest::{
            ManifestEditorKind, ManifestSettings, ManifestSettingsCategory,
            ManifestSettingsField,
        };
        Plugin {
            name: plugin.to_string(),
            root,
            marketplace: None,
            version: None,
            description: None,
            extension: None,
            manifest: Some(crate::skills::manifest::PluginManifest {
                name: plugin.to_string(),
                version: None,
                description: None,
                keybinds: vec![],
                compatibility: None,
                commands: vec![],
                extension: None,
                provides: None,
                settings: Some(ManifestSettings {
                    categories: vec![ManifestSettingsCategory {
                        id: "demo".to_string(),
                        label: "Demo".to_string(),
                        fields: vec![
                            ManifestSettingsField {
                                key: "backend".to_string(),
                                label: "Backend".to_string(),
                                editor: ManifestEditorKind::Cycler,
                                options: vec!["a".to_string(), "b".to_string()],
                                help: None,
                                default: None,
                                numeric: false,
                            },
                            ManifestSettingsField {
                                key: "endpoint".to_string(),
                                label: "Endpoint".to_string(),
                                editor: ManifestEditorKind::Text,
                                options: vec![],
                                help: Some("URL".to_string()),
                                default: None,
                                numeric: false,
                            },
                        ],
                    }],
                }),
            }),
        }
    }

    #[test]
    fn plugin_settings_categories_exposed_after_rebuild() {
        let r = CommandRegistry::new_with_plugins(
            &[],
            vec![],
            vec![mk_plugin_with_settings("demo-plugin", PathBuf::from("/tmp/demo"))],
        );
        let cats = r.plugin_settings_categories();
        assert_eq!(cats.len(), 1, "expected one plugin settings category");
        let cat = &cats[0];
        assert_eq!(cat.plugin, "demo-plugin");
        assert_eq!(cat.id, "demo");
        assert_eq!(cat.label, "Demo");
        assert_eq!(cat.fields.len(), 2);

        match &cat.fields[0].editor {
            PluginSettingsEditor::Cycler { options } => {
                assert_eq!(options, &vec!["a".to_string(), "b".to_string()]);
            }
            other => panic!("expected cycler, got {other:?}"),
        }
        assert!(matches!(
            cat.fields[1].editor,
            PluginSettingsEditor::Text { numeric: false }
        ));
        assert_eq!(cat.fields[1].help.as_deref(), Some("URL"));
    }

    #[test]
    fn plugin_settings_categories_empty_without_settings_block() {
        let r = CommandRegistry::new_with_plugins(
            &[],
            vec![],
            vec![mk_cmd("p", "hello", PathBuf::from("/tmp/p"))],
        );
        assert!(r.plugin_settings_categories().is_empty());
    }

    #[test]
    fn plugin_settings_categories_replaced_on_rebuild() {
        let r = CommandRegistry::new_with_plugins(
            &[],
            vec![],
            vec![mk_plugin_with_settings("demo-plugin", PathBuf::from("/tmp/demo"))],
        );
        assert_eq!(r.plugin_settings_categories().len(), 1);
        r.rebuild_with_plugins(vec![], vec![]);
        assert!(r.plugin_settings_categories().is_empty());
    }

    #[test]
    fn plugin_settings_categories_does_not_hardcode_voice() {
        // Acceptance: declarative cycler/text fields are represented in
        // core data without any voice-specific knowledge.
        let r = CommandRegistry::new_with_plugins(
            &[],
            vec![],
            vec![mk_plugin_with_settings("totally-unrelated", PathBuf::from("/tmp/x"))],
        );
        let cats = r.plugin_settings_categories();
        assert_eq!(cats[0].plugin, "totally-unrelated");
        assert_eq!(cats[0].id, "demo");
        assert!(cats[0].fields.iter().any(|f| matches!(
            f.editor,
            PluginSettingsEditor::Cycler { .. }
        )));
        assert!(cats[0].fields.iter().any(|f| matches!(
            f.editor,
            PluginSettingsEditor::Text { .. }
        )));
    }
}
