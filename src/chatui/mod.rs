//! Chat TUI binary — event loop, terminal setup, module wiring.

mod theme;
mod highlight;
mod markdown;
mod app;
mod render;
mod gamba;
mod draw;
mod toast;
mod commands;
mod input;
mod stream_handler;
mod settings;
mod plugins;
mod models;
mod help_find;
mod helpers;
mod lifecycle;
mod viewport;
mod sidecar;
mod signals;
mod lightbox;

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
    let (shutdown_signal_tx, mut shutdown_signal_rx) = tokio::sync::mpsc::unbounded_channel();
    let shutdown_signal_task = signals::spawn_shutdown_signal_task(shutdown_signal_tx);
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

    // Phase 8 slice 8A.8: copy legacy `sidecar_toggle_key` into the
    // namespace of any plugin that has staked a lifecycle claim with
    // a settings_category. Idempotent: skips already-set new keys.
    migrate_sidecar_toggle_key_to_claimed_plugins(&registry.lifecycle_claims());

    if !no_extensions {
        app.extension_loader_running = true;
        app.toasts.upsert(toast::Toast::new("extension-loader", "Discovering extensions…")
            .titled("Extensions")
            .at(toast::ToastPosition::TOP_CENTER)
            .ttl(None));
        synaps_cli::extensions::loader::spawn_discover_and_load(
            std::sync::Arc::clone(&ext_mgr_shared),
            app.extension_loader_tx.clone(),
        );
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

            // ── OS shutdown signals: Ctrl-C from terminal, SIGTERM from tmux kill-pane/session ──
            signal = shutdown_signal_rx.recv() => {
                if let Some(signal) = signal {
                    tracing::info!(signal = signals::signal_label(signal), "chat UI shutdown signal received");
                    app.push_msg(ChatMessage::System(format!("shutting down ({})", signals::signal_label(signal))));
                    exit_fx = Some(quit_effect());
                }
            }

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

            // ── Async extension loader progress ──
            event = app.extension_loader_rx.recv(), if app.extension_loader_running => {
                if let Some(event) = event {
                    handle_extension_loader_event(&mut app, &runtime, event).await;
                } else {
                    app.extension_loader_running = false;
                    app.toasts.dismiss("extension-loader");
                }
            }

            // ── Sidecar events — multiplexed across all hosted sidecars (Phase 8 8B) ──
            sidecar_event = async {
                if app.sidecars.is_empty() {
                    let _: () = std::future::pending().await;
                    unreachable!()
                } else {
                    // Collect (plugin_id, &mut manager) and race them.
                    let mut futures = Vec::with_capacity(app.sidecars.len());
                    for (pid, v) in app.sidecars.iter_mut() {
                        let pid = pid.clone();
                        futures.push(Box::pin(async move {
                            let ev = v.manager.next_event().await;
                            (pid, ev)
                        }));
                    }
                    let ((pid, ev), _, _) = futures::future::select_all(futures).await;
                    (pid, ev)
                }
            } => {
                let (pid, sidecar_event) = sidecar_event;
                if let Some(event) = sidecar_event {
                    self::sidecar::handle_event(&mut app, &pid, event);
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
            _ = tokio::time::sleep(std::time::Duration::from_millis(16)), if boot_fx.is_some() || exit_fx.is_some() || app.streaming || app.compact_task.is_some() || app.messages.is_empty() || app.logo_dismiss_t.is_some() || app.logo_build_t.is_some() || app.gamba_child.is_some() || secret_prompts.is_active() || !app.toasts.is_empty() || app.plugins.as_ref().is_some_and(|p| p.is_install_active()) => {
                secret_prompts.poll_requests(&secret_prompt_rx);
                if app.toasts.tick() {
                    app.invalidate();
                }
                // Tick the in-flight plugin install spinner and reap the
                // background clone task once it finishes.
                let mut install_did_work = false;
                let mut install_finished = false;
                if let Some(plugins_state) = app.plugins.as_mut() {
                    if plugins_state.is_install_active() {
                        plugins_state.tick_install_spinner();
                        install_did_work = true;
                        if plugins_state.install_ready_to_reap() {
                            install_finished = true;
                        }
                    }
                }
                if install_finished {
                    if let Some(plugins_state) = app.plugins.as_mut() {
                        self::plugins::actions::complete_pending_install_clone(
                            plugins_state, &registry, &config,
                        ).await;
                    }
                }
                if install_did_work || install_finished {
                    app.invalidate();
                }
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
                            InputAction::HelpFindOutcome => {}
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
                                    CommandAction::OpenHelpFind { query } => {
                                        let registry = synaps_cli::help::HelpRegistry::new(
                                            synaps_cli::help::builtin_entries(),
                                            registry.plugin_help_entries(),
                                        );
                                        app.help_find = Some(synaps_cli::help::HelpFindState::new(
                                            registry.entries().to_vec(),
                                            &query,
                                        ));
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
                                        if matches!(
                                            command.backend,
                                            synaps_cli::skills::registry::RegisteredPluginCommandBackend::Interactive { .. }
                                        ) {
                                            let manager = ext_mgr_shared.read().await;
                                            commands::execute_interactive_plugin_command_events(
                                                &command,
                                                &arg,
                                                &manager,
                                                &mut app,
                                            ).await;
                                        } else {
                                            commands::execute_command_action(
                                                CommandAction::PluginCommand { command, arg },
                                                &mut app,
                                                &runtime,
                                            ).await;
                                        }
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
                                                // Capability declarations (grouped from the `future` list).
                                                // Each entry has a free-form kind declared by the plugin
                                                // (e.g. "capture", "ocr", "agent"). Render grouped by kind so
                                                // future capability types surface without core changes.
                                                if !snap.future.is_empty() {
                                                    use std::collections::BTreeMap;
                                                    // kind -> name -> Vec<mode>
                                                    let mut by_kind: BTreeMap<String, BTreeMap<String, Vec<String>>> = BTreeMap::new();
                                                    for entry in &snap.future {
                                                        let bucket = by_kind.entry(entry.kind.clone()).or_default();
                                                        // entry.name is "<plugin-name> (<mode>)" in the legacy
                                                        // shim; preserve the existing display behaviour.
                                                        if let Some(open) = entry.name.rfind(" (") {
                                                            if entry.name.ends_with(')') {
                                                                let name = entry.name[..open].to_string();
                                                                let mode = entry.name[open + 2..entry.name.len() - 1].to_string();
                                                                bucket.entry(name).or_default().push(mode);
                                                                continue;
                                                            }
                                                        }
                                                        bucket.entry(entry.name.clone()).or_default();
                                                    }
                                                    for (kind, names) in &by_kind {
                                                        for (name, modes) in names {
                                                            let modes_str = modes.join("/");
                                                            if modes_str.is_empty() {
                                                                app.push_msg(ChatMessage::System(format!(
                                                                    "    {}: {}",
                                                                    kind, name
                                                                )));
                                                            } else {
                                                                app.push_msg(ChatMessage::System(format!(
                                                                    "    {}: {} [{}]",
                                                                    kind, name, modes_str
                                                                )));
                                                            }
                                                        }
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
                                                    synaps_cli::extensions::config::ConfigSource::PluginConfig => "plugin config".to_string(),
                                                    synaps_cli::extensions::config::ConfigSource::LegacyConfigKey(name) => format!("legacy config key ({})", name),
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

                                    CommandAction::SidecarToggle { plugin_id } => {
                                        // Phase 8 8B: target either the
                                        // claim-supplied plugin id, or fall
                                        // back to the legacy single-slot
                                        // discovery for the unclaimed case.
                                        let all = synaps_cli::sidecar::discovery::discover_all();
                                        let target = plugin_id
                                            .clone()
                                            .or_else(|| all.first().map(|s| s.plugin_name.clone()));
                                        let Some(target_pid) = target else {
                                            app.push_msg(ChatMessage::Error(
                                                "sidecar unavailable: no plugin provides a sidecar binary".to_string()
                                            ));
                                            continue;
                                        };

                                        if app.sidecars.contains_key(&target_pid) {
                                            // Subsequent toggle on existing sidecar — arm flag is source of truth.
                                            let label = app.sidecars.get(&target_pid)
                                                .and_then(|s| s.display_name.as_deref())
                                                .unwrap_or("sidecar")
                                                .to_string();
                                            let v = app.sidecars.get_mut(&target_pid).unwrap();
                                            if v.armed {
                                                v.armed = false;
                                                if let Err(err) = v.manager.release().await {
                                                    app.push_msg(ChatMessage::Error(format!("{label} release failed: {err}")));
                                                }
                                                app.push_msg(ChatMessage::System(
                                                    format!("{label}: stopping — final transcript will be appended")
                                                ));
                                            } else {
                                                v.armed = true;
                                                if let Err(err) = v.manager.press().await {
                                                    v.armed = false;
                                                    app.push_msg(ChatMessage::Error(format!("{label} press failed: {err}")));
                                                }
                                            }
                                        } else {
                                            // Spawn new sidecar instance for target_pid.
                                            let Some(discovered) = all.into_iter().find(|s| s.plugin_name == target_pid) else {
                                                app.push_msg(ChatMessage::Error(format!(
                                                    "sidecar plugin '{}' not discoverable", target_pid,
                                                )));
                                                continue;
                                            };
                                            let (sidecar_plugin_info, sidecar_spawn_args) = {
                                                let manager = ext_mgr_shared.read().await;
                                                let info = manager.plugin_info(&target_pid).cloned();
                                                let args = match manager.sidecar_spawn_args(&target_pid).await {
                                                    Ok(a) => Some(a),
                                                    Err(err) => {
                                                        tracing::debug!(
                                                            plugin = %target_pid,
                                                            error = %err,
                                                            "sidecar.spawn_args RPC unavailable; using manifest defaults",
                                                        );
                                                        None
                                                    }
                                                };
                                                (info, args)
                                            };
                                            match self::sidecar::SidecarUiState::spawn_for(
                                                discovered,
                                                sidecar_spawn_args,
                                                sidecar_plugin_info.as_ref(),
                                            ).await {
                                                Ok(mut state) => {
                                                    let claims = registry.lifecycle_claims();
                                                    let display = pick_display_name_for_plugin(
                                                        &state.sidecar.plugin_name,
                                                        &claims,
                                                    );
                                                    state.set_display_name(display);
                                                    let label = state.display_name.clone()
                                                        .unwrap_or_else(|| "sidecar".to_string());
                                                    let plugin_key = state.sidecar.plugin_name.clone();
                                                    app.sidecars.insert(plugin_key.clone(), state);
                                                    app.push_msg(ChatMessage::System(
                                                        format!("{label} active — press the toggle again to stop")
                                                    ));
                                                    if let Some(v) = app.sidecars.get_mut(&plugin_key) {
                                                        v.armed = true;
                                                        if let Err(err) = v.manager.press().await {
                                                            v.armed = false;
                                                            v.status = self::sidecar::SidecarUiStatus::Error(err.to_string());
                                                            app.push_msg(ChatMessage::Error(format!("{label} press failed: {err}")));
                                                        }
                                                    }
                                                }
                                                Err(err) => {
                                                    app.push_msg(ChatMessage::Error(format!("sidecar unavailable: {err}")));
                                                }
                                            }
                                        }
                                    }

                                    CommandAction::SidecarStatus { plugin_id } => {
                                        // Phase 8 8B: show status for the
                                        // requested plugin, or — when None —
                                        // for the single legacy sidecar (or
                                        // the discovery hint when none have
                                        // been spawned).
                                        let line = if let Some(pid) = plugin_id.as_deref() {
                                            match app.sidecars.get(pid) {
                                                Some(v) => v.status_line(),
                                                None => match synaps_cli::sidecar::discovery::discover_all().into_iter().find(|s| s.plugin_name == pid) {
                                                    Some(s) => format!(
                                                        "sidecar: not yet started — sidecar available from plugin '{}' at {}",
                                                        s.plugin_name, s.binary.display()
                                                    ),
                                                    None => format!("sidecar: no plugin '{}' provides a sidecar", pid),
                                                },
                                            }
                                        } else if app.sidecars.len() == 1 {
                                            app.sidecars.values().next().unwrap().status_line()
                                        } else if app.sidecars.is_empty() {
                                            match synaps_cli::sidecar::discovery::discover() {
                                                Some(s) => format!(
                                                    "sidecar: not yet started — sidecar available from plugin '{}' at {}",
                                                    s.plugin_name, s.binary.display()
                                                ),
                                                None => "sidecar: no plugin provides a sidecar binary (install a plugin that declares provides.sidecar)".to_string(),
                                            }
                                        } else {
                                            // Multiple active — list each.
                                            let mut lines: Vec<String> = app.sidecars.values()
                                                .map(|v| v.status_line()).collect();
                                            lines.sort();
                                            lines.join("\n")
                                        };
                                        app.push_msg(ChatMessage::System(line));
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
                                        CommandAction::OpenHelpFind { .. } => {}
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
                                        CommandAction::SidecarToggle { .. } => {}
                                        CommandAction::SidecarStatus { .. } => {}
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
                            InputAction::PluginEditorOpen { plugin_id, category, field } => {
                                let manager = ext_mgr_shared.read().await;
                                match manager.settings_editor_open(&plugin_id, &category, &field).await
                                    .and_then(settings::plugin_editor::render_from_open_result)
                                {
                                    Ok(render) => {
                                        if let Some(state) = app.settings.as_mut() {
                                            state.row_error = None;
                                            state.edit_mode = Some(settings::ActiveEditor::PluginCustom {
                                                plugin_id: plugin_id.clone(),
                                                category: category.clone(),
                                                field: field.clone(),
                                                render: settings::plugin_editor::PluginEditorSession {
                                                    plugin_id,
                                                    category,
                                                    field,
                                                    render,
                                                },
                                            });
                                        }
                                    }
                                    Err(err) => {
                                        if let Some(state) = app.settings.as_mut() {
                                            state.row_error = Some((
                                                format!("plugin.{}.{}", plugin_id, field),
                                                err,
                                            ));
                                        }
                                    }
                                }
                            }
                            InputAction::PluginEditorKey { plugin_id, category, field, key } => {
                                let wire_key = settings::plugin_editor::key_to_wire(key);
                                if wire_key == "Enter" {
                                    let selected = app.settings.as_ref().and_then(|state| {
                                        match &state.edit_mode {
                                            Some(settings::ActiveEditor::PluginCustom { render, .. }) => {
                                                let cursor = render.render.cursor.unwrap_or(0);
                                                render.render.rows.get(cursor).and_then(|r| r.data.clone())
                                            }
                                            _ => None,
                                        }
                                    });
                                    if let Some(value) = selected {
                                        let manager = ext_mgr_shared.read().await;
                                        match manager.settings_editor_commit(&plugin_id, &category, &field, value.clone()).await {
                                            Ok(reply) => {
                                                let effect = settings::plugin_editor::effect_from_commit_reply(
                                                    &plugin_id,
                                                    &field,
                                                    reply,
                                                );
                                                match effect {
                                                    settings::plugin_editor::PluginEditorEffect::None => {}
                                                    settings::plugin_editor::PluginEditorEffect::ConfigWrite { plugin_id, key, value } => {
                                                        match synaps_cli::extensions::config_store::write_plugin_config(&plugin_id, &key, &value) {
                                                            Ok(()) => {
                                                                if let Some(state) = app.settings.as_mut() {
                                                                    state.edit_mode = None;
                                                                    state.row_error = Some((format!("plugin.{}.{}", plugin_id, key), "saved".to_string()));
                                                                }
                                                            }
                                                            Err(err) => {
                                                                if let Some(state) = app.settings.as_mut() {
                                                                    state.row_error = Some((format!("plugin.{}.{}", plugin_id, key), err.to_string()));
                                                                }
                                                            }
                                                        }
                                                    }
                                                    settings::plugin_editor::PluginEditorEffect::InvokeCommand { plugin_id, command, args } => {
                                                        if let Some(state) = app.settings.as_mut() {
                                                            state.edit_mode = None;
                                                            state.row_error = Some((format!("plugin.{}.{}", plugin_id, field), "download started".to_string()));
                                                        }
                                                        commands::execute_interactive_plugin_command_by_parts(
                                                            &plugin_id,
                                                            &command,
                                                            args,
                                                            &manager,
                                                            &mut app,
                                                        ).await;
                                                    }
                                                }
                                            }
                                            Err(err) => {
                                                if let Some(state) = app.settings.as_mut() {
                                                    state.row_error = Some((format!("plugin.{}.{}", plugin_id, field), err));
                                                }
                                            }
                                        }
                                    }
                                } else {
                                    let manager = ext_mgr_shared.read().await;
                                    match manager.settings_editor_key(&plugin_id, &category, &field, &wire_key).await
                                        .and_then(settings::plugin_editor::render_from_key_result)
                                    {
                                        Ok(Some(render)) => {
                                            if let Some(settings::ActiveEditor::PluginCustom { render: session, .. }) =
                                                app.settings.as_mut().and_then(|s| s.edit_mode.as_mut())
                                            {
                                                session.render = render;
                                            }
                                        }
                                        Ok(None) => {}
                                        Err(err) => {
                                            if let Some(state) = app.settings.as_mut() {
                                                state.row_error = Some((format!("plugin.{}.{}", plugin_id, field), err));
                                            }
                                        }
                                    }
                                }
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

    // Let extension shutdown continue in the background; exit should not hang on
    // extension post/session-end cleanup or slow child-process teardown.
    let _extension_shutdown = synaps_cli::extensions::manager::ExtensionManager::shutdown_all_detached(
        std::sync::Arc::clone(&ext_mgr_shared),
    );
    shutdown_signal_task.abort();

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

fn handle_extension_loader_toast(app: &mut App, title: &str, lines: Vec<String>, persistent: bool) {
    app.toasts.upsert(toast::Toast::new("extension-loader", "")
        .titled(title)
        .lines(lines)
        .at(toast::ToastPosition::TOP_CENTER)
        .ttl(if persistent { None } else { Some(std::time::Duration::from_secs(5)) }));
    app.invalidate();
}

async fn handle_extension_loader_event(
    app: &mut App,
    runtime: &Runtime,
    event: synaps_cli::extensions::loader::ExtensionLoaderEvent,
) {
    use synaps_cli::extensions::loader::ExtensionLoaderEvent;
    match event {
        ExtensionLoaderEvent::Started => {
            handle_extension_loader_toast(app, "Extensions", vec!["Discovering extensions…".into()], true);
        }
        ExtensionLoaderEvent::Loaded { plugin, loaded, failed } => {
            handle_extension_loader_toast(
                app,
                "Extensions",
                vec![format!("Loaded {loaded} extension{}", if loaded == 1 { "" } else { "s" }), format!("Latest: {plugin}"), format!("Failures: {failed}")],
                true,
            );
        }
        ExtensionLoaderEvent::Failed { failure, loaded, failed } => {
            handle_extension_loader_toast(
                app,
                "Extensions",
                vec![format!("Loaded {loaded}, failed {failed}"), format!("⚠ {}", failure.plugin)],
                true,
            );
            app.push_msg(ChatMessage::System(format!(
                "⚠ Extension '{}' failed: {}",
                failure.plugin,
                failure.concise_message()
            )));
        }
        ExtensionLoaderEvent::Finished { loaded, failed } => {
            app.extension_loader_running = false;
            let handler_count = runtime.hook_bus().handler_count().await;
            tracing::info!(extensions = loaded.len(), failures = failed.len(), handlers = handler_count, "Extension discovery complete");
            let lines = if failed.is_empty() {
                vec![format!("✓ Loaded {} extension{}", loaded.len(), if loaded.len() == 1 { "" } else { "s" })]
            } else {
                vec![
                    format!("Loaded {} extension{}", loaded.len(), if loaded.len() == 1 { "" } else { "s" }),
                    format!("{} failed — see transcript", failed.len()),
                ]
            };
            handle_extension_loader_toast(app, "Extensions", lines, false);
        }
    }
}

/// Phase 8 slice 8A.8: when a plugin has staked a lifecycle claim and
/// declared a `settings_category`, copy the legacy global
/// `sidecar_toggle_key` value into the plugin-namespaced equivalent
/// (`plugins.{plugin}.{cat}._lifecycle_toggle_key`) so the user's
/// toggle-key choice follows them across the rename. Idempotent: any
/// claim whose new key is already set is skipped, and a missing legacy
/// value is a no-op.
fn migrate_sidecar_toggle_key_to_claimed_plugins(
    claims: &[synaps_cli::skills::registry::LifecycleClaim],
) {
    const LEGACY: &str = "sidecar_toggle_key";
    let Some(legacy_value) = synaps_cli::config::read_config_value(LEGACY) else {
        return;
    };
    let trimmed = legacy_value.trim();
    if trimmed.is_empty() {
        return;
    }
    for claim in claims {
        let Some(ref cat) = claim.settings_category else { continue };
        let new_key = format!(
            "plugins.{}.{}._lifecycle_toggle_key",
            claim.plugin, cat
        );
        if synaps_cli::config::read_config_value(&new_key).is_some() {
            continue;
        }
        match synaps_cli::config::write_config_value(&new_key, trimmed) {
            Ok(()) => tracing::info!(
                "sidecar migration: copied global `{}` → `{}` for plugin `{}`",
                LEGACY,
                new_key,
                claim.plugin,
            ),
            Err(err) => tracing::warn!(
                "sidecar migration: failed to copy `{}` → `{}`: {}",
                LEGACY,
                new_key,
                err,
            ),
        }
    }
}

/// Look up the display name for a sidecar's owning plugin from the
/// lifecycle-claim snapshot. Returns `None` if no claim matches.
///
/// Phase 8 8A.5 follow-up: used post-spawn to populate
/// [`SidecarUiState::display_name`] from the registry claim.
fn pick_display_name_for_plugin(
    plugin_name: &str,
    claims: &[synaps_cli::skills::registry::LifecycleClaim],
) -> Option<String> {
    claims
        .iter()
        .find(|c| c.plugin == plugin_name)
        .map(|c| c.display_name.clone())
}

#[cfg(test)]
mod migration_tests {
    use super::*;
    use synaps_cli::skills::registry::LifecycleClaim;

    fn make_test_home(subdir: &str) -> std::path::PathBuf {
        let dir = std::path::PathBuf::from(format!("/tmp/synaps-mig-test-{}", subdir));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".synaps-cli")).unwrap();
        dir
    }

    fn with_home<F: FnOnce()>(home: &std::path::Path, f: F) {
        let original = std::env::var("HOME").ok();
        std::env::set_var("HOME", home);
        f();
        if let Some(h) = original {
            std::env::set_var("HOME", h);
        } else {
            std::env::remove_var("HOME");
        }
    }

    fn claim(plugin: &str, command: &str, cat: Option<&str>) -> LifecycleClaim {
        LifecycleClaim {
            plugin: plugin.to_string(),
            command: command.to_string(),
            settings_category: cat.map(str::to_string),
            display_name: command.to_string(),
            importance: 0,
        }
    }

    #[test]
    fn migrate_copies_legacy_into_namespaced_key() {
        let home = make_test_home("copy-into-namespaced");
        let cfg = home.join(".synaps-cli/config");
        std::fs::write(&cfg, "sidecar_toggle_key = F2\n").unwrap();
        with_home(&home, || {
            migrate_sidecar_toggle_key_to_claimed_plugins(&[claim(
                "sample-sidecar",
                "capture",
                Some("capture"),
            )]);
            let v = synaps_cli::config::read_config_value(
                "plugins.sample-sidecar.capture._lifecycle_toggle_key",
            );
            assert_eq!(v.as_deref(), Some("F2"));
        });
    }

    #[test]
    fn migrate_skips_when_new_key_already_set() {
        let home = make_test_home("skip-existing");
        let cfg = home.join(".synaps-cli/config");
        std::fs::write(
            &cfg,
            "sidecar_toggle_key = F2\nplugins.sample-sidecar.capture._lifecycle_toggle_key = F12\n",
        ).unwrap();
        with_home(&home, || {
            migrate_sidecar_toggle_key_to_claimed_plugins(&[claim(
                "sample-sidecar",
                "capture",
                Some("capture"),
            )]);
            let v = synaps_cli::config::read_config_value(
                "plugins.sample-sidecar.capture._lifecycle_toggle_key",
            );
            assert_eq!(v.as_deref(), Some("F12"), "must not overwrite a user-set value");
        });
    }

    #[test]
    fn migrate_is_noop_when_legacy_unset() {
        let home = make_test_home("noop-no-legacy");
        let cfg = home.join(".synaps-cli/config");
        std::fs::write(&cfg, "model = claude-sonnet-4-6\n").unwrap();
        with_home(&home, || {
            migrate_sidecar_toggle_key_to_claimed_plugins(&[claim(
                "sample-sidecar",
                "capture",
                Some("capture"),
            )]);
            assert!(synaps_cli::config::read_config_value(
                "plugins.sample-sidecar.capture._lifecycle_toggle_key"
            ).is_none());
        });
    }

    #[test]
    fn migrate_skips_claim_without_settings_category() {
        let home = make_test_home("skip-no-category");
        let cfg = home.join(".synaps-cli/config");
        std::fs::write(&cfg, "sidecar_toggle_key = F8\n").unwrap();
        with_home(&home, || {
            migrate_sidecar_toggle_key_to_claimed_plugins(&[claim("p", "ocr", None)]);
            // No namespaced key written for a claim with no category.
            let contents = std::fs::read_to_string(&cfg).unwrap();
            assert!(
                !contents.contains("_lifecycle_toggle_key"),
                "no namespaced key should be written when settings_category is None: {contents}"
            );
        });
    }

    #[test]
    fn migrate_handles_multiple_claims_in_one_pass() {
        let home = make_test_home("multi-claim");
        let cfg = home.join(".synaps-cli/config");
        std::fs::write(&cfg, "sidecar_toggle_key = C-V\n").unwrap();
        with_home(&home, || {
            migrate_sidecar_toggle_key_to_claimed_plugins(&[
                claim("sample-sidecar", "capture", Some("capture")),
                claim("ocr-plugin", "ocr", Some("ocr")),
            ]);
            assert_eq!(
                synaps_cli::config::read_config_value(
                    "plugins.sample-sidecar.capture._lifecycle_toggle_key"
                ).as_deref(),
                Some("C-V")
            );
            assert_eq!(
                synaps_cli::config::read_config_value(
                    "plugins.ocr-plugin.ocr._lifecycle_toggle_key"
                ).as_deref(),
                Some("C-V")
            );
        });
    }
}

#[cfg(test)]
mod display_name_helper_tests {
    use super::pick_display_name_for_plugin;
    use synaps_cli::skills::registry::LifecycleClaim;

    fn claim(plugin: &str, display: &str) -> LifecycleClaim {
        LifecycleClaim {
            plugin: plugin.into(),
            command: "capture".into(),
            settings_category: None,
            display_name: display.into(),
            importance: 0,
        }
    }

    #[test]
    fn pick_display_name_for_plugin_returns_match() {
        let claims = vec![claim("sample-sidecar", "Sample")];
        assert_eq!(
            pick_display_name_for_plugin("sample-sidecar", &claims),
            Some("Sample".to_string())
        );
    }

    #[test]
    fn pick_display_name_for_plugin_returns_none_for_unmatched() {
        let claims = vec![claim("sample-sidecar", "Sample")];
        assert_eq!(pick_display_name_for_plugin("unknown", &claims), None);
    }

    #[test]
    fn pick_display_name_for_plugin_returns_none_with_empty_claims() {
        assert_eq!(pick_display_name_for_plugin("sample-sidecar", &[]), None);
    }
}
