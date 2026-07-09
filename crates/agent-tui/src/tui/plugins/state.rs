// Task 14/15 will use these variants/fields; keep them declared now for API stability.
#![allow(dead_code)]

use std::path::PathBuf;

use synaps_cli::skills::state::{CachedPlugin, InstalledPlugin, PluginsState};

use super::progress::InstallProgressHandle;
use crate::tui::focus::FocusRing;

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum LeftRow {
    Installed,
    Marketplace(String),
    AddMarketplace,
}

pub enum RightRow<'a> {
    Installed(&'a InstalledPlugin),
    Browseable {
        plugin: &'a CachedPlugin,
        installed: bool,
    },
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Focus {
    Left,
    Right,
}

#[derive(Debug, Clone)]
pub enum RightMode {
    List,
    Detail {
        row_idx: usize,
    },
    AddMarketplaceEditor {
        buffer: String,
        error: Option<String>,
    },
    TrustPrompt {
        plugin_name: String,
        host: String,
        pending_source: String,
        summary: Vec<String>,
    },
    Confirm {
        prompt: String,
        on_yes: ConfirmAction,
        summary: Vec<String>,
    },
    /// Background `git clone --progress` is running. The handle points at
    /// the same `InstallProgress` the worker thread is updating.
    Installing {
        progress: InstallProgressHandle,
    },
    PendingInstallConfirm {
        plugin_name: String,
        source_url: String,
        subdir: Option<String>,
        marketplace_name: Option<String>,
        summary: Vec<String>,
        installed_commit: String,
        checksum_algorithm: Option<String>,
        checksum_value: Option<String>,
        temp_dir: std::path::PathBuf,
        final_dir: std::path::PathBuf,
    },
    PendingUpdateConfirm {
        plugin_name: String,
        summary: Vec<String>,
        installed_commit: String,
        temp_dir: std::path::PathBuf,
        final_dir: std::path::PathBuf,
    },
}

#[derive(Debug, Clone)]
pub enum ConfirmAction {
    Uninstall(String), // plugin name
    EnablePlugin(String),
    RemoveMarketplace(String),
}

/// Bookkeeping for a background `git clone` started by [`crate::tui::plugins::actions::run_install_flow`].
/// The main loop's tick branch polls [`PluginsModalState::install_task_finished`]
/// and, when it returns true, calls
/// [`crate::tui::plugins::actions::complete_pending_install_clone`] which
/// reaps `join`, drains the result, and finishes the install pipeline
/// (executable-extension confirm prompt, finalize, post-install setup).
pub struct PendingInstallTask {
    pub join: tokio::task::JoinHandle<Result<(String, PathBuf), String>>,
    pub progress: InstallProgressHandle,
    pub plugin_name: String,
    pub source_url: String,
    pub subdir: Option<String>,
    pub marketplace_name: Option<String>,
    pub expected_checksum: Option<(String, String)>,
    pub final_dir: PathBuf,
}

impl std::fmt::Debug for PendingInstallTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingInstallTask")
            .field("plugin_name", &self.plugin_name)
            .field("source_url", &self.source_url)
            .field("subdir", &self.subdir)
            .field("marketplace_name", &self.marketplace_name)
            .field("final_dir", &self.final_dir)
            .field("join_finished", &self.join.is_finished())
            .finish()
    }
}

pub struct PluginsModalState {
    pub file: PluginsState,
    pub selected_left: usize,
    pub selected_right: usize,
    /// Left/Right focus as read by draw code (kept public and unchanged so
    /// `plugins/draw.rs` reads it directly). P7.6: this is now a synced
    /// projection of `focus_ring` — the authoritative traversal store below.
    pub focus: Focus,
    /// Authoritative Left/Right traversal store: a two-slot [`FocusRing`] from
    /// the FocusManager (slot 0 = Left, slot 1 = Right). Tab / focus moves go
    /// through this ring (P7.6), replacing the old direct `focus` assignments;
    /// `focus` above is re-derived from it after every move. Focus survives
    /// occlusion because it lives in the retained `PluginsModalState` (§4).
    focus_ring: FocusRing,
    pub mode: RightMode,
    pub row_error: Option<String>,
    /// Background install task (spawned by `run_install_flow`); `Some` while
    /// a `git clone --progress` is in flight. The main loop's animation tick
    /// polls this to know when to render the spinner and when to reap the
    /// task and finish the install.
    pub pending_install: Option<PendingInstallTask>,
}

impl Clone for PluginsModalState {
    /// Clone for snapshotting into `RenderModel`. `pending_install` (which
    /// holds a `JoinHandle`) is deliberately dropped — the snapshot is
    /// render-only and the main loop retains the authoritative state.
    fn clone(&self) -> Self {
        Self {
            file: self.file.clone(),
            selected_left: self.selected_left,
            selected_right: self.selected_right,
            focus: self.focus.clone(),
            focus_ring: self.focus_ring.clone(),
            mode: self.mode.clone(),
            row_error: self.row_error.clone(),
            pending_install: None,
        }
    }
}

impl PluginsModalState {
    pub fn new(file: PluginsState) -> Self {
        Self {
            file,
            selected_left: 0,
            selected_right: 0,
            focus: Focus::Left,
            focus_ring: FocusRing::of_len(2),
            mode: RightMode::List,
            row_error: None,
            pending_install: None,
        }
    }

    /// True while a background `git clone --progress` is in flight. Used by
    /// the main loop to (a) keep the animation tick firing so the spinner
    /// updates and (b) gate the polling that reaps the JoinHandle.
    pub fn is_install_active(&self) -> bool {
        self.pending_install.is_some()
    }

    /// True if the background clone task has finished and is ready to be
    /// awaited / reaped. Cheap — just queries the JoinHandle.
    ///
    /// This intentionally does *not* gate on the min-display timer; use
    /// [`install_ready_to_reap`](Self::install_ready_to_reap) for that.
    pub fn install_task_finished(&self) -> bool {
        self.pending_install
            .as_ref()
            .map(|p| p.join.is_finished())
            .unwrap_or(false)
    }

    /// Minimum time the "Installing…" overlay stays on screen after the
    /// clone completes, so a fast (cached) clone doesn't visibly flash by.
    /// Tunable via the `SYNAPS_INSTALL_MIN_DISPLAY_MS` env var (testing).
    const MIN_INSTALL_DISPLAY_MS: u64 = 500;

    fn min_install_display_ms() -> u64 {
        std::env::var("SYNAPS_INSTALL_MIN_DISPLAY_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(Self::MIN_INSTALL_DISPLAY_MS)
    }

    /// True iff the background clone is finished AND the overlay has been
    /// visible for at least `MIN_INSTALL_DISPLAY_MS`. Used by the main
    /// loop tick to decide when to actually reap the JoinHandle and
    /// transition out of the Installing overlay.
    pub fn install_ready_to_reap(&self) -> bool {
        let Some(p) = self.pending_install.as_ref() else {
            return false;
        };
        if !p.join.is_finished() {
            return false;
        }
        let Ok(prog) = p.progress.lock() else {
            return true;
        };
        prog.started_at.elapsed().as_millis() as u64 >= Self::min_install_display_ms()
    }

    /// Advance the spinner frame on the in-flight install (no-op if none).
    /// Called from the UI tick at ~60fps.
    pub fn tick_install_spinner(&mut self) {
        if let Some(p) = &self.pending_install {
            if let Ok(mut prog) = p.progress.lock() {
                prog.tick_spinner();
            }
        }
    }

    /// Build a PluginsModalState from persisted file state, pre-positioning the
    /// cursor based on whether any marketplaces exist. Mirrors the landing
    /// behavior wanted when the nested overlay is opened from /settings.
    ///
    /// - With marketplaces: land on the first marketplace row (index 1) and
    ///   focus the right pane so the user immediately sees its cached plugins.
    /// - Without marketplaces: rows are `[Installed, AddMarketplace]`, so land
    ///   on the `AddMarketplace` row (index 1) with default (Left) focus so
    ///   Enter opens the add-marketplace editor.
    pub(crate) fn new_from_settings(file: PluginsState) -> Self {
        let has_marketplaces = !file.marketplaces.is_empty();
        let mut st = Self::new(file);
        st.selected_left = 1;
        if has_marketplaces {
            st.set_focus(Focus::Right);
        }
        st
    }

    /// Current Left/Right focus, read from the two-slot [`FocusRing`] backing
    /// store (slot 0 = Left, slot 1 = Right). Equals the synced `focus` field.
    pub fn focus(&self) -> Focus {
        Self::focus_from_ring(&self.focus_ring)
    }

    fn focus_from_ring(ring: &FocusRing) -> Focus {
        match ring.current().map(|slot| slot.id()) {
            Some(1) => Focus::Right,
            _ => Focus::Left,
        }
    }

    /// Re-derive the public `focus` projection from the authoritative ring.
    fn sync_focus_field(&mut self) {
        self.focus = Self::focus_from_ring(&self.focus_ring);
    }

    /// Tab: toggle Left <-> Right. On the two-slot ring this is `next()`
    /// (wraps), preserving today's exact toggle semantics.
    pub fn toggle_focus(&mut self) {
        self.focus_ring.next();
        self.sync_focus_field();
    }

    /// Set focus to a specific side. On the two-slot ring, one `next()` flips to
    /// the other slot, so we step only when currently on the wrong side.
    pub fn set_focus(&mut self, target: Focus) {
        if self.focus() != target {
            self.focus_ring.next();
        }
        self.sync_focus_field();
    }

    pub fn left_rows(&self) -> Vec<LeftRow> {
        let mut rows = vec![LeftRow::Installed];
        for m in &self.file.marketplaces {
            rows.push(LeftRow::Marketplace(m.name.clone()));
        }
        rows.push(LeftRow::AddMarketplace);
        rows
    }

    pub fn right_rows(&self) -> Vec<RightRow<'_>> {
        let left = self.left_rows();
        match left.get(self.selected_left) {
            Some(LeftRow::Installed) => self
                .file
                .installed
                .iter()
                .map(RightRow::Installed)
                .collect(),
            Some(LeftRow::Marketplace(mname)) => {
                let Some(m) = self.file.marketplaces.iter().find(|m| &m.name == mname) else {
                    return Vec::new();
                };
                m.cached_plugins
                    .iter()
                    .map(|p| RightRow::Browseable {
                        plugin: p,
                        installed: self.file.installed.iter().any(|i| i.name == p.name),
                    })
                    .collect()
            }
            _ => Vec::new(),
        }
    }

    pub fn move_left_down(&mut self) {
        let n = self.left_rows().len();
        if self.selected_left + 1 < n {
            self.selected_left += 1;
            self.selected_right = 0;
        }
    }
    pub fn move_left_up(&mut self) {
        if self.selected_left > 0 {
            self.selected_left -= 1;
            self.selected_right = 0;
        }
    }
    pub fn move_right_down(&mut self) {
        let n = self.right_rows().len();
        if self.selected_right + 1 < n {
            self.selected_right += 1;
        }
    }
    pub fn move_right_up(&mut self) {
        if self.selected_right > 0 {
            self.selected_right -= 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use synaps_cli::skills::state::{CachedPlugin, InstalledPlugin, Marketplace, PluginsState};

    fn mk_state() -> PluginsModalState {
        let file = PluginsState {
            marketplaces: vec![Marketplace {
                name: "pi".into(),
                url: "https://github.com/m/pi".into(),
                description: None,
                last_refreshed: None,
                cached_plugins: vec![CachedPlugin {
                    name: "web".into(),
                    source: "https://github.com/m/web.git".into(),
                    version: None,
                    description: None,
                    index: None,
                }],
                repo_url: None,
            }],
            installed: vec![InstalledPlugin {
                name: "tools".into(),
                marketplace: None,
                source_url: "https://github.com/x/tools.git".into(),
                installed_commit: "aaa".into(),
                latest_commit: Some("aaa".into()),
                installed_at: "now".into(),
                source_subdir: None,
                checksum_algorithm: None,
                checksum_value: None,
                setup_status: Default::default(),
            }],
            trusted_hosts: vec![],
        };
        PluginsModalState::new(file)
    }

    #[test]
    fn left_rows_include_installed_and_marketplaces_and_add() {
        let s = mk_state();
        let rows = s.left_rows();
        assert_eq!(rows[0], LeftRow::Installed);
        assert_eq!(rows[1], LeftRow::Marketplace("pi".into()));
        assert_eq!(rows.last().unwrap(), &LeftRow::AddMarketplace);
    }

    #[test]
    fn right_rows_when_installed_selected() {
        let mut s = mk_state();
        s.selected_left = 0; // Installed
        let rows = s.right_rows();
        assert_eq!(rows.len(), 1);
        assert!(matches!(&rows[0], RightRow::Installed(p) if p.name == "tools"));
    }

    #[test]
    fn right_rows_when_marketplace_selected() {
        let mut s = mk_state();
        s.selected_left = 1; // pi
        let rows = s.right_rows();
        assert_eq!(rows.len(), 1);
        assert!(
            matches!(&rows[0], RightRow::Browseable { plugin, installed: false } if plugin.name == "web")
        );
    }

    #[test]
    fn right_rows_when_add_marketplace_selected() {
        let mut s = mk_state();
        s.selected_left = s.left_rows().len() - 1;
        let rows = s.right_rows();
        assert!(rows.is_empty());
    }

    #[test]
    fn move_down_clamps_in_left_pane() {
        let mut s = mk_state();
        let last = s.left_rows().len() - 1;
        for _ in 0..100 {
            s.move_left_down();
        }
        assert_eq!(s.selected_left, last);
    }

    #[test]
    fn new_from_settings_with_marketplaces_focuses_right_and_selects_first() {
        let file = PluginsState {
            marketplaces: vec![Marketplace {
                name: "pi".into(),
                url: "https://github.com/m/pi".into(),
                description: None,
                last_refreshed: None,
                cached_plugins: vec![],
                repo_url: None,
            }],
            installed: vec![],
            trusted_hosts: vec![],
        };
        let st = PluginsModalState::new_from_settings(file);
        assert_eq!(st.selected_left, 1);
        assert!(matches!(st.focus, Focus::Right));
    }

    #[test]
    fn new_from_settings_without_marketplaces_stays_on_add_row() {
        let file = PluginsState::default();
        let st = PluginsModalState::new_from_settings(file);
        // Rows are [Installed, AddMarketplace]; index 1 is the AddMarketplace row.
        assert_eq!(st.selected_left, 1);
    }

    /// Regression: a fast (cached) clone shouldn't make the "Installing…"
    /// overlay flash by — `install_ready_to_reap` must hold the overlay
    /// open until at least `MIN_INSTALL_DISPLAY_MS` has elapsed even if
    /// the JoinHandle is already done.
    #[tokio::test(flavor = "current_thread")]
    async fn install_ready_to_reap_holds_until_min_display_elapsed() {
        // Override to 200ms so the test stays fast but still meaningful.
        std::env::set_var("SYNAPS_INSTALL_MIN_DISPLAY_MS", "200");
        let mut s = mk_state();
        let progress = std::sync::Arc::new(std::sync::Mutex::new(
            crate::tui::plugins::progress::InstallProgress::new("test"),
        ));
        // Spawn a task that finishes immediately.
        let join =
            tokio::task::spawn_blocking(|| -> Result<(String, std::path::PathBuf), String> {
                Ok(("sha".into(), std::path::PathBuf::from("/tmp/x")))
            });
        s.pending_install = Some(PendingInstallTask {
            join,
            progress: progress.clone(),
            plugin_name: "p".into(),
            source_url: "u".into(),
            subdir: None,
            marketplace_name: None,
            expected_checksum: None,
            final_dir: std::path::PathBuf::from("/tmp/p"),
        });
        // Wait for the join to finish but well before 200ms.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(s.install_task_finished(), "join should be done by now");
        assert!(
            !s.install_ready_to_reap(),
            "must not reap before MIN_INSTALL_DISPLAY_MS elapses"
        );
        // Wait past the threshold.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(
            s.install_ready_to_reap(),
            "should reap once min-display window has passed"
        );
        std::env::remove_var("SYNAPS_INSTALL_MIN_DISPLAY_MS");
    }
}
