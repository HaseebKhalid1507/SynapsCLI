//! Broker-proxied streaming call to Code Assist's
//! `v1internal:streamGenerateContent`. Composes:
//!
//!   1. `translate_generate_content_request` (pure translator)
//!   2. `CredentialBroker::proxy_stream` (bearer + pinned host + no redirect)
//!   3. line-buffered SSE decoding via `from_stream_line`
//!
//! The runtime never touches the OAuth access token, the refresh token, or
//! auth.json — those live behind the broker boundary. Cancellation is
//! honored between chunks so long streams terminate promptly.

use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};

/// Bounded producer/consumer handoff for decoded Gemini events. The producer
/// awaits capacity (explicit backpressure); dropping the receiver makes
/// `send` fail immediately and releases the pump.
pub const GEMINI_EVENT_CHANNEL_CAPACITY: usize = 64;
static GEMINI_PRODUCED: AtomicU64 = AtomicU64::new(0);
static GEMINI_FORWARDED: AtomicU64 = AtomicU64::new(0);
static GEMINI_DROPPED: AtomicU64 = AtomicU64::new(0);
static GEMINI_ACTIVE_PRODUCERS: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy)]
pub struct GeminiEventChannelSnapshot {
    pub produced_events: u64,
    pub forwarded_events: u64,
    pub dropped_events: u64,
    pub retained_events: u64,
    pub active_producers: u64,
}

pub fn gemini_event_channel_snapshot() -> GeminiEventChannelSnapshot {
    let produced = GEMINI_PRODUCED.load(Ordering::Relaxed);
    let forwarded = GEMINI_FORWARDED.load(Ordering::Relaxed);
    let dropped = GEMINI_DROPPED.load(Ordering::Relaxed);
    GeminiEventChannelSnapshot {
        produced_events: produced,
        forwarded_events: forwarded,
        dropped_events: dropped,
        retained_events: produced.saturating_sub(forwarded).saturating_sub(dropped),
        active_producers: GEMINI_ACTIVE_PRODUCERS.load(Ordering::SeqCst),
    }
}

async fn send_event(
    tx: &tokio::sync::mpsc::Sender<Result<GeminiStreamEvent, StreamError>>,
    event: Result<GeminiStreamEvent, StreamError>,
) -> bool {
    GEMINI_PRODUCED.fetch_add(1, Ordering::Relaxed);
    if tx.send(event).await.is_ok() {
        GEMINI_FORWARDED.fetch_add(1, Ordering::Relaxed);
        true
    } else {
        GEMINI_DROPPED.fetch_add(1, Ordering::Relaxed);
        false
    }
}

use agent_core::auth::{BrokerError, CredentialBroker, ProxyRequest};
use bytes::BytesMut;
use futures::{Stream, StreamExt};
use tokio_util::sync::CancellationToken;

use super::translate::{
    from_stream_line, translate_generate_content_request, ChatTurn, GeminiStreamEvent, ToolSpec,
    MAX_INBOUND_LINE_BYTES,
};

/// Runtime-facing outcome of a single streamed model turn.
#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    #[error("gemini stream error: {0}")]
    Broker(#[from] BrokerError),
    #[error("gemini stream error: {0}")]
    Decode(String),
    #[error("gemini stream error: cancelled")]
    Cancelled,
    #[error("gemini stream error: line buffer exceeded {MAX_INBOUND_LINE_BYTES} bytes")]
    LineTooLarge,
}

/// Build the exact-byte `ProxyRequest` for one streamed Code Assist turn.
/// The JSON envelope is serialized **once** via
/// [`ProxyRequest::post_json_exact`]; the returned [`bytes::Bytes`] handle is
/// the very buffer `LocalBroker` sends verbatim, so a caller-side trace
/// digest over it describes the true wire body on the local-broker path.
pub fn build_stream_request(
    model: impl Into<String>,
    project: Option<String>,
    system_prompt: Option<String>,
    turns: &[ChatTurn],
    tools: &[ToolSpec],
) -> Result<(ProxyRequest, bytes::Bytes), StreamError> {
    let body = translate_generate_content_request(model, project, system_prompt, turns, tools);
    let body = serde_json::to_value(&body)
        .map_err(|e| StreamError::Decode(format!("failed to serialize request: {e}")))?;
    ProxyRequest::post_json_exact(
        "google-gemini",
        "/v1internal:streamGenerateContent",
        body,
        true,
    )
    .map_err(StreamError::Broker)
}

/// Start a broker-proxied streaming turn against Code Assist and yield
/// decoded events as they arrive. The returned stream is `Send + 'static`
/// so it can be forwarded through the runtime's event bus.
pub async fn stream_gemini<B: CredentialBroker + ?Sized>(
    broker: &B,
    model: impl Into<String>,
    project: Option<String>,
    system_prompt: Option<String>,
    turns: &[ChatTurn],
    tools: &[ToolSpec],
    cancel: CancellationToken,
) -> Result<Pin<Box<dyn Stream<Item = Result<GeminiStreamEvent, StreamError>> + Send>>, StreamError>
{
    let (request, _bytes) = build_stream_request(model, project, system_prompt, turns, tools)?;
    stream_gemini_request(broker, request, cancel).await
}

/// Start one broker stream attempt from a prebuilt request (see
/// [`build_stream_request`]). Retry loops reuse the same request value per
/// attempt so every attempt sends identical bytes.
pub async fn stream_gemini_request<B: CredentialBroker + ?Sized>(
    broker: &B,
    request: ProxyRequest,
    cancel: CancellationToken,
) -> Result<Pin<Box<dyn Stream<Item = Result<GeminiStreamEvent, StreamError>> + Send>>, StreamError>
{
    let mut byte_stream = broker.proxy_stream(request).await?;

    // Pump on a dedicated task and forward decoded events through mpsc. The
    // pump is `'static`-bounded, so we don't need async-stream / self-refs.
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<GeminiStreamEvent, StreamError>>(
        GEMINI_EVENT_CHANNEL_CAPACITY,
    );
    GEMINI_ACTIVE_PRODUCERS.fetch_add(1, Ordering::SeqCst);
    tokio::spawn(async move {
        struct ProducerGauge;
        impl Drop for ProducerGauge {
            fn drop(&mut self) {
                GEMINI_ACTIVE_PRODUCERS.fetch_sub(1, Ordering::SeqCst);
            }
        }
        let _gauge = ProducerGauge;
        let mut buf = BytesMut::new();
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    let _ = send_event(&tx, Err(StreamError::Cancelled)).await;
                    return;
                }
                chunk = byte_stream.next() => {
                    match chunk {
                        Some(Ok(bytes)) => {
                            if buf.len().saturating_add(bytes.len()) > MAX_INBOUND_LINE_BYTES {
                                let _ = send_event(&tx, Err(StreamError::LineTooLarge)).await;
                                return;
                            }
                            buf.extend_from_slice(&bytes);
                            while let Some(pos) = buf.iter().position(|b| *b == b'\n') {
                                let line = buf.split_to(pos + 1);
                                let line = std::str::from_utf8(&line)
                                    .map(|s| s.trim_end_matches(['\r', '\n']).to_string());
                                match line {
                                    Ok(line) => match from_stream_line(&line) {
                                        Ok(events) => {
                                            for ev in events {
                                                if !send_event(&tx, Ok(ev)).await {
                                                    return;
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            let _ = send_event(&tx, Err(StreamError::Decode(e))).await;
                                            return;
                                        }
                                    },
                                    Err(_) => {
                                        let _ = send_event(&tx, Err(StreamError::Decode(
                                            "non-utf8 in stream".into(),
                                        ))).await;
                                        return;
                                    }
                                }
                            }
                        }
                        Some(Err(e)) => {
                            let _ = send_event(&tx, Err(StreamError::Broker(e))).await;
                            return;
                        }
                        None => {
                            if !buf.is_empty() {
                                let tail = std::str::from_utf8(&buf)
                                    .map(|s| s.trim_end().to_string())
                                    .unwrap_or_default();
                                if !tail.is_empty() {
                                    match from_stream_line(&tail) {
                                        Ok(events) => {
                                            for ev in events {
                                                let _ = send_event(&tx, Ok(ev)).await;
                                            }
                                        }
                                        Err(e) => {
                                            let _ = send_event(&tx, Err(StreamError::Decode(e))).await;
                                        }
                                    }
                                }
                            }
                            return;
                        }
                    }
                }
            }
        }
    });

    Ok(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::auth::{AccessToken, ProxyByteStream, ProxyMethod, ProxyResponse};
    use async_trait::async_trait;
    use futures::stream;
    use std::sync::{Arc, Mutex};

    struct StubBroker {
        chunks: Mutex<Option<Vec<Result<bytes::Bytes, BrokerError>>>>,
        seen: Arc<Mutex<Option<ProxyRequest>>>,
    }

    impl StubBroker {
        fn new(chunks: Vec<Result<bytes::Bytes, BrokerError>>) -> Self {
            Self {
                chunks: Mutex::new(Some(chunks)),
                seen: Arc::default(),
            }
        }
    }

    #[async_trait]
    impl CredentialBroker for StubBroker {
        async fn access_token(
            &self,
            _p: agent_core::auth::OAuthProviderId,
        ) -> Result<AccessToken, BrokerError> {
            Err(BrokerError::NotConfigured("stub".into()))
        }
        async fn proxy(&self, _r: ProxyRequest) -> Result<ProxyResponse, BrokerError> {
            Err(BrokerError::Denied("not implemented".into()))
        }
        async fn proxy_stream(
            &self,
            request: ProxyRequest,
        ) -> Result<ProxyByteStream, BrokerError> {
            *self.seen.lock().unwrap() = Some(request);
            let chunks = self.chunks.lock().unwrap().take().unwrap_or_default();
            Ok(Box::pin(stream::iter(chunks)))
        }
        async fn anthropic_usage(&self) -> Result<serde_json::Value, BrokerError> {
            Err(BrokerError::Denied("not implemented".into()))
        }
        async fn capabilities(&self) -> Result<Vec<agent_core::auth::ProviderStatus>, BrokerError> {
            Ok(vec![])
        }
    }

    fn chunk(s: &str) -> Result<bytes::Bytes, BrokerError> {
        Ok(bytes::Bytes::copy_from_slice(s.as_bytes()))
    }

    /// A stalled consumer retains at most the explicit channel capacity;
    /// dropping the stream releases the producer task.
    #[tokio::test]
    async fn slow_consumer_is_bounded_at_the_model_event_production_boundary() {
        let mut lines = Vec::new();
        for _ in 0..10_000 {
            lines.push(chunk("data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\"}]}}]}}\n"));
        }
        let before = gemini_event_channel_snapshot();
        let broker = StubBroker::new(lines);
        let stream = stream_gemini(
            &broker,
            "gemini-2.5-pro",
            None,
            None,
            &[ChatTurn::User { text: "hi".into() }],
            &[],
            CancellationToken::new(),
        )
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let stalled = gemini_event_channel_snapshot();
        let retained_delta = stalled
            .produced_events
            .saturating_sub(before.produced_events)
            .saturating_sub(
                stalled
                    .forwarded_events
                    .saturating_sub(before.forwarded_events),
            )
            .saturating_sub(stalled.dropped_events.saturating_sub(before.dropped_events));
        assert!(retained_delta <= GEMINI_EVENT_CHANNEL_CAPACITY as u64);
        let active_before = before.active_producers;
        drop(stream);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while gemini_event_channel_snapshot().active_producers > active_before
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(gemini_event_channel_snapshot().active_producers <= active_before);
    }

    #[tokio::test]
    async fn streams_text_deltas_and_tool_calls_in_order() {
        let broker = StubBroker::new(vec![
            chunk("data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hello \"}]}}]}}\n"),
            chunk("data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"world\"}]}}]}}\n"),
            chunk("data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"do\",\"args\":{}}}]},\"finishReason\":\"STOP\"}]}}\n"),
            chunk("data: [DONE]\n"),
        ]);
        let mut stream = stream_gemini(
            &broker,
            "gemini-2.5-pro",
            Some("proj".into()),
            None,
            &[ChatTurn::User { text: "hi".into() }],
            &[],
            CancellationToken::new(),
        )
        .await
        .unwrap();

        let mut got = Vec::new();
        while let Some(ev) = stream.next().await {
            got.push(ev.unwrap());
        }
        assert!(matches!(&got[0], GeminiStreamEvent::TextDelta(t) if t == "Hello "));
        assert!(matches!(&got[1], GeminiStreamEvent::TextDelta(t) if t == "world"));
        assert!(matches!(&got[2], GeminiStreamEvent::ToolCall(c) if c.name == "do"));
        assert!(matches!(&got[3], GeminiStreamEvent::Finish { reason: Some(r) } if r == "STOP"));
        assert!(matches!(
            &got[4],
            GeminiStreamEvent::Finish { reason: None }
        ));

        // Wire request went to the exact pinned method + provider + streamed.
        let seen = broker.seen.lock().unwrap().clone().unwrap();
        assert_eq!(seen.provider, "google-gemini");
        assert_eq!(seen.path, "/v1internal:streamGenerateContent");
        assert!(seen.stream);
        assert_eq!(seen.method, ProxyMethod::Post);
        let body = seen.body.unwrap();
        assert_eq!(body["model"], "gemini-2.5-pro");
        assert_eq!(body["project"], "proj");
        assert_eq!(body["request"]["contents"][0]["parts"][0]["text"], "hi");
    }

    #[tokio::test]
    async fn cancellation_terminates_stream_between_chunks() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        // First chunk is delivered; then the stream stays open, so the
        // cancel token is the only path to termination.
        tx.send(chunk("data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"one\"}]}}]}}\n"))
            .unwrap();

        struct Blocking(
            Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<Result<bytes::Bytes, BrokerError>>>>,
        );
        #[async_trait]
        impl CredentialBroker for Blocking {
            async fn access_token(
                &self,
                _p: agent_core::auth::OAuthProviderId,
            ) -> Result<AccessToken, BrokerError> {
                Err(BrokerError::NotConfigured("stub".into()))
            }
            async fn proxy(&self, _r: ProxyRequest) -> Result<ProxyResponse, BrokerError> {
                Err(BrokerError::Denied("not implemented".into()))
            }
            async fn proxy_stream(&self, _r: ProxyRequest) -> Result<ProxyByteStream, BrokerError> {
                let rx = self.0.lock().unwrap().take().unwrap();
                Ok(Box::pin(
                    tokio_stream::wrappers::UnboundedReceiverStream::new(rx),
                ))
            }
            async fn anthropic_usage(&self) -> Result<serde_json::Value, BrokerError> {
                Err(BrokerError::Denied("not implemented".into()))
            }
            async fn capabilities(
                &self,
            ) -> Result<Vec<agent_core::auth::ProviderStatus>, BrokerError> {
                Ok(vec![])
            }
        }

        let broker = Blocking(Mutex::new(Some(rx)));
        let cancel = CancellationToken::new();
        let mut stream = stream_gemini(
            &broker,
            "gemini-2.5-pro",
            None,
            None,
            &[ChatTurn::User { text: "hi".into() }],
            &[],
            cancel.clone(),
        )
        .await
        .unwrap();

        // First event: TextDelta("one").
        let first = stream.next().await.unwrap().unwrap();
        assert!(matches!(first, GeminiStreamEvent::TextDelta(t) if t == "one"));

        // Cancel and expect Cancelled next.
        cancel.cancel();
        let next = stream.next().await.unwrap();
        assert!(matches!(next, Err(StreamError::Cancelled)));
        // Sender is dropped — stream ends cleanly after cancel.
        drop(tx);
    }

    #[tokio::test]
    async fn malformed_chunk_yields_decode_error_and_terminates() {
        let broker = StubBroker::new(vec![
            chunk("data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"ok\"}]}}]}}\n"),
            chunk("data: {not json\n"),
            chunk("data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"never\"}]}}]}}\n"),
        ]);
        let mut stream = stream_gemini(
            &broker,
            "gemini-2.5-pro",
            None,
            None,
            &[ChatTurn::User { text: "hi".into() }],
            &[],
            CancellationToken::new(),
        )
        .await
        .unwrap();

        let a = stream.next().await.unwrap().unwrap();
        assert!(matches!(a, GeminiStreamEvent::TextDelta(t) if t == "ok"));
        let b = stream.next().await.unwrap();
        assert!(matches!(b, Err(StreamError::Decode(_))));
        // No further events after a decode error.
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn broker_transport_error_is_surfaced() {
        let broker = StubBroker::new(vec![
            chunk("data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"partial\"}]}}]}}\n"),
            Err(BrokerError::Transport("upstream reset".into())),
        ]);
        let mut stream = stream_gemini(
            &broker,
            "gemini-2.5-pro",
            None,
            None,
            &[ChatTurn::User { text: "hi".into() }],
            &[],
            CancellationToken::new(),
        )
        .await
        .unwrap();
        stream.next().await.unwrap().unwrap();
        match stream.next().await.unwrap() {
            Err(StreamError::Broker(BrokerError::Transport(m))) => {
                assert!(m.contains("upstream reset"))
            }
            other => panic!("expected broker transport error, got {other:?}"),
        }
    }
}
