//! Input handling — keyboard events, cursor movement, paste, mouse scroll.

use std::sync::Arc;

use crossterm::event::{KeyCode, KeyModifiers, MouseEventKind, MouseButton, Event};
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
    PluginsOutcome(super::plugins::InputOutcome),
    /// Settings modal asked to open the plugins marketplace as a nested overlay.
    OpenPluginsMarketplace,
    PingModels,
}

/// Process a crossterm Event and return what the main loop should do.
pub(super) fn handle_event(
    event: Event,
    app: &mut App,
    runtime: &synaps_cli::Runtime,
    streaming: bool,
    registry: &Arc<CommandRegistry>,
) -> InputAction {
    // Route events to the plugins modal while it's open. Most outcomes run
    // async side-effects (fetch manifest, git clone, etc.), so we delegate
    // them to the main loop via `InputAction::PluginsOutcome`.
    if let Some(state) = app.plugins.as_mut() {
        if let Event::Key(key) = event {
            let outcome = super::plugins::handle_event(state, key);
            return match outcome {
                super::plugins::InputOutcome::Close => {
                    app.plugins = None;
                    InputAction::None
                }
                super::plugins::InputOutcome::None => InputAction::None,
                other => InputAction::PluginsOutcome(other),
            };
        }
        return InputAction::None;
    }
    // Route events to the settings modal while it's open.
    if let Some(state) = app.settings.as_mut() {
        // Handle paste into active editors (API key, text, custom model)
        if let Event::Paste(text) = event {
            match &mut state.edit_mode {
                Some(super::settings::ActiveEditor::ApiKey { buffer, .. }) => {
                    buffer.push_str(&text);
                }
                Some(super::settings::ActiveEditor::Text { buffer, .. }) => {
                    buffer.push_str(&text);
                }
                Some(super::settings::ActiveEditor::CustomModel { buffer, .. }) => {
                    buffer.push_str(&text);
                }
                _ => {}
            }
            return InputAction::None;
        }
        if let Event::Key(key) = event {
            let snap = super::settings::RuntimeSnapshot::from_runtime_with_health(runtime, registry, app.model_health.clone());
            match super::settings::handle_event(state, key, &snap) {
                super::settings::InputOutcome::Close => { app.settings = None; }
                super::settings::InputOutcome::None => {}
                super::settings::InputOutcome::Apply { key, value } => {
                    return InputAction::SettingsApply(key, value);
                }
                super::settings::InputOutcome::SetProviderKey { provider_id, value } => {
                    let cfg_key = format!("provider.{}", provider_id);
                    match synaps_cli::config::write_config_value(&cfg_key, &value) {
                        Ok(()) => {
                            state.edit_mode = None;
                            state.row_error = Some((cfg_key, "saved".to_string()));
                        }
                        Err(e) => {
                            state.row_error = Some((cfg_key, e.to_string()));
                        }
                    }
                }
                super::settings::InputOutcome::TogglePlugin { name, enabled } => {
                    let mut config = synaps_cli::config::load_config();
                    match super::plugins::actions::toggle_plugin_config(
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
                super::settings::InputOutcome::PreviewTheme { name } => {
                    if let Some(theme) = super::theme::load_theme_by_name(&name) {
                        super::theme::set_theme(theme);
                    }
                }
                super::settings::InputOutcome::RevertTheme => {
                    let theme = super::theme::load_theme_from_config();
                    super::theme::set_theme(theme);
                }
                super::settings::InputOutcome::OpenPluginsMarketplace => {
                    return InputAction::OpenPluginsMarketplace;
                }
                super::settings::InputOutcome::PingModels => {
                    return InputAction::PingModels;
                }
            }
        }
        // Swallow all other events while settings is open.
        return InputAction::None;
    }
    match event {
        Event::Key(key) => handle_key(key.code, key.modifiers, app, streaming, registry),
        Event::Mouse(mouse) => {
            handle_mouse(mouse, app)
        }
        Event::Paste(text) => {
            // Suppress paste events that fire immediately after a right-click copy.
            // Some terminals send both a Mouse(Down(Right)) AND an Event::Paste
            // when the user right-clicks, causing unintended paste into the input box.
            if app.suppress_next_paste {
                app.suppress_next_paste = false;
                return InputAction::None;
            }
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

/// Handle mouse events: scroll, text selection (left drag), right-click copy/paste.
fn handle_mouse(mouse: crossterm::event::MouseEvent, app: &mut App) -> InputAction {
    match mouse.kind {
        MouseEventKind::ScrollUp => {
            app.clear_selection();
            app.scroll_back = app.scroll_back.saturating_add(3);
            app.scroll_pinned = false;
        }
        MouseEventKind::ScrollDown => {
            app.clear_selection();
            app.scroll_back = app.scroll_back.saturating_sub(3);
            if app.scroll_back == 0 {
                app.scroll_pinned = true;
            }
        }

        // Left-click starts a new selection (clears any existing one)
        MouseEventKind::Down(MouseButton::Left) => {
            // Only start selection if click is inside the message area
            if is_in_msg_area(app, mouse.column, mouse.row) {
                app.selection_anchor = Some((mouse.column, mouse.row));
                app.selection_end = None;
            } else {
                app.clear_selection();
            }
        }

        // Left-drag extends the selection
        MouseEventKind::Drag(MouseButton::Left) => {
            if app.selection_anchor.is_some() {
                app.selection_end = Some((mouse.column, mouse.row));
            }
        }

        // Left-release finalizes the selection
        MouseEventKind::Up(MouseButton::Left) => {
            if let Some(anchor) = app.selection_anchor {
                let end = (mouse.column, mouse.row);
                // If start == end, it was a click not a drag — clear selection
                if anchor == end {
                    app.clear_selection();
                } else {
                    app.selection_end = Some(end);
                }
            }
        }

        // Right-click: copy if selection exists, paste if not
        MouseEventKind::Down(MouseButton::Right) => {
            if app.has_selection() {
                // Copy selected text to clipboard — right-click with selection is COPY ONLY
                if let Some(text) = app.selected_text() {
                    copy_to_clipboard(&text);
                    app.push_msg(ChatMessage::System(format!("Copied {} chars", text.chars().count())));
                }
                // Suppress any terminal-generated paste event that follows this right-click
                app.suppress_next_paste = true;
                // Clear selection after copy
                app.clear_selection();
            } else {
                // No selection — paste from clipboard at cursor position
                if let Some(text) = paste_from_clipboard() {
                    if !text.is_empty() {
                        if app.input_before_paste.is_none() {
                            app.input_before_paste = Some(app.input.clone());
                        }
                        let byte_pos = app.cursor_byte_pos();
                        app.input.insert_str(byte_pos, &text);
                        app.cursor_pos += text.chars().count();
                        app.pasted_char_count += text.chars().count();
                    }
                }
                // Suppress the terminal-generated paste event that follows this right-click
                app.suppress_next_paste = true;
            }
        }

        _ => {}
    }
    InputAction::None
}

/// Check if a terminal coordinate is inside the message area.
fn is_in_msg_area(app: &App, col: u16, row: u16) -> bool {
    if let Some(rect) = app.msg_area_rect {
        col >= rect.x && col < rect.x + rect.width
            && row >= rect.y + 1 && row < rect.y + rect.height.saturating_sub(1)
    } else {
        false
    }
}

/// Copy text to system clipboard. Spawns a background thread to hold
/// clipboard ownership long enough for clipboard managers to read it
/// (X11/Wayland require the owner process to serve selection requests).
fn copy_to_clipboard(text: &str) {
    let text = text.to_string();
    std::thread::spawn(move || {
        if let Ok(mut clipboard) = arboard::Clipboard::new() {
            let _ = clipboard.set_text(&text);
            // Hold ownership so clipboard managers / other apps can read it
            std::thread::sleep(std::time::Duration::from_secs(5));
        }
    });
}

/// Read text from system clipboard. Returns None if clipboard is empty or inaccessible.
fn paste_from_clipboard() -> Option<String> {
    if let Ok(mut clipboard) = arboard::Clipboard::new() {
        if let Ok(text) = clipboard.get_text() {
            if !text.is_empty() {
                return Some(text);
            }
        }
    }
    None
}

/// Handle a key event.
fn handle_key(
    code: KeyCode,
    modifiers: KeyModifiers,
    app: &mut App,
    streaming: bool,
    registry: &Arc<CommandRegistry>,
) -> InputAction {
    // Clear text selection on any keypress (typing dismisses selection)
    app.clear_selection();
    // Any non-Tab key resets the tab-completion cycle state. (Tab handler
    // below returns early after setting its own cycle state.)
    if !matches!(code, KeyCode::Tab) {
        app.tab_cycle = None;
    }
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
            // Skip the tab_cycle reset below — we just set it.
            return InputAction::None;
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
            app.invalidate();
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

/// Tab completion for slash commands. First Tab completes to the longest
/// common prefix; subsequent Tabs cycle through all matches. Falls back to
/// fuzzy matching when no prefix matches exist. Cycle state is cleared by
/// any non-Tab keypress (see input handler).
fn handle_tab_complete(app: &mut App, registry: &Arc<CommandRegistry>) {
    let commands = super::commands::all_commands_with_skills(registry);

    // If already cycling, advance to the next match.
    if let Some((ref prefix, idx, ref matching_cmds)) = app.tab_cycle.clone() {
        if matching_cmds.is_empty() {
            app.tab_cycle = None;
            return;
        }
        let next = (idx + 1) % matching_cmds.len();
        app.input = format!("/{}", matching_cmds[next]);
        app.cursor_pos = app.input.chars().count();
        app.tab_cycle = Some((prefix.clone(), next, matching_cmds.clone()));
        return;
    }

    // Fresh tab press — find matches for the current partial.
    let partial = app.input[1..].to_string();
    let matches: Vec<String> = commands.iter()
        .filter(|c| c.starts_with(partial.as_str()))
        .cloned()
        .collect();

    if matches.len() == 1 {
        app.input = format!("/{}", matches[0]);
        app.cursor_pos = app.input.chars().count();
        return;
    }

    if !matches.is_empty() {
        // Multiple prefix matches: first extend to longest common prefix; if that
        // didn't add anything new, start cycling through matches.
        let first = &matches[0];
        let common_len = (0..first.len())
            .take_while(|&i| matches.iter().all(|m| m.as_bytes().get(i) == first.as_bytes().get(i)))
            .count();

        if common_len > partial.len() {
            // Extend to common prefix — don't start cycling yet.
            app.input = format!("/{}", &first[..common_len]);
            app.cursor_pos = app.input.chars().count();
        } else {
            // Already at common prefix — start cycle from match[0].
            app.input = format!("/{}", matches[0]);
            app.cursor_pos = app.input.chars().count();
            app.tab_cycle = Some((partial, 0, matches));
        }
        return;
    }

    // No prefix matches — try fuzzy matching
    if let Some(fuzzy) = super::commands::fuzzy_match(&partial, &commands) {
        app.input = format!("/{}", fuzzy);
        app.cursor_pos = app.input.chars().count();
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
