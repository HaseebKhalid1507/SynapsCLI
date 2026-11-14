//! Keep the prompt cache warm while a provider round runs
//! (`cache_keepwarm_tools`).
//!
//! A provider round that asks for tools is followed, once they return, by a
//! request that re-sends that round's whole prefix. The cache entry that
//! request reads lives for one TTL from the START of the round's request
//! (workers are pinned to 5m), and a round can outlast it twice over:
//!
//! - **while its response streams.** One long generation is enough:
//!   minutes of thinking before a short tool call, or a 30k-token `write`.
//! - **while its tools run.** Builds, test sweeps and `sleep; poll` loops
//!   routinely take that long.
//!
//! Either way the entry has expired by the time the next request goes out,
//! and that request rewrites the whole prefix at the write price (1.25× on
//! 5m) instead of reading it (0.1×, 0.05× on the 5.5 models).
//!
//! From the moment a round's request is sent, this task re-sends **that
//! round's own request** — the exact request messages, tools, system prompt,
//! thinking config and options it went out with — plus one trailing ping
//! text block (`KeepWarmMode::Replay`, appended by the transport after
//! marker placement), shortly before the TTL lapses, and again after every
//! accepted ping, until the tools return, the response ends without asking
//! for any, the turn is cancelled or the stream ends (the guard drops). It
//! is a keep-warm ping in every other respect (`runtime::keepwarm`):
//! `ApiOptions::keepwarm` cuts `max_tokens`, disables retries and marks the
//! telemetry record `"keepwarm": true`; the transport holds the ping text
//! out of marker placement, so every breakpoint lands on the block the round
//! itself marked and the ping is an uncached tail. A read refreshes the
//! entry's TTL, so the request after the round reads the prefix the round
//! cached. Nothing is persisted and no history is touched.
//!
//! Policy (the idle keep-warm's, `session::cache_keepwarm`, minus most of
//! the idle guesswork). While the tools run, the next request is certain
//! unless the turn is cancelled. While the response streams it is not: the
//! response may be the turn's last word. But a response that outlasts the
//! TTL has lost its cache by the time it ends, whatever reads the prefix
//! next (its tool results, a resume of the worker, the user's next
//! message), and what a long response does first is think: the tool call
//! comes at the end, too late to wait for. So the response is pinged as
//! the tools are, under the same bound:
//! - eligible only on the native Anthropic transport and when the round's
//!   request cached something. While the response streams that is the
//!   prefix the provider reports in `message_start`, passed on by the
//!   transport ([`RoundSignal`]): the round's own usage is not booked until
//!   the response ends;
//! - due at `anchor + ping_interval(honoured TTL)` (4.5 min on 5m, 55 min on
//!   1h) on whichever clock comes first; never once the cache is within
//!   `LATE_GUARD` of expiry (it is presumed gone: the next request rewrites
//!   it anyway, a ping would only add a write);
//! - ski-rental bound: a ping is sent only while `ledger + expected ping ≤
//!   rewriting the prefix at the honoured TTL`;
//! - the turn's `max_cost_usd` (if any) must admit the ping's worst case.
//!   While the response streams, the round's own request is not booked yet:
//!   it is counted at the most its input side can cost, a full write of the
//!   prefix;
//! - stop on a ping that read nothing, wrote or paid uncached for more than
//!   `SHARE_LIMIT_PERCENT` of the prefix, a 4xx, or exhausted transient
//!   retries (`BACKOFF`).
//!
//! Stopping. When the tools return, no new ping is scheduled, but a ping
//! already in flight is allowed to finish — for up to [`FINISH_GRACE`] —
//! and is booked like any other before the next request goes out. Only a
//! turn cancel, the stream ending, or the grace running out aborts one in
//! flight. Either way the ping is settled before the round's next request
//! is sent, so its usage never lands after that request's. (Provider-side
//! order would not matter anyway: an aborted ping and the next request read
//! the same prefix, and a ping writes nothing — its tail is uncached.) A
//! response that ends without asking for tools ends the round and its
//! pings: one in flight is aborted and booked before the turn goes on. (A
//! ping it got while streaming is spent on a prefix that only a resume or
//! the next turn can still read; the idle keep-warm, where one runs,
//! schedules from the anchor that ping left.)
//!
//! Booking. Every ping that may have been billed is booked into the turn,
//! by the idle keep-warm's rule (`keepwarm::ping_booking`): with measured
//! usage the transport records it in the turn's usage counters (turn
//! budget) and its `Usage` event is forwarded to the turn's event channel
//! (session / worker cost); a ping dispatched with no usage seen (aborted in
//! flight, a read timeout) is booked at its worst case — the whole prefix
//! read, [`PING_INPUT_ESTIMATE`] uncached, `max_tokens` out — through both;
//! one refused before the network or rejected with an HTTP error books
//! nothing. A ping sent while the response streams is booked when it
//! completes, normally before the round's own usage; both are sums, so the
//! order does not matter to them. `UsageCounters::latest_context_tokens` is
//! the one last-writer field: a ping that completes in the instant between
//! the round's own booking and the stream loop's read leaves its own
//! footprint there, which is the round's request plus the ping text (~40
//! tokens).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde_json::Value;
use tokio::sync::{mpsc, RwLock};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::api::{ApiMethods, ApiOptions, KeepWarmMode, KeepWarmRequest};
use super::keepwarm::{CacheAnchor, PingBooking, PingFailure, PingTap};
use super::types::{AuthState, SessionEvent, StreamEvent};
use crate::core::config::CacheTtl;
use crate::session::cache_keepwarm::{
    governing_ttl, honoured_ttl, ping_interval, ping_output_estimate, request_cost_usd,
    rewrite_cost_usd, TtlEvidence, BACKOFF, LATE_GUARD, PING_INPUT_ESTIMATE, SHARE_LIMIT_PERCENT,
    WALL_POLL,
};
use crate::{SharedMessage, ToolRegistry};

/// How long the end of a tool round waits for a ping already in flight
/// before aborting it (and booking it at its worst case). Longer than every
/// keep-warm ping in the api-log so far (122 real pings: p50 1.9 s, p95
/// 11 s, max 15.8 s): the wait only happens when a ping straddles the end of
/// the round (a few seconds out of every 270 s interval), and it delays the
/// next request by at most the rest of that ping.
pub const FINISH_GRACE: Duration = Duration::from_secs(20);

/// Per-turn settings: present only when `cache_keepwarm_tools` is on.
#[derive(Debug, Clone, Default)]
pub struct ToolKeepWarm {
    /// The runtime's base trace context: a ping is traced like an idle
    /// keep-warm ping and never consumes a one-shot `/trace next`.
    pub trace: super::trace::TraceContext,
    /// Replaces `ping_interval(honoured TTL)` (tests only, see
    /// [`interval_override`]).
    pub interval_override: Option<Duration>,
    /// Replaces [`FINISH_GRACE`] (tests only).
    #[cfg(test)]
    pub finish_grace: Option<Duration>,
    /// Notified when the end of a round starts waiting for a ping in flight
    /// (tests only: the sync point that releases a held ping).
    #[cfg(test)]
    pub finish_hook: Option<Arc<tokio::sync::Notify>>,
}

impl ToolKeepWarm {
    pub fn new(trace: super::trace::TraceContext, interval_override: Option<Duration>) -> Self {
        Self {
            trace,
            interval_override,
            #[cfg(test)]
            finish_grace: None,
            #[cfg(test)]
            finish_hook: None,
        }
    }

    fn finish_grace(&self) -> Duration {
        #[cfg(test)]
        if let Some(grace) = self.finish_grace {
            return grace;
        }
        FINISH_GRACE
    }
}

/// The ping interval override, in ms (0 = none). Test-only, like the idle
/// keep-warm's: written solely by [`testing::IntervalOverride`].
#[cfg(any(test, feature = "testing"))]
static INTERVAL_OVERRIDE_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Test seam replacing `ping_interval(honoured TTL)` for tool-round pings.
/// Separate from the idle keep-warm's seam so tests of one never fire the
/// other. A constant `None` without `cfg(test)` / the `testing` feature; no
/// binary input (env, config, argv) can reach it.
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

    /// Shortens the tool-round ping interval for every runtime in this
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
}

/// Taken immediately before a round's provider call.
#[derive(Debug, Clone, Copy)]
pub(super) struct RoundMark {
    sent: CacheAnchor,
    cached_before: u64,
}

fn cached_total(counters: &super::budget::UsageCounters) -> u64 {
    let (_, _, read, write) = counters.totals();
    read.saturating_add(write)
}

impl RoundMark {
    pub(super) fn new(counters: &super::budget::UsageCounters) -> Self {
        Self {
            sent: CacheAnchor::now(),
            cached_before: cached_total(counters),
        }
    }
}

/// What a round's own request tells its keep-warm while the response is
/// still streaming ([`ApiOptions::round_signal`]): written by the transport
/// as the response starts, and by the stream loop once it is in; read by
/// the round's task. One per round, never shared with a ping (a ping's own
/// `message_start` must not pass for the round's).
#[derive(Debug, Default)]
pub struct RoundSignal {
    /// Tokens the provider read from or wrote to the cache for the round's
    /// request (`message_start`). 0 until reported: nothing to keep warm.
    cached_tokens: AtomicU64,
    /// The response is in and asks for tools.
    response_in: AtomicBool,
    /// Wakes the round's task (its one waiter) when the above change.
    changed: tokio::sync::Notify,
}

impl RoundSignal {
    /// `message_start` of an attempt of the round's request: the size of
    /// its cached prefix (a retried attempt reports its own).
    pub(super) fn message_started(&self, cached_tokens: u64) {
        self.cached_tokens.store(cached_tokens, Ordering::Release);
        self.changed.notify_one();
    }

    /// The response is in and asks for tools. `booked` is what the turn's
    /// counters say the round cached, for a provider whose `message_start`
    /// carried no usage (no ping can have gone out by then: one needs a
    /// reported prefix). Returns whether there is a prefix to keep warm.
    fn tools_running(&self, booked: u64) -> bool {
        let _ = self
            .cached_tokens
            .compare_exchange(0, booked, Ordering::AcqRel, Ordering::Acquire);
        self.response_in.store(true, Ordering::Release);
        self.changed.notify_one();
        self.cached_tokens() > 0
    }

    pub(super) fn cached_tokens(&self) -> u64 {
        self.cached_tokens.load(Ordering::Acquire)
    }

    fn response_in(&self) -> bool {
        self.response_in.load(Ordering::Acquire)
    }
}

/// Whether a provider response asks for tools.
pub(super) fn has_tool_use(response: &Value) -> bool {
    response["content"]
        .as_array()
        .is_some_and(|blocks| blocks.iter().any(|b| b["type"] == "tool_use"))
}

/// Everything one round's request went out with (borrowed from the stream
/// loop; [`ToolRoundKeepWarm::start`] copies what it keeps — Arc bumps for
/// the messages).
pub(super) struct Round<'a> {
    pub auth: &'a Arc<RwLock<AuthState>>,
    pub client: &'a reqwest::Client,
    pub model: &'a str,
    pub tools: &'a ToolRegistry,
    pub system_prompt: &'a Option<String>,
    pub thinking_budget: u32,
    pub reasoning_level: agent_core::reasoning::ReasoningLevel,
    pub messages: &'a [SharedMessage],
    pub options: &'a ApiOptions,
    pub telemetry_level: super::telemetry::TelemetryLevel,
}

/// The turn the round belongs to.
pub(super) struct Turn<'a> {
    pub usage: &'a Arc<super::budget::UsageCounters>,
    pub max_cost_usd: Option<f64>,
    pub tx: &'a mpsc::UnboundedSender<StreamEvent>,
    pub cancel: &'a CancellationToken,
}

/// Why the pings of a round stopped (logged once).
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Stop {
    /// The tools returned, the response asked for none, the turn was
    /// cancelled or the stream ended.
    Done,
    /// Within `LATE_GUARD` of expiry (or past it): presumed expired.
    Expired,
    /// The next ping would pass the rewrite it saves.
    Bound,
    /// The turn's `max_cost_usd` would not admit the next ping.
    TurnCost,
    /// A ping read nothing, or wrote / paid uncached for a large share.
    Miss,
    /// 4xx / configuration / exhausted transient retries.
    Failed(String),
}

impl std::fmt::Display for Stop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Stop::Done => f.write_str("round finished"),
            Stop::Expired => f.write_str("cache presumed expired"),
            Stop::Bound => f.write_str("next ping would pass the rewrite it saves"),
            Stop::TurnCost => f.write_str("turn max_cost_usd reached"),
            Stop::Miss => f.write_str("ping missed the cache"),
            Stop::Failed(why) => write!(f, "ping failed: {why}"),
        }
    }
}

/// What the task needs to send and schedule pings.
struct Job {
    auth: Arc<RwLock<AuthState>>,
    client: reqwest::Client,
    model: String,
    tools: ToolRegistry,
    system_prompt: Option<String>,
    thinking_budget: u32,
    reasoning_level: agent_core::reasoning::ReasoningLevel,
    /// The round's request messages, exactly (the transport appends the
    /// ping text after marker placement, `KeepWarmMode::Replay`).
    messages: Vec<SharedMessage>,
    options: ApiOptions,
    telemetry_level: super::telemetry::TelemetryLevel,
    /// The earliest the cache entry can have been written or last
    /// refreshed: the round's send, then each accepted ping's.
    floor: CacheAnchor,
    /// Stamped by the round's request (every accepted attempt) and by the
    /// pings: the latest stamp at or after `floor` is the anchor.
    anchor_cell: Arc<super::keepwarm::CacheAnchorCell>,
    signal: Arc<RoundSignal>,
    /// The configured TTL for the turn's credentials; what the server
    /// honours is read from the session's latches at every ping
    /// ([`Job::ttl`]): the round's own response can be the first evidence.
    configured_ttl: CacheTtl,
    interval_override: Option<Duration>,
    max_tokens: u64,
    usage: Arc<super::budget::UsageCounters>,
    max_cost_usd: Option<f64>,
    tx: mpsc::UnboundedSender<StreamEvent>,
}

/// The task's controls: `stop` ends the schedule (no new ping), `abort`
/// (a child of the turn's cancel) also aborts a ping in flight.
struct Controls {
    stop: CancellationToken,
    abort: CancellationToken,
    in_flight: Arc<AtomicBool>,
}

/// Whether the turn's `max_cost_usd` admits a ping: what is booked, plus
/// the round's own request while it is not booked yet (`unbooked_usd`),
/// plus the ping's worst case. No cap, or a non-finite one, admits it.
fn turn_admits(cap: Option<f64>, booked_usd: f64, unbooked_usd: f64, worst_usd: f64) -> bool {
    match cap.filter(|c| c.is_finite()) {
        Some(cap) => booked_usd + unbooked_usd + worst_usd <= cap,
        None => true,
    }
}

/// Running keep-warm for one round, started as its request goes out.
/// [`tools_running`](Self::tools_running) carries it from the response into
/// the tool calls, [`finish`](Self::finish) stops it gracefully once they
/// return, [`abandon`](Self::abandon) ends it when the response asks for
/// none; dropping it (any other way out of the round) aborts a ping in
/// flight, which the task still books.
pub(super) struct ToolRoundKeepWarm {
    signal: Arc<RoundSignal>,
    cached_before: u64,
    stop: CancellationToken,
    abort: CancellationToken,
    in_flight: Arc<AtomicBool>,
    grace: Duration,
    #[cfg(test)]
    hook: Option<Arc<tokio::sync::Notify>>,
    handle: Option<tokio::task::JoinHandle<Stop>>,
}

impl Drop for ToolRoundKeepWarm {
    fn drop(&mut self) {
        self.stop.cancel();
        self.abort.cancel();
    }
}

impl ToolRoundKeepWarm {
    /// Start keeping `round`'s prefix warm, as its request goes out (`mark`
    /// was taken just before, `signal` rides in the request's options), or
    /// `None` when it cannot be: not the native Anthropic transport.
    /// Nothing is sent until the transport reports a cached prefix
    /// ([`RoundSignal`]).
    pub(super) async fn start(
        settings: &ToolKeepWarm,
        mark: RoundMark,
        signal: &Arc<RoundSignal>,
        round: Round<'_>,
        turn: Turn<'_>,
    ) -> Option<Self> {
        if !super::auth::model_is_anthropic(round.model) {
            return None;
        }
        let auth_type = round.auth.read().await.auth_type.clone();
        let configured_ttl = round.options.cache_ttl.resolve_for_auth_type(&auth_type);
        // The round's request and the pings stamp the round's anchor cell
        // (or the pings a private one): every accepted request moves the
        // anchor the next ping is scheduled from.
        let anchor_cell = round.options.cache_anchor.clone().unwrap_or_default();
        let messages = round.messages.to_vec();
        let options = ApiOptions {
            // A ping runs no tool: nothing of the chat's goes with it.
            tool_context: None,
            trace: settings.trace.clone(),
            request_correlation: None,
            suppress_stream_deltas: true,
            usage_counters: Some(Arc::clone(turn.usage)),
            cache_anchor: Some(Arc::clone(&anchor_cell)),
            context_rollover: None,
            keepwarm: None,
            // A ping reports nothing about the round.
            round_signal: None,
            ..round.options.clone()
        };
        let max_tokens = match round.thinking_budget {
            0 => super::keepwarm::KEEPWARM_OUTPUT_TOKENS,
            budget => u64::from(budget) + super::keepwarm::KEEPWARM_OUTPUT_TOKENS,
        };
        let job = Job {
            auth: Arc::clone(round.auth),
            client: round.client.clone(),
            model: round.model.to_string(),
            tools: round.tools.clone(),
            system_prompt: round.system_prompt.clone(),
            thinking_budget: round.thinking_budget,
            reasoning_level: round.reasoning_level,
            messages,
            options,
            telemetry_level: round.telemetry_level,
            floor: mark.sent,
            anchor_cell,
            signal: Arc::clone(signal),
            configured_ttl,
            interval_override: settings.interval_override,
            max_tokens,
            usage: Arc::clone(turn.usage),
            max_cost_usd: turn.max_cost_usd,
            tx: turn.tx.clone(),
        };
        let controls = Controls {
            stop: CancellationToken::new(),
            abort: turn.cancel.child_token(),
            in_flight: Arc::default(),
        };
        let (stop, abort, in_flight) = (
            controls.stop.clone(),
            controls.abort.clone(),
            Arc::clone(&controls.in_flight),
        );
        let handle = tokio::spawn(job.run(controls));
        Some(Self {
            signal: Arc::clone(signal),
            cached_before: mark.cached_before,
            stop,
            abort,
            in_flight,
            grace: settings.finish_grace(),
            #[cfg(test)]
            hook: settings.finish_hook.clone(),
            handle: Some(handle),
        })
    }

    /// The round's response is in and asks for tools: its prefix is kept
    /// warm while they run. `false` when its request cached nothing (there
    /// is nothing to keep: [`abandon`](Self::abandon) it).
    pub(super) fn tools_running(&self, usage: &super::budget::UsageCounters) -> bool {
        self.signal
            .tools_running(cached_total(usage).saturating_sub(self.cached_before))
    }

    /// The round is over with no tools to run (the response asked for none,
    /// or was cut by a cancel): nothing in this turn reads its prefix next.
    /// Schedule no new ping, abort one in flight, and wait until it is
    /// booked.
    pub(super) async fn abandon(mut self) -> Stop {
        self.stop.cancel();
        self.abort.cancel();
        match self.handle.take() {
            Some(handle) => handle.await.unwrap_or(Stop::Done),
            None => Stop::Done,
        }
    }

    /// The round's tools are done: schedule no new ping, let one in flight
    /// finish (up to the grace, then abort it), and wait until it is booked.
    /// Returns why the pings stopped.
    pub(super) async fn finish(mut self) -> Stop {
        self.stop.cancel();
        let Some(mut handle) = self.handle.take() else {
            return Stop::Done;
        };
        if self.in_flight.load(Ordering::Acquire) {
            tracing::debug!(
                event = "tool_round_keepwarm_drain",
                "tool round done with a keep-warm ping in flight; letting it finish"
            );
            #[cfg(test)]
            if let Some(hook) = &self.hook {
                hook.notify_one();
            }
        }
        match tokio::time::timeout(self.grace, &mut handle).await {
            Ok(stop) => stop.unwrap_or(Stop::Done),
            Err(_) => {
                tracing::info!(
                    event = "tool_round_keepwarm_grace_expired",
                    grace_ms = self.grace.as_millis() as u64,
                    "tool-round keep-warm ping still in flight at the end of the round; aborting"
                );
                self.abort.cancel();
                handle.await.unwrap_or(Stop::Done)
            }
        }
    }
}

fn over_share(tokens: u64, prefix: u64) -> bool {
    tokens.saturating_mul(100) > prefix.saturating_mul(SHARE_LIMIT_PERCENT)
}

impl Job {
    /// When the cache entry was last written or refreshed, as far as this
    /// side can tell (never later than the provider's own request start).
    fn anchor(&self) -> CacheAnchor {
        self.anchor_cell
            .get()
            .filter(|a| a.mono >= self.floor.mono)
            .unwrap_or(self.floor)
    }

    fn elapsed_since(anchor: CacheAnchor) -> Duration {
        let mono = Instant::now().saturating_duration_since(anchor.mono);
        let wall = SystemTime::now()
            .duration_since(anchor.wall)
            .unwrap_or_default();
        mono.max(wall)
    }

    fn elapsed(&self) -> Duration {
        Self::elapsed_since(self.anchor())
    }

    /// The TTL the server honours for this session, on the evidence so far.
    fn ttl(&self) -> CacheTtl {
        honoured_ttl(
            self.configured_ttl,
            TtlEvidence {
                downgraded: self
                    .options
                    .ttl_downgrade_notified
                    .load(std::sync::atomic::Ordering::Relaxed),
                saw_1h: self
                    .options
                    .saw_1h_honored
                    .load(std::sync::atomic::Ordering::Relaxed),
                ..Default::default()
            },
        )
    }

    fn interval(&self, ttl: CacheTtl) -> Duration {
        self.interval_override
            .unwrap_or_else(|| ping_interval(ttl))
    }

    /// Sleep until the next ping is due (either clock), or `false` once
    /// stopped or aborted. Nothing is due before the provider has reported
    /// a cached prefix for the round: there is nothing to keep warm, and no
    /// accepted request to schedule from.
    async fn wait_due(&self, c: &Controls) -> bool {
        loop {
            if c.stop.is_cancelled() || c.abort.is_cancelled() {
                return false;
            }
            let wake = if self.signal.cached_tokens() > 0 {
                let anchor = self.anchor();
                let interval = self.interval(self.ttl());
                if Self::elapsed_since(anchor) >= interval {
                    return true;
                }
                (anchor.mono + interval).min(Instant::now() + WALL_POLL)
            } else {
                Instant::now() + WALL_POLL
            };
            tokio::select! {
                _ = c.stop.cancelled() => return false,
                _ = c.abort.cancelled() => return false,
                _ = self.signal.changed.notified() => {}
                _ = tokio::time::sleep_until(wake) => {}
            }
        }
    }

    /// Book a ping that may have been billed but reported no usage (aborted
    /// in flight, a read timeout after dispatch) at its worst case, into the
    /// turn's counters and event channel — where a measured ping's usage
    /// goes. A measured ping is already booked by the transport.
    fn book_unmeasured(&self, bill: &super::keepwarm::PingBill, prefix_tokens: u64) {
        if bill.booking != PingBooking::WorstCase {
            return;
        }
        let (input, output, read) = (PING_INPUT_ESTIMATE, self.max_tokens, prefix_tokens);
        self.usage
            .record_priced(&self.model, input, output, read, 0, None, None);
        let _ = self.tx.send(StreamEvent::Session(SessionEvent::Usage {
            input_tokens: input,
            output_tokens: output,
            cache_read_input_tokens: read,
            cache_creation_input_tokens: 0,
            cache_creation_5m: None,
            cache_creation_1h: None,
            model: Some(self.model.clone()),
        }));
        tracing::info!(
            event = "tool_round_keepwarm_unmeasured",
            model = %self.model,
            cost_usd = bill.cost_usd,
            "tool-round keep-warm ping dispatched without usage; booked at its worst case"
        );
    }

    fn ping_cost(&self, ttl: CacheTtl, prefix_tokens: u64, output: u64) -> f64 {
        request_cost_usd(
            &self.model,
            ttl,
            prefix_tokens,
            0,
            PING_INPUT_ESTIMATE,
            output,
        )
    }

    async fn run(mut self, controls: Controls) -> Stop {
        let stop = self.pings(&controls).await;
        if stop != Stop::Done {
            tracing::info!(
                event = "tool_round_keepwarm_stop",
                model = %self.model,
                reason = %stop,
                "tool-round keep-warm stopped"
            );
        }
        stop
    }

    async fn pings(&mut self, c: &Controls) -> Stop {
        // The cost of the last measured ping; an estimate before the first.
        let mut measured: Option<f64> = None;
        let mut ledger = 0.0;
        let mut retries = 0usize;
        let mut pings = 0u32;
        loop {
            if !self.wait_due(c).await {
                return Stop::Done;
            }
            let ttl = self.ttl();
            // Test intervals are far below the TTL; production ones leave
            // more than LATE_GUARD.
            if self.elapsed() + LATE_GUARD >= governing_ttl(ttl) {
                return Stop::Expired;
            }
            // Known by now: a ping is only due once the prefix is reported.
            let prefix_tokens = self.signal.cached_tokens();
            let rewrite = rewrite_cost_usd(&self.model, ttl, prefix_tokens);
            let worst = self.ping_cost(ttl, prefix_tokens, self.max_tokens);
            let expected = measured.unwrap_or_else(|| {
                self.ping_cost(ttl, prefix_tokens, ping_output_estimate(self.max_tokens))
            });
            if ledger + expected > rewrite {
                return Stop::Bound;
            }
            // The round's own request is booked when its response ends;
            // until then it counts at the most its input side can cost.
            let mid_stream = !self.signal.response_in();
            let unbooked = if mid_stream { rewrite } else { 0.0 };
            if !turn_admits(self.max_cost_usd, self.usage.cost_usd(), unbooked, worst) {
                return Stop::TurnCost;
            }
            // A ping never selects, switches or re-binds an account (the
            // auth state is the turn's): it stops if its account is cooling.
            if let Err(e) = super::auth::AuthMethods::refresh_if_needed(
                Arc::clone(&self.auth),
                &self.client,
                &self.options.credential_source,
                &self.options.token_cache,
                Some(&self.model),
                super::auth::RefreshPoint::KeepWarm,
            )
            .await
            {
                return Stop::Failed(format!("auth refresh: {e}"));
            }
            // The tools may have returned during the refresh.
            if c.stop.is_cancelled() || c.abort.is_cancelled() {
                return Stop::Done;
            }
            let tap = PingTap::default();
            let sent_at = CacheAnchor::now();
            c.in_flight.store(true, Ordering::Release);
            let result = self.ping(&c.abort, &tap).await;
            c.in_flight.store(false, Ordering::Release);
            let bill = tap.bill(&self.model, worst);
            ledger += bill.cost_usd;
            self.book_unmeasured(&bill, prefix_tokens);
            match result {
                Ok(()) => {
                    pings += 1;
                    retries = 0;
                    let u = &bill.usage;
                    tracing::info!(
                        event = "tool_round_keepwarm_ping",
                        model = %self.model,
                        ping = pings,
                        mid_stream,
                        cache_read = u.cache_read_tokens,
                        cache_write = u.cache_write_tokens,
                        input = u.input_tokens,
                        output = u.output_tokens,
                        cost_usd = bill.cost_usd,
                        ledger_usd = ledger,
                        rewrite_usd = rewrite,
                        "tool-round keep-warm ping"
                    );
                    if u.cache_read_tokens == 0
                        || over_share(u.cache_write_tokens, prefix_tokens)
                        || over_share(u.input_tokens, prefix_tokens)
                    {
                        return Stop::Miss;
                    }
                    measured = Some(bill.cost_usd);
                    // The accepted ping refreshed the entry: the anchor is
                    // its stamp, or failing that its send.
                    self.floor = sent_at;
                }
                Err(PingFailure::Canceled) => {
                    tracing::info!(
                        event = "tool_round_keepwarm_aborted",
                        booking = ?bill.booking,
                        cost_usd = bill.cost_usd,
                        "tool-round keep-warm ping aborted in flight"
                    );
                    return Stop::Done;
                }
                Err(PingFailure::Transient(why)) if retries < BACKOFF.len() => {
                    tracing::debug!(%why, retry = retries + 1, "tool-round keep-warm ping: transient failure");
                    let delay = BACKOFF[retries];
                    retries += 1;
                    tokio::select! {
                        _ = c.stop.cancelled() => return Stop::Done,
                        _ = c.abort.cancelled() => return Stop::Done,
                        _ = tokio::time::sleep(delay) => {}
                    }
                }
                Err(failure) => return Stop::Failed(failure.to_string()),
            }
        }
    }

    /// Send one ping: the round's request plus the ping text.
    async fn ping(&self, cancel: &CancellationToken, tap: &PingTap) -> Result<(), PingFailure> {
        let options = ApiOptions {
            keepwarm: Some(KeepWarmRequest {
                mode: KeepWarmMode::Replay,
                status: Arc::clone(&tap.status),
                sent: Arc::clone(&tap.sent),
                connect_failed: Arc::clone(&tap.connect_failed),
            }),
            ..self.options.clone()
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        let call = ApiMethods::call_api_stream_inner(
            &self.auth,
            &self.client,
            &self.model,
            &self.tools,
            &self.system_prompt,
            self.thinking_budget,
            self.reasoning_level,
            &self.messages,
            tx,
            cancel,
            0,
            0,
            &options,
            self.telemetry_level,
        );
        let (result, _notices) =
            super::keepwarm::drive_ping(call, &mut rx, tap, Some(&self.tx)).await;
        match result {
            Ok(value) if value.get("keepwarm_prefix").is_none() => Ok(()),
            Ok(_) => Err(PingFailure::Fatal("ping was not sent".into())),
            Err(e) => Err(super::keepwarm::classify_failure(
                &e,
                tap.status.load(std::sync::atomic::Ordering::Relaxed),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The provider's `message_start` reports the round's cached prefix,
    /// and wakes the round's task even when it starts waiting later. A
    /// retried attempt reports its own.
    #[tokio::test]
    async fn the_reported_prefix_reaches_the_task() {
        let signal = RoundSignal::default();
        assert_eq!(signal.cached_tokens(), 0, "nothing reported: nothing to keep warm");
        assert!(!signal.response_in());
        signal.message_started(120_000);
        assert_eq!(signal.cached_tokens(), 120_000);
        assert!(!signal.response_in(), "still streaming");
        tokio::time::timeout(Duration::from_secs(5), signal.changed.notified())
            .await
            .expect("the wake-up is kept for a task that starts waiting later");
        signal.message_started(120_500);
        assert_eq!(signal.cached_tokens(), 120_500, "the retry's own prefix");
    }

    /// Once the response is in and asks for tools, the turn's counters
    /// only stand in for a provider that reported no prefix, and a request
    /// that cached nothing has nothing to keep warm.
    #[test]
    fn the_response_being_in_settles_the_round() {
        let signal = RoundSignal::default();
        signal.message_started(120_000);
        assert!(signal.tools_running(999), "a prefix to keep warm");
        assert!(signal.response_in());
        assert_eq!(signal.cached_tokens(), 120_000, "the reported prefix wins");

        let unreported = RoundSignal::default();
        assert!(unreported.tools_running(80_000));
        assert_eq!(unreported.cached_tokens(), 80_000, "the turn's counters");

        let uncached = RoundSignal::default();
        uncached.message_started(0);
        assert!(!uncached.tools_running(0), "nothing cached: nothing to keep");
        assert_eq!(uncached.cached_tokens(), 0);
    }

    /// The turn's cost cap counts the round's own request while it is not
    /// booked yet, so a ping sent as the response streams cannot be what
    /// takes the turn past it.
    #[test]
    fn the_cost_cap_counts_the_unbooked_round() {
        // No cap, or a non-finite one: always admitted.
        assert!(turn_admits(None, 9.0, 9.0, 9.0));
        assert!(turn_admits(Some(f64::INFINITY), 9.0, 9.0, 9.0));
        assert!(turn_admits(Some(f64::NAN), 9.0, 9.0, 9.0));
        // Tools running: the round is booked, only the ping is added.
        assert!(turn_admits(Some(1.0), 0.70, 0.0, 0.25));
        assert!(!turn_admits(Some(1.0), 0.70, 0.0, 0.35));
        // Response streaming: the round (up to 0.60 for its input side) is
        // not booked yet, and the same ping no longer fits on top of 0.20.
        assert!(turn_admits(Some(1.0), 0.10, 0.60, 0.25));
        assert!(!turn_admits(Some(1.0), 0.20, 0.60, 0.25));
    }
}
