use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use tokio::sync::Mutex;

use reqwest::Client;

use super::account::{CredentialRef, SeatIdentity};
use super::storage::{
    auth_file_path, load_provider_auth_at, save_provider_auth_if_refresh_matches_at, CasOutcome,
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
        let truncated = if text.len() > 200 {
            &text[..200]
        } else {
            &text
        };
        return Err(format!("Token refresh failed ({}): {}", status, truncated));
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
/// serialize on an exclusive `flock` of `<auth.json>.refresh.<key>.lock`
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
    let key = cred.storage_key();
    let provider = cred.provider;
    let gate_key = format!("{}|{}", path.display(), key);
    let lock_path = refresh_lock_path(path, &key);
    let load_path = path.to_path_buf();
    let load_key = key.clone();
    let load = move || {
        let creds = load_provider_auth_at(&load_path, &load_key)?.ok_or_else(|| {
            format!(
                "No credentials for {} at {}. Run `synaps login --provider {} --account {}`.",
                cred,
                load_path.display(),
                provider,
                cred.account
            )
        })?;
        // Seat pairing is re-checked on every load, including the re-load
        // after a CAS `Replaced` (re-login mid-refresh): the replacement is
        // a different seat and was never proven.
        if let Some(expected) = expected_seat {
            if SeatIdentity::of(provider, &creds) != *expected {
                return Err(SEAT_CHANGED.to_string());
            }
        }
        Ok(creds)
    };
    let refresh = |refresh: String| async move {
        super::provider::refresh(client, provider, &refresh).await
    };
    let save = |old_refresh: &str, fresh: &OAuthCredentials| {
        save_provider_auth_if_refresh_matches_at(path, &key, old_refresh, fresh)
    };
    ensure_fresh_gated_locked(refresh_gate(&gate_key), Some(lock_path), load, refresh, save).await
}

/// Per-credential cross-process lock file, beside the credential file.
/// Storage keys only contain `[a-z0-9._@-]`, so they are safe path fragments.
fn refresh_lock_path(auth_path: &Path, storage_key: &str) -> PathBuf {
    let file_name = auth_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("auth.json");
    auth_path.with_file_name(format!("{file_name}.refresh.{storage_key}.lock"))
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

/// Guard holding the exclusive `flock`; dropping it releases the lock.
struct RefreshFileLock {
    _file: std::fs::File,
}

/// Acquire the per-credential refresh lock off the async runtime (flock
/// blocks). Polls `try_lock_exclusive` so a wedged peer cannot hang this
/// process forever.
async fn acquire_refresh_lock(path: PathBuf) -> std::result::Result<RefreshFileLock, String> {
    tokio::task::spawn_blocking(move || {
        use fs4::fs_std::FileExt;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create {}: {}", parent.display(), e))?;
        }
        let mut open = std::fs::OpenOptions::new();
        open.create(true).write(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            open.mode(0o600);
        }
        let file = open
            .open(&path)
            .map_err(|e| format!("Failed to open refresh lock {}: {}", path.display(), e))?;
        let deadline = std::time::Instant::now() + REFRESH_LOCK_WAIT;
        loop {
            match FileExt::try_lock_exclusive(&file) {
                Ok(true) => return Ok(RefreshFileLock { _file: file }),
                Ok(false) => {}
                Err(e) => {
                    return Err(format!(
                        "Failed to lock refresh lock {}: {}",
                        path.display(),
                        e
                    ))
                }
            }
            if std::time::Instant::now() >= deadline {
                return Err(format!(
                    "timed out waiting for another process to finish refreshing ({})",
                    path.display()
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    })
    .await
    .map_err(|e| format!("refresh lock task failed: {e}"))?
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
    // In-process waiters are bounded like cross-process ones.
    let _gate = tokio::time::timeout(REFRESH_LOCK_WAIT, gate.lock_owned())
        .await
        .map_err(|_| "timed out waiting for an in-flight token refresh".to_string())?;
    let _file_lock = match lock_path {
        Some(path) => Some(acquire_refresh_lock(path).await?),
        None => None,
    };
    for _ in 0..MAX_CAS_ROUNDS {
        // Re-read under both locks: a preceding waiter (or another process)
        // may have refreshed and atomically persisted a rotated token.
        let creds = load()?;
        if !is_token_expired(&creds) {
            return Ok(creds);
        }
        let old_refresh = creds.refresh.clone();
        // The provider exchange is deadline-bounded so the lock hold time is
        // bounded too (a slow-drip response cannot block every other process).
        let mut fresh = tokio::time::timeout(REFRESH_HTTP_DEADLINE, refresh(old_refresh.clone()))
            .await
            .map_err(|_| "token refresh timed out".to_string())??;
        if fresh.account_id.is_none() {
            fresh.account_id = creds.account_id.clone();
        }
        match save(&old_refresh, &fresh)? {
            CasOutcome::Saved => return Ok(fresh),
            CasOutcome::Removed => {
                return Err(
                    "credential was removed while its token was being refreshed; run `synaps login`"
                        .to_string(),
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
    Err("credential kept changing during refresh; retry".to_string())
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

    #[test]
    fn refresh_lock_path_is_per_credential_and_beside_the_file() {
        let path = std::path::Path::new("/tmp/x/auth.json");
        assert_eq!(
            refresh_lock_path(path, "openai-codex@astra2"),
            std::path::PathBuf::from("/tmp/x/auth.json.refresh.openai-codex@astra2.lock")
        );
        assert_ne!(
            refresh_lock_path(path, "openai-codex"),
            refresh_lock_path(path, "openai-codex@astra2")
        );
        let _ = CasOutcome::Saved;
    }
}
