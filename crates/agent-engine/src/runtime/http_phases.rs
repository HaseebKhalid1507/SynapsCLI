//! Anthropic upload/header deadline and independent streaming idle deadline.
//! Never attach a reqwest total timeout: it follows the response body and
//! kills healthy long generations. The ordinary client retains its read
//! timeout for auth, sync calls and other providers.
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use futures::{Stream, StreamExt};
use reqwest::{Client, RequestBuilder, Response};

use crate::{Result, RuntimeError};

pub(super) const STREAM_IDLE: Duration = Duration::from_secs(90);

pub(super) fn header_deadline(bytes: usize) -> Duration {
    let seconds = bytes / 65536 + usize::from(bytes % 65536 != 0);
    Duration::from_secs(90u64.saturating_add(seconds as u64))
}

/// Lazy per-Runtime pool. Clones of one runtime share it, separately created
/// workers do not: hyper connections must not outlive their Tokio executor.
pub(super) fn client(slot: &Arc<OnceLock<Client>>) -> Result<&Client> {
    if slot.get().is_none() {
        let client = Client::builder()
            .tls_built_in_webpki_certs(true)
            .tls_built_in_native_certs(true)
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(RuntimeError::Api)?;
        let _ = slot.set(client);
    }
    Ok(slot.get().expect("client initialized"))
}

pub(super) async fn send(req: RequestBuilder, deadline: Duration) -> Result<Response> {
    tokio::time::timeout(deadline, req.send())
        .await
        .map_err(|_| {
            RuntimeError::NetworkTimeout("uploading request / waiting for response headers")
        })?
        .map_err(RuntimeError::Api)
}

/// Reset only when another raw chunk arrives, not on parsed SSE events.
pub(super) async fn next<S>(stream: &mut S, idle: Duration) -> Option<Result<bytes::Bytes>>
where
    S: Stream<Item = std::result::Result<bytes::Bytes, reqwest::Error>> + Unpin,
{
    match tokio::time::timeout(idle, stream.next()).await {
        Ok(next) => next.map(|chunk| chunk.map_err(RuntimeError::Api)),
        Err(_) => Some(Err(RuntimeError::NetworkTimeout(
            "waiting for response body bytes",
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn upload_allowance_scales_with_wire_bytes() {
        for (bytes, seconds) in [
            (0, 90),
            (1, 91),
            (65536, 91),
            (65537, 92),
            (4 * 1024 * 1024, 154),
            (12 * 1024 * 1024, 282),
        ] {
            assert_eq!(header_deadline(bytes).as_secs(), seconds);
        }
        assert!(header_deadline(usize::MAX).as_secs() >= 90);
    }

    async fn server(
        header_wait: Duration,
        chunks: usize,
        gap: Duration,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0; 4096];
            let _ = stream.read(&mut buf).await;
            tokio::time::sleep(header_wait).await;
            if stream
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
                .await
                .is_err()
            {
                return;
            }
            for _ in 0..chunks {
                if stream.write_all(b"1\r\nx\r\n").await.is_err() {
                    return;
                }
                tokio::time::sleep(gap).await;
            }
            tokio::time::sleep(Duration::from_secs(10)).await;
        });
        (format!("http://{address}/"), task)
    }

    #[tokio::test]
    async fn slow_headers_survive_idle_limit_then_stalled_body_times_out() {
        let slot = Arc::new(OnceLock::new());
        let client = client(&slot).unwrap();
        let (url, task) = server(Duration::from_millis(150), 1, Duration::ZERO).await;
        let response = send(client.post(url).body("body"), Duration::from_secs(2))
            .await
            .unwrap();
        let mut stream = response.bytes_stream();
        assert!(next(&mut stream, Duration::from_millis(50))
            .await
            .unwrap()
            .is_ok());
        assert!(matches!(
            next(&mut stream, Duration::from_millis(50)).await,
            Some(Err(RuntimeError::NetworkTimeout(_)))
        ));
        task.abort();
    }

    #[tokio::test]
    async fn hung_headers_are_bounded_and_active_stream_has_no_total_deadline() {
        let slot = Arc::new(OnceLock::new());
        let client = client(&slot).unwrap();
        let (url, task) = server(Duration::from_secs(10), 0, Duration::ZERO).await;
        assert!(matches!(
            send(client.get(url), Duration::from_millis(50)).await,
            Err(RuntimeError::NetworkTimeout(_))
        ));
        task.abort();
        let (url, task) = server(Duration::ZERO, 8, Duration::from_millis(50)).await;
        let mut stream = send(client.get(url), Duration::from_millis(200))
            .await
            .unwrap()
            .bytes_stream();
        for _ in 0..8 {
            assert!(next(&mut stream, Duration::from_millis(200))
                .await
                .unwrap()
                .is_ok());
        }
        task.abort();
    }

    #[tokio::test]
    async fn cancellation_interrupts_hung_send() {
        let slot = Arc::new(OnceLock::new());
        let client = client(&slot).unwrap();
        let (url, task) = server(Duration::from_secs(10), 0, Duration::ZERO).await;
        let cancel = crate::CancellationToken::new();
        cancel.cancel();
        assert!(matches!(
            super::super::api::await_or_cancel(
                &cancel,
                send(client.get(url), Duration::from_secs(10))
            )
            .await,
            Err(RuntimeError::Canceled)
        ));
        task.abort();
    }
}
