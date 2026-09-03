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
///
/// Provider-agnostic form: broker status labels are classified but not
/// humanized. Prefer [`provider_error_to_runtime_for`] when the qualified
/// model (and therefore the provider key) is known.
pub fn provider_error_to_runtime(e: BoxedProviderError) -> RuntimeError {
    provider_error_to_runtime_for("", e)
}

/// Like [`provider_error_to_runtime`], but `qualified_model` (e.g.
/// `kimi-code/k3`) lets a vetted broker status label such as
/// `provider request failed: 403 Forbidden [access_terminated_error]` be
/// rendered as an actionable, provider-aware message. The label is one of
/// OUR static constants (selected broker-side, never provider bytes) and the
/// humanizer only ever returns our own text; the raw prefix message is kept
/// for the log line.
pub fn provider_error_to_runtime_for(qualified_model: &str, e: BoxedProviderError) -> RuntimeError {
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

    // Upstream returned an HTTP error status or the Responses API reported a
    // terminal stream failure — API failures, not configuration errors.
    if msg.starts_with("codex request failed:")
        || msg.starts_with("openai request failed:")
        || is_responses_terminal_failure_message(&msg)
    {
        tracing::warn!(error = %msg, "provider API error");
        if let Some(human) = humanize_broker_status_message(qualified_model, &msg) {
            return RuntimeError::ApiStatus(human);
        }
        return RuntimeError::ApiStatus(msg);
    }

    tracing::warn!(error = %msg, "provider config error");
    RuntimeError::Config(format!("openai provider: {msg}"))
}

/// Render a broker status failure carrying a vetted label as an actionable
/// message. `None` when the message has no label, the label is unknown to
/// the humanizer, or the provider key cannot be derived from the model.
fn humanize_broker_status_message(qualified_model: &str, msg: &str) -> Option<String> {
    let provider = qualified_model.split_once('/')?.0;
    if provider.is_empty() {
        return None;
    }
    let status = crate::runtime::trace::openai::broker_error_status(msg)?;
    let label = broker_error_label(msg)?;
    crate::core::error::humanize_proxy_status_error(provider, status, Some(label))
}

/// Extract the `[label]` the broker appends after the canonical reason
/// phrase (`provider request failed: 403 Forbidden [access_terminated_error]`).
/// Only identifier-shaped labels are accepted; anything else is `None`.
pub(crate) fn broker_error_label(msg: &str) -> Option<&str> {
    const MARKER: &str = "provider request failed: ";
    let rest = &msg[msg.find(MARKER)? + MARKER.len()..];
    let open = rest.find(" [")?;
    let after = &rest[open + 2..];
    let close = after.find(']')?;
    let label = &after[..close];
    (!label.is_empty()
        && label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'))
    .then_some(label)
}

/// Responses-API terminal stream failures are `"{label}{suffix}"` where the
/// label is a provider display name ("Codex", "xAI") and the suffix is one of
/// the static templates in `stream.rs`. Match label-independently on the
/// suffix so every provider label classifies as an API failure.
fn is_responses_terminal_failure_message(msg: &str) -> bool {
    use super::stream::{
        RESPONSES_CAPACITY_SUFFIX, RESPONSES_CONTEXT_SUFFIX, RESPONSES_EMPTY_SUFFIX,
        RESPONSES_FAILED_SUFFIX, RESPONSES_INCOMPLETE_SUFFIX, RESPONSES_MISSING_TERMINAL_SUFFIX,
    };
    const SUFFIXES: &[&str] = &[
        RESPONSES_FAILED_SUFFIX,
        RESPONSES_CAPACITY_SUFFIX,
        RESPONSES_CONTEXT_SUFFIX,
        RESPONSES_INCOMPLETE_SUFFIX,
        RESPONSES_EMPTY_SUFFIX,
        RESPONSES_MISSING_TERMINAL_SUFFIX,
    ];
    SUFFIXES.iter().any(|suffix| {
        msg.strip_suffix(suffix)
            .is_some_and(|label| !label.is_empty() && !label.contains(char::is_whitespace))
    })
}

/// Walk the boxed error and its source chain looking for a `reqwest::Error`.
///
/// `send().await?` boxes the `reqwest::Error` directly, but helpers may wrap
/// it another level deep — check the whole chain, not just the top.
fn find_reqwest_error<'a>(e: &'a (dyn std::error::Error + 'static)) -> Option<&'a reqwest::Error> {
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
        assert!(
            display.contains("127.0.0.1"),
            "must name the host: {display}"
        );
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
    fn responses_terminal_errors_are_api_status_not_config() {
        for msg in [
            "Codex response failed in stream. Provider error details withheld because they can echo request content.",
            "Codex response was incomplete. Retry the request or reduce the requested output/context size.",
            "Codex completed without text or tool output. Retry the request.",
            "Codex response stream ended without a terminal event. Retry the request.",
            "Codex rejected the request: the conversation exceeds this model's context window. Run /compact or start a fresh session to continue.",
            // xAI-labelled variants (xai-auth Responses route).
            "xAI response failed in stream. Provider error details withheld because they can echo request content.",
            "xAI reports the model is at capacity (retries exhausted). Try again in a few minutes or switch models with /model.",
            "xAI completed without text or tool output. Retry the request.",
            "xAI response stream ended without a terminal event. Retry the request.",
            "xAI response was incomplete. Retry the request or reduce the requested output/context size.",
        ] {
            let err = provider_error_to_runtime(msg.into());
            assert!(
                matches!(err, RuntimeError::ApiStatus(_)),
                "Responses terminal must classify as ApiStatus: {err:?}"
            );
            assert!(!err.to_string().starts_with("Config error"));
        }
    }

    /// A message that merely mentions a suffix mid-sentence (or carries a
    /// non-label prefix) must not be mistaken for a Responses terminal.
    #[test]
    fn responses_terminal_matcher_requires_bare_label_prefix() {
        assert!(!is_responses_terminal_failure_message(
            "No API key for 'xai'. response failed in stream."
        ));
        assert!(!is_responses_terminal_failure_message(
            " response failed in stream. Provider error details withheld because they can echo request content."
        ));
        assert!(is_responses_terminal_failure_message(
            "xAI reports the model is at capacity (retries exhausted). Try again in a few minutes or switch models with /model."
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
            runtime_err
                .to_string()
                .starts_with("Config error: openai provider: "),
            "must keep the historical prefix for genuine config errors: {runtime_err}"
        );
    }

    /// A broker status failure carrying a vetted label is humanized into an
    /// actionable, provider-aware message when the qualified model is known
    /// — and never echoes anything but our own text.
    #[test]
    fn vetted_kimi_quota_label_is_humanized_for_known_provider() {
        let raw = "openai request failed: broker transport error: provider request failed: 403 Forbidden [access_terminated_error]";
        let err: BoxedProviderError = raw.into();
        let runtime_err = provider_error_to_runtime_for("kimi-code/k3", err);
        let RuntimeError::ApiStatus(msg) = runtime_err else {
            panic!("expected ApiStatus, got {runtime_err:?}");
        };
        assert!(
            msg.starts_with("kimi-code: usage quota exhausted (HTTP 403)"),
            "{msg}"
        );
        assert!(msg.contains("weekly"), "{msg}");
        assert!(
            !msg.contains("login"),
            "quota must not read as an auth failure: {msg}"
        );
        assert!(
            !msg.contains("[access_terminated_error]"),
            "label must be rendered, not leaked: {msg}"
        );
    }

    /// Without a provider key (legacy entry point) or without a label the
    /// raw, status-only prefix message is preserved verbatim.
    #[test]
    fn broker_status_without_label_or_provider_keeps_raw_message() {
        let labelled = "openai request failed: broker transport error: provider request failed: 403 Forbidden [access_terminated_error]";
        match provider_error_to_runtime(labelled.into()) {
            RuntimeError::ApiStatus(msg) => assert_eq!(msg, labelled),
            other => panic!("expected ApiStatus, got {other:?}"),
        }
        let plain =
            "openai request failed: broker transport error: provider request failed: 403 Forbidden";
        match provider_error_to_runtime_for("kimi-code/k3", plain.into()) {
            RuntimeError::ApiStatus(msg) => assert_eq!(msg, plain),
            other => panic!("expected ApiStatus, got {other:?}"),
        }
        // Unknown (status, label) combos also fall through unchanged.
        let unknown =
            "openai request failed: provider request failed: 418 I'm a teapot [server_error]";
        match provider_error_to_runtime_for("groq/llama", unknown.into()) {
            RuntimeError::ApiStatus(msg) => assert_eq!(msg, unknown),
            other => panic!("expected ApiStatus, got {other:?}"),
        }
    }

    #[test]
    fn broker_error_label_accepts_only_identifier_shaped_labels() {
        assert_eq!(
            broker_error_label(
                "x: provider request failed: 403 Forbidden [access_terminated_error]"
            ),
            Some("access_terminated_error")
        );
        assert_eq!(
            broker_error_label("provider request failed: 403 Forbidden"),
            None
        );
        assert_eq!(
            broker_error_label("provider request failed: 403 Forbidden []"),
            None
        );
        assert_eq!(
            broker_error_label("provider request failed: 403 Forbidden [<script>ECHO]"),
            None
        );
        assert_eq!(
            broker_error_label("provider request failed: 403 Forbidden [Mixed-Case]"),
            None
        );
        assert_eq!(
            broker_error_label("no marker here [access_terminated_error]"),
            None
        );
    }
}
