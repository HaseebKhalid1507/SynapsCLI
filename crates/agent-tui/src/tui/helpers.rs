//! Free helper functions used by the main chat event loop.
//!
//! Extracted from `mod.rs` to keep `run()` focused on orchestration.

use std::time::Duration;


use super::app::{App, ChatMessage};
use super::theme;
use super::settings;

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
pub(super) fn apply_setting(
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

pub(super) async fn fetch_usage() -> std::result::Result<Vec<String>, String> {
    let auth_path = synaps_cli::config::base_dir().join("auth.json");
    let content = std::fs::read_to_string(&auth_path)
        .map_err(|e| format!("Auth read failed: {}", e))?;
    let auth: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| format!("Auth parse failed: {}", e))?;
    let access = auth["anthropic"]["access"].as_str()
        .ok_or("No OAuth token")?;

    let client = reqwest::Client::builder()
        .tls_built_in_webpki_certs(true)
        .tls_built_in_native_certs(true)
        .build()
        .map_err(|e| format!("API client build error: {}", e))?;
    let resp = client.get("https://api.anthropic.com/api/oauth/usage")
        .header("Authorization", format!("Bearer {}", access))
        .header("anthropic-beta", "oauth-2025-04-20")
        .send().await
        .map_err(|e| format!("API error: {}", e))?;

    let data: serde_json::Value = resp.json().await
        .map_err(|e| format!("Parse error: {}", e))?;

    fn format_block(label: &str, data: &serde_json::Value) -> Option<Vec<String>> {
        let util = data["utilization"].as_f64()?;
        let resets = data["resets_at"].as_str()?;
        let reset_display = if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(resets) {
            let diff = dt.signed_duration_since(chrono::Utc::now());
            let hours = diff.num_hours();
            let mins = diff.num_minutes() % 60;
            if hours > 24 { format!("{}d {}h", hours / 24, hours % 24) }
            else if hours > 0 { format!("{}h {}m", hours, mins) }
            else { format!("{}m", diff.num_minutes()) }
        } else { "—".to_string() };

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
    if let Some(rows) = format_block("5-hour window", &data["five_hour"]) { lines.extend(rows); lines.push(String::new()); }
    if let Some(rows) = format_block("7-day window", &data["seven_day"]) { lines.extend(rows); lines.push(String::new()); }
    if let Some(rows) = format_block("Sonnet (7-day)", &data["seven_day_sonnet"]) { lines.extend(rows); }

    Ok(lines)
}

pub(super) fn rebuild_display_messages(api_messages: &[synaps_cli::SharedMessage], app: &mut App) {
    app.transcript.clear();
    for msg in api_messages {
        // Skip compaction summary messages — internal context, not user-visible
        if let Some(content) = msg["content"].as_str() {
            if content.contains("<context-summary>") {
                continue;
            }
        }
        // Skip event messages — already displayed as event cards
        if let Some(content) = msg["content"].as_str() {
            if content.starts_with("<event ") && content.ends_with("</event>") {
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
                                let tool_id = block["id"].as_str().unwrap_or("").to_string();
                                app.push_msg(ChatMessage::ToolUse { tool_id, tool_name: name, input });
                            }
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
        }
    }
    // Cap the rendered scrollback so a long --continue doesn't markdown-render
    // hundreds of messages on the first frame (the slow-boot cause). Full
    // history stays in api_messages for the model. See App::cap_resumed_display.
    app.cap_resumed_display(120);

    // Wholesale rebuild = a full message-list reshuffle. `messages.clear()` above
    // does NOT touch the line cache, and push_msg's incremental invalidate_last()
    // only covers the tail — so if api_messages was empty or fully filtered, the
    // stale cache would render deleted messages. Force a full invalidate; this is
    // exactly the "message list reshuffle" case invalidate() is documented for.
    app.invalidate();
}

#[cfg(test)]
mod tests {
    use super::{should_draw, rebuild_display_messages};
    use super::super::app::{App, ChatMessage, LineCache, MsgSlot};
    use synaps_cli::Session;
    use std::time::Duration;

    fn test_app() -> App {
        App::new(Session::new("test-model", "low", None))
    }

    /// Build a populated LineCache for all messages in `app` at `width`, then
    /// mark it Clean (old "dirty_from = None"). This simulates the state right
    /// after a successful draw — the renderer believes the cache is valid.
    fn prime_clean_cache(app: &mut App, width: usize) {
        let per_msg: Vec<MsgSlot> = (0..app.transcript.message_count())
            .map(|i| app.render_message_lines(i, width))
            .collect();
        // Clean — renderer thinks nothing changed
        app.transcript.test_set_cache_clean(LineCache::new(width, per_msg));
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
        assert!(app.transcript.line_cache().is_some(), "pre-condition: cache must be primed");
        assert!(app.transcript.cache_dirty_from().is_none(), "pre-condition: cache must be Clean (no dirty watermark)");

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

        assert!(app.transcript.line_cache().is_some(), "pre-condition: cache must be primed");
        assert!(app.transcript.cache_dirty_from().is_none(), "pre-condition: cache must be Clean (no dirty watermark)");

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
        assert!(!should_draw(true, false, true, Duration::from_millis(16), T)); // too soon → wait
        assert!(should_draw(true, false, true, Duration::from_millis(100), T)); // throttle elapsed → paint
    }

    #[test]
    fn user_input_bypasses_streaming_throttle() {
        // THE FIX: scroll/typing during streaming must paint instantly,
        // even mid-throttle. (Pre-fix this returned false → choppy scroll.)
        assert!(should_draw(true, true, true, Duration::ZERO, T));
    }
}
