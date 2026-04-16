//! Chat TUI binary — event loop, terminal setup, module wiring.

mod theme;
mod highlight;
mod markdown;
mod app;
mod render;
mod gamba;
mod draw;
mod commands;
mod input;
mod stream_handler;
mod attach;

use app::{App, ChatMessage};
use draw::{draw, boot_effect, quit_effect};
use commands::CommandAction;
use input::InputAction;
use stream_handler::StreamAction;

use synaps_cli::{Runtime, Result, Session, latest_session, find_session};
use synaps_cli::transport::{Inbound, ConversationDriver, driver::DriverConfig};
use clap::Parser;
use crossterm::{
    event::{EventStream, EnableMouseCapture, DisableMouseCapture, EnableBracketedPaste, DisableBracketedPaste},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::StreamExt;
use ratatui::{
    backend::CrosstermBackend,
    Terminal,
};
use serde_json::{json, Value};
use std::io;
use std::time::Instant;
use tachyonfx::{Effect, Shader};


fn rebuild_display_messages(api_messages: &[Value], app: &mut App) {
    for msg in api_messages {
        match msg["role"].as_str() {
            Some("user") => {
                if let Some(content) = msg["content"].as_str() {
                    app.push_msg(ChatMessage::User(content.to_string()));
                }
            }
            Some("assistant") => {
                if let Some(content) = msg["content"].as_array() {
                    for block in content {
                        match block["type"].as_str() {
                            Some("thinking") => {
                                if let Some(text) = block["thinking"].as_str() {
                                    app.push_msg(ChatMessage::Thinking(text.to_string()));
                                }
                            }
                            Some("text") => {
                                if let Some(text) = block["text"].as_str() {
                                    app.push_msg(ChatMessage::Text(text.to_string()));
                                }
                            }
                            Some("tool_use") => {
                                let name = block["name"].as_str().unwrap_or("").to_string();
                                let input = serde_json::to_string(&block["input"]).unwrap_or_default();
                                app.push_msg(ChatMessage::ToolUse { tool_name: name, input });
                            }
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

#[derive(Parser)]
#[command(name = "chatui", about = "Terminal chat UI for SynapsCLI")]
struct Cli {
    /// Continue a previous session. Optionally provide a session ID (partial match supported).
    #[arg(long = "continue", value_name = "SESSION_ID")]
    continue_session: Option<Option<String>>,

    /// System prompt: a string or a path to a file.
    #[arg(long = "system", short = 's', value_name = "PROMPT_OR_FILE")]
    system: Option<String>,

    #[arg(long, global = true)]
    profile: Option<String>,

    /// Attach to a running watcher agent's event stream (full TUI).
    #[arg(long = "attach", value_name = "AGENT_NAME")]
    attach: Option<String>,

    /// Attach in read-only mode (observe only, no sending messages).
    #[arg(long = "readonly", requires = "attach")]
    readonly: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(ref prof) = cli.profile {
        synaps_cli::config::set_profile(Some(prof.clone()));
    }

    let _log_guard = synaps_cli::logging::init_logging();

    // ── Two modes: local driver OR attach to running agent ──
    let (mut event_rx, inbound_tx, mut app, _model_name, _thinking_level, system_prompt_path) = if let Some(ref agent_name) = cli.attach {
        // ── ATTACH MODE: connect to watcher agent ──
        let session = match attach::AttachSession::connect(agent_name, cli.readonly).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Attach failed: {}", e);
                std::process::exit(1);
            }
        };

        let model_name = session.sync_state.model.clone();
        let thinking_level = session.sync_state.thinking_level.clone();

        // Create a minimal session for the App (display only — no local save needed)
        let display_session = Session::new(&model_name, &thinking_level, None);
        let mut app = App::new(display_session);
        app.model_name = model_name.clone();
        app.thinking_level = thinking_level.clone();
        let agent_label = session.sync_state.agent_name.as_deref().unwrap_or(agent_name);
        app.push_msg(ChatMessage::System(format!("attached to agent '{}' ({})", agent_label, if cli.readonly { "read-only" } else { "interactive" })));
        if session.sync_state.is_streaming {
            app.streaming = true;
            app.push_msg(ChatMessage::System("agent is currently streaming...".into()));
        }

        let event_rx = session.event_rx;
        let inbound_tx = session.inbound_tx;

        // Keep session tasks alive by storing the session
        // (tokio tasks continue running even if JoinHandle is dropped,
        // but we hold it to be explicit about lifecycle)
        let _attach_session = (session._reader_task, session._writer_task);

        let system_prompt_path: Option<std::path::PathBuf> = None;
        (event_rx, inbound_tx, app, model_name, thinking_level, system_prompt_path)
    } else {
        // ── LOCAL MODE: create runtime + driver ──
        let mut runtime = Runtime::new().await?;

        // Load config and apply
        let config = synaps_cli::config::load_config();
        runtime.apply_config(&config);

        // Load system prompt
        let system_prompt = synaps_cli::config::resolve_system_prompt(cli.system.as_deref());

        // Auto-load skills specified in config (injected into system prompt)
        let mut final_prompt = system_prompt;
        if let Some(ref skill_names) = config.skills {
            let auto_skills = synaps_cli::skills::load_skills(Some(skill_names));
            if !auto_skills.is_empty() {
                let names: Vec<&str> = auto_skills.iter().map(|s| s.name.as_str()).collect();
                eprintln!("\x1b[2m  📚 {} skills auto-loaded: {}\x1b[0m", auto_skills.len(), names.join(", "));
                final_prompt.push_str(&synaps_cli::skills::format_skills_for_prompt(&auto_skills));
            }
        }
        runtime.set_system_prompt(final_prompt);

        // Register load_skill tool for on-demand activation of any skill
        let skill_count = synaps_cli::skills::setup_skill_tool(&runtime.tools_shared()).await;
        if skill_count > 0 {
            eprintln!("\x1b[2m  📚 {} skills available on demand (load_skill tool)\x1b[0m", skill_count);
        }

        // Set up lazy MCP loading (if configured in ~/.synaps-cli/mcp.json)
        let mcp_server_count = synaps_cli::mcp::setup_lazy_mcp(&runtime.tools_shared()).await;
        if mcp_server_count > 0 {
            eprintln!("\x1b[2m  ⚡ {} MCP servers available (use mcp_connect to activate)\x1b[0m", mcp_server_count);
        }

        let system_prompt_path = synaps_cli::config::resolve_read_path("system.md");

        // Capture display info before handing runtime to driver
        let model_name = runtime.model().to_string();
        let thinking_level = runtime.thinking_level().to_string();

        // Session: continue existing or create new
        let (app, session) = match cli.continue_session {
            Some(maybe_id) => {
                let session = match maybe_id {
                    Some(id) => find_session(&id).unwrap_or_else(|e| {
                        eprintln!("Failed to load session '{}': {}", id, e);
                        std::process::exit(1);
                    }),
                    None => latest_session().unwrap_or_else(|e| {
                        eprintln!("No sessions to continue: {}", e);
                        std::process::exit(1);
                    }),
                };
                runtime.set_model(session.model.clone());
                if let Some(ref sp) = session.system_prompt {
                    runtime.set_system_prompt(sp.clone());
                }
                let mut app = App::new(session.clone());
                app.api_messages = session.api_messages.clone();
                app.total_input_tokens = session.total_input_tokens;
                app.total_output_tokens = session.total_output_tokens;
                app.session_cost = session.session_cost;
                app.abort_context = session.abort_context.clone();
                app.model_name = session.model.clone();
                app.thinking_level = runtime.thinking_level().to_string();
                rebuild_display_messages(&session.api_messages, &mut app);
                app.push_msg(ChatMessage::System(format!("resumed session {}", session.id)));
                if app.abort_context.is_some() {
                    app.push_msg(ChatMessage::System("⚠ abort context from previous session will be injected into next message".to_string()));
                }
                (app, session)
            }
            None => {
                let session = Session::new(&model_name, &thinking_level, runtime.system_prompt());
                let app = App::new(session.clone());
                (app, session)
            }
        };

        // ── Create driver and bus ──
        let driver_config = DriverConfig {
            agent_name: None,
            auto_save: true,
            event_buffer_size: 100,
        };
        let mut driver = ConversationDriver::new(runtime, session, driver_config);

        // Get bus handle before spawning driver
        let bus_handle = driver.bus().handle();
        let event_rx = bus_handle.subscribe();
        let inbound_tx = bus_handle.inbound();

        // Spawn driver in background
        tokio::spawn(async move {
            if let Err(e) = driver.run().await {
                tracing::error!("Driver error: {}", e);
            }
        });

        (event_rx, inbound_tx, app, model_name, thinking_level, Some(system_prompt_path))
    };

    // ── Terminal setup ──
    enable_raw_mode().map_err(|e| synaps_cli::error::RuntimeError::Tool(format!("terminal setup failed: {}", e)))?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture, EnableBracketedPaste)
        .map_err(|e| synaps_cli::error::RuntimeError::Tool(format!("terminal setup failed: {}", e)))?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)
        .map_err(|e| synaps_cli::error::RuntimeError::Tool(format!("terminal setup failed: {}", e)))?;
    let mut event_reader = EventStream::new();
    let mut boot_fx: Option<Effect> = Some(boot_effect());
    let mut exit_fx: Option<Effect> = None;
    let mut last_frame = Instant::now();

    // ── Event loop ──
    loop {
        let elapsed = last_frame.elapsed();
        last_frame = Instant::now();
        { let m = app.model_name.clone(); let t = app.thinking_level.clone(); let _ = draw(&mut terminal, &mut app, &m, &t, &mut boot_fx, &mut exit_fx, elapsed); }

        tokio::select! {
            // ── Tick: animations + spinner (~60fps) ──
            _ = tokio::time::sleep(std::time::Duration::from_millis(16)), if boot_fx.is_some() || exit_fx.is_some() || app.streaming || app.messages.is_empty() || app.logo_dismiss_t.is_some() || app.logo_build_t.is_some() || app.gamba_child.is_some() => {
                if let Some(ref mut t) = app.logo_build_t {
                    *t += 0.025;
                    if *t >= 1.0 { app.logo_build_t = None; }
                }
                if let Some(ref mut t) = app.logo_dismiss_t {
                    *t += 0.04;
                    if *t >= 1.0 { app.logo_dismiss_t = None; }
                }
                if !app.subagents.is_empty() || app.streaming {
                    app.spinner_frame = app.spinner_frame.wrapping_add(1);
                    if app.spinner_frame % 3 == 0 {
                        app.dirty = true;
                        app.line_cache.clear();
                    }
                }
                if let Some(msg) = app.check_gamba_exited() {
                    terminal.clear().ok();
                    app.push_msg(ChatMessage::System(msg));
                    app.dirty = true;
                    app.line_cache.clear();
                    let elapsed = last_frame.elapsed();
                    last_frame = Instant::now();
                    { let m = app.model_name.clone(); let t = app.thinking_level.clone(); let _ = draw(&mut terminal, &mut app, &m, &t, &mut boot_fx, &mut exit_fx, elapsed); }
                }
                if exit_fx.as_ref().is_some_and(|fx| fx.done()) {
                    break;
                }
                continue;
            }

            // ── Input: keyboard, mouse, paste ──
            maybe_event = event_reader.next(), if app.gamba_child.is_none() => {
                match maybe_event {
                    Some(Ok(event)) => {
                        let is_streaming = app.streaming;
                        let action = input::handle_event(event, &mut app, is_streaming);
                        match action {
                            InputAction::None => {}
                            InputAction::Quit => {
                                exit_fx = Some(quit_effect());
                            }
                            InputAction::Abort => {
                                let _ = inbound_tx.send(Inbound::Cancel);
                                app.capture_abort_context();
                                if let Some(ref q) = app.queued_message.take() {
                                    app.push_msg(ChatMessage::System(format!("dequeued: {}", q)));
                                }
                                app.streaming = false;
                                app.subagents.clear();
                                let abort_msg = if app.abort_context.is_some() {
                                    "aborted — context saved for next message"
                                } else {
                                    "aborted"
                                };
                                app.push_msg(ChatMessage::Error(abort_msg.to_string()));
                                app.save_session().await;
                            }
                            InputAction::SlashCommand(cmd, arg) => {
                                match commands::handle_command(&cmd, &arg, &mut app, &inbound_tx, system_prompt_path.as_ref()).await {
                                    CommandAction::None => {}
                                    CommandAction::StartStream => {} // reserved for future use
                                    CommandAction::Quit => {
                                        exit_fx = Some(quit_effect());
                                    }
                                    CommandAction::LaunchGamba => {
                                        drop(event_reader);
                                        match app.launch_gamba() {
                                            Ok(()) => {}
                                            Err(msg) => {
                                                terminal.clear().ok();
                                                app.push_msg(ChatMessage::Error(msg));
                                            }
                                        }
                                        event_reader = EventStream::new();
                                    }
                                }
                            }
                            InputAction::Submit(input) => {
                                // Build display text with paste info
                                let display_text = if app.pasted_char_count > 0 {
                                    let typed = app.input_before_paste.as_deref().unwrap_or("");
                                    let typed_char_count = typed.chars().count();
                                    let paste_byte_start = input.char_indices()
                                        .nth(typed_char_count)
                                        .map(|(i, _)| i)
                                        .unwrap_or(input.len());
                                    let paste_content = &input[paste_byte_start..];
                                    let line_count = paste_content.lines().count();
                                    let pasted_char_count = input.chars().count().saturating_sub(typed_char_count);
                                    let paste_label = if line_count > 1 {
                                        format!("[Pasted {} lines]", line_count)
                                    } else {
                                        format!("[Pasted {} chars]", pasted_char_count)
                                    };
                                    if typed.is_empty() { paste_label } else { format!("{} {}", typed.trim(), paste_label) }
                                } else {
                                    input.clone()
                                };
                                app.push_msg(ChatMessage::User(display_text));
                                app.input_before_paste = None;
                                app.pasted_char_count = 0;
                                // Inject abort context if previous response was interrupted
                                let api_content = if let Some(ref ctx) = app.abort_context {
                                    let combined = format!("{}\n\n{}", ctx, input);
                                    app.abort_context = None;
                                    combined
                                } else {
                                    input
                                };
                                // Keep local mirror
                                app.api_messages.push(json!({"role": "user", "content": api_content.clone()}));
                                // Send to driver via bus
                                let _ = inbound_tx.send(Inbound::Message { content: api_content });
                                app.status_text = Some("connecting…".to_string());
                                app.streaming = true;
                                app.spinner_frame = 0;
                                let elapsed = last_frame.elapsed();
                                last_frame = Instant::now();
                                { let m = app.model_name.clone(); let t = app.thinking_level.clone(); let _ = draw(&mut terminal, &mut app, &m, &t, &mut boot_fx, &mut exit_fx, elapsed); }
                                app.status_text = None;
                                app.push_msg(ChatMessage::Thinking("…".to_string()));
                            }
                            InputAction::StreamingInput(input) => {
                                // Check for streaming slash commands
                                if let Some(rest) = input.strip_prefix('/') {
                                    let raw_cmd = rest.split_whitespace().next().unwrap_or("");
                                    let cmd = commands::resolve_prefix(raw_cmd, commands::STREAMING_COMMANDS);
                                    match commands::handle_streaming_command(&cmd, &input, &mut app) {
                                        CommandAction::None => {
                                            // Unknown streaming command — steer/queue
                                            let _ = inbound_tx.send(Inbound::Steer { content: input.clone() });
                                            app.push_msg(ChatMessage::System(format!("→ steering: {}", input)));
                                            app.queued_message = Some(input);
                                        }
                                        CommandAction::Quit => {
                                            exit_fx = Some(quit_effect());
                                        }
                                        CommandAction::LaunchGamba => {
                                            drop(event_reader);
                                            match app.launch_gamba() {
                                                Ok(()) => {}
                                                Err(msg) => {
                                                    terminal.clear().ok();
                                                    app.push_msg(ChatMessage::Error(msg));
                                                }
                                            }
                                            event_reader = EventStream::new();
                                        }
                                        CommandAction::StartStream => {}
                                    }
                                } else {
                                    // Normal text during streaming — steer/queue
                                    let _ = inbound_tx.send(Inbound::Steer { content: input.clone() });
                                    app.push_msg(ChatMessage::System(format!("→ steering: {}", input)));
                                    app.queued_message = Some(input);
                                }
                            }
                        }
                    }
                    Some(Err(_)) | None => break,
                }
            }

            // ── Agent events from bus ──
            event = event_rx.recv() => {
                match event {
                    Ok(event) => {
                        let do_draw = stream_handler::needs_immediate_draw(&event);
                        let action = stream_handler::handle_agent_event(event, &mut app);

                        match action {
                            StreamAction::Continue => {
                                // For Done/Error, clear stream state
                                if !app.streaming {
                                    // Reclaim gamba if running
                                    if let Some(msg) = app.reclaim_gamba() {
                                        terminal.clear().ok();
                                        app.push_msg(ChatMessage::System(msg));
                                        app.dirty = true;
                                        app.line_cache.clear();
                                        let elapsed = last_frame.elapsed();
                                        last_frame = Instant::now();
                                        { let m = app.model_name.clone(); let t = app.thinking_level.clone(); let _ = draw(&mut terminal, &mut app, &m, &t, &mut boot_fx, &mut exit_fx, elapsed); }
                                    }
                                }
                            }
                            StreamAction::AutoSendQueued(queued) => {
                                // Reclaim gamba if running
                                if let Some(msg) = app.reclaim_gamba() {
                                    terminal.clear().ok();
                                    app.push_msg(ChatMessage::System(msg));
                                    app.dirty = true;
                                    app.line_cache.clear();
                                    let elapsed = last_frame.elapsed();
                                    last_frame = Instant::now();
                                    { let m = app.model_name.clone(); let t = app.thinking_level.clone(); let _ = draw(&mut terminal, &mut app, &m, &t, &mut boot_fx, &mut exit_fx, elapsed); }
                                }
                                // Auto-send the queued message
                                app.push_msg(ChatMessage::User(queued.clone()));
                                app.scroll_back = 0;
                                app.scroll_pinned = true;
                                let api_content = if let Some(ref ctx) = app.abort_context {
                                    let combined = format!("{}\n\n{}", ctx, queued);
                                    app.abort_context = None;
                                    combined
                                } else {
                                    queued
                                };
                                app.api_messages.push(json!({"role": "user", "content": api_content.clone()}));
                                let _ = inbound_tx.send(Inbound::Message { content: api_content });
                                app.status_text = Some("connecting…".to_string());
                                app.streaming = true;
                                app.spinner_frame = 0;
                                let elapsed = last_frame.elapsed();
                                last_frame = Instant::now();
                                { let m = app.model_name.clone(); let t = app.thinking_level.clone(); let _ = draw(&mut terminal, &mut app, &m, &t, &mut boot_fx, &mut exit_fx, elapsed); }
                                app.status_text = None;
                                app.push_msg(ChatMessage::Thinking("…".to_string()));
                            }
                        }

                        if do_draw {
                            let elapsed = last_frame.elapsed();
                            last_frame = Instant::now();
                            { let m = app.model_name.clone(); let t = app.thinking_level.clone(); let _ = draw(&mut terminal, &mut app, &m, &t, &mut boot_fx, &mut exit_fx, elapsed); }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("TUI lagged, missed {} events", n);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        // In attach mode this means the socket connection was lost
                        app.push_msg(ChatMessage::Error("connection lost — agent disconnected".to_string()));
                        app.streaming = false;
                        let elapsed = last_frame.elapsed();
                        last_frame = Instant::now();
                        { let m = app.model_name.clone(); let t = app.thinking_level.clone(); let _ = draw(&mut terminal, &mut app, &m, &t, &mut boot_fx, &mut exit_fx, elapsed); }
                        // Wait for user to quit manually instead of vanishing
                        if cli.attach.is_some() {
                            loop {
                                if let Some(Ok(crossterm::event::Event::Key(key))) = event_reader.next().await {
                                    if matches!(key.code, crossterm::event::KeyCode::Char('q') | crossterm::event::KeyCode::Esc) {
                                        break;
                                    }
                                }
                            }
                        }
                        break;
                    }
                }
            }
        }
    }

    // Save session on exit
    app.save_session().await;

    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), DisableBracketedPaste, DisableMouseCapture, LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();

    Ok(())
}
