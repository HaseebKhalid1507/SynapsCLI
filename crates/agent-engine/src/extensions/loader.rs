//! Async extension loading orchestration.
//!
//! The chat UI owns the manager behind an async lock; this module keeps startup
//! snappy by running discovery/loading in the background and streaming progress
//! events back to the UI (which can render them as toasts).

use std::sync::Arc;

use tokio::sync::{mpsc, RwLock};

use super::hooks::events::{HookEvent, HookResult};
use super::manager::{ExtensionLoadFailure, ExtensionManager};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtensionLoaderEvent {
    Started,
    Loaded {
        plugin: String,
        loaded: usize,
        failed: usize,
    },
    Failed {
        failure: ExtensionLoadFailure,
        loaded: usize,
        failed: usize,
    },
    Finished {
        loaded: Vec<String>,
        failed: Vec<ExtensionLoadFailure>,
    },
}

impl ExtensionLoaderEvent {
    pub fn progress_counts(&self) -> Option<(usize, usize)> {
        match self {
            ExtensionLoaderEvent::Started => Some((0, 0)),
            ExtensionLoaderEvent::Loaded { loaded, failed, .. } => Some((*loaded, *failed)),
            ExtensionLoaderEvent::Failed { loaded, failed, .. } => Some((*loaded, *failed)),
            ExtensionLoaderEvent::Finished { loaded, failed } => Some((loaded.len(), failed.len())),
        }
    }
}

/// Discover and load extensions in the background, then fire `on_session_start`.
///
/// The hook is emitted HERE, not at engine boot, and that placement is the
/// whole point. `engine::setup::boot()` used to emit it while constructing an
/// empty `ExtensionManager` — before any host had called this function — so
/// the event reached zero subscribers in every host (TUI, server, rpc). No
/// extension had ever received `on_session_start`. A comment in
/// `cmd/server.rs` recorded the symptom ("no subscribers until extensions
/// loaded") without it being recognised as a defect.
///
/// `session_id` is the session the hook reports. `None` skips the emit, for
/// callers that have no session (or extensions disabled).
pub fn spawn_discover_and_load(
    manager: Arc<RwLock<ExtensionManager>>,
    tx: mpsc::UnboundedSender<ExtensionLoaderEvent>,
    session_id: Option<String>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let _ = tx.send(ExtensionLoaderEvent::Started);
        let (loaded, failed) = manager
            .write()
            .await
            .discover_and_load_with_progress(|event| {
                let _ = tx.send(event);
            })
            .await;

        // Extensions are now subscribed; the event has somewhere to land.
        if let Some(session_id) = session_id {
            let hook_bus = manager.read().await.hook_bus().clone();
            let event = HookEvent::on_session_start(&session_id);
            if let HookResult::Inject { content } = hook_bus.emit(&event).await {
                tracing::debug!(
                    len = content.len(),
                    "on_session_start injected session-scoped context"
                );
                hook_bus.set_session_injection(content).await;
            }
        }

        let _ = tx.send(ExtensionLoaderEvent::Finished { loaded, failed });
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_counts_report_finished_totals() {
        let event = ExtensionLoaderEvent::Finished {
            loaded: vec!["a".into(), "b".into()],
            failed: vec![ExtensionLoadFailure {
                plugin: "bad".into(),
                manifest_path: None,
                reason: "oops".into(),
                hint: "fix it".into(),
            }],
        };
        assert_eq!(event.progress_counts(), Some((2, 1)));
    }
}
