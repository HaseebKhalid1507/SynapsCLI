//! Anthropic upload/header deadline, independent streaming idle deadline and
//! the stream's progress deadlines. Never attach a reqwest total timeout: it
//! follows the response body and kills healthy long generations. The
//! ordinary client retains its read timeout for auth, sync calls and other
//! providers.
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use futures::{Stream, StreamExt};
use reqwest::{Client, RequestBuilder, Response};

use crate::{Result, RuntimeError};

pub(super) const STREAM_IDLE: Duration = Duration::from_secs(90);
/// No `message_start` this long after the stream opened.
pub(super) const FIRST_EVENT: Duration = Duration::from_secs(180);
/// No parsed non-ping event for this long.
pub(super) const PROGRESS: Duration = Duration::from_secs(600);

pub(super) fn header_deadline(bytes: usize) -> Duration {
    let seconds = bytes / 65536 + usize::from(!bytes.is_multiple_of(65536));
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

#[cfg(test)]
tokio::task_local! {
    /// Test seam: the (`FIRST_EVENT`, `PROGRESS`) deadlines for streams
    /// opened inside this task, so they fire in milliseconds.
    pub(crate) static STREAM_PROGRESS_FOR_TEST: (Duration, Duration);
}

/// A progress deadline that ended a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Deadline {
    FirstEvent,
    Progress,
}

impl Deadline {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::FirstEvent => "first_event",
            Self::Progress => "progress",
        }
    }

    fn message(self) -> &'static str {
        match self {
            Self::FirstEvent => "waiting for the stream's first event",
            Self::Progress => "waiting for stream progress",
        }
    }
}

/// One stream's liveness, counts and timings only (never content). The
/// raw-byte idle timer resets on every chunk, keep-alive pings included, so
/// a stream that only pings never reaches it; these deadlines count parsed
/// events, and a ping is never progress.
#[derive(Debug)]
pub(super) struct StreamProgress {
    opened: tokio::time::Instant,
    first_event: Duration,
    progress: Duration,
    last_event: Option<tokio::time::Instant>,
    pub(super) chunks: u64,
    pub(super) pings: u64,
    /// Parsed non-ping events.
    pub(super) events: u64,
    pub(super) message_start_seen: bool,
    /// From the open to `message_start`, once it came.
    pub(super) message_start_after: Option<Duration>,
    /// The longest gap between two non-ping events.
    pub(super) max_gap: Duration,
    pub(super) fired: Option<Deadline>,
}

impl StreamProgress {
    pub(super) fn new() -> Self {
        let limits = (FIRST_EVENT, PROGRESS);
        #[cfg(test)]
        let limits = STREAM_PROGRESS_FOR_TEST.try_with(|l| *l).unwrap_or(limits);
        Self {
            opened: tokio::time::Instant::now(),
            first_event: limits.0,
            progress: limits.1,
            last_event: None,
            chunks: 0,
            pings: 0,
            events: 0,
            message_start_seen: false,
            message_start_after: None,
            max_gap: Duration::ZERO,
            fired: None,
        }
    }

    /// A later attempt of a request whose stream already reached the
    /// first-event deadline: `message_start` gets the progress window, as a
    /// cold prefill of a huge prompt may take minutes before it, so at most
    /// one prefill is wasted. Under the test seam, its progress value.
    pub(super) fn relax_first_event(&mut self) {
        self.first_event = self.progress;
    }

    pub(super) fn ping(&mut self) {
        self.pings += 1;
    }

    /// A parsed non-ping event: progress.
    pub(super) fn event(&mut self, message_start: bool) {
        let now = tokio::time::Instant::now();
        if let Some(last) = self.last_event {
            self.max_gap = self.max_gap.max(now.saturating_duration_since(last));
        }
        if message_start && self.message_start_after.is_none() {
            self.message_start_after = Some(now.saturating_duration_since(self.opened));
        }
        self.last_event = Some(now);
        self.events += 1;
        self.message_start_seen |= message_start;
    }

    /// The earliest deadline still armed: the first event's until
    /// `message_start`, progress since the last event (else the open).
    fn due(&self) -> (tokio::time::Instant, Deadline) {
        let progress = self.last_event.unwrap_or(self.opened) + self.progress;
        let first = self.opened + self.first_event;
        if !self.message_start_seen && first <= progress {
            (first, Deadline::FirstEvent)
        } else {
            (progress, Deadline::Progress)
        }
    }
}

/// [`next`] within the stream's progress deadline too. It is checked before
/// every read and bounds the read, so it fires on time however often pings
/// arrive. A deadline ends the read as a timeout with its own message.
pub(super) async fn next_or_deadline<S>(
    stream: &mut S,
    idle: Duration,
    progress: &mut StreamProgress,
) -> Option<Result<bytes::Bytes>>
where
    S: Stream<Item = std::result::Result<bytes::Bytes, reqwest::Error>> + Unpin,
{
    let (due, deadline) = progress.due();
    let read = if tokio::time::Instant::now() < due {
        tokio::time::timeout_at(due, next(stream, idle)).await.ok()
    } else {
        None
    };
    match read {
        Some(Some(Ok(chunk))) => {
            progress.chunks += 1;
            Some(Ok(chunk))
        }
        Some(other) => other,
        None => {
            progress.fired = Some(deadline);
            Some(Err(RuntimeError::NetworkTimeout(deadline.message())))
        }
    }
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
        // Headers take longer than the body idle limit below; the header
        // phase has its own (larger) deadline, so they still arrive. Only the
        // stall assertion depends on a limit firing, and the server sleeps
        // 10 s there, so a loaded machine can't make it pass or fail early.
        let (url, task) = server(Duration::from_millis(600), 1, Duration::ZERO).await;
        let response = send(client.post(url).body("body"), Duration::from_secs(10))
            .await
            .unwrap();
        let mut stream = response.bytes_stream();
        // The chunk is written with the headers: generous, not a bound.
        assert!(next(&mut stream, Duration::from_secs(5))
            .await
            .unwrap()
            .is_ok());
        assert!(matches!(
            next(&mut stream, Duration::from_millis(250)).await,
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
        // 8 chunks 200 ms apart (1.6 s) outlast the 1 s header deadline: an
        // active stream has no total deadline. The windows are generous so a
        // loaded machine can't fail them.
        let (url, task) = server(Duration::ZERO, 8, Duration::from_millis(200)).await;
        let mut stream = send(client.get(url), Duration::from_secs(1))
            .await
            .unwrap()
            .bytes_stream();
        for _ in 0..8 {
            assert!(next(&mut stream, Duration::from_secs(2))
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
