//! Extension runtime trait and registry.

pub mod process;

use async_trait::async_trait;
use crate::extensions::hooks::events::{HookEvent, HookResult};

/// Health state for an extension handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionHealth {
    Healthy,
    Restarting,
    Failed,
}

/// Trait for extension runtimes that can handle hook events.
#[async_trait]
pub trait ExtensionHandler: Send + Sync {
    /// Unique identifier for this extension.
    fn id(&self) -> &str;

    /// Handle a hook event. Returns the handler's decision.
    async fn handle(&self, event: &HookEvent) -> HookResult;

    /// Gracefully shut down the extension.
    async fn shutdown(&self);

    /// Current health state of this handler.
    async fn health(&self) -> ExtensionHealth {
        ExtensionHealth::Healthy
    }
}
