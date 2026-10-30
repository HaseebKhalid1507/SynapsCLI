//! Keep the prompt cache warm while a tool round runs (`cache_keepwarm_tools`).
//!
//! A provider round that asks for tools is followed, once they return, by a
//! request that re-sends that round's whole prefix. If the tools take longer
//! than the cache TTL (workers are pinned to 5m and routinely run builds,
//! test sweeps and `sleep; poll` loops for that long), the entry has expired
//! by then and the next request rewrites the whole prefix at the write price
//! (1.25× on 5m) instead of reading it (0.1×, 0.05× on the 5.5 models).
//!
//! While the round's tools run, this task re-sends **that round's own
//! request** — the exact request messages, tools, system prompt, thinking
//! config and options it went out with — plus one trailing ping text block
//! (`KeepWarmMode::Replay`, appended by the transport after marker placement),
//! shortly before the TTL lapses, and again after every accepted ping, until
//! the tools return, the turn is cancelled or the stream ends (the guard
//! drops). It is a keep-warm ping in every other respect
//! (`runtime::keepwarm`): `ApiOptions::keepwarm` cuts `max_tokens`, disables
//! retries and marks the telemetry record `"keepwarm": true`; the transport
//! holds the ping text out of marker placement, so every breakpoint lands on
//! the block the round itself marked and the ping is an uncached tail. A
//! read refreshes the entry's TTL, so the request after the round reads the
//! prefix the round cached. Nothing is persisted and no history is touched.
//!
//! Policy (the idle keep-warm's, `session::cache_keepwarm`, minus the idle
//! guesswork: the next request is certain unless the turn is cancelled):
//! - eligible only on the native Anthropic transport and when the round's
//!   request cached something;
//! - due at `anchor + ping_interval(honoured TTL)` (4.5 min on 5m, 55 min on
//!   1h) on whichever clock comes first; never once the cache is within
//!   `LATE_GUARD` of expiry (it is presumed gone: the next request rewrites
//!   it anyway, a ping would only add a write);
//! - ski-rental bound: a ping is sent only while `ledger + expected ping ≤
//!   rewriting the prefix at the honoured TTL`;
//! - the turn's `max_cost_usd` (if any) must admit the ping's worst case;
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
//! the same prefix, and a ping writes nothing — its tail is uncached.)
//!
//! Booking. Every ping that may have been billed is booked into the turn,
//! by the idle keep-warm's rule (`keepwarm::ping_booking`): with measured
//! usage the transport records it in the turn's usage counters (turn
//! budget) and its `Usage` event is forwarded to the turn's event channel
//! (session / worker cost); a ping dispatched with no usage seen (aborted in
//! flight, a read timeout) is booked at its worst case — the whole prefix
//! read, [`PING_INPUT_ESTIMATE`] uncached, `max_tokens` out — through both;
//! one refused before the network or rejected with an HTTP error books
//! nothing.

use std::sync::atomic::{AtomicBool, Ordering};
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
    /// The tools returned, the turn was cancelled or the stream ended.
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
    anchor: CacheAnchor,
    anchor_cell: Arc<super::keepwarm::CacheAnchorCell>,
    ttl: CacheTtl,
    interval: Duration,
    prefix_tokens: u64,
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

/// Running keep-warm for one tool round. [`finish`](Self::finish) stops it
/// gracefully; dropping it (any other way out of the round) aborts a ping
/// in flight, which the task still books.
pub(super) struct ToolRoundKeepWarm {
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
    /// Start keeping `round`'s prefix warm, or `None` when it cannot or
    /// need not be: not the native Anthropic transport, or the round's
    /// request cached nothing.
    pub(super) async fn start(
        settings: &ToolKeepWarm,
        mark: RoundMark,
        round: Round<'_>,
        turn: Turn<'_>,
    ) -> Option<Self> {
        if !super::auth::model_is_anthropic(round.model) {
            return None;
        }
        let prefix_tokens = cached_total(turn.usage).saturating_sub(mark.cached_before);
        if prefix_tokens == 0 {
            return None;
        }
        let auth_type = round.auth.read().await.auth_type.clone();
        let configured = round.options.cache_ttl.resolve_for_auth_type(&auth_type);
        let ttl = honoured_ttl(
            configured,
            TtlEvidence {
                downgraded: round
                    .options
                    .ttl_downgrade_notified
                    .load(std::sync::atomic::Ordering::Relaxed),
                saw_1h: round
                    .options
                    .saw_1h_honored
                    .load(std::sync::atomic::Ordering::Relaxed),
                ..Default::default()
            },
        );
        let interval = settings
            .interval_override
            .unwrap_or_else(|| ping_interval(ttl));
        // Pings stamp the round's anchor cell (or a private one): an
        // accepted ping moves the anchor the next ping is scheduled from.
        let anchor_cell = round.options.cache_anchor.clone().unwrap_or_default();
        let anchor = anchor_cell
            .get()
            .filter(|a| a.mono >= mark.sent.mono)
            .unwrap_or(mark.sent);
        let messages = round.messages.to_vec();
        let options = ApiOptions {
            memory_backend: None,
            trace: settings.trace.clone(),
            request_correlation: None,
            suppress_stream_deltas: true,
            usage_counters: Some(Arc::clone(turn.usage)),
            cache_anchor: Some(Arc::clone(&anchor_cell)),
            context_rollover: None,
            keepwarm: None,
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
            anchor,
            anchor_cell,
            ttl,
            interval,
            prefix_tokens,
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
            stop,
            abort,
            in_flight,
            grace: settings.finish_grace(),
            #[cfg(test)]
            hook: settings.finish_hook.clone(),
            handle: Some(handle),
        })
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
    fn elapsed(&self) -> Duration {
        let mono = Instant::now().saturating_duration_since(self.anchor.mono);
        let wall = SystemTime::now()
            .duration_since(self.anchor.wall)
            .unwrap_or_default();
        mono.max(wall)
    }

    /// Sleep until the next ping is due (either clock), or `false` once
    /// stopped or aborted.
    async fn wait_due(&self, c: &Controls) -> bool {
        loop {
            if c.stop.is_cancelled() || c.abort.is_cancelled() {
                return false;
            }
            if self.elapsed() >= self.interval {
                return true;
            }
            let due = self.anchor.mono + self.interval;
            let wake = due.min(Instant::now() + WALL_POLL);
            tokio::select! {
                _ = c.stop.cancelled() => return false,
                _ = c.abort.cancelled() => return false,
                _ = tokio::time::sleep_until(wake) => {}
            }
        }
    }

    /// Book a ping that may have been billed but reported no usage (aborted
    /// in flight, a read timeout after dispatch) at its worst case, into the
    /// turn's counters and event channel — where a measured ping's usage
    /// goes. A measured ping is already booked by the transport.
    fn book_unmeasured(&self, bill: &super::keepwarm::PingBill) {
        if bill.booking != PingBooking::WorstCase {
            return;
        }
        let (input, output, read) = (PING_INPUT_ESTIMATE, self.max_tokens, self.prefix_tokens);
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

    fn ping_cost(&self, output: u64) -> f64 {
        request_cost_usd(
            &self.model,
            self.ttl,
            self.prefix_tokens,
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
        let rewrite = rewrite_cost_usd(&self.model, self.ttl, self.prefix_tokens);
        let worst = self.ping_cost(self.max_tokens);
        let mut expected = self.ping_cost(ping_output_estimate(self.max_tokens));
        let mut ledger = 0.0;
        let mut retries = 0usize;
        let mut pings = 0u32;
        loop {
            if !self.wait_due(c).await {
                return Stop::Done;
            }
            // Test intervals are far below the TTL; production ones leave
            // more than LATE_GUARD.
            if self.elapsed() + LATE_GUARD >= governing_ttl(self.ttl) {
                return Stop::Expired;
            }
            if ledger + expected > rewrite {
                return Stop::Bound;
            }
            if let Some(cap) = self.max_cost_usd.filter(|c| c.is_finite()) {
                if self.usage.cost_usd() + worst > cap {
                    return Stop::TurnCost;
                }
            }
            if let Err(e) = super::auth::AuthMethods::refresh_if_needed(
                Arc::clone(&self.auth),
                &self.client,
                &self.options.credential_source,
                &self.options.token_cache,
                Some(&self.model),
                super::auth::RefreshPoint::WithinTurn,
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
            self.book_unmeasured(&bill);
            match result {
                Ok(()) => {
                    pings += 1;
                    retries = 0;
                    let u = &bill.usage;
                    tracing::info!(
                        event = "tool_round_keepwarm_ping",
                        model = %self.model,
                        ping = pings,
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
                        || over_share(u.cache_write_tokens, self.prefix_tokens)
                        || over_share(u.input_tokens, self.prefix_tokens)
                    {
                        return Stop::Miss;
                    }
                    expected = bill.cost_usd;
                    self.anchor = self
                        .anchor_cell
                        .get()
                        .filter(|a| a.mono >= sent_at.mono)
                        .unwrap_or(sent_at);
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
