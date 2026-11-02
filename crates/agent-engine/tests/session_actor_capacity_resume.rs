//! A3 phase 2 (`capacity_resume`): a turn that ends on a reset-based
//! capacity stop is resumed ONCE at that reset by the session itself, and
//! never after the owner or another sender moved on.
//!
//! In-process actors, a loopback Anthropic SSE stub, isolated HOME, no
//! daemon. Automatic selection is a scripted broker installed for every
//! session of this binary (`turn_broker_testing`); the scratch config sets
//! `capacity_wait = off`, so a cooling selection ends the turn at once with
//! its stop noted. The crash record's writer is a stand-in ([`FakeWriter`]):
//! it acknowledges, fails or holds the writes of the sessions it watches,
//! at the point where the daemon's writer stamps them, and the host's record
//! link is the real one. Timings are shortened (`capacity_resume::testing`).
//!
//! A restore's side (a reload's or a crash restore's rehydrate) is stood in
//! for: a fresh actor from the stopped session's journal is handed the stop
//! (`ArmCapacityResume`), as the daemon's rehydrate does; a crash is the
//! actor's task aborted, with the record left as the writer last landed it.

mod session_actor_common;
use session_actor_common::*;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_engine::auth::{
    AccessToken, Account, AccountFailure, AccountFailureKind, AccountSelector, BrokerError,
    CredentialBroker, CredentialRef, OAuthProviderId, PinnedToken, ProxyByteStream, ProxyRequest,
    ProxyResponse, SelectionFailure, SelectionFailureKind,
};
use agent_engine::runtime::turn_broker_testing::TurnBrokerGuard;
use agent_engine::session::capacity_resume::{
    armed_notice, canceled_notice, resume_text,
    testing::{PauseAfterAck, Timings},
};
use agent_engine::session::live::{self, LiveSlot, RecordLink};
use agent_engine::session::session_start_testing as start_seam;
use agent_engine::session::wire::CHECKPOINT_QUERY_ID;
use agent_engine::session::{
    CapacityStop, CheckpointReason, ClientKind, ClientMeta, ClientTransport, ContinueLookup,
    EndReason, Envelope, LocalTransport, SessionCommand, SessionConfig, SessionEventWire,
    SessionHandle, SessionLifecycle, SessionQuery, TurnTrigger,
};
use agent_engine::events::types::Event;
use agent_engine::{EngineHost, SessionEvent, StreamEvent};
use chrono::{DateTime, Utc};
use serde_json::Value;
use serial_test::serial;
use tokio::sync::watch;

// ── the scripted broker ─────────────────────────────────────────────────

/// What automatic selection answers.
#[derive(Clone, Copy, Debug)]
enum Script {
    /// Every account cools down until this reset (epoch ms).
    Cool(u64),
    /// Every account cools down until this long after the selection (ms):
    /// each stop resets strictly later than the one before.
    CoolAhead(u64),
    /// An account is back: a token is vended.
    Vend,
    /// The first selection (the turn start's, inline in the actor) vends;
    /// from then on the vended seat and every account cool down until this
    /// reset, so the turn's round top (in its spawned task) selects again
    /// and stops.
    VendThenCool(u64),
}

struct ScriptedBroker {
    script: Mutex<Script>,
    selections: AtomicUsize,
    /// Closed: a selection that is about to fail waits (after it is
    /// counted) until it opens. A vending selection never waits.
    open: watch::Sender<bool>,
}

impl ScriptedBroker {
    fn new(script: Script) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script),
            selections: AtomicUsize::new(0),
            open: watch::channel(true).0,
        })
    }

    /// Hold every failing selection until [`Self::release`].
    fn hold(&self) {
        self.open.send_replace(false);
    }

    fn release(&self) {
        self.open.send_replace(true);
    }

    /// Wait until `n` selections have been asked for (at most 10 s).
    async fn selected(&self, n: usize) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while self.selections.load(Ordering::SeqCst) < n {
            assert!(tokio::time::Instant::now() < deadline, "never selected {n} times");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn set(&self, script: Script) {
        *self.script.lock().unwrap() = script;
    }

    /// The reset everything cools down until, once `selected` selections
    /// were asked for; `None`: an account is available.
    fn reset_after(&self, selected: usize) -> Option<u64> {
        match *self.script.lock().unwrap() {
            Script::Cool(ms) => Some(ms),
            Script::CoolAhead(ahead) => Some(agent_engine::epoch_millis() + ahead),
            Script::Vend => None,
            Script::VendThenCool(ms) => (selected > 1).then_some(ms),
        }
    }
}

#[async_trait::async_trait]
impl CredentialBroker for ScriptedBroker {
    async fn access_token(&self, _p: OAuthProviderId) -> Result<AccessToken, BrokerError> {
        Err(BrokerError::Denied("legacy path must not be used".into()))
    }
    fn account_selector(&self, _p: OAuthProviderId) -> AccountSelector {
        AccountSelector::Auto
    }
    fn active_cooldown(&self, _cred: &CredentialRef) -> Option<u64> {
        // Asked about a seat already vended: the next selection's view.
        self.reset_after(self.selections.load(Ordering::SeqCst) + 1)
    }
    async fn access_token_for(&self, _cred: &CredentialRef) -> Result<AccessToken, BrokerError> {
        Err(BrokerError::Denied("no explicit vend in this test".into()))
    }
    async fn access_token_pinned_for(
        &self,
        provider: OAuthProviderId,
        _model: Option<&str>,
    ) -> Result<PinnedToken, BrokerError> {
        let selected = self.selections.fetch_add(1, Ordering::SeqCst) + 1;
        let Some(reset) = self.reset_after(selected) else {
            return Ok(PinnedToken {
                credential: CredentialRef::new(provider, Account::parse("claude1").unwrap()),
                token: AccessToken { token: "tok-scripted".into(), expires: u64::MAX },
            });
        };
        let mut open = self.open.subscribe();
        drop(open.wait_for(|open| *open).await);
        Err(BrokerError::NoAccountAvailable {
            provider: "anthropic".into(),
            reason: "scripted".into(),
            failure: Some(SelectionFailure {
                kind: SelectionFailureKind::AllCoolingDown,
                accounts: vec![AccountFailure {
                    account: "claude4".into(),
                    kind: AccountFailureKind::Exhausted,
                    http_status: None,
                    retry_after_s: None,
                    until_ms: Some(reset),
                    cooling_down: true,
                }],
                earliest_reset_ms: Some(reset),
                usage_read_retry_ms: None,
            }),
        })
    }
    async fn proxy(&self, _r: ProxyRequest) -> Result<ProxyResponse, BrokerError> {
        Err(BrokerError::Denied("not implemented in stub".into()))
    }
    async fn proxy_stream(&self, _r: ProxyRequest) -> Result<ProxyByteStream, BrokerError> {
        Err(BrokerError::Denied("not implemented in stub".into()))
    }
    async fn anthropic_usage(&self) -> Result<Value, BrokerError> {
        Err(BrokerError::Denied("not implemented in stub".into()))
    }
    async fn capabilities(&self) -> Result<Vec<agent_engine::auth::ProviderStatus>, BrokerError> {
        Ok(vec![])
    }
}

// ── the crash record writer's stand-in ──────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// Every write succeeds and acknowledges the entries it held.
    Ack,
    /// Every write fails: nothing is acknowledged.
    Fail,
    /// The write under way stays in flight until the mode changes; it then
    /// lands (acknowledges what it held) if the new mode is `Ack`.
    Hold,
}

/// On every change (`live::mark_dirty`) it reads the change count, then the
/// watched slots' entries, as the daemon writer's snapshot does, and
/// acknowledges them per its mode, as the daemon writer does after a write.
/// It keeps, per watched slot, the capacity stop of the last entry a write
/// landed for it: what the record on disk holds. A crashed slot's record
/// stays as it last landed (a write in flight at the crash never lands).
struct FakeWriter {
    mode: watch::Sender<Mode>,
    /// The watched slots, in watch order; `false`: its daemon crashed.
    slots: Mutex<Vec<(LiveSlot, bool)>>,
    /// Per watched slot: `None` until a write landed for it.
    landed: Mutex<Vec<Option<Option<CapacityStop>>>>,
}

impl FakeWriter {
    fn install(host: &EngineHost, mode: Mode) -> (Arc<Self>, Arc<RecordLink>) {
        let link = Arc::new(RecordLink::new());
        host.set_record_link(Some(Arc::clone(&link)));
        let me = Arc::new(Self {
            mode: watch::channel(mode).0,
            slots: Mutex::new(Vec::new()),
            landed: Mutex::new(Vec::new()),
        });
        let w = Arc::clone(&me);
        tokio::spawn(async move {
            let mut changes = live::subscribe();
            let mut mode = w.mode.subscribe();
            loop {
                if changes.changed().await.is_err() {
                    return;
                }
                let started = live::changes_now();
                let snapshot: Vec<(usize, LiveSlot, u64, Option<CapacityStop>)> = w
                    .slots
                    .lock()
                    .unwrap()
                    .iter()
                    .enumerate()
                    .filter(|(_, (_, alive))| *alive)
                    .filter_map(|(i, (s, _))| {
                        s.load_full().map(|e| (i, s.clone(), e.gen, e.record.capacity_stop.clone()))
                    })
                    .collect();
                let mut m = *mode.borrow_and_update();
                while m == Mode::Hold {
                    if mode.changed().await.is_err() {
                        return;
                    }
                    m = *mode.borrow_and_update();
                }
                if m == Mode::Ack {
                    for (i, slot, gen, stop) in snapshot {
                        if w.slots.lock().unwrap()[i].1 {
                            w.landed.lock().unwrap()[i] = Some(stop);
                        }
                        slot.stamp_durable(started, gen);
                    }
                }
            }
        });
        (me, link)
    }

    /// Watch `h`'s slot; its index.
    fn watch(&self, h: &SessionHandle) -> usize {
        let mut slots = self.slots.lock().unwrap();
        slots.push((h.live_slot().clone(), true));
        self.landed.lock().unwrap().push(None);
        slots.len() - 1
    }

    /// The daemon serving slot `i` died: its record stays as it last landed.
    fn crash(&self, i: usize) {
        self.slots.lock().unwrap()[i].1 = false;
    }

    /// The capacity stop the record holds for slot `i` (`None`: no write
    /// landed for it yet).
    fn landed(&self, i: usize) -> Option<Option<CapacityStop>> {
        self.landed.lock().unwrap()[i].clone()
    }

    fn set(&self, mode: Mode) {
        self.mode.send_replace(mode);
    }
}

// ── the fixture ─────────────────────────────────────────────────────────

/// The request bodies the provider stub received.
type Bodies = Arc<Mutex<Vec<Value>>>;

struct Fixture {
    host: Arc<EngineHost>,
    broker: Arc<ScriptedBroker>,
    hits: Arc<AtomicUsize>,
    bodies: Bodies,
    writer: Option<Arc<FakeWriter>>,
    link: Option<Arc<RecordLink>>,
    reset_ms: u64,
    _installed: TurnBrokerGuard,
    _timings: Timings,
    _home: Home,
}

struct Opts {
    /// The stop's reset, this long from now.
    reset_in: Duration,
    /// `None`: no crash record (no record link on the host).
    writer: Option<Mode>,
    /// Extra lines for the scratch config (after `capacity_wait = off`).
    config: &'static str,
    max_jitter: Duration,
    ack_wait: Duration,
    look_again: Duration,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            // Room for the stop, its turn's end (a save) and the test's own
            // steps before the reset on a loaded host.
            reset_in: Duration::from_secs(3),
            writer: Some(Mode::Ack),
            config: "",
            max_jitter: Duration::ZERO,
            ack_wait: Duration::from_secs(2),
            look_again: Duration::from_millis(300),
        }
    }
}

impl Fixture {
    async fn new(o: Opts) -> Self {
        let home = Home::new();
        std::fs::write(
            home.dir.path().join(".synaps-cli").join("config"),
            format!("capacity_wait = off\n{}", o.config),
        )
        .unwrap();
        let timings = Timings::set(o.max_jitter, o.ack_wait, o.look_again);
        let reset_ms = agent_engine::epoch_millis() + o.reset_in.as_millis() as u64;
        let broker = ScriptedBroker::new(Script::Cool(reset_ms));
        let installed = agent_engine::runtime::turn_broker_testing::install_global(broker.clone());
        let (url, hits, bodies) = capture_stub().await;
        std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
        let host = host().await;
        let (writer, link) = match o.writer {
            Some(mode) => {
                let (w, l) = FakeWriter::install(&host, mode);
                (Some(w), Some(l))
            }
            None => (None, None),
        };
        Self {
            host,
            broker,
            hits,
            bodies,
            writer,
            link,
            reset_ms,
            _installed: installed,
            _timings: timings,
            _home: home,
        }
    }

    fn resume_at(&self) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp_millis(self.reset_ms as i64).unwrap()
    }

    fn writer(&self) -> &FakeWriter {
        self.writer.as_deref().expect("a writer")
    }

    fn link(&self) -> &RecordLink {
        self.link.as_deref().expect("a record link")
    }

    /// A persisted session the writer watches.
    async fn session(&self) -> SessionHandle {
        let handle = self.host.create_session(SessionConfig { persist: true, ..cfg() }).await.unwrap();
        if let Some(w) = &self.writer {
            w.watch(&handle);
        }
        handle
    }

    /// [`Self::session`], with its event queue.
    async fn session_with_queue(&self) -> (SessionHandle, Arc<agent_engine::events::EventQueue>) {
        let (handle, task) = self
            .host
            .create_session_unspawned(SessionConfig { persist: true, ..cfg() })
            .await
            .unwrap();
        let queue = task.event_queue();
        tokio::spawn(task.run());
        if let Some(w) = &self.writer {
            w.watch(&handle);
        }
        (handle, queue)
    }

    async fn attach(&self, handle: &SessionHandle) -> LocalTransport {
        LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Test))
            .await
            .unwrap()
            .0
    }

    /// A session whose first turn stopped on the reset: its error, then the
    /// armed notice, are seen.
    async fn stopped(&self) -> (SessionHandle, LocalTransport, Vec<Envelope>) {
        let handle = self.session().await;
        let mut client = self.attach(&handle).await;
        client.send(submit("work that stops")).await.unwrap();
        let mut seen = until(&mut client, |e| matches!(e, SessionEventWire::Idle)).await;
        seen.extend(drain_for(&mut client, Duration::from_millis(200)).await);
        (handle, client, seen)
    }

    /// Sleep until this long past the reset.
    async fn past_reset(&self, by: Duration) {
        let at = self.resume_at() + chrono::Duration::from_std(by).unwrap();
        let left = (at - Utc::now()).to_std().unwrap_or_default();
        tokio::time::sleep(left).await;
    }
}

// ── reading what happened ───────────────────────────────────────────────

fn notices(seen: &[Envelope], text: &str) -> usize {
    seen.iter()
        .filter(|e| matches!(&e.event, SessionEventWire::SystemNotice(t) if t == text))
        .count()
}

fn armed(seen: &[Envelope], f: &Fixture) -> usize {
    notices(seen, &armed_notice(f.resume_at()))
}

fn armed_any(seen: &[Envelope]) -> usize {
    seen.iter()
        .filter(|e| matches!(&e.event, SessionEventWire::SystemNotice(t) if t.starts_with("Stopped: no Anthropic account has capacity until ")))
        .count()
}

fn errors(seen: &[Envelope]) -> usize {
    seen.iter()
        .filter(|e| matches!(&e.event, SessionEventWire::Stream(StreamEvent::Session(SessionEvent::Error(_)))))
        .count()
}

/// The resumes: turns the session started by itself with a `resume:` text.
fn resumes(seen: &[Envelope]) -> Vec<(DateTime<Utc>, String)> {
    seen.iter()
        .filter_map(|e| match &e.event {
            SessionEventWire::TurnStarted {
                trigger: TurnTrigger::QueuedAuto,
                user_text: Some(text),
                ..
            } if text.starts_with("resume: ") => Some((e.ts, text.clone())),
            _ => None,
        })
        .collect()
}

fn turns(seen: &[Envelope]) -> usize {
    seen.iter()
        .filter(|e| matches!(e.event, SessionEventWire::TurnStarted { .. }))
        .count()
}

fn entry_stop(h: &SessionHandle) -> Option<agent_engine::session::CapacityStop> {
    h.live_slot().load_full().and_then(|e| e.record.capacity_stop.clone())
}

/// Poll `h`'s published entry until `pred` holds (at most 10 s).
async fn entry_until(h: &SessionHandle, what: &str, pred: impl Fn(&live::LiveEntry) -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if h.live_slot().load_full().is_some_and(|e| pred(&e)) {
            return;
        }
        assert!(tokio::time::Instant::now() < deadline, "the entry never reached: {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// The entry is cleared and the writer acknowledged it.
fn cleared_and_acked(h: &SessionHandle) -> bool {
    let e = h.live_slot().load_full().unwrap();
    e.record.capacity_stop.is_none() && h.live_slot().durable().gen >= e.gen
}

// ── tests ───────────────────────────────────────────────────────────────

/// A1: the stop arms the resume (its notice; the entry carries it), and at
/// the reset exactly ONE turn starts with the resume text; the entry that
/// turn start publishes is clear, and the turn runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_capacity_stop_resumes_the_session_once_at_its_reset() {
    let f = Fixture::new(Opts::default()).await;
    let (handle, mut client, seen) = f.stopped().await;
    assert_eq!(errors(&seen), 1, "the turn ends with the selection's error: {seen:#?}");
    assert_eq!(armed(&seen, &f), 1, "one armed notice: {seen:#?}");
    let stop = entry_stop(&handle).expect("the entry carries the stop");
    assert_eq!(stop.resume_at, f.resume_at());
    assert_eq!(stop.kind, "all_cooling_down");
    assert_eq!(stop.attempt, 0, "a first stop");
    assert_eq!(stop.history_len, 1, "the user's message");
    assert_eq!(stop.last_digest.len(), 64);
    assert!(stop.stopped_at <= Utc::now() && stop.stopped_at < stop.resume_at);
    assert_eq!(f.hits.load(Ordering::SeqCst), 0, "nothing was sent");

    // The account comes back at the reset.
    f.broker.set(Script::Vend);
    let mut seen = until(&mut client, |e| {
        matches!(e, SessionEventWire::TurnStarted { trigger: TurnTrigger::QueuedAuto, .. })
    })
    .await;
    assert!(entry_stop(&handle).is_none(), "the turn start's entry is clear");
    seen.extend(until(&mut client, |e| matches!(e, SessionEventWire::Idle)).await);
    seen.extend(drain_for(&mut client, Duration::from_secs(2)).await);
    let r = resumes(&seen);
    assert_eq!(r.len(), 1, "{seen:#?}");
    assert!(r[0].0 >= f.resume_at(), "not before the reset");
    assert_eq!(r[0].1, resume_text(stop.stopped_at, stop.resume_at));
    assert_eq!(turns(&seen), 1, "{seen:#?}");
    assert_eq!(errors(&seen), 0, "the resumed turn ran: {seen:#?}");
    assert_eq!(f.hits.load(Ordering::SeqCst), 1);
    assert!(entry_stop(&handle).is_none());
    end(&mut client).await;
}

/// A2: another sender's message before the reset continues the session:
/// its turn clears the resume, and nothing starts at the reset.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_message_before_the_reset_clears_the_resume() {
    let f = Fixture::new(Opts::default()).await;
    let (handle, mut client, seen) = f.stopped().await;
    assert_eq!(armed(&seen, &f), 1, "{seen:#?}");
    f.broker.set(Script::Vend);
    client.send(submit("carry on, I am back")).await.unwrap();
    let mut seen = until(&mut client, |e| matches!(e, SessionEventWire::Idle)).await;
    assert!(entry_stop(&handle).is_none());
    f.past_reset(Duration::from_millis(1500)).await;
    seen.extend(drain_for(&mut client, Duration::from_millis(300)).await);
    assert_eq!(turns(&seen), 1, "only the sender's turn: {seen:#?}");
    assert!(resumes(&seen).is_empty(), "{seen:#?}");
    assert_eq!(f.hits.load(Ordering::SeqCst), 1);
    end(&mut client).await;
}

/// A4 and the owner's clears: Esc while idle cancels the resume (its
/// notice, after the clear is acknowledged), and nothing starts at the
/// reset. `/new` and closing the session clear it the same way.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn the_owner_cancels_the_resume_with_esc_new_or_a_close() {
    let f = Fixture::new(Opts { reset_in: Duration::from_secs(12), ..Opts::default() }).await;

    // Esc.
    let (esc, mut c1, seen) = f.stopped().await;
    assert_eq!(armed(&seen, &f), 1, "{seen:#?}");
    c1.send(SessionCommand::Cancel).await.unwrap();
    let seen = until(&mut c1, |e| matches!(e, SessionEventWire::SystemNotice(t) if *t == canceled_notice())).await;
    assert!(cleared_and_acked(&esc), "cleared and acknowledged before the notice: {seen:#?}");

    // /new.
    let (new, mut c2, seen) = f.stopped().await;
    assert_eq!(armed(&seen, &f), 1, "{seen:#?}");
    c2.send(SessionCommand::NewSession).await.unwrap();
    until(&mut c2, |e| matches!(e, SessionEventWire::Cleared { .. })).await;
    assert!(cleared_and_acked(&new), "cleared and acknowledged before the new session");

    // A close.
    let (closed, mut c3, seen) = f.stopped().await;
    assert_eq!(armed(&seen, &f), 1, "{seen:#?}");
    end(&mut c3).await;
    assert!(cleared_and_acked(&closed), "cleared and acknowledged before the session ended");

    f.broker.set(Script::Vend);
    f.past_reset(Duration::from_millis(1500)).await;
    let mut seen = drain_for(&mut c1, Duration::from_millis(200)).await;
    seen.extend(drain_for(&mut c2, Duration::from_millis(200)).await);
    assert_eq!(turns(&seen), 0, "{seen:#?}");
    assert_eq!(f.hits.load(Ordering::SeqCst), 0);
    end(&mut c1).await;
    end(&mut c2).await;
}

/// The D3 limit's live half: an Esc whose clear the writer never
/// acknowledges still cancels the resume while this actor lives (one WARN;
/// the clear is in memory and rides the writer's next write).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn an_unacknowledged_esc_still_cancels_the_resume_here() {
    let f = Fixture::new(Opts { writer: Some(Mode::Fail), ack_wait: Duration::from_millis(300), ..Opts::default() })
        .await;
    let (handle, mut client, seen) = f.stopped().await;
    assert_eq!(armed(&seen, &f), 1, "{seen:#?}");
    client.send(SessionCommand::Cancel).await.unwrap();
    until(&mut client, |e| matches!(e, SessionEventWire::SystemNotice(t) if *t == canceled_notice())).await;
    let e = handle.live_slot().load_full().unwrap();
    assert!(e.record.capacity_stop.is_none());
    assert!(handle.live_slot().durable().gen < e.gen, "never acknowledged");
    f.broker.set(Script::Vend);
    f.past_reset(Duration::from_millis(1500)).await;
    let seen = drain_for(&mut client, Duration::from_millis(300)).await;
    assert_eq!(turns(&seen), 0, "{seen:#?}");
    end(&mut client).await;
}

/// A3: a session that parked while armed is NOT evicted past its eviction
/// age; at the reset it unparks and resumes once (its history checked
/// against the stop after the reload from disk); once the resume is
/// spent, it parks and is evicted as usual.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_parked_armed_session_is_held_from_eviction_and_resumes_once() {
    let _env = Env::set(&[("SYNAPS_DAEMON_PARK_GRACE_SECS", "0"), ("SYNAPS_DAEMON_PARKED_EVICT_SECS", "1")]);
    let f = Fixture::new(Opts { reset_in: Duration::from_secs(8), ..Opts::default() }).await;
    let (handle, mut client, seen) = f.stopped().await;
    assert_eq!(armed(&seen, &f), 1, "{seen:#?}");
    f.broker.set(Script::Vend);
    let mut watch = handle.subscribe();
    detach(&mut client).await;
    assert!(wait_lifecycle(&handle, SessionLifecycle::Parked, Duration::from_secs(5)).await);
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert!(Utc::now() < f.resume_at(), "the check runs before the reset");
    assert!(handle.is_alive(), "held past the eviction age");
    assert_eq!(handle.lifecycle(), SessionLifecycle::Parked);
    assert!(entry_stop(&handle).is_some());

    // The reset: it unparks and resumes once.
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while resumes(&seen).is_empty() || !matches!(seen.last().map(|e: &Envelope| &e.event), Some(SessionEventWire::Idle)) {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, watch.recv()).await {
            Ok(Ok(env)) => seen.push(env),
            other => panic!("no resume after the reset ({other:?}): {seen:#?}"),
        }
    }
    assert_eq!(resumes(&seen).len(), 1, "{seen:#?}");
    assert_eq!(f.hits.load(Ordering::SeqCst), 1);

    // Spent: it parks again and is evicted.
    tokio::time::timeout(Duration::from_secs(10), handle.closed())
        .await
        .expect("evicted once the resume is spent");
    assert_eq!(resumes(&seen).len(), 1);
}

/// Esc reaching a parked armed session clears it without waking it, and
/// its eviction runs again (from when it parked).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn esc_to_a_parked_armed_session_clears_it_and_lets_it_be_evicted() {
    let _env = Env::set(&[("SYNAPS_DAEMON_PARK_GRACE_SECS", "0"), ("SYNAPS_DAEMON_PARKED_EVICT_SECS", "1")]);
    let f = Fixture::new(Opts { reset_in: Duration::from_secs(20), ..Opts::default() }).await;
    let (handle, mut client, seen) = f.stopped().await;
    assert_eq!(armed(&seen, &f), 1, "{seen:#?}");
    detach(&mut client).await;
    assert!(wait_lifecycle(&handle, SessionLifecycle::Parked, Duration::from_secs(5)).await);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(handle.is_alive(), "held while armed");
    handle.send(SessionCommand::Cancel).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), handle.closed())
        .await
        .expect("evicted once the resume is canceled");
    assert!(cleared_and_acked(&handle));
    assert_eq!(f.hits.load(Ordering::SeqCst), 0);
}

/// A5: `capacity_resume = off` arms nothing; turned off after the arm, the
/// resume never fires (dropped at its time).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn the_kill_switch_stops_arming_and_firing() {
    {
        let f = Fixture::new(Opts { config: "capacity_resume = off\n", ..Opts::default() }).await;
        let (handle, mut client, seen) = f.stopped().await;
        assert_eq!(errors(&seen), 1, "{seen:#?}");
        assert_eq!(armed_any(&seen), 0, "{seen:#?}");
        assert!(entry_stop(&handle).is_none());
        end(&mut client).await;
    }

    let f = Fixture::new(Opts::default()).await;
    let (handle, mut client, seen) = f.stopped().await;
    assert_eq!(armed(&seen, &f), 1, "{seen:#?}");
    let mut config = (**f.host.config()).clone();
    config.capacity_resume = false;
    f.host.set_config_for_test(config);
    f.broker.set(Script::Vend);
    f.past_reset(Duration::from_millis(1500)).await;
    let seen = drain_for(&mut client, Duration::from_millis(300)).await;
    assert_eq!(turns(&seen), 0, "{seen:#?}");
    assert!(entry_stop(&handle).is_none(), "dropped at its time");
    assert_eq!(f.hits.load(Ordering::SeqCst), 0);
    end(&mut client).await;
}

/// No crash record (no record link): nothing is armed, nothing fires.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn without_a_crash_record_nothing_is_armed() {
    let f = Fixture::new(Opts { writer: None, ..Opts::default() }).await;
    let (handle, mut client, seen) = f.stopped().await;
    assert_eq!(errors(&seen), 1, "{seen:#?}");
    assert_eq!(armed_any(&seen), 0, "{seen:#?}");
    assert!(entry_stop(&handle).is_none());
    f.broker.set(Script::Vend);
    f.past_reset(Duration::from_millis(1000)).await;
    let seen = drain_for(&mut client, Duration::from_millis(300)).await;
    assert_eq!(turns(&seen), 0, "{seen:#?}");
    end(&mut client).await;
}

/// A6: a resumed turn that stops again on a later reset arms again, with
/// the chain's attempt: three resumes at most, and the stop after the
/// third is not armed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn the_resume_chain_stops_after_three() {
    let f = Fixture::new(Opts::default()).await;
    f.broker.set(Script::CoolAhead(3000));
    let (handle, mut client, mut seen) = f.stopped().await;
    assert_eq!(armed_any(&seen), 1, "{seen:#?}");
    assert_eq!(entry_stop(&handle).map(|s| s.attempt), Some(0));
    let mut attempts = Vec::new();
    for _ in 0..3 {
        let more = until(&mut client, |e| {
            matches!(e, SessionEventWire::TurnStarted { trigger: TurnTrigger::QueuedAuto, .. })
        })
        .await;
        seen.extend(more);
        seen.extend(until(&mut client, |e| matches!(e, SessionEventWire::Idle)).await);
        seen.extend(drain_for(&mut client, Duration::from_millis(200)).await);
        attempts.push(entry_stop(&handle).map(|s| s.attempt));
    }
    seen.extend(drain_for(&mut client, Duration::from_secs(3)).await);
    assert_eq!(attempts, vec![Some(1), Some(2), None], "{seen:#?}");
    assert_eq!(resumes(&seen).len(), 3, "{seen:#?}");
    assert_eq!(armed_any(&seen), 3, "{seen:#?}");
    assert_eq!(errors(&seen), 4, "{seen:#?}");
    assert!(entry_stop(&handle).is_none());
    assert_eq!(f.hits.load(Ordering::SeqCst), 0);
    end(&mut client).await;
}

/// A8: a reload checkpoint while armed: its record carries the resume, the
/// fenced actor never fires it, and once the reload is abandoned the
/// overdue resume fires once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_reload_checkpoint_carries_the_resume_and_the_fence_holds_it() {
    let f = Fixture::new(Opts::default()).await;
    let (handle, mut client, seen) = f.stopped().await;
    assert_eq!(armed(&seen, &f), 1, "{seen:#?}");
    let stop = entry_stop(&handle).unwrap();
    f.broker.set(Script::Vend);
    handle
        .send(SessionCommand::Checkpoint { reason: CheckpointReason::Reload, deadline: None, skip_quiesce_grace: false })
        .await
        .unwrap();
    let seen = until(&mut client, |e| {
        matches!(e, SessionEventWire::QueryResult { id, .. } if *id == CHECKPOINT_QUERY_ID)
    })
    .await;
    let SessionEventWire::QueryResult { value, .. } = &seen.last().unwrap().event else { unreachable!() };
    let carried: agent_engine::session::CapacityStop =
        serde_json::from_value(value["record"]["capacity_stop"].clone()).expect("the record carries it");
    assert_eq!(carried, stop);
    f.past_reset(Duration::from_millis(1500)).await;
    let seen = drain_for(&mut client, Duration::from_millis(300)).await;
    assert_eq!(turns(&seen), 0, "fenced: {seen:#?}");

    handle.send(SessionCommand::ReloadAborted).await.unwrap();
    let mut seen = until(&mut client, |e| matches!(e, SessionEventWire::TurnStarted { .. })).await;
    seen.extend(until(&mut client, |e| matches!(e, SessionEventWire::Idle)).await);
    seen.extend(drain_for(&mut client, Duration::from_secs(1)).await);
    assert_eq!(resumes(&seen).len(), 1, "{seen:#?}");
    assert_eq!(turns(&seen), 1, "{seen:#?}");
    end(&mut client).await;
}

/// The fenced-Esc limit (beside D3): an Esc that lands under a reload's
/// fence is not refused; it clears the resume here (its notice), but the
/// reload record, taken before it, still carries the marker, so the next
/// image may fire it once. If the reload is abandoned instead, the clear
/// holds: nothing fires here.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn an_esc_under_a_reload_fence_clears_here_but_the_record_keeps_the_resume() {
    let f = Fixture::new(Opts::default()).await;
    let (handle, mut client, seen) = f.stopped().await;
    assert_eq!(armed(&seen, &f), 1, "{seen:#?}");
    let stop = entry_stop(&handle).unwrap();
    handle
        .send(SessionCommand::Checkpoint { reason: CheckpointReason::Reload, deadline: None, skip_quiesce_grace: false })
        .await
        .unwrap();
    let seen = until(&mut client, |e| {
        matches!(e, SessionEventWire::QueryResult { id, .. } if *id == CHECKPOINT_QUERY_ID)
    })
    .await;
    let SessionEventWire::QueryResult { value, .. } = &seen.last().unwrap().event else { unreachable!() };
    let record = value["record"].clone();

    client.send(SessionCommand::Cancel).await.unwrap();
    let seen = until(&mut client, |e| matches!(e, SessionEventWire::SystemNotice(t) if *t == canceled_notice()))
        .await;
    assert!(
        !seen.iter().any(|e| matches!(e.event, SessionEventWire::Refused { .. })),
        "not refused under the fence: {seen:#?}"
    );
    assert!(entry_stop(&handle).is_none(), "cleared here");
    let carried: agent_engine::session::CapacityStop =
        serde_json::from_value(record["capacity_stop"].clone()).expect("the reload record keeps it");
    assert_eq!(carried, stop);

    f.broker.set(Script::Vend);
    f.past_reset(Duration::from_millis(1500)).await;
    handle.send(SessionCommand::ReloadAborted).await.unwrap();
    let seen = drain_for(&mut client, Duration::from_secs(2)).await;
    assert_eq!(turns(&seen), 0, "the clear holds once the reload is abandoned: {seen:#?}");
    assert_eq!(f.hits.load(Ordering::SeqCst), 0);
    end(&mut client).await;
}

/// A stop's seal closes admission; a reload
/// sealing after it and reopening its own seal leaves it closed. The due
/// resume is not consumed while sealed: no turn, and it stays owed. Once
/// nothing seals, it fires once. (A seal that comes while the consumption
/// waits for its acknowledgement: the next test.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_sealed_record_admits_no_resume() {
    let f = Fixture::new(Opts::default()).await;
    let (handle, mut client, seen) = f.stopped().await;
    assert_eq!(armed(&seen, &f), 1, "{seen:#?}");
    f.broker.set(Script::Vend);
    let stop = f.link().seal();
    let reload = f.link().seal();
    f.link().unseal(reload);
    f.past_reset(Duration::from_millis(1500)).await;
    let seen = drain_for(&mut client, Duration::from_millis(300)).await;
    assert_eq!(turns(&seen), 0, "{seen:#?}");
    assert!(entry_stop(&handle).is_some(), "still owed");

    f.link().unseal(stop);
    let mut seen = until(&mut client, |e| matches!(e, SessionEventWire::TurnStarted { .. })).await;
    seen.extend(until(&mut client, |e| matches!(e, SessionEventWire::Idle)).await);
    seen.extend(drain_for(&mut client, Duration::from_secs(1)).await);
    assert_eq!(resumes(&seen).len(), 1, "{seen:#?}");
    end(&mut client).await;
}

/// A force stop seals while the consumption waits for its acknowledgement:
/// the write lands, the acknowledgement comes, and admission still refuses
/// the dispatch (what the stop's snapshot read was the consumed entry).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_stop_during_the_acknowledgement_admits_no_resume() {
    let f = Fixture::new(Opts::default()).await;
    let (handle, mut client, seen) = f.stopped().await;
    assert_eq!(armed(&seen, &f), 1, "{seen:#?}");
    f.broker.set(Script::Vend);
    f.writer().set(Mode::Hold);
    entry_until(&handle, "consumed", |e| e.record.capacity_stop.is_none()).await;
    let _stop = f.link().seal();
    assert!(entry_stop(&handle).is_none(), "the stop's snapshot reads the consumed entry");
    f.writer().set(Mode::Ack);
    f.past_reset(Duration::from_millis(2000)).await;
    let seen = drain_for(&mut client, Duration::from_millis(300)).await;
    assert_eq!(turns(&seen), 0, "{seen:#?}");
    assert_eq!(f.hits.load(Ordering::SeqCst), 0);
    end(&mut client).await;
}

/// Commands that arrive while the consumption waits for its
/// acknowledgement win: the sender's message runs, its turn clears the
/// resume, and no resume ever starts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_message_during_the_acknowledgement_wins() {
    let f = Fixture::new(Opts::default()).await;
    let (handle, mut client, seen) = f.stopped().await;
    assert_eq!(armed(&seen, &f), 1, "{seen:#?}");
    f.broker.set(Script::Vend);
    f.writer().set(Mode::Hold);
    entry_until(&handle, "consumed", |e| e.record.capacity_stop.is_none()).await;
    client.send(submit("me first")).await.unwrap();
    f.writer().set(Mode::Ack);
    let mut seen = until(&mut client, |e| matches!(e, SessionEventWire::Idle)).await;
    f.past_reset(Duration::from_millis(1500)).await;
    seen.extend(drain_for(&mut client, Duration::from_millis(500)).await);
    assert!(resumes(&seen).is_empty(), "{seen:#?}");
    assert_eq!(turns(&seen), 1, "the sender's turn only: {seen:#?}");
    assert!(entry_stop(&handle).is_none());
    assert_eq!(f.hits.load(Ordering::SeqCst), 1);
    end(&mut client).await;
}

/// Work the stopped turn left queued goes first: a message the owner sent
/// while the failing turn was held is the next user turn at the reset; the
/// first request after the stop carries it, and the resume text is never
/// sent, then or after.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_message_queued_behind_the_stopped_turn_goes_first_and_no_resume_follows() {
    const OWNER: &str = "stop that, do the other task";
    let f = Fixture::new(Opts { reset_in: Duration::from_secs(5), ..Opts::default() }).await;
    let handle = f.session().await;
    let mut client = f.attach(&handle).await;
    f.broker.set(Script::VendThenCool(f.reset_ms));
    f.broker.hold();
    client.send(submit("work that stops")).await.unwrap();
    // Held at its round top, in the turn's task.
    f.broker.selected(2).await;
    client.send(submit(OWNER)).await.unwrap();
    until(&mut client, |e| matches!(e, SessionEventWire::Steered { .. })).await;
    f.broker.release();
    let mut seen = until(&mut client, |e| matches!(e, SessionEventWire::Idle)).await;
    seen.extend(drain_for(&mut client, Duration::from_millis(200)).await);
    assert_eq!(armed(&seen, &f), 1, "{seen:#?}");
    assert_eq!(f.hits.load(Ordering::SeqCst), 0);

    f.broker.set(Script::Vend);
    f.past_reset(Duration::ZERO).await;
    let mut seen = until(&mut client, |e| matches!(e, SessionEventWire::TurnStarted { .. })).await;
    seen.extend(until(&mut client, |e| matches!(e, SessionEventWire::Idle)).await);
    seen.extend(drain_for(&mut client, Duration::from_secs(2)).await);
    assert!(resumes(&seen).is_empty(), "{seen:#?}");
    assert_eq!(turns(&seen), 1, "{seen:#?}");
    assert!(
        seen.iter().any(|e| matches!(
            &e.event,
            SessionEventWire::TurnStarted { user_text: Some(t), .. } if t == OWNER
        )),
        "{seen:#?}"
    );
    assert!(entry_stop(&handle).is_none());
    let bodies = f.bodies.lock().unwrap().clone();
    assert_eq!(bodies.len(), 1, "{bodies:#?}");
    let texts = user_texts(&bodies[0]);
    assert_eq!(texts.last().map(String::as_str), Some(OWNER), "{texts:#?}");
    assert!(!texts.iter().any(|t| t.starts_with("resume: ")), "{texts:#?}");
    end(&mut client).await;
}

/// Events buffered while the stopped turn was held go first: at the reset
/// they start an event turn (the first request after the stop carries
/// them), and the resume text is never sent, then or after.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn events_buffered_behind_the_stopped_turn_go_first_and_no_resume_follows() {
    let f = Fixture::new(Opts { reset_in: Duration::from_secs(5), ..Opts::default() }).await;
    let (handle, queue) = f.session_with_queue().await;
    let mut client = f.attach(&handle).await;
    f.broker.set(Script::VendThenCool(f.reset_ms));
    f.broker.hold();
    client.send(submit("work that stops")).await.unwrap();
    f.broker.selected(2).await;
    queue.push(Event::simple("test", "build finished", None)).unwrap();
    until(&mut client, |e| matches!(e, SessionEventWire::External(_))).await;
    f.broker.release();
    let mut seen = until(&mut client, |e| matches!(e, SessionEventWire::Idle)).await;
    seen.extend(drain_for(&mut client, Duration::from_millis(200)).await);
    assert_eq!(armed(&seen, &f), 1, "{seen:#?}");
    assert_eq!(f.hits.load(Ordering::SeqCst), 0);

    f.broker.set(Script::Vend);
    f.past_reset(Duration::ZERO).await;
    let mut seen = until(&mut client, |e| matches!(e, SessionEventWire::TurnStarted { .. })).await;
    seen.extend(until(&mut client, |e| matches!(e, SessionEventWire::Idle)).await);
    seen.extend(drain_for(&mut client, Duration::from_secs(2)).await);
    assert!(resumes(&seen).is_empty(), "{seen:#?}");
    assert_eq!(turns(&seen), 1, "{seen:#?}");
    assert!(
        seen.iter().any(|e| matches!(
            e.event,
            SessionEventWire::TurnStarted { trigger: TurnTrigger::EventAuto, .. }
        )),
        "{seen:#?}"
    );
    assert!(entry_stop(&handle).is_none());
    let bodies = f.bodies.lock().unwrap().clone();
    assert_eq!(bodies.len(), 1, "{bodies:#?}");
    let texts = user_texts(&bodies[0]);
    assert!(texts.last().is_some_and(|t| t.contains("build finished")), "{texts:#?}");
    assert!(!texts.iter().any(|t| t.starts_with("resume: ")), "{texts:#?}");
    end(&mut client).await;
}

/// An event-only continuation takes the usual claim: with the auto-turn
/// cap spent (the stopped turn was itself an event turn), the events go
/// into history with the cap notice, and no turn starts: neither an event
/// turn nor the resume.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn an_event_only_continuation_obeys_the_auto_turn_cap() {
    let f = Fixture::new(Opts {
        reset_in: Duration::from_secs(5),
        config: "events.auto_turn_cap = 1\n",
        ..Opts::default()
    })
    .await;
    let (handle, queue) = f.session_with_queue().await;
    let mut client = f.attach(&handle).await;
    f.broker.set(Script::VendThenCool(f.reset_ms));
    f.broker.hold();
    queue.push(Event::simple("test", "first event", None)).unwrap();
    f.broker.selected(2).await;
    queue.push(Event::simple("test", "second event", None)).unwrap();
    until(&mut client, |e| {
        matches!(e, SessionEventWire::External(ev) if ev.content.text == "second event")
    })
    .await;
    f.broker.release();
    let mut seen = until(&mut client, |e| matches!(e, SessionEventWire::Idle)).await;
    seen.extend(drain_for(&mut client, Duration::from_millis(200)).await);
    assert_eq!(armed(&seen, &f), 1, "{seen:#?}");

    f.broker.set(Script::Vend);
    f.past_reset(Duration::ZERO).await;
    let seen = drain_for(&mut client, Duration::from_secs(3)).await;
    assert_eq!(turns(&seen), 0, "{seen:#?}");
    assert_eq!(
        seen.iter()
            .filter(|e| matches!(e.event, SessionEventWire::AutoTurnCapReached { cap: 1 }))
            .count(),
        1,
        "{seen:#?}"
    );
    assert!(entry_stop(&handle).is_none(), "consumed");
    assert_eq!(f.hits.load(Ordering::SeqCst), 0);
    end(&mut client).await;
}

/// The turn that queued work starts in place of the resume carries no
/// resume chain: when it stops on a later reset in turn, that stop is a
/// first stop (attempt 0), armed afresh.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn the_turn_queued_work_starts_carries_no_resume_chain() {
    const OWNER: &str = "do the other task";
    let f = Fixture::new(Opts { reset_in: Duration::from_secs(5), ..Opts::default() }).await;
    let handle = f.session().await;
    let mut client = f.attach(&handle).await;
    f.broker.set(Script::VendThenCool(f.reset_ms));
    f.broker.hold();
    client.send(submit("work that stops")).await.unwrap();
    f.broker.selected(2).await;
    client.send(submit(OWNER)).await.unwrap();
    until(&mut client, |e| matches!(e, SessionEventWire::Steered { .. })).await;
    f.broker.release();
    let mut seen = until(&mut client, |e| matches!(e, SessionEventWire::Idle)).await;
    seen.extend(drain_for(&mut client, Duration::from_millis(200)).await);
    assert_eq!(armed(&seen, &f), 1, "{seen:#?}");

    f.broker.set(Script::CoolAhead(60_000));
    f.past_reset(Duration::ZERO).await;
    let mut seen = until(&mut client, |e| matches!(e, SessionEventWire::TurnStarted { .. })).await;
    seen.extend(until(&mut client, |e| matches!(e, SessionEventWire::Idle)).await);
    seen.extend(drain_for(&mut client, Duration::from_millis(500)).await);
    assert!(resumes(&seen).is_empty(), "{seen:#?}");
    assert_eq!(turns(&seen), 1, "{seen:#?}");
    assert_eq!(armed_any(&seen), 1, "the owner's turn stopped and armed: {seen:#?}");
    assert_eq!(entry_stop(&handle).map(|s| s.attempt), Some(0), "a first stop");
    assert_eq!(f.hits.load(Ordering::SeqCst), 0);
    end(&mut client).await;
}

/// A13: a consumption the writer fails to acknowledge dispatches nothing
/// and re-arms; the retry, acknowledged, fires exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn an_unacknowledged_consumption_dispatches_nothing_and_retries_once() {
    let f = Fixture::new(Opts {
        writer: Some(Mode::Fail),
        ack_wait: Duration::from_millis(500),
        look_again: Duration::from_secs(3),
        ..Opts::default()
    })
    .await;
    let (handle, mut client, seen) = f.stopped().await;
    assert_eq!(armed(&seen, &f), 1, "{seen:#?}");
    f.broker.set(Script::Vend);
    f.past_reset(Duration::from_millis(1500)).await;
    let mut seen = drain_for(&mut client, Duration::from_millis(100)).await;
    assert_eq!(turns(&seen), 0, "{seen:#?}");
    assert!(entry_stop(&handle).is_some(), "armed again");

    f.writer().set(Mode::Ack);
    seen.extend(until(&mut client, |e| matches!(e, SessionEventWire::TurnStarted { .. })).await);
    seen.extend(until(&mut client, |e| matches!(e, SessionEventWire::Idle)).await);
    seen.extend(drain_for(&mut client, Duration::from_secs(4)).await);
    assert_eq!(resumes(&seen).len(), 1, "{seen:#?}");
    assert_eq!(turns(&seen), 1, "{seen:#?}");
    assert_eq!(f.hits.load(Ordering::SeqCst), 1);
    end(&mut client).await;
}

/// A15: the acknowledgement times out while the write is held: no turn,
/// the resume is armed again under a new generation; the held write then
/// lands (acknowledging only the old one) and the next write records the
/// re-armed entry; the later look fires exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_late_acknowledgement_never_dispatches_and_the_rearm_is_recorded() {
    let f = Fixture::new(Opts {
        ack_wait: Duration::from_millis(500),
        look_again: Duration::from_secs(3),
        ..Opts::default()
    })
    .await;
    let (handle, mut client, seen) = f.stopped().await;
    assert_eq!(armed(&seen, &f), 1, "{seen:#?}");
    f.broker.set(Script::Vend);
    f.writer().set(Mode::Hold);
    entry_until(&handle, "consumed", |e| e.record.capacity_stop.is_none()).await;
    let consumed = handle.live_slot().load_full().unwrap().gen;
    entry_until(&handle, "armed again", |e| e.record.capacity_stop.is_some() && e.gen > consumed).await;
    let rearmed = handle.live_slot().load_full().unwrap().gen;

    f.writer().set(Mode::Ack);
    entry_until(&handle, "the re-armed entry recorded", |e| {
        e.record.capacity_stop.is_some() && e.gen == rearmed
    })
    .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while handle.live_slot().durable().gen < rearmed {
        assert!(tokio::time::Instant::now() < deadline, "the next write never recorded the re-arm");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let mut seen = drain_for(&mut client, Duration::from_millis(100)).await;
    assert_eq!(turns(&seen), 0, "the late acknowledgement dispatched nothing: {seen:#?}");

    seen.extend(until(&mut client, |e| matches!(e, SessionEventWire::TurnStarted { .. })).await);
    seen.extend(until(&mut client, |e| matches!(e, SessionEventWire::Idle)).await);
    seen.extend(drain_for(&mut client, Duration::from_secs(4)).await);
    assert_eq!(resumes(&seen).len(), 1, "{seen:#?}");
    assert_eq!(turns(&seen), 1, "{seen:#?}");
    end(&mut client).await;
}

/// HERD: five sessions stopped on one reset start exactly five turns, one
/// each, spread by the jitter (none before the reset, none past its
/// ceiling).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn sessions_stopped_on_one_reset_resume_once_each_spread_by_the_jitter() {
    let max_jitter = Duration::from_secs(2);
    let f = Fixture::new(Opts { reset_in: Duration::from_secs(8), max_jitter, ..Opts::default() }).await;
    // Stopped together, as on one account switch (and all well before the
    // reset: one after one, five turn ends can outlast it on a loaded host).
    let mut sessions = Vec::new();
    for (handle, client, seen) in futures::future::join_all((0..5).map(|_| f.stopped())).await {
        assert_eq!(armed(&seen, &f), 1, "{seen:#?}");
        sessions.push((handle, client));
    }
    f.broker.set(Script::Vend);
    f.past_reset(max_jitter + Duration::from_secs(3)).await;
    let mut starts = Vec::new();
    for (_, client) in sessions.iter_mut() {
        let seen = drain_for(client, Duration::from_millis(300)).await;
        let r = resumes(&seen);
        assert_eq!(r.len(), 1, "one resume per session: {seen:#?}");
        assert_eq!(turns(&seen), 1, "{seen:#?}");
        starts.push(r[0].0);
    }
    let ceiling = f.resume_at() + chrono::Duration::from_std(max_jitter).unwrap() + chrono::Duration::seconds(1);
    for at in &starts {
        assert!(*at >= f.resume_at() && *at <= ceiling, "{starts:?}");
    }
    let spread = *starts.iter().max().unwrap() - *starts.iter().min().unwrap();
    assert!(spread >= chrono::Duration::milliseconds(100), "spread by the jitter: {starts:?}");
    assert_eq!(f.hits.load(Ordering::SeqCst), 5);
    for (_, client) in sessions.iter_mut() {
        end(client).await;
    }
}

// ── restored resumes: a reload's or a crash restore's rehydrate ────────
//
// The rehydrate creates the session from its journal (the restore's
// config), sends the resume decision and the carried workers, then
// `ArmCapacityResume` with the stop its record carried, then any Park
// (`daemon::reload::rehydrate_one`). These tests stand in for it on the
// actor side; the daemon side is in its own tests.

impl Fixture {
    /// A stopped, armed session whose daemon then went away (a host end
    /// keeps its resume in the record): the stop, and the journal id.
    async fn stopped_then_gone(&self) -> (CapacityStop, String) {
        let (handle, client, seen) = self.stopped().await;
        assert_eq!(armed(&seen, self), 1, "{seen:#?}");
        let stop = entry_stop(&handle).expect("armed");
        let journal = handle.journal_id();
        drop(client);
        handle.send(SessionCommand::End { reason: EndReason::HostShutdown }).await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), handle.closed()).await.expect("ended");
        (stop, journal)
    }

    /// The rehydrate's create of `journal`, watched by the writer.
    async fn restored(&self, journal: &str, parked: bool) -> SessionHandle {
        let handle = self.host.create_session(restore_cfg(journal, parked)).await.unwrap();
        if let Some(w) = &self.writer {
            w.watch(&handle);
        }
        handle
    }

    /// A persisted session whose actor task the test can abort (a crash of
    /// its daemon): the handle, the task, and its writer slot.
    async fn crashable(&self) -> (SessionHandle, tokio::task::JoinHandle<()>, usize) {
        let (handle, task) = self
            .host
            .create_session_unspawned(SessionConfig { persist: true, ..cfg() })
            .await
            .unwrap();
        let actor = tokio::spawn(task.run());
        let slot = self.writer().watch(&handle);
        (handle, actor, slot)
    }
}

/// The config `daemon::reload::restore_config` gives a restored session.
fn restore_cfg(journal: &str, parked: bool) -> SessionConfig {
    SessionConfig {
        persist: true,
        continue_session: Some(Some(journal.to_string())),
        continue_lookup: ContinueLookup::ExactJournalId,
        await_extensions: false,
        restore_parked: parked,
        ..cfg()
    }
}

fn arm(stop: &CapacityStop) -> SessionCommand {
    SessionCommand::ArmCapacityResume { stop: stop.clone() }
}

/// Every command sent to the session before this returns was handled (a
/// query sent after them is answered).
async fn settled(client: &mut LocalTransport, id: u64) -> Vec<Envelope> {
    client.send(SessionCommand::Query { id, query: SessionQuery::Status }).await.unwrap();
    until(client, |e| matches!(e, SessionEventWire::QueryResult { id: i, .. } if *i == id)).await
}

/// Events from a subscription until `pred` holds (at most `within`).
async fn watch_until(
    rx: &mut tokio::sync::broadcast::Receiver<Envelope>,
    within: Duration,
    pred: impl Fn(&SessionEventWire) -> bool,
) -> Vec<Envelope> {
    let deadline = tokio::time::Instant::now() + within;
    let mut seen = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Ok(env)) => {
                let done = pred(&env.event);
                seen.push(env);
                if done {
                    return seen;
                }
            }
            other => panic!("never seen ({other:?}): {seen:#?}"),
        }
    }
}

/// The capacity resumes of `stop`: turns started with its own text.
fn resumes_of(seen: &[Envelope], stop: &CapacityStop) -> usize {
    let text = resume_text(stop.stopped_at, stop.resume_at);
    resumes(seen).iter().filter(|(_, t)| *t == text).count()
}

/// A7: a restore hands the actor the resume its record carried; one whose
/// reset passed while the daemon was down fires right after the restore,
/// ONCE, however often it is handed over (a second arm is not taken).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_restored_resume_fires_once_however_often_it_is_handed_over() {
    let f = Fixture::new(Opts::default()).await;
    let (stop, journal) = f.stopped_then_gone().await;
    f.broker.set(Script::Vend);
    f.past_reset(Duration::from_millis(300)).await;
    let handle = f.restored(&journal, false).await;
    let mut client = f.attach(&handle).await;
    handle.send(arm(&stop)).await.unwrap();
    handle.send(arm(&stop)).await.unwrap();
    let mut seen = until(&mut client, |e| matches!(e, SessionEventWire::TurnStarted { .. })).await;
    seen.extend(until(&mut client, |e| matches!(e, SessionEventWire::Idle)).await);
    seen.extend(drain_for(&mut client, Duration::from_secs(2)).await);
    assert_eq!(resumes_of(&seen, &stop), 1, "{seen:#?}");
    assert_eq!(turns(&seen), 1, "{seen:#?}");
    assert_eq!(errors(&seen), 0, "{seen:#?}");
    assert_eq!(f.hits.load(Ordering::SeqCst), 1);
    assert!(entry_stop(&handle).is_none());
    end(&mut client).await;
}

/// A7, M18: a restored stop is validated before it is armed (a kind
/// outside the closed set is not), and one that validates but pins another
/// history is dropped at its time, after the session is live: neither
/// starts a turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_restored_resume_that_fails_validation_or_pins_another_history_never_fires() {
    let f = Fixture::new(Opts { reset_in: Duration::from_secs(6), ..Opts::default() }).await;
    let (stop_a, journal_a) = f.stopped_then_gone().await;
    let (stop_b, journal_b) = f.stopped_then_gone().await;
    f.broker.set(Script::Vend);

    let invalid = CapacityStop { kind: "seat_churn".into(), ..stop_a };
    let a = f.restored(&journal_a, false).await;
    let mut ca = f.attach(&a).await;
    a.send(arm(&invalid)).await.unwrap();
    let mut seen_a = settled(&mut ca, 901).await;
    assert!(entry_stop(&a).is_none(), "not armed");

    let other_history = CapacityStop { last_digest: "ab".repeat(32), ..stop_b };
    let b = f.restored(&journal_b, false).await;
    let mut cb = f.attach(&b).await;
    b.send(arm(&other_history)).await.unwrap();
    let mut seen_b = settled(&mut cb, 902).await;
    assert_eq!(entry_stop(&b), Some(other_history.clone()), "valid: armed as recorded");

    f.past_reset(Duration::from_millis(1500)).await;
    seen_a.extend(drain_for(&mut ca, Duration::from_millis(300)).await);
    seen_b.extend(drain_for(&mut cb, Duration::from_millis(300)).await);
    assert_eq!(turns(&seen_a), 0, "{seen_a:#?}");
    assert_eq!(turns(&seen_b), 0, "{seen_b:#?}");
    assert!(entry_stop(&b).is_none(), "dropped at its time");
    assert_eq!(f.hits.load(Ordering::SeqCst), 0);
    end(&mut ca).await;
    end(&mut cb).await;
}

/// A10: once the owner moved on, a resume handed over late (a delayed or
/// repeated arm) is not taken: an Esc before the first arm, and an arm
/// again after the Esc that canceled the first one. Nothing starts at the
/// reset.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_resume_handed_over_after_the_owner_moved_on_is_not_armed() {
    let f = Fixture::new(Opts { reset_in: Duration::from_secs(8), ..Opts::default() }).await;
    let (stop_a, journal_a) = f.stopped_then_gone().await;
    let (stop_b, journal_b) = f.stopped_then_gone().await;

    // Esc first, then the arm.
    let a = f.restored(&journal_a, false).await;
    let mut ca = f.attach(&a).await;
    ca.send(SessionCommand::Cancel).await.unwrap();
    a.send(arm(&stop_a)).await.unwrap();
    let mut seen_a = settled(&mut ca, 911).await;
    assert!(entry_stop(&a).is_none(), "the Esc came first: not armed");

    // Armed; the Esc cancels it; the arm again is not taken.
    let b = f.restored(&journal_b, false).await;
    let mut cb = f.attach(&b).await;
    b.send(arm(&stop_b)).await.unwrap();
    settled(&mut cb, 912).await;
    assert_eq!(entry_stop(&b), Some(stop_b.clone()), "armed");
    cb.send(SessionCommand::Cancel).await.unwrap();
    until(&mut cb, |e| matches!(e, SessionEventWire::SystemNotice(t) if *t == canceled_notice())).await;
    assert!(cleared_and_acked(&b));
    b.send(arm(&stop_b)).await.unwrap();
    let mut seen_b = settled(&mut cb, 913).await;
    assert!(entry_stop(&b).is_none(), "armed again after the Esc: not taken");

    f.broker.set(Script::Vend);
    f.past_reset(Duration::from_millis(1500)).await;
    seen_a.extend(drain_for(&mut ca, Duration::from_millis(300)).await);
    seen_b.extend(drain_for(&mut cb, Duration::from_millis(300)).await);
    assert_eq!(turns(&seen_a), 0, "{seen_a:#?}");
    assert_eq!(turns(&seen_b), 0, "{seen_b:#?}");
    assert_eq!(f.hits.load(Ordering::SeqCst), 0);
    end(&mut ca).await;
    end(&mut cb).await;
}

/// A11: an idle parent whose workers the crash lost gets the crash
/// restore's note turn (the resume decision comes before the arm); that
/// turn continues the session, so the arm after it is not taken: exactly
/// one turn, and no capacity resume at the reset.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn an_idle_parent_whose_workers_were_lost_gets_one_turn_and_no_resume() {
    let f = Fixture::new(Opts { reset_in: Duration::from_secs(8), ..Opts::default() }).await;
    let (stop, journal) = f.stopped_then_gone().await;
    f.broker.set(Script::Vend);
    let handle = f.restored(&journal, false).await;
    let mut client = f.attach(&handle).await;
    handle
        .send(SessionCommand::ResumeAfterCrash {
            crash: "12236:99:boot".into(),
            crashed_at: Utc::now(),
            turn_in_flight: false,
            lost_workers: vec!["sa_9".into()],
            reply: None,
        })
        .await
        .unwrap();
    handle.send(arm(&stop)).await.unwrap();
    let mut seen = until(&mut client, |e| matches!(e, SessionEventWire::TurnStarted { .. })).await;
    seen.extend(until(&mut client, |e| matches!(e, SessionEventWire::Idle)).await);
    seen.extend(settled(&mut client, 921).await);
    assert!(entry_stop(&handle).is_none(), "the note's turn came first: not armed");
    f.past_reset(Duration::from_millis(1500)).await;
    seen.extend(drain_for(&mut client, Duration::from_millis(300)).await);
    assert_eq!(turns(&seen), 1, "the note's turn only: {seen:#?}");
    assert_eq!(resumes_of(&seen, &stop), 0, "{seen:#?}");
    assert_eq!(f.hits.load(Ordering::SeqCst), 1);
    end(&mut client).await;
}

/// SC: a resume restored with a parked session unparks it at its time and
/// is consumed once, but its turn sends NOTHING until the session's
/// `on_session_start` (held here) has run; an Esc while it is held ends
/// that turn with nothing sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_restored_parked_resume_sends_nothing_before_its_session_start() {
    let f = Fixture::new(Opts { reset_in: Duration::from_secs(8), ..Opts::default() }).await;
    let (stop_a, journal_a) = f.stopped_then_gone().await;
    let (stop_b, journal_b) = f.stopped_then_gone().await;
    f.broker.set(Script::Vend);
    let mut parked = Vec::new();
    for (stop, journal) in [(&stop_a, &journal_a), (&stop_b, &journal_b)] {
        let h = f.restored(journal, true).await;
        h.send(arm(stop)).await.unwrap();
        h.send(SessionCommand::Park).await.unwrap();
        assert!(wait_lifecycle(&h, SessionLifecycle::Parked, Duration::from_secs(5)).await);
        assert_eq!(entry_stop(&h).as_ref(), Some(stop), "armed, parked");
        parked.push(h);
    }
    let (a, b) = (parked[0].clone(), parked[1].clone());
    let (mut wa, mut wb) = (a.subscribe(), b.subscribe());
    let hold = start_seam::hold_gate(&f.host).await;
    let calls_a = start_seam::calls(&journal_a);

    let started = |e: &SessionEventWire| {
        matches!(e, SessionEventWire::TurnStarted { trigger: TurnTrigger::QueuedAuto, .. })
    };
    let mut seen_a = watch_until(&mut wa, Duration::from_secs(20), started).await;
    let mut seen_b = watch_until(&mut wb, Duration::from_secs(20), started).await;
    assert!(entry_stop(&a).is_none() && entry_stop(&b).is_none(), "consumed");
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(f.hits.load(Ordering::SeqCst), 0, "nothing before on_session_start");
    assert_eq!(start_seam::calls(&journal_a), calls_a, "a's hook waits");

    b.send(SessionCommand::Cancel).await.unwrap();
    seen_b.extend(watch_until(&mut wb, Duration::from_secs(10), |e| matches!(e, SessionEventWire::Idle)).await);
    drop(hold);
    seen_a.extend(watch_until(&mut wa, Duration::from_secs(20), |e| matches!(e, SessionEventWire::Idle)).await);
    assert!(start_seam::calls(&journal_a) > calls_a, "a's hook ran");
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(f.hits.load(Ordering::SeqCst), 1, "a's request only, after its hook");
    assert_eq!(resumes_of(&seen_a, &stop_a), 1, "{seen_a:#?}");
    assert_eq!(errors(&seen_a), 0, "{seen_a:#?}");
    assert_eq!(resumes_of(&seen_b, &stop_b), 1, "b's turn started, then the Esc: {seen_b:#?}");
    for h in [a, b] {
        h.send(SessionCommand::End { reason: EndReason::HostShutdown }).await.unwrap();
    }
}

/// D4, window 1: the daemon dies while the consumption's write is in
/// flight (never acknowledged): nothing was dispatched, the record still
/// holds the resume, and the next daemon's restore resumes it ONCE.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_crash_before_the_consumption_lands_resumes_once_from_the_record() {
    let f = Fixture::new(Opts { reset_in: Duration::from_secs(4), ack_wait: Duration::from_secs(30), ..Opts::default() })
        .await;
    let (handle, actor, slot) = f.crashable().await;
    let journal = handle.journal_id();
    let mut client = f.attach(&handle).await;
    client.send(submit("work that stops")).await.unwrap();
    let mut seen = until(&mut client, |e| matches!(e, SessionEventWire::Idle)).await;
    seen.extend(drain_for(&mut client, Duration::from_millis(200)).await);
    assert_eq!(armed(&seen, &f), 1, "{seen:#?}");
    let stop = entry_stop(&handle).expect("armed");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while f.writer().landed(slot) != Some(Some(stop.clone())) {
        assert!(tokio::time::Instant::now() < deadline, "the armed entry never landed");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    f.writer().set(Mode::Hold);
    f.broker.set(Script::Vend);
    entry_until(&handle, "consumed", |e| e.record.capacity_stop.is_none()).await;
    f.writer().crash(slot);
    actor.abort();
    let _ = actor.await;
    let after = drain_for(&mut client, Duration::from_millis(200)).await;
    assert_eq!(turns(&after), 0, "nothing dispatched before the acknowledgement: {after:#?}");
    f.writer().set(Mode::Ack);
    let recorded = f.writer().landed(slot).flatten().expect("the record still holds the resume");
    assert_eq!(recorded, stop);

    let restored = f.restored(&journal, false).await;
    let mut c2 = f.attach(&restored).await;
    restored.send(arm(&recorded)).await.unwrap();
    let mut seen = until(&mut c2, |e| matches!(e, SessionEventWire::TurnStarted { .. })).await;
    seen.extend(until(&mut c2, |e| matches!(e, SessionEventWire::Idle)).await);
    seen.extend(drain_for(&mut c2, Duration::from_secs(2)).await);
    assert_eq!(resumes_of(&seen, &stop), 1, "{seen:#?}");
    assert_eq!(turns(&seen), 1, "{seen:#?}");
    assert_eq!(f.hits.load(Ordering::SeqCst), 1, "one resume in all");
    end(&mut c2).await;
}

/// D4, window 2: the daemon dies after the consumption was acknowledged
/// and before the dispatch: the record no longer holds the resume, so the
/// next daemon's restore has nothing to hand over. That resume is lost
/// (at most once), never given twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_crash_after_the_consumption_lands_loses_that_resume() {
    let f = Fixture::new(Opts { reset_in: Duration::from_secs(4), ..Opts::default() }).await;
    let pause = PauseAfterAck::set();
    let (handle, actor, slot) = f.crashable().await;
    let journal = handle.journal_id();
    let mut client = f.attach(&handle).await;
    client.send(submit("work that stops")).await.unwrap();
    let mut seen = until(&mut client, |e| matches!(e, SessionEventWire::Idle)).await;
    seen.extend(drain_for(&mut client, Duration::from_millis(200)).await);
    assert_eq!(armed(&seen, &f), 1, "{seen:#?}");
    f.broker.set(Script::Vend);
    tokio::time::timeout(Duration::from_secs(20), pause.reached()).await.expect("consumed");
    assert_eq!(f.writer().landed(slot), Some(None), "the consumption landed before any dispatch");
    f.writer().crash(slot);
    actor.abort();
    let _ = actor.await;
    drop(pause);
    let after = drain_for(&mut client, Duration::from_millis(200)).await;
    assert_eq!(turns(&after), 0, "nothing dispatched: {after:#?}");

    let restored = f.restored(&journal, false).await;
    let mut c2 = f.attach(&restored).await;
    f.past_reset(Duration::from_millis(1500)).await;
    let seen = drain_for(&mut c2, Duration::from_millis(300)).await;
    assert_eq!(turns(&seen), 0, "{seen:#?}");
    assert!(entry_stop(&restored).is_none());
    assert_eq!(f.hits.load(Ordering::SeqCst), 0);
    end(&mut c2).await;
}

/// A7b: a compaction while the session is armed moves the stop onto the
/// compacted history: the published entry's length and last digest follow
/// it (its reset, stop time, kind and attempt stay), and at the reset
/// exactly ONE resume goes out. The background job refingerprints here;
/// the inline body (`SYNAPS_SESSION_COMPACT_INLINE=1`) in the next test.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn an_armed_stop_follows_a_compaction_and_resumes_once() {
    an_armed_stop_follows(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn an_armed_stop_follows_an_inline_compaction_and_resumes_once() {
    let _env = Env::set(&[("SYNAPS_SESSION_COMPACT_INLINE", "1")]);
    an_armed_stop_follows(true).await;
}

async fn an_armed_stop_follows(inline: bool) {
    let f = Fixture::new(Opts { reset_in: Duration::from_secs(10), ..Opts::default() }).await;
    let handle = f.session().await;
    let mut client = f.attach(&handle).await;
    f.broker.set(Script::Vend);
    for i in 0..2 {
        client.send(submit(&format!("hello {i}"))).await.unwrap();
        until(&mut client, |e| matches!(e, SessionEventWire::Idle)).await;
    }
    f.broker.set(Script::Cool(f.reset_ms));
    client.send(submit("work that stops")).await.unwrap();
    let mut seen = until(&mut client, |e| matches!(e, SessionEventWire::Idle)).await;
    seen.extend(drain_for(&mut client, Duration::from_millis(200)).await);
    assert_eq!(armed(&seen, &f), 1, "{seen:#?}");
    let before = entry_stop(&handle).expect("armed");
    assert_eq!(before.history_len, 5, "two warm turns and the stopped message");
    let warm = f.hits.load(Ordering::SeqCst);

    // An account is back before the reset; the resume still waits for it.
    f.broker.set(Script::Vend);
    client.send(SessionCommand::Compact { instructions: None }).await.unwrap();
    if inline {
        until(&mut client, |e| {
            matches!(e, SessionEventWire::SystemNotice(n) if n.starts_with("[compacted → ~"))
        })
        .await;
    } else {
        until(&mut client, |e| matches!(e, SessionEventWire::CompactionApplied { .. })).await;
    }
    let conv = until(&mut client, |e| matches!(e, SessionEventWire::Conversation(_))).await;
    let Some(SessionEventWire::Conversation(snap)) = conv.last().map(|e| &e.event) else {
        unreachable!()
    };
    assert!(f.hits.load(Ordering::SeqCst) > warm, "the compaction's request");
    assert_eq!(snap.api_messages.len(), snap.messages_len, "the whole history");
    let after = entry_stop(&handle).expect("still armed");
    assert_eq!(after.history_len as usize, snap.messages_len, "{after:?}");
    assert_eq!(after.last_digest, sha256_hex(snap.api_messages.last().expect("a history")));
    assert_ne!(
        (after.history_len, &after.last_digest),
        (before.history_len, &before.last_digest),
        "the compaction changed the history"
    );
    assert_eq!(
        (after.resume_at, after.stopped_at, &after.kind, after.attempt),
        (before.resume_at, before.stopped_at, &before.kind, before.attempt)
    );
    assert!(Utc::now() < f.resume_at(), "checked before the reset");

    f.past_reset(Duration::ZERO).await;
    let mut seen = until(&mut client, |e| {
        matches!(e, SessionEventWire::TurnStarted { trigger: TurnTrigger::QueuedAuto, .. })
    })
    .await;
    seen.extend(until(&mut client, |e| matches!(e, SessionEventWire::Idle)).await);
    seen.extend(drain_for(&mut client, Duration::from_secs(2)).await);
    let r = resumes(&seen);
    assert_eq!(r.len(), 1, "{seen:#?}");
    assert!(r[0].0 >= f.resume_at(), "not before the reset");
    assert_eq!(r[0].1, resume_text(before.stopped_at, before.resume_at));
    assert_eq!(turns(&seen), 1, "{seen:#?}");
    assert_eq!(errors(&seen), 0, "the resumed turn ran: {seen:#?}");
    let bodies = f.bodies.lock().unwrap().clone();
    let resumed = bodies
        .iter()
        .filter(|b| user_texts(b).iter().any(|t| t.starts_with("resume: ")))
        .count();
    assert_eq!(resumed, 1, "{bodies:#?}");
    assert!(entry_stop(&handle).is_none());
    end(&mut client).await;
}

/// Buffered events AND a queued user message behind the stopped turn, with
/// the auto-turn cap already spent (the stopped turn was an event turn): at
/// the reset the events go into history first, then the owner's message is
/// the ONE turn that starts (a user turn: no cap notice, no resume text).
/// Being the owner's, it resets the auto-turn counter, so a later event
/// starts an event turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn events_and_a_message_queued_with_the_cap_spent_start_one_user_turn() {
    const OWNER: &str = "the owner's correction";
    let f = Fixture::new(Opts {
        reset_in: Duration::from_secs(5),
        config: "events.auto_turn_cap = 1\n",
        ..Opts::default()
    })
    .await;
    let (handle, queue) = f.session_with_queue().await;
    let mut client = f.attach(&handle).await;
    f.broker.set(Script::VendThenCool(f.reset_ms));
    f.broker.hold();
    queue.push(Event::simple("test", "first event", None)).unwrap();
    f.broker.selected(2).await;
    queue.push(Event::simple("test", "second event", None)).unwrap();
    until(&mut client, |e| {
        matches!(e, SessionEventWire::External(ev) if ev.content.text == "second event")
    })
    .await;
    client.send(submit(OWNER)).await.unwrap();
    until(&mut client, |e| matches!(e, SessionEventWire::Steered { .. })).await;
    f.broker.release();
    let mut seen = until(&mut client, |e| matches!(e, SessionEventWire::Idle)).await;
    seen.extend(drain_for(&mut client, Duration::from_millis(200)).await);
    assert_eq!(armed(&seen, &f), 1, "{seen:#?}");
    assert_eq!(f.hits.load(Ordering::SeqCst), 0);

    f.broker.set(Script::Vend);
    f.past_reset(Duration::ZERO).await;
    let mut seen = until(&mut client, |e| matches!(e, SessionEventWire::TurnStarted { .. })).await;
    seen.extend(until(&mut client, |e| matches!(e, SessionEventWire::Idle)).await);
    seen.extend(drain_for(&mut client, Duration::from_secs(2)).await);
    assert_eq!(turns(&seen), 1, "{seen:#?}");
    assert!(
        seen.iter().any(|e| matches!(
            &e.event,
            SessionEventWire::TurnStarted { trigger: TurnTrigger::QueuedAuto, user_text: Some(t), .. } if t == OWNER
        )),
        "{seen:#?}"
    );
    assert!(resumes(&seen).is_empty(), "{seen:#?}");
    assert!(
        !seen.iter().any(|e| matches!(e.event, SessionEventWire::AutoTurnCapReached { .. })),
        "{seen:#?}"
    );
    assert!(entry_stop(&handle).is_none());
    let bodies = f.bodies.lock().unwrap().clone();
    assert_eq!(bodies.len(), 1, "{bodies:#?}");
    let texts = user_texts(&bodies[0]);
    assert_eq!(texts.last().map(String::as_str), Some(OWNER), "{texts:#?}");
    let second = texts.iter().position(|t| t.contains("second event"));
    assert!(second.is_some_and(|i| i + 1 < texts.len()), "the event before the owner's text: {texts:#?}");
    assert!(!texts.iter().any(|t| t.starts_with("resume: ")), "{texts:#?}");

    // The owner's turn reset the counter: a later event claims a turn.
    queue.push(Event::simple("test", "third event", None)).unwrap();
    let mut seen = until(&mut client, |e| matches!(e, SessionEventWire::TurnStarted { .. })).await;
    seen.extend(until(&mut client, |e| matches!(e, SessionEventWire::Idle)).await);
    assert!(
        seen.iter().any(|e| matches!(
            e.event,
            SessionEventWire::TurnStarted { trigger: TurnTrigger::EventAuto, .. }
        )),
        "{seen:#?}"
    );
    assert!(
        !seen.iter().any(|e| matches!(e.event, SessionEventWire::AutoTurnCapReached { .. })),
        "{seen:#?}"
    );
    assert_eq!(f.hits.load(Ordering::SeqCst), 2);
    end(&mut client).await;
}

// ── helpers ─────────────────────────────────────────────────────────────

/// The digest a capacity stop pins (`capacity_resume::last_digest`): the
/// SHA-256 of the message's JSON, in hex.
fn sha256_hex(m: &Value) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(serde_json::to_vec(m).unwrap())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A loopback provider stub answering every streamed request with
/// [`SSE_HI`] and a request that is not streamed (a compaction's summary)
/// with a summary message, keeping each request body.
async fn capture_stub() -> (String, Arc<AtomicUsize>, Bodies) {
    use axum::response::IntoResponse;
    let hits = Arc::new(AtomicUsize::new(0));
    let seen: Bodies = Arc::default();
    let (hits_c, seen_c) = (Arc::clone(&hits), Arc::clone(&seen));
    let app = axum::Router::new().fallback(move |body: axum::body::Bytes| {
        let (hits, seen) = (Arc::clone(&hits_c), Arc::clone(&seen_c));
        async move {
            hits.fetch_add(1, Ordering::SeqCst);
            let v = serde_json::from_slice::<Value>(&body).ok();
            let streamed = v.as_ref().is_some_and(|v| v["stream"] == true);
            if let Some(v) = v {
                seen.lock().unwrap().push(v);
            }
            if !streamed {
                return axum::Json(serde_json::json!({
                    "id": "msg_c", "type": "message", "role": "assistant",
                    "model": MODEL, "stop_reason": "end_turn",
                    "content": [{"type": "text", "text": "SUMMARY: hello twice, then work that stopped."}],
                    "usage": {"input_tokens": 50, "output_tokens": 10}
                }))
                .into_response();
            }
            (
                axum::http::StatusCode::OK,
                [("content-type", "text/event-stream")],
                SSE_HI.to_string(),
            )
                .into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), hits, seen)
}

/// The texts of a request body's user messages, in order (a string
/// content, or each text block).
fn user_texts(body: &Value) -> Vec<String> {
    let mut out = Vec::new();
    for m in body["messages"].as_array().into_iter().flatten() {
        if m["role"] != "user" {
            continue;
        }
        match &m["content"] {
            Value::String(s) => out.push(s.clone()),
            Value::Array(blocks) => {
                out.extend(blocks.iter().filter_map(|b| b["text"].as_str().map(str::to_string)))
            }
            _ => {}
        }
    }
    out
}

/// Environment variables for one test, removed when it ends.
struct Env(Vec<&'static str>);

impl Env {
    fn set(vars: &[(&'static str, &str)]) -> Self {
        for (k, v) in vars {
            std::env::set_var(k, v);
        }
        Self(vars.iter().map(|(k, _)| *k).collect())
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        for k in &self.0 {
            std::env::remove_var(k);
        }
    }
}

async fn detach(t: &mut LocalTransport) {
    t.send(SessionCommand::Detach { client: t.client_id() }).await.unwrap();
    until(t, |e| matches!(e, SessionEventWire::ClientLeft { .. })).await;
}

async fn wait_lifecycle(handle: &SessionHandle, want: SessionLifecycle, within: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + within;
    while tokio::time::Instant::now() < deadline {
        if handle.lifecycle() == want {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    handle.lifecycle() == want
}
