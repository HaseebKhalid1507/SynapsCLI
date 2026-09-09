//! Daemon-mode phase 2 (C3): extension notification router.
//!
//! One subscriber per loaded extension (`ProcessExtension::subscribe_notifications`),
//! started ONCE by the host after discovery; forwards `widget.*` frames
//! ([`super::widgets::is_widget_method`]) to every live session as
//! `SessionEventWire::ExtensionNotification` via
//! [`crate::host::EngineHost::broadcast_extension_notification`].
//!
//! Routing (phase 3, C2): a frame whose `params.session_id` is a string is
//! delivered to THAT session only (dropped with a `debug!` if it is not
//! live); a frame without one is daemon-global and goes to every session
//! (the pre-phase-3 behaviour — `docs/extensions/session-id.md` §plugin
//! contract). Delivery is non-blocking: a session whose
//! command queue is full drops the frame with a `warn!` — widget upserts are
//! idempotent last-writer-wins UI state (`loop_arms.rs` semantics), and
//! blocking would backpressure the extension's lossless fan-out and stall
//! `command.invoke` / `provider.stream` subscribers on the same queue.
//!
//! The inline TUI keeps its own watcher (`loop_arms.rs`) today; this router
//! serves actor-hosted sessions (daemon, `synaps chat` on the actor).

use std::sync::Arc;
use std::time::Duration;

use crate::host::EngineHost;

/// Resubscribe delay after an extension's notification channel closes
/// (EOF / restart) — mirrors the TUI watcher.
const RESUBSCRIBE_DELAY: Duration = Duration::from_millis(500);

/// Start one forwarding task per loaded extension. Idempotent per call
/// site by construction — the host calls it once after
/// `extensions_ready()`; call it again only after a genuine re-discovery.
/// The returned handle completes once every per-extension task has been
/// spawned (the tasks themselves run for the host's lifetime and end when
/// their extension is gone and stays gone).
pub fn spawn_notification_router(host: Arc<EngineHost>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let handlers = host.ext_manager().read().await.handlers();
        for (ext_id, handler) in handlers {
            let host = Arc::clone(&host);
            tokio::spawn(async move {
                forward_extension_notifications(host, ext_id, handler).await;
            });
        }
    })
}

/// Forward loop for ONE extension. Exits when the extension has been
/// unloaded from the manager (its channel closed and it is no longer
/// listed); otherwise resubscribes after [`RESUBSCRIBE_DELAY`].
pub async fn forward_extension_notifications(
    host: Arc<EngineHost>,
    ext_id: String,
    handler: Arc<dyn super::runtime::ExtensionHandler>,
) {
    loop {
        let (_sub_id, mut rx) = handler.subscribe_notifications().await;
        while let Some(frame) = rx.recv().await {
            if !super::widgets::is_widget_method(&frame.method) {
                continue;
            }
            if super::widgets::parse_widget_event(&frame.method, &frame.params).is_err() {
                tracing::debug!(extension = %ext_id, method = %frame.method,
                    "ignoring malformed widget frame");
                continue;
            }
            let delivered = match route_target(&frame.params) {
                Some(sid) => {
                    let id = crate::session::SessionId(sid.to_string());
                    match host.attach(&id) {
                        Some(handle) => {
                            let cmd = crate::session::SessionCommand::HostEvent(
                                crate::session::HostEvent::ExtensionNotification {
                                    extension_id: ext_id.clone(),
                                    method: frame.method.clone(),
                                    params: frame.params,
                                },
                            );
                            match handle.send(cmd).await {
                                Ok(()) => 1,
                                Err(err) => {
                                    tracing::warn!(session = %id, extension = %ext_id,
                                        method = %frame.method, error = %err,
                                        "dropping extension notification for session");
                                    0
                                }
                            }
                        }
                        None => {
                            tracing::debug!(session = %id, extension = %ext_id,
                                method = %frame.method,
                                "extension notification for a session that is not live; dropped");
                            0
                        }
                    }
                }
                None => {
                    host.broadcast_extension_notification(&ext_id, &frame.method, frame.params)
                        .await
                }
            };
            tracing::trace!(extension = %ext_id, method = %frame.method, sessions = delivered,
                "extension notification routed");
        }
        // Channel closed (EOF/restart). Stop if the extension is gone for
        // good; otherwise wait and resubscribe.
        tokio::time::sleep(RESUBSCRIBE_DELAY).await;
        let still_loaded = host
            .ext_manager()
            .read()
            .await
            .handlers()
            .iter()
            .any(|(id, h)| id == &ext_id && Arc::ptr_eq(h, &handler));
        if !still_loaded {
            tracing::debug!(extension = %ext_id, "notification router: extension unloaded; stopping");
            return;
        }
    }
}

/// `params.session_id` when it is a non-empty string — the frame is
/// session-scoped; otherwise daemon-global (broadcast).
pub fn route_target(params: &serde_json::Value) -> Option<&str> {
    params
        .get("session_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::route_target;

    #[test]
    fn route_target_is_session_id_string_or_global() {
        assert_eq!(
            route_target(&serde_json::json!({"id": "w", "session_id": "s-1"})),
            Some("s-1")
        );
        assert_eq!(route_target(&serde_json::json!({"id": "w"})), None);
        assert_eq!(route_target(&serde_json::json!({"session_id": ""})), None);
        assert_eq!(route_target(&serde_json::json!({"session_id": 7})), None);
        assert_eq!(route_target(&serde_json::json!(null)), None);
    }
}
