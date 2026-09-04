//! Daemon shutdown: end every session concurrently inside ONE wall budget,
//! then shut the shared extension host down.

use std::sync::Arc;
use std::time::Duration;

use super::DaemonState;
use crate::session::{EndReason, SessionCommand};

/// `session::budgets::TEARDOWN_TIMEOUT_SECS` (SAVE + HOOKS); shared across
/// all sessions, not ×N.
pub const SESSION_END_BUDGET: Duration = Duration::from_secs(crate::session::budgets::TEARDOWN_TIMEOUT_SECS);
/// `--force`: give the actors this long, then drop them.
pub const FORCE_BUDGET: Duration = Duration::from_millis(500);

pub async fn shutdown_all(state: &Arc<DaemonState>, force: bool) {
    let sessions = state.live_sessions();
    let n = sessions.len();
    tracing::info!(sessions = n, force, "daemon: ending sessions");
    let budget = if force { FORCE_BUDGET } else { SESSION_END_BUDGET };
    let ends = sessions.iter().map(|h| async move {
        let _ = h.send(SessionCommand::End { reason: EndReason::HostShutdown }).await;
        h.closed().await;
    });
    if tokio::time::timeout(budget, futures::future::join_all(ends)).await.is_err() {
        tracing::warn!(budget = ?budget, "daemon: session shutdown budget exceeded; continuing");
    }
    state.sessions.lock().unwrap_or_else(|e| e.into_inner()).clear();

    let ext = Arc::clone(state.host.ext_manager());
    let ext_shutdown = async move { ext.write().await.shutdown_all().await };
    if tokio::time::timeout(Duration::from_secs(5), ext_shutdown).await.is_err() {
        tracing::warn!("daemon: extension shutdown budget exceeded");
    }
    state.host.flush_logs();
}
