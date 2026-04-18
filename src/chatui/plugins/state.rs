// Task 14/15 will use these variants/fields; keep them declared now for API stability.
#![allow(dead_code)]

use synaps_cli::skills::state::{PluginsState, InstalledPlugin, CachedPlugin};

#[derive(Debug, PartialEq, Eq)]
pub enum LeftRow {
    Installed,
    Marketplace(String),
    AddMarketplace,
}

pub enum RightRow<'a> {
    Installed(&'a InstalledPlugin),
    Browseable { plugin: &'a CachedPlugin, installed: bool },
}

pub enum Focus { Left, Right }

pub enum RightMode {
    List,
    Detail { row_idx: usize },
    AddMarketplaceEditor { buffer: String, error: Option<String> },
    TrustPrompt { plugin_name: String, host: String, pending_source: String },
    Confirm { prompt: String, on_yes: ConfirmAction },
}

pub enum ConfirmAction {
    Uninstall(String),       // plugin name
    RemoveMarketplace(String),
}

pub struct PluginsModalState {
    pub file: PluginsState,
    pub selected_left: usize,
    pub selected_right: usize,
    pub focus: Focus,
    pub mode: RightMode,
    pub row_error: Option<String>,
}

impl PluginsModalState {
    pub fn new(file: PluginsState) -> Self {
        Self {
            file,
            selected_left: 0,
            selected_right: 0,
            focus: Focus::Left,
            mode: RightMode::List,
            row_error: None,
        }
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
            Some(LeftRow::Installed) => self.file.installed.iter()
                .map(RightRow::Installed).collect(),
            Some(LeftRow::Marketplace(mname)) => {
                let Some(m) = self.file.marketplaces.iter().find(|m| &m.name == mname) else {
                    return Vec::new();
                };
                m.cached_plugins.iter()
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
        if self.selected_left + 1 < n { self.selected_left += 1; self.selected_right = 0; }
    }
    pub fn move_left_up(&mut self) {
        if self.selected_left > 0 { self.selected_left -= 1; self.selected_right = 0; }
    }
    pub fn move_right_down(&mut self) {
        let n = self.right_rows().len();
        if self.selected_right + 1 < n { self.selected_right += 1; }
    }
    pub fn move_right_up(&mut self) {
        if self.selected_right > 0 { self.selected_right -= 1; }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use synaps_cli::skills::state::{PluginsState, Marketplace, CachedPlugin, InstalledPlugin};

    fn mk_state() -> PluginsModalState {
        let file = PluginsState {
            marketplaces: vec![
                Marketplace {
                    name: "pi".into(),
                    url: "https://github.com/m/pi".into(),
                    description: None,
                    last_refreshed: None,
                    cached_plugins: vec![
                        CachedPlugin {
                            name: "web".into(),
                            source: "https://github.com/m/web.git".into(),
                            version: None,
                            description: None,
                        },
                    ],
                },
            ],
            installed: vec![
                InstalledPlugin {
                    name: "tools".into(),
                    marketplace: None,
                    source_url: "https://github.com/x/tools.git".into(),
                    installed_commit: "aaa".into(),
                    latest_commit: Some("aaa".into()),
                    installed_at: "now".into(),
                },
            ],
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
        assert!(matches!(&rows[0], RightRow::Browseable { plugin, installed: false } if plugin.name == "web"));
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
        for _ in 0..100 { s.move_left_down(); }
        assert_eq!(s.selected_left, last);
    }
}
