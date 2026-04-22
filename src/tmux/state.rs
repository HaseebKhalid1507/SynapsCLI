//! Tracked state of tmux objects (sessions, windows, panes).

use std::collections::HashMap;

/// Role assigned to a tmux pane — tracks what it's being used for.
#[derive(Debug, Clone, PartialEq)]
pub enum PaneRole {
    /// The main Synaps TUI chat pane.
    SynapsTui,
    /// A shell session created by the agent.
    AgentShell { session_id: String },
    /// A subagent output display pane.
    Subagent { handle_id: String },
    /// A user-created pane (not managed by Synaps).
    User,
}

/// Tracked state of a single tmux pane.
#[derive(Debug, Clone)]
pub struct TmuxPane {
    pub id: String,
    pub window_id: String,
    pub title: String,
    pub width: u32,
    pub height: u32,
    pub active: bool,
    pub role: PaneRole,
}

/// Tracked state of a single tmux window.
#[derive(Debug, Clone)]
pub struct TmuxWindow {
    pub id: String,
    pub name: String,
    pub index: u32,
    pub layout: String,
}

/// Full tracked state of the tmux session.
#[derive(Debug)]
pub struct TmuxState {
    pub session_id: String,
    pub session_name: String,
    pub windows: HashMap<String, TmuxWindow>,
    pub panes: HashMap<String, TmuxPane>,
    pub self_pane: Option<String>,
}

impl TmuxState {
    pub fn new(session_id: &str, session_name: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
            session_name: session_name.to_string(),
            windows: HashMap::new(),
            panes: HashMap::new(),
            self_pane: None,
        }
    }

    pub fn add_pane(&mut self, pane: TmuxPane) {
        self.panes.insert(pane.id.clone(), pane);
    }

    pub fn remove_pane(&mut self, pane_id: &str) {
        self.panes.remove(pane_id);
    }

    pub fn pane(&self, pane_id: &str) -> Option<&TmuxPane> {
        self.panes.get(pane_id)
    }

    pub fn pane_mut(&mut self, pane_id: &str) -> Option<&mut TmuxPane> {
        self.panes.get_mut(pane_id)
    }

    pub fn add_window(&mut self, window: TmuxWindow) {
        self.windows.insert(window.id.clone(), window);
    }

    pub fn remove_window(&mut self, window_id: &str) {
        self.windows.remove(window_id);
    }

    pub fn window(&self, window_id: &str) -> Option<&TmuxWindow> {
        self.windows.get(window_id)
    }

    /// Get all panes matching a role filter.
    pub fn panes_by_role_filter<F>(&self, f: F) -> Vec<&TmuxPane>
    where
        F: Fn(&PaneRole) -> bool,
    {
        self.panes.values().filter(|p| f(&p.role)).collect()
    }

    /// Get count of panes with a specific role.
    pub fn pane_count_by_role<F>(&self, f: F) -> usize
    where
        F: Fn(&PaneRole) -> bool,
    {
        self.panes.values().filter(|p| f(&p.role)).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_state_new() {
        let state = TmuxState::new("$0", "my-session");
        assert_eq!(state.session_id, "$0");
        assert_eq!(state.session_name, "my-session");
        assert!(state.windows.is_empty());
        assert!(state.panes.is_empty());
    }

    #[test]
    fn test_add_and_get_pane() {
        let mut state = TmuxState::new("$0", "test");
        state.add_pane(TmuxPane {
            id: "%0".to_string(),
            window_id: "@0".to_string(),
            title: "main".to_string(),
            width: 200,
            height: 50,
            active: true,
            role: PaneRole::SynapsTui,
        });
        assert_eq!(state.panes.len(), 1);
        assert!(state.pane("%0").is_some());
        assert_eq!(state.pane("%0").unwrap().role, PaneRole::SynapsTui);
    }

    #[test]
    fn test_remove_pane() {
        let mut state = TmuxState::new("$0", "test");
        state.add_pane(TmuxPane {
            id: "%1".to_string(),
            window_id: "@0".to_string(),
            title: "shell".to_string(),
            width: 80,
            height: 24,
            active: false,
            role: PaneRole::AgentShell { session_id: "shell_01".to_string() },
        });
        assert_eq!(state.panes.len(), 1);
        state.remove_pane("%1");
        assert_eq!(state.panes.len(), 0);
    }

    #[test]
    fn test_panes_by_role() {
        let mut state = TmuxState::new("$0", "test");
        state.add_pane(TmuxPane {
            id: "%0".to_string(), window_id: "@0".to_string(),
            title: "tui".to_string(), width: 100, height: 50,
            active: true, role: PaneRole::SynapsTui,
        });
        state.add_pane(TmuxPane {
            id: "%1".to_string(), window_id: "@0".to_string(),
            title: "sh1".to_string(), width: 100, height: 25,
            active: false, role: PaneRole::AgentShell { session_id: "shell_01".to_string() },
        });
        state.add_pane(TmuxPane {
            id: "%2".to_string(), window_id: "@0".to_string(),
            title: "sa1".to_string(), width: 100, height: 25,
            active: false, role: PaneRole::Subagent { handle_id: "sa_1".to_string() },
        });

        let shells: Vec<_> = state.panes_by_role_filter(|r| matches!(r, PaneRole::AgentShell { .. }));
        assert_eq!(shells.len(), 1);
        assert_eq!(shells[0].id, "%1");

        assert_eq!(state.pane_count_by_role(|r| matches!(r, PaneRole::Subagent { .. })), 1);
    }

    #[test]
    fn test_add_and_get_window() {
        let mut state = TmuxState::new("$0", "test");
        state.add_window(TmuxWindow {
            id: "@0".to_string(),
            name: "main".to_string(),
            index: 0,
            layout: "".to_string(),
        });
        assert_eq!(state.windows.len(), 1);
        assert!(state.window("@0").is_some());
        assert_eq!(state.window("@0").unwrap().name, "main");
    }
}
