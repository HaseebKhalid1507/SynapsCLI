//! Chat TUI binary — event loop, terminal setup, module wiring.
mod transcript;

mod app;
mod clock;
mod commands;
mod dispatch;
mod draw;
mod focus;
mod gamba;
mod help_find;
mod helpers;
mod highlight;
mod input;
mod lifecycle;
mod lightbox;
mod loop_arms;
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
/// P16.1: terminal capability facts (env-only detection; inert seam).
mod termcaps;
/// Headless test harness — see [`testing::TestHarness`]. Compiled only for
/// in-crate tests or downstream consumers of the `testing` feature.
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub(crate) mod text_metrics;
mod theme;
mod toast;
mod view_model;
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
        term_caps,
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
    // P16.1+P16.2: terminal capabilities — env detection merged with the
    // DA1-fenced query burst run inside run_setup() (after raw-mode enable,
    // BEFORE the EventStream above was created; see run_setup.rs). Still
    // inert: nothing gates on this until P16.3. The only wiring is this
    // `--verbose` (debug-level) boot line dumping the negotiated caps.
    tracing::debug!(caps = %term_caps.summary(), "negotiated terminal capabilities (env + DA1-fenced burst)");
    // P12.2: Option-wrapped so dispatch::handle_input_action can drop the
    // reader early (gamba terminal handoff) through a `&mut` without moving
    // it out of the loop. Always Some outside the dispatch call.
    let mut event_reader = Some(event_reader);
    // Throttle state for idle subagent reconcile (~1s cadence in the tick arm).
    let mut last_subagent_reconcile: Option<std::time::Instant> = None;
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
        if should_draw(
            app.needs_redraw,
            app.force_redraw,
            app.streaming,
            last_draw.elapsed(),
            throttle,
        ) {
            // Terminal lives on the render thread — get size via the crossterm
            // TTY syscall directly (doesn't need the Terminal object).
            // Skip the frame entirely if the reported size is 0×0 (terminal not
            // yet ready, or a transient resize event) — publishing a 0×0 model
            // would produce layout artifacts.
            let term_size = match crossterm::terminal::size() {
                Ok((w, h)) if w > 0 && h > 0 => ratatui::layout::Size {
                    width: w,
                    height: h,
                },
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
            let built = build_render_model(
                &mut view_model::ViewInputs::from_app(&mut app),
                &runtime,
                &registry,
                term_size,
            );
            if let Some((model, patch)) = built {
                patch.apply(&mut app);
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
                loop_arms::handle_ping_arm(&mut app, result);
            }

            // ── Expanded model-list results ──
            result = app.model_list_rx.recv() => {
                loop_arms::handle_model_list_arm(&mut app, result);
            }

            // ── Async extension loader progress ──
            event = app.extension_loader_rx.recv(), if app.extension_loader_running => {
                loop_arms::handle_extension_loader_arm(&mut app, &runtime, event, &ext_mgr_shared).await;
            }

            // ── Widget events from background extension notification watchers ──
            Some(widget_event) = app.widget_rx.recv() => {
                loop_arms::handle_widget_arm(&mut app, widget_event);
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

            // ── Event bus wake — fires instantly when an event is pushed to the queue.
            // P12.4: arm body moved verbatim to stream_handler::handle_event_queue_arm.
            _ = runtime.event_queue().notified() => {
                stream_handler::handle_event_queue_arm(
                    &mut app, &runtime, &secret_prompt_handle,
                    &mut stream, &mut cancel_token, &mut steer_tx,
                ).await;
            }

            // ── Tick: animations + spinner (~60fps when active) ──
            _ = tokio::time::sleep(std::time::Duration::from_millis(16)), if boot_fx_sent || exit_fx_sent || app.streaming || app.compact_task.is_some() || app.transcript.is_empty() || app.logo_dismiss_t.is_some() || app.logo_build_t.is_some() || app.gamba_child.is_some() || app.secret_prompts.is_active() || !app.toasts.is_empty() || app.plugins.as_ref().is_some_and(|p| p.is_install_active()) || !app.subagents.is_empty() => {
                if loop_arms::handle_animation_tick(
                    &mut app, &runtime, &config, &registry, &render_handle,
                    &secret_prompt_rx, &boot_done, &exit_done,
                    &mut boot_fx_sent, exit_fx_sent,
                    &mut last_subagent_reconcile,
                )
                .await
                {
                    break;
                }
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

            // ── Stream events from runtime. P12.4: the polling future stays
            // inline (it borrows `stream`); the arm body moved verbatim to
            // stream_handler::handle_stream_arm — delta/tool_use/done/abort
            // lifecycle is preserved exactly (hot path).
            maybe_event = async {
                if let Some(ref mut s) = stream {
                    s.next().await
                } else {
                    std::future::pending().await
                }
            } => {
                stream_handler::handle_stream_arm(
                    maybe_event, &mut app, &runtime, &registry, &render_handle,
                    &secret_prompt_handle, &mut stream, &mut cancel_token, &mut steer_tx,
                ).await;
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
