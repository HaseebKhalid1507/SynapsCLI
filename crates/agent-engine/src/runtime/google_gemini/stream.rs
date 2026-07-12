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

use agent_core::auth::{BrokerError, CredentialBroker, ProxyMethod, ProxyRequest};
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
    let body = translate_generate_content_request(model, project, system_prompt, turns, tools);
    let request = ProxyRequest {
        provider: "google-gemini".into(),
        method: ProxyMethod::Post,
        path: "/v1internal:streamGenerateContent".into(),
        body: Some(serde_json::to_value(&body).map_err(|e| {
            StreamError::Decode(format!("failed to serialize request: {e}"))
        })?),
        stream: true,
    };
    let mut byte_stream = broker.proxy_stream(request).await?;

    // Pump on a dedicated task and forward decoded events through mpsc. The
    // pump is `'static`-bounded, so we don't need async-stream / self-refs.
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Result<GeminiStreamEvent, StreamError>>();
    tokio::spawn(async move {
        let mut buf = BytesMut::new();
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    let _ = tx.send(Err(StreamError::Cancelled));
                    return;
                }
                chunk = byte_stream.next() => {
                    match chunk {
                        Some(Ok(bytes)) => {
                            if buf.len().saturating_add(bytes.len()) > MAX_INBOUND_LINE_BYTES {
                                let _ = tx.send(Err(StreamError::LineTooLarge));
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
                                                if tx.send(Ok(ev)).is_err() {
                                                    return;
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            let _ = tx.send(Err(StreamError::Decode(e)));
                                            return;
                                        }
                                    },
                                    Err(_) => {
                                        let _ = tx.send(Err(StreamError::Decode(
                                            "non-utf8 in stream".into(),
                                        )));
                                        return;
                                    }
                                }
                            }
                        }
                        Some(Err(e)) => {
                            let _ = tx.send(Err(StreamError::Broker(e)));
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
                                                let _ = tx.send(Ok(ev));
                                            }
                                        }
                                        Err(e) => {
                                            let _ = tx.send(Err(StreamError::Decode(e)));
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

    Ok(Box::pin(tokio_stream::wrappers::UnboundedReceiverStream::new(rx)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::auth::{AccessToken, ProxyByteStream, ProxyResponse};
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
        async fn capabilities(
            &self,
        ) -> Result<Vec<agent_core::auth::ProviderStatus>, BrokerError> {
            Ok(vec![])
        }
    }

    fn chunk(s: &str) -> Result<bytes::Bytes, BrokerError> {
        Ok(bytes::Bytes::copy_from_slice(s.as_bytes()))
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
        assert!(matches!(&got[4], GeminiStreamEvent::Finish { reason: None }));

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

        struct Blocking(Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<Result<bytes::Bytes, BrokerError>>>>);
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
            async fn proxy_stream(
                &self,
                _r: ProxyRequest,
            ) -> Result<ProxyByteStream, BrokerError> {
                let rx = self.0.lock().unwrap().take().unwrap();
                Ok(Box::pin(tokio_stream::wrappers::UnboundedReceiverStream::new(rx)))
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
