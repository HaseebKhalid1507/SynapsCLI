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
mod settings;
mod plugins;

use app::{App, ChatMessage};
use draw::{draw, boot_effect, quit_effect};
use commands::CommandAction;
use input::InputAction;
use stream_handler::StreamAction;

use synaps_cli::{Runtime, StreamEvent, Result, CancellationToken, Session, latest_session, find_session};
use synaps_cli::core::compaction::compact_conversation;
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


/// Apply a settings-menu change: mutate Runtime where possible, persist to config,
/// and stash write errors in the modal's row_error slot.
///
/// The runtime mutation is delegated to the macro-generated dispatch in
/// `settings/defs.rs` — single source of truth for schema + apply.
fn apply_setting(
    key: &'static str,
    value: &str,
    app: &mut App,
    runtime: &mut synaps_cli::Runtime,
) {
    // Runtime mutation (generated from settings/defs.rs).
    settings::defs::apply_setting_dispatch(key, value, runtime, app);

    // `skills` is internal — not persisted via write_config_value.
    if key == "skills" { return; }

    match synaps_cli::config::write_config_value(key, value) {
        Ok(()) => {
            if let Some(st) = app.settings.as_mut() {
                if key == "theme" {
                    if let Some(t) = theme::load_theme_by_name(value) {
                        theme::set_theme(t);
                    }
                    st.row_error = None;
                } else {
                    st.row_error = None;
                }
                st.edit_mode = None;
            }
        }
        Err(e) => {
            if let Some(st) = app.settings.as_mut() {
                st.row_error = Some((key.to_string(), e.to_string()));
            }
        }
    }
}

fn rebuild_display_messages(api_messages: &[Value], app: &mut App) {
    for msg in api_messages {
        // Skip compaction summary messages — internal context, not user-visible
        if let Some(content) = msg["content"].as_str() {
            if content.contains("<context-summary>") {
                continue;
            }
        }
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

pub async fn run(
    continue_session: Option<Option<String>>,
    system: Option<String>,
    profile: Option<String>,
) -> Result<()> {
    if let Some(ref prof) = profile {
        synaps_cli::config::set_profile(Some(prof.clone()));
    }

    let _log_guard = synaps_cli::logging::init_logging();
    let mut runtime = Runtime::new().await?;

    // Load config and apply
    let mut config = synaps_cli::config::load_config();
    runtime.apply_config(&config);

    // Load system prompt
    let system_prompt = synaps_cli::config::resolve_system_prompt(system.as_deref());
    runtime.set_system_prompt(system_prompt);

    // Discover plugins/skills, build command registry, register load_skill tool.
    let tools_shared = runtime.tools_shared();
    let registry = synaps_cli::skills::register(&tools_shared, &config).await;
    let skill_count = registry.all_skills().len();

    // Set up lazy MCP loading (if configured in ~/.synaps-cli/mcp.json)
    let mcp_server_count = synaps_cli::mcp::setup_lazy_mcp(&runtime.tools_shared()).await;

    let system_prompt_path = synaps_cli::config::resolve_read_path("system.md");

    // Session: continue existing or create new
    let mut app = match continue_session {
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

    if skill_count > 0 {
        app.push_msg(ChatMessage::System(format!(
            "📚 {} skills available (type / to list)",
            skill_count
        )));
    }

    // Sync the context bar denominator with the runtime's effective window
    // (respects config override like `context_window = 200k`).
    app.last_turn_context_window = runtime.context_window();
    if mcp_server_count > 0 {
        app.push_msg(ChatMessage::System(format!(
            "⚡ {} MCP servers available (use mcp_connect to activate)",
            mcp_server_count
        )));
    }

    // ── Terminal setup ──
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

    // ── Event loop ──
    loop {
        let elapsed = last_frame.elapsed();
        last_frame = Instant::now();
        let _ = draw(&mut terminal, &mut app, &runtime, &mut boot_fx, &mut exit_fx, elapsed, &registry);

        tokio::select! {
            // ── Tick: animations + spinner (~60fps) ──
            _ = tokio::time::sleep(std::time::Duration::from_millis(16)), if boot_fx.is_some() || exit_fx.is_some() || app.streaming || app.compact_task.is_some() || app.messages.is_empty() || app.logo_dismiss_t.is_some() || app.logo_build_t.is_some() || app.gamba_child.is_some() => {
                if let Some(ref mut t) = app.logo_build_t {
                    *t += 0.025;
                    if *t >= 1.0 { app.logo_build_t = None; }
                }
                if let Some(ref mut t) = app.logo_dismiss_t {
                    *t += 0.04;
                    if *t >= 1.0 { app.logo_dismiss_t = None; }
                }
                if !app.subagents.is_empty() || app.streaming || app.compact_task.is_some() {
                    app.spinner_frame = app.spinner_frame.wrapping_add(1);
                    if app.spinner_frame % 3 == 0 {
                        app.invalidate();
                    }
                }
                if let Some(msg) = app.check_gamba_exited() {
                    terminal.clear().ok();
                    app.push_msg(ChatMessage::System(msg));
                    app.invalidate();
                    let elapsed = last_frame.elapsed();
                    last_frame = Instant::now();
                    let _ = draw(&mut terminal, &mut app, &runtime, &mut boot_fx, &mut exit_fx, elapsed, &registry);
                }
                // Poll background compaction task
                if app.compact_task.as_ref().is_some_and(|t| t.is_finished()) {
                    let handle = app.compact_task.take().unwrap();
                    let msg_count = app.api_messages.len();
                    match handle.await {
                        Ok(Ok(summary)) => {
                            let old_id = app.session.id.clone();
                            let new_session = Session::new_from_compaction(&app.session, summary.clone());
                            let new_id = new_session.id.clone();
                            app.session.compacted_into = Some(new_id.clone());
                            app.session.save().await.ok();
                            app.session = new_session;
                            app.api_messages = app.session.api_messages.clone();
                            app.total_input_tokens = 0;
                            app.total_output_tokens = 0;
                            app.session_cost = 0.0;
                            let msgs = app.api_messages.clone();
                            rebuild_display_messages(&msgs, &mut app);
                            app.save_session().await;
                            app.push_msg(ChatMessage::System(format!(
                                "✓ compacted {} messages → new session {} (from {})",
                                msg_count, new_id, old_id
                            )));
                        }
                        Ok(Err(e)) => {
                            app.push_msg(ChatMessage::Error(format!("compaction failed: {}", e)));
                        }
                        Err(e) => {
                            app.push_msg(ChatMessage::Error(format!("compaction task panicked: {}", e)));
                        }
                    }
                    app.status_text = None;
                    app.invalidate();
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
                        let action = input::handle_event(event, &mut app, &runtime, is_streaming, &registry);
                        match action {
                            InputAction::None => {}
                            InputAction::Quit => {
                                exit_fx = Some(quit_effect());
                            }
                            InputAction::Abort => {
                                if let Some(ref ct) = cancel_token { ct.cancel(); }
                                app.capture_abort_context();
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
                                app.save_session().await;
                            }
                            InputAction::SlashCommand(cmd, arg) => {
                                match commands::handle_command(&cmd, &arg, &mut app, &mut runtime, &system_prompt_path, &registry).await {
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
                                    CommandAction::OpenSettings => {
                                        app.settings = Some(settings::SettingsState::new());
                                    }
                                    CommandAction::OpenPlugins => {
                                        let path = synaps_cli::skills::state::PluginsState::default_path();
                                        match synaps_cli::skills::state::PluginsState::load_from(&path) {
                                            Ok(file) => {
                                                app.plugins = Some(plugins::PluginsModalState::new(file));
                                            }
                                            Err(e) => {
                                                app.push_msg(ChatMessage::Error(format!(
                                                    "failed to load plugins.json: {}", e
                                                )));
                                            }
                                        }
                                    }
                                    CommandAction::ReloadPlugins => {
                                        synaps_cli::skills::reload_registry(&registry, &config);
                                        app.push_msg(ChatMessage::System("plugins reloaded".to_string()));
                                    }
                                    CommandAction::LoadSkill { skill, arg } => {
                                        use synaps_cli::skills::tool::LoadSkillTool;

                                        let tool_use_id = format!("toolu_skill_{}", uuid::Uuid::new_v4().simple());
                                        let body = LoadSkillTool::format_body(&skill);

                                        app.api_messages.push(json!({
                                            "role": "assistant",
                                            "content": [{
                                                "type": "tool_use",
                                                "id": tool_use_id,
                                                "name": "load_skill",
                                                "input": {"skill": skill.name.clone()}
                                            }]
                                        }));
                                        app.api_messages.push(json!({
                                            "role": "user",
                                            "content": [{
                                                "type": "tool_result",
                                                "tool_use_id": tool_use_id,
                                                "content": body
                                            }]
                                        }));
                                        let display_name = match &skill.plugin {
                                            Some(p) => format!("{}:{}", p, skill.name),
                                            None => skill.name.clone(),
                                        };
                                        app.push_msg(ChatMessage::System(format!("loaded skill: {}", display_name)));

                                        if !arg.is_empty() {
                                            app.api_messages.push(json!({"role": "user", "content": arg.clone()}));
                                            app.push_msg(ChatMessage::User(arg));
                                        }
                                        // Start stream — mirror InputAction::Submit stream-start pattern.
                                        let ct = CancellationToken::new();
                                        let (s_tx, s_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
                                        app.status_text = Some("connecting…".to_string());
                                        app.streaming = true;
                                        app.spinner_frame = 0;
                                        let elapsed = last_frame.elapsed();
                                        last_frame = Instant::now();
                                        let _ = draw(&mut terminal, &mut app, &runtime, &mut boot_fx, &mut exit_fx, elapsed, &registry);
                                        stream = Some(runtime.run_stream_with_messages(app.api_messages.clone(), ct.clone(), Some(s_rx)).await);
                                        app.status_text = None;
                                        app.push_msg(ChatMessage::Thinking("…".to_string()));
                                        cancel_token = Some(ct);
                                        steer_tx = Some(s_tx);
                                    }
                                    CommandAction::Compact { custom_instructions } => {
                                        // Need at least 2 full turns (user + assistant = 2 messages each).
                                        if app.api_messages.len() < 4 {
                                            app.push_msg(ChatMessage::System(
                                                "nothing to compact (need at least 2 turns)".to_string(),
                                            ));
                                        } else if app.compact_task.is_some() {
                                            app.push_msg(ChatMessage::System(
                                                "compaction already in progress".to_string(),
                                            ));
                                        } else {
                                            app.push_msg(ChatMessage::System(
                                                "compacting conversation...".to_string(),
                                            ));
                                            app.status_text = Some("compacting…".to_string());
                                            app.spinner_frame = 0;

                                            let msgs = app.api_messages.clone();
                                            let rt = runtime.clone();
                                            let instr = custom_instructions.clone();
                                            let handle = tokio::spawn(async move {
                                                compact_conversation(&msgs, &rt, instr.as_deref()).await
                                            });
                                            app.compact_task = Some(handle);
                                        }
                                    }
                                    CommandAction::Chain => {
                                        // Walk the parent_session chain backward from current session
                                        let mut chain: Vec<(String, String, usize)> = Vec::new(); // (id, title, msg_count)

                                        // Current session first
                                        chain.push((
                                            app.session.id.clone(),
                                            if app.session.title.is_empty() { "(untitled)".to_string() } else { app.session.title.clone() },
                                            app.api_messages.len(),
                                        ));

                                        // Walk backward through parents
                                        let mut current_parent = app.session.parent_session.clone();
                                        while let Some(ref parent_id) = current_parent {
                                            match synaps_cli::core::session::Session::load(parent_id) {
                                                Ok(parent) => {
                                                    let title = if parent.title.is_empty() { "(untitled)".to_string() } else { parent.title.clone() };
                                                    let msg_count = parent.api_messages.len();
                                                    chain.push((parent.id.clone(), title, msg_count));
                                                    current_parent = parent.parent_session.clone();
                                                }
                                                Err(_) => {
                                                    chain.push((parent_id.clone(), "(not found)".to_string(), 0));
                                                    break;
                                                }
                                            }
                                        }

                                        // Reverse so root is first
                                        chain.reverse();

                                        if chain.len() <= 1 {
                                            app.push_msg(ChatMessage::System("no compaction history — this is the root session".to_string()));
                                        } else {
                                            let mut lines = vec!["Session chain:".to_string()];
                                            for (i, (id, title, msgs)) in chain.iter().enumerate() {
                                                let marker = if i == chain.len() - 1 { " ← active" } else { "" };
                                                let short_id: String = id.chars().take(19).collect();
                                                let short_title: String = title.chars().take(40).collect();
                                                lines.push(format!("  {} {} ({} msgs) {}{}",
                                                    if i == 0 { "●" } else { "→" },
                                                    short_id, msgs, short_title, marker
                                                ));
                                            }
                                            app.push_msg(ChatMessage::System(lines.join("\n")));
                                        }
                                    }
                                }
                            }
                            InputAction::Submit(input) => {
                                // Queue input during compaction — will be sent after session swap
                                if app.compact_task.is_some() {
                                    app.push_msg(ChatMessage::System(format!("queued: {}", input)));
                                    app.queued_message = Some(input);
                                    continue;
                                }
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
                                app.api_messages.push(json!({"role": "user", "content": api_content}));
                                let ct = CancellationToken::new();
                                let (s_tx, s_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
                                app.status_text = Some("connecting…".to_string());
                                app.streaming = true;
                                app.spinner_frame = 0;
                                let elapsed = last_frame.elapsed();
                                last_frame = Instant::now();
                                let _ = draw(&mut terminal, &mut app, &runtime, &mut boot_fx, &mut exit_fx, elapsed, &registry);
                                stream = Some(runtime.run_stream_with_messages(app.api_messages.clone(), ct.clone(), Some(s_rx)).await);
                                app.status_text = None;
                                app.push_msg(ChatMessage::Thinking("…".to_string()));
                                cancel_token = Some(ct);
                                steer_tx = Some(s_tx);
                            }
                            InputAction::StreamingInput(input) => {
                                // Check for streaming slash commands
                                if let Some(rest) = input.strip_prefix('/') {
                                    let raw_cmd = rest.split_whitespace().next().unwrap_or("");
                                    let streaming_cmds = commands::to_owned_commands(commands::STREAMING_COMMANDS);
                                    let cmd = commands::resolve_prefix(raw_cmd, &streaming_cmds);
                                    match commands::handle_streaming_command(&cmd, &input, &mut app) {
                                        CommandAction::None => {
                                            // Not a streaming-safe command. If it's still a KNOWN
                                            // command (settings, model, system, etc.), refuse with
                                            // a clear message — don't leak command text into the
                                            // model stream as steering input.
                                            let all_cmds = commands::all_commands_with_skills(&registry);
                                            let resolved_full = commands::resolve_prefix(raw_cmd, &all_cmds);
                                            if all_cmds.iter().any(|c| c == &resolved_full) {
                                                app.push_msg(ChatMessage::System(
                                                    format!("/{} can't run while streaming — press Esc to cancel first", resolved_full)
                                                ));
                                            } else {
                                                // Unknown slash text — treat as steering
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
                                        CommandAction::OpenSettings => {}
                                        CommandAction::OpenPlugins => {}
                                        CommandAction::ReloadPlugins => {}
                                        // handle_streaming_command never returns LoadSkill or Compact.
                                        CommandAction::LoadSkill { .. } => {}
                                        CommandAction::Compact { .. } => {}
                                        CommandAction::Chain => {}
                                    }
                                } else {
                                    // Normal text during streaming — steer/queue
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
                            InputAction::SettingsApply(key, value) => {
                                apply_setting(key, &value, &mut app, &mut runtime);
                            }
                            InputAction::PluginsOutcome(outcome) => {
                                if let Some(state) = app.plugins.as_mut() {
                                    use self::plugins::InputOutcome as PO;
                                    match outcome {
                                        PO::None | PO::Close => {}
                                        PO::AddMarketplace(url) => {
                                            plugins::actions::apply_add_marketplace(state, url).await;
                                        }
                                        PO::Install { marketplace, plugin } => {
                                            plugins::actions::apply_install(
                                                state, marketplace, plugin, &registry, &config,
                                            ).await;
                                        }
                                        PO::TrustAndInstall { plugin_name, host, source } => {
                                            plugins::actions::apply_trust_and_install(
                                                state, plugin_name, host, source, &registry, &config,
                                            ).await;
                                        }
                                        PO::Uninstall(name) => {
                                            plugins::actions::apply_uninstall(
                                                state, name, &registry, &config,
                                            ).await;
                                        }
                                        PO::Update(name) => {
                                            plugins::actions::apply_update(
                                                state, name, &registry, &config,
                                            ).await;
                                        }
                                        PO::RefreshMarketplace(name) => {
                                            plugins::actions::apply_refresh_marketplace(state, name).await;
                                        }
                                        PO::RemoveMarketplace(name) => {
                                            plugins::actions::apply_remove_marketplace(
                                                state, name, &registry, &config,
                                            ).await;
                                        }
                                        PO::TogglePlugin { name, enabled } => {
                                            plugins::actions::apply_toggle_plugin(
                                                state, name, enabled, &registry, &mut config,
                                            );
                                        }
                                    }
                                }
                            }
                            InputAction::OpenPluginsMarketplace => {
                                let path = synaps_cli::skills::state::PluginsState::default_path();
                                match synaps_cli::skills::state::PluginsState::load_from(&path) {
                                    Ok(file) => {
                                        app.plugins = Some(plugins::PluginsModalState::new_from_settings(file));
                                    }
                                    Err(e) => {
                                        if let Some(s) = app.settings.as_mut() {
                                            s.row_error = Some((
                                                "plugins".to_string(),
                                                format!("failed to load plugins.json: {}", e),
                                            ));
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Some(Err(_)) | None => break,
                }
            }

            // ── Stream events from runtime ──
            maybe_event = async {
                if let Some(ref mut s) = stream {
                    s.next().await
                } else {
                    std::future::pending().await
                }
            } => {
                if let Some(event) = maybe_event {
                    let do_draw = stream_handler::needs_immediate_draw(&event);
                    let action = stream_handler::handle_stream_event(event, &mut app, &runtime).await;

                    match action {
                        StreamAction::Continue => {
                            // For Done/Error, clear stream state
                            if !app.streaming {
                                stream = None;
                                cancel_token = None;
                                steer_tx = None;
                                // Reclaim gamba if running
                                if let Some(msg) = app.reclaim_gamba() {
                                    terminal.clear().ok();
                                    app.push_msg(ChatMessage::System(msg));
                                    app.invalidate();
                                    let elapsed = last_frame.elapsed();
                                    last_frame = Instant::now();
                                    let _ = draw(&mut terminal, &mut app, &runtime, &mut boot_fx, &mut exit_fx, elapsed, &registry);
                                }
                            }
                        }
                        StreamAction::AutoSendQueued(queued) => {
                            // Drop old stream state (important for cleanup)
                            drop(stream.take());
                            drop(cancel_token.take());
                            drop(steer_tx.take());
                            // Reclaim gamba if running
                            if let Some(msg) = app.reclaim_gamba() {
                                terminal.clear().ok();
                                app.push_msg(ChatMessage::System(msg));
                                app.invalidate();
                                let elapsed = last_frame.elapsed();
                                last_frame = Instant::now();
                                let _ = draw(&mut terminal, &mut app, &runtime, &mut boot_fx, &mut exit_fx, elapsed, &registry);
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
                            app.api_messages.push(json!({"role": "user", "content": api_content}));
                            let ct = CancellationToken::new();
                            let (s_tx, s_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
                            app.status_text = Some("connecting…".to_string());
                            app.streaming = true;
                            app.spinner_frame = 0;
                            let elapsed = last_frame.elapsed();
                            last_frame = Instant::now();
                            let _ = draw(&mut terminal, &mut app, &runtime, &mut boot_fx, &mut exit_fx, elapsed, &registry);
                            stream = Some(runtime.run_stream_with_messages(app.api_messages.clone(), ct.clone(), Some(s_rx)).await);
                            app.status_text = None;
                            app.push_msg(ChatMessage::Thinking("…".to_string()));
                            cancel_token = Some(ct);
                            steer_tx = Some(s_tx);
                        }
                    }

                    if do_draw {
                        let elapsed = last_frame.elapsed();
                        last_frame = Instant::now();
                        let _ = draw(&mut terminal, &mut app, &runtime, &mut boot_fx, &mut exit_fx, elapsed, &registry);
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
