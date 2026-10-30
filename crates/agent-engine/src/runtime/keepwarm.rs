//! Prompt-cache keep-warm ping (task #444).
//!
//! Anthropic refreshes a cache entry's TTL every time it is read ("hits and
//! refreshes" are billed at the cache-read rate), and the lifetime counts from
//! the START of the request that read or wrote it. Re-sending the session's
//! exact prefix shortly before the TTL lapses keeps a large prefix warm at the
//! read price instead of paying a full rewrite when the user comes back. The
//! *policy* (when to ping, when to stop, what it costs) lives in the session
//! actor (`session::cache_keepwarm`); this module builds, fingerprints and
//! sends one ping.
//!
//! The ping is the request the NEXT real turn would send, minus the parts that
//! are per-turn by construction, and goes through the SAME request builder
//! (`ApiMethods::call_api_stream_inner` → `build_anthropic_request`):
//!
//! - tools, system prompt (orchestration doctrine, frozen tool catalog,
//!   context guidance, session-scoped extension injection), thinking config,
//!   effort, model, TTL markers and the durable history are assembled exactly
//!   as `run_stream_tracked` + `run_stream_internal` assemble them;
//! - the trailing message is a fixed user text block, appended request-locally
//!   AFTER the last cache marker (`ApiOptions::keepwarm` moves the
//!   conversational marker onto the last history message), so it never enters
//!   the cached prefix and is never persisted;
//! - no memory recall, no `before_message` / Inject hooks, no steering, no
//!   context-pressure message: those only ever touch the request tail;
//! - no `MessageHistory`, no turn, no transcript: events go to a private
//!   channel and only the `Usage` numbers come back;
//! - `max_tokens` is cut to the minimum the thinking mode allows
//!   (`RequestBody::apply_keepwarm_max_tokens`; not part of the cache key),
//!   retries are zero, and the telemetry record says `"keepwarm": true`.
//!
//! **Prefix fingerprint.** A ping is only correct while the next real turn's
//! prefix is byte-identical to what is cached; on a changed prefix it would
//! WRITE a new entry instead of reading the old one. The builder hashes the
//! serialized prefix (`RequestBody::prefix_fingerprint`) of every keep-warm
//! request. [`Runtime::cache_keepwarm_probe`] returns that hash without
//! sending anything; [`Runtime::cache_keepwarm_ping`] takes the expected hash
//! and refuses to send — inside the builder, after assembly, right before the
//! network — when the freshly built prefix differs.

use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::api::{ApiMethods, ApiOptions, KeepWarmMode, KeepWarmRequest};
use super::helpers::HelperMethods;
use super::{Runtime, SessionEvent, StreamEvent};
use crate::{Result, RuntimeError, SharedMessage};

/// Output tokens a ping may produce on top of a thinking budget (or in total
/// with thinking off / adaptive). One token: the answer is discarded.
pub const KEEPWARM_OUTPUT_TOKENS: u64 = 1;

/// The trailing user message. Sits after the last cache marker, is never
/// persisted, and asks for the shortest possible answer with no reasoning so
/// budgeted thinking spends as little of its budget as the model allows.
pub const KEEPWARM_PING_TEXT: &str = "[automated prompt-cache keep-warm ping, not from the user] \
     This needs no reasoning. Reply with exactly: ok";

/// When this session's runtime last had an Anthropic request accepted
/// (HTTP 2xx): the moment its cache entries were last written or refreshed.
/// Taken immediately BEFORE the bytes are handed to the HTTP client, so it is
/// never later than the provider's own request start (the TTL origin).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheAnchor {
    /// Monotonic clock: scheduling.
    pub mono: tokio::time::Instant,
    /// Wall clock: the provider's TTL keeps running while this machine
    /// sleeps; the monotonic clock does not.
    pub wall: std::time::SystemTime,
}

impl CacheAnchor {
    pub fn now() -> Self {
        Self {
            mono: tokio::time::Instant::now(),
            wall: std::time::SystemTime::now(),
        }
    }
}

/// Shared "last accepted Anthropic request" slot (see [`CacheAnchor`]).
/// Runtime clones get a FRESH cell (a clone is a different request chain,
/// like the TTL latches); `share_ttl_latches` shares it.
#[derive(Debug, Default)]
pub struct CacheAnchorCell(std::sync::Mutex<Option<CacheAnchor>>);

impl CacheAnchorCell {
    pub fn stamp(&self, at: CacheAnchor) {
        let mut slot = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.is_none_or(|prev| prev.mono <= at.mono) {
            *slot = Some(at);
        }
    }

    pub fn get(&self) -> Option<CacheAnchor> {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Token usage of one ping, as reported by the provider.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct KeepWarmUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cache_write_5m: Option<u64>,
    pub cache_write_1h: Option<u64>,
    /// Provider-reported model, if any.
    pub model: Option<String>,
}

impl KeepWarmUsage {
    pub(crate) fn add(&mut self, event: &StreamEvent) {
        if let StreamEvent::Session(SessionEvent::Usage {
            input_tokens,
            output_tokens,
            cache_read_input_tokens,
            cache_creation_input_tokens,
            cache_creation_5m,
            cache_creation_1h,
            model,
        }) = event
        {
            self.input_tokens += input_tokens;
            self.output_tokens += output_tokens;
            self.cache_read_tokens += cache_read_input_tokens;
            self.cache_write_tokens += cache_creation_input_tokens;
            if let Some(n) = cache_creation_5m {
                *self.cache_write_5m.get_or_insert(0) += n;
            }
            if let Some(n) = cache_creation_1h {
                *self.cache_write_1h.get_or_insert(0) += n;
            }
            if model.is_some() {
                self.model = model.clone();
            }
        }
    }

    /// USD cost of the ping (cache read + any cache write + uncached input +
    /// output incl. thinking), priced exactly like a turn's usage.
    pub fn cost_usd(&self, fallback_model: &str) -> f64 {
        crate::pricing::calculate_cost_optional_split(
            self.model.as_deref().unwrap_or(fallback_model),
            self.input_tokens,
            self.output_tokens,
            self.cache_read_tokens,
            self.cache_write_tokens,
            self.cache_write_5m,
            self.cache_write_1h,
        )
    }
}

/// What a ping did.
#[derive(Debug, Clone, PartialEq)]
pub enum PingOutcome {
    /// Sent and accepted; its usage is in [`PingReport::usage`].
    Sent,
    /// Not sent: the freshly built prefix no longer matches the expected
    /// fingerprint (model, thinking, tools, system or history changed).
    PrefixChanged { expected: u64, actual: u64 },
}

/// One ping's result AND whatever the provider billed for it. Usage is
/// returned on every path — success, transient or fatal error, in-stream
/// error under a 200 — so the caller books it all.
#[derive(Debug, Clone, PartialEq)]
pub struct PingReport {
    pub result: std::result::Result<PingOutcome, PingFailure>,
    pub usage: KeepWarmUsage,
    /// Notices the request raised (e.g. the one-per-session silent 1h→5m
    /// downgrade notice: the ping shares the session's latch, so the actor
    /// must surface it).
    pub notices: Vec<String>,
}

/// Live view of an in-flight ping, shared with the actor so an ABORTED ping
/// (a turn started, the session ended) can still be booked: `sent` flips once
/// the request is dispatched (the HTTP client pulls the body over an
/// established connection, or a response arrives — never for a refused
/// connection), `connect_failed` when `send()` could not connect, `status`
/// holds the last HTTP status (0 = none), and `usage` accumulates Usage
/// events as they stream in.
#[derive(Debug, Clone, Default)]
pub struct PingTap {
    pub sent: Arc<std::sync::atomic::AtomicBool>,
    pub connect_failed: Arc<std::sync::atomic::AtomicBool>,
    pub status: Arc<AtomicU16>,
    pub usage: Arc<std::sync::Mutex<KeepWarmUsage>>,
}

impl PingTap {
    pub fn was_sent(&self) -> bool {
        self.sent.load(Ordering::Relaxed)
    }

    pub fn usage(&self) -> KeepWarmUsage {
        self.usage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// What this ping is booked at, by [`ping_booking`], and its cost:
    /// measured usage, else `worst_usd` once dispatched, else nothing.
    /// EVERY path that ends a ping (done Ok / Err, abort, task panic) books
    /// through here.
    pub fn bill(&self, model: &str, worst_usd: f64) -> PingBill {
        let usage = self.usage();
        let booking = ping_booking(
            usage != KeepWarmUsage::default(),
            self.was_sent(),
            self.connect_failed.load(Ordering::Relaxed),
            self.status.load(Ordering::Relaxed),
        );
        let cost_usd = match booking {
            PingBooking::Measured => usage.cost_usd(model),
            PingBooking::WorstCase => worst_usd,
            PingBooking::Nothing => 0.0,
        };
        PingBill {
            booking,
            usage,
            cost_usd,
        }
    }
}

/// How a finished or aborted ping is booked (see [`ping_booking`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PingBooking {
    /// The provider reported usage: book exactly that.
    Measured,
    /// Dispatched, no usage seen: the provider may bill it — book the
    /// period's worst case for the ping.
    WorstCase,
    /// Nothing can be billed: never dispatched (refused before the network,
    /// or a connect failure with no HTTP status), or rejected outright with an
    /// HTTP error status before any usage was reported — the provider does not
    /// bill a request it refused (an in-stream error after a 200 arrives with
    /// `message_start` usage and is `Measured`).
    Nothing,
}

/// The one booking rule for a ping, on every path: usage arrived → its real
/// cost; otherwise rejected with an HTTP error status (≥ 400) → $0; otherwise
/// dispatched → the worst case, unless the error was a connect failure with no
/// HTTP status; otherwise $0.
pub fn ping_booking(
    usage_arrived: bool,
    sent: bool,
    connect_failed: bool,
    http_status: u16,
) -> PingBooking {
    if usage_arrived {
        PingBooking::Measured
    } else if http_status >= 400 {
        PingBooking::Nothing
    } else if sent && !(connect_failed && http_status == 0) {
        PingBooking::WorstCase
    } else {
        PingBooking::Nothing
    }
}

/// A ping's booking (see [`PingTap::bill`]).
#[derive(Debug, Clone, PartialEq)]
pub struct PingBill {
    pub booking: PingBooking,
    pub usage: KeepWarmUsage,
    pub cost_usd: f64,
}

/// What a probe learned about the next ping, without sending anything.
#[derive(Debug, Clone, PartialEq)]
pub struct KeepWarmProbe {
    /// Prefix fingerprint (see the module docs).
    pub fingerprint: u64,
    /// The ping's `max_tokens`: the worst-case output it can be billed for
    /// (1, or `budget + 1` under budgeted thinking).
    pub max_tokens: u64,
    /// `Some(reason)` when the next real turn — this history plus an empty
    /// user message — would roll the context over or hard-stop before its
    /// first request (the same assessment `run_stream_internal` makes). The
    /// cached prefix is then not what the next turn sends.
    pub context_blocks: Option<String>,
}

/// Why a ping failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PingFailure {
    /// Worth a bounded retry: 429/5xx/529, network, in-stream transient.
    Transient(String),
    /// Stop: any other 4xx, auth, configuration, ineligible session.
    Fatal(String),
    /// Aborted by the actor (a turn started, the session is ending).
    Canceled,
}

impl std::fmt::Display for PingFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PingFailure::Transient(m) => write!(f, "transient: {m}"),
            PingFailure::Fatal(m) => write!(f, "{m}"),
            PingFailure::Canceled => f.write_str("canceled"),
        }
    }
}

/// Classify a ping error. `status` is the last HTTP status the transport saw
/// (0 = none: network failure before any response).
pub(crate) fn classify_failure(err: &RuntimeError, status: u16) -> PingFailure {
    if matches!(err, RuntimeError::Canceled) {
        return PingFailure::Canceled;
    }
    if (400..500).contains(&status) && status != 429 {
        return PingFailure::Fatal(format!("HTTP {status}"));
    }
    match err {
        // `NetworkTimeout` (upload/header deadline, stream idle) is a network
        // blip like the `Api(timeout)` it replaced: back off, don't end the period.
        RuntimeError::Api(_)
        | RuntimeError::ApiStatus(_)
        | RuntimeError::Timeout
        | RuntimeError::NetworkTimeout(_) => PingFailure::Transient(if status == 0 {
            "network".to_string()
        } else {
            format!("HTTP {status}")
        }),
        other => PingFailure::Fatal(other.to_string()),
    }
}

/// The marker moves onto the last history message, so it must be a block the
/// API accepts `cache_control` on: a finished assistant reply ending in a
/// non-empty text block (never a thinking block, never a dangling tool_use).
fn tail_is_markable(last: &Value) -> bool {
    if last["role"] != "assistant" {
        return false;
    }
    match last["content"].as_array() {
        Some(blocks) => blocks.last().is_some_and(|b| {
            b["type"] == "text" && b["text"].as_str().is_some_and(|t| !t.is_empty())
        }),
        None => last["content"].as_str().is_some_and(|t| !t.is_empty()),
    }
}

/// Whether `history` can be kept warm: it must end with a finished assistant
/// reply (a turn that errored or was interrupted leaves nothing worth pinging).
pub fn history_ready(history: &[SharedMessage]) -> bool {
    history.last().is_some_and(|m| tail_is_markable(m))
}

/// Conservative token estimate of the part of the history's last message
/// (the turn's final reply) that the cached prefix will hold: its content
/// minus thinking blocks, which the API strips when the next request
/// re-sends the reply (they were billed as output, never cached). Ping 1
/// may write at most this (plus the last request's uncached input) without
/// counting as a partial miss.
pub fn reply_prefix_tokens(history: &[SharedMessage]) -> u64 {
    let Some(last) = history.last() else {
        return 0;
    };
    let kept = match last["content"].as_array() {
        Some(blocks) => Value::Array(
            blocks
                .iter()
                .filter(|b| !matches!(b["type"].as_str(), Some("thinking" | "redacted_thinking")))
                .cloned()
                .collect(),
        ),
        None => last["content"].clone(),
    };
    super::context::conservative_token_estimate(&kept.to_string())
}

impl Runtime {
    /// Why this runtime cannot keep its cache warm, or `None` when it can.
    /// Only the native Anthropic transport carries `cache_control`; every
    /// TTL (5m, 1h, hybrid) and every thinking mode is supported.
    pub fn cache_keepwarm_ineligibility(&self) -> Option<String> {
        if !super::auth::model_is_anthropic(&self.model) {
            return Some(format!(
                "model {} is not on the native Anthropic transport",
                self.model
            ));
        }
        None
    }

    /// The last accepted Anthropic request of this runtime's request chain.
    pub fn cache_anchor(&self) -> Option<CacheAnchor> {
        self.cache_anchor.get()
    }

    /// The session's silent 1h→5m downgrade latch has fired: the server
    /// honours only 5m whatever the configured TTL says.
    pub fn cache_ttl_downgraded(&self) -> bool {
        self.ttl_downgrade_notified.load(Ordering::Relaxed)
    }

    /// A request of this session has written a 1h cache entry.
    pub fn cache_1h_honoured(&self) -> bool {
        self.saw_1h_honored.load(Ordering::Relaxed)
    }

    /// Probe the ping the next real turn would justify for `history` (see
    /// the module docs): its prefix fingerprint, its worst-case output, and
    /// whether the next turn would roll the context over first. Builds the
    /// request through the real builder and returns before anything is
    /// sent; no network I/O.
    pub async fn cache_keepwarm_probe(&self, history: &[SharedMessage]) -> Result<KeepWarmProbe> {
        let cancel = CancellationToken::new();
        let tap = PingTap::default();
        let (value, _) = self
            .keepwarm_request(history, &cancel, KeepWarmMode::Probe, &tap)
            .await;
        let value = value?;
        let fingerprint = value["keepwarm_prefix"].as_u64().ok_or_else(|| {
            RuntimeError::Session("keep-warm probe produced no fingerprint".into())
        })?;
        Ok(KeepWarmProbe {
            fingerprint,
            max_tokens: value["keepwarm_max_tokens"].as_u64().unwrap_or(0),
            context_blocks: value["keepwarm_context_blocks"]
                .as_str()
                .map(str::to_string),
        })
    }

    /// Send ONE keep-warm ping for `history` (the session's durable history,
    /// unchanged) if its freshly built prefix still hashes to `expect`.
    /// Never mutates `history`, the runtime's session state, or any persisted
    /// state; fires no extension hooks; emits nothing to any frontend.
    /// `tap` sees the request leave and its usage arrive while it runs.
    pub async fn cache_keepwarm_ping(
        &self,
        history: &[SharedMessage],
        expect: u64,
        cancel: &CancellationToken,
        tap: &PingTap,
    ) -> PingReport {
        let (value, notices) = self
            .keepwarm_request(history, cancel, KeepWarmMode::Send { expect }, tap)
            .await;
        let result = match value {
            Ok(value) => match value.get("keepwarm_prefix").and_then(Value::as_u64) {
                Some(actual) => Ok(PingOutcome::PrefixChanged {
                    expected: expect,
                    actual,
                }),
                None => Ok(PingOutcome::Sent),
            },
            Err(e) => Err(classify_failure(&e, tap.status.load(Ordering::Relaxed))),
        };
        PingReport {
            result,
            usage: tap.usage(),
            notices,
        }
    }

    async fn keepwarm_request(
        &self,
        history: &[SharedMessage],
        cancel: &CancellationToken,
        mode: KeepWarmMode,
        tap: &PingTap,
    ) -> (Result<Value>, Vec<String>) {
        match self
            .keepwarm_request_inner(history, cancel, mode, tap)
            .await
        {
            Ok(r) => r,
            Err(e) => (Err(e), Vec::new()),
        }
    }

    /// `Err` = refused before any request was built (nothing billed);
    /// `Ok((Err, _))` = the request itself failed (usage, if any, is in `tap`).
    async fn keepwarm_request_inner(
        &self,
        history: &[SharedMessage],
        cancel: &CancellationToken,
        mode: KeepWarmMode,
        tap: &PingTap,
    ) -> Result<(Result<Value>, Vec<String>)> {
        if let Some(reason) = self.cache_keepwarm_ineligibility() {
            return Err(RuntimeError::Config(format!(
                "cache keep-warm skipped: {reason}"
            )));
        }
        if !history_ready(history) {
            return Err(RuntimeError::Config(
                "cache keep-warm skipped: history does not end with a finished assistant reply"
                    .into(),
            ));
        }
        let (durability_blocked, context_enabled) = {
            let state = self
                .continuation
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (state.durability_blocked, state.enabled())
        };
        if durability_blocked {
            return Err(RuntimeError::Session(
                "cache keep-warm skipped: context head save is unresolved".into(),
            ));
        }

        // Same preflight as a real turn (run_stream_tracked). A probe sends
        // nothing, so it never needs a token refresh.
        self.validate_request_preflight().await?;
        let anthropic_execution_plan = self.authorized_anthropic_plan().await?;
        if matches!(mode, KeepWarmMode::Send { .. }) {
            self.refresh_if_needed().await?;
        }

        // System prompt: run_stream_tracked → run_stream_internal, verbatim.
        let system_prompt = self.effective_system_prompt().await;
        let system_prompt = if context_enabled {
            Some(format!(
                "{}\n\n{}",
                system_prompt.as_deref().unwrap_or_default(),
                super::continuation::GUIDANCE
            ))
        } else {
            system_prompt
        };
        let session_injection = match self.session_id.as_deref() {
            Some(id) => self.hook_bus.session_injection_for(id).await,
            None => None,
        };
        let system_prompt = match session_injection {
            Some(content) => Some(super::stream::wrap_extension_context(
                system_prompt.as_deref().unwrap_or_default(),
                &content,
            )),
            None => system_prompt,
        };

        // Messages: the durable history plus one request-local trailing user
        // message. The same request-local image retention as a real round
        // (`image_retention::round_images`), so the retired prefix, and the
        // retention anchor the transport places from it, match the next turn.
        let mut request: Vec<SharedMessage> = history.to_vec();
        request.push(Arc::new(
            json!({"role": "user", "content": KEEPWARM_PING_TEXT}),
        ));
        let round_images =
            super::image_retention::round_images(&self.model, self.codex_request_role(), &request)
                .map_err(RuntimeError::Config)?;
        let (request_messages, _) = round_images.views(&request);

        // Tools: a snapshot of the live registry (the previous turn already
        // applied its idempotent setup), projected exactly like a stream round
        // when progressive disclosure is on — read-only: the retained session
        // tool set is never written.
        let tools_snapshot = self.tools.read().await.clone();
        let (request_tools_schema, anthropic_tool_split) = if self.progressive_tool_disclosure {
            let fresh = super::continuation::context_tool_set(
                self.host_tool_session.clone(),
                tools_snapshot.catalog(),
                true,
                context_enabled,
            );
            // The SAME selection a turn start makes (`retain_session_tool_set`),
            // through its pure half: nothing is written back.
            let retained = self
                .retained_tool_set
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let existing = retained.as_ref().map(|shared| {
                shared
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
            });
            let (mut set, _, _) = super::stream::select_turn_start_tool_set(
                existing.as_deref(),
                fresh,
                tools_snapshot.catalog(),
                true,
            );
            drop(existing);
            // Activations carried from the previous runtime (reload, unpark)
            // that the next turn start will restore: projected here too, on
            // this local copy only, so a ping before that turn warms the
            // prefix the turn will send.
            let pending = self
                .pending_tool_restore
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if let Some(carried) = pending {
                if crate::tools::activation::carry_forward_enabled() {
                    let _ = super::stream::restore_pending_activations(
                        &mut set,
                        &carried,
                        tools_snapshot.catalog(),
                        self.activation_confirm,
                    );
                }
            }
            let schema = tools_snapshot.session_tools_schema(&set).schema;
            let split = self.deferred_tool_loading.then(|| {
                let referenced = HelperMethods::referenced_tool_ids(request_messages);
                Arc::new(tools_snapshot.deferred_tool_split(&set, &referenced))
            });
            (Some(Arc::new(schema)), split)
        } else {
            (None, None)
        };

        let options = ApiOptions {
            memory_backend: None,
            use_1m_context: self.context_window_override == Some(1_000_000),
            cache_ttl: self.cache_ttl,
            // The SESSION's latch: a ping that sees the silent 1h→5m
            // downgrade raises the one notice (the actor surfaces it) and
            // the actor schedules by the 5m TTL the server honours.
            ttl_downgrade_notified: self.ttl_downgrade_notified.clone(),
            saw_1h_honored: self.saw_1h_honored.clone(),
            credential_source: self.credential_source.clone(),
            token_cache: self.token_cache.clone(),
            anthropic_base_url: None,
            anthropic_stream_client: self.anthropic_stream_client.clone(),
            request_compression: self.request_compression.clone(),
            anthropic_execution_plan,
            codex_request_role: self.codex_request_role(),
            retention_frontier: round_images.applied_frontier(),
            // The base context: a ping must never consume `/trace next`.
            trace: self.trace_ctx.clone(),
            request_correlation: None,
            suppress_stream_deltas: true,
            telemetry: self.request_telemetry(),
            tool_session_id: Some(self.host_tool_session.clone()),
            session_tool_set: None,
            request_tools_schema: request_tools_schema.clone(),
            anthropic_tool_split,
            usage_counters: None,
            // A successful ping refreshes the cache: it moves the anchor the
            // next ping is scheduled from.
            cache_anchor: Some(Arc::clone(&self.cache_anchor)),
            keepwarm: Some(KeepWarmRequest {
                mode,
                status: Arc::clone(&tap.status),
                sent: Arc::clone(&tap.sent),
                connect_failed: Arc::clone(&tap.connect_failed),
            }),
            // Header only (cache-key parity with the turns this ping keeps
            // warm); a ping never sends `diagnostics` nor moves the chain.
            cache_diagnosis: self.cache_diagnosis(),
            context_rollover: None,
        };

        let (tx, mut rx) = mpsc::unbounded_channel();
        let call = ApiMethods::call_api_stream_inner(
            &self.auth,
            &self.client,
            &self.model,
            &tools_snapshot,
            &system_prompt,
            self.thinking_budget,
            self.reasoning_level(),
            request_messages,
            tx,
            cancel,
            0,
            0,
            &options,
            self.telemetry_level,
        );
        tokio::pin!(call);
        // Drain events WHILE the request runs: usage lands in the tap as it
        // arrives, so an aborted ping can still be booked.
        let mut notices = Vec::new();
        let mut on_event = |event: StreamEvent| {
            if let StreamEvent::Session(SessionEvent::Notice(n) | SessionEvent::Retry(n)) = &event {
                notices.push(n.clone());
            }
            tap.usage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .add(&event);
        };
        let mut result = loop {
            tokio::select! {
                r = &mut call => break r,
                Some(event) = rx.recv() => on_event(event),
            }
        };
        while let Ok(event) = rx.try_recv() {
            on_event(event);
        }
        // Probe: also run the next turn's pre-request context assessment.
        if let (Ok(value), KeepWarmMode::Probe) = (&mut result, mode) {
            if context_enabled {
                if let Some(why) = self.keepwarm_context_blocks(
                    history,
                    system_prompt.as_deref(),
                    request_tools_schema.as_deref().map(|s| s.as_slice()),
                    &tools_snapshot,
                ) {
                    value["keepwarm_context_blocks"] = Value::String(why);
                }
            }
        }
        Ok((result, notices))
    }

    /// The pre-request context assessment of `run_stream_internal`, for the
    /// next turn = `history` + an empty user message, WITHOUT committing the
    /// policy's next state. `Some(reason)` on Rollover / HardStop.
    ///
    /// Mirrors the real round exactly: it assesses the round's final
    /// `request_messages` — after the request-local history image cap — via
    /// the same [`super::stream::pre_request_context_budget`].
    fn keepwarm_context_blocks(
        &self,
        history: &[SharedMessage],
        system_prompt: Option<&str>,
        projected_schema: Option<&[Value]>,
        tools_snapshot: &crate::ToolRegistry,
    ) -> Option<String> {
        use agent_core::core::context_policy::{assess_context, ContextAction};
        let mut next: Vec<SharedMessage> = history.to_vec();
        next.push(Arc::new(json!({"role": "user", "content": ""})));
        // Estimated exactly like a real round: over the retention view.
        let round_images =
            super::image_retention::round_images(&self.model, self.codex_request_role(), &next)
                .ok();
        let (_, request_messages) = match &round_images {
            Some(round) => round.views(&next),
            None => (&next[..], &next[..]),
        };
        let fallback_schema;
        let schema = match projected_schema {
            Some(s) => s,
            None => {
                fallback_schema = tools_snapshot.tools_schema();
                fallback_schema.as_slice()
            }
        };
        let (assessment, budget) = super::stream::pre_request_context_budget(
            &self.model,
            self.context_window(),
            system_prompt,
            schema,
            request_messages,
            self.thinking_budget as u64,
            self.max_tool_output as u64,
        );
        let state = self
            .continuation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let decision = assess_context(&state.config, &state.policy, budget);
        match decision.action {
            ContextAction::Rollover => Some(format!(
                "the next turn would roll the context over ({} tokens used)",
                assessment.used_tokens()
            )),
            ContextAction::HardStop => Some(format!(
                "the next turn would hit the context hard limit ({} tokens used)",
                assessment.used_tokens()
            )),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::CacheTtl;
    use crate::extensions::hooks::events::{HookEvent, HookKind, HookResult};
    use crate::extensions::permissions::{Permission, PermissionSet};
    use agent_core::reasoning::ReasoningLevel;
    use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::post, Router};
    use futures::StreamExt;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Mutex;

    const SSE_HI: &str = concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_kw\",\"type\":\"message\",",
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

    /// A 200 stream that reports usage, then fails in-stream (overloaded).
    const SSE_USAGE_THEN_ERROR: &str = concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_kw\",\"type\":\"message\",",
        "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-opus-4-7\",\"stop_reason\":null,",
        "\"stop_sequence\":null,\"usage\":{\"input_tokens\":12,\"output_tokens\":0,",
        "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":5000}}}\n\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":null,",
        "\"stop_sequence\":null},\"usage\":{\"input_tokens\":12,\"output_tokens\":1,",
        "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":5000}}\n\n",
        "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",",
        "\"message\":\"synthetic\"}}\n\n",
    );

    #[derive(Clone)]
    struct Mock {
        bodies: Arc<Mutex<Vec<String>>>,
        /// HTTP status served (200 = `sse`).
        status: u16,
        sse: &'static str,
    }

    async fn spawn_mock() -> (String, Mock) {
        spawn_mock_full(200, SSE_HI).await
    }

    async fn spawn_mock_with(status: u16) -> (String, Mock) {
        spawn_mock_full(status, SSE_HI).await
    }

    async fn spawn_mock_full(status: u16, sse: &'static str) -> (String, Mock) {
        let mock = Mock {
            bodies: Arc::default(),
            status,
            sse,
        };
        let app = Router::new()
            .route(
                "/v1/messages",
                post(|State(m): State<Mock>, body: String| async move {
                    m.bodies.lock().unwrap().push(body);
                    if m.status == 200 {
                        (
                            StatusCode::OK,
                            [("content-type", "text/event-stream")],
                            m.sse.to_string(),
                        )
                            .into_response()
                    } else {
                        (
                            StatusCode::from_u16(m.status).unwrap(),
                            [("content-type", "application/json")],
                            r#"{"type":"error","error":{"type":"api_error","message":"synthetic"}}"#
                                .to_string(),
                        )
                            .into_response()
                    }
                }),
            )
            .layer(axum::extract::DefaultBodyLimit::max(64 * 1024 * 1024))
            .with_state(mock.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), mock)
    }

    /// `before_message` hook that counts its calls and injects per-turn text.
    struct CountingInject(Arc<AtomicUsize>);
    #[async_trait::async_trait]
    impl crate::extensions::runtime::ExtensionHandler for CountingInject {
        fn id(&self) -> &str {
            "keepwarm-counting-inject"
        }
        async fn handle(&self, _event: &HookEvent) -> HookResult {
            self.0.fetch_add(1, Ordering::SeqCst);
            HookResult::Inject {
                content: "PER-TURN-INJECTED-CONTEXT".into(),
            }
        }
        async fn shutdown(&self) {}
    }

    /// Run one real turn and return its final durable history.
    async fn real_turn(rt: &Runtime, messages: Vec<SharedMessage>) -> Vec<SharedMessage> {
        let mut stream = rt
            .run_stream_with_messages(messages, CancellationToken::new(), None, None, false)
            .await;
        let mut history = None;
        while let Some(event) = stream.next().await {
            match event {
                StreamEvent::Session(SessionEvent::MessageHistory(h)) => history = Some(h),
                StreamEvent::Session(SessionEvent::Error(e)) => panic!("turn failed: {e:?}"),
                _ => {}
            }
        }
        history.expect("turn published its history")
    }

    fn user(text: &str) -> SharedMessage {
        Arc::new(json!({"role": "user", "content": text}))
    }

    fn canon(v: &Value) -> String {
        serde_json::to_string(v).unwrap()
    }

    fn strip_cache_control(mut msg: Value) -> Value {
        if let Some(blocks) = msg["content"].as_array_mut() {
            for b in blocks {
                if let Some(obj) = b.as_object_mut() {
                    obj.remove("cache_control");
                }
            }
        }
        msg
    }

    fn marker_count(v: &Value) -> usize {
        match v {
            Value::Object(map) => {
                usize::from(map.contains_key("cache_control"))
                    + map.values().map(marker_count).sum::<usize>()
            }
            Value::Array(items) => items.iter().map(marker_count).sum(),
            _ => 0,
        }
    }

    /// What [`ping_then_next_turn`] observed.
    struct Observed {
        ping: Value,
        real: Value,
        raw_ping: String,
        history_len: usize,
        usage: KeepWarmUsage,
        /// `before_message` calls before the ping, after it, after the next turn.
        hook_calls: (usize, usize, usize),
    }

    /// Previous turn → probe → keep-warm ping → the real next turn, on one
    /// runtime. Also proves: the probe sends nothing and its fingerprint is
    /// the one the ping accepts; the ping does not touch history; accepted
    /// requests move the cache anchor.
    async fn ping_then_next_turn(rt: &Runtime) -> Observed {
        let (url, mock) = spawn_mock().await;
        let _url = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
        let calls = Arc::new(AtomicUsize::new(0));
        let mut perms = PermissionSet::new();
        perms.grant(Permission::LlmContent);
        rt.hook_bus()
            .subscribe(
                HookKind::BeforeMessage,
                Arc::new(CountingInject(Arc::clone(&calls))),
                None,
                None,
                perms,
            )
            .await
            .unwrap();

        assert_eq!(rt.cache_anchor(), None, "no request has run yet");
        // The previous turn: the session is idle afterwards.
        let history = real_turn(rt, vec![user("hello")]).await;
        assert_eq!(history.last().unwrap()["role"], "assistant");
        let turn_anchor = rt
            .cache_anchor()
            .expect("an accepted turn request stamps the anchor");
        let before_ping = calls.load(Ordering::SeqCst);
        let snapshot = history.clone();

        let fingerprint = rt
            .cache_keepwarm_probe(&history)
            .await
            .expect("probe")
            .fingerprint;
        assert_eq!(
            mock.bodies.lock().unwrap().len(),
            1,
            "a probe sends nothing"
        );
        assert_eq!(
            rt.cache_keepwarm_probe(&history).await.unwrap().fingerprint,
            fingerprint,
            "the fingerprint is deterministic"
        );

        let tap = PingTap::default();
        let report = rt
            .cache_keepwarm_ping(&history, fingerprint, &CancellationToken::new(), &tap)
            .await;
        assert_eq!(report.result, Ok(PingOutcome::Sent), "{report:?}");
        assert!(tap.was_sent(), "the tap saw the request leave");
        assert_eq!(tap.usage(), report.usage, "the tap saw the usage");
        let usage = report.usage;
        assert!(
            usage.output_tokens >= 1,
            "usage comes back from the private channel"
        );
        let ping_anchor = rt.cache_anchor().expect("anchor");
        assert!(
            ping_anchor.mono >= turn_anchor.mono,
            "the ping moved the anchor"
        );
        let after_ping = calls.load(Ordering::SeqCst);
        assert_eq!(history, snapshot, "the ping must not mutate history");

        // The real next turn.
        let mut next = history.clone();
        next.push(user("next question"));
        real_turn(rt, next).await;
        let after_next = calls.load(Ordering::SeqCst);

        let bodies = mock.bodies.lock().unwrap().clone();
        assert_eq!(bodies.len(), 3, "previous turn, ping, next turn");
        Observed {
            ping: serde_json::from_str(&bodies[1]).unwrap(),
            real: serde_json::from_str(&bodies[2]).unwrap(),
            raw_ping: bodies[1].clone(),
            history_len: history.len(),
            usage,
            hook_calls: (before_ping, after_ping, after_next),
        }
    }

    fn assert_prefix_identical(
        ping: &Value,
        real: &Value,
        n: usize,
        history_marker: &Value,
        ping_max_tokens: u64,
    ) {
        // Everything the cache key covers besides messages.
        for key in [
            "model",
            "tools",
            "system",
            "thinking",
            "output_config",
            "stream",
        ] {
            assert_eq!(
                canon(&ping[key]),
                canon(&real[key]),
                "`{key}` differs between ping and the real next turn"
            );
        }
        let pm = ping["messages"].as_array().unwrap();
        let rm = real["messages"].as_array().unwrap();
        assert_eq!(pm.len(), n + 1, "history + one trailing ping message");
        assert!(rm.len() > n);
        // History before the last message: byte-identical as sent.
        for i in 0..n - 1 {
            assert_eq!(canon(&pm[i]), canon(&rm[i]), "message {i} differs");
        }
        // The last history message: identical except the ping's marker,
        // which carries the history TTL (the governing one).
        let last = &pm[n - 1];
        let marker = &last["content"].as_array().unwrap().last().unwrap()["cache_control"];
        assert_eq!(marker, history_marker);
        assert_eq!(
            canon(&strip_cache_control(last.clone())),
            canon(&rm[n - 1]),
            "last history message differs beyond the cache marker"
        );
        // The trailing ping message sits AFTER the last breakpoint.
        assert_eq!(marker_count(&pm[n]), 0, "ping message must be unmarked");
        assert_eq!(pm[n]["role"], "user");
        assert_eq!(pm[n]["content"], KEEPWARM_PING_TEXT);
        // Output is cut; the real turn keeps its own max_tokens.
        assert_eq!(ping["max_tokens"], json!(ping_max_tokens));
        assert!(real["max_tokens"].as_u64().unwrap() > ping_max_tokens);
        assert!(
            marker_count(ping) <= 4,
            "Anthropic allows at most 4 breakpoints"
        );
    }

    fn marker_1h() -> Value {
        json!({"type": "ephemeral", "ttl": "1h"})
    }

    fn marker_5m() -> Value {
        json!({"type": "ephemeral"})
    }

    fn opus(ttl: CacheTtl) -> Runtime {
        let mut rt = Runtime::new_headless();
        rt.set_model("claude-opus-4-7".to_string());
        assert_eq!(rt.model(), "claude-opus-4-7");
        rt.set_reasoning_level_explicit(ReasoningLevel::High);
        rt.set_cache_ttl(ttl);
        rt.set_system_prompt("You are a keep-warm test.".into());
        assert_eq!(rt.cache_keepwarm_ineligibility(), None);
        rt
    }

    /// Adaptive thinking (Opus 4.7), 1h TTL: same thinking + effort, prefix
    /// through the last marker matches the real next turn, no hook ran for
    /// the ping, output capped at one token.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn ping_prefix_matches_next_turn_with_adaptive_thinking() {
        let _base = crate::test_env::BaseDirGuard::new();
        let rt = opus(CacheTtl::OneHour);

        let o = ping_then_next_turn(&rt).await;
        assert!(
            !o.ping["thinking"].is_null(),
            "thinking must stay on: {}",
            o.ping
        );
        assert_prefix_identical(&o.ping, &o.real, o.history_len, &marker_1h(), 1);

        // Hooks: live for real turns, never for the ping.
        let (before, after_ping, after_next) = o.hook_calls;
        assert!(
            before >= 1,
            "control: the previous turn fired before_message"
        );
        assert_eq!(after_ping, before, "the ping fired a hook");
        assert!(
            after_next > after_ping,
            "control: the next turn fired before_message"
        );
        assert!(!o.raw_ping.contains("PER-TURN-INJECTED-CONTEXT"));
        assert!(!o.raw_ping.contains("Extension context"));
        // Control: the real next turn DID carry the injection — in its tail.
        assert!(canon(&o.real).contains("PER-TURN-INJECTED-CONTEXT"));
    }

    /// 5m TTL is first-class: same identity, bare ephemeral marker.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn ping_prefix_matches_next_turn_with_5m_ttl() {
        let _base = crate::test_env::BaseDirGuard::new();
        let rt = opus(CacheTtl::FiveMinutes);
        let o = ping_then_next_turn(&rt).await;
        assert_prefix_identical(&o.ping, &o.real, o.history_len, &marker_5m(), 1);
    }

    /// Hybrid: the history marker is the 5m one (it governs the interval);
    /// tools/system keep their 1h markers byte-identically.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn ping_prefix_matches_next_turn_with_hybrid_ttl() {
        let _base = crate::test_env::BaseDirGuard::new();
        let rt = opus(CacheTtl::Hybrid);
        let o = ping_then_next_turn(&rt).await;
        assert_prefix_identical(&o.ping, &o.real, o.history_len, &marker_5m(), 1);
        assert!(
            canon(&o.ping["system"]).contains("\"ttl\":\"1h\""),
            "hybrid keeps the 1h stable-prefix marker: {}",
            o.ping["system"]
        );
    }

    /// Thinking off on a budgeted-thinking model.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn ping_prefix_matches_next_turn_with_thinking_off() {
        let _base = crate::test_env::BaseDirGuard::new();
        let mut rt = Runtime::new_headless();
        rt.set_model("claude-sonnet-4-6".to_string());
        rt.set_reasoning_level_explicit(ReasoningLevel::Off);
        rt.set_cache_ttl(CacheTtl::OneHour);
        assert_eq!(rt.cache_keepwarm_ineligibility(), None);

        let o = ping_then_next_turn(&rt).await;
        assert!(o.ping.get("thinking").is_none(), "thinking off is omitted");
        assert_prefix_identical(&o.ping, &o.real, o.history_len, &marker_1h(), 1);
    }

    /// Budgeted thinking (Sonnet 4.6, `type: enabled`) is SUPPORTED: the
    /// budget is part of the cache key and stays; `max_tokens` (not part of
    /// the key) is `budget + 1`, the API's minimum; the ping is costed from
    /// its measured usage. Under 5m TTL here.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn budgeted_thinking_ping_keeps_budget_and_is_costed() {
        let _base = crate::test_env::BaseDirGuard::new();
        let mut rt = Runtime::new_headless();
        rt.set_model("claude-sonnet-4-6".to_string());
        rt.set_reasoning_level_explicit(ReasoningLevel::High);
        rt.set_cache_ttl(CacheTtl::FiveMinutes);
        assert_eq!(
            rt.cache_keepwarm_ineligibility(),
            None,
            "budgeted thinking is eligible"
        );

        let o = ping_then_next_turn(&rt).await;
        assert_eq!(o.ping["thinking"]["type"], "enabled", "{}", o.ping);
        let budget = o.ping["thinking"]["budget_tokens"]
            .as_u64()
            .expect("budget");
        assert_eq!(canon(&o.ping["thinking"]), canon(&o.real["thinking"]));
        assert_prefix_identical(
            &o.ping,
            &o.real,
            o.history_len,
            &marker_5m(),
            budget + KEEPWARM_OUTPUT_TOKENS,
        );
        assert!(
            o.usage.cost_usd("claude-sonnet-4-6") > 0.0,
            "the ping is costed"
        );
    }

    /// A changed prefix is never sent: model, thinking level, system prompt
    /// and history changes all flip the fingerprint, the ping reports
    /// `PrefixChanged`, and the provider sees no request.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn changed_prefix_is_refused_before_the_network() {
        let _base = crate::test_env::BaseDirGuard::new();
        let (url, mock) = spawn_mock().await;
        let _url = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
        let mut rt = opus(CacheTtl::FiveMinutes);
        let history = real_turn(&rt, vec![user("hello")]).await;
        let fp = rt.cache_keepwarm_probe(&history).await.unwrap().fingerprint;
        let sent = mock.bodies.lock().unwrap().len();

        // History grew (e.g. a compaction / /clear / a new turn).
        let mut longer = history.clone();
        longer.push(user("more"));
        longer.push(Arc::new(
            json!({"role": "assistant", "content": [{"type": "text", "text": "x"}]}),
        ));
        let changed = |r: PingReport| matches!(r.result, Ok(PingOutcome::PrefixChanged { .. }));
        let tap = PingTap::default();
        assert!(changed(
            rt.cache_keepwarm_ping(&longer, fp, &CancellationToken::new(), &tap)
                .await
        ));

        rt.set_system_prompt("A different system prompt.".into());
        assert_ne!(
            rt.cache_keepwarm_probe(&history).await.unwrap().fingerprint,
            fp
        );
        assert!(changed(
            rt.cache_keepwarm_ping(&history, fp, &CancellationToken::new(), &tap)
                .await
        ));
        rt.set_system_prompt("You are a keep-warm test.".into());
        assert_eq!(
            rt.cache_keepwarm_probe(&history).await.unwrap().fingerprint,
            fp,
            "restored"
        );

        rt.set_reasoning_level_explicit(ReasoningLevel::Low);
        assert_ne!(
            rt.cache_keepwarm_probe(&history).await.unwrap().fingerprint,
            fp,
            "effort is in the key"
        );
        rt.set_reasoning_level_explicit(ReasoningLevel::High);

        rt.set_model("claude-opus-4-6".to_string());
        assert_eq!(rt.model(), "claude-opus-4-6");
        assert_ne!(
            rt.cache_keepwarm_probe(&history).await.unwrap().fingerprint,
            fp,
            "model is in the key"
        );

        assert_eq!(
            mock.bodies.lock().unwrap().len(),
            sent,
            "no refused ping reached the provider"
        );
        assert!(!tap.was_sent(), "a refused ping never leaves");
    }

    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn http_4xx_is_fatal_and_5xx_is_transient() {
        let _base = crate::test_env::BaseDirGuard::new();
        let history = vec![
            user("hello"),
            Arc::new(json!({"role": "assistant", "content": [{"type": "text", "text": "hi"}]})),
        ];
        for (status, fatal) in [(400u16, true), (404, true), (500, false), (429, false)] {
            let (url, mock) = spawn_mock_with(status).await;
            let _url = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
            let rt = opus(CacheTtl::FiveMinutes);
            let fp = rt.cache_keepwarm_probe(&history).await.unwrap().fingerprint;
            let err = rt
                .cache_keepwarm_ping(&history, fp, &CancellationToken::new(), &PingTap::default())
                .await
                .result
                .expect_err("non-2xx fails");
            assert_eq!(
                matches!(err, PingFailure::Fatal(_)),
                fatal,
                "HTTP {status}: {err:?}"
            );
            assert_eq!(
                mock.bodies.lock().unwrap().len(),
                1,
                "HTTP {status}: no retry"
            );
            assert_eq!(
                rt.cache_anchor(),
                None,
                "a rejected request refreshes nothing"
            );
        }
    }

    /// The prefix-identity proof for one more request shape: the ping and
    /// the real next turn send identical model/tools/system/thinking/effort
    /// and history through the marker.
    async fn assert_identity_1h(rt: &Runtime) -> Observed {
        let o = ping_then_next_turn(rt).await;
        assert_prefix_identical(&o.ping, &o.real, o.history_len, &marker_1h(), 1);
        o
    }

    /// Progressive tool disclosure: the tool schema is the projection of the
    /// retained session tool set, selected exactly as a turn start selects
    /// it (`select_turn_start_tool_set`).
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn ping_prefix_matches_next_turn_with_progressive_disclosure() {
        let _base = crate::test_env::BaseDirGuard::new();
        let mut rt = opus(CacheTtl::OneHour);
        rt.progressive_tool_disclosure = true;
        assert_identity_1h(&rt).await;
    }

    /// Deferred tool loading on top of progressive disclosure: the
    /// loaded/deferred split is part of the identity.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn ping_prefix_matches_next_turn_with_deferred_tools() {
        let _base = crate::test_env::BaseDirGuard::new();
        let mut rt = opus(CacheTtl::OneHour);
        rt.progressive_tool_disclosure = true;
        rt.deferred_tool_loading = true;
        assert_identity_1h(&rt).await;
    }

    /// A ping projects exactly what the next turn start selects — including
    /// activations handed over by a daemon reload or an unpark that the turn
    /// start has not restored yet. (The session only schedules pings after
    /// an accepted request of the same runtime, so this is the projection
    /// invariant, not a reachable pre-turn ping.)
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn ping_projects_carried_activations_the_turn_start_restores() {
        use crate::tools::activation::{activate_exact_for_user, SessionId, SessionToolSet};
        use crate::tools::catalog::ToolId;
        let _base = crate::test_env::BaseDirGuard::new();
        let (url, mock) = spawn_mock().await;
        let _url = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
        let mut rt = opus(CacheTtl::OneHour);
        rt.progressive_tool_disclosure = true;
        rt.deferred_tool_loading = true;
        let carried = {
            let registry = rt.tools.read().await;
            let mut previous = SessionToolSet::progressive_core_for_catalog(
                SessionId::parse("previous-runtime").unwrap(),
                registry.catalog(),
            );
            activate_exact_for_user(
                &mut previous,
                registry.catalog(),
                &ToolId::builtin("subagent_status"),
            )
            .unwrap();
            previous.carried_activations()
        };
        rt.restore_tool_activations(carried);

        let history = vec![
            user("hello"),
            Arc::new(json!({"role": "assistant", "content": [{"type": "text", "text": "hi"}]})),
        ];
        let fp = rt.cache_keepwarm_probe(&history).await.unwrap().fingerprint;
        let report = rt
            .cache_keepwarm_ping(&history, fp, &CancellationToken::new(), &PingTap::default())
            .await;
        assert_eq!(report.result, Ok(PingOutcome::Sent), "{report:?}");
        let mut next = history.clone();
        next.push(user("next"));
        real_turn(&rt, next).await;

        let bodies: Vec<Value> = mock
            .bodies
            .lock()
            .unwrap()
            .iter()
            .map(|b| serde_json::from_str(b).unwrap())
            .collect();
        let (ping, real) = (&bodies[0], &bodies[1]);
        // The non-deferred tools are the cached prefix. (The deferred list
        // can still differ here: this runtime's first turn applies its
        // idempotent setup, e.g. hiding the forum tools under the legacy
        // memory backend; deferred definitions are outside the prefix.)
        let loaded = |b: &Value| -> Vec<Value> {
            b["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|t| t.get("defer_loading").is_none())
                .cloned()
                .collect()
        };
        assert_eq!(
            serde_json::to_string(&loaded(ping)).unwrap(),
            serde_json::to_string(&loaded(real)).unwrap()
        );
        assert!(
            real["tools"].as_array().unwrap().iter().any(|t| {
                t["name"] == "subagent_status" && t.get("defer_loading").is_none()
            }),
            "the restored activation is a loaded tool: {}",
            real["tools"]
        );
    }

    /// Context management on: the guidance is appended to the system prompt
    /// of both.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn ping_prefix_matches_next_turn_with_context_management_guidance() {
        let _base = crate::test_env::BaseDirGuard::new();
        let rt = opus(CacheTtl::OneHour);
        rt.continuation.lock().unwrap().config.mode =
            crate::core::config::ContextManagementMode::Auto;
        let o = assert_identity_1h(&rt).await;
        assert!(
            canon(&o.ping["system"]).contains("Work normally."),
            "guidance present: {}",
            o.ping["system"]
        );
    }

    /// A session-scoped extension injection rides in the system prompt of
    /// both (unlike the per-turn `before_message` injection, which is tail).
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn ping_prefix_matches_next_turn_with_session_injection() {
        let _base = crate::test_env::BaseDirGuard::new();
        let mut rt = opus(CacheTtl::OneHour);
        rt.set_session_id(Some("kw-session".into()));
        rt.hook_bus()
            .set_session_injection_for("kw-session", "SESSION-SCOPED-INJECTION".into())
            .await;
        let o = assert_identity_1h(&rt).await;
        assert!(canon(&o.ping["system"]).contains("SESSION-SCOPED-INJECTION"));
    }

    /// OAuth: the Claude Code transport identity and the extra system blocks
    /// are identical on both.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn ping_prefix_matches_next_turn_with_oauth_system_blocks() {
        let _base = crate::test_env::BaseDirGuard::new();
        let mut rt = opus(CacheTtl::OneHour);
        rt.auth = Arc::new(tokio::sync::RwLock::new(super::super::types::AuthState {
            auth_token: "oauth-test-token".to_string(),
            auth_type: "oauth".to_string(),
            refresh_token: None,
            token_expires: Some(crate::epoch_millis() + 3_600_000),
            auth_file_mtime: None,
            // Bound to the default local seat so the turn's binding check
            // takes the fast path instead of re-resolving from auth.json.
            bound_credential: Some(crate::runtime::auth::anthropic_binding(
                &crate::auth::CredentialSource::Local,
                &crate::auth::CredentialRef::default_for(crate::auth::OAuthProviderId::Anthropic),
            )),
        }));
        let o = assert_identity_1h(&rt).await;
        assert!(
            canon(&o.ping["system"]).contains("You are Claude Code"),
            "{}",
            o.ping["system"]
        );
    }

    /// Context rollover ahead: the probe runs the next turn's pre-request
    /// assessment (history + an empty user message) and reports a Rollover /
    /// HardStop without committing any policy state.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn probe_reports_a_context_rollover_ahead() {
        let _base = crate::test_env::BaseDirGuard::new();
        let (url, _mock) = spawn_mock().await;
        let _url = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
        let rt = opus(CacheTtl::OneHour);
        rt.continuation.lock().unwrap().config.mode =
            crate::core::config::ContextManagementMode::Auto;
        let history = real_turn(&rt, vec![user("hello")]).await;
        let probe = rt.cache_keepwarm_probe(&history).await.unwrap();
        assert_eq!(probe.context_blocks, None, "plenty of room");
        let before = rt.continuation.lock().unwrap().policy;
        // The next round needs more headroom than the window has left.
        rt.continuation.lock().unwrap().config.reserve_tokens = 10_000_000;
        let probe = rt.cache_keepwarm_probe(&history).await.unwrap();
        assert!(probe.context_blocks.is_some(), "{probe:?}");
        assert_eq!(
            rt.continuation.lock().unwrap().policy,
            before,
            "the probe never commits policy state"
        );
        // Context management off: no assessment, like a real turn.
        rt.continuation.lock().unwrap().config.mode =
            crate::core::config::ContextManagementMode::Off;
        assert_eq!(
            rt.cache_keepwarm_probe(&history)
                .await
                .unwrap()
                .context_blocks,
            None
        );
    }

    /// The probe shares the turn's context budget, so it gains the realistic
    /// output reserve too: an Opus session at ~60k of its 200k window is
    /// warmed. With the full 128k `max_tokens` reserved the next turn was at
    /// hard capacity from ~31.5k on and the probe refused every ping there.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn probe_warms_an_opus_200k_session_past_the_old_hard_stop() {
        let _base = crate::test_env::BaseDirGuard::new();
        let (url, _mock) = spawn_mock().await;
        let _url = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
        let rt = opus(CacheTtl::OneHour);
        assert_eq!(rt.context_window(), 200_000);
        rt.continuation.lock().unwrap().config.mode =
            crate::core::config::ContextManagementMode::Auto;
        let history = real_turn(&rt, vec![user(&"x".repeat(150_000))]).await;
        let used = rt.assess_context(&history).await.used_tokens();
        assert!((60_000..100_000).contains(&used), "{used}");
        let probe = rt.cache_keepwarm_probe(&history).await.unwrap();
        assert_eq!(probe.context_blocks, None, "{probe:?}");
    }

    /// The probe reports the ping's real `max_tokens` (worst-case output).
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn probe_reports_max_tokens() {
        let _base = crate::test_env::BaseDirGuard::new();
        let history = vec![
            user("hello"),
            Arc::new(json!({"role": "assistant", "content": [{"type": "text", "text": "hi"}]})),
        ];
        let rt = opus(CacheTtl::OneHour);
        assert_eq!(
            rt.cache_keepwarm_probe(&history).await.unwrap().max_tokens,
            1
        );
        let mut rt = Runtime::new_headless();
        rt.set_model("claude-sonnet-4-6".to_string());
        rt.set_reasoning_level_explicit(ReasoningLevel::XHigh);
        let probe = rt.cache_keepwarm_probe(&history).await.unwrap();
        let budget = u64::from(rt.thinking_budget);
        assert!(budget >= 16_384, "budgeted thinking at xhigh: {budget}");
        assert_eq!(
            probe.max_tokens,
            budget + KEEPWARM_OUTPUT_TOKENS,
            "{probe:?}"
        );
    }

    /// Usage is returned on EVERY path: a ping that was billed and then
    /// failed in-stream still reports what it cost (and so does the tap).
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn ping_usage_is_returned_on_error() {
        let _base = crate::test_env::BaseDirGuard::new();
        let (url, mock) = spawn_mock_full(200, SSE_USAGE_THEN_ERROR).await;
        let _url = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
        let history = vec![
            user("hello"),
            Arc::new(json!({"role": "assistant", "content": [{"type": "text", "text": "hi"}]})),
        ];
        let rt = opus(CacheTtl::OneHour);
        let fp = rt.cache_keepwarm_probe(&history).await.unwrap().fingerprint;
        let tap = PingTap::default();
        let report = rt
            .cache_keepwarm_ping(&history, fp, &CancellationToken::new(), &tap)
            .await;
        assert!(
            matches!(report.result, Err(PingFailure::Transient(_))),
            "{report:?}"
        );
        assert_eq!(report.usage.cache_read_tokens, 5_000, "{report:?}");
        assert!(report.usage.cost_usd("claude-opus-4-7") > 0.0);
        assert_eq!(tap.usage(), report.usage);
        assert!(tap.was_sent());
        assert_eq!(mock.bodies.lock().unwrap().len(), 1, "no transport retry");
    }

    /// `cache_diagnostics = true`: a ping keeps the turn's beta set (cache-key
    /// parity — the probe and the ping agree on the fingerprint, so it is
    /// sent) but never sends `diagnostics` and never moves the turn chain.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn ping_never_carries_or_moves_the_diagnostics_chain() {
        let _base = crate::test_env::BaseDirGuard::new();
        let (url, mock) = spawn_mock().await;
        let _url = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
        let history = vec![
            user("hello"),
            Arc::new(json!({"role": "assistant", "content": [{"type": "text", "text": "hi"}]})),
        ];
        let mut rt = opus(CacheTtl::OneHour);
        rt.set_cache_diagnostics(true);
        rt.cache_diagnosis().unwrap().record("msg_turn");
        let mut ping_rt = rt.clone();
        ping_rt.share_ttl_latches(&rt);
        let fp = ping_rt
            .cache_keepwarm_probe(&history)
            .await
            .unwrap()
            .fingerprint;
        let report = ping_rt
            .cache_keepwarm_ping(&history, fp, &CancellationToken::new(), &PingTap::default())
            .await;
        assert!(report.result.is_ok(), "{report:?}");
        let bodies = mock.bodies.lock().unwrap().clone();
        assert_eq!(bodies.len(), 1);
        assert!(!bodies[0].contains("\"diagnostics\""), "{}", bodies[0]);
        assert_eq!(
            rt.cache_diagnosis()
                .unwrap()
                .previous_message_id()
                .as_deref(),
            Some("msg_turn"),
            "the ping's msg_kw never enters the chain"
        );
    }

    /// Driver-turn and ping clones continue the session's chain; a plain
    /// clone (subagent-style) starts its own chain but shares the rejection
    /// latch; off means no chain at all.
    #[test]
    fn diagnostics_chain_is_shared_like_the_ttl_latches() {
        let mut rt = opus(CacheTtl::FiveMinutes);
        assert!(rt.cache_diagnosis().is_none(), "off: wire unchanged");
        rt.set_cache_diagnostics(true);
        rt.cache_diagnosis().unwrap().record("msg_a");
        let fresh = rt.clone();
        assert_eq!(fresh.cache_diagnosis().unwrap().previous_message_id(), None);
        let mut shared = rt.clone();
        shared.share_ttl_latches(&rt);
        shared.cache_diagnosis().unwrap().record("msg_b");
        shared.cache_diagnosis().unwrap().reject(400);
        let chain = rt.cache_diagnosis().unwrap();
        assert_eq!(chain.previous_message_id().as_deref(), Some("msg_b"));
        assert!(!chain.active(), "the rejection latch is session-wide");
        // Subagent-style clones keep their own chain but share the latch:
        // a rejecting API is paid for (and logged) once per session.
        let fresh_chain = fresh.cache_diagnosis().unwrap();
        assert!(!fresh_chain.active(), "latch shared across Clone");
        assert_eq!(fresh_chain.previous_message_id(), None);
        assert!(!rt.clone().cache_diagnosis().unwrap().active());
    }

    /// The ping shares the session's silent-downgrade latch (not a fresh
    /// one): a downgrade it observes is the session's one notice.
    #[test]
    fn ping_shares_the_session_ttl_latch() {
        let rt = opus(CacheTtl::OneHour);
        assert!(!rt.cache_ttl_downgraded());
        let mut ping_rt = rt.clone();
        ping_rt.share_ttl_latches(&rt);
        ping_rt
            .ttl_downgrade_notified
            .store(true, Ordering::Relaxed);
        assert!(rt.cache_ttl_downgraded(), "the latch is shared");
    }

    #[test]
    fn failure_classification() {
        let api = || RuntimeError::ApiStatus("x".into());
        assert_eq!(
            classify_failure(&RuntimeError::Canceled, 0),
            PingFailure::Canceled
        );
        assert!(matches!(
            classify_failure(&api(), 400),
            PingFailure::Fatal(_)
        ));
        assert!(matches!(
            classify_failure(&api(), 401),
            PingFailure::Fatal(_)
        ));
        assert!(matches!(
            classify_failure(&api(), 429),
            PingFailure::Transient(_)
        ));
        assert!(matches!(
            classify_failure(&api(), 529),
            PingFailure::Transient(_)
        ));
        assert!(matches!(
            classify_failure(&api(), 0),
            PingFailure::Transient(_)
        ));
        // An in-stream error under a 200 is transient.
        assert!(matches!(
            classify_failure(&api(), 200),
            PingFailure::Transient(_)
        ));
        assert!(matches!(
            classify_failure(&RuntimeError::Config("x".into()), 0),
            PingFailure::Fatal(_)
        ));
    }

    /// 3f2be9a3 split upload/header and stream-idle timeouts into
    /// `NetworkTimeout`. A slow ping is a network blip: back off and retry,
    /// never end the keep-warm period.
    #[test]
    fn network_timeout_is_transient() {
        for msg in ["upload/header deadline", "stream idle"] {
            assert_eq!(
                classify_failure(&RuntimeError::NetworkTimeout(msg), 0),
                PingFailure::Transient("network".to_string()),
                "{msg}"
            );
        }
    }

    #[test]
    fn every_ttl_and_thinking_mode_is_eligible() {
        for ttl in [CacheTtl::FiveMinutes, CacheTtl::OneHour, CacheTtl::Hybrid] {
            for (model, level) in [
                ("claude-opus-4-7", ReasoningLevel::High),
                ("claude-sonnet-4-6", ReasoningLevel::High),
                ("claude-sonnet-4-6", ReasoningLevel::Off),
            ] {
                let mut rt = Runtime::new_headless();
                rt.set_model(model.to_string());
                rt.set_reasoning_level_explicit(level);
                rt.set_cache_ttl(ttl);
                assert_eq!(
                    rt.cache_keepwarm_ineligibility(),
                    None,
                    "{model} {level:?} {ttl:?}"
                );
            }
        }
    }

    #[test]
    fn non_anthropic_models_are_ineligible() {
        let mut rt = Runtime::new_headless();
        rt.set_model("groq/llama-3.3-70b-versatile".to_string());
        if rt.model() == "groq/llama-3.3-70b-versatile" {
            assert!(rt.cache_keepwarm_ineligibility().is_some());
        }
    }

    /// An ineligible ping (non-Anthropic model) fails closed, fatally, before
    /// any network I/O.
    #[tokio::test]
    async fn ineligible_ping_sends_nothing() {
        let mut rt = Runtime::new_headless();
        rt.set_model("groq/llama-3.3-70b-versatile".to_string());
        if rt.model() != "groq/llama-3.3-70b-versatile" {
            return;
        }
        let history = vec![
            user("hello"),
            Arc::new(json!({"role": "assistant", "content": [{"type": "text", "text": "hi"}]})),
        ];
        let tap = PingTap::default();
        let err = rt
            .cache_keepwarm_ping(&history, 0, &CancellationToken::new(), &tap)
            .await
            .result
            .expect_err("non-Anthropic must refuse");
        assert!(!tap.was_sent());
        assert!(
            matches!(err, PingFailure::Fatal(ref m) if m.contains("keep-warm skipped")),
            "{err:?}"
        );
    }

    #[test]
    fn anchor_cell_only_moves_forward() {
        let cell = CacheAnchorCell::default();
        assert_eq!(cell.get(), None);
        let a = CacheAnchor::now();
        let later = CacheAnchor {
            mono: a.mono + std::time::Duration::from_secs(5),
            wall: a.wall + std::time::Duration::from_secs(5),
        };
        cell.stamp(later);
        cell.stamp(a);
        assert_eq!(
            cell.get(),
            Some(later),
            "a late-arriving older stamp never rewinds"
        );
    }

    #[test]
    fn only_a_finished_assistant_text_reply_is_markable() {
        assert!(tail_is_markable(
            &json!({"role": "assistant", "content": [{"type": "thinking", "thinking": "t"}, {"type": "text", "text": "ok"}]})
        ));
        assert!(!tail_is_markable(&json!({"role": "user", "content": "q"})));
        assert!(!tail_is_markable(
            &json!({"role": "assistant", "content": [{"type": "tool_use", "id": "t", "name": "x", "input": {}}]})
        ));
        assert!(!tail_is_markable(
            &json!({"role": "assistant", "content": [{"type": "thinking", "thinking": "t"}]})
        ));
    }

    #[test]
    fn usage_is_summed_and_priced() {
        let mut u = KeepWarmUsage::default();
        u.add(&StreamEvent::Session(SessionEvent::Usage {
            input_tokens: 20,
            output_tokens: 16,
            cache_read_input_tokens: 500_000,
            cache_creation_input_tokens: 0,
            cache_creation_5m: None,
            cache_creation_1h: Some(0),
            model: Some("claude-opus-4-7".into()),
        }));
        assert_eq!(u.cache_read_tokens, 500_000);
        assert_eq!(u.output_tokens, 16);
        // 500k reads at $0.50/MTok + 16 output at $25/MTok + 20 input at $5/MTok.
        let cost = u.cost_usd("claude-opus-4-7");
        assert!((cost - 0.2505).abs() < 0.001, "{cost}");
    }

    /// The one booking rule (I-2), as a table.
    #[test]
    fn booking_rule_table() {
        use PingBooking::*;
        // (usage arrived, sent, connect failed, HTTP status) → booking
        let table = [
            ((true, true, false, 200), Measured),
            ((true, false, false, 0), Measured),
            ((true, true, false, 529), Measured),
            ((false, true, false, 200), WorstCase), // aborted mid-stream
            ((false, true, false, 0), WorstCase),   // read timeout after dispatch
            ((false, true, false, 500), Nothing),   // rejected by status: not billed
            ((false, true, false, 429), Nothing),
            ((false, true, false, 529), Nothing),
            ((false, true, true, 0), Nothing), // connect failure, no status
            ((false, true, true, 503), Nothing), // an error status, no usage: not billed
            ((false, false, false, 0), Nothing), // refused before the network
            ((false, false, true, 0), Nothing),
        ];
        for ((usage, sent, connect, status), want) in table {
            assert_eq!(
                ping_booking(usage, sent, connect, status),
                want,
                "usage={usage} sent={sent} connect_failed={connect} status={status}"
            );
        }
        // And the bill prices each booking.
        let tap = PingTap::default();
        assert_eq!(tap.bill("claude-opus-4-7", 0.5).cost_usd, 0.0);
        tap.sent.store(true, Ordering::Relaxed);
        let bill = tap.bill("claude-opus-4-7", 0.5);
        assert_eq!((bill.booking, bill.cost_usd), (WorstCase, 0.5));
        tap.usage.lock().unwrap().cache_read_tokens = 1_000_000;
        let bill = tap.bill("claude-opus-4-7", 0.5);
        assert_eq!(bill.booking, Measured);
        assert!((bill.cost_usd - 0.5).abs() < 1e-9, "1M reads at $0.50/MTok");
    }

    fn two_message_history() -> Vec<SharedMessage> {
        vec![
            user("hello"),
            Arc::new(json!({"role": "assistant", "content": [{"type": "text", "text": "hi"}]})),
        ]
    }

    /// A refused connection never marks the ping sent: it is booked at $0.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn a_refused_connection_is_never_sent_and_books_nothing() {
        let _base = crate::test_env::BaseDirGuard::new();
        let addr = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap()
        }; // dropped: nothing listens there now
        let _url = crate::test_env::EnvVarGuard::set(
            "SYNAPS_ANTHROPIC_BASE_URL",
            &format!("http://{addr}"),
        );
        let history = two_message_history();
        let rt = opus(CacheTtl::OneHour);
        let fp = rt.cache_keepwarm_probe(&history).await.unwrap().fingerprint;
        let tap = PingTap::default();
        let report = rt
            .cache_keepwarm_ping(&history, fp, &CancellationToken::new(), &tap)
            .await;
        assert!(report.result.is_err(), "{report:?}");
        assert!(!tap.was_sent(), "never dispatched");
        assert!(tap.connect_failed.load(Ordering::Relaxed));
        let bill = tap.bill("claude-opus-4-7", 0.75);
        assert_eq!((bill.booking, bill.cost_usd), (PingBooking::Nothing, 0.0));
    }

    /// The body went out, the provider never answered (read timeout / the
    /// actor aborted it): no usage, but it may be billed — worst case.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn a_dispatched_ping_without_usage_books_the_worst_case() {
        use tokio::io::AsyncReadExt;
        let _base = crate::test_env::BaseDirGuard::new();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Accept, swallow the request, never respond.
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 64 * 1024];
            while sock.read(&mut buf).await.is_ok_and(|n| n > 0) {}
        });
        let _url = crate::test_env::EnvVarGuard::set(
            "SYNAPS_ANTHROPIC_BASE_URL",
            &format!("http://{addr}"),
        );
        let history = two_message_history();
        let rt = opus(CacheTtl::OneHour);
        let fp = rt.cache_keepwarm_probe(&history).await.unwrap().fingerprint;
        let tap = PingTap::default();
        let cancel = CancellationToken::new();
        let ping = {
            let (rt, history, tap, cancel) =
                (rt.clone(), history.clone(), tap.clone(), cancel.clone());
            tokio::spawn(async move { rt.cache_keepwarm_ping(&history, fp, &cancel, &tap).await })
        };
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !tap.was_sent() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the body was pulled over the established connection");
        cancel.cancel();
        let report = ping.await.unwrap();
        assert_eq!(report.result, Err(PingFailure::Canceled));
        assert!(!tap.connect_failed.load(Ordering::Relaxed));
        let bill = tap.bill("claude-opus-4-7", 0.75);
        assert_eq!(
            (bill.booking, bill.cost_usd),
            (PingBooking::WorstCase, 0.75)
        );
    }

    #[test]
    fn reply_prefix_tokens_skip_thinking() {
        let text = "a".repeat(250); // 100 tokens at 2.5 chars/token
        let with_thinking = vec![Arc::new(json!({"role": "assistant", "content": [
            {"type": "thinking", "thinking": "x".repeat(50_000), "signature": "s"},
            {"type": "text", "text": text},
        ]}))];
        let plain = vec![Arc::new(json!({"role": "assistant", "content": [
            {"type": "text", "text": text},
        ]}))];
        let t = reply_prefix_tokens(&with_thinking);
        assert_eq!(t, reply_prefix_tokens(&plain), "thinking is not counted");
        assert!((100..200).contains(&t), "{t}");
        assert_eq!(reply_prefix_tokens(&[]), 0);
    }
}
