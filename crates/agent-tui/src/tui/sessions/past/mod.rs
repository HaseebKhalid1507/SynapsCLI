//! The **Past** tab — an archive browser over every saved session, backed by
//! `agent_core::session_catalog` (headers + index join, preview cache,
//! facets, query tokens, deep search, rename).
//!
//! Nothing here touches the disk on the draw loop:
//! - [`tasks::spawn_load`] (at modal open) loads the catalog on
//!   `spawn_blocking`, sends it, then refreshes the preview cache in the
//!   background (only sessions whose files changed) and sends the catalog
//!   again with first/last messages and true message counts.
//! - Deep search and rename run on `spawn_blocking` too ([`tasks::run`]).
//!
//! State is cheap to clone into every render snapshot: the catalog, the view
//! and the display lines are `Arc`s, rebuilt only when the query, facets,
//! sort or data change ([`PastState::recompute`]).
//!
//! Layout: a search line (`/`), then the facet sidebar | the day-grouped
//! list, then a preview strip for the selected session. Focus moves between
//! sidebar and list with ←/→ (tab stays the Live/Past switch).

pub(crate) mod draw;
pub(crate) mod input;
pub(crate) mod tasks;

#[cfg(test)]
mod tests;

pub(crate) use draw::render;

use agent_core::session_catalog::{
    build_view, day_groups, parse_query, Catalog, CatalogEntry, Clock, DayGroup, DeepEvent, DeepHit,
    DeepSummary, FacetCount, FacetSelection, Query, Sort, View, When,
};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::Arc;

/// What the background Past tasks send back (wrapped in `SessionsMsg::Past`).
#[derive(Debug)]
pub(crate) enum PastMsg {
    /// Headers loaded (with whatever previews the cache already had).
    Catalog(Result<Arc<Catalog>, String>),
    /// Preview refresh progress (sessions re-read / to re-read).
    PreviewProgress { done: usize, total: usize },
    /// Preview refresh finished: the catalog again, previews applied.
    Previews(Arc<Catalog>),
    Deep { search: u64, ev: DeepEvent },
    DeepDone { search: u64, summary: DeepSummary },
    Renamed { id: String, name: Option<String>, result: Result<(), String> },
}

/// Background work the Past tab asks the dispatch loop to start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PastCmd {
    /// Start deep search number `search` (phrase/targets/cancel are read
    /// from the state when the task is spawned).
    Deep { search: u64 },
    Rename { id: String, name: Option<String> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Focus {
    Sidebar,
    List,
}

/// One sidebar row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FacetItem {
    Header(&'static str),
    When(When, usize),
    Project(FacetCount),
    Model(FacetCount),
    Named(usize),
}

impl FacetItem {
    fn selectable(&self) -> bool {
        !matches!(self, FacetItem::Header(_))
    }
}

/// One line of the (virtual) list: a day header or a session row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ListLine {
    /// Index into [`Derived::groups`].
    Header(usize),
    /// Position in `view.rows`.
    Row(usize),
}

/// Everything derived from (catalog, query, facets, sort), shared by Arc.
#[derive(Debug, Default)]
pub(crate) struct Derived {
    pub(crate) view: View,
    pub(crate) groups: Vec<DayGroup>,
    pub(crate) lines: Vec<ListLine>,
    pub(crate) sidebar: Vec<FacetItem>,
}

/// Deep search state machine:
/// editing (`running == false`, no summary) → Enter → running → done
/// (summary) / cancelled (esc, or an edit while running).
#[derive(Clone, Debug)]
pub(crate) struct DeepState {
    pub(crate) phrase: String,
    /// Bumped per start; late events from an older search are dropped.
    pub(crate) search: u64,
    pub(crate) running: bool,
    pub(crate) done: usize,
    pub(crate) total: usize,
    pub(crate) hits: Arc<Vec<DeepHit>>,
    pub(crate) summary: Option<DeepSummary>,
    pub(crate) cancelled: bool,
    pub(crate) cancel: Arc<AtomicBool>,
    pub(crate) cursor: usize,
    pub(crate) scroll: usize,
}

impl DeepState {
    fn new(phrase: String, search: u64) -> Self {
        Self {
            phrase,
            search,
            running: false,
            done: 0,
            total: 0,
            hits: Arc::new(Vec::new()),
            summary: None,
            cancelled: false,
            cancel: Arc::new(AtomicBool::new(false)),
            cursor: 0,
            scroll: 0,
        }
    }

    pub(crate) fn cancel_running(&mut self) {
        if self.running {
            self.cancel.store(true, Ordering::Relaxed);
            self.running = false;
            self.cancelled = true;
        }
    }
}

/// The rename popup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RenameEdit {
    pub(crate) id: String,
    pub(crate) buffer: String,
    pub(crate) error: Option<String>,
    /// Request in flight.
    pub(crate) pending: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct PastState {
    /// Where to load from / save the preview cache. `None` in the headless
    /// harness and unit tests (no background work; data is fed directly).
    pub(crate) paths: Option<agent_core::session_catalog::CatalogPaths>,
    /// Full catalog as loaded (`None` while loading).
    pub(crate) catalog: Option<Arc<Catalog>>,
    /// Catalog minus ids the daemon lists (those are on the Live tab).
    pub(crate) entries: Arc<Vec<CatalogEntry>>,
    pub(crate) exclude: Arc<HashSet<String>>,
    /// Journal ids in the daemon's pin book (drawn ⚑; `p` unpins).
    pub(crate) pinned: Arc<HashSet<String>>,
    pub(crate) error: Option<String>,
    /// Preview refresh progress (`None` when idle/done).
    pub(crate) previews: Option<(usize, usize)>,
    /// Set on modal close: stops the preview refresh / deep scan early.
    pub(crate) stop: Arc<AtomicBool>,

    pub(crate) query_text: String,
    /// The search line has the keyboard.
    pub(crate) editing: bool,
    pub(crate) sel: FacetSelection,
    pub(crate) show_hidden: bool,
    pub(crate) sort: Sort,
    pub(crate) focus: Focus,
    pub(crate) derived: Arc<Derived>,

    /// Selected row: position in `view.rows`, tracked by id across recomputes.
    pub(crate) cursor: usize,
    pub(crate) selected_id: Option<String>,
    /// First visible list line.
    pub(crate) scroll: usize,
    /// Sidebar cursor (index into `derived.sidebar`).
    pub(crate) facet_cursor: usize,
    /// Visible list height, written by the draw (shared, so input can page
    /// and keep the cursor in view without knowing the terminal size).
    pub(crate) viewport: Arc<AtomicU16>,

    pub(crate) deep: Option<DeepState>,
    pub(crate) deep_seq: u64,
    pub(crate) rename: Option<RenameEdit>,
    /// Pinned clock (tests); `None` = local now at each recompute.
    pub(crate) clock: Option<Clock>,
}

impl Default for PastState {
    fn default() -> Self {
        Self {
            paths: None,
            catalog: None,
            entries: Arc::new(Vec::new()),
            exclude: Arc::new(HashSet::new()),
            pinned: Arc::new(HashSet::new()),
            error: None,
            previews: None,
            stop: Arc::new(AtomicBool::new(false)),
            query_text: String::new(),
            editing: false,
            sel: FacetSelection::default(),
            show_hidden: false,
            sort: Sort::Recent,
            focus: Focus::List,
            derived: Arc::new(Derived::default()),
            cursor: 0,
            selected_id: None,
            scroll: 0,
            facet_cursor: 1,
            viewport: Arc::new(AtomicU16::new(20)),
            deep: None,
            deep_seq: 0,
            rename: None,
            clock: None,
        }
    }
}

impl PastState {
    pub(crate) fn clock(&self) -> Clock {
        self.clock.unwrap_or_else(Clock::local_now)
    }

    /// The count on the tab (`Past 1,102`); `None` while loading.
    pub(crate) fn count(&self) -> Option<usize> {
        self.catalog.as_ref().map(|_| self.entries.len())
    }

    pub(crate) fn loading(&self) -> bool {
        self.catalog.is_none() && self.error.is_none()
    }

    /// Apply a background message. `false` = stale/ignored.
    pub(crate) fn apply(&mut self, msg: PastMsg) -> bool {
        match msg {
            PastMsg::Catalog(Ok(cat)) | PastMsg::Previews(cat) => {
                self.error = None;
                self.set_catalog(cat);
            }
            PastMsg::Catalog(Err(e)) => {
                if self.catalog.is_none() {
                    self.error = Some(e);
                }
            }
            PastMsg::PreviewProgress { done, total } => {
                self.previews = (done < total).then_some((done, total));
            }
            PastMsg::Deep { search, ev } => {
                let Some(d) = self.deep.as_mut().filter(|d| d.search == search && d.running) else {
                    return false;
                };
                match ev {
                    DeepEvent::Hit(h) => Arc::make_mut(&mut d.hits).push(h),
                    DeepEvent::Progress { done, total } => {
                        d.done = done;
                        d.total = total;
                    }
                }
            }
            PastMsg::DeepDone { search, summary } => {
                let Some(d) = self.deep.as_mut().filter(|d| d.search == search) else {
                    return false;
                };
                if d.running {
                    d.running = false;
                    d.done = summary.files_done;
                    d.total = summary.files_total;
                    d.cancelled = summary.cancelled;
                }
                d.summary = Some(summary);
            }
            PastMsg::Renamed { id, name, result } => return self.renamed(id, name, result),
        }
        true
    }

    fn renamed(&mut self, id: String, name: Option<String>, result: Result<(), String>) -> bool {
        match result {
            Ok(()) => {
                if self.rename.as_ref().is_some_and(|r| r.id == id) {
                    self.rename = None;
                }
                if let Some(cat) = self.catalog.as_mut() {
                    let cat = Arc::make_mut(cat);
                    if let Some(e) = cat.entries.iter_mut().find(|e| e.id == id) {
                        e.set_name(name);
                    }
                    let cat = self.catalog.clone().expect("just borrowed");
                    self.set_catalog(cat);
                }
            }
            Err(e) => match self.rename.as_mut().filter(|r| r.id == id) {
                Some(r) => {
                    r.pending = false;
                    r.error = Some(e);
                }
                None => return false,
            },
        }
        true
    }

    pub(crate) fn set_catalog(&mut self, cat: Arc<Catalog>) {
        self.catalog = Some(cat);
        self.rebuild_entries();
    }

    /// Ids (session ids and journal ids) the daemon currently lists.
    pub(crate) fn set_exclude(&mut self, ids: HashSet<String>) {
        if *self.exclude == ids {
            return;
        }
        self.exclude = Arc::new(ids);
        self.rebuild_entries();
    }

    pub(crate) fn set_pinned(&mut self, ids: HashSet<String>) {
        if *self.pinned != ids {
            self.pinned = Arc::new(ids);
        }
    }

    pub(crate) fn is_pinned(&self, id: &str) -> bool {
        self.pinned.contains(id)
    }

    fn rebuild_entries(&mut self) {
        let Some(cat) = &self.catalog else { return };
        let entries: Vec<CatalogEntry> = cat
            .entries
            .iter()
            .filter(|e| !self.exclude.contains(&e.id))
            .cloned()
            .collect();
        self.entries = Arc::new(entries);
        self.recompute();
    }

    /// Re-run the query/facets/sort and rebuild the display lines, keeping
    /// the selection on the same session when it is still listed.
    pub(crate) fn recompute(&mut self) {
        let clock = self.clock();
        let query = if self.deep.is_some() { Query::default() } else { parse_query(&self.query_text, &clock) };
        let view = build_view(&self.entries, &query, &self.sel, self.show_hidden, self.sort, &clock);
        let groups = match self.sort.date_key() {
            Some(k) => day_groups(&self.entries, &view.rows, k, &clock),
            None => Vec::new(),
        };
        let mut lines = Vec::with_capacity(view.rows.len() + groups.len());
        if groups.is_empty() {
            lines.extend((0..view.rows.len()).map(ListLine::Row));
        } else {
            for (gi, g) in groups.iter().enumerate() {
                lines.push(ListLine::Header(gi));
                lines.extend((g.start..g.start + g.len).map(ListLine::Row));
            }
        }
        let sidebar = self.sidebar_items(&view);
        // Keep the cursor on the same session.
        let cursor = self
            .selected_id
            .as_ref()
            .and_then(|id| view.rows.iter().position(|&i| &self.entries[i].id == id))
            .unwrap_or_else(|| self.cursor.min(view.rows.len().saturating_sub(1)));
        self.derived = Arc::new(Derived { view, groups, lines, sidebar });
        self.facet_cursor = self.facet_cursor.min(self.derived.sidebar.len().saturating_sub(1));
        if !self.derived.sidebar.get(self.facet_cursor).is_some_and(FacetItem::selectable) {
            self.move_facet(1);
        }
        self.set_cursor(cursor);
    }

    /// When / Project / Model / Named, with the current selections always
    /// listed (count 0 if the query excludes them) so they can be undone.
    fn sidebar_items(&self, view: &View) -> Vec<FacetItem> {
        let f = &view.facets;
        let mut v = vec![FacetItem::Header("WHEN")];
        v.extend(f.when.iter().map(|(w, n)| FacetItem::When(*w, *n)));
        v.push(FacetItem::Header("PROJECT"));
        let mut projects = f.projects.clone();
        if let Some(k) = &self.sel.project {
            if !projects.iter().any(|p| &p.key == k) {
                projects.push(FacetCount { key: k.clone(), label: project_label(k), count: 0 });
            }
        }
        v.extend(projects.into_iter().map(FacetItem::Project));
        v.push(FacetItem::Header("MODEL"));
        let mut models = f.models.clone();
        if let Some(k) = &self.sel.model {
            if !models.iter().any(|m| &m.key == k) {
                models.push(FacetCount { key: k.clone(), label: agent_core::session_catalog::model_label(k).to_string(), count: 0 });
            }
        }
        v.extend(models.into_iter().map(FacetItem::Model));
        v.push(FacetItem::Header("NAMED"));
        v.push(FacetItem::Named(f.named));
        v
    }

    /// Is this sidebar item part of the current selection?
    pub(crate) fn facet_selected(&self, item: &FacetItem) -> bool {
        match item {
            FacetItem::Header(_) => false,
            FacetItem::When(w, _) => self.sel.when == Some(*w),
            FacetItem::Project(p) => self.sel.project.as_deref() == Some(p.key.as_str()),
            FacetItem::Model(m) => self.sel.model.as_deref() == Some(m.key.as_str()),
            FacetItem::Named(_) => self.sel.named_only,
        }
    }

    /// Toggle the sidebar item under the cursor (one value per group; the
    /// groups combine).
    pub(crate) fn toggle_facet(&mut self) {
        let Some(item) = self.derived.sidebar.get(self.facet_cursor).cloned() else { return };
        let on = !self.facet_selected(&item);
        match item {
            FacetItem::Header(_) => return,
            FacetItem::When(w, _) => self.sel.when = on.then_some(w),
            FacetItem::Project(p) => self.sel.project = on.then_some(p.key),
            FacetItem::Model(m) => self.sel.model = on.then_some(m.key),
            FacetItem::Named(_) => self.sel.named_only = on,
        }
        self.recompute();
    }

    pub(crate) fn clear_facets(&mut self) {
        if self.sel != FacetSelection::default() {
            self.sel = FacetSelection::default();
            self.recompute();
        }
    }

    pub(crate) fn move_facet(&mut self, delta: isize) {
        let items = &self.derived.sidebar;
        if items.is_empty() {
            return;
        }
        let mut i = self.facet_cursor as isize;
        loop {
            let next = i + delta.signum();
            if next < 0 || next >= items.len() as isize {
                break;
            }
            i = next;
            if items[i as usize].selectable() {
                self.facet_cursor = i as usize;
                if delta.abs() <= 1 {
                    break;
                }
            }
        }
    }

    // ── list cursor / scrolling ─────────────────────────────────────────────

    pub(crate) fn rows_len(&self) -> usize {
        match &self.deep {
            Some(d) => d.hits.len(),
            None => self.derived.view.rows.len(),
        }
    }

    pub(crate) fn page(&self) -> usize {
        usize::from(self.viewport.load(Ordering::Relaxed)).max(1)
    }

    /// Selected session entry (list row, or the hit's session in deep mode).
    pub(crate) fn selected_entry(&self) -> Option<&CatalogEntry> {
        match &self.deep {
            Some(d) => {
                let id = &d.hits.get(d.cursor)?.id;
                self.entries.iter().find(|e| &e.id == id)
            }
            None => {
                let i = *self.derived.view.rows.get(self.cursor)?;
                self.entries.get(i)
            }
        }
    }

    pub(crate) fn move_cursor(&mut self, delta: isize) {
        let n = self.rows_len();
        if n == 0 {
            return;
        }
        let cur = match &self.deep {
            Some(d) => d.cursor,
            None => self.cursor,
        } as isize;
        let next = (cur + delta).clamp(0, n as isize - 1) as usize;
        self.set_cursor(next);
    }

    fn set_cursor(&mut self, pos: usize) {
        let page = self.page();
        if let Some(d) = self.deep.as_mut() {
            d.cursor = pos.min(d.hits.len().saturating_sub(1));
            d.scroll = keep_in_view(d.scroll, d.cursor, d.cursor, page, d.hits.len());
            return;
        }
        let rows = &self.derived.view.rows;
        self.cursor = pos.min(rows.len().saturating_sub(1));
        self.selected_id = rows.get(self.cursor).map(|&i| self.entries[i].id.clone());
        let line = self.cursor_line();
        // Keep the day header above the first row of a group in view too.
        let top = match line.checked_sub(1).map(|l| self.derived.lines[l]) {
            Some(ListLine::Header(_)) => line - 1,
            _ => line,
        };
        self.scroll = keep_in_view(self.scroll, top, line, page, self.derived.lines.len());
    }

    /// Display line of the cursor row.
    pub(crate) fn cursor_line(&self) -> usize {
        let pos = self.cursor;
        if self.derived.groups.is_empty() {
            return pos;
        }
        // Rows of group g are preceded by g+1 headers.
        let g = self
            .derived
            .groups
            .iter()
            .position(|g| pos < g.start + g.len)
            .unwrap_or(self.derived.groups.len().saturating_sub(1));
        pos + g + 1
    }

    // ── search / deep ───────────────────────────────────────────────────────

    pub(crate) fn set_query(&mut self, text: String) {
        if let Some(d) = self.deep.as_mut() {
            if d.phrase != text {
                d.cancel_running();
                d.phrase = text;
                d.summary = None;
                d.cancelled = false;
            }
            return;
        }
        if self.query_text != text {
            self.query_text = text;
            self.recompute();
        }
    }

    /// The text the search line edits (query, or the deep phrase).
    pub(crate) fn search_text(&self) -> &str {
        match &self.deep {
            Some(d) => &d.phrase,
            None => &self.query_text,
        }
    }

    /// ctrl+f: enter deep mode (phrase seeded from the query's free words).
    pub(crate) fn enter_deep(&mut self) {
        if self.deep.is_some() {
            return;
        }
        let clock = self.clock();
        let seed = parse_query(&self.query_text, &clock).terms.join(" ");
        self.deep_seq += 1;
        self.deep = Some(DeepState::new(seed, self.deep_seq));
        self.editing = true;
        self.recompute();
    }

    pub(crate) fn leave_deep(&mut self) {
        if let Some(mut d) = self.deep.take() {
            d.cancel_running();
        }
        self.recompute();
    }

    /// Enter in deep mode: (re)start the scan. `None` if nothing to search.
    pub(crate) fn start_deep(&mut self) -> Option<PastCmd> {
        self.deep_seq += 1;
        let seq = self.deep_seq;
        let d = self.deep.as_mut()?;
        d.cancel_running();
        if d.phrase.trim().is_empty() {
            return None;
        }
        let phrase = std::mem::take(&mut d.phrase);
        *d = DeepState::new(phrase, seq);
        d.running = true;
        d.total = self.derived.view.rows.len();
        Some(PastCmd::Deep { search: seq })
    }

    /// The sessions a deep search scans: the current view (facets + noise
    /// filter apply; the free-text query does not — deep mode has its own
    /// phrase), newest first.
    pub(crate) fn deep_targets(&self) -> Vec<usize> {
        self.derived.view.rows.clone()
    }

    /// Stop background work (modal closing).
    pub(crate) fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(d) = self.deep.as_mut() {
            d.cancel_running();
        }
    }

    /// Short header text: `hiding 258 (sandbox, <3 msgs)` / `showing 258 hidden`.
    pub(crate) fn hiding_text(&self) -> String {
        let h = self.derived.view.hidden;
        if h.total == 0 {
            return String::new();
        }
        let mut why = Vec::new();
        if h.sandbox > 0 {
            why.push("sandbox");
        }
        if h.few_messages > 0 {
            why.push("<3 msgs");
        }
        if h.junk_id > 0 {
            why.push("junk ids");
        }
        if self.show_hidden {
            format!("showing {} hidden", thousands(h.total))
        } else {
            format!("hiding {} ({})", thousands(h.total), why.join(", "))
        }
    }
}

/// Scroll offset keeping lines `top..=bottom` inside a `page`-line window.
fn keep_in_view(scroll: usize, top: usize, bottom: usize, page: usize, total: usize) -> usize {
    let mut s = scroll;
    if bottom >= s + page {
        s = bottom + 1 - page;
    }
    if top < s {
        s = top;
    }
    s.min(total.saturating_sub(page))
}

fn project_label(key: &str) -> String {
    if key.is_empty() {
        return "(unknown)".into();
    }
    std::path::Path::new(key)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| key.to_string())
}

/// `1,102`.
pub(crate) fn thousands(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}
