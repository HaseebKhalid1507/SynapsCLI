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
    recently_opened: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelpFindRow<'a> {
    Category(&'a str),
    Entry(&'a HelpEntry),
}

impl<'a> HelpFindRow<'a> {
    pub fn category(&self) -> Option<&'a str> {
        match self {
            Self::Category(category) => Some(category),
            Self::Entry(_) => None,
        }
    }

    pub fn entry(&self) -> Option<&'a HelpEntry> {
        match self {
            Self::Category(_) => None,
            Self::Entry(entry) => Some(entry),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HighlightSegment {
    pub text: String,
    pub matched: bool,
}

impl HelpFindState {
    pub fn new(entries: Vec<HelpEntry>, query: &str) -> Self {
        let mut state = Self {
            entries,
            filter: query.trim().to_string(),
            cursor: 0,
            scroll: 0,
            visible_height: 10,
            detail_idx: None,
            recently_opened: Vec::new(),
        };
        state.reset_position();
        state
    }

    pub fn filter(&self) -> &str {
        &self.filter
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn result_cursor(&self) -> usize {
        self.filtered_rows()
            .iter()
            .take(self.cursor + 1)
            .filter(|row| row.entry().is_some())
            .count()
            .saturating_sub(1)
    }

    pub fn scroll(&self) -> usize {
        self.scroll
    }

    pub fn set_visible_height(&mut self, height: usize) {
        self.visible_height = height.max(1);
        self.scroll_to_cursor();
    }

    pub fn filtered_entries(&self) -> Vec<&HelpEntry> {
        ranked_entries_with_mru(&self.entries, &self.filter, &self.recently_opened)
    }

    pub fn filtered_rows(&self) -> Vec<HelpFindRow<'_>> {
        let entries = self.filtered_entries();
        if !self.filter.trim().is_empty() {
            return entries.into_iter().map(HelpFindRow::Entry).collect();
        }

        let mut rows = Vec::new();
        let mut current_category: Option<&str> = None;
        for entry in entries {
            if current_category != Some(entry.category.as_str()) {
                current_category = Some(entry.category.as_str());
                rows.push(HelpFindRow::Category(entry.category.as_str()));
            }
            rows.push(HelpFindRow::Entry(entry));
        }
        rows
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
        self.filtered_rows().get(self.cursor).and_then(HelpFindRow::entry)
    }

    pub fn open_selected(&mut self) {
        let selected_command = self.selected().map(|entry| entry.command.clone());
        if let Some(command) = selected_command.as_ref() {
            self.remember_opened(command);
        }
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
        let rows = self.filtered_rows();
        if rows.is_empty() {
            self.cursor = 0;
            self.scroll = 0;
            return;
        }
        let mut next = (self.cursor + 1).min(rows.len() - 1);
        while next < rows.len() && rows[next].entry().is_none() {
            if next == rows.len() - 1 {
                break;
            }
            next += 1;
        }
        if rows[next].entry().is_some() {
            self.cursor = next;
        }
        self.scroll_to_cursor();
    }

    pub fn move_up(&mut self) {
        let rows = self.filtered_rows();
        if rows.is_empty() {
            self.cursor = 0;
            self.scroll = 0;
            return;
        }
        let mut next = self.cursor.saturating_sub(1);
        while next > 0 && rows[next].entry().is_none() {
            next = next.saturating_sub(1);
        }
        if rows[next].entry().is_some() {
            self.cursor = next;
        }
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
        self.cursor = self.first_entry_row_index();
        self.scroll = 0;
    }

    fn first_entry_row_index(&self) -> usize {
        self.filtered_rows()
            .iter()
            .position(|row| row.entry().is_some())
            .unwrap_or(0)
    }

    fn remember_opened(&mut self, command: &str) {
        self.recently_opened.retain(|existing| existing != command);
        self.recently_opened.insert(0, command.to_string());
        self.recently_opened.truncate(10);
    }

    fn scroll_to_cursor(&mut self) {
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        }
        let bottom = self.scroll + self.visible_height;
        if self.cursor >= bottom {
            self.scroll = self.cursor + 1 - self.visible_height;
        }
        let len = self.filtered_rows().len();
        if self.cursor >= len {
            self.cursor = self.first_entry_row_index();
            self.scroll = 0;
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
            entry.source = Some(plugin_source_label(entry.source.as_deref()));
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

    pub fn entry_for_help_topic(&self, topic: &str) -> Option<&HelpEntry> {
        let normalized = normalize_help_topic(topic);
        if normalized.is_empty() {
            return None;
        }

        self.entry_by_command_exact(&format!("/help {}", normalized))
            .or_else(|| self.entry_by_command_exact(&normalized))
            .or_else(|| self.entry_by_command_exact(&format!("/{}", normalized)))
            .or_else(|| self.entry_by_command(&format!("/{}", normalized)))
            .or_else(|| self.branch(&normalized))
    }

    pub fn command_prefix_match_count(&self, partial: &str) -> usize {
        let needle = partial.trim().trim_start_matches('/').to_ascii_lowercase();
        if needle.is_empty() {
            return 0;
        }
        self.entries
            .iter()
            .filter(|entry| {
                entry.command.trim_start_matches('/').to_ascii_lowercase().starts_with(&needle)
                    || entry.aliases.iter().any(|alias| {
                        alias.trim_start_matches('/').to_ascii_lowercase().starts_with(&needle)
                    })
            })
            .count()
    }

    fn entry_by_command_exact(&self, command: &str) -> Option<&HelpEntry> {
        let needle = normalize_query_command(command);
        self.entries.iter().find(|entry| entry.command == needle)
    }
}

pub fn builtin_entries() -> Vec<HelpEntry> {
    serde_json::from_str(BUILTIN_HELP_JSON).expect("assets/help.json must be valid help JSON")
}

pub fn prefilter_query_for_slash_command(input: &str) -> Option<String> {
    let trimmed = input.trim();
    let partial = trimmed.strip_prefix('/')?.trim();
    if partial.is_empty() {
        return None;
    }
    Some(partial.to_string())
}

pub fn render_help(registry: &HelpRegistry, branch: Option<&str>) -> Option<String> {
    match branch.map(str::trim).filter(|s| !s.is_empty()) {
        None => registry.entry_by_command("/help").map(render_entry),
        Some("find") => registry.entry_by_command("/help find").map(render_entry),
        Some("topics") => Some(render_topics(registry)),
        Some("reference") => Some(render_reference(registry)),
        Some(topic) => registry
            .entry_for_help_topic(topic)
            .map(render_entry)
            .or_else(|| Some(render_unknown_topic(registry, topic))),
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

fn render_topics(registry: &HelpRegistry) -> String {
    let mut entries: Vec<&HelpEntry> = registry
        .entries()
        .iter()
        .filter(|entry| entry.topic == HelpTopicKind::Branch)
        .collect();
    entries.sort_by(|a, b| a.category.cmp(&b.category).then_with(|| a.command.cmp(&b.command)));

    let mut lines = vec![
        "Help topics".to_string(),
        String::new(),
        "Conceptual guides and discovery paths. Use /help <topic> for details.".to_string(),
        String::new(),
    ];

    for entry in entries {
        lines.push(format!("  {} — {}", entry.command, entry.summary));
    }

    lines.join("\n")
}

fn render_reference(registry: &HelpRegistry) -> String {
    let mut entries: Vec<&HelpEntry> = registry.entries().iter().collect();
    entries.sort_by(|a, b| a.category.cmp(&b.category).then_with(|| a.command.cmp(&b.command)));

    let mut lines = vec![
        "Help reference".to_string(),
        String::new(),
        "All help entries grouped by category.".to_string(),
    ];
    let mut current_category: Option<&str> = None;

    for entry in entries {
        if current_category != Some(entry.category.as_str()) {
            current_category = Some(entry.category.as_str());
            lines.push(String::new());
            lines.push(entry.category.clone());
        }
        lines.push(format!("  {} — {}", entry.command, entry.summary));
    }

    lines.join("\n")
}

pub fn source_display(entry: &HelpEntry) -> Option<String> {
    match entry.source.as_deref() {
        Some(source) if !source.trim().is_empty() => Some(plugin_source_label(Some(source))),
        _ => None,
    }
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

fn normalize_help_topic(topic: &str) -> String {
    let mut normalized = topic.trim();
    if let Some(rest) = normalized.strip_prefix("/help") {
        normalized = rest.trim();
    }
    normalized.trim_start_matches('/').trim().to_ascii_lowercase()
}

fn render_unknown_topic(registry: &HelpRegistry, topic: &str) -> String {
    let mut lines = vec![
        format!("No help topic for '{}'.", topic),
        String::new(),
        "Try /help find to search every topic.".to_string(),
    ];
    let suggestions = closest_help_matches(registry, topic, 3);
    if !suggestions.is_empty() {
        lines.push(String::new());
        lines.push(format!("Closest matches: {}", suggestions.join(", ")));
    }
    lines.join("\n")
}

fn closest_help_matches(registry: &HelpRegistry, topic: &str, limit: usize) -> Vec<String> {
    let needle = normalize_help_topic(topic);
    if needle.is_empty() {
        return Vec::new();
    }

    let mut scored: Vec<(&HelpEntry, usize)> = registry
        .entries()
        .iter()
        .filter_map(|entry| {
            suggestion_distance(entry, &needle)
                .filter(|distance| *distance <= 3)
                .map(|distance| (entry, distance))
        })
        .collect();

    scored.sort_by(|(entry_a, distance_a), (entry_b, distance_b)| {
        distance_a
            .cmp(distance_b)
            .then_with(|| entry_b.common.cmp(&entry_a.common))
            .then_with(|| entry_a.command.len().cmp(&entry_b.command.len()))
            .then_with(|| entry_a.command.cmp(&entry_b.command))
    });
    scored
        .into_iter()
        .map(|(entry, _)| entry.command.clone())
        .take(limit)
        .collect()
}

fn suggestion_distance(entry: &HelpEntry, needle: &str) -> Option<usize> {
    entry
        .aliases
        .iter()
        .map(|alias| normalize_help_topic(alias))
        .chain(std::iter::once(normalize_help_topic(&entry.command)))
        .chain(std::iter::once(entry.id.to_ascii_lowercase()))
        .chain(std::iter::once(entry.title.to_ascii_lowercase()))
        .map(|candidate| levenshtein(needle, candidate.trim_start_matches("help ")))
        .min()
}

fn levenshtein(a: &str, b: &str) -> usize {
    let b_chars: Vec<char> = b.chars().collect();
    let mut costs: Vec<usize> = (0..=b_chars.len()).collect();

    for (i, ca) in a.chars().enumerate() {
        let mut previous = costs[0];
        costs[0] = i + 1;
        for (j, cb) in b_chars.iter().enumerate() {
            let current = costs[j + 1];
            costs[j + 1] = if ca == *cb {
                previous
            } else {
                1 + previous.min(current).min(costs[j])
            };
            previous = current;
        }
    }

    costs[b_chars.len()]
}

fn plugin_source_label(source: Option<&str>) -> String {
    match source.map(str::trim).filter(|source| !source.is_empty()) {
        None => "plugin".to_string(),
        Some(source) if source.eq_ignore_ascii_case("plugin") => "plugin".to_string(),
        Some(source) if source.to_ascii_lowercase().starts_with("plugin ") => source.to_string(),
        Some(source) => format!("plugin {}", source),
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
    ranked_entries_with_mru(entries, query, &[])
}

fn ranked_entries_with_mru<'a>(entries: &'a [HelpEntry], query: &str, recently_opened: &[String]) -> Vec<&'a HelpEntry> {
    let needle = query.trim().to_ascii_lowercase();
    let mut scored: Vec<(&HelpEntry, i32)> = entries
        .iter()
        .filter_map(|entry| {
            if needle.is_empty() {
                Some((entry, empty_query_score(entry)))
            } else {
                match_score(entry, &needle).map(|score| (entry, score + mru_bonus(entry, recently_opened)))
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

fn mru_bonus(entry: &HelpEntry, recently_opened: &[String]) -> i32 {
    recently_opened
        .iter()
        .position(|command| command == &entry.command)
        .map(|idx| 50 - (idx as i32).min(49))
        .unwrap_or(0)
}

pub fn highlight_segments(text: &str, query: &str) -> Vec<HighlightSegment> {
    let needle = query.trim().to_ascii_lowercase();
    if needle.is_empty() {
        return vec![HighlightSegment { text: text.to_string(), matched: false }];
    }

    let lower = text.to_ascii_lowercase();
    let mut segments = Vec::new();
    let mut start = 0;
    while let Some(relative) = lower[start..].find(&needle) {
        let match_start = start + relative;
        let match_end = match_start + needle.len();
        if match_start > start {
            segments.push(HighlightSegment { text: text[start..match_start].to_string(), matched: false });
        }
        segments.push(HighlightSegment { text: text[match_start..match_end].to_string(), matched: true });
        start = match_end;
    }
    if start < text.len() {
        segments.push(HighlightSegment { text: text[start..].to_string(), matched: false });
    }
    if segments.is_empty() {
        segments.push(HighlightSegment { text: text.to_string(), matched: false });
    }
    segments
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
