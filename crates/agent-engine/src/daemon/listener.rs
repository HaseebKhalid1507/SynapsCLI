//! Daemon listener: bind (a UDS with 0600 on Unix, an owner-only named pipe
//! on Windows — see [`crate::ipc`]), accept until shutdown, one task per
//! connection.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::DaemonState;
use crate::ipc::Listener;

/// Bind `path` (refusing to replace a symlink), then chmod 0600.
#[cfg(unix)]
pub fn bind(path: &Path) -> std::io::Result<Listener> {
    crate::events::socket::cleanup_socket(&path.to_string_lossy());
    let listener = tokio::net::UnixListener::bind(path)?;
    {
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::io::AsRawFd;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        // Reload (C3) execs the daemon in place: the listener must NOT
        // survive exec — the new image rebinds the path. tokio sets
        // FD_CLOEXEC; assert it rather than trust it.
        let fd = listener.as_raw_fd();
        // SAFETY: fcntl on our own fd.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags >= 0 && flags & libc::FD_CLOEXEC == 0 {
            // SAFETY: as above.
            unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) };
        }
    }
    Ok(listener.into())
}

/// Create the endpoint's named pipe (owner-only DACL, see [`crate::ipc`]).
/// Nothing is stale on Windows — a pipe dies with its last handle — but
/// after a reload the previous image's instances can outlive its lock for
/// the moment its process takes to be torn down, so a refused first
/// instance is retried briefly.
#[cfg(windows)]
pub fn bind(path: &Path) -> std::io::Result<Listener> {
    use std::time::{Duration, Instant};
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match Listener::bind(path) {
            Err(e)
                if e.raw_os_error() == Some(windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED as i32)
                    && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(100));
            }
            r => return r,
        }
    }
}

pub async fn accept_loop(state: Arc<DaemonState>, mut listener: Listener, shutdown: CancellationToken) {
    loop {
        let accepted = tokio::select! {
            _ = shutdown.cancelled() => break,
            r = listener.accept() => r,
        };
        match accepted {
            Ok(stream) => {
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
