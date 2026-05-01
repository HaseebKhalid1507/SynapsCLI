//! Extension runtime trait and registry.

pub mod process;
pub mod restart;

pub use restart::RestartPolicy;

use async_trait::async_trait;
use serde_json::Value;
use crate::extensions::hooks::events::{HookEvent, HookResult};
use self::process::{ProviderCompleteParams, ProviderCompleteResult, ProviderStreamEvent};

/// Health state for an extension handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionHealth {
    /// Manifest validated, process spawned, but `initialize` not yet completed.
    Loaded,
    /// Manifest failed validation — the extension never started.
    FailedValidation,
    /// `initialize` rpc failed — the extension started but couldn't capability-handshake.
    FailedInitialize,
    /// Healthy and serving requests.
    Running,
    /// Process restarting after transport failure but within restart budget.
    Restarting,
    /// Running, but at least one recent operation failed (e.g. hook timeout).
    Degraded,
    /// Permanent failure — restart budget exhausted or unrecoverable error.
    Failed,
}

impl ExtensionHealth {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Loaded => "loaded",
            Self::FailedValidation => "failed_validation",
            Self::FailedInitialize => "failed_initialize",
            Self::Running => "running",
            Self::Restarting => "restarting",
            Self::Degraded => "degraded",
            Self::Failed => "failed",
        }
    }
}

#[cfg(test)]
mod health_tests {
    use super::ExtensionHealth;

    #[test]
    fn as_str_covers_all_variants() {
        assert_eq!(ExtensionHealth::Loaded.as_str(), "loaded");
        assert_eq!(ExtensionHealth::FailedValidation.as_str(), "failed_validation");
        assert_eq!(ExtensionHealth::FailedInitialize.as_str(), "failed_initialize");
        assert_eq!(ExtensionHealth::Running.as_str(), "running");
        assert_eq!(ExtensionHealth::Restarting.as_str(), "restarting");
        assert_eq!(ExtensionHealth::Degraded.as_str(), "degraded");
        assert_eq!(ExtensionHealth::Failed.as_str(), "failed");
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

    /// Stream a chat request through an extension-provided model provider.
    ///
    /// The handler must forward `provider.stream.event` notifications to `sink`
    /// in order. The returned `ProviderCompleteResult` is the final aggregated
    /// response (so callers that don't need streaming can use it as the final
    /// state). Implementations that don't support streaming should return
    /// `Err("provider.stream is not supported by this extension")`.
    async fn provider_stream(
        &self,
        _params: ProviderCompleteParams,
        _sink: tokio::sync::mpsc::UnboundedSender<ProviderStreamEvent>,
    ) -> Result<ProviderCompleteResult, String> {
        Err("provider.stream is not supported by this extension".to_string())
    }

    /// Gracefully shut down the extension.
    async fn shutdown(&self);

    /// Current health state of this handler.
    async fn health(&self) -> ExtensionHealth {
        ExtensionHealth::Running
    }

    /// Number of transport restarts observed by this handler.
    async fn restart_count(&self) -> usize {
        0
    }
}
