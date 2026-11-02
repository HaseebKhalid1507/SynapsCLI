//! Requests the session makes on its own (keep-warm pings, rollover
//! summaries) under an `auto` account selector: they go to the seat the
//! session is bound to, never select or rebind one, and are not sent when
//! that seat cannot be vended. A 401 on such a request is not recovered.
//!
//! The fixture is a remote credential broker that records every `GET /token`
//! (`account=auto` is a selection, any other label an explicit vend of that
//! seat) and a loopback Anthropic endpoint that records the bearer of every
//! request. Shared with the rolling-summary route tests.
use std::sync::{Arc, Mutex};

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Router,
};

use super::keepwarm::{PingFailure, PingTap};
use crate::test_env::{BaseDirGuard, EnvVarGuard};
use crate::auth::{CredentialRef, OAuthProviderId};
use crate::SharedMessage;

pub(crate) const FAR: u64 = 9_999_999_999_999;
pub(crate) const STALE: u64 = 1;

/// The account a selection would move the session to.
pub(crate) const SELECTED: &str = "other";

#[derive(Clone, Default)]
pub(crate) struct SeatDaemon {
    /// `account` query of every `GET /token`, in order.
    vends: Arc<Mutex<Vec<String>>>,
    /// Accounts the daemon no longer serves (404).
    gone: Arc<Mutex<Vec<String>>>,
    /// When set, a vend of this account is held once received, until
    /// released (see [`SeatDaemon::hold`]).
    held: Arc<Mutex<Option<(String, VendGate)>>>,
    /// Expiry of every vended token (default [`FAR`]).
    expires: Arc<Mutex<Option<u64>>>,
}

/// A vend held at the daemon: `received` is notified once the request has
/// arrived, and the reply waits for `release`.
#[derive(Clone, Default)]
pub(crate) struct VendGate {
    pub(crate) received: Arc<tokio::sync::Notify>,
    pub(crate) release: Arc<tokio::sync::Notify>,
}

impl SeatDaemon {
    pub(crate) async fn spawn() -> (Self, String) {
        let daemon = Self::default();
        let app = Router::new()
            .route(
                "/token",
                get(
                    |State(d): State<SeatDaemon>,
                     Query(q): Query<std::collections::HashMap<String, String>>| async move {
                        let account = q.get("account").cloned().unwrap_or_default();
                        d.vends.lock().unwrap().push(account.clone());
                        let held = d.held.lock().unwrap().clone();
                        if let Some((_, gate)) = held.filter(|(a, _)| *a == account) {
                            gate.received.notify_one();
                            gate.release.notified().await;
                        }
                        if d.gone.lock().unwrap().contains(&account) {
                            return (StatusCode::NOT_FOUND, String::new());
                        }
                        let served = if account == "auto" {
                            SELECTED.to_string()
                        } else {
                            account
                        };
                        let body = serde_json::json!({
                            "access_token": format!("tok-{served}"),
                            "expires": d.expires.lock().unwrap().unwrap_or(FAR),
                            "account": served,
                        });
                        (StatusCode::OK, body.to_string())
                    },
                ),
            )
            .with_state(daemon.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (daemon, format!("http://{addr}"))
    }

    pub(crate) fn vends(&self) -> Vec<String> {
        self.vends.lock().unwrap().clone()
    }

    pub(crate) fn forget(&self, account: &str) {
        self.gone.lock().unwrap().push(account.to_string());
    }

    /// Vend every later token with this expiry.
    pub(crate) fn expire_at(&self, expires: u64) {
        *self.expires.lock().unwrap() = Some(expires);
    }

    /// Hold every vend of `account` until the returned gate is released.
    pub(crate) fn hold(&self, account: &str) -> VendGate {
        let gate = VendGate::default();
        *self.held.lock().unwrap() = Some((account.to_string(), gate.clone()));
        gate
    }
}

#[derive(Clone)]
pub(crate) struct Upstream {
    /// `authorization` of every request, in order.
    auths: Arc<Mutex<Vec<String>>>,
    /// Non-streaming (summary) requests received.
    summaries: Arc<std::sync::atomic::AtomicUsize>,
    status: u16,
}

const SSE_OK: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_bg\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-opus-4-7\",\"stop_reason\":null,",
    "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,",
    "\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,",
    "\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",",
    "\"stop_sequence\":null},\"usage\":{\"input_tokens\":10,\"output_tokens\":1,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

/// Text of every successful non-streaming (summary) reply.
pub(crate) const SUMMARY_TEXT: &str = "## Window 2\nBOUND-SEAT";

/// A loopback Anthropic endpoint answering every request with `status`
/// (200: an SSE reply to a streaming request, a message otherwise).
pub(crate) async fn spawn_upstream(status: u16) -> (Upstream, EnvVarGuard) {
    let upstream = Upstream {
        auths: Arc::default(),
        summaries: Arc::default(),
        status,
    };
    let app = Router::new()
        .route(
            "/v1/messages",
            post(
                |State(u): State<Upstream>, headers: HeaderMap, body: String| async move {
                    u.auths.lock().unwrap().push(
                        headers
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("")
                            .to_string(),
                    );
                    if !body.contains("\"stream\":true") {
                        u.summaries.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                    if u.status != 200 {
                        return (
                            StatusCode::from_u16(u.status).unwrap(),
                            [("content-type", "application/json")],
                            r#"{"type":"error","error":{"type":"authentication_error","message":"synthetic"}}"#
                                .to_string(),
                        )
                            .into_response();
                    }
                    if body.contains("\"stream\":true") {
                        return (
                            StatusCode::OK,
                            [("content-type", "text/event-stream")],
                            SSE_OK.to_string(),
                        )
                            .into_response();
                    }
                    let message = serde_json::json!({
                        "id": "s", "type": "message", "role": "assistant",
                        "model": "claude-sonnet-4-6",
                        "content": [{"type": "text", "text": SUMMARY_TEXT}],
                        "stop_reason": "end_turn",
                        "usage": {"input_tokens": 700, "output_tokens": 70},
                    });
                    (
                        StatusCode::OK,
                        [("content-type", "application/json")],
                        message.to_string(),
                    )
                        .into_response()
                },
            ),
        )
        .layer(axum::extract::DefaultBodyLimit::max(64 * 1024 * 1024))
        .with_state(upstream.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let guard = EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &format!("http://{addr}"));
    (upstream, guard)
}

impl Upstream {
    pub(crate) fn auths(&self) -> Vec<String> {
        self.auths.lock().unwrap().clone()
    }

    pub(crate) fn summaries(&self) -> usize {
        self.summaries.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// `rt` on the remote broker at `endpoint`.
pub(crate) fn on_remote(mut rt: crate::Runtime, endpoint: &str) -> crate::Runtime {
    rt.credential_source = crate::auth::CredentialSource::Remote {
        endpoint: endpoint.to_string(),
        machine_token: "machine-token".to_string(),
    };
    rt
}

fn seat(label: &str) -> CredentialRef {
    CredentialRef::new(
        OAuthProviderId::Anthropic,
        crate::auth::Account::parse(label).unwrap(),
    )
}

/// What the last turn left behind: the session bound to `label` on its
/// source, holding a token with this expiry. `None` unbinds.
pub(crate) async fn bind(rt: &crate::Runtime, label: Option<&str>, expires: u64) {
    let mut g = rt.auth.write().await;
    g.auth_token = format!("tok-held-{}", label.unwrap_or("none"));
    g.auth_type = "oauth".into();
    g.refresh_token = None;
    g.token_expires = Some(expires);
    g.bound_credential =
        label.map(|l| super::auth::anthropic_binding(&rt.credential_source, &seat(l)));
}

/// The session's seat binding: (held token, bound credential).
pub(crate) async fn binding(rt: &crate::Runtime) -> (String, Option<String>) {
    let g = rt.auth.read().await;
    (g.auth_token.clone(), g.bound_credential.clone())
}

/// Every field of the session's shared auth state: (token, auth type,
/// refresh token, expiry, bound credential).
pub(crate) type AuthFields = (String, String, Option<String>, Option<u64>, Option<String>);

pub(crate) async fn shared_auth(rt: &crate::Runtime) -> AuthFields {
    let g = rt.auth.read().await;
    (
        g.auth_token.clone(),
        g.auth_type.clone(),
        g.refresh_token.clone(),
        g.token_expires,
        g.bound_credential.clone(),
    )
}

/// Runs `background` while a foreground turn start rebinds the session
/// under `auto` (the daemon selects [`SELECTED`]): the background vend of
/// `held` is paused at the daemon until the rebind has finished.
pub(crate) async fn during_a_foreground_rebind<T>(
    rt: &crate::Runtime,
    daemon: &SeatDaemon,
    held: &str,
    background: impl std::future::Future<Output = T>,
) -> T {
    let gate = daemon.hold(held);
    let rebind = async {
        tokio::time::timeout(std::time::Duration::from_secs(10), gate.received.notified())
            .await
            .expect("the background vend was reached");
        rt.refresh_if_needed().await.expect("the foreground rebind");
        gate.release.notify_one();
    };
    let (out, ()) = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        tokio::join!(background, rebind)
    })
    .await
    .expect("bounded");
    out
}

/// The bound credential `label` has on `rt`'s source.
pub(crate) fn bound_to(rt: &crate::Runtime, label: &str) -> Option<String> {
    Some(super::auth::anthropic_binding(
        &rt.credential_source,
        &seat(label),
    ))
}

fn opus() -> crate::Runtime {
    let mut rt = crate::Runtime::new_headless();
    rt.set_model("claude-opus-4-7".to_string());
    rt.set_cache_ttl(crate::core::config::CacheTtl::OneHour);
    rt.set_system_prompt("You are a keep-warm test.".into());
    rt
}

fn history() -> Vec<SharedMessage> {
    vec![
        Arc::new(serde_json::json!({"role": "user", "content": "hello"})),
        Arc::new(serde_json::json!({
            "role": "assistant",
            "content": [{"type": "text", "text": "hi"}]
        })),
    ]
}

struct Pinged {
    result: std::result::Result<(), PingFailure>,
    sent: bool,
}

async fn ping(rt: &crate::Runtime) -> Pinged {
    let history = history();
    let fp = rt
        .cache_keepwarm_probe(&history)
        .await
        .expect("the probe")
        .fingerprint;
    let tap = PingTap::default();
    let report = rt
        .cache_keepwarm_ping(
            &history,
            fp,
            &tokio_util::sync::CancellationToken::new(),
            &tap,
        )
        .await;
    Pinged {
        result: report.result.map(drop),
        sent: tap.was_sent(),
    }
}

/// Under `auto`, a ping on a stale token re-vends the bound seat by name
/// (no selection) and goes out on it; the binding stays.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn a_ping_under_auto_re_vends_the_bound_seat_and_never_selects() {
    let _base = BaseDirGuard::new();
    let _auto = EnvVarGuard::set("SYNAPS_ACCOUNT_ANTHROPIC", "auto");
    let (daemon, endpoint) = SeatDaemon::spawn().await;
    let (upstream, _url) = spawn_upstream(200).await;
    let rt = on_remote(opus(), &endpoint);
    bind(&rt, Some("work"), STALE).await;
    let before = shared_auth(&rt).await;

    let out = ping(&rt).await;

    assert!(out.result.is_ok(), "{:?}", out.result);
    assert_eq!(daemon.vends(), vec!["work"], "the bound seat, by name");
    assert_eq!(upstream.auths(), vec!["Bearer tok-work"], "the re-vended token");
    assert_eq!(
        shared_auth(&rt).await,
        before,
        "the re-vended token is the request's own: shared auth untouched"
    );
}

/// A ping whose bound seat is re-vended while a foreground turn start
/// rebinds the session to another seat never restores the old seat: the
/// rebind survives, and the ping went out on the old seat's re-vended
/// token (or not at all).
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn a_background_vend_never_undoes_a_concurrent_foreground_rebind() {
    let _base = BaseDirGuard::new();
    let _auto = EnvVarGuard::set("SYNAPS_ACCOUNT_ANTHROPIC", "auto");
    let (daemon, endpoint) = SeatDaemon::spawn().await;
    let (upstream, _url) = spawn_upstream(200).await;
    let rt = on_remote(opus(), &endpoint);
    bind(&rt, Some("work"), STALE).await;

    let out = during_a_foreground_rebind(&rt, &daemon, "work", ping(&rt)).await;

    assert_eq!(daemon.vends(), vec!["work", "auto"]);
    assert_eq!(
        binding(&rt).await,
        (format!("tok-{SELECTED}"), bound_to(&rt, SELECTED)),
        "the foreground rebind survives"
    );
    if out.sent {
        assert_eq!(upstream.auths(), vec!["Bearer tok-work"], "the old seat's token");
    } else {
        assert!(upstream.auths().is_empty());
    }
}

/// An explicit selector with no seat bound yet: the ping is not sent, and
/// nothing is vended or bound.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn a_ping_with_an_unbound_explicit_selector_is_not_sent() {
    let _base = BaseDirGuard::new();
    let _work = EnvVarGuard::set("SYNAPS_ACCOUNT_ANTHROPIC", "work");
    let (daemon, endpoint) = SeatDaemon::spawn().await;
    let (upstream, _url) = spawn_upstream(200).await;
    let rt = on_remote(opus(), &endpoint);
    bind(&rt, None, STALE).await;
    let before = shared_auth(&rt).await;

    let out = ping(&rt).await;

    assert!(matches!(out.result, Err(PingFailure::Fatal(_))), "{:?}", out.result);
    assert!(!out.sent);
    assert!(daemon.vends().is_empty(), "no vend");
    assert!(upstream.auths().is_empty(), "nothing sent");
    assert_eq!(shared_auth(&rt).await, before, "nothing bound");
}

/// Under `auto`, a ping whose seat can no longer be vended (or with no seat
/// bound) is not sent: no selection, no request, binding untouched.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn a_ping_whose_seat_cannot_be_vended_is_not_sent() {
    let _base = BaseDirGuard::new();
    let _auto = EnvVarGuard::set("SYNAPS_ACCOUNT_ANTHROPIC", "auto");
    let (daemon, endpoint) = SeatDaemon::spawn().await;
    let (upstream, _url) = spawn_upstream(200).await;
    daemon.forget("work");
    for label in [Some("work"), None] {
        let rt = on_remote(opus(), &endpoint);
        bind(&rt, label, STALE).await;
        let before = binding(&rt).await;

        let out = ping(&rt).await;

        assert!(
            matches!(out.result, Err(PingFailure::Fatal(_))),
            "{label:?}: {:?}",
            out.result
        );
        assert!(!out.sent, "{label:?}");
        assert_eq!(binding(&rt).await, before, "{label:?}: binding untouched");
    }
    assert_eq!(daemon.vends(), vec!["work"], "the bound seat only; no selection");
    assert!(upstream.auths().is_empty(), "nothing sent");
}

/// A 401 on a ping is the ping's failure: one request, no credential
/// recovery (no vend, no retry), binding untouched.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn a_rejected_ping_is_not_recovered_or_retried() {
    let _base = BaseDirGuard::new();
    let _auto = EnvVarGuard::set("SYNAPS_ACCOUNT_ANTHROPIC", "auto");
    let (daemon, endpoint) = SeatDaemon::spawn().await;
    let (upstream, _url) = spawn_upstream(401).await;
    let rt = on_remote(opus(), &endpoint);
    bind(&rt, Some("work"), FAR).await;
    let before = binding(&rt).await;

    let out = ping(&rt).await;

    assert!(out.result.is_err(), "{:?}", out.result);
    assert_eq!(upstream.auths(), vec!["Bearer tok-held-work"], "one request");
    assert!(daemon.vends().is_empty(), "no recovery vend");
    assert_eq!(binding(&rt).await, before);
}
