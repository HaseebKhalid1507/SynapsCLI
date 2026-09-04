//! UDS listener: bind with 0600, accept until shutdown, one task per
//! connection.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use tokio::net::UnixListener;
use tokio_util::sync::CancellationToken;

use super::DaemonState;

/// Bind `path` (refusing to replace a symlink), then chmod 0600.
pub fn bind(path: &Path) -> std::io::Result<UnixListener> {
    crate::events::socket::cleanup_socket(&path.to_string_lossy());
    let listener = UnixListener::bind(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(listener)
}

pub async fn accept_loop(state: Arc<DaemonState>, listener: UnixListener, shutdown: CancellationToken) {
    loop {
        let accepted = tokio::select! {
            _ = shutdown.cancelled() => break,
            r = listener.accept() => r,
        };
        match accepted {
            Ok((stream, _)) => {
                let state = Arc::clone(&state);
                let shutdown = shutdown.clone();
                state.connections.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    super::conn::serve(Arc::clone(&state), stream, shutdown).await;
                    state.connections.fetch_sub(1, Ordering::SeqCst);
                });
            }
            Err(e) => {
                tracing::warn!("daemon: accept error: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
    }
}
