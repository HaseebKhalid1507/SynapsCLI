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
        let truncated = if text.len() > 200 { &text[..200] } else { &text };
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
        let truncated = if text.len() > 200 { &text[..200] } else { &text };
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
    let key = provider.as_str();
    ensure_fresh_gated(
        refresh_gate(key),
        || {
            load_provider_auth(key)?.ok_or_else(|| {
                format!(
                    "No credentials for {} at {}. Run `synaps login`.",
                    provider,
                    auth_file_path().display()
                )
            })
        },
        |refresh| async move { super::provider::refresh(client, provider, &refresh).await },
        |creds| save_provider_auth(key, creds),
    )
    .await
}

/// Generic single-flight refresh core. Separated from network/storage so the
/// concurrency, rotation, and failure invariants are directly testable.
///
/// Contract:
/// 1. Exactly one refresh runs at a time per gate; waiters re-`load` after the
///    gate and observe the rotated credential without a second refresh.
/// 2. `save` runs only after a successful refresh (failures persist nothing).
/// 3. The refreshed credential is persisted before the gate is released.
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
}
