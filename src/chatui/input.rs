//! Input handling — keyboard events, cursor movement, paste, mouse scroll.

use std::sync::Arc;

use crossterm::event::{KeyCode, KeyModifiers, MouseEventKind, Event};
use synaps_cli::skills::registry::CommandRegistry;
use super::app::{App, ChatMessage};

/// What the event loop should do after processing input.
pub(super) enum InputAction {
    /// Nothing special — continue the loop.
    None,
    /// User submitted text (non-slash) — contains the raw input string.
    Submit(String),
    /// User submitted a slash command — (resolved_cmd, arg).
    SlashCommand(String, String),
    /// User submitted input while streaming — contains the raw input string.
    StreamingInput(String),
    /// Start the quit animation.
    Quit,
    /// Abort the current stream (Esc during streaming).
    Abort,
    /// Settings modal requested an apply — (key, value).
    SettingsApply(&'static str, String),
    /// Plugins modal emitted an outcome — handled in the async main loop
    /// because most variants perform async I/O (network, filesystem).
    PluginsOutcome(crate::plugins::InputOutcome),
}

/// Process a crossterm Event and return what the main loop should do.
pub(super) fn handle_event(
    event: Event,
    app: &mut App,
    runtime: &synaps_cli::Runtime,
    streaming: bool,
    registry: &Arc<CommandRegistry>,
) -> InputAction {
    // Route events to the settings modal while it's open.
    if app.settings.is_some() {
        if let Event::Key(key) = event {
            let snap = crate::settings::RuntimeSnapshot::from_runtime(runtime, registry);
            let state = app.settings.as_mut().expect("just checked");
            match crate::settings::handle_event(state, key, &snap) {
                crate::settings::InputOutcome::Close => { app.settings = None; }
                crate::settings::InputOutcome::None => {}
                crate::settings::InputOutcome::Apply { key, value } => {
                    return InputAction::SettingsApply(key, value);
                }
                crate::settings::InputOutcome::TogglePlugin { name, enabled } => {
                    let mut config = synaps_cli::config::load_config();
                    match crate::plugins::actions::toggle_plugin_config(
                        &name, enabled, &mut config, registry,
                    ) {
                        Ok(()) => {
                            state.row_error = None;
                        }
                        Err(e) => {
                            state.row_error = Some(("disabled_plugins".to_string(), e));
                        }
                    }
                }
            }
        }
        // Swallow all other events while settings is open.
        return InputAction::None;
    }
    // Route events to the plugins modal while it's open. Most outcomes run
    // async side-effects (fetch manifest, git clone, etc.), so we delegate
    // them to the main loop via `InputAction::PluginsOutcome`.
    if app.plugins.is_some() {
        if let Event::Key(key) = event {
            let state = app.plugins.as_mut().expect("just checked");
            let outcome = crate::plugins::handle_event(state, key);
            return match outcome {
                crate::plugins::InputOutcome::Close => {
                    app.plugins = None;
                    InputAction::None
                }
                crate::plugins::InputOutcome::None => InputAction::None,
                other => InputAction::PluginsOutcome(other),
            };
        }
        return InputAction::None;
    }
    match event {
        Event::Key(key) => handle_key(key.code, key.modifiers, app, streaming, registry),
        Event::Mouse(mouse) => {
            match mouse.kind {
                MouseEventKind::ScrollUp => {
                    app.scroll_back = app.scroll_back.saturating_add(3);
                    app.scroll_pinned = false;
                }
                MouseEventKind::ScrollDown => {
                    app.scroll_back = app.scroll_back.saturating_sub(3);
                    if app.scroll_back == 0 {
                        app.scroll_pinned = true;
                    }
                }
                _ => {}
            }
            InputAction::None
        }
        Event::Paste(text) => {
            const MAX_PASTE_CHARS: usize = 100_000;
            if !streaming || !app.input.is_empty() {
                let text = if text.chars().count() > MAX_PASTE_CHARS {
                    let truncated: String = text.chars().take(MAX_PASTE_CHARS).collect();
                    app.push_msg(ChatMessage::System(
                        format!("Paste truncated to {} chars (was {})", MAX_PASTE_CHARS, text.chars().count())
                    ));
                    truncated
                } else {
                    text
                };
                if app.input_before_paste.is_none() {
                    app.input_before_paste = Some(app.input.clone());
                }
                let byte_pos = app.cursor_byte_pos();
                app.input.insert_str(byte_pos, &text);
                app.cursor_pos += text.chars().count();
                app.pasted_char_count += text.chars().count();
            }
            InputAction::None
        }
        _ => InputAction::None,
    }
}

/// Handle a key event.
fn handle_key(
    code: KeyCode,
    modifiers: KeyModifiers,
    app: &mut App,
    streaming: bool,
    registry: &Arc<CommandRegistry>,
) -> InputAction {
    match (code, modifiers) {
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
            return InputAction::Quit;
        }
        (KeyCode::Esc, _) if streaming => {
            return InputAction::Abort;
        }
        (KeyCode::Enter, KeyModifiers::SHIFT) if !streaming => {
            let byte_pos = app.cursor_byte_pos();
            app.input.insert(byte_pos, '\n');
            app.cursor_pos += 1;
        }
        (KeyCode::Enter, _) if !streaming && !app.input.is_empty() => {
            return process_submit(app, registry);
        }
        (KeyCode::Enter, _) if streaming && !app.input.is_empty() => {
            return process_streaming_submit(app);
        }
        (KeyCode::Tab, _) if app.input.starts_with('/') && app.input.len() > 1 => {
            handle_tab_complete(app, registry);
        }
        // Cursor movement
        (KeyCode::Char('a'), KeyModifiers::CONTROL) => {
            app.cursor_pos = 0;
        }
        (KeyCode::Char('e'), KeyModifiers::CONTROL) => {
            app.cursor_pos = app.input.chars().count();
        }
        (KeyCode::Char('w'), KeyModifiers::CONTROL) | (KeyCode::Backspace, KeyModifiers::ALT) => {
            delete_word_backward(app);
        }
        (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
            app.input.clear();
            app.cursor_pos = 0;
        }
        (KeyCode::Home, _) => {
            app.cursor_pos = 0;
        }
        (KeyCode::End, _) => {
            app.cursor_pos = app.input.chars().count();
        }
        (KeyCode::Left, KeyModifiers::ALT) => {
            jump_word_left(app);
        }
        (KeyCode::Right, KeyModifiers::ALT) => {
            jump_word_right(app);
        }
        (KeyCode::Char('o'), KeyModifiers::CONTROL) => {
            app.show_full_output = !app.show_full_output;
            app.dirty = true;
            app.line_cache.clear();
        }
        (KeyCode::Char(c), _) => {
            let byte_pos = app.cursor_byte_pos();
            app.input.insert(byte_pos, c);
            app.cursor_pos += 1;
        }
        (KeyCode::Backspace, _) if app.cursor_pos > 0 => {
            app.cursor_pos -= 1;
            let byte_pos = app.cursor_byte_pos();
            app.input.remove(byte_pos);
        }
        (KeyCode::Left, _) if app.cursor_pos > 0 => {
            app.cursor_pos -= 1;
        }
        (KeyCode::Right, _) if app.cursor_pos < app.input_char_count() => {
            app.cursor_pos += 1;
        }
        (KeyCode::Up, KeyModifiers::SHIFT) => {
            app.scroll_back = app.scroll_back.saturating_add(1);
            app.scroll_pinned = false;
        }
        (KeyCode::Down, KeyModifiers::SHIFT) => {
            app.scroll_back = app.scroll_back.saturating_sub(1);
            if app.scroll_back == 0 {
                app.scroll_pinned = true;
            }
        }
        (KeyCode::Up, _) => {
            app.history_up();
        }
        (KeyCode::Down, _) => {
            app.history_down();
        }
        _ => {}
    }
    InputAction::None
}

/// User pressed Enter with non-empty input while not streaming.
fn process_submit(app: &mut App, registry: &Arc<CommandRegistry>) -> InputAction {
    if app.messages.is_empty() {
        app.logo_dismiss_t = Some(0.001);
    }
    let input = app.input.clone();
    app.input_history.push(input.clone());
    app.history_index = None;
    app.input_stash.clear();
    app.input.clear();
    app.cursor_pos = 0;
    app.scroll_back = 0;
    app.scroll_pinned = true;

    if input.starts_with('/') && input.len() > 1 {
        let parts: Vec<&str> = input[1..].splitn(2, ' ').collect();
        let raw_cmd = parts[0];
        let arg = parts.get(1).map(|s| s.trim()).unwrap_or("").to_string();
        let commands = super::commands::all_commands_with_skills(registry);
        let cmd = super::commands::resolve_prefix(raw_cmd, &commands);
        InputAction::SlashCommand(cmd, arg)
    } else {
        InputAction::Submit(input)
    }
}

/// User pressed Enter with non-empty input while streaming.
fn process_streaming_submit(app: &mut App) -> InputAction {
    let input = app.input.clone();
    app.input_history.push(input.clone());
    app.history_index = None;
    app.input_stash.clear();
    app.input.clear();
    app.cursor_pos = 0;
    app.input_before_paste = None;
    app.pasted_char_count = 0;

    InputAction::StreamingInput(input)
}

/// Tab completion for slash commands.
fn handle_tab_complete(app: &mut App, registry: &Arc<CommandRegistry>) {
    let partial = &app.input[1..];
    let commands = super::commands::all_commands_with_skills(registry);
    let matches: Vec<&String> = commands.iter()
        .filter(|c| c.starts_with(partial))
        .collect();
    if matches.len() == 1 {
        app.input = format!("/{}", matches[0]);
        app.cursor_pos = app.input.chars().count();
    } else if matches.len() > 1 {
        let first = matches[0];
        let common_len = (0..first.len())
            .take_while(|&i| matches.iter().all(|m| m.as_bytes().get(i) == first.as_bytes().get(i)))
            .count();
        if common_len > partial.len() {
            app.input = format!("/{}", &first[..common_len]);
            app.cursor_pos = app.input.chars().count();
        }
    }
}

/// Delete word backward (Ctrl+W / Alt+Backspace).
fn delete_word_backward(app: &mut App) {
    let chars: Vec<char> = app.input.chars().collect();
    let mut pos = app.cursor_pos;
    while pos > 0 && chars[pos - 1] == ' ' { pos -= 1; }
    while pos > 0 && chars[pos - 1] != ' ' { pos -= 1; }
    let byte_start = app.input.char_indices().nth(pos).map(|(i, _)| i).unwrap_or(app.input.len());
    let byte_end = app.cursor_byte_pos();
    app.input.drain(byte_start..byte_end);
    app.cursor_pos = pos;
}

/// Jump cursor one word left.
fn jump_word_left(app: &mut App) {
    let chars: Vec<char> = app.input.chars().collect();
    let mut pos = app.cursor_pos;
    while pos > 0 && chars[pos - 1] == ' ' { pos -= 1; }
    while pos > 0 && chars[pos - 1] != ' ' { pos -= 1; }
    app.cursor_pos = pos;
}

/// Jump cursor one word right.
fn jump_word_right(app: &mut App) {
    let chars: Vec<char> = app.input.chars().collect();
    let len = chars.len();
    let mut pos = app.cursor_pos;
    while pos < len && chars[pos] != ' ' { pos += 1; }
    while pos < len && chars[pos] == ' ' { pos += 1; }
    app.cursor_pos = pos;
}
