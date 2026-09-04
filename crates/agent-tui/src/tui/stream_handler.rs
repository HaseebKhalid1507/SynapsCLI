//! Session-event handling — the presentation half of the turn machine
//! (PLAN-phase3 §2.4). `Stream(ev)` keeps `handle_stream_event` as it was;
//! every other envelope maps to the `ChatMessage`/state mutation the inline
//! turn machine used to perform. The actor never renders.

use synaps_cli::{AgentEvent, LlmEvent, SessionEvent, StreamEvent};

use agent_engine::session::{Envelope, RuntimeView, SessionEventWire, TurnTrigger};

use super::app::{App, ChatMessage, SubagentState, THINKING_PLACEHOLDER};
use super::draw::build_render_model;
use super::render_thread::RenderHandle;
use super::session_link::{PromptBridge, SessionLink};
use super::view_model::ViewInputs;

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

/// Process a StreamEvent, update app state. Presentation only: history,
/// saves, queued/pending flushes and auto-turns are the actor's (mirrored
/// back by `Conversation` / `TurnStarted`).
pub(super) fn handle_stream_event(event: StreamEvent, app: &mut App, view: &RuntimeView) {
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
        }
        StreamEvent::Llm(LlmEvent::ToolResultDelta { delta, tool_id }) => {
            app.on_tool_result_delta(tool_id, delta);
        }
        StreamEvent::Llm(LlmEvent::ToolResult { result, tool_id }) => {
            app.on_tool_result(tool_id, result);
        }
        StreamEvent::Session(SessionEvent::MessageHistory(history)) => {
            app.api_messages = history;
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
            let model_for_pricing = usage_model.as_deref().unwrap_or(&view.model);
            app.add_usage(
                input_tokens,
                output_tokens,
                cache_read_input_tokens,
                cache_creation_input_tokens,
                cache_creation_5m,
                cache_creation_1h,
                model_for_pricing,
                Some(view.context_window),
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
            // Reconcile HUD against the registry rows (last `SubagentRows`)
            // instead of clearing — retains running entries whose tx_events
            // is now dead (stream dropped) but threads live on.
            reconcile_subagents(
                &mut app.subagents,
                &app.subagent_rows,
                std::time::Instant::now(),
            );
            // NOTE: cleanup_finished removed — engine now reaps at turn completion
            // inside the tokio::spawn wrapper (runtime/mod.rs) before Done is sent.
            // Pending-event flush / queued auto-send / auto-trigger: the
            // actor's; it announces the next turn with `TurnStarted`.
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
            // Reconcile HUD against registry rows on error path too (same as Done).
            reconcile_subagents(
                &mut app.subagents,
                &app.subagent_rows,
                std::time::Instant::now(),
            );
            // History repair is the actor's (mirrored by `Conversation`).
        }
    }
}

// ── Session-event arm (PLAN-phase3 §2.4) ─────────────────────────────────

/// Outcome of one envelope for the `run()` loop.
pub(super) enum ArmFlow {
    Continue,
    /// `Ended` arrived: the session is gone; leave the loop.
    Ended,
}

fn publish_frame(
    app: &mut App,
    view: &RuntimeView,
    registry: &std::sync::Arc<synaps_cli::skills::registry::CommandRegistry>,
    render_handle: &RenderHandle,
) {
    let term_size = crossterm::terminal::size()
        .map(|(w, h)| ratatui::layout::Size {
            width: w,
            height: h,
        })
        .unwrap_or_default();
    let built = build_render_model(&mut ViewInputs::from_app(app), view, registry, term_size);
    if let Some((model, patch)) = built {
        patch.apply(app);
        render_handle.publish(model);
    }
}

/// Event card + HUD mark for a drained engine event (was the presentation
/// half of `handle_event_queue_arm`, verbatim).
pub(super) fn on_external(app: &mut App, event: &synaps_cli::events::types::Event) {
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
    app.invalidate();
}

/// The auto-turn cap line (was stream_handler.rs:368-378 / :516-520).
pub(super) fn on_auto_turn_cap(app: &mut App, cap: u32) {
    app.push_msg(ChatMessage::System(format!(
        "auto-turn cap reached ({} consecutive) — waiting for your input",
        cap
    )));
    app.invalidate();
}

/// One envelope from the session → the exact `ChatMessage`s / state
/// mutations the inline turn machine performed (§2.4 table).
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_session_event_arm(
    env: Envelope,
    app: &mut App,
    link: &mut SessionLink,
    registry: &std::sync::Arc<synaps_cli::skills::registry::CommandRegistry>,
    render_handle: &RenderHandle,
    prompt_bridge: &mut PromptBridge,
    ext_mgr: Option<
        &std::sync::Arc<tokio::sync::RwLock<synaps_cli::extensions::manager::ExtensionManager>>,
    >,
) -> ArmFlow {
    let me = link.client_id();
    match env.event {
        SessionEventWire::Stream(ev) => {
            let do_draw = needs_immediate_draw(&ev);
            let view = std::sync::Arc::clone(link.view());
            handle_stream_event(ev, app, &view);
            // For Done/Error: reclaim gamba if running — resume render
            // thread after reclaim restores the terminal.
            if !app.streaming {
                if let Some(msg) = app.reclaim_gamba() {
                    render_handle.resume();
                    app.push_msg(ChatMessage::System(msg));
                    app.invalidate();
                }
            }
            if do_draw {
                publish_frame(app, &view, registry, render_handle);
            }
        }
        SessionEventWire::TurnStarted {
            turn_baseline,
            trigger,
            user_text,
        } => match trigger {
            TurnTrigger::User | TurnTrigger::PluginCommand => {
                // Pre-send presentation (User card, "connecting…",
                // streaming=true, spinner, frame) already happened in the
                // dispatch arm; this is the tail after the stream opened.
                app.last_submitted = None;
                app.streaming = true;
                app.turn_baseline = turn_baseline;
                app.status_text = None;
                app.push_msg(ChatMessage::Thinking(THINKING_PLACEHOLDER.to_string()));
            }
            TurnTrigger::QueuedAuto => {
                if let Some(msg) = app.reclaim_gamba() {
                    render_handle.resume();
                    app.push_msg(ChatMessage::System(msg));
                    app.invalidate();
                }
                // Auto-send of the queued message (user-authored).
                app.consecutive_auto_turns = 0;
                if let Some(q) = user_text {
                    app.push_msg(ChatMessage::User(q));
                }
                app.transcript.scroll_to_bottom();
                app.status_text = Some("connecting…".to_string());
                app.streaming = true;
                app.turn_baseline = turn_baseline;
                app.spinner_frame = 0;
                let view = std::sync::Arc::clone(link.view());
                publish_frame(app, &view, registry, render_handle);
                app.status_text = None;
                app.push_msg(ChatMessage::Thinking(THINKING_PLACEHOLDER.to_string()));
            }
            TurnTrigger::EventAuto | TurnTrigger::Compaction => {
                app.streaming = true;
                app.turn_baseline = turn_baseline;
                app.spinner_frame = 0;
                app.push_msg(ChatMessage::Thinking(THINKING_PLACEHOLDER.to_string()));
            }
        },
        SessionEventWire::Conversation(snap) => {
            app.apply_conversation(&snap);
            if let Some(applied) = app.compaction_applied.take() {
                finish_compaction(app, applied);
            }
            if let Some(pending) = app.resume_pending.take() {
                let msgs = app.api_messages.clone();
                super::helpers::rebuild_display_messages(&msgs, app);
                if let Some(notice) = pending.clamp_notice {
                    app.push_msg(ChatMessage::System(notice));
                }
                let via = pending
                    .via
                    .map(|v| format!(" (via {v})"))
                    .unwrap_or_default();
                app.push_msg(ChatMessage::System(format!(
                    "switched from {} to {}{}",
                    pending.old_id, pending.new_id, via
                )));
            }
        }
        SessionEventWire::Prompt(req) => prompt_bridge.on_prompt(req),
        SessionEventWire::PromptResolved { prompt_id } => {
            if prompt_bridge.on_resolved(prompt_id) {
                app.secret_prompts.dismiss();
                app.invalidate();
            }
        }
        SessionEventWire::External(ev) => on_external(app, &ev),
        SessionEventWire::AutoTurnCapReached { cap } => on_auto_turn_cap(app, cap),
        SessionEventWire::Idle => {}
        SessionEventWire::Steered { text, delivered } => {
            if delivered {
                app.push_msg(ChatMessage::System(format!("→ steering: {}", text)));
            } else {
                app.push_msg(ChatMessage::System(format!("queued: {}", text)));
            }
            app.queued_message = Some(text);
        }
        SessionEventWire::Dequeued { text } => {
            app.push_msg(ChatMessage::System(format!("dequeued: {}", text)));
        }
        SessionEventWire::SystemNotice(text) => {
            app.push_msg(ChatMessage::System(sanitize_notice(&text)));
        }
        SessionEventWire::LoaderProgress(ev) => {
            super::loop_arms::handle_extension_loader_event(app, link.view(), ev, ext_mgr).await;
            app.request_redraw();
        }
        SessionEventWire::ExtensionNotification {
            extension_id,
            method,
            params,
        } => {
            // Socket path: the in-process watchers are not running, so the
            // widget frames arrive here (same parse the watchers use). In
            // process the watchers own `widget_rx`; the actor sees no frames.
            if ext_mgr.is_none() && synaps_cli::extensions::widgets::is_widget_method(&method) {
                if let Ok(event) =
                    synaps_cli::extensions::widgets::parse_widget_event(&method, &params)
                {
                    let ev = synaps_cli::extensions::widgets::ExtensionWidgetEvent {
                        extension_id,
                        event,
                    };
                    if super::loop_arms::handle_widget_event(app, ev) {
                        app.request_redraw();
                    }
                }
            }
        }
        SessionEventWire::SettingChanged(_) | SessionEventWire::QueryResult { .. } => {
            // Unsolicited (another client's Set / a host query): the view
            // was refreshed by `SessionLink::note`; nothing to render.
        }
        SessionEventWire::Attached { .. }
        | SessionEventWire::ClientJoined { .. }
        | SessionEventWire::ClientLeft { .. } => {}
        SessionEventWire::Ended { .. } => return ArmFlow::Ended,
        SessionEventWire::Aborted { context_saved } => {
            app.streaming = false;
            app.subagents.clear();
            // The actor decided (`TurnLog::abort_context`); the mirror's
            // `abort_context` only lands with the `Conversation` that follows.
            let abort_msg = if context_saved {
                "aborted — context saved for next message"
            } else {
                "aborted"
            };
            app.drop_empty_thinking();
            app.push_msg(ChatMessage::Error(abort_msg.to_string()));
        }
        SessionEventWire::Cleared { .. } => {
            app.transcript.clear();
            app.invalidate();
            app.api_messages.clear();
            app.total_input_tokens = 0;
            app.total_output_tokens = 0;
            app.total_cache_read_tokens = 0;
            app.total_cache_creation_tokens = 0;
            app.session_cost = 0.0;
            app.input_tokens = 0;
            app.output_tokens = 0;
            app.push_msg(ChatMessage::System("new session started".to_string()));
        }
        SessionEventWire::CompactionStarted { disclosure, .. } => {
            app.push_msg(ChatMessage::System(disclosure));
            app.push_msg(ChatMessage::System(
                "compacting conversation...".to_string(),
            ));
            app.status_text = Some("compacting…".to_string());
            app.spinner_frame = 0;
            app.compacting = true;
        }
        SessionEventWire::CompactionApplied {
            previous_session_id,
            session_id,
            chains_advanced,
            queued_restored,
            msg_count,
        } => {
            // The successor's messages/header arrive in the `Conversation`
            // that follows; the lines are pushed once it has been applied.
            app.compaction_applied = Some(super::app::CompactionApplied {
                previous_session_id,
                session_id,
                chains_advanced,
                queued_restored,
                msg_count,
            });
        }
        SessionEventWire::CompactionFailed { message, panicked } => {
            if panicked {
                app.push_msg(ChatMessage::Error(format!(
                    "compaction task panicked: {}",
                    message
                )));
            } else {
                app.push_msg(ChatMessage::Error(format!("compaction failed: {}", message)));
            }
            app.status_text = None;
            app.compacting = false;
            app.invalidate();
        }
        SessionEventWire::CompactionCancelled => {
            app.push_msg(ChatMessage::System("compaction cancelled".to_string()));
            app.status_text = None;
            app.compacting = false;
            app.invalidate();
        }
        SessionEventWire::SubagentRows(rows) => app.subagent_rows = rows,
        SessionEventWire::Resumed { .. } => {}
        SessionEventWire::InputOwnerChanged { from, to, .. } => {
            if from == Some(me) && to != Some(me) {
                let who = to
                    .map(|c| format!("client #{}", c.0))
                    .unwrap_or_else(|| "nobody".to_string());
                app.toasts.upsert(
                    super::toast::Toast::new("input-owner", format!("input taken over by {who}"))
                        .titled("Session"),
                );
            } else if to == Some(me) && from != Some(me) {
                app.toasts.upsert(
                    super::toast::Toast::new("input-owner", "you own input").titled("Session"),
                );
            }
            app.request_redraw();
        }
        SessionEventWire::Refused {
            client,
            command,
            reason,
        } => {
            if client == me {
                app.push_msg(ChatMessage::Error(format!("{command} refused: {reason}")));
                // The pre-send presentation assumed a turn: undo it.
                if app.streaming && app.turn_baseline == app.api_messages.len() {
                    app.streaming = false;
                    app.status_text = None;
                    app.drop_empty_thinking();
                }
                // §6 #9: a Submit refused after the editor was cleared —
                // give the text back.
                if let Some(text) = app.last_submitted.take() {
                    app.set_input_text(&text);
                }
            }
        }
        SessionEventWire::AttachRefused { message } => {
            app.push_msg(ChatMessage::Error(format!("attach refused: {message}")));
        }
        SessionEventWire::Lifecycle(state) => {
            app.toasts.upsert(
                super::toast::Toast::new("lifecycle", format!("session {state:?}"))
                    .titled("Session"),
            );
            app.request_redraw();
        }
        SessionEventWire::Reloading { generation, .. } => {
            app.toasts.upsert(
                super::toast::Toast::new(
                    "reload",
                    format!("daemon reloading (generation {generation})…"),
                )
                .titled("Daemon")
                .ttl(None),
            );
            app.request_redraw();
        }
    }
    ArmFlow::Continue
}

/// `CompactionApplied` + the `Conversation` that followed: the lines the
/// compaction poll used to push (loop_arms.rs:783-812).
fn finish_compaction(app: &mut App, applied: super::app::CompactionApplied) {
    let old_id = applied.previous_session_id;
    let msgs = app.api_messages.clone();
    super::helpers::rebuild_display_messages(&msgs, app);
    for name in &applied.chains_advanced {
        app.push_msg(ChatMessage::System(format!(
            "chain '{}' advanced: {} → {}",
            name, old_id, app.session.id
        )));
    }
    if let Some(q) = applied.queued_restored {
        app.push_msg(ChatMessage::System(format!(
            "queued message restored: {}",
            q
        )));
    }
    app.push_msg(ChatMessage::System(format!(
        "✓ compacted {} messages → new session {} (from {})",
        applied.msg_count, app.session.id, old_id
    )));
    app.status_text = None;
    app.compacting = false;
    app.invalidate();
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
