//! Chat TUI binary — event loop, terminal setup, module wiring.
mod transcript;


mod app;
mod clock;
mod commands;
mod dispatch;
mod draw;
mod focus;
pub(crate) mod text_metrics;
mod gamba;
mod help_find;
mod helpers;
mod highlight;
mod input;
mod lifecycle;
mod lightbox;
mod markdown;
mod models;
mod plugins;
mod render;
mod render_model;
mod render_thread;
mod run_setup;
mod settings;
mod sidecar;
mod signals;
mod stream_handler;
/// Headless test harness — see [`testing::TestHarness`]. Compiled only for
/// in-crate tests or downstream consumers of the `testing` feature.
#[cfg(any(test, feature = "testing"))]
pub mod testing;
mod theme;
mod toast;
mod viewport;

/// Single process-global lock for ALL tests that mutate config-env vars
/// (`SYNAPS_BASE_DIR`, `HOME`).  Both `migration_tests` (this file) and the
/// `BASE_DIR_TEST_LOCK` tests in `plugins/actions.rs` must hold this lock for
/// the duration of any test that sets or reads those vars, so the two groups
/// can never interleave even when `cargo test` runs them on parallel threads.
#[cfg(test)]
pub(crate) static CONFIG_ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

use app::{App, ChatMessage, THINKING_PLACEHOLDER};
use commands::CommandAction;
use draw::{boot_effect, build_render_model, quit_effect};
use helpers::{apply_setting, fetch_usage, rebuild_display_messages, should_draw};
use input::InputAction;
use lifecycle::setup_terminal;
use render_thread::spawn_render_thread;
use stream_handler::StreamAction;

use crossterm::event::EventStream;
use futures::StreamExt;
use serde_json::json;
use std::sync::atomic::Ordering;
use std::time::Instant;
use synaps_cli::core::session_index::SessionIndexRecord;
use synaps_cli::runtime::compaction::compact_conversation;
use synaps_cli::{CancellationToken, Result, Runtime, Session};

pub async fn run(
    continue_session: Option<Option<String>>,
    system: Option<String>,
    profile: Option<String>,
    no_extensions: bool,
) -> Result<()> {
    // ── Boot prologue (P12.1: extracted to run_setup.rs) ──
    // run_setup performs engine-boot unpack, App construction (incl. the
    // resumed-session branch), channel/handle setup, render-thread spawn,
    // and tick-throttle init. Behavior byte-identical; the loop below owns
    // every field by value exactly as it did when they were locals.
    let run_setup::RunContext {
        mut app,
        mut runtime,
        mut config,
        registry,
        keybind_registry,
        system_prompt_path,
        render_handle,
        boot_done,
        exit_done,
        event_reader,
        mut shutdown_signal_rx,
        shutdown_signal_task,
        mut stream,
        secret_prompt_handle,
        secret_prompt_rx,
        mut cancel_token,
        mut steer_tx,
        background,
        ext_mgr_shared,
        mut boot_fx_sent,
        mut exit_fx_sent,
        mut last_draw,
    } = run_setup::run_setup(continue_session, system, profile, no_extensions).await?;
    // P12.2: Option-wrapped so dispatch::handle_input_action can drop the
    // reader early (gamba terminal handoff) through a `&mut` without moving
    // it out of the loop. Always Some outside the dispatch call.
    let mut event_reader = Some(event_reader);
    loop {
        // Only draw when something actually changed. During streaming, coalesce
        // redraws to the configured frame budget (`max_fps`, default 60fps =
        // ~16ms) — deltas and the spinner arrive faster than the eye reads, and
        // building/publishing the RenderModel per frame is main-thread work
        // (it re-renders the streaming message's markdown each frame). User
        // input bypasses the cap via `force_redraw` so scroll/typing stays
        // instant, and the `!app.streaming` short-circuit renders the final/idle
        // frame immediately so end-of-turn state never lags. Tune via
        // `max_fps = 60|144|240|…` in ~/.synaps-cli/config. (Was a hardcoded
        // 100ms/10fps #131 throttle; 0.3.6 made publish O(viewport) so the cap
        // could be raised to a real frame rate without burning a core.)
        let throttle = std::time::Duration::from_millis(1000 / config.max_fps.max(1) as u64);
        if should_draw(app.needs_redraw, app.force_redraw, app.streaming, last_draw.elapsed(), throttle) {
            // Terminal lives on the render thread — get size via the crossterm
            // TTY syscall directly (doesn't need the Terminal object).
            // Skip the frame entirely if the reported size is 0×0 (terminal not
            // yet ready, or a transient resize event) — publishing a 0×0 model
            // would produce layout artifacts.
            let term_size = match crossterm::terminal::size() {
                Ok((w, h)) if w > 0 && h > 0 => ratatui::layout::Size { width: w, height: h },
                _ => {
                    // Terminal not yet ready or transient resize — clear redraw
                    // flags and back off so we don't busy-spin when the size is
                    // 0×0 or the syscall fails (#tui-safety fix 1).
                    app.needs_redraw = false;
                    app.force_redraw = false;
                    last_draw = Instant::now();
                    continue;
                }
            };
            app.needs_redraw = false;
            app.force_redraw = false;
            last_draw = Instant::now();
            if let Some(model) = build_render_model(
                &mut app,
                &runtime,
                &registry,
                term_size,
            ) {
                render_handle.publish(model);
            }
        }

        tokio::select! {

            // ── OS shutdown signals: Ctrl-C from terminal, SIGTERM from systemd/tmux/SSH ──
            signal = shutdown_signal_rx.recv() => {
                if let Some(signal) = signal {
                    tracing::info!(signal = signals::signal_label(signal), "chat UI shutdown signal received");
                    // All OS signals map to ImmediateExit (see signals.rs).
                    // The /quit command sends SpawnExitFx to the render thread
                    // and does NOT go through this path, so removing AnimatedExit
                    // from signals does not affect interactive quit.
                    let signals::ShutdownAction::ImmediateExit = signals::shutdown_action(signal);
                    tracing::info!("immediate exit on {:?}", signal);
                    // Cancel any in-flight stream so the tool/subagent is not
                    // orphaned for the full watchdog window.
                    if let Some(ref ct) = cancel_token { ct.cancel(); }
                    // Abort any in-flight compaction so it doesn't hold state
                    // open past the teardown budget.
                    if let Some(ref h) = app.compact_task { h.abort(); }
                    // Fall through to unified bounded-teardown below the loop.
                    break;
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
                        app.request_redraw();
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
                    app.request_redraw();
                }
            }

            // ── Async extension loader progress ──
            event = app.extension_loader_rx.recv(), if app.extension_loader_running => {
                if let Some(event) = event {
                    handle_extension_loader_event(&mut app, &runtime, event, &ext_mgr_shared).await;
                } else {
                    app.extension_loader_running = false;
                    app.toasts.dismiss("extension-loader");
                }
                app.request_redraw();
            }

            // ── Widget events from background extension notification watchers ──
            Some(widget_event) = app.widget_rx.recv() => {
                // Only redraw when the widget's VISIBLE content actually changed.
                // Plugins (d20/jawz-widget/synaps-tasks) re-send unchanged widgets
                // on a poll loop; redrawing on every one pinned the render loop at
                // ~30% CPU at idle (#119). The dirty-check in upsert/dismiss makes an
                // idle session genuinely idle.
                if handle_widget_event(&mut app, widget_event) {
                    app.request_redraw();
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
                    app.request_redraw();
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
                            stream = Some(runtime.run_stream_with_messages(app.api_messages.clone(), ct.clone(), Some(s_rx), Some(secret_prompt_handle.clone()), false).await);
                            app.push_msg(ChatMessage::Thinking(THINKING_PLACEHOLDER.to_string()));
                            cancel_token = Some(ct);
                            steer_tx = Some(s_tx);
                        }
                    }
                }
            }

            // ── Tick: animations + spinner (~60fps when active) ──
            _ = tokio::time::sleep(std::time::Duration::from_millis(16)), if boot_fx_sent || exit_fx_sent || app.streaming || app.compact_task.is_some() || app.transcript.is_empty() || app.logo_dismiss_t.is_some() || app.logo_build_t.is_some() || app.gamba_child.is_some() || app.secret_prompts.is_active() || !app.toasts.is_empty() || app.plugins.as_ref().is_some_and(|p| p.is_install_active()) => {
                // Active animations/effects always need a redraw each tick.
                // messages.is_empty() = idle logo screen — its color gradient
                // is time-based and needs ticking too (S206 regression: the
                // dirty-flag loop froze it until first keystroke).
                // Update local effect-sent flags from the render thread's done signals.
                if boot_fx_sent && boot_done.load(Ordering::Acquire) {
                    boot_fx_sent = false;
                }
                if exit_fx_sent || boot_fx_sent || app.streaming || app.logo_build_t.is_some() || app.logo_dismiss_t.is_some() || app.gamba_child.is_some() || app.transcript.is_empty() {
                    app.request_redraw();
                }
                app.secret_prompts.poll_requests(&secret_prompt_rx);
                // P7.8: activation/deactivation happen OUTSIDE any input event
                // (async queue + auto-chaining); reconcile the stack to the
                // queue's is_active() so SecretPrompt is pushed/popped (§5).
                input::reconcile_secret_prompt(&mut app);
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
                if message_animation_needs_clear
                    && crossterm::terminal::size().is_ok_and(|(w, h)| w > 0 && h > 0) {
                        render_handle.send_clear();
                    }
                if let Some(ref mut t) = app.logo_build_t {
                    *t += 0.025;
                    if *t >= 1.0 { app.logo_build_t = None; }
                    app.request_redraw();
                }
                if let Some(ref mut t) = app.logo_dismiss_t {
                    *t += 0.04;
                    if *t >= 1.0 { app.logo_dismiss_t = None; }
                    app.request_redraw();
                }
                if app.advance_animations() {
                    // Spinner ticks only affect the tail message (THINKING_PLACEHOLDER,
                    // active tool animation). Mark just the last slot dirty instead of
                    // full invalidation — O(1) instead of O(n) per frame.
                    app.invalidate_last();
                }
                if let Some(msg) = app.check_gamba_exited() {
                    // check_gamba_exited() already called restore_terminal();
                    // resume the render thread now that we own the terminal again.
                    render_handle.resume();
                    app.push_msg(ChatMessage::System(msg));
                    app.invalidate(); // invalidate already sets needs_redraw
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
                if exit_done.load(Ordering::Acquire) {
                    break;
                }
                continue;
            }

            // ── Input: keyboard, mouse, paste ──
            maybe_event = event_reader.as_mut().expect("event_reader is always Some outside dispatch").next(), if app.gamba_child.is_none() => {
                match maybe_event {
                    Some(Ok(event)) => {
                        // P7.8: the secret-prompt interception is gone — SecretPrompt
                        // is a stack-routed pane (`route_secret_prompt`), dispatched
                        // below by `input::handle_event` on `modal_stack.top()`.
                        let is_streaming = app.streaming;
                        // Scope the registry read guard to this block so it is
                        // provably released before any later `.await`
                        // (clippy::await_holding_lock) — the guard never spans a
                        // yield point.
                        let action = {
                            let kb_guard = keybind_registry.read().expect("keybind registry poisoned");
                            input::handle_event(event, &mut app, &runtime, is_streaming, &registry, &kb_guard, config.scroll_lines.unwrap_or(3))
                        };
                        // Input events (keys, mouse, paste, resize) almost always
                        // change visible state (cursor, input buffer, scroll) and
                        // must feel instant — bypass the streaming redraw throttle.
                        app.request_immediate_redraw();
                        // P12.2: the ~1,300-line InputAction dispatch match moved
                        // verbatim to dispatch::handle_input_action (dispatch.rs).
                        // LoopState lends exactly the loop-locals the match
                        // touched; Break maps to the outer `break` (unused today,
                        // honored anyway), Continue to the old fall-through.
                        let state = dispatch::LoopState {
                            app: &mut app,
                            runtime: &mut runtime,
                            config: &mut config,
                            registry: &registry,
                            keybind_registry: &keybind_registry,
                            system_prompt_path: &system_prompt_path,
                            render_handle: &render_handle,
                            event_reader: &mut event_reader,
                            stream: &mut stream,
                            secret_prompt_handle: &secret_prompt_handle,
                            cancel_token: &mut cancel_token,
                            steer_tx: &mut steer_tx,
                            ext_mgr_shared: &ext_mgr_shared,
                            exit_fx_sent: &mut exit_fx_sent,
                        };
                        if dispatch::handle_input_action(action, state).await.is_break() {
                            break;
                        }
                    }
                    // FIX C (defense in depth): EventStream yields Err or None when
                    // crossterm detects the PTY is gone. Break cleanly here.
                    // NOTE: on some kernels crossterm's EPOLL loop can spin without ever
                    // yielding Err/None on a dead PTY (the confirmed busy-loop bug). The
                    // render thread's I/O error path is the backstop: it logs the error
                    // and keeps rendering until the main loop tears down (does NOT break
                    // the render loop on a single I/O error).
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
                                // Reclaim gamba if running — resume render thread
                                // after reclaim restores the terminal.
                                if let Some(msg) = app.reclaim_gamba() {
                                    render_handle.resume();
                                    app.push_msg(ChatMessage::System(msg));
                                    app.invalidate();
                                }
                            }
                        }
                        StreamAction::AutoSendQueued(queued) => {
                            // Drop old stream state (important for cleanup)
                            drop(stream.take());
                            drop(cancel_token.take());
                            drop(steer_tx.take());
                            // Reclaim gamba if running — resume render thread
                            // after reclaim restores the terminal.
                            if let Some(msg) = app.reclaim_gamba() {
                                render_handle.resume();
                                app.push_msg(ChatMessage::System(msg));
                                app.invalidate();
                            }
                            // Auto-send the queued message
                            app.push_msg(ChatMessage::User(queued.clone()));
                            app.transcript.scroll_to_bottom();
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
                            let term_size = crossterm::terminal::size().map(|(w, h)| ratatui::layout::Size { width: w, height: h }).unwrap_or_default();
                            if let Some(model) = build_render_model(&mut app, &runtime, &registry, term_size) {
                                render_handle.publish(model);
                            }
                            stream = Some(runtime.run_stream_with_messages(app.api_messages.clone(), ct.clone(), Some(s_rx), Some(secret_prompt_handle.clone()), false).await);
                            app.status_text = None;
                            app.push_msg(ChatMessage::Thinking(THINKING_PLACEHOLDER.to_string()));
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
                            stream = Some(runtime.run_stream_with_messages(app.api_messages.clone(), ct.clone(), Some(s_rx), Some(secret_prompt_handle.clone()), false).await);
                            app.push_msg(ChatMessage::Thinking(THINKING_PLACEHOLDER.to_string()));
                            cancel_token = Some(ct);
                            steer_tx = Some(s_tx);
                        }
                    }

                    if do_draw {
                        let term_size = crossterm::terminal::size().map(|(w, h)| ratatui::layout::Size { width: w, height: h }).unwrap_or_default();
                        if let Some(model) = build_render_model(&mut app, &runtime, &registry, term_size) {
                            render_handle.publish(model);
                        }
                    }
                }
            }
        }
    }

    // ── PART 2: Bounded teardown — two sequential budgets.
    //
    // All timing constants are defined in signals.rs (single source of truth):
    //   SAVE_TIMEOUT_SECS  — session save + index record (data safety first)
    //   HOOKS_TIMEOUT_SECS — on_session_end hook emit (concurrent, fail-open)
    //   TEARDOWN_TIMEOUT_SECS = SAVE_TIMEOUT_SECS + HOOKS_TIMEOUT_SECS
    //
    // Session save ALWAYS runs first in its own timeout so slow extension
    // handlers cannot starve it.  Even if the hook budget is exhausted, the
    // session data on disk is already safe before hooks are attempted.
    {
        let session_id = app.session.id.clone();
        let api_messages = app.api_messages.clone();

        // ── STEP 1: Save session data — own bounded timeout, highest priority ──
        let save_fut = async {
            app.save_session().await;

            let mut index_record = SessionIndexRecord::end(&session_id);
            index_record.turns = Some(api_messages.len());
            if let Err(err) = synaps_cli::core::session_index::append_record(&index_record) {
                tracing::warn!("failed to append session end index record: {}", err);
            }
        };

        match tokio::time::timeout(
            std::time::Duration::from_secs(signals::SAVE_TIMEOUT_SECS),
            save_fut,
        )
        .await
        {
            Ok(()) => tracing::debug!("session save completed"),
            Err(_elapsed) => {
                tracing::warn!(
                    budget_secs = signals::SAVE_TIMEOUT_SECS,
                    "session save timed out — data may be incomplete"
                );
                lifecycle::emergency_teardown_terminal();
                std::process::exit(1);
            }
        }

        // ── STEP 2: Fire on_session_end hook — own bounded timeout, after save ──
        //
        // emit_concurrent() dispatches all on_session_end handlers simultaneously
        // under one shared timeout window instead of N×5 s serial.  This is safe
        // because on_session_end only allows `Continue` results — handlers are
        // independent fire-and-forget notification calls (deck, d20, jawz-widget,
        // synaps-tasks each write to their own stores; no ordering dependency).
        //
        // Ordering-safety evidence: HookKind::OnSessionEnd::allowed_action_names()
        // returns &["continue"] exclusively; allows_result() permits only Continue;
        // emit_concurrent() merges injections (N/A here) and treats timeouts as
        // continue (fail-open).  Serial ordering cannot matter when the return
        // value is always Continue and handlers touch disjoint state.
        let transcript = Some(api_messages);
        let hook_event = synaps_cli::extensions::hooks::events::HookEvent::on_session_end(
            &session_id,
            transcript,
        );
        match tokio::time::timeout(
            std::time::Duration::from_secs(signals::HOOKS_TIMEOUT_SECS),
            runtime.hook_bus().emit_concurrent(&hook_event),
        )
        .await
        {
            Ok(_) => tracing::debug!("on_session_end hooks completed"),
            Err(_elapsed) => {
                tracing::warn!(
                    budget_secs = signals::HOOKS_TIMEOUT_SECS,
                    "on_session_end hooks timed out — extensions may not have flushed"
                );
                // Session is already saved above — no data loss here.
                // Fall through to normal teardown.
            }
        }

        tracing::debug!("clean teardown completed");
    }

    // Let extension shutdown continue in the background; exit should not hang on
    // extension post/session-end cleanup or slow child-process teardown.
    let _extension_shutdown =
        synaps_cli::extensions::manager::ExtensionManager::shutdown_all_detached(
            std::sync::Arc::clone(&ext_mgr_shared),
        );
    // Stop the signal-listener thread (signal-hook handle, not a JoinHandle).
    shutdown_signal_task.close();

    // Shut down background tasks (inbox watcher, socket, session registry)
    background.shutdown();

    // ── Render-thread teardown ───────────────────────────────────────────────
    //
    // The render thread owns the Terminal.  We send it a Teardown command and
    // wait for the ack within the combined SAVE + HOOKS budget already spent
    // above.  If the ack doesn't arrive the thread is wedged (dead PTY); we
    // skip the join and let process exit reap it — see RenderHandle::teardown.
    // This self-bounding teardown replaced the old signal watchdog (#116).
    //
    // The render thread's do_teardown() calls emergency_teardown_terminal()
    // (disable_raw_mode + LeaveAlternateScreen + etc.) and show_cursor(), then
    // sends the ack and exits its loop.  The Terminal is dropped when the
    // thread exits — that's safe because crossterm teardown was already done.
    let teardown_budget = std::time::Duration::from_secs(
        signals::TEARDOWN_TIMEOUT_SECS.saturating_sub(signals::SAVE_TIMEOUT_SECS),
    )
    .max(std::time::Duration::from_secs(2));
    let acked = render_handle.teardown(teardown_budget);
    if !acked {
        tracing::warn!("render thread did not ack teardown within budget — watchdog is backstop");
        // emergency_teardown_terminal is a no-op if the terminal is already
        // restored, so calling it here is safe even if the render thread did
        // eventually finish teardown after the timeout.
        lifecycle::emergency_teardown_terminal();
    }

    Ok(())
}

fn handle_widget_event(
    app: &mut App,
    event: synaps_cli::extensions::widgets::ExtensionWidgetEvent,
) -> bool {
    use synaps_cli::extensions::widgets::WidgetEvent;
    match event.event {
        WidgetEvent::Upsert {
            id,
            lines,
            styled_lines,
            position,
            title,
            ttl_secs,
        } => {
            let pos = match position.as_str() {
                "top_left" => toast::ToastPosition::TOP_LEFT,
                "top_center" => toast::ToastPosition::TOP_CENTER,
                "top_right" => toast::ToastPosition::TOP_RIGHT,
                "middle_left" => toast::ToastPosition::MIDDLE_LEFT,
                "center" => toast::ToastPosition::CENTER,
                "middle_right" => toast::ToastPosition::MIDDLE_RIGHT,
                "bottom_left" => toast::ToastPosition::BOTTOM_LEFT,
                "bottom_center" => toast::ToastPosition::BOTTOM_CENTER,
                "bottom_right" => toast::ToastPosition::BOTTOM_RIGHT,
                _ => toast::ToastPosition::TOP_RIGHT,
            };
            let ttl = ttl_secs.map(std::time::Duration::from_secs);
            let mut t = toast::Toast::new(
                format!("widget:{}", id),
                lines.first().cloned().unwrap_or_default(),
            )
            .lines(lines)
            .at(pos)
            .ttl(ttl);
            // Convert styled_lines → rich ratatui Lines if present.
            if let Some(styled) = styled_lines {
                use ratatui::style::Style;
                use ratatui::text::{Line, Span};
                let rich: Vec<Line<'static>> = styled
                    .into_iter()
                    .map(|spans| {
                        Line::from(
                            spans
                                .into_iter()
                                .map(|s| {
                                    let mut style = Style::default();
                                    if let Some(ref fg) = s.fg {
                                        if let Some(c) = parse_hex_color(fg) {
                                            style = style.fg(c);
                                        }
                                    }
                                    if let Some(ref bg) = s.bg {
                                        if let Some(c) = parse_hex_color(bg) {
                                            style = style.bg(c);
                                        }
                                    }
                                    Span::styled(s.text, style)
                                })
                                .collect::<Vec<_>>(),
                        )
                    })
                    .collect();
                t = t.rich(rich);
            }
            if let Some(title) = title {
                t = t.titled(title);
            }
            app.toasts.upsert(t)
        }
        WidgetEvent::Dismiss { id } => {
            app.toasts.dismiss(&format!("widget:{}", id))
        }
    }
}

/// Parse a CSS-style hex color string (e.g. "#ff0000") into a ratatui Color.
fn parse_hex_color(s: &str) -> Option<ratatui::style::Color> {
    let s = s.strip_prefix('#')?;
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some(ratatui::style::Color::Rgb(r, g, b))
}

fn handle_extension_loader_toast(app: &mut App, title: &str, lines: Vec<String>, persistent: bool) {
    app.toasts.upsert(
        toast::Toast::new("extension-loader", "")
            .titled(title)
            .lines(lines)
            .at(toast::ToastPosition::TOP_CENTER)
            .ttl(if persistent {
                None
            } else {
                Some(std::time::Duration::from_secs(5))
            }),
    );
    app.invalidate();
}

async fn handle_extension_loader_event(
    app: &mut App,
    runtime: &Runtime,
    event: synaps_cli::extensions::loader::ExtensionLoaderEvent,
    ext_mgr: &std::sync::Arc<
        tokio::sync::RwLock<synaps_cli::extensions::manager::ExtensionManager>,
    >,
) {
    use synaps_cli::extensions::loader::ExtensionLoaderEvent;
    match event {
        ExtensionLoaderEvent::Started => {
            handle_extension_loader_toast(
                app,
                "Extensions",
                vec!["Discovering extensions…".into()],
                true,
            );
        }
        ExtensionLoaderEvent::Loaded {
            plugin,
            loaded,
            failed,
        } => {
            handle_extension_loader_toast(
                app,
                "Extensions",
                vec![
                    format!(
                        "Loaded {loaded} extension{}",
                        if loaded == 1 { "" } else { "s" }
                    ),
                    format!("Latest: {plugin}"),
                    format!("Failures: {failed}"),
                ],
                true,
            );
        }
        ExtensionLoaderEvent::Failed {
            failure,
            loaded,
            failed,
        } => {
            handle_extension_loader_toast(
                app,
                "Extensions",
                vec![
                    format!("Loaded {loaded}, failed {failed}"),
                    format!("⚠ {}", failure.plugin),
                ],
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
            tracing::info!(
                extensions = loaded.len(),
                failures = failed.len(),
                handlers = handler_count,
                "Extension discovery complete"
            );
            let lines = if failed.is_empty() {
                vec![format!(
                    "✓ Loaded {} extension{}",
                    loaded.len(),
                    if loaded.len() == 1 { "" } else { "s" }
                )]
            } else {
                vec![
                    format!(
                        "Loaded {} extension{}",
                        loaded.len(),
                        if loaded.len() == 1 { "" } else { "s" }
                    ),
                    format!("{} failed — see transcript", failed.len()),
                ]
            };
            handle_extension_loader_toast(app, "Extensions", lines, false);

            // Spawn a background notification watcher for each loaded extension.
            // The watcher forwards widget.* notifications to the TUI via widget_tx.
            let handlers = ext_mgr.read().await.handlers();
            for (ext_id, handler) in handlers {
                let widget_tx = app.widget_tx.clone();
                tokio::spawn(async move {
                    loop {
                        let (_sub_id, mut rx) = handler.subscribe_notifications().await;
                        while let Some(frame) = rx.recv().await {
                            if synaps_cli::extensions::widgets::is_widget_method(&frame.method) {
                                if let Ok(event) =
                                    synaps_cli::extensions::widgets::parse_widget_event(
                                        &frame.method,
                                        &frame.params,
                                    )
                                {
                                    let _ = widget_tx.send(
                                        synaps_cli::extensions::widgets::ExtensionWidgetEvent {
                                            extension_id: ext_id.clone(),
                                            event,
                                        },
                                    );
                                }
                            }
                        }
                        // rx closed (EOF/restart) — resubscribe after a brief delay
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    }
                });
            }
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
        let Some(ref cat) = claim.settings_category else {
            continue;
        };
        let new_key = format!("plugins.{}.{}._lifecycle_toggle_key", claim.plugin, cat);
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

    // RAII guard: sets SYNAPS_BASE_DIR for the duration of the test, then
    // restores the previous value (or removes the var) on drop.  This is the
    // canonical override – base_dir() checks SYNAPS_BASE_DIR *before* HOME, so
    // setting it here completely shadows the real ~/.synaps-cli regardless of
    // what HOME is, and is immune to the HOME-vs-SYNAPS_BASE_DIR race that was
    // the root cause of T137 flakiness.
    struct BaseDir {
        _dir: tempfile::TempDir,
        old: Option<String>,
    }

    impl BaseDir {
        /// Create a fresh TempDir, point SYNAPS_BASE_DIR at it, write the
        /// given initial config content into `<tmpdir>/config`, and return the
        /// guard.  The directory is removed automatically when the guard drops.
        fn new(initial_config: &str) -> Self {
            let dir = tempfile::TempDir::new().expect("tempdir");
            let old = std::env::var("SYNAPS_BASE_DIR").ok();
            synaps_cli::config::set_base_dir_for_tests(dir.path().to_path_buf());
            std::fs::write(dir.path().join("config"), initial_config)
                .expect("write test config");
            Self { _dir: dir, old }
        }

        /// Path to the config file that base_dir() resolves to.
        fn config_path(&self) -> std::path::PathBuf {
            self._dir.path().join("config")
        }
    }

    impl Drop for BaseDir {
        fn drop(&mut self) {
            match &self.old {
                Some(v) => std::env::set_var("SYNAPS_BASE_DIR", v),
                None    => std::env::remove_var("SYNAPS_BASE_DIR"),
            }
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
        let _lock = crate::tui::CONFIG_ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _base = BaseDir::new("sidecar_toggle_key = F2\n");

        migrate_sidecar_toggle_key_to_claimed_plugins(&[claim(
            "sample-sidecar",
            "capture",
            Some("capture"),
        )]);
        let v = synaps_cli::config::read_config_value(
            "plugins.sample-sidecar.capture._lifecycle_toggle_key",
        );
        assert_eq!(v.as_deref(), Some("F2"));
    }

    #[test]
    fn migrate_skips_when_new_key_already_set() {
        let _lock = crate::tui::CONFIG_ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _base = BaseDir::new(
            "sidecar_toggle_key = F2\nplugins.sample-sidecar.capture._lifecycle_toggle_key = F12\n",
        );

        migrate_sidecar_toggle_key_to_claimed_plugins(&[claim(
            "sample-sidecar",
            "capture",
            Some("capture"),
        )]);
        let v = synaps_cli::config::read_config_value(
            "plugins.sample-sidecar.capture._lifecycle_toggle_key",
        );
        assert_eq!(v.as_deref(), Some("F12"), "must not overwrite a user-set value");
    }

    #[test]
    fn migrate_is_noop_when_legacy_unset() {
        let _lock = crate::tui::CONFIG_ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _base = BaseDir::new("model = claude-sonnet-4-6\n");

        migrate_sidecar_toggle_key_to_claimed_plugins(&[claim(
            "sample-sidecar",
            "capture",
            Some("capture"),
        )]);
        assert!(synaps_cli::config::read_config_value(
            "plugins.sample-sidecar.capture._lifecycle_toggle_key"
        )
        .is_none());
    }

    #[test]
    fn migrate_skips_claim_without_settings_category() {
        let _lock = crate::tui::CONFIG_ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let base = BaseDir::new("sidecar_toggle_key = F8\n");

        migrate_sidecar_toggle_key_to_claimed_plugins(&[claim("p", "ocr", None)]);
        // No namespaced key written for a claim with no category.
        let contents = std::fs::read_to_string(base.config_path()).unwrap();
        assert!(
            !contents.contains("_lifecycle_toggle_key"),
            "no namespaced key should be written when settings_category is None: {contents}"
        );
    }

    #[test]
    fn migrate_handles_multiple_claims_in_one_pass() {
        let _lock = crate::tui::CONFIG_ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _base = BaseDir::new("sidecar_toggle_key = C-V\n");

        migrate_sidecar_toggle_key_to_claimed_plugins(&[
            claim("sample-sidecar", "capture", Some("capture")),
            claim("ocr-plugin", "ocr", Some("ocr")),
        ]);
        assert_eq!(
            synaps_cli::config::read_config_value(
                "plugins.sample-sidecar.capture._lifecycle_toggle_key"
            )
            .as_deref(),
            Some("C-V")
        );
        assert_eq!(
            synaps_cli::config::read_config_value(
                "plugins.ocr-plugin.ocr._lifecycle_toggle_key"
            )
            .as_deref(),
            Some("C-V")
        );
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
