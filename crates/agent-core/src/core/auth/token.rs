use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use tokio::sync::Mutex;

use reqwest::Client;

use super::account::{CredentialRef, SeatIdentity};
use super::provider::RefreshEndpoints;
use super::storage::{
    auth_file_path, load_provider_auth_typed_at, save_provider_auth_if_refresh_matches_typed_at,
    CasOutcome, StoreError,
};
use super::{is_token_expired, now_millis, OAuthCredentials, TokenResponse, CLIENT_ID, TOKEN_URL};

/// Exchange an authorization code for access + refresh tokens.
pub async fn exchange_code_for_tokens(
    code: &str,
    state: &str,
    verifier: &str,
    port: u16,
) -> std::result::Result<OAuthCredentials, String> {
    let redirect_uri = format!("http://localhost:{}/callback", port);

    let body = serde_json::json!({
        "grant_type": "authorization_code",
        "client_id": CLIENT_ID,
        "code": code,
        "state": state,
        "redirect_uri": redirect_uri,
        "code_verifier": verifier,
    });

    let client = Client::builder()
        .tls_built_in_webpki_certs(true)
        .tls_built_in_native_certs(true)
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {}", e))?;
    let resp = client
        .post(TOKEN_URL)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Token exchange request failed: {}", e))?;

    if !resp.status().is_success() {
        let status = resp.status();
        // Server responses may contain sensitive data (a proxy echoing the
        // code/verifier, internal errors): keep only the OAuth error fields.
        // Byte slicing here also panicked on a non-ASCII body.
        let text = resp.text().await.unwrap_or_default();
        return Err(format!(
            "Token exchange failed ({}): {}",
            status,
            sanitize_oauth_error_body(&text)
        ));
    }

    let token_resp: TokenResponse = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse token response: {}", e))?;

    // expires_in is seconds; store as epoch millis with 5-minute buffer (matches Pi/Claude Code)
    let expires = now_millis() + (token_resp.expires_in * 1000) - (5 * 60 * 1000);

    Ok(OAuthCredentials {
        auth_type: "oauth".to_string(),
        refresh: token_resp.refresh_token,
        access: token_resp.access_token,
        expires,
        account_id: None,
    })
}

/// Refresh an expired OAuth token. Failures are typed at the source: no
/// response or an unusable success body is transient, a status is
/// classified by [`status_kind`].
pub async fn refresh_token(
    client: &Client,
    refresh: &str,
) -> std::result::Result<OAuthCredentials, RefreshFailure> {
    refresh_token_at(client, TOKEN_URL, refresh).await
}

pub(crate) async fn refresh_token_at(
    client: &Client,
    url: &str,
    refresh: &str,
) -> std::result::Result<OAuthCredentials, RefreshFailure> {
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "client_id": CLIENT_ID,
        "refresh_token": refresh,
    });

    let resp = client
        .post(url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| RefreshFailure::transport(format!("Token refresh request failed: {}", e)))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(RefreshFailure::status(
            status.as_u16(),
            format!(
                "Token refresh failed ({}): {}",
                status,
                sanitize_oauth_error_body(&text)
            ),
        ));
    }

    // A 200 whose body is not a token response (captive portal HTML, a
    // connection cut mid-body) says nothing about the login.
    let token_resp: TokenResponse = resp
        .json()
        .await
        .map_err(|e| RefreshFailure::bad_response(format!("Failed to parse refresh response: {}", e)))?;

    let expires = now_millis() + (token_resp.expires_in * 1000) - (5 * 60 * 1000);

    Ok(OAuthCredentials {
        auth_type: "oauth".to_string(),
        refresh: token_resp.refresh_token,
        access: token_resp.access_token,
        expires,
        account_id: None,
    })
}

/// Reduce an OAuth token-endpoint error body to its RFC 6749 `error` /
/// `error_description` fields (each capped at 120 chars, char-boundary safe).
/// Anything else — HTML from a captive portal, a proxy echoing the request
/// (which carries the refresh token) — is dropped, never surfaced.
pub(crate) fn sanitize_oauth_error_body(text: &str) -> String {
    fn cap(s: &str) -> String {
        s.chars().take(120).collect()
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return "<non-JSON error body omitted>".to_string();
    };
    // Anthropic nests as {"error":{"type":..,"message":..}}; RFC 6749 is flat.
    let (code, desc) = match v.get("error") {
        Some(serde_json::Value::String(e)) => (
            Some(e.as_str()),
            v.get("error_description").and_then(|d| d.as_str()),
        ),
        Some(serde_json::Value::Object(o)) => (
            o.get("type").and_then(|t| t.as_str()),
            o.get("message").and_then(|m| m.as_str()),
        ),
        _ => (None, None),
    };
    match (code, desc) {
        (Some(c), Some(d)) => format!("{}: {}", cap(c), cap(d)),
        (Some(c), None) => cap(c),
        (None, Some(d)) => cap(d),
        (None, None) => "<error body omitted>".to_string(),
    }
}

/// Acquire the Anthropic refresh gate, check token freshness, refresh if
/// needed, and persist the result atomically. Returns the current (possibly
/// refreshed) credentials.
///
/// Anthropic now flows through the exact same single-flight gate + atomic
/// merge-persistence path as every other OAuth provider
/// (`ensure_fresh_provider_token`), so concurrent async callers can never
/// double-rotate the refresh token, and the write preserves every other
/// provider entry (fixes #184) via `save_provider_auth` → tmp+rename(2).
pub async fn ensure_fresh_token(client: &Client) -> std::result::Result<OAuthCredentials, String> {
    ensure_fresh_provider_token(client, super::provider::OAuthProviderId::Anthropic).await
}

/// Process-wide refresh gates keyed by `(credential file, storage key)`. The
/// file lock protects persistence and cross-process rotation; these gates
/// additionally keep concurrent async callers in this process from rotating
/// the same refresh token while the network request is in flight. Two
/// accounts of one provider have independent gates.
fn refresh_gate(gate_key: &str) -> Arc<Mutex<()>> {
    static GATES: OnceLock<std::sync::Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();
    let mut gates = GATES
        .get_or_init(|| std::sync::Mutex::new(HashMap::new()))
        .lock()
        .expect("refresh gate registry poisoned");
    gates
        .entry(gate_key.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

/// Ensure an OAuth provider (including Anthropic) has a fresh token in its
/// **default** account slot. Equivalent to
/// [`ensure_fresh_credential`] with [`CredentialRef::default_for`].
pub async fn ensure_fresh_provider_token<P>(
    client: &Client,
    provider: P,
) -> std::result::Result<OAuthCredentials, String>
where
    P: TryInto<super::provider::OAuthProviderId>,
    P::Error: std::fmt::Display,
{
    let provider = provider.try_into().map_err(|e| e.to_string())?;
    ensure_fresh_credential(client, &CredentialRef::default_for(provider)).await
}

/// Ensure one addressed credential (provider + account) has a fresh token.
///
/// Single-flight per credential: concurrent async callers in this process
/// serialize on a `(file, storage key)` gate, and concurrent *processes*
/// serialize on an exclusive `flock` of `<auth.json>.<key>.refresh.lock`
/// held across load → refresh → save, so exactly one party ever rotates a
/// given refresh token. Different accounts never wait on each other.
///
/// The rotated credential is written compare-and-swap: if the slot was
/// removed or re-logged-in while the network call was in flight, the stale
/// rotation is not written. A removed slot is an error; a replaced slot is
/// re-loaded and, if needed, refreshed on its own (bounded) so the returned
/// credential is always fresh and always the one that is actually stored.
pub async fn ensure_fresh_credential(
    client: &Client,
    cred: &CredentialRef,
) -> std::result::Result<OAuthCredentials, String> {
    // The rotation is written back to the file the refresh token was READ
    // from. With an active profile that has no auth.json yet, reads fall
    // back to the base file — the rotated token must return there, never be
    // forked into a fresh profile copy (the base copy would be dead).
    let path = auth_file_path();
    ensure_fresh_credential_at(client, cred, &path).await
}

/// Why a seat-pinned vend ([`ensure_fresh_credential_for_seat`]) produced no
/// token.
#[derive(Debug)]
pub enum SeatVendError {
    /// The slot no longer holds the seat the caller's evidence was about
    /// (re-login, removal, or rotation by another party). Nothing was vended.
    SeatChanged,
    Other(String),
}

impl std::fmt::Display for SeatVendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SeatChanged => f.write_str(SEAT_CHANGED),
            Self::Other(e) => f.write_str(e),
        }
    }
}

const SEAT_CHANGED: &str = "credential changed since its capacity was read; nothing vended";

/// Why a locked refresh produced no token, so callers can tell the user
/// the right thing: only [`RefreshFailureKind::Credential`] means the login
/// itself is unusable ("run `synaps login`").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshFailureKind {
    /// A forced (401-driven) refresh was refused by the per-account
    /// [`FORCED_REFRESH_COOLDOWN`].
    Cooldown { retry_in_secs: u64 },
    /// Another refresh of this account (this process or another) did not
    /// finish within the lock wait.
    LockBusy,
    /// The provider exchange failed for a transient reason: network, a
    /// timeout, or a 5xx / 429 from the token endpoint.
    Transient,
    /// The stored credential is missing, removed or rejected by the provider.
    Credential,
    /// Local failure (lock file I/O, the refresh task itself).
    Other,
    /// The user must act outside synaps (a corrupt credential store, a
    /// provider billing requirement); the message says exactly what. Not
    /// retryable, and logging in again does not help.
    ActionRequired,
}

impl RefreshFailureKind {
    /// Retrying later can succeed without the user doing anything: a
    /// transient provider/network failure, a busy refresh lock, or a local
    /// failure. The single definition the broker and the runtime share.
    pub fn is_retryable(self) -> bool {
        matches!(self, Self::Transient | Self::LockBusy | Self::Other)
    }
}

/// A refresh failure: its kind plus a log-safe message (our own texts, or
/// provider errors that went through the provider's sanitizer). Never
/// carries token material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshFailure {
    pub kind: RefreshFailureKind,
    message: String,
}

impl RefreshFailure {
    pub fn new(kind: RefreshFailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for RefreshFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Storage / load / CAS errors (strings from the storage layer and our own
/// slot checks) are credential problems.
impl From<String> for RefreshFailure {
    fn from(message: String) -> Self {
        Self::new(RefreshFailureKind::Credential, message)
    }
}

/// A store lock that stayed held past its deadline is a local failure
/// (`Other`, retry shortly); any other store failure keeps the credential
/// classification.
impl From<StoreError> for RefreshFailure {
    fn from(e: StoreError) -> Self {
        match e {
            // The credential store's own lock, not this account's refresh
            // lock: its message names `auth.json.lock`.
            StoreError::Busy(m) => Self::new(RefreshFailureKind::Other, m),
            StoreError::Corrupt(m) => Self::new(RefreshFailureKind::ActionRequired, m),
            StoreError::Failed(m) => Self::new(RefreshFailureKind::Credential, m),
        }
    }
}

impl From<RefreshFailure> for String {
    fn from(f: RefreshFailure) -> Self {
        f.message
    }
}

/// Kind of a non-success token-endpoint status: request timeout, too
/// early, rate limit and 5xx are transient; every other status (400
/// `invalid_grant`, 401, 403, ...) means the login was rejected.
pub fn status_kind(status: u16) -> RefreshFailureKind {
    match status {
        408 | 425 | 429 | 500..=599 => RefreshFailureKind::Transient,
        _ => RefreshFailureKind::Credential,
    }
}

impl RefreshFailure {
    /// The request produced no usable response (connect, TLS, timeout, a
    /// body cut mid-transfer).
    pub fn transport(message: impl Into<String>) -> Self {
        Self::new(RefreshFailureKind::Transient, message)
    }

    /// The token endpoint answered `status`; see [`status_kind`].
    pub fn status(status: u16, message: impl Into<String>) -> Self {
        Self::new(status_kind(status), message)
    }

    /// A success status whose body is not a usable token response.
    pub fn bad_response(message: impl Into<String>) -> Self {
        Self::new(RefreshFailureKind::Transient, message)
    }

    /// The stored login is missing or was rejected.
    pub fn credential(message: impl Into<String>) -> Self {
        Self::new(RefreshFailureKind::Credential, message)
    }

    /// A local or configuration failure (pinning, client build).
    pub fn other(message: impl Into<String>) -> Self {
        Self::new(RefreshFailureKind::Other, message)
    }
}

/// [`ensure_fresh_credential`] for a slot whose stored credential the caller
/// already inspected (e.g. read its usage): the slot must still hold the same
/// [`SeatIdentity`], checked under the same locks as the refresh, so the
/// returned token is derived from exactly that seat — never from a
/// credential that replaced it in between.
pub async fn ensure_fresh_credential_for_seat(
    client: &Client,
    cred: &CredentialRef,
    expected: &SeatIdentity,
) -> std::result::Result<OAuthCredentials, SeatVendError> {
    let path = auth_file_path();
    ensure_fresh_credential_checked_at(client, cred, &path, Some(expected))
        .await
        .map_err(|e| {
            if e == SEAT_CHANGED {
                SeatVendError::SeatChanged
            } else {
                SeatVendError::Other(e)
            }
        })
}

/// Path-explicit core of [`ensure_fresh_credential`] (tests use temp dirs).
pub(crate) async fn ensure_fresh_credential_at(
    client: &Client,
    cred: &CredentialRef,
    path: &Path,
) -> std::result::Result<OAuthCredentials, String> {
    ensure_fresh_credential_checked_at(client, cred, path, None).await
}

pub(crate) async fn ensure_fresh_credential_checked_at(
    client: &Client,
    cred: &CredentialRef,
    path: &Path,
    expected_seat: Option<&SeatIdentity>,
) -> std::result::Result<OAuthCredentials, String> {
    ensure_credential_checked_at(
        client,
        cred,
        path,
        expected_seat,
        None,
        &RefreshEndpoints::default(),
    )
    .await
    .map_err(String::from)
}

/// Recovery is broker-owned: no refresh credentials cross into the runtime.
/// Typed so the caller can tell a cooldown / busy lock / network failure
/// from an unusable login.
pub(super) async fn recover_rejected_credential(
    client: &Client,
    cred: &CredentialRef,
    rejected: &str,
    endpoints: &RefreshEndpoints,
) -> std::result::Result<OAuthCredentials, RefreshFailure> {
    ensure_credential_checked_at(client, cred, &auth_file_path(), None, Some(rejected), endpoints)
        .await
}

/// [`ensure_fresh_credential`] through `endpoints` (the broker's test
/// seams).
pub(super) async fn ensure_fresh_credential_via(
    client: &Client,
    cred: &CredentialRef,
    endpoints: &RefreshEndpoints,
) -> std::result::Result<OAuthCredentials, RefreshFailure> {
    ensure_credential_checked_at(client, cred, &auth_file_path(), None, None, endpoints).await
}

async fn ensure_credential_checked_at(
    client: &Client,
    cred: &CredentialRef,
    path: &Path,
    expected_seat: Option<&SeatIdentity>,
    rejected: Option<&str>,
    endpoints: &RefreshEndpoints,
) -> std::result::Result<OAuthCredentials, RefreshFailure> {
    // Cheap pre-check without gate, file lock or task: the common
    // fresh-token path stays a single file read. The locked core re-loads
    // and re-decides under both locks.
    let creds = load_checked(path, cred, expected_seat)?;
    let rejected_now = rejected.is_some_and(|r| !r.is_empty() && r == creds.access);
    if !rejected_now && !is_token_expired(&creds) {
        return Ok(creds);
    }
    let (client, cred, path) = (client.clone(), cred.clone(), path.to_path_buf());
    let (seat, rejected) = (expected_seat.cloned(), rejected.map(str::to_string));
    let endpoints = endpoints.clone();
    run_detached(async move {
        let provider = cred.provider;
        let (client, endpoints) = (&client, &endpoints);
        refresh_credential_locked(
            &cred,
            &path,
            seat.as_ref(),
            rejected.as_deref(),
            |refresh: String| async move {
                super::provider::refresh_via(client, provider, &refresh, endpoints).await
            },
        )
        .await
    })
    .await
}

/// Run the locked refresh DETACHED from the caller, on the process-lifetime
/// [`refresh_runtime`]. If the caller is cancelled (Esc mid-request, a
/// dropped turn) or its whole runtime is dropped (a subagent worker's
/// throwaway `current_thread` runtime on timeout/cancel) after the provider
/// has rotated the refresh token but before the CAS save, the refresh still
/// completes and persists it — dropping a rotated refresh token would log
/// the user out. The lock poller's `spawn_blocking` also lives on that
/// runtime, so tearing down a caller's runtime never waits on it.
async fn run_detached<T, E, F>(fut: F) -> std::result::Result<T, E>
where
    F: std::future::Future<Output = std::result::Result<T, E>> + Send + 'static,
    T: Send + 'static,
    E: From<RefreshFailure> + Send + 'static,
{
    run_detached_on(refresh_runtime(), fut).await
}

async fn run_detached_on<T, E, F>(
    runtime: std::result::Result<&'static tokio::runtime::Runtime, RefreshFailure>,
    fut: F,
) -> std::result::Result<T, E>
where
    F: std::future::Future<Output = std::result::Result<T, E>> + Send + 'static,
    T: Send + 'static,
    E: From<RefreshFailure> + Send + 'static,
{
    runtime.map_err(E::from)?.spawn(fut).await.map_err(|e| {
        E::from(RefreshFailure::new(
            RefreshFailureKind::Other,
            format!("credential refresh task failed: {e}"),
        ))
    })?
}

/// Small multi-thread runtime that lives for the whole process and runs
/// every locked refresh (gate, flock poller, provider exchange, CAS save).
/// Callers on any runtime just await the `JoinHandle`. If it cannot be
/// started (thread spawn failure under resource exhaustion) the refresh
/// fails with [`RefreshFailureKind::Other`] instead of aborting the process,
/// and the next refresh tries to build it again (only success is kept, so a
/// long-lived daemon recovers once resources free up).
fn refresh_runtime() -> std::result::Result<&'static tokio::runtime::Runtime, RefreshFailure> {
    static RT: std::sync::Mutex<Option<&'static tokio::runtime::Runtime>> =
        std::sync::Mutex::new(None);
    runtime_from(&RT, || {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("synaps-auth-refresh")
            .enable_all()
            .build()
    })
}

fn runtime_from(
    slot: &std::sync::Mutex<Option<&'static tokio::runtime::Runtime>>,
    build: impl FnOnce() -> std::io::Result<tokio::runtime::Runtime>,
) -> std::result::Result<&'static tokio::runtime::Runtime, RefreshFailure> {
    let mut slot = slot.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(runtime) = *slot {
        return Ok(runtime);
    }
    let runtime = build().map_err(|e| {
        RefreshFailure::other(format!("could not start the auth refresh runtime: {e}"))
    })?;
    // Lives for the process, like the OnceLock it replaces.
    let runtime: &'static tokio::runtime::Runtime = Box::leak(Box::new(runtime));
    *slot = Some(runtime);
    Ok(runtime)
}

fn load_checked(
    path: &Path,
    cred: &CredentialRef,
    expected_seat: Option<&SeatIdentity>,
) -> std::result::Result<OAuthCredentials, RefreshFailure> {
    let creds = load_provider_auth_typed_at(path, &cred.storage_key())?.ok_or_else(|| {
        RefreshFailure::credential(format!(
            "No credentials for {} at {}. Run `synaps login --provider {} --account {}`.",
            cred,
            path.display(),
            cred.provider,
            cred.account
        ))
    })?;
    // Seat pairing is re-checked on every load, including the re-load
    // after a CAS `Replaced` (re-login mid-refresh): the replacement is
    // a different seat and was never proven.
    if let Some(expected) = expected_seat {
        if SeatIdentity::of(cred.provider, &creds) != *expected {
            return Err(RefreshFailure::credential(SEAT_CHANGED));
        }
    }
    Ok(creds)
}

/// Gate → per-credential cross-process lock → CAS save, plus the forced
/// refresh rate limit. `refresh` is the provider exchange (injected so the
/// whole production composition is testable without a network).
async fn refresh_credential_locked<Refresh, RFut, RErr>(
    cred: &CredentialRef,
    path: &Path,
    expected_seat: Option<&SeatIdentity>,
    rejected: Option<&str>,
    refresh: Refresh,
) -> std::result::Result<OAuthCredentials, RefreshFailure>
where
    Refresh: Fn(String) -> RFut,
    RFut: std::future::Future<Output = std::result::Result<OAuthCredentials, RErr>>,
    RErr: Into<RefreshFailure>,
{
    let key = cred.storage_key();
    let gate_key = format!("{}|{}", path.display(), key);
    let lock_path = refresh_lock_path(path, &key);
    let load = || load_checked(path, cred, expected_seat);
    // Off the refresh runtime's two workers (the store lock is polled with
    // a blocking sleep), with the long post-rotation deadline.
    let save = |old_refresh: &str, fresh: &OAuthCredentials| {
        let (path, key) = (path.to_path_buf(), key.clone());
        let (old_refresh, fresh) = (old_refresh.to_string(), fresh.clone());
        async move {
            tokio::task::spawn_blocking(move || {
                save_provider_auth_if_refresh_matches_typed_at(
                    &path,
                    &key,
                    &old_refresh,
                    &fresh,
                    REFRESH_SAVE_WAIT,
                )
            })
            .await
            .map_err(|e| RefreshFailure::other(format!("credential save task failed: {e}")))?
            .map_err(RefreshFailure::from)
        }
    };
    ensure_gated_locked_policy(
        refresh_gate(&gate_key),
        Some(lock_path),
        load,
        refresh,
        save,
        rejected,
        |last_refresh_ms| forced_refresh_allowed(cred, last_refresh_ms, crate::epoch_millis()),
    )
    .await
}

/// Minimum interval between two *forced* (401-driven) refreshes of the same
/// credential. If the provider rejects a token minted less than this long
/// ago, a new token will not help (org / permission / header problem) and
/// spending the refresh token again only widens the rotation race window.
///
/// Per account, across processes: every successful refresh stamps its time
/// into that account's own lock file (`auth.json.<storage_key>.refresh.lock`)
/// while holding the lock, and the check reads it under the same lock. A
/// refresh or `synaps login` of account A therefore never cools down
/// account B, in this process or any other. This is a rate limit only; the
/// flock + CAS save are what prevent a double spend.
pub(crate) const FORCED_REFRESH_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(60);

/// Rate limit for forcing a refresh of a not-yet-expired token.
/// `last_refresh_ms` is the epoch-ms stamp read from this account's lock
/// file (any process's last successful refresh of THIS account). Refused
/// only if that is less than [`FORCED_REFRESH_COOLDOWN`] ago; a missing,
/// unparsable or future-dated stamp allows. Only consulted while storage
/// still holds the rejected token, so an external `synaps login` that
/// changes the token never hits it.
fn forced_refresh_allowed(
    cred: &CredentialRef,
    last_refresh_ms: Option<u64>,
    now_ms: u64,
) -> std::result::Result<(), RefreshFailure> {
    let cooldown_ms = FORCED_REFRESH_COOLDOWN.as_millis() as u64;
    match last_refresh_ms.filter(|t| *t <= now_ms).map(|t| now_ms - t) {
        Some(age_ms) if age_ms < cooldown_ms => {
            let retry_in_secs = (cooldown_ms - age_ms).div_ceil(1000);
            Err(RefreshFailure::new(
                RefreshFailureKind::Cooldown { retry_in_secs },
                format!(
                    "{} rejected a token refreshed {}s ago; not refreshing again for {}s \
                     (likely an account or permission issue, not a stale token)",
                    cred,
                    age_ms / 1000,
                    retry_in_secs
                ),
            ))
        }
        _ => Ok(()),
    }
}

/// Per-credential cross-process lock file, beside the credential file.
/// Storage keys only contain `[a-z0-9._@-]`, so they are safe path fragments.
fn refresh_lock_path(auth_path: &Path, storage_key: &str) -> PathBuf {
    let file_name = auth_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("auth.json");
    auth_path.with_file_name(format!("{file_name}.{storage_key}.refresh.lock"))
}

/// Hard deadline for one provider refresh HTTP exchange while the
/// per-credential lock is held. Bounds the lock hold time regardless of the
/// caller's HTTP client configuration.
const REFRESH_HTTP_DEADLINE: std::time::Duration = std::time::Duration::from_secs(60);

/// Bounded wait for the cross-process refresh lock (another process may be
/// mid-rotation). Strictly longer than `REFRESH_HTTP_DEADLINE` plus the
/// persistence step, so a healthy peer always finishes before a waiter
/// gives up; a wedged peer cannot hang this process forever.
const REFRESH_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(120);

/// Deadline for the CAS save that follows a provider exchange. The
/// provider may already have rotated (spent) the old refresh token, so
/// giving up loses the login: wait as long as any refresh-lock waiter would.
/// Login and static-key saves keep the short `STORE_LOCK_WAIT`.
const REFRESH_SAVE_WAIT: std::time::Duration = REFRESH_LOCK_WAIT;

/// Sidecar for a rotation whose save failed, beside the account's lock.
fn pending_path(lock_path: &Path) -> PathBuf {
    lock_path.with_extension("pending")
}

/// The save after a rotation failed. Park `fresh` in the 0600 sidecar
/// (under the held refresh lock) and hand it out; the next locked load
/// applies it. Without a lock, or if parking fails too, the rotation is lost.
fn park_unsaved_rotation(
    lock: Option<&RefreshFileLock>,
    old_refresh: &str,
    fresh: OAuthCredentials,
    err: RefreshFailure,
) -> std::result::Result<OAuthCredentials, RefreshFailure> {
    let Some(lock) = lock else { return Err(err) };
    match lock.park_pending(old_refresh, &fresh) {
        Ok(()) => {
            tracing::warn!(
                lock = %lock.path.display(),
                error = %err,
                "refreshed login could not be saved; parked it beside the refresh lock, \
                 it will be applied on next use"
            );
            Ok(fresh)
        }
        Err(e) => {
            tracing::error!(
                lock = %lock.path.display(),
                save_error = %err,
                park_error = %e,
                "refreshed login could NOT be saved: if the provider rotated the refresh \
                 token, this account is now logged out and needs `synaps login`"
            );
            Err(err)
        }
    }
}

/// Guard holding the exclusive `flock`; dropping it releases the lock.
///
/// The lock file's content is this account's last-refresh stamp (epoch ms,
/// plain ASCII), read and written only while the lock is held. Older
/// clients open the same file with `truncate(false)` and never write it, so
/// the stamp survives them.
struct RefreshFileLock {
    file: std::fs::File,
    /// Lock file path: keys the in-process [`refresh_floor`].
    path: PathBuf,
}

/// In-process last-refresh floor per refresh-lock path, recorded only when
/// the on-disk stamp could not be written. Without it a failed stamp
/// (ENOSPC, a read-only lock file) would silently switch the forced-refresh
/// cooldown off and every 401 would rotate / re-mint again.
fn refresh_floor() -> &'static std::sync::Mutex<HashMap<PathBuf, u64>> {
    static FLOOR: OnceLock<std::sync::Mutex<HashMap<PathBuf, u64>>> = OnceLock::new();
    FLOOR.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

impl RefreshFileLock {
    /// Last successful refresh of this account: the on-disk stamp (any
    /// process), or this process's floor if that is later.
    fn last_refresh_ms(&self) -> Option<u64> {
        let floor = refresh_floor()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&self.path)
            .copied();
        self.stamped_ms().max(floor)
    }

    fn stamped_ms(&self) -> Option<u64> {
        use std::io::{Read, Seek};
        let mut file = &self.file;
        file.rewind().ok()?;
        let mut text = String::new();
        file.take(32).read_to_string(&mut text).ok()?;
        text.trim().parse().ok()
    }

    /// Record a successful refresh. Never fails a refresh that is already
    /// persisted: if the stamp cannot be written, warn and keep the time
    /// in the in-process floor instead.
    fn record_refreshed(&self, now_ms: u64) {
        let mut floor = refresh_floor().lock().unwrap_or_else(|e| e.into_inner());
        match self.stamp_refreshed(now_ms) {
            Ok(()) => {
                floor.remove(&self.path);
            }
            Err(e) => {
                tracing::warn!(
                    lock = %self.path.display(),
                    error = %e,
                    "could not stamp the refresh lock file; keeping the forced-refresh \
                     cooldown in this process only"
                );
                floor.insert(self.path.clone(), now_ms);
            }
        }
    }

    /// Write `{expected_refresh, credential}` to the 0600 sidecar
    /// (tmp + rename). Only called while holding this lock.
    fn park_pending(&self, expected_refresh: &str, fresh: &OAuthCredentials) -> std::io::Result<()> {
        use std::io::Write;
        let body = serde_json::json!({ "expected_refresh": expected_refresh, "credential": fresh });
        let path = pending_path(&self.path);
        let tmp = path.with_extension("pending.tmp");
        let mut open = std::fs::OpenOptions::new();
        open.create(true).write(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            open.mode(0o600);
        }
        let mut file = open.open(&tmp)?;
        file.write_all(body.to_string().as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&tmp, &path)
    }

    /// CAS a parked rotation into the store. Applied or stale (the slot was
    /// re-logged-in or removed) deletes the sidecar; a failed save keeps it
    /// and fails, so the spent refresh token it replaced is never used.
    async fn apply_pending<S, SFut>(&self, save: &S) -> std::result::Result<(), RefreshFailure>
    where
        S: Fn(&str, &OAuthCredentials) -> SFut,
        SFut: std::future::Future<Output = std::result::Result<CasOutcome, RefreshFailure>>,
    {
        let path = pending_path(&self.path);
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Ok(());
        };
        let parked = serde_json::from_str::<serde_json::Value>(&text).ok().and_then(|v| {
            let expected = v.get("expected_refresh")?.as_str()?.to_string();
            let cred = serde_json::from_value::<OAuthCredentials>(v.get("credential")?.clone()).ok()?;
            Some((expected, cred))
        });
        if let Some((expected, cred)) = parked {
            save(&expected, &cred).await.map_err(|e| {
                RefreshFailure::other(format!(
                    "a refreshed login is waiting in {} and could not be applied yet ({e}); retry shortly",
                    path.display()
                ))
            })?;
        }
        let _ = std::fs::remove_file(&path);
        Ok(())
    }

    fn stamp_refreshed(&self, now_ms: u64) -> std::io::Result<()> {
        use std::io::{Seek, Write};
        let mut file = &self.file;
        file.set_len(0)?;
        file.rewind()?;
        file.write_all(now_ms.to_string().as_bytes())
    }
}

/// Acquire the per-credential refresh lock off the async runtime (flock
/// blocks). Polls `try_lock_exclusive` so a wedged peer cannot hang this
/// process forever.
async fn acquire_refresh_lock(
    path: PathBuf,
    wait: std::time::Duration,
) -> std::result::Result<RefreshFileLock, RefreshFailure> {
    tokio::task::spawn_blocking(move || {
        use fs4::fs_std::FileExt;
        let other = |m: String| RefreshFailure::new(RefreshFailureKind::Other, m);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| other(format!("Failed to create {}: {}", parent.display(), e)))?;
        }
        let mut open = std::fs::OpenOptions::new();
        // Never truncate on open: the content is the refresh stamp, and
        // must also survive older clients' opens of the same lock.
        open.create(true).read(true).write(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            open.mode(0o600);
        }
        let file = open
            .open(&path)
            .map_err(|e| other(format!("Failed to open refresh lock {}: {}", path.display(), e)))?;
        let deadline = std::time::Instant::now() + wait;
        loop {
            match FileExt::try_lock_exclusive(&file) {
                Ok(true) => return Ok(RefreshFileLock { file, path }),
                Ok(false) => {}
                Err(e) => {
                    return Err(other(format!(
                        "Failed to lock refresh lock {}: {}",
                        path.display(),
                        e
                    )))
                }
            }
            if std::time::Instant::now() >= deadline {
                return Err(RefreshFailure::new(
                    RefreshFailureKind::LockBusy,
                    format!(
                        "timed out waiting for another process to finish refreshing ({})",
                        path.display()
                    ),
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    })
    .await
    .map_err(|e| RefreshFailure::new(RefreshFailureKind::Other, format!("refresh lock task failed: {e}")))?
}

/// Generic single-flight refresh core. Separated from network/storage so the
/// concurrency, rotation, and failure invariants are directly testable.
///
/// Contract:
/// 1. Exactly one refresh runs at a time per gate; waiters re-`load` after the
///    gate and observe the rotated credential without a second refresh.
/// 2. `save` runs only after a successful refresh (failures persist nothing).
/// 3. The refreshed credential is persisted before the gate is released.
///
/// Production goes through [`ensure_fresh_gated_locked`] (same contract plus
/// the cross-process lock and CAS save); this variant remains as the
/// reference core for the concurrency tests.
#[cfg(test)]
pub(crate) async fn ensure_fresh_gated<Load, Refresh, RFut, Save>(
    gate: Arc<Mutex<()>>,
    load: Load,
    refresh: Refresh,
    save: Save,
) -> std::result::Result<OAuthCredentials, String>
where
    Load: Fn() -> std::result::Result<OAuthCredentials, String>,
    Refresh: FnOnce(String) -> RFut,
    RFut: std::future::Future<Output = std::result::Result<OAuthCredentials, String>>,
    Save: FnOnce(&OAuthCredentials) -> std::result::Result<(), String>,
{
    let _gate = gate.lock_owned().await;
    // Re-read after entering the gate: a preceding waiter may have refreshed
    // and atomically persisted a rotated refresh token.
    let creds = load()?;
    if !is_token_expired(&creds) {
        return Ok(creds);
    }
    let fresh = refresh(creds.refresh).await?;
    save(&fresh)?;
    Ok(fresh)
}

/// Upper bound on load→refresh→CAS rounds when the slot keeps being replaced
/// underneath us (re-login storms). Two rounds cover the realistic case.
const MAX_CAS_ROUNDS: usize = 2;

/// Single-flight refresh with an optional cross-process file lock and a
/// compare-and-swap save. Extends [`ensure_fresh_gated`]'s contract with:
/// 4. The cross-process lock is held for the whole load → refresh → save
///    cycle, so two processes never rotate the same refresh token.
/// 5. `save(old_refresh, fresh)` reports whether the slot still held
///    `old_refresh`. `Removed` is an error (nothing is resurrected);
///    `Replaced` re-loads the stored credential and, if it is expired,
///    refreshes *that* one — bounded — so the result is never a stale or
///    orphaned credential.
/// 6. Metadata (`accountId`) is carried over when the provider's refresh
///    response omits it.
#[cfg(test)]
pub(crate) async fn ensure_fresh_gated_locked<Load, Refresh, RFut, Save>(
    gate: Arc<Mutex<()>>,
    lock_path: Option<PathBuf>,
    load: Load,
    refresh: Refresh,
    save: Save,
) -> std::result::Result<OAuthCredentials, String>
where
    Load: Fn() -> std::result::Result<OAuthCredentials, String>,
    Refresh: Fn(String) -> RFut,
    RFut: std::future::Future<Output = std::result::Result<OAuthCredentials, String>>,
    Save: Fn(&str, &OAuthCredentials) -> std::result::Result<CasOutcome, String>,
{
    let save = |old: &str, fresh: &OAuthCredentials| {
        std::future::ready(save(old, fresh).map_err(RefreshFailure::from))
    };
    let load = || load().map_err(RefreshFailure::from);
    ensure_gated_locked_policy(gate, lock_path, load, refresh, save, None, |_| Ok(()))
        .await
        .map_err(String::from)
}

/// `rejected` is an access token the provider just refused (HTTP 401). If
/// the stored credential still carries it, a refresh is forced even though
/// its expiry is in the future (revoked-but-unexpired) — provided
/// `may_force` allows it (per-credential rate limit,
/// [`FORCED_REFRESH_COOLDOWN`]; it receives the last-refresh stamp read from
/// the held lock file, `None` without a lock or stamp). An expired
/// credential always refreshes without consulting `may_force`. Every
/// successful save stamps the lock file before the lock is released.
async fn ensure_gated_locked_policy<Load, Refresh, RFut, RErr, Save, SFut, MayForce>(
    gate: Arc<Mutex<()>>,
    lock_path: Option<PathBuf>,
    load: Load,
    refresh: Refresh,
    save: Save,
    rejected: Option<&str>,
    may_force: MayForce,
) -> std::result::Result<OAuthCredentials, RefreshFailure>
where
    Load: Fn() -> std::result::Result<OAuthCredentials, RefreshFailure>,
    Refresh: Fn(String) -> RFut,
    RFut: std::future::Future<Output = std::result::Result<OAuthCredentials, RErr>>,
    RErr: Into<RefreshFailure>,
    Save: Fn(&str, &OAuthCredentials) -> SFut,
    SFut: std::future::Future<Output = std::result::Result<CasOutcome, RefreshFailure>>,
    MayForce: Fn(Option<u64>) -> std::result::Result<(), RefreshFailure>,
{
    // In-process waiters are bounded like cross-process ones.
    let _gate = tokio::time::timeout(REFRESH_LOCK_WAIT, gate.lock_owned())
        .await
        .map_err(|_| {
            RefreshFailure::new(
                RefreshFailureKind::LockBusy,
                "timed out waiting for an in-flight token refresh",
            )
        })?;
    let file_lock = match lock_path {
        Some(path) => Some(acquire_refresh_lock(path, REFRESH_LOCK_WAIT).await?),
        None => None,
    };
    // A rotation an earlier save could not persist: apply it before
    // anything else, so its spent refresh token is never sent again.
    if let Some(lock) = &file_lock {
        lock.apply_pending(&save).await?;
    }
    for _ in 0..MAX_CAS_ROUNDS {
        // Re-read under both locks: a preceding waiter (or another process)
        // may have refreshed and atomically persisted a rotated token.
        let creds = load()?;
        // A peer already rotated/replaced the rejected token: adopt it, do
        // not force another rotation. Otherwise refresh even if unexpired.
        let expired = is_token_expired(&creds);
        let still_rejected = rejected.is_some_and(|r| !r.is_empty() && creds.access == r);
        if !expired && !still_rejected {
            return Ok(creds);
        }
        if !expired {
            // Forced (rejection-driven) refresh of an unexpired credential.
            may_force(file_lock.as_ref().and_then(RefreshFileLock::last_refresh_ms))?;
        }
        let old_refresh = creds.refresh.clone();
        // The provider exchange is deadline-bounded so the lock hold time is
        // bounded too (a slow-drip response cannot block every other process).
        let mut fresh = tokio::time::timeout(REFRESH_HTTP_DEADLINE, refresh(old_refresh.clone()))
            .await
            .map_err(|_| RefreshFailure::new(RefreshFailureKind::Transient, "token refresh timed out"))?
            .map_err(Into::into)?;
        if fresh.account_id.is_none() {
            fresh.account_id = creds.account_id.clone();
        }
        let saved = match save(&old_refresh, &fresh).await {
            Ok(saved) => saved,
            Err(e) => return park_unsaved_rotation(file_lock.as_ref(), &old_refresh, fresh, e),
        };
        match saved {
            CasOutcome::Saved => {
                if let Some(lock) = &file_lock {
                    lock.record_refreshed(crate::epoch_millis());
                }
                return Ok(fresh);
            }
            CasOutcome::Removed => {
                return Err(
                    "credential was removed while its token was being refreshed; run `synaps login`"
                        .to_string()
                        .into(),
                )
            }
            CasOutcome::Replaced => {
                // A re-login replaced this slot mid-refresh: the rotation we
                // hold is orphaned. Loop: load the replacement and refresh it
                // only if it is actually expired.
                continue;
            }
        }
    }
    Err(RefreshFailure::new(
        RefreshFailureKind::Other,
        "credential kept changing during refresh; retry",
    ))
}

#[cfg(test)]
mod tests {
    use super::super::storage::save_provider_auth_at_test_hook;
    use super::super::OAuthCredentials;

    fn fresh_creds(refresh: &str) -> OAuthCredentials {
        OAuthCredentials {
            auth_type: "oauth".to_string(),
            refresh: refresh.to_string(),
            access: "access".to_string(),
            expires: crate::epoch_millis() + 3_600_000,
            account_id: None,
        }
    }

    // ── Bug #2 atomicity regression suite ─────────────────────────────────────
    //
    // Root cause (pre-fix): ensure_fresh_token used seek(0)+set_len(0)+write_all
    // on the live auth.json file (token.rs ~139-168). A crash between truncate
    // and write completion zeroed the file. Because Anthropic rotates the
    // refresh token on every refresh, the zeroed-file state is unrecoverable.
    //
    // Fix: the write goes through save_provider_auth → save_provider_auth_at
    // (storage.rs), which does write-to-tmp + rename(2). rename(2) is atomic:
    // the file is either the old content or the new content, never empty.
    //
    // These tests exercise save_provider_auth_at directly (the same function
    // ensure_fresh_token now delegates to) and prove the atomicity contract.

    /// After a successful save, auth.json must not be empty or zero-length.
    /// This would be violated by the old truncate-in-place path on a crash.
    #[test]
    fn bug2_write_produces_non_empty_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");
        save_provider_auth_at_test_hook(&path, "anthropic", &fresh_creds("tok1"))
            .expect("save must succeed");
        let meta = std::fs::metadata(&path).expect("file must exist");
        assert!(meta.len() > 0, "auth.json must not be empty after save");
    }

    /// The write must go through a .json.tmp file, not truncate-in-place.
    /// We prove this by checking that after a successful save the tmp file
    /// is gone (renamed → auth.json) and the final file is valid JSON.
    /// The tmp file should not persist after a successful write.
    #[test]
    fn bug2_tmp_file_cleaned_up_after_successful_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");
        save_provider_auth_at_test_hook(&path, "anthropic", &fresh_creds("tok2"))
            .expect("save must succeed");
        let tmp = path.with_extension("json.tmp");
        assert!(
            !tmp.exists(),
            "auth.json.tmp must not persist after a successful atomic rename"
        );
    }

    /// The final file must be valid JSON with the correct credential.
    /// Truncate-in-place failure would leave `{}` or `null`; atomic rename
    /// either keeps old content or produces fully-written new content.
    #[test]
    fn bug2_final_file_is_valid_json_with_correct_credential() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");
        save_provider_auth_at_test_hook(&path, "anthropic", &fresh_creds("tok3"))
            .expect("save must succeed");
        let content = std::fs::read_to_string(&path).expect("must be readable");
        let parsed: serde_json::Value =
            serde_json::from_str(&content).expect("must be valid JSON after atomic write");
        assert_eq!(
            parsed["anthropic"]["refresh"].as_str(),
            Some("tok3"),
            "refresh token must be persisted correctly"
        );
    }

    /// Refresh write must preserve other providers (fixes #184 / AuthFile
    /// 2-field issue). Old code rebuilt `AuthFile { anthropic, openai_codex }`
    /// which drops any 3rd provider; new code does read-merge-write.
    #[test]
    fn bug2_refresh_write_preserves_other_providers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");

        // Simulate state after two prior logins
        save_provider_auth_at_test_hook(&path, "openai-codex", &fresh_creds("codex-tok"))
            .expect("save codex");
        save_provider_auth_at_test_hook(&path, "future-provider", &fresh_creds("future-tok"))
            .expect("save future");

        // Simulate ensure_fresh_token writing the refreshed anthropic credential
        save_provider_auth_at_test_hook(&path, "anthropic", &fresh_creds("anth-new"))
            .expect("save anthropic after refresh");

        let content = std::fs::read_to_string(&path).expect("readable");
        let parsed: serde_json::Value = serde_json::from_str(&content).expect("valid JSON");

        assert_eq!(parsed["anthropic"]["refresh"].as_str(), Some("anth-new"));
        assert_eq!(
            parsed["openai-codex"]["refresh"].as_str(),
            Some("codex-tok"),
            "openai-codex must survive the anthropic refresh write"
        );
        assert_eq!(
            parsed["future-provider"]["refresh"].as_str(),
            Some("future-tok"),
            "future-provider must survive (fixes #184 — 3rd provider not dropped)"
        );
    }

    // ── Single-flight refresh invariants (concurrency / rotation / failure) ──
    //
    // These drive `ensure_fresh_gated` — the exact core `ensure_fresh_token`
    // (Anthropic) and `ensure_fresh_provider_token` (all providers) execute —
    // with injected load/refresh/save so no network or real auth.json is
    // involved.

    use super::ensure_fresh_gated;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex as StdMutex};

    fn expired_creds(refresh: &str) -> OAuthCredentials {
        OAuthCredentials {
            auth_type: "oauth".to_string(),
            refresh: refresh.to_string(),
            access: "stale-access".to_string(),
            expires: 0,
            account_id: None,
        }
    }

    /// Shared fake persistence: models auth.json for a single provider.
    #[derive(Clone)]
    struct FakeStore {
        creds: Arc<StdMutex<OAuthCredentials>>,
        refresh_calls: Arc<AtomicUsize>,
        save_calls: Arc<AtomicUsize>,
    }

    impl FakeStore {
        fn new(initial: OAuthCredentials) -> Self {
            Self {
                creds: Arc::new(StdMutex::new(initial)),
                refresh_calls: Arc::new(AtomicUsize::new(0)),
                save_calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        async fn ensure(
            &self,
            gate: Arc<tokio::sync::Mutex<()>>,
        ) -> Result<OAuthCredentials, String> {
            let store = self.clone();
            let refresh_calls = self.refresh_calls.clone();
            ensure_fresh_gated(
                gate,
                move || Ok(store.creds.lock().unwrap().clone()),
                move |old_refresh| async move {
                    refresh_calls.fetch_add(1, Ordering::SeqCst);
                    // Rotate: new refresh token derived from the one presented.
                    Ok(OAuthCredentials {
                        auth_type: "oauth".to_string(),
                        refresh: format!("{old_refresh}-rotated"),
                        access: "fresh-access".to_string(),
                        expires: crate::epoch_millis() + 3_600_000,
                        account_id: None,
                    })
                },
                |c| {
                    self.save_calls.fetch_add(1, Ordering::SeqCst);
                    *self.creds.lock().unwrap() = c.clone();
                    Ok(())
                },
            )
            .await
        }
    }

    /// Concurrency: N concurrent callers with an expired credential perform
    /// exactly ONE network refresh; everyone observes the rotated result.
    #[tokio::test]
    async fn anthropic_refresh_single_flight_under_concurrency() {
        let store = FakeStore::new(expired_creds("r0"));
        let gate = Arc::new(tokio::sync::Mutex::new(()));
        let mut handles = Vec::new();
        for _ in 0..16 {
            let store = store.clone();
            let gate = gate.clone();
            handles.push(tokio::spawn(async move { store.ensure(gate).await }));
        }
        for h in handles {
            let creds = h.await.unwrap().expect("refresh must succeed");
            assert_eq!(creds.access, "fresh-access");
            assert_eq!(creds.refresh, "r0-rotated");
        }
        assert_eq!(
            store.refresh_calls.load(Ordering::SeqCst),
            1,
            "exactly one caller may rotate the refresh token"
        );
        assert_eq!(store.save_calls.load(Ordering::SeqCst), 1);
    }

    /// Rotation: the persisted refresh token is the rotated one, and a later
    /// expiry cycle refreshes FROM the rotated token (never the stale one).
    #[tokio::test]
    async fn refresh_rotation_chains_from_persisted_token() {
        let store = FakeStore::new(expired_creds("gen0"));
        let gate = Arc::new(tokio::sync::Mutex::new(()));
        store.ensure(gate.clone()).await.unwrap();
        assert_eq!(store.creds.lock().unwrap().refresh, "gen0-rotated");

        // Force expiry again; the second cycle must present the rotated token.
        store.creds.lock().unwrap().expires = 0;
        let second = store.ensure(gate).await.unwrap();
        assert_eq!(second.refresh, "gen0-rotated-rotated");
        assert_eq!(store.refresh_calls.load(Ordering::SeqCst), 2);
    }

    /// Freshness: an unexpired credential is returned without refresh or save.
    #[tokio::test]
    async fn fresh_credential_short_circuits_without_refresh() {
        let store = FakeStore::new(fresh_creds("keep"));
        let gate = Arc::new(tokio::sync::Mutex::new(()));
        let creds = store.ensure(gate).await.unwrap();
        assert_eq!(creds.refresh, "keep");
        assert_eq!(store.refresh_calls.load(Ordering::SeqCst), 0);
        assert_eq!(store.save_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn oauth_error_body_is_sanitized_and_utf8_safe() {
        use super::sanitize_oauth_error_body as s;
        assert_eq!(
            s(r#"{"error":"invalid_grant","error_description":"Refresh token revoked"}"#),
            "invalid_grant: Refresh token revoked"
        );
        assert_eq!(
            s(r#"{"type":"error","error":{"type":"authentication_error","message":"bad"}}"#),
            "authentication_error: bad"
        );
        // A proxy echoing the request body must not leak the refresh token.
        let echoed = s(r#"{"grant_type":"refresh_token","refresh_token":"rt-SECRET"}"#);
        assert!(!echoed.contains("rt-SECRET"), "{echoed}");
        let html = format!("<html>{}</html>", "é".repeat(300));
        assert_eq!(s(&html), "<non-JSON error body omitted>");
        // Multi-byte chars at the cap boundary never panic.
        let long = format!(r#"{{"error":"{}"}}"#, "é".repeat(500));
        assert_eq!(s(&long).chars().count(), 120);
        // The pre-sanitizer exchange path byte-sliced at 200, which panics
        // when a multi-byte char straddles the cut.
        let straddle = format!("{}é{}", "a".repeat(199), "b".repeat(50));
        assert!(!straddle.is_char_boundary(200));
        assert_eq!(s(&straddle), "<non-JSON error body omitted>");
    }
    /// Failure: a refresh error propagates and persists nothing — the stored
    /// credential is untouched so a later retry can still present it.
    #[tokio::test]
    async fn refresh_failure_persists_nothing() {
        let saves = Arc::new(AtomicUsize::new(0));
        let saves2 = saves.clone();
        let err = ensure_fresh_gated(
            Arc::new(tokio::sync::Mutex::new(())),
            || Ok(expired_creds("r-fail")),
            |_r| async move { Err("provider 500".to_string()) },
            move |_c| {
                saves2.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        )
        .await
        .unwrap_err();
        assert!(err.contains("provider 500"));
        assert_eq!(
            saves.load(Ordering::SeqCst),
            0,
            "failed refresh must not persist"
        );
    }

    // ── Account-aware refresh: gates, cross-process lock, CAS (G1) ───────────

    use super::super::account::{Account, CredentialRef};
    use super::super::provider::OAuthProviderId;
    use super::super::storage::{
        load_provider_auth_at, remove_key_at, save_provider_auth_at,
        save_provider_auth_if_refresh_matches_at,
    };
    use super::{ensure_fresh_gated_locked, refresh_lock_path, CasOutcome};

    fn named(label: &str) -> CredentialRef {
        CredentialRef::new(OAuthProviderId::OpenAiCodex, Account::named(label).unwrap())
    }

    /// Drives `ensure_fresh_gated_locked` against a real temp auth.json with a
    /// fake provider that rotates tokens and records which refresh token it
    /// was presented.
    async fn ensure_on_file(
        path: &std::path::Path,
        cred: &CredentialRef,
        presented: Arc<StdMutex<Vec<String>>>,
        delay: std::time::Duration,
    ) -> Result<OAuthCredentials, String> {
        let key = cred.storage_key();
        let gate = super::refresh_gate(&format!("{}|{}", path.display(), key));
        let lock = refresh_lock_path(path, &key);
        let load_path = path.to_path_buf();
        let load_key = key.clone();
        ensure_fresh_gated_locked(
            gate,
            Some(lock),
            move || {
                load_provider_auth_at(&load_path, &load_key)?.ok_or_else(|| "missing".to_string())
            },
            |old| {
                let presented = presented.clone();
                async move {
                    presented.lock().unwrap().push(old.clone());
                    tokio::time::sleep(delay).await;
                    Ok(OAuthCredentials {
                        auth_type: "oauth".into(),
                        refresh: format!("{old}-rotated"),
                        access: format!("{old}-access"),
                        expires: crate::epoch_millis() + 3_600_000,
                        account_id: None,
                    })
                }
            },
            |old, fresh| save_provider_auth_if_refresh_matches_at(path, &key, old, fresh),
        )
        .await
    }

    /// Two accounts of one provider refresh independently and concurrently:
    /// each rotates exactly once from its own refresh token, and neither
    /// write disturbs the other slot (or the default slot).
    #[tokio::test]
    async fn two_accounts_refresh_independently_without_cross_talk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        save_provider_auth_at(&path, "openai-codex", &fresh_creds("default-r")).unwrap();
        save_provider_auth_at(&path, "openai-codex@a", &expired_creds("a-r")).unwrap();
        save_provider_auth_at(&path, "openai-codex@b", &expired_creds("b-r")).unwrap();
        let presented = Arc::new(StdMutex::new(Vec::new()));
        let mut handles = Vec::new();
        for label in ["a", "b", "a", "b", "a", "b"] {
            let path = path.clone();
            let presented = presented.clone();
            handles.push(tokio::spawn(async move {
                ensure_on_file(&path, &named(label), presented, std::time::Duration::from_millis(30))
                    .await
                    .unwrap()
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let mut seen = presented.lock().unwrap().clone();
        seen.sort();
        assert_eq!(seen, vec!["a-r".to_string(), "b-r".to_string()], "one rotation per account");
        let a = load_provider_auth_at(&path, "openai-codex@a").unwrap().unwrap();
        let b = load_provider_auth_at(&path, "openai-codex@b").unwrap().unwrap();
        let d = load_provider_auth_at(&path, "openai-codex").unwrap().unwrap();
        assert_eq!(a.refresh, "a-r-rotated");
        assert_eq!(b.refresh, "b-r-rotated");
        assert_eq!(d.refresh, "default-r", "default slot untouched");
    }

    /// Cross-process rotation lock: while another "process" holds the
    /// per-credential flock, this refresh waits; a second account's lock is
    /// unaffected.
    #[tokio::test]
    async fn refresh_waits_for_cross_process_lock_of_same_credential_only() {
        use fs4::fs_std::FileExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        save_provider_auth_at(&path, "openai-codex@a", &expired_creds("a-r")).unwrap();
        save_provider_auth_at(&path, "openai-codex@b", &expired_creds("b-r")).unwrap();
        // Foreign holder of account a's lock (separate fd, like another process).
        let lock_a = refresh_lock_path(&path, "openai-codex@a");
        let holder = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_a)
            .unwrap();
        FileExt::lock_exclusive(&holder).unwrap();

        let presented = Arc::new(StdMutex::new(Vec::new()));
        // Account b proceeds immediately.
        let started = std::time::Instant::now();
        ensure_on_file(&path, &named("b"), presented.clone(), std::time::Duration::ZERO)
            .await
            .unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(5));

        // Account a blocks until the holder releases.
        let path_a = path.clone();
        let presented_a = presented.clone();
        let task = tokio::spawn(async move {
            ensure_on_file(&path_a, &named("a"), presented_a, std::time::Duration::ZERO).await
        });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(
            !presented.lock().unwrap().contains(&"a-r".to_string()),
            "must not rotate while another process holds the credential lock"
        );
        // Simulate the other process finishing its rotation before releasing.
        save_provider_auth_at(&path, "openai-codex@a", &fresh_creds("a-r-by-peer")).unwrap();
        FileExt::unlock(&holder).unwrap();
        let result = task.await.unwrap().unwrap();
        assert_eq!(result.refresh, "a-r-by-peer", "waiter re-reads the peer's rotation");
        assert!(
            !presented.lock().unwrap().contains(&"a-r".to_string()),
            "no second rotation of an already-rotated token"
        );
    }

    /// Removal during refresh: the stale rotation is never written back and
    /// the caller gets an error rather than a resurrected slot.
    #[tokio::test]
    async fn removal_during_refresh_is_not_resurrected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        save_provider_auth_at(&path, "openai-codex@a", &expired_creds("a-r")).unwrap();
        save_provider_auth_at(&path, "openai-codex@b", &fresh_creds("b-r")).unwrap();
        let presented = Arc::new(StdMutex::new(Vec::new()));
        let path2 = path.clone();
        let task = tokio::spawn(async move {
            ensure_on_file(&path2, &named("a"), presented, std::time::Duration::from_millis(300)).await
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(remove_key_at(&path, "openai-codex@a").unwrap());
        let err = task.await.unwrap().unwrap_err();
        assert!(err.contains("removed"), "{err}");
        assert!(load_provider_auth_at(&path, "openai-codex@a").unwrap().is_none());
        assert_eq!(
            load_provider_auth_at(&path, "openai-codex@b").unwrap().unwrap().refresh,
            "b-r"
        );
    }

    /// Re-login during refresh: the newly stored credential wins; the
    /// orphaned rotation is dropped and the result is the fresh replacement.
    #[tokio::test]
    async fn relogin_during_refresh_returns_fresh_replacement_not_stale_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        save_provider_auth_at(&path, "openai-codex@a", &expired_creds("a-r")).unwrap();
        let presented = Arc::new(StdMutex::new(Vec::new()));
        let path2 = path.clone();
        let presented2 = presented.clone();
        let task = tokio::spawn(async move {
            ensure_on_file(&path2, &named("a"), presented2, std::time::Duration::from_millis(300)).await
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        save_provider_auth_at(&path, "openai-codex@a", &fresh_creds("relogin-r")).unwrap();
        let result = task.await.unwrap().unwrap();
        assert_eq!(result.refresh, "relogin-r");
        assert_eq!(
            load_provider_auth_at(&path, "openai-codex@a").unwrap().unwrap().refresh,
            "relogin-r"
        );
        assert_eq!(presented.lock().unwrap().as_slice(), ["a-r"], "no refresh of the fresh replacement");
    }

    /// Re-login during refresh with an already-expired replacement: the
    /// replacement itself is refreshed (bounded) — never an expired result.
    #[tokio::test]
    async fn expired_replacement_is_refreshed_not_returned() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        save_provider_auth_at(&path, "openai-codex@a", &expired_creds("a-r")).unwrap();
        let presented = Arc::new(StdMutex::new(Vec::new()));
        let path2 = path.clone();
        let presented2 = presented.clone();
        let task = tokio::spawn(async move {
            ensure_on_file(&path2, &named("a"), presented2, std::time::Duration::from_millis(300)).await
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        save_provider_auth_at(&path, "openai-codex@a", &expired_creds("relogin-expired")).unwrap();
        let result = task.await.unwrap().unwrap();
        assert!(!super::is_token_expired(&result));
        assert_eq!(result.refresh, "relogin-expired-rotated");
        assert_eq!(
            presented.lock().unwrap().as_slice(),
            ["a-r", "relogin-expired"]
        );
    }

    /// The rotated credential inherits `accountId` when the provider's
    /// refresh response omits it (Codex header pairing must survive refresh).
    #[tokio::test]
    async fn refresh_preserves_account_id_when_response_omits_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let mut stored = expired_creds("a-r");
        stored.account_id = Some("acct-keep".into());
        save_provider_auth_at(&path, "openai-codex@a", &stored).unwrap();
        let result = ensure_on_file(
            &path,
            &named("a"),
            Arc::new(StdMutex::new(Vec::new())),
            std::time::Duration::ZERO,
        )
        .await
        .unwrap();
        assert_eq!(result.account_id.as_deref(), Some("acct-keep"));
        assert_eq!(
            load_provider_auth_at(&path, "openai-codex@a").unwrap().unwrap().account_id.as_deref(),
            Some("acct-keep")
        );
    }

    /// Seat-pinned vend: the slot must still hold the inspected seat. A
    /// re-login (different refresh material / account id) yields the typed
    /// `SeatChanged` error and no token; the check never needs the network
    /// for a fresh credential.
    #[tokio::test]
    async fn seat_pinned_vend_refuses_a_replaced_slot() {
        use super::super::account::SeatIdentity;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let mut stored = fresh_creds("a-r");
        stored.account_id = Some("acct-a".into());
        save_provider_auth_at(&path, "openai-codex@a", &stored).unwrap();
        let client = reqwest::Client::new();
        let seat = SeatIdentity::of(OAuthProviderId::OpenAiCodex, &stored);
        let ok = super::ensure_fresh_credential_checked_at(&client, &named("a"), &path, Some(&seat))
            .await
            .unwrap();
        assert_eq!(ok.refresh, "a-r");
        // Same seat, rotated refresh token (another process refreshed): the
        // account id anchors the identity, so the vend still pairs.
        let mut rotated = fresh_creds("a-r2");
        rotated.account_id = Some("acct-a".into());
        save_provider_auth_at(&path, "openai-codex@a", &rotated).unwrap();
        assert!(
            super::ensure_fresh_credential_checked_at(&client, &named("a"), &path, Some(&seat))
                .await
                .is_ok()
        );
        // Re-login with another seat under the same alias: refused.
        let mut other = fresh_creds("b-r");
        other.account_id = Some("acct-b".into());
        save_provider_auth_at(&path, "openai-codex@a", &other).unwrap();
        let err = super::ensure_fresh_credential_checked_at(&client, &named("a"), &path, Some(&seat))
            .await
            .unwrap_err();
        assert_eq!(err, super::SEAT_CHANGED);
        assert!(!err.contains("b-r") && !err.contains("acct"));
        // Unchecked vend of the same slot still works (explicit selection).
        assert_eq!(
            super::ensure_fresh_credential_checked_at(&client, &named("a"), &path, None)
                .await
                .unwrap()
                .refresh,
            "b-r"
        );
        // Removed slot: the ordinary load-miss error, not a seat mismatch.
        assert!(remove_key_at(&path, "openai-codex@a").unwrap());
        let err = super::ensure_fresh_credential_checked_at(&client, &named("a"), &path, Some(&seat))
            .await
            .unwrap_err();
        assert!(err.starts_with("No credentials for "), "{err}");
        assert_eq!(
            super::SeatVendError::SeatChanged.to_string(),
            super::SEAT_CHANGED
        );
    }

    /// Cancel safety: the caller of a refresh is dropped (Esc) while the
    /// provider exchange is in flight; the detached task still completes
    /// and persists the rotated token, so it is never lost.
    #[tokio::test]
    async fn detached_refresh_persists_after_caller_is_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let cred = named("a");
        let key = cred.storage_key();
        save_provider_auth_at(&path, &key, &expired_creds("a-r")).unwrap();
        let (task_path, task_key) = (path.clone(), key.clone());
        let caller = super::run_detached(async move {
            let gate = super::refresh_gate(&format!("{}|{}", task_path.display(), task_key));
            let lock = refresh_lock_path(&task_path, &task_key);
            ensure_fresh_gated_locked(
                gate,
                Some(lock),
                || {
                    load_provider_auth_at(&task_path, &task_key)?
                        .ok_or_else(|| "missing".to_string())
                },
                |old| async move {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    Ok(OAuthCredentials {
                        auth_type: "oauth".into(),
                        refresh: format!("{old}-rotated"),
                        access: format!("{old}-access"),
                        expires: crate::epoch_millis() + 3_600_000,
                        account_id: None,
                    })
                },
                |old, fresh| {
                    save_provider_auth_if_refresh_matches_at(&task_path, &task_key, old, fresh)
                },
            )
            .await
        });
        // Cancel the caller mid-exchange, as Esc does via `await_or_cancel`.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), caller)
                .await
                .is_err(),
            "caller must still be waiting when cancelled"
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let stored = load_provider_auth_at(&path, &key).unwrap().unwrap();
            if stored.refresh == "a-r-rotated" {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "rotated token was never persisted after cancel"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// A slow fake refresh of account `label` in `path`, as a 'static future
    /// handed to `run_detached` (owned path and key, like production).
    fn slow_refresh_future(
        path: std::path::PathBuf,
        label: &str,
        started: Arc<std::sync::atomic::AtomicBool>,
    ) -> impl std::future::Future<Output = Result<OAuthCredentials, String>> + Send + 'static {
        let key = named(label).storage_key();
        async move {
            let gate = super::refresh_gate(&format!("{}|{}", path.display(), key));
            let lock = refresh_lock_path(&path, &key);
            ensure_fresh_gated_locked(
                gate,
                Some(lock),
                || load_provider_auth_at(&path, &key)?.ok_or_else(|| "missing".to_string()),
                |old| {
                    let started = started.clone();
                    async move {
                        started.store(true, std::sync::atomic::Ordering::SeqCst);
                        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                        Ok(OAuthCredentials {
                            auth_type: "oauth".into(),
                            refresh: format!("{old}-rotated"),
                            access: format!("{old}-access"),
                            expires: crate::epoch_millis() + 3_600_000,
                            account_id: None,
                        })
                    }
                },
                |old, fresh| save_provider_auth_if_refresh_matches_at(&path, &key, old, fresh),
            )
            .await
        }
    }

    fn wait_for_rotation(path: &std::path::Path, label: &str, want: &str) {
        let key = named(label).storage_key();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while load_provider_auth_at(path, &key).unwrap().unwrap().refresh != want {
            assert!(
                std::time::Instant::now() < deadline,
                "rotated token was never persisted after the caller's runtime died"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// Review I1: subagent workers run on throwaway `current_thread`
    /// runtimes. Dropping one mid-exchange must neither kill the refresh
    /// (the rotated token is still saved) nor block on it.
    #[test]
    fn refresh_survives_dropping_the_callers_runtime_mid_exchange() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        save_provider_auth_at(&path, &named("a").storage_key(), &expired_creds("a-r")).unwrap();
        let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let fut = slow_refresh_future(path.clone(), "a", started.clone());
        worker.spawn(super::run_detached(fut));
        worker.block_on(async {
            while !started.load(std::sync::atomic::Ordering::SeqCst) {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        });
        let t = std::time::Instant::now();
        drop(worker); // the worker timed out / was cancelled mid-exchange
        assert!(t.elapsed() < std::time::Duration::from_secs(1), "{:?}", t.elapsed());
        wait_for_rotation(&path, "a", "a-r-rotated");
    }

    /// Same, while the refresh is still waiting in the flock poller behind a
    /// peer process: dropping the worker returns promptly (the poller is not
    /// on the worker's blocking pool), and the refresh completes once the
    /// peer releases the lock.
    #[test]
    fn dropping_the_callers_runtime_while_waiting_on_the_lock_is_prompt() {
        use fs4::fs_std::FileExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let key = named("b").storage_key();
        save_provider_auth_at(&path, &key, &expired_creds("b-r")).unwrap();
        let holder = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(refresh_lock_path(&path, &key))
            .unwrap();
        FileExt::lock_exclusive(&holder).unwrap();
        let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        worker.spawn(super::run_detached(slow_refresh_future(path.clone(), "b", started.clone())));
        worker.block_on(async { tokio::time::sleep(std::time::Duration::from_millis(100)).await });
        assert!(!started.load(std::sync::atomic::Ordering::SeqCst), "must wait for the peer");
        let t = std::time::Instant::now();
        drop(worker);
        assert!(t.elapsed() < std::time::Duration::from_secs(1), "{:?}", t.elapsed());
        drop(holder);
        wait_for_rotation(&path, "b", "b-r-rotated");
    }

    /// Ported from dev's `refresh_file_lock_excludes_second_holder_until_released`
    /// (#237/#238): a second holder of the same refresh lock (flock is per
    /// open file description, so this contends exactly like another process)
    /// times out as `LockBusy`, and the lock is acquirable again once the
    /// first holder releases it.
    #[tokio::test]
    async fn refresh_file_lock_excludes_second_holder_until_released() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json.anthropic.refresh.lock");
        let short = std::time::Duration::from_millis(150);
        let held = super::acquire_refresh_lock(path.clone(), short).await.unwrap();
        let err = match super::acquire_refresh_lock(path.clone(), short).await {
            Ok(_) => panic!("second holder must time out"),
            Err(e) => e,
        };
        assert_eq!(err.kind, super::RefreshFailureKind::LockBusy);
        assert!(err.message().contains("timed out"), "{err}");
        drop(held);
        super::acquire_refresh_lock(path, short).await.unwrap();
    }

    /// The fresh-token fast path never takes the cross-process lock: a
    /// foreign holder does not delay a vend of an unexpired credential.
    #[tokio::test]
    async fn fresh_credential_skips_the_refresh_lock() {
        use fs4::fs_std::FileExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let cred = named("a");
        save_provider_auth_at(&path, &cred.storage_key(), &fresh_creds("a-r")).unwrap();
        let holder = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(refresh_lock_path(&path, &cred.storage_key()))
            .unwrap();
        FileExt::lock_exclusive(&holder).unwrap();
        let got = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            super::ensure_fresh_credential_at(&reqwest::Client::new(), &cred, &path),
        )
        .await
        .expect("fresh vend must not wait on the refresh lock")
        .unwrap();
        assert_eq!(got.refresh, "a-r");
    }

    #[test]
    fn refresh_lock_path_is_per_credential_and_beside_the_file() {
        let path = std::path::Path::new("/tmp/x/auth.json");
        assert_eq!(
            refresh_lock_path(path, "openai-codex@astra2"),
            std::path::PathBuf::from("/tmp/x/auth.json.openai-codex@astra2.refresh.lock")
        );
        // The default Anthropic slot shares the lock name used by pre-#215
        // builds (`auth.json.<provider>.refresh.lock`), so an older client
        // still running alongside serializes on the same file.
        assert_eq!(
            refresh_lock_path(path, "anthropic"),
            std::path::PathBuf::from("/tmp/x/auth.json.anthropic.refresh.lock")
        );
        assert_ne!(
            refresh_lock_path(path, "openai-codex"),
            refresh_lock_path(path, "openai-codex@astra2")
        );
        let _ = CasOutcome::Saved;
    }
}

#[cfg(test)]
mod rejected_token_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex as StdMutex;

    fn credential(access: &str) -> OAuthCredentials {
        OAuthCredentials {
            auth_type: "oauth".into(),
            access: access.into(),
            refresh: "r0".into(),
            expires: crate::epoch_millis() + 3_600_000,
            account_id: Some("account".into()),
        }
    }
    async fn recover(
        store: &StdMutex<OAuthCredentials>,
        gate: Arc<Mutex<()>>,
        calls: &AtomicUsize,
        lock: Option<PathBuf>,
        rejected: &str,
    ) -> Result<OAuthCredentials, String> {
        ensure_gated_locked_policy(
            gate,
            lock,
            || Ok(store.lock().unwrap().clone()),
            |_| async {
                calls.fetch_add(1, Ordering::SeqCst);
                tokio::task::yield_now().await;
                Ok::<_, String>(credential("new"))
            },
            |_, fresh| {
                *store.lock().unwrap() = fresh.clone();
                std::future::ready(Ok(CasOutcome::Saved))
            },
            Some(rejected),
            |_| Ok(()),
        )
        .await
        .map_err(String::from)
    }
    /// Round-2 m-g: a failed runtime build is not cached; the next call
    /// builds again, and a success is kept.
    #[test]
    fn refresh_runtime_build_is_retried_after_a_failure() {
        let slot = std::sync::Mutex::new(None);
        let err = runtime_from(&slot, || Err(std::io::Error::other("EAGAIN"))).unwrap_err();
        assert_eq!(err.kind, RefreshFailureKind::Other);
        assert!(err.message().contains("EAGAIN"), "{err}");
        let built = runtime_from(&slot, || {
            tokio::runtime::Builder::new_current_thread().build()
        })
        .expect("second attempt builds");
        let again = runtime_from(&slot, || panic!("must reuse the built runtime")).unwrap();
        assert!(std::ptr::eq(built, again));
    }

    /// m4: a stamp that cannot be written (ENOSPC, read-only lock file)
    /// must not switch the cooldown off: the in-process floor keeps it.
    #[test]
    fn failed_stamp_write_keeps_an_in_process_cooldown_floor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json.muse.refresh.lock");
        std::fs::write(&path, "").unwrap();
        // Read-only handle: every stamp write fails.
        let lock = RefreshFileLock {
            file: std::fs::File::open(&path).unwrap(),
            path: path.clone(),
        };
        let now = 5_000_000_000;
        lock.record_refreshed(now);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "", "stamp write failed");
        assert_eq!(lock.last_refresh_ms(), Some(now));
        let cred = CredentialRef::default_for(super::super::provider::OAuthProviderId::Muse);
        let refused = forced_refresh_allowed(&cred, lock.last_refresh_ms(), now + 5_000).unwrap_err();
        assert!(matches!(refused.kind, RefreshFailureKind::Cooldown { .. }), "{refused}");

        // Another account's lock never sees this floor.
        let other_path = dir.path().join("auth.json.muse@work.refresh.lock");
        std::fs::write(&other_path, "").unwrap();
        let other = RefreshFileLock {
            file: std::fs::File::open(&other_path).unwrap(),
            path: other_path,
        };
        assert_eq!(other.last_refresh_ms(), None);
    }

    /// Round-2 I2: a corrupt store is not a login problem (login would fail
    /// on it too): the refresh says what to do, typed ActionRequired.
    #[test]
    fn corrupt_store_load_is_action_required_not_credential() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        std::fs::write(&path, "{oops").unwrap();
        let cred = CredentialRef::default_for(super::super::provider::OAuthProviderId::Anthropic);
        let err = load_checked(&path, &cred, None).unwrap_err();
        assert_eq!(err.kind, RefreshFailureKind::ActionRequired, "{err}");
        assert!(err.message().contains("mv '"), "{err}");
        assert!(!err.message().contains("synaps login"), "{err}");
    }

    /// m3: a refresh runtime that failed to start is a refresh error, not
    /// a process abort (release builds use `panic = "abort"`).
    #[tokio::test]
    async fn refresh_runtime_start_failure_is_an_error() {
        let err = run_detached_on(
            Err(RefreshFailure::other("could not start the auth refresh runtime: boom")),
            async { Ok::<u8, RefreshFailure>(1) },
        )
        .await
        .unwrap_err();
        assert_eq!(err.kind, RefreshFailureKind::Other);
        assert!(err.message().contains("refresh runtime"), "{err}");
        // A started runtime still runs the task.
        let ok: Result<u8, RefreshFailure> =
            run_detached_on(refresh_runtime(), async { Ok(7) }).await;
        assert_eq!(ok.unwrap(), 7);
    }

    /// I1 (round 2): the save after a rotation must not lose the rotated
    /// token. A failed save parks it in a 0600 sidecar under the account's
    /// refresh lock; the caller still gets the fresh token, and the next
    /// locked load applies the sidecar instead of spending the old refresh
    /// token again.
    #[tokio::test]
    async fn rotated_token_survives_a_failed_save() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("auth.json.anthropic.refresh.lock");
        let mut stale = credential("old-access");
        stale.expires = 0;
        let store = StdMutex::new(stale);
        let refreshes = AtomicUsize::new(0);
        let fail_saves = AtomicUsize::new(1);
        let run = || {
            ensure_gated_locked_policy(
                Arc::new(Mutex::new(())),
                Some(lock.clone()),
                || Ok(store.lock().unwrap().clone()),
                |old| {
                    refreshes.fetch_add(1, Ordering::SeqCst);
                    let mut c = credential("rotated-access");
                    c.refresh = format!("{old}-rotated");
                    async move { Ok::<_, String>(c) }
                },
                |expected: &str, fresh: &OAuthCredentials| {
                    let out = if fail_saves.load(Ordering::SeqCst) > 0 {
                        fail_saves.fetch_sub(1, Ordering::SeqCst);
                        Err(StoreError::Busy("timed out waiting for auth.json.lock".into()).into())
                    } else if store.lock().unwrap().refresh == expected {
                        *store.lock().unwrap() = fresh.clone();
                        Ok(CasOutcome::Saved)
                    } else {
                        Ok(CasOutcome::Replaced)
                    };
                    std::future::ready(out)
                },
                None,
                |_| Ok(()),
            )
        };
        let first = run().await.expect("the rotated token is handed out");
        assert_eq!(first.refresh, "r0-rotated");
        assert_eq!(store.lock().unwrap().refresh, "r0", "save failed");
        let pending = pending_path(&lock);
        assert!(pending.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&pending).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        // Next locked load: the sidecar is applied; the spent "r0" is never
        // sent again.
        let second = run().await.unwrap();
        assert_eq!(second.refresh, "r0-rotated");
        assert_eq!(store.lock().unwrap().refresh, "r0-rotated");
        assert_eq!(refreshes.load(Ordering::SeqCst), 1);
        assert!(!pending.exists());
    }

    /// A sidecar the store has moved past (re-login, removal) is dropped;
    /// one that can't be applied yet blocks the refresh instead of spending
    /// the old token.
    #[tokio::test]
    async fn stale_or_unapplied_sidecar_is_handled_under_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("auth.json.anthropic.refresh.lock");
        let file_lock = RefreshFileLock {
            file: std::fs::File::create(&lock).unwrap(),
            path: lock.clone(),
        };
        let mut parked = credential("parked");
        parked.refresh = "parked-r".into();
        file_lock.park_pending("r-before-relogin", &parked).unwrap();

        // Store moved on: Replaced -> sidecar deleted, nothing written.
        file_lock
            .apply_pending(&|_: &str, _: &OAuthCredentials| {
                std::future::ready(Ok(CasOutcome::Replaced))
            })
            .await
            .unwrap();
        assert!(!pending_path(&lock).exists());

        // Store still busy: error, sidecar kept for the next try.
        file_lock.park_pending("r0", &parked).unwrap();
        let err = file_lock
            .apply_pending(&|_: &str, _: &OAuthCredentials| {
                std::future::ready(Err(StoreError::Busy("busy".into()).into()))
            })
            .await
            .unwrap_err();
        assert_eq!(err.kind, RefreshFailureKind::Other);
        assert!(pending_path(&lock).exists());
    }

    /// A store-lock timeout names the store lock (it is not another refresh
    /// of this account), and the refresh save waits far longer than a login
    /// or static-key save.
    #[test]
    fn store_lock_timeout_is_described_honestly() {
        let busy: RefreshFailure =
            StoreError::Busy("timed out waiting for auth.json.lock".into()).into();
        assert_eq!(busy.kind, RefreshFailureKind::Other);
        assert!(busy.message().contains("auth.json.lock"));
        assert!(REFRESH_SAVE_WAIT >= REFRESH_LOCK_WAIT);
        assert!(REFRESH_SAVE_WAIT >= std::time::Duration::from_secs(120));
        assert_eq!(super::super::storage::STORE_LOCK_WAIT, std::time::Duration::from_secs(15));
    }

    #[tokio::test]
    async fn changed_token_is_adopted_without_refresh() {
        let store = StdMutex::new(credential("peer-token"));
        let calls = AtomicUsize::new(0);
        let got = recover(&store, Arc::new(Mutex::new(())), &calls, None, "rejected")
            .await
            .unwrap();
        assert_eq!(got.access, "peer-token");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
    #[tokio::test]
    async fn unexpired_rejected_token_rotates_once_for_concurrent_waiters() {
        let store = StdMutex::new(credential("rejected"));
        let calls = AtomicUsize::new(0);
        let gate = Arc::new(Mutex::new(()));
        let (a, b) = tokio::join!(
            recover(&store, gate.clone(), &calls, None, "rejected"),
            recover(&store, gate, &calls, None, "rejected")
        );
        assert_eq!(a.unwrap().access, "new");
        assert_eq!(b.unwrap().access, "new");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn separate_process_gates_serialize_on_the_refresh_file_lock() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("refresh.lock");
        let store = StdMutex::new(credential("rejected"));
        let calls = AtomicUsize::new(0);
        let (a, b) = tokio::join!(
            recover(
                &store,
                Arc::new(Mutex::new(())),
                &calls,
                Some(lock.clone()),
                "rejected"
            ),
            recover(
                &store,
                Arc::new(Mutex::new(())),
                &calls,
                Some(lock),
                "rejected"
            )
        );
        assert_eq!(a.unwrap().access, "new");
        assert_eq!(b.unwrap().access, "new");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn removed_slot_is_not_resurrected_by_forced_refresh() {
        let result = ensure_gated_locked_policy(
            Arc::new(Mutex::new(())),
            None,
            || Ok(credential("rejected")),
            |_| async { Ok::<_, String>(credential("new")) },
            |_, _| std::future::ready(Ok(CasOutcome::Removed)),
            Some("rejected"),
            |_| Ok(()),
        )
        .await;
        assert!(result.unwrap_err().to_string().contains("removed"));
    }

    // ── Forced-refresh rate limit (ported from #237/#238, now per account) ──

    /// Single-credential fake store driven through the production policy.
    struct FakeStore {
        creds: StdMutex<OAuthCredentials>,
        refresh_calls: AtomicUsize,
        save_calls: AtomicUsize,
    }

    impl FakeStore {
        fn new(initial: OAuthCredentials) -> Self {
            Self {
                creds: StdMutex::new(initial),
                refresh_calls: AtomicUsize::new(0),
                save_calls: AtomicUsize::new(0),
            }
        }

        async fn ensure_rejecting(
            &self,
            gate: Arc<Mutex<()>>,
            rejected: &str,
        ) -> Result<OAuthCredentials, String> {
            ensure_gated_locked_policy(
                gate,
                None,
                || Ok(self.creds.lock().unwrap().clone()),
                |old_refresh| async move {
                    self.refresh_calls.fetch_add(1, Ordering::SeqCst);
                    tokio::task::yield_now().await;
                    Ok::<_, String>(OAuthCredentials {
                        auth_type: "oauth".into(),
                        refresh: format!("{old_refresh}-rotated"),
                        access: "fresh-access".into(),
                        expires: crate::epoch_millis() + 3_600_000,
                        account_id: None,
                    })
                },
                |_, c| {
                    self.save_calls.fetch_add(1, Ordering::SeqCst);
                    *self.creds.lock().unwrap() = c.clone();
                    std::future::ready(Ok(CasOutcome::Saved))
                },
                Some(rejected),
                |_| Ok(()),
            )
            .await
            .map_err(String::from)
        }
    }

    /// Revoked-but-unexpired: the stored credential still holds the exact
    /// access token the provider rejected -> refresh is forced.
    #[tokio::test]
    async fn rejected_unexpired_token_forces_refresh() {
        let store = FakeStore::new(credential("rejected"));
        let creds = store
            .ensure_rejecting(Arc::new(Mutex::new(())), "rejected")
            .await
            .unwrap();
        assert_eq!(creds.access, "fresh-access");
        assert_eq!(creds.refresh, "r0-rotated");
        assert_eq!(store.refresh_calls.load(Ordering::SeqCst), 1);
        assert_eq!(store.save_calls.load(Ordering::SeqCst), 1);
    }

    /// Disk already rotated (external `synaps login` / cron / earlier waiter):
    /// the rejected token no longer matches -> no refresh, disk creds returned.
    #[tokio::test]
    async fn rejected_token_differs_from_disk_uses_disk_without_refresh() {
        let store = FakeStore::new(credential("keep"));
        let creds = store
            .ensure_rejecting(Arc::new(Mutex::new(())), "some-old-token")
            .await
            .unwrap();
        assert_eq!(creds.access, "keep");
        assert_eq!(store.refresh_calls.load(Ordering::SeqCst), 0);
        assert_eq!(store.save_calls.load(Ordering::SeqCst), 0);
    }

    /// Concurrent 401s with the same rejected token: exactly one forced
    /// refresh; later waiters see the rotated token and skip.
    #[tokio::test]
    async fn concurrent_rejections_single_flight() {
        let store = FakeStore::new(credential("rejected"));
        let gate = Arc::new(Mutex::new(()));
        let waiters: Vec<_> = (0..8)
            .map(|_| store.ensure_rejecting(gate.clone(), "rejected"))
            .collect();
        for got in futures::future::join_all(waiters).await {
            assert_eq!(got.unwrap().access, "fresh-access");
        }
        assert_eq!(store.refresh_calls.load(Ordering::SeqCst), 1);
    }

    /// Rate limit: a forced refresh denied by `may_force` spends nothing.
    #[tokio::test]
    async fn forced_refresh_denied_by_cooldown_spends_nothing() {
        let refreshes = AtomicUsize::new(0);
        let err = ensure_gated_locked_policy(
            Arc::new(Mutex::new(())),
            None,
            || Ok(credential("rejected")),
            |_| async {
                refreshes.fetch_add(1, Ordering::SeqCst);
                Ok::<_, String>(credential("never"))
            },
            |_, _| std::future::ready(Ok(CasOutcome::Saved)),
            Some("rejected"),
            |_| Err("cooldown".to_string().into()),
        )
        .await
        .unwrap_err();
        assert_eq!(err.to_string(), "cooldown");
        assert_eq!(refreshes.load(Ordering::SeqCst), 0);
    }

    /// An EXPIRED credential refreshes even inside the cooldown: the rate
    /// limit applies only to forcing an unexpired token. It also refreshes
    /// when the caller's rejected token is no longer the stored one.
    #[tokio::test]
    async fn expired_credential_ignores_force_cooldown() {
        for rejected in ["stale", "some-other-token"] {
            let creds = ensure_gated_locked_policy(
                Arc::new(Mutex::new(())),
                None,
                || {
                    let mut c = credential("stale");
                    c.expires = 0;
                    Ok(c)
                },
                |_| async { Ok::<_, String>(credential("new")) },
                |_, _| std::future::ready(Ok(CasOutcome::Saved)),
                Some(rejected),
                |_| Err("cooldown".to_string().into()),
            )
            .await
            .unwrap();
            assert_eq!(creds.access, "new", "rejected = {rejected}");
        }
    }

    use super::super::account::Account;
    use super::super::provider::OAuthProviderId;
    use super::super::storage::save_provider_auth_at;

    fn account(label: &str) -> CredentialRef {
        CredentialRef::new(OAuthProviderId::Anthropic, Account::named(label).unwrap())
    }

    /// auth.json seeded long ago. (The mtime no longer matters to the
    /// cooldown; the old timestamp keeps these fixtures neutral.)
    fn seed_old(path: &Path, entries: &[(&CredentialRef, &str)]) {
        for (cred, access) in entries {
            save_provider_auth_at(path, &cred.storage_key(), &credential(access)).unwrap();
        }
        let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(old)
            .unwrap();
    }

    /// Production composition (gate + lock + CAS save + cooldown) with a
    /// fake provider exchange that mints a distinct access token per call.
    async fn forced(
        path: &Path,
        cred: &CredentialRef,
        rejected: &str,
        calls: &AtomicUsize,
    ) -> Result<OAuthCredentials, String> {
        refresh_credential_locked(cred, path, None, Some(rejected), |old| async move {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            Ok::<_, String>(OAuthCredentials {
                auth_type: "oauth".into(),
                refresh: format!("{old}-rotated"),
                access: format!("minted-{n}"),
                expires: crate::epoch_millis() + 3_600_000,
                account_id: None,
            })
        })
        .await
        .map_err(String::from)
    }

    /// A permanent 401 stops spending refresh tokens: the first rejection
    /// forces one refresh; when the provider rejects the freshly minted
    /// token too, the cooldown refuses a second spend. A second account of
    /// the same provider has its own cooldown and still recovers.
    #[tokio::test]
    async fn forced_refresh_cooldown_is_per_account_and_stops_a_persistent_401() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let (a, b) = (account("a"), account("b"));
        seed_old(&path, &[(&a, "a-rejected"), (&b, "b-rejected")]);
        let calls = AtomicUsize::new(0);

        let first = forced(&path, &a, "a-rejected", &calls).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let err = forced(&path, &a, &first.access, &calls).await.unwrap_err();
        assert!(err.contains("not refreshing again"), "{err}");
        assert!(!err.contains(&first.access), "error must not carry tokens: {err}");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "cooldown spends nothing");

        // Account b: a's refresh stamped only a's lock file.
        forced(&path, &b, "b-rejected", &calls).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2, "b has its own cooldown");
    }

    /// What another synaps process leaves behind after refreshing `cred`:
    /// the refresh time stamped into that account's lock file.
    fn stamp_as_other_process(path: &Path, cred: &CredentialRef, stamp: &str) {
        std::fs::write(refresh_lock_path(path, &cred.storage_key()), stamp).unwrap();
    }

    fn now_stamp() -> String {
        crate::epoch_millis().to_string()
    }

    /// Another process refreshed account A seconds ago (stamp in A's lock
    /// file): a forced refresh of A is refused and spends nothing.
    #[tokio::test]
    async fn other_process_refresh_stamp_blocks_forced_refresh_of_that_account() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let a = account("a");
        seed_old(&path, &[(&a, "a-rejected")]);
        stamp_as_other_process(&path, &a, &now_stamp());
        let calls = AtomicUsize::new(0);
        let err = forced(&path, &a, "a-rejected", &calls).await.unwrap_err();
        assert!(err.contains("not refreshing again"), "{err}");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    /// A's stamp never cools down B in the same auth.json.
    #[tokio::test]
    async fn refresh_stamp_of_one_account_does_not_block_another() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let (a, b) = (account("a"), account("b"));
        seed_old(&path, &[(&a, "a-rejected"), (&b, "b-rejected")]);
        stamp_as_other_process(&path, &a, &now_stamp());
        let calls = AtomicUsize::new(0);
        forced(&path, &b, "b-rejected", &calls).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// A fresh auth.json write (e.g. `synaps login` of any account) with no
    /// refresh stamp no longer blocks a forced refresh.
    #[tokio::test]
    async fn foreign_auth_file_write_without_a_stamp_does_not_block() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let a = account("a");
        // Written "now", by something other than a refresh.
        save_provider_auth_at(&path, &a.storage_key(), &credential("a-rejected")).unwrap();
        let calls = AtomicUsize::new(0);
        forced(&path, &a, "a-rejected", &calls).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// Review I3's leak: another process force-refreshed A, then THIS process
    /// refreshed B (so the last auth.json write is ours). A 401 on A's new
    /// token must still be refused — the cooldown is A's own stamp, not a
    /// file-level "who wrote last" signal.
    #[tokio::test]
    async fn own_write_after_another_process_refreshed_a_does_not_hide_a_s_cooldown() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let (a, b) = (account("a"), account("b"));
        seed_old(&path, &[(&a, "a-new-from-peer"), (&b, "b-rejected")]);
        stamp_as_other_process(&path, &a, &now_stamp());
        let calls = AtomicUsize::new(0);
        forced(&path, &b, "b-rejected", &calls).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1, "b refreshed and wrote auth.json");
        let err = forced(&path, &a, "a-new-from-peer", &calls).await.unwrap_err();
        assert!(err.contains("not refreshing again"), "{err}");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "a's refresh token not spent again");
    }

    /// The cooldown only gates FORCING an unexpired token: an expired one
    /// refreshes even right after another process stamped it.
    #[tokio::test]
    async fn expired_token_refreshes_inside_the_stamp_cooldown() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let a = account("a");
        let mut expired = credential("a-old");
        expired.expires = 0;
        save_provider_auth_at(&path, &a.storage_key(), &expired).unwrap();
        stamp_as_other_process(&path, &a, &now_stamp());
        let calls = AtomicUsize::new(0);
        let got = forced(&path, &a, "a-old", &calls).await.unwrap();
        assert_eq!(got.access, "minted-0");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// The stamp is advisory: unreadable or future-dated values allow.
    #[tokio::test]
    async fn garbage_or_future_stamps_allow_a_forced_refresh() {
        let future = (crate::epoch_millis() + 3_600_000).to_string();
        for stamp in ["", "not-a-number", "-5", "12abc", future.as_str()] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("auth.json");
            let a = account("a");
            seed_old(&path, &[(&a, "a-rejected")]);
            stamp_as_other_process(&path, &a, stamp);
            let calls = AtomicUsize::new(0);
            forced(&path, &a, "a-rejected", &calls)
                .await
                .unwrap_or_else(|e| panic!("stamp {stamp:?}: {e}"));
            assert_eq!(calls.load(Ordering::SeqCst), 1, "stamp {stamp:?}");
        }
    }

    /// Our refresh writes the stamp, and an older client that opens and
    /// flocks the same lock file the pre-#215 way (`create`, `write`,
    /// `truncate(false)`, never writing it) leaves it intact.
    #[tokio::test]
    async fn refresh_stamp_survives_an_old_client_style_lock_open() {
        use fs4::fs_std::FileExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let a = account("a");
        seed_old(&path, &[(&a, "a-rejected")]);
        let calls = AtomicUsize::new(0);
        let first = forced(&path, &a, "a-rejected", &calls).await.unwrap();
        let lock = refresh_lock_path(&path, &a.storage_key());
        let stamp = std::fs::read_to_string(&lock).unwrap();
        let ms: u64 = stamp.parse().expect("plain ASCII epoch ms");
        assert!(crate::epoch_millis().saturating_sub(ms) < 60_000, "{stamp}");

        // Old client (origin/dev `lock_file_at`): open + flock + unlock.
        let old = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock)
            .unwrap();
        FileExt::lock_exclusive(&old).unwrap();
        drop(old);
        assert_eq!(std::fs::read_to_string(&lock).unwrap(), stamp);

        // ...so the cooldown it records still holds.
        let err = forced(&path, &a, &first.access, &calls).await.unwrap_err();
        assert!(err.contains("not refreshing again"), "{err}");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// Typed failures: callers can tell a cooldown and a transient provider
    /// failure from an unusable login without parsing text.
    #[test]
    fn refresh_failures_are_typed() {
        let a = account("a");
        let now = 1_000_000_000;
        let f = forced_refresh_allowed(&a, Some(now - 12_000), now).unwrap_err();
        assert_eq!(f.kind, RefreshFailureKind::Cooldown { retry_in_secs: 48 });
        assert!(f.message().contains("not refreshing again for 48s"), "{f}");
        assert!(forced_refresh_allowed(&a, Some(now - 60_000), now).is_ok());

        use RefreshFailureKind::Credential;
        // Storage/slot strings are credential failures.
        assert_eq!(RefreshFailure::from("No credentials".to_string()).kind, Credential);
    }
}

/// Minimal loopback HTTP server for refresh-classification tests: answers
/// every request with one canned raw response, then closes the connection.
#[cfg(test)]
pub(crate) mod fake_http {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// `HTTP/1.1 <status>` response with a JSON content type and an exact
    /// `Content-Length`.
    pub(crate) fn response(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn request_complete(got: &[u8]) -> bool {
        let Some(end) = got.windows(4).position(|w| w == b"\r\n\r\n") else {
            return false;
        };
        let head = String::from_utf8_lossy(&got[..end]).to_ascii_lowercase();
        let len = head
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(0);
        got.len() >= end + 4 + len
    }

    /// Serve `raw` to every connection; returns `http://127.0.0.1:<port>`.
    pub(crate) async fn serve(raw: String) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let raw = raw.clone();
                tokio::spawn(async move {
                    let mut got = Vec::new();
                    let mut buf = [0u8; 4096];
                    while !request_complete(&got) {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => got.extend_from_slice(&buf[..n]),
                        }
                    }
                    let _ = sock.write_all(raw.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        format!("http://{addr}")
    }

    /// A loopback URL nothing listens on (connection refused).
    pub(crate) async fn dead_url() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        format!("http://{addr}")
    }
}

/// m1: refresh failures are classified where they are produced (status code
/// and error type), never by matching message text.
#[cfg(test)]
mod source_classification_tests {
    use super::fake_http::{dead_url, response, serve};
    use super::*;
    use RefreshFailureKind::{Credential, Transient};

    #[test]
    fn status_kinds() {
        for s in [408, 425, 429, 500, 502, 503, 504] {
            assert_eq!(status_kind(s), Transient, "{s}");
        }
        for s in [400, 401, 403, 404] {
            assert_eq!(status_kind(s), Credential, "{s}");
        }
    }

    async fn anthropic(raw: String) -> RefreshFailure {
        let url = format!("{}/v1/oauth/token", serve(raw).await);
        refresh_token_at(&Client::new(), &url, "rt").await.unwrap_err()
    }

    #[tokio::test]
    async fn anthropic_refresh_failures_are_typed_at_the_source() {
        // A captive portal answering 200 with HTML: not a login problem.
        let portal = anthropic(response("200 OK", "<html>sign in to wifi</html>")).await;
        assert_eq!(portal.kind, Transient, "{portal}");
        // A body cut mid-transfer (declared 200 bytes, sent 10).
        let cut = anthropic(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 200\r\n\
             Connection: close\r\n\r\n{\"access_t"
                .to_string(),
        )
        .await;
        assert_eq!(cut.kind, Transient, "{cut}");
        let timeout = anthropic(response("408 Request Timeout", "{}")).await;
        assert_eq!(timeout.kind, Transient, "{timeout}");
        let busy = anthropic(response("503 Service Unavailable", "<html/>")).await;
        assert_eq!(busy.kind, Transient, "{busy}");
        // A real rejection stays a credential failure, sanitized.
        let revoked = anthropic(response(
            "400 Bad Request",
            r#"{"error":"invalid_grant","error_description":"revoked"}"#,
        ))
        .await;
        assert_eq!(revoked.kind, Credential, "{revoked}");
        assert!(revoked.message().contains("invalid_grant"), "{revoked}");
        // No network at all.
        let url = format!("{}/v1/oauth/token", dead_url().await);
        let down = refresh_token_at(&Client::new(), &url, "rt").await.unwrap_err();
        assert_eq!(down.kind, Transient, "{down}");
    }

    /// Kind follows the typed provider error, even when the text would
    /// have fooled the old substring matcher.
    #[test]
    fn provider_errors_carry_their_kind() {
        use super::super::google_gemini::GeminiAuthError as G;
        assert_eq!(G::TokenRequestFailed(503).refresh_failure().kind, Transient);
        assert_eq!(G::TokenRequestFailed(400).refresh_failure().kind, Credential);
        assert_eq!(G::Transport.refresh_failure().kind, Transient);
        assert_eq!(G::EmptyRefreshToken.refresh_failure().kind, Credential);

        use super::super::muse::MuseAuthError as M;
        assert_eq!(M::LoginRejected(401).refresh_failure().kind, Credential);
        assert_eq!(M::HttpStatus(503).refresh_failure().kind, Transient);
        assert_eq!(M::Transport.refresh_failure().kind, Transient);
        assert_eq!(M::InvalidMintResponse.refresh_failure().kind, Transient);
        assert_eq!(M::OnboardingRequired.refresh_failure().kind, Credential);
        // Billing, not the login: never "sign in again".
        assert_eq!(M::PaymentRequired.refresh_failure().kind, RefreshFailureKind::ActionRequired);

        use super::super::kimi_code::KimiAuthError as K;
        assert_eq!(K::Unauthorized.refresh_failure().kind, Credential);
        assert_eq!(K::HttpStatus(502).refresh_failure().kind, Transient);
        assert_eq!(K::Transport.refresh_failure().kind, Transient);

        use super::super::github_copilot::CopilotAuthError as C;
        assert_eq!(C::SessionMintRejected(401).refresh_failure().kind, Credential);
        assert_eq!(C::HttpStatus(500).refresh_failure().kind, Transient);
        assert_eq!(C::Transport.refresh_failure().kind, Transient);
    }

    #[tokio::test]
    async fn gemini_503_is_transient_and_invalid_grant_is_credential() {
        use super::super::google_gemini::{refresh_with_registration_typed, GeminiRegistration};
        let reg = GeminiRegistration::test("client-id", None).unwrap();
        let run = |raw: String| {
            let reg = &reg;
            async move {
                let url = format!("{}/token", serve(raw).await);
                refresh_with_registration_typed(&Client::new(), &url, true, reg, "rt")
                    .await
                    .unwrap_err()
            }
        };
        let busy = run(response("503 Service Unavailable", "{}")).await;
        assert_eq!(busy.kind, Transient, "{busy}");
        assert!(busy.message().contains("(HTTP 503)"), "{busy}");
        let revoked = run(response("400 Bad Request", r#"{"error":"invalid_grant"}"#)).await;
        assert_eq!(revoked.kind, Credential, "{revoked}");
    }

    #[tokio::test]
    async fn xai_discovery_failure_is_transient() {
        let url = format!("{}/.well-known/openid-configuration", dead_url().await);
        let err = super::super::xai::discover_at(&url).await.unwrap_err();
        assert_eq!(err.kind, Transient, "{err}");
        assert!(err.message().contains("discovery failed"), "{err}");
    }

    #[tokio::test]
    async fn codex_refresh_failures_are_typed() {
        let run = |raw: String| async move {
            let url = format!("{}/oauth/token", serve(raw).await);
            super::super::openai_codex::token_request_at(&Client::new(), &url, &[("a", "b")])
                .await
                .unwrap_err()
        };
        assert_eq!(run(response("502 Bad Gateway", "")).await.kind, Transient);
        assert_eq!(run(response("200 OK", "<html/>")).await.kind, Transient);
        let revoked = run(response("401 Unauthorized", r#"{"error":"invalid_grant"}"#)).await;
        assert_eq!(revoked.kind, Credential);
        assert!(revoked.message().contains("invalid_grant"), "{revoked}");
        // Round-2 m-i: a proxy echoing the request (refresh token included)
        // never reaches the error text.
        let echoed = run(response(
            "400 Bad Request",
            "grant_type=refresh_token&refresh_token=SECRET-RT-123",
        ))
        .await;
        assert!(!echoed.message().contains("SECRET-RT-123"), "{echoed}");
        let nested = run(response(
            "400 Bad Request",
            r#"{"error":"invalid_request","error_description":"bad","echo":"SECRET-RT-123"}"#,
        ))
        .await;
        assert!(!nested.message().contains("SECRET-RT-123"), "{nested}");
        assert!(nested.message().contains("invalid_request: bad"), "{nested}");
    }
}
