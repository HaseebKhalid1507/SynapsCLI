//! Trace emission seam (Task 8, spec §6.2/§6.4).
//!
//! Transports hand fully-built, schema-valid [`RequestTrace`] records to a
//! [`TraceSink`]. The seam is deliberately tiny:
//!
//! - **Record rule (documented invariant):** one record is emitted per
//!   *actual transport attempt* (one HTTP send). A request that is retried
//!   N times therefore yields N+1 records sharing the same `request_id`,
//!   with strictly increasing `attempt` ordinals; record `attempt = k`
//!   carries the k−1 prior failed tries in `outcome.retries`, so the Task 7
//!   invariant `attempt == retries.len() + 1` holds for every record.
//!   Non-final attempts carry a typed `ProviderFailed` terminal describing
//!   that attempt's own failure; the final record carries the request's
//!   terminal outcome (`Completed`, `Canceled`, or `ProviderFailed`).
//!   A cancellation observed *between* attempts (during backoff sleep, when
//!   no send is in flight) emits no extra record — the preceding failed
//!   attempt was already recorded.
//! - **Correctness firewall:** nothing in this module can fail a request.
//!   Key I/O failure degrades the record (digest-bearing sections omitted)
//!   and bumps a metadata-only counter + one warning; it never propagates.
//! - **No persistence here:** the bounded background writer is Task 11. The
//!   default sink is a no-op; tests install [`CollectingTraceSink`].
//!
//! No hidden global mutable state: all shared state (lazy key cell,
//! counters, sequence) lives inside the [`TraceContext`] handle the caller
//! owns and clones.

use super::key::{
    keyed_digest, load_or_create_digest_key, load_or_create_digest_key_at, DigestDomain,
    TraceDigestKey,
};
use super::types::{
    CacheMeta, EndpointMeta, MessageMeta, RequestAnatomy, RequestTrace, RetryClass, RetryMeta,
    StopReason, SystemSegmentMeta, TimingStages, ToolMeta, TraceId, TraceSchemaVersion,
    TransportKind, TransportOutcome, UsageMeta, WireMeta,
};
use agent_core::prompt::QualifiedModelId;
use agent_core::TurnOutcome;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// --- Sink seam ---

/// Receives finished trace records. Implementations must be cheap and
/// non-blocking: they run inline on the request path.
pub trait TraceSink: Send + Sync + std::fmt::Debug {
    fn emit(&self, record: RequestTrace);
    /// When `false`, transports skip record construction entirely.
    fn enabled(&self) -> bool {
        true
    }
}

/// Default production sink until the Task 11 bounded writer lands: accepts
/// and discards. `enabled() == false` lets transports skip all trace work.
#[derive(Debug, Default)]
pub struct NoopTraceSink;

impl TraceSink for NoopTraceSink {
    fn emit(&self, _record: RequestTrace) {}
    fn enabled(&self) -> bool {
        false
    }
}

/// In-memory collector for deterministic tests.
#[derive(Debug, Default)]
pub struct CollectingTraceSink {
    records: std::sync::Mutex<Vec<RequestTrace>>,
}

impl CollectingTraceSink {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Snapshot of everything emitted so far, in emission order.
    pub fn records(&self) -> Vec<RequestTrace> {
        self.records.lock().expect("trace sink poisoned").clone()
    }
}

impl TraceSink for CollectingTraceSink {
    fn emit(&self, record: RequestTrace) {
        self.records
            .lock()
            .expect("trace sink poisoned")
            .push(record);
    }
}

// --- Context handle ---

/// Cloneable handle carrying the sink, the lazily-loaded digest key, the
/// session identity, and metadata-only degradation counters.
#[derive(Clone)]
pub struct TraceContext {
    sink: Arc<dyn TraceSink>,
    session_id: TraceId,
    /// `None` → the installation default key path.
    key_path: Option<Arc<PathBuf>>,
    /// Lazily initialized once per context; `None` inside means the key was
    /// unavailable (I/O failure) — records degrade, requests are unaffected.
    key: Arc<tokio::sync::OnceCell<Option<Arc<TraceDigestKey>>>>,
    key_warned: Arc<AtomicBool>,
    /// Count of records that were degraded or dropped for internal reasons
    /// (key unavailable, unrepresentable structural identity). Metadata only.
    degraded: Arc<AtomicU64>,
    request_seq: Arc<AtomicU64>,
}

impl std::fmt::Debug for TraceContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TraceContext")
            .field("enabled", &self.sink.enabled())
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

impl Default for TraceContext {
    fn default() -> Self {
        Self::disabled()
    }
}

impl TraceContext {
    /// Disabled context: no-op sink, no key I/O, zero request-path work.
    pub fn disabled() -> Self {
        Self::with_sink(Arc::new(NoopTraceSink))
    }

    /// Context with the given sink and the installation default key path.
    pub fn with_sink(sink: Arc<dyn TraceSink>) -> Self {
        let session_id = TraceId::new(format!("session-{}-{}", std::process::id(), unix_ms_now()))
            .expect("generated session id is valid");
        Self {
            sink,
            session_id,
            key_path: None,
            key: Arc::new(tokio::sync::OnceCell::new()),
            key_warned: Arc::new(AtomicBool::new(false)),
            degraded: Arc::new(AtomicU64::new(0)),
            request_seq: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Override the digest-key path (tests / non-default installs).
    pub fn with_key_path(mut self, path: PathBuf) -> Self {
        self.key_path = Some(Arc::new(path));
        self
    }

    pub fn enabled(&self) -> bool {
        self.sink.enabled()
    }

    /// Records degraded/dropped for internal reasons since context creation.
    pub fn degraded_records(&self) -> u64 {
        self.degraded.load(Ordering::Relaxed)
    }

    /// Load (or create) the installation digest key, once per context, off
    /// the async threads. Failure is remembered: it warns once (metadata
    /// only), bumps the degradation counter, and yields `None` forever —
    /// it can never fail or delay the request itself.
    pub async fn digest_key(&self) -> Option<Arc<TraceDigestKey>> {
        self.key
            .get_or_init(|| async {
                let path = self.key_path.clone();
                let loaded = tokio::task::spawn_blocking(move || match path {
                    Some(p) => load_or_create_digest_key_at(&p),
                    None => load_or_create_digest_key(),
                })
                .await;
                match loaded {
                    Ok(Ok(key)) => Some(Arc::new(key)),
                    Ok(Err(err)) => {
                        self.note_key_failure(&err.to_string());
                        None
                    }
                    Err(join_err) => {
                        self.note_key_failure(&join_err.to_string());
                        None
                    }
                }
            })
            .await
            .clone()
    }

    fn note_key_failure(&self, reason: &str) {
        self.degraded.fetch_add(1, Ordering::Relaxed);
        if !self.key_warned.swap(true, Ordering::Relaxed) {
            // Metadata-only: the reason describes our own key file I/O,
            // never provider or request content.
            tracing::warn!(
                reason,
                "trace digest key unavailable — traces degrade to digest-free records"
            );
        }
    }

    fn note_degraded(&self) {
        self.degraded.fetch_add(1, Ordering::Relaxed);
    }

    fn next_request_seq(&self) -> u64 {
        self.request_seq.fetch_add(1, Ordering::Relaxed)
    }
}

fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// --- Per-attempt monotonic clock ---

/// Monotonic ([`Instant`]-based) timing capture for one transport attempt.
/// Every mark is set-once; unobserved stages stay `None` (never zero).
#[derive(Debug, Clone, Copy)]
pub struct AttemptClock {
    start: Instant,
    send_start_unix_ms: u64,
    headers_ms: Option<u64>,
    first_byte_ms: Option<u64>,
    first_model_event_ms: Option<u64>,
    stream_end_ms: Option<u64>,
}

impl AttemptClock {
    /// Start the clock immediately before handing bytes to the transport.
    pub fn start() -> Self {
        Self {
            start: Instant::now(),
            send_start_unix_ms: unix_ms_now(),
            headers_ms: None,
            first_byte_ms: None,
            first_model_event_ms: None,
            stream_end_ms: None,
        }
    }

    fn elapsed_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    /// Response headers observed (set-once).
    pub fn mark_headers(&mut self) {
        if self.headers_ms.is_none() {
            self.headers_ms = Some(self.elapsed_ms());
        }
    }

    /// First response body byte / SSE frame observed (set-once).
    pub fn mark_first_byte(&mut self) {
        if self.first_byte_ms.is_none() {
            self.first_byte_ms = Some(self.elapsed_ms());
        }
    }

    /// First parsed model event, expressed as a monotonic offset from the
    /// headers mark (the stream parser measures from post-headers).
    pub fn set_first_model_event_after_headers(&mut self, offset_ms: u64) {
        if self.first_model_event_ms.is_none() {
            if let Some(headers) = self.headers_ms {
                self.first_model_event_ms = Some(headers.saturating_add(offset_ms));
            }
        }
    }

    /// First parsed model event, measured directly on this attempt's own
    /// monotonic clock (set-once). Used by transports that observe the
    /// event on the same task that owns the clock (OpenAI-compatible
    /// paths); the Anthropic stream parser instead reports a post-headers
    /// offset via [`Self::set_first_model_event_after_headers`].
    pub fn mark_first_model_event(&mut self) {
        if self.first_model_event_ms.is_none() {
            self.first_model_event_ms = Some(self.elapsed_ms());
        }
    }

    /// First parsed model event at a caller-measured monotonic offset from
    /// this attempt's send start (set-once). Used by transports whose stream
    /// events are observed on a task that does not own the clock (the
    /// extension-provider forwarder records the offset through an atomic and
    /// the owning task applies it here before finishing).
    pub fn set_first_model_event_offset(&mut self, offset_ms: u64) {
        if self.first_model_event_ms.is_none() {
            self.first_model_event_ms = Some(offset_ms);
        }
    }

    /// Stream fully consumed / request finished (set-once).
    pub fn mark_stream_end(&mut self) {
        if self.stream_end_ms.is_none() {
            self.stream_end_ms = Some(self.elapsed_ms());
        }
    }

    fn stages(&self) -> TimingStages {
        TimingStages {
            send_start_unix_ms: Some(self.send_start_unix_ms),
            headers_ms: self.headers_ms,
            first_byte_ms: self.first_byte_ms,
            first_model_event_ms: self.first_model_event_ms,
            stream_end_ms: self.stream_end_ms,
        }
    }
}

// --- Request structure (built once per request from trusted inputs) ---

/// Structural, metadata-only description of one outgoing request, built by a
/// transport-specific helper (see `trace::anthropic`) from trusted inputs —
/// including [`WireMeta`] computed from the exact bytes handed to reqwest.
#[derive(Debug, Clone, Default)]
pub struct RequestStructure {
    pub anatomy: RequestAnatomy,
    /// `None` when the digest key is unavailable — never re-serialized.
    pub wire: Option<WireMeta>,
    pub system_segments: Vec<SystemSegmentMeta>,
    pub messages: Vec<MessageMeta>,
    pub tools: Vec<ToolMeta>,
    pub cache: CacheMeta,
    /// Provider-adapter translation report entries (Task 9): every semantic
    /// loss/rewrite, positional IDs only. Populates `translation_losses`.
    pub translation: Vec<super::types::TranslationLoss>,
}

/// Compute exact-wire metadata from the very byte buffer handed to the
/// transport. This is the ONLY constructor for [`WireMeta`] used by
/// transports — there is no re-serialization path.
pub fn wire_meta_from_sent_bytes(key: &TraceDigestKey, sent_bytes: &[u8]) -> WireMeta {
    WireMeta {
        byte_len: sent_bytes.len() as u64,
        digest: keyed_digest(key, DigestDomain::Wire, sent_bytes),
    }
}

// --- Per-request tracer ---

/// Builds and emits one record per actual transport attempt (see module
/// docs for the record rule). Owned by a single request; not shared.
pub struct RequestTracer {
    ctx: TraceContext,
    session_id: TraceId,
    turn_id: TraceId,
    request_id: TraceId,
    model: QualifiedModelId,
    transport: TransportKind,
    endpoint: EndpointMeta,
    structure: RequestStructure,
    retries: Vec<RetryMeta>,
    attempt: u32,
}

impl RequestTracer {
    /// Begin tracing one request. Returns `None` when the context is
    /// disabled or the structural identity is unrepresentable (counted as a
    /// degraded record — never an error).
    pub fn begin(
        ctx: &TraceContext,
        model: QualifiedModelId,
        transport: TransportKind,
        endpoint: EndpointMeta,
        structure: RequestStructure,
    ) -> Option<Self> {
        if !ctx.enabled() {
            return None;
        }
        let seq = ctx.next_request_seq();
        let pid = std::process::id();
        let request_id = match TraceId::new(format!("req-{pid}-{seq}")) {
            Ok(id) => id,
            Err(_) => {
                ctx.note_degraded();
                return None;
            }
        };
        // The transport layer has no turn knowledge yet; the turn ID shares
        // the request sequence until runtime-level correlation lands.
        let turn_id = TraceId::new(format!("turn-{pid}-{seq}")).ok()?;
        Some(Self {
            ctx: ctx.clone(),
            session_id: ctx.session_id.clone(),
            turn_id,
            request_id,
            model,
            transport,
            endpoint,
            structure,
            retries: Vec::new(),
            attempt: 1,
        })
    }

    /// The shared request ID for correlating this request's records.
    pub fn request_id(&self) -> &TraceId {
        &self.request_id
    }

    /// Record a failed attempt that WILL be retried: emits this attempt's
    /// record (typed `ProviderFailed` terminal), then appends the retry to
    /// the shared history and advances the attempt ordinal.
    #[allow(clippy::too_many_arguments)]
    pub fn attempt_failed(
        &mut self,
        clock: AttemptClock,
        class: RetryClass,
        delay: Duration,
        http_status: Option<u16>,
        provider_request_id: Option<TraceId>,
        code: &str,
    ) {
        let outcome = TransportOutcome {
            timings: clock.stages(),
            retries: self.retries.clone(),
            provider_request_id,
            http_status,
            stop_reason: None,
            usage: None,
            terminal: TurnOutcome::ProviderFailed {
                code: code.to_string(),
                correlation_id: self.request_id.as_str().to_string(),
            },
        };
        self.emit_record(outcome);
        self.retries.push(RetryMeta {
            attempt: self.attempt,
            class,
            delay_ms: delay.as_millis() as u64,
        });
        self.attempt += 1;
    }

    /// Emit the final record for this request's last attempt.
    #[allow(clippy::too_many_arguments)]
    pub fn finish(
        self,
        clock: AttemptClock,
        http_status: Option<u16>,
        provider_request_id: Option<TraceId>,
        stop_reason: Option<StopReason>,
        usage: Option<UsageMeta>,
        terminal: TurnOutcome,
    ) {
        let outcome = TransportOutcome {
            timings: clock.stages(),
            retries: self.retries.clone(),
            provider_request_id,
            http_status,
            stop_reason,
            usage,
            terminal,
        };
        self.emit_record(outcome);
    }

    /// A typed terminal for an attempt that failed: the correlation ID is
    /// the trace request ID — never a raw error string.
    pub fn failed_terminal(&self, code: &str) -> TurnOutcome {
        TurnOutcome::ProviderFailed {
            code: code.to_string(),
            correlation_id: self.request_id.as_str().to_string(),
        }
    }

    fn emit_record(&self, outcome: TransportOutcome) {
        let record = RequestTrace {
            schema: TraceSchemaVersion,
            session_id: self.session_id.clone(),
            turn_id: self.turn_id.clone(),
            request_id: self.request_id.clone(),
            attempt: self.attempt,
            model: self.model.clone(),
            transport: self.transport,
            endpoint: self.endpoint.clone(),
            anatomy: self.structure.anatomy,
            wire: self.structure.wire.clone(),
            system_segments: self.structure.system_segments.clone(),
            messages: self.structure.messages.clone(),
            tools: self.structure.tools.clone(),
            cache: self.structure.cache.clone(),
            translation_losses: self.structure.translation.clone(),
            outcome,
        };
        self.ctx.sink.emit(record);
    }
}
