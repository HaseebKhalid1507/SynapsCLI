//! Extension runtime trait and registry.

pub mod process;

use async_trait::async_trait;
use crate::extensions::hooks::events::{HookEvent, HookResult};

/// Trait for extension runtimes that can handle hook events.
#[async_trait]
pub trait ExtensionHandler: Send + Sync {
    /// Unique identifier for this extension.
    fn id(&self) -> &str;

    /// Handle a hook event. Returns the handler's decision.
    async fn handle(&self, event: &HookEvent) -> HookResult;

    /// Gracefully shut down the extension.
    async fn shutdown(&self);
}
