use std::collections::VecDeque;
use std::time::Instant;

use futures::StreamExt;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{Runtime, StreamEvent, Session};
use super::{AgentBus, AgentEvent, Inbound, MetaEvent, SubagentEvent, SyncState, ToolEvent};

#[derive(Debug, Clone)]
pub struct DriverConfig {
    pub agent_name: Option<String>,
    pub auto_save: bool,
    pub event_buffer_size: usize,
}

impl Default for DriverConfig {
    fn default() -> Self {
        Self {
            agent_name: None,
            auto_save: true,
            event_buffer_size: 100,
        }
    }
}

pub struct ConversationDriver {
    runtime: Runtime,
    bus: AgentBus,
    config: DriverConfig,
    messages: Vec<Value>,
    session: Session,

    // Accumulators
    total_input_tokens: u64,
    total_output_tokens: u64,
    total_cost_usd: f64,
    turn_count: u64,
    tool_call_count: u64,
    tool_start_time: Option<Instant>,

    // Streaming state
    cancel: Option<CancellationToken>,
    steer_tx: Option<mpsc::UnboundedSender<String>>,
    is_streaming: bool,
    queued_followup: Option<String>,

    // For SyncState
    partial_text: String,
    partial_thinking: String,
    active_tool: Option<String>,
    recent_events: VecDeque<AgentEvent>,
}

impl ConversationDriver {
    pub fn new(runtime: Runtime, mut session: Session, config: DriverConfig) -> Self {
        let messages = std::mem::take(&mut session.api_messages);
        Self {
            runtime,
            bus: AgentBus::new(),
            config,
            messages,
            session,
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_cost_usd: 0.0,
            turn_count: 0,
            tool_call_count: 0,
            tool_start_time: None,
            cancel: None,
            steer_tx: None,
            is_streaming: false,
            queued_followup: None,
            partial_text: String::new(),
            partial_thinking: String::new(),
            active_tool: None,
            recent_events: VecDeque::new(),
        }
    }

    pub fn bus(&self) -> &AgentBus {
        &self.bus
    }

    pub fn bus_mut(&mut self) -> &mut AgentBus {
        &mut self.bus
    }

    pub fn messages(&self) -> &[Value] {
        &self.messages
    }

    pub fn is_streaming(&self) -> bool {
        self.is_streaming
    }

    pub fn sync_state(&self) -> SyncState {
        SyncState {
            agent_name: self.config.agent_name.clone(),
            model: self.runtime.model().to_string(),
            thinking_level: self.runtime.thinking_level().to_string(),
            session_id: self.session.id.clone(),
            is_streaming: self.is_streaming,
            turn_count: self.turn_count,
            total_input_tokens: self.total_input_tokens,
            total_output_tokens: self.total_output_tokens,
            total_cost_usd: self.total_cost_usd,
            partial_text: if self.partial_text.is_empty() { None } else { Some(self.partial_text.clone()) },
            partial_thinking: if self.partial_thinking.is_empty() { None } else { Some(self.partial_thinking.clone()) },
            active_tool: self.active_tool.clone(),
            recent_events: self.recent_events.iter().cloned().collect(),
        }
    }

    /// Main event loop. Pulls inbound messages from the bus and dispatches.
    pub async fn run(&mut self) -> crate::Result<()> {
        let mut inbound_rx = self.bus.take_inbound_rx()
            .expect("run() called twice — inbound_rx already taken");

        while let Some(inbound) = inbound_rx.recv().await {
            match inbound {
                Inbound::Message { content } => {
                    self.handle_user_message(content, &mut inbound_rx).await?;
                    // If a message arrived during streaming (e.g. user aborted
                    // then resent), process it now.
                    if let Some(followup) = self.queued_followup.take() {
                        self.handle_user_message(followup, &mut inbound_rx).await?;
                    }
                }
                Inbound::Steer { content } => {
                    if let Some(ref tx) = self.steer_tx {
                        let _ = tx.send(content);
                    }
                }
                Inbound::Cancel => {
                    if let Some(ref ct) = self.cancel {
                        ct.cancel();
                    }
                }
                Inbound::Command { name, args } => {
                    self.handle_command(&name, &args);
                }
                Inbound::SyncRequest => {
                    // Handled externally via sync_state()
                }
            }
        }
        Ok(())
    }

    /// Inject a user message programmatically (e.g. agent boot messages).
    /// Note: does not drain inbound during streaming — use bus Inbound::Message
    /// for interactive contexts where abort/steer must work.
    pub async fn inject_user_message(&mut self, content: String) -> crate::Result<()> {
        // Create a dummy receiver — inject_user_message doesn't support
        // mid-stream cancel/steer since there's no inbound channel to drain.
        let (_tx, mut rx) = mpsc::unbounded_channel();
        self.handle_user_message(content, &mut rx).await
    }

    /// Broadcast shutdown and cancel any active stream.
    pub fn shutdown(&self, reason: String) {
        self.bus.broadcast(AgentEvent::Meta(MetaEvent::Shutdown { reason }));
        if let Some(ref ct) = self.cancel {
            ct.cancel();
        }
    }

    async fn handle_user_message(
        &mut self,
        content: String,
        inbound_rx: &mut mpsc::UnboundedReceiver<Inbound>,
    ) -> crate::Result<()> {
        // Push user message
        self.messages.push(json!({"role": "user", "content": content}));

        // Reset per-turn state
        self.partial_text.clear();
        self.partial_thinking.clear();
        self.active_tool = None;

        // Create cancellation + steering
        let cancel = CancellationToken::new();
        self.cancel = Some(cancel.clone());

        let (steer_tx, steer_rx) = mpsc::unbounded_channel();
        self.steer_tx = Some(steer_tx);

        self.is_streaming = true;

        let mut stream = self.runtime
            .run_stream_with_messages(self.messages.clone(), cancel, Some(steer_rx))
            .await;

        // Select on both stream events AND inbound commands so that
        // Cancel and Steer are processed while the stream is active.
        // Without this, Esc (abort) and mid-stream steering are dead
        // because the outer run() loop isn't polled during streaming.
        loop {
            tokio::select! {
                event = stream.next() => {
                    match event {
                        Some(event) => self.handle_stream_event(event).await,
                        None => break,
                    }
                }
                Some(inbound) = inbound_rx.recv() => {
                    match inbound {
                        Inbound::Cancel => {
                            if let Some(ref ct) = self.cancel {
                                ct.cancel();
                            }
                        }
                        Inbound::Steer { content } => {
                            if let Some(ref tx) = self.steer_tx {
                                let _ = tx.send(content);
                            }
                        }
                        Inbound::Command { name, args } => {
                            self.handle_command(&name, &args);
                        }
                        // Messages arriving during streaming get queued —
                        // the sender (TUI) expects them to be delivered after
                        // the current stream finishes (e.g. abort then resend).
                        Inbound::Message { content } => {
                            // Push back: the outer run() loop will pick this up
                            // after handle_user_message returns.
                            // We can't easily push back into inbound_rx, so
                            // we handle it as a queued follow-up.
                            self.queued_followup = Some(content);
                        }
                        _ => {}
                    }
                }
            }
        }

        self.is_streaming = false;
        self.cancel = None;
        self.steer_tx = None;
        Ok(())
    }

    async fn handle_stream_event(&mut self, event: StreamEvent) {
        match event {
            StreamEvent::Text(ref t) => {
                self.partial_text.push_str(t);
                self.emit(AgentEvent::Text(t.clone()));
            }
            StreamEvent::Thinking(ref t) => {
                self.partial_thinking.push_str(t);
                self.emit(AgentEvent::Thinking(t.clone()));
            }
            StreamEvent::ToolUseStart(ref name) => {
                self.tool_call_count += 1;
                self.tool_start_time = Some(Instant::now());
                self.active_tool = Some(name.clone());
                self.emit(AgentEvent::Tool(ToolEvent::Start {
                    tool_name: name.clone(),
                    tool_id: String::new(),
                }));
            }
            StreamEvent::ToolUseDelta(ref d) => {
                self.emit(AgentEvent::Tool(ToolEvent::ArgsDelta(d.clone())));
            }
            StreamEvent::ToolUse { ref tool_name, ref tool_id, ref input } => {
                self.emit(AgentEvent::Tool(ToolEvent::Invoke {
                    tool_name: tool_name.clone(),
                    tool_id: tool_id.clone(),
                    input: input.clone(),
                }));
            }
            StreamEvent::ToolResultDelta { ref tool_id, ref delta } => {
                self.emit(AgentEvent::Tool(ToolEvent::OutputDelta {
                    tool_id: tool_id.clone(),
                    delta: delta.clone(),
                }));
            }
            StreamEvent::ToolResult { ref tool_id, ref result } => {
                let elapsed_ms = self.tool_start_time.take()
                    .map(|t| t.elapsed().as_millis() as u64);
                self.active_tool = None;
                self.emit(AgentEvent::Tool(ToolEvent::Complete {
                    tool_id: tool_id.clone(),
                    result: result.clone(),
                    elapsed_ms,
                }));
            }
            StreamEvent::SubagentStart { subagent_id, ref agent_name, ref task_preview } => {
                self.emit(AgentEvent::Subagent(SubagentEvent::Start {
                    id: subagent_id,
                    agent_name: agent_name.clone(),
                    task_preview: task_preview.clone(),
                }));
            }
            StreamEvent::SubagentUpdate { subagent_id, ref agent_name, ref status } => {
                self.emit(AgentEvent::Subagent(SubagentEvent::Update {
                    id: subagent_id,
                    agent_name: agent_name.clone(),
                    status: status.clone(),
                }));
            }
            StreamEvent::SubagentDone { subagent_id, ref agent_name, ref result_preview, duration_secs } => {
                self.emit(AgentEvent::Subagent(SubagentEvent::Done {
                    id: subagent_id,
                    agent_name: agent_name.clone(),
                    result_preview: result_preview.clone(),
                    duration_secs,
                }));
            }
            StreamEvent::SteeringDelivered { ref message } => {
                self.emit(AgentEvent::Meta(MetaEvent::Steered { message: message.clone() }));
            }
            StreamEvent::Usage { input_tokens, output_tokens, cache_read_input_tokens, cache_creation_input_tokens, ref model } => {
                let model_str = model.as_deref().unwrap_or(self.runtime.model());
                let cost = estimate_cost(input_tokens, output_tokens, model_str);
                self.total_input_tokens += input_tokens;
                self.total_output_tokens += output_tokens;
                self.total_cost_usd += cost;
                self.emit(AgentEvent::Meta(MetaEvent::Usage {
                    input_tokens,
                    output_tokens,
                    cache_read_tokens: cache_read_input_tokens,
                    cache_creation_tokens: cache_creation_input_tokens,
                    model: model_str.to_string(),
                    cost_usd: cost,
                }));
            }
            StreamEvent::MessageHistory(h) => {
                self.messages = h;
                if self.config.auto_save {
                    self.save_session().await;
                }
                // Broadcast to TUI so app.api_messages stays in sync
                self.emit(AgentEvent::MessageHistory(self.messages.clone()));
            }
            StreamEvent::Done => {
                self.turn_count += 1;
                self.emit(AgentEvent::Meta(MetaEvent::SessionStats {
                    total_input_tokens: self.total_input_tokens,
                    total_output_tokens: self.total_output_tokens,
                    total_cost_usd: self.total_cost_usd,
                    turn_count: self.turn_count,
                    tool_call_count: self.tool_call_count,
                }));
                self.emit(AgentEvent::TurnComplete);
            }
            StreamEvent::Error(ref e) => {
                // Clean up trailing broken messages
                if let Some(last) = self.messages.last() {
                    let role = last["role"].as_str().unwrap_or("");
                    let is_text_user = role == "user" && last["content"].is_string();
                    let is_assistant = role == "assistant";
                    if is_text_user || is_assistant {
                        self.messages.pop();
                    }
                }
                self.emit(AgentEvent::Error(e.clone()));
                self.emit(AgentEvent::TurnComplete);
            }
        }
    }

    fn handle_command(&mut self, name: &str, args: &str) {
        match name {
            "model" => {
                if args.is_empty() {
                    self.bus.broadcast(AgentEvent::Meta(MetaEvent::Steered {
                        message: format!("current model: {}", self.runtime.model()),
                    }));
                } else {
                    self.runtime.set_model(args.to_string());
                    self.bus.broadcast(AgentEvent::Meta(MetaEvent::Steered {
                        message: format!("model set to: {}", args),
                    }));
                }
            }
            "thinking" => {
                match args {
                    "low" => self.runtime.set_thinking_budget(2048),
                    "medium" | "med" => self.runtime.set_thinking_budget(4096),
                    "high" => self.runtime.set_thinking_budget(16384),
                    "xhigh" => self.runtime.set_thinking_budget(32768),
                    "" => {
                        self.bus.broadcast(AgentEvent::Meta(MetaEvent::Steered {
                            message: format!("thinking: {} ({})", self.runtime.thinking_level(), self.runtime.thinking_budget()),
                        }));
                        return;
                    }
                    _ => {
                        self.bus.broadcast(AgentEvent::Error(
                            "usage: /thinking low|medium|high|xhigh".to_string(),
                        ));
                        return;
                    }
                }
                self.bus.broadcast(AgentEvent::Meta(MetaEvent::Steered {
                    message: format!("thinking set to: {}", self.runtime.thinking_level()),
                }));
            }
            "clear" => {
                self.messages.clear();
                self.total_input_tokens = 0;
                self.total_output_tokens = 0;
                self.total_cost_usd = 0.0;
                self.turn_count = 0;
                self.tool_call_count = 0;
                self.session = Session::new(
                    self.runtime.model(),
                    self.runtime.thinking_level(),
                    self.runtime.system_prompt(),
                );
                self.bus.broadcast(AgentEvent::Meta(MetaEvent::Steered {
                    message: "session cleared".to_string(),
                }));
            }
            "system" => {
                if args.is_empty() || args == "show" {
                    let prompt = self.runtime.system_prompt().unwrap_or("(none)");
                    let display = crate::truncate_str(prompt, 200);
                    self.bus.broadcast(AgentEvent::Meta(MetaEvent::Steered {
                        message: format!("system prompt: {}", display),
                    }));
                } else {
                    self.runtime.set_system_prompt(args.to_string());
                    self.bus.broadcast(AgentEvent::Meta(MetaEvent::Steered {
                        message: "system prompt updated".to_string(),
                    }));
                }
            }
            _ => {
                self.bus.broadcast(AgentEvent::Error(
                    format!("unknown command: {}", name),
                ));
            }
        }
    }

    fn emit(&mut self, event: AgentEvent) {
        // Buffer for SyncState replays
        self.recent_events.push_back(event.clone());
        if self.recent_events.len() > self.config.event_buffer_size {
            self.recent_events.pop_front();
        }
        self.bus.broadcast(event);
    }

    async fn save_session(&mut self) {
        self.session.api_messages = self.messages.clone();
        self.session.total_input_tokens = self.total_input_tokens;
        self.session.total_output_tokens = self.total_output_tokens;
        self.session.session_cost = self.total_cost_usd;
        self.session.updated_at = chrono::Utc::now();
        self.session.auto_title();
        if let Err(e) = self.session.save().await {
            tracing::error!("Failed to save session: {}", e);
        }
    }
}

pub fn estimate_cost(input_tokens: u64, output_tokens: u64, model: &str) -> f64 {
    let (input_price, output_price) = match model {
        m if m.contains("opus") => (15.0, 75.0),
        m if m.contains("sonnet") => (3.0, 15.0),
        m if m.contains("haiku") => (0.80, 4.0),
        _ => (3.0, 15.0),
    };
    (input_tokens as f64 / 1_000_000.0) * input_price
        + (output_tokens as f64 / 1_000_000.0) * output_price
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_driver_default_config() {
        let config = DriverConfig::default();
        assert!(config.agent_name.is_none());
        assert!(config.auto_save);
        assert_eq!(config.event_buffer_size, 100);
    }

    #[test]
    fn test_estimate_cost_opus() {
        let cost = estimate_cost(1_000_000, 1_000_000, "claude-opus-4-6");
        assert!((cost - 90.0).abs() < 0.001); // 15 + 75
    }

    #[test]
    fn test_estimate_cost_sonnet() {
        let cost = estimate_cost(1_000_000, 1_000_000, "claude-sonnet-4-20250514");
        assert!((cost - 18.0).abs() < 0.001); // 3 + 15
    }

    #[test]
    fn test_estimate_cost_haiku() {
        let cost = estimate_cost(1_000_000, 1_000_000, "claude-3-haiku");
        assert!((cost - 4.80).abs() < 0.001); // 0.80 + 4.0
    }

    #[test]
    fn test_estimate_cost_unknown_defaults_to_sonnet() {
        let cost = estimate_cost(1_000_000, 1_000_000, "some-future-model");
        assert!((cost - 18.0).abs() < 0.001);
    }

    #[test]
    fn test_estimate_cost_zero_tokens() {
        let cost = estimate_cost(0, 0, "claude-opus-4-6");
        assert!((cost - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_estimate_cost_small_tokens() {
        // 1000 input tokens of opus: 1000/1M * 15 = 0.015
        let cost = estimate_cost(1000, 0, "claude-opus-4-6");
        assert!((cost - 0.015).abs() < 0.0001);
    }
}
