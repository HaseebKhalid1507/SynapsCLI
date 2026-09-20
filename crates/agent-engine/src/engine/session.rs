//! Engine-level session management — save, load, resume, clear.
//!
//! Owns the conversation state that both TUI and headless modes need:
//! messages, token counts, cost, abort context.

use crate::pricing::calculate_cost_optional_split;
use crate::SharedMessage;
use crate::{Runtime, Session};

/// Session-scoped durability barrier state, shared by persistent frontends.
/// A failed (or dropped in-flight) publication requires explicit reload/new
/// session before saving or scheduling more inference. Never rollback-save an
/// old head: `save_durable` can fail after publishing its replacement.
#[derive(Default)]
pub struct ContextHeadPersistence {
    blocked_session: Option<String>,
}

impl ContextHeadPersistence {
    pub fn is_blocked(&self, session: &Session) -> bool {
        self.blocked_session.as_deref() == Some(session.id.as_str())
    }

    /// Candidate metadata must come from the host's current session, never
    /// from message content. The event supplies only identity and messages.
    pub async fn persist(
        &mut self,
        session: &mut Session,
        messages: &mut Vec<SharedMessage>,
        session_id: &str,
        candidate: Session,
    ) -> std::io::Result<()> {
        self.persist_with(
            session,
            messages,
            session_id,
            candidate,
            |candidate| async move { candidate.save_durable().await },
        )
        .await
    }

    async fn persist_with<F, Fut>(
        &mut self,
        session: &mut Session,
        messages: &mut Vec<SharedMessage>,
        session_id: &str,
        candidate: Session,
        save: F,
    ) -> std::io::Result<()>
    where
        F: FnOnce(Session) -> Fut,
        Fut: std::future::Future<Output = std::io::Result<()>>,
    {
        if self.is_blocked(session) {
            return Err(std::io::Error::other(
                "context head is unverified; reload the session before continuing",
            ));
        }
        // Latch before any await, including on identity rejection. Dropping
        // this future cannot authorize a later ordinary save of the old head.
        self.blocked_session = Some(session.id.clone());
        if session_id != session.id || candidate.id != session.id {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "context head checkpoint does not match the current session",
            ));
        }
        let result = save(candidate.clone()).await;
        // Success adopts the durable head. An attempted-save error adopts it
        // ONLY as conservative recovery state (not a successful commit): a
        // rename may already have happened. The latch remains set, so neither
        // post-turn saves nor automatic inference can act on this ambiguity.
        *messages = candidate.api_messages.clone();
        *session = candidate;
        if result.is_ok() {
            self.blocked_session = None;
        }
        result
    }
}

/// Conversation state tracked by the engine.
pub struct ConversationState {
    pub session: Session,
    pub context_head: ContextHeadPersistence,
    pub api_messages: Vec<SharedMessage>,
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
            context_head: ContextHeadPersistence::default(),
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
            context_head: ContextHeadPersistence::default(),
        }
    }

    /// Save the current conversation state to disk.
    pub async fn save(&mut self) {
        if self.context_head.is_blocked(&self.session) || self.api_messages.is_empty() {
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

    /// Persist a runtime-requested head using host metadata and latest usage.
    /// The caller completes the receipt with this exact result and continues
    /// consuming the runtime's typed terminal error on failure.
    pub async fn persist_context_head(
        &mut self,
        session_id: &str,
        messages: Vec<SharedMessage>,
    ) -> std::io::Result<()> {
        let mut candidate = self.session.clone();
        candidate.api_messages = messages;
        candidate.total_input_tokens = self.total_input_tokens;
        candidate.total_output_tokens = self.total_output_tokens;
        candidate.session_cost = self.session_cost;
        candidate.abort_context = self.abort_context.clone();
        candidate.updated_at = chrono::Utc::now();
        candidate.auto_title();
        self.context_head
            .persist(
                &mut self.session,
                &mut self.api_messages,
                session_id,
                candidate,
            )
            .await
    }

    /// Clear the current session and start fresh.
    pub async fn clear(&mut self, runtime: &Runtime) {
        self.save().await;
        self.context_head = ContextHeadPersistence::default();
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
        runtime.reset_context_continuation(&self.session.id, &[]);
    }

    /// Serializable mirror for clients (`SessionEventWire::Conversation`).
    /// `consecutive_auto_turns` is actor/App-side state, not conversation
    /// state, so the caller supplies it.
    pub fn snapshot(
        &self,
        consecutive_auto_turns: u32,
    ) -> crate::session::ConversationSnapshot {
        crate::session::ConversationSnapshot {
            header: crate::session::SessionHeader::from(&self.session),
            api_messages: self.api_messages.clone(),
            messages_len: self.api_messages.len(),
            tokens: crate::session::ConversationTokens {
                input: self.total_input_tokens,
                output: self.total_output_tokens,
                cache_read: self.total_cache_read_tokens,
                cache_creation: self.total_cache_creation_tokens,
            },
            cost: self.session_cost,
            abort_context: self.abort_context.clone(),
            queued_message: self.queued_message.clone(),
            pending_events_len: self.pending_events.len(),
            consecutive_auto_turns,
        }
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
            model,
            input_tokens,
            output_tokens,
            cache_read,
            cache_creation,
            cache_creation_5m,
            cache_creation_1h,
        );
    }
}
