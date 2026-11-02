//! Daemon listener: bind (a UDS with 0600 on Unix, an owner-only named pipe
//! on Windows — see [`crate::ipc`]), accept until shutdown, one task per
//! connection. Every connection must come from the daemon's own user.

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
                // On Windows the pipe's owner-only DACL admits only the
                // daemon's user (see `crate::ipc`).
                #[cfg(unix)]
                // SAFETY: geteuid has no preconditions and cannot fail.
                if let Err(why) = admit(&stream, unsafe { libc::geteuid() }) {
                    tracing::warn!("daemon: refused connection: {why}");
                    continue; // dropped unread: the peer sees EOF, no frame
                }
                let state = Arc::clone(&state);
                let shutdown = shutdown.clone();
                state.connections.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    // Decrement on drop: a panicking `serve` (panic =
                    // "unwind") ends only this task, and a count left high
                    // would keep idle-exit off and the reload announce
                    // waiting for a client that is gone.
                    let counted = Arc::clone(&state);
                    let _count = OnDrop(move || {
                        counted.connections.fetch_sub(1, Ordering::SeqCst);
                    });
                    super::conn::serve(Arc::clone(&state), stream, shutdown).await;
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
#[cfg(unix)]
fn admit(stream: &tokio::net::UnixStream, own_uid: u32) -> Result<(), String> {
    match stream.peer_cred() {
        Ok(cred) if cred.uid() == own_uid => Ok(()),
        Ok(cred) => Err(format!("peer uid {} is not the daemon's uid {own_uid}", cred.uid())),
        Err(e) => Err(format!("peer credentials unreadable: {e}")),
    }
}

/// Runs its closure when dropped: on return and on unwind alike.
struct OnDrop<F: FnMut()>(F);

impl<F: FnMut()> Drop for OnDrop<F> {
    fn drop(&mut self) {
        (self.0)()
    }
}

#[cfg(all(test, unix))]
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

#[cfg(test)]
mod on_drop_tests {
    use super::OnDrop;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[tokio::test]
    async fn a_panicking_connection_task_still_gives_its_count_back() {
        let connections = Arc::new(AtomicUsize::new(0));
        connections.fetch_add(1, Ordering::SeqCst);
        let counted = Arc::clone(&connections);
        let task = tokio::spawn(async move {
            let counted = Arc::clone(&counted);
            let _count = OnDrop(move || {
                counted.fetch_sub(1, Ordering::SeqCst);
            });
            tokio::task::yield_now().await;
            panic!("serve panicked");
        });
        assert!(task.await.expect_err("the task panics").is_panic());
        assert_eq!(connections.load(Ordering::SeqCst), 0);
    }
}
