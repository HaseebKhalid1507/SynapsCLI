// src/events/socket.rs
// Unix socket listener for per-session event delivery

use std::sync::{atomic::{AtomicBool, Ordering}, Arc};

use tokio::io::AsyncReadExt;
use tokio::net::UnixListener;

use super::queue::EventQueue;
use super::types::Event;

const MAX_PAYLOAD: usize = 256 * 1024; // 256KB

/// Remove socket file if it exists. Best-effort, never panics.
/// Checks for symlinks to prevent symlink attacks on /tmp.
pub fn cleanup_socket(socket_path: &str) {
    let path = std::path::Path::new(socket_path);
    #[cfg(unix)]
    {
        if let Ok(meta) = std::fs::symlink_metadata(path) {
            if meta.file_type().is_symlink() {
                tracing::warn!("socket: refusing to remove symlink at {}", socket_path);
                return;
            }
        }
    }
    if path.exists() {
        let _ = std::fs::remove_file(path);
    }
}

/// Bind a Unix socket at `socket_path`, accept connections, parse incoming
/// events, push to `queue`. Runs until `shutdown` is set.
///
/// Protocol: client connects → sends full JSON event → closes connection.
/// One event per connection. Max payload 256KB.
pub fn listen_session_socket(
    socket_path: String,
    queue: Arc<EventQueue>,
    shutdown: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Remove stale socket from a previous crash
        cleanup_socket(&socket_path);

        let listener = match UnixListener::bind(&socket_path) {
            Ok(l) => l,
            Err(e) => {
                tracing::error!("socket: failed to bind {}: {}", socket_path, e);
                return;
            }
        };

        // Lock down the socket — session traffic only
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(&socket_path) {
                let mut perms = meta.permissions();
                perms.set_mode(0o600);
                let _ = std::fs::set_permissions(&socket_path, perms);
            }
        }

        tracing::info!("socket: listening on {}", socket_path);

        loop {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }

            // Poll accept with a timeout so we can check shutdown periodically
            let accept = tokio::time::timeout(
                std::time::Duration::from_millis(500),
                listener.accept(),
            );

            match accept.await {
                Ok(Ok((mut stream, _addr))) => {
                    let queue = queue.clone();
                    tokio::spawn(async move {
                        handle_connection(&mut stream, &queue).await;
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

        cleanup_socket(&socket_path);
        tracing::info!("socket: shut down, removed {}", socket_path);
    })
}

async fn handle_connection(
    stream: &mut tokio::net::UnixStream,
    queue: &EventQueue,
) {
    // Read up to MAX_PAYLOAD + 1 so we can detect oversized payloads
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 8192];

    loop {
        match stream.read(&mut chunk).await {
            Ok(0) => break, // EOF — client closed connection
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
        Ok(event) => {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{Event, Severity};
    use std::sync::atomic::AtomicBool;
    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixStream;

    fn tmp_socket_path() -> String {
        format!(
            "/tmp/test-session-socket-{}.sock",
            uuid::Uuid::new_v4().simple()
        )
    }

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
    async fn delivers_event_to_queue() {
        let path = tmp_socket_path();
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
            if queue.len() > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        shutdown.store(true, Ordering::Relaxed);
        handle.await.unwrap();

        let popped = queue.pop().expect("event should be in queue");
        assert_eq!(popped.content.text, "hello socket");
        assert_eq!(popped.source.source_type, "test");
    }

    #[tokio::test]
    async fn rejects_oversized_payload() {
        let path = tmp_socket_path();
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

        shutdown.store(true, Ordering::Relaxed);
        handle.await.unwrap();

        assert_eq!(queue.len(), 0, "oversized payload should not reach queue");
    }

    #[tokio::test]
    async fn invalid_json_does_not_crash() {
        let path = tmp_socket_path();
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
            if queue.len() > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        shutdown.store(true, Ordering::Relaxed);
        handle.await.unwrap();

        assert_eq!(queue.len(), 1);
        assert_eq!(queue.pop().unwrap().content.text, "still alive");
    }

    #[tokio::test]
    async fn stale_socket_removed_on_startup() {
        let path = tmp_socket_path();
        // Plant a stale file
        std::fs::write(&path, b"stale").unwrap();
        assert!(std::path::Path::new(&path).exists());

        let queue = Arc::new(EventQueue::new(10));
        let shutdown = Arc::new(AtomicBool::new(false));

        // Should not panic — bind replaces the stale file
        let handle = listen_session_socket(path.clone(), queue.clone(), shutdown.clone());
        wait_for_socket(&path).await;

        shutdown.store(true, Ordering::Relaxed);
        handle.await.unwrap();
    }
}
