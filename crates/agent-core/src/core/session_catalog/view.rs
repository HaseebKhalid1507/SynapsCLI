//! Noise hiding, facets with counts, sorting and day grouping.

use super::query::Query;
use super::time::{Clock, When};
use super::{model_key, model_label, tilde, CatalogEntry};
use chrono::{DateTime, NaiveDate, Utc};
use std::collections::HashMap;
use std::path::Path;

/// Sessions with fewer messages than this are noise (when the count is known).
pub const MIN_MESSAGES: usize = 3;

/// Why an entry is hidden by default. Several can apply at once.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NoiseReasons {
    /// cwd under a temp dir (`/tmp`, `/var/tmp`, `$TMPDIR`, `*-sandbox`).
    pub sandbox: bool,
    /// Known message count < [`MIN_MESSAGES`].
    pub few_messages: bool,
    /// Not a real session id (stem/id mismatch or not `YYYYMMDD-HHMMSS-x`).
    pub junk_id: bool,
}

impl NoiseReasons {
    pub fn any(self) -> bool {
        self.sandbox || self.few_messages || self.junk_id
    }
}

/// Noise classification for one entry.
pub fn noise_reasons(e: &CatalogEntry) -> NoiseReasons {
    NoiseReasons {
        sandbox: e.cwd.as_deref().is_some_and(is_sandbox_cwd),
        few_messages: e.message_count.is_some_and(|n| n < MIN_MESSAGES),
        junk_id: e.junk_id,
    }
}

fn is_sandbox_cwd(cwd: &Path) -> bool {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    is_sandbox_cwd_with(cwd, home.as_deref(), &std::env::temp_dir())
}

/// [`is_sandbox_cwd`] with `$HOME` and the temp dir passed in. The user's
/// own home is never scratch space, even when it lives under a temp dir
/// (CI seeds `HOME` in `/tmp`); a home that IS a temp root (or `/`) earns
/// no exemption.
fn is_sandbox_cwd_with(cwd: &Path, home: Option<&Path>, tmp: &Path) -> bool {
    if cwd.to_string_lossy().contains("synaps-web-sandbox") {
        return true;
    }
    let temp_roots = [Path::new("/tmp"), Path::new("/var/tmp"), Path::new("/private/tmp"), tmp];
    let real_home = home.filter(|h| h.components().count() > 2 && !temp_roots.contains(h));
    if real_home.is_some_and(|h| cwd.starts_with(h)) {
        return false;
    }
    temp_roots.iter().any(|r| r.as_os_str().len() > 1 && cwd.starts_with(r))
}

/// Per-rule counts of hidden entries ("hiding N (sandbox, <3 msgs)").
/// `total` counts each entry once even when several rules match.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NoiseCounts {
    pub sandbox: usize,
    pub few_messages: usize,
    pub junk_id: usize,
    pub total: usize,
}

/// Sort orders. All descending (biggest / newest first).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Sort {
    /// `updated_at` (default).
    #[default]
    Recent,
    Cost,
    /// Message count (unknown counts last).
    Length,
    Created,
}

impl Sort {
    pub const ALL: [Sort; 4] = [Sort::Recent, Sort::Cost, Sort::Length, Sort::Created];

    pub fn label(self) -> &'static str {
        match self {
            Sort::Recent => "recent",
            Sort::Cost => "cost",
            Sort::Length => "length",
            Sort::Created => "created",
        }
    }

    pub fn next(self) -> Sort {
        match self {
            Sort::Recent => Sort::Cost,
            Sort::Cost => Sort::Length,
            Sort::Length => Sort::Created,
            Sort::Created => Sort::Recent,
        }
    }

    /// Which date the day headers follow, or `None` when the order is not
    /// chronological (cost / length: render a flat list).
    pub fn date_key(self) -> Option<DateKey> {
        match self {
            Sort::Recent => Some(DateKey::Updated),
            Sort::Created => Some(DateKey::Created),
            Sort::Cost | Sort::Length => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateKey {
    Updated,
    Created,
}

impl DateKey {
    fn of(self, e: &CatalogEntry) -> DateTime<Utc> {
        match self {
            DateKey::Updated => e.updated_at,
            DateKey::Created => e.created_at,
        }
    }
}

/// Active facet choices from the sidebar (all `None`/false = no filter).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FacetSelection {
    pub when: Option<When>,
    /// [`FacetCount::key`] of a project (`""` = unknown cwd).
    pub project: Option<String>,
    /// [`FacetCount::key`] of a model (provider-stripped key).
    pub model: Option<String>,
    pub named_only: bool,
}

/// One sidebar row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FacetCount {
    /// Stable selection key (full cwd path / model key).
    pub key: String,
    /// Display label (`~/`, `agent-runtime`, `opus-4-6`).
    pub label: String,
    pub count: usize,
}

/// Facet counts. Each group is counted with the query, noise filter and
/// the OTHER groups' selections applied — never its own — so picking a
/// project still shows how many sessions each other project has.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Facets {
    /// Always all four buckets, in [`When::ALL`] order.
    pub when: Vec<(When, usize)>,
    /// Count-descending, then label.
    pub projects: Vec<FacetCount>,
    pub models: Vec<FacetCount>,
    pub named: usize,
}

/// Rows to render plus the sidebar/header numbers.
#[derive(Debug, Clone, Default)]
pub struct View {
    /// Indices into `Catalog::entries`, in display order.
    pub rows: Vec<usize>,
    pub facets: Facets,
    /// Entries matching query + facets that the noise filter hides
    /// (counted even when `show_hidden` is on, so the header can say
    /// "showing N hidden").
    pub hidden: NoiseCounts,
}

/// Filter, facet and sort. O(n) over the catalog plus the sort; cheap enough
/// to rerun on every keystroke for thousands of entries.
pub fn build_view(
    entries: &[CatalogEntry],
    query: &Query,
    sel: &FacetSelection,
    show_hidden: bool,
    sort: Sort,
    clock: &Clock,
) -> View {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let mut view = View::default();
    let mut when_counts: HashMap<When, usize> = HashMap::new();
    let mut projects: HashMap<String, (String, usize)> = HashMap::new();
    let mut models: HashMap<String, (String, usize)> = HashMap::new();

    for (i, e) in entries.iter().enumerate() {
        if !query.matches(e) {
            continue;
        }
        let noise = noise_reasons(e);
        let pass = (
            sel.when.is_none_or_eq(e_when(e, clock)),
            sel.project
                .as_deref()
                .is_none_or_eq(project_key(e).as_str()),
            sel.model
                .as_deref()
                .is_none_or_eq(model_key(&e.model).to_lowercase().as_str()),
            !sel.named_only || e.name.is_some(),
        );
        if pass.0 && pass.1 && pass.2 && pass.3 && noise.any() {
            let h = &mut view.hidden;
            h.sandbox += usize::from(noise.sandbox);
            h.few_messages += usize::from(noise.few_messages);
            h.junk_id += usize::from(noise.junk_id);
            h.total += 1;
        }
        if noise.any() && !show_hidden {
            continue;
        }
        if pass.1 && pass.2 && pass.3 {
            *when_counts.entry(e_when(e, clock)).or_default() += 1;
        }
        if pass.0 && pass.2 && pass.3 {
            let key = project_key(e);
            let slot = projects
                .entry(key)
                .or_insert_with(|| (project_label(e, home.as_deref()), 0));
            slot.1 += 1;
        }
        if pass.0 && pass.1 && pass.3 {
            let key = model_key(&e.model).to_lowercase();
            let slot = models
                .entry(key)
                .or_insert_with(|| (model_label(&e.model).to_string(), 0));
            slot.1 += 1;
        }
        if pass.0 && pass.1 && pass.2 && e.name.is_some() {
            view.facets.named += 1;
        }
        if pass.0 && pass.1 && pass.2 && pass.3 {
            view.rows.push(i);
        }
    }

    view.facets.when = When::ALL
        .iter()
        .map(|w| (*w, when_counts.get(w).copied().unwrap_or(0)))
        .collect();
    view.facets.projects = facet_list(projects);
    view.facets.models = facet_list(models);
    sort_rows(entries, &mut view.rows, sort);
    view
}

trait NoneOrEq<T> {
    fn is_none_or_eq(&self, v: T) -> bool;
}
impl<T: PartialEq + Copy> NoneOrEq<T> for Option<T> {
    fn is_none_or_eq(&self, v: T) -> bool {
        self.is_none_or(|s| s == v)
    }
}

fn e_when(e: &CatalogEntry, clock: &Clock) -> When {
    clock.when(e.updated_at)
}

/// Project key: the full cwd (`""` when the index has none).
fn project_key(e: &CatalogEntry) -> String {
    e.cwd
        .as_deref()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// `~/` for home, the last path component otherwise, `(unknown)` without
/// a cwd. Distinct cwds may share a label; the key stays unique.
fn project_label(e: &CatalogEntry, home: Option<&Path>) -> String {
    let Some(cwd) = e.cwd.as_deref() else {
        return "(unknown)".into();
    };
    if home.is_some_and(|h| h == cwd) {
        return "~/".into();
    }
    match cwd.file_name() {
        Some(n) => n.to_string_lossy().into_owned(),
        None => tilde(cwd),
    }
}

fn facet_list(map: HashMap<String, (String, usize)>) -> Vec<FacetCount> {
    let mut v: Vec<FacetCount> = map
        .into_iter()
        .map(|(key, (label, count))| FacetCount { key, label, count })
        .collect();
    v.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| a.label.cmp(&b.label))
            .then_with(|| a.key.cmp(&b.key))
    });
    v
}

fn sort_rows(entries: &[CatalogEntry], rows: &mut [usize], sort: Sort) {
    rows.sort_by(|&a, &b| {
        let (x, y) = (&entries[a], &entries[b]);
        let primary = match sort {
            Sort::Recent => y.updated_at.cmp(&x.updated_at),
            Sort::Created => y.created_at.cmp(&x.created_at),
            Sort::Cost => y.cost.total_cmp(&x.cost),
            Sort::Length => y.message_count.cmp(&x.message_count),
        };
        primary
            .then_with(|| y.updated_at.cmp(&x.updated_at))
            .then_with(|| y.id.cmp(&x.id))
    });
}

/// A run of consecutive rows sharing a local day.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DayGroup {
    pub date: NaiveDate,
    /// `Today` / `Yesterday` / `Oct 21` / `Oct 21 2025`.
    pub label: String,
    /// Range into the `rows` slice that was grouped.
    pub start: usize,
    pub len: usize,
}

/// Split already-sorted `rows` into day groups by `key` (use
/// [`Sort::date_key`]). Rows sorted by something else just yield more,
/// smaller groups — callers should skip headers when `date_key` is `None`.
pub fn day_groups(
    entries: &[CatalogEntry],
    rows: &[usize],
    key: DateKey,
    clock: &Clock,
) -> Vec<DayGroup> {
    let mut out: Vec<DayGroup> = Vec::new();
    for (pos, &i) in rows.iter().enumerate() {
        let date = clock.date_of(key.of(&entries[i])).min(clock.today());
        match out.last_mut() {
            Some(g) if g.date == date => g.len += 1,
            _ => out.push(DayGroup {
                date,
                label: clock.day_label(date),
                start: pos,
                len: 1,
            }),
        }
    }
    out
}

#[cfg(test)]
mod sandbox_tests {
    use super::is_sandbox_cwd_with;
    use std::path::Path;

    #[test]
    fn a_home_under_a_temp_dir_is_not_sandbox_but_the_rest_of_tmp_is() {
        let tmp = Path::new("/tmp");
        let home = Some(Path::new("/tmp/synaps-dirty-home.ab12"));
        assert!(!is_sandbox_cwd_with(Path::new("/tmp/synaps-dirty-home.ab12"), home, tmp));
        assert!(!is_sandbox_cwd_with(Path::new("/tmp/synaps-dirty-home.ab12/Projects/x"), home, tmp));
        assert!(is_sandbox_cwd_with(Path::new("/tmp/other"), home, tmp));
        assert!(is_sandbox_cwd_with(Path::new("/tmp/synaps-dirty-home.ab12/synaps-web-sandbox/a"), home, tmp));
    }

    #[test]
    fn a_home_that_is_a_temp_root_or_slash_earns_no_exemption() {
        let tmp = Path::new("/tmp");
        for h in ["/tmp", "/var/tmp", "/"] {
            assert!(is_sandbox_cwd_with(Path::new("/tmp/x"), Some(Path::new(h)), tmp), "{h}");
        }
        assert!(is_sandbox_cwd_with(Path::new("/var/tmp/x"), None, tmp));
        assert!(!is_sandbox_cwd_with(Path::new("/home/u/Projects/x"), Some(Path::new("/home/u")), tmp));
        // A custom $TMPDIR counts as a temp root too.
        assert!(is_sandbox_cwd_with(Path::new("/scratch/t/x"), Some(Path::new("/home/u")), Path::new("/scratch/t")));
    }
}
