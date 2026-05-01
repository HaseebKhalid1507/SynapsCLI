//! Streaming downloader for whisper.cpp model files.
//!
//! See Task B2 of `docs/plans/2026-05-02-whisper-model-manager-and-backends.md`.
//!
//! Pipeline:
//!   1. GET the HuggingFace URL for a [`CatalogEntry`].
//!   2. Stream chunks into `<models_dir>/.<filename>.partial`.
//!   3. Update a running SHA256 hasher (when the catalog has a hash).
//!   4. Verify hash, atomically rename to the final filename.
//!
//! Cancellable via dropping the `tokio::sync::watch` receiver attached
//! to `progress_tx`. Cancellation removes the partial file.

use std::path::{Path, PathBuf};

use futures::StreamExt;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::voice::models::CatalogEntry;

/// Snapshot of in-flight download progress, broadcast on a watch channel.
#[derive(Debug, Clone, Default)]
pub struct DownloadProgress {
    pub bytes: u64,
    pub total: Option<u64>,
    pub done: bool,
    pub error: Option<String>,
}

/// Errors a download can terminate with.
#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    #[error("network error: {0}")]
    Network(String),
    #[error("io error: {0}")]
    Io(String),
    #[error("checksum mismatch (expected {expected}, got {actual})")]
    ChecksumMismatch { expected: String, actual: String },
    #[error("cancelled")]
    Cancelled,
    #[error("server returned status {0}")]
    HttpStatus(u16),
}

impl From<reqwest::Error> for DownloadError {
    fn from(e: reqwest::Error) -> Self {
        DownloadError::Network(e.to_string())
    }
}

impl From<std::io::Error> for DownloadError {
    fn from(e: std::io::Error) -> Self {
        DownloadError::Io(e.to_string())
    }
}

/// Streams a model from HuggingFace to `<models_dir>/<filename>`.
pub async fn download_model(
    entry: &CatalogEntry,
    models_dir: &Path,
    progress_tx: tokio::sync::watch::Sender<DownloadProgress>,
    force: bool,
) -> Result<PathBuf, DownloadError> {
    tokio::fs::create_dir_all(models_dir).await?;
    let final_path = models_dir.join(entry.filename);

    if !force && tokio::fs::metadata(&final_path).await.is_ok() {
        // Already installed; publish a terminal progress and return.
        let total = tokio::fs::metadata(&final_path).await.ok().map(|m| m.len());
        let _ = progress_tx.send(DownloadProgress {
            bytes: total.unwrap_or(0),
            total,
            done: true,
            error: None,
        });
        return Ok(final_path);
    }

    let url = crate::voice::models::download_url(entry);
    let expected = if entry.sha256.is_empty() {
        None
    } else {
        Some(entry.sha256)
    };
    download_from_url(&url, &final_path, expected, progress_tx).await
}

/// Lower-level download primitive, addressed by URL — used by tests so
/// they don't need to monkey-patch the catalog's URL builder.
pub(crate) async fn download_from_url(
    url: &str,
    final_path: &Path,
    expected_sha256: Option<&str>,
    progress_tx: tokio::sync::watch::Sender<DownloadProgress>,
) -> Result<PathBuf, DownloadError> {
    let parent = final_path.parent().ok_or_else(|| {
        DownloadError::Io("final_path has no parent directory".to_string())
    })?;
    tokio::fs::create_dir_all(parent).await?;

    let filename = final_path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| DownloadError::Io("final_path has no filename".to_string()))?;
    let partial_path = parent.join(format!(".{filename}.partial"));

    // Best-effort cleanup of any leftover partial.
    let _ = tokio::fs::remove_file(&partial_path).await;

    let resp = reqwest::Client::new().get(url).send().await?;
    let status = resp.status();
    if !status.is_success() {
        return Err(DownloadError::HttpStatus(status.as_u16()));
    }

    let total = resp.content_length();
    let mut progress = DownloadProgress {
        bytes: 0,
        total,
        done: false,
        error: None,
    };
    let _ = progress_tx.send(progress.clone());

    let mut file = tokio::fs::File::create(&partial_path).await?;
    let mut hasher = expected_sha256.map(|_| Sha256::new());

    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        if progress_tx.is_closed() {
            drop(file);
            let _ = tokio::fs::remove_file(&partial_path).await;
            return Err(DownloadError::Cancelled);
        }
        let chunk = chunk.map_err(|e| {
            // Try to clean up before bubbling.
            let _ = std::fs::remove_file(&partial_path);
            DownloadError::Network(e.to_string())
        })?;
        if let Err(e) = file.write_all(&chunk).await {
            drop(file);
            let _ = tokio::fs::remove_file(&partial_path).await;
            return Err(DownloadError::Io(e.to_string()));
        }
        if let Some(h) = hasher.as_mut() {
            h.update(&chunk);
        }
        progress.bytes += chunk.len() as u64;
        let _ = progress_tx.send(progress.clone());
    }

    file.flush().await?;
    drop(file);

    match (expected_sha256, hasher) {
        (Some(expected), Some(h)) => {
            let actual = hex_lower(&h.finalize());
            if !actual.eq_ignore_ascii_case(expected) {
                let _ = tokio::fs::remove_file(&partial_path).await;
                let err = DownloadError::ChecksumMismatch {
                    expected: expected.to_string(),
                    actual,
                };
                let _ = progress_tx.send(DownloadProgress {
                    bytes: progress.bytes,
                    total,
                    done: true,
                    error: Some(err.to_string()),
                });
                return Err(err);
            }
        }
        _ => {
            tracing::warn!(
                target: "voice::download",
                "skipping SHA256 verification for {} (no expected hash in catalog)",
                filename
            );
        }
    }

    tokio::fs::rename(&partial_path, final_path).await?;

    let _ = progress_tx.send(DownloadProgress {
        bytes: progress.bytes,
        total,
        done: true,
        error: None,
    });

    Ok(final_path.to_path_buf())
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(s, "{:02x}", b);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::sync::Arc;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;
    use tokio::sync::Notify;

    /// Spawn a one-shot HTTP/1.1 server that responds to one connection
    /// with `status` and `body` (or 404 if `body` is `None`).
    /// Returns the bound URL.
    async fn spawn_server(status: u16, body: Option<Vec<u8>>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                // Drain request headers (until \r\n\r\n).
                let mut buf = [0u8; 1024];
                let mut acc = Vec::new();
                loop {
                    let n = match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    acc.extend_from_slice(&buf[..n]);
                    if acc.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let reason = match status {
                    200 => "OK",
                    404 => "Not Found",
                    _ => "Status",
                };
                let body = body.unwrap_or_default();
                let head = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(&body).await;
                let _ = sock.shutdown().await;
            }
        });
        format!("http://{addr}/file")
    }

    /// Spawn a server that writes `body` in two chunks separated by a
    /// notify-await, so the test can drop the receiver mid-stream.
    async fn spawn_drip_server(body: Vec<u8>, gate: Arc<Notify>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let mut acc = Vec::new();
                loop {
                    let n = match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    acc.extend_from_slice(&buf[..n]);
                    if acc.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let half = body.len() / 2;
                let _ = sock.write_all(&body[..half]).await;
                let _ = sock.flush().await;
                gate.notified().await;
                let _ = sock.write_all(&body[half..]).await;
                let _ = sock.shutdown().await;
            }
        });
        format!("http://{addr}/file")
    }

    fn sha256_hex(data: &[u8]) -> String {
        hex_lower(&Sha256::digest(data))
    }

    #[tokio::test]
    async fn early_return_when_already_installed() {
        let dir = tempfile::tempdir().unwrap();
        let entry = CatalogEntry {
            id: "test",
            filename: "ggml-test.bin",
            size_mb: 1,
            multilingual: true,
            sha256: "deadbeef",
        };
        let final_path = dir.path().join(entry.filename);
        tokio::fs::write(&final_path, b"already here").await.unwrap();

        let (tx, _rx) = tokio::sync::watch::channel(DownloadProgress::default());
        let path = download_model(&entry, dir.path(), tx, false).await.unwrap();
        assert_eq!(path, final_path);
        // File should be untouched.
        let bytes = tokio::fs::read(&final_path).await.unwrap();
        assert_eq!(bytes, b"already here");
    }

    #[tokio::test]
    async fn force_redownload_overwrites_existing() {
        let dir = tempfile::tempdir().unwrap();
        let body = b"fresh-body".repeat(50);
        let sha = sha256_hex(&body);
        let url = spawn_server(200, Some(body.clone())).await;
        let final_path = dir.path().join("ggml-test.bin");
        tokio::fs::write(&final_path, b"stale").await.unwrap();

        let (tx, _rx) = tokio::sync::watch::channel(DownloadProgress::default());
        let out = download_from_url(&url, &final_path, Some(&sha), tx)
            .await
            .unwrap();
        assert_eq!(out, final_path);
        let bytes = tokio::fs::read(&final_path).await.unwrap();
        assert_eq!(bytes, body);
    }

    #[tokio::test]
    async fn streams_and_verifies_checksum() {
        let dir = tempfile::tempdir().unwrap();
        let body = b"hello world".repeat(100);
        let sha = sha256_hex(&body);
        let url = spawn_server(200, Some(body.clone())).await;
        let final_path = dir.path().join("ggml-test.bin");

        let (tx, _rx) = tokio::sync::watch::channel(DownloadProgress::default());
        let out = download_from_url(&url, &final_path, Some(&sha), tx)
            .await
            .unwrap();
        assert_eq!(tokio::fs::read(&out).await.unwrap(), body);
    }

    #[tokio::test]
    async fn checksum_mismatch_deletes_partial() {
        let dir = tempfile::tempdir().unwrap();
        let body = b"body".repeat(20);
        let url = spawn_server(200, Some(body)).await;
        let final_path = dir.path().join("ggml-test.bin");
        let partial = dir.path().join(".ggml-test.bin.partial");

        let (tx, _rx) = tokio::sync::watch::channel(DownloadProgress::default());
        let err = download_from_url(&url, &final_path, Some("00".repeat(32).as_str()), tx)
            .await
            .unwrap_err();
        assert!(matches!(err, DownloadError::ChecksumMismatch { .. }));
        assert!(!partial.exists());
        assert!(!final_path.exists());
    }

    #[tokio::test]
    async fn non_2xx_returns_http_status_error() {
        let dir = tempfile::tempdir().unwrap();
        let url = spawn_server(404, None).await;
        let final_path = dir.path().join("ggml-test.bin");

        let (tx, _rx) = tokio::sync::watch::channel(DownloadProgress::default());
        let err = download_from_url(&url, &final_path, None, tx)
            .await
            .unwrap_err();
        match err {
            DownloadError::HttpStatus(404) => {}
            other => panic!("expected HttpStatus(404), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cancellation_via_dropped_receiver_removes_partial() {
        let dir = tempfile::tempdir().unwrap();
        let body = b"abcdefgh".repeat(2048);
        let gate = Arc::new(Notify::new());
        let url = spawn_drip_server(body.clone(), gate.clone()).await;
        let final_path = dir.path().join("ggml-test.bin");
        let partial = dir.path().join(".ggml-test.bin.partial");

        let (tx, rx) = tokio::sync::watch::channel(DownloadProgress::default());
        let sha = sha256_hex(&body);
        let final_path_c = final_path.clone();
        let url_c = url.clone();
        let handle = tokio::spawn(async move {
            download_from_url(&url_c, &final_path_c, Some(&sha), tx).await
        });

        // Wait for first chunk to arrive.
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            if rx.borrow().bytes > 0 {
                break;
            }
        }
        assert!(rx.borrow().bytes > 0, "no bytes received before drop");
        drop(rx);
        // Release the second chunk so the stream loop wakes and observes the closure.
        gate.notify_one();

        let res = handle.await.unwrap();
        assert!(matches!(res, Err(DownloadError::Cancelled)), "got {res:?}");
        assert!(!partial.exists(), "partial file leaked");
        assert!(!final_path.exists());
    }

    #[tokio::test]
    async fn empty_sha_skips_verification() {
        let dir = tempfile::tempdir().unwrap();
        let body = b"whatever bytes".to_vec();
        let url = spawn_server(200, Some(body.clone())).await;
        let final_path = dir.path().join("ggml-test.bin");

        let (tx, _rx) = tokio::sync::watch::channel(DownloadProgress::default());
        let out = download_from_url(&url, &final_path, None, tx)
            .await
            .unwrap();
        assert_eq!(tokio::fs::read(&out).await.unwrap(), body);
    }

    #[tokio::test]
    async fn progress_updates_published() {
        let dir = tempfile::tempdir().unwrap();
        let body = b"chunk".repeat(500);
        let sha = sha256_hex(&body);
        let url = spawn_server(200, Some(body.clone())).await;
        let final_path = dir.path().join("ggml-test.bin");

        let (tx, mut rx) = tokio::sync::watch::channel(DownloadProgress::default());
        let collector = tokio::spawn(async move {
            let mut snapshots = Vec::new();
            loop {
                snapshots.push(rx.borrow().clone());
                if rx.borrow().done {
                    break;
                }
                if rx.changed().await.is_err() {
                    snapshots.push(rx.borrow().clone());
                    break;
                }
            }
            snapshots
        });

        download_from_url(&url, &final_path, Some(&sha), tx)
            .await
            .unwrap();
        let snapshots = collector.await.unwrap();
        assert!(snapshots.iter().any(|s| s.done && s.error.is_none()));
        let mut last = 0u64;
        for s in &snapshots {
            assert!(s.bytes >= last, "bytes regressed: {} -> {}", last, s.bytes);
            last = s.bytes;
        }
        assert_eq!(last, body.len() as u64);
    }
}
