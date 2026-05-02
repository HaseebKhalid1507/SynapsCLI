use serde::{Deserialize, Serialize};
use std::collections::HashSet;

const BUILTIN_HELP_JSON: &str = include_str!("../assets/help.json");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HelpTopicKind {
    Branch,
    Command,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelpExample {
    pub command: String,
    pub description: String,
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
    pub usage: Option<String>,
    #[serde(default)]
    pub examples: Vec<HelpExample>,
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
    detail_idx: Option<usize>,
}

impl HelpFindState {
    pub fn new(entries: Vec<HelpEntry>, query: &str) -> Self {
        Self {
            entries,
            filter: query.trim().to_string(),
            cursor: 0,
            scroll: 0,
            visible_height: 10,
            detail_idx: None,
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
        ranked_entries(&self.entries, &self.filter)
    }

    pub fn no_results_message(&self) -> String {
        let query = self.filter.trim();
        if query.is_empty() {
            "No help topics available. Try: model, settings, plugins, sessions, doctor".to_string()
        } else {
            format!(
                "No help matches for '{}'. Try: model, settings, plugins, sessions, doctor",
                query
            )
        }
    }

    pub fn selected(&self) -> Option<&HelpEntry> {
        self.filtered_entries().get(self.cursor).copied()
    }

    pub fn open_selected(&mut self) {
        let selected_command = self.selected().map(|entry| entry.command.clone());
        self.detail_idx = selected_command
            .and_then(|command| self.entries.iter().position(|entry| entry.command == command));
    }

    pub fn close_detail(&mut self) {
        self.detail_idx = None;
    }

    pub fn detail_entry(&self) -> Option<&HelpEntry> {
        self.detail_idx.and_then(|idx| self.entries.get(idx))
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
            if protected.contains(&entry.command)
                || protected.contains(&entry.id)
                || entry.aliases.iter().any(|alias| protected.contains(alias))
            {
                tracing::warn!(
                    command = %entry.command,
                    id = %entry.id,
                    "plugin help entry conflicts with protected namespace; ignoring"
                );
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
        ranked_entries(&self.entries, query)
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
            .or_else(|| registry.entry_by_command(&format!("/help {}", topic)).map(render_entry))
            .or_else(|| registry.entry_by_command(&format!("/{}", topic.trim_start_matches('/'))).map(render_entry))
            .or_else(|| Some(format!(
                "No help topic for '{}'.\n\nTry /help find to search every topic.",
                topic
            ))),
    }
}

pub fn render_entry(entry: &HelpEntry) -> String {
    let mut lines = vec![entry.title.clone(), String::new(), entry.summary.clone()];

    if !entry.lines.is_empty() {
        lines.push(String::new());
        lines.extend(entry.lines.clone());
    }

    append_usage_examples_related(&mut lines, entry);
    lines.join("\n")
}

fn append_usage_examples_related(lines: &mut Vec<String>, entry: &HelpEntry) {
    if let Some(usage) = entry.usage.as_ref().filter(|usage| !usage.trim().is_empty()) {
        lines.push(String::new());
        lines.push("Usage".to_string());
        lines.push(format!("  {}", usage));
    }

    if !entry.examples.is_empty() {
        lines.push(String::new());
        lines.push("Examples".to_string());
        for example in &entry.examples {
            if example.description.trim().is_empty() {
                lines.push(format!("  {}", example.command));
            } else {
                lines.push(format!("  {:<16} {}", example.command, example.description));
            }
        }
    }

    if !entry.related.is_empty() {
        lines.push(String::new());
        lines.push(format!("Related: {}", entry.related.join(", ")));
    }
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

fn ranked_entries<'a>(entries: &'a [HelpEntry], query: &str) -> Vec<&'a HelpEntry> {
    let needle = query.trim().to_ascii_lowercase();
    let mut scored: Vec<(&HelpEntry, i32)> = entries
        .iter()
        .filter_map(|entry| {
            if needle.is_empty() {
                Some((entry, empty_query_score(entry)))
            } else {
                match_score(entry, &needle).map(|score| (entry, score))
            }
        })
        .collect();

    scored.sort_by(|(a, score_a), (b, score_b)| {
        score_b
            .cmp(score_a)
            .then_with(|| a.category.cmp(&b.category))
            .then_with(|| a.command.cmp(&b.command))
    });
    scored.into_iter().map(|(entry, _)| entry).collect()
}

fn empty_query_score(entry: &HelpEntry) -> i32 {
    let mut score = 0;
    if entry.common {
        score += 2_000;
    }
    if entry.category.eq_ignore_ascii_case("core") {
        score += 1_000;
    }
    score
}

fn match_score(entry: &HelpEntry, needle: &str) -> Option<i32> {
    let command = entry.command.to_ascii_lowercase();
    let title = entry.title.to_ascii_lowercase();
    if command == needle || command.trim_start_matches('/') == needle {
        return Some(11_000 + common_bonus(entry));
    }
    if title == needle {
        return Some(10_000 + common_bonus(entry));
    }
    if command.starts_with(needle)
        || command.trim_start_matches('/').starts_with(needle.trim_start_matches('/'))
    {
        return Some(8_500 + common_bonus(entry));
    }
    if title.starts_with(needle) {
        return Some(8_000 + common_bonus(entry));
    }
    if entry
        .aliases
        .iter()
        .any(|alias| field_matches(alias, needle))
    {
        return Some(6_000 + common_bonus(entry));
    }
    if entry
        .keywords
        .iter()
        .any(|keyword| field_matches(keyword, needle))
    {
        return Some(5_000 + common_bonus(entry));
    }
    if field_matches(&entry.summary, needle) {
        return Some(4_000 + common_bonus(entry));
    }
    if entry.lines.iter().any(|line| field_matches(line, needle))
        || entry.usage.as_ref().is_some_and(|usage| field_matches(usage, needle))
        || entry.examples.iter().any(|example| {
            field_matches(&example.command, needle) || field_matches(&example.description, needle)
        })
    {
        return Some(3_000 + common_bonus(entry));
    }
    None
}

fn field_matches(value: &str, needle: &str) -> bool {
    value.to_ascii_lowercase().contains(needle)
}

fn common_bonus(entry: &HelpEntry) -> i32 {
    if entry.common { 100 } else { 0 }
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
