//! Prompt-cache keep-warm policy (task #444), owned by the session actor.
//!
//! Why the actor: it is the one place that knows a session is idle (turn
//! finished, nothing queued, no compaction, no prompt, no driver), when it
//! parks or ends, and what it has spent. The ping itself is
//! `Runtime::cache_keepwarm_ping` (`runtime::keepwarm`).
//!
//! # Policy (docs/specs/cache-ttl-spec.md §7)
//!
//! An **idle period** starts when a turn whose last Anthropic request was
//! accepted ends with a finished assistant reply and `cache_keepwarm` is not
//! `off`. It holds:
//!
//! - **the anchor**: send time of the last accepted request (monotonic +
//!   wall clock). The provider's TTL runs from the start of that request.
//! - **the governing TTL**: the history marker's TTL — 1h under `1h`, 5m
//!   under `5m` *and* `hybrid` (hybrid's history tail is the 5m entry; its
//!   1h tools/system entries are refreshed by the same read). Pings are due
//!   at `anchor + TTL − margin` (55 min / 4.5 min).
//! - **the prefix fingerprint** recorded at idle. Every ping re-builds the
//!   prefix and is refused (period ends) if it no longer matches. No command
//!   allowlist: a command that changes the next request's prefix (model,
//!   thinking, tools, system prompt, history) ends the period through the
//!   fingerprint; every other command leaves it alone.
//! - **a ski-rental ledger**: the measured cost of every ping (cache read,
//!   any cache write, uncached input, output incl. thinking). A ping is only
//!   sent while `ledger + next ping ≤ bound`, where the bound is what the
//!   period protects: re-writing the current prefix at its TTL's write price.
//!   Deterministic ski rental: at most 2× the optimal spend whenever the user
//!   comes back, at most ~one rewrite when nobody does.
//!
//! The period ends on the first of: bound, `<N>h` window, prefix change, a
//! ping that wrote instead of read, expiry (machine slept / ran late), a 4xx,
//! exhausted transient retries, `max_session_cost`, a turn or compaction
//! starting, `/keepwarm off`, or the session ending. While a period is active
//! the session does not park; when it ends, parking resumes normally.

use std::time::{Duration, SystemTime};

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::actor::SessionActor;
use super::types::ClientKind;
use crate::core::config::{CacheKeepWarm, CacheTtl};
use crate::runtime::keepwarm::{CacheAnchor, KeepWarmUsage, PingFailure, PingOutcome};

/// No ping may start once the cache is this close to expiry: the request
/// still has to reach the provider (connect, upload a large body).
pub const LATE_GUARD: Duration = Duration::from_secs(15);

/// Delays before retrying a transient ping failure; then the period ends.
pub const BACKOFF: [Duration; 3] = [
    Duration::from_secs(5),
    Duration::from_secs(15),
    Duration::from_secs(30),
];

/// Relative slack for the bound comparison (f64 sums of equal ping costs).
const BOUND_EPSILON: f64 = 1e-9;

/// TTL of the entry that covers the history prefix: the shortest TTL among
/// the breakpoints a ping must keep alive. Anthropic requires longer TTLs to
/// precede shorter ones, so under `hybrid` the history (tail) marker is the
/// 5m one.
pub fn governing_ttl(ttl: CacheTtl) -> Duration {
    match ttl {
        CacheTtl::OneHour => Duration::from_secs(3600),
        CacheTtl::FiveMinutes | CacheTtl::Hybrid => Duration::from_secs(300),
    }
}

/// Safety margin before the TTL: 5 min for 1h, 30 s for 5m.
pub fn safety_margin(ttl: CacheTtl) -> Duration {
    match ttl {
        CacheTtl::OneHour => Duration::from_secs(300),
        CacheTtl::FiveMinutes | CacheTtl::Hybrid => Duration::from_secs(30),
    }
}

/// Ping interval, measured from the anchor: governing TTL − margin.
/// `SYNAPS_CACHE_KEEPWARM_INTERVAL_MS` overrides it (test seam only).
pub fn ping_interval(ttl: CacheTtl) -> Duration {
    if let Some(ms) = std::env::var("SYNAPS_CACHE_KEEPWARM_INTERVAL_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
    {
        return Duration::from_millis(ms);
    }
    governing_ttl(ttl) - safety_margin(ttl)
}

/// Cost of re-writing `prefix_tokens` at the governing TTL's write price:
/// what the next turn pays if the cache lapses — the ski-rental "buy" price.
pub fn rewrite_cost_usd(model: &str, ttl: CacheTtl, prefix_tokens: u64) -> f64 {
    match ttl {
        CacheTtl::OneHour => crate::pricing::calculate_cost_split(model, 0, 0, 0, 0, prefix_tokens),
        CacheTtl::FiveMinutes | CacheTtl::Hybrid => {
            crate::pricing::calculate_cost_split(model, 0, 0, 0, prefix_tokens, 0)
        }
    }
}

/// Why a period ended (or never started). Logged once, shown by
/// `/keepwarm status`.
#[derive(Debug, Clone, PartialEq)]
pub enum PeriodEnd {
    /// The last request cached nothing (prefix below the model minimum).
    NothingCached,
    /// Ski-rental bound: the next ping would push the ledger past the
    /// rewrite it prevents.
    BoundReached {
        ledger_usd: f64,
        bound_usd: f64,
    },
    /// `<N>h` window elapsed.
    HoursReached(u32),
    /// The cache is (nearly) expired: the machine slept, or the session went
    /// idle too late. A ping now would be a full write.
    Expired {
        age_secs: u64,
    },
    /// The next turn's prefix is no longer what is cached.
    PrefixChanged,
    /// A ping wrote the prefix instead of reading it.
    CacheMiss {
        written: u64,
    },
    /// 4xx / auth / configuration: never retried.
    Rejected(String),
    /// Transient errors outlasted the backoff (or the TTL).
    TransientExhausted(String),
    /// `max_session_cost` reached.
    CostCap {
        cap: f64,
    },
    TurnStarted,
    Compaction,
    /// `/keepwarm off`.
    Disabled,
    SessionEnding,
    /// Something else owns the session (driver, queued work, prompt).
    Blocked(&'static str),
}

impl std::fmt::Display for PeriodEnd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PeriodEnd::NothingCached => f.write_str("nothing cached"),
            PeriodEnd::BoundReached {
                ledger_usd,
                bound_usd,
            } => write!(
                f,
                "ski-rental bound reached (${ledger_usd:.4} spent of ${bound_usd:.4})"
            ),
            PeriodEnd::HoursReached(h) => write!(f, "{h}h window reached"),
            PeriodEnd::Expired { age_secs } => {
                write!(
                    f,
                    "cache expired or about to ({age_secs}s since last request; slept?)"
                )
            }
            PeriodEnd::PrefixChanged => f.write_str("prefix changed"),
            PeriodEnd::CacheMiss { written } => {
                write!(f, "ping missed the cache (wrote {written} tokens)")
            }
            PeriodEnd::Rejected(m) => write!(f, "ping rejected: {m}"),
            PeriodEnd::TransientExhausted(m) => write!(f, "transient errors: {m}"),
            PeriodEnd::CostCap { cap } => write!(f, "max_session_cost ${cap:.4} reached"),
            PeriodEnd::TurnStarted => f.write_str("turn started"),
            PeriodEnd::Compaction => f.write_str("compaction"),
            PeriodEnd::Disabled => f.write_str("disabled"),
            PeriodEnd::SessionEnding => f.write_str("session ending"),
            PeriodEnd::Blocked(why) => write!(f, "{why}"),
        }
    }
}

/// What the period needs to know about the session at start.
#[derive(Debug, Clone)]
pub struct PeriodConfig {
    pub mode: CacheKeepWarm,
    pub ttl: CacheTtl,
    pub model: String,
    pub interval: Duration,
}

/// Timer decision.
#[derive(Debug, Clone, PartialEq)]
pub enum Fire {
    NotYet,
    Ping,
    End(PeriodEnd),
}

/// One idle period. Pure: every clock reading is passed in.
#[derive(Debug, Clone)]
pub struct Period {
    pub id: u64,
    cfg: PeriodConfig,
    idle_since: Instant,
    anchor: CacheAnchor,
    fingerprint: u64,
    prefix_tokens: u64,
    bound_usd: f64,
    ledger_usd: f64,
    est_next_usd: f64,
    pings: u32,
    retries: usize,
    next_at: Instant,
    in_flight: bool,
    /// Cache age when the in-flight ping was fired (for the log line).
    fired_age: Duration,
}

/// Cache age: the larger of monotonic and wall-clock elapsed time since the
/// anchor. The monotonic clock stops while the machine sleeps; the
/// provider's TTL does not.
fn age(anchor: &CacheAnchor, now: Instant, wall: SystemTime) -> Duration {
    let mono = now.saturating_duration_since(anchor.mono);
    let wall = wall.duration_since(anchor.wall).unwrap_or_default();
    mono.max(wall)
}

impl Period {
    /// Start a period at `now` (the session just went idle). `prefix_tokens`
    /// estimates the cached prefix from the turn's last request (read +
    /// write + input + output); every ping then measures it exactly.
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        id: u64,
        cfg: PeriodConfig,
        fingerprint: u64,
        prefix_tokens: u64,
        anchor: CacheAnchor,
        now: Instant,
        wall: SystemTime,
    ) -> Result<Period, PeriodEnd> {
        if prefix_tokens == 0 {
            return Err(PeriodEnd::NothingCached);
        }
        let ttl = governing_ttl(cfg.ttl);
        let age_now = age(&anchor, now, wall);
        if age_now + LATE_GUARD >= ttl {
            return Err(PeriodEnd::Expired {
                age_secs: age_now.as_secs(),
            });
        }
        let bound_usd = rewrite_cost_usd(&cfg.model, cfg.ttl, prefix_tokens);
        let est_next_usd =
            crate::pricing::calculate_cost_split(&cfg.model, 0, 0, prefix_tokens, 0, 0);
        // Due at anchor + interval; a turn that streamed past it is due now.
        let next_at = (anchor.mono + cfg.interval).max(now);
        let p = Period {
            id,
            cfg,
            idle_since: now,
            anchor,
            fingerprint,
            prefix_tokens,
            bound_usd,
            ledger_usd: 0.0,
            est_next_usd,
            pings: 0,
            retries: 0,
            next_at,
            in_flight: false,
            fired_age: Duration::ZERO,
        };
        if let Some(r) = p.past_window(next_at).or_else(|| p.past_bound()) {
            return Err(r);
        }
        Ok(p)
    }

    pub fn fingerprint(&self) -> u64 {
        self.fingerprint
    }
    pub fn pings(&self) -> u32 {
        self.pings
    }
    pub fn ledger_usd(&self) -> f64 {
        self.ledger_usd
    }
    pub fn bound_usd(&self) -> f64 {
        self.bound_usd
    }
    pub fn prefix_tokens(&self) -> u64 {
        self.prefix_tokens
    }
    pub fn next_at(&self) -> Instant {
        self.next_at
    }
    pub fn mode(&self) -> CacheKeepWarm {
        self.cfg.mode
    }
    pub fn ttl(&self) -> CacheTtl {
        self.cfg.ttl
    }
    pub fn fired_age(&self) -> Duration {
        self.fired_age
    }
    pub fn in_flight(&self) -> bool {
        self.in_flight
    }

    /// `/keepwarm auto|<N>h` while a period runs: the new window applies.
    pub fn set_mode(&mut self, mode: CacheKeepWarm) {
        self.cfg.mode = mode;
    }

    /// When the timer should next wake the actor; `None` while in flight.
    pub fn deadline(&self) -> Option<Instant> {
        (!self.in_flight).then_some(self.next_at)
    }

    fn past_window(&self, at: Instant) -> Option<PeriodEnd> {
        match self.cfg.mode {
            CacheKeepWarm::Hours(h)
                if at >= self.idle_since + Duration::from_secs(u64::from(h) * 3600) =>
            {
                Some(PeriodEnd::HoursReached(h))
            }
            _ => None,
        }
    }

    fn past_bound(&self) -> Option<PeriodEnd> {
        (self.ledger_usd + self.est_next_usd > self.bound_usd * (1.0 + BOUND_EPSILON)).then_some(
            PeriodEnd::BoundReached {
                ledger_usd: self.ledger_usd,
                bound_usd: self.bound_usd,
            },
        )
    }

    /// The timer fired. `Ping` marks the ping in flight.
    pub fn fire(
        &mut self,
        now: Instant,
        wall: SystemTime,
        session_cost: f64,
        max_session_cost: Option<f64>,
    ) -> Fire {
        if self.in_flight || now < self.next_at {
            return Fire::NotYet;
        }
        let age_now = age(&self.anchor, now, wall);
        if age_now + LATE_GUARD >= governing_ttl(self.cfg.ttl) {
            return Fire::End(PeriodEnd::Expired {
                age_secs: age_now.as_secs(),
            });
        }
        if let Some(r) = self.past_window(now).or_else(|| self.past_bound()) {
            return Fire::End(r);
        }
        if let Some(cap) = max_session_cost.filter(|c| c.is_finite()) {
            if session_cost + self.est_next_usd >= cap {
                return Fire::End(PeriodEnd::CostCap { cap });
            }
        }
        self.in_flight = true;
        self.fired_age = age_now;
        Fire::Ping
    }

    /// The in-flight ping was sent and accepted; `cost_usd` is its measured
    /// cost, `anchor` the runtime's anchor after it. Schedules the next ping
    /// or says why the period is over.
    pub fn ping_sent(
        &mut self,
        usage: &KeepWarmUsage,
        cost_usd: f64,
        anchor: Option<CacheAnchor>,
    ) -> Result<(), PeriodEnd> {
        self.in_flight = false;
        self.retries = 0;
        self.pings += 1;
        self.ledger_usd += cost_usd;
        if usage.cache_read_tokens == 0 && usage.cache_write_tokens > 0 {
            return Err(PeriodEnd::CacheMiss {
                written: usage.cache_write_tokens,
            });
        }
        let measured = usage.cache_read_tokens + usage.cache_write_tokens;
        if measured > 0 {
            self.prefix_tokens = measured;
            self.bound_usd = rewrite_cost_usd(&self.cfg.model, self.cfg.ttl, measured);
        }
        // Steady state: a full read of the (now complete) prefix plus this
        // ping's own uncached input and output (thinking included). A one-off
        // write (the first ping caches the last reply) is in the ledger, not
        // in the estimate.
        self.est_next_usd = crate::pricing::calculate_cost_split(
            usage.model.as_deref().unwrap_or(&self.cfg.model),
            usage.input_tokens,
            usage.output_tokens,
            self.prefix_tokens,
            0,
            0,
        );
        if let Some(a) = anchor.filter(|a| a.mono >= self.anchor.mono) {
            self.anchor = a;
        }
        self.next_at = self.anchor.mono + self.cfg.interval;
        if let Some(r) = self.past_window(self.next_at).or_else(|| self.past_bound()) {
            return Err(r);
        }
        Ok(())
    }

    /// The in-flight ping failed. Transient failures retry with bounded
    /// backoff as long as the retry still lands before expiry.
    pub fn ping_failed(
        &mut self,
        failure: &PingFailure,
        now: Instant,
        wall: SystemTime,
    ) -> Result<Duration, PeriodEnd> {
        self.in_flight = false;
        let msg = match failure {
            PingFailure::Fatal(m) => return Err(PeriodEnd::Rejected(m.clone())),
            PingFailure::Canceled => return Err(PeriodEnd::Rejected("canceled".into())),
            PingFailure::Transient(m) => m.clone(),
        };
        let Some(delay) = BACKOFF.get(self.retries).copied() else {
            return Err(PeriodEnd::TransientExhausted(msg));
        };
        let age_then = age(&self.anchor, now, wall) + delay;
        if age_then + LATE_GUARD >= governing_ttl(self.cfg.ttl) {
            return Err(PeriodEnd::TransientExhausted(format!(
                "{msg}; no retry fits before the cache expires"
            )));
        }
        self.retries += 1;
        self.next_at = now + delay;
        Ok(delay)
    }
}

/// One in-flight ping.
pub(crate) struct PingTask {
    pub(crate) handle: tokio::task::JoinHandle<std::result::Result<PingOutcome, PingFailure>>,
    cancel: CancellationToken,
}

/// Actor-owned keep-warm state.
#[derive(Default)]
pub(crate) struct KeepWarmState {
    /// `cache_keepwarm` from config at session create.
    configured: CacheKeepWarm,
    /// `/keepwarm …` for this session (survives park and daemon reload).
    pub(crate) override_mode: Option<CacheKeepWarm>,
    pub(crate) period: Option<Period>,
    pub(crate) task: Option<PingTask>,
    next_period_id: u64,
    /// Set by `start_turn`, consumed by the next `Idle`.
    turn_started_at: Option<Instant>,
    /// Prefix estimate from the turn's latest Usage (read+write+input+output).
    last_request_tokens: u64,
    /// A TUI/attach client has been attached at some point. Script/RPC-only
    /// sessions (nobody will be back in human time) never keep warm.
    pub(crate) interactive_seen: bool,
    last_end: Option<String>,
    session_pings: u32,
}

impl KeepWarmState {
    pub(crate) fn new(configured: CacheKeepWarm) -> Self {
        Self {
            configured,
            ..Self::default()
        }
    }

    pub(crate) fn mode(&self) -> CacheKeepWarm {
        self.override_mode.unwrap_or(self.configured)
    }

    pub(crate) fn active(&self) -> bool {
        self.period.is_some()
    }

    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.period.as_ref().and_then(Period::deadline)
    }

    fn abort_ping(&mut self) {
        if let Some(task) = self.task.take() {
            task.cancel.cancel();
            task.handle.abort();
        }
    }
}

/// `select!` arm: the next ping is due.
pub(crate) async fn keepwarm_timer(deadline: Option<Instant>) {
    match deadline {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

/// `select!` arm: the in-flight ping finished.
pub(crate) async fn poll_keepwarm_ping(
    task: &mut Option<PingTask>,
) -> std::result::Result<std::result::Result<PingOutcome, PingFailure>, tokio::task::JoinError> {
    match task {
        Some(t) => (&mut t.handle).await,
        None => std::future::pending().await,
    }
}

impl SessionActor {
    /// Why keep-warm must not ping right now (`None` = clear).
    fn cache_keepwarm_blocker(&self) -> Option<&'static str> {
        if !self.runtime.is_live() || !self.conv.is_live() || self.is_parked() {
            return Some("session parked");
        }
        if self.streaming {
            return Some("turn running");
        }
        if self.compact.is_some() {
            return Some("compaction running");
        }
        if self.conv.queued_message.is_some() || !self.event_queue.is_empty() {
            return Some("work queued");
        }
        if !self.pending_prompts.is_empty() {
            return Some("prompt pending");
        }
        if self.driver.is_some() || self.driver_pending.is_some() {
            return Some("session driver armed");
        }
        None
    }

    /// A client attached: TUI/attach clients mark the session interactive.
    pub(crate) fn cache_keepwarm_note_attach(&mut self, kind: ClientKind) {
        if matches!(kind, ClientKind::Tui | ClientKind::Attach) {
            self.cache_keepwarm.interactive_seen = true;
        }
    }

    /// A provider Usage event of the running turn: remember the request's
    /// total token count as the prefix estimate.
    pub(crate) fn cache_keepwarm_note_usage(
        &mut self,
        input: u64,
        output: u64,
        cache_read: u64,
        cache_write: u64,
    ) {
        self.cache_keepwarm.last_request_tokens = input + output + cache_read + cache_write;
    }

    /// A real turn is starting: the idle period is over, any ping in flight
    /// is aborted. The next `Idle` may start a new period.
    pub(crate) fn cache_keepwarm_turn_started(&mut self) {
        self.cache_keepwarm_end(PeriodEnd::TurnStarted);
        self.cache_keepwarm.turn_started_at = Some(Instant::now());
        self.cache_keepwarm.last_request_tokens = 0;
    }

    /// Compaction replaces the history: nothing is worth keeping warm until
    /// the next real turn.
    pub(crate) fn cache_keepwarm_compaction(&mut self) {
        self.cache_keepwarm_end(PeriodEnd::Compaction);
        self.cache_keepwarm.turn_started_at = None;
    }

    /// End the active period (and abort any ping) with one final log line.
    pub(crate) fn cache_keepwarm_end(&mut self, reason: PeriodEnd) {
        self.cache_keepwarm.abort_ping();
        let Some(p) = self.cache_keepwarm.period.take() else {
            return;
        };
        tracing::info!(
            session = %self.id,
            period = p.id,
            pings = p.pings(),
            ledger_usd = p.ledger_usd(),
            bound_usd = p.bound_usd(),
            reason = %reason,
            "cache keep-warm period ended"
        );
        self.cache_keepwarm.last_end = Some(reason.to_string());
        // A period defers parking; now the session may park normally.
        self.rearm_park();
    }

    /// The session announced `Idle`: start a period if the turn that just
    /// ended had an Anthropic request accepted and keep-warm is on.
    pub(crate) async fn cache_keepwarm_on_idle(&mut self) {
        let Some(turn_started_at) = self.cache_keepwarm.turn_started_at.take() else {
            return;
        };
        let mode = self.cache_keepwarm.mode();
        if mode.is_off() || !self.cache_keepwarm.interactive_seen {
            return;
        }
        // Never before a real Anthropic request of THIS turn was accepted.
        let Some(anchor) = self
            .runtime
            .cache_anchor()
            .filter(|a| a.mono >= turn_started_at)
        else {
            return;
        };
        if let Some(why) = self.cache_keepwarm_blocker() {
            self.cache_keepwarm.last_end = Some(format!("not started: {why}"));
            return;
        }
        if let Some(why) = self.runtime.cache_keepwarm_ineligibility() {
            self.cache_keepwarm.last_end = Some(format!("not started: {why}"));
            return;
        }
        if !crate::runtime::keepwarm::history_ready(&self.conv.api_messages) {
            return;
        }
        let fingerprint = match self
            .runtime
            .cache_keepwarm_probe(&self.conv.api_messages)
            .await
        {
            Ok(fp) => fp,
            Err(e) => {
                tracing::info!(session = %self.id, "cache keep-warm not started: {e}");
                self.cache_keepwarm.last_end = Some(format!("not started: {e}"));
                return;
            }
        };
        let ttl = self.runtime.cache_ttl();
        let cfg = PeriodConfig {
            mode,
            ttl,
            model: self.runtime.model().to_string(),
            interval: ping_interval(ttl),
        };
        self.cache_keepwarm.next_period_id += 1;
        let id = self.cache_keepwarm.next_period_id;
        let now = Instant::now();
        match Period::start(
            id,
            cfg,
            fingerprint,
            self.cache_keepwarm.last_request_tokens,
            anchor,
            now,
            SystemTime::now(),
        ) {
            Ok(p) => {
                tracing::info!(
                    session = %self.id,
                    period = id,
                    mode = %mode,
                    ttl_secs = governing_ttl(ttl).as_secs(),
                    first_ping_in_secs = p.next_at().saturating_duration_since(now).as_secs(),
                    prefix_tokens = p.prefix_tokens(),
                    bound_usd = p.bound_usd(),
                    "cache keep-warm period started"
                );
                self.cache_keepwarm.period = Some(p);
            }
            Err(reason) => {
                tracing::info!(session = %self.id, reason = %reason, "cache keep-warm not started");
                self.cache_keepwarm.last_end = Some(format!("not started: {reason}"));
            }
        }
    }

    /// `select!` arm: a ping is due.
    pub(crate) fn on_cache_keepwarm_due(&mut self) {
        if self.cache_keepwarm.period.is_none() {
            return;
        }
        if let Some(why) = self.cache_keepwarm_blocker() {
            self.cache_keepwarm_end(PeriodEnd::Blocked(why));
            return;
        }
        if !crate::runtime::keepwarm::history_ready(&self.conv.api_messages) {
            // /clear, /resume onto an unfinished session, an interrupted
            // edit: the prefix is not the one that is cached.
            self.cache_keepwarm_end(PeriodEnd::PrefixChanged);
            return;
        }
        let session_cost = self.conv.session_cost;
        let cap = self.config.max_session_cost;
        let Some(p) = self.cache_keepwarm.period.as_mut() else {
            return;
        };
        match p.fire(Instant::now(), SystemTime::now(), session_cost, cap) {
            Fire::NotYet => {}
            Fire::End(reason) => self.cache_keepwarm_end(reason),
            Fire::Ping => {
                let expect = p.fingerprint();
                // A detached clone sharing the anchor: the actor keeps serving
                // clients while the ping runs; a turn start aborts it.
                let history = self.conv.api_messages.clone();
                let mut runtime = self.runtime.clone();
                runtime.share_ttl_latches(&self.runtime);
                let cancel = CancellationToken::new();
                let token = cancel.clone();
                let handle = tokio::spawn(async move {
                    runtime.cache_keepwarm_ping(&history, expect, &token).await
                });
                self.cache_keepwarm.task = Some(PingTask { handle, cancel });
            }
        }
    }

    /// `select!` arm: the in-flight ping finished.
    pub(crate) fn on_cache_keepwarm_done(
        &mut self,
        result: std::result::Result<
            std::result::Result<PingOutcome, PingFailure>,
            tokio::task::JoinError,
        >,
    ) {
        self.cache_keepwarm.task = None;
        if self.cache_keepwarm.period.is_none() {
            return;
        }
        let outcome = match result {
            Ok(r) => r,
            Err(e) if e.is_cancelled() => Err(PingFailure::Canceled),
            Err(e) => Err(PingFailure::Fatal(format!("ping task: {e}"))),
        };
        match outcome {
            Ok(PingOutcome::Sent(usage)) => {
                let model = self.runtime.model().to_string();
                let cost = usage.cost_usd(&model);
                // Cost accounting (never history): into the session total,
                // and separately labelled.
                self.conv.session_cost += cost;
                self.conv.keepwarm_cost += cost;
                self.cache_keepwarm.session_pings += 1;
                let anchor = self.runtime.cache_anchor();
                let Some(p) = self.cache_keepwarm.period.as_mut() else {
                    return;
                };
                let result = p.ping_sent(&usage, cost, anchor);
                tracing::info!(
                    session = %self.id,
                    period = p.id,
                    ping = p.pings(),
                    age_secs = p.fired_age().as_secs(),
                    ttl_secs = governing_ttl(p.ttl()).as_secs(),
                    cache_read = usage.cache_read_tokens,
                    cache_write = usage.cache_write_tokens,
                    input = usage.input_tokens,
                    output = usage.output_tokens,
                    cost_usd = cost,
                    ledger_usd = p.ledger_usd(),
                    bound_usd = p.bound_usd(),
                    "cache keep-warm ping"
                );
                self.emit_conversation();
                if let Err(reason) = result {
                    self.cache_keepwarm_end(reason);
                    return;
                }
                if let Some(cap) = self.config.max_session_cost.filter(|c| c.is_finite()) {
                    if self.conv.session_cost >= cap {
                        self.cache_keepwarm_end(PeriodEnd::CostCap { cap });
                    }
                }
            }
            Ok(PingOutcome::PrefixChanged { .. }) => {
                self.cache_keepwarm_end(PeriodEnd::PrefixChanged);
            }
            Err(failure) => {
                let Some(p) = self.cache_keepwarm.period.as_mut() else {
                    return;
                };
                match p.ping_failed(&failure, Instant::now(), SystemTime::now()) {
                    Ok(delay) => tracing::info!(
                        session = %self.id,
                        period = p.id,
                        retry_in_secs = delay.as_secs(),
                        "cache keep-warm ping failed ({failure}); retrying"
                    ),
                    Err(reason) => self.cache_keepwarm_end(reason),
                }
            }
        }
    }

    /// Re-check the active period's fingerprint now (status, last detach).
    /// Ends the period if the prefix changed. Returns whether it matches
    /// (`None` = no period / could not build).
    pub(crate) async fn cache_keepwarm_recheck(&mut self) -> Option<bool> {
        let expect = self.cache_keepwarm.period.as_ref()?.fingerprint();
        if self.cache_keepwarm.period.as_ref()?.in_flight() {
            return Some(true);
        }
        let matches = crate::runtime::keepwarm::history_ready(&self.conv.api_messages)
            && self
                .runtime
                .cache_keepwarm_probe(&self.conv.api_messages)
                .await
                .is_ok_and(|fp| fp == expect);
        if !matches {
            self.cache_keepwarm_end(PeriodEnd::PrefixChanged);
        }
        Some(matches)
    }

    /// Structured keep-warm state (`/keepwarm status`, `Status` query).
    pub(crate) fn cache_keepwarm_status_json(
        &self,
        prefix_matches: Option<bool>,
    ) -> serde_json::Value {
        let kw = &self.cache_keepwarm;
        let now = Instant::now();
        let period = kw.period.as_ref().map(|p| {
            serde_json::json!({
                "id": p.id,
                "ttl_secs": governing_ttl(p.ttl()).as_secs(),
                "pings": p.pings(),
                "ledger_usd": p.ledger_usd(),
                "bound_usd": p.bound_usd(),
                "prefix_tokens": p.prefix_tokens(),
                "next_ping_in_secs": p.next_at().saturating_duration_since(now).as_secs(),
                "in_flight": p.in_flight(),
            })
        });
        serde_json::json!({
            "mode": kw.mode().to_string(),
            "active": kw.active(),
            "period": period,
            "prefix_matches": prefix_matches,
            "session_pings": kw.session_pings,
            "session_keepwarm_usd": self.conv.keepwarm_cost,
            "last_end": kw.last_end,
        })
    }

    /// `/keepwarm [status|off|auto|<N>h]` (actor-owned, like `/saveas`).
    pub(crate) async fn cache_keepwarm_command(&mut self, arg: &str) -> serde_json::Value {
        let arg = arg.trim();
        if !(arg.is_empty() || arg.eq_ignore_ascii_case("status")) {
            let Some(mode) = CacheKeepWarm::parse(arg) else {
                return serde_json::json!({
                    "kind": "error",
                    "text": format!(
                        "usage: /keepwarm [status|off|auto|<N>h] (N = 1–{}; unknown: {arg})",
                        crate::core::config::MAX_CACHE_KEEPWARM_HOURS
                    ),
                });
            };
            self.cache_keepwarm.override_mode = Some(mode);
            if mode.is_off() {
                self.cache_keepwarm_end(PeriodEnd::Disabled);
            } else if let Some(p) = self.cache_keepwarm.period.as_mut() {
                p.set_mode(mode);
            }
        }
        let prefix_matches = self.cache_keepwarm_recheck().await;
        let status = self.cache_keepwarm_status_json(prefix_matches);
        serde_json::json!({
            "kind": "output",
            "text": render_status(&status),
            "keepwarm": status,
        })
    }
}

/// One human line (plus detail) for `/keepwarm status`.
pub(crate) fn render_status(s: &serde_json::Value) -> String {
    let mode = s["mode"].as_str().unwrap_or("off");
    let spent = s["session_keepwarm_usd"].as_f64().unwrap_or(0.0);
    let pings = s["session_pings"].as_u64().unwrap_or(0);
    let mut out = format!("cache keep-warm: {mode} — this session: {pings} pings, ${spent:.4}");
    let p = &s["period"];
    if p.is_object() {
        out.push_str(&format!(
            "\nactive: ping {} due in {}s (TTL {}s); ${:.4} spent of ${:.4} bound; prefix {}",
            p["pings"].as_u64().unwrap_or(0) + 1,
            p["next_ping_in_secs"].as_u64().unwrap_or(0),
            p["ttl_secs"].as_u64().unwrap_or(0),
            p["ledger_usd"].as_f64().unwrap_or(0.0),
            p["bound_usd"].as_f64().unwrap_or(0.0),
            match s["prefix_matches"].as_bool() {
                Some(true) => "unchanged",
                Some(false) => "changed",
                None => "unchecked",
            },
        ));
    } else {
        out.push_str("\nidle period: none");
    }
    if let Some(last) = s["last_end"].as_str() {
        out.push_str(&format!("\nlast: {last}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: Duration = Duration::from_secs(60);
    const OPUS: &str = "claude-opus-4-7";
    const SONNET: &str = "claude-sonnet-4-6";
    const P: u64 = 300_000;

    fn cfg(mode: CacheKeepWarm, ttl: CacheTtl, model: &str) -> PeriodConfig {
        PeriodConfig {
            mode,
            ttl,
            model: model.to_string(),
            interval: governing_ttl(ttl) - safety_margin(ttl),
        }
    }

    fn anchor_at(mono: Instant, wall: SystemTime) -> CacheAnchor {
        CacheAnchor { mono, wall }
    }

    /// A ping that read the whole prefix: `extra_in` uncached input (the
    /// trailing ping message) and `out` output tokens (thinking included).
    fn read_ping(model: &str, read: u64, write: u64, extra_in: u64, out: u64) -> KeepWarmUsage {
        KeepWarmUsage {
            input_tokens: extra_in,
            output_tokens: out,
            cache_read_tokens: read,
            cache_write_tokens: write,
            cache_write_5m: None,
            cache_write_1h: None,
            model: Some(model.to_string()),
        }
    }

    /// Drive a period with the given ping usages (last one repeats) until it
    /// ends. Every ping is sent exactly when due and moves the anchor.
    /// Returns (pings, ledger, bound, reason, elapsed since idle).
    fn run(cfg: PeriodConfig, usages: &[KeepWarmUsage]) -> (u32, f64, f64, PeriodEnd, Duration) {
        let t0 = Instant::now();
        let w0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let model = cfg.model.clone();
        let mut p = Period::start(1, cfg, 7, P, anchor_at(t0, w0), t0, w0).expect("starts");
        let mut i = 0;
        loop {
            let at = p.next_at();
            let wall = w0 + at.saturating_duration_since(t0);
            match p.fire(at, wall, 0.0, None) {
                Fire::Ping => {}
                Fire::End(r) => {
                    return (p.pings(), p.ledger_usd(), p.bound_usd(), r, at - t0);
                }
                Fire::NotYet => panic!("due but not fired"),
            }
            let u = &usages[i.min(usages.len() - 1)];
            i += 1;
            let cost = u.cost_usd(&model);
            if let Err(r) = p.ping_sent(u, cost, Some(anchor_at(at, wall))) {
                return (p.pings(), p.ledger_usd(), p.bound_usd(), r, at - t0);
            }
            assert!(i < 1000, "runaway");
        }
    }

    // ── ski-rental bound, both TTLs, measured costs ──

    #[test]
    fn ski_rental_1h_is_twenty_pure_reads_over_about_18h() {
        // A pure read of a 300k prefix: $0.15 on Opus; rewrite at 1h = $3.00.
        let (pings, ledger, bound, reason, elapsed) = run(
            cfg(CacheKeepWarm::Auto, CacheTtl::OneHour, OPUS),
            &[read_ping(OPUS, P, 0, 0, 0)],
        );
        assert!((bound - 3.0).abs() < 1e-9, "{bound}");
        assert_eq!(pings, 20, "2.0× write / 0.1× read");
        assert!(ledger <= bound * (1.0 + 1e-9));
        assert!(matches!(reason, PeriodEnd::BoundReached { .. }), "{reason}");
        // 20 pings, 55 min apart, the first 55 min after the last request;
        // the period ends right after ping 20 (~18.3 h), and the cache it
        // refreshed stays warm for one more hour.
        assert_eq!(elapsed, 20 * 55 * MIN);
    }

    #[test]
    fn ski_rental_5m_is_twelve_pure_reads_over_about_55_min() {
        // Rewrite at 5m = 300k × $6.25/MTok = $1.875; 12 reads = $1.80.
        let (pings, ledger, bound, reason, elapsed) = run(
            cfg(CacheKeepWarm::Auto, CacheTtl::FiveMinutes, OPUS),
            &[read_ping(OPUS, P, 0, 0, 0)],
        );
        assert!((bound - 1.875).abs() < 1e-9, "{bound}");
        assert_eq!(pings, 12, "1.25× write / 0.1× read");
        assert!((ledger - 1.8).abs() < 1e-9);
        assert!(matches!(reason, PeriodEnd::BoundReached { .. }), "{reason}");
        // Ping 12 at 54 min; the cache stays warm until ~59 min.
        assert_eq!(elapsed, 12 * Duration::from_secs(270));
    }

    #[test]
    fn measured_overhead_and_first_write_are_in_the_ledger() {
        // First ping writes the last reply (2k tokens at 1h price), every
        // ping carries 40 uncached input tokens and 1 output token.
        let first = read_ping(OPUS, P - 2_000, 2_000, 40, 1);
        let steady = read_ping(OPUS, P, 0, 40, 1);
        let (pings, ledger, bound, reason, _) = run(
            cfg(CacheKeepWarm::Auto, CacheTtl::OneHour, OPUS),
            &[first.clone(), steady.clone()],
        );
        assert!(matches!(reason, PeriodEnd::BoundReached { .. }));
        let expected_ledger = first.cost_usd(OPUS) + f64::from(pings - 1) * steady.cost_usd(OPUS);
        assert!(
            (ledger - expected_ledger).abs() < 1e-9,
            "ledger is the measured sum"
        );
        assert!(ledger <= bound * (1.0 + 1e-9), "never past the bound");
        assert!(
            ledger + steady.cost_usd(OPUS) > bound,
            "stops only when the next would cross"
        );
        assert_eq!(pings, 19, "overhead costs one ping out of twenty");
    }

    #[test]
    fn an_expensive_ping_stops_the_period_early() {
        // Budgeted thinking that actually thought: 5k output tokens on
        // Sonnet 4.6 = $0.075 per ping on top of a $0.09 read.
        let thinking = read_ping(SONNET, P, 0, 40, 5_000);
        let cheap = read_ping(SONNET, P, 0, 40, 30);
        let (heavy, ..) = run(
            cfg(CacheKeepWarm::Auto, CacheTtl::OneHour, SONNET),
            &[thinking],
        );
        let (light, ..) = run(
            cfg(CacheKeepWarm::Auto, CacheTtl::OneHour, SONNET),
            &[cheap],
        );
        assert!(heavy < light, "{heavy} vs {light}");
        assert_eq!(heavy, 10, "rewrite $1.80 / ($0.09 read + $0.075 thinking)");
        assert_eq!(light, 19);
    }

    #[test]
    fn budgeted_thinking_output_is_accounted() {
        let u = read_ping(SONNET, P, 0, 40, 300);
        let read_only = read_ping(SONNET, P, 0, 0, 0);
        let thinking_usd = u.cost_usd(SONNET) - read_only.cost_usd(SONNET);
        // 300 thinking/output tokens at $15/MTok + 40 input at $3/MTok.
        assert!((thinking_usd - (300.0 * 15.0 + 40.0 * 3.0) / 1e6).abs() < 1e-12);
        let (pings, ledger, ..) = run(
            cfg(CacheKeepWarm::Auto, CacheTtl::FiveMinutes, SONNET),
            &[u.clone()],
        );
        assert!((ledger - f64::from(pings) * u.cost_usd(SONNET)).abs() < 1e-9);
    }

    // ── scheduling: 5m, hybrid, anchor ──

    #[test]
    fn five_minute_pings_are_scheduled_from_the_anchor() {
        let t0 = Instant::now();
        let w0 = SystemTime::UNIX_EPOCH + Duration::from_secs(5_000);
        let c = cfg(CacheKeepWarm::Auto, CacheTtl::FiveMinutes, OPUS);
        assert_eq!(c.interval, Duration::from_secs(270));
        // The turn's last request was accepted at t0; idle a minute later.
        let idle = t0 + MIN;
        let mut p = Period::start(1, c, 7, P, anchor_at(t0, w0), idle, w0 + MIN).unwrap();
        assert_eq!(p.deadline(), Some(t0 + Duration::from_secs(270)));
        assert_eq!(
            p.fire(t0 + Duration::from_secs(200), w0, 0.0, None),
            Fire::NotYet
        );
        let at = t0 + Duration::from_secs(270);
        let wat = w0 + Duration::from_secs(270);
        assert_eq!(p.fire(at, wat, 0.0, None), Fire::Ping);
        assert_eq!(p.deadline(), None, "no second ping while one is in flight");
        assert_eq!(p.fire(at, wat, 0.0, None), Fire::NotYet);
        // The ping actually reached the provider 2s after firing.
        let sent = anchor_at(at + Duration::from_secs(2), wat + Duration::from_secs(2));
        p.ping_sent(&read_ping(OPUS, P, 0, 0, 1), 0.15, Some(sent))
            .unwrap();
        assert_eq!(p.deadline(), Some(sent.mono + Duration::from_secs(270)));
    }

    #[test]
    fn hybrid_is_governed_by_the_5m_history_marker() {
        assert_eq!(governing_ttl(CacheTtl::Hybrid), Duration::from_secs(300));
        assert_eq!(governing_ttl(CacheTtl::OneHour), Duration::from_secs(3600));
        assert_eq!(
            governing_ttl(CacheTtl::FiveMinutes),
            Duration::from_secs(300)
        );
        let c = cfg(CacheKeepWarm::Auto, CacheTtl::Hybrid, OPUS);
        assert_eq!(c.interval, Duration::from_secs(270));
        // Priced at the 5m write rate (the entry that lapses), not 1h.
        assert!((rewrite_cost_usd(OPUS, CacheTtl::Hybrid, P) - 1.875).abs() < 1e-9);
        let (pings, ..) = run(c, &[read_ping(OPUS, P, 0, 0, 0)]);
        assert_eq!(pings, 12);
    }

    #[test]
    fn a_turn_that_streamed_past_the_interval_pings_at_once_or_not_at_all() {
        let t0 = Instant::now();
        let w0 = SystemTime::UNIX_EPOCH + Duration::from_secs(9_000);
        let c = || cfg(CacheKeepWarm::Auto, CacheTtl::FiveMinutes, OPUS);
        let idle = t0 + Duration::from_secs(280);
        let p = Period::start(
            1,
            c(),
            7,
            P,
            anchor_at(t0, w0),
            idle,
            w0 + Duration::from_secs(280),
        )
        .unwrap();
        assert_eq!(p.deadline(), Some(idle), "due immediately");
        let late = t0 + Duration::from_secs(290);
        let err = Period::start(
            1,
            c(),
            7,
            P,
            anchor_at(t0, w0),
            late,
            w0 + Duration::from_secs(290),
        )
        .unwrap_err();
        assert!(matches!(err, PeriodEnd::Expired { age_secs: 290 }), "{err}");
    }

    // ── robustness ──

    #[test]
    fn sleep_past_expiry_ends_the_period_without_pinging() {
        let t0 = Instant::now();
        let w0 = SystemTime::UNIX_EPOCH + Duration::from_secs(50_000);
        for ttl in [CacheTtl::FiveMinutes, CacheTtl::OneHour] {
            let c = cfg(CacheKeepWarm::Auto, ttl, OPUS);
            let due = t0 + c.interval;
            let mut p = Period::start(1, c, 7, P, anchor_at(t0, w0), t0, w0).unwrap();
            // Monotonic time says "on schedule"; the wall clock says the
            // laptop was closed for 3 hours.
            let wall = w0 + Duration::from_secs(3 * 3600);
            match p.fire(due, wall, 0.0, None) {
                Fire::End(PeriodEnd::Expired { age_secs }) => assert_eq!(age_secs, 3 * 3600),
                other => panic!("{ttl:?}: {other:?}"),
            }
        }
    }

    #[test]
    fn transient_errors_back_off_within_the_ttl_then_end() {
        let t0 = Instant::now();
        let w0 = SystemTime::UNIX_EPOCH + Duration::from_secs(70_000);
        let c = cfg(CacheKeepWarm::Auto, CacheTtl::OneHour, OPUS);
        let mut p = Period::start(1, c, 7, P, anchor_at(t0, w0), t0, w0).unwrap();
        let mut now = p.next_at();
        let transient = PingFailure::Transient("HTTP 529".into());
        for want in BACKOFF {
            let wall = w0 + now.saturating_duration_since(t0);
            assert_eq!(p.fire(now, wall, 0.0, None), Fire::Ping);
            assert_eq!(p.ping_failed(&transient, now, wall), Ok(want));
            assert_eq!(p.deadline(), Some(now + want));
            now += want;
        }
        let wall = w0 + now.saturating_duration_since(t0);
        assert_eq!(p.fire(now, wall, 0.0, None), Fire::Ping);
        assert!(matches!(
            p.ping_failed(&transient, now, wall),
            Err(PeriodEnd::TransientExhausted(_))
        ));

        // 5m: at 270s only one 5s retry fits before the late guard.
        let c = cfg(CacheKeepWarm::Auto, CacheTtl::FiveMinutes, OPUS);
        let mut p = Period::start(2, c, 7, P, anchor_at(t0, w0), t0, w0).unwrap();
        let at = p.next_at();
        let wat = w0 + Duration::from_secs(270);
        assert_eq!(p.fire(at, wat, 0.0, None), Fire::Ping);
        assert_eq!(p.ping_failed(&transient, at, wat), Ok(BACKOFF[0]));
        let at = at + BACKOFF[0];
        let wat = wat + BACKOFF[0];
        assert_eq!(p.fire(at, wat, 0.0, None), Fire::Ping);
        assert!(matches!(
            p.ping_failed(&transient, at, wat),
            Err(PeriodEnd::TransientExhausted(_))
        ));
    }

    #[test]
    fn a_4xx_ends_the_period() {
        let t0 = Instant::now();
        let w0 = SystemTime::UNIX_EPOCH + Duration::from_secs(80_000);
        let c = cfg(CacheKeepWarm::Auto, CacheTtl::OneHour, OPUS);
        let mut p = Period::start(1, c, 7, P, anchor_at(t0, w0), t0, w0).unwrap();
        let at = p.next_at();
        let wat = w0 + (at - t0);
        assert_eq!(p.fire(at, wat, 0.0, None), Fire::Ping);
        let err = p
            .ping_failed(&PingFailure::Fatal("HTTP 400".into()), at, wat)
            .unwrap_err();
        assert_eq!(err, PeriodEnd::Rejected("HTTP 400".into()));
    }

    #[test]
    fn a_ping_that_writes_instead_of_reading_ends_the_period() {
        let t0 = Instant::now();
        let w0 = SystemTime::UNIX_EPOCH + Duration::from_secs(90_000);
        let c = cfg(CacheKeepWarm::Auto, CacheTtl::FiveMinutes, OPUS);
        let mut p = Period::start(1, c, 7, P, anchor_at(t0, w0), t0, w0).unwrap();
        let at = p.next_at();
        assert_eq!(p.fire(at, w0 + (at - t0), 0.0, None), Fire::Ping);
        let miss = read_ping(OPUS, 0, P, 40, 1);
        assert_eq!(
            p.ping_sent(&miss, miss.cost_usd(OPUS), None),
            Err(PeriodEnd::CacheMiss { written: P })
        );
    }

    #[test]
    fn hours_window_and_bound_whichever_first() {
        // 1h TTL, 1h window: one ping at 55 min, the next would be at 110.
        let (pings, _, _, reason, _) = run(
            cfg(CacheKeepWarm::Hours(1), CacheTtl::OneHour, OPUS),
            &[read_ping(OPUS, P, 0, 0, 0)],
        );
        assert_eq!((pings, reason), (1, PeriodEnd::HoursReached(1)));
        // 5m TTL, 2h window: the bound (12 pings, ~58 min) comes first.
        let (pings, _, _, reason, _) = run(
            cfg(CacheKeepWarm::Hours(2), CacheTtl::FiveMinutes, OPUS),
            &[read_ping(OPUS, P, 0, 0, 0)],
        );
        assert_eq!(pings, 12);
        assert!(matches!(reason, PeriodEnd::BoundReached { .. }));
        // A window shorter than the first interval never starts.
        let t0 = Instant::now();
        let w0 = SystemTime::UNIX_EPOCH;
        let mut c = cfg(CacheKeepWarm::Hours(1), CacheTtl::OneHour, OPUS);
        c.interval = Duration::from_secs(2 * 3600);
        assert_eq!(
            Period::start(1, c, 7, P, anchor_at(t0, w0), t0, w0).unwrap_err(),
            PeriodEnd::HoursReached(1)
        );
    }

    #[test]
    fn max_session_cost_is_respected() {
        let t0 = Instant::now();
        let w0 = SystemTime::UNIX_EPOCH + Duration::from_secs(100_000);
        let c = cfg(CacheKeepWarm::Auto, CacheTtl::OneHour, OPUS);
        let mut p = Period::start(1, c, 7, P, anchor_at(t0, w0), t0, w0).unwrap();
        let at = p.next_at();
        let wat = w0 + (at - t0);
        // $4.90 spent, cap $5.00: the next $0.15 read would cross it.
        assert_eq!(
            p.fire(at, wat, 4.90, Some(5.0)),
            Fire::End(PeriodEnd::CostCap { cap: 5.0 })
        );
        assert_eq!(p.fire(at, wat, 1.0, Some(5.0)), Fire::Ping);
    }

    #[test]
    fn nothing_cached_never_starts() {
        let t0 = Instant::now();
        let w0 = SystemTime::UNIX_EPOCH;
        let c = cfg(CacheKeepWarm::Auto, CacheTtl::OneHour, OPUS);
        assert_eq!(
            Period::start(1, c, 7, 0, anchor_at(t0, w0), t0, w0).unwrap_err(),
            PeriodEnd::NothingCached
        );
    }

    #[test]
    fn status_renders_spend_and_period() {
        let s = serde_json::json!({
            "mode": "auto",
            "active": true,
            "period": {"id": 3, "ttl_secs": 300, "pings": 2, "ledger_usd": 0.3,
                       "bound_usd": 1.875, "prefix_tokens": P, "next_ping_in_secs": 120,
                       "in_flight": false},
            "prefix_matches": true,
            "session_pings": 5,
            "session_keepwarm_usd": 0.75,
            "last_end": "prefix changed",
        });
        let text = render_status(&s);
        assert!(text.contains("auto"), "{text}");
        assert!(text.contains("5 pings, $0.7500"), "{text}");
        assert!(text.contains("ping 3 due in 120s"), "{text}");
        assert!(text.contains("$0.3000 spent of $1.8750 bound"), "{text}");
        assert!(text.contains("prefix unchanged"), "{text}");
        assert!(text.contains("last: prefix changed"), "{text}");
    }
}
