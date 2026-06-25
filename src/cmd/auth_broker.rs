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
//!   GET /healthz            -> { status, fresh_until }   (no secret)
//!   GET /token?provider=X   -> { access_token, expires }  (machine-auth required)

use std::net::SocketAddr;
use std::time::Duration;

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;
use synaps_cli::auth;

#[derive(Clone)]
struct BrokerState {
    /// Token clients must present as `Authorization: Bearer <token>`. `None`
    /// disables auth (only safe on a fully trusted/private network).
    machine_token: Option<String>,
    /// HTTP client used for the (central, single) token refresh to the provider.
    client: reqwest::Client,
}

#[derive(Deserialize)]
struct TokenQuery {
    provider: Option<String>,
}

pub async fn run(bind: String, machine_token: Option<String>) -> anyhow::Result<()> {
    let machine_token = machine_token
        .or_else(|| std::env::var("SYNAPS_BROKER_TOKEN").ok())
        .filter(|s| !s.trim().is_empty());

    let client = reqwest::Client::builder()
        .tls_built_in_webpki_certs(true)
        .tls_built_in_native_certs(true)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .build()?;

    let state = BrokerState { machine_token: machine_token.clone(), client };

    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/token", get(token))
        .with_state(state);

    let addr: SocketAddr = bind
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid --bind '{}': {}", bind, e))?;

    eprintln!(
        "synaps auth-broker listening on http://{addr}  (machine auth: {})",
        if machine_token.is_some() { "ON" } else { "OFF — trusted-network only!" }
    );
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

/// Liveness + credential freshness, without leaking any secret.
async fn healthz() -> impl IntoResponse {
    let fresh_until = match auth::load_auth() {
        Ok(Some(f)) => Some(f.anthropic.expires),
        _ => None,
    };
    (StatusCode::OK, Json(json!({ "status": "ok", "fresh_until": fresh_until })))
}

/// Issue a current access token for `provider` (default: anthropic).
/// The broker refreshes centrally if needed (file-locked — the single refresher).
async fn token(
    State(st): State<BrokerState>,
    headers: HeaderMap,
    Query(q): Query<TokenQuery>,
) -> impl IntoResponse {
    // ── machine auth ──
    if let Some(ref expected) = st.machine_token {
        let got = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if got != format!("Bearer {expected}") {
            return (StatusCode::UNAUTHORIZED, Json(json!({ "error": "bad machine auth" })))
                .into_response();
        }
    }

    let provider = q.provider.unwrap_or_else(|| "anthropic".to_string());
    let creds = if provider == "anthropic" {
        auth::ensure_fresh_token(&st.client).await
    } else {
        auth::ensure_fresh_provider_token(&st.client, &provider).await
    };

    match creds {
        Ok(c) => {
            eprintln!("[auth-broker] issued {} token (expires {})", provider, c.expires);
            (StatusCode::OK, Json(json!({ "access_token": c.access, "expires": c.expires })))
                .into_response()
        }
        Err(e) => {
            eprintln!("[auth-broker] refresh failed for {}: {}", provider, e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e }))).into_response()
        }
    }
}
