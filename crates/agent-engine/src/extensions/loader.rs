//! Async extension loading orchestration.
//!
//! The chat UI owns the manager behind an async lock; this module keeps startup
//! snappy by running discovery/loading in the background and streaming progress
//! events back to the UI (which can render them as toasts).

use std::sync::Arc;

use tokio::sync::{mpsc, RwLock};

use super::manager::{ExtensionLoadFailure, ExtensionManager};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtensionLoaderEvent {
    Started,
    Loaded { plugin: String, loaded: usize, failed: usize },
    Failed { failure: ExtensionLoadFailure, loaded: usize, failed: usize },
    Finished { loaded: Vec<String>, failed: Vec<ExtensionLoadFailure> },
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

pub fn spawn_discover_and_load(
    manager: Arc<RwLock<ExtensionManager>>,
    tx: mpsc::UnboundedSender<ExtensionLoaderEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let _ = tx.send(ExtensionLoaderEvent::Started);
        let (loaded, failed) = manager.write().await.discover_and_load_with_progress(|event| {
            let _ = tx.send(event);
        }).await;
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
