//! Remote credential source — Option C (task #157, epic #155).
//!
//! A Synaps client can resolve its provider **access token** from a broker over
//! the network instead of the local `auth.json`. This lets many machines share
//! one OAuth credential without copying the secret to each disk.
//!
//! INVARIANT (the whole point — enforced by construction + tests):
//! the `Remote` path NEVER reads or holds a refresh token, NEVER writes
//! `auth.json`, and NEVER refreshes client-side. It only fetches short-lived
//! access tokens from the broker and caches them in memory. The single
//! refresher is the broker (Anthropic rotates the refresh token on every
//! refresh, so exactly one party may refresh).

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// Where a client gets its provider credentials.
#[derive(Clone, PartialEq, Eq, Default)]
pub enum CredentialSource {
    /// Read + refresh the local `auth.json` (default — unchanged behavior).
    #[default]
    Local,
    /// Fetch short-lived access tokens from a broker over the network.
    Remote {
        /// Broker base URL, e.g. `https://jade.jade:8181` (no trailing slash).
        endpoint: String,
        /// Per-machine bearer presented TO the broker. This is the machine's own
        /// identity, NOT the provider credential.
        machine_token: String,
    },
}

impl CredentialSource {
    /// Build from explicit config values. Returns `Remote` iff a non-empty
    /// endpoint is given; otherwise `Local`. Trailing slashes on the endpoint
    /// are trimmed so callers can join paths uniformly.
    pub fn from_parts(endpoint: Option<String>, machine_token: Option<String>) -> Self {
        match endpoint {
            Some(e) if !e.trim().is_empty() => CredentialSource::Remote {
                endpoint: e.trim().trim_end_matches('/').to_string(),
                machine_token: machine_token.unwrap_or_default().trim().to_string(),
            },
            _ => CredentialSource::Local,
        }
    }

    pub fn is_remote(&self) -> bool {
        matches!(self, CredentialSource::Remote { .. })
    }
}

/// Redacting Debug — never print the machine token (board M3/B3).
impl std::fmt::Debug for CredentialSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CredentialSource::Local => write!(f, "Local"),
            CredentialSource::Remote { endpoint, .. } => f
                .debug_struct("Remote")
                .field("endpoint", endpoint)
                .field("machine_token", &"***")
                .finish(),
        }
    }
}

/// An access token as returned by the broker's `GET /token`.
///
/// Deliberately has **no** refresh-token field: a Remote client must never
/// receive or hold one. This is the invariant made structural — there is no
/// place to put a refresh token even if the broker mistakenly sent one.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct BrokerToken {
    pub access_token: String,
    /// Absolute expiry, unix-epoch **milliseconds** (matches
    /// `OAuthCredentials.expires`). When `ttl_ms` is present the client
    /// overwrites this with its own clock + ttl to defeat clock skew.
    pub expires: u64,
    /// Optional relative TTL in ms. When the broker sends it, the client
    /// recomputes `expires = client_now + ttl_ms`, eliminating broker↔client
    /// clock skew on suspend/resume VMs (board C3). Absent → use `expires`.
    #[serde(default)]
    pub ttl_ms: Option<u64>,
}

/// Current unix time in milliseconds.
fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// True if `expires_ms` is already past, or within `margin_ms` of now.
///
/// The margin absorbs clock skew + request latency so a client refetches
/// slightly early rather than presenting a token that dies mid-flight.
pub fn is_expired_with_margin(expires_ms: u64, margin_ms: u64) -> bool {
    now_millis().saturating_add(margin_ms) >= expires_ms
}

/// Default refetch margin: 5 minutes (mirrors `is_token_expired`).
pub const DEFAULT_MARGIN_MS: u64 = 5 * 60 * 1000;

// ── In-memory token cache ────────────────────────────────────────────────────

/// Thread-safe, per-provider cache of broker access tokens. Cloneable handle
/// over shared state. Holds ONLY short-lived access tokens, never a refresh
/// token, never persisted to disk.
#[derive(Clone, Default)]
pub struct TokenCache {
    inner: Arc<RwLock<HashMap<String, BrokerToken>>>,
}

/// Redacting Debug — print only the cached provider names, never the tokens.
impl std::fmt::Debug for TokenCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let providers: Vec<String> = self
            .inner
            .read()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        f.debug_struct("TokenCache").field("providers", &providers).finish()
    }
}

impl TokenCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the cached token for `provider` only if present AND not within
    /// `margin_ms` of expiry. Otherwise `None` (caller should fetch).
    pub fn get_fresh(&self, provider: &str, margin_ms: u64) -> Option<BrokerToken> {
        let map = self.inner.read().ok()?;
        let tok = map.get(provider)?;
        if is_expired_with_margin(tok.expires, margin_ms) {
            None
        } else {
            Some(tok.clone())
        }
    }

    pub fn put(&self, provider: &str, token: BrokerToken) {
        if let Ok(mut map) = self.inner.write() {
            map.insert(provider.to_string(), token);
        }
    }

    pub fn invalidate(&self, provider: &str) {
        if let Ok(mut map) = self.inner.write() {
            map.remove(provider);
        }
    }

    /// Cached token if present and not PAST hard expiry (ignores the refetch
    /// margin). Used as a degraded-mode fallback when the broker is unreachable:
    /// a token slightly inside its refetch window is still better than failing.
    pub fn get_unexpired(&self, provider: &str) -> Option<BrokerToken> {
        let map = self.inner.read().ok()?;
        let tok = map.get(provider)?;
        if is_expired_with_margin(tok.expires, 0) {
            None
        } else {
            Some(tok.clone())
        }
    }
}

// ── Fetcher abstraction + resolver ───────────────────────────────────────────

/// "Get a fresh access token from somewhere." Abstracted so the cache/resolve
/// logic is unit-testable without real HTTP. The real impl is `BrokerClient`.
#[allow(async_fn_in_trait)]
pub trait TokenFetcher {
    async fn fetch_token(&self, provider: &str) -> Result<BrokerToken, String>;
}

/// Resolve a provider token via cache-or-fetch, returning the full
/// `BrokerToken` (access + expiry). The runtime needs the expiry to drive its
/// in-memory refresh trigger. NEVER touches a refresh token or `auth.json`.
pub async fn resolve_remote<F: TokenFetcher>(
    fetcher: &F,
    cache: &TokenCache,
    provider: &str,
    margin_ms: u64,
) -> Result<BrokerToken, String> {
    if let Some(tok) = cache.get_fresh(provider, margin_ms) {
        return Ok(tok);
    }
    match fetcher.fetch_token(provider).await {
        Ok(tok) => {
            cache.put(provider, tok.clone());
            Ok(tok)
        }
        Err(e) => {
            // Degraded mode: the broker is unreachable, but if we still hold a
            // token that hasn't hit hard expiry, serve it rather than failing
            // the turn. Self-heals on the next call once the broker is back.
            if let Some(tok) = cache.get_unexpired(provider) {
                return Ok(tok);
            }
            Err(e)
        }
    }
}

/// Like [`resolve_remote`] but returns only the access token string.
pub async fn resolve_remote_token<F: TokenFetcher>(
    fetcher: &F,
    cache: &TokenCache,
    provider: &str,
    margin_ms: u64,
) -> Result<String, String> {
    Ok(resolve_remote(fetcher, cache, provider, margin_ms).await?.access_token)
}

// ── Broker HTTP client ───────────────────────────────────────────────────────

/// HTTP client for a credential broker. Presents the machine's own bearer token
/// (NOT the provider credential) and receives a short-lived access token.
pub struct BrokerClient {
    http: reqwest::Client,
    endpoint: String,
    machine_token: String,
}

impl BrokerClient {
    pub fn new(endpoint: impl Into<String>, machine_token: impl Into<String>) -> Self {
        // D1: bound the request so a hung/unreachable broker can't stall the
        // caller's whole turn. (The runtime path uses `with_client` and inherits
        // the runtime's configured client; this is the standalone fallback.)
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .unwrap_or_default();
        Self { http, endpoint: endpoint.into(), machine_token: machine_token.into() }
    }

    /// Like `new` but reuses an existing `reqwest::Client` (shared connection
    /// pool) instead of building a fresh one per call. (#158 A5)
    pub fn with_client(
        endpoint: impl Into<String>,
        machine_token: impl Into<String>,
        http: reqwest::Client,
    ) -> Self {
        Self { http, endpoint: endpoint.into(), machine_token: machine_token.into() }
    }

    /// Build from a `CredentialSource`. `None` for `Local`.
    pub fn from_source(source: &CredentialSource) -> Option<Self> {
        match source {
            CredentialSource::Remote { endpoint, machine_token } => {
                Some(Self::new(endpoint.clone(), machine_token.clone()))
            }
            CredentialSource::Local => None,
        }
    }
}

/// Provider-aware token resolution honoring the credential source — the single
/// entry point both the Anthropic and the OpenAI/codex paths use. (#158 C4)
///
/// - `Local`: refresh the provider's own local credential (`ensure_fresh_*`).
/// - `Remote`: fetch a short-lived access token from the broker, keyed by
///   provider. Reuses the caller's `http` client (no per-call pool — A5), and
///   never holds a refresh token / never writes `auth.json` (invariant 1).
pub async fn resolve_access_token(
    provider: &str,
    source: &CredentialSource,
    cache: &TokenCache,
    http: &reqwest::Client,
) -> Result<String, String> {
    match source {
        CredentialSource::Remote { endpoint, machine_token } => {
            let broker = BrokerClient::with_client(endpoint.clone(), machine_token.clone(), http.clone());
            Ok(resolve_remote(&broker, cache, provider, DEFAULT_MARGIN_MS).await?.access_token)
        }
        CredentialSource::Local => {
            let creds = if provider == "anthropic" {
                super::ensure_fresh_token(http).await?
            } else {
                super::ensure_fresh_provider_token(http, provider).await?
            };
            Ok(creds.access)
        }
    }
}

impl TokenFetcher for BrokerClient {
    async fn fetch_token(&self, provider: &str) -> Result<BrokerToken, String> {
        let url = format!("{}/token", self.endpoint);
        let resp = self
            .http
            .get(&url)
            .query(&[("provider", provider)])
            .bearer_auth(&self.machine_token)
            .send()
            .await
            .map_err(|e| format!("broker request failed: {e}"))?;
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err("broker rejected machine auth (401)".to_string());
        }
        if !status.is_success() {
            return Err(format!("broker returned HTTP {status}"));
        }
        let mut tok = resp
            .json::<BrokerToken>()
            .await
            .map_err(|e| format!("invalid broker token response: {e}"))?;
        // C3: prefer the broker's relative TTL over its absolute clock.
        if let Some(ttl) = tok.ttl_ms {
            tok.expires = now_millis().saturating_add(ttl);
        }
        // C2: reject a malformed/dead token rather than caching it (which would
        // cause a permanent refetch storm or a dud bearer).
        if tok.access_token.is_empty() {
            return Err("broker returned an empty access_token".to_string());
        }
        if tok.expires <= now_millis() {
            return Err("broker returned an already-expired token".to_string());
        }
        Ok(tok)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_parts_local_when_no_endpoint() {
        assert_eq!(CredentialSource::from_parts(None, None), CredentialSource::Local);
        assert_eq!(
            CredentialSource::from_parts(Some("   ".into()), Some("m".into())),
            CredentialSource::Local
        );
    }

    #[test]
    fn from_parts_remote_when_endpoint_set() {
        let s = CredentialSource::from_parts(Some("https://jade.jade:8181".into()), Some("tok".into()));
        assert_eq!(
            s,
            CredentialSource::Remote {
                endpoint: "https://jade.jade:8181".into(),
                machine_token: "tok".into()
            }
        );
        assert!(s.is_remote());
    }

    #[test]
    fn from_parts_trims_trailing_slash_and_whitespace() {
        let s = CredentialSource::from_parts(Some("  https://b/  ".into()), Some("  tok  ".into()));
        assert_eq!(
            s,
            CredentialSource::Remote { endpoint: "https://b".into(), machine_token: "tok".into() }
        );
    }

    #[test]
    fn remote_with_missing_machine_token_defaults_empty() {
        let s = CredentialSource::from_parts(Some("https://b".into()), None);
        assert_eq!(
            s,
            CredentialSource::Remote { endpoint: "https://b".into(), machine_token: String::new() }
        );
    }

    #[test]
    fn local_is_not_remote() {
        assert!(!CredentialSource::Local.is_remote());
    }

    #[test]
    fn expiry_far_future_not_expired() {
        let far = now_millis() + 60 * 60 * 1000; // +1h
        assert!(!is_expired_with_margin(far, DEFAULT_MARGIN_MS));
    }

    #[test]
    fn expiry_past_is_expired() {
        let past = now_millis().saturating_sub(1000);
        assert!(is_expired_with_margin(past, 0));
    }

    #[test]
    fn expiry_within_margin_is_expired() {
        // expires in 2 minutes, margin 5 minutes -> treated as expired (refetch early)
        let soon = now_millis() + 2 * 60 * 1000;
        assert!(is_expired_with_margin(soon, DEFAULT_MARGIN_MS));
        // ...but with a 1-minute margin it is NOT yet expired
        assert!(!is_expired_with_margin(soon, 60 * 1000));
    }

    #[test]
    fn broker_token_deserializes_without_refresh_field() {
        let json = r#"{"access_token":"sk-abc","expires":1750000000000}"#;
        let t: BrokerToken = serde_json::from_str(json).unwrap();
        assert_eq!(t.access_token, "sk-abc");
        assert_eq!(t.expires, 1_750_000_000_000);
    }

    // ── cache ────────────────────────────────────────────────────────────
    fn tok(expires: u64) -> BrokerToken {
        BrokerToken { access_token: "sk-live".into(), expires, ttl_ms: None }
    }

    #[test]
    fn cache_get_fresh_returns_unexpired() {
        let c = TokenCache::new();
        c.put("anthropic", tok(now_millis() + 60 * 60 * 1000));
        assert!(c.get_fresh("anthropic", DEFAULT_MARGIN_MS).is_some());
    }

    #[test]
    fn cache_get_fresh_none_when_expired() {
        let c = TokenCache::new();
        c.put("anthropic", tok(now_millis().saturating_sub(1000)));
        assert!(c.get_fresh("anthropic", 0).is_none());
    }

    #[test]
    fn cache_invalidate_removes() {
        let c = TokenCache::new();
        c.put("anthropic", tok(now_millis() + 60 * 60 * 1000));
        c.invalidate("anthropic");
        assert!(c.get_fresh("anthropic", 0).is_none());
    }

    #[test]
    fn cache_providers_isolated() {
        let c = TokenCache::new();
        c.put("anthropic", tok(now_millis() + 60 * 60 * 1000));
        assert!(c.get_fresh("openai", 0).is_none());
    }

    // ── resolve with a fake fetcher (counts calls) ───────────────────────
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FakeFetcher {
        token: BrokerToken,
        calls: AtomicUsize,
    }
    impl TokenFetcher for FakeFetcher {
        async fn fetch_token(&self, _provider: &str) -> Result<BrokerToken, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.token.clone())
        }
    }

    #[tokio::test]
    async fn resolve_cache_hit_does_not_fetch() {
        let cache = TokenCache::new();
        cache.put("anthropic", tok(now_millis() + 60 * 60 * 1000));
        let f = FakeFetcher { token: tok(0), calls: AtomicUsize::new(0) };
        let t = resolve_remote_token(&f, &cache, "anthropic", DEFAULT_MARGIN_MS).await.unwrap();
        assert_eq!(t, "sk-live");
        assert_eq!(f.calls.load(Ordering::SeqCst), 0, "must not fetch on a cache hit");
    }

    #[tokio::test]
    async fn resolve_miss_fetches_then_caches() {
        let cache = TokenCache::new();
        let f = FakeFetcher { token: tok(now_millis() + 60 * 60 * 1000), calls: AtomicUsize::new(0) };
        resolve_remote_token(&f, &cache, "anthropic", DEFAULT_MARGIN_MS).await.unwrap();
        resolve_remote_token(&f, &cache, "anthropic", DEFAULT_MARGIN_MS).await.unwrap();
        assert_eq!(f.calls.load(Ordering::SeqCst), 1, "second resolve should hit the cache");
    }

    // ── broker-down degradation ───────────────────────────────────────────
    struct FailFetcher;
    impl TokenFetcher for FailFetcher {
        async fn fetch_token(&self, _provider: &str) -> Result<BrokerToken, String> {
            Err("broker unreachable".into())
        }
    }

    #[tokio::test]
    async fn resolve_serves_stale_cache_when_broker_down() {
        // Token expires in 2 min; margin is 5 min -> get_fresh misses (would
        // refetch), the broker is down, but it's not HARD-expired, so we serve it.
        let cache = TokenCache::new();
        cache.put("anthropic", tok(now_millis() + 2 * 60 * 1000));
        let t = resolve_remote(&FailFetcher, &cache, "anthropic", DEFAULT_MARGIN_MS).await.unwrap();
        assert_eq!(t.access_token, "sk-live");
    }

    #[tokio::test]
    async fn resolve_errors_when_broker_down_and_no_cache() {
        let cache = TokenCache::new();
        let err = resolve_remote(&FailFetcher, &cache, "anthropic", DEFAULT_MARGIN_MS).await.unwrap_err();
        assert!(err.contains("unreachable"), "got: {err}");
    }

    #[tokio::test]
    async fn resolve_errors_when_broker_down_and_cache_hard_expired() {
        let cache = TokenCache::new();
        cache.put("anthropic", tok(now_millis().saturating_sub(1000))); // already expired
        let err = resolve_remote(&FailFetcher, &cache, "anthropic", 0).await.unwrap_err();
        assert!(err.contains("unreachable"), "got: {err}");
    }

    // ── BrokerClient against a tiny in-test axum server ──────────────────
    async fn spawn_broker(token_json: &'static str, require_token: &'static str) -> String {
        use axum::{http::HeaderMap, http::StatusCode, routing::get, Router};
        let app = Router::new().route(
            "/token",
            get(move |headers: HeaderMap| async move {
                let auth = headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                if auth == format!("Bearer {require_token}") {
                    (StatusCode::OK, token_json.to_string())
                } else {
                    (StatusCode::UNAUTHORIZED, String::new())
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

    #[tokio::test]
    async fn broker_client_fetches_and_parses() {
        let url = spawn_broker(
            r#"{"access_token":"sk-from-broker","expires":9999999999999}"#,
            "machine-xyz",
        )
        .await;
        let c = BrokerClient::new(url, "machine-xyz");
        let t = c.fetch_token("anthropic").await.unwrap();
        assert_eq!(t.access_token, "sk-from-broker");
        assert_eq!(t.expires, 9_999_999_999_999);
    }

    #[tokio::test]
    async fn broker_client_401_on_bad_machine_auth() {
        let url = spawn_broker(
            r#"{"access_token":"x","expires":9999999999999}"#,
            "right-token",
        )
        .await;
        let c = BrokerClient::new(url, "WRONG-token");
        let err = c.fetch_token("anthropic").await.unwrap_err();
        assert!(err.contains("401"), "expected 401 error, got: {err}");
    }

    #[tokio::test]
    async fn fetch_token_recomputes_expiry_from_ttl_ms() {
        // Broker sends a STALE absolute `expires` but a fresh `ttl_ms`; the
        // client must recompute from its own clock (clock-skew defense, C3).
        let url = spawn_broker(r#"{"access_token":"sk","expires":1,"ttl_ms":3600000}"#, "m").await;
        let c = BrokerClient::new(url, "m");
        let t = c.fetch_token("anthropic").await.unwrap();
        assert!(t.expires > now_millis(), "expires must come from ttl_ms, got {}", t.expires);
    }

    #[tokio::test]
    async fn fetch_token_rejects_empty_access_token() {
        let url = spawn_broker(r#"{"access_token":"","expires":9999999999999}"#, "m").await;
        let c = BrokerClient::new(url, "m");
        assert!(c.fetch_token("anthropic").await.unwrap_err().contains("empty"));
    }

    #[tokio::test]
    async fn fetch_token_rejects_already_expired() {
        let url = spawn_broker(r#"{"access_token":"sk","expires":1}"#, "m").await;
        let c = BrokerClient::new(url, "m");
        assert!(c.fetch_token("anthropic").await.unwrap_err().contains("expired"));
    }

    #[tokio::test]
    async fn resolve_access_token_remote_fetches_from_broker() {
        let url = spawn_broker(
            r#"{"access_token":"sk-broker","expires":9999999999999}"#,
            "m",
        )
        .await;
        let source = CredentialSource::Remote { endpoint: url, machine_token: "m".into() };
        let cache = TokenCache::new();
        let http = reqwest::Client::new();
        let t = resolve_access_token("anthropic", &source, &cache, &http).await.unwrap();
        assert_eq!(t, "sk-broker");
        // second call hits the cache (shared http client reused, no new pool)
        let t2 = resolve_access_token("anthropic", &source, &cache, &http).await.unwrap();
        assert_eq!(t2, "sk-broker");
    }

    // ── invariant: a Remote client can NEVER hold a refresh token ─────────
    #[test]
    fn broker_token_structurally_drops_any_refresh_field() {
        // Even a misbehaving/compromised broker that leaks a refresh token in
        // the JSON cannot make a client hold one: BrokerToken has no field for
        // it, so serde silently drops it. The "clients never hold a refresh
        // token" invariant is structural, not a runtime check.
        let json = r#"{"access_token":"a","expires":1,"refresh_token":"LEAK","refresh":"LEAK"}"#;
        let t: BrokerToken = serde_json::from_str(json).unwrap();
        assert_eq!(t.access_token, "a");
        assert_eq!(t.expires, 1);
        // There is no `refresh`/`refresh_token` field to even read — confirmed
        // at compile time by the struct definition above.
    }
}
