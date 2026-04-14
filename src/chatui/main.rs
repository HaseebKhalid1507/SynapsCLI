mod theme;
mod highlight;
mod markdown;
mod app;
mod render;
mod gamba;
mod draw;

use app::{App, ChatMessage, SubagentState};
use draw::{draw, boot_effect, quit_effect};

use synaps_cli::{Runtime, StreamEvent, Result, CancellationToken, Session, list_sessions, latest_session, find_session};
use clap::Parser;
use crossterm::{
    event::{Event, KeyCode, KeyModifiers, MouseEventKind, EnableMouseCapture, DisableMouseCapture, EnableBracketedPaste, DisableBracketedPaste, EventStream},
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
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(ref prof) = cli.profile {
        synaps_cli::config::set_profile(Some(prof.clone()));
    }

    let _log_guard = synaps_cli::logging::init_logging();
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
    // Only registers the mcp_connect gateway tool — servers connect on demand.
    let mcp_server_count = synaps_cli::mcp::setup_lazy_mcp(&runtime.tools_shared()).await;
    if mcp_server_count > 0 {
        eprintln!("\x1b[2m  ⚡ {} MCP servers available (use mcp_connect to activate)\x1b[0m", mcp_server_count);
    }

    // Keep reference to system prompt path for save functionality
    let system_prompt_path = synaps_cli::config::resolve_read_path("system.md");

    // Session: continue existing or create new
    let mut app = match cli.continue_session {
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
            // Restore runtime settings from session
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
            // Rebuild display messages from api_messages
            rebuild_display_messages(&session.api_messages, &mut app);
            app.push_msg(ChatMessage::System(format!("resumed session {}", session.id)));
            if app.abort_context.is_some() {
                app.push_msg(ChatMessage::System("⚠ abort context from previous session will be injected into next message".to_string()));
            }
            app
        }
        None => {
            App::new(Session::new(runtime.model(), runtime.thinking_level(), runtime.system_prompt()))
        }
    };

    enable_raw_mode().map_err(|e| synaps_cli::error::RuntimeError::Tool(format!("terminal setup failed: {}", e)))?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture, EnableBracketedPaste)
        .map_err(|e| synaps_cli::error::RuntimeError::Tool(format!("terminal setup failed: {}", e)))?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)
        .map_err(|e| synaps_cli::error::RuntimeError::Tool(format!("terminal setup failed: {}", e)))?;
    let mut event_reader = EventStream::new();
    let mut stream: Option<std::pin::Pin<Box<dyn futures::Stream<Item = StreamEvent> + Send>>> = None;
    let mut cancel_token: Option<CancellationToken> = None;
    let mut steer_tx: Option<tokio::sync::mpsc::UnboundedSender<String>> = None;
    let mut boot_fx: Option<Effect> = Some(boot_effect());
    let mut exit_fx: Option<Effect> = None;
    let mut last_frame = Instant::now();

    loop {
        let elapsed = last_frame.elapsed();
        last_frame = Instant::now();
        let _ = draw(&mut terminal, &mut app, runtime.model(), runtime.thinking_level(), &mut boot_fx, &mut exit_fx, elapsed);

        tokio::select! {
            // Tick: redraws during animations AND during streaming (~60fps throttle)
            _ = tokio::time::sleep(std::time::Duration::from_millis(16)), if boot_fx.is_some() || exit_fx.is_some() || app.streaming || app.messages.is_empty() || app.logo_dismiss_t.is_some() || app.logo_build_t.is_some() || app.gamba_child.is_some() => {
                // Progress logo build-in animation
                if let Some(ref mut t) = app.logo_build_t {
                    *t += 0.025; // ~40 frames = ~0.66s
                    if *t >= 1.0 {
                        app.logo_build_t = None;
                    }
                }
                // Progress logo dismiss animation
                if let Some(ref mut t) = app.logo_dismiss_t {
                    *t += 0.04; // ~25 frames at 60fps = ~0.4s
                    if *t >= 1.0 {
                        app.logo_dismiss_t = None;
                    }
                }
                // Advance spinner for active subagents or streaming (~6 fps visual)
                if !app.subagents.is_empty() || app.streaming {
                    app.spinner_frame = app.spinner_frame.wrapping_add(1);
                    // Only dirty every 3rd tick (~20fps spinner, smooth but not crazy)
                    if app.spinner_frame % 3 == 0 {
                        app.dirty = true;
                        app.line_cache.clear();
                    }
                }
                // Check if casino exited on its own (user quit)
                if let Some(msg) = app.check_gamba_exited() {
                    terminal.clear().ok();
                    app.push_msg(ChatMessage::System(msg));
                    app.dirty = true;
                    app.line_cache.clear();
                    // Force immediate redraw so user doesn't see bare tmux
                    let elapsed = last_frame.elapsed();
                    last_frame = Instant::now();
                    let _ = draw(&mut terminal, &mut app, runtime.model(), runtime.thinking_level(), &mut boot_fx, &mut exit_fx, elapsed);
                }
                if exit_fx.as_ref().is_some_and(|fx| fx.done()) {
                    break;
                }
                continue;
            }
            maybe_event = event_reader.next(), if app.gamba_child.is_none() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) => {
                        match (key.code, key.modifiers) {
                            (KeyCode::Char('c'), KeyModifiers::CONTROL) if exit_fx.is_none() => {
                                exit_fx = Some(quit_effect());
                            }
                            (KeyCode::Esc, _) if app.streaming => {
                                if let Some(ref ct) = cancel_token {
                                    ct.cancel();
                                }
                                // Capture partial work before clearing state
                                app.capture_abort_context();
                                // Clear queued message — user can retype or use input history
                                if let Some(ref q) = app.queued_message.take() {
                                    app.push_msg(ChatMessage::System(format!("dequeued: {}", q)));
                                }
                                stream = None;
                                cancel_token = None;
                                steer_tx = None;
                                app.streaming = false;
                                app.subagents.clear();
                                let abort_msg = if app.abort_context.is_some() {
                                    "aborted — context saved for next message"
                                } else {
                                    "aborted"
                                };
                                app.push_msg(ChatMessage::Error(abort_msg.to_string()));
                                // Save session so partial work survives /continue
                                app.save_session().await;
                            }
                            (KeyCode::Enter, KeyModifiers::SHIFT) if !app.streaming => {
                                // Shift+Enter inserts a literal newline
                                let byte_pos = app.cursor_byte_pos();
                                app.input.insert(byte_pos, '\n');
                                app.cursor_pos += 1;
                            }
                            (KeyCode::Enter, _) if !app.streaming && !app.input.is_empty() => {
                                // Trigger CRT dismiss if this is the first message
                                if app.messages.is_empty() {
                                    app.logo_dismiss_t = Some(0.001);
                                }
                                let input = app.input.clone();
                                app.input_history.push(input.clone());
                                app.history_index = None;
                                app.input_stash.clear();
                                app.input.clear();
                                app.cursor_pos = 0;
                                app.scroll_back = 0;
                                app.scroll_pinned = true;

                                if input.starts_with('/') && input.len() > 1 {
                                    let parts: Vec<&str> = input[1..].splitn(2, ' ').collect();
                                    let raw_cmd = parts[0];
                                    let arg = parts.get(1).map(|s| s.trim()).unwrap_or("");
                                    let all_cmds = ["clear", "model", "system", "thinking", "sessions", "resume", "theme", "gamba", "help", "quit", "exit"];
                                    // Resolve prefix: exact match first, then unique prefix
                                    let cmd = if all_cmds.contains(&raw_cmd) {
                                        raw_cmd.to_string()
                                    } else {
                                        let matches: Vec<&&str> = all_cmds.iter().filter(|c| c.starts_with(raw_cmd)).collect();
                                        if matches.len() == 1 {
                                            matches[0].to_string()
                                        } else {
                                            raw_cmd.to_string()
                                        }
                                    };
                                    match cmd.as_str() {
                                        "clear" => {
                                            app.save_session().await;
                                            app.messages.clear();
                                            app.dirty = true;
                                            app.api_messages.clear();
                                            app.total_input_tokens = 0;
                                            app.total_output_tokens = 0;
                                            app.total_cache_read_tokens = 0;
                                            app.total_cache_creation_tokens = 0;
                                            app.session_cost = 0.0;
                                            app.input_tokens = 0;
                                            app.output_tokens = 0;
                                            app.session = Session::new(runtime.model(), runtime.thinking_level(), runtime.system_prompt());
                                            app.push_msg(ChatMessage::System("new session started".to_string()));
                                        }
                                        "model" => {
                                            if arg.is_empty() {
                                                app.push_msg(ChatMessage::System(
                                                    format!("current model: {}", runtime.model())
                                                ));
                                            } else {
                                                runtime.set_model(arg.to_string());
                                                app.push_msg(ChatMessage::System(
                                                    format!("model set to: {}", arg)
                                                ));
                                            }
                                        }
                                        "system" => {
                                            if arg.is_empty() {
                                                app.push_msg(ChatMessage::System(
                                                    "usage: /system <prompt>  |  /system save  |  /system show".to_string()
                                                ));
                                            } else if arg == "save" {
                                                let _ = std::fs::create_dir_all(synaps_cli::config::get_active_config_dir());
                                                match std::fs::write(&system_prompt_path, runtime.system_prompt().unwrap_or("")) {
                                                    Ok(_) => app.push_msg(ChatMessage::System(
                                                        format!("saved to {}", system_prompt_path.display())
                                                    )),
                                                    Err(e) => app.push_msg(ChatMessage::Error(
                                                        format!("failed to save: {}", e)
                                                    )),
                                                }
                                            } else if arg == "show" {
                                                let prompt = runtime.system_prompt().unwrap_or("(none)");
                                                app.push_msg(ChatMessage::System(prompt.to_string()));
                                            } else {
                                                runtime.set_system_prompt(arg.to_string());
                                                app.push_msg(ChatMessage::System(
                                                    "system prompt updated".to_string()
                                                ));
                                            }
                                        }
                                        "thinking" => {
                                            match arg {
                                                "low" => { runtime.set_thinking_budget(2048); }
                                                "medium" | "med" => { runtime.set_thinking_budget(4096); }
                                                "high" => { runtime.set_thinking_budget(16384); }
                                                "xhigh" => { runtime.set_thinking_budget(32768); }
                                                "" => {
                                                    app.push_msg(ChatMessage::System(
                                                        format!("thinking: {} ({})", runtime.thinking_level(), runtime.thinking_budget())
                                                    ));
                                                }
                                                _ => {
                                                    app.push_msg(ChatMessage::Error(
                                                        "usage: /thinking low|medium|high|xhigh".to_string()
                                                    ));
                                                }
                                            }
                                            if !arg.is_empty() && ["low", "medium", "med", "high", "xhigh"].contains(&arg) {
                                                app.push_msg(ChatMessage::System(
                                                    format!("thinking set to: {}", runtime.thinking_level())
                                                ));
                                            }
                                        }
                                        "sessions" => {
                                            match list_sessions() {
                                                Ok(sessions) if sessions.is_empty() => {
                                                    app.push_msg(ChatMessage::System("no saved sessions".to_string()));
                                                }
                                                Ok(sessions) => {
                                                    app.push_msg(ChatMessage::System(format!("{} session(s):", sessions.len())));
                                                    for s in sessions.iter().take(20) {
                                                        let title = if s.title.is_empty() { "(untitled)" } else { &s.title };
                                                        let active = if s.id == app.session.id { " *" } else { "" };
                                                        app.push_msg(ChatMessage::System(format!(
                                                            "  {} — {} [{}] ${:.4}{}",
                                                            &s.id, title, s.model, s.session_cost, active
                                                        )));
                                                    }
                                                }
                                                Err(e) => {
                                                    app.push_msg(ChatMessage::Error(format!("failed to list sessions: {}", e)));
                                                }
                                            }
                                        }
                                        "resume" => {
                                            if arg.is_empty() {
                                                app.push_msg(ChatMessage::System("usage: /resume <session_id>".to_string()));
                                            } else {
                                                match find_session(arg) {
                                                    Ok(session) => {
                                                        runtime.set_model(session.model.clone());
                                                        if let Some(ref sp) = session.system_prompt {
                                                            runtime.set_system_prompt(sp.clone());
                                                        }
                                                        // Save current session before switching
                                                        app.save_session().await;
                                                        let old_id = app.session.id.clone();
                                                        // Rebuild app state from loaded session
                                                        app.messages.clear();
                                                        app.dirty = true;
                                                        app.api_messages = session.api_messages.clone();
                                                        app.total_input_tokens = session.total_input_tokens;
                                                        app.total_output_tokens = session.total_output_tokens;
                                                        app.session_cost = session.session_cost;
                                                        // Rebuild display messages
                                                        rebuild_display_messages(&session.api_messages, &mut app);
                                                        app.session = session;
                                                        app.push_msg(ChatMessage::System(
                                                            format!("switched from {} to {}", old_id, app.session.id)
                                                        ));
                                                    }
                                                    Err(e) => {
                                                        app.push_msg(ChatMessage::Error(format!("failed to load session: {}", e)));
                                                    }
                                                }
                                            }
                                        }
                                        "help" => {
                                            app.push_msg(ChatMessage::System(
                                                "/clear — reset conversation".to_string()
                                            ));
                                            app.push_msg(ChatMessage::System(
                                                "/model [name] — show or set model".to_string()
                                            ));
                                            app.push_msg(ChatMessage::System(
                                                "/system <prompt|show|save> — system prompt".to_string()
                                            ));
                                            app.push_msg(ChatMessage::System(
                                                "/thinking [low|medium|high|xhigh] — thinking budget".to_string()
                                            ));
                                            app.push_msg(ChatMessage::System(
                                                "/sessions — list saved sessions".to_string()
                                            ));
                                            app.push_msg(ChatMessage::System(
                                                "/resume <id> — switch to a different session".to_string()
                                            ));
                                            app.push_msg(ChatMessage::System(
                                                "/help — show this".to_string()
                                            ));
                                            app.push_msg(ChatMessage::System(
                                                "/theme — list available themes".to_string()
                                            ));
                                            app.push_msg(ChatMessage::System(
                                                "/gamba — open the casino 🎰".to_string()
                                            ));
                                        }
                                        "quit" | "exit" => {
                                            exit_fx = Some(quit_effect());
                                        }
                                        "theme" => {
                                            app.handle_theme_command(arg);
                                        }
                                        "gamba" => {
                                            // Drop event reader so crossterm stops consuming stdin
                                            drop(event_reader);
                                            match app.launch_gamba() {
                                                Ok(()) => {} // casino running, terminal yielded
                                                Err(msg) => {
                                                    terminal.clear().ok();
                                                    app.push_msg(ChatMessage::Error(msg));
                                                }
                                            }
                                            // Recreate event reader (casino may still be running — that's ok,
                                            // the select! guard prevents polling until gamba exits)
                                            event_reader = EventStream::new();
                                        }
                                        _ => {
                                            app.push_msg(ChatMessage::Error(
                                                format!("unknown command: /{}", cmd)
                                            ));
                                        }
                                    }
                                } else {
                                    // Display: typed text + paste indicator
                                    let display_text = if app.pasted_char_count > 0 {
                                        let typed = app.input_before_paste.as_deref().unwrap_or("");
                                        // Use char boundary from char count, not byte len
                                        let typed_char_count = typed.chars().count();
                                        let pasted_char_count = input.chars().count().saturating_sub(typed_char_count);
                                        let paste_byte_start = input.char_indices()
                                            .nth(typed_char_count)
                                            .map(|(i, _)| i)
                                            .unwrap_or(input.len());
                                        let paste_content = &input[paste_byte_start..];
                                        let line_count = paste_content.lines().count();
                                        let paste_label = if line_count > 1 {
                                            format!("[Pasted {} lines]", line_count)
                                        } else {
                                            format!("[Pasted {} chars]", pasted_char_count)
                                        };
                                        if typed.is_empty() {
                                            paste_label
                                        } else {
                                            format!("{} {}", typed.trim(), paste_label)
                                        }
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
                                    app.api_messages.push(json!({"role": "user", "content": api_content}));
                                    let ct = CancellationToken::new();
                                    let (s_tx, s_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
                                    // Show auth status in header during token refresh
                                    app.status_text = Some("connecting…".to_string());
                                    app.streaming = true;  // Start spinner immediately
                                    app.spinner_frame = 0;
                                    let elapsed = last_frame.elapsed();
                                    last_frame = Instant::now();
                                    let _ = draw(&mut terminal, &mut app, runtime.model(), runtime.thinking_level(), &mut boot_fx, &mut exit_fx, elapsed);
                                    stream = Some(runtime.run_stream_with_messages(app.api_messages.clone(), ct.clone(), Some(s_rx)).await);
                                    app.status_text = None;
                                    // Show thinking placeholder until first real token arrives
                                    app.push_msg(ChatMessage::Thinking("…".to_string()));
                                    cancel_token = Some(ct);
                                    steer_tx = Some(s_tx);
                                }
                            }
                            (KeyCode::Enter, _) if app.streaming && !app.input.is_empty() => {
                                let input = app.input.clone();
                                app.input_history.push(input.clone());
                                app.history_index = None;
                                app.input_stash.clear();
                                app.input.clear();
                                app.cursor_pos = 0;
                                app.input_before_paste = None;
                                app.pasted_char_count = 0;

                                // Intercept slash commands during streaming
                                if let Some(rest) = input.strip_prefix('/') {
                                    let raw_cmd = rest.split_whitespace().next().unwrap_or("");
                                    // Use same prefix resolution as non-streaming path
                                    let streaming_cmds = ["gamba", "theme", "quit", "exit"];
                                    let cmd = if streaming_cmds.contains(&raw_cmd) {
                                        raw_cmd.to_string()
                                    } else {
                                        let matches: Vec<&&str> = streaming_cmds.iter().filter(|c| c.starts_with(raw_cmd)).collect();
                                        if matches.len() == 1 { matches[0].to_string() } else { raw_cmd.to_string() }
                                    };
                                    match cmd.as_str() {
                                        "gamba" => {
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
                                        "theme" => {
                                            let arg = input[1..].split_once(' ').map(|x| x.1).unwrap_or("").trim();
                                            app.handle_theme_command(arg);
                                        }
                                        "quit" | "exit" => {
                                            exit_fx = Some(quit_effect());
                                        }
                                        _ => {
                                            // Unknown slash — steer/queue as normal
                                            let steered = steer_tx.as_ref()
                                                .map(|tx| tx.send(input.clone()).is_ok())
                                                .unwrap_or(false);
                                            if steered {
                                                app.push_msg(ChatMessage::System(format!("→ steering: {}", input)));
                                            } else {
                                                app.push_msg(ChatMessage::System(format!("queued: {}", input)));
                                            }
                                            app.queued_message = Some(input);
                                        }
                                    }
                                } else {
                                    // Normal text — steer/queue
                                    let steered = steer_tx.as_ref()
                                        .map(|tx| tx.send(input.clone()).is_ok())
                                        .unwrap_or(false);
                                    if steered {
                                        app.push_msg(ChatMessage::System(format!("→ steering: {}", input)));
                                    } else {
                                        app.push_msg(ChatMessage::System(format!("queued: {}", input)));
                                    }
                                    app.queued_message = Some(input);
                                }
                            }
                            (KeyCode::Tab, _) if app.input.starts_with('/') && app.input.len() > 1 => {
                                let partial = &app.input[1..];
                                let commands = ["clear", "model", "system", "thinking", "sessions", "resume", "theme", "gamba", "help", "quit", "exit"];
                                let matches: Vec<&&str> = commands.iter()
                                    .filter(|c| c.starts_with(partial))
                                    .collect();
                                if matches.len() == 1 {
                                    app.input = format!("/{}", matches[0]);
                                    app.cursor_pos = app.input.chars().count();
                                } else if matches.len() > 1 {
                                    // Find common prefix
                                    let first = matches[0];
                                    let common_len = (0..first.len())
                                        .take_while(|&i| matches.iter().all(|m| m.as_bytes().get(i) == first.as_bytes().get(i)))
                                        .count();
                                    if common_len > partial.len() {
                                        app.input = format!("/{}", &first[..common_len]);
                                        app.cursor_pos = app.input.chars().count();
                                    }
                                }
                            }
                            (KeyCode::Char('a'), KeyModifiers::CONTROL) => {
                                app.cursor_pos = 0;
                            }
                            (KeyCode::Char('e'), KeyModifiers::CONTROL) => {
                                app.cursor_pos = app.input.chars().count();
                            }
                            (KeyCode::Char('w'), KeyModifiers::CONTROL) => {
                                // Delete word backward (same as Alt+Backspace)
                                let chars: Vec<char> = app.input.chars().collect();
                                let mut pos = app.cursor_pos;
                                while pos > 0 && chars[pos - 1] == ' ' { pos -= 1; }
                                while pos > 0 && chars[pos - 1] != ' ' { pos -= 1; }
                                let byte_start = app.input.char_indices().nth(pos).map(|(i, _)| i).unwrap_or(app.input.len());
                                let byte_end = app.cursor_byte_pos();
                                app.input.drain(byte_start..byte_end);
                                app.cursor_pos = pos;
                            }
                            (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                                // Clear input line
                                app.input.clear();
                                app.cursor_pos = 0;
                            }
                            (KeyCode::Home, _) => {
                                app.cursor_pos = 0;
                            }
                            (KeyCode::End, _) => {
                                app.cursor_pos = app.input.chars().count();
                            }
                            (KeyCode::Left, KeyModifiers::ALT) => {
                                // Jump word left
                                let chars: Vec<char> = app.input.chars().collect();
                                let mut pos = app.cursor_pos;
                                while pos > 0 && chars[pos - 1] == ' ' { pos -= 1; }
                                while pos > 0 && chars[pos - 1] != ' ' { pos -= 1; }
                                app.cursor_pos = pos;
                            }
                            (KeyCode::Right, KeyModifiers::ALT) => {
                                // Jump word right
                                let chars: Vec<char> = app.input.chars().collect();
                                let len = chars.len();
                                let mut pos = app.cursor_pos;
                                while pos < len && chars[pos] != ' ' { pos += 1; }
                                while pos < len && chars[pos] == ' ' { pos += 1; }
                                app.cursor_pos = pos;
                            }
                            (KeyCode::Backspace, KeyModifiers::ALT) => {
                                // Delete word backward
                                let chars: Vec<char> = app.input.chars().collect();
                                let mut pos = app.cursor_pos;
                                while pos > 0 && chars[pos - 1] == ' ' { pos -= 1; }
                                while pos > 0 && chars[pos - 1] != ' ' { pos -= 1; }
                                let byte_start = app.input.char_indices().nth(pos).map(|(i, _)| i).unwrap_or(app.input.len());
                                let byte_end = app.cursor_byte_pos();
                                app.input.drain(byte_start..byte_end);
                                app.cursor_pos = pos;
                            }
                            (KeyCode::Char('o'), KeyModifiers::CONTROL) => {
                                app.show_full_output = !app.show_full_output;
                                app.dirty = true;
                                app.line_cache.clear();
                            }
                            (KeyCode::Char(c), _) => {
                                let byte_pos = app.cursor_byte_pos();
                                app.input.insert(byte_pos, c);
                                app.cursor_pos += 1;
                            }
                            (KeyCode::Backspace, _) if app.cursor_pos > 0 => {
                                app.cursor_pos -= 1;
                                let byte_pos = app.cursor_byte_pos();
                                app.input.remove(byte_pos);
                            }
                            (KeyCode::Left, _) if app.cursor_pos > 0 => {
                                app.cursor_pos -= 1;
                            }
                            (KeyCode::Right, _) if app.cursor_pos < app.input_char_count() => {
                                app.cursor_pos += 1;
                            }
                            (KeyCode::Up, KeyModifiers::SHIFT) => {
                                app.scroll_back = app.scroll_back.saturating_add(1);
                                app.scroll_pinned = false;
                            }
                            (KeyCode::Down, KeyModifiers::SHIFT) => {
                                app.scroll_back = app.scroll_back.saturating_sub(1);
                                if app.scroll_back == 0 {
                                    app.scroll_pinned = true;
                                }
                            }
                            (KeyCode::Up, _) => {
                                app.history_up();
                            }
                            (KeyCode::Down, _) => {
                                app.history_down();
                            }
                            _ => {}
                        }
                    }
                    Some(Ok(Event::Mouse(mouse))) => {
                        match mouse.kind {
                            MouseEventKind::ScrollUp => {
                                app.scroll_back = app.scroll_back.saturating_add(3);
                                app.scroll_pinned = false;
                            }
                            MouseEventKind::ScrollDown => {
                                app.scroll_back = app.scroll_back.saturating_sub(3);
                                if app.scroll_back == 0 {
                                    app.scroll_pinned = true;
                                }
                            }
                            _ => {}
                        }
                    }
                    Some(Ok(Event::Paste(text))) => {
                        // Bracketed paste — insert the full text (newlines included)
                        // into the input buffer at cursor position
                        const MAX_PASTE_CHARS: usize = 100_000;
                        if !app.streaming || !app.input.is_empty() {
                            // Cap paste size to prevent OOM
                            let text = if text.chars().count() > MAX_PASTE_CHARS {
                                let truncated: String = text.chars().take(MAX_PASTE_CHARS).collect();
                                app.push_msg(ChatMessage::System(
                                    format!("Paste truncated to {} chars (was {})", MAX_PASTE_CHARS, text.chars().count())
                                ));
                                truncated
                            } else {
                                text
                            };
                            // Snapshot input before first paste so we can show typed text separately
                            if app.input_before_paste.is_none() {
                                app.input_before_paste = Some(app.input.clone());
                            }
                            let byte_pos = app.cursor_byte_pos();
                            app.input.insert_str(byte_pos, &text);
                            app.cursor_pos += text.chars().count();
                            app.pasted_char_count += text.chars().count();
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break,
                }
            }

            maybe_event = async {
                if let Some(ref mut s) = stream {
                    s.next().await
                } else {
                    std::future::pending().await
                }
            } => {
                if let Some(event) = maybe_event {
                    // Only redraw immediately for structural events (tool calls,
                    // completion, errors). Text/thinking tokens are batched and
                    // rendered by the 16ms tick to avoid hundreds of redraws/sec.
                    let needs_immediate_draw = matches!(&event,
                        StreamEvent::ToolUse { .. }
                        | StreamEvent::ToolResult { .. }
                        | StreamEvent::SubagentStart { .. }
                        | StreamEvent::SubagentUpdate { .. }
                        | StreamEvent::SubagentDone { .. }
                        | StreamEvent::SteeringDelivered { .. }
                        | StreamEvent::Done
                        | StreamEvent::Error(_)
                    );

                    match event {
                        StreamEvent::Thinking(text) => {
                            app.append_or_update_thinking(&text);
                        }
                        StreamEvent::Text(text) => {
                            app.append_or_update_text(&text);
                        }
                        StreamEvent::ToolUseStart(name) => {
                            app.tool_start_time = Some(std::time::Instant::now());
                            app.push_msg(ChatMessage::ToolUseStart(name, String::new()));
                        }
                        StreamEvent::ToolUseDelta(delta) => {
                            if let Some(last) = app.messages.last_mut() {
                                if let ChatMessage::ToolUseStart(_, ref mut partial) = last.msg {
                                    partial.push_str(&delta);
                                    app.dirty = true;
                                    app.line_cache.clear();
                                    continue;
                                }
                            }
                        }
                        StreamEvent::ToolUse { tool_name, input, .. } => {
                            app.tool_start_time = Some(std::time::Instant::now());
                            let input_str = serde_json::to_string(&input).unwrap_or_default();
                            if let Some(last) = app.messages.last_mut() {
                                if let ChatMessage::ToolUseStart(name, _) = &last.msg {
                                    if name == &tool_name {
                                        last.msg = ChatMessage::ToolUse { tool_name, input: input_str };
                                        app.dirty = true;
                                        app.line_cache.clear();
                                        continue;
                                    }
                                }
                            }
                            app.push_msg(ChatMessage::ToolUse { tool_name, input: input_str });
                        }
                        StreamEvent::ToolResultDelta { delta, .. } => {
                            if let Some(last) = app.messages.last_mut() {
                                if let ChatMessage::ToolResult { ref mut content, .. } = last.msg {
                                    content.push_str(&delta);
                                    app.dirty = true;
                                    app.line_cache.clear();
                                    continue;
                                }
                            }
                            app.push_msg(ChatMessage::ToolResult { content: delta, elapsed_ms: None });
                        }
                        StreamEvent::ToolResult { result, .. } => {
                            let elapsed = app.tool_start_time.take()
                                .map(|t| t.elapsed().as_millis() as u64);
                            if let Some(last) = app.messages.last_mut() {
                                if let ChatMessage::ToolResult { ref mut content, elapsed_ms: ref mut el, .. } = last.msg {
                                    *content = result;
                                    *el = elapsed;
                                    app.dirty = true;
                                    app.line_cache.clear();
                                    continue;
                                }
                            }
                            app.push_msg(ChatMessage::ToolResult { content: result, elapsed_ms: elapsed });
                        }
                        StreamEvent::MessageHistory(history) => {
                            app.api_messages = history;
                            app.save_session().await;
                        }
                        StreamEvent::SubagentStart { subagent_id, agent_name, task_preview } => {
                            app.subagents.push(SubagentState {
                                id: subagent_id,
                                name: agent_name,
                                status: format!("starting: {}", task_preview),
                                start_time: std::time::Instant::now(),
                                done: false,
                                duration_secs: None,
                            });
                            app.dirty = true;
                            app.line_cache.clear();
                        }
                        StreamEvent::SubagentUpdate { subagent_id, status, .. } => {
                            if let Some(sa) = app.subagents.iter_mut().find(|s| s.id == subagent_id) {
                                sa.status = status;
                            }
                            app.dirty = true;
                            app.line_cache.clear();
                        }
                        StreamEvent::SubagentDone { subagent_id, result_preview, duration_secs, .. } => {
                            if let Some(sa) = app.subagents.iter_mut().find(|s| s.id == subagent_id) {
                                sa.done = true;
                                sa.duration_secs = Some(duration_secs);
                                let preview: String = result_preview.chars().take(40).collect();
                                if result_preview.starts_with("[TIMED OUT") {
                                    sa.status = "\u{26a0} timed out".to_string();
                                } else if result_preview.starts_with("ERROR") {
                                    sa.status = format!("\u{2718} {}", preview);
                                } else {
                                    sa.status = format!("\u{2714} {}", preview);
                                }
                            }
                            app.dirty = true;
                            app.line_cache.clear();
                        }
                        StreamEvent::SteeringDelivered { message } => {
                            app.push_msg(ChatMessage::User(message.clone()));
                            // Steering delivered — clear queue so Done doesn't double-send
                            if app.queued_message.as_ref() == Some(&message) {
                                app.queued_message = None;
                            }
                            app.scroll_back = 0;
                            app.scroll_pinned = true;
                            app.dirty = true;
                            app.line_cache.clear();
                        }
                        StreamEvent::Usage {
                            input_tokens,
                            output_tokens,
                            cache_read_input_tokens,
                            cache_creation_input_tokens,
                            model: usage_model,
                        } => {
                            let model_for_pricing = usage_model.as_deref().unwrap_or(runtime.model());
                            app.add_usage(
                                input_tokens,
                                output_tokens,
                                cache_read_input_tokens,
                                cache_creation_input_tokens,
                                model_for_pricing,
                            );
                        }
                        StreamEvent::Done => {
                            app.streaming = false;
                            // Clear completed subagents panel
                            app.subagents.clear();
                            stream = None;
                            cancel_token = None;
                            steer_tx = None;

                            // Reclaim terminal from casino if running
                            if let Some(msg) = app.reclaim_gamba() {
                                terminal.clear().ok();
                                app.push_msg(ChatMessage::System(msg));
                                app.dirty = true;
                                app.line_cache.clear();
                                let elapsed = last_frame.elapsed();
                                last_frame = Instant::now();
                                let _ = draw(&mut terminal, &mut app, runtime.model(), runtime.thinking_level(), &mut boot_fx, &mut exit_fx, elapsed);
                            }

                            // Auto-send queued message if one was typed during streaming
                            if let Some(queued) = app.queued_message.take() {
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
                                app.api_messages.push(json!({"role": "user", "content": api_content}));
                                let ct = CancellationToken::new();
                                let (s_tx, s_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
                                // Show auth status in header during token refresh
                                app.status_text = Some("connecting…".to_string());
                                app.streaming = true;  // Start spinner immediately
                                app.spinner_frame = 0;
                                let elapsed = last_frame.elapsed();
                                last_frame = Instant::now();
                                let _ = draw(&mut terminal, &mut app, runtime.model(), runtime.thinking_level(), &mut boot_fx, &mut exit_fx, elapsed);
                                stream = Some(runtime.run_stream_with_messages(app.api_messages.clone(), ct.clone(), Some(s_rx)).await);
                                app.status_text = None;
                                // Show thinking placeholder until first real token arrives
                                app.push_msg(ChatMessage::Thinking("…".to_string()));
                                cancel_token = Some(ct);
                                steer_tx = Some(s_tx);
                            }
                        }
                        StreamEvent::Error(err) => {
                            app.push_msg(ChatMessage::Error(err));
                            app.streaming = false;
                            app.subagents.clear();
                            stream = None;
                            cancel_token = None;
                            steer_tx = None;
                            // Reclaim terminal from casino if running
                            if let Some(msg) = app.reclaim_gamba() {
                                terminal.clear().ok();
                                app.push_msg(ChatMessage::System(msg));
                                app.dirty = true;
                                app.line_cache.clear();
                                let elapsed = last_frame.elapsed();
                                last_frame = Instant::now();
                                let _ = draw(&mut terminal, &mut app, runtime.model(), runtime.thinking_level(), &mut boot_fx, &mut exit_fx, elapsed);
                            }
                            // Restore a valid trailing state. The runtime guarantees that
                            // each tool_use has a matching tool_result, so we only need to
                            // drop an unmatched trailing assistant message or a trailing
                            // plain-text user message (so the user can retry cleanly).
                            // We must NOT pop a trailing user tool_result message, because
                            // that would orphan the preceding assistant tool_use blocks.
                            if let Some(last) = app.api_messages.last() {
                                let role = last["role"].as_str().unwrap_or("");
                                let is_text_user = role == "user" && last["content"].is_string();
                                let is_assistant = role == "assistant";
                                if is_text_user || is_assistant {
                                    app.api_messages.pop();
                                }
                            }
                        }
                    }
                    if needs_immediate_draw {
                        let elapsed = last_frame.elapsed();
                        last_frame = Instant::now();
                        let _ = draw(&mut terminal, &mut app, runtime.model(), runtime.thinking_level(), &mut boot_fx, &mut exit_fx, elapsed);
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
