//! Engine-level session management — save, load, resume, clear.
//!
//! Owns the conversation state that both TUI and headless modes need:
//! messages, token counts, cost, abort context.

use crate::{Session, Runtime};
use crate::pricing::calculate_cost_optional_split;
use serde_json::Value;

/// Conversation state tracked by the engine.
pub struct ConversationState {
    pub session: Session,
    pub api_messages: Vec<Value>,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cache_read_tokens: u64,
    pub total_cache_creation_tokens: u64,
    pub session_cost: f64,
    pub abort_context: Option<String>,
    /// Message queued to send after current stream completes.
    pub queued_message: Option<String>,
    /// Events buffered during streaming — drained on stream completion.
    pub pending_events: Vec<String>,
}

impl ConversationState {
    /// Create a new conversation with a fresh session.
    pub fn new(session: Session) -> Self {
        Self {
            session,
            api_messages: Vec::new(),
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_cache_read_tokens: 0,
            total_cache_creation_tokens: 0,
            session_cost: 0.0,
            abort_context: None,
            queued_message: None,
            pending_events: Vec::new(),
        }
    }

    /// Create from a resumed session.
    pub fn from_resumed(session: Session) -> Self {
        Self {
            api_messages: session.api_messages.clone(),
            total_input_tokens: session.total_input_tokens,
            total_output_tokens: session.total_output_tokens,
            total_cache_read_tokens: 0,
            total_cache_creation_tokens: 0,
            session_cost: session.session_cost,
            abort_context: session.abort_context.clone(),
            queued_message: None,
            pending_events: Vec::new(),
            session,
        }
    }

    /// Save the current conversation state to disk.
    pub async fn save(&mut self) {
        if self.api_messages.is_empty() {
            return;
        }
        self.session.api_messages = self.api_messages.clone();
        self.session.total_input_tokens = self.total_input_tokens;
        self.session.total_output_tokens = self.total_output_tokens;
        self.session.session_cost = self.session_cost;
        self.session.abort_context = self.abort_context.clone();
        self.session.updated_at = chrono::Utc::now();
        self.session.auto_title();
        if let Err(e) = self.session.save().await {
            tracing::error!("Failed to save session: {}", e);
        }
    }

    /// Clear the current session and start fresh.
    pub async fn clear(&mut self, runtime: &Runtime) {
        self.save().await;
        self.api_messages.clear();
        self.total_input_tokens = 0;
        self.total_output_tokens = 0;
        self.total_cache_read_tokens = 0;
        self.total_cache_creation_tokens = 0;
        self.session_cost = 0.0;
        self.abort_context = None;
        self.queued_message = None;
        self.pending_events.clear();
        self.session = Session::new(
            runtime.model(),
            runtime.thinking_level(),
            runtime.system_prompt(),
        );
    }

    /// Add usage from a model turn.
    ///
    /// `cache_creation_5m` / `cache_creation_1h` are the cache-write TTL
    /// split. When either is present the cost uses the split rates
    /// (5m: 1.25×, 1h: 2.0×); when both are `None` the aggregate
    /// `cache_creation` is billed at the 5m rate (fail-cheap fallback).
    #[allow(clippy::too_many_arguments)]
    pub fn add_usage(
        &mut self,
        input_tokens: u64,
        output_tokens: u64,
        cache_read: u64,
        cache_creation: u64,
        cache_creation_5m: Option<u64>,
        cache_creation_1h: Option<u64>,
        model: &str,
    ) {
        self.total_input_tokens += input_tokens;
        self.total_output_tokens += output_tokens;
        self.total_cache_read_tokens += cache_read;
        self.total_cache_creation_tokens += cache_creation;

        self.session_cost += calculate_cost_optional_split(
            model, input_tokens, output_tokens, cache_read,
            cache_creation, cache_creation_5m, cache_creation_1h,
        );
    }

    /// Estimate current token count (for compaction decisions).
    pub fn estimate_tokens(&self) -> usize {
        let mut total_chars = 0usize;
        for msg in &self.api_messages {
            if let Some(s) = msg["content"].as_str() {
                total_chars += s.len();
            } else if let Some(arr) = msg["content"].as_array() {
                for block in arr {
                    if let Some(s) = block["text"].as_str() {
                        total_chars += s.len();
                    }
                    if let Some(s) = block["thinking"].as_str() {
                        total_chars += s.len();
                    }
                    if let Some(s) = block["content"].as_str() {
                        total_chars += s.len();
                    } else if let Some(content_arr) = block["content"].as_array() {
                        for inner in content_arr {
                            if let Some(s) = inner["text"].as_str() {
                                total_chars += s.len();
                            }
                        }
                    }
                    if let Some(input) = block.get("input") {
                        total_chars += input.to_string().len();
                    }
                }
            }
        }
        total_chars / 4
    }
}
