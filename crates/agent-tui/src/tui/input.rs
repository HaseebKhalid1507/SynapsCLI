//! Input handling — keyboard events, cursor movement, paste, mouse scroll.

use std::sync::Arc;

use super::app::{App, ChatMessage};
use crossterm::event::{Event, KeyCode, KeyModifiers, MouseButton, MouseEventKind};
use synaps_cli::skills::registry::CommandRegistry;

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
    /// Models modal requested switching to a runtime model id.
    ModelsApply(String),
    /// Effort lightbox requested applying a reasoning level (string form).
    /// The dispatch arm re-checks streaming + exact-model validity
    /// (`effort::apply_guard`) before any mutation/persist.
    EffortApply(String),
    /// Models modal requested expanding provider models.
    ModelsExpandProvider(String),
    /// Plugins modal emitted an outcome — handled in the async main loop
    /// because most variants perform async I/O (network, filesystem).
    PluginsOutcome(super::plugins::InputOutcome),
    /// /help find lightbox emitted an outcome.
    HelpFindOutcome,
    /// Settings modal asked to open the plugins marketplace as a nested overlay.
    OpenPluginsMarketplace,
    PingModels,
    /// Open a plugin-owned custom settings editor via `settings.editor.open`.
    PluginEditorOpen {
        plugin_id: String,
        category: String,
        field: String,
    },
    /// Forward a keypress to the active plugin-owned custom settings editor.
    PluginEditorKey {
        plugin_id: String,
        category: String,
        field: String,
        key: crossterm::event::KeyEvent,
    },
}

/// Process a crossterm Event and return what the main loop should do.
///
/// P7.8: modal routing is now FULLY stack-driven. `handle_event` dispatches on
/// `modal_stack.top()`: the empty-stack `Chat` case is the base pane
/// (`handle_event_inner`), every modal has its own stack-routed pane handler,
/// and the async SecretPrompt is folded in (§5). The legacy if-let modal chain
/// is gone. BOTH the base path and every pane-handler path (including their pop
/// paths) converge on `action` so the debug-only stack/app sync tripwire always
/// runs afterwards.
pub(super) fn handle_event(
    event: Event,
    app: &mut App,
    runtime: &synaps_cli::Runtime,
    streaming: bool,
    registry: &Arc<CommandRegistry>,
    keybinds: &synaps_cli::skills::keybinds::KeybindRegistry,
    scroll_lines: u16,
) -> InputAction {
    // P7.8: stack-driven routing — one arm per pane, no fall-through chain.
    // `Chat` (empty stack) is the base pane; every modal + the folded-in
    // SecretPrompt has its own handler. The match is exhaustive over `PaneId`.
    let action = match app.modal_stack.top() {
        super::focus::PaneId::Chat => {
            handle_event_inner(event, app, streaming, registry, keybinds, scroll_lines)
        }
        super::focus::PaneId::HelpFind => route_help_find(event, app),
        super::focus::PaneId::Effort => route_effort(event, app),
        super::focus::PaneId::Models => route_models(event, app, runtime),
        super::focus::PaneId::Plugins => route_plugins(event, app),
        super::focus::PaneId::Settings => route_settings(event, app, runtime, registry),
        super::focus::PaneId::PluginEditor => route_settings(event, app, runtime, registry),
        super::focus::PaneId::SecretPrompt => route_secret_prompt(event, app),
    };

    // Stack/app-field sync tripwire (§3 contract 4) — debug/test builds only.
    // Runs after BOTH the chain path and the pane-handler pop path, so a missed
    // push/pop fails the harness loudly instead of misrouting silently.
    #[cfg(debug_assertions)]
    super::focus::debug_assert_stack_sync(app);

    action
}

/// Base Chat pane handler: keyboard / mouse / paste for the transcript + input
/// box. Reached from `handle_event` when `modal_stack.top() == Chat` (empty
/// stack). All modals — and the async secret prompt — are stack-routed via
/// their own pane handlers (P7.8); the legacy if-let modal chain no longer
/// exists, so `runtime` (its sole ex-consumer was the settings arm, now in
/// `route_settings`) is no longer threaded here.
fn handle_event_inner(
    event: Event,
    app: &mut App,
    streaming: bool,
    registry: &Arc<CommandRegistry>,
    keybinds: &synaps_cli::skills::keybinds::KeybindRegistry,
    scroll_lines: u16,
) -> InputAction {
    match event {
        Event::Key(key) => handle_key(key.code, key.modifiers, app, streaming, registry, keybinds),
        Event::Mouse(mouse) => handle_mouse(mouse, app, scroll_lines),
        Event::Paste(text) => {
            // Suppress paste events that fire immediately after a right-click copy.
            // Some terminals send both a Mouse(Down(Right)) AND an Event::Paste
            // when the user right-clicks, causing unintended paste into the input box.
            if let Some(deadline) = app.suppress_paste_until {
                if app.clock.now() < deadline {
                    app.suppress_paste_until = None;
                    return InputAction::None;
                }
                app.suppress_paste_until = None;
            }
            const MAX_PASTE_CHARS: usize = 100_000;
            // Paste is allowed regardless of streaming state — typing into the
            // input during streaming already works (queued submit), so paste
            // must behave the same. Previously gated on !streaming, which
            // silently ate pastes into an empty input mid-stream.
            let text = if text.chars().count() > MAX_PASTE_CHARS {
                let truncated: String = text.chars().take(MAX_PASTE_CHARS).collect();
                app.push_msg(ChatMessage::System(format!(
                    "Paste truncated to {} chars (was {})",
                    MAX_PASTE_CHARS,
                    text.chars().count()
                )));
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
            InputAction::None
        }
        _ => InputAction::None,
    }
}

/// Handle mouse events: scroll, text selection (left drag), right-click copy/paste.
fn handle_mouse(
    mouse: crossterm::event::MouseEvent,
    app: &mut App,
    scroll_lines: u16,
) -> InputAction {
    // Selection events may need to promote a demoted slot on demand (P11
    // lock L3: wheel + click in one input batch maps into rows demoted as of
    // the last frame). The re-render crosses the seam via RenderCtx, same as
    // the draw path — field borrows are disjoint from `app.transcript`.
    let ctx = super::transcript::RenderCtx {
        spinner_frame: app.spinner_frame,
        streaming: app.streaming,
        agent_name: &app.agent_name,
    };
    match mouse.kind {
        // Wheel scroll no longer clears the selection (P10 lock L4):
        // endpoints are content-relative, so the selection scrolls with the
        // content and the highlight clamps to the window. Keypresses still
        // clear (handle_key top) — including Shift+Up/Down keyboard scroll.
        MouseEventKind::ScrollUp => {
            app.transcript.scroll_up(scroll_lines);
        }
        MouseEventKind::ScrollDown => {
            app.transcript.scroll_down(scroll_lines);
        }

        // Left-click starts a new selection (clears any existing one)
        MouseEventKind::Down(MouseButton::Left) => {
            // Only start selection if click is inside the message area
            if app.transcript.hit_test(mouse.column, mouse.row) {
                app.transcript
                    .selection_begin(mouse.column, mouse.row, &ctx);
            } else {
                app.transcript.clear_selection();
            }
        }

        // Left-drag extends the selection (no-op without an anchor)
        MouseEventKind::Drag(MouseButton::Left) => {
            app.transcript.selection_drag(mouse.column, mouse.row, &ctx);
        }

        // Left-release finalizes the selection (click == anchor ⇒ clear)
        MouseEventKind::Up(MouseButton::Left) => {
            app.transcript
                .selection_release(mouse.column, mouse.row, &ctx);
        }

        // Right-click: copy if selection exists, paste if not
        MouseEventKind::Down(MouseButton::Right) => {
            if app.transcript.has_selection() {
                // Copy selected text to clipboard — right-click with selection is COPY ONLY
                if let Some(text) = app.transcript.selected_text() {
                    copy_to_clipboard(&text);
                    app.push_msg(ChatMessage::System(format!(
                        "Copied {} chars",
                        text.chars().count()
                    )));
                }
                // Suppress any terminal-generated paste event that follows this right-click
                app.suppress_paste_until =
                    Some(app.clock.now() + std::time::Duration::from_millis(150));
                // Clear selection after copy
                app.transcript.clear_selection();
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
                app.suppress_paste_until =
                    Some(app.clock.now() + std::time::Duration::from_millis(150));
            }
        }

        _ => {}
    }
    InputAction::None
}

/// Copy text to system clipboard. Uses a singleton background thread that
/// holds one clipboard handle for the lifetime of the app. New copies replace
/// the previous content atomically — no thread accumulation, no races.
fn copy_to_clipboard(text: &str) {
    use std::sync::{mpsc, OnceLock};
    static TX: OnceLock<mpsc::Sender<String>> = OnceLock::new();
    let sender = TX.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            let Ok(mut clipboard) = arboard::Clipboard::new() else {
                return;
            };
            while let Ok(text) = rx.recv() {
                let _ = clipboard.set_text(&text);
            }
        });
        tx
    });
    let _ = sender.send(text.to_string());
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
    keybinds: &synaps_cli::skills::keybinds::KeybindRegistry,
) -> InputAction {
    // Clear text selection on any keypress (typing dismisses selection)
    app.transcript.clear_selection();
    // Any non-Tab key resets the tab-completion cycle state. (Tab handler
    // below returns early after setting its own cycle state.)
    if !matches!(code, KeyCode::Tab) {
        app.tab_cycle = None;
    }

    // Plugin/user keybinds — check before core binds, but only when not streaming
    if !streaming {
        // Trace modifier-heavy or special keys to help debug "key X not
        // working" issues across terminals. Plain typing isn't logged.
        let traceable = matches!(code, KeyCode::F(_))
            || modifiers.contains(KeyModifiers::CONTROL)
            || modifiers.contains(KeyModifiers::ALT);
        if traceable {
            tracing::info!(?code, ?modifiers, "key event received in chatui input");
        }
        if let Some(bind) = keybinds.match_key(code, modifiers) {
            use synaps_cli::skills::keybinds::KeybindAction;
            return match &bind.action {
                KeybindAction::SlashCommand(cmd) => {
                    let parts: Vec<&str> = cmd.splitn(2, ' ').collect();
                    let resolved = super::commands::resolve_prefix(
                        parts[0],
                        &super::commands::all_commands_with_skills(registry),
                    );
                    InputAction::SlashCommand(resolved, parts.get(1).unwrap_or(&"").to_string())
                }
                KeybindAction::LoadSkill(skill) => {
                    InputAction::SlashCommand("load".to_string(), skill.clone())
                }
                KeybindAction::InjectPrompt(text) => InputAction::Submit(text.clone()),
                KeybindAction::Disabled => InputAction::None,
                KeybindAction::RunScript { .. } => {
                    // TODO: execute script and inject output
                    InputAction::None
                }
            };
        }
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
            if open_help_find_for_ambiguous_slash(app, registry) {
                return InputAction::HelpFindOutcome;
            }
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
            // Store-owned toggle invalidates internally (locked decision #1);
            // App only signals the frame scheduler.
            app.transcript
                .set_show_full_output(!app.transcript.show_full_output());
            app.request_redraw();
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
            app.transcript.scroll_up(1);
        }
        (KeyCode::Down, KeyModifiers::SHIFT) => {
            app.transcript.scroll_down(1);
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
    if app.transcript.is_empty() {
        app.logo_dismiss_t = Some(0.001);
    }
    let input = app.input.clone();
    app.input_history.push(input.clone());
    app.history_index = None;
    app.input_stash.clear();
    app.input.clear();
    app.cursor_pos = 0;
    app.transcript.scroll_to_bottom();

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

/// P7.4 stack-routed pane handler for the `/help find` lightbox.
///
/// Performs exactly what the deleted legacy chain arm did (byte-identical
/// dispatch to `help_find::handle_event`), and additionally POPS the modal
/// stack when the lightbox closes — keeping `modal_stack.contains(HelpFind)`
/// in sync with `app.help_find.is_some()` (asserted by
/// `debug_assert_stack_sync`). Returns the loop's existing `InputAction`
/// directly: help_find never coexists with another modal (§6), so no
/// `PaneOutcome` indirection is required.
fn route_help_find(event: Event, app: &mut App) -> InputAction {
    // Invariant (checked by the tripwire): top() == HelpFind ⇒ help_find is Some.
    let Some(state) = &mut app.help_find else {
        return InputAction::None;
    };
    if let Event::Key(key) = event {
        return match super::help_find::handle_event(state, key) {
            super::help_find::HelpFindAction::Close => {
                app.help_find = None;
                app.modal_stack.pop();
                InputAction::None
            }
            super::help_find::HelpFindAction::None => InputAction::HelpFindOutcome,
        };
    }
    InputAction::None
}

/// Stack-routed pane handler for the `/effort` lightbox.
///
/// Outcome translation mirrors `route_models`: `Close` → clear field + pop
/// (cancel — nothing applied); `Apply` → clear field + pop, then defer the
/// guarded apply to the async loop (`InputAction::EffortApply`); `None` →
/// consumed. The modal closes on BOTH paths, so the race-safe apply decision
/// lives entirely in the dispatch guard.
fn route_effort(event: Event, app: &mut App) -> InputAction {
    // Invariant (checked by the tripwire): top() == Effort ⇒ effort is Some.
    let Some(state) = &mut app.effort else {
        return InputAction::None;
    };
    if let Event::Key(key) = event {
        return match super::effort::handle_event(state, key) {
            super::effort::InputOutcome::Close => {
                app.effort = None;
                app.modal_stack.pop();
                InputAction::None
            }
            super::effort::InputOutcome::Apply(level) => {
                app.effort = None;
                app.modal_stack.pop();
                InputAction::EffortApply(level)
            }
            super::effort::InputOutcome::None => InputAction::None,
        };
    }
    InputAction::None
}

/// P7.5 stack-routed pane handler for the `/model` · `/models` modal.
///
/// Performs exactly what the deleted legacy chain arm did (byte-identical
/// dispatch to `models::handle_event`), and additionally POPS the modal stack
/// on the two close paths — keeping `modal_stack.contains(Models)` in sync
/// with `app.models.is_some()` (asserted by `debug_assert_stack_sync`).
///
/// Outcome translation (§7 P7.5): `Close` → `PaneOutcome::Pop` (clear the
/// field and pop, return `None`); `Apply` → `PaneOutcome::PopThen(ModelsApply)`
/// (clear the field and pop, then defer the apply to the async loop);
/// `ExpandProvider` → `PaneOutcome::Action(ModelsExpandProvider)` (defer only,
/// modal stays open, no pop); `None` → `Consumed`. Models never coexists with
/// another modal (§6), so the `InputAction` is returned directly — the
/// `PaneOutcome` mapping above is realized inline, matching the P7.4
/// `route_help_find` shape.
fn route_models(event: Event, app: &mut App, runtime: &synaps_cli::Runtime) -> InputAction {
    // Invariant (checked by the tripwire): top() == Models ⇒ models is Some.
    let Some(state) = &mut app.models else {
        return InputAction::None;
    };
    if let Event::Key(key) = event {
        return match super::models::handle_event(state, key, runtime.model()) {
            super::models::InputOutcome::Close => {
                app.models = None;
                app.modal_stack.pop();
                InputAction::None
            }
            super::models::InputOutcome::Apply(model) => {
                app.models = None;
                app.modal_stack.pop();
                InputAction::ModelsApply(model)
            }
            super::models::InputOutcome::None => InputAction::None,
            super::models::InputOutcome::ExpandProvider(provider) => {
                InputAction::ModelsExpandProvider(provider)
            }
        };
    }
    InputAction::None
}

/// P7.6 stack-routed pane handler for the `/plugins` marketplace modal.
///
/// Performs exactly what the deleted legacy chain arm did (byte-identical
/// dispatch to `plugins::handle_event`), and additionally POPS the modal stack
/// on the sole close path — keeping `modal_stack.contains(Plugins)` in sync
/// with `app.plugins.is_some()` (asserted by `debug_assert_stack_sync`).
///
/// Outcome translation (§7 P7.6): `Close` → `PaneOutcome::Pop` (clear the field
/// and pop, return `None`); `None` → `Consumed`; every OTHER `InputOutcome`
/// (AddMarketplace, InstallRequested, Uninstall, …) → `PaneOutcome::Action`,
/// i.e. deferred to the async loop verbatim via `InputAction::PluginsOutcome`,
/// leaving the modal open (no pop). The `PaneOutcome` mapping is realized
/// inline, matching the P7.4/P7.5 `route_help_find` / `route_models` shape.
///
/// Depth subtlety (post-P7.7): when plugins is opened from an already-open
/// settings modal (marketplace-from-settings), settings is ITSELF a stack
/// member — so the stack is a real two-deep `[Settings, Plugins]`, with Plugins
/// on top. This `Close` path pops Plugins (`app.plugins = None` + pop) back to
/// `[Settings]`, and `route_settings` resumes routing the still-open settings
/// modal. The chain no longer exists; the fall-back is the stack level below.
fn route_plugins(event: Event, app: &mut App) -> InputAction {
    // Invariant (checked by the tripwire): top() == Plugins ⇒ plugins is Some.
    let Some(state) = &mut app.plugins else {
        return InputAction::None;
    };
    if let Event::Key(key) = event {
        return match super::plugins::handle_event(state, key) {
            super::plugins::InputOutcome::Close => {
                app.plugins = None;
                app.modal_stack.pop();
                InputAction::None
            }
            super::plugins::InputOutcome::None => InputAction::None,
            other => InputAction::PluginsOutcome(other),
        };
    }
    InputAction::None
}

/// P7.7 stack-routed pane handler for the `/settings` modal AND its nested
/// `PaneId::PluginEditor` (the `ActiveEditor::PluginCustom` editor promoted to
/// a real second stack level). BOTH `PaneId::Settings` and `PaneId::PluginEditor`
/// dispatch here: when a PluginCustom editor is active it is `top()` and this
/// handler's first block forwards keys to it (Esc pops PluginEditor); otherwise
/// Settings is `top()` and the normal settings handling runs.
///
/// The 12 `InputOutcome` match arms — INCLUDING the synchronous config-file
/// writes (`write_plugin_config`, `write_config_value`, `toggle_plugin_config`)
/// and the theme preview/revert side-effects — are moved VERBATIM from the
/// deleted legacy chain arm. They execute at exactly the same point, in the same
/// order, as before: nothing is reordered, deferred, or hoisted. The ONLY
/// additions are the two `app.modal_stack.pop()` calls that mirror the two
/// `= None` close assignments (`app.settings = None` on `Close`; `edit_mode =
/// None` on the PluginCustom Esc) — keeping the stack in sync with the fields
/// (asserted by `debug_assert_stack_sync`). `app.modal_stack` is a field
/// disjoint from `app.settings`, so the pops borrow-check beside the live
/// `state` borrow exactly as the existing `app.model_health.clone()` does.
fn route_settings(
    event: Event,
    app: &mut App,
    runtime: &synaps_cli::Runtime,
    registry: &Arc<CommandRegistry>,
) -> InputAction {
    // Invariant (checked by the tripwire): top() == Settings | PluginEditor ⇒
    // settings is Some.
    if let Some(state) = &mut app.settings {
        if let Some(super::settings::ActiveEditor::PluginCustom {
            plugin_id,
            category,
            field,
            ..
        }) = &state.edit_mode
        {
            if let Event::Key(key) = event {
                if key.code == KeyCode::Esc {
                    // P7.7: Esc on the nested PluginCustom editor POPS
                    // PaneId::PluginEditor — clears edit_mode while Settings stays
                    // open, byte-identical to the pre-migration chain behaviour.
                    state.edit_mode = None;
                    app.modal_stack.pop();
                    return InputAction::None;
                }
                return InputAction::PluginEditorKey {
                    plugin_id: plugin_id.clone(),
                    category: category.clone(),
                    field: field.clone(),
                    key,
                };
            }
            return InputAction::None;
        }
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
            let mut snap = super::settings::RuntimeSnapshot::from_runtime_with_health(
                runtime,
                registry,
                app.model_health.clone(),
            );
            snap.catalog_overrides = app.catalog_overrides.clone();
            match super::settings::handle_event(state, key, &snap) {
                super::settings::InputOutcome::Close => {
                    app.settings = None;
                    app.modal_stack.pop();
                }
                super::settings::InputOutcome::None => {}
                super::settings::InputOutcome::Apply { key, value } => {
                    return InputAction::SettingsApply(key, value);
                }
                super::settings::InputOutcome::PluginApply {
                    plugin_id,
                    key,
                    value,
                } => {
                    let row_key = format!("plugin.{}.{}", plugin_id, key);
                    match synaps_cli::extensions::config_store::write_plugin_config(
                        &plugin_id, &key, &value,
                    ) {
                        Ok(()) => {
                            state.edit_mode = None;
                            state.row_error = Some((row_key, "saved".to_string()));
                        }
                        Err(e) => {
                            state.row_error = Some((row_key, e.to_string()));
                        }
                    }
                }
                super::settings::InputOutcome::PluginCustomOpen {
                    plugin_id,
                    category,
                    key,
                } => {
                    return InputAction::PluginEditorOpen {
                        plugin_id,
                        category,
                        field: key,
                    };
                }
                super::settings::InputOutcome::SetProviderKey { provider_id, value } => {
                    // `local.url` is non-secret endpoint configuration and
                    // stays in the config file. Actual API keys are
                    // broker-owned: persist them into the broker credential
                    // store instead of plaintext config.
                    let result = if provider_id == "local.url" {
                        synaps_cli::config::write_config_value(
                            &format!("provider.{}", provider_id),
                            &value,
                        )
                        .map_err(|e| e.to_string())
                    } else {
                        synaps_cli::auth::save_static_key(&provider_id, &value)
                    };
                    let row_key = format!("provider.{}", provider_id);
                    match result {
                        Ok(()) => {
                            state.edit_mode = None;
                            state.row_error = Some((row_key, "saved".to_string()));
                        }
                        Err(e) => {
                            state.row_error = Some((row_key, e));
                        }
                    }
                }
                super::settings::InputOutcome::TogglePlugin { name, enabled } => {
                    let mut config = synaps_cli::config::load_config();
                    match super::plugins::actions::toggle_plugin_config(
                        &name,
                        enabled,
                        &mut config,
                        registry,
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
    InputAction::None
}

/// P7.8: reconcile the SecretPrompt pane against the async queue (§5).
///
/// The secret-prompt queue is NOT user-opened: tools deep in the engine send
/// requests over an mpsc channel, and `submit()` / `cancel()` auto-activate the
/// next queued prompt — so activation and deactivation both happen OUTSIDE any
/// input event. Rather than couple push/pop to a keypress, this reconciles the
/// stack to the queue's `is_active()`: push when active but absent, remove when
/// inactive but present. Called (a) after `poll_requests` in the tick arm
/// (`mod.rs`), and (b) after every `submit()` / `cancel()` in the pane handler.
/// Queue chaining is handled for free — the pane stays on the stack while
/// `is_active()` remains true across consecutive prompts.
pub(super) fn reconcile_secret_prompt(app: &mut App) {
    let active = app.secret_prompts.is_active();
    let on_stack = app.modal_stack.contains(super::focus::PaneId::SecretPrompt);
    match (active, on_stack) {
        (true, false) => app.modal_stack.push(super::focus::PaneId::SecretPrompt),
        (false, true) => app.modal_stack.remove(super::focus::PaneId::SecretPrompt),
        _ => {}
    }
}

/// P7.8 stack-routed pane handler for the async secret / masked-input prompt.
///
/// Reproduces the former inline `mod.rs` interception VERBATIM: Enter submits,
/// Esc cancels, Backspace deletes, Char / per-char Paste append, everything
/// else is swallowed (`PaneOutcome::Consumed`). After `submit()` / `cancel()`
/// (which may auto-activate the next queued prompt) it reconciles the stack so
/// the pane stays while another prompt is pending and pops when the queue
/// drains. Returns `InputAction::None`; the former inline `app.request_redraw()`
/// is preserved by `request_immediate_redraw` on the input path (`mod.rs`).
fn route_secret_prompt(event: Event, app: &mut App) -> InputAction {
    match event {
        Event::Key(key) => match key.code {
            KeyCode::Enter => {
                app.secret_prompts.submit();
                reconcile_secret_prompt(app);
            }
            KeyCode::Esc => {
                app.secret_prompts.cancel();
                reconcile_secret_prompt(app);
            }
            KeyCode::Backspace => app.secret_prompts.backspace(),
            KeyCode::Char(c) => app.secret_prompts.push_char(c),
            _ => {}
        },
        Event::Paste(text) => {
            for ch in text.chars() {
                app.secret_prompts.push_char(ch);
            }
        }
        _ => {}
    }
    InputAction::None
}

fn open_help_find_for_ambiguous_slash(app: &mut App, registry: &Arc<CommandRegistry>) -> bool {
    let Some(query) = synaps_cli::help::prefilter_query_for_slash_command(&app.input) else {
        return false;
    };
    let help_registry = synaps_cli::help::HelpRegistry::new(
        synaps_cli::help::builtin_entries(),
        registry.plugin_help_entries(),
    );
    if help_registry.command_prefix_match_count(&query) < 2 {
        return false;
    }
    app.help_find = Some(synaps_cli::help::HelpFindState::new(
        help_registry.entries().to_vec(),
        &query,
    ));
    // P7.4: mirror the `= Some(..)` open with a stack push (§6: push accompanies
    // every open of a migrated modal). help_find never coexists, so this is a
    // depth 0 → 1 push.
    app.modal_stack.push(super::focus::PaneId::HelpFind);
    true
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
    let matches: Vec<String> = commands
        .iter()
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
            .take_while(|&i| {
                matches
                    .iter()
                    .all(|m| m.as_bytes().get(i) == first.as_bytes().get(i))
            })
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
    while pos > 0 && chars[pos - 1] == ' ' {
        pos -= 1;
    }
    while pos > 0 && chars[pos - 1] != ' ' {
        pos -= 1;
    }
    let byte_start = app
        .input
        .char_indices()
        .nth(pos)
        .map(|(i, _)| i)
        .unwrap_or(app.input.len());
    let byte_end = app.cursor_byte_pos();
    app.input.drain(byte_start..byte_end);
    app.cursor_pos = pos;
}

/// Jump cursor one word left.
fn jump_word_left(app: &mut App) {
    let chars: Vec<char> = app.input.chars().collect();
    let mut pos = app.cursor_pos;
    while pos > 0 && chars[pos - 1] == ' ' {
        pos -= 1;
    }
    while pos > 0 && chars[pos - 1] != ' ' {
        pos -= 1;
    }
    app.cursor_pos = pos;
}

/// Jump cursor one word right.
fn jump_word_right(app: &mut App) {
    let chars: Vec<char> = app.input.chars().collect();
    let len = chars.len();
    let mut pos = app.cursor_pos;
    while pos < len && chars[pos] != ' ' {
        pos += 1;
    }
    while pos < len && chars[pos] == ' ' {
        pos += 1;
    }
    app.cursor_pos = pos;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyModifiers, MouseEvent, MouseEventKind};
    use synaps_cli::Session;

    fn models_key(code: KeyCode) -> Event {
        Event::Key(crossterm::event::KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn routed_models_search_navigation_applies_visible_provider_qualified_id() {
        use crate::tui::focus::PaneId;
        use crate::tui::models::{
            ExpandedLoadState, ExpandedModelEntry, ExpandedModelsState, ModelsModalState,
        };
        let runtime = synaps_cli::Runtime::new_headless();
        let mut app = make_app();
        let mut models = ModelsModalState::new();
        models.expanded = Some(ExpandedModelsState {
            provider_key: "github-copilot".into(),
            provider_name: "GitHub Copilot".into(),
            cursor: 0,
            search: String::new(),
            load_state: ExpandedLoadState::Ready(vec![
                ExpandedModelEntry::new(
                    "github-copilot/claude-opus-4.7".into(),
                    "Opus 4.7".into(),
                    false,
                ),
                ExpandedModelEntry::new(
                    "github-copilot/claude-opus-4.8".into(),
                    "Opus 4.8".into(),
                    false,
                ),
            ]),
        });
        app.models = Some(models);
        app.modal_stack.push(PaneId::Models);
        for ch in "opus 4".chars() {
            assert!(matches!(
                route_models(models_key(KeyCode::Char(ch)), &mut app, &runtime),
                InputAction::None
            ));
        }
        assert!(matches!(
            route_models(models_key(KeyCode::Down), &mut app, &runtime),
            InputAction::None
        ));
        match route_models(models_key(KeyCode::Enter), &mut app, &runtime) {
            InputAction::ModelsApply(id) => assert_eq!(id, "github-copilot/claude-opus-4.8"),
            _ => panic!("Enter did not apply selected visible model"),
        }
        assert!(app.models.is_none());
        assert_eq!(app.modal_stack.top(), PaneId::Chat);
    }

    fn make_app() -> App {
        App::new(Session::new("test-model", "low", None))
    }

    fn scroll_event(kind: MouseEventKind) -> crossterm::event::MouseEvent {
        MouseEvent {
            kind,
            column: 5,
            row: 5,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// When scroll_lines=5 is configured, one ScrollUp event must add exactly 5
    /// to scroll_back (not the hardcoded 3).
    #[test]
    fn scroll_up_uses_configured_step() {
        let mut app = make_app();
        app.transcript.test_set_scroll_back(0);
        handle_mouse(scroll_event(MouseEventKind::ScrollUp), &mut app, 5);
        assert_eq!(
            app.transcript.scroll_back_pos(),
            5,
            "scroll_back should be 5 with scroll_lines=5"
        );
    }

    /// When scroll_lines=5 is configured, one ScrollDown event must subtract 5
    /// (clamped to 0) from scroll_back.
    #[test]
    fn scroll_down_uses_configured_step() {
        let mut app = make_app();
        app.transcript.test_set_scroll_back(10);
        handle_mouse(scroll_event(MouseEventKind::ScrollDown), &mut app, 5);
        assert_eq!(
            app.transcript.scroll_back_pos(),
            5,
            "scroll_back should decrease by 5"
        );
    }

    /// When scroll_lines is absent (None) the caller passes the default of 3.
    /// Verify the default of 3 is what the caller should use.
    #[test]
    fn scroll_default_step_is_3() {
        let cfg = synaps_cli::config::SynapsConfig::default();
        let step = cfg.scroll_lines.unwrap_or(3);
        assert_eq!(step, 3, "default scroll step must be 3 when unconfigured");
    }

    /// Configured value is read through SynapsConfig::scroll_lines.
    #[test]
    fn scroll_configured_step_is_used_via_config() {
        let cfg = synaps_cli::config::SynapsConfig {
            scroll_lines: Some(7),
            ..Default::default()
        };
        let step = cfg.scroll_lines.unwrap_or(3);
        assert_eq!(step, 7);
    }
}
