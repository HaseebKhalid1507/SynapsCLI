//! `synaps auth-broker` — credential broker (epic #155, task #156).
//!
//! Holds ONE OAuth credential (this machine's `auth.json`), is the SINGLE
//! refresher (Anthropic rotates the refresh token on every refresh, so exactly
//! one party may refresh), and serves short-lived **access** tokens to authorized
//! client machines.
//!
//! ## Transport security (task #156 subtask 6)
//!
//! ```text
//! bind addr    | --tls-cert/key | --insecure-http | result
//! -------------|----------------|-----------------|-------------------------------------------
//! loopback     | absent         | any             | plain HTTP, no warning
//! loopback     | both set       | any             | HTTPS (redundant but allowed)
//! non-loopback | absent         | absent          | REFUSED — must TLS or explicit opt-in
//! non-loopback | absent         | present         | plain HTTP + WireGuard guidance warning
//! non-loopback | both set       | any             | HTTPS, clean startup
//! one of pair  | one set        | any             | REFUSED — must provide both cert+key
//! ```
//!
//! TLS is served via `axum-server` (tls-rustls feature, rustls 0.23).
//!
//! Clients run `synaps` with `auth.remote_endpoint` pointed here; they fetch a
//! token per request and never store the credential on their own disk.
//!
//! Endpoints:
//!   GET  /healthz            -> { status }                 (no secret, non-200 if cred missing)
//!   GET  /token?provider=X   -> { access_token, expires }  (machine-auth, OAuth providers ONLY)
//!   POST /proxy              -> typed broker proxy         (machine-auth, static-key providers;
//!                               the key is applied broker-side and never vended)
//!   GET  /usage              -> Anthropic usage JSON       (machine-auth, typed operation; the
//!                               OAuth token is resolved broker-side and never vended)
//!   GET  /capabilities       -> provider status list       (machine-auth, no secret values)

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use std::sync::Arc;

use axum::{
    extract::{ConnectInfo, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::json;
use synaps_cli::auth;

/// OAuth providers whose descriptors permit access-token vending. Static-key
/// strategies are intentionally not served by this endpoint.
fn broker_provider(value: &str) -> Option<auth::OAuthProviderId> {
    let id: auth::OAuthProviderId = value.try_into().ok()?;
    let descriptor = auth::provider::registry().get(id).copied()?;
    (descriptor.broker_strategy == auth::BrokerCredentialStrategy::OAuthAccessToken).then_some(id)
}

// ── TLS config types ─────────────────────────────────────────────────────────

/// Validated, ready-to-use TLS material (PEM bytes confirmed parseable).
#[derive(Clone, Debug)]
pub struct TlsConfig {
    pub cert_pem: Vec<u8>,
    pub key_pem: Vec<u8>,
}

/// Outcome of [`check_bind_policy`] — what transport mode to use or why to abort.
#[derive(Debug, PartialEq, Eq)]
pub enum BindDecision {
    /// Plain HTTP is allowed (loopback or explicit opt-in on non-loopback).
    Http {
        /// Print the WireGuard guidance warning (non-loopback + --insecure-http).
        warn: bool,
    },
    /// Serve TLS.
    Tls,
    /// Refuse to start; contains the error message.
    Refuse(String),
}

// ── Bind-policy logic (pure — trivial to unit test) ──────────────────────────

/// Pure function — no I/O. Determines whether to serve HTTP, HTTPS, or refuse.
///
/// `is_loopback` — true if the bind address is 127.x or ::1  
/// `has_tls`     — both --tls-cert and --tls-key were provided  
/// `insecure_http` — --insecure-http flag was passed
pub fn check_bind_policy(is_loopback: bool, has_tls: bool, insecure_http: bool) -> BindDecision {
    if has_tls {
        return BindDecision::Tls;
    }
    if is_loopback {
        return BindDecision::Http { warn: false };
    }
    // non-loopback, no TLS
    if insecure_http {
        return BindDecision::Http { warn: true };
    }
    BindDecision::Refuse(
        "refusing to start: non-loopback bind without TLS. \
         Pass --tls-cert + --tls-key to enable HTTPS (recommended), \
         or --insecure-http to acknowledge you are running behind WireGuard \
         / a private network and accept plaintext."
            .to_string(),
    )
}

// ── PEM validation ────────────────────────────────────────────────────────────

/// Read and validate a cert+key PEM pair. Fails fast with a human-readable error
/// so the broker never starts with a broken TLS config.
pub fn load_and_validate_tls(cert_path: &PathBuf, key_path: &PathBuf) -> anyhow::Result<TlsConfig> {
    // Read files first — surface "file not found" before doing any parsing.
    let cert_pem = std::fs::read(cert_path)
        .map_err(|e| anyhow::anyhow!("--tls-cert '{}': {e}", cert_path.display()))?;
    let key_pem = std::fs::read(key_path)
        .map_err(|e| anyhow::anyhow!("--tls-key '{}': {e}", key_path.display()))?;

    // Parse to catch garbage PEM / wrong file type before binding.
    validate_pem_pair(&cert_pem, &key_pem)?;

    Ok(TlsConfig { cert_pem, key_pem })
}

/// Parse the PEM bytes — returns a readable error if either is malformed.
/// Uses `rustls-pki-types` (same parser axum-server uses internally).
/// Separated for unit-testability with in-memory bytes.
pub fn validate_pem_pair(cert_pem: &[u8], key_pem: &[u8]) -> anyhow::Result<()> {
    use rustls_pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};

    // Validate cert chain — must have at least one cert.
    let certs: Vec<CertificateDer<'_>> = CertificateDer::pem_slice_iter(cert_pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow::anyhow!("TLS cert PEM is malformed: {e}"))?;
    if certs.is_empty() {
        anyhow::bail!("TLS cert PEM contains no certificates (empty or wrong format)");
    }

    // Validate private key — must parse as one of the known key types.
    let keys: Vec<PrivateKeyDer<'_>> = PrivateKeyDer::pem_slice_iter(key_pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow::anyhow!("TLS key PEM is malformed: {e}"))?;
    if keys.is_empty() {
        anyhow::bail!("TLS key PEM contains no private key (empty or wrong format)");
    }
    if keys.len() > 1 {
        anyhow::bail!("TLS key PEM contains multiple private keys; it must contain exactly one");
    }

    Ok(())
}

// ── Broker internal state ─────────────────────────────────────────────────────

#[derive(Clone)]
struct BrokerState {
    /// Token clients must present as `Authorization: Bearer <token>`. `None`
    /// disables auth (only safe on loopback or with explicit `--insecure-no-auth`).
    machine_token: Option<String>,
    /// HTTP client used for the (central, single) token refresh to the provider.
    client: reqwest::Client,
    /// The in-process broker that owns static keys and executes proxied
    /// requests. Raw keys never leave this boundary.
    local: Arc<auth::LocalBroker>,
}

/// Constant-time machine-auth check shared by every credential endpoint.
fn machine_auth_ok(st: &BrokerState, headers: &HeaderMap) -> bool {
    let Some(ref expected) = st.machine_token else {
        return true; // auth disabled (loopback / explicit insecure opt-in)
    };
    let got = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let expected_header = format!("Bearer {expected}");
    ct_eq(got.as_bytes(), expected_header.as_bytes())
}

#[derive(Deserialize)]
struct TokenQuery {
    provider: Option<String>,
}

/// Constant-time byte comparison — avoids a timing oracle on the machine token.
/// (#158 B1 / CWE-208.) Length mismatch short-circuits; token length is not the
/// secret. Equal-length inputs are compared in constant time.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
pub async fn run(
    bind: String,
    machine_token: Option<String>,
    machine_token_file: Option<PathBuf>,
    insecure_no_auth: bool,
    tls_cert: Option<PathBuf>,
    tls_key: Option<PathBuf>,
    insecure_http: bool,
) -> anyhow::Result<()> {
    // ── TLS arg validation: must supply both or neither ──
    let tls = match (tls_cert, tls_key) {
        (Some(cert), Some(key)) => {
            let cfg = load_and_validate_tls(&cert, &key)?;
            Some(cfg)
        }
        (Some(_), None) => anyhow::bail!(
            "--tls-cert was given but --tls-key is missing. Provide both to enable TLS."
        ),
        (None, Some(_)) => anyhow::bail!(
            "--tls-key was given but --tls-cert is missing. Provide both to enable TLS."
        ),
        (None, None) => None,
    };

    // B3: token precedence — flag > file (read once, not in argv) > env.
    let machine_token = match machine_token {
        Some(t) => Some(t),
        None => match machine_token_file {
            Some(ref path) => Some(
                std::fs::read_to_string(path)
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "could not read --machine-token-file {}: {e}",
                            path.display()
                        )
                    })?
                    .trim()
                    .to_string(),
            ),
            None => std::env::var("SYNAPS_BROKER_TOKEN").ok(),
        },
    }
    .filter(|s| !s.trim().is_empty());

    let addr: SocketAddr = bind
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid --bind '{}': {}", bind, e))?;

    // ── Bind policy ──────────────────────────────────────────────────────────
    let decision = check_bind_policy(addr.ip().is_loopback(), tls.is_some(), insecure_http);
    match &decision {
        BindDecision::Refuse(msg) => anyhow::bail!("{}", msg),
        BindDecision::Http { warn: true } => {
            eprintln!(
                "  ⚠  non-loopback plain-HTTP bind on {addr}.\n\
                 \n\
                 You have acknowledged this with --insecure-http. Ensure this broker is\n\
                 ONLY reachable via WireGuard or another encrypted private network layer.\n\
                 TLS in-process is available: pass --tls-cert <path> --tls-key <path>.\n"
            );
        }
        BindDecision::Http { warn: false } | BindDecision::Tls => {}
    }

    // B2: refuse to start unauthenticated on a non-loopback bind unless the
    // operator explicitly opts in.
    if machine_token.is_none() && !addr.ip().is_loopback() && !insecure_no_auth {
        anyhow::bail!(
            "refusing to start: no machine token (set --machine-token / --machine-token-file / \
             SYNAPS_BROKER_TOKEN) while bound to non-loopback {addr}. That would serve credentials \
             unauthenticated to the network. Pass --insecure-no-auth to override, or bind 127.0.0.1."
        );
    }

    // D1: fail fast if the credential isn't present/readable.
    match auth::load_auth() {
        Ok(Some(_)) => {}
        Ok(None) => anyhow::bail!(
            "no credential at {}. Run `synaps login` on the broker host first.",
            auth::auth_file_path().display()
        ),
        Err(e) => anyhow::bail!("credential unreadable/corrupt: {e}"),
    }

    let client = reqwest::Client::builder()
        .tls_built_in_webpki_certs(true)
        .tls_built_in_native_certs(true)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .build()?;

    let state = BrokerState {
        machine_token: machine_token.clone(),
        client: client.clone(),
        local: Arc::new(auth::LocalBroker::new(client.clone())),
    };

    // Proactive refresh, SUPERVISED.
    {
        let refresh_client = client;
        let handle = tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            loop {
                tick.tick().await;
                if let Err(e) = auth::ensure_fresh_token(&refresh_client).await {
                    eprintln!("[auth-broker] proactive refresh failed: {e}");
                }
            }
        });
        tokio::spawn(async move {
            if let Err(e) = handle.await {
                eprintln!("[auth-broker] FATAL: proactive refresh task died: {e:?} — tokens will go stale; restart the broker.");
            }
        });
    }

    let app = build_router(state);

    match decision {
        BindDecision::Tls => {
            eprintln!(
                "synaps auth-broker listening on https://{addr}  (machine auth: {}, TLS: ON)",
                if machine_token.is_some() {
                    "ON"
                } else {
                    "OFF — loopback/insecure only!"
                }
            );
            let tls_cfg = tls.expect("Tls decision requires tls config");
            // rustls 0.23 requires an explicit CryptoProvider when multiple backends
            // are compiled in; install ring as the default (idempotent — Err means already set).
            let _ = rustls::crypto::ring::default_provider().install_default();
            let rustls_config =
                axum_server::tls_rustls::RustlsConfig::from_pem(tls_cfg.cert_pem, tls_cfg.key_pem)
                    .await
                    .map_err(|e| anyhow::anyhow!("failed to build TLS config from PEM: {e}"))?;

            // Handle<SocketAddr> — matches addr type for bind_rustls.
            let handle = axum_server::Handle::<SocketAddr>::new();
            let h2 = handle.clone();
            tokio::spawn(async move {
                shutdown_signal().await;
                eprintln!("[auth-broker] shutting down (draining in-flight requests)…");
                h2.graceful_shutdown(Some(Duration::from_secs(30)));
            });

            axum_server::bind_rustls(addr, rustls_config)
                .handle(handle)
                .serve(app.into_make_service_with_connect_info::<SocketAddr>())
                .await
                .map_err(|e| anyhow::anyhow!("axum-server TLS serve error: {e}"))?;
        }
        BindDecision::Http { .. } => {
            eprintln!(
                "synaps auth-broker listening on http://{addr}  (machine auth: {})",
                if machine_token.is_some() {
                    "ON"
                } else {
                    "OFF — loopback/insecure only!"
                }
            );
            let listener = tokio::net::TcpListener::bind(addr).await?;
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(shutdown_signal())
            .await?;
        }
        BindDecision::Refuse(_) => unreachable!("Refuse is handled above"),
    }

    Ok(())
}

/// Build the shared axum Router. Extracted so tests can reuse it.
fn build_router(state: BrokerState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/token", get(token))
        .route("/proxy", post(proxy))
        .route("/usage", get(usage))
        .route("/capabilities", get(capabilities))
        // Bound inbound request bodies to the broker's typed proxy limit.
        .layer(axum::extract::DefaultBodyLimit::max(
            auth::MAX_PROXY_REQUEST_BYTES,
        ))
        // B7: cap concurrent in-flight requests.
        .layer(tower::limit::ConcurrencyLimitLayer::new(64))
        .with_state(state)
}

/// Resolve on Ctrl-C or SIGTERM so the broker drains cleanly.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// Liveness + credential readiness. Returns non-200 when the credential is
/// missing/corrupt so monitors actually catch it.
async fn healthz() -> impl IntoResponse {
    match auth::load_auth() {
        Ok(Some(_)) => (StatusCode::OK, Json(json!({ "status": "ok" }))),
        Ok(None) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "status": "no_credential" })),
        ),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "status": "error" })),
        ),
    }
}

/// Issue a current access token for `provider` (default: anthropic, allowlisted).
async fn token(
    State(st): State<BrokerState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<TokenQuery>,
) -> impl IntoResponse {
    // ── machine auth (constant-time) ──
    if !machine_auth_ok(&st, &headers) {
        eprintln!("[auth-broker] DENIED from {} (bad machine auth)", peer.ip());
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "bad machine auth" })),
        )
            .into_response();
    }

    // ── provider allowlist ──
    let provider = q.provider.unwrap_or_else(|| "anthropic".to_string());
    let Some(provider_id) = broker_provider(&provider) else {
        eprintln!(
            "[auth-broker] DENIED from {} (provider not allowed, {} chars)",
            peer.ip(),
            provider.len()
        );
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "unknown provider" })),
        )
            .into_response();
    };

    let creds = if provider_id == auth::OAuthProviderId::Anthropic {
        auth::ensure_fresh_token(&st.client).await
    } else {
        auth::ensure_fresh_provider_token(&st.client, provider_id).await
    };

    match creds {
        Ok(c) => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let ttl_ms = c.expires.saturating_sub(now);
            eprintln!(
                "[auth-broker] issued {provider} token to {} (expires {})",
                peer.ip(),
                c.expires
            );
            (
                StatusCode::OK,
                Json(json!({ "access_token": c.access, "expires": c.expires, "ttl_ms": ttl_ms })),
            )
                .into_response()
        }
        Err(e) => {
            eprintln!(
                "[auth-broker] refresh failed for {provider} from {}: {}",
                peer.ip(),
                e
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "token refresh failed" })),
            )
                .into_response()
        }
    }
}

/// Typed broker proxy: execute a static-key provider request broker-side.
///
/// OAuth providers are structurally excluded (`ProxyRequest::validate`), so
/// this endpoint can never become a second token-vending path, and no raw
/// static key ever appears in a response — the broker attaches it upstream.
async fn proxy(
    State(st): State<BrokerState>,
    headers: HeaderMap,
    Json(request): Json<auth::ProxyRequest>,
) -> axum::response::Response {
    if !machine_auth_ok(&st, &headers) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "bad machine auth" })),
        )
            .into_response();
    }
    if let Err(e) = request.validate() {
        // BrokerError Display is secret-free by contract.
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response();
    }
    use synaps_cli::auth::CredentialBroker;
    if request.stream {
        match st.local.proxy_stream(request).await {
            Ok(stream) => {
                let body = axum::body::Body::from_stream(
                    stream.map(|chunk| chunk.map_err(|e| std::io::Error::other(e.to_string()))),
                );
                (
                    StatusCode::OK,
                    [("content-type", "text/event-stream")],
                    body,
                )
                    .into_response()
            }
            Err(e) => broker_error_response(e),
        }
    } else {
        match st.local.proxy(request).await {
            Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
            Err(e) => broker_error_response(e),
        }
    }
}

/// Typed usage operation: the local broker resolves the Anthropic OAuth token
/// behind the boundary and calls the pinned usage URL; clients receive usage
/// JSON only. No token, refresh token, or URL choice ever crosses this
/// endpoint — it is deliberately not a general OAuth proxy.
async fn usage(State(st): State<BrokerState>, headers: HeaderMap) -> axum::response::Response {
    if !machine_auth_ok(&st, &headers) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "bad machine auth" })),
        )
            .into_response();
    }
    use synaps_cli::auth::CredentialBroker;
    match st.local.anthropic_usage().await {
        Ok(value) => (StatusCode::OK, Json(value)).into_response(),
        Err(e) => broker_error_response(e),
    }
}

/// Non-secret provider capability/status list.
async fn capabilities(
    State(st): State<BrokerState>,
    headers: HeaderMap,
) -> axum::response::Response {
    if !machine_auth_ok(&st, &headers) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "bad machine auth" })),
        )
            .into_response();
    }
    use synaps_cli::auth::CredentialBroker;
    match st.local.capabilities().await {
        Ok(caps) => (StatusCode::OK, Json(caps)).into_response(),
        Err(e) => broker_error_response(e),
    }
}

/// Map a broker error to an HTTP response. Messages are secret-free.
fn broker_error_response(e: auth::BrokerError) -> axum::response::Response {
    let status = match e {
        auth::BrokerError::UnknownProvider(_) | auth::BrokerError::Denied(_) => {
            StatusCode::BAD_REQUEST
        }
        auth::BrokerError::NotConfigured(_) => StatusCode::FORBIDDEN,
        auth::BrokerError::Unauthorized => StatusCode::UNAUTHORIZED,
        _ => StatusCode::BAD_GATEWAY,
    };
    (status, Json(json!({ "error": e.to_string() }))).into_response()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // ── ct_eq (preserved) ────────────────────────────────────────────────────

    #[test]
    fn ct_eq_matches_only_identical() {
        assert!(ct_eq(b"Bearer secret", b"Bearer secret"));
        assert!(!ct_eq(b"Bearer secret", b"Bearer secreT"));
        assert!(!ct_eq(b"Bearer secret", b"Bearer wrong"));
        assert!(!ct_eq(b"short", b"longer-value"));
        assert!(!ct_eq(b"", b"x"));
        assert!(ct_eq(b"", b""));
    }

    // ── Bind-policy matrix (pure fn, no I/O) ─────────────────────────────────

    #[test]
    fn policy_loopback_no_tls_is_plain_http() {
        assert_eq!(
            check_bind_policy(true, false, false),
            BindDecision::Http { warn: false }
        );
    }

    #[test]
    fn policy_loopback_insecure_flag_is_still_plain_http_no_warn() {
        // loopback + insecure flag → still HTTP, no warning (flag is irrelevant on loopback)
        assert_eq!(
            check_bind_policy(true, false, true),
            BindDecision::Http { warn: false }
        );
    }

    #[test]
    fn policy_loopback_with_tls_is_https() {
        assert_eq!(check_bind_policy(true, true, false), BindDecision::Tls);
    }

    #[test]
    fn policy_nonloopback_no_tls_no_flag_refuses() {
        let d = check_bind_policy(false, false, false);
        assert!(matches!(d, BindDecision::Refuse(_)));
        if let BindDecision::Refuse(msg) = d {
            assert!(
                msg.contains("non-loopback"),
                "message should mention non-loopback: {msg}"
            );
            assert!(
                msg.contains("--tls-cert"),
                "message should mention --tls-cert: {msg}"
            );
            assert!(
                msg.contains("--insecure-http"),
                "message should mention --insecure-http: {msg}"
            );
        }
    }

    #[test]
    fn policy_nonloopback_no_tls_insecure_flag_warns() {
        assert_eq!(
            check_bind_policy(false, false, true),
            BindDecision::Http { warn: true }
        );
    }

    #[test]
    fn policy_nonloopback_with_tls_is_https() {
        assert_eq!(check_bind_policy(false, true, false), BindDecision::Tls);
    }

    #[test]
    fn policy_nonloopback_tls_and_insecure_flag_still_tls() {
        // TLS wins — insecure-http is irrelevant when TLS is configured.
        assert_eq!(check_bind_policy(false, true, true), BindDecision::Tls);
    }

    // ── PEM validation unit tests ─────────────────────────────────────────────

    #[test]
    fn validate_garbage_pem_cert_returns_error() {
        let garbage = b"this is not a pem file at all";
        let valid_key = generate_test_cert_key().1;
        let err = validate_pem_pair(garbage, &valid_key);
        assert!(err.is_err(), "garbage cert PEM should fail validation");
        let msg = err.unwrap_err().to_string();
        assert!(
            msg.contains("cert") || msg.contains("no certificates"),
            "error should mention cert issue: {msg}"
        );
    }

    #[test]
    fn validate_garbage_pem_key_returns_error() {
        let (valid_cert, _) = generate_test_cert_key();
        let garbage_key = b"this is definitely not a private key";
        let err = validate_pem_pair(&valid_cert, garbage_key);
        assert!(err.is_err(), "garbage key PEM should fail validation");
        let msg = err.unwrap_err().to_string();
        assert!(
            msg.contains("key") || msg.contains("no private key"),
            "error should mention key issue: {msg}"
        );
    }

    #[test]
    fn validate_valid_self_signed_pair_succeeds() {
        let (cert_pem, key_pem) = generate_test_cert_key();
        validate_pem_pair(&cert_pem, &key_pem).expect("self-signed pair should validate");
    }

    #[test]
    fn validate_cert_pem_used_as_key_returns_error() {
        // Providing cert PEM where key is expected → should fail.
        let (cert_pem, _) = generate_test_cert_key();
        let err = validate_pem_pair(&cert_pem, &cert_pem);
        assert!(err.is_err(), "cert-as-key should fail validation");
    }

    // ── load_and_validate_tls file-path tests ─────────────────────────────────

    #[test]
    fn load_tls_missing_cert_file_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let cert_path = dir.path().join("nonexistent-cert.pem");
        let key_path = dir.path().join("nonexistent-key.pem");
        // Key exists but cert doesn't
        let (_, key_pem) = generate_test_cert_key();
        std::fs::write(&key_path, &key_pem).unwrap();
        let err = load_and_validate_tls(&cert_path, &key_path);
        assert!(err.is_err());
        let msg = err.unwrap_err().to_string();
        assert!(
            msg.contains("--tls-cert"),
            "error should mention --tls-cert flag: {msg}"
        );
    }

    #[test]
    fn load_tls_missing_key_file_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("nonexistent-key.pem");
        let (cert_pem, _) = generate_test_cert_key();
        std::fs::write(&cert_path, &cert_pem).unwrap();
        let err = load_and_validate_tls(&cert_path, &key_path);
        assert!(err.is_err());
        let msg = err.unwrap_err().to_string();
        assert!(
            msg.contains("--tls-key"),
            "error should mention --tls-key flag: {msg}"
        );
    }

    #[test]
    fn load_tls_garbage_files_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");
        std::fs::write(&cert_path, b"not a cert").unwrap();
        std::fs::write(&key_path, b"not a key").unwrap();
        let err = load_and_validate_tls(&cert_path, &key_path);
        assert!(err.is_err(), "garbage files should fail validation");
    }

    #[test]
    fn load_tls_valid_pair_from_tempdir() {
        let dir = tempfile::tempdir().unwrap();
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");
        let (cert_pem, key_pem) = generate_test_cert_key();
        std::fs::write(&cert_path, &cert_pem).unwrap();
        std::fs::write(&key_path, &key_pem).unwrap();
        load_and_validate_tls(&cert_path, &key_path).expect("valid pair from files should succeed");
    }

    // ── TLS integration tests (axum-server + reqwest over real TLS) ───────────
    //
    // Spin up a real HTTPS listener on 127.0.0.1:<ephemeral>, hit it with
    // reqwest (danger_accept_invalid_certs for the self-signed test CA),
    // then gracefully shut down.

    /// Build a minimal HTTPS router for integration tests (no auth::load_auth dependency).
    fn build_test_router(machine_token: Option<String>) -> Router {
        let tok = machine_token.clone();
        Router::new()
            .route(
                "/healthz",
                get(|| async { Json(json!({ "status": "ok" })) }),
            )
            .route(
                "/token",
                get(move |headers: HeaderMap| {
                    let tok = tok.clone();
                    async move {
                        if let Some(ref expected) = tok {
                            let got = headers
                                .get("authorization")
                                .and_then(|v| v.to_str().ok())
                                .unwrap_or("");
                            let want = format!("Bearer {expected}");
                            if !ct_eq(got.as_bytes(), want.as_bytes()) {
                                return (
                                    StatusCode::UNAUTHORIZED,
                                    Json(json!({ "error": "bad machine auth" })),
                                );
                            }
                        }
                        (
                            StatusCode::OK,
                            Json(json!({
                                "access_token": "test-tok",
                                "expires": 9_999_999_999u64,
                                "ttl_ms": 999_999u64
                            })),
                        )
                    }
                }),
            )
    }

    /// Spin up a real TLS listener on a random loopback port; return addr + handle.
    async fn spawn_tls_server(
        machine_token: Option<String>,
    ) -> (SocketAddr, axum_server::Handle<SocketAddr>) {
        // rustls 0.23 requires an explicit CryptoProvider when multiple backends
        // are compiled in (both aws-lc-rs and ring are in the dep tree here).
        // install_default() is idempotent — the Err case just means it was already set.
        let _ = rustls::crypto::ring::default_provider().install_default();

        let (cert_pem, key_pem) = generate_test_cert_key();
        let rustls_config = axum_server::tls_rustls::RustlsConfig::from_pem(cert_pem, key_pem)
            .await
            .expect("test TLS config should build");

        let app = build_test_router(machine_token);

        // Port 0 → let the OS assign an ephemeral port. Keep the listener alive
        // and pass it directly to from_tcp_rustls so there's no TOCTOU window
        // between get-addr and re-bind.
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        std_listener.set_nonblocking(true).unwrap();
        let addr = std_listener.local_addr().unwrap();

        let handle = axum_server::Handle::<SocketAddr>::new();
        let h2 = handle.clone();
        tokio::spawn(async move {
            axum_server::tls_rustls::from_tcp_rustls(std_listener, rustls_config)
                .expect("from_tcp_rustls should succeed")
                .handle(h2)
                .serve(app.into_make_service())
                .await
                .ok();
        });
        // Give it a tick to bind and start accepting.
        tokio::time::sleep(Duration::from_millis(80)).await;
        (addr, handle)
    }

    /// reqwest client that accepts self-signed certs (test-only, never ship to prod).
    fn insecure_client() -> reqwest::Client {
        reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .expect("insecure reqwest client should build")
    }

    #[tokio::test]
    async fn tls_healthz_returns_200() {
        let (addr, handle) = spawn_tls_server(None).await;
        let url = format!("https://{addr}/healthz");
        let resp = insecure_client()
            .get(&url)
            .send()
            .await
            .expect("GET /healthz over TLS");
        assert_eq!(resp.status(), 200, "/healthz should return 200 over TLS");
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["status"], "ok");
        handle.graceful_shutdown(Some(Duration::from_millis(200)));
    }

    #[tokio::test]
    async fn tls_token_no_auth_returns_401() {
        let (addr, handle) = spawn_tls_server(Some("secret123".to_string())).await;
        let url = format!("https://{addr}/token");
        let resp = insecure_client()
            .get(&url)
            .send()
            .await
            .expect("GET /token no auth");
        assert_eq!(
            resp.status(),
            401,
            "/token without bearer should 401 over TLS"
        );
        handle.graceful_shutdown(Some(Duration::from_millis(200)));
    }

    #[tokio::test]
    async fn tls_token_with_correct_auth_returns_200() {
        let (addr, handle) = spawn_tls_server(Some("my-machine-token".to_string())).await;
        let url = format!("https://{addr}/token");
        let resp = insecure_client()
            .get(&url)
            .header("Authorization", "Bearer my-machine-token")
            .send()
            .await
            .expect("GET /token with correct auth");
        assert_eq!(
            resp.status(),
            200,
            "/token with correct bearer should 200 over TLS"
        );
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["access_token"], "test-tok");
        handle.graceful_shutdown(Some(Duration::from_millis(200)));
    }

    #[tokio::test]
    async fn tls_token_wrong_auth_returns_401() {
        let (addr, handle) = spawn_tls_server(Some("my-machine-token".to_string())).await;
        let url = format!("https://{addr}/token");
        let resp = insecure_client()
            .get(&url)
            .header("Authorization", "Bearer wrong-token")
            .send()
            .await
            .expect("GET /token with wrong auth");
        assert_eq!(
            resp.status(),
            401,
            "/token with wrong bearer should 401 over TLS"
        );
        handle.graceful_shutdown(Some(Duration::from_millis(200)));
    }

    // ── Broker service: /proxy, /capabilities, /token isolation ─────────────

    /// Spawn the REAL router (real handlers/state) on an ephemeral loopback
    /// port. `local_upstream` pins the `local` provider at a fake endpoint so
    /// no real provider or credential file is touched.
    async fn spawn_broker_service(
        machine_token: Option<String>,
        local_upstream: Option<String>,
    ) -> String {
        let client = reqwest::Client::new();
        let local = match local_upstream {
            Some(url) => auth::LocalBroker::with_local_base_url(client.clone(), url),
            None => auth::LocalBroker::new(client.clone()),
        };
        let state = BrokerState {
            machine_token,
            client,
            local: Arc::new(local),
        };
        let app = build_router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        format!("http://{addr}")
    }

    /// Fake OpenAI-compatible upstream that records the Authorization header
    /// it receives and serves an SSE body.
    async fn spawn_sse_upstream(seen_auth: Arc<std::sync::Mutex<String>>) -> String {
        use axum::routing::post as axum_post;
        let app = Router::new().route(
            "/chat/completions",
            axum_post(move |headers: HeaderMap| {
                let seen = seen_auth.clone();
                async move {
                    *seen.lock().unwrap() = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    (
                        [("content-type", "text/event-stream")],
                        "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n",
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    fn proxy_body(stream: bool) -> serde_json::Value {
        json!({
            "provider": "local",
            "method": "post",
            "path": "/chat/completions",
            "body": {"model": "m", "messages": []},
            "stream": stream,
        })
    }

    /// Proxy authorization: no bearer and a wrong bearer are both 401, and the
    /// denial body contains no credential material.
    #[tokio::test]
    async fn proxy_requires_machine_auth() {
        let base = spawn_broker_service(Some("machine-secret".into()), None).await;
        let client = reqwest::Client::new();

        let resp = client
            .post(format!("{base}/proxy"))
            .json(&proxy_body(false))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401, "missing bearer must be denied");

        let resp = client
            .post(format!("{base}/proxy"))
            .bearer_auth("wrong-token")
            .json(&proxy_body(false))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401, "wrong bearer must be denied");
        let body = resp.text().await.unwrap();
        assert!(
            !body.contains("machine-secret"),
            "denial must not echo the expected token"
        );
    }

    /// The typed /usage operation is a credential endpoint: it requires
    /// machine auth, and denials carry no token material.
    #[tokio::test]
    async fn usage_requires_machine_auth_and_denials_are_secret_free() {
        let base = spawn_broker_service(Some("machine-secret".into()), None).await;
        let client = reqwest::Client::new();

        let resp = client.get(format!("{base}/usage")).send().await.unwrap();
        assert_eq!(resp.status(), 401, "missing bearer must be denied");

        let resp = client
            .get(format!("{base}/usage"))
            .bearer_auth("wrong-token")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401, "wrong bearer must be denied");
        let body = resp.text().await.unwrap();
        assert!(
            !body.contains("machine-secret") && !body.contains("access"),
            "denial must not echo credentials: {body}"
        );
    }

    /// Capabilities also require machine auth (they reveal configured-ness).
    #[tokio::test]
    async fn capabilities_require_machine_auth_and_expose_no_secrets() {
        let base = spawn_broker_service(Some("machine-secret".into()), None).await;
        let client = reqwest::Client::new();

        let resp = client
            .get(format!("{base}/capabilities"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);

        let resp = client
            .get(format!("{base}/capabilities"))
            .bearer_auth("machine-secret")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let caps: serde_json::Value = resp.json().await.unwrap();
        for row in caps.as_array().expect("array") {
            let obj = row.as_object().unwrap();
            let mut fields: Vec<&str> = obj.keys().map(String::as_str).collect();
            fields.sort_unstable();
            assert_eq!(
                fields,
                vec!["configured", "key", "kind", "name"],
                "capability rows carry status only — no credential fields"
            );
            assert!(obj["configured"].is_boolean());
        }
    }

    /// Remote non-disclosure / cross-provider isolation: the /token endpoint
    /// refuses to vend anything for a static-key provider — static keys are
    /// proxy-only and never leave the broker.
    #[tokio::test]
    async fn token_endpoint_denies_static_key_providers() {
        let base = spawn_broker_service(Some("machine-secret".into()), None).await;
        let client = reqwest::Client::new();
        for provider in ["groq", "openrouter", "local", "definitely-unknown"] {
            let resp = client
                .get(format!("{base}/token"))
                .query(&[("provider", provider)])
                .bearer_auth("machine-secret")
                .send()
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                400,
                "static/unknown provider '{provider}' must never be vended a token"
            );
            let body = resp.text().await.unwrap();
            assert!(
                !body.contains("access_token"),
                "no token material for {provider}"
            );
        }
    }

    /// Proxy validation fails closed: OAuth providers and absolute/escaping
    /// paths are rejected before any upstream contact.
    #[tokio::test]
    async fn proxy_rejects_oauth_providers_and_bad_paths() {
        let base = spawn_broker_service(Some("machine-secret".into()), None).await;
        let client = reqwest::Client::new();

        for (provider, path) in [
            ("anthropic", "/v1/messages"),
            ("openai-codex", "/responses"),
            ("local", "https://evil.example/steal"),
            ("local", "/../../etc/passwd"),
        ] {
            let resp = client
                .post(format!("{base}/proxy"))
                .bearer_auth("machine-secret")
                .json(&json!({
                    "provider": provider,
                    "method": "post",
                    "path": path,
                    "stream": false,
                }))
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 400, "{provider} {path} must be rejected");
        }
    }

    /// Streaming forwarding: an authenticated /proxy stream call reaches the
    /// upstream with the BROKER-applied credential and the SSE bytes flow back
    /// to the client unmodified. The client never supplied a provider key.
    #[tokio::test]
    async fn proxy_streams_sse_and_applies_key_broker_side() {
        let seen_auth = Arc::new(std::sync::Mutex::new(String::new()));
        let upstream = spawn_sse_upstream(seen_auth.clone()).await;
        let base = spawn_broker_service(Some("machine-secret".into()), Some(upstream)).await;
        let client = reqwest::Client::new();

        let resp = client
            .post(format!("{base}/proxy"))
            .bearer_auth("machine-secret")
            .json(&proxy_body(true))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("text/event-stream")
        );
        let body = resp.text().await.unwrap();
        assert!(
            body.contains("data: {\"choices\""),
            "SSE payload forwarded: {body}"
        );
        assert!(body.contains("[DONE]"));
        // The upstream saw the broker-owned credential, not the machine token.
        assert_eq!(&*seen_auth.lock().unwrap(), "Bearer local");
    }

    /// Non-streaming proxy returns the JSON envelope RemoteBroker expects.
    #[tokio::test]
    async fn proxy_non_streaming_returns_status_envelope() {
        let seen_auth = Arc::new(std::sync::Mutex::new(String::new()));
        let upstream = spawn_sse_upstream(seen_auth.clone()).await;
        let base = spawn_broker_service(None, Some(upstream)).await;
        let client = reqwest::Client::new();
        let resp = client
            .post(format!("{base}/proxy"))
            .json(&proxy_body(false))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let envelope: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(envelope["status"], 200);
        assert!(envelope["body"].as_str().unwrap().contains("[DONE]"));
    }

    // ── Helper: rcgen self-signed cert ────────────────────────────────────────

    /// Mint a self-signed cert + PKCS#8 key pair for localhost.
    /// Returns (cert_pem_bytes, key_pem_bytes).
    pub(super) fn generate_test_cert_key() -> (Vec<u8>, Vec<u8>) {
        use rcgen::generate_simple_self_signed;
        let san = vec!["localhost".to_string(), "127.0.0.1".to_string()];
        let cert = generate_simple_self_signed(san).expect("rcgen cert generation should succeed");
        let cert_pem = cert.cert.pem().into_bytes();
        let key_pem = cert.signing_key.serialize_pem().into_bytes();
        (cert_pem, key_pem)
    }
}
