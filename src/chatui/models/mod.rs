pub(crate) mod input;
pub(crate) use input::{handle_event, InputOutcome};

use std::collections::{BTreeMap, BTreeSet, HashSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DevProviderSelection {
    key: &'static str,
    name: &'static str,
    auth_kind: DevProviderAuth,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DevProviderAuth {
    OAuth(&'static str),
    ApiKey,
    LocalModels,
}

fn dev_model_providers() -> Vec<DevProviderSelection> {
    let mut providers = vec![
        DevProviderSelection {
            key: "claude",
            name: "Anthropic",
            auth_kind: DevProviderAuth::OAuth("anthropic"),
        },
        DevProviderSelection {
            key: "openai-codex",
            name: "OpenAI Codex",
            auth_kind: DevProviderAuth::OAuth("openai-codex"),
        },
    ];

    providers.extend(
        synaps_cli::runtime::openai::registry::providers()
            .iter()
            .map(|spec| DevProviderSelection {
                key: spec.key,
                name: spec.name,
                auth_kind: DevProviderAuth::ApiKey,
            })
    );

    providers.push(DevProviderSelection {
        key: "local",
        name: "Local",
        auth_kind: DevProviderAuth::LocalModels,
    });

    providers
}

use ratatui::{
    buffer::Buffer,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Padding, Paragraph, Widget},
};

use super::theme::THEME;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ModelsView {
    All,
    Favorites,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ModelEntry {
    pub id: String,
    pub display_id: String,
    pub label: String,
    pub tier: String,
    pub provider_key: String,
    pub provider_name: String,
    pub favorite_id: String,
    pub configured: bool,
    pub is_current: bool,
    pub is_favorite: bool,
    pub order: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExpandedModelEntry {
    pub id: String,
    pub label: String,
    pub is_favorite: bool,
    pub metadata: Vec<String>,
}

impl ExpandedModelEntry {
    #[allow(dead_code)]
    pub(crate) fn new(id: String, label: String, is_favorite: bool) -> Self {
        Self { id, label, is_favorite, metadata: Vec::new() }
    }

    pub(crate) fn with_metadata(id: String, label: String, is_favorite: bool, metadata: Vec<String>) -> Self {
        Self { id, label, is_favorite, metadata }
    }

    pub(crate) fn metadata_label(&self) -> String {
        self.metadata.join(" · ")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FuzzyScore {
    gaps: usize,
    first_match: usize,
    haystack_len: usize,
}

pub(crate) fn fuzzy_model_score(query: &str, haystack: &str) -> Option<FuzzyScore> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Some(FuzzyScore { gaps: 0, first_match: 0, haystack_len: haystack.chars().count() });
    }

    let haystack_lower = haystack.to_lowercase();
    let mut search_start = 0usize;
    let mut positions = Vec::new();
    for ch in query.chars() {
        let tail = &haystack_lower[search_start..];
        let found = tail.char_indices().find(|(_, candidate)| *candidate == ch)?;
        let pos = search_start + found.0;
        positions.push(pos);
        search_start = pos + found.1.len_utf8();
    }
    let first_match = positions.first().copied().unwrap_or(0);
    let span = positions.last().copied().unwrap_or(first_match).saturating_sub(first_match) + 1;
    let gaps = span.saturating_sub(query.chars().count());
    Some(FuzzyScore { gaps, first_match, haystack_len: haystack_lower.len() })
}

pub(crate) fn sort_expanded_models(models: &mut [ExpandedModelEntry], query: &str) {
    models.sort_by(|a, b| {
        let score_a = fuzzy_model_score(query, &format!("{} {}", a.id, a.label));
        let score_b = fuzzy_model_score(query, &format!("{} {}", b.id, b.label));
        match (score_a, score_b) {
            (Some(a_score), Some(b_score)) => a_score
                .gaps
                .cmp(&b_score.gaps)
                .then_with(|| a_score.first_match.cmp(&b_score.first_match))
                .then_with(|| a_score.haystack_len.cmp(&b_score.haystack_len))
                .then_with(|| a.id.cmp(&b.id)),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.id.cmp(&b.id),
        }
    });
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ModelSection {
    pub provider_key: String,
    pub provider_name: String,
    pub configured: bool,
    pub entries: Vec<ModelEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum VisibleRow {
    Section { idx: usize },
    Model { section: usize, idx: usize },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ExpandedLoadState {
    Loading,
    Ready(Vec<ExpandedModelEntry>),
    Error(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExpandedModelsState {
    pub provider_key: String,
    pub provider_name: String,
    pub cursor: usize,
    pub search: String,
    pub load_state: ExpandedLoadState,
}

#[derive(Clone, Debug)]
pub(crate) struct ModelsModalState {
    pub cursor: usize,
    pub search: String,
    pub view: ModelsView,
    pub collapsed: HashSet<String>,
    pub favorites: BTreeSet<String>,
    pub expanded: Option<ExpandedModelsState>,
}

impl ModelsModalState {
    pub fn new() -> Self {
        let favorites: BTreeSet<String> = synaps_cli::config::load_config()
            .favorite_models
            .into_iter()
            .collect();
        let view = if favorites.is_empty() { ModelsView::All } else { ModelsView::Favorites };
        Self {
            cursor: 0,
            search: String::new(),
            view,
            collapsed: HashSet::new(),
            favorites,
            expanded: None,
        }
    }

    pub fn refresh_favorites(&mut self) {
        self.favorites = synaps_cli::config::load_config()
            .favorite_models
            .into_iter()
            .collect();
        if self.favorites.is_empty() && self.view == ModelsView::Favorites {
            self.view = ModelsView::All;
        }
    }
}

pub(crate) fn normalize_favorite_id(model: &str) -> String {
    if model.contains('/') {
        model.to_string()
    } else {
        format!("claude/{model}")
    }
}

pub(crate) fn model_id_for_runtime(favorite_id: &str) -> String {
    favorite_id
        .strip_prefix("claude/")
        .unwrap_or(favorite_id)
        .to_string()
}

pub(crate) fn build_sections(current_model: &str, state: &ModelsModalState) -> Vec<ModelSection> {
    let config = synaps_cli::config::load_config();
    let logged_in_oauth = logged_in_oauth_providers();
    build_sections_from_parts(current_model, state, &config.provider_keys, &logged_in_oauth)
}

fn logged_in_oauth_providers() -> BTreeSet<&'static str> {
    ["anthropic", "openai-codex"]
        .into_iter()
        .filter(|storage_key| {
            synaps_cli::auth::load_provider_auth(storage_key)
                .ok()
                .flatten()
                .is_some_and(|creds| !synaps_cli::auth::is_token_expired(&creds))
        })
        .collect()
}

fn build_sections_from_parts(
    current_model: &str,
    state: &ModelsModalState,
    provider_keys: &BTreeMap<String, String>,
    logged_in_oauth: &BTreeSet<&'static str>,
) -> Vec<ModelSection> {
    let query = state.search.trim().to_lowercase();
    let favorites_only = state.view == ModelsView::Favorites;
    let current_fav = normalize_favorite_id(current_model);
    let mut sections = Vec::new();

    for provider in dev_model_providers() {
        if !dev_provider_is_logged_in(&provider, provider_keys, logged_in_oauth) {
            continue;
        }

        let mut entries: Vec<ModelEntry> = provider_model_selections(&provider, provider_keys)
            .into_iter()
            .map(|model| {
                let (runtime_id, favorite_id) = if provider.key == "claude" {
                    (model.id.clone(), format!("claude/{}", model.id))
                } else {
                    let full_id = format!("{}/{}", provider.key, model.id);
                    (full_id.clone(), full_id)
                };
                ModelEntry {
                    id: runtime_id.clone(),
                    display_id: model.id.clone(),
                    label: model.label.clone(),
                    tier: model.tier.clone(),
                    provider_key: provider.key.to_string(),
                    provider_name: provider.name.to_string(),
                    configured: true,
                    is_current: current_model == runtime_id || current_fav == favorite_id,
                    is_favorite: state.favorites.contains(&favorite_id),
                    favorite_id,
                    order: model.order,
                }
            })
            .filter(|m| model_matches(m, &query, favorites_only))
            .collect();
        pin_favorites_first(&mut entries);
        if !entries.is_empty() {
            sections.push(ModelSection {
                provider_key: provider.key.to_string(),
                provider_name: provider.name.to_string(),
                configured: true,
                entries,
            });
        }
    }

    sections
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ModelSelectionItem {
    id: String,
    label: String,
    tier: String,
    order: usize,
}

fn provider_model_selections(
    provider: &DevProviderSelection,
    provider_keys: &BTreeMap<String, String>,
) -> Vec<ModelSelectionItem> {
    if provider.auth_kind == DevProviderAuth::LocalModels {
        return local_model_ids(provider_keys)
            .into_iter()
            .enumerate()
            .map(|(order, id)| ModelSelectionItem {
                label: local_model_label(&id),
                id,
                tier: "local".to_string(),
                order,
            })
            .collect();
    }

    provider_static_model_seeds(provider)
        .into_iter()
        .enumerate()
        .map(|(order, (id, label, tier))| ModelSelectionItem {
            id,
            label,
            tier,
            order,
        })
        .collect()
}

fn provider_static_model_seeds(provider: &DevProviderSelection) -> Vec<(String, String, String)> {
    match provider.key {
        "claude" => synaps_cli::models::KNOWN_MODELS
            .iter()
            .enumerate()
            .map(|(idx, (id, label))| {
                let tier = match idx {
                    0 => "S+",
                    1 | 2 => "S",
                    _ => "A",
                };
                ((*id).to_string(), (*label).to_string(), tier.to_string())
            })
            .collect(),
        "openai-codex" => synaps_cli::runtime::openai::catalog::codex_static_catalog_models()
            .into_iter()
            .map(|model| {
                let tier = match model.id.as_str() {
                    "gpt-5.5" => "S+",
                    _ => "",
                };
                (model.id, model.label.unwrap_or_default(), tier.to_string())
            })
            .collect(),
        key => synaps_cli::runtime::openai::registry::providers()
            .iter()
            .find(|spec| spec.key == key)
            .map(|spec| {
                spec.models
                    .iter()
                    .map(|(id, label, tier)| ((*id).to_string(), (*label).to_string(), (*tier).to_string()))
                    .collect()
            })
            .unwrap_or_default(),
    }
}

fn local_model_ids(provider_keys: &BTreeMap<String, String>) -> Vec<String> {
    provider_keys
        .get("local.models")
        .map(|value| {
            value
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn local_model_label(id: &str) -> String {
    id.split(':')
        .next()
        .unwrap_or(id)
        .replace('-', " ")
        .split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn dev_provider_is_logged_in(
    provider: &DevProviderSelection,
    provider_keys: &BTreeMap<String, String>,
    logged_in_oauth: &BTreeSet<&'static str>,
) -> bool {
    match provider.auth_kind {
        DevProviderAuth::LocalModels => !local_model_ids(provider_keys).is_empty(),
        DevProviderAuth::ApiKey => provider_keys
            .get(provider.key)
            .is_some_and(|v| !v.is_empty())
            || registry_env_vars(provider.key)
                .iter()
                .any(|var| std::env::var(var).ok().is_some_and(|v| !v.is_empty())),
        DevProviderAuth::OAuth(storage_key) => logged_in_oauth.contains(storage_key),
    }
}

fn registry_env_vars(provider_key: &str) -> &'static [&'static str] {
    synaps_cli::runtime::openai::registry::providers()
        .iter()
        .find(|provider| provider.key == provider_key)
        .map(|provider| provider.env_vars)
        .unwrap_or(&[])
}

fn model_matches(model: &ModelEntry, query: &str, favorites_only: bool) -> bool {
    if favorites_only && !model.is_favorite {
        return false;
    }
    if query.is_empty() {
        return true;
    }
    let haystack = format!(
        "{} {} {} {}",
        model.id, model.display_id, model.label, model.provider_name
    )
    .to_lowercase();
    haystack.contains(query)
}

fn pin_favorites_first(entries: &mut [ModelEntry]) {
    entries.sort_by(|a, b| {
        b.is_favorite
            .cmp(&a.is_favorite)
            .then_with(|| a.order.cmp(&b.order))
            .then_with(|| a.display_id.cmp(&b.display_id))
    });
}

pub(crate) fn visible_rows(sections: &[ModelSection], state: &ModelsModalState) -> Vec<VisibleRow> {
    let mut rows = Vec::new();
    for (section_idx, section) in sections.iter().enumerate() {
        rows.push(VisibleRow::Section { idx: section_idx });
        if !state.collapsed.contains(&section.provider_key) {
            for (idx, _) in section.entries.iter().enumerate() {
                rows.push(VisibleRow::Model { section: section_idx, idx });
            }
        }
    }
    rows
}

pub(crate) fn selected_provider<'a>(sections: &'a [ModelSection], state: &ModelsModalState) -> Option<&'a ModelSection> {
    let rows = visible_rows(sections, state);
    match rows.get(state.cursor)? {
        VisibleRow::Section { idx } => sections.get(*idx),
        VisibleRow::Model { section, .. } => sections.get(*section),
    }
}

pub(crate) fn expanded_visible_models(state: &ModelsModalState) -> Vec<ExpandedModelEntry> {
    let Some(expanded) = state.expanded.as_ref() else { return Vec::new(); };
    let ExpandedLoadState::Ready(models) = &expanded.load_state else { return Vec::new(); };
    let mut visible: Vec<_> = models
        .iter()
        .filter(|model| fuzzy_model_score(&expanded.search, &format!("{} {}", model.id, model.label)).is_some())
        .cloned()
        .collect();
    sort_expanded_models(&mut visible, &expanded.search);
    visible
}

pub(crate) fn selected_expanded_model(state: &ModelsModalState) -> Option<ExpandedModelEntry> {
    let expanded = state.expanded.as_ref()?;
    expanded_visible_models(state).get(expanded.cursor).cloned()
}

pub(crate) fn set_expanded_models(
    state: &mut ModelsModalState,
    provider_key: &str,
    result: Result<Vec<ExpandedModelEntry>, String>,
) {
    let Some(expanded) = state.expanded.as_mut() else { return; };
    if expanded.provider_key != provider_key {
        return;
    }
    expanded.cursor = 0;
    expanded.load_state = match result {
        Ok(mut models) => {
            for model in &mut models {
                model.is_favorite = state.favorites.contains(&model.id);
            }
            ExpandedLoadState::Ready(models)
        }
        Err(err) => ExpandedLoadState::Error(err),
    };
}

pub(crate) fn selected_model<'a>(sections: &'a [ModelSection], state: &ModelsModalState) -> Option<&'a ModelEntry> {
    let rows = visible_rows(sections, state);
    match rows.get(state.cursor)? {
        VisibleRow::Model { section, idx } => sections.get(*section)?.entries.get(*idx),
        VisibleRow::Section { .. } => None,
    }
}

pub(crate) fn render(frame: &mut ratatui::Frame<'_>, area: Rect, state: &ModelsModalState, current_model: &str) {
    let sections = build_sections(current_model, state);
    frame.render_widget(Clear, area);
    frame.render_widget(ModelsModalWidget { state, sections, current_model }, area);
}

struct ModelsModalWidget<'a> {
    state: &'a ModelsModalState,
    sections: Vec<ModelSection>,
    current_model: &'a str,
}

impl Widget for ModelsModalWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let theme = THEME.load();
        let block = Block::default()
            .title(" Models ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.border_active))
            .padding(Padding { left: 2, right: 2, top: 1, bottom: 1 });
        let inner = block.inner(area);
        block.render(area, buf);

        let rows = visible_rows(&self.sections, self.state);
        let model_count: usize = self.sections.iter().map(|s| s.entries.len()).sum();
        let favorite_count = self.state.favorites.len();
        let view = match self.state.view {
            ModelsView::All => "[✦ All]  ★ Favorites",
            ModelsView::Favorites => "✦ All  [★ Favorites]",
        };

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(inner);

        Paragraph::new(Line::from(vec![
            Span::styled("◇ Switch model", Style::default().fg(theme.header_fg).add_modifier(Modifier::BOLD)),
            Span::styled(format!(" · {model_count} available · {favorite_count} ★ favorites"), Style::default().fg(theme.muted)),
        ]))
        .render(chunks[0], buf);

        Paragraph::new(Line::from(vec![
            Span::styled("Search: ", Style::default().fg(theme.muted)),
            Span::styled(if self.state.search.is_empty() { "▎".to_string() } else { format!("{}▎", self.state.search) }, Style::default().fg(theme.header_fg)),
        ]))
        .render(chunks[1], buf);

        Paragraph::new(Line::from(vec![
            Span::styled("View:   ", Style::default().fg(theme.muted)),
            Span::styled(view, Style::default().fg(theme.help_fg)),
            Span::styled(format!("   current: {}", self.current_model), Style::default().fg(theme.muted)),
        ]))
        .render(chunks[2], buf);

        let list_height = chunks[3].height as usize;
        let offset = self.state.cursor.saturating_sub(list_height.saturating_sub(1));
        let lines: Vec<Line> = rows
            .iter()
            .enumerate()
            .skip(offset)
            .take(list_height)
            .map(|(row_idx, row)| match row {
                VisibleRow::Section { idx } => {
                    let section = &self.sections[*idx];
                    let selected = row_idx == self.state.cursor;
                    let glyph = if self.state.collapsed.contains(&section.provider_key) { "▸" } else { "▾" };
                    let style = if selected {
                        Style::default().fg(theme.header_fg).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(theme.help_fg)
                    };
                    Line::from(vec![
                        Span::styled(if selected { "● " } else { "  " }, Style::default().fg(theme.border_active)),
                        Span::styled(format!("{glyph} {}", section.provider_name), style),
                        Span::styled(format!(" ({})", section.entries.len()), Style::default().fg(theme.muted)),
                    ])
                }
                VisibleRow::Model { section, idx } => {
                    let entry = &self.sections[*section].entries[*idx];
                    let selected = row_idx == self.state.cursor;
                    let marker = if selected { "●" } else { "○" };
                    let fav = if entry.is_favorite { "★" } else { " " };
                    let current = if entry.is_current { " default" } else { "" };
                    let tier = if entry.tier.is_empty() { String::new() } else { format!(" {}", entry.tier) };
                    let style = if selected {
                        Style::default().fg(theme.header_fg).add_modifier(Modifier::BOLD)
                    } else if entry.is_current {
                        Style::default().fg(theme.status_ready)
                    } else {
                        Style::default().fg(theme.input_fg)
                    };
                    Line::from(vec![
                        Span::styled(format!("  {marker} "), Style::default().fg(theme.border_active)),
                        Span::styled(format!("{:<38}", entry.id), style),
                        Span::styled(format!(" — {}", entry.label), Style::default().fg(theme.muted)),
                        Span::styled(format!(" {fav}"), Style::default().fg(theme.status_streaming)),
                        Span::styled(tier, Style::default().fg(theme.muted)),
                        Span::styled(current, Style::default().fg(theme.status_ready)),
                    ])
                }
            })
            .collect();

        let empty_line = if self.state.view == ModelsView::Favorites {
            "No favorite models yet — press Tab for All, then f to favorite."
        } else {
            "No configured models match the current search."
        };
        let list = if lines.is_empty() {
            vec![Line::from(Span::styled(empty_line, Style::default().fg(theme.muted)))]
        } else {
            lines
        };
        Paragraph::new(list).render(chunks[3], buf);

        Paragraph::new("↑/↓ select • Enter use • f favorite • e expand • Tab view • c collapse • Esc close")
            .style(Style::default().fg(theme.muted))
            .alignment(Alignment::Center)
            .render(chunks[4], buf);

        if let Some(expanded) = self.state.expanded.as_ref() {
            render_expanded_lightbox(area, buf, self.state, expanded);
        }
    }
}

fn render_expanded_lightbox(area: Rect, buf: &mut Buffer, state: &ModelsModalState, expanded: &ExpandedModelsState) {
    let theme = THEME.load();
    let width = area.width.saturating_sub(10).min(110);
    let height = area.height.saturating_sub(6).min(28);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    let popup = Rect { x, y, width, height };

    Clear.render(popup, buf);
    let status = match &expanded.load_state {
        ExpandedLoadState::Loading => "loading…".to_string(),
        ExpandedLoadState::Ready(models) => format!("{} loaded", models.len()),
        ExpandedLoadState::Error(_) => "error".to_string(),
    };
    let block = Block::default()
        .title(format!(" {} models · {} ", expanded.provider_name, status))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border_active))
        .padding(Padding { left: 2, right: 2, top: 1, bottom: 1 });
    let inner = block.inner(popup);
    block.render(popup, buf);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(inner);

    Paragraph::new(Line::from(vec![
        Span::styled("Search: ", Style::default().fg(theme.muted)),
        Span::styled(if expanded.search.is_empty() { "▎".to_string() } else { format!("{}▎", expanded.search) }, Style::default().fg(theme.header_fg)),
    ]))
    .render(chunks[0], buf);

    let list_height = chunks[1].height as usize;
    let visible = expanded_visible_models(state);
    let offset = expanded.cursor.saturating_sub(list_height.saturating_sub(1));
    let lines = match &expanded.load_state {
        ExpandedLoadState::Loading => vec![Line::from(Span::styled("Loading provider models…", Style::default().fg(theme.muted)))],
        ExpandedLoadState::Error(err) => vec![Line::from(Span::styled(format!("Failed to load models: {err}"), Style::default().fg(theme.error_color)))],
        ExpandedLoadState::Ready(_) if visible.is_empty() => vec![Line::from(Span::styled("No models match the current search.", Style::default().fg(theme.muted)))],
        ExpandedLoadState::Ready(_) => visible
            .iter()
            .enumerate()
            .skip(offset)
            .take(list_height)
            .map(|(idx, model)| {
                let selected = idx == expanded.cursor;
                let marker = if selected { "●" } else { "○" };
                let fav = if model.is_favorite { "★" } else { " " };
                let style = if selected {
                    Style::default().fg(theme.header_fg).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.input_fg)
                };
                let meta = model.metadata_label();
                let meta_text = if meta.is_empty() { String::new() } else { format!(" · {meta}") };
                Line::from(vec![
                    Span::styled(format!("{marker} "), Style::default().fg(theme.border_active)),
                    Span::styled(format!("{:<48}", model.id), style),
                    Span::styled(format!(" — {}{}", model.label, meta_text), Style::default().fg(theme.muted)),
                    Span::styled(format!(" {fav}"), Style::default().fg(theme.status_streaming)),
                ])
            })
            .collect(),
    };
    Paragraph::new(lines).render(chunks[1], buf);

    Paragraph::new("type to fuzzy-search • ↑/↓ select • Enter use • f favorite • Esc back")
        .style(Style::default().fg(theme.muted))
        .alignment(Alignment::Center)
        .render(chunks[2], buf);
}

#[cfg(test)]
mod tests {
    use super::*;


    #[test]
    fn expanded_entry_metadata_label_joins_badges() {
        let entry = ExpandedModelEntry::with_metadata(
            "openrouter/deepseek/deepseek-r1".to_string(),
            "DeepSeek R1".to_string(),
            false,
            vec!["64K ctx".to_string(), "thinking".to_string()],
        );
        assert_eq!(entry.metadata_label(), "64K ctx · thinking");
    }

    #[test]
    fn expanded_entry_new_has_no_metadata() {
        let entry = ExpandedModelEntry::new("model".to_string(), "Model".to_string(), false);
        assert!(entry.metadata.is_empty());
        assert_eq!(entry.metadata_label(), "");
    }

    #[test]
    fn claude_favorite_ids_round_trip_to_runtime_ids() {
        assert_eq!(normalize_favorite_id("claude-opus-4-7"), "claude/claude-opus-4-7");
        assert_eq!(model_id_for_runtime("claude/claude-opus-4-7"), "claude-opus-4-7");
        assert_eq!(model_id_for_runtime("groq/llama-3.3-70b-versatile"), "groq/llama-3.3-70b-versatile");
    }

    #[test]
    fn favorites_view_only_keeps_favorite_entries() {
        let mut state = ModelsModalState {
            cursor: 0,
            search: String::new(),
            view: ModelsView::Favorites,
            collapsed: HashSet::new(),
            favorites: BTreeSet::from(["claude/claude-opus-4-7".to_string()]),
            expanded: None,
        };
        let sections = build_sections_from_parts(
            "claude-opus-4-7",
            &state,
            &BTreeMap::new(),
            &BTreeSet::from(["anthropic"]),
        );
        let total: usize = sections.iter().map(|s| s.entries.len()).sum();
        assert_eq!(total, 1);
        assert_eq!(sections[0].entries[0].favorite_id, "claude/claude-opus-4-7");

        state.view = ModelsView::All;
        let sections = build_sections_from_parts(
            "claude-opus-4-7",
            &state,
            &BTreeMap::new(),
            &BTreeSet::from(["anthropic"]),
        );
        assert!(sections.iter().map(|s| s.entries.len()).sum::<usize>() > 1);
    }

    #[test]
    fn unconfigured_openai_providers_are_hidden() {
        let state = ModelsModalState {
            cursor: 0,
            search: String::new(),
            view: ModelsView::All,
            collapsed: HashSet::new(),
            favorites: BTreeSet::new(),
            expanded: None,
        };
        let sections = build_sections_from_parts(
            "claude-opus-4-7",
            &state,
            &BTreeMap::new(),
            &BTreeSet::from(["anthropic"]),
        );
        assert!(sections.iter().any(|s| s.provider_key == "claude"));
        assert!(!sections.iter().any(|s| s.provider_key == "groq"));
        assert!(!sections.iter().any(|s| s.provider_key == "openrouter"));
    }

    #[test]
    fn fuzzy_model_matches_are_case_insensitive_subsequences() {
        assert!(fuzzy_model_score("qwc", "Qwen3 Coder 480B").is_some());
        assert!(fuzzy_model_score("xyz", "Qwen3 Coder 480B").is_none());
    }

    #[test]
    fn fuzzy_model_matches_rank_contiguous_and_early_matches_first() {
        let mut models = vec![
            ExpandedModelEntry::new("provider/alpha-qwen-coder".to_string(), "alpha qwen coder".to_string(), false),
            ExpandedModelEntry::new("provider/qwen3-coder".to_string(), "qwen3 coder".to_string(), false),
            ExpandedModelEntry::new("provider/my-q-w-e-n".to_string(), "my q w e n".to_string(), false),
        ];
        sort_expanded_models(&mut models, "qwen");
        assert_eq!(models[0].id, "provider/qwen3-coder");
        assert_eq!(models[2].id, "provider/my-q-w-e-n");
    }

    #[test]
    fn local_provider_is_hidden_when_no_local_models_are_configured() {
        let state = ModelsModalState {
            cursor: 0,
            search: String::new(),
            view: ModelsView::All,
            collapsed: HashSet::new(),
            favorites: BTreeSet::new(),
            expanded: None,
        };
        let sections = build_sections_from_parts(
            "openai-codex/gpt-5.5",
            &state,
            &BTreeMap::new(),
            &BTreeSet::from(["openai-codex"]),
        );

        assert!(!sections.iter().any(|s| s.provider_key == "local"));
    }

    #[test]
    fn local_provider_uses_explicit_local_models_config() {
        let state = ModelsModalState {
            cursor: 0,
            search: String::new(),
            view: ModelsView::All,
            collapsed: HashSet::new(),
            favorites: BTreeSet::new(),
            expanded: None,
        };
        let provider_keys = BTreeMap::from([
            ("local.models".to_string(), "qwen3-coder:latest, devstral:latest".to_string()),
        ]);
        let sections = build_sections_from_parts(
            "local/qwen3-coder:latest",
            &state,
            &provider_keys,
            &BTreeSet::new(),
        );

        let local = sections.iter().find(|s| s.provider_key == "local").expect("local section");
        let ids: Vec<_> = local.entries.iter().map(|entry| entry.id.as_str()).collect();
        assert_eq!(ids, vec!["local/qwen3-coder:latest", "local/devstral:latest"]);
    }

    #[test]
    fn dev_selected_models_populate_for_logged_in_providers() {
        let state = ModelsModalState {
            cursor: 0,
            search: String::new(),
            view: ModelsView::All,
            collapsed: HashSet::new(),
            favorites: BTreeSet::new(),
            expanded: None,
        };
        let provider_keys = BTreeMap::from([("openrouter".to_string(), "sk-test".to_string())]);
        let sections = build_sections_from_parts(
            "openai-codex/gpt-5.5",
            &state,
            &provider_keys,
            &BTreeSet::from(["anthropic", "openai-codex"]),
        );

        assert!(sections.iter().any(|s| s.provider_key == "claude"));
        assert!(sections.iter().any(|s| s.provider_key == "openai-codex"));
        assert!(sections.iter().any(|s| s.provider_key == "openrouter"));
        assert!(!sections.iter().any(|s| s.provider_key == "groq"));

        let all_ids: Vec<_> = sections
            .iter()
            .flat_map(|s| s.entries.iter().map(|entry| entry.id.as_str()))
            .collect();
        assert!(all_ids.contains(&"openrouter/qwen/qwen3-coder"));
        assert!(all_ids.contains(&"openrouter/google/gemma-3-27b-it"));
    }

    #[test]
    fn configured_registry_provider_uses_all_registry_static_seeds() {
        let state = ModelsModalState {
            cursor: 0,
            search: String::new(),
            view: ModelsView::All,
            collapsed: HashSet::new(),
            favorites: BTreeSet::new(),
            expanded: None,
        };
        let provider_keys = BTreeMap::from([("nvidia".to_string(), "sk-test".to_string())]);
        let sections = build_sections_from_parts(
            "nvidia/meta/llama-3.3-70b-instruct",
            &state,
            &provider_keys,
            &BTreeSet::new(),
        );
        let nvidia = sections.iter().find(|s| s.provider_key == "nvidia").expect("nvidia section");
        let registry_count = synaps_cli::runtime::openai::registry::providers()
            .iter()
            .find(|provider| provider.key == "nvidia")
            .expect("nvidia registry provider")
            .models
            .len();
        assert_eq!(nvidia.entries.len(), registry_count);
        assert!(nvidia.entries.iter().any(|entry| entry.id == "nvidia/stepfun-ai/step-3.5-flash"));
    }
}
