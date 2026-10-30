//! `ClientFrame::ReloadExtension` (`synaps daemon reload-extension <NAME>`):
//! stop one extension and start it again from what is installed now,
//! without `daemon reload` (which checkpoints every session and cancels the
//! turns in flight).
//!
//! Extensions are daemon-wide: one process each, shared by every session
//! through the host's tool registry, hook bus and lease manager. The reload
//! updates those in place ([`ExtensionManager::reload_from_disk`]), so every
//! session gets the new version from its next request; nothing in a session
//! is touched. A call into the old process that is in flight when it stops
//! fails like any extension crash would.
//!
//! [`ExtensionManager::reload_from_disk`]: crate::extensions::manager::ExtensionManager::reload_from_disk

use std::sync::atomic::Ordering;
use std::sync::Arc;

use super::DaemonState;
use crate::session::wire::{DaemonFrame, RefuseReason};

/// Serve `ReloadExtension{extension}`. Always answers with one frame:
/// `ExtensionReloaded`, `ExtensionReloadFailed` or `Refused{Busy}`.
pub async fn reload_extension(state: &Arc<DaemonState>, extension: &str) -> DaemonFrame {
    // A daemon reload shuts every extension down and starts them again
    // itself; a single reload racing it could start a process the exec
    // then orphans.
    if state.reloading.load(Ordering::SeqCst) {
        return DaemonFrame::Refused { reason: RefuseReason::Busy, message: "daemon reloading; retry in a moment".into() };
    }
    // Write lock for the whole stop-and-start: two reloads of one extension
    // cannot interleave, and discovery (which holds it for its walk) has
    // finished before this runs.
    let mut manager = state.host.ext_manager().write().await;
    let was_loaded = manager.is_loaded(extension);
    match manager.reload_from_disk(extension).await {
        Ok(report) => DaemonFrame::ExtensionReloaded { report },
        Err(failure) => DaemonFrame::ExtensionReloadFailed {
            extension: extension.to_string(),
            message: failure.concise_message(),
            unloaded: was_loaded && !manager.is_loaded(extension),
        },
    }
}
