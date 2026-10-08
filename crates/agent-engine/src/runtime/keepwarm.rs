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
        if slot.map_or(true, |prev| prev.mono <= at.mono) {
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
    /// Sent; the provider's usage for it.
    Sent(KeepWarmUsage),
    /// Not sent: the freshly built prefix no longer matches the expected
    /// fingerprint (model, thinking, tools, system or history changed).
    PrefixChanged { expected: u64, actual: u64 },
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
        RuntimeError::Api(_) | RuntimeError::ApiStatus(_) | RuntimeError::Timeout => {
            PingFailure::Transient(if status == 0 {
                "network".to_string()
            } else {
                format!("HTTP {status}")
            })
        }
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

    /// Fingerprint of the prefix the next real turn would send for
    /// `history` (see the module docs). Builds the request through the real
    /// builder and returns before anything is sent; no network I/O.
    pub async fn cache_keepwarm_probe(&self, history: &[SharedMessage]) -> Result<u64> {
        let cancel = CancellationToken::new();
        let (value, _) = self
            .keepwarm_request(history, &cancel, KeepWarmMode::Probe, Arc::default())
            .await?;
        value["keepwarm_prefix"]
            .as_u64()
            .ok_or_else(|| RuntimeError::Session("keep-warm probe produced no fingerprint".into()))
    }

    /// Send ONE keep-warm ping for `history` (the session's durable history,
    /// unchanged) if its freshly built prefix still hashes to `expect`.
    /// Never mutates `history`, the runtime's session state, or any persisted
    /// state; fires no extension hooks; emits nothing to any frontend.
    pub async fn cache_keepwarm_ping(
        &self,
        history: &[SharedMessage],
        expect: u64,
        cancel: &CancellationToken,
    ) -> std::result::Result<PingOutcome, PingFailure> {
        let status = Arc::new(AtomicU16::new(0));
        match self
            .keepwarm_request(
                history,
                cancel,
                KeepWarmMode::Send { expect },
                Arc::clone(&status),
            )
            .await
        {
            Ok((value, usage)) => match value.get("keepwarm_prefix").and_then(Value::as_u64) {
                Some(actual) => Ok(PingOutcome::PrefixChanged {
                    expected: expect,
                    actual,
                }),
                None => Ok(PingOutcome::Sent(usage)),
            },
            Err(e) => Err(classify_failure(&e, status.load(Ordering::Relaxed))),
        }
    }

    async fn keepwarm_request(
        &self,
        history: &[SharedMessage],
        cancel: &CancellationToken,
        mode: KeepWarmMode,
        status: Arc<AtomicU16>,
    ) -> Result<(Value, KeepWarmUsage)> {
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
        // message. Same request-local image cap as a real round.
        let mut request: Vec<SharedMessage> = history.to_vec();
        request.push(Arc::new(
            json!({"role": "user", "content": KEEPWARM_PING_TEXT}),
        ));
        let capped;
        let request_messages: &[SharedMessage] = match super::stream::cap_history_image_bytes(
            &request,
            super::stream::HISTORY_IMAGE_BYTE_CAP,
        ) {
            Some(c) => {
                capped = c;
                &capped
            }
            None => &request,
        };

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
            let retained = self
                .retained_tool_set
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let set = match retained {
                Some(shared) if crate::tools::activation::carry_forward_enabled() => {
                    let existing = shared
                        .read()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if existing.session() == fresh.session() {
                        existing
                            .carry_activations_into(fresh, tools_snapshot.catalog())
                            .0
                    } else {
                        fresh
                    }
                }
                _ => fresh,
            };
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
            // A fresh latch: a downgrade notice raised by a ping goes nowhere,
            // so it must not consume the session's one real notice.
            ttl_downgrade_notified: Default::default(),
            saw_1h_honored: self.saw_1h_honored.clone(),
            credential_source: self.credential_source.clone(),
            token_cache: self.token_cache.clone(),
            anthropic_base_url: None,
            anthropic_execution_plan,
            codex_request_role: self.codex_request_role(),
            // The base context: a ping must never consume `/trace next`.
            trace: self.trace_ctx.clone(),
            request_correlation: None,
            suppress_stream_deltas: true,
            telemetry: self.telemetry_writer.clone(),
            tool_session_id: Some(self.host_tool_session.clone()),
            session_tool_set: None,
            request_tools_schema,
            anthropic_tool_split,
            usage_counters: None,
            // A successful ping refreshes the cache: it moves the anchor the
            // next ping is scheduled from.
            cache_anchor: Some(Arc::clone(&self.cache_anchor)),
            keepwarm: Some(KeepWarmRequest { mode, status }),
        };

        let (tx, mut rx) = mpsc::unbounded_channel();
        let result = ApiMethods::call_api_stream_inner(
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
        )
        .await;
        let mut usage = KeepWarmUsage::default();
        while let Ok(event) = rx.try_recv() {
            usage.add(&event);
        }
        Ok((result?, usage))
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

    #[derive(Clone)]
    struct Mock {
        bodies: Arc<Mutex<Vec<String>>>,
        /// HTTP status served (200 = the SSE body above).
        status: u16,
    }

    async fn spawn_mock() -> (String, Mock) {
        spawn_mock_with(200).await
    }

    async fn spawn_mock_with(status: u16) -> (String, Mock) {
        let mock = Mock {
            bodies: Arc::default(),
            status,
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
                            SSE_HI.to_string(),
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

        let fingerprint = rt.cache_keepwarm_probe(&history).await.expect("probe");
        assert_eq!(
            mock.bodies.lock().unwrap().len(),
            1,
            "a probe sends nothing"
        );
        assert_eq!(
            rt.cache_keepwarm_probe(&history).await.unwrap(),
            fingerprint,
            "the fingerprint is deterministic"
        );

        let usage = match rt
            .cache_keepwarm_ping(&history, fingerprint, &CancellationToken::new())
            .await
            .expect("ping succeeds")
        {
            PingOutcome::Sent(usage) => usage,
            other => panic!("expected a sent ping, got {other:?}"),
        };
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
        let fp = rt.cache_keepwarm_probe(&history).await.unwrap();
        let sent = mock.bodies.lock().unwrap().len();

        // History grew (e.g. a compaction / /clear / a new turn).
        let mut longer = history.clone();
        longer.push(user("more"));
        longer.push(Arc::new(
            json!({"role": "assistant", "content": [{"type": "text", "text": "x"}]}),
        ));
        let changed = |o: std::result::Result<PingOutcome, PingFailure>| {
            matches!(o, Ok(PingOutcome::PrefixChanged { .. }))
        };
        assert!(changed(
            rt.cache_keepwarm_ping(&longer, fp, &CancellationToken::new())
                .await
        ));

        rt.set_system_prompt("A different system prompt.".into());
        assert_ne!(rt.cache_keepwarm_probe(&history).await.unwrap(), fp);
        assert!(changed(
            rt.cache_keepwarm_ping(&history, fp, &CancellationToken::new())
                .await
        ));
        rt.set_system_prompt("You are a keep-warm test.".into());
        assert_eq!(
            rt.cache_keepwarm_probe(&history).await.unwrap(),
            fp,
            "restored"
        );

        rt.set_reasoning_level_explicit(ReasoningLevel::Low);
        assert_ne!(
            rt.cache_keepwarm_probe(&history).await.unwrap(),
            fp,
            "effort is in the key"
        );
        rt.set_reasoning_level_explicit(ReasoningLevel::High);

        rt.set_model("claude-opus-4-6".to_string());
        assert_eq!(rt.model(), "claude-opus-4-6");
        assert_ne!(
            rt.cache_keepwarm_probe(&history).await.unwrap(),
            fp,
            "model is in the key"
        );

        assert_eq!(
            mock.bodies.lock().unwrap().len(),
            sent,
            "no refused ping reached the provider"
        );
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
            let fp = rt.cache_keepwarm_probe(&history).await.unwrap();
            let err = rt
                .cache_keepwarm_ping(&history, fp, &CancellationToken::new())
                .await
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
        let err = rt
            .cache_keepwarm_ping(&history, 0, &CancellationToken::new())
            .await
            .expect_err("non-Anthropic must refuse");
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
}
