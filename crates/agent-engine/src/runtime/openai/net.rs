//! Error classification for the OpenAI-compat / Codex provider routes.
//!
//! Historically every error escaping `try_route` was blanket-wrapped in
//! `RuntimeError::Config("openai provider: …")` — so a transient network
//! blip against `chatgpt.com/backend-api/codex/responses` surfaced to the
//! user as **"Config error"**, implying their configuration was broken
//! (real incident: session 20260714-025948-3dab). This module classifies
//! provider-route errors into honest categories:
//!
//! * transport failures (`reqwest::Error`) → `RuntimeError::ApiStatus` with
//!   a host-aware humanized message that preserves the full cause chain
//! * upstream HTTP status failures (`codex request failed: 503: …`) →
//!   `RuntimeError::ApiStatus`
//! * cancellation → `RuntimeError::Canceled`
//! * everything else (missing keys, malformed tokens, …) → genuine
//!   `RuntimeError::Config`, same prefix as before

use crate::core::error::{error_chain_string, humanize_provider_network_error};
use crate::RuntimeError;

/// Boxed error type produced by the provider routes.
pub type BoxedProviderError = Box<dyn std::error::Error + Send + Sync>;

/// Classify an error escaping the OpenAI/Codex provider route into the
/// right `RuntimeError` variant. Also logs the full cause chain at WARN —
/// provider-route failures were previously invisible in the logs.
pub fn provider_error_to_runtime(e: BoxedProviderError) -> RuntimeError {
    // Transport-level failure (connect / timeout / mid-stream drop)?
    if let Some(re) = find_reqwest_error(e.as_ref()) {
        let msg = humanize_provider_network_error(re);
        tracing::warn!(error = %error_chain_string(e.as_ref()), "provider transport error");
        return RuntimeError::ApiStatus(msg);
    }

    let msg = error_chain_string(e.as_ref());

    if msg.contains("operation canceled") || msg.contains("request canceled") {
        return RuntimeError::Canceled;
    }

    // Upstream returned an HTTP error status — an API failure, not config.
    if msg.starts_with("codex request failed:") || msg.starts_with("openai request failed:") {
        tracing::warn!(error = %msg, "provider API error");
        return RuntimeError::ApiStatus(msg);
    }

    tracing::warn!(error = %msg, "provider config error");
    RuntimeError::Config(format!("openai provider: {msg}"))
}

/// Walk the boxed error and its source chain looking for a `reqwest::Error`.
///
/// `send().await?` boxes the `reqwest::Error` directly, but helpers may wrap
/// it another level deep — check the whole chain, not just the top.
fn find_reqwest_error<'a>(
    e: &'a (dyn std::error::Error + 'static),
) -> Option<&'a reqwest::Error> {
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(err) = current {
        if let Some(re) = err.downcast_ref::<reqwest::Error>() {
            return Some(re);
        }
        current = err.source();
    }
    None
}

/// Strip the provider response-body snippet from a broker proxy error.
///
/// `LocalBroker::proxy_stream` flattens an upstream HTTP failure into
/// `… provider request failed: {status}: {body snippet}`. The snippet is
/// provider-controlled and may echo the full request (spec §5.1) — a byte
/// bound is not redaction. Keep everything up to and including the status
/// (canonical reason phrase, no `:`), drop the snippet. Messages without the
/// marker pass through unchanged: every other `BrokerError` variant carries
/// broker-authored, secret-free text, and transport-level reqwest errors are
/// not response bodies.
pub(crate) fn redact_provider_proxy_error(msg: &str) -> String {
    const MARKER: &str = "provider request failed: ";
    let Some(start) = msg.find(MARKER) else {
        return msg.to_string();
    };
    let status_start = start + MARKER.len();
    match msg[status_start..].find(':') {
        Some(idx) => msg[..status_start + idx].to_string(),
        None => msg.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn connect_error() -> reqwest::Error {
        // Bind then drop → guaranteed-refused port.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        reqwest::Client::new()
            .post(format!("http://{addr}/codex/responses"))
            .send()
            .await
            .expect_err("must fail to connect")
    }

    #[tokio::test]
    async fn transport_error_is_api_status_not_config() {
        let err = connect_error().await;
        let runtime_err = provider_error_to_runtime(Box::new(err));
        assert!(
            matches!(runtime_err, RuntimeError::ApiStatus(_)),
            "transport failure must classify as ApiStatus, got: {runtime_err:?}"
        );
        let display = runtime_err.to_string();
        assert!(
            !display.starts_with("Config error"),
            "transient network failure must not be presented as a config problem: {display}"
        );
        assert!(display.contains("127.0.0.1"), "must name the host: {display}");
    }

    #[tokio::test]
    async fn transport_error_preserves_cause_chain() {
        let err = connect_error().await;
        let display = provider_error_to_runtime(Box::new(err)).to_string();
        // The top-level reqwest Display alone says only "error sending
        // request" — the classified message must keep the underlying cause.
        assert!(
            display.contains("refused") || display.contains("connect"),
            "underlying cause must survive classification: {display}"
        );
    }

    #[test]
    fn upstream_status_error_is_api_status() {
        let e: BoxedProviderError = "codex request failed: 503: upstream sad".into();
        let runtime_err = provider_error_to_runtime(e);
        assert!(
            matches!(runtime_err, RuntimeError::ApiStatus(_)),
            "HTTP status failure must classify as ApiStatus, got: {runtime_err:?}"
        );
        assert!(runtime_err.to_string().contains("503"));
    }

    #[test]
    fn openai_status_error_is_api_status() {
        let e: BoxedProviderError = "openai request failed: 429: slow down".into();
        assert!(matches!(
            provider_error_to_runtime(e),
            RuntimeError::ApiStatus(_)
        ));
    }

    #[test]
    fn cancellation_is_canceled() {
        let e: BoxedProviderError = "operation canceled".into();
        assert!(matches!(
            provider_error_to_runtime(e),
            RuntimeError::Canceled
        ));
        let e: BoxedProviderError = "request canceled".into();
        assert!(matches!(
            provider_error_to_runtime(e),
            RuntimeError::Canceled
        ));
    }

    #[test]
    fn redact_provider_proxy_error_drops_body_snippet_keeps_status() {
        let msg = "gemini stream error: broker transport error: provider request failed: \
                   429 Too Many Requests: {\"error\":{\"message\":\"ECHOED:secret\"}}";
        let redacted = redact_provider_proxy_error(msg);
        assert_eq!(
            redacted,
            "gemini stream error: broker transport error: provider request failed: \
             429 Too Many Requests"
        );
    }

    #[test]
    fn redact_provider_proxy_error_passes_through_non_proxy_messages() {
        for msg in [
            "broker transport error: connection reset",
            "no credential configured for 'groq'. Run `synaps login` to add one.",
            "request canceled",
        ] {
            assert_eq!(redact_provider_proxy_error(msg), msg);
        }
    }

    #[test]
    fn genuine_config_problems_stay_config_with_original_prefix() {
        let e: BoxedProviderError =
            "No API key for 'groq'. Set provider.groq in ~/.synaps-cli/config or the corresponding env var."
                .into();
        let runtime_err = provider_error_to_runtime(e);
        assert!(
            matches!(runtime_err, RuntimeError::Config(_)),
            "missing key IS a config problem, got: {runtime_err:?}"
        );
        assert!(
            runtime_err.to_string().starts_with("Config error: openai provider: "),
            "must keep the historical prefix for genuine config errors: {runtime_err}"
        );
    }
}
