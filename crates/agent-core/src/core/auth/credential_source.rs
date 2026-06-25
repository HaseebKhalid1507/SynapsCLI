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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialSource {
    /// Read + refresh the local `auth.json` (default — unchanged behavior).
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

/// An access token as returned by the broker's `GET /token`.
///
/// Deliberately has **no** refresh-token field: a Remote client must never
/// receive or hold one. This is the invariant made structural — there is no
/// place to put a refresh token even if the broker mistakenly sent one.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct BrokerToken {
    pub access_token: String,
    /// Absolute expiry, unix-epoch **milliseconds** (matches
    /// `OAuthCredentials.expires`).
    pub expires: u64,
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
}

// ── Fetcher abstraction + resolver ───────────────────────────────────────────

/// "Get a fresh access token from somewhere." Abstracted so the cache/resolve
/// logic is unit-testable without real HTTP. The real impl is `BrokerClient`.
#[allow(async_fn_in_trait)]
pub trait TokenFetcher {
    async fn fetch_token(&self, provider: &str) -> Result<BrokerToken, String>;
}

/// Resolve a provider access token via cache-or-fetch. Returns the cached token
/// if fresh; otherwise fetches from `fetcher`, caches it, and returns it.
///
/// This is the entire Remote credential path. It NEVER reads or holds a refresh
/// token, NEVER writes `auth.json`, NEVER refreshes client-side — `fetcher` only
/// ever yields a short-lived `BrokerToken` (which structurally has no refresh).
pub async fn resolve_remote_token<F: TokenFetcher>(
    fetcher: &F,
    cache: &TokenCache,
    provider: &str,
    margin_ms: u64,
) -> Result<String, String> {
    if let Some(tok) = cache.get_fresh(provider, margin_ms) {
        return Ok(tok.access_token);
    }
    let tok = fetcher.fetch_token(provider).await?;
    cache.put(provider, tok.clone());
    Ok(tok.access_token)
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
        Self { http: reqwest::Client::new(), endpoint: endpoint.into(), machine_token: machine_token.into() }
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
        resp.json::<BrokerToken>()
            .await
            .map_err(|e| format!("invalid broker token response: {e}"))
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
        BrokerToken { access_token: "sk-live".into(), expires }
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
}
