//! P12.2: the `InputAction` dispatch match extracted from `run()` — PURE
//! CODE MOTION.
//!
//! The match body below is byte-identical to `run()`'s former event_reader
//! arm dispatch (previously `mod.rs:486-1796`), with exactly these mechanical
//! rewrites and nothing else:
//!
//! * loop-locals became [`LoopState`] fields, destructured at fn entry, so
//!   arm code reads unchanged (`app.…`, `runtime.…`); writes to the loop's
//!   `Option` slots gained a `*` deref (`*stream = …`, `*cancel_token = …`,
//!   `*steer_tx = …`, `*exit_fx_sent = …`).
//! * `event_reader` is `&mut Option<EventStream>` so the deliberate early
//!   drop before the gamba terminal handoff keeps its exact timing:
//!   `drop(event_reader)` → `drop(event_reader.take())`, and the re-create
//!   after launch is `*event_reader = Some(EventStream::new())`.
//! * the four `continue`s that targeted `run()`'s outer loop became
//!   `return ControlFlow::Continue(())` (there was no trailing code in the
//!   arm, so fall-through and `continue` were already equivalent; `break`
//!   never occurred inside the match — `ControlFlow::Break` is reserved for
//!   the caller's `break` mapping and future arms).
//!
//! Stream-START sites inside the `Submit`/`SlashCommand` arms are untouched.
//! No logic changed.

use super::view_model::ViewInputs;
use super::*;

use std::ops::ControlFlow;

fn spawn_auto_catalog_refreshes(app: &App, runtime: &synaps_cli::Runtime) {
    for &provider_key in models::auto_refresh_catalog_providers() {
        let client = runtime.http_client().clone();
        let tx = app.model_list_tx.clone();
        let key = provider_key.to_string();
        tokio::spawn(async move {
            let result = synaps_cli::runtime::openai::catalog::fetch_catalog_models(&client, &key)
                .await
                .map(|models| {
                    models
                        .into_iter()
                        .map(|model| {
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
                                synaps_cli::runtime::openai::catalog::ReasoningSupport::Unknown => {
                                }
                                _ => metadata.push("thinking".to_string()),
                            }
                            if model.pricing.has_internal_reasoning_cost() {
                                metadata.push("reasoning $".to_string());
                            }
                            models::ExpandedModelEntry::with_metadata(
                                full_id, label, false, metadata,
                            )
                        })
                        .collect()
                });
            let _ = tx.send((key, result));
        });
    }
}

/// The borrow bundle: everything `run()`'s event loop lends the dispatch for
/// the duration of one `InputAction`. Constructed fresh per event at the call
/// site and consumed by value, so no borrow outlives the arm.
pub(crate) struct LoopState<'a> {
    pub app: &'a mut App,
    pub runtime: &'a mut synaps_cli::Runtime,
    pub config: &'a mut synaps_cli::SynapsConfig,
    pub registry: &'a std::sync::Arc<synaps_cli::skills::registry::CommandRegistry>,
    pub keybind_registry:
        &'a std::sync::Arc<std::sync::RwLock<synaps_cli::skills::keybinds::KeybindRegistry>>,
    pub system_prompt_path: &'a std::path::PathBuf,
    pub render_handle: &'a render_thread::RenderHandle,
    /// `Option`-wrapped by `run()` (P12.2) so the gamba handoff can drop the
    /// reader early through a `&mut` without moving it; always `Some` on
    /// entry and on return.
    pub event_reader: &'a mut Option<EventStream>,
    pub stream: &'a mut Option<
        std::pin::Pin<Box<dyn futures::Stream<Item = synaps_cli::StreamEvent> + Send>>,
    >,
    pub secret_prompt_handle: &'a synaps_cli::tools::SecretPromptHandle,
    pub cancel_token: &'a mut Option<CancellationToken>,
    pub steer_tx: &'a mut Option<tokio::sync::mpsc::UnboundedSender<String>>,
    pub ext_mgr_shared:
        &'a std::sync::Arc<tokio::sync::RwLock<synaps_cli::extensions::manager::ExtensionManager>>,
    pub exit_fx_sent: &'a mut bool,
}

/// Dispatch one decoded [`InputAction`]. `ControlFlow::Continue(())` means
/// "next loop iteration" (identical to the old arm fall-through / `continue`);
/// `ControlFlow::Break(())` means "break the outer event loop" (no current
/// arm produces it — the mapping is honored by the caller regardless).
pub(crate) async fn handle_input_action(
    action: InputAction,
    state: LoopState<'_>,
) -> ControlFlow<()> {
    let LoopState {
        app,
        runtime,
        config,
        registry,
        keybind_registry,
        system_prompt_path,
        render_handle,
        event_reader,
        stream,
        secret_prompt_handle,
        cancel_token,
        steer_tx,
        ext_mgr_shared,
        exit_fx_sent,
    } = state;
    // Body verbatim from mod.rs:486-1796 (original indentation preserved for
    // diff-ability of the motion; see module header for the mechanical edits).
    match action {
        InputAction::None => {}
        InputAction::HelpFindOutcome => {}
        InputAction::Quit => {
            render_handle.send_exit_fx(quit_effect());
            *exit_fx_sent = true;
        }
        InputAction::Abort => {
            if let Some(ref ct) = *cancel_token {
                ct.cancel();
            }
            app.capture_abort_context();
            if let Some(ref q) = app.queued_message.take() {
                app.push_msg(ChatMessage::System(format!("dequeued: {}", q)));
            }
            // Flush any events that arrived during streaming
            for formatted in app.pending_events.drain(..) {
                app.api_messages
                    .push(std::sync::Arc::new(serde_json::json!({
                        "role": "user",
                        "content": formatted
                    })));
            }
            *stream = None;
            *cancel_token = None;
            *steer_tx = None;
            app.streaming = false;
            app.subagents.clear();
            // Cancel all running reactive subagents. A poisoned
            // registry mutex must not turn a user abort into a
            // panic (the old `.unwrap()`), but nor should it
            // silently skip cancellation and leave orphaned
            // subagents burning tokens — recover the guard and
            // cancel anyway, logging the poison. Scoped in its
            // own block so the guard drops before any `.await`
            // below (clippy::await_holding_lock).
            {
                let mut registry = match runtime.subagent_registry().lock() {
                    Ok(g) => g,
                    Err(poisoned) => {
                        tracing::warn!(
                                                "subagent registry mutex poisoned during abort; recovering to cancel running handles"
                                            );
                        poisoned.into_inner()
                    }
                };
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
            app.drop_empty_thinking();
            app.push_msg(ChatMessage::Error(abort_msg.to_string()));
            app.save_session().await;
        }
        InputAction::SlashCommand(cmd, arg) => {
            let kb_snapshot = {
                let g = keybind_registry.read().expect("keybind registry poisoned");
                g.clone()
            };
            match commands::handle_command(
                &cmd,
                &arg,
                app,
                runtime,
                system_prompt_path,
                registry,
                &kb_snapshot,
            )
            .await
            {
                CommandAction::None => {}
                CommandAction::StartStream => {} // reserved for future use
                CommandAction::Quit => {
                    render_handle.send_exit_fx(quit_effect());
                    *exit_fx_sent = true;
                }
                CommandAction::LaunchGamba => {
                    drop(event_reader.take());
                    // Pause the render thread BEFORE touching the terminal —
                    // eliminates the stdout race between terminal.draw() and our mode changes.
                    render_handle.pause();
                    match app.launch_gamba() {
                        Ok(()) => {}
                        Err(msg) => {
                            // launch failed — restore and resume
                            render_handle.resume();
                            app.push_msg(ChatMessage::Error(msg));
                        }
                    }
                    // If gamba launched OK, resume is sent by reclaim/check_gamba_exited.
                    *event_reader = Some(EventStream::new());
                }
                CommandAction::OpenModels => {
                    let mut models_state = models::ModelsModalState::new();
                    // Seed with the app-level live catalog cache so previously
                    // fetched lists render instantly while refreshes run.
                    models_state.provider_catalog_overrides = app.catalog_overrides.clone();
                    app.models = Some(models_state);
                    // P7.5: mirror the `= Some(..)` open with a
                    // stack push (§6). GATE-1 note B: models
                    // opens on this async arm, so assert sync
                    // RIGHT HERE — a missed push is caught this
                    // event, not one event late.
                    app.modal_stack.push(focus::PaneId::Models);
                    #[cfg(debug_assertions)]
                    focus::debug_assert_stack_sync(app);
                    spawn_auto_catalog_refreshes(app, runtime);
                }
                CommandAction::OpenEffort => {
                    // Defense in depth: the streaming-input path already
                    // refuses /effort while streaming; this guard covers any
                    // future action source. Never open mid-stream.
                    if app.streaming || stream.is_some() {
                        app.push_msg(ChatMessage::System(
                            "/effort can't run while streaming — press Esc to cancel first"
                                .to_string(),
                        ));
                    } else {
                        app.effort = Some(effort::EffortModalState::new(
                            runtime.model(),
                            runtime.thinking_level(),
                        ));
                        app.modal_stack.push(focus::PaneId::Effort);
                        #[cfg(debug_assertions)]
                        focus::debug_assert_stack_sync(app);
                    }
                }
                CommandAction::OpenSettings => {
                    app.settings = Some(settings::SettingsState::new());
                    // P7.7: mirror the `= Some(..)` open with a stack
                    // push (§6). GATE-1 note B: settings opens on this
                    // async command arm, so assert sync RIGHT HERE — a
                    // missed push is caught this event, not one later.
                    app.modal_stack.push(focus::PaneId::Settings);
                    #[cfg(debug_assertions)]
                    focus::debug_assert_stack_sync(app);
                    // Same live-catalog refresh the /models modal runs: the
                    // settings model picker feeds off app.catalog_overrides.
                    spawn_auto_catalog_refreshes(app, runtime);
                }
                CommandAction::OpenPlugins => {
                    let path = synaps_cli::skills::state::PluginsState::default_path();
                    match synaps_cli::skills::state::PluginsState::load_from(&path) {
                        Ok(file) => {
                            app.plugins = Some(plugins::PluginsModalState::new(file));
                            // P7.6: mirror the `= Some(..)` open with a
                            // stack push (§6). GATE-1 note B: plugins
                            // opens on this async command arm, so assert
                            // sync RIGHT HERE — a missed push is caught
                            // this event, not one event late. Only the
                            // Ok(..) branch opens the modal, so the push
                            // lives inside it (Err pushes nothing).
                            app.modal_stack.push(focus::PaneId::Plugins);
                            #[cfg(debug_assertions)]
                            focus::debug_assert_stack_sync(app);
                        }
                        Err(e) => {
                            app.push_msg(ChatMessage::Error(format!(
                                "failed to load plugins.json: {}",
                                e
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
                    // P7.4: mirror the open with a stack push
                    // (§6). Covered by the tripwire one event
                    // late (acceptable per note B).
                    app.modal_stack.push(focus::PaneId::HelpFind);
                }
                CommandAction::ReloadPlugins => {
                    synaps_cli::skills::reload_registry(registry, config);
                    app.push_msg(ChatMessage::System("plugins reloaded".to_string()));
                }
                CommandAction::LoadSkill { skill, arg } => {
                    use synaps_cli::skills::tool::LoadSkillTool;

                    let tool_use_id = format!("toolu_skill_{}", uuid::Uuid::new_v4().simple());
                    let body = LoadSkillTool::format_body(&skill);

                    app.api_messages.push(std::sync::Arc::new(json!({
                        "role": "assistant",
                        "content": [{
                            "type": "tool_use",
                            "id": tool_use_id,
                            "name": "load_skill",
                            "input": {"skill": skill.name.clone()}
                        }]
                    })));
                    app.api_messages.push(std::sync::Arc::new(json!({
                        "role": "user",
                        "content": [{
                            "type": "tool_result",
                            "tool_use_id": tool_use_id,
                            "content": body
                        }]
                    })));
                    let display_name = match &skill.plugin {
                        Some(p) => format!("{}:{}", p, skill.name),
                        None => skill.name.clone(),
                    };
                    app.push_msg(ChatMessage::System(format!(
                        "loaded skill: {}",
                        display_name
                    )));

                    if !arg.is_empty() {
                        app.api_messages.push(std::sync::Arc::new(
                            json!({"role": "user", "content": arg.clone()}),
                        ));
                        app.push_msg(ChatMessage::User(arg));
                    }
                    // Start stream — mirror InputAction::Submit stream-start pattern.
                    let ct = CancellationToken::new();
                    let (s_tx, s_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
                    app.status_text = Some("connecting…".to_string());
                    app.streaming = true;
                    app.spinner_frame = 0;
                    let term_size = crossterm::terminal::size()
                        .map(|(w, h)| ratatui::layout::Size {
                            width: w,
                            height: h,
                        })
                        .unwrap_or_default();
                    let built = build_render_model(
                        &mut ViewInputs::from_app(app),
                        runtime,
                        registry,
                        term_size,
                    );
                    if let Some((model, patch)) = built {
                        patch.apply(app);
                        render_handle.publish(model);
                    }
                    *stream = Some(
                        runtime
                            .run_stream_with_messages(
                                app.api_messages.clone(),
                                ct.clone(),
                                Some(s_rx),
                                Some(secret_prompt_handle.clone()),
                                false,
                            )
                            .await,
                    );
                    app.status_text = None;
                    app.push_msg(ChatMessage::Thinking(THINKING_PLACEHOLDER.to_string()));
                    *cancel_token = Some(ct);
                    *steer_tx = Some(s_tx);
                }
                CommandAction::PluginCommand { command, arg } => {
                    if matches!(
                        command.backend,
                        synaps_cli::skills::registry::RegisteredPluginCommandBackend::Interactive { .. }
                    ) {
                        let manager = ext_mgr_shared.read().await;
                        commands::execute_interactive_plugin_command_events(
                            &command, &arg, &manager, app,
                        )
                        .await;
                    } else {
                        commands::execute_command_action(
                            CommandAction::PluginCommand { command, arg },
                            app,
                            runtime,
                        )
                        .await;
                    }
                }
                CommandAction::Compact {
                    custom_instructions,
                } => {
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
                        if app.session.title.is_empty() {
                            "(untitled)".to_string()
                        } else {
                            app.session.title.clone()
                        },
                        app.api_messages.len(),
                    ));

                    // Walk backward through parents
                    let mut current_parent = app.session.parent_session.clone();
                    while let Some(ref parent_id) = current_parent {
                        match synaps_cli::core::session::Session::load(parent_id) {
                            Ok(parent) => {
                                let title = if parent.title.is_empty() {
                                    "(untitled)".to_string()
                                } else {
                                    parent.title.clone()
                                };
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
                        app.push_msg(ChatMessage::System(
                            "no compaction history — this is the root session".to_string(),
                        ));
                    } else {
                        let mut lines = vec!["Session chain:".to_string()];
                        for (i, (id, title, msgs)) in chain.iter().enumerate() {
                            let marker = if i == chain.len() - 1 {
                                " ← active"
                            } else {
                                ""
                            };
                            let short_id: String = id.chars().take(19).collect();
                            let short_title: String = title.chars().take(40).collect();
                            lines.push(format!(
                                "  {} {} ({} msgs) {}{}",
                                if i == 0 { "●" } else { "→" },
                                short_id,
                                msgs,
                                short_title,
                                marker
                            ));
                        }
                        app.push_msg(ChatMessage::System(lines.join("\n")));
                    }

                    // Show any named chain bookmarking the active head
                    match synaps_cli::chain::find_all_chains_by_head(&app.session.id) {
                        Ok(named) if !named.is_empty() => {
                            let names: Vec<String> =
                                named.iter().map(|c| format!("@{}", c.name)).collect();
                            app.push_msg(ChatMessage::System(format!(
                                "bookmarked by: {}",
                                names.join(", ")
                            )));
                        }
                        _ => {}
                    }
                }
                CommandAction::ChainList => match synaps_cli::chain::list_chains() {
                    Ok(chains) if chains.is_empty() => {
                        app.push_msg(ChatMessage::System("no named chains".to_string()));
                    }
                    Ok(chains) => {
                        app.push_msg(ChatMessage::System(format!("{} chain(s):", chains.len())));
                        for c in chains {
                            let active = if c.head == app.session.id { " *" } else { "" };
                            app.push_msg(ChatMessage::System(format!(
                                "  @{} → {}{}",
                                c.name, c.head, active
                            )));
                        }
                    }
                    Err(e) => {
                        app.push_msg(ChatMessage::Error(format!("failed to list chains: {}", e)));
                    }
                },
                CommandAction::ChainName { name } => {
                    match synaps_cli::chain::save_chain(&name, &app.session.id) {
                        Ok(()) => {
                            app.push_msg(ChatMessage::System(format!(
                                "chain '{}' → {}",
                                name, app.session.id
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
                        app.push_msg(ChatMessage::System(
                            "Usage stats are only available for Anthropic models.".to_string(),
                        ));
                    } else {
                        app.push_msg(ChatMessage::System("Checking usage...".to_string()));
                        match fetch_usage().await {
                            Ok(lines) => {
                                for line in lines {
                                    app.push_msg(ChatMessage::System(line));
                                }
                            }
                            Err(e) => app
                                .push_msg(ChatMessage::Error(format!("Usage check failed: {}", e))),
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
                        app.push_msg(ChatMessage::System(format!(
                            "Extensions ({}):",
                            snapshots.len()
                        )));
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
                                app.push_msg(ChatMessage::System(format!(
                                    "    hooks: {}",
                                    rendered
                                )));
                            }
                            if !snap.tools.is_empty() {
                                let rendered = snap
                                    .tools
                                    .iter()
                                    .map(|t| t.name.clone())
                                    .collect::<Vec<_>>()
                                    .join(", ");
                                app.push_msg(ChatMessage::System(format!(
                                    "    tools: {}",
                                    rendered
                                )));
                            }
                            // Capability declarations (grouped from the `future` list).
                            // Each entry has a free-form kind declared by the plugin
                            // (e.g. "capture", "ocr", "agent"). Render grouped by kind so
                            // future capability types surface without core changes.
                            if !snap.future.is_empty() {
                                use std::collections::BTreeMap;
                                // kind -> name -> Vec<mode>
                                let mut by_kind: BTreeMap<String, BTreeMap<String, Vec<String>>> =
                                    BTreeMap::new();
                                for entry in &snap.future {
                                    let bucket = by_kind.entry(entry.kind.clone()).or_default();
                                    // entry.name is "<plugin-name> (<mode>)" in the legacy
                                    // shim; preserve the existing display behaviour.
                                    if let Some(open) = entry.name.rfind(" (") {
                                        if entry.name.ends_with(')') {
                                            let name = entry.name[..open].to_string();
                                            let mode = entry.name[open + 2..entry.name.len() - 1]
                                                .to_string();
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
                                    provider.runtime_id, provider.display_name, disabled_suffix
                                )));
                                for model in &provider.models {
                                    let mut badges: Vec<&str> = Vec::new();
                                    if model.tool_use {
                                        badges.push("tool-use");
                                    }
                                    if model.streaming {
                                        badges.push("streaming");
                                    }
                                    let label = if badges.is_empty() {
                                        model.runtime_id.clone()
                                    } else {
                                        let suffix = badges
                                            .iter()
                                            .map(|b| format!("[{}]", b))
                                            .collect::<Vec<_>>()
                                            .join(" ");
                                        format!("{} {}", model.runtime_id, suffix)
                                    };
                                    app.push_msg(ChatMessage::System(format!(
                                        "      model {}",
                                        label
                                    )));
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
                                let mut by_provider: std::collections::BTreeMap<&str, Vec<&str>> =
                                    std::collections::BTreeMap::new();
                                for (pid, key) in &diag.provider_missing {
                                    by_provider
                                        .entry(pid.as_str())
                                        .or_default()
                                        .push(key.as_str());
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
                    let diags: Vec<synaps_cli::extensions::config::ExtensionConfigDiagnostics> =
                        match &id {
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
                            app.push_msg(ChatMessage::System(
                                "  (no manifest config entries)".to_string(),
                            ));
                        }
                        for entry in &diag.entries {
                            let source_label = match &entry.source {
                                synaps_cli::extensions::config::ConfigSource::EnvOverride(name) => {
                                    format!("env override ({})", name)
                                }
                                synaps_cli::extensions::config::ConfigSource::SecretEnv(name) => {
                                    format!("secret env ({})", name)
                                }
                                synaps_cli::extensions::config::ConfigSource::PluginConfig => {
                                    "plugin config".to_string()
                                }
                                synaps_cli::extensions::config::ConfigSource::LegacyConfigKey(
                                    name,
                                ) => format!("legacy config key ({})", name),
                                synaps_cli::extensions::config::ConfigSource::Default => {
                                    "default".to_string()
                                }
                                synaps_cli::extensions::config::ConfigSource::Missing => {
                                    "missing".to_string()
                                }
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
                    use crate::tui::commands::ExtensionsTrustAction;
                    match action {
                        ExtensionsTrustAction::List => {
                            let manager = ext_mgr_shared.read().await;
                            let providers = manager.provider_summaries();
                            let trust = synaps_cli::extensions::trust::load_trust_state()
                                .unwrap_or_default();
                            if providers.is_empty() {
                                app.push_msg(ChatMessage::System(
                                    "No providers registered.".to_string(),
                                ));
                            } else {
                                app.push_msg(ChatMessage::System(format!(
                                    "Provider trust ({}):",
                                    providers.len()
                                )));
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
                                    synaps_cli::extensions::trust::enable_provider(
                                        &mut state,
                                        &runtime_id,
                                    );
                                    match synaps_cli::extensions::trust::save_trust_state(&state) {
                                        Ok(()) => app.push_msg(ChatMessage::System(format!(
                                            "Provider '{}' enabled.",
                                            runtime_id
                                        ))),
                                        Err(e) => app.push_msg(ChatMessage::Error(format!(
                                            "failed to save trust state: {}",
                                            e
                                        ))),
                                    }
                                }
                                Err(e) => app.push_msg(ChatMessage::Error(format!(
                                    "failed to load trust state: {}",
                                    e
                                ))),
                            }
                        }
                        ExtensionsTrustAction::Disable { runtime_id, reason } => {
                            match synaps_cli::extensions::trust::load_trust_state() {
                                Ok(mut state) => {
                                    synaps_cli::extensions::trust::disable_provider(
                                        &mut state,
                                        &runtime_id,
                                        reason.clone(),
                                    );
                                    match synaps_cli::extensions::trust::save_trust_state(&state) {
                                        Ok(()) => {
                                            let suffix = match &reason {
                                                Some(r) => format!(" [reason: {}]", r),
                                                None => String::new(),
                                            };
                                            app.push_msg(ChatMessage::System(format!(
                                                "Provider '{}' disabled.{}",
                                                runtime_id, suffix
                                            )));
                                        }
                                        Err(e) => app.push_msg(ChatMessage::Error(format!(
                                            "failed to save trust state: {}",
                                            e
                                        ))),
                                    }
                                }
                                Err(e) => app.push_msg(ChatMessage::Error(format!(
                                    "failed to load trust state: {}",
                                    e
                                ))),
                            }
                        }
                    }
                }
                CommandAction::ExtensionsAudit { tail } => {
                    // Use bounded tail read — only the last N entries are
                    // deserialised regardless of how large audit.jsonl has grown.
                    let read_result = match tail {
                        Some(n) => synaps_cli::extensions::audit::read_audit_entries_tail(n),
                        None => synaps_cli::extensions::audit::read_audit_entries(),
                    };
                    match read_result {
                        Ok(entries) => {
                            let slice = entries;
                            if slice.is_empty() {
                                app.push_msg(ChatMessage::System(
                                    "No audit entries yet.".to_string(),
                                ));
                            } else {
                                app.push_msg(ChatMessage::System(format!(
                                    "Audit ({} entries):",
                                    slice.len()
                                )));
                                for e in slice {
                                    let stream_tag = if e.streamed {
                                        "[streamed]"
                                    } else {
                                        "[complete]"
                                    };
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
                            "failed to read audit log: {}",
                            e
                        ))),
                    }
                }
                CommandAction::ExtensionsMemory(action) => {
                    use crate::tui::commands::ExtensionsMemoryAction;
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
                                        "Memory namespaces ({}):",
                                        nss.len()
                                    )));
                                    for ns in nss {
                                        app.push_msg(ChatMessage::System(format!("  {}", ns)));
                                    }
                                }
                                Err(e) => app.push_msg(ChatMessage::Error(format!(
                                    "failed to list memory namespaces: {}",
                                    e
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
                                        "No records in '{}'.",
                                        namespace
                                    )));
                                }
                                Ok(records) => {
                                    app.push_msg(ChatMessage::System(format!(
                                        "Recent in '{}' ({}):",
                                        namespace,
                                        records.len()
                                    )));
                                    for rec in records {
                                        // ISO8601 / RFC3339 UTC from epoch ms via chrono.
                                        let ts =
                                            chrono::DateTime::<chrono::Utc>::from_timestamp_millis(
                                                rec.timestamp_ms as i64,
                                            )
                                            .map(|dt| {
                                                dt.to_rfc3339_opts(
                                                    chrono::SecondsFormat::Secs,
                                                    true,
                                                )
                                            })
                                            .unwrap_or_else(|| rec.timestamp_ms.to_string());
                                        // Truncate content at 80 chars (char-aware).
                                        let mut content: String =
                                            rec.content.chars().take(80).collect();
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
                                            "  {} {} {}",
                                            ts, tags, content
                                        )));
                                    }
                                }
                                Err(e) => app.push_msg(ChatMessage::Error(format!(
                                    "failed to query memory '{}': {}",
                                    namespace, e
                                ))),
                            }
                        }
                    }
                }

                CommandAction::Ping => {
                    app.push_msg(ChatMessage::System("📡 Pinging models...".to_string()));
                    app.ping_print = true;
                    // Configured-ness comes from the credential broker; no
                    // provider keys are read or held here.
                    let count: usize = synaps_cli::runtime::openai::registry::providers()
                        .iter()
                        .filter(|s| {
                            synaps_cli::runtime::openai::registry::resolve_provider_model(
                                s.key,
                                s.default_model,
                            )
                            .is_some()
                        })
                        .map(|s| s.models.len())
                        .sum();
                    app.ping_pending = count;
                    let health_tx = app.ping_tx.clone();
                    tokio::spawn(async move {
                        synaps_cli::runtime::openai::ping::ping_all_configured(health_tx).await;
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
                            "sidecar unavailable: no plugin provides a sidecar binary".to_string(),
                        ));
                        return ControlFlow::Continue(());
                    };

                    if app.sidecars.contains_key(&target_pid) {
                        // Subsequent toggle on existing sidecar — arm flag is source of truth.
                        let label = app
                            .sidecars
                            .get(&target_pid)
                            .and_then(|s| s.display_name.as_deref())
                            .unwrap_or("sidecar")
                            .to_string();
                        let v = app.sidecars.get_mut(&target_pid).unwrap();
                        if v.armed {
                            v.armed = false;
                            if let Err(err) = v.manager.release().await {
                                app.push_msg(ChatMessage::Error(format!(
                                    "{label} release failed: {err}"
                                )));
                            }
                            app.push_msg(ChatMessage::System(format!(
                                "{label}: stopping — final transcript will be appended"
                            )));
                        } else {
                            v.armed = true;
                            if let Err(err) = v.manager.press().await {
                                v.armed = false;
                                app.push_msg(ChatMessage::Error(format!(
                                    "{label} press failed: {err}"
                                )));
                            }
                        }
                    } else {
                        // Spawn new sidecar instance for target_pid.
                        let Some(discovered) =
                            all.into_iter().find(|s| s.plugin_name == target_pid)
                        else {
                            app.push_msg(ChatMessage::Error(format!(
                                "sidecar plugin '{}' not discoverable",
                                target_pid,
                            )));
                            return ControlFlow::Continue(());
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
                        )
                        .await
                        {
                            Ok(mut state) => {
                                let claims = registry.lifecycle_claims();
                                let display = loop_arms::pick_display_name_for_plugin(
                                    &state.sidecar.plugin_name,
                                    &claims,
                                );
                                state.set_display_name(display);
                                let label = state
                                    .display_name
                                    .clone()
                                    .unwrap_or_else(|| "sidecar".to_string());
                                let plugin_key = state.sidecar.plugin_name.clone();
                                app.sidecars.insert(plugin_key.clone(), state);
                                app.push_msg(ChatMessage::System(format!(
                                    "{label} active — press the toggle again to stop"
                                )));
                                if let Some(v) = app.sidecars.get_mut(&plugin_key) {
                                    v.armed = true;
                                    if let Err(err) = v.manager.press().await {
                                        v.armed = false;
                                        v.status =
                                            self::sidecar::SidecarUiStatus::Error(err.to_string());
                                        app.push_msg(ChatMessage::Error(format!(
                                            "{label} press failed: {err}"
                                        )));
                                    }
                                }
                            }
                            Err(err) => {
                                app.push_msg(ChatMessage::Error(format!(
                                    "sidecar unavailable: {err}"
                                )));
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
                        let mut lines: Vec<String> =
                            app.sidecars.values().map(|v| v.status_line()).collect();
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
                return ControlFlow::Continue(());
            }
            let display_text = app.user_display_text_for_submission(&input);
            app.push_msg(ChatMessage::User(display_text));
            app.input_before_paste = None;
            app.pasted_char_count = 0;
            // Real user send — reset auto-turn counter.
            app.consecutive_auto_turns = 0;
            // Inject abort context if previous response was interrupted
            let api_content = if let Some(ref ctx) = app.abort_context {
                let combined = format!("{}\n\n{}", ctx, input);
                app.abort_context = None;
                combined
            } else {
                input
            };
            app.api_messages.push(std::sync::Arc::new(
                json!({"role": "user", "content": api_content}),
            ));
            let ct = CancellationToken::new();
            let (s_tx, s_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
            app.status_text = Some("connecting…".to_string());
            app.streaming = true;
            app.spinner_frame = 0;
            let term_size = crossterm::terminal::size()
                .map(|(w, h)| ratatui::layout::Size {
                    width: w,
                    height: h,
                })
                .unwrap_or_default();
            let built =
                build_render_model(&mut ViewInputs::from_app(app), runtime, registry, term_size);
            if let Some((model, patch)) = built {
                patch.apply(app);
                render_handle.publish(model);
            }
            *stream = Some(
                runtime
                    .run_stream_with_messages(
                        app.api_messages.clone(),
                        ct.clone(),
                        Some(s_rx),
                        Some(secret_prompt_handle.clone()),
                        false,
                    )
                    .await,
            );
            app.status_text = None;
            app.push_msg(ChatMessage::Thinking(THINKING_PLACEHOLDER.to_string()));
            *cancel_token = Some(ct);
            *steer_tx = Some(s_tx);
        }
        InputAction::StreamingInput(input) => {
            // Check for streaming slash commands
            if let Some(rest) = input.strip_prefix('/') {
                let raw_cmd = rest.split_whitespace().next().unwrap_or("");
                let streaming_cmds = commands::to_owned_commands(commands::STREAMING_COMMANDS);
                let cmd = commands::resolve_prefix(raw_cmd, &streaming_cmds);
                match commands::handle_streaming_command(&cmd, &input, app) {
                    CommandAction::None => {
                        // Not a streaming-safe command. If it's still a KNOWN
                        // command (settings, model, system, etc.), refuse with
                        // a clear message — don't leak command text into the
                        // model stream as steering input.
                        let all_cmds = commands::all_commands_with_skills(registry);
                        let resolved_full = commands::resolve_prefix(raw_cmd, &all_cmds);
                        if all_cmds.iter().any(|c| c == &resolved_full) {
                            app.push_msg(ChatMessage::System(format!(
                                "/{} can't run while streaming — press Esc to cancel first",
                                resolved_full
                            )));
                        } else {
                            // Unknown slash text — treat as steering
                            let steered = steer_tx
                                .as_ref()
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
                        render_handle.send_exit_fx(quit_effect());
                        *exit_fx_sent = true;
                    }
                    CommandAction::LaunchGamba => {
                        drop(event_reader.take());
                        // Pause the render thread BEFORE touching the terminal —
                        // eliminates the stdout race between terminal.draw() and our mode changes.
                        render_handle.pause();
                        match app.launch_gamba() {
                            Ok(()) => {}
                            Err(msg) => {
                                // launch failed — restore and resume
                                render_handle.resume();
                                app.push_msg(ChatMessage::Error(msg));
                            }
                        }
                        // If gamba launched OK, resume is sent by reclaim/check_gamba_exited.
                        *event_reader = Some(EventStream::new());
                    }
                    CommandAction::StartStream => {}
                    CommandAction::OpenModels => {}
                    CommandAction::OpenEffort => {}
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
                let steered = steer_tx
                    .as_ref()
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
            let status = synaps_cli::engine::commands::persist_to_config("model", &applied);
            app.session.model = applied.clone();
            app.push_msg(ChatMessage::System(format!(
                "model set to: {} {}",
                applied, status
            )));
        }
        InputAction::EffortApply(value) => {
            // Race-safe apply gate: a stream may have started between the
            // lightbox opening and this apply (queued-message auto-starts).
            // Reject without ANY state/config mutation; otherwise reuse the
            // existing checked mutation + persistence path (identical to
            // /thinking: set_reasoning_level_checked → persist → session).
            match effort::apply_guard(app.streaming || stream.is_some(), &value, runtime.model()) {
                Ok(level) => match runtime.set_reasoning_level_checked(level) {
                    Ok(()) => {
                        let canonical = level.as_str();
                        app.session.thinking_level = canonical.to_string();
                        let status =
                            synaps_cli::engine::commands::persist_to_config("thinking", canonical);
                        app.push_msg(ChatMessage::System(format!(
                            "effort set to: {} {}",
                            canonical, status
                        )));
                    }
                    Err(e) => app.push_msg(ChatMessage::Error(e)),
                },
                Err(e) => app.push_msg(ChatMessage::Error(e)),
            }
        }
        InputAction::ModelsExpandProvider(provider_key) => {
            if provider_key == "openai-codex" {
                // Source-controlled provider: never fetch a live catalog.
                // Resolve through the same canonicalizing result path.
                let _ = app
                    .model_list_tx
                    .send((provider_key, Ok(models::codex_static_expanded_entries())));
                return ControlFlow::Continue(());
            }
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
                            Err(format!(
                                "extension provider '{}' is not loaded",
                                provider_key
                            ))
                        }
                    } else {
                        Err("extension provider registry is not available".to_string())
                    };
                    let _ = tx.send((provider_key, result));
                });
                return ControlFlow::Continue(());
            }
            let client = runtime.http_client().clone();
            let tx = app.model_list_tx.clone();
            tokio::spawn(async move {
                if let Ok(provider) = provider_key.parse::<synaps_cli::auth::CloudProviderId>() {
                    let broker = synaps_cli::auth::global_broker();
                    let result = broker
                        .cloud_catalog(provider, provider.as_str(), true)
                        .await
                        .map(|entries| {
                            entries
                                .into_iter()
                                .map(|entry| {
                                    let route = synaps_cli::auth::cloud::qualify_model_route(
                                        &entry.id,
                                        &entry.context_ref,
                                    )
                                    .unwrap_or(entry.id);
                                    models::ExpandedModelEntry::with_metadata(
                                        route,
                                        entry.display_name,
                                        false,
                                        vec![
                                            entry.context_label,
                                            if entry.stale {
                                                "stale".into()
                                            } else {
                                                "dynamic".into()
                                            },
                                        ],
                                    )
                                })
                                .collect()
                        })
                        .map_err(|e| e.to_string());
                    let _ = tx.send((provider_key, result));
                    return;
                }
                let result = synaps_cli::runtime::openai::catalog::fetch_catalog_models(
                    &client,
                    &provider_key,
                )
                .await
                .map(|models| {
                    models
                        .into_iter()
                        .map(|model| {
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
                                synaps_cli::runtime::openai::catalog::ReasoningSupport::Unknown => {
                                }
                                _ => metadata.push("thinking".to_string()),
                            }
                            if model.pricing.has_internal_reasoning_cost() {
                                metadata.push("reasoning $".to_string());
                            }
                            models::ExpandedModelEntry::with_metadata(
                                full_id, label, false, metadata,
                            )
                        })
                        .collect()
                });
                let _ = tx.send((provider_key, result));
            });
        }
        InputAction::SettingsApply(key, value) => {
            apply_setting(key, &value, app, runtime);
        }
        InputAction::PluginEditorOpen {
            plugin_id,
            category,
            field,
        } => {
            let manager = ext_mgr_shared.read().await;
            match manager
                .settings_editor_open(&plugin_id, &category, &field)
                .await
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
                        // P7.7: PluginCustom editor is a REAL nested pane
                        // ON TOP of Settings — push PaneId::PluginEditor so
                        // the stack becomes [.., Settings, PluginEditor]
                        // (two-deep). Mirrors `edit_mode = Some(PluginCustom)`;
                        // the matching pops live at the Esc path (route_settings)
                        // and the two commit paths below. Only the Ok branch
                        // opens the editor, so the push lives inside it.
                        app.modal_stack.push(focus::PaneId::PluginEditor);
                        #[cfg(debug_assertions)]
                        focus::debug_assert_stack_sync(app);
                    }
                }
                Err(err) => {
                    if let Some(state) = app.settings.as_mut() {
                        state.row_error = Some((format!("plugin.{}.{}", plugin_id, field), err));
                    }
                }
            }
        }
        InputAction::PluginEditorKey {
            plugin_id,
            category,
            field,
            key,
        } => {
            let wire_key = settings::plugin_editor::key_to_wire(key);
            if wire_key == "Enter" {
                let selected = app
                    .settings
                    .as_ref()
                    .and_then(|state| match &state.edit_mode {
                        Some(settings::ActiveEditor::PluginCustom { render, .. }) => {
                            let cursor = render.render.cursor.unwrap_or(0);
                            render.render.rows.get(cursor).and_then(|r| r.data.clone())
                        }
                        _ => None,
                    });
                if let Some(value) = selected {
                    let manager = ext_mgr_shared.read().await;
                    match manager
                        .settings_editor_commit(&plugin_id, &category, &field, value.clone())
                        .await
                    {
                        Ok(reply) => {
                            let effect = settings::plugin_editor::effect_from_commit_reply(
                                &plugin_id, &field, reply,
                            );
                            match effect {
                                settings::plugin_editor::PluginEditorEffect::None => {}
                                settings::plugin_editor::PluginEditorEffect::ConfigWrite {
                                    plugin_id,
                                    key,
                                    value,
                                } => {
                                    match synaps_cli::extensions::config_store::write_plugin_config(
                                        &plugin_id, &key, &value,
                                    ) {
                                        Ok(()) => {
                                            if let Some(state) = app.settings.as_mut() {
                                                state.edit_mode = None;
                                                state.row_error = Some((
                                                    format!("plugin.{}.{}", plugin_id, key),
                                                    "saved".to_string(),
                                                ));
                                                // P7.7: commit cleared the PluginCustom editor ⇒ POP
                                                // PaneId::PluginEditor to keep contains(PluginEditor)
                                                // ⇔ edit_mode==Some(PluginCustom) (settings stays open).
                                                app.modal_stack.pop();
                                                #[cfg(debug_assertions)]
                                                focus::debug_assert_stack_sync(app);
                                            }
                                        }
                                        Err(err) => {
                                            if let Some(state) = app.settings.as_mut() {
                                                state.row_error = Some((
                                                    format!("plugin.{}.{}", plugin_id, key),
                                                    err.to_string(),
                                                ));
                                            }
                                        }
                                    }
                                }
                                settings::plugin_editor::PluginEditorEffect::InvokeCommand {
                                    plugin_id,
                                    command,
                                    args,
                                } => {
                                    if let Some(state) = app.settings.as_mut() {
                                        state.edit_mode = None;
                                        state.row_error = Some((
                                            format!("plugin.{}.{}", plugin_id, field),
                                            "download started".to_string(),
                                        ));
                                        // P7.7: InvokeCommand also clears the PluginCustom editor
                                        // ⇒ POP PaneId::PluginEditor (settings stays open) before
                                        // dispatching the interactive command below.
                                        app.modal_stack.pop();
                                        #[cfg(debug_assertions)]
                                        focus::debug_assert_stack_sync(app);
                                    }
                                    commands::execute_interactive_plugin_command_by_parts(
                                        &plugin_id, &command, args, &manager, app,
                                    )
                                    .await;
                                }
                            }
                        }
                        Err(err) => {
                            if let Some(state) = app.settings.as_mut() {
                                state.row_error =
                                    Some((format!("plugin.{}.{}", plugin_id, field), err));
                            }
                        }
                    }
                }
            } else {
                let manager = ext_mgr_shared.read().await;
                match manager
                    .settings_editor_key(&plugin_id, &category, &field, &wire_key)
                    .await
                    .and_then(settings::plugin_editor::render_from_key_result)
                {
                    Ok(Some(render)) => {
                        if let Some(settings::ActiveEditor::PluginCustom {
                            render: session, ..
                        }) = app.settings.as_mut().and_then(|s| s.edit_mode.as_mut())
                        {
                            session.render = render;
                        }
                    }
                    Ok(None) => {}
                    Err(err) => {
                        if let Some(state) = app.settings.as_mut() {
                            state.row_error =
                                Some((format!("plugin.{}.{}", plugin_id, field), err));
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
                    PO::InstallRequested {
                        marketplace,
                        plugin,
                    } => {
                        plugins::actions::apply_install(
                            state,
                            marketplace,
                            plugin,
                            registry,
                            config,
                        )
                        .await;
                    }
                    PO::TrustAndInstall {
                        plugin_name,
                        host,
                        source,
                        summary,
                    } => {
                        plugins::actions::apply_trust_and_install(
                            state,
                            plugin_name,
                            host,
                            source,
                            summary,
                            registry,
                            config,
                        )
                        .await;
                    }
                    PO::Uninstall(name) => {
                        plugins::actions::apply_uninstall(state, name, registry, config).await;
                    }
                    PO::Update(name) => {
                        plugins::actions::apply_update(state, name, registry, config).await;
                    }
                    PO::RefreshMarketplace(name) => {
                        plugins::actions::apply_refresh_marketplace(state, name).await;
                    }
                    PO::ConfirmPendingInstall => {
                        plugins::actions::apply_confirm_pending_install(state, registry, config)
                            .await;
                    }
                    PO::CancelPendingInstall => {
                        plugins::actions::apply_cancel_pending_install(state);
                    }
                    PO::ConfirmPendingUpdate => {
                        plugins::actions::apply_confirm_pending_update(state, registry, config)
                            .await;
                    }
                    PO::CancelPendingUpdate => {
                        plugins::actions::apply_cancel_pending_update(state);
                    }
                    PO::RemoveMarketplace(name) => {
                        plugins::actions::apply_remove_marketplace(state, name, registry, config)
                            .await;
                    }
                    PO::TogglePlugin { name, enabled } => {
                        plugins::actions::apply_toggle_plugin(
                            state, name, enabled, registry, config,
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
                    // P7.7: marketplace-from-settings is now a TRUE
                    // two-deep push. Settings is stack-routed (still
                    // Some here — the marketplace opens ON TOP), so the
                    // stack becomes [Settings, Plugins] (depth 2).
                    // Plugins is top() and gets input; on Close
                    // route_plugins pops back to [Settings] and
                    // route_settings resumes routing the still-open
                    // settings modal — behaviour-identical to the old
                    // chain fallthrough. GATE-1 note B: opened on this
                    // async arm, so assert sync inline.
                    app.modal_stack.push(focus::PaneId::Plugins);
                    #[cfg(debug_assertions)]
                    focus::debug_assert_stack_sync(app);
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
            let health_tx = app.ping_tx.clone();
            tokio::spawn(async move {
                synaps_cli::runtime::openai::ping::ping_all_configured(health_tx).await;
            });
        }
    }
    ControlFlow::Continue(())
}
