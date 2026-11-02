//! What a transport retry on a provider conversation path logs.
//!
//! A send that fails without an HTTP status (reset, stale pooled
//! connection, timeout, …) is retried on the Anthropic stream (`api`),
//! the sync calls (`api_sync`) and the OpenAI-wire send (`openai::stream`).
//! Each retry WARN carries [`TransportDiag`] as structured fields so the
//! cause survives into the log instead of only the humanized text
//! ("Network request failed").
//!
//! Metadata only: static labels, reqwest's classification flags and the
//! cause chain below reqwest's top level, with URLs cut (agent-core
//! `transport_cause_chain`), then the trace export's aggressive secret
//! scrub (`redact_command_text`), then a length cap. No headers, query
//! values, bodies or `Debug` dumps.
//!
//! Also what a transport retry shows: [`transport_retry_notice_due`] keeps
//! a fast first retry out of the user's view (display only).

use std::time::Duration;

use crate::RuntimeError;

/// Most characters of a rendered cause chain in a log field (an ellipsis
/// marks a cut).
pub(crate) const CHAIN_MAX_CHARS: usize = 300;

/// The `reused` value when the failed send's connection reuse is unknown.
///
/// Always the case today: hyper-util 0.1.20 records reuse on its private
/// `Pooled` (`src/client/legacy/pool.rs:524-531`, read at
/// `src/client/legacy/client.rs:329`) and reports it only inside its
/// private `TrySendError` (`client.rs:95-100`); reqwest 0.12.28's
/// `Error` and hyper-util's public `Connected` carry no reuse flag.
pub(crate) const REUSED_UNKNOWN: &str = "unknown";

/// One failed transport attempt, as metadata for a retry WARN.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TransportDiag {
    /// The keep-warm failure label of this failure (`transport`,
    /// `header_timeout`, `idle_timeout`, …).
    pub(crate) failure_label: &'static str,
    /// Vetted static class (agent-core `transport_failure_class`), or
    /// `deadline` for our own phase deadline.
    pub(crate) class: &'static str,
    pub(crate) is_timeout: bool,
    pub(crate) is_connect: bool,
    pub(crate) is_request: bool,
    pub(crate) is_body: bool,
    pub(crate) is_decode: bool,
    /// `true`, `false` or [`REUSED_UNKNOWN`].
    pub(crate) reused: &'static str,
    /// Redacted, capped cause chain; `none` without a source.
    pub(crate) chain: String,
}

impl TransportDiag {
    /// A reqwest transport error.
    pub(crate) fn from_reqwest(error: &reqwest::Error, failure_label: &'static str) -> Self {
        Self {
            is_timeout: error.is_timeout(),
            is_connect: error.is_connect(),
            is_request: error.is_request(),
            is_body: error.is_body(),
            is_decode: error.is_decode(),
            ..Self::from_chain(error, failure_label)
        }
    }

    /// A send-phase or stream error of the Anthropic path: a reqwest error,
    /// or our own phase deadline (`NetworkTimeout`, no source to render).
    pub(crate) fn from_runtime(error: &RuntimeError, failure_label: &'static str) -> Self {
        match error {
            RuntimeError::Api(error) => Self::from_reqwest(error, failure_label),
            RuntimeError::NetworkTimeout(_) => Self::bare(failure_label, "deadline"),
            _ => Self::bare(failure_label, "other"),
        }
    }

    /// Class and chain of any error; the reqwest flags all false.
    fn from_chain(error: &(dyn std::error::Error + 'static), failure_label: &'static str) -> Self {
        Self {
            class: crate::core::error::transport_failure_class(error),
            chain: render_chain(error),
            ..Self::bare(failure_label, "other")
        }
    }

    fn bare(failure_label: &'static str, class: &'static str) -> Self {
        Self {
            failure_label,
            class,
            is_timeout: false,
            is_connect: false,
            is_request: false,
            is_body: false,
            is_decode: false,
            reused: REUSED_UNKNOWN,
            chain: "none".to_string(),
        }
    }
}

/// The cause chain below `error`, URL-free, scrubbed, then capped (the scrub
/// runs before the cap so a cut can never leave a secret's prefix the
/// scrub no longer recognises).
fn render_chain(error: &(dyn std::error::Error + 'static)) -> String {
    match crate::core::error::transport_cause_chain(error) {
        Some(chain) => cap_chars(
            &crate::runtime::trace::export::redact_command_text(&chain),
            CHAIN_MAX_CHARS,
        ),
        None => "none".to_string(),
    }
}

fn cap_chars(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_string(),
    }
}

/// `tracing::warn!` with the [`TransportDiag`] fields plus `attempt` /
/// `max_attempts` (the retry counter and budget the message prints),
/// expanded at the caller so the line keeps its own callsite and target.
/// `warn_transport!(diag, attempt, max_attempts, "message", args…)`.
macro_rules! warn_transport {
    ($diag:expr, $attempt:expr, $max_attempts:expr, $($message:tt)+) => {{
        let diag: &$crate::runtime::net_retry::TransportDiag = &$diag;
        tracing::warn!(
            attempt = $attempt,
            max_attempts = $max_attempts,
            failure_label = diag.failure_label,
            class = diag.class,
            is_timeout = diag.is_timeout,
            is_connect = diag.is_connect,
            is_request = diag.is_request,
            is_body = diag.is_body,
            is_decode = diag.is_decode,
            reused = diag.reused,
            chain = %diag.chain,
            $($message)+
        )
    }};
}
pub(crate) use warn_transport;

/// The shortest wait (the failed attempt plus the backoff before the retry)
/// for which a FIRST transport retry is still announced: a wait this long
/// would look like a hang without a notice.
pub(crate) const SLOW_FIRST_RETRY: Duration = Duration::from_secs(3);

/// Whether a transport retry (a failure with no HTTP status) shows its
/// `SessionEvent::Retry` notice. `retry` is the 1-based retry number,
/// `waited` how long the user waits for it: the failed attempt's own time
/// (a header timeout is long) plus the backoff before the retry.
///
/// Display only: the caller logs, counts, backs off and replays the same
/// either way. A first retry after a fast failure (the stale pooled
/// connection that a new connection usually fixes) is silent; a second
/// consecutive one, or a first one whose wait reaches [`SLOW_FIRST_RETRY`],
/// is shown. HTTP-status retries (429, 5xx, 529) never ask.
pub(crate) fn transport_retry_notice_due(retry: u32, waited: Duration) -> bool {
    retry >= 2 || waited >= SLOW_FIRST_RETRY
}

#[cfg(test)]
tokio::task_local! {
    /// Test seam: inside `LEGACY_TRANSPORT_NOTICES_FOR_TEST.scope(true, f)`
    /// every transport retry shows its notice, as before
    /// [`transport_retry_notice_due`], so a test can run the same failures
    /// with and without the display change. Task-scoped: other tests never
    /// see it.
    pub(crate) static LEGACY_TRANSPORT_NOTICES_FOR_TEST: bool;
}

/// [`transport_retry_notice_due`] at a retry site (plus, in tests, the
/// legacy seam above).
pub(crate) fn show_transport_retry_notice(retry: u32, waited: Duration) -> bool {
    #[cfg(test)]
    let legacy = LEGACY_TRANSPORT_NOTICES_FOR_TEST
        .try_with(|legacy| *legacy)
        .unwrap_or(false);
    #[cfg(not(test))]
    let legacy = false;
    legacy || transport_retry_notice_due(retry, waited)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// One level of a constructed cause chain: fixed text, optional source.
    #[derive(Debug)]
    struct Level(String, Option<Box<dyn std::error::Error + Send + Sync>>);

    impl std::fmt::Display for Level {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.0)
        }
    }
    impl std::error::Error for Level {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.1
                .as_deref()
                .map(|e| e as &(dyn std::error::Error + 'static))
        }
    }

    fn level(
        text: &str,
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    ) -> Box<dyn std::error::Error + Send + Sync> {
        Box::new(Level(text.to_string(), source))
    }

    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// The formatted output of `log` under a capturing subscriber.
    fn captured(log: impl FnOnce()) -> String {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&buf);
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || Capture(Arc::clone(&sink)))
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, log);
        let bytes = buf.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    /// A source error deep in the chain carries a URL with a token query
    /// value AND a header-like credential line: neither reaches the WARN.
    #[test]
    fn retry_warn_names_the_cause_and_never_a_secret_from_a_nested_source() {
        let error = level(
            "error sending request for url (https://api.example.com/v1/messages?key=TOPQUERY0001)",
            Some(level(
                "client error (SendRequest)",
                Some(level(
                    "proxy refused https://login.example.com/oauth?key=QUERYSECRET42&x=1 \
                     with authorization: Bearer sk-ant-oat01-HEADERSECRET9876",
                    Some(Box::new(std::io::Error::new(
                        std::io::ErrorKind::ConnectionReset,
                        "Connection reset by peer (os error 54)",
                    ))),
                )),
            )),
        );
        let diag = TransportDiag::from_chain(&*error, "transport");
        assert_eq!(diag.class, "connection_reset");
        assert_eq!(diag.reused, "unknown");
        assert!(
            diag.chain
                .starts_with("client error (SendRequest): proxy refused [url]"),
            "{}",
            diag.chain
        );

        let logs = captured(|| {
            warn_transport!(
                diag,
                1u32,
                3u32,
                "API retry {}/{}: {}",
                1,
                3,
                "Network request failed"
            );
        });
        let line = logs
            .lines()
            .find(|l| l.contains("API retry 1/3"))
            .unwrap_or_else(|| panic!("the WARN is emitted: {logs}"));
        for field in [
            "attempt=1",
            "max_attempts=3",
            "failure_label=\"transport\"",
            "class=\"connection_reset\"",
            "is_timeout=false",
            "is_connect=false",
            "is_request=false",
            "is_body=false",
            "is_decode=false",
            "reused=\"unknown\"",
            "chain=client error (SendRequest)",
        ] {
            assert!(line.contains(field), "{field} missing: {line}");
        }
        for leak in [
            "TOPQUERY0001",
            "QUERYSECRET42",
            "key=",
            "login.example.com",
            "api.example.com",
            "HEADERSECRET9876",
            "sk-ant",
            "Bearer",
        ] {
            assert!(!logs.contains(leak), "{leak} reached the log: {logs}");
        }
    }

    #[test]
    fn a_long_chain_is_capped_after_the_scrub() {
        let long = "x".repeat(CHAIN_MAX_CHARS * 2);
        let error = level("top", Some(level(&long, None)));
        let diag = TransportDiag::from_chain(&*error, "transport");
        assert_eq!(diag.chain.chars().count(), CHAIN_MAX_CHARS + 1, "{}", diag.chain);
        assert!(diag.chain.ends_with('…'));
        assert_eq!(diag.class, "other");
    }

    #[test]
    fn runtime_errors_map_to_static_fields() {
        let deadline = TransportDiag::from_runtime(
            &RuntimeError::NetworkTimeout("uploading request / waiting for response headers"),
            "header_timeout",
        );
        assert_eq!(
            deadline,
            TransportDiag {
                failure_label: "header_timeout",
                class: "deadline",
                is_timeout: false,
                is_connect: false,
                is_request: false,
                is_body: false,
                is_decode: false,
                reused: "unknown",
                chain: "none".to_string(),
            }
        );

        // A real reqwest::Error, built offline (an unclosed IPv6 host), with a
        // secret-bearing URL attached at reqwest's top level.
        let error = reqwest::Client::new()
            .get("http://[::1/")
            .build()
            .expect_err("an invalid host fails the builder")
            .with_url("https://api.example.com/v1?key=URLSECRET7".parse().unwrap());
        assert!(error.to_string().contains("URLSECRET7"), "precondition: {error}");
        let diag = TransportDiag::from_runtime(&RuntimeError::Api(error), "transport");
        assert_eq!(diag.failure_label, "transport");
        assert_eq!(diag.class, "other");
        // url 2.5.8 `ParseError::InvalidIpv6Address` (src/parser.rs:93).
        assert_eq!(diag.chain, "invalid IPv6 address");
        assert!(!diag.is_timeout && !diag.is_connect && !diag.is_request);
    }

    #[test]
    fn only_a_fast_first_transport_retry_is_silent() {
        use crate::runtime::telemetry::network_retry_delay;
        for (retry, backoff, shown) in [
            // Today's first network backoff (1 s): the stale-connection case.
            (1, network_retry_delay(1), false),
            (1, Duration::from_millis(2_999), false),
            (1, SLOW_FIRST_RETRY, true),
            (1, Duration::from_secs(64), true),
            (2, Duration::ZERO, true),
            (2, network_retry_delay(2), true),
            (3, network_retry_delay(3), true),
        ] {
            assert_eq!(
                transport_retry_notice_due(retry, backoff),
                shown,
                "retry {retry} after {backoff:?}"
            );
            // Outside the legacy seam a retry site decides the same.
            assert_eq!(show_transport_retry_notice(retry, backoff), shown);
        }
    }
}
