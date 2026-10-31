//! gzip `Content-Encoding` for Anthropic request bodies.
//!
//! The Messages API is stateless: every request re-uploads the whole
//! conversation (tool results, file contents, diffs). Upload is the slow
//! direction of most home and campus links, and these bodies are JSON text
//! that gzips about 2x, so large requests sent straight to
//! `api.anthropic.com` go out gzip-encoded. The server decodes them before
//! anything reads them: tokens, billing and prompt caching are unchanged,
//! and the trace keeps digesting the JSON, not the wire bytes.
//!
//! Only gzip: the API refuses `br`, `zstd` and `deflate` request bodies.
//!
//! The level defaults to 1. On real session bodies it runs at about 250 MB/s
//! and keeps over 80% of level 6's saving. Level 6 runs at about 55 MB/s,
//! which costs more than it saves on uplinks faster than 200-375 Mbps; level
//! 1 breaks even between 750 Mbps and 1.3 Gbps. On a slow uplink the extra
//! CPU time is tens of milliseconds and the upload saved is seconds, so a
//! higher level wins there: against level 1, levels 3-6 pay off below
//! roughly 10-50 Mbps, depending on the body. Above 6 the body barely
//! shrinks further.
//!
//! Configured by `request_compression` (`auto` | `on` | `off`) and
//! `request_compression_level` (1–9), both in /settings. With `auto` (the
//! default) the body goes out uncompressed when any of these hold:
//! * the endpoint is not `https://api.anthropic.com` (a gateway set through
//!   `SYNAPS_ANTHROPIC_BASE_URL` may not decode request bodies);
//! * an HTTPS proxy is configured, or a custom CA bundle (`SSL_CERT_FILE`,
//!   `SSL_CERT_DIR`): TLS-inspecting middleboxes are the usual reason a
//!   compressed request body gets mangled;
//! * the body is smaller than [`MIN_BYTES`].
//!
//! `on` drops the endpoint, proxy and CA checks, for a proxy or gateway known
//! to pass gzip bodies; `off` never compresses. In every mode the body goes
//! out uncompressed when `SYNAPS_GZIP_REQUEST_BODIES` is `0`, `false`, `off`
//! or `no`, when an earlier request in this process showed gzip is refused
//! (below), or when compressing would not make it smaller.
//!
//! Refusal fallback: a gzip request answered 400 or 415 is resent once
//! uncompressed, without spending retry budget. If that resend succeeds, the
//! encoding was the problem and gzip is turned off for the rest of the
//! process. If the resend fails too, the error was real and gzip stays on,
//! so an ordinary 400 (prompt too long, a bad image) never disables it.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use agent_core::config::{
    RequestCompressionMode, DEFAULT_REQUEST_COMPRESSION_LEVEL, MAX_REQUEST_COMPRESSION_LEVEL,
};
use bytes::Bytes;
use reqwest::RequestBuilder;

/// Bodies below this go out uncompressed: the upload saved is a few ms at
/// most, and it keeps small requests (titles, probes) byte-for-byte as before.
pub const MIN_BYTES: usize = 16 * 1024;

/// Kill switch. Off on `0`, `false`, `off`, `no`; anything else is ignored.
pub const ENV_SWITCH: &str = "SYNAPS_GZIP_REQUEST_BODIES";

const ANTHROPIC_HOST: &str = "api.anthropic.com";

/// Proxy variables reqwest honours for an `https://` URL, plus the CA bundle
/// overrides rustls-native-certs reads. Upper and lower case, as reqwest
/// and curl both accept either.
const SKIP_VARS: &[&str] = &[
    "HTTPS_PROXY",
    "https_proxy",
    "ALL_PROXY",
    "all_proxy",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Mode {
    /// gzip when every condition in the module docs holds.
    #[default]
    Auto,
    /// gzip whatever the endpoint, proxy or CA bundle.
    On,
    /// Never compress.
    Off,
    /// Compress every body, whatever the host, size or environment. Tests
    /// point requests at loopback servers, which `Auto` never compresses.
    #[cfg(test)]
    Always,
}

/// Request-body compression policy for the Anthropic transports. Lives on
/// `ApiOptions`; the default is `Auto` with the process-wide refusal latch.
#[derive(Debug, Clone)]
pub struct RequestCompression {
    mode: Mode,
    /// gzip level, 1–9.
    level: u32,
    /// Set once the API has accepted a request only uncompressed. Whether
    /// gzip survives the network path is a property of the machine, not the
    /// session, so production shares one latch across the process.
    refused: Arc<AtomicBool>,
}

fn process_latch() -> &'static Arc<AtomicBool> {
    static LATCH: OnceLock<Arc<AtomicBool>> = OnceLock::new();
    LATCH.get_or_init(|| Arc::new(AtomicBool::new(false)))
}

impl Default for RequestCompression {
    fn default() -> Self {
        Self::from_config(
            RequestCompressionMode::default(),
            DEFAULT_REQUEST_COMPRESSION_LEVEL,
        )
    }
}

impl RequestCompression {
    /// The policy for `request_compression` / `request_compression_level`,
    /// sharing the process-wide refusal latch. The level is clamped to 1–9.
    pub fn from_config(mode: RequestCompressionMode, level: u32) -> Self {
        Self {
            mode: match mode {
                RequestCompressionMode::Auto => Mode::Auto,
                RequestCompressionMode::On => Mode::On,
                RequestCompressionMode::Off => Mode::Off,
            },
            level: level.clamp(1, MAX_REQUEST_COMPRESSION_LEVEL),
            refused: Arc::clone(process_latch()),
        }
    }

    /// Compress every body, with a latch of its own. For loopback tests.
    #[cfg(test)]
    pub fn always() -> Self {
        Self {
            mode: Mode::Always,
            level: DEFAULT_REQUEST_COMPRESSION_LEVEL,
            refused: Arc::new(AtomicBool::new(false)),
        }
    }

    /// `(mode, level)` as configured; `None` for the test-only `always()`.
    #[cfg(test)]
    pub(crate) fn configured(&self) -> (Option<RequestCompressionMode>, u32) {
        let mode = match self.mode {
            Mode::Auto => Some(RequestCompressionMode::Auto),
            Mode::On => Some(RequestCompressionMode::On),
            Mode::Off => Some(RequestCompressionMode::Off),
            Mode::Always => None,
        };
        (mode, self.level)
    }

    /// True once a refusal has turned gzip off.
    pub fn refused(&self) -> bool {
        self.refused.load(Ordering::Relaxed)
    }

    fn wants_gzip(&self, url: &str, len: usize, env: impl Fn(&str) -> Option<String>) -> bool {
        if self.refused() {
            return false;
        }
        match self.mode {
            #[cfg(test)]
            Mode::Always => true,
            Mode::Off => false,
            Mode::On => len >= MIN_BYTES && !switched_off(&env),
            Mode::Auto => {
                len >= MIN_BYTES
                    && !switched_off(&env)
                    && is_direct_anthropic(url)
                    && !SKIP_VARS
                        .iter()
                        .any(|&k| env(k).is_some_and(|v| !v.trim().is_empty()))
            }
        }
    }

    /// The wire form of `raw` for a request to `url`. Compression runs on the
    /// blocking pool (about 4 ms per MB); if it fails, or would not shrink
    /// the body, the body goes out uncompressed.
    pub(super) async fn encode(&self, url: &str, raw: &Bytes) -> WireBody {
        if !self.wants_gzip(url, raw.len(), |k| std::env::var(k).ok()) {
            return WireBody::identity(raw);
        }
        let input = raw.clone();
        let level = self.level;
        let started = std::time::Instant::now();
        match tokio::task::spawn_blocking(move || gzip(&input, level)).await {
            Ok(Ok(out)) if out.len() < raw.len() => {
                tracing::debug!(
                    level,
                    raw_bytes = raw.len(),
                    wire_bytes = out.len(),
                    elapsed_us = started.elapsed().as_micros() as u64,
                    "gzip request body"
                );
                WireBody {
                    bytes: out.into(),
                    gzip: true,
                    resend: false,
                }
            }
            Ok(Ok(_)) => WireBody::identity(raw),
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "gzip request body failed; sending uncompressed");
                WireBody::identity(raw)
            }
            Err(e) => {
                tracing::warn!(error = %e, "gzip request body task failed; sending uncompressed");
                WireBody::identity(raw)
            }
        }
    }

    /// After a non-success status to an attempt sent as `wire`: the body to
    /// resend when the server may have refused the encoding. The caller
    /// resends it once without spending retry budget; `None` means handle the
    /// error as usual.
    pub(super) fn resend_uncompressed(&self, wire: &WireBody, raw: &Bytes, status: u16) -> Option<WireBody> {
        if !wire.gzip || !matches!(status, 400 | 415) {
            return None;
        }
        tracing::warn!(status, "gzip request body refused; resending it uncompressed once");
        Some(WireBody {
            bytes: raw.clone(),
            gzip: false,
            resend: true,
        })
    }

    /// Record the status of an attempt sent as `wire`. When that attempt was
    /// the uncompressed resend and it succeeded, the encoding was the problem:
    /// gzip goes off for the rest of the process.
    pub(super) fn settle(&self, wire: &mut WireBody, success: bool) {
        if !std::mem::take(&mut wire.resend) {
            return;
        }
        if success && !self.refused.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                "{ANTHROPIC_HOST} accepted a request only uncompressed; sending request bodies uncompressed for the rest of this process"
            );
        }
    }
}

/// The bytes handed to reqwest for one request, and how they are encoded.
#[derive(Debug, Clone)]
pub(super) struct WireBody {
    pub(super) bytes: Bytes,
    pub(super) gzip: bool,
    /// This is the uncompressed resend after a refusal; its outcome decides
    /// the latch (see [`RequestCompression::settle`]).
    resend: bool,
}

impl WireBody {
    pub(super) fn identity(raw: &Bytes) -> Self {
        Self {
            bytes: raw.clone(),
            gzip: false,
            resend: false,
        }
    }

    /// Adds `Content-Encoding: gzip` when the body is compressed.
    pub(super) fn encoding_header(&self, req: RequestBuilder) -> RequestBuilder {
        if self.gzip {
            req.header(reqwest::header::CONTENT_ENCODING, "gzip")
        } else {
            req
        }
    }
}

fn switched_off(env: &impl Fn(&str) -> Option<String>) -> bool {
    env(ENV_SWITCH).is_some_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        )
    })
}

fn is_direct_anthropic(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|u| {
        u.scheme() == "https"
            && u.host_str() == Some(ANTHROPIC_HOST)
            && u.port_or_known_default() == Some(443)
    })
}

fn gzip(raw: &[u8], level: u32) -> std::io::Result<Vec<u8>> {
    let mut enc = flate2::write::GzEncoder::new(
        Vec::with_capacity(raw.len() / 2),
        flate2::Compression::new(level),
    );
    enc.write_all(raw)?;
    enc.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::Read;

    const API: &str = "https://api.anthropic.com/v1/messages";

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    /// A production policy with a latch of its own.
    fn policy(mode: RequestCompressionMode, level: u32) -> RequestCompression {
        RequestCompression {
            refused: Arc::new(AtomicBool::new(false)),
            ..RequestCompression::from_config(mode, level)
        }
    }

    fn auto() -> RequestCompression {
        policy(RequestCompressionMode::Auto, DEFAULT_REQUEST_COMPRESSION_LEVEL)
    }

    fn gunzip(b: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(b).read_to_end(&mut out).unwrap();
        out
    }

    /// A body shaped like a real request: repetitive JSON well above the
    /// threshold.
    fn big_body() -> Bytes {
        let msgs: Vec<serde_json::Value> = (0..400)
            .map(|i| serde_json::json!({"role": "user", "content": [{"type": "text", "text": format!("turn {i}: cargo test --workspace output line")}]}))
            .collect();
        serde_json::to_vec(&serde_json::json!({"model": "claude-sonnet-4-6", "messages": msgs}))
            .unwrap()
            .into()
    }

    #[test]
    fn auto_compresses_large_bodies_to_the_anthropic_api_only() {
        let c = auto();
        let none = env(&[]);
        assert!(c.wants_gzip(API, MIN_BYTES, &none));
        assert!(!c.wants_gzip(API, MIN_BYTES - 1, &none), "below threshold");
        for url in [
            "http://api.anthropic.com/v1/messages",
            "https://api.anthropic.com:8443/v1/messages",
            "https://gateway.example.com/v1/messages",
            "https://api.anthropic.com.example.com/v1/messages",
            "http://127.0.0.1:4000/v1/messages",
            "not a url",
        ] {
            assert!(!c.wants_gzip(url, 1 << 20, &none), "{url}");
        }
        assert!(c.wants_gzip("https://api.anthropic.com:443/v1/messages", 1 << 20, &none));
    }

    #[test]
    fn proxies_custom_cas_and_the_switch_turn_it_off() {
        let c = auto();
        for &var in SKIP_VARS {
            assert!(!c.wants_gzip(API, 1 << 20, env(&[(var, "http://proxy:3128")])), "{var}");
        }
        // Set but empty is unset, as reqwest treats it.
        assert!(c.wants_gzip(API, 1 << 20, env(&[("HTTPS_PROXY", " ")])));
        // HTTP_PROXY never applies to an https:// URL.
        assert!(c.wants_gzip(API, 1 << 20, env(&[("HTTP_PROXY", "http://proxy:3128")])));
        for off in ["0", "false", "OFF", " no "] {
            assert!(!c.wants_gzip(API, 1 << 20, env(&[(ENV_SWITCH, off)])), "{off}");
        }
        for on in ["1", "true", "yes", ""] {
            assert!(c.wants_gzip(API, 1 << 20, env(&[(ENV_SWITCH, on)])), "{on:?}");
        }
    }

    #[test]
    fn on_skips_endpoint_proxy_and_ca_checks_but_not_the_switch() {
        let on = policy(RequestCompressionMode::On, 1);
        let none = env(&[]);
        assert!(on.wants_gzip("https://gateway.example.com/v1/messages", 1 << 20, &none));
        assert!(on.wants_gzip("http://127.0.0.1:4000/v1/messages", 1 << 20, &none));
        for &var in SKIP_VARS {
            assert!(on.wants_gzip(API, 1 << 20, env(&[(var, "x")])), "{var}");
        }
        assert!(!on.wants_gzip(API, MIN_BYTES - 1, &none), "threshold still applies");
        assert!(!on.wants_gzip(API, 1 << 20, env(&[(ENV_SWITCH, "0")])), "the env switch wins");

        let off = policy(RequestCompressionMode::Off, 9);
        assert!(!off.wants_gzip(API, 1 << 20, &none));
    }

    #[test]
    fn config_level_is_clamped_and_shares_the_process_latch() {
        let at = |level| RequestCompression::from_config(RequestCompressionMode::Auto, level).level;
        assert_eq!(at(0), 1);
        assert_eq!(at(6), 6);
        assert_eq!(at(42), MAX_REQUEST_COMPRESSION_LEVEL);
        assert_eq!(RequestCompression::default().level, DEFAULT_REQUEST_COMPRESSION_LEVEL);
        assert_eq!(RequestCompression::default().mode, Mode::Auto);
        assert!(Arc::ptr_eq(
            &RequestCompression::from_config(RequestCompressionMode::On, 9).refused,
            process_latch()
        ));
    }

    #[test]
    fn the_runtime_takes_the_policy_from_config_and_passes_it_on() {
        let mut rt = crate::runtime::Runtime::new_headless();
        assert_eq!(rt.request_compression.mode, Mode::Auto);
        assert_eq!(rt.request_compression.level, DEFAULT_REQUEST_COMPRESSION_LEVEL);
        rt.apply_config(&agent_core::config::SynapsConfig {
            request_compression: RequestCompressionMode::On,
            request_compression_level: 6,
            ..Default::default()
        });
        assert_eq!(rt.request_compression.mode, Mode::On);
        assert_eq!(rt.request_compression.level, 6);
        // Subagents are cloned runtimes: same policy, same process latch.
        let child = rt.clone();
        assert_eq!((child.request_compression.mode, child.request_compression.level), (Mode::On, 6));
        assert!(Arc::ptr_eq(&child.request_compression.refused, process_latch()));
    }

    #[tokio::test]
    async fn a_higher_level_shrinks_more() {
        // Varied text, so the levels' match search actually differs.
        const WORDS: &[&str] = &[
            "fn", "let", "mut", "self", "impl", "match", "Some", "None", "Ok", "Err", "await",
            "async", "return", "struct", "enum", "pub", "use", "crate", "error:", "warning:",
            "test", "passed", "failed", "src/runtime/api.rs", "line", "=>", "{", "}", "(", ")",
        ];
        let mut x: u64 = 0x2545_f491_4f6c_dd1d;
        let text: Vec<&str> = (0..20_000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                WORDS[(x % WORDS.len() as u64) as usize]
            })
            .collect();
        let body = serde_json::json!({"messages": [{"role": "user", "content": text.join(" ")}]});
        let raw: Bytes = serde_json::to_vec(&body).unwrap().into();
        let fast = policy(RequestCompressionMode::On, 1).encode(API, &raw).await;
        let best = policy(RequestCompressionMode::On, 9).encode(API, &raw).await;
        assert!(fast.gzip && best.gzip);
        let (best_len, fast_len) = (best.bytes.len(), fast.bytes.len());
        assert!(best_len < fast_len, "level 9 {best_len} vs level 1 {fast_len}");
        assert_eq!(gunzip(&best.bytes), raw.as_ref());
    }

    #[test]
    fn modes_and_the_latch() {
        let none = env(&[]);
        let always = RequestCompression::always();
        assert!(always.wants_gzip("http://127.0.0.1:1/v1/messages", 1, &none));
        always.refused.store(true, Ordering::Relaxed);
        assert!(!always.wants_gzip("http://127.0.0.1:1/v1/messages", 1, &none));
        // `always()` latches are independent of each other and of the process.
        assert!(!RequestCompression::always().refused());
        assert!(Arc::ptr_eq(
            &RequestCompression::default().refused,
            &RequestCompression::default().refused
        ));
    }

    #[tokio::test]
    async fn encode_round_trips_and_shrinks() {
        let raw = big_body();
        let wire = RequestCompression::always().encode(API, &raw).await;
        assert!(wire.gzip);
        assert!(wire.bytes.len() * 2 < raw.len(), "{} -> {}", raw.len(), wire.bytes.len());
        assert_eq!(gunzip(&wire.bytes), raw.as_ref());

        // Production policy, loopback URL: never compressed.
        let plain = auto().encode("http://127.0.0.1:1/v1/messages", &raw).await;
        assert!(!plain.gzip);
        assert_eq!(plain.bytes, raw);
    }

    #[tokio::test]
    async fn incompressible_bodies_go_out_as_they_are() {
        // Random bytes (what an already-compressed image looks like) do not
        // shrink, so the body goes out as it is.
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        let raw: Bytes = (0..64 * 1024)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect::<Vec<u8>>()
            .into();
        let wire = RequestCompression::always().encode(API, &raw).await;
        assert!(!wire.gzip);
        assert_eq!(wire.bytes, raw);
    }

    #[tokio::test]
    async fn refusal_latches_only_when_the_uncompressed_resend_succeeds() {
        let raw = big_body();
        for (status, success, latched) in [(400, true, true), (415, true, true), (400, false, false)] {
            let c = RequestCompression::always();
            let wire = c.encode(API, &raw).await;
            let mut resend = c.resend_uncompressed(&wire, &raw, status).expect("resend");
            assert!(!resend.gzip);
            assert_eq!(resend.bytes, raw);
            c.settle(&mut resend, success);
            assert_eq!(c.refused(), latched, "{status} success={success}");
            // Settling is one-shot: a later success on the same body is not
            // evidence about the encoding.
            c.settle(&mut resend, true);
            assert_eq!(c.refused(), latched);
        }
    }

    #[tokio::test]
    async fn no_resend_for_other_statuses_or_uncompressed_requests() {
        let raw = big_body();
        let c = RequestCompression::always();
        let wire = c.encode(API, &raw).await;
        for status in [401, 403, 404, 413, 422, 429, 500, 529] {
            assert!(c.resend_uncompressed(&wire, &raw, status).is_none(), "{status}");
        }
        let plain = WireBody::identity(&raw);
        assert!(c.resend_uncompressed(&plain, &raw, 400).is_none());
        let mut w = wire.clone();
        c.settle(&mut w, true);
        assert!(!c.refused(), "a normal success never latches");
    }
}

/// End to end through the real Anthropic transports, against a loopback
/// server that decodes gzip request bodies the way the API does.
#[cfg(test)]
mod transport_tests {
    use super::RequestCompression;
    use crate::runtime::api::{ApiMethods, ApiOptions};
    use crate::runtime::telemetry::TelemetryLevel;
    use crate::runtime::types::AuthState;
    use crate::{StreamEvent, ToolRegistry};
    use axum::body::Bytes;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::post as axum_post;
    use axum::Router;
    use serde_json::{json, Value};
    use std::io::Read;
    use std::sync::{Arc, Mutex};
    use tokio::sync::{mpsc, RwLock};

    /// Per request: the `Content-Encoding` header and the decoded JSON body.
    type Seen = Arc<Mutex<Vec<(Option<String>, Value)>>>;

    #[derive(Clone, Copy)]
    enum Server {
        Accepts,
        /// 400 to every gzip body: a middlebox mangles them, say.
        RefusesGzip,
        /// 400 to everything: a genuinely bad request.
        RefusesAll,
    }

    const SSE: &str = concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",",
        "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-6\",\"stop_reason\":null,",
        "\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );

    async fn server(mode: Server) -> (String, Seen) {
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let cap = Arc::clone(&seen);
        let app = Router::new().route(
            "/v1/messages",
            axum_post(move |headers: HeaderMap, body: Bytes| {
                let cap = Arc::clone(&cap);
                async move {
                    let encoding = headers
                        .get("content-encoding")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string);
                    let raw = match encoding.as_deref() {
                        Some("gzip") => {
                            let mut out = Vec::new();
                            flate2::read::GzDecoder::new(&body[..])
                                .read_to_end(&mut out)
                                .expect("valid gzip");
                            out
                        }
                        None => body.to_vec(),
                        Some(other) => panic!("unexpected encoding {other}"),
                    };
                    let json: Value = serde_json::from_slice(&raw).expect("JSON body");
                    let stream = json["stream"].as_bool().unwrap_or(false);
                    let refuse = match mode {
                        Server::Accepts => false,
                        Server::RefusesGzip => encoding.is_some(),
                        Server::RefusesAll => true,
                    };
                    cap.lock().unwrap().push((encoding, json));
                    if refuse {
                        return (
                            StatusCode::BAD_REQUEST,
                            r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad request"}}"#,
                        )
                            .into_response();
                    }
                    if stream {
                        (StatusCode::OK, [("content-type", "text/event-stream")], SSE)
                            .into_response()
                    } else {
                        (
                            StatusCode::OK,
                            [("content-type", "application/json")],
                            json!({
                                "content": [{"type": "text", "text": "summary"}],
                                "stop_reason": "end_turn",
                                "usage": {"input_tokens": 3, "output_tokens": 1}
                            })
                            .to_string(),
                        )
                            .into_response()
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), seen)
    }

    fn auth() -> Arc<RwLock<AuthState>> {
        Arc::new(RwLock::new(AuthState {
            auth_token: "sk-test".into(),
            auth_type: "api_key".into(),
            refresh_token: None,
            token_expires: Some(9_999_999_999_999),
            auth_file_mtime: None,
            bound_credential: None,
        }))
    }

    /// A conversation big enough to look like a real one.
    fn messages() -> Vec<crate::SharedMessage> {
        let log = "test runtime::request_encoding::tests ... ok\n".repeat(800);
        vec![Arc::new(json!({"role": "user", "content": format!("why did this pass?\n{log}")}))]
    }

    fn options(url: &str, compression: RequestCompression) -> ApiOptions {
        ApiOptions {
            anthropic_base_url: Some(url.to_string()),
            request_compression: compression,
            ..Default::default()
        }
    }

    async fn stream_turn(options: &ApiOptions) -> crate::error::Result<Value> {
        let (tx, _rx) = mpsc::unbounded_channel::<StreamEvent>();
        ApiMethods::call_api_stream(
            &auth(),
            &reqwest::Client::new(),
            "claude-sonnet-4-6",
            &ToolRegistry::new(),
            &None,
            0,
            agent_core::reasoning::ReasoningLevel::Adaptive,
            &messages(),
            tx,
            0, // max_retries: the fallback must not depend on the error budget
            0,
            options,
            TelemetryLevel::Off,
        )
        .await
    }

    fn encodings(seen: &Seen) -> Vec<Option<String>> {
        seen.lock().unwrap().iter().map(|(e, _)| e.clone()).collect()
    }

    #[tokio::test]
    async fn streaming_body_goes_out_gzip_and_decodes_to_the_identity_json() {
        let (url, seen) = server(Server::Accepts).await;
        stream_turn(&options(&url, RequestCompression::always()))
            .await
            .unwrap();
        // The production policy never compresses for a loopback URL.
        stream_turn(&options(&url, RequestCompression::default()))
            .await
            .unwrap();
        assert_eq!(encodings(&seen), [Some("gzip".to_string()), None]);
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].1, seen[1].1, "same request either way");
    }

    #[tokio::test]
    async fn refused_gzip_is_resent_uncompressed_then_stays_off() {
        let (url, seen) = server(Server::RefusesGzip).await;
        let c = RequestCompression::always();
        let opts = options(&url, c.clone());
        stream_turn(&opts)
            .await
            .expect("a gzip refusal must never fail the turn");
        assert!(c.refused(), "latched off once the plain resend went through");
        stream_turn(&opts).await.unwrap();
        assert_eq!(
            encodings(&seen),
            [Some("gzip".to_string()), None, None],
            "refused + resend + next turn"
        );
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].1, seen[1].1, "the resend is the same request");
    }

    #[tokio::test]
    async fn a_genuine_400_still_fails_and_leaves_gzip_on() {
        let (url, seen) = server(Server::RefusesAll).await;
        let c = RequestCompression::always();
        let err = stream_turn(&options(&url, c.clone())).await.unwrap_err();
        assert!(err.to_string().contains("400"), "{err}");
        assert_eq!(encodings(&seen), [Some("gzip".to_string()), None]);
        assert!(!c.refused(), "the plain resend failed too: not the encoding");
    }

    /// The window clamp is wired into all three transports: a model that
    /// rejects input + `max_tokens` over the window (Sonnet 4) gets the room
    /// its ~160k-token input leaves of 200k; a Claude 4.5+ model gets its
    /// full 64k, the same body as before the clamp existed.
    #[tokio::test]
    async fn every_transport_clamps_max_tokens_only_for_models_that_reject_overflow() {
        let near = vec![Arc::new(json!({"role": "user", "content": "x".repeat(400_000)}))
            as crate::SharedMessage];
        for (model, clamped) in [("claude-sonnet-4-20250514", true), ("claude-sonnet-4-5-20250929", false)] {
            let (url, seen) = server(Server::Accepts).await;
            let opts = options(&url, RequestCompression::default());
            let (tx, _rx) = mpsc::unbounded_channel::<StreamEvent>();
            ApiMethods::call_api_stream(
                &auth(),
                &reqwest::Client::new(),
                model,
                &ToolRegistry::new(),
                &None,
                0,
                agent_core::reasoning::ReasoningLevel::Adaptive,
                &near,
                tx,
                0,
                0,
                &opts,
                TelemetryLevel::Off,
            )
            .await
            .expect("stream");
            ApiMethods::call_api(
                &auth(),
                &reqwest::Client::new(),
                model,
                &ToolRegistry::new(),
                &None,
                0,
                agent_core::reasoning::ReasoningLevel::Adaptive,
                &near,
                0,
                &opts,
            )
            .await
            .expect("sync");
            ApiMethods::call_api_simple(
                &auth(),
                &reqwest::Client::new(),
                model,
                "summarize",
                0,
                agent_core::reasoning::ReasoningLevel::Adaptive,
                &near,
                0,
                &opts,
            )
            .await
            .expect("compaction");
            let seen = seen.lock().unwrap();
            assert_eq!(seen.len(), 3, "{model}");
            for (_, body) in seen.iter() {
                let max = body["max_tokens"].as_u64().unwrap();
                assert_eq!(body["thinking"]["budget_tokens"], 16_384, "{model}");
                if clamped {
                    // 200k − ~160k of messages − the tool schemas (none on
                    // the compaction path), above the thinking floor.
                    assert!((20_480..40_000).contains(&max), "{model}: {max}");
                } else {
                    assert_eq!(max, 64_000, "{model}");
                }
            }
        }
    }

    /// `thinking_display = omitted` reaches the streaming and the
    /// non-streaming turn; compaction (an internal summary) stays summarized.
    #[tokio::test]
    async fn thinking_display_reaches_both_turn_transports_not_compaction() {
        let (url, seen) = server(Server::Accepts).await;
        let opts = ApiOptions {
            thinking_display: crate::config::ThinkingDisplay::Omitted,
            ..options(&url, RequestCompression::default())
        };
        let level = agent_core::reasoning::ReasoningLevel::High;
        let model = "claude-opus-4-7";
        let (tx, _rx) = mpsc::unbounded_channel::<StreamEvent>();
        ApiMethods::call_api_stream(
            &auth(), &reqwest::Client::new(), model, &ToolRegistry::new(), &None, 0, level,
            &messages(), tx, 0, 0, &opts, TelemetryLevel::Off,
        )
        .await
        .expect("stream");
        ApiMethods::call_api(
            &auth(), &reqwest::Client::new(), model, &ToolRegistry::new(), &None, 0, level,
            &messages(), 0, &opts,
        )
        .await
        .expect("sync");
        ApiMethods::call_api_simple(
            &auth(), &reqwest::Client::new(), model, "summarize", 0, level, &messages(), 0, &opts,
        )
        .await
        .expect("compaction");
        let seen = seen.lock().unwrap();
        let displays: Vec<&Value> = seen.iter().map(|(_, b)| &b["thinking"]["display"]).collect();
        assert_eq!(displays, [&json!("omitted"), &json!("omitted"), &json!("summarized")]);
    }

    #[tokio::test]
    async fn sync_and_compaction_paths_compress_and_fall_back_too() {
        let (url, seen) = server(Server::RefusesGzip).await;
        let c = RequestCompression::always();
        let opts = options(&url, c.clone());
        ApiMethods::call_api(
            &auth(),
            &reqwest::Client::new(),
            "claude-sonnet-4-6",
            &ToolRegistry::new(),
            &None,
            0,
            agent_core::reasoning::ReasoningLevel::Adaptive,
            &messages(),
            0,
            &opts,
        )
        .await
        .expect("sync call survives the refusal");
        assert!(c.refused());

        let (url, seen2) = server(Server::RefusesGzip).await;
        let c = RequestCompression::always();
        let text = ApiMethods::call_api_simple(
            &auth(),
            &reqwest::Client::new(),
            "claude-haiku-4-5",
            "summarize",
            0,
            agent_core::reasoning::ReasoningLevel::Adaptive,
            &messages(),
            0,
            &options(&url, c.clone()),
        )
        .await
        .expect("compaction survives the refusal");
        assert_eq!(text, "summary");
        assert!(c.refused());
        for seen in [&seen, &seen2] {
            assert_eq!(encodings(seen), [Some("gzip".to_string()), None]);
        }
    }
}
