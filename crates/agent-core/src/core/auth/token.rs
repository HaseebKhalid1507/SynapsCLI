use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use tokio::sync::Mutex;

use reqwest::Client;

use super::storage::{auth_file_path, load_provider_auth, save_provider_auth};
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
        // Truncate error body — server responses may contain sensitive data
        // (tokens, internal errors). Status code alone is usually sufficient.
        let text = resp.text().await.unwrap_or_default();
        let truncated = if text.len() > 200 {
            &text[..200]
        } else {
            &text
        };
        return Err(format!("Token exchange failed ({}): {}", status, truncated));
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

/// Refresh an expired OAuth token.
pub async fn refresh_token(
    client: &Client,
    refresh: &str,
) -> std::result::Result<OAuthCredentials, String> {
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "client_id": CLIENT_ID,
        "refresh_token": refresh,
    });

    let resp = client
        .post(TOKEN_URL)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Token refresh request failed: {}", e))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!(
            "Token refresh failed ({}): {}",
            status,
            sanitize_oauth_error_body(&text)
        ));
    }

    let token_resp: TokenResponse = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse refresh response: {}", e))?;

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
fn sanitize_oauth_error_body(text: &str) -> String {
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

/// Process-wide, per-provider refresh gates. The file lock protects persistence;
/// these gates additionally keep concurrent async callers in this process from
/// rotating the same refresh token while the network request is in flight.
fn refresh_gate(provider: &str) -> Arc<Mutex<()>> {
    static GATES: OnceLock<std::sync::Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();
    let mut gates = GATES
        .get_or_init(|| std::sync::Mutex::new(HashMap::new()))
        .lock()
        .expect("refresh gate registry poisoned");
    gates
        .entry(provider.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

/// Ensure an OAuth provider (including Anthropic) has a fresh token.
///
/// Single-flight: concurrent async callers for the same provider serialize on
/// a per-provider gate; only the first performs the network refresh, later
/// waiters re-read the persisted (rotated) credential.
pub async fn ensure_fresh_provider_token<P>(
    client: &Client,
    provider: P,
) -> std::result::Result<OAuthCredentials, String>
where
    P: TryInto<super::provider::OAuthProviderId>,
    P::Error: std::fmt::Display,
{
    let provider = provider.try_into().map_err(|e| e.to_string())?;
    refresh_provider_detached(client.clone(), provider, None).await
}

/// Minimum interval between two *forced* (401-driven) refreshes of the same
/// provider's token in this process. If the provider rejects a token we
/// ourselves minted less than this long ago, a new token will not help (org /
/// permission / header problem) and spending the refresh token again only
/// widens the rotation race window.
pub(crate) const FORCED_REFRESH_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(60);

/// When this process last refreshed each provider's token (any refresh).
fn last_refresh_registry() -> &'static std::sync::Mutex<HashMap<String, std::time::Instant>> {
    static LAST: OnceLock<std::sync::Mutex<HashMap<String, std::time::Instant>>> = OnceLock::new();
    LAST.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

fn note_refreshed(provider: &str) {
    last_refresh_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(provider.to_string(), std::time::Instant::now());
}

/// Cross-process exclusive lock for the refresh critical section
/// (load → compare → refresh → save). A revocation (or a shared expiry) hits
/// every running synaps process at once; without this they would all spend
/// the same rotating refresh token. Uses a SEPARATE lock file from the save path's
/// `auth.json.lock`, so `save_provider_auth` can still take that lock inside.
/// Released when the returned file is dropped.
async fn lock_refresh_file(provider: &str) -> std::result::Result<std::fs::File, String> {
    // Per provider: a hung Anthropic refresh must not block a Codex one.
    let path = auth_file_path().with_extension(format!("json.{provider}.refresh.lock"));
    lock_file_at(&path, std::time::Duration::from_secs(30)).await
}

async fn lock_file_at(
    path: &std::path::Path,
    wait: std::time::Duration,
) -> std::result::Result<std::fs::File, String> {
    use fs4::fs_std::FileExt;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create {}: {}", parent.display(), e))?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(|e| format!("Failed to open lock file {}: {}", path.display(), e))?;
    let deadline = std::time::Instant::now() + wait;
    loop {
        match FileExt::try_lock_exclusive(&file) {
            Ok(true) => return Ok(file),
            Ok(false) if std::time::Instant::now() < deadline => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Ok(false) => {
                return Err(
                    "Timed out waiting for another synaps process to finish refreshing credentials"
                        .to_string(),
                )
            }
            Err(e) => return Err(format!("Failed to lock {}: {}", path.display(), e)),
        }
    }
}

/// Like [`ensure_fresh_provider_token`], but the caller reports an access
/// token the provider just REJECTED (HTTP 401). If the credential on disk
/// still carries that exact access token, a refresh is forced even though its
/// recorded expiry is in the future (revoked-but-unexpired token). If the disk
/// credential differs (rotated externally, or a concurrent waiter/process
/// already refreshed), it is returned as-is — so N concurrent 401s, in this
/// process or others, perform at most one network refresh.
///
/// Forced refreshes are rate-limited per provider by
/// [`FORCED_REFRESH_COOLDOWN`]. The rejected token is only compared for
/// equality; it is never logged.
pub async fn ensure_fresh_provider_token_after_rejection<P>(
    client: &Client,
    provider: P,
    rejected_access: &str,
) -> std::result::Result<OAuthCredentials, String>
where
    P: TryInto<super::provider::OAuthProviderId>,
    P::Error: std::fmt::Display,
{
    let provider = provider.try_into().map_err(|e| e.to_string())?;
    refresh_provider_detached(client.clone(), provider, Some(rejected_access.to_string())).await
}

/// Shared body of both provider entry points, run on a DETACHED task: if the
/// caller is cancelled (Esc mid-request) after the provider has rotated the
/// refresh token but before `save`, the task still persists it — dropping
/// the rotated refresh token would log the user out.
async fn refresh_provider_detached(
    client: Client,
    provider: super::provider::OAuthProviderId,
    rejected: Option<String>,
) -> std::result::Result<OAuthCredentials, String> {
    tokio::spawn(
        async move { refresh_provider_locked(&client, provider, rejected.as_deref()).await },
    )
    .await
    .map_err(|e| format!("credential refresh task failed: {e}"))?
}

/// Lock order, fixed everywhere: in-process gate → per-provider
/// `auth.json.<p>.refresh.lock` (cross-process) → save's `auth.json.lock`.
/// The file lock is only taken when a refresh looks necessary, so the common
/// fresh-token path stays a single file read.
async fn refresh_provider_locked(
    client: &Client,
    provider: super::provider::OAuthProviderId,
    rejected: Option<&str>,
) -> std::result::Result<OAuthCredentials, String> {
    let key = provider.as_str();
    let load = || {
        load_provider_auth(key)?.ok_or_else(|| {
            format!(
                "No credentials for {} at {}. Run `synaps login`.",
                provider,
                auth_file_path().display()
            )
        })
    };
    let gate = refresh_gate(key);
    let _gate = gate.lock_owned().await;

    // Cheap pre-check without the file lock.
    let creds = load()?;
    let is_rejected = rejected.is_some_and(|r| !r.is_empty() && r == creds.access);
    if !is_rejected && !is_token_expired(&creds) {
        return Ok(creds);
    }

    let _file_lock = lock_refresh_file(key).await?;
    // Re-load under the cross-process lock inside the core: another process
    // may have refreshed while we waited.
    ensure_fresh_gated_rejecting(
        Arc::new(Mutex::new(())), // already serialized by the held gate above
        load,
        |refresh| async move { super::provider::refresh(client, provider, &refresh).await },
        |creds| {
            save_provider_auth(key, creds)?;
            note_refreshed(key);
            Ok(())
        },
        rejected,
        || forced_refresh_allowed(provider),
    )
    .await
}

/// Rate limit for forcing a refresh of a not-yet-expired token. Refused if
/// THIS process refreshed the provider within [`FORCED_REFRESH_COOLDOWN`], or
/// if auth.json itself was written that recently (covers other processes —
/// the file is only consulted here, i.e. when disk still holds the rejected
/// token, so an external `synaps login` that changes the token never hits it).
fn forced_refresh_allowed(
    provider: super::provider::OAuthProviderId,
) -> std::result::Result<(), String> {
    let in_process = last_refresh_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(provider.as_str())
        .map(|t| t.elapsed());
    let on_disk = std::fs::metadata(auth_file_path())
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok());
    let age = [in_process, on_disk].into_iter().flatten().min();
    match age {
        Some(age) if age < FORCED_REFRESH_COOLDOWN => Err(format!(
            "{} rejected a token refreshed {}s ago; not refreshing again for {}s \
             (likely an account/permission issue, not a stale token). \
             Run `synaps login` if this persists",
            provider,
            age.as_secs(),
            (FORCED_REFRESH_COOLDOWN - age).as_secs()
        )),
        _ => Ok(()),
    }
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
/// Production goes through [`ensure_fresh_gated_rejecting`] (via
/// `refresh_provider_locked`); this `rejected = None` form is kept as the
/// directly-tested contract.
#[cfg_attr(not(test), allow(dead_code))]
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
    ensure_fresh_gated_rejecting(gate, load, refresh, save, None, || Ok(())).await
}

/// [`ensure_fresh_gated`] with an optional provider-rejected access token.
/// When `rejected_access` equals the loaded (post-gate) access token, the
/// credential is treated as expired regardless of its recorded expiry —
/// provided `may_force` allows it (rate limit). An already-expired credential
/// refreshes normally without consulting `may_force`.
pub(crate) async fn ensure_fresh_gated_rejecting<Load, Refresh, RFut, Save, MayForce>(
    gate: Arc<Mutex<()>>,
    load: Load,
    refresh: Refresh,
    save: Save,
    rejected_access: Option<&str>,
    may_force: MayForce,
) -> std::result::Result<OAuthCredentials, String>
where
    Load: Fn() -> std::result::Result<OAuthCredentials, String>,
    Refresh: FnOnce(String) -> RFut,
    RFut: std::future::Future<Output = std::result::Result<OAuthCredentials, String>>,
    Save: FnOnce(&OAuthCredentials) -> std::result::Result<(), String>,
    MayForce: FnOnce() -> std::result::Result<(), String>,
{
    let _gate = gate.lock_owned().await;
    // Re-read after entering the gate: a preceding waiter may have refreshed
    // and atomically persisted a rotated refresh token.
    let creds = load()?;
    let rejected = rejected_access.is_some_and(|r| !r.is_empty() && r == creds.access);
    let expired = is_token_expired(&creds);
    if !rejected && !expired {
        return Ok(creds);
    }
    if !expired {
        // Forced (rejection-driven) refresh of a not-yet-expired credential.
        may_force()?;
    }
    let fresh = refresh(creds.refresh).await?;
    save(&fresh)?;
    Ok(fresh)
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

    use super::{ensure_fresh_gated, ensure_fresh_gated_rejecting};
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
            self.ensure_inner(gate, None).await
        }

        async fn ensure_rejecting(
            &self,
            gate: Arc<tokio::sync::Mutex<()>>,
            rejected: &str,
        ) -> Result<OAuthCredentials, String> {
            self.ensure_inner(gate, Some(rejected)).await
        }

        async fn ensure_inner(
            &self,
            gate: Arc<tokio::sync::Mutex<()>>,
            rejected: Option<&str>,
        ) -> Result<OAuthCredentials, String> {
            let store = self.clone();
            let refresh_calls = self.refresh_calls.clone();
            ensure_fresh_gated_rejecting(
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
                rejected,
                || Ok(()),
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

    /// Revoked-but-unexpired: the disk credential still holds the exact
    /// access token the provider rejected -> refresh is forced.
    #[tokio::test]
    async fn rejected_unexpired_token_forces_refresh() {
        let store = FakeStore::new(fresh_creds("keep"));
        let rejected = store.creds.lock().unwrap().access.clone();
        let creds = store
            .ensure_rejecting(Arc::new(tokio::sync::Mutex::new(())), &rejected)
            .await
            .unwrap();
        assert_eq!(creds.access, "fresh-access");
        assert_eq!(creds.refresh, "keep-rotated");
        assert_eq!(store.refresh_calls.load(Ordering::SeqCst), 1);
        assert_eq!(store.save_calls.load(Ordering::SeqCst), 1);
    }

    /// Disk already rotated (external `synaps login` / cron / earlier waiter):
    /// the rejected token no longer matches -> no refresh, disk creds returned.
    #[tokio::test]
    async fn rejected_token_differs_from_disk_uses_disk_without_refresh() {
        let store = FakeStore::new(fresh_creds("keep"));
        let creds = store
            .ensure_rejecting(Arc::new(tokio::sync::Mutex::new(())), "some-old-token")
            .await
            .unwrap();
        assert_eq!(creds.refresh, "keep");
        assert_eq!(store.refresh_calls.load(Ordering::SeqCst), 0);
        assert_eq!(store.save_calls.load(Ordering::SeqCst), 0);
    }

    /// Concurrent 401s with the same rejected token: exactly one forced
    /// refresh; later waiters see the rotated disk token and skip.
    #[tokio::test]
    async fn concurrent_rejections_single_flight() {
        let store = FakeStore::new(fresh_creds("r0"));
        let rejected = store.creds.lock().unwrap().access.clone();
        let gate = Arc::new(tokio::sync::Mutex::new(()));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let store = store.clone();
            let gate = gate.clone();
            let rejected = rejected.clone();
            handles.push(tokio::spawn(async move {
                store.ensure_rejecting(gate, &rejected).await
            }));
        }
        for h in handles {
            assert_eq!(h.await.unwrap().unwrap().access, "fresh-access");
        }
        assert_eq!(store.refresh_calls.load(Ordering::SeqCst), 1);
    }

    /// Rate limit: a forced refresh denied by `may_force` spends nothing.
    #[tokio::test]
    async fn forced_refresh_denied_by_cooldown_spends_nothing() {
        let refreshes = Arc::new(AtomicUsize::new(0));
        let r2 = refreshes.clone();
        let err = ensure_fresh_gated_rejecting(
            Arc::new(tokio::sync::Mutex::new(())),
            || Ok(fresh_creds("rt")),
            move |_r| async move {
                r2.fetch_add(1, Ordering::SeqCst);
                Ok(fresh_creds("never"))
            },
            |_c| Ok(()),
            Some("access"),
            || Err("cooldown".to_string()),
        )
        .await
        .unwrap_err();
        assert_eq!(err, "cooldown");
        assert_eq!(refreshes.load(Ordering::SeqCst), 0);
    }

    /// An EXPIRED credential refreshes even inside the cooldown: the rate
    /// limit applies only to forcing an unexpired token.
    #[tokio::test]
    async fn expired_credential_ignores_force_cooldown() {
        let creds = ensure_fresh_gated_rejecting(
            Arc::new(tokio::sync::Mutex::new(())),
            || Ok(expired_creds("rt")),
            |_r| async move { Ok(fresh_creds("rt2")) },
            |_c| Ok(()),
            Some("stale-access"),
            || Err("cooldown".to_string()),
        )
        .await
        .unwrap();
        assert_eq!(creds.refresh, "rt2");
    }

    #[tokio::test]
    async fn refresh_file_lock_excludes_second_holder_until_released() {
        // flock is per open file description, so a second open in the same
        // process contends exactly like another synaps process would.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json.anthropic.refresh.lock");
        let short = std::time::Duration::from_millis(150);
        let held = super::lock_file_at(&path, short).await.unwrap();
        let err = super::lock_file_at(&path, short).await.unwrap_err();
        assert!(err.contains("Timed out"), "{err}");
        drop(held);
        super::lock_file_at(&path, short).await.unwrap();
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
}
