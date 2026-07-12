//! Zero-network GitHub Copilot OAuth harness.
//!
//! Exercises device start → pending → slow_down → authorize → session mint →
//! atomic store, plus denial/expiry/cancel paths, using only injectable fakes.
//! No production GitHub or Copilot host is contacted.
//!
//! Also proves the long-lived GitHub user token cannot leave the broker boundary
//! through AccessToken / remote-shaped wire types.

use agent_core::auth::{
    github_copilot::{
        login_with, mint_credentials, start_device_authorization, validate_device_endpoint,
        validate_session_mint_endpoint, validate_verification_uri, wait_for_device_authorization,
        CopilotAuthError, CopilotBrowser, CopilotCancel, CopilotClock, CopilotHttp,
        InjectedHttpResponse, LoginHooks, PROVIDER, SESSION_MINT_URL,
    },
    load_provider_auth, save_provider_auth, AccessToken, CredentialBroker, LocalBroker,
    OAuthCredentials, OAuthProviderId,
};
use async_trait::async_trait;
use serial_test::serial;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    time::Duration,
};

#[derive(Default)]
struct FakeClock {
    now: Mutex<u64>,
    sleeps: Mutex<Vec<u64>>,
}
impl FakeClock {
    fn new(now: u64) -> Self {
        Self {
            now: Mutex::new(now),
            sleeps: Mutex::new(Vec::new()),
        }
    }
}
#[async_trait]
impl CopilotClock for FakeClock {
    fn now_millis(&self) -> u64 {
        *self.now.lock().unwrap()
    }
    async fn sleep_cancellable<X: CopilotCancel + ?Sized>(
        &self,
        duration: Duration,
        cancel: &X,
    ) -> Result<(), CopilotAuthError> {
        let total_secs = duration.as_secs().max(if duration.is_zero() { 0 } else { 1 });
        if total_secs == 0 {
            if cancel.is_cancelled() {
                return Err(CopilotAuthError::Cancelled);
            }
            return Ok(());
        }
        let mut slept = 0u64;
        for _ in 0..total_secs {
            if cancel.is_cancelled() {
                if slept > 0 {
                    self.sleeps.lock().unwrap().push(slept);
                }
                return Err(CopilotAuthError::Cancelled);
            }
            slept += 1;
            {
                let mut now = self.now.lock().unwrap();
                *now = now.saturating_add(1000);
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        self.sleeps.lock().unwrap().push(slept);
        if cancel.is_cancelled() {
            return Err(CopilotAuthError::Cancelled);
        }
        Ok(())
    }
}

#[allow(dead_code)]
enum Scripted {
    Ok(String),
    Status(u16, String),
}
#[derive(Default)]
struct FakeHttp {
    posts: Mutex<Vec<(String, Vec<(String, String)>)>>,
    gets: Mutex<Vec<(String, String, Vec<(String, String)>)>>,
    post_q: Mutex<Vec<Scripted>>,
    get_q: Mutex<Vec<Scripted>>,
}
impl FakeHttp {
    fn push_post(&self, s: Scripted) {
        self.post_q.lock().unwrap().push(s);
    }
    fn push_get(&self, s: Scripted) {
        self.get_q.lock().unwrap().push(s);
    }
}
#[async_trait]
impl CopilotHttp for FakeHttp {
    async fn post_form(
        &self,
        url: &str,
        form: &[(&str, &str)],
    ) -> Result<InjectedHttpResponse, CopilotAuthError> {
        validate_device_endpoint(url)?;
        self.posts.lock().unwrap().push((
            url.to_string(),
            form.iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        ));
        let next = {
            let mut q = self.post_q.lock().unwrap();
            if q.is_empty() {
                return Err(CopilotAuthError::Transport);
            }
            q.remove(0)
        };
        match next {
            Scripted::Ok(body) => Ok(InjectedHttpResponse { status: 200, body }),
            Scripted::Status(status, body) => Ok(InjectedHttpResponse { status, body }),
        }
    }
    async fn get_bearer(
        &self,
        url: &str,
        bearer: &str,
        headers: &[(&str, &str)],
    ) -> Result<InjectedHttpResponse, CopilotAuthError> {
        validate_session_mint_endpoint(url)?;
        self.gets.lock().unwrap().push((
            url.to_string(),
            bearer.to_string(),
            headers
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        ));
        let next = {
            let mut q = self.get_q.lock().unwrap();
            if q.is_empty() {
                return Err(CopilotAuthError::Transport);
            }
            q.remove(0)
        };
        match next {
            Scripted::Ok(body) => Ok(InjectedHttpResponse { status: 200, body }),
            Scripted::Status(status, body) => Ok(InjectedHttpResponse { status, body }),
        }
    }
}

#[derive(Default)]
struct RecordingBrowser {
    opened: Mutex<Vec<String>>,
}
impl CopilotBrowser for RecordingBrowser {
    fn open(&self, url: &str) -> Result<(), CopilotAuthError> {
        validate_verification_uri(url)?;
        self.opened.lock().unwrap().push(url.to_string());
        Ok(())
    }
}

fn device_body() -> String {
    serde_json::json!({
        "device_code": "device-secret-code-e2e",
        "user_code": "WXYZ-9876",
        "verification_uri": "https://github.com/login/device",
        "expires_in": 900,
        "interval": 5
    })
    .to_string()
}
fn pending() -> String {
    r#"{"error":"authorization_pending"}"#.into()
}
fn slow_down() -> String {
    r#"{"error":"slow_down"}"#.into()
}
fn denied() -> String {
    r#"{"error":"access_denied"}"#.into()
}
fn authorized(tok: &str) -> String {
    serde_json::json!({"access_token": tok, "token_type": "bearer", "scope": "read:user"})
        .to_string()
}
fn session(tok: &str, exp_secs: u64) -> String {
    serde_json::json!({"token": tok, "expires_at": exp_secs}).to_string()
}

fn isolate_home() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("HOME", home.path());
    home
}

#[tokio::test]
#[serial]
async fn github_copilot_oauth_e2e_start_pending_slow_down_authorize_mint_store() {
    let _home = isolate_home();
    let now = 1_700_000_000_000u64;
    let http = FakeHttp::default();
    http.push_post(Scripted::Ok(device_body()));
    http.push_post(Scripted::Ok(pending()));
    http.push_post(Scripted::Ok(slow_down()));
    http.push_post(Scripted::Ok(authorized("gho_e2e_long_lived_secret")));
    http.push_get(Scripted::Ok(session(
        "tid=e2e_session_token",
        now / 1000 + 1800,
    )));
    let clock = FakeClock::new(now);
    let browser = RecordingBrowser::default();
    let cancel = AtomicBool::new(false);
    let seen = Mutex::new(None);
    let creds = login_with(
        &http,
        &clock,
        LoginHooks {
            browser: &browser,
            on_user_code: Some(&|code, uri| {
                *seen.lock().unwrap() = Some((code.to_string(), uri.to_string()));
            }),
        },
        &cancel,
        false,
    )
    .await
    .expect("happy path");

    assert_eq!(creds.refresh, "gho_e2e_long_lived_secret");
    assert_eq!(creds.access, "tid=e2e_session_token");
    assert_eq!(
        seen.lock().unwrap().clone().unwrap().0.as_str(),
        "WXYZ-9876"
    );
    assert_eq!(
        browser.opened.lock().unwrap().as_slice(),
        &["https://github.com/login/device".to_string()]
    );
    let posts = http.posts.lock().unwrap().clone();
    assert!(posts
        .iter()
        .all(|(u, _)| u.starts_with("https://github.com/")));
    assert_eq!(http.gets.lock().unwrap()[0].0, SESSION_MINT_URL);
    let mint_headers: std::collections::HashMap<_, _> =
        http.gets.lock().unwrap()[0].2.iter().cloned().collect();
    assert_eq!(
        mint_headers.get("Copilot-Integration-Id").map(String::as_str),
        Some(agent_core::auth::github_copilot::MINT_COPILOT_INTEGRATION_ID)
    );
    assert!(mint_headers.contains_key("User-Agent"));

    save_provider_auth(
        "anthropic",
        &OAuthCredentials {
            auth_type: "oauth".into(),
            refresh: "keep-me".into(),
            access: "a".into(),
            expires: now + 9_000_000,
            account_id: None,
        },
    )
    .unwrap();
    save_provider_auth(PROVIDER, &creds).unwrap();
    let stored = load_provider_auth(PROVIDER).unwrap().unwrap();
    assert_eq!(stored.access, "tid=e2e_session_token");
    assert_eq!(stored.refresh, "gho_e2e_long_lived_secret");
    assert_eq!(
        load_provider_auth("anthropic").unwrap().unwrap().refresh,
        "keep-me"
    );

    let vended = AccessToken {
        token: stored.access.clone(),
        expires: stored.expires,
    };
    let wire = serde_json::to_value(&vended).unwrap();
    assert_eq!(wire["token"], "tid=e2e_session_token");
    assert!(wire.get("refresh").is_none());
    let wire_s = wire.to_string();
    assert!(!wire_s.contains("gho_e2e_long_lived_secret"));
    assert!(!wire_s.contains("gho_"));
}

#[tokio::test]
#[serial]
async fn github_copilot_oauth_e2e_denial_expiry_cancel_paths() {
    let _home = isolate_home();
    {
        let http = FakeHttp::default();
        http.push_post(Scripted::Ok(device_body()));
        http.push_post(Scripted::Ok(denied()));
        let clock = FakeClock::new(0);
        let authz = start_device_authorization(&http, &clock).await.unwrap();
        let err = wait_for_device_authorization(&http, &clock, &authz, &AtomicBool::new(false))
            .await
            .unwrap_err();
        assert_eq!(err, CopilotAuthError::AccessDenied);
        assert!(http.gets.lock().unwrap().is_empty());
    }
    {
        let http = FakeHttp::default();
        http.push_post(Scripted::Ok(device_body()));
        let clock = FakeClock::new(0);
        let authz = start_device_authorization(&http, &clock).await.unwrap();
        let err = wait_for_device_authorization(&http, &clock, &authz, &AtomicBool::new(true))
            .await
            .unwrap_err();
        assert_eq!(err, CopilotAuthError::Cancelled);
    }
    {
        let http = FakeHttp::default();
        http.push_post(Scripted::Ok(device_body()));
        let clock = FakeClock::new(0);
        let authz = start_device_authorization(&http, &clock).await.unwrap();
        *clock.now.lock().unwrap() = authz.issued_at_ms + authz.expires_in_secs * 1000 + 1;
        let err = wait_for_device_authorization(&http, &clock, &authz, &AtomicBool::new(false))
            .await
            .unwrap_err();
        assert_eq!(err, CopilotAuthError::Expired);
    }
}

#[tokio::test]
#[serial]
async fn github_copilot_remote_broker_cannot_obtain_github_user_token() {
    let _home = isolate_home();
    save_provider_auth(
        PROVIDER,
        &OAuthCredentials {
            auth_type: "oauth".into(),
            refresh: "gho_MUST_NEVER_LEAVE_BROKER".into(),
            access: "tid=session_visible".into(),
            expires: agent_core::epoch_millis() + 3_600_000,
            account_id: None,
        },
    )
    .unwrap();

    let broker = LocalBroker::new(reqwest::Client::new());
    let access = broker
        .access_token(OAuthProviderId::GitHubCopilot)
        .await
        .expect("fresh session token");
    assert_eq!(access.token, "tid=session_visible");
    assert!(!access.token.contains("gho_"));
    let json = serde_json::to_string(&access).unwrap();
    assert!(!json.contains("gho_MUST_NEVER_LEAVE_BROKER"));
    assert!(!json.contains("gho_"));
    assert!(!json.contains("refresh"));

    let caps = broker.capabilities().await.unwrap();
    let caps_json = serde_json::to_string(&caps).unwrap();
    assert!(!caps_json.contains("gho_MUST_NEVER_LEAVE_BROKER"));
    assert!(!caps_json.contains("tid=session_visible"));
}

#[tokio::test]
async fn github_copilot_descriptor_is_registry_driven() {
    let id = OAuthProviderId::GitHubCopilot;
    assert_eq!(id.as_str(), "github-copilot");
    let reg = agent_core::auth::provider::registry();
    let desc = reg.get(id).expect("descriptor");
    assert_eq!(desc.display_name, "GitHub Copilot");
    assert_eq!(
        desc.broker_strategy,
        agent_core::auth::BrokerCredentialStrategy::OAuthAccessToken
    );
}

#[tokio::test]
async fn github_copilot_refresh_remint_path_is_mint_only() {
    let http = FakeHttp::default();
    let now = 1_700_000_000_000u64;
    http.push_get(Scripted::Ok(session("tid=reminted", now / 1000 + 2000)));
    let clock = FakeClock::new(now);
    let creds = mint_credentials(&http, &clock, "gho_refresh_only")
        .await
        .unwrap();
    assert_eq!(creds.access, "tid=reminted");
    assert_eq!(creds.refresh, "gho_refresh_only");
    assert!(http.posts.lock().unwrap().is_empty());
    assert_eq!(http.gets.lock().unwrap()[0].0, SESSION_MINT_URL);
}

#[tokio::test]
async fn github_copilot_e2e_cancel_interrupts_sleep() {
    let http = FakeHttp::default();
    // Long interval so cancel lands mid-sleep, not after a failed empty poll.
    let body = serde_json::json!({
        "device_code": "device-secret-code-e2e",
        "user_code": "WXYZ-9876",
        "verification_uri": "https://github.com/login/device",
        "expires_in": 900,
        "interval": 30
    })
    .to_string();
    http.push_post(Scripted::Ok(body));
    let clock = FakeClock::new(0);
    let authz = start_device_authorization(&http, &clock).await.unwrap();
    let cancel = std::sync::Arc::new(AtomicBool::new(false));
    let flag = std::sync::Arc::clone(&cancel);
    let waiter = tokio::spawn(async move {
        wait_for_device_authorization(&http, &clock, &authz, flag.as_ref()).await
    });
    tokio::time::sleep(Duration::from_millis(10)).await;
    cancel.store(true, Ordering::SeqCst);
    let err = waiter.await.unwrap().unwrap_err();
    assert_eq!(err, CopilotAuthError::Cancelled);
}
