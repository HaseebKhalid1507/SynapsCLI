//! Free helper functions used by the main chat event loop.
//!
//! Extracted from `mod.rs` to keep `run()` focused on orchestration.

use serde_json::Value;

use super::app::{App, ChatMessage};
use super::theme;
use super::settings;

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

    let client = reqwest::Client::new();
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

pub(super) fn rebuild_display_messages(api_messages: &[Value], app: &mut App) {
    app.messages.clear();
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
                                app.push_msg(ChatMessage::ToolUse { tool_name: name, input });
                            }
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
        }
    }
}
