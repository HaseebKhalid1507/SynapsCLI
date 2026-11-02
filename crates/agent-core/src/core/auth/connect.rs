//! `synaps auth connect --json` (pria.synaps.accounts/1 §5): a browser login
//! for an app that drives Synaps over a pipe.
//!
//! stdout carries JSON lines only — `started`, `open_url`, `exchanging` and
//! exactly one `done` — each compact, LF-terminated and flushed. Nothing here
//! opens a browser, reads a pasted redirect or prints the authorize URL (or
//! the state, verifier or code) anywhere but the one `open_url` line; no
//! error text ever reaches the JSON.
//!
//! Lifecycle: stdin is the parent-death signal. Its bytes are discarded;
//! EOF (or SIGTERM/SIGINT, via `interrupt`) before `exchanging` cancels the
//! flow and frees the callback port. After `exchanging` EOF is ignored and
//! the bounded exchange, seat lookup and save run to their end. The timeout
//! always ends the flow unless the locked save has begun; a begun save
//! completes (it is one atomic read-modify-write) and its result is reported.

use std::future::Future;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::time::Instant;

use super::account::{Account, AccountPolicy, AccountSelector, AccountSummary, CredentialRef};
use super::identity::{identity_http_client, resolve_seat, SeatResolution};
use super::login_flow::{BrowserEndpoints, LoginFailure, LoginFailureKind, LoginSink};
use super::provider::OAuthProviderId;
use super::storage::{
    save_connect_credential_at, stored_seat_fingerprint, AccountMetadata, ConnectSave,
    SlotExpectation, StoreError, STORE_LOCK_WAIT,
};
use super::OAuthCredentials;

/// `--timeout` when absent, and its clamp (seconds).
pub const DEFAULT_TIMEOUT_SECS: u64 = 600;
pub const MIN_TIMEOUT_SECS: u64 = 30;
pub const MAX_TIMEOUT_SECS: u64 = 1800;

/// The closed list of authorize endpoints an `open_url` line may carry.
pub const ALLOWED_AUTHORIZE_URLS: [&str; 2] = [
    "https://claude.ai/oauth/authorize",
    "https://auth.openai.com/oauth/authorize",
];

/// Wire bounds (§0).
const MAX_URL_BYTES: usize = 4096;
const MAX_IDENTITY_BYTES: usize = 254;
pub const MAX_LINE_BYTES: usize = 16 * 1024;

/// `--expect-seat none`: sign in again into a slot that has no seat.
pub const EXPECT_NO_SEAT: &str = "none";

/// The command line of one connect.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConnectArgs {
    pub provider: String,
    pub account: String,
    /// `--expect-seat`: `None` adds a new slot; `Some(fp)` / `Some("none")`
    /// signs in again into an existing one.
    pub expect_seat: Option<String>,
    pub timeout_secs: Option<u64>,
}

/// `--timeout` as used: default 600 s, clamped to 30..=1800 s.
pub fn clamp_timeout(secs: Option<u64>) -> Duration {
    Duration::from_secs(
        secs.unwrap_or(DEFAULT_TIMEOUT_SECS)
            .clamp(MIN_TIMEOUT_SECS, MAX_TIMEOUT_SECS),
    )
}

/// The closed `done.reason` set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectReason {
    Cancelled,
    Timeout,
    Denied,
    StateMismatch,
    PortBusy,
    SlotExists,
    NotFound,
    Conflict,
    DuplicateSeat,
    IdentityChanged,
    BadLabel,
    RefusedProfile,
    UnsupportedProvider,
    ProviderError,
    Network,
    StoreBusy,
    Internal,
}

impl ConnectReason {
    pub const ALL: [ConnectReason; 17] = [
        Self::Cancelled,
        Self::Timeout,
        Self::Denied,
        Self::StateMismatch,
        Self::PortBusy,
        Self::SlotExists,
        Self::NotFound,
        Self::Conflict,
        Self::DuplicateSeat,
        Self::IdentityChanged,
        Self::BadLabel,
        Self::RefusedProfile,
        Self::UnsupportedProvider,
        Self::ProviderError,
        Self::Network,
        Self::StoreBusy,
        Self::Internal,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::Timeout => "timeout",
            Self::Denied => "denied",
            Self::StateMismatch => "state_mismatch",
            Self::PortBusy => "port_busy",
            Self::SlotExists => "slot_exists",
            Self::NotFound => "not_found",
            Self::Conflict => "conflict",
            Self::DuplicateSeat => "duplicate_seat",
            Self::IdentityChanged => "identity_changed",
            Self::BadLabel => "bad_label",
            Self::RefusedProfile => "refused_profile",
            Self::UnsupportedProvider => "unsupported_provider",
            Self::ProviderError => "provider_error",
            Self::Network => "network",
            Self::StoreBusy => "store_busy",
            Self::Internal => "internal",
        }
    }

    fn from_login(kind: LoginFailureKind) -> Self {
        match kind {
            LoginFailureKind::Denied => Self::Denied,
            LoginFailureKind::StateMismatch => Self::StateMismatch,
            LoginFailureKind::PortBusy => Self::PortBusy,
            LoginFailureKind::ProviderError => Self::ProviderError,
            LoginFailureKind::Network => Self::Network,
            LoginFailureKind::Internal => Self::Internal,
        }
    }
}

impl Serialize for ConnectReason {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

// ── Wire lines (field order is the wire order) ──────────────────────────────

#[derive(Serialize)]
struct StartedLine<'a> {
    v: u8,
    event: &'static str,
    provider: &'a str,
    account: &'a str,
    mode: &'static str,
    flow: &'static str,
    expires_at_ms: u64,
}

#[derive(Serialize)]
struct OpenUrlLine<'a> {
    v: u8,
    event: &'static str,
    url: &'a str,
    allow: &'a str,
}

#[derive(Serialize)]
struct ExchangingLine {
    v: u8,
    event: &'static str,
}

#[derive(Serialize)]
struct DoneOkLine<'a> {
    v: u8,
    event: &'static str,
    ok: bool,
    account: &'a AccountSummary,
}

#[derive(Serialize)]
struct DoneErrLine<'a> {
    v: u8,
    event: &'static str,
    ok: bool,
    reason: ConnectReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    existing: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    identity: Option<&'a str>,
}

/// How a connect ended: the `done` line it prints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Finish {
    Saved(AccountSummary),
    Failed {
        reason: ConnectReason,
        /// The slot already holding the seat (`duplicate_seat` only).
        existing: Option<String>,
        /// The seat the provider returned (`identity_changed` only).
        identity: Option<String>,
    },
}

impl Finish {
    fn fail(reason: ConnectReason) -> Self {
        Self::Failed {
            reason,
            existing: None,
            identity: None,
        }
    }

    fn exit_code(&self) -> i32 {
        match self {
            Self::Saved(_) => 0,
            Self::Failed { .. } => 1,
        }
    }
}

/// stdout: whole lines, flushed one by one. The first failed write marks
/// the pipe broken and every later line is dropped (no panic, no retry).
struct LineOut<W> {
    w: W,
    broken: bool,
}

impl<W: Write> LineOut<W> {
    fn emit<T: Serialize>(&mut self, line: &T) -> bool {
        if self.broken {
            return false;
        }
        let Ok(mut bytes) = serde_json::to_vec(line) else {
            return false;
        };
        bytes.push(b'\n');
        debug_assert!(bytes.len() <= MAX_LINE_BYTES, "connect line over 16 KiB");
        if self.w.write_all(&bytes).and_then(|()| self.w.flush()).is_err() {
            self.broken = true;
            return false;
        }
        true
    }
}

type SharedOut<W> = Arc<Mutex<LineOut<W>>>;

fn emit<W: Write, T: Serialize>(out: &SharedOut<W>, line: &T) -> bool {
    match out.lock() {
        Ok(mut guard) => guard.emit(line),
        Err(_) => false,
    }
}

/// Writes the one `done` line; the process exit code. A broken stdout
/// exits 1 whatever the outcome.
fn write_done<W: Write>(out: &SharedOut<W>, finish: &Finish) -> i32 {
    let written = match finish {
        Finish::Saved(row) => emit(
            out,
            &DoneOkLine {
                v: 1,
                event: "done",
                ok: true,
                account: row,
            },
        ),
        Finish::Failed {
            reason,
            existing,
            identity,
        } => emit(
            out,
            &DoneErrLine {
                v: 1,
                event: "done",
                ok: false,
                reason: *reason,
                existing: existing.as_deref(),
                identity: identity.as_deref(),
            },
        ),
    };
    if written {
        finish.exit_code()
    } else {
        1
    }
}

// ── Flow phases (read by the cancel/timeout arms of the driver) ─────────────

/// Waiting for the browser: EOF, a signal and the timeout all cancel.
const WAITING: u8 = 0;
/// `exchanging` was printed: EOF is ignored from here on.
const EXCHANGING: u8 = 1;
/// The locked save was started: it completes and its result is reported.
const SAVING: u8 = 2;

/// The sink of a JSON connect: no browser, no paste, the URL on the one
/// `open_url` line only.
struct JsonSink<W> {
    out: SharedOut<W>,
    phase: Arc<AtomicU8>,
}

impl<W: Write + Send> LoginSink for JsonSink<W> {
    fn authorize(&mut self, url: &str, allow: &str) -> Result<(), LoginFailure> {
        let well_formed = ALLOWED_AUTHORIZE_URLS.contains(&allow)
            && url.len() <= MAX_URL_BYTES
            && url
                .strip_prefix(allow)
                .is_some_and(|rest| rest.starts_with('?'));
        if !well_formed {
            return Err(LoginFailure::internal("authorize URL outside the contract"));
        }
        let line = OpenUrlLine {
            v: 1,
            event: "open_url",
            url,
            allow,
        };
        if emit(&self.out, &line) {
            Ok(())
        } else {
            Err(LoginFailure::internal("stdout closed"))
        }
    }

    fn exchanging(&mut self) {
        self.phase.store(EXCHANGING, Ordering::SeqCst);
        // A closed stdout no longer stops the flow: the save still runs and
        // the process exits 1 without a `done`.
        let _ = emit(
            &self.out,
            &ExchangingLine {
                v: 1,
                event: "exchanging",
            },
        );
    }

    fn paste_fallback(&self) -> bool {
        false
    }
}

// ── Environment and preconditions ────────────────────────────────────────────

/// Everything a connect reads besides its arguments; tests point the
/// endpoints at loopback fakes and the store at a temp dir.
pub(crate) struct ConnectEnv {
    /// The auth.json this connect writes (the profile-aware write path).
    pub(crate) store: PathBuf,
    /// The active profile has no auth.json of its own but inherits one: a
    /// login there would hide every inherited account (refused, as login).
    pub(crate) inherited_store: bool,
    pub(crate) policy: AccountPolicy,
    pub(crate) anthropic: BrowserEndpoints,
    pub(crate) openai_codex: BrowserEndpoints,
    /// Loopback-only override of the Anthropic profile endpoint (tests).
    pub(crate) profile_url: Option<String>,
}

impl ConnectEnv {
    fn production() -> Self {
        let read = super::storage::auth_file_path();
        let store = crate::config::resolve_write_path("auth.json");
        Self {
            inherited_store: read != store && read.exists(),
            store,
            policy: crate::config::load_config()
                .auth
                .account_policy()
                .with_env_overlay(),
            anthropic: BrowserEndpoints::anthropic(),
            openai_codex: BrowserEndpoints::openai_codex(),
            profile_url: None,
        }
    }

    fn endpoints(&self, provider: OAuthProviderId) -> &BrowserEndpoints {
        match provider {
            OAuthProviderId::OpenAiCodex => &self.openai_codex,
            _ => &self.anthropic,
        }
    }
}

/// A connect that passed every precondition.
struct Plan {
    cred: CredentialRef,
    expect: SlotExpectation,
}

impl Plan {
    fn mode(&self) -> &'static str {
        match self.expect {
            SlotExpectation::Absent => "add",
            SlotExpectation::Seat(_) => "reauth",
        }
    }
}

/// The checks made before `started`, in order: provider, label, profile,
/// then the slot (read once without the lock; the save re-checks under it).
fn preflight(env: &ConnectEnv, args: &ConnectArgs) -> Result<Plan, ConnectReason> {
    let provider = match args.provider.as_str() {
        "anthropic" => OAuthProviderId::Anthropic,
        "openai-codex" => OAuthProviderId::OpenAiCodex,
        _ => return Err(ConnectReason::UnsupportedProvider),
    };
    if args.account.trim() != args.account {
        return Err(ConnectReason::BadLabel);
    }
    let account = Account::parse(&args.account).map_err(|_| ConnectReason::BadLabel)?;
    let cred = CredentialRef::new(provider, account);
    let expect = match args.expect_seat.as_deref() {
        None => SlotExpectation::Absent,
        Some(EXPECT_NO_SEAT) => SlotExpectation::Seat(None),
        Some(fp) => SlotExpectation::Seat(Some(fp.to_string())),
    };
    if env.inherited_store {
        return Err(ConnectReason::RefusedProfile);
    }
    let root = read_store(&env.store)?;
    let entry = root.get(&cred.storage_key());
    match &expect {
        SlotExpectation::Absent => {
            if entry.is_some() {
                return Err(ConnectReason::SlotExists);
            }
        }
        SlotExpectation::Seat(want) => {
            match entry.and_then(|e| stored_seat_fingerprint(provider, e)) {
                None => return Err(ConnectReason::NotFound),
                Some(fp) if fp != *want => return Err(ConnectReason::Conflict),
                Some(_) => {}
            }
        }
    }
    Ok(Plan { cred, expect })
}

fn read_store(
    path: &std::path::Path,
) -> Result<serde_json::Map<String, serde_json::Value>, ConnectReason> {
    match std::fs::read_to_string(path) {
        Ok(content) => serde_json::from_str(&content).map_err(|_| ConnectReason::Internal),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(serde_json::Map::new()),
        Err(_) => Err(ConnectReason::Internal),
    }
}

// ── The flow ─────────────────────────────────────────────────────────────────

/// Browser login, seat lookup and the locked save. Dropping this future
/// before [`SAVING`] abandons the login with nothing written (the callback
/// listener closes with it); from [`SAVING`] on the save runs to its end on
/// a blocking thread even if the future is dropped.
async fn connect_flow<W: Write + Send>(
    env: &ConnectEnv,
    plan: &Plan,
    sink: &mut JsonSink<W>,
    deadline: Instant,
) -> Finish {
    let provider = plan.cred.provider;
    let login = match provider {
        OAuthProviderId::OpenAiCodex => {
            super::openai_codex::browser_login(sink, env.endpoints(provider)).await
        }
        _ => super::browser_login(sink, env.endpoints(provider)).await,
    };
    let mut creds = match login {
        Ok(creds) => creds,
        Err(failure) => return Finish::fail(ConnectReason::from_login(failure.kind)),
    };

    // The seat behind the new token (Codex: its JWT; Anthropic: one
    // profile request, bounded by its own timeout).
    let seat = match identity_http_client() {
        Ok(client) => {
            resolve_seat(provider, &creds.access, &client, env.profile_url.as_deref()).await
        }
        Err(_) => return Finish::fail(ConnectReason::Internal),
    };
    let must_match = matches!(plan.expect, SlotExpectation::Seat(Some(_)));
    let mut identity = None;
    match seat {
        SeatResolution::Resolved(seat) => {
            if creds.account_id.is_none() {
                creds.account_id = Some(seat.account_id);
            }
            identity = seat
                .identity
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty() && s.len() <= MAX_IDENTITY_BYTES);
        }
        // Add / no-seat reauth: saved without a seat, as `synaps login`.
        SeatResolution::Unsupported | SeatResolution::Failed(_) if !must_match => {}
        SeatResolution::Unsupported => return Finish::fail(ConnectReason::ProviderError),
        SeatResolution::Failed(msg) => {
            return Finish::fail(if super::anthropic_profile::is_transport_failure(&msg) {
                ConnectReason::Network
            } else {
                ConnectReason::ProviderError
            })
        }
    }

    // The timeout wins until the locked save starts; then the save wins.
    let now = Instant::now();
    if now >= deadline {
        return Finish::fail(ConnectReason::Timeout);
    }
    let remaining = deadline - now;
    let (wait, wait_is_deadline) = if remaining < STORE_LOCK_WAIT {
        (remaining, true)
    } else {
        (STORE_LOCK_WAIT, false)
    };
    let metadata = AccountMetadata {
        identity: identity.clone(),
        added_at: Some(crate::epoch_millis() / 1000),
    };
    sink.phase.store(SAVING, Ordering::SeqCst);
    let saved = {
        let store = env.store.clone();
        let cred = plan.cred.clone();
        let expect = plan.expect.clone();
        let creds = creds.clone();
        let metadata = metadata.clone();
        tokio::task::spawn_blocking(move || {
            save_connect_credential_at(&store, &cred, &creds, &metadata, &expect, wait)
        })
        .await
    };
    match saved {
        Ok(Ok(ConnectSave::Saved)) => {
            Finish::Saved(saved_row(env, &plan.cred, &creds, &metadata))
        }
        Ok(Ok(ConnectSave::SlotExists)) => Finish::fail(ConnectReason::SlotExists),
        Ok(Ok(ConnectSave::Conflict)) => Finish::fail(ConnectReason::Conflict),
        Ok(Ok(ConnectSave::IdentityChanged)) => Finish::Failed {
            reason: ConnectReason::IdentityChanged,
            existing: None,
            identity,
        },
        Ok(Ok(ConnectSave::Duplicate(account))) => Finish::Failed {
            reason: ConnectReason::DuplicateSeat,
            existing: Some(account.label_str().to_string()),
            identity: None,
        },
        Ok(Err(StoreError::Busy(_))) if wait_is_deadline => Finish::fail(ConnectReason::Timeout),
        Ok(Err(StoreError::Busy(_))) => Finish::fail(ConnectReason::StoreBusy),
        Ok(Err(StoreError::Corrupt(_) | StoreError::Failed(_))) | Err(_) => {
            Finish::fail(ConnectReason::Internal)
        }
    }
}

/// The `auth list --json` row of the slot just saved, built from exactly
/// what was written.
fn saved_row(
    env: &ConnectEnv,
    cred: &CredentialRef,
    creds: &OAuthCredentials,
    metadata: &AccountMetadata,
) -> AccountSummary {
    let stored_id = creds
        .account_id
        .as_deref()
        .filter(|id| !id.trim().is_empty());
    let selected = matches!(
        env.policy.selector(cred.provider),
        AccountSelector::Account(a) if a.label_str() == cred.account.label_str()
    );
    AccountSummary {
        identity: metadata
            .identity
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        expires: creds.expires,
        added_at: metadata.added_at,
        selected,
        ..AccountSummary::new(cred.provider, &cred.account).with_account_id(stored_id)
    }
}

/// Reads and discards stdin until EOF (or a read error: the parent is gone
/// either way). Never parsed.
async fn drain_until_eof<R: AsyncRead + Unpin>(mut stdin: R) {
    let mut buf = [0u8; 4096];
    loop {
        match stdin.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
    }
}

/// One connect against `env`, `timeout` already clamped. `interrupt`
/// resolves on SIGTERM/SIGINT. Returns the exit code.
pub(crate) async fn drive<W, R, I>(
    env: &ConnectEnv,
    args: &ConnectArgs,
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
    let out: SharedOut<W> = Arc::new(Mutex::new(LineOut { w: out, broken: false }));
    let plan = match preflight(env, args) {
        Ok(plan) => plan,
        Err(reason) => return write_done(&out, &Finish::fail(reason)),
    };
    let deadline = Instant::now() + timeout;
    let started = StartedLine {
        v: 1,
        event: "started",
        provider: plan.cred.provider.as_str(),
        account: plan.cred.account.label_str(),
        mode: plan.mode(),
        flow: "browser",
        expires_at_ms: crate::epoch_millis()
            .saturating_add(u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX)),
    };
    if !emit(&out, &started) {
        return 1;
    }

    let phase = Arc::new(AtomicU8::new(WAITING));
    let mut sink = JsonSink {
        out: out.clone(),
        phase: phase.clone(),
    };
    let finish = {
        let flow = connect_flow(env, &plan, &mut sink, deadline);
        tokio::pin!(flow);
        tokio::pin!(interrupt);
        let eof = drain_until_eof(stdin);
        tokio::pin!(eof);
        let expiry = tokio::time::sleep_until(deadline);
        tokio::pin!(expiry);
        let (mut interrupted, mut eof_seen, mut expired) = (false, false, false);
        loop {
            tokio::select! {
                biased;
                finish = &mut flow => break finish,
                () = &mut interrupt, if !interrupted => {
                    interrupted = true;
                    if phase.load(Ordering::SeqCst) < SAVING {
                        break Finish::fail(ConnectReason::Cancelled);
                    }
                }
                () = &mut eof, if !eof_seen => {
                    eof_seen = true;
                    if phase.load(Ordering::SeqCst) < EXCHANGING {
                        break Finish::fail(ConnectReason::Cancelled);
                    }
                }
                () = &mut expiry, if !expired => {
                    expired = true;
                    if phase.load(Ordering::SeqCst) < SAVING {
                        break Finish::fail(ConnectReason::Timeout);
                    }
                }
            }
        }
        // Leaving this block drops an unfinished flow: the callback
        // listener closes and nothing is saved.
    };
    write_done(&out, &finish)
}

/// `synaps auth connect --json`: one connect against the real store and
/// providers. `out` is stdout, `stdin` the parent's pipe, `interrupt`
/// resolves on SIGTERM/SIGINT. Returns the exit code; the caller exits
/// with it right away (which also releases the callback port).
pub async fn run<W, R, I>(args: &ConnectArgs, out: W, stdin: R, interrupt: I) -> i32
where
    W: Write + Send,
    R: AsyncRead + Unpin,
    I: Future<Output = ()>,
{
    let env = ConnectEnv::production();
    drive(&env, args, clamp_timeout(args.timeout_secs), out, stdin, interrupt).await
}

#[cfg(test)]
#[path = "connect_tests.rs"]
mod tests;
