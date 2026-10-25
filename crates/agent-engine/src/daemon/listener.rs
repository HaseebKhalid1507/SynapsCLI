//! UDS listener: bind with 0600, accept until shutdown, one task per
//! connection. Every connection must come from the daemon's own user.

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
                // SAFETY: geteuid has no preconditions and cannot fail.
                if let Err(why) = admit(&stream, unsafe { libc::geteuid() }) {
                    tracing::warn!("daemon: refused connection: {why}");
                    continue; // dropped unread: the peer sees EOF, no frame
                }
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

/// Every client attached to a session is the daemon's own user (spec
/// `multi-client-sessions.md` §3). The socket's 0600 mode already says
/// so; this keeps it true if the mode is loosened, `SYNAPS_BASE_DIR`
/// points somewhere shared, or a future listener forgets to chmod. Root
/// is refused too (`sudo synaps --attach` would also resolve another
/// home). Credentials that can't be read refuse the connection.
fn admit(stream: &tokio::net::UnixStream, own_uid: u32) -> Result<(), String> {
    match stream.peer_cred() {
        Ok(cred) if cred.uid() == own_uid => Ok(()),
        Ok(cred) => Err(format!("peer uid {} is not the daemon's uid {own_uid}", cred.uid())),
        Err(e) => Err(format!("peer credentials unreadable: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn own_uid() -> u32 {
        // SAFETY: as in `accept_loop`.
        unsafe { libc::geteuid() }
    }

    #[tokio::test]
    async fn a_connection_from_the_daemons_own_user_is_admitted() {
        let (a, _b) = tokio::net::UnixStream::pair().unwrap();
        assert_eq!(admit(&a, own_uid()), Ok(()));
    }

    /// Tests can't switch uid, so the daemon's uid is what varies: a real
    /// peer (this process) seen by a daemon running as anyone else.
    #[tokio::test]
    async fn a_connection_from_another_user_is_refused() {
        let (a, _b) = tokio::net::UnixStream::pair().unwrap();
        let other = own_uid().wrapping_add(1);
        let err = admit(&a, other).unwrap_err();
        assert!(err.contains("is not the daemon's uid"), "{err}");
    }
}
