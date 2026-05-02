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
mod models;
mod helpers;
mod lifecycle;
mod voice;

use app::{App, ChatMessage};
use draw::{draw, boot_effect, quit_effect};
use commands::CommandAction;
use input::InputAction;
use stream_handler::StreamAction;
use helpers::{apply_setting, fetch_usage, rebuild_display_messages};
use lifecycle::{setup_terminal, teardown_terminal};

use synaps_cli::{Runtime, StreamEvent, Result, CancellationToken, Session, latest_session, resolve_session};
use synaps_cli::core::compaction::compact_conversation;
use synaps_cli::core::session_index::SessionIndexRecord;
use crossterm::event::EventStream;
use futures::StreamExt;
use serde_json::json;
use std::time::Instant;
use tachyonfx::{Effect, Shader};


pub async fn run(
    continue_session: Option<Option<String>>,
    system: Option<String>,
    profile: Option<String>,
    no_extensions: bool,
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
    let (registry, keybind_registry) = synaps_cli::skills::register(&tools_shared, &config).await;
    let _skill_count = registry.all_skills().len();

    // Set up lazy MCP loading (if configured in ~/.synaps-cli/mcp.json)
    let mcp_server_count = synaps_cli::mcp::setup_lazy_mcp(&runtime.tools_shared()).await;

    let system_prompt_path = synaps_cli::config::resolve_read_path("system.md");

    // Session: continue existing or create new
    let mut app = match continue_session {
        Some(ref maybe_id) => {
            let session = match maybe_id {
                Some(ref id) => resolve_session(id).unwrap_or_else(|e| {
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
            match continue_session.as_ref().and_then(|o| o.as_ref()) {
                Some(q) if *q != session.id => {
                    if synaps_cli::chain::load_chain(q).is_ok() {
                        app.push_msg(ChatMessage::System(format!("  ↳ resolved via chain '{}'", q)));
                    } else if synaps_cli::session::find_session_by_name(q).is_ok() {
                        app.push_msg(ChatMessage::System(format!("  ↳ resolved via name '{}'", q)));
                    }
                }
                _ => {}
            }
            if app.abort_context.is_some() {
                app.push_msg(ChatMessage::System("⚠ abort context from previous session will be injected into next message".to_string()));
            }
            app
        }
        None => {
            App::new(Session::new(runtime.model(), runtime.thinking_level(), runtime.system_prompt()))
        }
    };
    app.keybinds = Some(keybind_registry.clone());

    // Sync the context bar denominator with the runtime's effective window
    // (respects config override like `context_window = 200k`).
    app.last_turn_context_window = runtime.context_window();
    // MCP server count logged but not shown — the banner hides the ASCII art.
    if mcp_server_count > 0 {
        tracing::info!("{} MCP servers available (use connect_mcp_server to activate)", mcp_server_count);
    }

    // ── Terminal setup ──
    let mut terminal = setup_terminal()?;
    let mut event_reader = EventStream::new();
    let mut stream: Option<std::pin::Pin<Box<dyn futures::Stream<Item = StreamEvent> + Send>>> = None;
    let (secret_prompt_tx, secret_prompt_rx) = tokio::sync::mpsc::unbounded_channel();
    let secret_prompt_handle = synaps_cli::tools::SecretPromptHandle::new(secret_prompt_tx);
    let secret_prompt_rx = std::sync::Arc::new(std::sync::Mutex::new(secret_prompt_rx));
    let mut secret_prompts = synaps_cli::tools::SecretPromptQueue::new();
    let mut cancel_token: Option<CancellationToken> = None;
    let mut steer_tx: Option<tokio::sync::mpsc::UnboundedSender<String>> = None;
    let mut boot_fx: Option<Effect> = Some(boot_effect());
    let mut exit_fx: Option<Effect> = None;
    let mut last_frame = Instant::now();

    // Start inbox watcher — file-drop ingestion for external events
    let watcher_shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watcher_task = {
        let inbox_dir = synaps_cli::config::base_dir().join("inbox");
        let event_queue = runtime.event_queue().clone();
        let shutdown = watcher_shutdown.clone();
        tokio::spawn(async move {
            synaps_cli::events::watch_inbox(inbox_dir, event_queue, shutdown).await;
        })
    };

    // Start per-session Unix socket listener + register in session registry
    let socket_shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let session_socket_path = synaps_cli::events::registry::socket_path_for_session(&app.session.id);
    let socket_task = synaps_cli::events::socket::listen_session_socket(
        session_socket_path.clone(),
        runtime.event_queue().clone(),
        socket_shutdown.clone(),
    );
    let session_registration = synaps_cli::events::registry::SessionRegistration {
        session_id: app.session.id.clone(),
        name: app.session.name.clone(),
        socket_path: session_socket_path.clone(),
        pid: std::process::id(),
        started_at: chrono::Utc::now(),
    };
    if let Err(e) = synaps_cli::events::registry::register_session(&session_registration) {
        tracing::warn!("Failed to register session: {}", e);
    }


    // ═══ Extension Discovery ═══
    // Scan ~/.synaps-cli/plugins/ for extensions and load them
    let ext_mgr = synaps_cli::extensions::manager::ExtensionManager::new_with_tools(
        std::sync::Arc::clone(runtime.hook_bus()),
        runtime.tools_shared(),
    );
    let ext_mgr_shared = std::sync::Arc::new(tokio::sync::RwLock::new(ext_mgr));
    synaps_cli::runtime::openai::set_extension_manager_for_routing(std::sync::Arc::clone(&ext_mgr_shared));
    if !no_extensions {
        let (loaded, failed) = ext_mgr_shared.write().await.discover_and_load().await;
        let handler_count = runtime.hook_bus().handler_count().await;
        tracing::info!(extensions = loaded.len(), handlers = handler_count, "Extension discovery complete");
        // Extensions load silently — only surface failures
        for failure in &failed {
            app.push_msg(ChatMessage::System(format!(
                "⚠ Extension '{}' failed: {}",
                failure.plugin,
                failure.concise_message()
            )));
        }
    }

    // ═══ HOOK: on_session_start ═══
    {
        let mut index_record = SessionIndexRecord::start(&app.session.id);
        index_record.model = Some(app.session.model.clone());
        index_record.profile = synaps_cli::core::config::get_profile();
        index_record.cwd = std::env::current_dir().ok();
        if let Err(err) = synaps_cli::core::session_index::append_record(&index_record) {
            tracing::warn!("failed to append session start index record: {}", err);
        }

        let hook_event = synaps_cli::extensions::hooks::events::HookEvent::on_session_start(&app.session.id);
        let _ = runtime.hook_bus().emit(&hook_event).await;
    }

    // ── Event loop ──
    loop {
        let elapsed = last_frame.elapsed();
        last_frame = Instant::now();
        let _ = draw(&mut terminal, &mut app, &runtime, &mut boot_fx, &mut exit_fx, elapsed, &registry, &secret_prompts);

        tokio::select! {

            // ── Ping results — fires when a model ping completes ──
            result = app.ping_rx.recv() => {
                match result {
                    Some((key, status, ms)) => {
                        if app.ping_print {
                            let detail = match status {
                                synaps_cli::runtime::openai::ping::PingStatus::Online => format!("{}ms", ms),
                                synaps_cli::runtime::openai::ping::PingStatus::RateLimited => "429 rate limited".to_string(),
                                synaps_cli::runtime::openai::ping::PingStatus::Unauthorized => "401 unauthorized".to_string(),
                                synaps_cli::runtime::openai::ping::PingStatus::NotFound => "404 not found".to_string(),
                                synaps_cli::runtime::openai::ping::PingStatus::Timeout => "timeout".to_string(),
                                synaps_cli::runtime::openai::ping::PingStatus::Error => "error".to_string(),
                            };
                            app.push_msg(ChatMessage::System(format!("  {} {:<50} — {}", status.icon(), key, detail)));
                            app.ping_pending = app.ping_pending.saturating_sub(1);
                            if app.ping_pending == 0 {
                                app.ping_print = false;
                            }
                        }
                        app.model_health.insert(key, (status, ms));
                        let elapsed = last_frame.elapsed();
                        last_frame = Instant::now();
                        let _ = draw(&mut terminal, &mut app, &runtime, &mut boot_fx, &mut exit_fx, elapsed, &registry, &secret_prompts);
                    }
                    None => {
                        // All ping tasks done (tx dropped) — stop printing
                        app.ping_print = false;
                    }
                }
            }

            // ── Expanded model-list results ──
            result = app.model_list_rx.recv() => {
                if let Some((provider_key, models_result)) = result {
                    if let Some(state) = app.models.as_mut() {
                        models::set_expanded_models(state, &provider_key, models_result);
                    }
                }
            }

            // ── Whisper model download progress — fires when the active
            //    download publishes a new progress snapshot, or completes
            //    (sender dropped → `changed()` returns Err). ──
            download_change = async {
                match app.download_rx.as_mut() {
                    Some(rx) => rx.changed().await.map(|_| rx.borrow().clone()),
                    None => std::future::pending().await,
                }
            } => {
                match download_change {
                    Ok(progress) => {
                        let done = progress.done;
                        let error = progress.error.clone();
                        app.on_download_progress(progress);
                        if done {
                            let filename = app.download_filename.clone().unwrap_or_default();
                            if let Some(err) = error {
                                app.push_msg(ChatMessage::Error(format!(
                                    "Download failed: {}", err
                                )));
                            } else {
                                app.push_msg(ChatMessage::System(format!(
                                    "✓ Downloaded {}. Now available in the model browser.",
                                    filename
                                )));
                            }
                            app.on_download_complete();
                        }
                    }
                    Err(_) => {
                        // Sender dropped without a final progress event —
                        // this should be rare (the downloader always emits
                        // done=true on success or error). Treat as silent
                        // completion to avoid hanging UI state.
                        app.on_download_complete();
                    }
                }
            }

            // ── Voice sidecar events — fires when the voice manager emits ──
            voice_event = async {
                match app.voice.as_mut() {
                    Some(v) => v.manager.next_event().await,
                    None => std::future::pending().await,
                }
            } => {
                if let Some(event) = voice_event {
                    self::voice::handle_event(&mut app, event);
                }
            }

            // ── Event bus wake — fires instantly when an event is pushed to the queue ──
            _ = runtime.event_queue().notified() => {
                let mut event_received = false;
                while let Some(event) = runtime.event_queue().pop() {
                    event_received = true;
                    let formatted = synaps_cli::events::format_event_for_agent(&event);
                    let severity_str = event.content.severity
                        .as_ref()
                        .map(|s| s.as_str().to_string())
                        .unwrap_or_else(|| "medium".to_string());
                    app.push_msg(ChatMessage::Event {
                        source: event.source.source_type.clone(),
                        severity: severity_str,
                        text: event.content.text.clone(),
                    });

                    if app.streaming || app.compact_task.is_some() {
                        // Steer into active stream if possible, otherwise buffer
                        let steered = steer_tx.as_ref()
                            .map(|tx| tx.send(formatted.clone()).is_ok())
                            .unwrap_or(false);
                        if !steered {
                            app.pending_events.push(formatted);
                        }
                    } else {
                        app.api_messages.push(serde_json::json!({
                            "role": "user",
                            "content": formatted
                        }));
                    }
                    app.invalidate();
                }

                // Auto-trigger model turn when idle — only if we actually received events
                if event_received && !app.streaming && stream.is_none() && app.compact_task.is_none() && !app.api_messages.is_empty() {
                    if let Some(last) = app.api_messages.last() {
                        if last["role"].as_str() == Some("user") {
                            let ct = CancellationToken::new();
                            let (s_tx, s_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
                            app.streaming = true;
                            app.spinner_frame = 0;
                            stream = Some(runtime.run_stream_with_messages(app.api_messages.clone(), ct.clone(), Some(s_rx), Some(secret_prompt_handle.clone())).await);
                            app.push_msg(ChatMessage::Thinking("…".to_string()));
                            cancel_token = Some(ct);
                            steer_tx = Some(s_tx);
                        }
                    }
                }
            }

            // ── Tick: animations + spinner (~60fps) ──
            _ = tokio::time::sleep(std::time::Duration::from_millis(16)), if boot_fx.is_some() || exit_fx.is_some() || app.streaming || app.compact_task.is_some() || app.messages.is_empty() || app.logo_dismiss_t.is_some() || app.logo_build_t.is_some() || app.gamba_child.is_some() || secret_prompts.is_active() => {
                secret_prompts.poll_requests(&secret_prompt_rx);
                let message_animation_needs_clear = app.needs_clear_for_animation_redraw();
                if message_animation_needs_clear {
                    if let Ok(size) = terminal.size() {
                        if size.width > 0 && size.height > 0 {
                            terminal.clear().ok();
                        }
                    }
                }
                if let Some(ref mut t) = app.logo_build_t {
                    *t += 0.025;
                    if *t >= 1.0 { app.logo_build_t = None; }
                }
                if let Some(ref mut t) = app.logo_dismiss_t {
                    *t += 0.04;
                    if *t >= 1.0 { app.logo_dismiss_t = None; }
                }
                if app.advance_animations() {
                    app.invalidate();
                }
                if let Some(msg) = app.check_gamba_exited() {
                    terminal.clear().ok();
                    app.push_msg(ChatMessage::System(msg));
                    app.invalidate();
                    let elapsed = last_frame.elapsed();
                    last_frame = Instant::now();
                    let _ = draw(&mut terminal, &mut app, &runtime, &mut boot_fx, &mut exit_fx, elapsed, &registry, &secret_prompts);
                }
                // Poll background compaction task
                if app.compact_task.as_ref().is_some_and(|t| t.is_finished()) {
                    let handle = app.compact_task.take().unwrap();
                    let msg_count = app.api_messages.len();
                    match handle.await {
                        Ok(Ok(summary)) => {
                            let old_id = app.session.id.clone();
                            // Find chains pointing at the old head before we swap
                            let chains_to_advance = synaps_cli::chain::find_all_chains_by_head(&old_id)
                                .unwrap_or_default();
                            let new_session = Session::new_from_compaction(&app.session, summary.clone());
                            let new_id = new_session.id.clone();
                            // Save new session FIRST — if we crash after this but before
                            // saving old, the new session still exists and chain is intact
                            app.session = new_session;
                            app.api_messages = app.session.api_messages.clone();
                            app.total_input_tokens = 0;
                            app.total_output_tokens = 0;
                            app.session_cost = 0.0;
                            let msgs = app.api_messages.clone();
                            rebuild_display_messages(&msgs, &mut app);
                            app.save_session().await;
                            // Load old session fresh from disk and update its forward link
                            match synaps_cli::core::session::Session::load(&old_id) {
                                Ok(mut old_session) => {
                                    old_session.compacted_into = Some(new_id.clone());
                                    // Clear name from old session — it transferred to the new one
                                    old_session.name = None;
                                    old_session.save().await.ok();
                                }
                                Err(e) => {
                                    tracing::warn!("Failed to update old session {}: {}", old_id, e);
                                }
                            }
                            let compaction_event = synaps_cli::extensions::hooks::events::HookEvent::on_compaction(
                                &old_id,
                                &new_id,
                                &summary,
                                msg_count,
                                serde_json::json!({"source": "manual"}),
                            );
                            let _ = runtime.hook_bus().emit(&compaction_event).await;

                            // Advance any named chains that pointed at the old head
                            for ch in &chains_to_advance {
                                match synaps_cli::chain::save_chain(&ch.name, &new_id) {
                                    Ok(()) => {
                                        app.push_msg(ChatMessage::System(format!(
                                            "chain '{}' advanced: {} → {}",
                                            ch.name, old_id, new_id
                                        )));
                                    }
                                    Err(e) => {
                                        app.push_msg(ChatMessage::Error(format!(
                                            "failed to advance chain '{}': {}", ch.name, e
                                        )));
                                    }
                                }
                            }
                            // Flush any events that arrived during compaction
                            for formatted in app.pending_events.drain(..) {
                                app.api_messages.push(serde_json::json!({
                                    "role": "user",
                                    "content": formatted
                                }));
                            }
                            if let Some(queued) = app.queued_message.take() {
                                app.api_messages.push(serde_json::json!({"role": "user", "content": queued}));
                                app.push_msg(ChatMessage::System(format!("queued message restored: {}", queued)));
                            }
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
                        if secret_prompts.is_active() {
                            match event {
                                crossterm::event::Event::Key(key) => match key.code {
                                    crossterm::event::KeyCode::Enter => secret_prompts.submit(),
                                    crossterm::event::KeyCode::Esc => secret_prompts.cancel(),
                                    crossterm::event::KeyCode::Backspace => secret_prompts.backspace(),
                                    crossterm::event::KeyCode::Char(c) => secret_prompts.push_char(c),
                                    _ => {}
                                },
                                crossterm::event::Event::Paste(text) => {
                                    for ch in text.chars() {
                                        secret_prompts.push_char(ch);
                                    }
                                }
                                _ => {}
                            }
                            continue;
                        }
                        let is_streaming = app.streaming;
                        let kb_guard = keybind_registry.read().expect("keybind registry poisoned");
                        let action = input::handle_event(event, &mut app, &runtime, is_streaming, &registry, &kb_guard);
                        drop(kb_guard);
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
                                // Flush any events that arrived during streaming
                                for formatted in app.pending_events.drain(..) {
                                    app.api_messages.push(serde_json::json!({
                                        "role": "user",
                                        "content": formatted
                                    }));
                                }
                                stream = None;
                                cancel_token = None;
                                steer_tx = None;
                                app.streaming = false;
                                app.subagents.clear();
                                // Cancel all running reactive subagents
                                {
                                    let mut registry = runtime.subagent_registry().lock().unwrap();
                                    for handle in registry.iter_mut_handles() {
                                        if handle.status() == synaps_cli::runtime::subagent::SubagentStatus::Running {
                                            handle.cancel();
                                        }
                                    }
                                }
                                let abort_msg = if app.abort_context.is_some() {
                                    "aborted — context saved for next message"
                                } else {
                                    "aborted"
                                };
                                app.push_msg(ChatMessage::Error(abort_msg.to_string()));
                                app.save_session().await;
                            }
                            InputAction::SlashCommand(cmd, arg) => {
                                let kb_snapshot = {
                                    let g = keybind_registry.read().expect("keybind registry poisoned");
                                    g.clone()
                                };
                                match commands::handle_command(&cmd, &arg, &mut app, &mut runtime, &system_prompt_path, &registry, &kb_snapshot).await {
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
                                    CommandAction::OpenModels => {
                                        app.models = Some(models::ModelsModalState::new());
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
                                        let _ = draw(&mut terminal, &mut app, &runtime, &mut boot_fx, &mut exit_fx, elapsed, &registry, &secret_prompts);
                                        stream = Some(runtime.run_stream_with_messages(app.api_messages.clone(), ct.clone(), Some(s_rx), Some(secret_prompt_handle.clone())).await);
                                        app.status_text = None;
                                        app.push_msg(ChatMessage::Thinking("…".to_string()));
                                        cancel_token = Some(ct);
                                        steer_tx = Some(s_tx);
                                    }
                                    CommandAction::PluginCommand { command, arg } => {
                                        commands::execute_command_action(
                                            CommandAction::PluginCommand { command, arg },
                                            &mut app,
                                            &runtime,
                                        ).await;
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

                                        // Show any named chain bookmarking the active head
                                        match synaps_cli::chain::find_all_chains_by_head(&app.session.id) {
                                            Ok(named) if !named.is_empty() => {
                                                let names: Vec<String> = named.iter().map(|c| format!("@{}", c.name)).collect();
                                                app.push_msg(ChatMessage::System(format!(
                                                    "bookmarked by: {}", names.join(", ")
                                                )));
                                            }
                                            _ => {}
                                        }
                                    }
                                    CommandAction::ChainList => {
                                        match synaps_cli::chain::list_chains() {
                                            Ok(chains) if chains.is_empty() => {
                                                app.push_msg(ChatMessage::System("no named chains".to_string()));
                                            }
                                            Ok(chains) => {
                                                app.push_msg(ChatMessage::System(format!("{} chain(s):", chains.len())));
                                                for c in chains {
                                                    let active = if c.head == app.session.id { " *" } else { "" };
                                                    app.push_msg(ChatMessage::System(format!(
                                                        "  @{} → {}{}", c.name, c.head, active
                                                    )));
                                                }
                                            }
                                            Err(e) => {
                                                app.push_msg(ChatMessage::Error(format!("failed to list chains: {}", e)));
                                            }
                                        }
                                    }
                                    CommandAction::ChainName { name } => {
                                        match synaps_cli::chain::save_chain(&name, &app.session.id) {
                                            Ok(()) => {
                                                app.push_msg(ChatMessage::System(format!(
                                                    "chain '{}' → {}", name, app.session.id
                                                )));
                                            }
                                            Err(e) => {
                                                app.push_msg(ChatMessage::Error(format!("chain name failed: {}", e)));
                                            }
                                        }
                                    }
                                    CommandAction::ChainUnname { name } => {
                                        match synaps_cli::chain::delete_chain(&name) {
                                            Ok(()) => {
                                                app.push_msg(ChatMessage::System(format!("chain '{}' deleted", name)));
                                            }
                                            Err(e) => {
                                                app.push_msg(ChatMessage::Error(format!("chain unname failed: {}", e)));
                                            }
                                        }
                                    }
                                    CommandAction::Status => {
                                        if runtime.model().contains('/') {
                                            app.push_msg(ChatMessage::System("Usage stats are only available for Anthropic models.".to_string()));
                                        } else {
                                            app.push_msg(ChatMessage::System("Checking usage...".to_string()));
                                            match fetch_usage().await {
                                                Ok(lines) => {
                                                    for line in lines {
                                                        app.push_msg(ChatMessage::System(line));
                                                    }
                                                }
                                                Err(e) => app.push_msg(ChatMessage::Error(format!("Usage check failed: {}", e))),
                                            }
                                        }
                                    }
                                    CommandAction::ExtensionsStatus => {
                                        let manager = ext_mgr_shared.read().await;
                                        let snapshots = manager.capability_snapshots().await;
                                        let trust_view = manager.provider_trust_view();
                                        if snapshots.is_empty() {
                                            app.push_msg(ChatMessage::System("No extensions loaded.".to_string()));
                                        } else {
                                            app.push_msg(ChatMessage::System(format!("Extensions ({}):", snapshots.len())));
                                            for snap in &snapshots {
                                                app.push_msg(ChatMessage::System(format!(
                                                    "  {} — {} (restarts: {})",
                                                    snap.id,
                                                    snap.health.as_str(),
                                                    snap.restart_count
                                                )));
                                                if !snap.hooks.is_empty() {
                                                    let rendered = snap
                                                        .hooks
                                                        .iter()
                                                        .map(|h| match &h.tool_filter {
                                                            Some(t) => format!("{}[{}]", h.kind, t),
                                                            None => h.kind.clone(),
                                                        })
                                                        .collect::<Vec<_>>()
                                                        .join(", ");
                                                    app.push_msg(ChatMessage::System(format!("    hooks: {}", rendered)));
                                                }
                                                if !snap.tools.is_empty() {
                                                    let rendered = snap
                                                        .tools
                                                        .iter()
                                                        .map(|t| t.name.clone())
                                                        .collect::<Vec<_>>()
                                                        .join(", ");
                                                    app.push_msg(ChatMessage::System(format!("    tools: {}", rendered)));
                                                }
                                                // Voice capabilities (grouped from the `future` list).
                                                let voice_entries: Vec<&str> = snap
                                                    .future
                                                    .iter()
                                                    .filter(|e| e.kind == "voice")
                                                    .map(|e| e.name.as_str())
                                                    .collect();
                                                if !voice_entries.is_empty() {
                                                    // Each entry is "<name> (<mode>)". Group by name; collect modes.
                                                    use std::collections::BTreeMap;
                                                    let mut grouped: BTreeMap<String, Vec<String>> = BTreeMap::new();
                                                    for entry in &voice_entries {
                                                        if let Some(open) = entry.rfind(" (") {
                                                            if entry.ends_with(')') {
                                                                let name = entry[..open].to_string();
                                                                let mode = entry[open + 2..entry.len() - 1].to_string();
                                                                grouped.entry(name).or_default().push(mode);
                                                                continue;
                                                            }
                                                        }
                                                        grouped.entry((*entry).to_string()).or_default();
                                                    }
                                                    for (name, modes) in grouped {
                                                        let modes_str = modes.join("/");
                                                        app.push_msg(ChatMessage::System(format!(
                                                            "    voice: {} [{}]",
                                                            name, modes_str
                                                        )));
                                                    }
                                                }
                                                for provider in &snap.providers {
                                                    let disabled_suffix = match trust_view.get(&provider.runtime_id) {
                                                        Some(false) => " [disabled]",
                                                        _ => "",
                                                    };
                                                    app.push_msg(ChatMessage::System(format!(
                                                        "    provider {} — {}{}",
                                                        provider.runtime_id,
                                                        provider.display_name,
                                                        disabled_suffix
                                                    )));
                                                    for model in &provider.models {
                                                        let mut badges: Vec<&str> = Vec::new();
                                                        if model.tool_use { badges.push("tool-use"); }
                                                        if model.streaming { badges.push("streaming"); }
                                                        let label = if badges.is_empty() {
                                                            model.runtime_id.clone()
                                                        } else {
                                                            let suffix = badges.iter().map(|b| format!("[{}]", b)).collect::<Vec<_>>().join(" ");
                                                            format!("{} {}", model.runtime_id, suffix)
                                                        };
                                                        app.push_msg(ChatMessage::System(format!("      model {}", label)));
                                                    }
                                                }
                                                // Surface config diagnostics warnings (no values printed).
                                                if let Some(diag) = manager.config_diagnostics(&snap.id) {
                                                    let missing_required: Vec<&str> = diag
                                                        .entries
                                                        .iter()
                                                        .filter(|e| e.required && matches!(e.source, synaps_cli::extensions::config::ConfigSource::Missing))
                                                        .map(|e| e.key.as_str())
                                                        .collect();
                                                    if !missing_required.is_empty() {
                                                        app.push_msg(ChatMessage::System(format!(
                                                            "    ⚠ missing required config: {}",
                                                            missing_required.join(", ")
                                                        )));
                                                    }
                                                    // Group provider_missing by provider id.
                                                    let mut by_provider: std::collections::BTreeMap<&str, Vec<&str>> = std::collections::BTreeMap::new();
                                                    for (pid, key) in &diag.provider_missing {
                                                        by_provider.entry(pid.as_str()).or_default().push(key.as_str());
                                                    }
                                                    for (pid, keys) in by_provider {
                                                        app.push_msg(ChatMessage::System(format!(
                                                            "    ⚠ provider {} missing required config: {}",
                                                            pid,
                                                            keys.join(", ")
                                                        )));
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    CommandAction::ExtensionsConfig { id } => {
                                        let manager = ext_mgr_shared.read().await;
                                        let diags: Vec<synaps_cli::extensions::config::ExtensionConfigDiagnostics> = match &id {
                                            Some(want) => match manager.config_diagnostics(want) {
                                                Some(d) => vec![d],
                                                None => {
                                                    app.push_msg(ChatMessage::Error(format!(
                                                        "extension not found: {}",
                                                        want
                                                    )));
                                                    Vec::new()
                                                }
                                            },
                                            None => manager.all_config_diagnostics(),
                                        };
                                        if diags.is_empty() && id.is_none() {
                                            app.push_msg(ChatMessage::System("No extensions loaded.".to_string()));
                                        }
                                        for diag in diags {
                                            app.push_msg(ChatMessage::System(format!(
                                                "Extension {} config:",
                                                diag.extension_id
                                            )));
                                            if diag.entries.is_empty() {
                                                app.push_msg(ChatMessage::System("  (no manifest config entries)".to_string()));
                                            }
                                            for entry in &diag.entries {
                                                let source_label = match &entry.source {
                                                    synaps_cli::extensions::config::ConfigSource::EnvOverride(name) => format!("env override ({})", name),
                                                    synaps_cli::extensions::config::ConfigSource::SecretEnv(name) => format!("secret env ({})", name),
                                                    synaps_cli::extensions::config::ConfigSource::ConfigKey(name) => format!("config key ({})", name),
                                                    synaps_cli::extensions::config::ConfigSource::Default => "default".to_string(),
                                                    synaps_cli::extensions::config::ConfigSource::Missing => "missing".to_string(),
                                                };
                                                let req = if entry.required { " [required]" } else { "" };
                                                app.push_msg(ChatMessage::System(format!(
                                                    "  {}{} — source: {}, has_value: {}",
                                                    entry.key, req, source_label, entry.has_value
                                                )));
                                                if let Some(desc) = &entry.description {
                                                    app.push_msg(ChatMessage::System(format!(
                                                        "    description: {}",
                                                        desc
                                                    )));
                                                }
                                            }
                                            for (pid, key) in &diag.provider_missing {
                                                app.push_msg(ChatMessage::System(format!(
                                                    "  ⚠ provider {} requires config '{}' (no manifest entry)",
                                                    pid, key
                                                )));
                                            }
                                        }
                                    }

                                    CommandAction::ExtensionsTrust(action) => {
                                        use crate::chatui::commands::ExtensionsTrustAction;
                                        match action {
                                            ExtensionsTrustAction::List => {
                                                let manager = ext_mgr_shared.read().await;
                                                let providers = manager.provider_summaries();
                                                let trust = synaps_cli::extensions::trust::load_trust_state().unwrap_or_default();
                                                if providers.is_empty() {
                                                    app.push_msg(ChatMessage::System("No providers registered.".to_string()));
                                                } else {
                                                    app.push_msg(ChatMessage::System(format!("Provider trust ({}):", providers.len())));
                                                    for p in providers {
                                                        let suffix = match trust.disabled.get(&p.runtime_id) {
                                                            Some(entry) if entry.disabled => match &entry.reason {
                                                                Some(r) => format!(" [disabled ({})]", r),
                                                                None => " [disabled]".to_string(),
                                                            },
                                                            _ => " [enabled]".to_string(),
                                                        };
                                                        app.push_msg(ChatMessage::System(format!(
                                                            "  {}{}",
                                                            p.runtime_id, suffix
                                                        )));
                                                    }
                                                }
                                            }
                                            ExtensionsTrustAction::Enable { runtime_id } => {
                                                match synaps_cli::extensions::trust::load_trust_state() {
                                                    Ok(mut state) => {
                                                        synaps_cli::extensions::trust::enable_provider(&mut state, &runtime_id);
                                                        match synaps_cli::extensions::trust::save_trust_state(&state) {
                                                            Ok(()) => app.push_msg(ChatMessage::System(format!(
                                                                "Provider '{}' enabled.", runtime_id
                                                            ))),
                                                            Err(e) => app.push_msg(ChatMessage::Error(format!(
                                                                "failed to save trust state: {}", e
                                                            ))),
                                                        }
                                                    }
                                                    Err(e) => app.push_msg(ChatMessage::Error(format!(
                                                        "failed to load trust state: {}", e
                                                    ))),
                                                }
                                            }
                                            ExtensionsTrustAction::Disable { runtime_id, reason } => {
                                                match synaps_cli::extensions::trust::load_trust_state() {
                                                    Ok(mut state) => {
                                                        synaps_cli::extensions::trust::disable_provider(&mut state, &runtime_id, reason.clone());
                                                        match synaps_cli::extensions::trust::save_trust_state(&state) {
                                                            Ok(()) => {
                                                                let suffix = match &reason {
                                                                    Some(r) => format!(" [reason: {}]", r),
                                                                    None => String::new(),
                                                                };
                                                                app.push_msg(ChatMessage::System(format!(
                                                                    "Provider '{}' disabled.{}", runtime_id, suffix
                                                                )));
                                                            }
                                                            Err(e) => app.push_msg(ChatMessage::Error(format!(
                                                                "failed to save trust state: {}", e
                                                            ))),
                                                        }
                                                    }
                                                    Err(e) => app.push_msg(ChatMessage::Error(format!(
                                                        "failed to load trust state: {}", e
                                                    ))),
                                                }
                                            }
                                        }
                                    }
                                    CommandAction::ExtensionsAudit { tail } => {
                                        match synaps_cli::extensions::audit::read_audit_entries() {
                                            Ok(entries) => {
                                                let slice: Vec<_> = match tail {
                                                    Some(n) if entries.len() > n => entries[entries.len() - n..].to_vec(),
                                                    _ => entries,
                                                };
                                                if slice.is_empty() {
                                                    app.push_msg(ChatMessage::System("No audit entries yet.".to_string()));
                                                } else {
                                                    app.push_msg(ChatMessage::System(format!("Audit ({} entries):", slice.len())));
                                                    for e in slice {
                                                        let stream_tag = if e.streamed { "[streamed]" } else { "[complete]" };
                                                        let class_part = match &e.error_class {
                                                            Some(c) => format!(" class={}", c),
                                                            None => String::new(),
                                                        };
                                                        let tools_part = if e.tools_requested > 0 {
                                                            format!(" tools={}", e.tools_requested)
                                                        } else {
                                                            String::new()
                                                        };
                                                        app.push_msg(ChatMessage::System(format!(
                                                            "  {} {}:{} {} outcome={}{}{}",
                                                            e.timestamp,
                                                            e.provider_id,
                                                            e.model_id,
                                                            stream_tag,
                                                            e.outcome,
                                                            class_part,
                                                            tools_part,
                                                        )));
                                                    }
                                                }
                                            }
                                            Err(e) => app.push_msg(ChatMessage::Error(format!(
                                                "failed to read audit log: {}", e
                                            ))),
                                        }
                                    }
                                    CommandAction::ExtensionsMemory(action) => {
                                        use crate::chatui::commands::ExtensionsMemoryAction;
                                        match action {
                                            ExtensionsMemoryAction::Namespaces => {
                                                match synaps_cli::memory::store::list_namespaces() {
                                                    Ok(nss) if nss.is_empty() => {
                                                        app.push_msg(ChatMessage::System(
                                                            "No memory namespaces.".to_string(),
                                                        ));
                                                    }
                                                    Ok(nss) => {
                                                        app.push_msg(ChatMessage::System(format!(
                                                            "Memory namespaces ({}):", nss.len()
                                                        )));
                                                        for ns in nss {
                                                            app.push_msg(ChatMessage::System(format!("  {}", ns)));
                                                        }
                                                    }
                                                    Err(e) => app.push_msg(ChatMessage::Error(format!(
                                                        "failed to list memory namespaces: {}", e
                                                    ))),
                                                }
                                            }
                                            ExtensionsMemoryAction::Recent { namespace, limit } => {
                                                let q = synaps_cli::memory::store::MemoryQuery {
                                                    limit: Some(limit.unwrap_or(20)),
                                                    ..Default::default()
                                                };
                                                match synaps_cli::memory::store::query(&namespace, &q) {
                                                    Ok(records) if records.is_empty() => {
                                                        app.push_msg(ChatMessage::System(format!(
                                                            "No records in '{}'.", namespace
                                                        )));
                                                    }
                                                    Ok(records) => {
                                                        app.push_msg(ChatMessage::System(format!(
                                                            "Recent in '{}' ({}):", namespace, records.len()
                                                        )));
                                                        for rec in records {
                                                            // ISO8601 / RFC3339 UTC from epoch ms via chrono.
                                                            let ts = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(
                                                                rec.timestamp_ms as i64,
                                                            )
                                                            .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
                                                            .unwrap_or_else(|| rec.timestamp_ms.to_string());
                                                            // Truncate content at 80 chars (char-aware).
                                                            let mut content: String = rec.content.chars().take(80).collect();
                                                            if rec.content.chars().count() > 80 {
                                                                content.push('…');
                                                            }
                                                            let tags = if rec.tags.is_empty() {
                                                                "[]".to_string()
                                                            } else {
                                                                format!("[{}]", rec.tags.join(", "))
                                                            };
                                                            // NOTE: meta intentionally not displayed (privacy).
                                                            app.push_msg(ChatMessage::System(format!(
                                                                "  {} {} {}", ts, tags, content
                                                            )));
                                                        }
                                                    }
                                                    Err(e) => app.push_msg(ChatMessage::Error(format!(
                                                        "failed to query memory '{}': {}", namespace, e
                                                    ))),
                                                }
                                            }
                                        }
                                    }

                                    CommandAction::Ping => {
                                        app.push_msg(ChatMessage::System("📡 Pinging models...".to_string()));
                                        app.ping_print = true;
                                        let client = runtime.http_client().clone();
                                        let provider_keys = synaps_cli::config::get_provider_keys();
                                        // Count how many models will be pinged
                                        let count: usize = synaps_cli::runtime::openai::registry::providers().iter()
                                            .filter(|s| synaps_cli::runtime::openai::registry::resolve_provider_model(s.key, s.default_model, &provider_keys).is_some())
                                            .map(|s| s.models.len())
                                            .sum();
                                        app.ping_pending = count;
                                        let health_tx = app.ping_tx.clone();
                                        tokio::spawn(async move {
                                            synaps_cli::runtime::openai::ping::ping_all_configured(
                                                &client, &provider_keys, health_tx,
                                            ).await;
                                        });
                                    }

                                    CommandAction::VoiceToggle => {
                                        // First toggle: spawn the sidecar and start listening.
                                        if app.voice.is_none() {
                                            match self::voice::VoiceUiState::spawn_default().await {
                                                Ok(state) => {
                                                    app.voice = Some(state);
                                                    app.push_msg(ChatMessage::System(
                                                        "🎤 voice sidecar online — press the toggle again to stop and transcribe".to_string()
                                                    ));
                                                    if let Some(v) = app.voice.as_mut() {
                                                        v.armed = true;
                                                        if let Err(err) = v.manager.press().await {
                                                            v.armed = false;
                                                            v.status = self::voice::VoiceUiStatus::Error(err.to_string());
                                                            app.push_msg(ChatMessage::Error(format!("voice press failed: {err}")));
                                                        }
                                                    }
                                                }
                                                Err(err) => {
                                                    app.push_msg(ChatMessage::Error(format!("voice unavailable: {err}")));
                                                }
                                            }
                                        } else {
                                            // Subsequent toggle: arm flag is the source of truth.
                                            // (status flaps between Listening/Idle as the VAD
                                            // delivers utterances, so we can't key off it.)
                                            let v = app.voice.as_mut().unwrap();
                                            if v.armed {
                                                v.armed = false;
                                                if let Err(err) = v.manager.release().await {
                                                    app.push_msg(ChatMessage::Error(format!("voice release failed: {err}")));
                                                }
                                                app.push_msg(ChatMessage::System(
                                                    "voice: stopping — final transcript will be appended".to_string()
                                                ));
                                            } else {
                                                v.armed = true;
                                                if let Err(err) = v.manager.press().await {
                                                    v.armed = false;
                                                    app.push_msg(ChatMessage::Error(format!("voice press failed: {err}")));
                                                }
                                            }
                                        }
                                    }

                                    CommandAction::VoiceStatus => {
                                        let line = match app.voice.as_ref() {
                                            Some(v) => v.status_line(),
                                            None => match synaps_cli::voice::discovery::discover() {
                                                Some(s) => format!(
                                                    "voice: not yet started — sidecar available from plugin '{}' at {}",
                                                    s.plugin_name,
                                                    s.binary.display()
                                                ),
                                                None => "voice: no plugin provides a voice sidecar (install local-voice from synaps-skills)".to_string(),
                                            },
                                        };
                                        app.push_msg(ChatMessage::System(line));
                                    }

                                    CommandAction::VoiceModels => {
                                        let dir = commands::voice_models_dir();
                                        app.push_msg(ChatMessage::System(commands::render_models_table(&dir)));
                                    }

                                    CommandAction::VoiceHelp => {
                                        app.push_msg(ChatMessage::System(commands::voice_help_text()));
                                    }

                                    CommandAction::VoiceDownload { id } => {
                                        match synaps_cli::voice::models::find_by_id(&id) {
                                            None => {
                                                let valid: Vec<&str> = synaps_cli::voice::models::CATALOG
                                                    .iter()
                                                    .map(|e| e.id)
                                                    .collect();
                                                app.push_msg(ChatMessage::Error(format!(
                                                    "unknown model id '{}'. Valid: {}",
                                                    id,
                                                    valid.join(", ")
                                                )));
                                            }
                                            Some(entry) => {
                                                if app.voice_download_in_flight || app.download_rx.is_some() {
                                                    app.push_msg(ChatMessage::System(
                                                        "A model download is already in progress.".to_string(),
                                                    ));
                                                } else {
                                                    // Non-blocking: spawn the download on a worker
                                                    // task and let the main event loop's
                                                    // `download_change` arm consume progress
                                                    // ticks. The chat area renders a sticky
                                                    // tqdm-style progress bar above the input
                                                    // box while in flight.
                                                    let (tx, rx) = tokio::sync::watch::channel(
                                                        synaps_cli::voice::download::DownloadProgress::default(),
                                                    );
                                                    let filename = entry.filename.to_string();
                                                    let size_mb = entry.size_mb;
                                                    let started = app.start_download(filename.clone(), rx);
                                                    debug_assert!(started);
                                                    let models_dir = commands::voice_models_dir();
                                                    let entry_static = entry;
                                                    tokio::spawn(async move {
                                                        let _ = synaps_cli::voice::download::download_model(
                                                            entry_static,
                                                            &models_dir,
                                                            tx,
                                                            false,
                                                        )
                                                        .await;
                                                    });
                                                    app.push_msg(ChatMessage::System(format!(
                                                        "Downloading {} ({} MB) from huggingface.co…",
                                                        filename, size_mb
                                                    )));
                                                }
                                            }
                                        }
                                    }

                                    CommandAction::VoiceRebuild { backend } => {
                                        // Resolve target backend: explicit arg > configured setting > auto.
                                        let configured = backend.unwrap_or_else(|| {
                                            synaps_cli::config::read_config_value("voice_stt_backend")
                                                .map(|v| v.trim().to_string())
                                                .filter(|v| !v.is_empty())
                                                .unwrap_or_else(|| "auto".to_string())
                                        });
                                        let target = synaps_cli::voice::rebuild::resolve_backend(&configured);

                                        let sidecar = synaps_cli::voice::discovery::discover();
                                        let Some(sidecar) = sidecar else {
                                            app.push_msg(ChatMessage::Error(
                                                "no voice plugin installed; cannot rebuild".to_string(),
                                            ));
                                            continue;
                                        };

                                        app.push_msg(ChatMessage::System(format!(
                                            "voice: rebuilding sidecar with backend '{}' (selected '{}')…",
                                            target, configured
                                        )));

                                        let plugin_root = sidecar.plugin_root.clone();
                                        let binary = sidecar.binary.clone();
                                        let target_clone = target.clone();
                                        let (tx, mut rx) = tokio::sync::mpsc::channel::<
                                            synaps_cli::voice::rebuild::RebuildEvent,
                                        >(64);

                                        tokio::spawn(async move {
                                            synaps_cli::voice::rebuild::rebuild_with_backend(
                                                plugin_root,
                                                binary,
                                                target_clone,
                                                tx,
                                            )
                                            .await;
                                        });

                                        // Drain events synchronously so output appears in chronological
                                        // order (the rebuild blocks the UI; richer streaming is a follow-up).
                                        while let Some(evt) = rx.recv().await {
                                            match evt {
                                                synaps_cli::voice::rebuild::RebuildEvent::Output(o) => {
                                                    let prefix = if o.from_stderr { "stderr" } else { "stdout" };
                                                    app.push_msg(ChatMessage::System(format!(
                                                        "[rebuild:{}] {}",
                                                        prefix, o.line
                                                    )));
                                                }
                                                synaps_cli::voice::rebuild::RebuildEvent::Done {
                                                    exit_code,
                                                    new_backend,
                                                } => {
                                                    let backend_str = new_backend
                                                        .as_deref()
                                                        .unwrap_or("(unknown — reprobe failed)");
                                                    if exit_code == 0 {
                                                        if let Some(v) = app.voice.as_mut() {
                                                            v.compiled_backend = new_backend.clone();
                                                        }
                                                        // Also update the eager cache so /settings
                                                        // reflects the new backend immediately even
                                                        // when no live voice session exists.
                                                        app.cached_voice_compiled_backend = new_backend.clone();
                                                        app.push_msg(ChatMessage::System(format!(
                                                            "✓ voice rebuild complete (exit 0). Backend now: {}",
                                                            backend_str
                                                        )));
                                                    } else {
                                                        app.push_msg(ChatMessage::Error(format!(
                                                            "voice rebuild exited with code {}. Backend reported: {}",
                                                            exit_code, backend_str
                                                        )));
                                                    }
                                                    break;
                                                }
                                                synaps_cli::voice::rebuild::RebuildEvent::Failed(msg) => {
                                                    app.push_msg(ChatMessage::Error(format!(
                                                        "voice rebuild failed: {}",
                                                        msg
                                                    )));
                                                    break;
                                                }
                                            }
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
                                let display_text = app.user_display_text_for_submission(&input);
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
                                let _ = draw(&mut terminal, &mut app, &runtime, &mut boot_fx, &mut exit_fx, elapsed, &registry, &secret_prompts);
                                stream = Some(runtime.run_stream_with_messages(app.api_messages.clone(), ct.clone(), Some(s_rx), Some(secret_prompt_handle.clone())).await);
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
                                        CommandAction::OpenModels => {}
                                        CommandAction::OpenSettings => {}
                                        CommandAction::OpenPlugins => {}
                                        CommandAction::ReloadPlugins => {}
                                        // handle_streaming_command never returns LoadSkill, PluginCommand, or Compact.
                                        CommandAction::LoadSkill { .. } => {}
                                        CommandAction::PluginCommand { .. } => {}
                                        CommandAction::Compact { .. } => {}
                                        CommandAction::Chain => {}
                                        CommandAction::ChainList => {}
                                        CommandAction::ChainName { .. } => {}
                                        CommandAction::ChainUnname { .. } => {}
                                        CommandAction::Status => {}
                                        CommandAction::ExtensionsStatus => {}
                                        CommandAction::ExtensionsConfig { .. } => {}
                                        CommandAction::ExtensionsTrust(_) => {}
                                        CommandAction::ExtensionsAudit { .. } => {}
                                        CommandAction::ExtensionsMemory(_) => {}
                                        CommandAction::Ping => {}
                                        CommandAction::VoiceToggle => {}
                                        CommandAction::VoiceStatus => {}
                                        CommandAction::VoiceModels => {}
                                        CommandAction::VoiceDownload { .. } => {}
                                        CommandAction::VoiceHelp => {}
                                        CommandAction::VoiceRebuild { .. } => {}
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
                            InputAction::ModelsApply(model) => {
                                runtime.set_model(model.clone());
                                let applied = runtime.model().to_string();
                                let _ = synaps_cli::config::write_config_value("model", &applied);
                                app.session.model = applied.clone();
                                app.push_msg(ChatMessage::System(format!("model set to: {}", applied)));
                            }
                            InputAction::ModelsExpandProvider(provider_key) => {
                                if provider_key.contains(':') {
                                    let tx = app.model_list_tx.clone();
                                    let manager = synaps_cli::runtime::openai::extension_manager_for_routing();
                                    tokio::spawn(async move {
                                        let result = if let Some(manager) = manager {
                                            let manager = manager.read().await;
                                            if let Some(provider) = manager.provider(&provider_key) {
                                                Ok(provider.spec.models.iter().map(|model| {
                                                    let full_id = synaps_cli::extensions::providers::ProviderRegistry::model_runtime_id(
                                                        &provider.plugin_id,
                                                        &provider.provider_id,
                                                        &model.id,
                                                    );
                                                    let mut metadata = vec![format!("plugin {}", provider.plugin_id)];
                                                    metadata.push(format!("provider {}", provider.provider_id));
                                                    if let Some(context) = model.context_window {
                                                        metadata.push(if context >= 1_000_000 {
                                                            format!("{}M ctx", context / 1_000_000)
                                                        } else if context >= 1_000 {
                                                            format!("{}K ctx", context / 1_000)
                                                        } else {
                                                            format!("{context} ctx")
                                                        });
                                                    }
                                                    if model.capabilities.get("tool_use").and_then(|value| value.as_bool()).unwrap_or(false) {
                                                        metadata.push("tool-use".to_string());
                                                    }
                                                    models::ExpandedModelEntry::with_metadata(
                                                        full_id,
                                                        model.display_name.clone().unwrap_or_else(|| model.id.clone()),
                                                        false,
                                                        metadata,
                                                    )
                                                }).collect())
                                            } else {
                                                Err(format!("extension provider '{}' is not loaded", provider_key))
                                            }
                                        } else {
                                            Err("extension provider registry is not available".to_string())
                                        };
                                        let _ = tx.send((provider_key, result));
                                    });
                                    continue;
                                }
                                let client = runtime.http_client().clone();
                                let provider_keys = synaps_cli::config::get_provider_keys();
                                let tx = app.model_list_tx.clone();
                                tokio::spawn(async move {
                                    let result = synaps_cli::runtime::openai::catalog::fetch_catalog_models(
                                        &client,
                                        &provider_key,
                                        &provider_keys,
                                    ).await.map(|models| {
                                        models.into_iter().map(|model| {
                                            let full_id = model.runtime_id();
                                            let label = model.display_label().to_string();
                                            let mut metadata = Vec::new();
                                            if let Some(context) = model.context_tokens {
                                                metadata.push(if context >= 1_000_000 {
                                                    format!("{}M ctx", context / 1_000_000)
                                                } else if context >= 1_000 {
                                                    format!("{}K ctx", context / 1_000)
                                                } else {
                                                    format!("{context} ctx")
                                                });
                                            }
                                            match model.reasoning {
                                                synaps_cli::runtime::openai::catalog::ReasoningSupport::None => {}
                                                synaps_cli::runtime::openai::catalog::ReasoningSupport::Unknown => {}
                                                _ => metadata.push("thinking".to_string()),
                                            }
                                            if model.pricing.has_internal_reasoning_cost() {
                                                metadata.push("reasoning $".to_string());
                                            }
                                            models::ExpandedModelEntry::with_metadata(full_id, label, false, metadata)
                                        }).collect()
                                    });
                                    let _ = tx.send((provider_key, result));
                                });
                            }
                            InputAction::SettingsApply(key, value) => {
                                apply_setting(key, &value, &mut app, &mut runtime);
                            }
                            InputAction::StartModelDownload(id) => {
                                let entry_opt = synaps_cli::voice::models::find_by_id(&id);
                                let entry = match entry_opt {
                                    Some(e) => e,
                                    None => {
                                        app.push_msg(ChatMessage::Error(format!(
                                            "unknown model id '{}'", id
                                        )));
                                        continue;
                                    }
                                };
                                if app.voice_download_in_flight || app.download_rx.is_some() {
                                    app.push_msg(ChatMessage::System(
                                        "A model download is already in progress.".to_string(),
                                    ));
                                    continue;
                                }
                                let (tx, rx) = tokio::sync::watch::channel(
                                    synaps_cli::voice::download::DownloadProgress::default(),
                                );
                                let filename = entry.filename.to_string();
                                let size_mb = entry.size_mb;
                                let started = app.start_download(filename.clone(), rx);
                                debug_assert!(started);
                                let models_dir = commands::voice_models_dir();
                                let entry_static = entry;
                                tokio::spawn(async move {
                                    let _ = synaps_cli::voice::download::download_model(
                                        entry_static,
                                        &models_dir,
                                        tx,
                                        false,
                                    )
                                    .await;
                                    // The downloader publishes a terminal
                                    // progress (done=true, with optional error)
                                    // before returning. Dropping the sender
                                    // here causes `rx.changed()` in the host
                                    // loop to error, which triggers cleanup.
                                });
                                app.push_msg(ChatMessage::System(format!(
                                    "Downloading {} ({} MB) from huggingface.co...",
                                    filename, size_mb
                                )));
                            }
                            InputAction::PluginsOutcome(outcome) => {
                                if let Some(state) = app.plugins.as_mut() {
                                    use self::plugins::InputOutcome as PO;
                                    match outcome {
                                        PO::None | PO::Close => {}
                                        PO::AddMarketplace(url) => {
                                            plugins::actions::apply_add_marketplace(state, url).await;
                                        }
                                        PO::InstallRequested { marketplace, plugin } => {
                                            plugins::actions::apply_install(
                                                state, marketplace, plugin, &registry, &config,
                                            ).await;
                                        }
                                        PO::TrustAndInstall { plugin_name, host, source, summary } => {
                                            plugins::actions::apply_trust_and_install(
                                                state, plugin_name, host, source, summary, &registry, &config,
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
                                        PO::ConfirmPendingInstall => {
                                            plugins::actions::apply_confirm_pending_install(state, &registry, &config).await;
                                        }
                                        PO::CancelPendingInstall => {
                                            plugins::actions::apply_cancel_pending_install(state);
                                        }
                                        PO::ConfirmPendingUpdate => {
                                            plugins::actions::apply_confirm_pending_update(state, &registry, &config).await;
                                        }
                                        PO::CancelPendingUpdate => {
                                            plugins::actions::apply_cancel_pending_update(state);
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
                                        PO::EnablePluginRequested(name) => {
                                            plugins::actions::confirm_enable_plugin(state, name);
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
                            InputAction::PingModels => {
                                let client = runtime.http_client().clone();
                                let provider_keys = synaps_cli::config::get_provider_keys();
                                let health_tx = app.ping_tx.clone();
                                tokio::spawn(async move {
                                    synaps_cli::runtime::openai::ping::ping_all_configured(
                                        &client, &provider_keys, health_tx,
                                    ).await;
                                });
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
                                    let _ = draw(&mut terminal, &mut app, &runtime, &mut boot_fx, &mut exit_fx, elapsed, &registry, &secret_prompts);
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
                                let _ = draw(&mut terminal, &mut app, &runtime, &mut boot_fx, &mut exit_fx, elapsed, &registry, &secret_prompts);
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
                            let _ = draw(&mut terminal, &mut app, &runtime, &mut boot_fx, &mut exit_fx, elapsed, &registry, &secret_prompts);
                            stream = Some(runtime.run_stream_with_messages(app.api_messages.clone(), ct.clone(), Some(s_rx), Some(secret_prompt_handle.clone())).await);
                            app.status_text = None;
                            app.push_msg(ChatMessage::Thinking("…".to_string()));
                            cancel_token = Some(ct);
                            steer_tx = Some(s_tx);
                        }
                        StreamAction::AutoTriggerEvents => {
                            drop(stream.take());
                            drop(cancel_token.take());
                            drop(steer_tx.take());
                            let ct = CancellationToken::new();
                            let (s_tx, s_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
                            app.streaming = true;
                            app.spinner_frame = 0;
                            stream = Some(runtime.run_stream_with_messages(app.api_messages.clone(), ct.clone(), Some(s_rx), Some(secret_prompt_handle.clone())).await);
                            app.push_msg(ChatMessage::Thinking("…".to_string()));
                            cancel_token = Some(ct);
                            steer_tx = Some(s_tx);
                        }
                    }

                    if do_draw {
                        let elapsed = last_frame.elapsed();
                        last_frame = Instant::now();
                        let _ = draw(&mut terminal, &mut app, &runtime, &mut boot_fx, &mut exit_fx, elapsed, &registry, &secret_prompts);
                    }
                }
            }
        }
    }

    // Save session on exit
    app.save_session().await;

    // ═══ HOOK: on_session_end ═══
    {
        let mut index_record = SessionIndexRecord::end(&app.session.id);
        index_record.turns = Some(app.api_messages.len());
        if let Err(err) = synaps_cli::core::session_index::append_record(&index_record) {
            tracing::warn!("failed to append session end index record: {}", err);
        }

        let transcript = Some(app.api_messages.clone());
        let hook_event = synaps_cli::extensions::hooks::events::HookEvent::on_session_end(&app.session.id, transcript);
        let _ = runtime.hook_bus().emit(&hook_event).await;
    }

    // Gracefully shut down all extensions
    ext_mgr_shared.write().await.shutdown_all().await;

    // Signal the inbox watcher's blocking thread to exit, then abort the async task.
    watcher_shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
    watcher_task.abort();

    // Shut down per-session socket + unregister from registry
    socket_shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
    socket_task.abort();
    synaps_cli::events::registry::unregister_session(&app.session.id);

    teardown_terminal(&mut terminal);

    Ok(())
}
