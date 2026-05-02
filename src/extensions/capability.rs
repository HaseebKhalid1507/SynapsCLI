//! Unified capability snapshot for extensions.
//!
//! Aggregates hook subscriptions, extension-provided tools, and registered
//! providers into a single per-extension summary. Future capabilities
//! (memory, indexer, voice) plug in here.

use crate::extensions::providers::RegisteredProviderSummary;
use crate::extensions::runtime::ExtensionHealth;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionCapabilitySnapshot {
    pub id: String,
    pub health: ExtensionHealth,
    pub restart_count: usize,
    pub hooks: Vec<HookCapabilityEntry>,
    pub tools: Vec<ToolCapabilityEntry>,
    pub providers: Vec<RegisteredProviderSummary>,
    /// Future: memory, indexer, voice. Empty for now.
    pub future: Vec<FutureCapabilityEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookCapabilityEntry {
    /// Hook kind name, e.g. "before_tool_call".
    pub kind: String,
    /// Optional tool filter, if the subscription is bound to a specific tool name.
    pub tool_filter: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCapabilityEntry {
    /// Fully namespaced tool name.
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FutureCapabilityEntry {
    pub kind: String, // free-form capability kind declared by the plugin
    pub name: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_literal_round_trips_equality() {
        let snap = ExtensionCapabilitySnapshot {
            id: "demo".to_string(),
            health: ExtensionHealth::Running,
            restart_count: 0,
            hooks: vec![HookCapabilityEntry {
                kind: "before_tool_call".to_string(),
                tool_filter: Some("bash".to_string()),
            }],
            tools: vec![ToolCapabilityEntry {
                name: "demo:hello".to_string(),
            }],
            providers: vec![],
            future: vec![FutureCapabilityEntry {
                kind: "memory".to_string(),
                name: "shortterm".to_string(),
            }],
        };

        let same = snap.clone();
        assert_eq!(snap, same);
        assert_eq!(snap.hooks[0].tool_filter.as_deref(), Some("bash"));
        assert_eq!(snap.tools[0].name, "demo:hello");
        assert_eq!(snap.future[0].kind, "memory");
    }
}
