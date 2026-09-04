//! Free helper functions used by the main chat event loop.
//!
//! Extracted from `mod.rs` to keep `run()` focused on orchestration.

use std::time::Duration;

use super::app::{App, ChatMessage};
use super::{session_link, settings};

/// Decide whether to repaint on this event-loop iteration.
///
/// During streaming, model-text redraws are coalesced to `throttle` — the #131
/// CPU win (per-delta full-model rebuilds at 60fps used to burn a core, and the
/// eye can't read faster than ~10fps anyway). But a `force` redraw — set by any
/// user input (scroll, typing, cursor, paste, resize) — bypasses the throttle so
/// interaction stays instant. When not streaming, redraws are always immediate.
/// Nothing repaints unless `needs_redraw` is set.
#[must_use]
pub(super) fn should_draw(
    needs_redraw: bool,
    force: bool,
    streaming: bool,
    since_last: Duration,
    throttle: Duration,
) -> bool {
    needs_redraw && (force || !streaming || since_last >= throttle)
}

/// Apply a settings-menu change: mutate Runtime where possible, persist to config,
/// and stash write errors in the modal's row_error slot.
///
/// The runtime mutation is delegated to the macro-generated dispatch in
/// `settings/defs.rs` — single source of truth for schema + apply.
pub(super) async fn apply_setting(
    key: &'static str,
    value: &str,
    app: &mut App,
    link: &mut session_link::SessionLink,
) {
    // Resolve (generated from settings/defs.rs). Runtime keys round-trip
    // through `Set{id}`; on Err/rejection: set row_error and do NOT write to
    // config — the value was rejected.
    match settings::defs::apply_setting_dispatch(key, value, app) {
        settings::defs::SettingApply::Local(Ok(())) => {}
        settings::defs::SettingApply::Local(Err(msg)) => {
            if let Some(st) = app.settings.as_mut() {
                st.row_error = Some((key.to_string(), msg));
            }
            return;
        }
        settings::defs::SettingApply::Session(setting) => match link.set_checked(setting).await {
            Ok(applied) => {
                if key == "context_window" {
                    // Also update the bar denominator immediately so the UI
                    // reflects the change.
                    app.last_turn_context_window = applied.view.context_window;
                }
            }
            Err(msg) => {
                if let Some(st) = app.settings.as_mut() {
                    st.row_error = Some((key.to_string(), msg));
                }
                return;
            }
        },
    }

    // Keep the embedded session in sync with settings changes so resume sees
    // the exact active provider/model reasoning selection.
    if key == "thinking" {
        app.session.thinking_level = value.to_string();
    } else if key == "model" {
        app.session.model = link.view().model.clone();
    }

    // `skills` is internal — not persisted via write_config_value.
    if key == "skills" {
        return;
    }

    match synaps_cli::config::write_config_value(key, value) {
        Ok(()) => {
            if key == "theme" {
                // Same apply as /theme: animated cross-fade PLUS the
                // live-MXC subscriber reconcile (spawn on "myx", abort on
                // switch-away). Skipping sync_myx_live here leaked the
                // subscriber forever — album colors re-stomped the chosen
                // theme on every track change (shady F2 / okarin F1).
                app.apply_theme_from_settings(value);
            }
            if let Some(st) = app.settings.as_mut() {
                st.row_error = None;
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

pub(super) async fn fetch_usage() -> std::result::Result<Vec<String>, String> {
    // Usage is a typed broker operation: the configured broker (local or
    // remote) pins the destination URL and resolves/attaches the OAuth token
    // behind the credential boundary. The TUI never reads the credential
    // store and never sees a token — it receives usage JSON only.
    let data = synaps_cli::auth::global_broker()
        .anthropic_usage()
        .await
        .map_err(|e| e.to_string())?;

    fn format_block(label: &str, data: &serde_json::Value) -> Option<Vec<String>> {
        let util = data["utilization"].as_f64()?;
        let resets = data["resets_at"].as_str()?;
        let reset_display = if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(resets) {
            let diff = dt.signed_duration_since(chrono::Utc::now());
            let hours = diff.num_hours();
            let mins = diff.num_minutes() % 60;
            if hours > 24 {
                format!("{}d {}h", hours / 24, hours % 24)
            } else if hours > 0 {
                format!("{}h {}m", hours, mins)
            } else {
                format!("{}m", diff.num_minutes())
            }
        } else {
            "—".to_string()
        };

        let filled = ((util / 100.0) * 20.0) as usize;
        let empty = 20usize.saturating_sub(filled);
        let bar = format!("{}{}", "█".repeat(filled), "░".repeat(empty));
        Some(vec![
            label.to_string(),
            format!("{} {:.0}%", bar, util),
            format!("resets in {}", reset_display),
        ])
    }

    let mut lines = vec!["⚡ Account Usage".to_string()];
    if let Some(rows) = format_block("5-hour window", &data["five_hour"]) {
        lines.extend(rows);
        lines.push(String::new());
    }
    if let Some(rows) = format_block("7-day window", &data["seven_day"]) {
        lines.extend(rows);
        lines.push(String::new());
    }
    if let Some(rows) = format_block("Sonnet (7-day)", &data["seven_day_sonnet"]) {
        lines.extend(rows);
    }

    Ok(lines)
}

/// True iff `content` is a canonical agent-event payload — produced in exactly
/// one place (`format_event_for_agent`): `<event id=… …>…</event>`.
///
/// Event payloads travel as `role=user` messages (the API needs a user turn)
/// and ride the same steering channel as genuine user steering, but they are
/// presented as Event cards. They must NEVER render as a `ChatMessage::User`
/// bubble — that made subagent completion wakes appear in the transcript as a
/// message the user typed and submitted.
pub(super) fn is_event_payload(content: &str) -> bool {
    content.starts_with("<event ") && content.ends_with("</event>")
}

/// Display items shown on resume/rebuild (matches `cap_resumed_display(120)`
/// at 741b6b60 and `ClientMeta::tail_items`' default).
pub(super) const DISPLAY_TAIL_ITEMS: usize = agent_engine::session::DEFAULT_TAIL_ITEMS;

/// Rebuild the transcript from a full history: the same projection the
/// daemon ships Digest clients (`session::display::display_tail`), so Local
/// and Socket render byte-identical lines.
pub(super) fn rebuild_display_messages(api_messages: &[synaps_cli::SharedMessage], app: &mut App) {
    let tail = agent_engine::session::display::display_tail(api_messages, DISPLAY_TAIL_ITEMS);
    apply_display_tail(&tail, app);
}

/// Replace the transcript with a daemon-projected tail. `omitted > 0`
/// prepends the identical sentinel `cap_resumed_display` produced at
/// 741b6b60 (same text, same position).
pub(super) fn apply_display_tail(tail: &agent_engine::session::DisplayTail, app: &mut App) {
    use agent_engine::session::DisplayItem;
    app.transcript.clear();
    if tail.omitted > 0 {
        app.push_msg(ChatMessage::System(format!(
            "… {} earlier message(s) hidden to speed resume — full history is still in the model's context",
            tail.omitted
        )));
    }
    for item in &tail.items {
        app.push_msg(match item {
            DisplayItem::User { text } => ChatMessage::User(text.clone()),
            DisplayItem::Thinking { text } => ChatMessage::Thinking(text.clone()),
            DisplayItem::Text { text } => ChatMessage::Text(text.clone()),
            DisplayItem::ToolUse {
                tool_id,
                tool_name,
                input,
            } => ChatMessage::ToolUse {
                tool_id: tool_id.clone(),
                tool_name: tool_name.clone(),
                input: input.clone(),
            },
        });
    }
    // Wholesale rebuild = a full message-list reshuffle. `messages.clear()` above
    // does NOT touch the line cache, and push_msg's incremental invalidate_last()
    // only covers the tail — so if the tail was empty, the stale cache would
    // render deleted messages. Force a full invalidate.
    app.invalidate();
}

#[cfg(test)]
mod tests {
    use super::super::app::{App, ChatMessage, LineCache, MsgSlot};
    use super::{is_event_payload, rebuild_display_messages, should_draw};
    use std::time::Duration;
    use synaps_cli::Session;

    fn test_app() -> App {
        App::new(Session::new("test-model", "low", None))
    }

    /// Canonical event payloads (subagent completion wakes, watcher alerts)
    /// must be recognized so they render as Event cards, never as user
    /// bubbles — neither on session resume (rebuild_display_messages) nor
    /// when steered into a live stream (SteeringDelivered).
    #[test]
    fn event_payloads_are_recognized_and_user_text_is_not() {
        assert!(is_event_payload(
            "<event id=\"c9ec5a4b\" type=\"subagent_completion\" severity=\"high\" \
             source=\"subagent\">Subagent 'inline' (sa_1) finished with status \
             'completed' after 231.1s. Call subagent_collect with handle_id \
             \"sa_1\" to retrieve the full result.</event>"
        ));
        // Genuine user steering — even when it mentions events — stays a user bubble.
        assert!(!is_event_payload("please re-read the <event ...> above"));
        assert!(!is_event_payload("regular steering text"));
        assert!(!is_event_payload(""));
    }

    /// Build a populated LineCache for all messages in `app` at `width`, then
    /// mark it Clean (old "dirty_from = None"). This simulates the state right
    /// after a successful draw — the renderer believes the cache is valid.
    fn prime_clean_cache(app: &mut App, width: usize) {
        let per_msg: Vec<MsgSlot> = (0..app.transcript.message_count())
            .map(|i| app.render_message_lines(i, width))
            .collect();
        // Clean — renderer thinks nothing changed
        app.transcript
            .test_set_cache_clean(LineCache::new(width, per_msg));
    }

    // -------------------------------------------------------------------------
    // Regression: rebuild_display_messages must invalidate the line cache even
    // when api_messages is empty (or all entries are filtered out).
    //
    // Before fbcfa05 the function only relied on push_msg's incremental
    // invalidate_last() to dirty the cache. When zero pushes happened (empty
    // or fully-filtered input) the stale cache survived: line_cache stayed
    // Some(...) and dirty_from stayed None — "clean" — so the next frame
    // rendered deleted messages. The fix adds app.invalidate() at the end of
    // rebuild_display_messages, which sets line_cache = None unconditionally.
    // -------------------------------------------------------------------------

    /// EMPTY api_messages case: no pushes happen, so without the explicit
    /// app.invalidate() the cache would stay "clean" and render stale content.
    #[test]
    fn rebuild_display_messages_empty_input_invalidates_line_cache() {
        let mut app = test_app();

        // Give the app a message so the cache is non-trivial.
        app.push_msg(ChatMessage::User("hello from the past".to_string()));
        prime_clean_cache(&mut app, 80);

        // Sanity: cache is primed and marked clean.
        assert!(
            app.transcript.line_cache().is_some(),
            "pre-condition: cache must be primed"
        );
        assert!(
            app.transcript.cache_dirty_from().is_none(),
            "pre-condition: cache must be Clean (no dirty watermark)"
        );

        // Trigger: rebuild with an empty message list — the exact bug trigger.
        // Zero push_msg calls happen → without the fix, no invalidation occurs.
        rebuild_display_messages(&[], &mut app);

        // Post-condition: cache must be fully invalidated so the next draw
        // doesn't render the now-deleted "hello from the past" message.
        // The fix calls app.invalidate() which sets line_cache = None.
        assert!(
            app.transcript.line_cache().is_none(),
            "cache must be Missing after rebuild with empty api_messages — \
             stale cache would render deleted messages (fbcfa05 regression)"
        );
    }

    /// FULLY-FILTERED api_messages case: every entry contains <context-summary>
    /// or <event ...>...</event> and is skipped, so again zero push_msg calls
    /// happen. Same latent bug, same fix.
    #[test]
    fn rebuild_display_messages_fully_filtered_input_invalidates_line_cache() {
        let mut app = test_app();

        app.push_msg(ChatMessage::User("previous content".to_string()));
        prime_clean_cache(&mut app, 80);

        assert!(
            app.transcript.line_cache().is_some(),
            "pre-condition: cache must be primed"
        );
        assert!(
            app.transcript.cache_dirty_from().is_none(),
            "pre-condition: cache must be Clean (no dirty watermark)"
        );

        // Every entry is filtered out by rebuild_display_messages.
        let api_messages = vec![
            std::sync::Arc::new(serde_json::json!({
                "role": "user",
                "content": "<context-summary>compacted context</context-summary>"
            })),
            std::sync::Arc::new(serde_json::json!({
                "role": "user",
                "content": "<event tool_use_id=\"x\">data</event>"
            })),
        ];

        rebuild_display_messages(&api_messages, &mut app);

        assert!(
            app.transcript.line_cache().is_none(),
            "cache must be Missing after rebuild with fully-filtered api_messages — \
             stale cache would render deleted messages (fbcfa05 regression)"
        );
    }

    // -------------------------------------------------------------------------
    // Golden: the daemon-side projection (`session::display::display_tail`)
    // must reproduce `rebuild_display_messages` @ 741b6b60 byte-for-byte.
    // The legacy body is kept VERBATIM below (including cap_resumed_display).
    // -------------------------------------------------------------------------

    fn legacy_rebuild(api_messages: &[synaps_cli::SharedMessage], app: &mut App) {
        app.transcript.clear();
        for msg in api_messages {
            if let Some(content) = msg["content"].as_str() {
                if content.contains("<context-summary>") {
                    continue;
                }
            }
            if let Some(content) = msg["content"].as_str() {
                if is_event_payload(content) {
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
                                    let input =
                                        serde_json::to_string(&block["input"]).unwrap_or_default();
                                    let tool_id = block["id"].as_str().unwrap_or("").to_string();
                                    app.push_msg(ChatMessage::ToolUse {
                                        tool_id,
                                        tool_name: name,
                                        input,
                                    });
                                }
                                _ => {}
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        app.transcript.cap_resumed_display(120);
        app.invalidate();
    }

    fn transcript_repr(app: &App) -> Vec<String> {
        app.transcript
            .messages()
            .iter()
            .map(|m| match &m.msg {
                ChatMessage::User(t) => format!("U|{t}"),
                ChatMessage::Thinking(t) => format!("K|{t}"),
                ChatMessage::Text(t) => format!("T|{t}"),
                ChatMessage::System(t) => format!("S|{t}"),
                ChatMessage::ToolUse {
                    tool_id,
                    tool_name,
                    input,
                } => format!("X|{tool_id}|{tool_name}|{input}"),
                _ => "?".to_string(),
            })
            .collect()
    }

    fn j(v: serde_json::Value) -> synaps_cli::SharedMessage {
        std::sync::Arc::new(v)
    }

    fn golden_fixtures() -> Vec<(&'static str, Vec<synaps_cli::SharedMessage>)> {
        use serde_json::json;
        let plain = vec![
            j(json!({"role": "user", "content": "hello"})),
            j(json!({"role": "assistant", "content": [{"type": "text", "text": "hi there"}]})),
            j(json!({"role": "user", "content": "what's 2+2"})),
            j(json!({"role": "assistant", "content": [
                {"type": "thinking", "thinking": "easy"},
                {"type": "text", "text": "4"}]})),
        ];
        let tool_loop = vec![
            j(json!({"role": "user", "content": "list files"})),
            j(json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": "bash", "input": {"command": "ls"}}]})),
            j(json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "a\nb"}]})),
            j(json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "t2", "name": "read", "input": {"path": "a"}},
                {"type": "tool_use", "id": "t3", "name": "read", "input": {"path": "b"}}]})),
            j(json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t2", "content": "A"},
                {"type": "tool_result", "tool_use_id": "t3", "content": "B"}]})),
            j(json!({"role": "assistant", "content": [{"type": "text", "text": "done"}]})),
        ];
        let context_summary = vec![
            j(json!({"role": "user", "content": "<context-summary>old stuff</context-summary>"})),
            j(json!({"role": "assistant", "content": [{"type": "text", "text": "ok, continuing"}]})),
            j(json!({"role": "user", "content": "mentions <context-summary> inline too"})),
            j(json!({"role": "assistant", "content": "string-content assistant (ignored)"})),
            j(json!({"role": "system", "content": "never shown"})),
        ];
        let events = vec![
            j(json!({"role": "user", "content": "<event id=\"1\" type=\"subagent_completion\">done</event>"})),
            j(json!({"role": "assistant", "content": [{"type": "text", "text": "noted"}]})),
            j(json!({"role": "user", "content": "please re-read the <event ...> above"})),
            j(json!({"role": "assistant", "content": [
                {"type": "text"},
                {"type": "thinking"},
                {"type": "tool_use", "input": {"x": 1}},
                {"type": "unknown", "text": "zzz"}]})),
        ];
        let mut long = Vec::new();
        for i in 0..70 {
            long.push(j(json!({"role": "user", "content": format!("q{i}")})));
            long.push(j(json!({"role": "assistant", "content": [
                {"type": "thinking", "thinking": format!("think{i}")},
                {"type": "text", "text": format!("a{i}")}]})));
        }
        vec![
            ("plain", plain),
            ("tool_loop", tool_loop),
            ("context_summary", context_summary),
            ("events", events),
            ("long_over_120", long),
            ("empty", Vec::new()),
        ]
    }

    #[test]
    fn display_tail_golden_matches_741b6b60_rebuild() {
        for (name, fixture) in golden_fixtures() {
            let mut old = test_app();
            legacy_rebuild(&fixture, &mut old);
            let mut new = test_app();
            rebuild_display_messages(&fixture, &mut new);
            assert_eq!(transcript_repr(&old), transcript_repr(&new), "fixture {name}");
            assert!(new.transcript.line_cache().is_none(), "fixture {name}: cache invalidated");
            // The daemon-projected tail applied directly is the same again.
            let tail = agent_engine::session::display::display_tail(&fixture, 120);
            let mut via_tail = test_app();
            super::apply_display_tail(&tail, &mut via_tail);
            assert_eq!(transcript_repr(&old), transcript_repr(&via_tail), "fixture {name} via tail");
        }
    }

    #[test]
    fn long_fixture_actually_exercises_the_cap() {
        let (_, long) = golden_fixtures().into_iter().find(|(n, _)| *n == "long_over_120").unwrap();
        let mut app = test_app();
        rebuild_display_messages(&long, &mut app);
        let repr = transcript_repr(&app);
        assert_eq!(repr.len(), 121);
        assert!(repr[0].starts_with("S|… 90 earlier message(s) hidden"), "{}", repr[0]);
    }

    const T: Duration = Duration::from_millis(100);

    #[test]
    fn never_draws_when_flag_unset() {
        // No redraw requested → never paint, regardless of other state.
        assert!(!should_draw(false, false, false, Duration::from_secs(1), T));
        assert!(!should_draw(false, true, true, Duration::from_secs(1), T));
    }

    #[test]
    fn idle_redraws_immediately() {
        // Not streaming → paint as soon as something needs it.
        assert!(should_draw(true, false, false, Duration::ZERO, T));
    }

    #[test]
    fn streaming_text_is_throttled() {
        // Streaming model text, not user-forced: coalesce to the throttle.
        assert!(!should_draw(
            true,
            false,
            true,
            Duration::from_millis(16),
            T
        )); // too soon → wait
        assert!(should_draw(
            true,
            false,
            true,
            Duration::from_millis(100),
            T
        )); // throttle elapsed → paint
    }

    #[test]
    fn user_input_bypasses_streaming_throttle() {
        // THE FIX: scroll/typing during streaming must paint instantly,
        // even mid-throttle. (Pre-fix this returned false → choppy scroll.)
        assert!(should_draw(true, true, true, Duration::ZERO, T));
    }
}
