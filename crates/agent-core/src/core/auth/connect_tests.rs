//! `auth connect --json` driven end to end in process: a loopback fake for
//! the token and profile endpoints, the browser simulated by one HTTP
//! request to the callback listener, stdin as a duplex pipe, a temp store.
//! Nothing here reaches a real provider or the user's store.

use super::*;
use std::net::TcpListener as StdListener;
use std::path::Path;
use std::sync::atomic::{AtomicU16, AtomicUsize};

use axum::http::StatusCode;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::{json, Value};
use tokio::io::{AsyncWriteExt, DuplexStream};
use tokio::sync::oneshot;

const CLAUDE_ALLOW: &str = ALLOWED_AUTHORIZE_URLS[0];
const CODEX_ALLOW: &str = ALLOWED_AUTHORIZE_URLS[1];
const SEAT_A: &str = "acct-anthropic-a";
const SEAT_B: &str = "acct-anthropic-b";
const SEAT_C: &str = "acct-anthropic-c";
const CODEX_SEAT: &str = "acct_codex_0001";
const LONG: Duration = Duration::from_secs(600);
/// The bound the app relies on for "exit promptly" after EOF or a signal.
const PROMPT: Duration = Duration::from_secs(1);

fn fp(provider: OAuthProviderId, id: &str) -> String {
    super::super::account::seat_fingerprint(provider, id).unwrap()
}

// ── Fake provider ────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TokenMode {
    Ok,
    Delay(u64),
    Stall,
    Reject,
}

struct FakeState {
    mode: Mutex<TokenMode>,
    access: Mutex<String>,
    profile_seat: Mutex<String>,
    profile_email: Mutex<String>,
    profile_status: AtomicU16,
    token_hits: AtomicUsize,
}

struct Fake {
    state: Arc<FakeState>,
    base: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Fake {
    fn token_hits(&self) -> usize {
        self.state.token_hits.load(Ordering::SeqCst)
    }
}

async fn fake(mode: TokenMode, access: &str, seat: &str, email: &str) -> Fake {
    let state = Arc::new(FakeState {
        mode: Mutex::new(mode),
        access: Mutex::new(access.to_string()),
        profile_seat: Mutex::new(seat.to_string()),
        profile_email: Mutex::new(email.to_string()),
        profile_status: AtomicU16::new(200),
        token_hits: AtomicUsize::new(0),
    });
    let token_state = state.clone();
    let profile_state = state.clone();
    let app = axum::Router::new()
        .route(
            "/token",
            axum::routing::post(move || {
                let s = token_state.clone();
                async move {
                    s.token_hits.fetch_add(1, Ordering::SeqCst);
                    let mode = *s.mode.lock().unwrap();
                    let access = s.access.lock().unwrap().clone();
                    match mode {
                        TokenMode::Ok => {}
                        TokenMode::Delay(ms) => tokio::time::sleep(Duration::from_millis(ms)).await,
                        TokenMode::Stall => tokio::time::sleep(Duration::from_secs(3600)).await,
                        TokenMode::Reject => {
                            return (
                                StatusCode::BAD_REQUEST,
                                axum::Json(json!({"error": "invalid_grant"})),
                            )
                        }
                    }
                    (
                        StatusCode::OK,
                        axum::Json(json!({
                            "access_token": access,
                            "refresh_token": "rt-fake-new",
                            "expires_in": 3600,
                        })),
                    )
                }
            }),
        )
        .route(
            "/profile",
            axum::routing::get(move || {
                let s = profile_state.clone();
                async move {
                    let status = s.profile_status.load(Ordering::SeqCst);
                    let body = json!({
                        "account": {
                            "uuid": *s.profile_seat.lock().unwrap(),
                            "email": *s.profile_email.lock().unwrap(),
                        },
                        "organization": {"uuid": "org-fake", "organization_type": "claude_max"},
                    });
                    (StatusCode::from_u16(status).unwrap(), axum::Json(body))
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Fake { state, base, task }
}

async fn claude_fake(mode: TokenMode, seat: &str, email: &str) -> Fake {
    fake(mode, "at-fake-new", seat, email).await
}

fn codex_jwt(account_id: &str, email: &str) -> String {
    let claims = json!({
        "https://api.openai.com/auth": {"chatgpt_account_id": account_id},
        "https://api.openai.com/profile": {"email": email},
    });
    format!(
        "{}.{}.sig",
        URL_SAFE_NO_PAD.encode(r#"{"alg":"none"}"#),
        URL_SAFE_NO_PAD.encode(claims.to_string())
    )
}

async fn codex_fake(mode: TokenMode, seat: &str, email: &str) -> Fake {
    fake(mode, &codex_jwt(seat, email), "unused", "unused").await
}

fn free_port() -> u16 {
    StdListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// The production endpoints with the token/profile hosts swapped for the
/// fake and the listener moved to `port` (the URL text stays production).
fn env_for(dir: &Path, fake: &Fake, port: u16) -> ConnectEnv {
    let mut anthropic = BrowserEndpoints::anthropic();
    anthropic.token_url = format!("{}/token", fake.base);
    anthropic.callback_port = port;
    let mut openai_codex = BrowserEndpoints::openai_codex();
    openai_codex.token_url = format!("{}/token", fake.base);
    openai_codex.callback_port = port;
    ConnectEnv {
        store: dir.join("auth.json"),
        inherited_store: false,
        policy: AccountPolicy::default(),
        anthropic,
        openai_codex,
        profile_url: Some(format!("{}/profile", fake.base)),
    }
}

fn args(provider: &str, account: &str, expect_seat: Option<&str>) -> ConnectArgs {
    ConnectArgs {
        provider: provider.into(),
        account: account.into(),
        expect_seat: expect_seat.map(str::to_string),
        timeout_secs: None,
    }
}

/// [`drive`] owning its args, so a call can sit in `tokio::join!`.
async fn drive_owned<W, R, I>(
    env: &ConnectEnv,
    args: ConnectArgs,
    timeout: Duration,
    out: W,
    stdin: R,
    interrupt: I,
) -> i32
where
    W: Write + Send,
    R: AsyncRead + Unpin,
    I: Future<Output = ()>,
{
    drive(env, &args, timeout, out, stdin, interrupt).await
}

// ── Store fixtures ───────────────────────────────────────────────────────────

fn slot(seat: Option<&str>, label: Option<&str>) -> Value {
    let mut v = json!({
        "type": "oauth",
        "refresh": "rt-old",
        "access": "at-old",
        "expires": 1_700_000_000_000u64,
        "addedAt": 1_700_000_000u64,
    });
    if let Some(seat) = seat {
        v["accountId"] = json!(seat);
    }
    if let Some(label) = label {
        v["label"] = json!(label);
    }
    v
}

fn seed(store: &Path, root: Value) -> Vec<u8> {
    let bytes = serde_json::to_vec_pretty(&root).unwrap();
    std::fs::write(store, &bytes).unwrap();
    bytes
}

fn stored(store: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(store).unwrap()).unwrap()
}

// ── Process plumbing: stdout capture, stdin pipe, interrupt ─────────────────

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Capture {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }

    fn lines(&self) -> Vec<Value> {
        self.text()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn events(&self) -> Vec<String> {
        self.lines()
            .iter()
            .map(|l| l["event"].as_str().unwrap().to_string())
            .collect()
    }

    fn done(&self) -> Value {
        let lines = self.lines();
        let done: Vec<_> = lines.iter().filter(|l| l["event"] == "done").collect();
        assert_eq!(done.len(), 1, "exactly one done line: {}", self.text());
        assert_eq!(lines.last().unwrap()["event"], "done", "done is last");
        done[0].clone()
    }

    async fn wait_for(&self, event: &str) -> Value {
        let until = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(line) = self.lines().into_iter().find(|l| l["event"] == event) {
                return line;
            }
            assert!(
                std::time::Instant::now() < until,
                "no {event} line in {:?}",
                self.events()
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

struct BrokenPipe;

impl Write for BrokenPipe {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::ErrorKind::BrokenPipe.into())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Err(std::io::ErrorKind::BrokenPipe.into())
    }
}

fn stdin_pipe() -> (DuplexStream, DuplexStream) {
    tokio::io::duplex(4096)
}

fn interrupt_pair() -> (oneshot::Sender<()>, impl Future<Output = ()>) {
    let (tx, rx) = oneshot::channel::<()>();
    (tx, async move {
        if rx.await.is_err() {
            std::future::pending::<()>().await;
        }
    })
}

fn never() -> impl Future<Output = ()> {
    std::future::pending::<()>()
}

fn query_param(url: &str, key: &str) -> String {
    url::Url::parse(url)
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
        .unwrap_or_else(|| panic!("no {key} in the authorize URL"))
}

/// The browser following the provider's redirect to the callback.
async fn redirect(port: u16, path: &str, query: &str) {
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let _ = client
        .get(format!("http://127.0.0.1:{port}{path}?{query}"))
        .send()
        .await;
}

/// Wait for `open_url` and approve: the redirect with a code and the state.
async fn approve(out: &Capture, port: u16, path: &str) {
    let open = out.wait_for("open_url").await;
    let state = query_param(open["url"].as_str().unwrap(), "state");
    redirect(port, path, &format!("code=c0de&state={}", urlencoding::encode(&state))).await;
}

async fn assert_port_free(port: u16) {
    for _ in 0..100 {
        if StdListener::bind(("127.0.0.1", port)).is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("callback port {port} still bound after the connect ended");
}

fn listed_row(store: &Path, provider: OAuthProviderId, label: &str) -> AccountSummary {
    super::super::storage::list_accounts_detailed_at(store, Some(provider))
        .unwrap()
        .accounts
        .into_iter()
        .find(|row| row.label == label)
        .unwrap_or_else(|| panic!("{label} not listed"))
}

// ── Fixtures (tests/fixtures/accounts_v1/connect_*.jsonl) ───────────────────

fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/accounts_v1")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Replace the value after every `marker` (up to the first `stop` char).
fn redact(text: &str, marker: &str, stop: &[char], with: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find(marker) {
        let (head, tail) = rest.split_at(at + marker.len());
        out.push_str(head);
        out.push_str(with);
        let end = tail.find(stop).unwrap_or(tail.len());
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// A transcript with its run-specific values (clock, PKCE, state) fixed.
fn normalized(text: &str) -> String {
    let text = redact(text, "\"expires_at_ms\":", &[',', '}'], "1800000600000");
    let text = redact(&text, "code_challenge=", &['&', '"'], "fixture");
    let text = redact(&text, "&state=", &['&', '"'], "fixture");
    let text = redact(&text, "\"expires\":", &[',', '}'], "1800003600000");
    redact(&text, "\"added_at\":", &[',', '}'], "1800000000")
}

fn assert_fixture(name: &str, out: &Capture) {
    let actual = normalized(&out.text());
    assert_eq!(actual, fixture(name), "{name} drifted; actual:\n{actual}");
}

// ── Pure pieces ──────────────────────────────────────────────────────────────

#[test]
fn reasons_are_the_closed_contract_set() {
    let wire: Vec<&str> = ConnectReason::ALL.iter().map(|r| r.as_str()).collect();
    assert_eq!(
        wire,
        [
            "cancelled",
            "timeout",
            "denied",
            "state_mismatch",
            "port_busy",
            "slot_exists",
            "not_found",
            "conflict",
            "duplicate_seat",
            "identity_changed",
            "bad_label",
            "refused_profile",
            "unsupported_provider",
            "provider_error",
            "network",
            "store_busy",
            "internal",
        ]
    );
}

#[test]
fn timeout_defaults_to_600_and_is_clamped_to_30_through_1800() {
    assert_eq!(clamp_timeout(None), Duration::from_secs(600));
    assert_eq!(clamp_timeout(Some(0)), Duration::from_secs(30));
    assert_eq!(clamp_timeout(Some(29)), Duration::from_secs(30));
    assert_eq!(clamp_timeout(Some(45)), Duration::from_secs(45));
    assert_eq!(clamp_timeout(Some(1801)), Duration::from_secs(1800));
    assert_eq!(clamp_timeout(Some(u64::MAX)), Duration::from_secs(1800));
}

#[test]
fn login_failures_map_onto_their_reasons() {
    for (kind, reason) in [
        (LoginFailureKind::Denied, "denied"),
        (LoginFailureKind::StateMismatch, "state_mismatch"),
        (LoginFailureKind::PortBusy, "port_busy"),
        (LoginFailureKind::ProviderError, "provider_error"),
        (LoginFailureKind::Network, "network"),
        (LoginFailureKind::Internal, "internal"),
    ] {
        assert_eq!(ConnectReason::from_login(kind).as_str(), reason);
    }
}

#[test]
fn json_sink_refuses_urls_outside_the_contract() {
    let out = Capture::default();
    let mut sink = JsonSink {
        out: Arc::new(Mutex::new(LineOut {
            w: out.clone(),
            broken: false,
        })),
        phase: Arc::new(AtomicU8::new(WAITING)),
    };
    assert!(!sink.paste_fallback());
    for (url, allow) in [
        ("https://evil.example/oauth/authorize?x=1", "https://evil.example/oauth/authorize"),
        ("https://claude.ai/oauth/authorizeX?x=1", CLAUDE_ALLOW),
        ("https://claude.ai/oauth/authorize", CLAUDE_ALLOW),
        (&format!("{CLAUDE_ALLOW}?{}", "a".repeat(MAX_URL_BYTES)), CLAUDE_ALLOW),
    ] {
        assert!(sink.authorize(url, allow).is_err(), "{url}");
    }
    assert_eq!(out.text(), "", "nothing printed for a refused URL");
    sink.authorize(&format!("{CODEX_ALLOW}?a=b"), CODEX_ALLOW).unwrap();
    assert_eq!(
        out.text(),
        "{\"v\":1,\"event\":\"open_url\",\"url\":\"https://auth.openai.com/oauth/authorize?a=b\",\"allow\":\"https://auth.openai.com/oauth/authorize\"}\n"
    );
}

// ── Preconditions: a single done line, before anything opens ────────────────

async fn pre_start(env: &ConnectEnv, a: &ConnectArgs) -> (i32, Capture) {
    let out = Capture::default();
    let (_stdin, stdin_rx) = stdin_pipe();
    let code = drive(env, a, LONG, out.clone(), stdin_rx, never()).await;
    (code, out)
}

#[tokio::test]
async fn bad_labels_are_refused_before_started() {
    let dir = tempfile::tempdir().unwrap();
    let fake = claude_fake(TokenMode::Ok, SEAT_A, "a@example.com").await;
    let env = env_for(dir.path(), &fake, free_port());
    for label in ["", " claude3", "claude3 ", "Claude3", "-x", "auto", "a/b", &"a".repeat(33)] {
        let (code, out) = pre_start(&env, &args("anthropic", label, None)).await;
        assert_eq!(code, 1, "{label:?}");
        assert_fixture("connect_bad_label.jsonl", &out);
    }
    assert!(!env.store.exists());
    assert_eq!(fake.token_hits(), 0);
}

#[tokio::test]
async fn only_anthropic_and_openai_codex_connect() {
    let dir = tempfile::tempdir().unwrap();
    let fake = claude_fake(TokenMode::Ok, SEAT_A, "a@example.com").await;
    let env = env_for(dir.path(), &fake, free_port());
    for provider in ["xai-auth", "github-copilot", "claude", "codex", "Anthropic", ""] {
        let (code, out) = pre_start(&env, &args(provider, "claude3", None)).await;
        assert_eq!(code, 1, "{provider:?}");
        assert_fixture("connect_unsupported_provider.jsonl", &out);
    }
}

#[tokio::test]
async fn an_inherited_profile_store_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let fake = claude_fake(TokenMode::Ok, SEAT_A, "a@example.com").await;
    let mut env = env_for(dir.path(), &fake, free_port());
    env.inherited_store = true;
    let (code, out) = pre_start(&env, &args("anthropic", "claude3", None)).await;
    assert_eq!(code, 1);
    assert_fixture("connect_refused_profile.jsonl", &out);
}

#[tokio::test]
async fn add_into_a_taken_slot_is_slot_exists() {
    let dir = tempfile::tempdir().unwrap();
    let fake = claude_fake(TokenMode::Ok, SEAT_A, "a@example.com").await;
    let port = free_port();
    let env = env_for(dir.path(), &fake, port);
    let before = seed(
        &env.store,
        json!({
            "anthropic": slot(Some(SEAT_A), None),
            "anthropic@claude3": slot(Some(SEAT_B), Some("claude3")),
            "openai-codex": {"type": "api", "key": "not-oauth"},
        }),
    );
    for (provider, label) in [
        ("anthropic", "default"),
        ("anthropic", "claude3"),
        ("openai-codex", "default"),
    ] {
        let (code, out) = pre_start(&env, &args(provider, label, None)).await;
        assert_eq!(code, 1, "{provider}/{label}");
        assert_fixture("connect_slot_exists.jsonl", &out);
    }
    assert_eq!(std::fs::read(&env.store).unwrap(), before);
    // Nothing listened: the port was never bound.
    assert_port_free(port).await;
}

#[tokio::test]
async fn reauth_of_a_missing_slot_is_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let fake = claude_fake(TokenMode::Ok, SEAT_A, "a@example.com").await;
    let env = env_for(dir.path(), &fake, free_port());
    let want = fp(OAuthProviderId::Anthropic, SEAT_A);
    let (code, out) = pre_start(&env, &args("anthropic", "claude3", Some(&want))).await;
    assert_eq!(code, 1);
    assert_fixture("connect_not_found.jsonl", &out);
    // A non-OAuth entry is no account either.
    seed(&env.store, json!({"anthropic": {"type": "api", "key": "k"}}));
    let (_, out) = pre_start(&env, &args("anthropic", "default", Some("none"))).await;
    assert_fixture("connect_not_found.jsonl", &out);
}

#[tokio::test]
async fn reauth_with_a_stale_fingerprint_is_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let fake = claude_fake(TokenMode::Ok, SEAT_A, "a@example.com").await;
    let env = env_for(dir.path(), &fake, free_port());
    let before = seed(
        &env.store,
        json!({
            "anthropic@claude3": slot(Some(SEAT_A), Some("claude3")),
            "anthropic@bare": slot(None, Some("bare")),
        }),
    );
    let fp_b = fp(OAuthProviderId::Anthropic, SEAT_B);
    let fp_a = fp(OAuthProviderId::Anthropic, SEAT_A);
    for (label, expect) in [
        ("claude3", fp_b.as_str()),
        ("claude3", "none"),
        ("claude3", "not-a-fingerprint"),
        ("bare", fp_a.as_str()),
    ] {
        let (code, out) = pre_start(&env, &args("anthropic", label, Some(expect))).await;
        assert_eq!(code, 1, "{label} {expect}");
        assert_fixture("connect_conflict.jsonl", &out);
    }
    assert_eq!(std::fs::read(&env.store).unwrap(), before);
}

#[tokio::test]
async fn a_closed_stdout_exits_1_without_panicking() {
    let dir = tempfile::tempdir().unwrap();
    let fake = claude_fake(TokenMode::Ok, SEAT_A, "a@example.com").await;
    let port = free_port();
    let env = env_for(dir.path(), &fake, port);
    let (_stdin, stdin_rx) = stdin_pipe();
    let code = drive(&env, &args("anthropic", "claude3", None), LONG, BrokenPipe, stdin_rx, never()).await;
    assert_eq!(code, 1);
    let (_stdin, stdin_rx) = stdin_pipe();
    let code = drive(&env, &args("anthropic", "-bad", None), LONG, BrokenPipe, stdin_rx, never()).await;
    assert_eq!(code, 1);
    assert!(!env.store.exists());
    assert_port_free(port).await;
}

// ── Full flows ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn add_saves_and_reports_the_list_row() {
    let dir = tempfile::tempdir().unwrap();
    let fake = claude_fake(TokenMode::Ok, SEAT_A, "a@example.com").await;
    let port = free_port();
    let env = env_for(dir.path(), &fake, port);
    let out = Capture::default();
    let (stdin_tx, stdin_rx) = stdin_pipe();
    let t0 = crate::epoch_millis();
    let (code, ()) = tokio::join!(
        drive_owned(&env, args("anthropic", "claude3", None), LONG, out.clone(), stdin_rx, never()),
        approve(&out, port, "/callback"),
    );
    drop(stdin_tx);
    assert_eq!(code, 0, "{}", out.text());
    assert_eq!(out.events(), ["started", "open_url", "exchanging", "done"]);
    let lines = out.lines();
    let started = &lines[0];
    assert_eq!(started["mode"], "add");
    let expires_at = started["expires_at_ms"].as_u64().unwrap();
    assert!(expires_at >= t0 + 600_000 && expires_at <= crate::epoch_millis() + 600_000);
    let url = lines[1]["url"].as_str().unwrap();
    assert!(url.starts_with(&format!("{CLAUDE_ALLOW}?")));
    assert_eq!(lines[1]["allow"], CLAUDE_ALLOW);
    // Every line is compact JSON within the wire bound.
    for line in out.text().lines() {
        assert!(line.len() <= MAX_LINE_BYTES && !line.contains(": "), "{line}");
    }

    let done = out.done();
    let row: AccountSummary = serde_json::from_value(done["account"].clone()).unwrap();
    assert_eq!(row, listed_row(&env.store, OAuthProviderId::Anthropic, "claude3"));
    assert_eq!(row.seat_fingerprint.as_deref(), Some(fp(OAuthProviderId::Anthropic, SEAT_A).as_str()));
    assert_eq!(row.identity.as_deref(), Some("a@example.com"));
    assert!(!row.selected, "the default policy selects the default slot");
    let slot = &stored(&env.store)["anthropic@claude3"];
    assert_eq!(slot["access"], "at-fake-new");
    assert_eq!(slot["refresh"], "rt-fake-new");
    assert_eq!(slot["accountId"], SEAT_A);
    assert_eq!(slot["label"], "claude3");
    assert_eq!(slot["addedAt"], json!(row.added_at.unwrap()));
    assert_eq!(fake.token_hits(), 1);
    assert_port_free(port).await;
    assert_fixture("connect_ok_add.jsonl", &out);
}

#[tokio::test]
async fn add_without_a_resolvable_seat_still_saves() {
    let dir = tempfile::tempdir().unwrap();
    let fake = claude_fake(TokenMode::Ok, SEAT_A, "a@example.com").await;
    fake.state.profile_status.store(500, Ordering::SeqCst);
    let port = free_port();
    let env = env_for(dir.path(), &fake, port);
    let out = Capture::default();
    let (_stdin, stdin_rx) = stdin_pipe();
    let (code, ()) = tokio::join!(
        drive_owned(&env, args("anthropic", "default", None), LONG, out.clone(), stdin_rx, never()),
        approve(&out, port, "/callback"),
    );
    assert_eq!(code, 0, "{}", out.text());
    let row: AccountSummary = serde_json::from_value(out.done()["account"].clone()).unwrap();
    assert_eq!(row.seat_fingerprint, None);
    assert!(row.selected, "default slot under the default policy");
    let mut listed = listed_row(&env.store, OAuthProviderId::Anthropic, "default");
    listed.selected = true;
    assert_eq!(row, listed);
}

#[tokio::test]
async fn codex_reauth_of_the_same_seat_saves() {
    let dir = tempfile::tempdir().unwrap();
    let fake = codex_fake(TokenMode::Ok, CODEX_SEAT, "c@example.com").await;
    let port = free_port();
    let env = env_for(dir.path(), &fake, port);
    seed(
        &env.store,
        json!({"openai-codex": slot(Some(CODEX_SEAT), None), "anthropic": slot(Some(SEAT_A), None)}),
    );
    let want = fp(OAuthProviderId::OpenAiCodex, CODEX_SEAT);
    let out = Capture::default();
    let (_stdin, stdin_rx) = stdin_pipe();
    let (code, ()) = tokio::join!(
        drive_owned(&env, args("openai-codex", "default", Some(&want)), LONG, out.clone(), stdin_rx, never()),
        approve(&out, port, "/auth/callback"),
    );
    assert_eq!(code, 0, "{}", out.text());
    assert_eq!(out.lines()[0]["mode"], "reauth");
    assert_eq!(out.lines()[1]["allow"], CODEX_ALLOW);
    let row: AccountSummary = serde_json::from_value(out.done()["account"].clone()).unwrap();
    let mut listed = listed_row(&env.store, OAuthProviderId::OpenAiCodex, "default");
    listed.selected = true;
    assert_eq!(row, listed);
    assert_eq!(row.seat_fingerprint.as_deref(), Some(want.as_str()));
    assert!(row.added_at.unwrap() > 1_700_000_000, "a reauth refreshes added_at");
    assert_eq!(stored(&env.store)["anthropic"], slot(Some(SEAT_A), None), "other slots untouched");
    assert_fixture("connect_ok_reauth_codex.jsonl", &out);
}

#[tokio::test]
async fn reauth_into_a_seatless_slot_with_none_saves_the_new_seat() {
    let dir = tempfile::tempdir().unwrap();
    let fake = claude_fake(TokenMode::Ok, SEAT_A, "a@example.com").await;
    let port = free_port();
    let env = env_for(dir.path(), &fake, port);
    seed(&env.store, json!({"anthropic@bare": slot(None, Some("bare"))}));
    let out = Capture::default();
    let (_stdin, stdin_rx) = stdin_pipe();
    let (code, ()) = tokio::join!(
        drive_owned(&env, args("anthropic", "bare", Some("none")), LONG, out.clone(), stdin_rx, never()),
        approve(&out, port, "/callback"),
    );
    assert_eq!(code, 0, "{}", out.text());
    assert_eq!(stored(&env.store)["anthropic@bare"]["accountId"], SEAT_A);
}

#[tokio::test]
async fn reauth_returning_another_seat_is_identity_changed_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let fake = claude_fake(TokenMode::Ok, SEAT_B, "b@example.com").await;
    let port = free_port();
    let env = env_for(dir.path(), &fake, port);
    let before = seed(&env.store, json!({"anthropic@claude3": slot(Some(SEAT_A), Some("claude3"))}));
    let want = fp(OAuthProviderId::Anthropic, SEAT_A);
    let out = Capture::default();
    let (_stdin, stdin_rx) = stdin_pipe();
    let (code, ()) = tokio::join!(
        drive_owned(&env, args("anthropic", "claude3", Some(&want)), LONG, out.clone(), stdin_rx, never()),
        approve(&out, port, "/callback"),
    );
    assert_eq!(code, 1);
    let done = out.done();
    assert_eq!(done["reason"], "identity_changed");
    assert_eq!(done["identity"], "b@example.com");
    assert_eq!(std::fs::read(&env.store).unwrap(), before, "store byte-unchanged");
    assert_fixture("connect_identity_changed.jsonl", &out);
}

#[tokio::test]
async fn reauth_whose_slot_changed_meanwhile_is_conflict_at_save() {
    let dir = tempfile::tempdir().unwrap();
    let fake = claude_fake(TokenMode::Ok, SEAT_A, "a@example.com").await;
    let port = free_port();
    let env = env_for(dir.path(), &fake, port);
    seed(&env.store, json!({"anthropic@claude3": slot(Some(SEAT_A), Some("claude3"))}));
    let want = fp(OAuthProviderId::Anthropic, SEAT_A);
    let out = Capture::default();
    let (_stdin, stdin_rx) = stdin_pipe();
    let changed = std::cell::RefCell::new(Vec::new());
    let (code, ()) = tokio::join!(
        drive_owned(&env, args("anthropic", "claude3", Some(&want)), LONG, out.clone(), stdin_rx, never()),
        async {
            out.wait_for("open_url").await;
            // Another process signs the slot into a third seat.
            *changed.borrow_mut() = seed(
                &env.store,
                json!({"anthropic@claude3": slot(Some(SEAT_C), Some("claude3"))}),
            );
            approve(&out, port, "/callback").await;
        },
    );
    assert_eq!(code, 1);
    assert_eq!(out.done()["reason"], "conflict");
    assert_eq!(std::fs::read(&env.store).unwrap(), *changed.borrow());
}

#[tokio::test]
async fn add_of_a_seat_another_slot_holds_is_duplicate_seat() {
    let dir = tempfile::tempdir().unwrap();
    let fake = claude_fake(TokenMode::Ok, SEAT_A, "a@example.com").await;
    let port = free_port();
    let env = env_for(dir.path(), &fake, port);
    let before = seed(&env.store, json!({"anthropic@work": slot(Some(SEAT_A), Some("work"))}));
    let out = Capture::default();
    let (_stdin, stdin_rx) = stdin_pipe();
    let (code, ()) = tokio::join!(
        drive_owned(&env, args("anthropic", "claude3", None), LONG, out.clone(), stdin_rx, never()),
        approve(&out, port, "/callback"),
    );
    assert_eq!(code, 1);
    let done = out.done();
    assert_eq!(done["reason"], "duplicate_seat");
    assert_eq!(done["existing"], "work");
    assert_eq!(std::fs::read(&env.store).unwrap(), before);
    assert_fixture("connect_duplicate_seat.jsonl", &out);
}

#[tokio::test]
async fn add_racing_another_add_is_slot_exists_at_save() {
    let dir = tempfile::tempdir().unwrap();
    let fake = claude_fake(TokenMode::Ok, SEAT_A, "a@example.com").await;
    let port = free_port();
    let env = env_for(dir.path(), &fake, port);
    let out = Capture::default();
    let (_stdin, stdin_rx) = stdin_pipe();
    let raced = std::cell::RefCell::new(Vec::new());
    let (code, ()) = tokio::join!(
        drive_owned(&env, args("anthropic", "claude3", None), LONG, out.clone(), stdin_rx, never()),
        async {
            out.wait_for("open_url").await;
            *raced.borrow_mut() =
                seed(&env.store, json!({"anthropic@claude3": slot(Some(SEAT_B), Some("claude3"))}));
            approve(&out, port, "/callback").await;
        },
    );
    assert_eq!(code, 1);
    assert_eq!(out.done()["reason"], "slot_exists");
    assert_eq!(std::fs::read(&env.store).unwrap(), *raced.borrow());
}

// ── Cancel, EOF, signals, timeout ────────────────────────────────────────────

#[tokio::test]
async fn stdin_eof_before_exchanging_cancels_promptly_and_frees_the_port() {
    let dir = tempfile::tempdir().unwrap();
    let fake = claude_fake(TokenMode::Ok, SEAT_A, "a@example.com").await;
    let port = free_port();
    let env = env_for(dir.path(), &fake, port);
    let out = Capture::default();
    let (stdin_tx, stdin_rx) = stdin_pipe();
    let mut stdin_tx = Some(stdin_tx);
    let eof_at = std::cell::Cell::new(None);
    let (code, ()) = tokio::join!(
        drive_owned(&env, args("anthropic", "claude3", None), LONG, out.clone(), stdin_rx, never()),
        async {
            out.wait_for("open_url").await;
            eof_at.set(Some(std::time::Instant::now()));
            drop(stdin_tx.take());
        },
    );
    let took = eof_at.get().unwrap().elapsed();
    assert!(took < PROMPT, "EOF to exit took {took:?}");
    assert_eq!(code, 1);
    assert_eq!(out.done()["reason"], "cancelled");
    assert_eq!(out.events(), ["started", "open_url", "done"]);
    assert!(!env.store.exists(), "nothing saved");
    assert_port_free(port).await;
    assert_eq!(fake.token_hits(), 0);
    assert_fixture("connect_cancelled.jsonl", &out);
}

#[tokio::test]
async fn stdin_bytes_are_discarded_never_parsed_as_a_paste() {
    let dir = tempfile::tempdir().unwrap();
    let fake = claude_fake(TokenMode::Ok, SEAT_A, "a@example.com").await;
    let port = free_port();
    let env = env_for(dir.path(), &fake, port);
    let out = Capture::default();
    let (stdin_tx, stdin_rx) = stdin_pipe();
    let mut stdin_tx = Some(stdin_tx);
    let (code, ()) = tokio::join!(
        drive_owned(&env, args("anthropic", "claude3", None), LONG, out.clone(), stdin_rx, never()),
        async {
            let open = out.wait_for("open_url").await;
            let state = query_param(open["url"].as_str().unwrap(), "state");
            let paste = format!("http://localhost:53692/callback?code=c0de&state={state}\nc0de#{state}\n");
            let tx = stdin_tx.as_mut().unwrap();
            for _ in 0..50 {
                tx.write_all(paste.as_bytes()).await.unwrap();
            }
            tx.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert_eq!(out.events(), ["started", "open_url"], "a paste must not log in");
            drop(stdin_tx.take());
        },
    );
    assert_eq!(code, 1);
    assert_eq!(out.done()["reason"], "cancelled");
    assert_eq!(fake.token_hits(), 0);
    assert!(!env.store.exists());
}

#[tokio::test]
async fn stdin_eof_after_exchanging_is_ignored_and_the_save_completes() {
    let dir = tempfile::tempdir().unwrap();
    let fake = claude_fake(TokenMode::Delay(400), SEAT_A, "a@example.com").await;
    let port = free_port();
    let env = env_for(dir.path(), &fake, port);
    let out = Capture::default();
    let (stdin_tx, stdin_rx) = stdin_pipe();
    let mut stdin_tx = Some(stdin_tx);
    let (code, ()) = tokio::join!(
        drive_owned(&env, args("anthropic", "claude3", None), LONG, out.clone(), stdin_rx, never()),
        async {
            let ((), _) = tokio::join!(approve(&out, port, "/callback"), async {
                out.wait_for("exchanging").await;
                drop(stdin_tx.take());
            });
        },
    );
    assert_eq!(code, 0, "{}", out.text());
    assert_eq!(out.events(), ["started", "open_url", "exchanging", "done"]);
    assert_eq!(stored(&env.store)["anthropic@claude3"]["accountId"], SEAT_A);
    assert_port_free(port).await;
}

#[tokio::test]
async fn a_signal_before_exchanging_cancels_and_frees_the_port() {
    let dir = tempfile::tempdir().unwrap();
    let fake = claude_fake(TokenMode::Ok, SEAT_A, "a@example.com").await;
    let port = free_port();
    let env = env_for(dir.path(), &fake, port);
    let out = Capture::default();
    let (_stdin, stdin_rx) = stdin_pipe();
    let (signal, interrupt) = interrupt_pair();
    let mut signal = Some(signal);
    let (code, ()) = tokio::join!(
        drive_owned(&env, args("anthropic", "claude3", None), LONG, out.clone(), stdin_rx, interrupt),
        async {
            out.wait_for("open_url").await;
            signal.take().unwrap().send(()).unwrap();
        },
    );
    assert_eq!(code, 1);
    assert_eq!(out.done()["reason"], "cancelled");
    assert!(!env.store.exists());
    assert_port_free(port).await;
}

#[tokio::test]
async fn a_signal_during_the_exchange_ends_it_without_saving() {
    let dir = tempfile::tempdir().unwrap();
    let fake = claude_fake(TokenMode::Stall, SEAT_A, "a@example.com").await;
    let port = free_port();
    let env = env_for(dir.path(), &fake, port);
    let out = Capture::default();
    let (_stdin, stdin_rx) = stdin_pipe();
    let (signal, interrupt) = interrupt_pair();
    let mut signal = Some(signal);
    let sent_at = std::cell::Cell::new(None);
    let (code, ()) = tokio::join!(
        drive_owned(&env, args("anthropic", "claude3", None), LONG, out.clone(), stdin_rx, interrupt),
        async {
            let ((), ()) = tokio::join!(approve(&out, port, "/callback"), async {
                out.wait_for("exchanging").await;
                sent_at.set(Some(std::time::Instant::now()));
                signal.take().unwrap().send(()).unwrap();
            });
        },
    );
    assert!(sent_at.get().unwrap().elapsed() < PROMPT);
    assert_eq!(code, 1);
    assert_eq!(out.done()["reason"], "cancelled");
    assert!(!env.store.exists());
}

#[tokio::test]
async fn the_timeout_ends_a_flow_waiting_for_the_browser() {
    let dir = tempfile::tempdir().unwrap();
    let fake = claude_fake(TokenMode::Ok, SEAT_A, "a@example.com").await;
    let port = free_port();
    let env = env_for(dir.path(), &fake, port);
    let out = Capture::default();
    let (_stdin, stdin_rx) = stdin_pipe();
    let t0 = std::time::Instant::now();
    let code = drive(
        &env,
        &args("anthropic", "claude3", None),
        Duration::from_millis(300),
        out.clone(),
        stdin_rx,
        never(),
    )
    .await;
    assert!(t0.elapsed() < Duration::from_millis(300) + PROMPT);
    assert_eq!(code, 1);
    assert_eq!(out.done()["reason"], "timeout");
    assert!(!env.store.exists());
    assert_port_free(port).await;
    assert_fixture("connect_timeout.jsonl", &out);
}

#[tokio::test]
async fn the_timeout_ends_a_stalled_exchange_with_nothing_saved() {
    let dir = tempfile::tempdir().unwrap();
    let fake = claude_fake(TokenMode::Stall, SEAT_A, "a@example.com").await;
    let port = free_port();
    let env = env_for(dir.path(), &fake, port);
    let out = Capture::default();
    let (_stdin, stdin_rx) = stdin_pipe();
    let t0 = std::time::Instant::now();
    let timeout = Duration::from_millis(1200);
    let (code, ()) = tokio::join!(
        drive_owned(&env, args("anthropic", "claude3", None), timeout, out.clone(), stdin_rx, never()),
        approve(&out, port, "/callback"),
    );
    assert!(t0.elapsed() < timeout + PROMPT, "took {:?}", t0.elapsed());
    assert_eq!(code, 1);
    assert_eq!(out.events(), ["started", "open_url", "exchanging", "done"]);
    assert_eq!(out.done()["reason"], "timeout");
    assert_eq!(fake.token_hits(), 1);
    assert!(!env.store.exists());
}

#[tokio::test]
async fn a_store_lock_held_past_the_timeout_is_timeout_with_nothing_written() {
    use fs4::fs_std::FileExt;
    let dir = tempfile::tempdir().unwrap();
    let fake = claude_fake(TokenMode::Ok, SEAT_A, "a@example.com").await;
    let port = free_port();
    let env = env_for(dir.path(), &fake, port);
    let before = seed(&env.store, json!({"anthropic@work": slot(Some(SEAT_B), Some("work"))}));
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(env.store.with_extension("json.lock"))
        .unwrap();
    assert!(FileExt::try_lock_exclusive(&lock).unwrap());
    let out = Capture::default();
    let (_stdin, stdin_rx) = stdin_pipe();
    let t0 = std::time::Instant::now();
    let timeout = Duration::from_millis(1500);
    let (code, ()) = tokio::join!(
        drive_owned(&env, args("anthropic", "claude3", None), timeout, out.clone(), stdin_rx, never()),
        approve(&out, port, "/callback"),
    );
    assert!(t0.elapsed() < timeout + PROMPT, "took {:?}", t0.elapsed());
    assert_eq!(code, 1);
    assert_eq!(out.done()["reason"], "timeout");
    assert_eq!(std::fs::read(&env.store).unwrap(), before);
    drop(lock);
}

// ── Provider-side failures ───────────────────────────────────────────────────

#[tokio::test]
async fn a_busy_callback_port_is_port_busy() {
    let dir = tempfile::tempdir().unwrap();
    let fake = claude_fake(TokenMode::Ok, SEAT_A, "a@example.com").await;
    let taken = StdListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port();
    let env = env_for(dir.path(), &fake, port);
    let out = Capture::default();
    let (_stdin, stdin_rx) = stdin_pipe();
    let code = drive(&env, &args("anthropic", "claude3", None), LONG, out.clone(), stdin_rx, never()).await;
    assert_eq!(code, 1);
    assert_eq!(out.events(), ["started", "done"]);
    assert_eq!(out.done()["reason"], "port_busy");
    assert_fixture("connect_port_busy.jsonl", &out);
}

async fn failing_flow(fake: Fake, query: impl Fn(&str) -> String) -> (Capture, Fake, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let env = env_for(dir.path(), &fake, port);
    let out = Capture::default();
    let (_stdin, stdin_rx) = stdin_pipe();
    let (code, ()) = tokio::join!(
        drive_owned(&env, args("anthropic", "claude3", None), LONG, out.clone(), stdin_rx, never()),
        async {
            let open = out.wait_for("open_url").await;
            let state = query_param(open["url"].as_str().unwrap(), "state");
            redirect(port, "/callback", &query(&urlencoding::encode(&state))).await;
        },
    );
    assert_eq!(code, 1, "{}", out.text());
    assert!(!env.store.exists());
    assert_port_free(port).await;
    (out, fake, dir)
}

#[tokio::test]
async fn the_user_refusing_is_denied() {
    let fake = claude_fake(TokenMode::Ok, SEAT_A, "a@example.com").await;
    let (out, fake, _dir) = failing_flow(fake, |state| {
        format!("error=access_denied&error_description=nope&state={state}")
    })
    .await;
    assert_eq!(out.done()["reason"], "denied");
    assert_eq!(fake.token_hits(), 0);
    assert_fixture("connect_denied.jsonl", &out);
}

#[tokio::test]
async fn a_callback_with_another_state_is_state_mismatch() {
    let fake = claude_fake(TokenMode::Ok, SEAT_A, "a@example.com").await;
    let (out, fake, _dir) = failing_flow(fake, |_| "code=c0de&state=forged".to_string()).await;
    assert_eq!(out.done()["reason"], "state_mismatch");
    assert_eq!(fake.token_hits(), 0);
    assert_fixture("connect_state_mismatch.jsonl", &out);
    // A forged denial is a state mismatch too, never `denied`.
    let fake = claude_fake(TokenMode::Ok, SEAT_A, "a@example.com").await;
    let (out, _, _dir) = failing_flow(fake, |_| "error=access_denied&state=forged".to_string()).await;
    assert_eq!(out.done()["reason"], "state_mismatch");
}

#[tokio::test]
async fn a_refused_code_exchange_is_provider_error() {
    let fake = claude_fake(TokenMode::Reject, SEAT_A, "a@example.com").await;
    let (out, _, _dir) = failing_flow(fake, |state| format!("code=c0de&state={state}")).await;
    assert_eq!(out.events(), ["started", "open_url", "exchanging", "done"]);
    assert_eq!(out.done()["reason"], "provider_error");
}

#[tokio::test]
async fn an_unreachable_token_endpoint_is_network() {
    let mut fake = claude_fake(TokenMode::Ok, SEAT_A, "a@example.com").await;
    fake.base = format!("http://127.0.0.1:{}", free_port());
    let (out, _, _dir) = failing_flow(fake, |state| format!("code=c0de&state={state}")).await;
    assert_eq!(out.done()["reason"], "network");
}

#[tokio::test]
async fn reauth_whose_seat_cannot_be_read_saves_nothing() {
    for (status, reason) in [(500u16, "provider_error"), (0, "network")] {
        let dir = tempfile::tempdir().unwrap();
        let fake = claude_fake(TokenMode::Ok, SEAT_A, "a@example.com").await;
        let port = free_port();
        let mut env = env_for(dir.path(), &fake, port);
        if status == 0 {
            env.profile_url = Some(format!("http://127.0.0.1:{}/profile", free_port()));
        } else {
            fake.state.profile_status.store(status, Ordering::SeqCst);
        }
        let before = seed(&env.store, json!({"anthropic@claude3": slot(Some(SEAT_A), Some("claude3"))}));
        let want = fp(OAuthProviderId::Anthropic, SEAT_A);
        let out = Capture::default();
        let (_stdin, stdin_rx) = stdin_pipe();
        let (code, ()) = tokio::join!(
            drive_owned(&env, args("anthropic", "claude3", Some(&want)), LONG, out.clone(), stdin_rx, never()),
            approve(&out, port, "/callback"),
        );
        assert_eq!(code, 1);
        assert_eq!(out.done()["reason"], reason);
        assert_eq!(std::fs::read(&env.store).unwrap(), before);
    }
}
