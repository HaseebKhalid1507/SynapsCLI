//! Session-event handling — the presentation half of the turn machine
//! (PLAN-phase3 §2.4). `Stream(ev)` keeps `handle_stream_event` as it was;
//! every other envelope maps to the `ChatMessage`/state mutation the inline
//! turn machine used to perform. The actor never renders.

use agent_engine::{TurnError, TurnOutcome};
use synaps_cli::{AgentEvent, LlmEvent, SessionEvent, StreamEvent};

use super::header::Activity;

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

/// What [`apply_transcript_event`] did to the transcript it was given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Applied {
    /// Not a transcript event (usage, subagent HUD, checkpoints).
    Ignored,
    /// The transcript was (or may have been) mutated — repaint.
    Changed,
    /// A transcript event that changed nothing (an orphaned reset, a drained
    /// engine-event payload riding the steering channel).
    Unchanged,
    /// `ResponseReset` rolled the in-flight preview back (structural edit).
    RolledBack,
}

/// Apply the transcript half of a stream event to ONE thread's transcript.
///
/// Pure presentation over `store` + its `ResponseStart` snapshot: no `App`
/// state (streaming flags, usage, subagent HUD, queued input, scroll
/// policy). The main thread runs it on `App::transcript` from
/// [`handle_stream_event`]; a watched subagent's thread runs the very same
/// function on its own store, so a worker renders exactly like the main
/// conversation without being able to touch it.
pub(super) fn apply_transcript_event(
    store: &mut super::transcript::TranscriptStore,
    preview: &mut Option<(usize, Option<ChatMessage>)>,
    event: &StreamEvent,
) -> Applied {
    match event {
        StreamEvent::Llm(LlmEvent::Thinking(text)) => {
            store.append_or_update_thinking(text);
        }
        StreamEvent::Llm(LlmEvent::Text(text)) => {
            store.append_or_update_text(text);
        }
        StreamEvent::Llm(LlmEvent::ToolUseStart { tool_name, tool_id }) => {
            store.on_tool_use_start(tool_id.clone(), tool_name.clone());
        }
        StreamEvent::Llm(LlmEvent::ToolUseDelta { tool_id, delta }) => {
            store.on_tool_use_delta(tool_id, delta);
        }
        StreamEvent::Llm(LlmEvent::ToolUse {
            tool_name,
            tool_id,
            input,
        }) => {
            let input_str = serde_json::to_string(input).unwrap_or_default();
            store.on_tool_use_finalized(tool_id.clone(), tool_name.clone(), input_str);
        }
        StreamEvent::Llm(LlmEvent::ToolResultDelta { delta, tool_id }) => {
            store.on_tool_result_delta(tool_id.clone(), delta.clone());
        }
        StreamEvent::Llm(LlmEvent::ToolResult { result, tool_id }) => {
            store.on_tool_result(tool_id.clone(), result.clone());
        }
        StreamEvent::Agent(AgentEvent::SteeringDelivered { message }) => {
            // Queue-drained events (subagent completion wakes, watcher
            // alerts) ride the same steering channel as genuine user
            // steering, but they were already presented as Event cards at
            // drain time (handle_event_queue_arm). Rendering them here as
            // ChatMessage::User made subagent continuation text land in the
            // transcript as a message the user typed and submitted.
            if super::helpers::is_event_payload(message) {
                return Applied::Unchanged;
            }
            store.push_msg(ChatMessage::User(message.clone()));
        }
        StreamEvent::Session(SessionEvent::Notice(text)) => {
            // Display-only retry/status notice — system line, not transcript.
            // Notice text can echo API error bodies; strip control chars
            // (esp. ESC) so a hostile payload can't inject terminal escapes.
            store.push_msg(ChatMessage::System(sanitize_notice(text)));
        }
        StreamEvent::Session(SessionEvent::Done) => {
            store.end_response();
            store.drop_empty_thinking();
        }
        StreamEvent::Session(SessionEvent::Error(err)) => {
            store.end_response();
            store.drop_empty_thinking();
            // Typed spec §5.2 outcome from the engine — surface the message
            // plus the terminal category + correlation ID, never re-derived.
            store.push_msg(ChatMessage::Error(format!(
                "{} [{}]",
                sanitize_notice(&err.message),
                err.category_label()
            )));
        }
        // ResponseStart: snapshot the transcript position for rollback on reset.
        StreamEvent::Llm(LlmEvent::ResponseStart) => {
            store.drop_empty_thinking();
            *preview = Some((
                store.messages().len(),
                store.messages().last().map(|m| m.msg.clone()),
            ));
            store.begin_response();
        }
        // ResponseReset: roll the in-flight response preview back to the
        // ResponseStart snapshot. Orphaned resets (no prior start) are
        // silently ignored (G Q2). The transcript's own start index is
        // preferred: it stays exact when scrollback drains the front.
        StreamEvent::Llm(LlmEvent::ResponseReset) => {
            let Some((start, last)) = preview.take() else {
                return Applied::Unchanged;
            };
            let start = store.response_start().unwrap_or(start);
            store.reset_response_preview(start, last);
            return Applied::RolledBack;
        }
        StreamEvent::Session(SessionEvent::Usage { .. })
        | StreamEvent::Session(SessionEvent::MessageHistory(_))
        | StreamEvent::Session(SessionEvent::ContextHeadCheckpoint { .. })
        | StreamEvent::Agent(AgentEvent::SubagentStart { .. })
        | StreamEvent::Agent(AgentEvent::SubagentUpdate { .. })
        | StreamEvent::Agent(AgentEvent::SubagentDone { .. }) => return Applied::Ignored,
    }
    Applied::Changed
}

/// Ghost header state for the MAIN thread: what the turn is doing now and
/// how it ended. App-level, so it stays out of [`apply_transcript_event`]
/// (a watched worker's thread never drives the main header).
fn track_activity(app: &mut App, event: &StreamEvent) {
    match event {
        StreamEvent::Llm(LlmEvent::Thinking(_)) => app.activity = Activity::Thinking,
        StreamEvent::Llm(LlmEvent::Text(_))
        | StreamEvent::Llm(LlmEvent::ToolUseStart { .. })
        | StreamEvent::Llm(LlmEvent::ToolUseDelta { .. }) => app.activity = Activity::Writing,
        StreamEvent::Llm(LlmEvent::ToolUse { .. }) => {
            app.tools_in_flight += 1;
            app.activity = Activity::Tool;
        }
        StreamEvent::Llm(LlmEvent::ToolResult { .. }) => {
            app.tools_in_flight = app.tools_in_flight.saturating_sub(1);
            if app.tools_in_flight == 0 {
                // All results back: the next request is on its way.
                app.activity = Activity::Waiting;
            }
        }
        // Retry notices all lead with ⏳ (runtime/api.rs).
        StreamEvent::Session(SessionEvent::Notice(text)) if text.starts_with('\u{23f3}') => {
            app.activity = Activity::Retry;
        }
        StreamEvent::Llm(LlmEvent::ResponseStart) if app.tools_in_flight == 0 => {
            app.activity = Activity::Waiting;
        }
        StreamEvent::Session(SessionEvent::Done) => app.header_turn_ended(None),
        StreamEvent::Session(SessionEvent::Error(err)) => {
            app.header_turn_ended(failure_label(err));
        }
        _ => {}
    }
}

/// Process a StreamEvent, update app state. Presentation only: history,
/// saves, queued/pending flushes and auto-turns are the actor's (mirrored
/// back by `Conversation` / `TurnStarted`).
///
/// The transcript half is [`apply_transcript_event`] on the MAIN thread's
/// store; everything below it is App-level state.
pub(super) fn handle_stream_event(event: StreamEvent, app: &mut App, view: &RuntimeView) {
    track_activity(app, &event);
    match apply_transcript_event(&mut app.transcript, &mut app.response_preview, &event) {
        Applied::Changed => app.request_redraw(),
        Applied::RolledBack => app.invalidate(),
        Applied::Ignored | Applied::Unchanged => {}
    }
    match event {
        // Digest clients never receive it; in-process the `Conversation`
        // that follows carries `messages_len` (phase 4 §2.2).
        StreamEvent::Session(SessionEvent::MessageHistory(_)) => {}
        StreamEvent::Agent(AgentEvent::SubagentStart {
            subagent_id,
            agent_name,
            task_preview,
        }) => {
            let row = SubagentState {
                id: subagent_id,
                name: agent_name,
                status: format!("starting: {}", task_preview),
                start_time: app.clock.now(),
                done: false,
                duration_secs: None,
                done_at: None,
                tools: 0,
                result: None,
            };
            // One row per id: a daemon older than the reload-carried handle
            // counter restarts ids at 1 after `daemon reload`, and a second
            // row under the same id would mirror the first one's progress
            // and outlive it as a ghost.
            match app.subagents.iter_mut().find(|s| s.id == subagent_id) {
                Some(existing) => *existing = row,
                None => app.subagents.push(row),
            }
            // Tray-only state: repaint, never touch the main transcript
            // (its cache, selection, or scroll). See `subagent_hud_redraw`.
            subagent_hud_redraw(app);
        }
        StreamEvent::Agent(AgentEvent::SubagentUpdate {
            subagent_id,
            status,
            ..
        }) => {
            if let Some(sa) = app.subagents.iter_mut().find(|s| s.id == subagent_id) {
                // "⚙ <tool> (tool #N)" marks a new tool call.
                if let Some((_, n)) = status
                    .strip_prefix("\u{2699} ")
                    .and_then(|rest| rest.rsplit_once(" (tool #"))
                {
                    sa.tools = n.trim_end_matches(')').parse().unwrap_or(sa.tools + 1);
                }
                sa.status = status;
            }
            subagent_hud_redraw(app);
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
                // The tray shows the first real line of the result.
                let first = result_preview
                    .lines()
                    .map(str::trim)
                    .find(|l| !l.is_empty() && !l.starts_with('#'))
                    .unwrap_or("");
                let first = first.strip_prefix("ERROR: ").unwrap_or(first);
                sa.result = Some(first.chars().take(120).collect());
            }
            subagent_hud_redraw(app);
        }
        StreamEvent::Agent(AgentEvent::SteeringDelivered { message }) => {
            let from_user = !super::helpers::is_event_payload(&message);
            if app.queued_message.as_ref() == Some(&message) {
                app.queued_message = None;
            }
            // Only the user's own steering yanks the view to the bottom. A
            // drained engine event (a subagent completion wake, a watcher
            // alert) is background activity: it must not move a view the
            // user scrolled up.
            if from_user {
                app.transcript.scroll_to_bottom();
                app.invalidate();
            } else {
                app.request_redraw();
            }
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
        StreamEvent::Session(SessionEvent::Done) | StreamEvent::Session(SessionEvent::Error(_)) => {
            app.streaming = false;
            app.quit_guard.reset();
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
            // actor's; it announces the next turn with `TurnStarted`. History
            // repair on Error is the actor's too (mirrored by `Conversation`).
        }
        // Transcript-only events: fully handled by `apply_transcript_event`.
        StreamEvent::Llm(_)
        | StreamEvent::Session(SessionEvent::Notice(_))
        | StreamEvent::Session(SessionEvent::ContextHeadCheckpoint { .. }) => {}
    }
}

/// Short label the Ghost header keeps after a failed turn (`None` = the
/// turn didn't fail in a way worth flagging).
fn failure_label(err: &TurnError) -> Option<String> {
    match &err.outcome {
        TurnOutcome::Completed | TurnOutcome::Canceled => None,
        TurnOutcome::ProviderFailed { code, .. } => {
            let code: String = code.chars().filter(|c| !c.is_control()).take(16).collect();
            Some(if code.is_empty() { "provider".into() } else { code })
        }
        TurnOutcome::ToolFailed { .. } => Some("tool failed".into()),
        TurnOutcome::BudgetExceeded { .. } => Some("budget".into()),
        TurnOutcome::InterruptedAfterSideEffect { .. } => Some("interrupted".into()),
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

    if event.content.content_type == "subagent_steer" {
        super::watch::steer_accepted(app);
    }

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
    // `push_msg` already dirtied the new tail slot; the HUD mark is tray-only.
    // A full invalidate here would drop the user's selection on every
    // background completion.
    app.request_redraw();
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
        } => {
            // A new turn: its reply never extends the previous turn's text.
            app.transcript.seal_text_block();
            match trigger {
            TurnTrigger::User | TurnTrigger::PluginCommand => {
                // Pre-send presentation (User card, "connecting…",
                // streaming=true, spinner, frame) already happened in the
                // dispatch arm of the client that SUBMITTED; this is the
                // tail after the stream opened. Every other attached client
                // (a mirror) draws the prompt here — the actor sends it as
                // `user_text` (older daemons send none: nothing drawn, as
                // before).
                let mine = app.last_submitted.take();
                if let Some(text) =
                    user_text.filter(|t| !t.is_empty() && mine.as_deref() != Some(t.as_str()))
                {
                    app.push_msg(ChatMessage::User(text));
                    app.transcript.scroll_to_bottom();
                }
                // The turn was accepted: the attachment drafts it carried are
                // consumed now (a `Refused` would have left them for a retry).
                app.pending_attachments.clear();
                if !app.streaming {
                    app.header_turn_started();
                }
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
                if !app.streaming {
                    app.header_turn_started();
                }
                app.streaming = true;
                app.turn_baseline = turn_baseline;
                app.spinner_frame = 0;
                let view = std::sync::Arc::clone(link.view());
                publish_frame(app, &view, registry, render_handle);
                app.status_text = None;
                app.push_msg(ChatMessage::Thinking(THINKING_PLACEHOLDER.to_string()));
            }
            TurnTrigger::EventAuto | TurnTrigger::Compaction | TurnTrigger::DriverAuto => {
                if !app.streaming {
                    app.header_turn_started();
                }
                app.streaming = true;
                app.turn_baseline = turn_baseline;
                app.spinner_frame = 0;
                app.push_msg(ChatMessage::Thinking(THINKING_PLACEHOLDER.to_string()));
            }
            }
        }
        SessionEventWire::Conversation(snap) => {
            app.apply_conversation(&snap);
            if let Some(applied) = app.compaction_applied.take() {
                super::helpers::rebuild_display_from_conversation(&snap, app, link).await;
                finish_compaction(app, applied);
            }
            if let Some(pending) = app.resume_pending.take() {
                super::helpers::rebuild_display_from_conversation(&snap, app, link).await;
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
        SessionEventWire::Prompt(req) => {
            prompt_bridge.on_prompt(req);
            sync_waiting_confirms(app, prompt_bridge);
        }
        SessionEventWire::PromptResolved { prompt_id } => {
            if prompt_bridge.on_resolved(prompt_id) {
                app.secret_prompts.dismiss();
                app.invalidate();
            }
            sync_waiting_confirms(app, prompt_bridge);
        }
        SessionEventWire::External(ev) => on_external(app, &ev),
        SessionEventWire::AutoTurnCapReached { cap } => on_auto_turn_cap(app, cap),
        SessionEventWire::Idle => {
            // A Cancel that raced a normal finish gets no `Aborted`.
            if app.status_text.as_deref() == Some(ABORTING_STATUS) {
                app.status_text = None;
            }
        }
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
            if !super::watch::absorb_notice(app, &text) {
                app.push_msg(ChatMessage::System(sanitize_notice(&text)));
            }
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
        SessionEventWire::QueryResult { id, value } => {
            // The subagent watch poll is fire-and-forget; anything else is
            // unsolicited (another client's / a host query): nothing to render.
            super::watch::on_query_result(app, id, &value, std::time::Instant::now());
        }
        SessionEventWire::SettingChanged(_) => {
            // Unsolicited (another client's Set): the view was refreshed by
            // `SessionLink::note`; nothing to render.
        }
        SessionEventWire::Attached { .. }
        | SessionEventWire::ClientJoined { .. }
        | SessionEventWire::ClientLeft { .. } => {}
        SessionEventWire::Ended { .. } => return ArmFlow::Ended,
        SessionEventWire::Aborted { context_saved } => {
            app.transcript.end_response();
            app.header_turn_ended(None);
            app.streaming = false;
            app.quit_guard.reset();
            app.subagents.clear();
            if app.status_text.as_deref() == Some(ABORTING_STATUS) {
                app.status_text = None;
            }
            // `context_saved` (wire name kept): the interrupted turn's partial
            // work stays in history; the `Conversation` that follows carries it.
            let abort_msg = if context_saved {
                "aborted — partial work kept"
            } else {
                "aborted"
            };
            app.drop_empty_thinking();
            app.push_msg(ChatMessage::Error(abort_msg.to_string()));
        }
        SessionEventWire::Cleared { .. } => {
            super::watch::reset(app);
            app.transcript.clear();
            app.invalidate();
            app.api_messages_len = 0;
            app.total_input_tokens = 0;
            app.total_output_tokens = 0;
            app.total_cache_read_tokens = 0;
            app.total_cache_creation_tokens = 0;
            app.session_cost = 0.0;
            app.keepwarm_cost = 0.0;
            app.keepwarm_tokens = Default::default();
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
        SessionEventWire::SubagentRows(rows) => {
            // A running worker the tray doesn't show yet: the client
            // reattached to a daemon whose workers kept running, or an abort
            // cleared the tray while `subagent_start` workers lived on. Add
            // it now. The tick only reconciles a non-empty tray, so an empty
            // one would otherwise wait for the agent to start another worker.
            let untracked = has_untracked_worker(&app.subagents, &rows);
            if untracked {
                reconcile_subagents(&mut app.subagents, &rows, std::time::Instant::now());
            }
            // Tray-only state either way: never invalidate the transcript
            // (see `subagent_hud_redraw`).
            if apply_subagent_progress(&mut app.subagents, &rows) || untracked {
                subagent_hud_redraw(app);
            }
            app.subagent_rows = rows;
            super::watch::prune(app, std::time::Instant::now());
        }
        SessionEventWire::Resumed { .. } => {}
        SessionEventWire::InputOwnerChanged { from, to, .. } => {
            prompt_bridge.set_owns_input(to == Some(me));
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
            if client == me && command == "subagent_steer" {
                // Steering a watched worker: nothing about the main thread's
                // turn or input changed, so none of the undo below applies.
                super::watch::steer_refused(app, &reason);
            } else if client == me {
                app.push_msg(ChatMessage::Error(format!("{command} refused: {reason}")));
                // The pre-send presentation assumed a turn: undo it.
                if app.streaming && app.turn_baseline == app.api_messages_len {
                    app.header_turn_ended(None);
                    app.streaming = false;
            app.quit_guard.reset();
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
            // The daemon re-execs: none of its workers survive (they are its
            // threads; the new image reports what it re-adopts). Running rows
            // would otherwise tick on forever as ghosts — reconcile keeps a
            // running row the registry doesn't list (in-flight oneshots).
            if mark_workers_stopped_by_reload(&mut app.subagents, std::time::Instant::now()) {
                app.invalidate();
            }
            app.subagent_rows.clear();
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
        // E-P8: driver wire events → thin client render (react, never compute).
        SessionEventWire::DriverArmed {
            plugin_id,
            run_id,
            models,
            selection,
            deadline_ms,
            notice,
        } => {
            super::driver_client::on_armed(
                app, plugin_id, run_id, models, selection, deadline_ms, notice,
            );
        }
        SessionEventWire::DriverRevoked {
            reason,
            undelivered_steering,
        } => {
            super::driver_client::on_revoked(app, reason, undelivered_steering);
        }
        SessionEventWire::DriverTurnOutcome {
            outcome,
            selection,
            feedback,
        } => {
            super::driver_client::on_turn_outcome(app, outcome, selection, feedback);
        }
        // E-P7: spend ceiling breached — surface it loudly.
        SessionEventWire::CostCapReached { scope, cost, cap } => {
            app.push_msg(ChatMessage::Error(format!(
                "{scope} cost cap reached (${cost:.4} ≥ ${cap:.4}) — turn cancelled, driver revoked"
            )));
            app.request_redraw();
        }
    }
    ArmFlow::Continue
}

/// `CompactionApplied` + the `Conversation` that followed: the lines the
/// compaction poll used to push (loop_arms.rs:783-812). The transcript was
/// rebuilt by the caller (`rebuild_display_from_conversation`).
fn finish_compaction(app: &mut App, applied: super::app::CompactionApplied) {
    let old_id = applied.previous_session_id;
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

/// Repaint after a subagent HUD (tray) change.
///
/// Subagent progress lives in `app.subagents` and is painted by the tray —
/// it is not part of the main transcript. Background workers report on
/// every tool call and every 1 Hz registry refresh, so this must stay a
/// plain redraw: invalidating the transcript here re-estimated every message
/// height, cleared the user's selection, and (through the scroll
/// bookkeeping) made a scrolled-up main thread drift while subagents worked.
fn subagent_hud_redraw(app: &mut App) {
    app.request_redraw();
}

/// Toast id of the confirm prompts left to another client.
const WAITING_CONFIRMS_TOAST: &str = "confirms-waiting";
/// How many waiting confirms the toast lists by id.
const WAITING_CONFIRMS_SHOWN: usize = 4;

/// Show (or clear) the confirm prompts this client leaves to another one
/// (`--no-confirm-answers`, [`super::session_link::ConfirmPolicy::ViewOnly`]):
/// one resident toast listing them, gone once none is waiting. Under the
/// default policy nothing ever waits, so this never shows.
pub(super) fn sync_waiting_confirms(app: &mut App, bridge: &PromptBridge) {
    let waiting = bridge.waiting_confirms();
    if waiting.is_empty() {
        if app.toasts.dismiss(WAITING_CONFIRMS_TOAST) {
            app.request_redraw();
        }
        return;
    }
    let mut lines = vec![
        "answer it in another client:".to_string(),
        "this one runs with --no-confirm-answers".to_string(),
    ];
    for (id, title) in waiting.iter().take(WAITING_CONFIRMS_SHOWN) {
        let title: String = sanitize_notice(title)
            .chars()
            .map(|c| if c.is_whitespace() { ' ' } else { c })
            .take(48)
            .collect();
        lines.push(format!("#{id} {title}"));
    }
    if waiting.len() > WAITING_CONFIRMS_SHOWN {
        lines.push(format!("+{} more", waiting.len() - WAITING_CONFIRMS_SHOWN));
    }
    let title = if waiting.len() == 1 {
        "Approval waiting".to_string()
    } else {
        format!("{} approvals waiting", waiting.len())
    };
    app.toasts.upsert(
        super::toast::Toast::new(WAITING_CONFIRMS_TOAST, String::new())
            .titled(title)
            .lines(lines)
            .ttl(None),
    );
    app.request_redraw();
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
/// Copy each running worker's live progress (step + tool count) from fresh
/// registry rows into its HUD entry. Called when rows ARRIVE, never from the
/// periodic reconcile: rows emitted at time T reflect every progress write
/// before T, so a just-received row is never older than an update the
/// stream already delivered, while the cached rows the 1 Hz reconcile reads
/// can be. This is how a background worker (`subagent_start`) keeps moving
/// in the tray after the turn that started it has ended (its
/// `SubagentUpdate` events rode that turn's stream). Done, cancelling, and
/// not-yet-updated entries are left alone. Returns true if anything changed.
pub(super) fn apply_subagent_progress(
    hud: &mut [SubagentState],
    rows: &[synaps_cli::tools::SubagentDisplayRow],
) -> bool {
    use synaps_cli::runtime::subagent::SubagentStatus;
    let mut changed = false;
    for row in rows {
        if !matches!(row.status, SubagentStatus::Running) || row.cancel_requested {
            continue;
        }
        let Some(sa) = hud.iter_mut().find(|s| s.id == row.subagent_id && !s.done) else {
            continue;
        };
        if !row.step.is_empty() && sa.status != row.step {
            sa.status = row.step.clone();
            changed = true;
        }
        if sa.tools != row.tools {
            sa.tools = row.tools;
            changed = true;
        }
    }
    changed
}

/// Status of a tray row whose worker a daemon reload stopped.
pub(super) const STOPPED_BY_RELOAD_STATUS: &str = "\u{26a0} stopped by daemon reload";

/// `Reloading`: every running row ends as stopped by the reload and flashes
/// out like any finished row. Returns whether any row changed.
pub(super) fn mark_workers_stopped_by_reload(
    hud: &mut [SubagentState],
    now: std::time::Instant,
) -> bool {
    let mut changed = false;
    for sa in hud.iter_mut().filter(|s| !s.done) {
        sa.done = true;
        sa.done_at = Some(now);
        if sa.duration_secs.is_none() {
            sa.duration_secs = Some(now.saturating_duration_since(sa.start_time).as_secs_f64());
        }
        sa.status = STOPPED_BY_RELOAD_STATUS.to_string();
        changed = true;
    }
    changed
}

/// How long a done entry stays visible before reconcile removes it.
pub(super) const SUBAGENT_DONE_FLASH_SECS: f64 = 5.0;

/// A row reconcile would add to the tray: running, not being cancelled, and
/// not already shown.
fn has_untracked_worker(
    hud: &[SubagentState],
    rows: &[synaps_cli::tools::SubagentDisplayRow],
) -> bool {
    use synaps_cli::runtime::subagent::SubagentStatus;
    rows.iter().any(|r| {
        matches!(r.status, SubagentStatus::Running)
            && !r.cancel_requested
            && !hud.iter().any(|s| s.id == r.subagent_id)
    })
}

/// Status line between Esc and the actor's `Aborted` (it drains the
/// cancelled stream first, bounded by `budgets::CANCEL_DRAIN_TIMEOUT`).
pub(super) const ABORTING_STATUS: &str = "aborting…";

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
                tools: 0,
                result: None,
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
    use super::{apply_subagent_progress, reconcile_subagents, SUBAGENT_DONE_FLASH_SECS};
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
            step: String::new(),
            tools: 0,
            handle_id: format!("sa_{id}"),
            model: String::new(),
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
            tools: 0,
            result: None,
        }
    }

    /// Fresh rows move a running entry's step and tool count; done,
    /// cancelling, unknown and not-yet-updated entries are left alone.
    #[test]
    fn progress_from_rows_moves_running_entries_only() {
        let mut hud = vec![
            make_hud_entry(1, false),
            make_hud_entry(2, true),
            make_hud_entry(3, false),
        ];
        hud[2].status = "starting: task".into();
        let mut running = make_row(1, SubagentStatus::Running, false);
        running.step = "$ sleep 20".into();
        running.tools = 1;
        let mut done = make_row(2, SubagentStatus::Completed, false);
        done.step = "stale".into();
        let quiet = make_row(3, SubagentStatus::Running, false); // no update yet
        let mut cancelling = make_row(4, SubagentStatus::Running, true);
        cancelling.step = "x".into();

        assert!(apply_subagent_progress(
            &mut hud,
            &[running.clone(), done, quiet, cancelling]
        ));
        assert_eq!((hud[0].status.as_str(), hud[0].tools), ("$ sleep 20", 1));
        assert_eq!(hud[1].status, "\u{2714} done", "done entry untouched");
        assert_eq!(
            hud[2].status, "starting: task",
            "empty step keeps the placeholder"
        );
        assert!(
            !apply_subagent_progress(&mut hud, &[running]),
            "unchanged → false"
        );
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
            !hud.iter().any(|s| s.id == 3),
            "sa_3 expired flash must be removed"
        );

        // sa_4 inserted
        assert!(hud.iter().any(|s| s.id == 4), "sa_4 must be inserted");
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

/// The transcript half of `handle_stream_event` is `apply_transcript_event`
/// — so a worker thread fed the same events renders exactly what the main
/// thread would, and never touches the main thread while doing it.
#[cfg(test)]
mod transcript_split_tests {
    use super::super::app::{App, ChatMessage};
    use super::super::clock::TuiClock;
    use super::super::threads::Threads;
    use super::super::transcript::TranscriptStore;
    use super::{apply_transcript_event, handle_stream_event, Applied};
    use agent_core::TurnError;
    use agent_engine::session::RuntimeView;
    use synaps_cli::{AgentEvent, LlmEvent, Runtime, Session, SessionEvent, StreamEvent};

    fn repr(store: &TranscriptStore) -> Vec<String> {
        store
            .messages()
            .iter()
            .map(|m| match &m.msg {
                ChatMessage::User(t) => format!("U|{t}"),
                ChatMessage::Thinking(t) => format!("K|{t}"),
                ChatMessage::Text(t) => format!("T|{t}"),
                ChatMessage::System(t) => format!("S|{t}"),
                ChatMessage::Error(t) => format!("E|{t}"),
                ChatMessage::ToolUseStart {
                    tool_id,
                    tool_name,
                    partial_input,
                } => {
                    format!("XS|{tool_id}|{tool_name}|{partial_input}")
                }
                ChatMessage::ToolUse {
                    tool_id,
                    tool_name,
                    input,
                } => {
                    format!("X|{tool_id}|{tool_name}|{input}")
                }
                // elapsed_ms depends on each store's clock — not compared.
                ChatMessage::ToolResult {
                    tool_id, content, ..
                } => format!("R|{tool_id}|{content}"),
                _ => "?".to_string(),
            })
            .collect()
    }

    /// Every transcript-bearing event kind, plus non-transcript events that
    /// must leave a worker store alone.
    fn script() -> Vec<StreamEvent> {
        use AgentEvent as A;
        use LlmEvent as L;
        use SessionEvent as S;
        use StreamEvent::{Agent, Llm, Session as Sess};
        let s = |v: &str| v.to_string();
        vec![
            Llm(L::ResponseStart),
            Llm(L::Thinking(s("plan the answer"))),
            Llm(L::Text(s("hello "))),
            Llm(L::Text(s("world"))),
            Llm(L::ToolUseStart {
                tool_name: s("bash"),
                tool_id: s("t1"),
            }),
            Llm(L::ToolUseDelta {
                tool_id: s("t1"),
                delta: s("{\"command\":"),
            }),
            Llm(L::ToolUse {
                tool_name: s("bash"),
                tool_id: s("t1"),
                input: serde_json::json!({ "command": "ls" }),
            }),
            Llm(L::ToolResultDelta {
                tool_id: s("t1"),
                delta: s("a\n"),
            }),
            Llm(L::ToolResult {
                tool_id: s("t1"),
                result: s("a\nb"),
            }),
            Agent(A::SubagentStart {
                subagent_id: 3,
                agent_name: s("scout"),
                task_preview: s("look around"),
            }),
            Sess(S::Notice(s("retry \x1b[2J in 2s"))),
            Sess(S::Done),
            // A response that is rolled back, then an orphaned reset.
            Llm(L::ResponseStart),
            Llm(L::Text(s("draft that gets discarded"))),
            Llm(L::ResponseReset),
            Llm(L::ResponseReset),
            Agent(A::SteeringDelivered {
                message: s("user steering text"),
            }),
            Agent(A::SteeringDelivered {
                message: s(
                    "<event id=\"e1\" type=\"subagent_completion\" severity=\"high\" \
                     source=\"subagent\">Subagent 'inline' (sa_1) finished.</event>",
                ),
            }),
            Llm(L::ResponseStart),
            Llm(L::Text(s("partial"))),
            Sess(S::Error(TurnError::provider(
                "boom",
                "auth_error",
                "turn-1",
            ))),
        ]
    }

    #[test]
    fn worker_store_renders_exactly_like_the_main_thread() {
        let mut app = App::new(Session::new("test-model", "low", None));
        let runtime = Runtime::new_headless();
        let view = RuntimeView::snapshot(&runtime, 0);
        let base = app.transcript.messages().len();

        let mut worker = TranscriptStore::new(TuiClock::test());
        let mut preview = None;
        for ev in script() {
            apply_transcript_event(&mut worker, &mut preview, &ev);
            handle_stream_event(ev, &mut app, &view);
        }

        let main = repr(&app.transcript);
        assert_eq!(
            main[base..].to_vec(),
            repr(&worker),
            "same events, same transcript"
        );
        // Spot-check the script actually exercised the interesting paths.
        let w = repr(&worker);
        assert!(
            w.iter().any(|m| m == "R|t1|a\nb"),
            "tool result routed: {w:?}"
        );
        assert!(w.contains(&"U|user steering text".to_string()));
        assert!(
            !w.iter().any(|m| m.contains("subagent_completion")),
            "event payload not a user bubble"
        );
        assert!(
            !w.iter().any(|m| m.contains("discarded")),
            "reset rolled the draft back"
        );
        assert!(
            w.iter().any(|m| m.starts_with("E|boom [")),
            "typed error surfaced"
        );
        assert!(!w.iter().any(|m| m.contains('\x1b')), "notice sanitized");
    }

    #[test]
    fn applied_classifies_events() {
        let mut store = TranscriptStore::new(TuiClock::test());
        let mut preview = None;
        let mut apply = |ev: StreamEvent| apply_transcript_event(&mut store, &mut preview, &ev);
        assert_eq!(
            apply(StreamEvent::Llm(LlmEvent::ResponseReset)),
            Applied::Unchanged,
            "orphan reset"
        );
        assert_eq!(
            apply(StreamEvent::Agent(AgentEvent::SubagentStart {
                subagent_id: 1,
                agent_name: "a".into(),
                task_preview: "t".into(),
            })),
            Applied::Ignored
        );
        assert_eq!(
            apply(StreamEvent::Llm(LlmEvent::ResponseStart)),
            Applied::Changed
        );
        assert_eq!(
            apply(StreamEvent::Llm(LlmEvent::Text("x".into()))),
            Applied::Changed
        );
        assert_eq!(
            apply(StreamEvent::Llm(LlmEvent::ResponseReset)),
            Applied::RolledBack
        );
    }

    #[test]
    fn feeding_a_worker_thread_never_touches_the_main_store() {
        let mut main = TranscriptStore::new(TuiClock::test());
        main.push_msg(ChatMessage::Text("main".into()));
        main.scroll_up(3);
        let (before, scroll) = (repr(&main), main.scroll_back_pos());

        let mut threads = Threads::default();
        let w = threads.open_worker("sa_9", 9, "scout", TuiClock::test(), (0, 0));
        for ev in script() {
            apply_transcript_event(&mut w.store, &mut w.response_preview, &ev);
        }
        assert!(!repr(&threads.worker("sa_9").unwrap().store).is_empty());
        assert_eq!(repr(&main), before, "main transcript untouched");
        assert_eq!(main.scroll_back_pos(), scroll, "main scroll untouched");
    }
}
