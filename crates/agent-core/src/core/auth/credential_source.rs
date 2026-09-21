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

use super::account::{Account, CredentialRef};

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
#[derive(Clone, serde::Deserialize)]
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

/// Redacting Debug — never print access_token (review finding H2).
impl std::fmt::Debug for BrokerToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrokerToken")
            .field("access_token", &"[REDACTED]")
            .field("expires", &self.expires)
            .field("ttl_ms", &self.ttl_ms)
            .finish()
    }
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

type LocalBrokerMap = std::collections::BTreeMap<
    (std::path::PathBuf, std::path::PathBuf),
    Arc<super::broker::LocalBroker>,
>;

/// Thread-safe cache of broker access tokens. Cloneable handle over shared
/// state. Holds ONLY short-lived access tokens, never a refresh token, never
/// persisted to disk.
///
/// Keys are either a bare provider id (legacy callers) or a scoped key built
/// by [`TokenCache::scoped_key`]: `"<storage_key>|<source scope>"`, where
/// the scope identifies the broker endpoint AND the machine principal (a
/// digest of the machine token — never the token itself) so a re-pointed or
/// re-keyed client can never reuse another principal's cached token.
#[derive(Clone, Default)]
pub struct TokenCache {
    inner: Arc<RwLock<HashMap<String, BrokerToken>>>,
    /// Local runtime authority retained across adapter construction. Clones
    /// share cooldown/current-seat state; profiles have separate entries.
    pub(crate) local_brokers: Arc<std::sync::Mutex<LocalBrokerMap>>,
}

/// Opaque, log-safe identity of a remote credential source.
pub fn source_scope(endpoint: &str, machine_token: &str) -> String {
    use sha2::{Digest, Sha256};
    let principal = if machine_token.is_empty() {
        "anon".to_string()
    } else {
        let digest = Sha256::digest(machine_token.as_bytes());
        hex_prefix(&digest, 16)
    };
    format!("{}#{principal}", endpoint.trim_end_matches('/'))
}

fn hex_prefix(bytes: &[u8], chars: usize) -> String {
    let mut out = String::with_capacity(chars);
    for b in bytes {
        if out.len() >= chars {
            break;
        }
        out.push_str(&format!("{b:02x}"));
    }
    out.truncate(chars);
    out
}

/// Provider id component of a cache key (bare provider or scoped key).
fn key_provider(key: &str) -> Option<String> {
    let storage_key = key.split_once('|').map(|(k, _)| k).unwrap_or(key);
    CredentialRef::parse_storage_key(storage_key)
        .map(|c| c.provider.as_str().to_string())
        .or_else(|| Some(storage_key.to_string()))
}

/// Redacting Debug — print only the cached provider names, never the tokens.
impl std::fmt::Debug for TokenCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let providers: Vec<String> = self
            .inner
            .read()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        f.debug_struct("TokenCache")
            .field("providers", &providers)
            .finish()
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

    /// Drop every cached token for `provider` — the legacy bare key and every
    /// scoped `(account, source)` key of that provider — so a 401 or a source
    /// switch can never leave an account's stale token behind.
    pub fn invalidate(&self, provider: &str) {
        if let Ok(mut map) = self.inner.write() {
            map.retain(|key, _| key_provider(key).as_deref() != Some(provider));
        }
    }

    /// Cache key for one credential fetched from one source scope.
    pub fn scoped_key(scope: &str, cred: &CredentialRef) -> String {
        format!("{}|{scope}", cred.storage_key())
    }

    /// Drop the cached token for exactly one scoped credential.
    pub fn invalidate_credential(&self, scope: &str, cred: &CredentialRef) {
        if let Ok(mut map) = self.inner.write() {
            map.remove(&Self::scoped_key(scope, cred));
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

/// Typed failure of an account-addressed token fetch. Display is secret-free.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenFetchError {
    /// Broker rejected machine auth (401).
    Unauthorized,
    /// Broker reports the explicit account slot does not exist (404).
    UnknownAccount,
    /// Broker rejected the account label (400).
    InvalidAccount,
    /// Fetcher/broker cannot address named slots.
    UnsupportedAccount,
    /// Broker served a token for a different slot than the one requested
    /// (or, for a named slot, did not say which). Never cached.
    AccountMismatch,
    /// Any other HTTP status.
    Http(u16),
    /// Transport or response-shape failure (message already secret-free).
    Other(String),
}

impl std::fmt::Display for TokenFetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unauthorized => write!(f, "broker rejected machine auth (401)"),
            Self::UnknownAccount => write!(f, "broker reports unknown account (404)"),
            Self::InvalidAccount => write!(f, "broker rejected account label (400)"),
            Self::UnsupportedAccount => write!(f, "named accounts are not supported by this fetcher"),
            Self::AccountMismatch => write!(
                f,
                "broker served a token for a different account than requested (broker too old for named accounts?)"
            ),
            Self::Http(status) => write!(f, "broker returned HTTP {status}"),
            Self::Other(msg) => f.write_str(msg),
        }
    }
}

impl TokenFetchError {
    pub fn into_broker_error(self, cred: &CredentialRef) -> super::broker::BrokerError {
        use super::broker::BrokerError;
        match self {
            Self::Unauthorized => BrokerError::Unauthorized,
            Self::UnknownAccount => BrokerError::UnknownAccount {
                provider: cred.provider.as_str().to_string(),
                label: cred.account.label_str().to_string(),
            },
            Self::InvalidAccount => {
                BrokerError::InvalidAccount(format!("broker rejected '{}'", cred.account))
            }
            Self::UnsupportedAccount => BrokerError::UnsupportedAccount {
                provider: cred.provider.as_str().to_string(),
                label: cred.account.label_str().to_string(),
            },
            Self::AccountMismatch => BrokerError::Transport(format!(
                "broker served a token for a different account than '{}' (no fallback)",
                cred
            )),
            Self::Http(status) => BrokerError::Transport(format!("broker returned HTTP {status}")),
            Self::Other(msg) => BrokerError::Transport(msg),
        }
    }
}

/// "Get a fresh access token from somewhere." Abstracted so the cache/resolve
/// logic is unit-testable without real HTTP. The real impl is `BrokerClient`.
#[allow(async_fn_in_trait)]
pub trait TokenFetcher {
    async fn fetch_token(&self, provider: &str) -> Result<BrokerToken, String>;

    /// Account-addressed fetch. Default: the default slot goes through
    /// [`fetch_token`](Self::fetch_token); a named slot is unsupported (never
    /// silently served from the default slot).
    async fn fetch_credential(&self, cred: &CredentialRef) -> Result<BrokerToken, TokenFetchError> {
        match &cred.account {
            Account::Default => self
                .fetch_token(cred.provider.as_str())
                .await
                .map_err(TokenFetchError::Other),
            Account::Named(_) => Err(TokenFetchError::UnsupportedAccount),
        }
    }
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

/// Account- and source-scoped variant of [`resolve_remote`]: the cache key
/// carries the storage key plus the source scope, so tokens for different
/// accounts, endpoints or machine principals never alias. Same degraded-mode
/// rule (serve an unexpired cached token when the broker is unreachable),
/// except that typed rejections (401/404/400) are never masked by the cache.
pub async fn resolve_remote_credential<F: TokenFetcher>(
    fetcher: &F,
    cache: &TokenCache,
    scope: &str,
    cred: &CredentialRef,
    margin_ms: u64,
) -> Result<BrokerToken, TokenFetchError> {
    let key = TokenCache::scoped_key(scope, cred);
    if let Some(tok) = cache.get_fresh(&key, margin_ms) {
        return Ok(tok);
    }
    match fetcher.fetch_credential(cred).await {
        Ok(tok) => {
            cache.put(&key, tok.clone());
            Ok(tok)
        }
        Err(e @ TokenFetchError::Other(_)) | Err(e @ TokenFetchError::Http(_)) => {
            if let Some(tok) = cache.get_unexpired(&key) {
                return Ok(tok);
            }
            Err(e)
        }
        Err(rejected @ TokenFetchError::AccountMismatch) => {
            // Wrong slot served: never serve a cached token for a slot the
            // broker no longer honours; the caller sees the mismatch.
            cache.invalidate_credential(scope, cred);
            Err(rejected)
        }
        Err(rejected) => {
            // The broker positively rejected this credential/principal: a
            // cached token must not paper over it.
            cache.invalidate_credential(scope, cred);
            Err(rejected)
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
    Ok(resolve_remote(fetcher, cache, provider, margin_ms)
        .await?
        .access_token)
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
        Self {
            http,
            endpoint: endpoint.into(),
            machine_token: machine_token.into(),
        }
    }

    /// Like `new` but reuses an existing `reqwest::Client` (shared connection
    /// pool) instead of building a fresh one per call. (#158 A5)
    pub fn with_client(
        endpoint: impl Into<String>,
        machine_token: impl Into<String>,
        http: reqwest::Client,
    ) -> Self {
        Self {
            http,
            endpoint: endpoint.into(),
            machine_token: machine_token.into(),
        }
    }

    /// Build from a `CredentialSource`. `None` for `Local`.
    pub fn from_source(source: &CredentialSource) -> Option<Self> {
        match source {
            CredentialSource::Remote {
                endpoint,
                machine_token,
            } => Some(Self::new(endpoint.clone(), machine_token.clone())),
            CredentialSource::Local => None,
        }
    }
}

/// Provider-aware token resolution honoring the credential source — kept as a
/// compatibility adapter for CLI callers (`synaps status`). Delegates to the
/// typed credential broker: the local in-process broker or the authenticated
/// remote transport. Vends the access token only.
pub async fn resolve_access_token(
    provider: &str,
    source: &CredentialSource,
    cache: &TokenCache,
    http: &reqwest::Client,
) -> Result<String, String> {
    let provider: super::provider::OAuthProviderId = provider.try_into()?;
    super::broker::broker_from_source(source, cache, http.clone())
        .access_token(provider)
        .await
        .map(|t| t.token)
        .map_err(|e| e.to_string())
}

impl BrokerClient {
    /// Source scope for cache keys (endpoint + machine-principal digest).
    pub fn scope(&self) -> String {
        source_scope(&self.endpoint, &self.machine_token)
    }

    /// `GET /token?provider=X[&account=Y]`.
    ///
    /// `account: None` is the legacy, policy-free call (`fetch_token`): the
    /// broker host applies its own policy and the result is cached under the
    /// bare provider key. `Some(slot)` is an EXPLICIT slot (including
    /// `default`): the parameter is always sent, and the response must name
    /// that same slot — a broker serving another slot (or an old broker that
    /// ignores the parameter and says nothing) is rejected for named slots and
    /// never cached under the requested key. An absent `account` is tolerated
    /// only for `default`, because pre-multi-account brokers always served the
    /// bare provider slot.
    async fn fetch_token_query(
        &self,
        provider: &str,
        account: Option<&str>,
    ) -> Result<BrokerToken, TokenFetchError> {
        let url = format!("{}/token", self.endpoint);
        let mut query: Vec<(&str, &str)> = vec![("provider", provider)];
        if let Some(account) = account {
            query.push(("account", account));
        }
        let resp = self
            .http
            .get(&url)
            .query(&query)
            .bearer_auth(&self.machine_token)
            .send()
            .await
            .map_err(|e| TokenFetchError::Other(format!("broker request failed: {e}")))?;
        let status = resp.status();
        match status.as_u16() {
            401 => return Err(TokenFetchError::Unauthorized),
            404 if account.is_some() => return Err(TokenFetchError::UnknownAccount),
            400 if account.is_some() => return Err(TokenFetchError::InvalidAccount),
            s if !status.is_success() => return Err(TokenFetchError::Http(s)),
            _ => {}
        }
        #[derive(serde::Deserialize)]
        struct Wire {
            access_token: String,
            expires: u64,
            #[serde(default)]
            ttl_ms: Option<u64>,
            #[serde(default)]
            account: Option<String>,
        }
        let wire = resp
            .json::<Wire>()
            .await
            .map_err(|e| TokenFetchError::Other(format!("invalid broker token response: {e}")))?;
        if let Some(requested) = account {
            match wire.account.as_deref() {
                Some(served) if served == requested => {}
                None if requested == super::account::DEFAULT_ACCOUNT_NAME => {}
                _ => return Err(TokenFetchError::AccountMismatch),
            }
        }
        let mut tok = BrokerToken {
            access_token: wire.access_token,
            expires: wire.expires,
            ttl_ms: wire.ttl_ms,
        };
        // C3: prefer the broker's relative TTL over its absolute clock.
        if let Some(ttl) = tok.ttl_ms {
            tok.expires = now_millis().saturating_add(ttl);
        }
        // C2: reject a malformed/dead token rather than caching it (which would
        // cause a permanent refetch storm or a dud bearer).
        if tok.access_token.is_empty() {
            return Err(TokenFetchError::Other(
                "broker returned an empty access_token".to_string(),
            ));
        }
        if tok.expires <= now_millis() {
            return Err(TokenFetchError::Other(
                "broker returned an already-expired token".to_string(),
            ));
        }
        Ok(tok)
    }
}

impl TokenFetcher for BrokerClient {
    async fn fetch_token(&self, provider: &str) -> Result<BrokerToken, String> {
        self.fetch_token_query(provider, None)
            .await
            .map_err(|e| e.to_string())
    }

    async fn fetch_credential(&self, cred: &CredentialRef) -> Result<BrokerToken, TokenFetchError> {
        // Explicit slot — ALWAYS sent, including `default`, so the broker
        // host's own policy (which may point at a named or `auto` slot) can
        // never be cached under this client's `default` key.
        self.fetch_token_query(cred.provider.as_str(), Some(cred.account.label_str()))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_parts_local_when_no_endpoint() {
        assert_eq!(
            CredentialSource::from_parts(None, None),
            CredentialSource::Local
        );
        assert_eq!(
            CredentialSource::from_parts(Some("   ".into()), Some("m".into())),
            CredentialSource::Local
        );
    }

    #[test]
    fn from_parts_remote_when_endpoint_set() {
        let s =
            CredentialSource::from_parts(Some("https://jade.jade:8181".into()), Some("tok".into()));
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
            CredentialSource::Remote {
                endpoint: "https://b".into(),
                machine_token: "tok".into()
            }
        );
    }

    #[test]
    fn remote_with_missing_machine_token_defaults_empty() {
        let s = CredentialSource::from_parts(Some("https://b".into()), None);
        assert_eq!(
            s,
            CredentialSource::Remote {
                endpoint: "https://b".into(),
                machine_token: String::new()
            }
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
        BrokerToken {
            access_token: "sk-live".into(),
            expires,
            ttl_ms: None,
        }
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
        let f = FakeFetcher {
            token: tok(0),
            calls: AtomicUsize::new(0),
        };
        let t = resolve_remote_token(&f, &cache, "anthropic", DEFAULT_MARGIN_MS)
            .await
            .unwrap();
        assert_eq!(t, "sk-live");
        assert_eq!(
            f.calls.load(Ordering::SeqCst),
            0,
            "must not fetch on a cache hit"
        );
    }

    #[tokio::test]
    async fn resolve_miss_fetches_then_caches() {
        let cache = TokenCache::new();
        let f = FakeFetcher {
            token: tok(now_millis() + 60 * 60 * 1000),
            calls: AtomicUsize::new(0),
        };
        resolve_remote_token(&f, &cache, "anthropic", DEFAULT_MARGIN_MS)
            .await
            .unwrap();
        resolve_remote_token(&f, &cache, "anthropic", DEFAULT_MARGIN_MS)
            .await
            .unwrap();
        assert_eq!(
            f.calls.load(Ordering::SeqCst),
            1,
            "second resolve should hit the cache"
        );
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
        let t = resolve_remote(&FailFetcher, &cache, "anthropic", DEFAULT_MARGIN_MS)
            .await
            .unwrap();
        assert_eq!(t.access_token, "sk-live");
    }

    #[tokio::test]
    async fn resolve_errors_when_broker_down_and_no_cache() {
        let cache = TokenCache::new();
        let err = resolve_remote(&FailFetcher, &cache, "anthropic", DEFAULT_MARGIN_MS)
            .await
            .unwrap_err();
        assert!(err.contains("unreachable"), "got: {err}");
    }

    #[tokio::test]
    async fn resolve_errors_when_broker_down_and_cache_hard_expired() {
        let cache = TokenCache::new();
        cache.put("anthropic", tok(now_millis().saturating_sub(1000))); // already expired
        let err = resolve_remote(&FailFetcher, &cache, "anthropic", 0)
            .await
            .unwrap_err();
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
        assert!(
            t.expires > now_millis(),
            "expires must come from ttl_ms, got {}",
            t.expires
        );
    }

    #[tokio::test]
    async fn fetch_token_rejects_empty_access_token() {
        let url = spawn_broker(r#"{"access_token":"","expires":9999999999999}"#, "m").await;
        let c = BrokerClient::new(url, "m");
        assert!(c
            .fetch_token("anthropic")
            .await
            .unwrap_err()
            .contains("empty"));
    }

    #[tokio::test]
    async fn fetch_token_rejects_already_expired() {
        let url = spawn_broker(r#"{"access_token":"sk","expires":1}"#, "m").await;
        let c = BrokerClient::new(url, "m");
        assert!(c
            .fetch_token("anthropic")
            .await
            .unwrap_err()
            .contains("expired"));
    }

    #[tokio::test]
    async fn resolve_access_token_remote_fetches_from_broker() {
        let url = spawn_broker(
            r#"{"access_token":"sk-broker","expires":9999999999999}"#,
            "m",
        )
        .await;
        let source = CredentialSource::Remote {
            endpoint: url,
            machine_token: "m".into(),
        };
        let cache = TokenCache::new();
        let http = reqwest::Client::new();
        let t = resolve_access_token("anthropic", &source, &cache, &http)
            .await
            .unwrap();
        assert_eq!(t, "sk-broker");
        // second call hits the cache (shared http client reused, no new pool)
        let t2 = resolve_access_token("anthropic", &source, &cache, &http)
            .await
            .unwrap();
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
