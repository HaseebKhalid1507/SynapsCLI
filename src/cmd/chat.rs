//! `synaps chat` — fully-featured headless mode.
//!
//! Same engine as the TUI (MCP, extensions, skills, session persistence,
//! compaction) but renders to stdin/stdout. Built for scripting,
//! piping, SSH, CI, and agent benchmark frameworks like Harbor.
//!
//! Note: the inbox watcher and session socket are started by `engine::setup::boot()`
//! but inbound events are not actively drained in this mode. Events will accumulate
//! until the session ends. Full event-queue handling is a TUI-only feature for now.

use synaps_cli::engine::setup::{self, EngineOpts};
use synaps_cli::engine::commands::{self, CommandResult};
use synaps_cli::engine::stream::{self, EngineStreamEvent, StreamCompletion, SubagentTracker};
use synaps_cli::engine::session::ConversationState;
use synaps_cli::{CancellationToken, flush_stdout};
use synaps_cli::runtime::compaction::compact_conversation;
use futures::StreamExt;
use serde_json::json;
use std::io::{self, Write, BufRead};

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
        profile,
        no_extensions,
    }).await?;

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

    // Extension discovery
    if !boot.no_extensions {
        let (loader_tx, mut loader_rx) = tokio::sync::mpsc::unbounded_channel();
        synaps_cli::extensions::loader::spawn_discover_and_load(
            std::sync::Arc::clone(&boot.ext_manager),
            loader_tx,
        );
        // Drain loader events in the background — prevents SendError crash
        tokio::spawn(async move {
            while loader_rx.recv().await.is_some() {}
        });
    }

    eprintln!("synaps {} | {} | session {}", 
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
    let stdin = io::stdin();
    let is_tty = std::io::IsTerminal::is_terminal(&std::io::stdin());
    let mut subagents: Vec<SubagentTracker> = Vec::new();
    let compact_threshold: usize = 80_000;
    let mut last_compacted_tokens: usize = 0;

    loop {
        if is_tty {
            eprint!("❯ ");
            io::stderr().flush().ok();
        }

        let mut input = String::new();
        match stdin.lock().read_line(&mut input) {
            Ok(0) => break, // EOF
            Err(e) => {
                eprintln!("input error: {}", e);
                break;
            }
            Ok(_) => {}
        }
        let input = input.trim();
        if input.is_empty() { continue; }

        // ── Slash commands ──
        if let Some((cmd, arg)) = commands::parse_command(input) {
            // Try engine-level command first
            if let Some(result) = commands::handle_engine_command(cmd, arg, &mut runtime) {
                match result {
                    CommandResult::Quit => break,
                    CommandResult::ModelChanged { model } => {
                        eprintln!("model → {}", model);
                    }
                    CommandResult::ThinkingChanged { level, budget } => {
                        eprintln!("thinking → {} ({})", level, budget);
                    }
                    CommandResult::Compact { custom_instructions } => {
                        eprintln!("compacting...");
                        if let Ok(summary) = compact_conversation(
                            &conv.api_messages, &runtime, custom_instructions.as_deref()
                        ).await {
                            conv.api_messages = vec![json!({
                                "role": "user",
                                "content": format!("<context-summary>\n{}\n</context-summary>", summary)
                            })];
                            last_compacted_tokens = conv.estimate_tokens();
                            eprintln!("compacted → ~{} tokens", last_compacted_tokens);
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
                "sessions" => {
                    match synaps_cli::list_recent_sessions(20) {
                        Ok(sessions) => {
                            for s in sessions.iter().take(20) {
                                let marker = if s.id == conv.session.id { "→ " } else { "  " };
                                eprintln!("{}{} {} ({}, ${:.4})", 
                                    marker, &s.id[..8], s.title, s.model, s.session_cost);
                            }
                        }
                        Err(e) => eprintln!("error: {}", e),
                    }
                }
                "status" => {
                    eprintln!("session: {}", &conv.session.id[..8]);
                    eprintln!("model: {}", runtime.model());
                    eprintln!("tokens: {}↑ {}↓", conv.total_input_tokens, conv.total_output_tokens);
                    eprintln!("cost: ${:.4}", conv.session_cost);
                    eprintln!("messages: {}", conv.api_messages.len());
                    eprintln!("est. tokens: ~{}", conv.estimate_tokens());
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

        // ── Send message ──
        // Inject abort context if present
        let message = if let Some(ctx) = conv.abort_context.take() {
            format!("{}\n\n[ABORT CONTEXT — your previous response was interrupted. Here's what you completed before the abort:]\n\n{}\n\n[END ABORT CONTEXT — continue from where you left off or adjust based on the user's new message]", input, ctx)
        } else {
            input.to_string()
        };

        conv.api_messages.push(json!({"role": "user", "content": message}));

        let cancel = CancellationToken::new();
        let mut stream = runtime.run_stream_with_messages(
            conv.api_messages.clone(), cancel, None, None, false
        ).await;

        let mut in_thinking = false;

        while let Some(event) = stream.next().await {
            let (engine_event, completion) = stream::process_stream_event(
                event,
                &mut conv.api_messages,
                &mut subagents,
                &mut conv.queued_message,
                &mut conv.pending_events,
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
                    if in_thinking { eprintln!("\x1b[0m"); in_thinking = false; }
                    eprint!("\x1b[33m⚙ {}\x1b[0m", tool_name);
                    io::stderr().flush().ok();
                }
                EngineStreamEvent::ToolFinalized { tool_name, input, .. } => {
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
                EngineStreamEvent::SubagentDone { status, duration_secs, .. } => {
                    eprintln!("\x1b[32m✔ {} ({:.1}s)\x1b[0m", status, duration_secs);
                }
                EngineStreamEvent::Usage { input_tokens, output_tokens, cache_read, cache_creation, cache_creation_5m, cache_creation_1h, model } => {
                    let model_name = model.as_deref().unwrap_or(runtime.model());
                    conv.add_usage(input_tokens, output_tokens, cache_read, cache_creation, cache_creation_5m, cache_creation_1h, model_name);
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
                _ => {} // deltas handled above
            }

            match completion {
                StreamCompletion::Done | StreamCompletion::Error(_) => {
                    if in_thinking { eprintln!("\x1b[0m"); }
                    println!();
                    break;
                }
                StreamCompletion::AutoSendQueued(queued) => {
                    // Re-inject and loop
                    conv.api_messages.push(json!({"role": "user", "content": queued}));
                    // Stream will continue from the outer loop
                    break;
                }
                StreamCompletion::AutoTriggerEvents => break,
                StreamCompletion::Continue => {}
            }
        }

        // Save after each turn
        conv.save().await;

        // Auto-compact
        let est = conv.estimate_tokens();
        let threshold = if last_compacted_tokens > 0 {
            last_compacted_tokens + compact_threshold
        } else {
            compact_threshold
        };
        if est > threshold && conv.api_messages.len() >= 4 {
            eprintln!("\x1b[2m[auto-compacting ~{} tokens...]\x1b[0m", est);
            if let Ok(summary) = compact_conversation(&conv.api_messages, &runtime, None).await {
                conv.api_messages = vec![json!({
                    "role": "user",
                    "content": format!("<context-summary>\n{}\n</context-summary>", summary)
                })];
                last_compacted_tokens = conv.estimate_tokens();
                eprintln!("\x1b[2m[compacted → ~{} tokens]\x1b[0m", last_compacted_tokens);
            }
        }
    }

    // ── Shutdown ──
    conv.save().await;

    // Fire on_session_end hook
    let hook_event = synaps_cli::extensions::hooks::events::HookEvent::on_session_end(
        &conv.session.id,
        None, // no transcript in headless
    );
    let _ = runtime.hook_bus().emit(&hook_event).await;

    boot.background.shutdown();
    eprintln!("session saved: {} (${:.4})", &conv.session.id[..8], conv.session_cost);
    Ok(())
}
