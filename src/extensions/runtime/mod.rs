//! Extension runtime trait and registry.

pub mod process;

use async_trait::async_trait;
use serde_json::Value;
use crate::extensions::hooks::events::{HookEvent, HookResult};
use self::process::{ProviderCompleteParams, ProviderCompleteResult};

/// Health state for an extension handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionHealth {
    Healthy,
    Restarting,
    Failed,
}

impl ExtensionHealth {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Restarting => "restarting",
            Self::Failed => "failed",
        }
    }
}

/// Trait for extension runtimes that can handle hook events.
#[async_trait]
pub trait ExtensionHandler: Send + Sync {
    /// Unique identifier for this extension.
    fn id(&self) -> &str;

    /// Handle a hook event. Returns the handler's decision.
    async fn handle(&self, event: &HookEvent) -> HookResult;

    /// Call an extension-provided tool.
    async fn call_tool(&self, _name: &str, _input: Value) -> Result<Value, String> {
        Err("extension runtime does not support tool.call".to_string())
    }

    /// Complete a chat request through an extension-provided model provider.
    async fn provider_complete(&self, _params: ProviderCompleteParams) -> Result<ProviderCompleteResult, String> {
        Err("extension runtime does not support provider.complete".to_string())
    }

    /// Gracefully shut down the extension.
    async fn shutdown(&self);

    /// Current health state of this handler.
    async fn health(&self) -> ExtensionHealth {
        ExtensionHealth::Healthy
    }

    /// Number of transport restarts observed by this handler.
    async fn restart_count(&self) -> usize {
        0
    }
}
