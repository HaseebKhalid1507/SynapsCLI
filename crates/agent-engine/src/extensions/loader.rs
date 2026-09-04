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

/// Session-level: fire `on_session_start` for one session and store any
/// `inject` as that session's keyed injection (`stream.rs` reads it when
/// composing the system prompt). Call AFTER discovery is known-finished —
/// in-process hosts get that from [`spawn_discover_and_load`] itself; the
/// daemon's `SessionActor` awaits `EngineHost::extensions_ready()` first.
/// Never re-runs discovery. Returns `true` when something was injected.
pub async fn emit_session_start(hook_bus: &Arc<super::hooks::HookBus>, session_id: &str) -> bool {
    let event = HookEvent::on_session_start(session_id);
    if let HookResult::Inject { content } = hook_bus.emit(&event).await {
        tracing::debug!(
            len = content.len(),
            "on_session_start injected session-scoped context"
        );
        hook_bus
            .set_session_injection_for(session_id, content)
            .await;
        return true;
    }
    false
}

/// Session-level: fire `on_session_end` for one session — concurrent
/// dispatch (the hook only allows `continue`, so ordering cannot matter),
/// fail-open under `budget`. Clears the session's keyed injection
/// afterwards. Returns `false` if the budget elapsed (handlers may not have
/// flushed; the transcript is already saved by the caller, so nothing is
/// lost). Per session, not per process: every session in a daemon gets its
/// own end event.
pub async fn emit_session_end(
    hook_bus: &Arc<super::hooks::HookBus>,
    session_id: &str,
    transcript: Option<Vec<crate::SharedMessage>>,
    budget: std::time::Duration,
) -> bool {
    let event = HookEvent::on_session_end(session_id, transcript);
    let completed = match tokio::time::timeout(budget, hook_bus.emit_concurrent(&event)).await {
        Ok(_) => {
            tracing::debug!(session = %session_id, "on_session_end hooks completed");
            true
        }
        Err(_elapsed) => {
            tracing::warn!(
                session = %session_id,
                budget_secs = budget.as_secs(),
                "on_session_end hooks timed out — extensions may not have flushed"
            );
            false
        }
    };
    hook_bus.clear_session_injection(session_id).await;
    completed
}

/// Discover and load extensions in the background, then fire `on_session_start`.
///
/// Process-level and IDEMPOTENT (daemon-mode C2): the manager records its
/// discovery result, so a second call on the same manager (second host in
/// one process, or a second session in a daemon) replays `Finished` from
/// that record and spawns nothing. `on_session_start` still fires for
/// `session_id` on every call — that part is per session.
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
    // Host seam (C2): when this is the process host's manager, tell the host
    // a walk is in flight BEFORE the task runs, and flip `extensions_ready`
    // at Finished so `SessionActor::create` fires `on_session_start` only
    // once extensions are subscribed.
    // The guard marks ready on ANY task exit (incl. panic) so
    // `extensions_ready()` waiters are never stranded.
    let ready_guard = crate::host::EngineHost::current()
        .filter(|host| Arc::ptr_eq(host.ext_manager(), &manager))
        .map(|host| host.extensions_loading_guard());
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
            emit_session_start(&hook_bus, &session_id).await;
        }

        // Ready BEFORE Finished, as before.
        drop(ready_guard);
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
