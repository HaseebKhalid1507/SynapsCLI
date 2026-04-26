//! `synaps daemon` — persistent headless agent that wakes on events.
//!
//! Boots with a system prompt, registers a session socket, sits idle.
//! When events arrive via socket or inbox, wakes up, runs a model turn
//! with full tool access, then goes back to sleep. Stays alive until killed.

use synaps_cli::{Runtime, StreamEvent, LlmEvent, SessionEvent};
use synaps_cli::core::compaction::compact_conversation;
use futures::StreamExt;
use serde_json::{json, Value};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio_util::sync::CancellationToken;

/// Estimate token count for a message array.
/// Uses chars/4 heuristic — good enough for triggering compaction thresholds.
fn estimate_tokens(messages: &[Value]) -> usize {
    let mut total_chars = 0usize;
    for msg in messages {
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
                // tool_result content — can be string or array of text blocks (MCP)
                if let Some(s) = block["content"].as_str() {
                    total_chars += s.len();
                } else if let Some(content_arr) = block["content"].as_array() {
                    for inner in content_arr {
                        if let Some(s) = inner["text"].as_str() {
                            total_chars += s.len();
                        }
                    }
                }
                // tool_use inputs
                if let Some(input) = block.get("input") {
                    total_chars += input.to_string().len();
                }
            }
        }
    }
    total_chars / 4
}

fn load_agent_prompt(name: &str) -> std::result::Result<String, String> {
    synaps_cli::tools::resolve_agent_prompt(name)
}

fn log(msg: &str) {
    let ts = chrono::Local::now().format("%H:%M:%S");
    eprintln!("[{}] {}", ts, msg);
}

pub async fn run(
    agent: Option<String>,
    system: Option<String>,
    name: Option<String>,
    model: Option<String>,
    thinking: Option<String>,
    compact_at: usize,
) -> synaps_cli::Result<()> {
    let _log_guard = synaps_cli::logging::init_logging();
    let mut runtime = Runtime::new().await?;

    // Load system prompt
    let display_name = if let Some(ref agent_name) = agent {
        match load_agent_prompt(agent_name) {
            Ok(p) => {
                runtime.set_system_prompt(p);
                agent_name.clone()
            }
            Err(e) => {
                eprintln!("❌ {}", e);
                std::process::exit(1);
            }
        }
    } else if let Some(ref val) = system {
        let prompt = synaps_cli::config::resolve_system_prompt(Some(val));
        runtime.set_system_prompt(prompt);
        "daemon".to_string()
    } else {
        eprintln!("❌ Either --agent or --system is required.");
        std::process::exit(1);
    };

    if let Some(ref m) = model {
        runtime.set_model(m.clone());
    }
    if let Some(ref t) = thinking {
        let budget = match t.as_str() {
            "low" => 2048,
            "medium" => 4096,
            "high" => 16384,
            "xhigh" => 32768,
            other => other.parse::<u32>().unwrap_or(4096),
        };
        runtime.set_thinking_budget(budget);
    }

    // Generate session ID — includes PID for collision resistance when
    // two daemons with the same agent name start in the same second.
    let session_id = format!(
        "{}-{}-{}",
        display_name,
        chrono::Utc::now().format("%Y%m%d-%H%M%S"),
        std::process::id()
    );
    let session_name = name.or_else(|| Some(display_name.clone()));

    log(&format!("booting daemon [{}] (model: {})", display_name, runtime.model()));

    // Register socket + session registry
    let socket_shutdown = Arc::new(AtomicBool::new(false));
    let socket_path = synaps_cli::events::registry::socket_path_for_session(&session_id);
    let socket_task = synaps_cli::events::socket::listen_session_socket(
        socket_path.clone(),
        runtime.event_queue().clone(),
        socket_shutdown.clone(),
    );

    let registration = synaps_cli::events::registry::SessionRegistration {
        session_id: session_id.clone(),
        name: session_name.clone(),
        socket_path: socket_path.clone(),
        pid: std::process::id(),
        started_at: chrono::Utc::now(),
    };
    if let Err(e) = synaps_cli::events::registry::register_session(&registration) {
        log(&format!("WARNING: failed to register session: {}", e));
    }

    // Start inbox watcher (fallback)
    let inbox_shutdown = Arc::new(AtomicBool::new(false));
    let inbox_task = {
        let inbox_dir = synaps_cli::config::base_dir().join("inbox");
        let eq = runtime.event_queue().clone();
        let sd = inbox_shutdown.clone();
        tokio::spawn(async move {
            synaps_cli::events::watch_inbox(inbox_dir, eq, sd).await;
        })
    };

    // Signal handlers — catch both SIGINT (ctrl-c) and SIGTERM (systemd, docker stop)
    let interrupted = Arc::new(AtomicBool::new(false));
    {
        let flag = interrupted.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            flag.store(true, Ordering::Relaxed);
        });
    }
    #[cfg(unix)]
    {
        let flag = interrupted.clone();
        tokio::spawn(async move {
            let mut sigterm = tokio::signal::unix::signal(
                tokio::signal::unix::SignalKind::terminate(),
            ).expect("failed to register SIGTERM handler");
            sigterm.recv().await;
            flag.store(true, Ordering::Relaxed);
        });
        let flag = interrupted.clone();
        tokio::spawn(async move {
            let mut sighup = tokio::signal::unix::signal(
                tokio::signal::unix::SignalKind::hangup(),
            ).expect("failed to register SIGHUP handler");
            sighup.recv().await;
            flag.store(true, Ordering::Relaxed);
        });
    }

    // Conversation history — persists across event batches
    let mut messages: Vec<Value> = Vec::new();
    // Token count after last compaction — prevents thrashing by requiring
    // growth beyond the post-compaction baseline before re-triggering.
    let mut last_compacted_tokens: usize = 0;

    log(&format!(
        "ready — listening on {} (name: {})",
        socket_path,
        session_name.as_deref().unwrap_or("none")
    ));

    // Event loop — idle until events arrive
    loop {
        tokio::select! {
            _ = runtime.event_queue().notified() => {
                // Drain all queued events into user messages
                let mut event_count = 0;
                while let Some(event) = runtime.event_queue().pop() {
                    event_count += 1;
                    let formatted = synaps_cli::events::format_event_for_agent(&event);
                    log(&format!(
                        "event [{}/{}]: {}",
                        event.source.source_type,
                        event.content.severity.as_ref().map(|s| s.as_str()).unwrap_or("medium"),
                        &event.content.text
                    ));
                    messages.push(json!({
                        "role": "user",
                        "content": formatted
                    }));
                }

                if event_count == 0 {
                    continue;
                }

                log(&format!("processing {} event(s)...", event_count));

                // Run model turn(s) — agent may use tools, triggering follow-up turns.
                // mem::take avoids cloning the full history; MessageHistory restores it.
                let cancel = CancellationToken::new();
                let mut stream = runtime.run_stream_with_messages(
                    std::mem::take(&mut messages),
                    cancel,
                    None,
                ).await;

                while let Some(event) = stream.next().await {
                    match event {
                        StreamEvent::Llm(LlmEvent::Text(text)) => {
                            if !text.is_empty() {
                                // Daemon is headless — response text goes to stderr
                                // alongside structured logs. stdout may be piped/buffered.
                                eprint!("{}", text);
                            }
                        }
                        StreamEvent::Llm(LlmEvent::ToolUseStart(name)) => {
                            log(&format!("  tool: {}", name));
                        }
                        StreamEvent::Llm(LlmEvent::ToolResult { result, .. }) => {
                            let preview: String = result.chars().take(100).collect();
                            log(&format!("  result: {}", preview));
                        }
                        StreamEvent::Session(SessionEvent::Usage {
                            input_tokens,
                            output_tokens,
                            ..
                        }) => {
                            log(&format!("  tokens: +{}↑ +{}↓", input_tokens, output_tokens));
                        }
                        StreamEvent::Session(SessionEvent::MessageHistory(history)) => {
                            messages = history;
                        }
                        StreamEvent::Session(SessionEvent::Done) => {
                            break;
                        }
                        StreamEvent::Session(SessionEvent::Error(e)) => {
                            log(&format!("  ERROR: {}", e));
                            break;
                        }
                        _ => {}
                    }
                }

                eprintln!(); // newline after response text

                // Auto-compact when token estimate exceeds threshold.
                // Hysteresis: after compaction, don't re-trigger until tokens have
                // grown past compact_at from the post-compaction baseline. This
                // prevents thrashing where a busy agent compacts every single turn.
                let est = estimate_tokens(&messages);
                let effective_threshold = if last_compacted_tokens > 0 {
                    last_compacted_tokens + compact_at
                } else {
                    compact_at
                };
                // Guard: need at least 4 messages (2 full turns) for a meaningful summary.
                if est > effective_threshold && messages.len() >= 4 {
                    log(&format!(
                        "token estimate {} exceeds threshold {} — compacting {} messages...",
                        est, effective_threshold, messages.len()
                    ));
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(90),
                        compact_conversation(&messages, &runtime, None),
                    ).await {
                        Ok(Ok(summary)) => {
                            let summary_tokens = summary.len() / 4;
                            log(&format!(
                                "compacted ~{} tokens → ~{} token summary",
                                est, summary_tokens
                            ));
                            messages = vec![json!({
                                "role": "user",
                                "content": format!(
                                    "<context-summary>\n{}\n</context-summary>",
                                    summary
                                )
                            })];
                            last_compacted_tokens = estimate_tokens(&messages);
                        }
                        Ok(Err(e)) => {
                            log(&format!("compaction failed: {} — continuing with full history", e));
                        }
                        Err(_) => {
                            log("compaction timed out (90s) — continuing with full history");
                        }
                    }
                }

                log("idle — waiting for events...");

                // Check for events that arrived during the model turn.
                // notified() may have fired while we were streaming — those
                // events are in the queue but nobody polled select! to see them.
                if !runtime.event_queue().is_empty() {
                    continue;
                }
            }

            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                if interrupted.load(Ordering::Relaxed) {
                    log("interrupted — shutting down");
                    break;
                }
                // Catch events whose Notify was lost during a model turn
                if !runtime.event_queue().is_empty() {
                    continue;
                }
            }
        }
    }

    // Shutdown — signal tasks to stop, then await them for clean cleanup
    // (socket task removes the socket file on exit; aborting races that).
    socket_shutdown.store(true, Ordering::Relaxed);
    inbox_shutdown.store(true, Ordering::Relaxed);
    let _ = tokio::join!(socket_task, inbox_task);
    synaps_cli::events::registry::unregister_session(&session_id);

    log("daemon stopped.");
    Ok(())
}
