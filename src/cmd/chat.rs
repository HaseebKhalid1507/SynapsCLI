//! `synaps chat` — fully-featured headless mode.
//!
//! Same engine as the TUI (MCP, extensions, skills, session persistence,
//! compaction) but renders to stdin/stdout. Built for scripting,
//! piping, SSH, CI, and agent benchmark frameworks like Harbor.
//!
//! C4a: chat continues turns when pending runtime events are injected at
//! turn end (AutoTriggerEvents), bounded by AUTO_TURN_CAP.
//!
//! C4b: blocking read_line replaced with tokio::io::stdin + select! against
//! event_queue.notified(), so runtime events wake the prompt immediately.
//! Exactly ONE waiter on notified() exists while idle at the prompt.
//! Piped stdin, EOF, and CRLF behaviour are preserved.

#[cfg(feature = "legacy_inline")]
use futures::StreamExt;
#[cfg(feature = "legacy_inline")]
use serde_json::json;
#[cfg(feature = "legacy_inline")]
use std::io::{self, Write};
#[cfg(feature = "legacy_inline")]
use synaps_cli::engine::commands::{self, CommandResult};
#[cfg(feature = "legacy_inline")]
use synaps_cli::engine::reactor::{
    claim_auto_turn_with_cap, drain_event_queue, wake_action_with_cap, WakeAction,
    AUTO_TURN_CAP_CONFIG_KEY,
};
#[cfg(feature = "legacy_inline")]
use synaps_cli::engine::session::ConversationState;
#[cfg(feature = "legacy_inline")]
use synaps_cli::engine::setup::{self, EngineOpts};
#[cfg(feature = "legacy_inline")]
use synaps_cli::engine::stream::{self, EngineStreamEvent, StreamCompletion, SubagentTracker};
#[cfg(feature = "legacy_inline")]
use synaps_cli::runtime::compaction::{
    apply_compaction, compact_conversation, preview_compaction_disclosure, CompactionPolicy,
    CompactionTransition,
};
#[cfg(feature = "legacy_inline")]
use synaps_cli::{flush_stdout, CancellationToken};
#[cfg(feature = "legacy_inline")]
use tokio::io::{AsyncBufReadExt, BufReader as TokioBufReader};

/// What was read while waiting at the prompt.
#[cfg(feature = "legacy_inline")]
enum PromptRead {
    /// User typed (or pipe delivered) a line.
    Line(String),
    /// EOF on stdin.
    Eof,
    /// A runtime event woke us; drain + wake_action already ran.
    /// `run_turn` = true when wake_action said RunTurn.
    EventWake { run_turn: bool },
    /// I/O error.
    Error(std::io::Error),
}

/// Entry point. Runs on the `SessionActor` via `LocalTransport` (A2); the
/// pre-actor inline loop is kept behind `--features legacy_inline` +
/// `SYNAPS_CHAT_INLINE=1` until day 3.
pub async fn run(
    continue_session: Option<String>,
    system: Option<String>,
    agent: Option<String>,
    profile: Option<String>,
    no_extensions: bool,
) -> synaps_cli::Result<()> {
    #[cfg(feature = "legacy_inline")]
    if std::env::var("SYNAPS_CHAT_INLINE").map(|v| v == "1").unwrap_or(false) {
        return run_inline(continue_session, system, agent, profile, no_extensions).await;
    }
    actor::run(continue_session, system, agent, profile, no_extensions).await
}

#[cfg(feature = "legacy_inline")]
async fn run_inline(
    continue_session: Option<String>,
    system: Option<String>,
    agent: Option<String>,
    profile: Option<String>,
    no_extensions: bool,
) -> synaps_cli::Result<()> {
    // ── Boot engine ──
    let boot = setup::boot(EngineOpts {
        continue_session: continue_session.map(Some),
        system,
        prompt_manifest: None,
        profile,
        no_extensions,
    })
    .await?;

    let mut runtime = boot.runtime;
    let mut conv = if boot.continued {
        ConversationState::from_resumed(boot.session)
    } else {
        ConversationState::new(boot.session)
    };

    // Load agent prompt if specified
    if let Some(ref agent_name) = agent {
        match synaps_cli::tools::resolve_agent_prompt(agent_name) {
            Ok(p) => {
                eprintln!("🎭 Agent: {}", agent_name);
                runtime.set_system_prompt(p);
            }
            Err(e) => {
                eprintln!("❌ {}", e);
                std::process::exit(1);
            }
        }
    }

    // Extension discovery — await completion before entering the read loop.
    //
    // In pipe/headless mode (echo "..." | synaps chat ...) stdin is immediately
    // ready, so the old fire-and-forget approach caused a race: the first API
    // call fired before extension processes finished spawning, and the model
    // never saw extension-registered tools. Awaiting here ensures all extensions
    // are loaded before we read stdin.
    //
    // The TUI is unaffected — it has its own extension loader path that runs
    // concurrently with the event loop (human typing provides natural latency).
    if !boot.no_extensions {
        boot.ext_manager.write().await.discover_and_load().await;
    }

    eprintln!(
        "synaps {} | {} | session {}",
        env!("CARGO_PKG_VERSION"),
        runtime.model(),
        &conv.session.id[..8]
    );
    if boot.continued {
        eprintln!("↳ resumed session ({} messages)", conv.api_messages.len());
    }
    if boot.mcp_server_count > 0 {
        eprintln!("↳ {} MCP servers available", boot.mcp_server_count);
    }
    eprintln!();

    // ── Main loop ──
    let is_tty = std::io::IsTerminal::is_terminal(&std::io::stdin());
    let mut subagents: Vec<SubagentTracker> = Vec::new();
    // Typed spec §5.2 terminal failure of the last turn. In headless (piped)
    // mode an unrecovered failure aborts the read loop and the process exits
    // nonzero — after the session (with valid partial history) is saved.
    let mut fatal_failure: Option<synaps_cli::TurnError> = None;

    // C4a: consecutive auto-turn counter; reset to 0 on real user input.
    // Initial value doesn't matter — always reset before first turn.
    #[allow(unused_assignments)]
    let mut consecutive_auto_turns: u32 = 0;
    // Configured `events.auto_turn_cap` (default 5; 0 = unlimited).
    let auto_turn_cap = synaps_cli::core::config::load_config().events.auto_turn_cap;

    // C4b: async tokio stdin — lets us select! against event_queue.notified()
    // while idle at the prompt so runtime events can wake us immediately.
    //
    // For piped stdin (is_tty=false) this is identical in behaviour to the
    // old blocking read_line: lines() returns Ok(None) on EOF, each poll
    // returns one complete line without blocking the executor.
    let async_stdin = tokio::io::stdin();
    let mut stdin_lines = TokioBufReader::new(async_stdin).lines();

    loop {
        // ── Prompt ──
        if is_tty {
            eprint!("❯ ");
            io::stderr().flush().ok();
        }

        // C4b: Select between a new stdin line and a runtime event notification.
        // Exactly ONE waiter on event_queue.notified() exists while we are idle.
        let read = {
            let event_queue = runtime.event_queue().clone();
            tokio::select! {
                // Branch 1: user typed a line (or pipe delivered one).
                line = stdin_lines.next_line() => {
                    match line {
                        Ok(Some(l)) => PromptRead::Line(l),
                        Ok(None)    => PromptRead::Eof,
                        Err(e)      => PromptRead::Error(e),
                    }
                }
                // Branch 2: a runtime event arrived while we were idle.
                _ = event_queue.notified() => {
                    let drained = drain_event_queue(
                        &event_queue,
                        &mut conv.api_messages,
                        &mut conv.pending_events,
                        false, // idle
                        None,  // no steer channel
                    );
                    for d in &drained {
                        eprintln!("\x1b[36m⚡ [event] {}\x1b[0m", d.formatted);
                    }
                    let action = wake_action_with_cap(
                        &drained,
                        &conv.api_messages,
                        false,
                        true,  // auto_turn_enabled in chat mode
                        consecutive_auto_turns,
                        auto_turn_cap,
                    );
                    PromptRead::EventWake { run_turn: action == WakeAction::RunTurn }
                }
            }
        };

        match read {
            PromptRead::Error(e) => {
                eprintln!("input error: {}", e);
                break;
            }
            PromptRead::Eof => break,

            PromptRead::EventWake { run_turn: false } => {
                // Forward/Nothing — redraw prompt and wait for real input.
                if is_tty {
                    eprint!("❯ ");
                    io::stderr().flush().ok();
                }
                continue;
            }

            PromptRead::EventWake { run_turn: true } => {
                // A runtime event was injected; policy says RunTurn.
                // Events were already drained + injected by drain_event_queue above.
                // claim_auto_turn: increment only if allowed; if denied (cap) we
                // got run_turn=true from wake_action which already checked < cap,
                // so this should always succeed here — but use the gate for safety.
                let _ = claim_auto_turn_with_cap(&mut consecutive_auto_turns, auto_turn_cap);
            }

            PromptRead::Line(raw_line) => {
                let trimmed = raw_line.trim_end_matches('\r').trim();

                if trimmed.is_empty() {
                    continue;
                }

                // Real user input — reset auto-turn counter.
                consecutive_auto_turns = 0;

                // ── Slash commands ──
                if let Some((cmd, arg)) = commands::parse_command(trimmed) {
                    // Try engine-level command first
                    if let Some(result) = commands::handle_engine_command(cmd, arg, &mut runtime) {
                        match result {
                            CommandResult::Quit => break,
                            CommandResult::ModelChanged {
                                model,
                                reasoning_clamped,
                            } => {
                                conv.session.model = runtime.model().to_string();
                                eprintln!("model → {}", model);
                                if let Some(clamp) = reasoning_clamped {
                                    conv.session.thinking_level =
                                        runtime.thinking_level().to_string();
                                    eprintln!(
                                        "thinking → {} (clamped from {}: not supported by {})",
                                        clamp.to.as_str(),
                                        clamp.from.as_str(),
                                        runtime.model()
                                    );
                                }
                            }
                            CommandResult::ThinkingChanged { spec } => {
                                conv.session.thinking_level = spec.config_value();
                                eprintln!("thinking → {}", spec.level());
                            }
                            CommandResult::Compact {
                                custom_instructions,
                            } => {
                                eprintln!("compacting...");
                                // Spec §9.4: surface provider/model and the
                                // approximate disclosure BEFORE dispatch.
                                eprintln!(
                                    "{}",
                                    preview_compaction_disclosure(&runtime, &conv.api_messages)
                                        .render_line()
                                );
                                match compact_conversation(
                                    &conv.api_messages,
                                    &runtime,
                                    custom_instructions.as_deref(),
                                )
                                .await
                                {
                                    Ok(outcome) => match apply_compaction(
                                        &runtime,
                                        &conv.session,
                                        &conv.api_messages,
                                        &outcome,
                                        CompactionTransition {
                                            policy: CompactionPolicy::InPlace,
                                            pending_events: Vec::new(),
                                            queued_message: None,
                                            hook_source: "manual".to_string(),
                                        },
                                    )
                                    .await
                                    {
                                        Ok(applied) => {
                                            conv.session = applied.session;
                                            conv.api_messages = applied.api_messages;
                                            let after =
                                                runtime.assess_context(&conv.api_messages).await;
                                            eprintln!(
                                                "compacted → ~{} tokens",
                                                after.used_tokens()
                                            );
                                        }
                                        Err(e) => eprintln!("compaction failed: {}", e),
                                    },
                                    Err(e) => eprintln!("compaction failed: {}", e),
                                }
                            }
                            CommandResult::Error(e) => eprintln!("error: {}", e),
                            CommandResult::Output(text) => println!("{}", text),
                            _ => {} // Other results handled by TUI only
                        }
                        continue;
                    }

                    // Commands not handled by engine — headless-specific handling
                    match cmd {
                        "clear" => {
                            conv.clear(&runtime).await;
                            eprintln!("session cleared → {}", &conv.session.id[..8]);
                        }
                        "sessions" => match synaps_cli::list_recent_sessions(20) {
                            Ok(sessions) => {
                                for s in sessions.iter().take(20) {
                                    let marker = if s.id == conv.session.id {
                                        "→ "
                                    } else {
                                        "  "
                                    };
                                    eprintln!(
                                        "{}{} {} ({}, ${:.4})",
                                        marker,
                                        &s.id[..8],
                                        s.title,
                                        s.model,
                                        s.session_cost
                                    );
                                }
                            }
                            Err(e) => eprintln!("error: {}", e),
                        },
                        "status" => {
                            eprintln!("session: {}", &conv.session.id[..8]);
                            eprintln!("model: {}", runtime.model());
                            eprintln!(
                                "tokens: {}↑ {}↓",
                                conv.total_input_tokens, conv.total_output_tokens
                            );
                            eprintln!("cost: ${:.4}", conv.session_cost);
                            eprintln!("messages: {}", conv.api_messages.len());
                            let assessment = runtime.assess_context(&conv.api_messages).await;
                            eprintln!(
                                "context: ~{} of {} budget tokens ({} window)",
                                assessment.used_tokens(),
                                assessment.budget_tokens(),
                                assessment.provider_window
                            );
                        }
                        "help" => {
                            eprintln!("commands: /model /thinking /compact /clear /sessions /status /quit");
                        }
                        _ => {
                            eprintln!("unknown command: /{} (try /help)", cmd);
                        }
                    }
                    continue;
                }

                // ── Regular user message ──
                let message = if let Some(ctx) = conv.abort_context.take() {
                    format!("{}\n\n[ABORT CONTEXT — your previous response was interrupted. Here's what you completed before the abort:]\n\n{}\n\n[END ABORT CONTEXT — continue from where you left off or adjust based on the user's new message]", trimmed, ctx)
                } else {
                    trimmed.to_string()
                };
                conv.api_messages.push(std::sync::Arc::new(
                    json!({"role": "user", "content": message}),
                ));
            }
        }

        // ── C4a: turn loop — run until no pending events or cap reached ──
        'turn_loop: loop {
            let cancel = CancellationToken::new();
            // Vec<SharedMessage> clone = pointer bumps only.
            let msgs_in: Vec<synaps_cli::SharedMessage> = conv.api_messages.clone();
            // Failure repair may only remove messages appended by this turn.
            let turn_baseline = msgs_in.len();
            let mut stream = runtime
                .run_stream_with_messages(msgs_in, cancel, None, None, false)
                .await;

            let mut in_thinking = false;

            let turn_completion = loop {
                let Some(event) = stream.next().await else {
                    break StreamCompletion::Done;
                };
                let (engine_event, completion) = stream::process_stream_event(
                    event,
                    &mut conv.api_messages,
                    &mut subagents,
                    &mut conv.queued_message,
                    &mut conv.pending_events,
                    turn_baseline,
                );

                match engine_event {
                    EngineStreamEvent::Thinking(text) => {
                        if !in_thinking {
                            eprint!("\x1b[2m"); // dim
                            in_thinking = true;
                        }
                        eprint!("{}", text);
                        io::stderr().flush().ok();
                    }
                    EngineStreamEvent::Text(text) => {
                        if in_thinking {
                            eprintln!("\x1b[0m"); // reset
                            in_thinking = false;
                        }
                        print!("{}", text);
                        flush_stdout();
                    }
                    EngineStreamEvent::ToolStart { tool_name, .. } => {
                        if in_thinking {
                            eprintln!("\x1b[0m");
                            in_thinking = false;
                        }
                        eprint!("\x1b[33m⚙ {}\x1b[0m", tool_name);
                        io::stderr().flush().ok();
                    }
                    EngineStreamEvent::ToolFinalized {
                        tool_name, input, ..
                    } => {
                        let input_preview = serde_json::to_string(&input).unwrap_or_default();
                        let preview: String = input_preview.chars().take(60).collect();
                        eprintln!("\x1b[33m ⚙ {} ({})\x1b[0m", tool_name, preview);
                    }
                    EngineStreamEvent::ToolResult { result, .. } => {
                        let preview: String = result.chars().take(80).collect();
                        eprintln!("\x1b[32m  → {}\x1b[0m", preview);
                    }
                    EngineStreamEvent::SubagentStart { name, task, .. } => {
                        eprintln!("\x1b[35m🎭 [{}] {}\x1b[0m", name, task);
                    }
                    EngineStreamEvent::SubagentDone {
                        status,
                        duration_secs,
                        ..
                    } => {
                        eprintln!("\x1b[32m✔ {} ({:.1}s)\x1b[0m", status, duration_secs);
                    }
                    EngineStreamEvent::Usage {
                        input_tokens,
                        output_tokens,
                        cache_read,
                        cache_creation,
                        cache_creation_5m,
                        cache_creation_1h,
                        model,
                    } => {
                        let model_name = model.as_deref().unwrap_or(runtime.model());
                        conv.add_usage(
                            input_tokens,
                            output_tokens,
                            cache_read,
                            cache_creation,
                            cache_creation_5m,
                            cache_creation_1h,
                            model_name,
                        );
                    }
                    EngineStreamEvent::SteeringDelivered { message } => {
                        eprintln!("\x1b[33m→ [steering] {}\x1b[0m", message);
                    }
                    EngineStreamEvent::Notice(text) => {
                        eprintln!("\x1b[2m{}\x1b[0m", text);
                    }
                    EngineStreamEvent::Done | EngineStreamEvent::Noop => {}
                    EngineStreamEvent::Error(e) => {
                        eprintln!("\x1b[31m❌ {}\x1b[0m", e);
                    }
                    _ => {}
                }

                match completion {
                    StreamCompletion::Done => {
                        if in_thinking {
                            eprintln!("\x1b[0m");
                        }
                        println!();
                        break StreamCompletion::Done;
                    }
                    StreamCompletion::Error(err) => {
                        // Typed spec §5.2 outcome — do NOT collapse into Done.
                        if in_thinking {
                            eprintln!("\x1b[0m");
                        }
                        println!();
                        break StreamCompletion::Error(err);
                    }
                    StreamCompletion::AutoSendQueued(queued) => {
                        if in_thinking {
                            eprintln!("\x1b[0m");
                        }
                        conv.api_messages.push(std::sync::Arc::new(
                            json!({"role": "user", "content": queued}),
                        ));
                        break StreamCompletion::AutoSendQueued(String::new());
                    }
                    StreamCompletion::AutoTriggerEvents => {
                        if in_thinking {
                            eprintln!("\x1b[0m");
                        }
                        break StreamCompletion::AutoTriggerEvents;
                    }
                    StreamCompletion::Continue => {}
                }
            };

            // Post-turn: save + auto-compact. The trigger decision is the
            // engine's request-aware budget (T29, spec §9.1) — no local
            // token math.
            conv.save().await;
            let assessment = runtime.assess_context(&conv.api_messages).await;
            if assessment.should_compact() {
                eprintln!(
                    "\x1b[2m[auto-compacting ~{} tokens...]\x1b[0m",
                    assessment.used_tokens()
                );
                // Spec §9.4: pre-dispatch disclosure (provider/model/bytes).
                eprintln!(
                    "\x1b[2m[{}]\x1b[0m",
                    preview_compaction_disclosure(&runtime, &conv.api_messages).render_line()
                );
                match compact_conversation(&conv.api_messages, &runtime, None).await {
                    Ok(outcome) => match apply_compaction(
                        &runtime,
                        &conv.session,
                        &conv.api_messages,
                        &outcome,
                        CompactionTransition {
                            policy: CompactionPolicy::InPlace,
                            pending_events: Vec::new(),
                            queued_message: None,
                            hook_source: "auto".to_string(),
                        },
                    )
                    .await
                    {
                        Ok(applied) => {
                            conv.session = applied.session;
                            conv.api_messages = applied.api_messages;
                            let after = runtime.assess_context(&conv.api_messages).await;
                            eprintln!(
                                "\x1b[2m[compacted → ~{} tokens]\x1b[0m",
                                after.used_tokens()
                            );
                        }
                        Err(e) => eprintln!("\x1b[2m[compaction failed: {}]\x1b[0m", e),
                    },
                    Err(e) => eprintln!("\x1b[2m[compaction failed: {}]\x1b[0m", e),
                }
            }

            // C4a: decide whether to continue for another auto-turn.
            match turn_completion {
                StreamCompletion::AutoSendQueued(_) => {
                    // User-driven queued message — reset cap and loop.
                    consecutive_auto_turns = 0;
                    continue 'turn_loop;
                }
                StreamCompletion::AutoTriggerEvents => {
                    // Pending events were injected by process_stream_event; drain any
                    // remaining queue items and decide via central wake_action.
                    let drained = drain_event_queue(
                        runtime.event_queue(),
                        &mut conv.api_messages,
                        &mut conv.pending_events,
                        false, // idle after turn
                        None,
                    );
                    for d in &drained {
                        eprintln!("\x1b[36m⚡ [event] {}\x1b[0m", d.formatted);
                    }
                    let action = wake_action_with_cap(
                        &drained,
                        &conv.api_messages,
                        false,
                        true, // auto_turn_enabled
                        consecutive_auto_turns,
                        auto_turn_cap,
                    );
                    match action {
                        WakeAction::RunTurn => {
                            if claim_auto_turn_with_cap(&mut consecutive_auto_turns, auto_turn_cap)
                            {
                                continue 'turn_loop;
                            } else {
                                // claim denied: counter was already at cap.
                                // fall through to park (treated as Forward).
                                eprintln!(
                                    "\x1b[2m[auto-turn cap ({}) reached — waiting for user input; raise with `{} = N`, 0 = unlimited]\x1b[0m",
                                    auto_turn_cap, AUTO_TURN_CAP_CONFIG_KEY
                                );
                                break 'turn_loop;
                            }
                        }
                        WakeAction::Forward | WakeAction::Nothing => {
                            // Cap reached or nothing more — park at prompt.
                            break 'turn_loop;
                        }
                    }
                }
                StreamCompletion::Error(err) => {
                    // Unrecovered turn failure. History was already repaired
                    // (turn-appended invalid messages only) and saved above.
                    eprintln!("\x1b[31m❌ turn failed [{}]\x1b[0m", err.category_label());
                    fatal_failure = Some(err);
                    break 'turn_loop;
                }
                _ => {
                    // Done — exit turn loop normally.
                    break 'turn_loop;
                }
            }
        } // end 'turn_loop

        // Headless (piped) mode: an unrecovered failure must terminate with a
        // nonzero exit instead of silently waiting for more input. In
        // interactive (TTY) mode the user keeps their session and can retry.
        if fatal_failure.is_some() {
            if !is_tty {
                break;
            }
            fatal_failure = None;
        }
    }

    // ── Shutdown ──
    conv.save().await;

    // Fire on_session_end hook
    let hook_event =
        synaps_cli::extensions::hooks::events::HookEvent::on_session_end(&conv.session.id, None);
    let _ = runtime.hook_bus().emit(&hook_event).await;

    boot.background.shutdown();

    // Bounded observability flush (Task 11): stop intake on the session's
    // telemetry/trace writer and drain it before the process exits. Runs
    // after session save + hooks, and BEFORE the success/failure branch
    // below so the fatal-failure return path flushes too. Telemetry `off`
    // → no writer → `None`, a true no-op. "Flushed" means appended into OS
    // file buffers (no fsync — best-effort diagnostic logs). A timeout
    // logs metadata-only stats and continues; trace loss never changes the
    // exit outcome.
    if let Some(outcome) = runtime
        .shutdown_observability_async(
            synaps_cli::runtime::telemetry::DEFAULT_SHUTDOWN_FLUSH_TIMEOUT,
        )
        .await
    {
        if !outcome.is_flushed() {
            tracing::warn!(
                stats = ?outcome.stats(),
                "observability flush timed out — detached worker keeps draining"
            );
        }
    }

    eprintln!(
        "session saved: {} (${:.4})",
        &conv.session.id[..8],
        conv.session_cost
    );

    // Criterion: headless `synaps chat` exits nonzero on an unrecovered
    // provider/tool failure — after the partial history was saved above.
    if let Some(err) = fatal_failure {
        return Err(synaps_cli::RuntimeError::Session(format!(
            "turn failed: {} [{}]",
            err.message,
            err.category_label()
        )));
    }
    Ok(())
}

// ── A2: `synaps chat` on the SessionActor ─────────────────────────────────────

mod actor {
    //! Same stdin/stdout/stderr rendering as the inline loop; the turn
    //! machine (submit, usage, save, auto-turns, compaction) lives in the
    //! actor. stdin is only polled while the session is idle, so a piped
    //! line is always a `Submit` — exactly the inline loop's sequencing.

    use std::io::{self, Write};

    use synaps_cli::engine::commands;
    use agent_engine::session::{
        ClientKind, ClientMeta, ClientTransport, EndReason, Envelope, LocalTransport,
        SessionCommand, SessionConfig, SessionEventWire, SessionQuery, SessionSetting,
    };
    use agent_engine::{EngineHost, HostOpts};
    use synaps_cli::flush_stdout;
    use synaps_cli::{AgentEvent, LlmEvent, SessionEvent, StreamEvent};
    use tokio::io::{AsyncBufReadExt, BufReader as TokioBufReader};

    /// Reserved query id for the post-compaction token readout (user
    /// queries start at 1 and count up).
    const COMPACT_TOKENS_QUERY: u64 = u64::MAX - 1;

    struct Render {
        is_tty: bool,
        in_thinking: bool,
        streaming: bool,
        idle: bool,
        fatal: Option<synaps_cli::TurnError>,
        cost: f64,
        ended: bool,
    }

    impl Render {
        fn end_thinking(&mut self) {
            if self.in_thinking {
                eprintln!("\x1b[0m");
                self.in_thinking = false;
            }
        }

        async fn on_event(&mut self, transport: &LocalTransport, env: Envelope) {
            match env.event {
                SessionEventWire::Stream(ev) => self.on_stream(ev),
                SessionEventWire::TurnStarted { .. } => {
                    self.streaming = true;
                    self.idle = false;
                }
                SessionEventWire::Conversation(c) => self.cost = c.cost,
                SessionEventWire::Idle => self.idle = true,
                SessionEventWire::Prompt(pr) => {
                    // Headless chat has no prompt UI: cancel, as the inline
                    // loop did by passing no SecretPromptHandle.
                    let _ = transport
                        .send(SessionCommand::Answer {
                            prompt_id: pr.id,
                            value: None,
                        })
                        .await;
                }
                SessionEventWire::External(event) => {
                    eprintln!(
                        "\x1b[36m⚡ [event] {}\x1b[0m",
                        synaps_cli::events::format_event_for_agent(&event)
                    );
                }
                SessionEventWire::AutoTurnCapReached { cap } => {
                    eprintln!(
                        "\x1b[2m[auto-turn cap ({}) reached — waiting for user input]\x1b[0m",
                        cap
                    );
                }
                SessionEventWire::SystemNotice(text) => eprintln!("\x1b[2m{}\x1b[0m", text),
                SessionEventWire::Aborted { context_saved } => eprintln!(
                    "\x1b[2m{}\x1b[0m",
                    if context_saved {
                        "aborted — context saved for next message"
                    } else {
                        "aborted"
                    }
                ),
                SessionEventWire::Cleared { session_id } => eprintln!(
                    "session cleared → {}",
                    &session_id[..8.min(session_id.len())]
                ),
                // B2: spawned compaction reports through typed events — each
                // rendered exactly once (the inline loop's three lines).
                SessionEventWire::CompactionStarted { disclosure, .. } => {
                    eprintln!("compacting...");
                    eprintln!("{}", disclosure);
                }
                SessionEventWire::CompactionApplied { .. } => {
                    // `compacted → ~N tokens` needs the post-apply assessment.
                    let _ = transport
                        .send(SessionCommand::Query {
                            id: COMPACT_TOKENS_QUERY,
                            query: SessionQuery::ContextAssessment,
                        })
                        .await;
                }
                SessionEventWire::QueryResult { id, value } if id == COMPACT_TOKENS_QUERY => {
                    eprintln!(
                        "\x1b[2m[compacted → ~{} tokens]\x1b[0m",
                        value["used_tokens"].as_u64().unwrap_or(0)
                    )
                }
                SessionEventWire::CompactionFailed { message, .. } => {
                    eprintln!("\x1b[2m[compaction failed: {}]\x1b[0m", message)
                }
                SessionEventWire::CompactionCancelled => {
                    eprintln!("\x1b[2m[compaction cancelled]\x1b[0m")
                }
                SessionEventWire::Ended { .. } => self.ended = true,
                _ => {}
            }
        }

        fn on_stream(&mut self, ev: StreamEvent) {
            match ev {
                StreamEvent::Llm(LlmEvent::Thinking(text)) => {
                    if !self.in_thinking {
                        eprint!("\x1b[2m");
                        self.in_thinking = true;
                    }
                    eprint!("{}", text);
                    io::stderr().flush().ok();
                }
                StreamEvent::Llm(LlmEvent::Text(text)) => {
                    self.end_thinking();
                    print!("{}", text);
                    flush_stdout();
                }
                StreamEvent::Llm(LlmEvent::ToolUseStart { tool_name, .. }) => {
                    self.end_thinking();
                    eprint!("\x1b[33m⚙ {}\x1b[0m", tool_name);
                    io::stderr().flush().ok();
                }
                StreamEvent::Llm(LlmEvent::ToolUse {
                    tool_name, input, ..
                }) => {
                    let input_preview = serde_json::to_string(&input).unwrap_or_default();
                    let preview: String = input_preview.chars().take(60).collect();
                    eprintln!("\x1b[33m ⚙ {} ({})\x1b[0m", tool_name, preview);
                }
                StreamEvent::Llm(LlmEvent::ToolResult { result, .. }) => {
                    let preview: String = result.chars().take(80).collect();
                    eprintln!("\x1b[32m  → {}\x1b[0m", preview);
                }
                StreamEvent::Agent(AgentEvent::SubagentStart {
                    agent_name,
                    task_preview,
                    ..
                }) => eprintln!("\x1b[35m🎭 [{}] {}\x1b[0m", agent_name, task_preview),
                StreamEvent::Agent(AgentEvent::SubagentDone {
                    result_preview,
                    duration_secs,
                    ..
                }) => {
                    let status = if result_preview.starts_with("[TIMED OUT") {
                        "\u{26a0} timed out".to_string()
                    } else if result_preview.starts_with("ERROR") {
                        let preview: String = result_preview.chars().take(40).collect();
                        format!("\u{2718} {}", preview)
                    } else {
                        let preview: String = result_preview.chars().take(40).collect();
                        format!("\u{2714} {}", preview)
                    };
                    eprintln!("\x1b[32m✔ {} ({:.1}s)\x1b[0m", status, duration_secs);
                }
                StreamEvent::Agent(AgentEvent::SteeringDelivered { message }) => {
                    eprintln!("\x1b[33m→ [steering] {}\x1b[0m", message);
                }
                StreamEvent::Session(SessionEvent::Notice(text)) => {
                    eprintln!("\x1b[2m{}\x1b[0m", text);
                }
                StreamEvent::Session(SessionEvent::Done) => {
                    self.end_thinking();
                    println!();
                    self.streaming = false;
                }
                StreamEvent::Session(SessionEvent::Error(err)) => {
                    eprintln!("\x1b[31m❌ {}\x1b[0m", err.message);
                    self.end_thinking();
                    println!();
                    eprintln!("\x1b[31m❌ turn failed [{}]\x1b[0m", err.category_label());
                    self.fatal = Some(err);
                    self.streaming = false;
                }
                _ => {}
            }
        }

        /// Pump events until the reply for query `id` arrives.
        async fn reply(&mut self, t: &mut LocalTransport, id: u64) -> serde_json::Value {
            while let Some(env) = t.next_event().await {
                if let SessionEventWire::QueryResult { id: rid, value } = &env.event {
                    if *rid == id {
                        return value.clone();
                    }
                }
                self.on_event(t, env).await;
                if self.ended {
                    break;
                }
            }
            serde_json::Value::Null
        }
    }

    pub(super) async fn run(
        continue_session: Option<String>,
        system: Option<String>,
        agent: Option<String>,
        profile: Option<String>,
        no_extensions: bool,
    ) -> synaps_cli::Result<()> {
        // Agent prompt resolves before any session work (inline loop: exit 1).
        let agent_prompt = match agent {
            Some(ref agent_name) => match synaps_cli::tools::resolve_agent_prompt(agent_name) {
                Ok(p) => {
                    eprintln!("🎭 Agent: {}", agent_name);
                    Some(p)
                }
                Err(e) => {
                    eprintln!("❌ {}", e);
                    std::process::exit(1);
                }
            },
            None => None,
        };

        let host = EngineHost::boot_and_install(HostOpts {
            profile,
            no_extensions,
        })
        .await?;
        // Extension discovery completes BEFORE the session exists, so
        // `on_session_start` reaches subscribers and the first API call sees
        // extension-registered tools (piped stdin is ready immediately).
        if !no_extensions {
            host.ext_manager().write().await.discover_and_load().await;
        }

        let handle = host
            .create_session(SessionConfig {
                continue_session: continue_session.map(Some),
                system,
                auto_compact: true,
                ..SessionConfig::default()
            })
            .await?;
        let (mut t, snap) = LocalTransport::attach(handle, ClientMeta::new(ClientKind::Chat))
            .await
            .map_err(|e| synaps_cli::RuntimeError::Session(e.to_string()))?;
        if let Some(p) = agent_prompt {
            let _ = t
                .send(SessionCommand::Set { id: 0, setting: SessionSetting::SystemPrompt { text: p } })
                .await;
        }

        let session_id = snap.meta.id.as_str().to_string();
        eprintln!(
            "synaps {} | {} | session {}",
            env!("CARGO_PKG_VERSION"),
            snap.view.model,
            &session_id[..8.min(session_id.len())]
        );
        if snap.meta.continued {
            eprintln!(
                "↳ resumed session ({} messages)",
                snap.conversation.api_messages.len()
            );
        }
        if host.mcp_server_count() > 0 {
            eprintln!("↳ {} MCP servers available", host.mcp_server_count());
        }
        eprintln!();

        let mut r = Render {
            is_tty: std::io::IsTerminal::is_terminal(&std::io::stdin()),
            in_thinking: false,
            streaming: false,
            idle: true,
            fatal: None,
            cost: snap.conversation.cost,
            ended: false,
        };
        let mut stdin_lines = TokioBufReader::new(tokio::io::stdin()).lines();
        let mut next_query: u64 = 1;
        let mut prompt_shown = false;

        loop {
            if r.idle && r.is_tty && !prompt_shown {
                eprint!("❯ ");
                io::stderr().flush().ok();
                prompt_shown = true;
            }
            // stdin only while idle: a piped line is always a Submit.
            let line = tokio::select! {
                biased;
                ev = t.next_event() => match ev {
                    Some(env) => {
                        r.on_event(&t, env).await;
                        if r.ended { break; }
                        // Headless (piped): an unrecovered failure terminates
                        // after the turn settles; TTY users keep their session.
                        if r.idle && r.fatal.is_some() && !r.is_tty { break; }
                        if r.idle && r.fatal.is_some() { r.fatal = None; }
                        continue;
                    }
                    None => break,
                },
                line = stdin_lines.next_line(), if r.idle => match line {
                    Ok(Some(l)) => l,
                    Ok(None) => break,
                    Err(e) => { eprintln!("input error: {}", e); break; }
                },
            };
            prompt_shown = false;

            let trimmed = line.trim_end_matches('\r').trim();
            if trimmed.is_empty() {
                continue;
            }

            if let Some((cmd, arg)) = commands::parse_command(trimmed) {
                let id = next_query;
                next_query += 1;
                let _ = t
                    .send(SessionCommand::EngineCommand {
                        id,
                        name: cmd.to_string(),
                        arg: arg.to_string(),
                    })
                    .await;
                let reply = r.reply(&mut t, id).await;
                if r.ended {
                    break;
                }
                match reply["kind"].as_str().unwrap_or("none") {
                    "quit" => break,
                    "notice" => eprintln!("{}", reply["text"].as_str().unwrap_or("")),
                    "error" => eprintln!("error: {}", reply["text"].as_str().unwrap_or("")),
                    "output" => println!("{}", reply["text"].as_str().unwrap_or("")),
                    "unhandled" => match cmd {
                        "clear" => {
                            let _ = t.send(SessionCommand::NewSession).await;
                        }
                        "sessions" => {
                            let id = next_query;
                            next_query += 1;
                            let _ = t
                                .send(SessionCommand::Query {
                                    id,
                                    query: SessionQuery::Status,
                                })
                                .await;
                            let status = r.reply(&mut t, id).await;
                            let current = status["session"].as_str().unwrap_or("");
                            match synaps_cli::list_recent_sessions(20) {
                                Ok(sessions) => {
                                    for s in sessions.iter().take(20) {
                                        let marker = if s.id == current { "→ " } else { "  " };
                                        eprintln!(
                                            "{}{} {} ({}, ${:.4})",
                                            marker,
                                            &s.id[..8],
                                            s.title,
                                            s.model,
                                            s.session_cost
                                        );
                                    }
                                }
                                Err(e) => eprintln!("error: {}", e),
                            }
                        }
                        "status" => {
                            let id = next_query;
                            next_query += 1;
                            let _ = t
                                .send(SessionCommand::Query {
                                    id,
                                    query: SessionQuery::Status,
                                })
                                .await;
                            let st = r.reply(&mut t, id).await;
                            let id2 = next_query;
                            next_query += 1;
                            let _ = t
                                .send(SessionCommand::Query {
                                    id: id2,
                                    query: SessionQuery::ContextAssessment,
                                })
                                .await;
                            let ctx = r.reply(&mut t, id2).await;
                            let sid = st["session"].as_str().unwrap_or("");
                            eprintln!("session: {}", &sid[..8.min(sid.len())]);
                            eprintln!("model: {}", st["model"].as_str().unwrap_or(""));
                            eprintln!(
                                "tokens: {}↑ {}↓",
                                st["tokens"]["input"].as_u64().unwrap_or(0),
                                st["tokens"]["output"].as_u64().unwrap_or(0)
                            );
                            eprintln!("cost: ${:.4}", st["cost"].as_f64().unwrap_or(0.0));
                            eprintln!("messages: {}", st["messages"].as_u64().unwrap_or(0));
                            eprintln!(
                                "context: ~{} of {} budget tokens ({} window)",
                                ctx["used_tokens"].as_u64().unwrap_or(0),
                                ctx["budget_tokens"].as_u64().unwrap_or(0),
                                ctx["provider_window"].as_u64().unwrap_or(0)
                            );
                        }
                        "help" => {
                            eprintln!("commands: /model /thinking /compact /clear /sessions /status /quit");
                            eprintln!("quitting (or EOF) mid-turn cancels the turn and saves an abort context for the next --continue");
                        }
                        _ => eprintln!("unknown command: /{} (try /help)", cmd),
                    },
                    _ => {}
                }
                continue;
            }

            // Regular user message.
            r.idle = false;
            if t
                .send(SessionCommand::Submit {
                    text: trimmed.to_string(),
                    attachments: Vec::new(),
                })
                .await
                .is_err()
            {
                break;
            }
        }

        // ── Shutdown: End → actor saves, fires on_session_end, unregisters ──
        if !r.ended {
            let _ = t
                .send(SessionCommand::End {
                    reason: EndReason::ClientQuit,
                })
                .await;
            while !r.ended {
                match t.next_event().await {
                    Some(env) => r.on_event(&t, env).await,
                    None => break,
                }
            }
        }

        eprintln!(
            "session saved: {} (${:.4})",
            &session_id[..8.min(session_id.len())],
            r.cost
        );

        if let Some(err) = r.fatal {
            return Err(synaps_cli::RuntimeError::Session(format!(
                "turn failed: {} [{}]",
                err.message,
                err.category_label()
            )));
        }
        Ok(())
    }
}
