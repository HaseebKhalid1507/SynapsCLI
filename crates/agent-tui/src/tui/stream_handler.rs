//! Stream event handling — processes StreamEvent variants from the runtime.

use serde_json::json;
use synaps_cli::engine::reactor::{
    auto_turn_cap_reached, claim_auto_turn_with_cap, drain_event_queue, wake_action_with_cap,
    EventDisposition, WakeAction,
};
use synaps_cli::{AgentEvent, CancellationToken, LlmEvent, Runtime, SessionEvent, StreamEvent};

use super::app::{App, ChatMessage, SubagentState, THINKING_PLACEHOLDER};
use super::draw::build_render_model;
use super::render_thread::RenderHandle;
use super::view_model::ViewInputs;

/// User-facing "cap reached" notice. Names the configured cap and the config
/// key to raise it. Never emitted when the cap is unlimited (0) — the gate
/// simply never trips.
pub(crate) fn auto_turn_cap_message(cap: u32) -> String {
    format!(
        "auto-turn cap reached ({} consecutive) — waiting for your input \
         (raise with `{} = N`, 0 = unlimited)",
        cap,
        synaps_cli::engine::reactor::AUTO_TURN_CAP_CONFIG_KEY
    )
}

/// What the event loop should do after processing a stream event.
pub(super) enum StreamAction {
    /// Continue processing — no special action needed.
    Continue,
    /// Stream completed and a queued message should be auto-sent.
    AutoSendQueued(String),
    /// Stream completed and buffered events need a model turn.
    AutoTriggerEvents,
}

/// Returns true if the event should trigger an immediate redraw.
pub(super) fn needs_immediate_draw(event: &StreamEvent) -> bool {
    matches!(
        event,
        StreamEvent::Llm(LlmEvent::ToolUse { .. })
            | StreamEvent::Llm(LlmEvent::ToolResult { .. })
            | StreamEvent::Agent(AgentEvent::SubagentStart { .. })
            | StreamEvent::Agent(AgentEvent::SubagentUpdate { .. })
            | StreamEvent::Agent(AgentEvent::SubagentDone { .. })
            | StreamEvent::Agent(AgentEvent::SteeringDelivered { .. })
            | StreamEvent::Session(SessionEvent::Done)
            | StreamEvent::Session(SessionEvent::Error(_))
    )
}

/// Process a StreamEvent, update app state, return what the loop should do.
pub(super) async fn handle_stream_event(
    event: StreamEvent,
    app: &mut App,
    runtime: &Runtime,
) -> StreamAction {
    match event {
        StreamEvent::Llm(LlmEvent::Thinking(text)) => {
            app.append_or_update_thinking(&text);
        }
        StreamEvent::Llm(LlmEvent::Text(text)) => {
            app.append_or_update_text(&text);
        }
        StreamEvent::Llm(LlmEvent::ToolUseStart { tool_name, tool_id }) => {
            app.on_tool_use_start(tool_id, tool_name);
        }
        StreamEvent::Llm(LlmEvent::ToolUseDelta { tool_id, delta }) => {
            app.on_tool_use_delta(&tool_id, &delta);
        }
        StreamEvent::Llm(LlmEvent::ToolUse {
            tool_name,
            tool_id,
            input,
        }) => {
            let input_str = serde_json::to_string(&input).unwrap_or_default();
            app.on_tool_use_finalized(tool_id, tool_name, input_str);
            return StreamAction::Continue;
        }
        StreamEvent::Llm(LlmEvent::ToolResultDelta { delta, tool_id }) => {
            app.on_tool_result_delta(tool_id, delta);
            return StreamAction::Continue;
        }
        StreamEvent::Llm(LlmEvent::ToolResult { result, tool_id }) => {
            app.on_tool_result(tool_id, result);
            return StreamAction::Continue;
        }
        StreamEvent::Session(SessionEvent::MessageHistory(history)) => {
            app.api_messages = history;
            app.save_session().await;
        }
        StreamEvent::Agent(AgentEvent::SubagentStart {
            subagent_id,
            agent_name,
            task_preview,
        }) => {
            app.subagents.push(SubagentState {
                id: subagent_id,
                name: agent_name,
                status: format!("starting: {}", task_preview),
                start_time: app.clock.now(),
                done: false,
                duration_secs: None,
                done_at: None,
            });
            app.invalidate();
        }
        StreamEvent::Agent(AgentEvent::SubagentUpdate {
            subagent_id,
            status,
            ..
        }) => {
            if let Some(sa) = app.subagents.iter_mut().find(|s| s.id == subagent_id) {
                sa.status = status;
            }
            app.invalidate();
        }
        StreamEvent::Agent(AgentEvent::SubagentDone {
            subagent_id,
            result_preview,
            duration_secs,
            ..
        }) => {
            if let Some(sa) = app.subagents.iter_mut().find(|s| s.id == subagent_id) {
                sa.done = true;
                sa.duration_secs = Some(duration_secs);
                sa.done_at = Some(std::time::Instant::now());
                let preview: String = result_preview.chars().take(40).collect();
                if result_preview.starts_with("[TIMED OUT") {
                    sa.status = "\u{26a0} timed out".to_string();
                } else if result_preview.starts_with("ERROR") {
                    sa.status = format!("\u{2718} {}", preview);
                } else {
                    sa.status = format!("\u{2714} {}", preview);
                }
            }
            app.invalidate();
        }
        StreamEvent::Agent(AgentEvent::SteeringDelivered { message }) => {
            // Queue-drained events (subagent completion wakes, watcher
            // alerts) ride the same steering channel as genuine user
            // steering, but they were already presented as Event cards at
            // drain time (handle_event_queue_arm). Rendering them here as
            // ChatMessage::User made subagent continuation text land in the
            // transcript as a message the user typed and submitted.
            if !super::helpers::is_event_payload(&message) {
                app.push_msg(ChatMessage::User(message.clone()));
            }
            if app.queued_message.as_ref() == Some(&message) {
                app.queued_message = None;
            }
            app.transcript.scroll_to_bottom();
            app.invalidate();
        }
        StreamEvent::Session(SessionEvent::Usage {
            input_tokens,
            output_tokens,
            cache_read_input_tokens,
            cache_creation_input_tokens,
            cache_creation_5m,
            cache_creation_1h,
            model: usage_model,
        }) => {
            let model_for_pricing = usage_model.as_deref().unwrap_or(runtime.model());
            app.add_usage(
                input_tokens,
                output_tokens,
                cache_read_input_tokens,
                cache_creation_input_tokens,
                cache_creation_5m,
                cache_creation_1h,
                model_for_pricing,
                Some(runtime.context_window()),
            );
        }
        StreamEvent::Session(SessionEvent::Notice(text)) => {
            // Display-only retry/status notice — system line, not transcript.
            // Notice text can echo API error bodies; strip control chars
            // (esp. ESC) so a hostile payload can't inject terminal escapes.
            app.push_msg(ChatMessage::System(sanitize_notice(&text)));
        }
        StreamEvent::Session(SessionEvent::Done) => {
            app.streaming = false;
            app.drop_empty_thinking();
            // Reconcile HUD against registry instead of clearing — retains running entries
            // whose tx_events is now dead (stream dropped) but threads live on.
            // Poison-recovering lock: a panicked subagent thread must not block teardown.
            {
                let rows = runtime
                    .subagent_registry()
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .display_rows();
                reconcile_subagents(&mut app.subagents, &rows, std::time::Instant::now());
            }
            // NOTE: cleanup_finished removed — engine now reaps at turn completion
            // inside the tokio::spawn wrapper (runtime/mod.rs) before Done is sent.

            // Flush events that arrived during streaming into api_messages
            let had_pending = !app.pending_events.is_empty();
            for formatted in app.pending_events.drain(..) {
                app.api_messages
                    .push(std::sync::Arc::new(serde_json::json!({
                        "role": "user",
                        "content": formatted
                    })));
            }

            // Check for queued message to auto-send
            if let Some(queued) = app.queued_message.take() {
                return StreamAction::AutoSendQueued(queued);
            }

            // If events arrived during streaming, trigger a new model turn
            if had_pending {
                app.save_session().await;
                return StreamAction::AutoTriggerEvents;
            }
        }
        StreamEvent::Session(SessionEvent::Error(err)) => {
            app.drop_empty_thinking();
            // Typed spec §5.2 outcome from the engine — surface the message
            // plus the terminal category + correlation ID, never re-derived.
            app.push_msg(ChatMessage::Error(format!(
                "{} [{}]",
                sanitize_notice(&err.message),
                err.category_label()
            )));
            app.streaming = false;
            // Reconcile HUD against registry on error path too (same as Done).
            {
                let rows = runtime
                    .subagent_registry()
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .display_rows();
                reconcile_subagents(&mut app.subagents, &rows, std::time::Instant::now());
            }
            // Restore a valid trailing state: remove only invalid messages
            // appended by the ACTIVE turn. A pre-existing trailing user
            // message (the prompt that started the turn) is never removed —
            // it stays in history so the failed turn can be retried with
            // full context (spec §5.2).
            synaps_cli::engine::stream::repair_history_after_failure(
                &mut app.api_messages,
                app.turn_baseline,
            );
        }
    }
    StreamAction::Continue
}

// ── P12.4: stream-lifecycle select! arms — pure code-motion from run(). ──
// The select! arm EXPRESSIONS (the event-queue `notified()` wake and the
// `stream.next()` polling future) stay inline in mod.rs; only the arm
// BODIES moved here, verbatim, apart from `*`-derefs for the loop-owned
// locals now borrowed via `&mut` and dropping the `stream_handler::` path
// prefix at the new scope. Behavior byte-identical — streaming lifecycle
// (delta → tool_use → done/abort) is the product's hot path.

/// The in-flight response stream, owned by the `run()` loop and lent to the
/// arm handlers below so they can clear/replace it exactly as the inline
/// arms did.
pub(super) type ActiveStream = std::pin::Pin<Box<dyn futures::Stream<Item = StreamEvent> + Send>>;

/// Event-bus wake arm body: drain queued engine events into the transcript,
/// steer them into an active stream (or buffer), and auto-trigger a model
/// turn when idle.
pub(super) async fn handle_event_queue_arm(
    app: &mut App,
    runtime: &Runtime,
    secret_prompt_handle: &synaps_cli::tools::SecretPromptHandle,
    stream: &mut Option<ActiveStream>,
    cancel_token: &mut Option<CancellationToken>,
    steer_tx: &mut Option<tokio::sync::mpsc::UnboundedSender<String>>,
) {
    let busy = app.streaming || app.compact_task.is_some();

    // Drain via the central reactor function.
    let drained = drain_event_queue(
        runtime.event_queue(),
        &mut app.api_messages,
        &mut app.pending_events,
        busy,
        steer_tx.as_ref(),
    );

    if drained.is_empty() {
        return;
    }

    // Presentation: push each event to the transcript and update the HUD.
    for de in &drained {
        let event = &de.event;
        let severity_str = event
            .content
            .severity
            .as_ref()
            .map(|s| s.as_str().to_string())
            .unwrap_or_else(|| "medium".to_string());
        app.push_msg(ChatMessage::Event {
            source: event.source.source_type.clone(),
            severity: severity_str,
            text: event.content.text.clone(),
        });

        // Seam 3: subagent_completion → mark HUD entry done directly from
        // event data (no lock needed — data was embedded at finalizer time).
        if event.content.content_type == "subagent_completion" {
            if let Some(data) = &event.content.data {
                let maybe_id = data["subagent_id"].as_u64();
                let maybe_status = data["status"].as_str();
                let maybe_duration = data["duration_secs"].as_f64();
                if let (Some(sid), Some(status_str)) = (maybe_id, maybe_status) {
                    let now = std::time::Instant::now();
                    if let Some(sa) = app.subagents.iter_mut().find(|s| s.id == sid && !s.done) {
                        sa.done = true;
                        sa.done_at = Some(now);
                        if let Some(dur) = maybe_duration {
                            sa.duration_secs = Some(dur);
                        }
                        sa.status = match status_str {
                            "completed" => "\u{2714} done".to_string(),
                            "cancelled" => "\u{26a0} cancelled".to_string(),
                            "timed_out" => "\u{26a0} timed out".to_string(),
                            s if s.starts_with("fail") => {
                                let reason = data["error"].as_str().unwrap_or("error");
                                let preview: String = reason.chars().take(30).collect();
                                format!("\u{2718} {}", preview)
                            }
                            _ => format!("\u{2714} {status_str}"),
                        };
                    }
                }
            }
        }
    }
    app.invalidate();

    // Wake decision.
    let auto_turn_enabled = true; // C2+ will wire config; always on for C1
    let action = wake_action_with_cap(
        &drained,
        &app.api_messages,
        busy,
        auto_turn_enabled,
        app.consecutive_auto_turns,
        app.auto_turn_cap,
    );

    match action {
        WakeAction::RunTurn => {
            // Cap check: if we ARE at cap this would have returned Forward.
            // Increment before spawning so cap is visible for the next wake.
            // Optional guard: skip if a stream is already active (shouldn't
            // happen if wake_action saw busy=false, but defend against it).
            if stream.is_some() {
                tracing::warn!("handle_event_arm: RunTurn with active stream — skipping");
            } else {
                app.consecutive_auto_turns += 1;
                let ct = CancellationToken::new();
                let (s_tx, s_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
                app.streaming = true;
                app.turn_baseline = app.api_messages.len();
                app.spinner_frame = 0;
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
                app.push_msg(ChatMessage::Thinking(THINKING_PLACEHOLDER.to_string()));
                *cancel_token = Some(ct);
                *steer_tx = Some(s_tx);
            }
        }
        WakeAction::Forward => {
            // Check if we hit the cap (some Injected events but cap blocked RunTurn).
            let hit_cap = drained
                .iter()
                .any(|d| d.disposition == EventDisposition::Injected)
                && !busy
                && auto_turn_enabled
                && auto_turn_cap_reached(app.consecutive_auto_turns, app.auto_turn_cap);
            if hit_cap {
                app.push_msg(ChatMessage::System(auto_turn_cap_message(app.auto_turn_cap)));
                app.invalidate();
            }
        }
        WakeAction::Nothing => {}
    }
}

/// Stream-event arm body: route one `StreamEvent` through
/// [`handle_stream_event`] and act on the returned [`StreamAction`] —
/// continue (incl. Done/Error stream-state teardown + gamba reclaim),
/// auto-send a queued message, or auto-trigger buffered events.
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_stream_arm(
    maybe_event: Option<StreamEvent>,
    app: &mut App,
    runtime: &Runtime,
    registry: &std::sync::Arc<synaps_cli::skills::registry::CommandRegistry>,
    render_handle: &RenderHandle,
    secret_prompt_handle: &synaps_cli::tools::SecretPromptHandle,
    stream: &mut Option<ActiveStream>,
    cancel_token: &mut Option<CancellationToken>,
    steer_tx: &mut Option<tokio::sync::mpsc::UnboundedSender<String>>,
) {
    if let Some(event) = maybe_event {
        let do_draw = needs_immediate_draw(&event);
        let action = handle_stream_event(event, app, runtime).await;

        match action {
            StreamAction::Continue => {
                // For Done/Error, clear stream state
                if !app.streaming {
                    *stream = None;
                    *cancel_token = None;
                    *steer_tx = None;
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
                // Auto-send the queued message (user-authored — reset auto-turn counter)
                app.consecutive_auto_turns = 0;
                app.push_msg(ChatMessage::User(queued.clone()));
                app.transcript.scroll_to_bottom();
                let api_content = if let Some(ref ctx) = app.abort_context {
                    let combined = format!("{}\n\n{}", ctx, queued);
                    app.abort_context = None;
                    combined
                } else {
                    queued
                };
                app.api_messages.push(std::sync::Arc::new(
                    json!({"role": "user", "content": api_content}),
                ));
                let ct = CancellationToken::new();
                let (s_tx, s_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
                app.status_text = Some("connecting…".to_string());
                app.streaming = true;
                app.turn_baseline = app.api_messages.len();
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
            StreamAction::AutoTriggerEvents => {
                drop(stream.take());
                drop(cancel_token.take());
                drop(steer_tx.take());

                // Use the central claim_auto_turn gate: allows turns while
                // counter < cap, denies once counter == cap (cap 0 = unlimited).
                // Increment happens inside claim on success — no inline +=.
                if claim_auto_turn_with_cap(&mut app.consecutive_auto_turns, app.auto_turn_cap) {
                    let ct = CancellationToken::new();
                    let (s_tx, s_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
                    app.streaming = true;
                    app.turn_baseline = app.api_messages.len();
                    app.spinner_frame = 0;
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
                    app.push_msg(ChatMessage::Thinking(THINKING_PLACEHOLDER.to_string()));
                    *cancel_token = Some(ct);
                    *steer_tx = Some(s_tx);
                } else {
                    // Unreachable when cap == 0 (claim never denies), so the
                    // message only ever appears for a finite cap.
                    app.push_msg(ChatMessage::System(auto_turn_cap_message(app.auto_turn_cap)));
                    app.invalidate();
                }
            }
        }

        if do_draw {
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
        }
    }
}

/// Strip ASCII control characters (except `\n` and `\t`) from notice text
/// before it reaches the render path. Notices can carry raw API error bodies;
/// an embedded ESC (0x1b) would otherwise inject terminal escape sequences.
fn sanitize_notice(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_ascii_control() || *c == '\n' || *c == '\t')
        .collect()
}

// ── Flash expiry constant ──────────────────────────────────────────────────────
/// How long a done entry stays visible before reconcile removes it.
pub(super) const SUBAGENT_DONE_FLASH_SECS: f64 = 5.0;

/// Pure reconcile: align the HUD Vec<SubagentState> with the registry snapshot.
///
/// Rules (applied in order per entry):
/// 1. Running && !cancel_requested in rows but missing from HUD → insert.
/// 2. Terminal (non-Running) in rows, HUD entry not yet done → mark done
///    (stamp glyph / duration / done_at from registry elapsed).
/// 3. done && done_at elapsed > SUBAGENT_DONE_FLASH_SECS → remove (flash expired).
/// 4. Not in rows at all → remove ONLY if already done (spares in-flight oneshots
///    that emitted SubagentStart but never registered — e.g. oneshot.rs).
/// 5. cancel_requested && still Running in HUD → update status to "⚠ cancelling…".
pub(super) fn reconcile_subagents(
    hud: &mut Vec<SubagentState>,
    rows: &[synaps_cli::tools::SubagentDisplayRow],
    now: std::time::Instant,
) {
    use synaps_cli::runtime::subagent::SubagentStatus;

    // Build a quick lookup: subagent_id → row
    let row_map: std::collections::HashMap<u64, &synaps_cli::tools::SubagentDisplayRow> =
        rows.iter().map(|r| (r.subagent_id, r)).collect();

    // Pass 1: update existing HUD entries
    hud.retain_mut(|sa| {
        match row_map.get(&sa.id) {
            Some(row) => {
                // Rule 3: done entry whose flash has expired → remove
                if sa.done {
                    if let Some(done_at) = sa.done_at {
                        if now.duration_since(done_at).as_secs_f64() > SUBAGENT_DONE_FLASH_SECS {
                            return false; // remove
                        }
                    }
                    return true; // still flashing
                }

                // Rule 2: terminal in registry, not yet marked done in HUD → mark done
                if !matches!(row.status, SubagentStatus::Running) {
                    sa.done = true;
                    sa.done_at = Some(now);
                    // Duration from registry elapsed (best available without tx_events)
                    if sa.duration_secs.is_none() {
                        sa.duration_secs = Some(row.elapsed_secs);
                    }
                    // Apply glyph if status wasn't already set by a stream event
                    if !sa.status.starts_with('\u{2714}')
                        && !sa.status.starts_with('\u{2718}')
                        && !sa.status.starts_with('\u{26a0}')
                    {
                        sa.status = match &row.status {
                            SubagentStatus::Completed => "\u{2714} done".to_string(),
                            SubagentStatus::Cancelled => "\u{26a0} cancelled".to_string(),
                            SubagentStatus::TimedOut => "\u{26a0} timed out".to_string(),
                            SubagentStatus::Failed(r) => {
                                let preview: String = r.chars().take(30).collect();
                                format!("\u{2718} {}", preview)
                            }
                            SubagentStatus::Running => sa.status.clone(), // unreachable
                        };
                    }
                    return true;
                }

                // Rule 5: cancel_requested && Running in HUD → show cancelling status
                if row.cancel_requested && !sa.done {
                    sa.status = "\u{26a0} cancelling\u{2026}".to_string();
                }

                true // keep running entry
            }
            None => {
                // Rule 4: not in registry → remove only if done (flash already set),
                // KEEP if still running (in-flight oneshot or race with registration)
                if sa.done {
                    // Apply flash expiry
                    if let Some(done_at) = sa.done_at {
                        if now.duration_since(done_at).as_secs_f64() > SUBAGENT_DONE_FLASH_SECS {
                            return false;
                        }
                    }
                }
                true // keep in-flight oneshots and still-flashing done entries
            }
        }
    });

    // Pass 2: insert missing Running entries from registry that aren't in HUD
    let hud_ids: std::collections::HashSet<u64> = hud.iter().map(|s| s.id).collect();
    for row in rows {
        if hud_ids.contains(&row.subagent_id) {
            continue;
        }
        // Rule 1: only insert Running, non-cancelled entries
        if matches!(row.status, SubagentStatus::Running) && !row.cancel_requested {
            let elapsed_dur = std::time::Duration::from_secs_f64(row.elapsed_secs.max(0.0));
            let start_time = now.checked_sub(elapsed_dur).unwrap_or(now);
            hud.push(SubagentState {
                id: row.subagent_id,
                name: row.agent_name.clone(),
                status: "running".to_string(),
                start_time,
                done: false,
                duration_secs: None,
                done_at: None,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::sanitize_notice;

    #[test]
    fn test_sanitize_notice_strips_ansi_escape_payload() {
        let payload = "API retry \x1b[2J\x1b]0;pwned\x07 in 2s\nline two\tend";
        let clean = sanitize_notice(payload);
        assert_eq!(clean, "API retry [2J]0;pwned in 2s\nline two\tend");
        assert!(!clean.contains('\x1b'));
        assert!(!clean.contains('\x07'));
    }

    #[test]
    fn test_sanitize_notice_passes_normal_text() {
        let s = "retrying (attempt 2/5) — overloaded";
        assert_eq!(sanitize_notice(s), s);
    }
}

#[cfg(test)]
mod reconcile_tests {
    use super::super::app::SubagentState;
    use super::{reconcile_subagents, SUBAGENT_DONE_FLASH_SECS};
    use std::time::{Duration, Instant};
    use synaps_cli::runtime::subagent::SubagentStatus;
    use synaps_cli::tools::SubagentDisplayRow;

    fn make_row(id: u64, status: SubagentStatus, cancel_requested: bool) -> SubagentDisplayRow {
        SubagentDisplayRow {
            subagent_id: id,
            agent_name: format!("agent-{id}"),
            status,
            cancel_requested,
            elapsed_secs: 1.5,
            finished_elapsed: None,
        }
    }

    fn make_hud_entry(id: u64, done: bool) -> SubagentState {
        SubagentState {
            id,
            name: format!("agent-{id}"),
            status: if done {
                "\u{2714} done".to_string()
            } else {
                "running".to_string()
            },
            start_time: Instant::now(),
            done,
            duration_secs: if done { Some(1.5) } else { None },
            done_at: if done { Some(Instant::now()) } else { None },
        }
    }

    // R1: idle-finish — Running in HUD, terminal in registry → marked done
    #[test]
    fn idle_finish_marks_done() {
        let now = Instant::now();
        let mut hud = vec![make_hud_entry(1, false)];
        let rows = vec![make_row(1, SubagentStatus::Completed, false)];
        reconcile_subagents(&mut hud, &rows, now);
        assert_eq!(hud.len(), 1);
        assert!(hud[0].done, "must be marked done");
        assert!(hud[0].done_at.is_some(), "done_at must be stamped");
        assert!(hud[0].status.contains('\u{2714}'), "must have ✔ glyph");
    }

    // R2: oneshot-in-flight — in HUD but NOT in registry (oneshot never registers)
    // The entry must survive (not removed) because it's still running
    #[test]
    fn oneshot_in_flight_survives() {
        let now = Instant::now();
        let mut hud = vec![make_hud_entry(99, false)]; // not in registry
        let rows: Vec<SubagentDisplayRow> = vec![]; // empty registry
        reconcile_subagents(&mut hud, &rows, now);
        assert_eq!(
            hud.len(),
            1,
            "in-flight oneshot must survive even if not in registry"
        );
    }

    // R3: cancelling — cancel_requested && Running → status updated
    #[test]
    fn cancelling_filtered() {
        let now = Instant::now();
        let mut hud = vec![make_hud_entry(1, false)];
        let rows = vec![make_row(1, SubagentStatus::Running, true)]; // cancel_requested
        reconcile_subagents(&mut hud, &rows, now);
        assert_eq!(hud.len(), 1);
        assert!(
            hud[0].status.contains("cancelling"),
            "status must say cancelling: {}",
            hud[0].status
        );
    }

    // R4: flash expiry — done entry with done_at > 5s → removed
    #[test]
    fn flash_expiry_removes_done() {
        let old_done_at = Instant::now() - Duration::from_secs_f64(SUBAGENT_DONE_FLASH_SECS + 1.0);
        let mut entry = make_hud_entry(1, true);
        entry.done_at = Some(old_done_at);

        let mut hud = vec![entry];
        let rows = vec![make_row(1, SubagentStatus::Completed, false)];
        let now = Instant::now();
        reconcile_subagents(&mut hud, &rows, now);
        assert!(hud.is_empty(), "expired done entry must be removed");
    }

    // R5: insert-missing — Running in registry but not in HUD → inserted
    #[test]
    fn insert_missing() {
        let now = Instant::now();
        let mut hud: Vec<SubagentState> = vec![];
        let rows = vec![make_row(5, SubagentStatus::Running, false)];
        reconcile_subagents(&mut hud, &rows, now);
        assert_eq!(hud.len(), 1, "missing running entry must be inserted");
        assert_eq!(hud[0].id, 5);
        assert!(!hud[0].done);
    }

    // R6: multiple subagents — mix of states
    #[test]
    fn multiple_subagents_mixed_states() {
        let now = Instant::now();

        // HUD: sa_1 running, sa_2 running, sa_3 done (fresh)
        let old_done_at = Instant::now() - Duration::from_secs_f64(SUBAGENT_DONE_FLASH_SECS + 1.0);
        let mut sa3 = make_hud_entry(3, true);
        sa3.done_at = Some(old_done_at); // expired

        let mut hud = vec![
            make_hud_entry(1, false), // running → will be completed
            make_hud_entry(2, false), // running → cancel_requested
            sa3,                      // done + expired → remove
        ];

        // Registry: sa_1 completed, sa_2 cancel_requested, sa_3 completed, sa_4 new running
        let rows = vec![
            make_row(1, SubagentStatus::Completed, false),
            make_row(2, SubagentStatus::Running, true),
            make_row(3, SubagentStatus::Completed, false),
            make_row(4, SubagentStatus::Running, false), // new — must be inserted
        ];

        reconcile_subagents(&mut hud, &rows, now);

        // sa_1 must be marked done
        let sa1 = hud.iter().find(|s| s.id == 1).expect("sa_1 must exist");
        assert!(sa1.done);

        // sa_2 must have cancelling status
        let sa2 = hud.iter().find(|s| s.id == 2).expect("sa_2 must exist");
        assert!(
            sa2.status.contains("cancelling"),
            "sa2 status: {}",
            sa2.status
        );

        // sa_3 expired → removed
        assert!(
            hud.iter().find(|s| s.id == 3).is_none(),
            "sa_3 expired flash must be removed"
        );

        // sa_4 inserted
        assert!(
            hud.iter().find(|s| s.id == 4).is_some(),
            "sa_4 must be inserted"
        );
    }

    // R7: done entry not in registry stays until flash expires (oneshot completed)
    #[test]
    fn done_not_in_registry_stays_during_flash() {
        let now = Instant::now();
        let mut entry = make_hud_entry(99, true);
        entry.done_at = Some(now - Duration::from_millis(100)); // very fresh done

        let mut hud = vec![entry];
        let rows: Vec<SubagentDisplayRow> = vec![];
        reconcile_subagents(&mut hud, &rows, now);
        assert_eq!(
            hud.len(),
            1,
            "recently-done entry not in registry must stay for flash"
        );
    }
}
