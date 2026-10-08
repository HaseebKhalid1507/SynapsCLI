//! Prompt-cache keep-warm ping (task #444).
//!
//! A cache READ refreshes an Anthropic prompt-cache entry's TTL, so re-sending
//! the session's exact prefix shortly before the 1h TTL lapses keeps a
//! 300k–1M token prefix warm at 0.1× input price instead of paying a 2×
//! rewrite when the user comes back. The *scheduling* lives in the session
//! actor (`session::cache_keepwarm`); this module only builds and sends one
//! ping.
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
//!   context-pressure message: those only ever touch the request tail and
//!   would add per-turn bytes;
//! - no `MessageHistory`, no turn, no transcript: events go to a private
//!   channel and only the `Usage` numbers come back;
//! - `max_tokens` is capped at [`KEEPWARM_MAX_TOKENS`] (not part of the cache
//!   key), retries are zero, and the telemetry record says `"keepwarm": true`.
//!
//! Thinking: the thinking configuration and effort ARE part of the cache key,
//! so the ping must carry them unchanged. With adaptive thinking (or thinking
//! off) `max_tokens` is a hard cap on thinking + text, so the ping's output is
//! bounded by [`KEEPWARM_MAX_TOKENS`]. With legacy budgeted thinking Anthropic
//! requires `max_tokens > budget_tokens`, so a ping could bill up to the whole
//! budget (2k–32k output tokens at 5× input price); keep-warm refuses those
//! configurations instead of silently spending (see
//! [`Runtime::cache_keepwarm_ineligibility`]).

use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::api::{ApiMethods, ApiOptions, KeepWarmRequest};
use super::helpers::HelperMethods;
use super::{Runtime, SessionEvent, StreamEvent};
use crate::core::config::CacheTtl;
use crate::{Result, RuntimeError, SharedMessage};

/// Output cap for one ping. Covers thinking + text on adaptive models. At
/// Opus pricing ($25/MTok output) this is $0.0004 per ping.
pub const KEEPWARM_MAX_TOKENS: u64 = 16;

/// The trailing user message. Sits after the last cache marker, is never
/// persisted, and asks for the shortest possible answer so adaptive thinking
/// has no reason to engage before the output cap cuts it off.
pub const KEEPWARM_PING_TEXT: &str =
    "[automated prompt-cache keep-warm ping — not from the user] Reply with only: ok";

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
    fn add(&mut self, event: &StreamEvent) {
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

    /// USD cost of the ping, priced exactly like a turn's usage.
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
/// reply (a turn that errored mid-way leaves nothing worth pinging).
pub fn history_ready(history: &[SharedMessage]) -> bool {
    history.last().is_some_and(|m| tail_is_markable(m))
}

impl Runtime {
    /// Why this runtime cannot keep its cache warm cheaply, or `None` when it
    /// can. Checked by the session actor before arming and again by the ping.
    pub fn cache_keepwarm_ineligibility(&self) -> Option<String> {
        if !super::auth::model_is_anthropic(&self.model) {
            return Some(format!(
                "model {} is not on the native Anthropic transport",
                self.model
            ));
        }
        match self.cache_ttl {
            CacheTtl::OneHour => {}
            CacheTtl::FiveMinutes => {
                return Some(
                    "cache_ttl = 5m: a ping every <5 min would cost more than it saves; \
                     keep-warm needs cache_ttl = 1h"
                        .into(),
                )
            }
            CacheTtl::Hybrid => {
                return Some(
                    "cache_ttl = hybrid: the conversation history is cached with the 5m TTL \
                     and cannot be kept warm hourly; keep-warm needs cache_ttl = 1h"
                        .into(),
                )
            }
        }
        let bare = self.model.strip_prefix("anthropic/").unwrap_or(&self.model);
        if self.reasoning_level() != agent_core::reasoning::ReasoningLevel::Off
            && !crate::core::models::model_supports_adaptive_thinking(bare)
        {
            let budget = if self.thinking_budget == 0 {
                crate::core::models::DEFAULT_LEGACY_ADAPTIVE_FALLBACK
            } else {
                self.thinking_budget
            };
            return Some(format!(
                "{bare} uses budgeted thinking (budget_tokens = {budget}); the API requires \
                 max_tokens > budget_tokens and the budget is part of the cache key, so a ping \
                 could bill up to {budget} output tokens. Keep-warm only runs with thinking \
                 off or on adaptive-thinking models"
            ));
        }
        None
    }

    /// Send ONE keep-warm ping for `history` (the session's durable history,
    /// unchanged). Never mutates `history`, the runtime's session state, or
    /// any persisted state; fires no extension hooks; emits nothing to any
    /// frontend. Returns the provider-reported usage.
    pub async fn cache_keepwarm_ping(
        &self,
        history: &[SharedMessage],
        cancel: &CancellationToken,
    ) -> Result<KeepWarmUsage> {
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

        // Same preflight as a real turn (run_stream_tracked).
        self.validate_request_preflight().await?;
        let anthropic_execution_plan = self.authorized_anthropic_plan().await?;
        self.refresh_if_needed().await?;

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
            keepwarm: Some(KeepWarmRequest {
                max_tokens: KEEPWARM_MAX_TOKENS,
            }),
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
        result?;
        Ok(usage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::hooks::events::{HookEvent, HookKind, HookResult};
    use crate::extensions::permissions::{Permission, PermissionSet};
    use agent_core::reasoning::ReasoningLevel;
    use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::post, Router};
    use futures::StreamExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
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

    #[derive(Clone, Default)]
    struct Mock {
        bodies: Arc<Mutex<Vec<String>>>,
    }

    async fn spawn_mock() -> (String, Mock) {
        let mock = Mock::default();
        let app = Router::new()
            .route(
                "/v1/messages",
                post(|State(m): State<Mock>, body: String| async move {
                    m.bodies.lock().unwrap().push(body);
                    (
                        StatusCode::OK,
                        [("content-type", "text/event-stream")],
                        SSE_HI.to_string(),
                    )
                        .into_response()
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
        /// `before_message` calls before the ping, after it, after the next turn.
        hook_calls: (usize, usize, usize),
    }

    /// Previous turn → keep-warm ping → the real next turn, on one runtime.
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

        // The previous turn: the session is idle afterwards.
        let history = real_turn(rt, vec![user("hello")]).await;
        assert_eq!(history.last().unwrap()["role"], "assistant");
        let before_ping = calls.load(Ordering::SeqCst);
        let snapshot = history.clone();

        let usage = rt
            .cache_keepwarm_ping(&history, &CancellationToken::new())
            .await
            .expect("ping succeeds");
        assert!(
            usage.output_tokens >= 1,
            "usage comes back from the private channel"
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
            hook_calls: (before_ping, after_ping, after_next),
        }
    }

    fn assert_prefix_identical(ping: &Value, real: &Value, n: usize) {
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
        // The last history message: identical except the ping's marker.
        let last = &pm[n - 1];
        let marker = &last["content"].as_array().unwrap().last().unwrap()["cache_control"];
        assert_eq!(*marker, json!({"type": "ephemeral", "ttl": "1h"}));
        assert_eq!(
            canon(&strip_cache_control(last.clone())),
            canon(&rm[n - 1]),
            "last history message differs beyond the cache marker"
        );
        // The trailing ping message sits AFTER the last breakpoint.
        assert_eq!(marker_count(&pm[n]), 0, "ping message must be unmarked");
        assert_eq!(pm[n]["role"], "user");
        assert_eq!(pm[n]["content"], KEEPWARM_PING_TEXT);
        // Output is capped; the real turn keeps its own max_tokens.
        assert_eq!(ping["max_tokens"], json!(KEEPWARM_MAX_TOKENS));
        assert_ne!(real["max_tokens"], json!(KEEPWARM_MAX_TOKENS));
        assert!(
            marker_count(ping) <= 4,
            "Anthropic allows at most 4 breakpoints"
        );
    }

    /// Adaptive thinking (Opus 4.7): the ping carries the same thinking and
    /// effort config, its prefix through the last marker matches the real
    /// next turn, and no hook ran for it.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn ping_prefix_matches_next_turn_with_adaptive_thinking() {
        let _base = crate::test_env::BaseDirGuard::new();
        let mut rt = Runtime::new_headless();
        rt.set_model("claude-opus-4-7".to_string());
        assert_eq!(rt.model(), "claude-opus-4-7");
        rt.set_reasoning_level_explicit(ReasoningLevel::High);
        rt.set_cache_ttl(CacheTtl::OneHour);
        rt.set_system_prompt("You are a keep-warm test.".into());
        assert_eq!(rt.cache_keepwarm_ineligibility(), None);

        let o = ping_then_next_turn(&rt).await;
        assert!(
            !o.ping["thinking"].is_null(),
            "thinking must stay on: {}",
            o.ping
        );
        assert_prefix_identical(&o.ping, &o.real, o.history_len);

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

    /// Thinking off on a budgeted-thinking model is eligible and matches too.
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
        assert_prefix_identical(&o.ping, &o.real, o.history_len);
    }

    #[test]
    fn five_minute_and_hybrid_ttl_are_ineligible() {
        let mut rt = Runtime::new_headless();
        rt.set_model("claude-opus-4-7".to_string());
        assert_eq!(rt.cache_ttl(), CacheTtl::FiveMinutes, "default TTL");
        let reason = rt.cache_keepwarm_ineligibility().expect("5m is ineligible");
        assert!(reason.contains("cache_ttl = 5m"), "{reason}");
        rt.set_cache_ttl(CacheTtl::Hybrid);
        let reason = rt
            .cache_keepwarm_ineligibility()
            .expect("hybrid is ineligible");
        assert!(reason.contains("hybrid"), "{reason}");
        rt.set_cache_ttl(CacheTtl::OneHour);
        assert_eq!(rt.cache_keepwarm_ineligibility(), None);
    }

    #[test]
    fn budgeted_thinking_is_ineligible_with_numbers() {
        let mut rt = Runtime::new_headless();
        rt.set_model("claude-sonnet-4-6".to_string());
        rt.set_cache_ttl(CacheTtl::OneHour);
        rt.set_reasoning_level_explicit(ReasoningLevel::High);
        let reason = rt
            .cache_keepwarm_ineligibility()
            .expect("budgeted thinking cannot be capped");
        assert!(reason.contains("budget_tokens = 16384"), "{reason}");
    }

    #[test]
    fn non_anthropic_models_are_ineligible() {
        let mut rt = Runtime::new_headless();
        rt.set_cache_ttl(CacheTtl::OneHour);
        rt.set_model("groq/llama-3.3-70b-versatile".to_string());
        if rt.model() == "groq/llama-3.3-70b-versatile" {
            assert!(rt.cache_keepwarm_ineligibility().is_some());
        }
    }

    /// An ineligible ping fails closed before any network I/O.
    #[tokio::test]
    async fn ineligible_ping_sends_nothing() {
        let rt = Runtime::new_headless(); // 5m TTL
        let history = vec![
            user("hello"),
            Arc::new(json!({"role": "assistant", "content": [{"type": "text", "text": "hi"}]})),
        ];
        let err = rt
            .cache_keepwarm_ping(&history, &CancellationToken::new())
            .await
            .expect_err("5m TTL must refuse");
        assert!(err.to_string().contains("keep-warm skipped"), "{err}");
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
