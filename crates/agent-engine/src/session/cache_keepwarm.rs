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
//! accepted AND cached something (`cache_read + cache_write > 0`) ends with a
//! finished assistant reply, `cache_keepwarm` is not `off`, and the next turn
//! would not roll the context over first. It holds:
//!
//! - **the anchor**: send time of the last accepted request (monotonic +
//!   wall clock). The provider's TTL runs from the start of that request.
//! - **the honoured TTL**: what the server actually keeps, not what is
//!   configured. `1h` only with evidence (a 1h write in the last request or
//!   earlier in the session, no silent-downgrade latch); otherwise `5m`.
//!   `hybrid` is governed by its 5m history marker. Pings are due at
//!   `anchor + TTL − margin` (55 min / 4.5 min), on whichever of the
//!   monotonic and wall clocks comes first (the monotonic clock stops while
//!   the machine sleeps; the provider's TTL does not).
//! - **the prefix fingerprint** recorded at idle. Every ping re-builds the
//!   prefix and is refused (period ends) if it no longer matches.
//! - **a ski-rental ledger** of measured spend. A ping is only sent while
//!   `ledger + worst-case ping ≤ bound`, where the bound is re-writing the
//!   prefix at the honoured TTL's write price, and the worst case is a full
//!   read of the prefix + the known write (ping 1: the last turn's uncached
//!   tail and reply) + uncached input + `max_tokens` of output. Under
//!   budgeted thinking `max_tokens` is `budget + 1`, so such pings only run
//!   for prefixes large enough that one worst-case ping fits the bound.
//! - **the cap exposure**: `max_session_cost` is a hard cap, so admission
//!   prices what a dispatched ping can actually bill — the larger of that
//!   read-path worst case and a full miss (the whole prefix written at the
//!   1h price if the request carries 1h markers, else the honoured TTL's,
//!   plus the same uncached input and `max_tokens` of output). The
//!   ski-rental bound stays on the read path: a miss *is* the rewrite and
//!   ends the period when it happens.
//!
//! The period ends on the first of: bound, `<N>h` window, prefix change,
//! context rollover ahead, a ping that cached nothing / missed / rewrote a
//! large share of the prefix / paid for a large uncached share, expiry, a
//! 4xx, exhausted transient retries, `max_session_cost`, a turn or
//! compaction starting, an explicit park, `/keepwarm off`, or the session
//! ending. While a period is active the session does not auto-park.

use std::time::{Duration, SystemTime};

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::actor::SessionActor;
use super::types::{AttachMode, ClientKind};
use crate::core::config::{CacheKeepWarm, CacheTtl};
use crate::runtime::keepwarm::{
    CacheAnchor, KeepWarmProbe, KeepWarmUsage, PingBill, PingFailure, PingOutcome, PingReport,
    PingTap,
};

/// No ping may start once the cache is this close to expiry: the request
/// still has to reach the provider (connect, upload a large body).
pub const LATE_GUARD: Duration = Duration::from_secs(15);

/// Delays before retrying a transient ping failure; then the period ends.
pub const BACKOFF: [Duration; 3] = [
    Duration::from_secs(5),
    Duration::from_secs(15),
    Duration::from_secs(30),
];

/// While a period is active the actor re-reads the wall clock at least this
/// often: after a suspend the monotonic deadline is late by the time slept.
pub const WALL_POLL: Duration = Duration::from_secs(15);

/// A ping whose uncached input, or (after ping 1) whose cache write, exceeds
/// this share of the prefix ends the period: it is paying for uncached
/// tokens or rewriting history, not refreshing a cache. 10%: the trailing
/// ping message is ~40 tokens, under 4% of the smallest cacheable prefix
/// (1,024 tokens), so a healthy ping never comes close.
pub const SHARE_LIMIT_PERCENT: u64 = 10;

/// Estimated uncached input of a ping before one has been measured: the
/// trailing ping message plus framing.
pub const PING_INPUT_ESTIMATE: u64 = 64;

/// Relative slack for the bound comparison (f64 sums of equal ping costs).
const BOUND_EPSILON: f64 = 1e-9;

fn over_share(tokens: u64, prefix: u64) -> bool {
    tokens.saturating_mul(100) > prefix.saturating_mul(SHARE_LIMIT_PERCENT)
}

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

/// Ping interval for `ttl`, measured from the anchor: TTL − margin.
pub fn ping_interval(ttl: CacheTtl) -> Duration {
    governing_ttl(ttl) - safety_margin(ttl)
}

/// The ping interval override, in ms (0 = none). Test-only: written solely
/// by [`testing::IntervalOverride`], which no binary calls.
#[cfg(any(test, feature = "testing"))]
static INTERVAL_OVERRIDE_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Test seam replacing `ping_interval(honoured TTL)`.
///
/// Not an environment variable on purpose: `cargo test` feature-unifies
/// `agent-engine/testing` into the dev build of the `synaps` binary, so an
/// env-driven override would be live in a runnable binary. This one is set
/// only by code ([`testing::IntervalOverride`]), compiled only under
/// `cfg(test)` / the `testing` feature, and no binary target calls it: no
/// binary input (env, config, argv) can reach it. In a build without the
/// feature it is a constant `None`.
pub fn interval_override() -> Option<Duration> {
    #[cfg(any(test, feature = "testing"))]
    {
        let ms = INTERVAL_OVERRIDE_MS.load(std::sync::atomic::Ordering::Relaxed);
        if ms > 0 {
            return Some(Duration::from_millis(ms));
        }
    }
    None
}

/// Test-only controls (`cfg(test)` / `testing` feature).
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    /// Shortens the keep-warm ping interval for every session in this
    /// process while alive (tests that use it must be `#[serial]`).
    #[must_use = "the override is cleared when the guard drops"]
    pub struct IntervalOverride(());

    impl IntervalOverride {
        pub fn set(interval: Duration) -> Self {
            let ms = u64::try_from(interval.as_millis())
                .unwrap_or(u64::MAX)
                .max(1);
            super::INTERVAL_OVERRIDE_MS.store(ms, Ordering::Relaxed);
            Self(())
        }
    }

    impl Drop for IntervalOverride {
        fn drop(&mut self) {
            super::INTERVAL_OVERRIDE_MS.store(0, Ordering::Relaxed);
        }
    }

    static DUE_CHECKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    /// How many times an actor in this process looked at a possibly due
    /// ping (`on_cache_keepwarm_due`). A timer left armed with a past
    /// deadline shows here as a fast-growing count.
    pub fn due_checks() -> u64 {
        DUE_CHECKS.load(Ordering::SeqCst)
    }

    pub(super) fn note_due_check() {
        DUE_CHECKS.fetch_add(1, Ordering::SeqCst);
    }
}

/// Evidence about the TTL the server honours for this session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TtlEvidence {
    /// The session's silent-downgrade latch fired (1h requested, 5m kept).
    pub downgraded: bool,
    /// A request of this session wrote a 1h entry.
    pub saw_1h: bool,
    /// The last request's 1h / 5m write buckets.
    pub write_1h: u64,
    pub write_5m: u64,
}

/// The TTL the server actually honours for the history entry. `1h` needs
/// positive evidence; with none (or a downgrade) a `1h` configuration is
/// scheduled — and its rewrite priced — as `5m`.
pub fn honoured_ttl(configured: CacheTtl, e: TtlEvidence) -> CacheTtl {
    if configured != CacheTtl::OneHour {
        return configured;
    }
    if e.downgraded {
        return CacheTtl::FiveMinutes;
    }
    if e.write_1h > 0 {
        return CacheTtl::OneHour;
    }
    if e.write_5m > 0 {
        return CacheTtl::FiveMinutes;
    }
    if e.saw_1h {
        CacheTtl::OneHour
    } else {
        CacheTtl::FiveMinutes
    }
}

/// Cost of one request: `read` cache reads, `write` cache writes at `ttl`'s
/// write price, `input` uncached input, `output` output tokens.
pub fn request_cost_usd(
    model: &str,
    ttl: CacheTtl,
    read: u64,
    write: u64,
    input: u64,
    output: u64,
) -> f64 {
    match ttl {
        CacheTtl::OneHour => {
            crate::pricing::calculate_cost_split(model, input, output, read, 0, write)
        }
        CacheTtl::FiveMinutes | CacheTtl::Hybrid => {
            crate::pricing::calculate_cost_split(model, input, output, read, write, 0)
        }
    }
}

/// Cost of re-writing `prefix_tokens` at the TTL's write price: what the
/// next turn pays if the cache lapses — the ski-rental "buy" price.
pub fn rewrite_cost_usd(model: &str, ttl: CacheTtl, prefix_tokens: u64) -> f64 {
    request_cost_usd(model, ttl, 0, prefix_tokens, 0, 0)
}

/// Why a period ended (or never started). Logged once, shown by
/// `/keepwarm status`.
#[derive(Debug, Clone, PartialEq)]
pub enum PeriodEnd {
    /// The last request (or a ping) cached nothing: the prefix is below the
    /// model's cache minimum, or caching is off for it.
    NothingCached,
    /// A ping paid for a large uncached share of the prefix.
    UncachedInput {
        input: u64,
        prefix: u64,
    },
    /// Ski-rental bound: the next worst-case ping would push the ledger past
    /// the rewrite it prevents.
    BoundReached {
        ledger_usd: f64,
        bound_usd: f64,
    },
    /// Not even one worst-case ping fits the bound (budgeted thinking on a
    /// small prefix).
    PingTooExpensive {
        worst_usd: f64,
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
    /// The next turn would roll the context over / hard-stop before its
    /// first request: what is cached is not what it sends.
    ContextRollover(String),
    /// A ping wrote the prefix instead of reading it.
    CacheMiss {
        written: u64,
    },
    /// A ping rewrote a large share of the prefix (partial miss).
    PartialMiss {
        written: u64,
        prefix: u64,
    },
    /// 4xx / auth / configuration: never retried.
    Rejected(String),
    /// Transient errors outlasted the backoff (or the TTL).
    TransientExhausted(String),
    /// `max_session_cost` reached (or the next ping would cross it if it
    /// missed the cache and rewrote the whole prefix).
    CostCap {
        cap: f64,
    },
    TurnStarted,
    Compaction,
    /// `/keepwarm off`.
    Disabled,
    /// An explicit park request (not the idle auto-park, which waits).
    ParkedByRequest,
    SessionEnding,
    /// Something else owns the session (driver, queued work, prompt).
    Blocked(&'static str),
}

impl std::fmt::Display for PeriodEnd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PeriodEnd::NothingCached => f.write_str("nothing cached"),
            PeriodEnd::UncachedInput { input, prefix } => write!(
                f,
                "ping paid for {input} uncached input tokens (prefix {prefix})"
            ),
            PeriodEnd::BoundReached {
                ledger_usd,
                bound_usd,
            } => write!(
                f,
                "ski-rental bound reached (${ledger_usd:.4} spent of ${bound_usd:.4})"
            ),
            PeriodEnd::PingTooExpensive {
                worst_usd,
                bound_usd,
            } => write!(
                f,
                "worst-case cache-hit ping ${worst_usd:.4} exceeds the ${bound_usd:.4} rewrite it would save"
            ),
            PeriodEnd::HoursReached(h) => write!(f, "{h}h window reached"),
            PeriodEnd::Expired { age_secs } => write!(
                f,
                "cache expired or about to ({age_secs}s since last request; slept?)"
            ),
            PeriodEnd::PrefixChanged => f.write_str("prefix changed"),
            PeriodEnd::ContextRollover(why) => write!(f, "{why}"),
            PeriodEnd::CacheMiss { written } => {
                write!(f, "ping missed the cache (wrote {written} tokens)")
            }
            PeriodEnd::PartialMiss { written, prefix } => write!(
                f,
                "ping rewrote {written} of {prefix} prefix tokens (history no longer cached)"
            ),
            PeriodEnd::Rejected(m) => write!(f, "ping rejected: {m}"),
            PeriodEnd::TransientExhausted(m) => write!(f, "transient errors: {m}"),
            PeriodEnd::CostCap { cap } => write!(f, "max_session_cost ${cap:.4} reached"),
            PeriodEnd::TurnStarted => f.write_str("turn started"),
            PeriodEnd::Compaction => f.write_str("compaction"),
            PeriodEnd::Disabled => f.write_str("disabled"),
            PeriodEnd::ParkedByRequest => f.write_str("parked by request"),
            PeriodEnd::SessionEnding => f.write_str("session ending"),
            PeriodEnd::Blocked(why) => write!(f, "{why}"),
        }
    }
}

/// What the period needs to know about the session at start.
#[derive(Debug, Clone)]
pub struct PeriodConfig {
    pub mode: CacheKeepWarm,
    /// The configured `cache_ttl` (the honoured one is derived from it).
    pub ttl: CacheTtl,
    pub model: String,
    /// The ping's `max_tokens` (worst-case output).
    pub max_tokens: u64,
    /// Replaces `ping_interval(honoured TTL)` (test seam only).
    pub interval_override: Option<Duration>,
    /// Longest an `auto` period may run, in hours
    /// (`cache_keepwarm_auto_max_hours`; 0 = no cap). `<N>h` ignores it.
    pub auto_max_hours: u32,
    /// Tokens of the last reply that the cached prefix will actually hold
    /// (thinking stripped on resend, see
    /// `runtime::keepwarm::reply_prefix_tokens`). `None`: the reply's
    /// billed `output_tokens`.
    pub reply_tokens: Option<u64>,
}

/// What a cache epoch carries from one period to the next. An epoch is the
/// life of one real request's cache entry: only a new real request (new
/// usage, new anchor) starts a fresh one. Ending a period (e.g. `/keepwarm
/// off`) and starting another without a real turn in between CONTINUES the
/// epoch: same ledger, same idle start, ping 1's write allowance not
/// re-granted.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EpochCarry {
    pub ledger_usd: f64,
    pub pings: u32,
    pub prefix_tokens: u64,
    pub ping_input: u64,
    pub idle_since: Instant,
    pub idle_wall: SystemTime,
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
    /// The TTL the server honours (see [`honoured_ttl`]).
    ttl: CacheTtl,
    evidence: TtlEvidence,
    idle_since: Instant,
    idle_wall: SystemTime,
    anchor: CacheAnchor,
    fingerprint: u64,
    prefix_tokens: u64,
    /// Ping 1 legitimately writes the last turn's uncached tail and reply.
    first_write: u64,
    bound_usd: f64,
    ledger_usd: f64,
    /// Read-path worst case of the next ping (cache hit, `max_tokens` of
    /// output): the ski-rental basis.
    worst_next_usd: f64,
    /// What the next ping can bill if dispatched, a full cache miss
    /// included: the `max_session_cost` admission basis.
    exposure_next_usd: f64,
    /// Measured uncached input of the last ping.
    ping_input: u64,
    pings: u32,
    retries: usize,
    next_at: Instant,
    next_wall: SystemTime,
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
    /// Start a period at `now` (the session just went idle). `last` is the
    /// usage of the turn's last request; `evidence` what the session knows
    /// about the TTL the server honours.
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        id: u64,
        cfg: PeriodConfig,
        fingerprint: u64,
        last: &KeepWarmUsage,
        evidence: TtlEvidence,
        anchor: CacheAnchor,
        now: Instant,
        wall: SystemTime,
    ) -> Result<Period, PeriodEnd> {
        Self::start_in_epoch(
            id,
            cfg,
            fingerprint,
            last,
            evidence,
            anchor,
            now,
            wall,
            None,
        )
    }

    /// [`start`](Self::start), continuing `carry` when the cache epoch has
    /// not changed since an earlier period (see [`EpochCarry`]).
    #[allow(clippy::too_many_arguments)]
    pub fn start_in_epoch(
        id: u64,
        cfg: PeriodConfig,
        fingerprint: u64,
        last: &KeepWarmUsage,
        evidence: TtlEvidence,
        anchor: CacheAnchor,
        now: Instant,
        wall: SystemTime,
        carry: Option<&EpochCarry>,
    ) -> Result<Period, PeriodEnd> {
        let cached = last.cache_read_tokens + last.cache_write_tokens;
        if cached == 0 {
            return Err(PeriodEnd::NothingCached);
        }
        let ttl = honoured_ttl(cfg.ttl, evidence);
        let age_now = age(&anchor, now, wall);
        if age_now + LATE_GUARD >= governing_ttl(ttl) {
            return Err(PeriodEnd::Expired {
                age_secs: age_now.as_secs(),
            });
        }
        // The next turn re-sends what is cached plus the last request's
        // uncached tail and its reply; ping 1 caches those. Only what the
        // prefix will hold counts: output the API strips on resend
        // (thinking) is billed once and never cached.
        let reply = cfg
            .reply_tokens
            .map_or(last.output_tokens, |r| r.min(last.output_tokens));
        let first_write = last.input_tokens + reply;
        let prefix_tokens = cached + first_write;
        let mut p = Period {
            id,
            cfg,
            ttl,
            evidence,
            idle_since: now,
            idle_wall: wall,
            anchor,
            fingerprint,
            prefix_tokens,
            first_write,
            bound_usd: 0.0,
            ledger_usd: 0.0,
            worst_next_usd: 0.0,
            exposure_next_usd: 0.0,
            ping_input: PING_INPUT_ESTIMATE,
            pings: 0,
            retries: 0,
            next_at: now,
            next_wall: wall,
            in_flight: false,
            fired_age: Duration::ZERO,
        };
        if let Some(c) = carry {
            // Same epoch: the ledger, the ping count (so ping 1's write
            // allowance is never re-granted), and the idle start continue.
            p.ledger_usd = c.ledger_usd;
            p.pings = c.pings;
            p.idle_since = c.idle_since;
            p.idle_wall = c.idle_wall;
            if c.pings > 0 {
                p.prefix_tokens = c.prefix_tokens;
                p.ping_input = c.ping_input;
            }
        }
        p.reprice();
        // Due at anchor + interval; a turn that streamed past it is due now.
        let interval = p.interval();
        p.next_at = (anchor.mono + interval).max(now);
        p.next_wall = (anchor.wall + interval).max(wall);
        if p.worst_next_usd > p.bound_usd * (1.0 + BOUND_EPSILON) {
            return Err(PeriodEnd::PingTooExpensive {
                worst_usd: p.worst_next_usd,
                bound_usd: p.bound_usd,
            });
        }
        if let Some(r) = p
            .past_window(p.next_at, p.next_wall)
            .or_else(|| p.past_bound())
        {
            return Err(r);
        }
        Ok(p)
    }

    /// What the next period of this cache epoch continues from.
    pub fn epoch_carry(&self) -> EpochCarry {
        EpochCarry {
            ledger_usd: self.ledger_usd,
            pings: self.pings,
            prefix_tokens: self.prefix_tokens,
            ping_input: self.ping_input,
            idle_since: self.idle_since,
            idle_wall: self.idle_wall,
        }
    }

    /// Bound, read-path worst case and cap exposure for the current prefix,
    /// TTL and ping count.
    fn reprice(&mut self) {
        let model = self.cfg.model.as_str();
        self.bound_usd = rewrite_cost_usd(model, self.ttl, self.prefix_tokens);
        let write = if self.pings == 0 { self.first_write } else { 0 };
        self.worst_next_usd = request_cost_usd(
            model,
            self.ttl,
            self.prefix_tokens.saturating_sub(write),
            write,
            self.ping_input,
            self.cfg.max_tokens,
        );
        // A miss rewrites the whole prefix. The ping carries the configured
        // markers, so under `1h` / `hybrid` that write can be billed at the
        // 1h price even while the honoured TTL is 5m: take the dearer one.
        let miss = |ttl| {
            request_cost_usd(
                model,
                ttl,
                0,
                self.prefix_tokens,
                self.ping_input,
                self.cfg.max_tokens,
            )
        };
        let mut miss_usd = miss(self.ttl);
        if matches!(self.cfg.ttl, CacheTtl::OneHour | CacheTtl::Hybrid) {
            miss_usd = miss_usd.max(miss(CacheTtl::OneHour));
        }
        self.exposure_next_usd = self.worst_next_usd.max(miss_usd);
    }

    fn interval(&self) -> Duration {
        self.cfg
            .interval_override
            .unwrap_or_else(|| ping_interval(self.ttl))
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
    /// Read-path worst case of the next ping (cache hit, `max_tokens` of
    /// output): what the ski-rental bound checks use, and what an aborted
    /// in-flight ping is booked at.
    pub fn worst_next_usd(&self) -> f64 {
        self.worst_next_usd
    }
    /// Most the next ping can bill once dispatched, a full cache miss
    /// included (≥ [`worst_next_usd`](Self::worst_next_usd)): what the
    /// `max_session_cost` admission check uses.
    pub fn exposure_next_usd(&self) -> f64 {
        self.exposure_next_usd
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
    /// The honoured TTL the period is governed by.
    pub fn ttl(&self) -> CacheTtl {
        self.ttl
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

    /// When the next ping is due: the earlier of the monotonic deadline and
    /// the wall-clock deadline mapped onto `now` (after a suspend the
    /// monotonic one is late by the time slept). `None` while in flight.
    pub fn deadline(&self, now: Instant, wall: SystemTime) -> Option<Instant> {
        if self.in_flight {
            return None;
        }
        let wall_left = self.next_wall.duration_since(wall).unwrap_or_default();
        Some(self.next_at.min(now + wall_left))
    }

    /// `<N>h` window, or `auto`'s `auto_max_hours` cap, from the idle start.
    fn past_window(&self, at: Instant, wall: SystemTime) -> Option<PeriodEnd> {
        let h = match self.cfg.mode {
            CacheKeepWarm::Hours(h) => h,
            CacheKeepWarm::Auto if self.cfg.auto_max_hours > 0 => self.cfg.auto_max_hours,
            _ => return None,
        };
        let window = Duration::from_secs(u64::from(h) * 3600);
        let walled = wall.duration_since(self.idle_wall).unwrap_or_default();
        (at.saturating_duration_since(self.idle_since) >= window || walled >= window)
            .then_some(PeriodEnd::HoursReached(h))
    }

    fn past_bound(&self) -> Option<PeriodEnd> {
        (self.ledger_usd + self.worst_next_usd > self.bound_usd * (1.0 + BOUND_EPSILON)).then_some(
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
        if self.in_flight || (now < self.next_at && wall < self.next_wall) {
            return Fire::NotYet;
        }
        let age_now = age(&self.anchor, now, wall);
        if age_now + LATE_GUARD >= governing_ttl(self.ttl) {
            return Fire::End(PeriodEnd::Expired {
                age_secs: age_now.as_secs(),
            });
        }
        if let Some(r) = self.past_window(now, wall).or_else(|| self.past_bound()) {
            return Fire::End(r);
        }
        if let Some(cap) = max_session_cost.filter(|c| c.is_finite()) {
            // A hard cap: admit only what fits even if the ping misses.
            if session_cost + self.exposure_next_usd >= cap {
                return Fire::End(PeriodEnd::CostCap { cap });
            }
        }
        self.in_flight = true;
        self.fired_age = age_now;
        Fire::Ping
    }

    /// Book spend that belongs to this period (every ping, failed or not).
    pub fn charge(&mut self, cost_usd: f64) {
        self.ledger_usd += cost_usd;
    }

    /// The in-flight ping was sent and accepted (its cost already
    /// [`charge`](Self::charge)d); `anchor` is the runtime's anchor after it,
    /// `downgraded` the session's silent-downgrade latch. Schedules the next
    /// ping or says why the period is over.
    pub fn ping_sent(
        &mut self,
        usage: &KeepWarmUsage,
        anchor: Option<CacheAnchor>,
        downgraded: bool,
    ) -> Result<(), PeriodEnd> {
        self.in_flight = false;
        self.retries = 0;
        self.pings += 1;
        let (read, write) = (usage.cache_read_tokens, usage.cache_write_tokens);
        if read == 0 && write == 0 {
            return Err(PeriodEnd::NothingCached);
        }
        if read == 0 {
            return Err(PeriodEnd::CacheMiss { written: write });
        }
        if over_share(usage.input_tokens, self.prefix_tokens) {
            return Err(PeriodEnd::UncachedInput {
                input: usage.input_tokens,
                prefix: self.prefix_tokens,
            });
        }
        let allowed = if self.pings == 1 { self.first_write } else { 0 };
        if over_share(write.saturating_sub(allowed), self.prefix_tokens) {
            return Err(PeriodEnd::PartialMiss {
                written: write,
                prefix: self.prefix_tokens,
            });
        }
        // What the server reports from now on is the prefix.
        self.prefix_tokens = read + write;
        self.ping_input = usage.input_tokens.max(1);
        self.evidence.downgraded |= downgraded;
        self.evidence.write_1h = usage.cache_write_1h.unwrap_or(0);
        self.evidence.write_5m = usage.cache_write_5m.unwrap_or(0);
        self.evidence.saw_1h |= self.evidence.write_1h > 0;
        self.ttl = honoured_ttl(self.cfg.ttl, self.evidence);
        self.reprice();
        if let Some(a) = anchor.filter(|a| a.mono >= self.anchor.mono) {
            self.anchor = a;
        }
        let interval = self.interval();
        self.next_at = self.anchor.mono + interval;
        self.next_wall = self.anchor.wall + interval;
        if let Some(r) = self
            .past_window(self.next_at, self.next_wall)
            .or_else(|| self.past_bound())
        {
            return Err(r);
        }
        Ok(())
    }

    /// The in-flight ping failed (its cost, if any, already charged).
    /// Transient failures retry with bounded backoff as long as the retry
    /// still lands before expiry.
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
        if age_then + LATE_GUARD >= governing_ttl(self.ttl) {
            return Err(PeriodEnd::TransientExhausted(format!(
                "{msg}; no retry fits before the cache expires"
            )));
        }
        if self.ledger_usd + self.worst_next_usd > self.bound_usd * (1.0 + BOUND_EPSILON) {
            return Err(PeriodEnd::BoundReached {
                ledger_usd: self.ledger_usd,
                bound_usd: self.bound_usd,
            });
        }
        self.retries += 1;
        self.next_at = now + delay;
        self.next_wall = wall + delay;
        Ok(delay)
    }
}

/// One in-flight ping.
pub(crate) struct PingTask {
    pub(crate) handle: tokio::task::JoinHandle<PingReport>,
    cancel: CancellationToken,
    tap: PingTap,
    /// Pricing model at send time (the runtime may be gone when it ends).
    model: String,
    /// The period's worst case for this ping at send time: what it is
    /// booked at if it was dispatched but no usage arrived.
    worst_usd: f64,
}

impl PingTask {
    /// The ONE booking rule (`runtime::keepwarm::ping_booking`) applied to
    /// this ping; every path that ends a ping books through here.
    fn bill(&self) -> PingBill {
        self.tap.bill(&self.model, self.worst_usd)
    }
}

/// Actor-owned keep-warm state.
#[derive(Default)]
pub(crate) struct KeepWarmState {
    /// `cache_keepwarm` from config at session create.
    configured: CacheKeepWarm,
    /// `cache_keepwarm_auto_max_hours` from config at session create.
    auto_max_hours: u32,
    /// The current cache epoch's carry (see [`EpochCarry`]); `None` until a
    /// period of this epoch ended. Reset by every real request's usage, a
    /// compaction and a conversation swap.
    epoch: Option<EpochCarry>,
    /// `/keepwarm …` for this session (survives park and daemon reload).
    pub(crate) override_mode: Option<CacheKeepWarm>,
    /// Shared with the session handle so a daemon reload that cannot
    /// checkpoint the actor still carries the override (see `reload.rs`).
    pub(crate) override_slot: OverrideSlot,
    pub(crate) period: Option<Period>,
    pub(crate) task: Option<PingTask>,
    next_period_id: u64,
    /// Set by `start_turn`, consumed by the next `Idle`.
    turn_started_at: Option<Instant>,
    /// Usage of the latest real request (`None` until one reported).
    last_request: Option<KeepWarmUsage>,
    /// A TUI/attach client that can type (not `Observe`) has been attached
    /// at some point. Script/RPC-only or observe-only sessions (nobody will
    /// be back to type in human time) never keep warm.
    pub(crate) interactive_seen: bool,
    last_end: Option<String>,
    session_pings: u32,
}

/// The per-session `/keepwarm` override, readable without the actor.
pub type OverrideSlot = std::sync::Arc<std::sync::Mutex<Option<CacheKeepWarm>>>;

impl KeepWarmState {
    pub(crate) fn new(
        configured: CacheKeepWarm,
        auto_max_hours: u32,
        override_slot: OverrideSlot,
    ) -> Self {
        let override_mode = *override_slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self {
            configured,
            auto_max_hours,
            override_mode,
            override_slot,
            ..Self::default()
        }
    }

    pub(crate) fn mode(&self) -> CacheKeepWarm {
        self.override_mode.unwrap_or(self.configured)
    }

    pub(crate) fn set_override(&mut self, mode: CacheKeepWarm) {
        self.override_mode = Some(mode);
        *self
            .override_slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(mode);
    }

    pub(crate) fn active(&self) -> bool {
        self.period.is_some()
    }

    /// When the actor's timer should next wake: the ping deadline, but never
    /// later than [`WALL_POLL`] (re-reads the wall clock after a suspend).
    /// `None` (no timer) with no period or while a ping is in flight.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.deadline_at(Instant::now(), SystemTime::now())
    }

    fn deadline_at(&self, now: Instant, wall: SystemTime) -> Option<Instant> {
        self.period
            .as_ref()
            .and_then(|p| p.deadline(now, wall))
            .map(|d| d.min(now + WALL_POLL))
    }
}

/// `select!` arm: the next ping is due (or the wall clock needs a look).
pub(crate) async fn keepwarm_timer(deadline: Option<Instant>) {
    match deadline {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

/// `select!` arm: the in-flight ping finished.
pub(crate) async fn poll_keepwarm_ping(
    task: &mut Option<PingTask>,
) -> std::result::Result<PingReport, tokio::task::JoinError> {
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

    /// A client attached: TUI/attach clients that can type (`Mirror` or
    /// `Takeover`) mark the session interactive. An `Observe` attach never
    /// does: it cannot send input, so only watching a session (`synaps
    /// attach --observe`, a read-only pane in another app) must not make it
    /// eligible for paid keep-warm.
    pub(crate) fn cache_keepwarm_note_attach(&mut self, kind: ClientKind, mode: AttachMode) {
        if mode == AttachMode::Observe {
            return;
        }
        if matches!(kind, ClientKind::Tui | ClientKind::Attach) {
            self.cache_keepwarm.interactive_seen = true;
        }
    }

    /// A provider Usage event of the running turn (one per request): the
    /// latest request's usage decides whether anything is cached at all.
    pub(crate) fn cache_keepwarm_note_usage(&mut self, usage: KeepWarmUsage) {
        self.cache_keepwarm.last_request = Some(usage);
        // A real request refreshed/rewrote the cache: a new epoch.
        self.cache_keepwarm.epoch = None;
    }

    /// A real turn is starting: the idle period is over, any ping in flight
    /// is aborted (and booked). The next `Idle` may start a new period.
    pub(crate) fn cache_keepwarm_turn_started(&mut self) {
        self.cache_keepwarm_end(PeriodEnd::TurnStarted);
        self.cache_keepwarm.turn_started_at = Some(Instant::now());
        self.cache_keepwarm.last_request = None;
    }

    /// Compaction replaces the history: nothing is worth keeping warm until
    /// the next real turn.
    pub(crate) fn cache_keepwarm_compaction(&mut self) {
        self.cache_keepwarm_end(PeriodEnd::Compaction);
        self.cache_keepwarm.turn_started_at = None;
        self.cache_keepwarm.last_request = None;
        self.cache_keepwarm.epoch = None;
    }

    /// `/clear` (`NewSession`) or `/resume` is about to replace the
    /// conversation. Call it BEFORE the swap: the period ends at once
    /// (prefix changed), a ping in flight is aborted, and its cost is booked
    /// to the conversation it was keeping warm — the old one, which the swap
    /// then saves. Nothing of the old conversation may start a period on
    /// the new one.
    pub(crate) fn cache_keepwarm_conversation_replaced(&mut self) {
        self.cache_keepwarm_end(PeriodEnd::PrefixChanged);
        self.cache_keepwarm.turn_started_at = None;
        self.cache_keepwarm.last_request = None;
        self.cache_keepwarm.epoch = None;
    }

    /// An explicit park request: an active period ends ("parked by
    /// request") so the park can proceed. The idle auto-park still waits.
    pub(crate) fn cache_keepwarm_park_requested(&mut self) {
        if self.cache_keepwarm.active() && self.can_park_ignoring_keepwarm() {
            self.cache_keepwarm_end(PeriodEnd::ParkedByRequest);
        }
    }

    /// Book keep-warm spend: session cost (so `max_session_cost` sees it),
    /// the labelled keep-warm cost, token totals (shown separately), and the
    /// active period's ledger — then queue a session save so the charge is
    /// durable without waiting for Save/Park/End.
    fn cache_keepwarm_book(&mut self, usage: &KeepWarmUsage, cost: f64) {
        if (cost <= 0.0 && *usage == KeepWarmUsage::default()) || !self.conv.is_live() {
            return;
        }
        self.conv.session_cost += cost;
        self.conv.keepwarm_cost += cost;
        self.conv.total_input_tokens += usage.input_tokens;
        self.conv.total_output_tokens += usage.output_tokens;
        self.conv.total_cache_read_tokens += usage.cache_read_tokens;
        self.conv.total_cache_creation_tokens += usage.cache_write_tokens;
        let t = &mut self.conv.keepwarm_tokens;
        t.input += usage.input_tokens;
        t.output += usage.output_tokens;
        t.cache_read += usage.cache_read_tokens;
        t.cache_creation += usage.cache_write_tokens;
        if let Some(p) = self.cache_keepwarm.period.as_mut() {
            p.charge(cost);
        }
        self.emit_conversation();
        // Settled spend must survive a crash during idle (otherwise the
        // charge is lost and `max_session_cost` headroom restored). The
        // ordinary session save, queued on the persister: never blocks the
        // actor. The keep-warm labels stay snapshot-only (never journaled);
        // in journal mode the journaled session cost carries the charge.
        self.request_save();
    }

    /// Abort the in-flight ping, if any, and book what it may have cost
    /// ([`PingTask::bill`]: real usage, else the worst case once dispatched,
    /// else nothing).
    fn cache_keepwarm_abort_ping(&mut self) {
        let Some(task) = self.cache_keepwarm.task.take() else {
            return;
        };
        task.cancel.cancel();
        task.handle.abort();
        let bill = task.bill();
        if bill.cost_usd > 0.0 {
            tracing::info!(
                session = %self.id,
                cost_usd = bill.cost_usd,
                booking = ?bill.booking,
                "cache keep-warm ping aborted in flight; booked"
            );
        }
        self.cache_keepwarm_book(&bill.usage, bill.cost_usd);
    }

    /// End the active period (and abort any ping) with one final log line.
    pub(crate) fn cache_keepwarm_end(&mut self, reason: PeriodEnd) {
        self.cache_keepwarm_abort_ping();
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
        // A later period of the same cache epoch continues from here.
        self.cache_keepwarm.epoch = Some(p.epoch_carry());
        // A period defers parking; now the session may park normally.
        self.rearm_park();
    }

    /// The session announced `Idle`: start a period if the turn that just
    /// ended had an Anthropic request accepted and keep-warm is on.
    pub(crate) async fn cache_keepwarm_on_idle(&mut self) {
        let Some(turn_started_at) = self.cache_keepwarm.turn_started_at.take() else {
            return;
        };
        self.cache_keepwarm_try_start(Some(turn_started_at)).await;
    }

    /// Start a period now if eligible. `turn_started_at`: the anchor must be
    /// from this turn (on `Idle`); `None` (a `/keepwarm` command on an idle
    /// session) accepts the current anchor as long as the last request's
    /// usage is still the latest.
    async fn cache_keepwarm_try_start(&mut self, turn_started_at: Option<Instant>) {
        if self.cache_keepwarm.active() {
            return;
        }
        let mode = self.cache_keepwarm.mode();
        // A pin is the owner's say-so that the session is coming back (a
        // restored pin often has no client; `synaps send` feeds it).
        if mode.is_off() || !(self.cache_keepwarm.interactive_seen || self.keep_warm) {
            return;
        }
        // Never before a real Anthropic request of THIS turn was accepted.
        let Some(anchor) = self
            .runtime
            .cache_anchor()
            .filter(|a| turn_started_at.is_none_or(|t| a.mono >= t))
        else {
            return;
        };
        let Some(last) = self.cache_keepwarm.last_request.clone() else {
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
        let probe: KeepWarmProbe = match self
            .runtime
            .cache_keepwarm_probe(&self.conv.api_messages)
            .await
        {
            Ok(p) => p,
            Err(e) => {
                tracing::info!(session = %self.id, "cache keep-warm not started: {e}");
                self.cache_keepwarm.last_end = Some(format!("not started: {e}"));
                return;
            }
        };
        if let Some(why) = probe.context_blocks {
            let reason = PeriodEnd::ContextRollover(why);
            tracing::info!(session = %self.id, reason = %reason, "cache keep-warm not started");
            self.cache_keepwarm.last_end = Some(format!("not started: {reason}"));
            return;
        }
        let cfg = PeriodConfig {
            mode,
            ttl: self.runtime.cache_ttl(),
            model: self.runtime.model().to_string(),
            max_tokens: probe.max_tokens,
            interval_override: interval_override(),
            auto_max_hours: self.cache_keepwarm.auto_max_hours,
            reply_tokens: Some(crate::runtime::keepwarm::reply_prefix_tokens(
                &self.conv.api_messages,
            )),
        };
        let evidence = TtlEvidence {
            downgraded: self.runtime.cache_ttl_downgraded(),
            saw_1h: self.runtime.cache_1h_honoured(),
            write_1h: last.cache_write_1h.unwrap_or(0),
            write_5m: last.cache_write_5m.unwrap_or(0),
        };
        self.cache_keepwarm.next_period_id += 1;
        let id = self.cache_keepwarm.next_period_id;
        let now = Instant::now();
        let carry = self.cache_keepwarm.epoch;
        match Period::start_in_epoch(
            id,
            cfg,
            probe.fingerprint,
            &last,
            evidence,
            anchor,
            now,
            SystemTime::now(),
            carry.as_ref(),
        ) {
            Ok(p) => {
                tracing::info!(
                    session = %self.id,
                    period = id,
                    mode = %mode,
                    ttl_secs = governing_ttl(p.ttl()).as_secs(),
                    first_ping_in_secs = p.next_at().saturating_duration_since(now).as_secs(),
                    prefix_tokens = p.prefix_tokens(),
                    bound_usd = p.bound_usd(),
                    worst_ping_usd = p.exposure_next_usd(),
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

    /// `select!` arm: a ping may be due.
    pub(crate) async fn on_cache_keepwarm_due(&mut self) {
        #[cfg(any(test, feature = "testing"))]
        testing::note_due_check();
        // Fenced for a reload, the exec would discard the ping's result. The
        // ping also repeats the next turn's prefix, session-start injection
        // included, so it waits for that hook: the timer arm is off
        // meanwhile (`holds`), and a due deadline fires once it has run.
        if self.holds(super::actor::WorkSource::KeepWarmTimer)
            || self.cache_keepwarm.period.is_none()
            || self.cache_keepwarm.task.is_some()
        {
            return;
        }
        let (now, wall) = (Instant::now(), SystemTime::now());
        // A wall-clock poll before anything is due: nothing to do.
        if self
            .cache_keepwarm
            .period
            .as_ref()
            .and_then(|p| p.deadline(now, wall))
            .is_some_and(|d| d > now)
        {
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
        // The same pre-request assessment a real turn makes: if the next
        // turn would roll over first, the cached prefix is not what it sends.
        match self
            .runtime
            .cache_keepwarm_probe(&self.conv.api_messages)
            .await
        {
            Ok(probe) => {
                if let Some(why) = probe.context_blocks {
                    self.cache_keepwarm_end(PeriodEnd::ContextRollover(why));
                    return;
                }
            }
            Err(e) => {
                self.cache_keepwarm_end(PeriodEnd::Rejected(e.to_string()));
                return;
            }
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
                let worst_usd = p.worst_next_usd();
                // A detached clone sharing the anchor and the TTL latches:
                // the actor keeps serving clients while the ping runs; a
                // turn start aborts it.
                let history = self.conv.api_messages.clone();
                let mut runtime = self.runtime.clone();
                runtime.share_ttl_latches(&self.runtime);
                let cancel = CancellationToken::new();
                let token = cancel.clone();
                let tap = PingTap::default();
                let task_tap = tap.clone();
                let handle = tokio::spawn(async move {
                    runtime
                        .cache_keepwarm_ping(&history, expect, &token, &task_tap)
                        .await
                });
                self.cache_keepwarm.task = Some(PingTask {
                    handle,
                    cancel,
                    tap,
                    model: self.runtime.model().to_string(),
                    worst_usd,
                });
            }
        }
    }

    /// `select!` arm: the in-flight ping finished.
    pub(crate) fn on_cache_keepwarm_done(
        &mut self,
        result: std::result::Result<PingReport, tokio::task::JoinError>,
    ) {
        let Some(task) = self.cache_keepwarm.task.take() else {
            return;
        };
        let report = match result {
            Ok(r) => r,
            Err(e) => PingReport {
                result: Err(if e.is_cancelled() {
                    PingFailure::Canceled
                } else {
                    PingFailure::Fatal(format!("ping task: {e}"))
                }),
                usage: task.tap.usage(),
                notices: Vec::new(),
            },
        };
        // Book on EVERY path (success, failure, panic) by the one rule.
        let bill = task.bill();
        let cost = bill.cost_usd;
        self.cache_keepwarm_book(&bill.usage, cost);
        for notice in &report.notices {
            self.emit(crate::session::SessionEventWire::SystemNotice(
                notice.clone(),
            ));
        }
        if self.cache_keepwarm.period.is_none() {
            return;
        }
        match report.result {
            Ok(PingOutcome::Sent) => {
                let usage = report.usage;
                self.cache_keepwarm.session_pings += 1;
                let anchor = self.runtime.cache_anchor();
                let downgraded = self.runtime.cache_ttl_downgraded();
                let Some(p) = self.cache_keepwarm.period.as_mut() else {
                    return;
                };
                let result = p.ping_sent(&usage, anchor, downgraded);
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
                if let Err(reason) = result {
                    if matches!(
                        reason,
                        PeriodEnd::PartialMiss { .. }
                            | PeriodEnd::CacheMiss { .. }
                            | PeriodEnd::UncachedInput { .. }
                            | PeriodEnd::NothingCached
                    ) {
                        tracing::warn!(
                            session = %self.id,
                            reason = %reason,
                            "cache keep-warm ping did not refresh the cache"
                        );
                    }
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
                .is_ok_and(|p| p.fingerprint == expect);
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
        let (now, wall) = (Instant::now(), SystemTime::now());
        let period = kw.period.as_ref().map(|p| {
            // Wall-clock aware: after a suspend the monotonic deadline lags.
            let next = p
                .deadline(now, wall)
                .map(|d| d.saturating_duration_since(now).as_secs());
            serde_json::json!({
                "id": p.id,
                "ttl_secs": governing_ttl(p.ttl()).as_secs(),
                "pings": p.pings(),
                "ledger_usd": p.ledger_usd(),
                "bound_usd": p.bound_usd(),
                "worst_ping_usd": p.exposure_next_usd(),
                "prefix_tokens": p.prefix_tokens(),
                "next_ping_in_secs": next.unwrap_or(0),
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
            self.cache_keepwarm.set_override(mode);
            if self.keep_warm {
                self.note_pin(); // the override comes back with the pin
            }
            if mode.is_off() {
                self.cache_keepwarm_end(PeriodEnd::Disabled);
            } else if let Some(p) = self.cache_keepwarm.period.as_mut() {
                p.set_mode(mode);
            } else {
                // Already idle: start now if eligible.
                self.cache_keepwarm_try_start(None).await;
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

    /// Streaming ping with thinking off / adaptive: `max_tokens` 1.
    fn cfg(mode: CacheKeepWarm, ttl: CacheTtl, model: &str) -> PeriodConfig {
        cfg_max(mode, ttl, model, 1)
    }

    fn cfg_max(mode: CacheKeepWarm, ttl: CacheTtl, model: &str, max_tokens: u64) -> PeriodConfig {
        PeriodConfig {
            mode,
            ttl,
            model: model.to_string(),
            max_tokens,
            interval_override: None,
            auto_max_hours: 0,
            reply_tokens: None,
        }
    }

    /// 1h honoured: the session has written a 1h entry.
    fn ev_1h() -> TtlEvidence {
        TtlEvidence {
            saw_1h: true,
            ..TtlEvidence::default()
        }
    }

    fn anchor_at(mono: Instant, wall: SystemTime) -> CacheAnchor {
        CacheAnchor { mono, wall }
    }

    fn usage(model: &str, read: u64, write: u64, input: u64, out: u64) -> KeepWarmUsage {
        KeepWarmUsage {
            input_tokens: input,
            output_tokens: out,
            cache_read_tokens: read,
            cache_write_tokens: write,
            cache_write_5m: None,
            cache_write_1h: None,
            model: Some(model.to_string()),
        }
    }

    /// The last real request read the whole `P` prefix (no tail to write).
    fn last_read(model: &str) -> KeepWarmUsage {
        usage(model, P, 0, 0, 0)
    }

    fn start(c: PeriodConfig, last: &KeepWarmUsage, t0: Instant, w0: SystemTime) -> Period {
        Period::start(1, c, 7, last, ev_1h(), anchor_at(t0, w0), t0, w0).expect("starts")
    }

    /// Drive a period with the given ping usages (last one repeats) until it
    /// ends. Every ping is sent exactly when due and moves the anchor.
    /// Returns (pings, ledger, bound, reason, elapsed since idle).
    fn run_from(
        cfg: PeriodConfig,
        last: &KeepWarmUsage,
        usages: &[KeepWarmUsage],
    ) -> (u32, f64, f64, PeriodEnd, Duration) {
        let t0 = Instant::now();
        let w0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let model = cfg.model.clone();
        let mut p = start(cfg, last, t0, w0);
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
            p.charge(u.cost_usd(&model));
            if let Err(r) = p.ping_sent(u, Some(anchor_at(at, wall)), false) {
                return (p.pings(), p.ledger_usd(), p.bound_usd(), r, at - t0);
            }
            assert!(i < 1000, "runaway");
        }
    }

    fn run(cfg: PeriodConfig, usages: &[KeepWarmUsage]) -> (u32, f64, f64, PeriodEnd, Duration) {
        let last = last_read(&cfg.model);
        run_from(cfg, &last, usages)
    }

    // ── ski-rental bound, both TTLs, measured costs, worst case ──

    /// Review #205: a dispatched ping can miss and rewrite the whole prefix
    /// at the honoured TTL. The hard cap must admit only what fits even then.
    #[test]
    fn review_205_possible_cache_miss_must_fit_admission_cap() {
        let t0 = Instant::now();
        let w0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut p = start(
            cfg(CacheKeepWarm::Auto, CacheTtl::OneHour, OPUS),
            &last_read(OPUS),
            t0,
            w0,
        );
        let at = p.next_at();
        let wall = w0 + at.saturating_duration_since(t0);
        let cap = 0.30;
        let decision = p.fire(at, wall, 0.0, Some(cap));
        let miss = KeepWarmUsage {
            cache_write_tokens: P,
            cache_write_1h: Some(P),
            ..KeepWarmUsage::default()
        };
        println!(
            "REVIEW estimated_worst={} possible_write={} cap={cap} decision={decision:?}",
            p.worst_next_usd,
            miss.cost_usd(OPUS)
        );
        if matches!(decision, Fire::Ping) {
            assert!(
                miss.cost_usd(OPUS) <= cap,
                "admitted request can exceed its session cost cap on a cache miss"
            );
        }
    }

    /// The miss-priced admission must not kill the feature: a cap with room
    /// for a full miss still pings, and the ski-rental bound stays on the
    /// read path.
    #[test]
    fn a_cap_with_room_for_a_cache_miss_still_pings() {
        let t0 = Instant::now();
        let w0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut p = start(
            cfg(CacheKeepWarm::Auto, CacheTtl::OneHour, OPUS),
            &last_read(OPUS),
            t0,
            w0,
        );
        // Full miss at 1h: 300k × $10/MTok = $3.00, + 64 input + 1 output.
        let exposure = p.exposure_next_usd();
        let miss = usage(OPUS, 0, P, PING_INPUT_ESTIMATE, 1);
        let miss = KeepWarmUsage {
            cache_write_1h: Some(P),
            ..miss
        };
        assert!((exposure - miss.cost_usd(OPUS)).abs() < 1e-9, "{exposure}");
        assert!(p.worst_next_usd() < p.bound_usd(), "bound basis is the read");
        let at = p.next_at();
        let wall = w0 + at.saturating_duration_since(t0);
        assert_eq!(
            p.fire(at, wall, 0.0, Some(3.0)),
            Fire::End(PeriodEnd::CostCap { cap: 3.0 })
        );
        assert_eq!(p.fire(at, wall, 0.0, Some(3.01)), Fire::Ping);
    }

    #[test]
    fn ski_rental_1h_is_nineteen_pure_reads_over_about_17h() {
        // A pure read of a 300k prefix: $0.15 on Opus; rewrite at 1h = $3.00.
        // The pre-check uses the WORST case (read + 1 output token), so the
        // 20th read ($3.00 exactly + $0.000025) no longer fits.
        let (pings, ledger, bound, reason, elapsed) = run(
            cfg(CacheKeepWarm::Auto, CacheTtl::OneHour, OPUS),
            &[usage(OPUS, P, 0, 0, 0)],
        );
        assert!((bound - 3.0).abs() < 1e-9, "{bound}");
        assert_eq!(pings, 19);
        assert!(ledger <= bound * (1.0 + 1e-9));
        assert!(matches!(reason, PeriodEnd::BoundReached { .. }), "{reason}");
        assert_eq!(elapsed, 19 * 55 * MIN);
    }

    #[test]
    fn ski_rental_5m_is_twelve_pure_reads_over_about_55_min() {
        // Rewrite at 5m = 300k × $6.25/MTok = $1.875; 12 reads = $1.80.
        let (pings, ledger, bound, reason, elapsed) = run(
            cfg(CacheKeepWarm::Auto, CacheTtl::FiveMinutes, OPUS),
            &[usage(OPUS, P, 0, 0, 0)],
        );
        assert!((bound - 1.875).abs() < 1e-9, "{bound}");
        assert_eq!(pings, 12);
        assert!((ledger - 1.8).abs() < 1e-9);
        assert!(matches!(reason, PeriodEnd::BoundReached { .. }), "{reason}");
        assert_eq!(elapsed, 12 * Duration::from_secs(270));
    }

    #[test]
    fn measured_overhead_and_first_write_are_in_the_ledger() {
        // The last turn read 298k and produced a 2k-token reply: ping 1
        // writes that reply (1h price), every ping carries 40 uncached input
        // tokens and 1 output token.
        let last = usage(OPUS, P - 2_000, 0, 0, 2_000);
        let first = usage(OPUS, P - 2_000, 2_000, 40, 1);
        let steady = usage(OPUS, P, 0, 40, 1);
        let (pings, ledger, bound, reason, _) = run_from(
            cfg(CacheKeepWarm::Auto, CacheTtl::OneHour, OPUS),
            &last,
            &[first.clone(), steady.clone()],
        );
        assert!(matches!(reason, PeriodEnd::BoundReached { .. }), "{reason}");
        let expected_ledger = first.cost_usd(OPUS) + f64::from(pings - 1) * steady.cost_usd(OPUS);
        assert!(
            (ledger - expected_ledger).abs() < 1e-9,
            "ledger is the measured sum"
        );
        assert!(ledger <= bound * (1.0 + 1e-9), "never past the bound");
        assert!(ledger + steady.cost_usd(OPUS) > bound);
        assert_eq!(pings, 19);
    }

    #[test]
    fn an_expensive_ping_stops_the_period_early() {
        // Budgeted thinking that actually thought: 5k output tokens on
        // Sonnet 4.6 = $0.075 per ping on top of a $0.09 read.
        let (heavy, ..) = run(
            cfg_max(CacheKeepWarm::Auto, CacheTtl::OneHour, SONNET, 5_001),
            &[usage(SONNET, P, 0, 40, 5_000)],
        );
        let (light, ..) = run(
            cfg_max(CacheKeepWarm::Auto, CacheTtl::OneHour, SONNET, 31),
            &[usage(SONNET, P, 0, 40, 30)],
        );
        assert_eq!(heavy, 10, "rewrite $1.80 / ($0.09 read + $0.075 thinking)");
        assert_eq!(light, 19);
    }

    #[test]
    fn budgeted_thinking_output_is_accounted() {
        let u = usage(SONNET, P, 0, 40, 300);
        let read_only = usage(SONNET, P, 0, 0, 0);
        let thinking_usd = u.cost_usd(SONNET) - read_only.cost_usd(SONNET);
        // 300 thinking/output tokens at $15/MTok + 40 input at $3/MTok.
        assert!((thinking_usd - (300.0 * 15.0 + 40.0 * 3.0) / 1e6).abs() < 1e-12);
        let (pings, ledger, ..) = run(
            cfg_max(CacheKeepWarm::Auto, CacheTtl::FiveMinutes, SONNET, 301),
            std::slice::from_ref(&u),
        );
        assert!((ledger - f64::from(pings) * u.cost_usd(SONNET)).abs() < 1e-9);
    }

    /// Item: budgeted thinking can bill up to `max_tokens` of output. Sonnet
    /// 4.6 at xhigh (budget 32,768 → `max_tokens` 32,769) under 5m with a
    /// 20k prefix: one worst-case ping ($0.49) costs more than the $0.075
    /// rewrite it would save, so no ping is ever sent.
    #[test]
    fn budgeted_thinking_worst_case_must_fit_the_bound() {
        let t0 = Instant::now();
        let w0 = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000);
        let small = usage(SONNET, 20_000, 0, 0, 0);
        let err = Period::start(
            1,
            cfg_max(CacheKeepWarm::Auto, CacheTtl::FiveMinutes, SONNET, 32_769),
            7,
            &small,
            TtlEvidence::default(),
            anchor_at(t0, w0),
            t0,
            w0,
        )
        .unwrap_err();
        match err {
            PeriodEnd::PingTooExpensive {
                worst_usd,
                bound_usd,
            } => {
                assert!((bound_usd - 0.075).abs() < 1e-9, "{bound_usd}");
                assert!(worst_usd > 0.49, "{worst_usd}");
            }
            other => panic!("{other}"),
        }
        // A large prefix under 1h does fit: budgeted thinking runs there,
        // and the worst case is what every pre-check charges.
        let p = start(
            cfg_max(CacheKeepWarm::Auto, CacheTtl::OneHour, SONNET, 32_769),
            &usage(SONNET, P, 0, 0, 0),
            t0,
            w0,
        );
        assert!(p.worst_next_usd() > 0.58 && p.worst_next_usd() < p.bound_usd());
    }

    #[test]
    fn max_session_cost_is_checked_against_the_worst_case() {
        let t0 = Instant::now();
        let w0 = SystemTime::UNIX_EPOCH + Duration::from_secs(100_000);
        // Opus, 1h, 300k: a read is $0.15 but budgeted thinking could bill
        // 10,001 output tokens ($0.25) on top.
        let mut p = start(
            cfg_max(CacheKeepWarm::Auto, CacheTtl::OneHour, OPUS, 10_001),
            &last_read(OPUS),
            t0,
            w0,
        );
        let at = p.next_at();
        let wat = w0 + (at - t0);
        // $4.70 spent, cap $5.00: a read alone fits, the worst case does not.
        assert_eq!(
            p.fire(at, wat, 4.70, Some(5.0)),
            Fire::End(PeriodEnd::CostCap { cap: 5.0 })
        );
        // $4.50 spent: the read-path worst case ($0.40) would fit, but a miss
        // rewriting the prefix at 1h ($3.00) + the thinking ($0.25) would not.
        assert_eq!(
            p.fire(at, wat, 4.50, Some(5.0)),
            Fire::End(PeriodEnd::CostCap { cap: 5.0 })
        );
        // $1.70 spent: even a full miss (~$3.2503) stays under the cap.
        assert_eq!(p.fire(at, wat, 1.70, Some(5.0)), Fire::Ping);
    }

    // ── "nothing cached" guards ──

    /// A prompt below the model's cache minimum: the API returns
    /// read = write = 0 (no error). No period starts.
    #[test]
    fn small_prompt_below_the_cache_minimum_never_starts() {
        let t0 = Instant::now();
        let w0 = SystemTime::UNIX_EPOCH;
        let below_minimum = usage(OPUS, 0, 0, 800, 20);
        assert_eq!(
            Period::start(
                1,
                cfg(CacheKeepWarm::Auto, CacheTtl::OneHour, OPUS),
                7,
                &below_minimum,
                ev_1h(),
                anchor_at(t0, w0),
                t0,
                w0
            )
            .unwrap_err(),
            PeriodEnd::NothingCached
        );
    }

    fn fired(c: PeriodConfig) -> (Period, Instant, SystemTime) {
        let t0 = Instant::now();
        let w0 = SystemTime::UNIX_EPOCH + Duration::from_secs(90_000);
        let mut p = start(c, &last_read(OPUS), t0, w0);
        let at = p.next_at();
        let wat = w0 + (at - t0);
        assert_eq!(p.fire(at, wat, 0.0, None), Fire::Ping);
        (p, at, wat)
    }

    #[test]
    fn a_ping_that_cached_nothing_ends_the_period() {
        let (mut p, ..) = fired(cfg(CacheKeepWarm::Auto, CacheTtl::FiveMinutes, OPUS));
        assert_eq!(
            p.ping_sent(&usage(OPUS, 0, 0, P, 1), None, false),
            Err(PeriodEnd::NothingCached)
        );
    }

    #[test]
    fn a_ping_paying_for_a_large_uncached_share_ends_the_period() {
        // 20% of the prefix came back as uncached input (> 10%).
        let (mut p, ..) = fired(cfg(CacheKeepWarm::Auto, CacheTtl::FiveMinutes, OPUS));
        assert_eq!(
            p.ping_sent(&usage(OPUS, P - 60_000, 0, 60_000, 1), None, false),
            Err(PeriodEnd::UncachedInput {
                input: 60_000,
                prefix: P
            })
        );
        // A normal ping message (40 tokens) is nowhere near.
        let (mut p, ..) = fired(cfg(CacheKeepWarm::Auto, CacheTtl::FiveMinutes, OPUS));
        assert_eq!(p.ping_sent(&usage(OPUS, P, 0, 40, 1), None, false), Ok(()));
    }

    #[test]
    fn a_ping_that_writes_instead_of_reading_ends_the_period() {
        let (mut p, ..) = fired(cfg(CacheKeepWarm::Auto, CacheTtl::FiveMinutes, OPUS));
        assert_eq!(
            p.ping_sent(&usage(OPUS, 0, P, 40, 1), None, false),
            Err(PeriodEnd::CacheMiss { written: P })
        );
    }

    /// After ping 1, a ping that rewrites a large share of the prefix
    /// (history no longer cached past some point) ends the period.
    #[test]
    fn a_partial_miss_after_ping_one_ends_the_period() {
        let (mut p, at, wat) = fired(cfg(CacheKeepWarm::Auto, CacheTtl::FiveMinutes, OPUS));
        p.ping_sent(&usage(OPUS, P, 0, 40, 1), Some(anchor_at(at, wat)), false)
            .unwrap();
        let at = p.next_at();
        let wat = wat + (at - p.anchor.mono);
        assert_eq!(p.fire(at, wat, 0.0, None), Fire::Ping);
        assert_eq!(
            p.ping_sent(&usage(OPUS, P - 50_000, 50_000, 40, 1), None, false),
            Err(PeriodEnd::PartialMiss {
                written: 50_000,
                prefix: P
            })
        );
    }

    #[test]
    fn ping_one_may_write_the_last_reply() {
        let t0 = Instant::now();
        let w0 = SystemTime::UNIX_EPOCH + Duration::from_secs(3_000);
        // The last turn: 250k read, 50k-token reply (a long tool result).
        let last = usage(OPUS, 250_000, 0, 0, 50_000);
        let mut p = start(
            cfg(CacheKeepWarm::Auto, CacheTtl::OneHour, OPUS),
            &last,
            t0,
            w0,
        );
        assert_eq!(p.prefix_tokens(), P);
        let at = p.next_at();
        assert_eq!(p.fire(at, w0 + (at - t0), 0.0, None), Fire::Ping);
        assert_eq!(
            p.ping_sent(&usage(OPUS, 250_000, 50_000, 40, 1), None, false),
            Ok(()),
            "the reply's write is expected on ping 1"
        );
    }

    // ── honoured TTL (silent downgrade) ──

    #[test]
    fn honoured_ttl_needs_evidence_for_1h() {
        use CacheTtl::*;
        let none = TtlEvidence::default();
        assert_eq!(honoured_ttl(FiveMinutes, ev_1h()), FiveMinutes);
        assert_eq!(honoured_ttl(Hybrid, ev_1h()), Hybrid);
        assert_eq!(honoured_ttl(OneHour, none), FiveMinutes, "no evidence");
        assert_eq!(honoured_ttl(OneHour, ev_1h()), OneHour);
        let wrote_1h = TtlEvidence {
            write_1h: 10,
            ..none
        };
        assert_eq!(honoured_ttl(OneHour, wrote_1h), OneHour);
        let wrote_5m = TtlEvidence {
            saw_1h: true,
            write_5m: 10,
            ..none
        };
        assert_eq!(
            honoured_ttl(OneHour, wrote_5m),
            FiveMinutes,
            "5m-only write"
        );
        let downgraded = TtlEvidence {
            downgraded: true,
            write_1h: 10,
            saw_1h: true,
            ..none
        };
        assert_eq!(honoured_ttl(OneHour, downgraded), FiveMinutes, "latch wins");
    }

    /// `cache_ttl = 1h` but the server honours 5m: the period runs on the
    /// 5m schedule and is bounded by the 5m rewrite, never sleeping 55 min
    /// into a lapsed cache.
    #[test]
    fn a_silent_downgrade_governs_by_5m() {
        let t0 = Instant::now();
        let w0 = SystemTime::UNIX_EPOCH + Duration::from_secs(4_000);
        let downgraded = TtlEvidence {
            downgraded: true,
            ..TtlEvidence::default()
        };
        let p = Period::start(
            1,
            cfg(CacheKeepWarm::Auto, CacheTtl::OneHour, OPUS),
            7,
            &last_read(OPUS),
            downgraded,
            anchor_at(t0, w0),
            t0,
            w0,
        )
        .unwrap();
        assert_eq!(p.ttl(), CacheTtl::FiveMinutes);
        assert_eq!(p.next_at(), t0 + Duration::from_secs(270));
        assert!((p.bound_usd() - 1.875).abs() < 1e-9);

        // No evidence at start → 5m; ping 1 writes the reply at 1h → 1h;
        // a later ping trips the session latch → back to 5m.
        let mut p = Period::start(
            2,
            cfg(CacheKeepWarm::Auto, CacheTtl::OneHour, OPUS),
            7,
            &usage(OPUS, P - 1_000, 0, 0, 1_000),
            TtlEvidence::default(),
            anchor_at(t0, w0),
            t0,
            w0,
        )
        .unwrap();
        assert_eq!(p.ttl(), CacheTtl::FiveMinutes);
        let at = p.next_at();
        let wat = w0 + (at - t0);
        assert_eq!(p.fire(at, wat, 0.0, None), Fire::Ping);
        let mut first = usage(OPUS, P - 1_000, 1_000, 40, 1);
        first.cache_write_1h = Some(1_000);
        first.cache_write_5m = Some(0);
        p.ping_sent(&first, Some(anchor_at(at, wat)), false)
            .unwrap();
        assert_eq!(p.ttl(), CacheTtl::OneHour);
        assert_eq!(p.next_at(), at + 55 * MIN);
        let at = p.next_at();
        let wat = wat + 55 * MIN;
        assert_eq!(p.fire(at, wat, 0.0, None), Fire::Ping);
        p.ping_sent(&usage(OPUS, P, 0, 40, 1), Some(anchor_at(at, wat)), true)
            .unwrap();
        assert_eq!(p.ttl(), CacheTtl::FiveMinutes);
        assert_eq!(p.next_at(), at + Duration::from_secs(270));
    }

    // ── scheduling: 5m, hybrid, anchor, sleep ──

    #[test]
    fn five_minute_pings_are_scheduled_from_the_anchor() {
        let t0 = Instant::now();
        let w0 = SystemTime::UNIX_EPOCH + Duration::from_secs(5_000);
        let c = cfg(CacheKeepWarm::Auto, CacheTtl::FiveMinutes, OPUS);
        // The turn's last request was accepted at t0; idle a minute later.
        let idle = t0 + MIN;
        let mut p = Period::start(
            1,
            c,
            7,
            &last_read(OPUS),
            ev_1h(),
            anchor_at(t0, w0),
            idle,
            w0 + MIN,
        )
        .unwrap();
        assert_eq!(
            p.deadline(idle, w0 + MIN),
            Some(t0 + Duration::from_secs(270))
        );
        assert_eq!(
            p.fire(
                t0 + Duration::from_secs(200),
                w0 + Duration::from_secs(200),
                0.0,
                None
            ),
            Fire::NotYet
        );
        let at = t0 + Duration::from_secs(270);
        let wat = w0 + Duration::from_secs(270);
        assert_eq!(p.fire(at, wat, 0.0, None), Fire::Ping);
        assert_eq!(p.deadline(at, wat), None, "no second ping while in flight");
        assert_eq!(p.fire(at, wat, 0.0, None), Fire::NotYet);
        // The ping actually reached the provider 2s after firing.
        let sent = anchor_at(at + Duration::from_secs(2), wat + Duration::from_secs(2));
        p.ping_sent(&usage(OPUS, P, 0, 0, 1), Some(sent), false)
            .unwrap();
        assert_eq!(
            p.deadline(sent.mono, sent.wall),
            Some(sent.mono + Duration::from_secs(270))
        );
    }

    #[test]
    fn hybrid_is_governed_by_the_5m_history_marker() {
        assert_eq!(governing_ttl(CacheTtl::Hybrid), Duration::from_secs(300));
        assert_eq!(governing_ttl(CacheTtl::OneHour), Duration::from_secs(3600));
        assert_eq!(ping_interval(CacheTtl::Hybrid), Duration::from_secs(270));
        // Priced at the 5m write rate (the entry that lapses), not 1h.
        assert!((rewrite_cost_usd(OPUS, CacheTtl::Hybrid, P) - 1.875).abs() < 1e-9);
        let (pings, ..) = run(
            cfg(CacheKeepWarm::Auto, CacheTtl::Hybrid, OPUS),
            &[usage(OPUS, P, 0, 0, 0)],
        );
        assert_eq!(pings, 12);
    }

    #[test]
    fn a_turn_that_streamed_past_the_interval_pings_at_once_or_not_at_all() {
        let t0 = Instant::now();
        let w0 = SystemTime::UNIX_EPOCH + Duration::from_secs(9_000);
        let c = || cfg(CacheKeepWarm::Auto, CacheTtl::FiveMinutes, OPUS);
        let at = |s: u64| (t0 + Duration::from_secs(s), w0 + Duration::from_secs(s));
        let (idle, widle) = at(280);
        let p = Period::start(
            1,
            c(),
            7,
            &last_read(OPUS),
            ev_1h(),
            anchor_at(t0, w0),
            idle,
            widle,
        )
        .unwrap();
        assert_eq!(p.deadline(idle, widle), Some(idle), "due immediately");
        let (late, wlate) = at(290);
        let err = Period::start(
            1,
            c(),
            7,
            &last_read(OPUS),
            ev_1h(),
            anchor_at(t0, w0),
            late,
            wlate,
        )
        .unwrap_err();
        assert!(matches!(err, PeriodEnd::Expired { age_secs: 290 }), "{err}");
    }

    #[test]
    fn sleep_past_expiry_ends_the_period_without_pinging() {
        let t0 = Instant::now();
        let w0 = SystemTime::UNIX_EPOCH + Duration::from_secs(50_000);
        for ttl in [CacheTtl::FiveMinutes, CacheTtl::OneHour] {
            let c = cfg(CacheKeepWarm::Auto, ttl, OPUS);
            let mut p = start(c, &last_read(OPUS), t0, w0);
            let due = p.next_at();
            // Monotonic time says "on schedule"; the wall clock says the
            // laptop was closed for 3 hours.
            let wall = w0 + Duration::from_secs(3 * 3600);
            match p.fire(due, wall, 0.0, None) {
                Fire::End(PeriodEnd::Expired { age_secs }) => assert_eq!(age_secs, 3 * 3600),
                other => panic!("{ttl:?}: {other:?}"),
            }
        }
    }

    /// The machine sleeps 30 min starting at minute 10 of a 1h period. On
    /// wake the monotonic clock says minute 10 (deadline 45 min away), the
    /// wall clock says minute 40: the deadline moves to minute 55 wall, and
    /// the ping fires before the cache expires.
    #[test]
    fn a_suspend_pulls_the_deadline_forward_by_the_wall_clock() {
        let t0 = Instant::now();
        let w0 = SystemTime::UNIX_EPOCH + Duration::from_secs(60_000);
        let mut p = start(
            cfg(CacheKeepWarm::Auto, CacheTtl::OneHour, OPUS),
            &last_read(OPUS),
            t0,
            w0,
        );
        // Wake: monotonic minute 10, wall minute 40.
        let (now, wall) = (t0 + 10 * MIN, w0 + 40 * MIN);
        assert_eq!(p.deadline(now, wall), Some(now + 15 * MIN));
        assert_eq!(p.fire(now, wall, 0.0, None), Fire::NotYet);
        // 15 min later (monotonic minute 25, wall minute 55): due, not
        // expired (55 min + 15 s guard < 60 min).
        let (now, wall) = (now + 15 * MIN, wall + 15 * MIN);
        assert_eq!(p.fire(now, wall, 0.0, None), Fire::Ping);
        assert_eq!(p.fired_age(), 55 * MIN);
    }

    #[test]
    fn the_actor_polls_the_wall_clock_while_a_period_is_active() {
        assert!(WALL_POLL < safety_margin(CacheTtl::FiveMinutes));
        let t0 = Instant::now();
        let w0 = SystemTime::UNIX_EPOCH + Duration::from_secs(61_000);
        let mut kw = KeepWarmState::default();
        assert_eq!(kw.deadline_at(t0, w0), None, "no period: no timer");
        // A 1h period: the ping is 55 min away, the timer wakes at WALL_POLL.
        kw.period = Some(start(
            cfg(CacheKeepWarm::Auto, CacheTtl::OneHour, OPUS),
            &last_read(OPUS),
            t0,
            w0,
        ));
        assert_eq!(kw.deadline_at(t0, w0), Some(t0 + WALL_POLL));
        // Due sooner than WALL_POLL: the ping deadline itself.
        let (near, wnear) = (
            t0 + 55 * MIN - Duration::from_secs(3),
            w0 + 55 * MIN - Duration::from_secs(3),
        );
        assert_eq!(kw.deadline_at(near, wnear), Some(t0 + 55 * MIN));
        // In flight: no timer (the ping's completion wakes the actor).
        let p = kw.period.as_mut().unwrap();
        assert_eq!(p.fire(t0 + 55 * MIN, w0 + 55 * MIN, 0.0, None), Fire::Ping);
        assert_eq!(kw.deadline_at(t0 + 55 * MIN, w0 + 55 * MIN), None);
    }

    // ── robustness ──

    #[test]
    fn transient_errors_back_off_within_the_ttl_then_end() {
        let t0 = Instant::now();
        let w0 = SystemTime::UNIX_EPOCH + Duration::from_secs(70_000);
        let mut p = start(
            cfg(CacheKeepWarm::Auto, CacheTtl::OneHour, OPUS),
            &last_read(OPUS),
            t0,
            w0,
        );
        let mut now = p.next_at();
        let transient = PingFailure::Transient("HTTP 529".into());
        for want in BACKOFF {
            let wall = w0 + now.saturating_duration_since(t0);
            assert_eq!(p.fire(now, wall, 0.0, None), Fire::Ping);
            assert_eq!(p.ping_failed(&transient, now, wall), Ok(want));
            assert_eq!(p.deadline(now, wall), Some(now + want));
            now += want;
        }
        let wall = w0 + now.saturating_duration_since(t0);
        assert_eq!(p.fire(now, wall, 0.0, None), Fire::Ping);
        assert!(matches!(
            p.ping_failed(&transient, now, wall),
            Err(PeriodEnd::TransientExhausted(_))
        ));

        // 5m: at 270s only one 5s retry fits before the late guard.
        let mut p = start(
            cfg(CacheKeepWarm::Auto, CacheTtl::FiveMinutes, OPUS),
            &last_read(OPUS),
            t0,
            w0,
        );
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

    /// Failed pings that were billed count against the bound too.
    #[test]
    fn failed_ping_spend_is_in_the_ledger() {
        let (mut p, at, wat) = fired(cfg(CacheKeepWarm::Auto, CacheTtl::FiveMinutes, OPUS));
        p.charge(1.80);
        assert!(matches!(
            p.ping_failed(&PingFailure::Transient("x".into()), at, wat),
            Err(PeriodEnd::BoundReached { .. })
        ));
    }

    #[test]
    fn a_4xx_ends_the_period() {
        let (mut p, at, wat) = fired(cfg(CacheKeepWarm::Auto, CacheTtl::OneHour, OPUS));
        let err = p
            .ping_failed(&PingFailure::Fatal("HTTP 400".into()), at, wat)
            .unwrap_err();
        assert_eq!(err, PeriodEnd::Rejected("HTTP 400".into()));
    }

    #[test]
    fn hours_window_and_bound_whichever_first() {
        // 1h TTL, 1h window: one ping at 55 min, the next would be at 110.
        let (pings, _, _, reason, _) = run(
            cfg(CacheKeepWarm::Hours(1), CacheTtl::OneHour, OPUS),
            &[usage(OPUS, P, 0, 0, 0)],
        );
        assert_eq!((pings, reason), (1, PeriodEnd::HoursReached(1)));
        // 5m TTL, 2h window: the bound (12 pings, ~54 min) comes first.
        let (pings, _, _, reason, _) = run(
            cfg(CacheKeepWarm::Hours(2), CacheTtl::FiveMinutes, OPUS),
            &[usage(OPUS, P, 0, 0, 0)],
        );
        assert_eq!(pings, 12);
        assert!(matches!(reason, PeriodEnd::BoundReached { .. }));
        // A window shorter than the first interval never starts.
        let t0 = Instant::now();
        let w0 = SystemTime::UNIX_EPOCH;
        let mut c = cfg(CacheKeepWarm::Hours(1), CacheTtl::OneHour, OPUS);
        c.interval_override = Some(Duration::from_secs(2 * 3600));
        assert_eq!(
            Period::start(
                1,
                c,
                7,
                &last_read(OPUS),
                ev_1h(),
                anchor_at(t0, w0),
                t0,
                w0
            )
            .unwrap_err(),
            PeriodEnd::HoursReached(1)
        );
    }

    /// The interval override is reachable only from code (the
    /// `testing::IntervalOverride` guard, compiled under `cfg(test)` / the
    /// `testing` feature and called by no binary), never from the
    /// environment — so a dev `synaps` binary built by `cargo test` with the
    /// feature unified in cannot be told to ping faster.
    #[test]
    #[serial_test::serial(synaps_keepwarm_interval)]
    fn the_interval_override_is_set_only_by_code_never_by_the_environment() {
        let _env = crate::test_env::EnvVarGuard::set("SYNAPS_CACHE_KEEPWARM_INTERVAL_MS", "250");
        assert_eq!(interval_override(), None, "the old env var does nothing");
        {
            let _g = testing::IntervalOverride::set(Duration::from_millis(250));
            assert_eq!(interval_override(), Some(Duration::from_millis(250)));
        }
        assert_eq!(interval_override(), None, "cleared with the guard");
    }

    // ── cache epochs (I-1) ──

    /// BoundReached → `/keepwarm off` → `auto` in the same epoch: the
    /// ledger continues, so the re-enabled period does not start.
    #[test]
    fn a_re_enabled_period_continues_the_exhausted_ledger() {
        let t0 = Instant::now();
        let w0 = SystemTime::UNIX_EPOCH + Duration::from_secs(80_000);
        let c = || cfg(CacheKeepWarm::Auto, CacheTtl::OneHour, OPUS);
        let mut p = start(c(), &last_read(OPUS), t0, w0);
        let read = usage(OPUS, P, 0, 0, 0);
        let end = loop {
            let at = p.next_at();
            let wat = w0 + (at - t0);
            match p.fire(at, wat, 0.0, None) {
                Fire::Ping => {}
                Fire::End(r) => break r,
                Fire::NotYet => unreachable!(),
            }
            p.charge(read.cost_usd(OPUS));
            if let Err(r) = p.ping_sent(&read, Some(anchor_at(at, wat)), false) {
                break r;
            }
        };
        assert!(matches!(end, PeriodEnd::BoundReached { .. }), "{end}");
        let carry = p.epoch_carry();
        assert_eq!(carry.pings, 19);
        // Re-enabled right after the last ping (its anchor): still bound.
        let anchor = p.anchor;
        let err = Period::start_in_epoch(
            2,
            c(),
            7,
            &last_read(OPUS),
            ev_1h(),
            anchor,
            anchor.mono,
            anchor.wall,
            Some(&carry),
        )
        .unwrap_err();
        assert!(matches!(err, PeriodEnd::BoundReached { .. }), "{err}");
        // A new epoch (no carry) would have started afresh.
        assert!(Period::start_in_epoch(
            3,
            c(),
            7,
            &last_read(OPUS),
            ev_1h(),
            anchor,
            anchor.mono,
            anchor.wall,
            None
        )
        .is_ok());
    }

    /// Ping 1's write allowance belongs to the epoch: a period resumed after
    /// ping 1 never re-grants it, and the ledger carries over.
    #[test]
    fn ping_one_write_allowance_is_never_re_granted_in_an_epoch() {
        let t0 = Instant::now();
        let w0 = SystemTime::UNIX_EPOCH + Duration::from_secs(81_000);
        let c = || cfg(CacheKeepWarm::Auto, CacheTtl::OneHour, OPUS);
        let last = usage(OPUS, 250_000, 0, 0, 50_000);
        let mut p = start(c(), &last, t0, w0);
        let at = p.next_at();
        let wat = w0 + (at - t0);
        assert_eq!(p.fire(at, wat, 0.0, None), Fire::Ping);
        let first = usage(OPUS, 250_000, 50_000, 40, 1);
        p.charge(first.cost_usd(OPUS));
        p.ping_sent(&first, Some(anchor_at(at, wat)), false)
            .unwrap();
        let carry = p.epoch_carry();
        // `/keepwarm off` then `auto`: same epoch.
        let mut q = Period::start_in_epoch(
            2,
            c(),
            7,
            &last,
            ev_1h(),
            anchor_at(at, wat),
            at,
            wat,
            Some(&carry),
        )
        .unwrap();
        assert_eq!(q.pings(), 1);
        assert!((q.ledger_usd() - first.cost_usd(OPUS)).abs() < 1e-12);
        let at2 = q.next_at();
        let wat2 = wat + (at2 - at);
        assert_eq!(q.fire(at2, wat2, 0.0, None), Fire::Ping);
        // Writing the reply again is now a partial miss, not ping 1.
        assert_eq!(
            q.ping_sent(&usage(OPUS, 250_000, 50_000, 40, 1), None, false),
            Err(PeriodEnd::PartialMiss {
                written: 50_000,
                prefix: P
            })
        );
    }

    // ── auto's maximum period length (M-1) ──

    /// Pure 1h reads per cache-read multiplier: the ski-rental bound alone
    /// gives 19 (0.1×), 39 (0.05×) and 79 (0.025×) pings; `auto`'s 24h cap
    /// stops the latter two at 26 (the 27th would be due at 24h45m).
    #[test]
    fn ping_counts_per_multiplier_and_the_auto_cap() {
        for (model, uncapped, capped) in [
            ("claude-opus-4-7", 19, 19),
            ("claude-opus-5-5", 39, 26),
            ("claude-fable-5-1", 79, 26),
        ] {
            let mut c = cfg(CacheKeepWarm::Auto, CacheTtl::OneHour, model);
            let (pings, _, _, reason, elapsed) = run(c.clone(), &[usage(model, P, 0, 0, 0)]);
            assert_eq!(pings, uncapped, "{model}");
            assert!(
                matches!(reason, PeriodEnd::BoundReached { .. }),
                "{model}: {reason}"
            );
            assert_eq!(elapsed, uncapped * 55 * MIN, "{model}");
            c.auto_max_hours = 24;
            let (pings, _, _, reason, _) = run(c, &[usage(model, P, 0, 0, 0)]);
            assert_eq!(pings, capped, "{model}");
            if capped < uncapped {
                assert_eq!(reason, PeriodEnd::HoursReached(24), "{model}");
            }
        }
        // 5m: 12 (0.1×), 24 (0.05×), 49 (0.025×) pings, all well under 24h.
        for (model, want) in [
            ("claude-opus-4-7", 12),
            ("claude-opus-5-5", 24),
            ("claude-fable-5-1", 49),
        ] {
            let mut c = cfg(CacheKeepWarm::Auto, CacheTtl::FiveMinutes, model);
            c.auto_max_hours = 24;
            let (pings, ..) = run(c, &[usage(model, P, 0, 0, 0)]);
            assert_eq!(pings, want, "{model} 5m");
        }
        // An explicit <N>h window ignores the auto cap.
        let mut c = cfg(
            CacheKeepWarm::Hours(48),
            CacheTtl::OneHour,
            "claude-fable-5-1",
        );
        c.auto_max_hours = 24;
        let (pings, ..) = run(c, &[usage("claude-fable-5-1", P, 0, 0, 0)]);
        assert_eq!(pings, 52, "48h window: 52 pings at 55 min");
    }

    // ── ping 1's write allowance (N-3) ──

    #[test]
    fn first_write_counts_only_what_the_prefix_will_hold() {
        let t0 = Instant::now();
        let w0 = SystemTime::UNIX_EPOCH + Duration::from_secs(82_000);
        // The last request read 250k, sent 1k uncached and billed 40k
        // output — of which the reply text is ~2k; the rest was thinking,
        // which the API strips on resend.
        let last = usage(OPUS, 250_000, 0, 1_000, 40_000);
        let mut c = cfg(CacheKeepWarm::Auto, CacheTtl::OneHour, OPUS);
        let billed = start(c.clone(), &last, t0, w0);
        assert_eq!(billed.prefix_tokens(), 291_000, "without the estimate");
        c.reply_tokens = Some(2_000);
        let p = start(c.clone(), &last, t0, w0);
        assert_eq!(p.prefix_tokens(), 253_000);
        // Never more than was billed.
        c.reply_tokens = Some(90_000);
        assert_eq!(start(c, &last, t0, w0).prefix_tokens(), 291_000);
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
