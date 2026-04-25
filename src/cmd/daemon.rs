//! `synaps daemon` — persistent headless agent that wakes on events.
//!
//! Boots with a system prompt, registers a session socket, sits idle.
//! When events arrive via socket or inbox, wakes up, runs a model turn
//! with full tool access, then goes back to sleep. Stays alive until killed.

use synaps_cli::{Runtime, StreamEvent, LlmEvent, SessionEvent};
use futures::StreamExt;
use serde_json::{json, Value};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio_util::sync::CancellationToken;

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
    max_history: usize,
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

    // Generate session ID and determine registry name
    let session_id = format!(
        "{}-{}",
        display_name,
        chrono::Utc::now().format("%Y%m%d-%H%M%S")
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

    // Signal handler
    let interrupted = Arc::new(AtomicBool::new(false));
    let int_flag = interrupted.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        int_flag.store(true, Ordering::Relaxed);
    });

    // Conversation history — persists across event batches
    let mut messages: Vec<Value> = Vec::new();

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

                // Run model turn(s) — agent may use tools, triggering follow-up turns
                let cancel = CancellationToken::new();
                let mut stream = runtime.run_stream_with_messages(
                    messages.clone(),
                    cancel,
                    None,
                ).await;

                while let Some(event) = stream.next().await {
                    match event {
                        StreamEvent::Llm(LlmEvent::Text(text)) => {
                            if !text.is_empty() {
                                print!("{}", text);
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
                            // Prune old messages to stay within context limits.
                            // Keep the most recent max_history messages.
                            if messages.len() > max_history {
                                let trim = messages.len() - max_history;
                                log(&format!("  pruned {} old messages (keeping {})", trim, max_history));
                                messages = messages.split_off(trim);
                            }
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

                println!(); // newline after response text
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
            }
        }
    }

    // Shutdown
    socket_shutdown.store(true, Ordering::Relaxed);
    socket_task.abort();
    inbox_shutdown.store(true, Ordering::Relaxed);
    inbox_task.abort();
    synaps_cli::events::registry::unregister_session(&session_id);

    log("daemon stopped.");
    Ok(())
}
