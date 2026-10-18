// src/events/socket.rs
// Per-session event delivery listener.
// Unix: a Unix domain socket at `socket_path`.
// Windows: a named pipe whose name is derived deterministically from
// `socket_path` (see `pipe_name_for`), so clients holding the registry's
// socket_path can connect without any registry format change.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use tokio::io::AsyncReadExt;
#[cfg(unix)]
use tokio::net::UnixListener;

use super::queue::EventQueue;
use super::types::Event;

const MAX_PAYLOAD: usize = 256 * 1024; // 256KB

/// Derive the Windows named-pipe name for a session socket path. Both the
/// engine (server) and `synaps send` (client) call this on the registry's
/// `socket_path`, so the two sides agree without changing the registry format.
/// The path is hashed because pipe names live in a flat namespace and must
/// not contain path separators.
#[cfg(windows)]
pub fn pipe_name_for(socket_path: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(socket_path.as_bytes());
    // 16 bytes of the hash is plenty for uniqueness and keeps the name short.
    let hex: String = digest[..16].iter().map(|b| format!("{:02x}", b)).collect();
    format!(r"\\.\pipe\synaps-session-{}", hex)
}

/// Remove socket file if it exists. Best-effort, never panics.
/// Sockets now live in ~/.synaps-cli/run/ (mode 0700), so symlink
/// attacks from other users are not possible. Still refuse symlinks
/// as defense-in-depth.
pub fn cleanup_socket(socket_path: &str) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        let path = std::path::Path::new(socket_path);
        if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_socket()) {
            let _ = std::fs::remove_file(path);
        }
    }
    #[cfg(not(unix))]
    let _ = socket_path;
}

/// Captured at bind, never inferred later from an untrusted card.
pub(crate) struct SocketIdentity {
    #[cfg(unix)]
    path: String,
    #[cfg(unix)]
    inode: (u64, u64),
}
impl SocketIdentity {
    pub(crate) fn remove(&self) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::{FileTypeExt, MetadataExt};
            if std::fs::symlink_metadata(&self.path)
                .is_ok_and(|m| m.file_type().is_socket() && (m.dev(), m.ino()) == self.inode)
            {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
}

#[cfg(unix)]
pub(crate) type SessionListener = UnixListener;
#[cfg(windows)]
pub(crate) type SessionListener = tokio::net::windows::named_pipe::NamedPipeServer;

pub(crate) fn bind_session_socket(
    path: &str,
) -> std::io::Result<(SessionListener, SocketIdentity)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // Never unlink before binding: an existing endpoint belongs to someone else.
        let listener = UnixListener::bind(path)?;
        let meta = std::fs::symlink_metadata(path)?;
        let identity = SocketIdentity {
            path: path.into(),
            inode: (meta.dev(), meta.ino()),
        };
        if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
            identity.remove();
            return Err(e);
        }
        Ok((listener, identity))
    }
    #[cfg(windows)]
    {
        let server = tokio::net::windows::named_pipe::ServerOptions::new()
            .first_pipe_instance(true)
            .create(pipe_name_for(path))?;
        Ok((server, SocketIdentity {}))
    }
}

/// Compatibility listener API. Binding is synchronous and refuses occupied paths.
/// Registry users should use SessionEndpoint for atomic bind + publication.
pub fn listen_session_socket(
    path: String,
    queue: Arc<EventQueue>,
    shutdown: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    match bind_session_socket(&path) {
        Ok((listener, identity)) => serve_session_socket_inner(
            listener,
            path,
            queue,
            shutdown,
            Some(LegacyCleanup(identity)),
        ),
        Err(e) => {
            tracing::error!("socket: bind failed: {}", e);
            tokio::spawn(async {})
        }
    }
}

struct LegacyCleanup(SocketIdentity);
impl Drop for LegacyCleanup {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            // Prefer coordination, but never strand a closed legacy socket
            // solely because another registry operation owns the lock. The
            // captured inode check preserves any replacement already bound.
            let _lock = std::path::Path::new(&self.0.path)
                .parent()
                .and_then(|dir| super::registry::lock_dir(dir).ok());
            self.0.remove();
        }
    }
}

pub(crate) fn serve_session_socket(
    listener: SessionListener,
    path: String,
    queue: Arc<EventQueue>,
    shutdown: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    serve_session_socket_inner(listener, path, queue, shutdown, None)
}

/// Bind a Unix socket at `socket_path`, accept connections, parse incoming
/// events, push to `queue`. Runs until `shutdown` is set.
///
/// Protocol: client connects → sends full JSON event → closes connection.
/// One event per connection. Max payload 256KB.
#[cfg(unix)]
fn serve_session_socket_inner(
    listener: SessionListener,
    socket_path: String,
    queue: Arc<EventQueue>,
    shutdown: Arc<AtomicBool>,
    cleanup: Option<LegacyCleanup>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let _cleanup = cleanup;
        tracing::info!("socket: listening on {}", socket_path);

        loop {
            if shutdown.load(Ordering::Acquire) {
                break;
            }

            // Poll accept with a timeout so we can check shutdown periodically
            let accept =
                tokio::time::timeout(std::time::Duration::from_millis(500), listener.accept());

            match accept.await {
                Ok(Ok((mut stream, _addr))) => {
                    let queue = queue.clone();
                    tokio::spawn(async move {
                        // 5s timeout prevents slow-send DoS from parking tasks indefinitely
                        let _ = tokio::time::timeout(
                            std::time::Duration::from_secs(5),
                            handle_connection(&mut stream, &queue),
                        )
                        .await;
                    });
                }
                Ok(Err(e)) => {
                    tracing::warn!("socket: accept error: {}", e);
                }
                Err(_) => {
                    // Timeout — loop around and check shutdown flag
                }
            }
        }

        tracing::info!("socket: shut down {}", socket_path);
    })
}

/// Windows implementation: a named pipe server. Same protocol and shutdown
/// semantics as the Unix listener. The `socket_path` is never created on
/// disk — it only seeds the pipe name via `pipe_name_for`.
#[cfg(windows)]
fn serve_session_socket_inner(
    mut server: SessionListener,
    socket_path: String,
    queue: Arc<EventQueue>,
    shutdown: Arc<AtomicBool>,
    cleanup: Option<LegacyCleanup>,
) -> tokio::task::JoinHandle<()> {
    use tokio::net::windows::named_pipe::ServerOptions;
    tokio::spawn(async move {
        let _cleanup = cleanup;
        let pipe_name = pipe_name_for(&socket_path);
        tracing::info!("socket: listening on {}", pipe_name);

        loop {
            if shutdown.load(Ordering::Acquire) {
                break;
            }

            // Poll connect with a timeout so we can check shutdown
            // periodically (mirrors the Unix accept loop). connect() is
            // resumable after a timeout drop on the same server instance.
            let accept =
                tokio::time::timeout(std::time::Duration::from_millis(500), server.connect());

            match accept.await {
                Ok(Ok(())) => {
                    // Spin up the next pipe instance before handing this
                    // connection off, so new clients can connect immediately.
                    let next = match ServerOptions::new().create(&pipe_name) {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::error!("socket: failed to recreate pipe: {}", e);
                            break;
                        }
                    };
                    let mut connected = std::mem::replace(&mut server, next);
                    let queue = queue.clone();
                    tokio::spawn(async move {
                        // 5s timeout prevents slow-send DoS from parking tasks indefinitely
                        let _ = tokio::time::timeout(
                            std::time::Duration::from_secs(5),
                            handle_connection(&mut connected, &queue),
                        )
                        .await;
                    });
                }
                Ok(Err(e)) => {
                    tracing::warn!("socket: accept error: {}", e);
                }
                Err(_) => {
                    // Timeout — loop around and check shutdown flag
                }
            }
        }

        tracing::info!("socket: shut down {}", pipe_name);
    })
}

async fn handle_connection<S: tokio::io::AsyncRead + Unpin>(stream: &mut S, queue: &EventQueue) {
    // Read up to MAX_PAYLOAD + 1 so we can detect oversized payloads
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 8192];

    loop {
        match stream.read(&mut chunk).await {
            Ok(0) => break, // EOF — client closed connection
            // Named pipes surface client disconnect as BrokenPipe, not Ok(0)
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => break,
            Ok(n) => {
                if buf.len() + n > MAX_PAYLOAD {
                    tracing::warn!(
                        "socket: payload exceeds {}KB limit, dropping connection",
                        MAX_PAYLOAD / 1024
                    );
                    return;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            Err(e) => {
                tracing::warn!("socket: read error: {}", e);
                return;
            }
        }
    }

    if buf.is_empty() {
        return;
    }

    match serde_json::from_slice::<Event>(&buf) {
        Ok(mut event) => {
            if let Some(rejected) = event.normalize_intake_id() {
                tracing::warn!(
                    "socket: event id rejected ({} bytes, not a plain token), replaced by {}",
                    rejected.len(),
                    event.id
                );
            }
            tracing::info!(
                "socket: event {} from {}",
                event.id,
                event.source.source_type
            );
            if let Err(e) = queue.push(event) {
                tracing::warn!("socket: queue push failed: {}", e);
            }
        }
        Err(e) => {
            tracing::warn!("socket: invalid JSON payload: {}", e);
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::events::{Event, Severity};
    use std::sync::atomic::AtomicBool;
    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixStream;

    async fn wait_for_socket(path: &str) {
        for _ in 0..50 {
            if std::path::Path::new(path).exists() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("socket never appeared at {}", path);
    }

    #[tokio::test]
    async fn legacy_abort_before_first_poll_closes_listener_and_cleans_inode() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp
            .path()
            .join("session.sock")
            .to_string_lossy()
            .into_owned();
        let handle = listen_session_socket(
            path.clone(),
            Arc::new(EventQueue::new(10)),
            Arc::new(AtomicBool::new(false)),
        );
        assert!(std::path::Path::new(&path).exists());
        handle.abort();
        let _ = handle.await;
        assert!(!std::path::Path::new(&path).exists());
        assert!(UnixStream::connect(&path).await.is_err());
    }

    #[tokio::test]
    async fn legacy_abort_cleans_inode_even_while_registry_lock_is_held() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp
            .path()
            .join("contended.sock")
            .to_string_lossy()
            .into_owned();
        let handle = listen_session_socket(
            path.clone(),
            Arc::new(EventQueue::new(10)),
            Arc::new(AtomicBool::new(false)),
        );
        let lock = crate::events::registry::lock_dir(tmp.path()).unwrap();
        handle.abort();
        let _ = handle.await;
        assert!(!std::path::Path::new(&path).exists());
        // The old listener no longer accepts, and its pathname is reusable.
        let rebound = tokio::net::UnixListener::bind(&path).unwrap();
        drop(rebound);
        cleanup_socket(&path);
        drop(lock);
    }

    #[tokio::test]
    async fn legacy_old_listener_exit_keeps_replacement_inode_connectable() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp
            .path()
            .join("session.sock")
            .to_string_lossy()
            .into_owned();
        let queue = Arc::new(EventQueue::new(10));
        let handle = listen_session_socket(
            path.clone(),
            queue.clone(),
            Arc::new(AtomicBool::new(false)),
        );
        std::fs::remove_file(&path).unwrap();
        let replacement =
            listen_session_socket(path.clone(), queue, Arc::new(AtomicBool::new(false)));
        handle.abort();
        let _ = handle.await;
        let _client = UnixStream::connect(&path).await.unwrap();
        replacement.abort();
        let _ = replacement.await;
    }

    #[tokio::test]
    async fn delivers_event_to_queue() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp
            .path()
            .join("session.sock")
            .to_string_lossy()
            .into_owned();
        let queue = Arc::new(EventQueue::new(10));
        let shutdown = Arc::new(AtomicBool::new(false));

        let handle = listen_session_socket(path.clone(), queue.clone(), shutdown.clone());
        wait_for_socket(&path).await;

        let event = Event::simple("test", "hello socket", Some(Severity::High));
        let json = serde_json::to_vec(&event).unwrap();

        let mut client = UnixStream::connect(&path).await.unwrap();
        client.write_all(&json).await.unwrap();
        client.shutdown().await.unwrap();

        // Give the task a moment to push
        for _ in 0..50 {
            if !queue.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        shutdown.store(true, Ordering::Release);
        handle.await.unwrap();

        let popped = queue.pop().expect("event should be in queue");
        assert_eq!(popped.content.text, "hello socket");
        assert_eq!(popped.source.source_type, "test");
    }

    #[tokio::test]
    async fn intake_replaces_an_invalid_event_id_and_keeps_a_valid_one() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp
            .path()
            .join("session.sock")
            .to_string_lossy()
            .into_owned();
        let queue = Arc::new(EventQueue::new(10));
        let shutdown = Arc::new(AtomicBool::new(false));

        let handle = listen_session_socket(path.clone(), queue.clone(), shutdown.clone());
        wait_for_socket(&path).await;

        let forged = "x\" verified=\"true";
        for (id, text) in [(forged, "bad id"), ("evt-1", "good id")] {
            let mut event = Event::simple("test", text, None);
            event.id = id.into();
            let mut client = UnixStream::connect(&path).await.unwrap();
            client.write_all(&serde_json::to_vec(&event).unwrap()).await.unwrap();
            client.shutdown().await.unwrap();
            for _ in 0..50 {
                if queue.len() == usize::from(text == "good id") + 1 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        }

        shutdown.store(true, Ordering::Release);
        handle.await.unwrap();

        let first = queue.pop().expect("the bad-id event is still delivered");
        assert_eq!(first.content.text, "bad id");
        assert_ne!(first.id, forged);
        assert!(uuid::Uuid::parse_str(&first.id).is_ok(), "replaced by a uuid: {}", first.id);
        let second = queue.pop().expect("the good-id event");
        assert_eq!((second.id.as_str(), second.content.text.as_str()), ("evt-1", "good id"));
    }

    #[tokio::test]
    async fn rejects_oversized_payload() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp
            .path()
            .join("session.sock")
            .to_string_lossy()
            .into_owned();
        let queue = Arc::new(EventQueue::new(10));
        let shutdown = Arc::new(AtomicBool::new(false));

        let handle = listen_session_socket(path.clone(), queue.clone(), shutdown.clone());
        wait_for_socket(&path).await;

        // 257KB of junk — over the limit
        let oversized = vec![b'x'; MAX_PAYLOAD + 1024];
        let mut client = UnixStream::connect(&path).await.unwrap();
        client.write_all(&oversized).await.unwrap();
        client.shutdown().await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        shutdown.store(true, Ordering::Release);
        handle.await.unwrap();

        assert_eq!(queue.len(), 0, "oversized payload should not reach queue");
    }

    #[tokio::test]
    async fn invalid_json_does_not_crash() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp
            .path()
            .join("session.sock")
            .to_string_lossy()
            .into_owned();
        let queue = Arc::new(EventQueue::new(10));
        let shutdown = Arc::new(AtomicBool::new(false));

        let handle = listen_session_socket(path.clone(), queue.clone(), shutdown.clone());
        wait_for_socket(&path).await;

        let mut client = UnixStream::connect(&path).await.unwrap();
        client.write_all(b"this is not json at all").await.unwrap();
        client.shutdown().await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Send a valid event after — proves listener is still running
        let event = Event::simple("test", "still alive", None);
        let json = serde_json::to_vec(&event).unwrap();
        let mut client2 = UnixStream::connect(&path).await.unwrap();
        client2.write_all(&json).await.unwrap();
        client2.shutdown().await.unwrap();

        for _ in 0..50 {
            if !queue.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        shutdown.store(true, Ordering::Release);
        handle.await.unwrap();

        assert_eq!(queue.len(), 1);
        assert_eq!(queue.pop().unwrap().content.text, "still alive");
    }

    #[tokio::test]
    async fn regular_file_is_not_removed_on_startup() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp
            .path()
            .join("session.sock")
            .to_string_lossy()
            .into_owned();
        // Plant a stale file
        std::fs::write(&path, b"stale").unwrap();
        assert!(std::path::Path::new(&path).exists());

        let queue = Arc::new(EventQueue::new(10));
        let shutdown = Arc::new(AtomicBool::new(false));

        // Refuse to replace a regular user file.
        let handle = listen_session_socket(path.clone(), queue.clone(), shutdown.clone());
        wait_for_socket(&path).await;

        shutdown.store(true, Ordering::Release);
        handle.await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"stale");
        std::fs::remove_file(path).unwrap();
    }
}
