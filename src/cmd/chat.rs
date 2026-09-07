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

use agent_engine::attachments::{load_attachment, PendingAttachments};
use futures::StreamExt;
use serde_json::json;
use std::io::{self, Write};
use synaps_cli::engine::commands::{self, CommandResult};
use synaps_cli::engine::reactor::{
    claim_auto_turn, drain_event_queue, wake_action, WakeAction, AUTO_TURN_CAP,
};
use synaps_cli::engine::session::ConversationState;
use synaps_cli::engine::setup::{self, EngineOpts};
use synaps_cli::engine::stream::{self, EngineStreamEvent, StreamCompletion, SubagentTracker};
use synaps_cli::runtime::compaction::{
    apply_compaction, compact_conversation, preview_compaction_disclosure, CompactionPolicy,
    CompactionTransition,
};
use synaps_cli::skills::registry::{attachment_path_argument, ATTACHMENT_DISCLOSURE};
use synaps_cli::{flush_stdout, CancellationToken, SessionEvent, StreamEvent};
use tokio::io::{AsyncBufReadExt, BufReader as TokioBufReader};

/// What was read while waiting at the prompt.
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

/// Only called from the idle stdin branch, never an event-triggered turn or
/// the string-only follow-up queue. Keep captured bytes on any rejection.
fn append_user_submission(
    conv: &mut ConversationState,
    pending: &mut PendingAttachments,
    model: &str,
    text: &str,
) -> Result<(), String> {
    if conv.context_head.is_blocked(&conv.session) {
        return Err("context head is unverified — restart/resume before continuing".into());
    }
    let text = if let Some(ctx) = &conv.abort_context {
        format!("{}\n\n[ABORT CONTEXT — your previous response was interrupted. Here's what you completed before the abort:]\n\n{}\n\n[END ABORT CONTEXT — continue from where you left off or adjust based on the user's new message]", text, ctx)
    } else {
        text.to_string()
    };
    let content = pending.build_content(&text);
    let message = std::sync::Arc::new(json!({"role": "user", "content": content}));
    if !pending.is_empty() {
        let mut proposed = conv.api_messages.clone();
        proposed.push(message.clone());
        synaps_cli::runtime::attachments::validate_messages(model, &proposed)?;
    }
    conv.api_messages.push(message);
    pending.clear();
    conv.abort_context = None;
    Ok(())
}

fn list_pending_attachments(pending: &PendingAttachments) {
    if pending.is_empty() {
        eprintln!("no pending attachments — /attach PATH to stage a file");
    } else {
        eprintln!(
            "Pending attachments ({}):\n{}\n{}",
            pending.len(),
            pending.summaries().join("\n"),
            ATTACHMENT_DISCLOSURE
        );
        eprintln!("Send with the next normal input (a blank line also sends); /detach to clear.");
    }
}

pub async fn run(
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
    let mut pending_attachments = PendingAttachments::default();
    // Piped input must not silently submit subsequent text after a failed attach.
    let mut input_failure: Option<String> = None;

    // C4a: consecutive auto-turn counter; reset to 0 on real user input.
    // Initial value doesn't matter — always reset before first turn.
    #[allow(unused_assignments)]
    let mut consecutive_auto_turns: u32 = 0;

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
                    let action = wake_action(
                        &drained,
                        &conv.api_messages,
                        false,
                        !conv.context_head.is_blocked(&conv.session),
                        consecutive_auto_turns,
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
                let _ = claim_auto_turn(&mut consecutive_auto_turns);
            }

            PromptRead::Line(raw_line) => {
                let trimmed = raw_line.trim_end_matches('\r').trim();

                if trimmed.is_empty() && pending_attachments.is_empty() {
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
                        "attach" => {
                            // Stdin is only consumed while idle. Streaming and
                            // compaction complete before returning to this loop.
                            let result = match attachment_path_argument(arg) {
                                Ok(path) => match load_attachment(path).await {
                                    Ok(attachment) => pending_attachments.add(attachment),
                                    Err(error) => Err(error),
                                },
                                Err(error) => Err(error),
                            };
                            match result {
                                Ok(()) => list_pending_attachments(&pending_attachments),
                                Err(error) => {
                                    eprintln!("attachment not staged: {error}");
                                    if !is_tty {
                                        input_failure = Some(error);
                                        break;
                                    }
                                }
                            }
                        }
                        "attachments" => {
                            if matches!(arg, "" | "list") {
                                list_pending_attachments(&pending_attachments);
                            } else {
                                eprintln!("usage: /attachments [list]");
                            }
                        }
                        "detach" => {
                            if matches!(arg, "" | "clear") {
                                let count = pending_attachments.len();
                                pending_attachments.clear();
                                eprintln!("cleared {count} pending attachment(s) (already-submitted history is unchanged)");
                            } else {
                                eprintln!("usage: /detach [clear]");
                            }
                        }
                        "clear" => {
                            conv.clear(&runtime).await;
                            pending_attachments.clear();
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
                            eprintln!("commands: /model /thinking /compact /clear /sessions /status /attach PATH /attachments [list] /detach [clear] /quit");
                            eprintln!("{ATTACHMENT_DISCLOSURE}");
                        }
                        _ => {
                            eprintln!("unknown command: /{} (try /help)", cmd);
                        }
                    }
                    continue;
                }

                // ── Regular user message (including attachment-only blank line) ──
                if let Err(error) = append_user_submission(
                    &mut conv,
                    &mut pending_attachments,
                    runtime.model(),
                    trimmed,
                ) {
                    eprintln!("message not submitted: {error}; pending attachments retained");
                    if !is_tty {
                        input_failure = Some(error);
                        break;
                    }
                    continue;
                }
            }
        }

        // ── C4a: turn loop — run until no pending events or cap reached ──
        'turn_loop: loop {
            if conv.context_head.is_blocked(&conv.session) {
                eprintln!("context head is unverified — restart/resume before continuing");
                break 'turn_loop;
            }
            let cancel = CancellationToken::new();
            // Vec<SharedMessage> clone = pointer bumps only.
            let msgs_in: Vec<synaps_cli::SharedMessage> = conv.api_messages.clone();
            // Failure repair may only remove messages appended by this turn.
            let mut turn_baseline = msgs_in.len();
            let mut stream = runtime
                .run_stream_with_messages(msgs_in, cancel, None, None, false)
                .await;

            let mut in_thinking = false;

            let turn_completion = loop {
                let Some(event) = stream.next().await else {
                    break StreamCompletion::Done;
                };
                if let StreamEvent::Session(SessionEvent::ContextHeadCheckpoint {
                    session_id,
                    messages,
                    receipt,
                }) = event
                {
                    let result = conv.persist_context_head(&session_id, messages).await;
                    turn_baseline = conv.api_messages.len();
                    receipt.complete(result);
                    continue;
                }
                // A failed publication is not a rollback. Ignore stale history
                // and disable failure repair; still consume the typed Error.
                if conv.context_head.is_blocked(&conv.session) {
                    if matches!(event, StreamEvent::Session(SessionEvent::MessageHistory(_))) {
                        continue;
                    }
                    turn_baseline = conv.api_messages.len();
                }
                let (engine_event, completion) = stream::process_stream_event(
                    event,
                    &mut conv.api_messages,
                    &mut subagents,
                    &mut conv.queued_message,
                    &mut conv.pending_events,
                    turn_baseline,
                );

                match engine_event {
                    EngineStreamEvent::ResponseStart => {}
                    EngineStreamEvent::ResponseReset => {
                        eprintln!("\x1b[0m\n[Interrupted response discarded; retrying — tools were not executed]");
                        in_thinking = false;
                    }
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
            if !conv.context_head.is_blocked(&conv.session)
                && !matches!(turn_completion, StreamCompletion::Error(_))
                && assessment.should_compact()
                && !runtime.context_management_enabled()
            {
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
                    let action = wake_action(
                        &drained,
                        &conv.api_messages,
                        false,
                        !conv.context_head.is_blocked(&conv.session),
                        consecutive_auto_turns,
                    );
                    match action {
                        WakeAction::RunTurn => {
                            if claim_auto_turn(&mut consecutive_auto_turns) {
                                continue 'turn_loop;
                            } else {
                                // claim denied: counter was already at cap.
                                // fall through to park (treated as Forward).
                                eprintln!(
                                    "\x1b[2m[auto-turn cap ({}) reached — waiting for user input]\x1b[0m",
                                    AUTO_TURN_CAP
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
            if !is_tty || conv.context_head.is_blocked(&conv.session) {
                break;
            }
            fatal_failure = None;
        }
    }

    // ── Shutdown ──
    if !pending_attachments.is_empty() {
        eprintln!(
            "{} pending attachment(s) were not submitted or saved",
            pending_attachments.len()
        );
    }
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
    if let Some(error) = input_failure {
        return Err(synaps_cli::RuntimeError::Session(format!(
            "attachment submission failed: {error}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod attachment_tests {
    use super::*;

    #[tokio::test]
    async fn rejected_attachment_submission_retains_bytes_and_abort_context() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.txt");
        std::fs::write(&path, "captured notes").unwrap();
        let mut pending = PendingAttachments::default();
        pending.add(load_attachment(&path).await.unwrap()).unwrap();
        std::fs::remove_file(&path).unwrap();
        let mut conv = ConversationState::new(synaps_cli::Session::new(
            "claude-sonnet-4-5",
            "medium",
            None,
        ));
        conv.abort_context = Some("interrupted".into());
        assert!(
            append_user_submission(&mut conv, &mut pending, "google-gemini/test", "question")
                .is_err()
        );
        assert!(conv.api_messages.is_empty());
        assert_eq!(pending.len(), 1);
        assert_eq!(conv.abort_context.as_deref(), Some("interrupted"));
        conv.api_messages.push(std::sync::Arc::new(json!({
            "role": "user", "content": [{"type": "image", "source": {"type": "url", "url": "https://invalid.test/image"}}]
        })));
        assert!(
            append_user_submission(&mut conv, &mut pending, "claude-sonnet-4-5", "question")
                .is_err()
        );
        assert_eq!(conv.api_messages.len(), 1);
        assert_eq!(pending.len(), 1);
        conv.api_messages.clear();
        conv.abort_context = None;
        append_user_submission(&mut conv, &mut pending, "claude-sonnet-4-5", "").unwrap();
        assert_eq!(
            conv.api_messages[0]["content"][0]["source"]["data"],
            "captured notes"
        );
        assert!(pending.is_empty());
        append_user_submission(&mut conv, &mut pending, "test-model", "hello").unwrap();
        assert_eq!(conv.api_messages[1]["content"], "hello");
    }
}
