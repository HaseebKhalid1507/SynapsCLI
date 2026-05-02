use serde::{Deserialize, Serialize};
use std::collections::HashSet;

const BUILTIN_HELP_JSON: &str = include_str!("../assets/help.json");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HelpTopicKind {
    Branch,
    Command,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelpEntry {
    pub id: String,
    pub command: String,
    pub title: String,
    pub summary: String,
    pub category: String,
    pub topic: HelpTopicKind,
    pub protected: bool,
    pub common: bool,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub lines: Vec<String>,
    #[serde(default)]
    pub related: Vec<String>,
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Clone)]
pub struct HelpRegistry {
    entries: Vec<HelpEntry>,
}

#[derive(Debug, Clone)]
pub struct HelpFindState {
    entries: Vec<HelpEntry>,
    filter: String,
    cursor: usize,
    scroll: usize,
    visible_height: usize,
}

impl HelpFindState {
    pub fn new(entries: Vec<HelpEntry>, query: &str) -> Self {
        Self {
            entries,
            filter: query.trim().to_string(),
            cursor: 0,
            scroll: 0,
            visible_height: 10,
        }
    }

    pub fn filter(&self) -> &str {
        &self.filter
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn scroll(&self) -> usize {
        self.scroll
    }

    pub fn set_visible_height(&mut self, height: usize) {
        self.visible_height = height.max(1);
        self.scroll_to_cursor();
    }

    pub fn filtered_entries(&self) -> Vec<&HelpEntry> {
        let needle = self.filter.to_ascii_lowercase();
        if needle.trim().is_empty() {
            return self.entries.iter().collect();
        }
        self.entries
            .iter()
            .filter(|entry| searchable_text(entry).contains(&needle))
            .collect()
    }

    pub fn selected(&self) -> Option<&HelpEntry> {
        self.filtered_entries().get(self.cursor).copied()
    }

    pub fn move_down(&mut self) {
        let len = self.filtered_entries().len();
        if len == 0 {
            self.cursor = 0;
            self.scroll = 0;
            return;
        }
        self.cursor = (self.cursor + 1).min(len - 1);
        self.scroll_to_cursor();
    }

    pub fn move_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
        self.scroll_to_cursor();
    }

    pub fn push_char(&mut self, ch: char) {
        self.filter.push(ch);
        self.reset_position();
    }

    pub fn backspace(&mut self) {
        self.filter.pop();
        self.reset_position();
    }

    pub fn clear_filter(&mut self) {
        self.filter.clear();
        self.reset_position();
    }

    fn reset_position(&mut self) {
        self.cursor = 0;
        self.scroll = 0;
    }

    fn scroll_to_cursor(&mut self) {
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        }
        let bottom = self.scroll + self.visible_height;
        if self.cursor >= bottom {
            self.scroll = self.cursor + 1 - self.visible_height;
        }
    }
}

impl HelpRegistry {
    pub fn new(core_entries: Vec<HelpEntry>, plugin_entries: Vec<HelpEntry>) -> Self {
        let protected = protected_commands(&core_entries);
        let mut seen = HashSet::new();
        let mut entries = Vec::new();

        for mut entry in core_entries {
            normalize_command(&mut entry);
            if seen.insert(entry.command.clone()) {
                entries.push(entry);
            }
        }

        for mut entry in plugin_entries {
            normalize_command(&mut entry);
            if protected.contains(&entry.command) || protected.contains(&entry.id) {
                continue;
            }
            if entry.protected {
                entry.protected = false;
            }
            if entry.source.is_none() {
                entry.source = Some("plugin".to_string());
            }
            if seen.insert(entry.command.clone()) {
                entries.push(entry);
            }
        }

        entries.sort_by(|a, b| a.command.cmp(&b.command));
        Self { entries }
    }

    pub fn entries(&self) -> &[HelpEntry] {
        &self.entries
    }

    pub fn entry_by_command(&self, command: &str) -> Option<&HelpEntry> {
        let needle = normalize_query_command(command);
        self.entries.iter().find(|entry| {
            entry.command == needle || entry.aliases.iter().any(|alias| normalize_query_command(alias) == needle)
        })
    }

    pub fn branch(&self, topic: &str) -> Option<&HelpEntry> {
        let normalized = topic.trim().trim_start_matches("/help").trim().trim_start_matches('/');
        self.entries.iter().find(|entry| {
            entry.topic == HelpTopicKind::Branch
                && (entry.id == normalized
                    || entry.command == format!("/help {}", normalized)
                    || entry.aliases.iter().any(|alias| alias.trim_start_matches("/help ") == normalized))
        })
    }

    pub fn search(&self, query: &str) -> Vec<&HelpEntry> {
        let needle = query.trim().to_ascii_lowercase();
        if needle.is_empty() {
            return self.entries.iter().collect();
        }
        self.entries
            .iter()
            .filter(|entry| searchable_text(entry).contains(&needle))
            .collect()
    }
}

pub fn builtin_entries() -> Vec<HelpEntry> {
    serde_json::from_str(BUILTIN_HELP_JSON).expect("assets/help.json must be valid help JSON")
}

pub fn render_help(registry: &HelpRegistry, branch: Option<&str>) -> Option<String> {
    match branch.map(str::trim).filter(|s| !s.is_empty()) {
        None => registry.entry_by_command("/help").map(render_entry),
        Some("find") => registry.entry_by_command("/help find").map(render_entry),
        Some(topic) => registry
            .branch(topic)
            .map(render_entry)
            .or_else(|| Some(format!(
                "No help topic for '{}'.\n\nTry /help find to search every topic.",
                topic
            ))),
    }
}

pub fn render_entry(entry: &HelpEntry) -> String {
    if !entry.lines.is_empty() {
        let mut lines = vec![format!("{}", entry.title)];
        lines.push(String::new());
        lines.extend(entry.lines.clone());
        return lines.join("\n");
    }

    let mut lines = vec![format!("{} — {}", entry.title, entry.summary)];
    if !entry.related.is_empty() {
        lines.push(String::new());
        lines.push(format!("Related: {}", entry.related.join(", ")));
    }
    lines.join("\n")
}

fn normalize_command(entry: &mut HelpEntry) {
    entry.command = normalize_query_command(&entry.command);
    entry.aliases = entry.aliases.iter().map(|alias| normalize_query_command(alias)).collect();
}

fn normalize_query_command(command: &str) -> String {
    let trimmed = command.trim();
    if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        format!("/{}", trimmed)
    }
}

fn protected_commands(entries: &[HelpEntry]) -> HashSet<String> {
    let mut protected = HashSet::new();
    for entry in entries.iter().filter(|entry| entry.protected) {
        protected.insert(normalize_query_command(&entry.command));
        protected.insert(entry.id.clone());
        for alias in &entry.aliases {
            protected.insert(normalize_query_command(alias));
        }
    }
    for builtin in crate::skills::BUILTIN_COMMANDS {
        protected.insert(format!("/{}", builtin));
        protected.insert((*builtin).to_string());
    }
    protected
}

fn searchable_text(entry: &HelpEntry) -> String {
    let mut text = format!(
        "{} {} {} {} {}",
        entry.command, entry.title, entry.summary, entry.category, entry.id
    );
    for alias in &entry.aliases {
        text.push(' ');
        text.push_str(alias);
    }
    for keyword in &entry.keywords {
        text.push(' ');
        text.push_str(keyword);
    }
    for line in &entry.lines {
        text.push(' ');
        text.push_str(line);
    }
    text.to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_help_json_loads() {
        let entries = builtin_entries();
        assert!(entries.iter().any(|entry| entry.command == "/help"));
        assert!(entries.iter().any(|entry| entry.command == "/help find"));
    }
}
