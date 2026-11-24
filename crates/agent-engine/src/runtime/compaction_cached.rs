//! Cache-preserving compaction (task 430).
//!
//! The transcript request (`Runtime::compact_call`) is a separate request:
//! its own system prompt, no tools, no `cache_control`, and the whole
//! transcript flattened into one user message. Nothing of it matches the
//! chat's cached prefix, so the provider bills the whole history as fresh
//! input and counts it against the per-minute input limit. A 400K-token
//! Opus session got HTTP 429 on every attempt that way while its own
//! (larger, cached) turns kept passing.
//!
//! When the conditions below hold, the summary request is instead the chat's
//! own next request: the same system blocks, tools, thinking, effort and
//! messages, plus ONE new user message carrying the summary instructions
//! ([`instructions`]). The message cache marker sits on the last history
//! message, where a keep-warm ping puts it, not on the instructions: they
//! are sent once, so an entry ending in them would be written (the whole
//! request, at the write price) and never read. The history is then read
//! from the cache: billed at the read price and not counted against the
//! per-minute input limit
//! (<https://platform.claude.com/docs/en/api/rate-limits#cache-aware-itpm>).
//!
//! What a request may change without losing the message cache is fixed by
//! <https://platform.claude.com/docs/en/build-with-claude/prompt-caching#what-invalidates-the-cache>:
//! tool definitions, system, `tool_choice`, images, thinking parameters and
//! effort each invalidate the message blocks. So NONE of them changes here:
//! the request is built by the turn's own builder
//! (`Runtime::compaction_cached_request`), tools stay offered and
//! `tool_choice` stays unset as the chat sends it. The model can therefore
//! answer with a tool call. No tool is ever run from a compaction reply
//! (this path has no tool loop); such a reply is discarded and the
//! transcript request runs instead ([`summary_from_reply`]).
//!
//! Conditions ([`cached_path`]); when one fails the transcript request runs,
//! exactly as before:
//!
//! 1. the mode is remote and the disclosure policy excludes no content
//!    class. The chat's own request carries every class, so an exclusion
//!    policy can only be honoured by the filtered transcript;
//! 2. the summary model is the chat's own model (another model has another
//!    cache);
//! 3. the chat is on the native Anthropic transport;
//! 4. the history, the instructions and the summary's output fit the
//!    model's context window;
//! 5. the cache is known to be warm ([`CacheState`]), written by a request
//!    with this one's cache identity, and for this history: the history
//!    starts with exactly the messages the last accepted request sent
//!    ([`CachedPrefix`]). The anchor alone does not say so: `/resume` in
//!    place swaps the history under a runtime whose anchor is minutes old;
//! 6. the instructions, a user message that is not a tool result, do not
//!    make the provider strip earlier thinking: on models before Opus 4.5,
//!    Sonnet 4.6 and Haiku 5.5 that removes every message after the first
//!    thinking block from the cache (same page, "Non-tool results passed to
//!    requests with thinking"), so a history holding thinking blocks would
//!    be written again, at the write price.
//!
//! Any failure of the request (an error, a tool call, no text) falls back to
//! the transcript request, once. Nothing here touches the history.

use std::time::Duration;

use serde_json::Value;

use super::compaction::{CompactionOutcome, DisclosurePolicy, RenderedCompactionInput};
use super::keepwarm::{CachedPrefix, KeepWarmUsage};
use super::Runtime;
use crate::session::cache_keepwarm::{governing_ttl, honoured_ttl, TtlEvidence, LATE_GUARD};
use crate::SharedMessage;

/// Opens the instruction message: the summary request arrives as a user
/// turn inside the chat, under the chat's own system prompt, so it must say
/// that it is not the user speaking and that the work stops here. Carries
/// what [`super::compaction::COMPACTION_SYSTEM_PROMPT`] tells the transcript
/// request's model.
pub const CACHED_COMPACTION_PREAMBLE: &str = "[Automated context-compaction request from the host application, not a message from the user]\n\nThis conversation is about to be replaced by a summary of it. Stop working on the task. Do NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary described below.";

/// Closes the instruction message. Tools stay offered (removing them, or
/// forcing `tool_choice`, would invalidate the cached messages), so the
/// model is told not to use them; a reply that does is discarded.
pub const CACHED_COMPACTION_REPLY_RULE: &str =
    "Reply with the summary only, as plain text. Call no tools.";

/// The one user message a cache-preserving compaction request adds: the
/// preamble, the transcript request's own instruction text (template,
/// `/compact` focus and file-operation records; see
/// [`RenderedCompactionInput::instructions`]) and the reply rule.
pub fn instructions(rendered: &RenderedCompactionInput) -> String {
    format!(
        "{CACHED_COMPACTION_PREAMBLE}\n\n{}\n\n{CACHED_COMPACTION_REPLY_RULE}",
        rendered.instructions
    )
}

/// What the runtime knows about the chat's cached prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheState {
    /// The last accepted request left this prefix, under a TTL that has
    /// not run out.
    Warm(CachedPrefix),
    /// The TTL has run out (or is within [`LATE_GUARD`] of it).
    Cold,
    /// No accepted request in this process yet (a resumed session), or one
    /// whose cached prefix is not known (it switched accounts mid-request).
    Unknown,
}

/// [`CacheState`] of an anchor `age` old under `ttl`. The lifetime counts
/// from the START of the request that read or wrote the entry, which is
/// when the anchor is taken; a request still has to reach the provider, so
/// the last [`LATE_GUARD`] of the lifetime counts as gone.
pub fn cache_state(
    prefix: Option<CachedPrefix>,
    age: Option<Duration>,
    ttl: Duration,
) -> CacheState {
    match (prefix, age) {
        (Some(prefix), Some(age)) if age + LATE_GUARD < ttl => CacheState::Warm(prefix),
        (Some(_), Some(_)) => CacheState::Cold,
        _ => CacheState::Unknown,
    }
}

/// Why a compaction goes out as the transcript request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skip {
    LocalOnly,
    PolicyExclusions,
    OtherModel,
    NotAnthropic,
    DoesNotFit { needed: u64, window: u64 },
    CacheCold,
    CacheUnknown,
    HistoryChanged,
    ThinkingStripped,
}

impl std::fmt::Display for Skip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Skip::LocalOnly => f.write_str("compaction is local-only"),
            Skip::PolicyExclusions => {
                f.write_str("the disclosure policy excludes content classes")
            }
            Skip::OtherModel => f.write_str("another model writes the summary"),
            Skip::NotAnthropic => {
                f.write_str("the chat is not on the native Anthropic transport")
            }
            Skip::DoesNotFit { needed, window } => write!(
                f,
                "history, instructions and summary need about {needed} tokens of a {window}-token window"
            ),
            Skip::CacheCold => f.write_str("the prompt cache is cold"),
            Skip::HistoryChanged => f.write_str(
                "the history does not start with the messages the last request sent",
            ),
            Skip::CacheUnknown => {
                f.write_str("nothing is known about the prompt cache (no request yet)")
            }
            Skip::ThinkingStripped => f.write_str(
                "this model drops earlier thinking blocks, and the cached messages with them, \
                 when a user message is added",
            ),
        }
    }
}

impl Runtime {
    /// What this runtime knows about its chat's cached prefix, now: the
    /// anchor keep-warm schedules from (the last accepted request of the
    /// chain, pings included), under the TTL the server is known to honour
    /// for the history entry (`honoured_ttl`: 1h only with evidence, 5m for
    /// `hybrid`). The age is the larger of the monotonic and the wall-clock
    /// one: the provider's TTL keeps running while this machine sleeps.
    pub async fn compaction_cache_state(&self) -> CacheState {
        let ttl = governing_ttl(honoured_ttl(
            self.resolved_cache_ttl().await,
            TtlEvidence {
                downgraded: self.cache_ttl_downgraded(),
                saw_1h: self.cache_1h_honoured(),
                ..TtlEvidence::default()
            },
        ));
        let age = self.cache_anchor().map(|anchor| {
            let wall = anchor.wall.elapsed().unwrap_or(Duration::ZERO);
            anchor.mono.elapsed().max(wall)
        });
        let state = cache_state(self.cached_prefix(), age, ttl);
        if let (CacheState::Cold, Some(age)) = (state, age) {
            tracing::info!(
                age_secs = age.as_secs(),
                ttl_secs = ttl.as_secs(),
                "compaction: the prompt cache is cold"
            );
        }
        state
    }
}

/// What the request must be able to read (the cached prefix of the last
/// accepted request), when every condition of the module docs holds; else
/// the first one that does not. Decided locally: nothing is sent.
pub async fn cached_path(
    runtime: &Runtime,
    history: &[SharedMessage],
    model: &str,
    policy: &DisclosurePolicy,
    instructions: &str,
) -> Result<CachedPrefix, Skip> {
    use agent_core::compaction::CompactionMode;
    if policy.mode != CompactionMode::Remote {
        return Err(Skip::LocalOnly);
    }
    if !policy.exclude.is_empty() {
        return Err(Skip::PolicyExclusions);
    }
    if model != runtime.model() {
        return Err(Skip::OtherModel);
    }
    if !super::auth::model_is_anthropic(model) {
        return Err(Skip::NotAnthropic);
    }
    if !crate::core::models::model_keeps_earlier_thinking(model) && holds_thinking(history) {
        return Err(Skip::ThinkingStripped);
    }
    // The request as it will be sent: the history plus the instructions.
    // Its footprint is the provider's own count of the last request plus an
    // estimate of what was appended, when the runtime has one.
    let mut request = history.to_vec();
    request.push(std::sync::Arc::new(
        serde_json::json!({"role": "user", "content": instructions}),
    ));
    let assessment = runtime.assess_context(&request).await;
    // The summary's output: what one round is expected to produce, thinking
    // included (`ContextReserves`), not the `max_tokens` ceiling (a model
    // that rejects input + `max_tokens` over the window has it clamped by
    // the request builder).
    let needed = assessment
        .used_tokens()
        .saturating_add(assessment.reserves.thinking_tokens)
        .saturating_add(assessment.reserves.output_tokens);
    if needed > assessment.provider_window {
        return Err(Skip::DoesNotFit {
            needed,
            window: assessment.provider_window,
        });
    }
    let prefix = match runtime.compaction_cache_state().await {
        CacheState::Warm(prefix) => prefix,
        CacheState::Cold => return Err(Skip::CacheCold),
        CacheState::Unknown => return Err(Skip::CacheUnknown),
    };
    // Warm, but for which messages? The anchor outlives a history swap
    // (`/resume` in place keeps the runtime): only a history that starts
    // with what the last request sent reads its cache.
    if !prefix.leads(history) {
        return Err(Skip::HistoryChanged);
    }
    Ok(prefix)
}

/// Whether any assistant message of `history` holds a thinking block.
fn holds_thinking(history: &[SharedMessage]) -> bool {
    history.iter().any(|message| {
        message["content"].as_array().is_some_and(|blocks| {
            blocks
                .iter()
                .any(|b| matches!(b["type"].as_str(), Some("thinking" | "redacted_thinking")))
        })
    })
}

/// The summary text of a cache-preserving compaction reply (`content`
/// blocks), or why it has none. A reply holding a tool call of any kind is
/// refused whole, text included: the model went on working instead of
/// summarizing. Thinking blocks are skipped.
pub fn summary_from_reply(reply: &Value) -> Result<String, &'static str> {
    let blocks = reply["content"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    if blocks
        .iter()
        .any(|b| b["type"].as_str().is_some_and(|t| t.ends_with("tool_use")))
    {
        return Err("the reply called a tool");
    }
    let text: String = blocks
        .iter()
        .filter(|b| b["type"] == "text")
        .filter_map(|b| b["text"].as_str())
        .collect();
    if text.trim().is_empty() {
        return Err("the reply has no text");
    }
    Ok(text)
}

/// What to say when a cache-preserving request did not read the cache:
/// the provider wrote more of it than it read (`cache_creation` over
/// `cache_read` in its usage), so the history was billed at the write
/// price. `None` when it mostly read, which is the expected case (the
/// reply after the last request is always written).
pub fn cache_miss_warning(session: Option<&str>, usage: &KeepWarmUsage) -> Option<String> {
    let (read, written) = (usage.cache_read_tokens, usage.cache_write_tokens);
    (written > read).then(|| {
        format!(
            "cache-preserving compaction missed the prompt cache (session {}): {written} tokens \
             written to the cache, {read} read from it. Likely cause: the cached prefix had \
             expired before the request arrived (a TTL shorter than assumed), or a message \
             ahead of the last one went out changed (images retired, thinking blocks dropped)",
            session.unwrap_or("unknown")
        )
    })
}

/// Summarize through the chat's own next request when [`cached_path`]
/// allows it. `None` (the conditions do not hold, or the request failed in
/// any way) sends the caller to the transcript request; why is logged. The
/// outcome's provenance is that of the request that was sent: the chat's
/// model, and a prompt-stack digest over the system prompt it carried and
/// the instruction message.
pub(super) async fn summarize(
    history: &[SharedMessage],
    runtime: &Runtime,
    model: &str,
    policy: &DisclosurePolicy,
    rendered: &RenderedCompactionInput,
) -> Option<CompactionOutcome> {
    let instructions = instructions(rendered);
    let expect = match cached_path(runtime, history, model, policy, &instructions).await {
        Ok(expect) => expect,
        Err(skip) => {
            tracing::info!(reason = %skip, "compaction: sending the transcript request");
            return None;
        }
    };
    let reply = runtime
        .compaction_cached_request(history, &instructions, expect)
        .await;
    let (summary_text, system_prompt) = match reply {
        Ok(sent) => match summary_from_reply(&sent.reply) {
            Ok(text) => (text, sent.system_prompt),
            Err(why) => {
                tracing::warn!(
                    reason = why,
                    "cache-preserving compaction failed; sending the transcript request"
                );
                return None;
            }
        },
        Err(e) => {
            tracing::warn!(
                error = %e.user_message(),
                "cache-preserving compaction failed; sending the transcript request"
            );
            return None;
        }
    };
    let mut outcome = CompactionOutcome::new(
        summary_text,
        model,
        &[system_prompt.as_deref().unwrap_or_default(), &instructions],
    );
    // The chat's own request: every class the history holds, none excluded
    // (condition 1), nothing truncated.
    outcome.included_classes = rendered.included_classes.clone();
    outcome.excluded_classes = Vec::new();
    outcome.redaction_policy = agent_core::compaction::RedactionPolicy::TruncationOnly;
    Some(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::CacheTtl;
    use crate::runtime::compaction::{
        compact_conversation, render_compaction_input, COMPACTION_SYSTEM_PROMPT,
    };
    use crate::runtime::keepwarm::CacheAnchor;
    use crate::{SessionEvent, StreamEvent};
    use agent_core::compaction::{prompt_stack_digest, CompactionMode, ContentClass};
    use agent_core::reasoning::ReasoningLevel;
    use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::post, Router};
    use futures::StreamExt;
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use tokio_util::sync::CancellationToken;

    const CACHED_SUMMARY: &str = "## Goal\nSUMMARY-FROM-THE-CHAT-REQUEST";
    const TRANSCRIPT_SUMMARY: &str = "## Goal\nSUMMARY-FROM-THE-TRANSCRIPT-REQUEST";
    const RESULT_SENTINEL: &str = "TOOL-RESULT-SENTINEL-9f3a";
    /// The cache usage every streamed mock reply reports.
    const MOCK_CACHE_READ: u64 = 1_200;
    const MOCK_CACHE_WRITE: u64 = 30;
    /// A cached prefix with no messages (it leads any history) and an
    /// arbitrary identity, for the checks that never build a request.
    const SEVEN: CachedPrefix = CachedPrefix {
        identity: 7,
        messages: 0,
        // FNV-1a 64 offset basis: `message_prefix_digest(&[])`.
        digest: 0xcbf2_9ce4_8422_2325,
    };

    /// One request the mock received.
    #[derive(Clone)]
    struct Seen {
        body: Value,
        beta: Option<String>,
    }

    impl Seen {
        /// The chat's transport streams; the transcript request does not.
        fn streams(&self) -> bool {
            self.body["stream"] == true
        }
    }

    /// What the mock answers a STREAMING request with: an SSE body, or an
    /// HTTP error status. Non-streaming (transcript) requests always get
    /// [`TRANSCRIPT_SUMMARY`].
    #[derive(Clone)]
    enum Stream {
        Blocks(Vec<Value>),
        Status(u16),
        /// HTTP 200, then an in-stream `overloaded_error`.
        Overloaded,
    }

    const SSE_OVERLOADED: &str = concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_ov\",\"type\":\"message\",",
        "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-opus-4-7\",\"stop_reason\":null,",
        "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\n",
        "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n",
    );

    #[derive(Clone)]
    struct Mock {
        seen: Arc<Mutex<Vec<Seen>>>,
        /// Reply to the n-th streaming request; the last entry repeats.
        streams: Arc<Vec<Stream>>,
    }

    impl Mock {
        fn seen(&self) -> Vec<Seen> {
            self.seen.lock().unwrap().clone()
        }
    }

    fn text(t: &str) -> Value {
        json!({"type": "text", "text": t})
    }

    /// An Anthropic SSE reply holding `blocks` (text and tool_use).
    fn sse(blocks: &[Value]) -> String {
        let mut out = String::from(concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_cc\",\"type\":\"message\",",
            "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-opus-4-7\",\"stop_reason\":null,",
            "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,",
            "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
        ));
        let mut event = |v: Value| out.push_str(&format!("data: {v}\n\n"));
        for (i, block) in blocks.iter().enumerate() {
            if block["type"] == "tool_use" {
                event(json!({"type": "content_block_start", "index": i, "content_block":
                    {"type": "tool_use", "id": block["id"], "name": block["name"]}}));
                event(json!({"type": "content_block_delta", "index": i, "delta":
                    {"type": "input_json_delta", "partial_json": block["input"].to_string()}}));
            } else {
                event(json!({"type": "content_block_start", "index": i,
                    "content_block": {"type": "text", "text": ""}}));
                event(json!({"type": "content_block_delta", "index": i,
                    "delta": {"type": "text_delta", "text": block["text"]}}));
            }
            event(json!({"type": "content_block_stop", "index": i}));
        }
        let stop = if blocks.iter().any(|b| b["type"] == "tool_use") {
            "tool_use"
        } else {
            "end_turn"
        };
        event(json!({"type": "message_delta", "delta": {"stop_reason": stop, "stop_sequence": null},
            "usage": {"input_tokens": 10, "output_tokens": 1,
                "cache_creation_input_tokens": MOCK_CACHE_WRITE,
                "cache_read_input_tokens": MOCK_CACHE_READ}}));
        event(json!({"type": "message_stop"}));
        out
    }

    async fn spawn_mock(streams: Vec<Stream>) -> (String, Mock) {
        let mock = Mock {
            seen: Arc::default(),
            streams: Arc::new(streams),
        };
        let app = Router::new()
            .route(
                "/v1/messages",
                post(
                    |State(m): State<Mock>, headers: axum::http::HeaderMap, body: String| async move {
                        let body: Value = serde_json::from_str(&body).unwrap();
                        let stream = body["stream"] == true;
                        let nth = {
                            let mut seen = m.seen.lock().unwrap();
                            let nth = seen.iter().filter(|s| s.streams()).count();
                            seen.push(Seen {
                                body,
                                beta: headers
                                    .get("anthropic-beta")
                                    .map(|v| v.to_str().unwrap().to_string()),
                            });
                            nth
                        };
                        if !stream {
                            let reply = json!({
                                "content": [text(TRANSCRIPT_SUMMARY)],
                                "stop_reason": "end_turn",
                                "usage": {"input_tokens": 10, "output_tokens": 5},
                            });
                            return (
                                StatusCode::OK,
                                [("content-type", "application/json")],
                                reply.to_string(),
                            )
                                .into_response();
                        }
                        match &m.streams[nth.min(m.streams.len() - 1)] {
                            Stream::Blocks(blocks) => (
                                StatusCode::OK,
                                [("content-type", "text/event-stream")],
                                sse(blocks),
                            )
                                .into_response(),
                            Stream::Overloaded => (
                                StatusCode::OK,
                                [("content-type", "text/event-stream")],
                                SSE_OVERLOADED.to_string(),
                            )
                                .into_response(),
                            Stream::Status(status) => (
                                StatusCode::from_u16(*status).unwrap(),
                                [("content-type", "application/json")],
                                r#"{"type":"error","error":{"type":"invalid_request_error","message":"synthetic"}}"#
                                    .to_string(),
                            )
                                .into_response(),
                        }
                    },
                ),
            )
            .layer(axum::extract::DefaultBodyLimit::max(64 * 1024 * 1024))
            .with_state(mock.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), mock)
    }

    fn user(t: &str) -> SharedMessage {
        Arc::new(json!({"role": "user", "content": t}))
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

    fn chat(model: &str, level: ReasoningLevel, ttl: CacheTtl) -> Runtime {
        let mut rt = Runtime::new_headless();
        rt.set_model(model.to_string());
        rt.set_reasoning_level_explicit(level);
        rt.set_cache_ttl(ttl);
        rt.set_system_prompt("You are a compaction test.".into());
        rt.set_api_retries(0);
        rt
    }

    fn opus() -> Runtime {
        chat("claude-opus-4-7", ReasoningLevel::High, CacheTtl::OneHour)
    }

    /// `rt` on a subscription (OAuth) login instead of an API key.
    fn on_a_subscription(mut rt: Runtime) -> Runtime {
        rt.auth = Arc::new(tokio::sync::RwLock::new(super::super::types::AuthState {
            auth_token: "oauth-test-token".to_string(),
            auth_type: "oauth".to_string(),
            refresh_token: None,
            token_expires: Some(crate::epoch_millis() + 3_600_000),
            auth_file_mtime: None,
            bound_credential: Some(crate::runtime::auth::anthropic_binding(
                &crate::auth::CredentialSource::Local,
                &crate::auth::CredentialRef::default_for(crate::auth::OAuthProviderId::Anthropic),
            )),
        }));
        rt
    }

    /// A chat that has just finished a turn: its cache is warm. The history
    /// then gains a tool round (never sent; the cache identity does not
    /// cover messages) so every content class has something to exclude.
    async fn warm_chat(rt: &Runtime) -> Vec<SharedMessage> {
        let mut history = real_turn(rt, vec![user("hello")]).await;
        assert!(
            matches!(rt.compaction_cache_state().await, CacheState::Warm(_)),
            "an accepted turn request leaves the cache known warm"
        );
        history.extend([
            user("read the file"),
            Arc::new(json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_1", "name": "read", "input": {"path": "/tmp/x"}},
            ]})),
            Arc::new(json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": RESULT_SENTINEL},
            ]})),
            Arc::new(json!({"role": "assistant", "content": [text("it says so")]})),
        ]);
        history
    }

    fn canon(v: &Value) -> String {
        serde_json::to_string(v).unwrap()
    }

    /// The text of a request's last message (string content, or one block).
    fn last_message_text(body: &Value) -> String {
        let last = body["messages"].as_array().unwrap().last().unwrap();
        match last["content"].as_str() {
            Some(text) => text.to_string(),
            None => last["content"][0]["text"].as_str().unwrap().to_string(),
        }
    }

    /// The transcript request, as it has always been sent.
    fn assert_transcript_request(seen: &Seen) {
        let body = &seen.body;
        assert!(!seen.streams(), "the transcript request does not stream");
        assert!(body.get("tools").is_none(), "it offers no tools");
        assert!(!canon(body).contains("cache_control"), "it marks nothing");
        assert!(canon(&body["system"]).contains(&COMPACTION_SYSTEM_PROMPT[..40]));
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1, "one flattened user message");
        assert!(messages[0]["content"]
            .as_str()
            .unwrap()
            .starts_with("<conversation>"));
    }

    /// THE KEY TEST. The compaction request and the chat's own next-turn
    /// request, built from the same history by one runtime, are the same
    /// request except for the appended last message.
    ///
    /// Everything the prompt cache keys on is asserted equal, byte for byte
    /// as sent: `model`, `tools`, `system` (transport identity included on
    /// OAuth), `thinking`, `output_config` (effort), every history message's
    /// content, and the `anthropic-beta` header. `max_tokens` and `stream`
    /// are not part of the cache key and are equal anyway: no top-level key
    /// differs.
    ///
    /// What must differ, and why:
    /// - the last message's text: the summary instructions instead of the
    ///   user's words. It is the request's uncached tail; that is the point.
    /// - where the ONE message cache marker sits. A turn marks its new user
    ///   message, so the next turn can read up to it. The instructions are
    ///   never sent again, so the compaction request marks the last history
    ///   message instead, like a keep-warm ping: nothing is written that no
    ///   request will read. `cache_control` is not part of the cached
    ///   content (a turn's marked tail recurs unmarked in the next request
    ///   and is read), so with the markers taken off the history messages
    ///   are byte-identical. (A marked message whose content is stored as
    ///   a string goes out as a one-block array, the documented shorthand
    ///   for the same text block; `content` below reads both alike.)
    /// - with `cache_diagnostics` on (off here and by default), the turn's
    ///   body also has a top-level `diagnostics` field threading the
    ///   previous message id; a compaction request is not a link of that
    ///   chain and omits it, like a keep-warm ping (`diagnostics_for`). The
    ///   field is not rendered into the prompt.
    async fn assert_same_request_as_the_next_turn(rt: &Runtime) -> (Seen, CompactionOutcome) {
        let (url, mock) = spawn_mock(vec![
            Stream::Blocks(vec![text("hi")]),
            Stream::Blocks(vec![text(CACHED_SUMMARY)]),
            Stream::Blocks(vec![text("hi again")]),
        ])
        .await;
        let _url = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
        let history = warm_chat(rt).await;
        let snapshot = history.clone();

        let outcome = compact_conversation(&history, rt, Some("FOCUS-ON-THE-PARSER"))
            .await
            .expect("compaction");
        assert_eq!(history, snapshot, "compaction never touches the history");

        let mut next = history.clone();
        next.push(user("next question"));
        real_turn(rt, next).await;

        let seen = mock.seen();
        assert_eq!(seen.len(), 3, "previous turn, compaction, next turn");
        let (compaction, turn) = (&seen[1], &seen[2]);
        assert_eq!(outcome.summary_text, CACHED_SUMMARY);

        let keys = |v: &Value| v.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
        assert_eq!(keys(&compaction.body), keys(&turn.body), "same top-level keys");
        for key in keys(&turn.body) {
            if key != "messages" {
                assert_eq!(
                    canon(&compaction.body[&key]),
                    canon(&turn.body[&key]),
                    "`{key}` differs between the compaction request and the next turn"
                );
            }
        }
        assert_eq!(compaction.beta, turn.beta, "anthropic-beta differs");

        let cm = compaction.body["messages"].as_array().unwrap();
        let tm = turn.body["messages"].as_array().unwrap();
        assert_eq!(cm.len(), tm.len());
        assert_eq!(cm.len(), history.len() + 1, "history + one appended message");
        // The cached content of a message: string content is shorthand for
        // one text block, and `cache_control` only marks where an entry
        // ends.
        let content = |message: &Value| {
            let mut message = message.clone();
            if let Some(text) = message["content"].as_str().map(str::to_owned) {
                message["content"] = json!([{"type": "text", "text": text}]);
            }
            for block in message["content"].as_array_mut().unwrap() {
                block.as_object_mut().unwrap().remove("cache_control");
            }
            canon(&message)
        };
        let marked = |messages: &[Value]| -> Vec<usize> {
            messages
                .iter()
                .enumerate()
                .filter(|(_, m)| canon(m).contains("cache_control"))
                .map(|(i, _)| i)
                .collect()
        };
        let (c_last, c_history) = cm.split_last().unwrap();
        let (t_last, t_history) = tm.split_last().unwrap();
        for (i, (c, t)) in c_history.iter().zip(t_history).enumerate() {
            assert_eq!(content(c), content(t), "history message {i} differs");
            if i + 1 < c_history.len() {
                assert_eq!(canon(c), canon(t), "history message {i} differs on the wire");
            }
        }
        // One message marker each: the turn's on its new message, the
        // compaction request's on the last history message.
        assert_eq!(marked(tm), vec![history.len()], "the turn marks its new message");
        assert_eq!(
            marked(cm),
            vec![history.len() - 1],
            "the compaction request marks the last history message, not the instructions"
        );
        assert_eq!(
            c_history.last().unwrap()["content"].as_array().unwrap().last().unwrap()
                ["cache_control"],
            t_last["content"][0]["cache_control"],
            "the same marker (TTL included) as the turn's"
        );
        // The appended message: the instructions, where the turn has the
        // user's words.
        let rendered = render_compaction_input(
            &history,
            Some("FOCUS-ON-THE-PARSER"),
            &rt.compaction_policy(),
        );
        let sent = instructions(&rendered);
        assert!(sent.starts_with(CACHED_COMPACTION_PREAMBLE));
        assert!(sent.contains(rendered.base_prompt));
        assert!(sent.contains("Additional focus: FOCUS-ON-THE-PARSER"));
        assert!(sent.contains("<read-files>\n/tmp/x\n</read-files>"));
        assert!(sent.ends_with(CACHED_COMPACTION_REPLY_RULE));
        assert_eq!(c_last["role"], "user");
        assert_eq!(content(c_last), content(&json!({"role": "user", "content": sent})));
        assert_eq!(
            content(t_last),
            content(&json!({"role": "user", "content": "next question"}))
        );
        (compaction.clone(), outcome)
    }

    /// Adaptive thinking with an effort, 1h TTL, API key.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn compaction_request_is_the_next_turn_request_plus_instructions() {
        let _base = crate::test_env::BaseDirGuard::new();
        let (compaction, _) = assert_same_request_as_the_next_turn(&opus()).await;
        assert!(compaction.body["tools"].as_array().is_some_and(|t| !t.is_empty()));
        assert!(compaction.body.get("tool_choice").is_none(), "as the chat sends it");
        assert_eq!(compaction.body["thinking"]["type"], "adaptive");
        assert_eq!(compaction.body["output_config"]["effort"], "high");
    }

    /// Budgeted thinking, 5m TTL; and thinking off.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn compaction_request_keeps_the_chat_thinking_settings() {
        let _base = crate::test_env::BaseDirGuard::new();
        let rt = chat("claude-sonnet-4-6", ReasoningLevel::High, CacheTtl::FiveMinutes);
        let (compaction, _) = assert_same_request_as_the_next_turn(&rt).await;
        assert_eq!(compaction.body["thinking"]["type"], "enabled");

        let rt = chat("claude-sonnet-4-6", ReasoningLevel::Off, CacheTtl::Hybrid);
        let (compaction, _) = assert_same_request_as_the_next_turn(&rt).await;
        assert!(compaction.body.get("thinking").is_none());
    }

    /// Subscription OAuth: the system blocks lead with the transport
    /// identity, exactly as the chat's.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn compaction_request_keeps_the_oauth_system_blocks() {
        let _base = crate::test_env::BaseDirGuard::new();
        let rt = on_a_subscription(opus());
        let (compaction, _) = assert_same_request_as_the_next_turn(&rt).await;
        assert_eq!(
            compaction.body["system"][0]["text"],
            super::super::helpers::CLAUDE_CODE_TRANSPORT_IDENTITY
        );
        assert!(!canon(&compaction.body["system"]).contains(&COMPACTION_SYSTEM_PROMPT[..40]));
    }

    /// Provenance names what was sent: the chat's model, and a prompt-stack
    /// digest over the system prompt and the instruction message as they
    /// went on the wire. A summary that came from the transcript request
    /// after a fallback carries the transcript request's stack instead.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn provenance_names_the_request_that_was_sent() {
        let _base = crate::test_env::BaseDirGuard::new();
        let rt = opus();
        let (compaction, outcome) = assert_same_request_as_the_next_turn(&rt).await;
        assert_eq!(outcome.summary_model, "claude-opus-4-7");
        assert_eq!(outcome.summary_provider, "anthropic");
        assert!(!outcome.local_only);
        let system = compaction.body["system"].as_array().unwrap();
        let system = system.last().unwrap()["text"].as_str().unwrap();
        let instructions = last_message_text(&compaction.body);
        assert_eq!(
            outcome.prompt_stack_digest,
            prompt_stack_digest(&[system, &instructions]),
            "the digest covers the system prompt and instructions that were sent"
        );
        assert!(outcome.excluded_classes.is_empty());
        assert_eq!(
            outcome.included_classes,
            vec![
                ContentClass::UserText,
                ContentClass::AssistantText,
                ContentClass::ToolCalls,
                ContentClass::ToolResults,
                ContentClass::FilePaths,
            ],
            "the classes the history holds"
        );

        // After a fallback the outcome describes the transcript request.
        let (url, mock) = spawn_mock(vec![
            Stream::Blocks(vec![text("hi")]),
            Stream::Blocks(vec![]),
        ])
        .await;
        let _url = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
        let rt = opus();
        let history = warm_chat(&rt).await;
        let fallen = compact_conversation(&history, &rt, None).await.unwrap();
        assert_eq!(mock.seen().len(), 3, "turn, chat request, transcript request");
        let rendered = render_compaction_input(&history, None, &rt.compaction_policy());
        assert_eq!(
            fallen.prompt_stack_digest,
            prompt_stack_digest(&[COMPACTION_SYSTEM_PROMPT, rendered.base_prompt])
        );
        assert_ne!(fallen.prompt_stack_digest, outcome.prompt_stack_digest);
    }

    /// Compact a warm chat after `change` and return what the provider saw
    /// AFTER the warm-up turn, with the outcome.
    async fn compact_after(
        mut rt: Runtime,
        streams: Vec<Stream>,
        change: impl FnOnce(&mut Runtime),
    ) -> (Vec<Seen>, CompactionOutcome) {
        let mut all = vec![Stream::Blocks(vec![text("hi")])];
        all.extend(streams);
        let (url, mock) = spawn_mock(all).await;
        let _url = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
        let history = warm_chat(&rt).await;
        change(&mut rt);
        let snapshot = history.clone();
        let outcome = compact_conversation(&history, &rt, None)
            .await
            .expect("compaction");
        assert_eq!(history, snapshot, "compaction never touches the history");
        (mock.seen().split_off(1), outcome)
    }

    fn cached_reply() -> Vec<Stream> {
        vec![Stream::Blocks(vec![text(CACHED_SUMMARY)])]
    }

    /// Exactly one request, the transcript one.
    fn assert_only_the_transcript_request(seen: &[Seen], outcome: &CompactionOutcome) {
        assert_eq!(seen.len(), 1, "one request");
        assert_transcript_request(&seen[0]);
        assert_eq!(outcome.summary_text, TRANSCRIPT_SUMMARY);
    }

    /// Control for the condition tests below: with nothing changed, the
    /// same harness sends the chat's own request and nothing else.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn a_warm_chat_is_summarized_by_its_own_next_request() {
        let _base = crate::test_env::BaseDirGuard::new();
        let (seen, outcome) = compact_after(opus(), cached_reply(), |_| {}).await;
        assert_eq!(seen.len(), 1);
        assert!(seen[0].streams());
        assert_eq!(outcome.summary_text, CACHED_SUMMARY);
    }

    /// Condition 1: an exclusion policy is never bypassed. The filtered
    /// transcript goes out, without the excluded class.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn an_exclusion_policy_sends_the_filtered_transcript() {
        let _base = crate::test_env::BaseDirGuard::new();
        let (seen, outcome) = compact_after(opus(), cached_reply(), |rt| {
            rt.set_compaction_exclusions(vec![ContentClass::ToolResults]);
        })
        .await;
        assert_only_the_transcript_request(&seen, &outcome);
        assert!(
            !canon(&seen[0].body).contains(RESULT_SENTINEL),
            "an excluded class reached the provider"
        );
        assert_eq!(outcome.excluded_classes, vec![ContentClass::ToolResults]);
    }

    /// Condition 1: local-only compaction sends nothing at all.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn local_only_sends_nothing() {
        let _base = crate::test_env::BaseDirGuard::new();
        let (seen, outcome) = compact_after(opus(), cached_reply(), |rt| {
            rt.set_compaction_mode(CompactionMode::LocalOnly);
        })
        .await;
        assert!(seen.is_empty());
        assert!(outcome.local_only);
    }

    /// Condition 2: a summary model other than the chat's has another cache.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn another_summary_model_sends_the_transcript() {
        let _base = crate::test_env::BaseDirGuard::new();
        let (seen, outcome) = compact_after(opus(), cached_reply(), |rt| {
            rt.set_compaction_model(Some("provider-small".to_string()));
            assert_ne!(rt.compact_model_choice().0, rt.model());
        })
        .await;
        assert_only_the_transcript_request(&seen, &outcome);
        assert_eq!(seen[0].body["model"], outcome.summary_model.as_str());
        assert_ne!(outcome.summary_model, "claude-opus-4-7");
    }

    /// Condition 3: only the native Anthropic transport. Decided before
    /// anything is built (no request can be observed for another provider
    /// here), with every other condition holding.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn another_provider_is_not_eligible() {
        let _base = crate::test_env::BaseDirGuard::new();
        let warm = |rt: &Runtime| rt.cache_anchor.stamp_identified(CacheAnchor::now(), SEVEN);
        let history = vec![user("hello"), Arc::new(json!({"role": "assistant", "content": "hi"}))];
        let policy = DisclosurePolicy::default();

        let rt = opus();
        warm(&rt);
        assert_eq!(
            cached_path(&rt, &history, rt.model(), &policy, "summarize").await,
            Ok(SEVEN),
            "control: an Anthropic chat with a warm cache is eligible"
        );

        let mut rt = opus();
        rt.set_model("groq/llama-3.3-70b-versatile".to_string());
        warm(&rt);
        assert_eq!(
            cached_path(&rt, &history, rt.model(), &policy, "summarize").await,
            Err(Skip::NotAnthropic)
        );
    }

    /// Condition 6: on a model that drops earlier thinking when a user
    /// message is added, a history with thinking blocks would lose its
    /// cached messages; one without them, or a model that keeps them, is
    /// eligible.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn a_model_that_strips_earlier_thinking_is_not_eligible_with_thinking_in_the_history() {
        let _base = crate::test_env::BaseDirGuard::new();
        let plain = vec![user("hello"), Arc::new(json!({"role": "assistant", "content": "hi"}))];
        let thought = vec![
            user("hello"),
            Arc::new(json!({"role": "assistant", "content": [
                {"type": "thinking", "thinking": "hm", "signature": "sig"},
                text("hi"),
            ]})),
        ];
        let policy = DisclosurePolicy::default();
        let eligible = |model: &'static str, history: Vec<SharedMessage>| {
            let policy = policy.clone();
            async move {
                let rt = chat(model, ReasoningLevel::High, CacheTtl::FiveMinutes);
                rt.cache_anchor.stamp_identified(CacheAnchor::now(), SEVEN);
                cached_path(&rt, &history, rt.model(), &policy, "summarize").await
            }
        };
        assert_eq!(
            eligible("claude-sonnet-4-5", thought.clone()).await,
            Err(Skip::ThinkingStripped)
        );
        assert_eq!(eligible("claude-sonnet-4-5", plain).await, Ok(SEVEN));
        assert_eq!(eligible("claude-sonnet-4-6", thought).await, Ok(SEVEN));
    }

    /// Condition 4: a history that does not fit the window with the
    /// instructions and the summary's output goes out as the (truncating)
    /// transcript.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn a_history_that_does_not_fit_sends_the_transcript() {
        let _base = crate::test_env::BaseDirGuard::new();
        let (seen, outcome) = compact_after(opus(), cached_reply(), |rt| {
            rt.set_context_window(Some(20_000));
        })
        .await;
        assert_only_the_transcript_request(&seen, &outcome);
    }

    /// Condition 5: a cache known to be cold. Writing the whole history at
    /// the write price costs more than sending the truncated transcript as
    /// plain input, and the entry would be thrown away with the history.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn a_cold_cache_sends_the_transcript() {
        let _base = crate::test_env::BaseDirGuard::new();
        let (seen, outcome) = compact_after(opus(), cached_reply(), |rt| {
            // The machine slept for two hours: the wall clock moved.
            let prefix = rt.cached_prefix().expect("the turn stamped one");
            rt.cache_anchor.stamp_identified(
                CacheAnchor {
                    mono: tokio::time::Instant::now(),
                    wall: std::time::SystemTime::now() - Duration::from_secs(7_200),
                },
                prefix,
            );
            assert!(matches!(
                futures::executor::block_on(rt.compaction_cache_state()),
                CacheState::Cold
            ));
        })
        .await;
        assert_only_the_transcript_request(&seen, &outcome);
    }

    /// Condition 5: nothing known about the cache (a session resumed in a
    /// new process has made no request yet).
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn an_unknown_cache_sends_the_transcript() {
        let _base = crate::test_env::BaseDirGuard::new();
        let (seen, outcome) = compact_after(opus(), cached_reply(), |rt| {
            // The same chat in a fresh process: a clone's request chain is
            // new (no anchor), like a runtime that has sent nothing.
            *rt = rt.clone();
            assert_eq!(
                futures::executor::block_on(rt.compaction_cache_state()),
                CacheState::Unknown
            );
        })
        .await;
        assert_only_the_transcript_request(&seen, &outcome);
    }

    /// Condition 5: the cache was written under another system prompt (or
    /// model, effort, thinking, tools, account): this request could not
    /// read it. Refused inside the request builder, before the network.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn a_changed_cache_identity_sends_the_transcript() {
        let _base = crate::test_env::BaseDirGuard::new();
        let (seen, outcome) = compact_after(opus(), cached_reply(), |rt| {
            rt.set_system_prompt("A different system prompt.".into());
        })
        .await;
        assert_only_the_transcript_request(&seen, &outcome);

        let (seen, outcome) = compact_after(opus(), cached_reply(), |rt| {
            rt.set_reasoning_level_explicit(ReasoningLevel::Low);
        })
        .await;
        assert_only_the_transcript_request(&seen, &outcome);
    }

    /// Condition 5: the cache is warm, but for another history. `/resume`
    /// in place swaps the history under the same runtime (same system
    /// prompt, tools and account, an anchor seconds old); a request for the
    /// resumed history would read none of the cached messages and write
    /// them all. Decided before anything is sent.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn a_swapped_history_sends_the_transcript() {
        let _base = crate::test_env::BaseDirGuard::new();
        let (url, mock) = spawn_mock(vec![
            Stream::Blocks(vec![text("hi")]),
            Stream::Blocks(vec![text(CACHED_SUMMARY)]),
        ])
        .await;
        let _url = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
        let rt = opus();
        let history = warm_chat(&rt).await;
        let policy = rt.compaction_policy();
        assert_eq!(SEVEN, CachedPrefix::of(7, &[]));
        let sent = rt.cached_prefix().expect("the turn stamped what it sent");
        assert_eq!(sent.messages, 1, "the turn's request held one message");
        assert_eq!(
            cached_path(&rt, &history, rt.model(), &policy, "summarize").await,
            Ok(sent),
            "control: the chat's own history is eligible"
        );

        let mut resumed = history.clone();
        resumed[0] = user("the first message of another session");
        assert_eq!(
            cached_path(&rt, &resumed, rt.model(), &policy, "summarize").await,
            Err(Skip::HistoryChanged)
        );
        let outcome = compact_conversation(&resumed, &rt, None).await.unwrap();
        assert_only_the_transcript_request(&mock.seen().split_off(1), &outcome);
    }

    /// The warm/cold boundary: the lifetime counts from the anchor, and its
    /// last [`LATE_GUARD`] is not usable.
    #[test]
    fn the_cache_is_warm_until_the_guard_before_its_ttl() {
        let (m5, h1) = (Duration::from_secs(300), Duration::from_secs(3_600));
        let at = |secs| Some(Duration::from_secs(secs));
        let p = Some(SEVEN);
        assert_eq!(cache_state(p, at(0), m5), CacheState::Warm(SEVEN));
        assert_eq!(cache_state(p, at(284), m5), CacheState::Warm(SEVEN));
        assert_eq!(cache_state(p, at(285), m5), CacheState::Cold);
        assert_eq!(cache_state(p, at(3_584), h1), CacheState::Warm(SEVEN));
        assert_eq!(cache_state(p, at(3_585), h1), CacheState::Cold);
        assert_eq!(cache_state(None, at(1), m5), CacheState::Unknown);
        assert_eq!(cache_state(p, None, m5), CacheState::Unknown);
    }

    /// The chat request was sent, failed, and the transcript request ran
    /// once after it. Nothing else reached the provider.
    fn assert_fell_back_once(seen: &[Seen], outcome: &CompactionOutcome) {
        assert_eq!(seen.len(), 2, "the chat request, then the transcript request");
        assert!(seen[0].streams());
        assert_transcript_request(&seen[1]);
        assert_eq!(outcome.summary_text, TRANSCRIPT_SUMMARY);
    }

    /// The model answers with a tool call (tools stay offered so the cache
    /// stays valid): the call never runs, its text is not a summary, and no
    /// tool result is ever sent back.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn a_tool_call_reply_runs_no_tool_and_falls_back_once() {
        let _base = crate::test_env::BaseDirGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("written-by-a-compaction-reply");
        let reply = vec![
            text("I'll save the summary to a file."),
            json!({"type": "tool_use", "id": "toolu_cc", "name": "write",
                "input": {"path": target.to_str().unwrap(), "content": "x"}}),
        ];
        let (seen, outcome) = compact_after(opus(), vec![Stream::Blocks(reply)], |_| {}).await;
        assert_fell_back_once(&seen, &outcome);
        assert!(!target.exists(), "a tool ran for a compaction reply");
        assert!(
            !canon(&seen[1].body).contains("toolu_cc"),
            "the tool call was answered"
        );
    }

    /// A reply with no text is not a summary.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn an_empty_reply_falls_back_once() {
        let _base = crate::test_env::BaseDirGuard::new();
        let (seen, outcome) = compact_after(opus(), vec![Stream::Blocks(vec![])], |_| {}).await;
        assert_fell_back_once(&seen, &outcome);

        let blank = vec![Stream::Blocks(vec![text("  \n")])];
        let (seen, outcome) = compact_after(opus(), blank, |_| {}).await;
        assert_fell_back_once(&seen, &outcome);
    }

    /// A refused request (HTTP 400, as a context-length refusal is).
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn an_error_falls_back_once() {
        let _base = crate::test_env::BaseDirGuard::new();
        let (seen, outcome) = compact_after(opus(), vec![Stream::Status(400)], |_| {}).await;
        assert_fell_back_once(&seen, &outcome);
    }

    /// The chat request is sent ONCE whatever fails: no header-timed 429
    /// wait (eight of them can outlast a 5-minute cache), no 5xx retry, no
    /// overload retry (ten on a subscription), even with retries
    /// configured. The transcript request that follows keeps its own.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn a_retryable_error_is_not_retried_on_the_chat_request() {
        let _base = crate::test_env::BaseDirGuard::new();
        for status in [429, 500, 529] {
            let mut rt = opus();
            rt.set_api_retries(3);
            let (seen, outcome) = compact_after(rt, vec![Stream::Status(status)], |_| {}).await;
            assert_fell_back_once(&seen, &outcome);
        }
        let mut rt = on_a_subscription(opus());
        rt.set_api_retries(3);
        let (seen, outcome) = compact_after(rt, vec![Stream::Overloaded], |_| {}).await;
        assert_fell_back_once(&seen, &outcome);
    }

    /// A request that wrote more than it read is reported, with the
    /// session and both numbers; one that mostly read is not.
    #[test]
    fn a_request_that_wrote_more_than_it_read_is_reported() {
        let usage = |read, written| KeepWarmUsage {
            cache_read_tokens: read,
            cache_write_tokens: written,
            ..KeepWarmUsage::default()
        };
        assert_eq!(cache_miss_warning(Some("s1"), &usage(400_000, 900)), None);
        assert_eq!(cache_miss_warning(Some("s1"), &usage(0, 0)), None);
        assert_eq!(cache_miss_warning(Some("s1"), &usage(5_000, 5_000)), None);
        let warning = cache_miss_warning(Some("sess-42"), &usage(4_000, 396_000)).expect("a miss");
        for part in ["sess-42", "396000 tokens written", "4000 read", "Likely cause"] {
            assert!(warning.contains(part), "{part}: {warning}");
        }
        assert!(cache_miss_warning(None, &usage(0, 1)).unwrap().contains("session unknown"));
    }

    /// The usage the provider reports for the chat request reaches that
    /// check: a reply that wrote the history is still a summary (it was
    /// paid for), and the miss is seen.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn the_usage_of_the_chat_request_is_read() {
        let _base = crate::test_env::BaseDirGuard::new();
        let (url, _mock) = spawn_mock(vec![
            Stream::Blocks(vec![text("hi")]),
            Stream::Blocks(vec![text(CACHED_SUMMARY)]),
        ])
        .await;
        let _url = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
        let rt = opus();
        let history = warm_chat(&rt).await;
        let expect = rt.cached_prefix().unwrap();
        let sent = rt
            .compaction_cached_request(&history, "summarize", expect)
            .await
            .expect("sent");
        assert_eq!(
            (sent.usage.cache_read_tokens, sent.usage.cache_write_tokens),
            (MOCK_CACHE_READ, MOCK_CACHE_WRITE),
            "the usage the provider reported"
        );
        assert!(cache_miss_warning(None, &sent.usage).is_none());
    }

    #[test]
    fn a_reply_is_a_summary_only_without_tool_calls() {
        let reply = |blocks: Value| json!({"content": blocks});
        assert_eq!(
            summary_from_reply(&reply(json!([
                {"type": "thinking", "thinking": "hm", "signature": "s"},
                {"type": "text", "text": "## Goal\n"},
                {"type": "text", "text": "x"},
            ]))),
            Ok("## Goal\nx".to_string())
        );
        for kind in ["tool_use", "server_tool_use", "mcp_tool_use"] {
            assert_eq!(
                summary_from_reply(&reply(json!([
                    {"type": "text", "text": "summary"},
                    {"type": kind, "id": "t", "name": "bash", "input": {}},
                ]))),
                Err("the reply called a tool")
            );
        }
        assert_eq!(summary_from_reply(&reply(json!([]))), Err("the reply has no text"));
        assert_eq!(summary_from_reply(&json!({})), Err("the reply has no text"));
    }
}
