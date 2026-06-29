//! `synaps auth-broker` — credential broker (epic #155, task #156).
//!
//! Holds ONE OAuth credential (this machine's `auth.json`), is the SINGLE
//! refresher (Anthropic rotates the refresh token on every refresh, so exactly
//! one party may refresh), and serves short-lived **access** tokens to authorized
//! client machines over HTTP.
//!
//! Clients run `synaps` with `auth.remote_endpoint` pointed here; they fetch a
//! token per request and never store the credential on their own disk. Run this
//! behind WireGuard / a private network — `--machine-token` gates who may fetch.
//!
//! Endpoints:
//!   GET /healthz            -> { status }                 (no secret, non-200 if cred missing)
//!   GET /token?provider=X   -> { access_token, expires }  (machine-auth required, allowlisted)

use std::net::SocketAddr;
use std::time::Duration;

use axum::{
    extract::{ConnectInfo, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;
use synaps_cli::auth;

/// Providers the broker will vend. Anything else is rejected before any work or
/// logging — prevents probing for configured providers and log injection via an
/// arbitrary provider string. (#158 B4)
const ALLOWED_PROVIDERS: &[&str] = &["anthropic", "openai-codex"];

#[derive(Clone)]
struct BrokerState {
    /// Token clients must present as `Authorization: Bearer <token>`. `None`
    /// disables auth (only safe on loopback or with explicit `--insecure-no-auth`).
    machine_token: Option<String>,
    /// HTTP client used for the (central, single) token refresh to the provider.
    client: reqwest::Client,
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

pub async fn run(
    bind: String,
    machine_token: Option<String>,
    machine_token_file: Option<std::path::PathBuf>,
    insecure: bool,
) -> anyhow::Result<()> {
    // B3: token precedence — flag > file (read once, not in argv) > env.
    let machine_token = match machine_token {
        Some(t) => Some(t),
        None => match machine_token_file {
            Some(ref path) => Some(
                std::fs::read_to_string(path)
                    .map_err(|e| anyhow::anyhow!("could not read --machine-token-file {}: {e}", path.display()))?
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

    // B2: refuse to start unauthenticated on a non-loopback bind unless the
    // operator explicitly opts in. A silent auth-OFF on the LAN is an open
    // credential tap, not a convenience.
    if machine_token.is_none() && !addr.ip().is_loopback() && !insecure {
        anyhow::bail!(
            "refusing to start: no machine token (set --machine-token / --machine-token-file / \
             SYNAPS_BROKER_TOKEN) while bound to non-loopback {addr}. That would serve credentials \
             unauthenticated to the network. Pass --insecure-no-auth to override, or bind 127.0.0.1."
        );
    }

    // D1: fail fast if the credential isn't present/readable, rather than
    // starting a broker that 500s every request.
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

    let state = BrokerState { machine_token: machine_token.clone(), client: client.clone() };

    // Proactive refresh, SUPERVISED: keep the credential warm and surface task
    // death loudly (a silently-dead refresher = unmonitored credential expiry).
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
        // D1: if the refresh task ever exits/panics, log loudly (it should run forever).
        tokio::spawn(async move {
            if let Err(e) = handle.await {
                eprintln!("[auth-broker] FATAL: proactive refresh task died: {e:?} — tokens will go stale; restart the broker.");
            }
        });
    }

    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/token", get(token))
        // B7: cap concurrent in-flight requests to bound resource use under a
        // flooding/crash-looping client (each /token can take a file lock + an
        // upstream refresh).
        .layer(tower::limit::ConcurrencyLimitLayer::new(64))
        .with_state(state);

    eprintln!(
        "synaps auth-broker listening on http://{addr}  (machine auth: {})",
        if machine_token.is_some() { "ON" } else { "OFF — loopback/insecure only!" }
    );
    if !addr.ip().is_loopback() {
        eprintln!("  ⚠ non-loopback bind — run behind WireGuard / a private network (no in-process TLS yet).");
    }
    let listener = tokio::net::TcpListener::bind(addr).await?;
    // D1: drain in-flight requests on SIGTERM/Ctrl-C instead of dropping them.
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
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
    eprintln!("[auth-broker] shutting down (draining in-flight requests)…");
}

/// Liveness + credential readiness. Returns non-200 when the credential is
/// missing/corrupt so monitors actually catch it. No secret, no expiry leak.
/// (#158 B8 / M1.)
async fn healthz() -> impl IntoResponse {
    match auth::load_auth() {
        Ok(Some(_)) => (StatusCode::OK, Json(json!({ "status": "ok" }))),
        Ok(None) => (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "status": "no_credential" }))),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "status": "error" }))),
    }
}

/// Issue a current access token for `provider` (default: anthropic, allowlisted).
/// The broker refreshes centrally if needed (file-locked — the single refresher).
async fn token(
    State(st): State<BrokerState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<TokenQuery>,
) -> impl IntoResponse {
    // ── machine auth (constant-time) ──
    if let Some(ref expected) = st.machine_token {
        let got = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let expected_header = format!("Bearer {expected}");
        if !ct_eq(got.as_bytes(), expected_header.as_bytes()) {
            eprintln!("[auth-broker] DENIED from {} (bad machine auth)", peer.ip());
            return (StatusCode::UNAUTHORIZED, Json(json!({ "error": "bad machine auth" })))
                .into_response();
        }
    }

    // ── provider allowlist (before any work, refresh, or logging of the value) ──
    let provider = q.provider.unwrap_or_else(|| "anthropic".to_string());
    if !ALLOWED_PROVIDERS.contains(&provider.as_str()) {
        // Never log the raw (attacker-controlled) provider — only its length,
        // so a `?provider=...%0A...` can't forge audit lines. (#158 B4 / CWE-117)
        eprintln!("[auth-broker] DENIED from {} (provider not allowed, {} chars)", peer.ip(), provider.len());
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "unknown provider" }))).into_response();
    }

    // `provider` is now a known-safe allowlisted value — safe to log verbatim.
    let creds = if provider == "anthropic" {
        auth::ensure_fresh_token(&st.client).await
    } else {
        auth::ensure_fresh_provider_token(&st.client, &provider).await
    };

    match creds {
        Ok(c) => {
            // C3: send a relative TTL so clients compute expiry on their own
            // clock (kills broker↔client skew on suspend/resume VMs).
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let ttl_ms = c.expires.saturating_sub(now);
            eprintln!("[auth-broker] issued {provider} token to {} (expires {})", peer.ip(), c.expires);
            (StatusCode::OK, Json(json!({ "access_token": c.access, "expires": c.expires, "ttl_ms": ttl_ms })))
                .into_response()
        }
        Err(e) => {
            // B5: log the detail server-side; return a generic message — the
            // error can contain fs paths or raw provider responses. (CWE-209)
            eprintln!("[auth-broker] refresh failed for {provider} from {}: {}", peer.ip(), e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": "token refresh failed" })))
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ct_eq;

    #[test]
    fn ct_eq_matches_only_identical() {
        assert!(ct_eq(b"Bearer secret", b"Bearer secret"));
        assert!(!ct_eq(b"Bearer secret", b"Bearer secreT"));
        assert!(!ct_eq(b"Bearer secret", b"Bearer wrong"));
        assert!(!ct_eq(b"short", b"longer-value"));
        assert!(!ct_eq(b"", b"x"));
        assert!(ct_eq(b"", b""));
    }
}
