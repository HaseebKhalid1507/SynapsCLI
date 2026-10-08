//! Prompt-cache keep-warm scheduling (task #444) — owned by the session actor.
//!
//! Why the actor: it is the one place that knows a session is idle (turn
//! finished, nothing queued, no compaction, no prompt, no driver), who is
//! attached, and when it parks or ends. The TUI only renders; subagents and
//! headless one-shot runs never have an interactive client attached, so they
//! never arm (see [`SessionActor::cache_keepwarm_blocker`]).
//!
//! Lifecycle (all times on the tokio clock):
//! - **arm** when the actor announces `Idle` after a turn that reached the
//!   provider: first ping at `last request start + 55 min` (1h TTL minus a
//!   5 min margin — the TTL counts from the START of the request that last
//!   read or wrote the entry);
//! - each successful ping re-schedules `ping start + 55 min`, as long as that
//!   is still inside `idle start + cache_keepwarm_hours`;
//! - **disarm** (and abort an in-flight ping) on any input command from a
//!   client, a turn start, the last interactive client detaching, park, session
//!   end, a spend cap, or any ping error. Nothing re-arms until the next idle.
//!
//! The ping itself is `Runtime::cache_keepwarm_ping` (`runtime::keepwarm`).

use std::time::Duration;

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::actor::SessionActor;
use super::types::ClientKind;
use crate::core::config::CacheTtl;
use crate::runtime::keepwarm::KeepWarmUsage;

/// 1h TTL minus a 5 min safety margin.
pub const PING_INTERVAL_1H: Duration = Duration::from_secs(55 * 60);

/// What `cache_keepwarm_hours` + `cache_ttl` resolve to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// `cache_keepwarm_hours = 0` (the default).
    Off,
    /// Configured, but the TTL makes it pointless; the reason is logged.
    Disabled(&'static str),
    On {
        cap: Duration,
        interval: Duration,
    },
}

impl Policy {
    pub fn resolve(hours: u32, ttl: CacheTtl) -> Self {
        if hours == 0 {
            return Self::Off;
        }
        match ttl {
            CacheTtl::OneHour => Self::On {
                cap: Duration::from_secs(
                    u64::from(hours.min(crate::core::config::MAX_CACHE_KEEPWARM_HOURS)) * 3600,
                ),
                interval: PING_INTERVAL_1H,
            },
            CacheTtl::FiveMinutes => Self::Disabled(
                "cache_keepwarm_hours is set but cache_ttl = 5m; keep-warm needs cache_ttl = 1h \
                 and stays off",
            ),
            CacheTtl::Hybrid => Self::Disabled(
                "cache_keepwarm_hours is set but cache_ttl = hybrid caches the history with the \
                 5m TTL; keep-warm needs cache_ttl = 1h and stays off",
            ),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Armed {
    idle_since: Instant,
    next_at: Instant,
    cap: Duration,
    interval: Duration,
    pings: u32,
    in_flight: bool,
}

/// Pure keep-warm timer state. Time is always passed in (`now`), so the whole
/// state machine is testable without sleeping.
#[derive(Debug, Default)]
pub struct Schedule {
    armed: Option<Armed>,
}

impl Schedule {
    /// Arm after the session went idle at `now`; the last provider request
    /// started at `last_request_at`. Returns whether a ping is scheduled
    /// (a cap shorter than the first ping schedules nothing).
    pub fn arm(&mut self, policy: Policy, now: Instant, last_request_at: Instant) -> bool {
        self.armed = None;
        let Policy::On { cap, interval } = policy else {
            return false;
        };
        let next_at = (last_request_at + interval).max(now);
        if next_at >= now + cap {
            return false;
        }
        self.armed = Some(Armed {
            idle_since: now,
            next_at,
            cap,
            interval,
            pings: 0,
            in_flight: false,
        });
        true
    }

    /// Stop. Returns whether anything was armed.
    pub fn disarm(&mut self) -> bool {
        self.armed.take().is_some()
    }

    pub fn is_armed(&self) -> bool {
        self.armed.is_some()
    }

    /// When the next ping is due; `None` while disarmed or a ping is in flight.
    pub fn deadline(&self) -> Option<Instant> {
        self.armed
            .as_ref()
            .filter(|a| !a.in_flight)
            .map(|a| a.next_at)
    }

    /// The timer fired at `now`: `true` = send a ping now (marks it in flight).
    pub fn fire(&mut self, now: Instant) -> bool {
        match self.armed.as_mut() {
            Some(a) if !a.in_flight && now >= a.next_at => {
                a.in_flight = true;
                true
            }
            _ => false,
        }
    }

    /// The ping that started at `started_at` succeeded. Schedules the next one
    /// if it still falls inside the cap; returns whether still armed.
    pub fn ping_succeeded(&mut self, started_at: Instant) -> bool {
        let Some(a) = self.armed.as_mut() else {
            return false;
        };
        a.in_flight = false;
        a.pings += 1;
        let next_at = started_at + a.interval;
        if next_at >= a.idle_since + a.cap {
            self.armed = None;
            return false;
        }
        a.next_at = next_at;
        true
    }

    /// Any ping error stops keep-warm until the next idle.
    pub fn ping_failed(&mut self) {
        self.armed = None;
    }

    /// Pings sent since the last arm.
    pub fn pings(&self) -> u32 {
        self.armed.as_ref().map_or(0, |a| a.pings)
    }
}

/// One in-flight ping.
pub(crate) struct PingTask {
    pub(crate) handle: tokio::task::JoinHandle<crate::Result<KeepWarmUsage>>,
    cancel: CancellationToken,
    started_at: Instant,
}

/// Actor-owned keep-warm state.
pub(crate) struct CacheKeepWarm {
    /// `cache_keepwarm_hours` at session create (0 = off).
    hours: u32,
    pub(crate) schedule: Schedule,
    /// Start of the newest provider request this turn (`ResponseStart`).
    last_request_at: Option<Instant>,
    pub(crate) task: Option<PingTask>,
    /// Ping spend this session (USD). Kept OUT of `ConversationState` so pings
    /// never touch persisted session state; counted against
    /// `max_session_cost` together with the session's own cost.
    spent_usd: f64,
    /// Last ineligibility reason logged (each distinct reason logs once).
    logged: Option<String>,
}

impl CacheKeepWarm {
    pub(crate) fn new(hours: u32) -> Self {
        Self {
            hours,
            schedule: Schedule::default(),
            last_request_at: None,
            task: None,
            spent_usd: 0.0,
            logged: None,
        }
    }

    fn log_once(&mut self, session: &str, reason: &str) {
        if self.logged.as_deref() != Some(reason) {
            tracing::info!(session, "cache keep-warm off: {reason}");
            self.logged = Some(reason.to_string());
        }
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
) -> std::result::Result<crate::Result<KeepWarmUsage>, tokio::task::JoinError> {
    match task {
        Some(t) => (&mut t.handle).await,
        None => std::future::pending().await,
    }
}

impl SessionActor {
    /// Why keep-warm must not run right now (`None` = clear to ping).
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
        // Subagents, headless one-shot runs, scripts and RPC clients never
        // attach as Tui/Attach: an unattended session is not kept warm.
        if !self
            .attached
            .values()
            .any(|c| matches!(c.meta.kind, ClientKind::Tui | ClientKind::Attach))
        {
            return Some("no interactive client attached");
        }
        if let Some(cap) = self.config.max_session_cost {
            if cap.is_finite() && self.conv.session_cost + self.cache_keepwarm.spent_usd >= cap {
                return Some("max_session_cost reached");
            }
        }
        None
    }

    /// A provider response started (stream `ResponseStart`).
    pub(crate) fn cache_keepwarm_note_request(&mut self) {
        self.cache_keepwarm.last_request_at = Some(Instant::now());
    }

    /// A turn (or compaction) is starting: stop pinging and forget the
    /// previous request's timing — only a new provider request re-enables it.
    pub(crate) fn cache_keepwarm_reset(&mut self, why: &str) {
        self.cache_keepwarm_disarm(why);
        self.cache_keepwarm.last_request_at = None;
    }

    /// Stop pinging (no-op when idle-off).
    pub(crate) fn cache_keepwarm_disarm(&mut self, why: &str) {
        let had_task = self.cache_keepwarm.task.is_some();
        self.cache_keepwarm.abort_ping();
        if self.cache_keepwarm.schedule.disarm() || had_task {
            tracing::info!(session = %self.id, "cache keep-warm stopped: {why}");
        }
    }

    /// Disarm if something now blocks keep-warm (e.g. a client detached).
    pub(crate) fn cache_keepwarm_recheck(&mut self) {
        if !self.cache_keepwarm.schedule.is_armed() && self.cache_keepwarm.task.is_none() {
            return;
        }
        if let Some(why) = self.cache_keepwarm_blocker() {
            self.cache_keepwarm_disarm(why);
        }
    }

    /// The session just announced `Idle`: arm if configured and eligible.
    pub(crate) fn cache_keepwarm_arm(&mut self) {
        if self.cache_keepwarm.hours == 0 {
            return;
        }
        // Only a turn that actually reached the provider wrote a cache entry.
        let Some(last_request_at) = self.cache_keepwarm.last_request_at else {
            return;
        };
        if self.cache_keepwarm_blocker().is_some()
            || !crate::runtime::keepwarm::history_ready(&self.conv.api_messages)
        {
            return;
        }
        let session = self.id.to_string();
        let policy = Policy::resolve(self.cache_keepwarm.hours, self.runtime.cache_ttl());
        if let Policy::Disabled(reason) = policy {
            self.cache_keepwarm.log_once(&session, reason);
            return;
        }
        if let Some(reason) = self.runtime.cache_keepwarm_ineligibility() {
            self.cache_keepwarm.log_once(&session, &reason);
            return;
        }
        self.cache_keepwarm.abort_ping();
        let now = Instant::now();
        if self
            .cache_keepwarm
            .schedule
            .arm(policy, now, last_request_at)
        {
            let first = self
                .cache_keepwarm
                .schedule
                .deadline()
                .map_or(0, |t| t.saturating_duration_since(now).as_secs());
            tracing::info!(
                session = %session,
                hours = self.cache_keepwarm.hours,
                first_ping_in_secs = first,
                "cache keep-warm armed"
            );
        }
    }

    /// `select!` arm: a ping is due.
    pub(crate) fn on_cache_keepwarm_due(&mut self) {
        if !self.cache_keepwarm.schedule.fire(Instant::now()) {
            return;
        }
        if let Some(why) = self.cache_keepwarm_blocker() {
            self.cache_keepwarm_disarm(why);
            return;
        }
        if let Some(reason) = self.runtime.cache_keepwarm_ineligibility() {
            self.cache_keepwarm_disarm(&reason);
            return;
        }
        // A detached clone: the actor keeps serving clients while the ping
        // runs, and any input aborts it (`cache_keepwarm_disarm`).
        let history = self.conv.api_messages.clone();
        let mut runtime = self.runtime.clone();
        runtime.share_ttl_latches(&self.runtime);
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let handle =
            tokio::spawn(async move { runtime.cache_keepwarm_ping(&history, &token).await });
        self.cache_keepwarm.task = Some(PingTask {
            handle,
            cancel,
            started_at: Instant::now(),
        });
    }

    /// `select!` arm: the in-flight ping finished.
    pub(crate) fn on_cache_keepwarm_done(
        &mut self,
        result: std::result::Result<crate::Result<KeepWarmUsage>, tokio::task::JoinError>,
    ) {
        let Some(task) = self.cache_keepwarm.task.take() else {
            return;
        };
        match result {
            Ok(Ok(usage)) => {
                let cost = usage.cost_usd(self.runtime.model());
                self.cache_keepwarm.spent_usd += cost;
                let still_armed = self.cache_keepwarm.schedule.ping_succeeded(task.started_at);
                tracing::info!(
                    session = %self.id,
                    keepwarm = true,
                    cache_read = usage.cache_read_tokens,
                    cache_write = usage.cache_write_tokens,
                    input = usage.input_tokens,
                    output = usage.output_tokens,
                    cost_usd = cost,
                    session_keepwarm_usd = self.cache_keepwarm.spent_usd,
                    still_armed,
                    "cache keep-warm ping sent"
                );
                if !still_armed {
                    tracing::info!(session = %self.id, "cache keep-warm stopped: cap reached");
                }
            }
            Ok(Err(e)) => {
                self.cache_keepwarm.schedule.ping_failed();
                tracing::warn!(session = %self.id, "cache keep-warm stopped: ping failed: {e}");
            }
            Err(e) => {
                self.cache_keepwarm.schedule.ping_failed();
                if !e.is_cancelled() {
                    tracing::warn!(session = %self.id, "cache keep-warm stopped: ping task: {e}");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: Duration = Duration::from_secs(60);

    fn on(hours: u64) -> Policy {
        Policy::On {
            cap: Duration::from_secs(hours * 3600),
            interval: PING_INTERVAL_1H,
        }
    }

    #[test]
    fn off_by_default_and_only_with_the_1h_ttl() {
        let defaults = crate::config::SynapsConfig::default();
        assert_eq!(
            Policy::resolve(defaults.cache_keepwarm_hours, defaults.cache_ttl),
            Policy::Off
        );
        assert_eq!(Policy::resolve(0, CacheTtl::OneHour), Policy::Off);
        assert!(matches!(
            Policy::resolve(4, CacheTtl::FiveMinutes),
            Policy::Disabled(r) if r.contains("5m")
        ));
        assert!(matches!(
            Policy::resolve(4, CacheTtl::Hybrid),
            Policy::Disabled(r) if r.contains("hybrid")
        ));
        assert_eq!(Policy::resolve(4, CacheTtl::OneHour), on(4));
        assert_eq!(
            Policy::resolve(1000, CacheTtl::OneHour),
            on(u64::from(crate::core::config::MAX_CACHE_KEEPWARM_HOURS))
        );
    }

    #[test]
    fn non_on_policies_never_arm() {
        let t0 = Instant::now();
        let mut s = Schedule::default();
        assert!(!s.arm(Policy::Off, t0, t0));
        assert!(!s.arm(Policy::Disabled("x"), t0, t0));
        assert_eq!(s.deadline(), None);
        assert!(!s.fire(t0 + 10 * PING_INTERVAL_1H));
    }

    #[test]
    fn fires_at_ttl_minus_margin_from_the_last_request() {
        let t0 = Instant::now();
        let mut s = Schedule::default();
        // The last request started 2 min before the turn went idle.
        let idle = t0 + 2 * MIN;
        assert!(s.arm(on(4), idle, t0));
        assert_eq!(s.deadline(), Some(t0 + 55 * MIN));
        assert!(!s.fire(t0 + 54 * MIN), "not before TTL minus margin");
        assert!(s.fire(t0 + 55 * MIN));
        assert_eq!(
            s.deadline(),
            None,
            "no second timer while a ping is in flight"
        );
        assert!(!s.fire(t0 + 56 * MIN), "never two pings at once");
    }

    #[test]
    fn a_stale_last_request_pings_immediately() {
        let t0 = Instant::now();
        let mut s = Schedule::default();
        let idle = t0 + 58 * MIN; // a very long final response
        assert!(s.arm(on(4), idle, t0));
        assert_eq!(s.deadline(), Some(idle));
    }

    #[test]
    fn stops_at_the_cap() {
        let t0 = Instant::now();
        let mut s = Schedule::default();
        assert!(s.arm(on(4), t0, t0));
        let mut sent = Vec::new();
        while let Some(at) = s.deadline() {
            assert!(s.fire(at));
            sent.push(at.duration_since(t0));
            s.ping_succeeded(at);
            assert!(sent.len() <= 10, "runaway schedule");
        }
        assert_eq!(
            sent,
            vec![55 * MIN, 110 * MIN, 165 * MIN, 220 * MIN],
            "4h cap: four pings, the last one keeps the cache warm to ~4h35m"
        );
        assert!(!s.is_armed());
        assert!(!s.fire(t0 + 300 * MIN));
    }

    #[test]
    fn one_hour_cap_pings_once() {
        let t0 = Instant::now();
        let mut s = Schedule::default();
        assert!(s.arm(on(1), t0, t0));
        assert!(s.fire(t0 + 55 * MIN));
        assert!(
            !s.ping_succeeded(t0 + 55 * MIN),
            "next ping would land past the cap"
        );
        assert_eq!(s.deadline(), None);
    }

    #[test]
    fn user_input_stops_it() {
        let t0 = Instant::now();
        let mut s = Schedule::default();
        assert!(s.arm(on(4), t0, t0));
        assert!(s.disarm(), "input disarms");
        assert_eq!(s.deadline(), None);
        assert!(!s.fire(t0 + 55 * MIN));
        // A late completion of an aborted ping cannot resurrect it.
        assert!(!s.ping_succeeded(t0 + 55 * MIN));
        assert!(!s.is_armed());
    }

    #[test]
    fn user_input_during_a_ping_stops_it() {
        let t0 = Instant::now();
        let mut s = Schedule::default();
        assert!(s.arm(on(4), t0, t0));
        assert!(s.fire(t0 + 55 * MIN));
        assert!(s.disarm());
        assert!(!s.ping_succeeded(t0 + 55 * MIN));
        assert_eq!(s.deadline(), None);
    }

    #[test]
    fn any_error_stops_it() {
        let t0 = Instant::now();
        let mut s = Schedule::default();
        assert!(s.arm(on(4), t0, t0));
        assert!(s.fire(t0 + 55 * MIN));
        s.ping_failed();
        assert!(!s.is_armed());
        assert_eq!(s.deadline(), None);
    }

    #[test]
    fn rearming_resets_the_window() {
        let t0 = Instant::now();
        let mut s = Schedule::default();
        assert!(s.arm(on(4), t0, t0));
        assert!(s.fire(t0 + 55 * MIN));
        assert!(s.ping_succeeded(t0 + 55 * MIN));
        assert_eq!(s.pings(), 1);
        // A new turn ends at t1: a fresh window, counted from t1.
        let t1 = t0 + 70 * MIN;
        assert!(s.arm(on(4), t1, t1));
        assert_eq!(s.pings(), 0);
        assert_eq!(s.deadline(), Some(t1 + 55 * MIN));
    }
}
