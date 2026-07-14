//! `/effort` lightbox — pick a reasoning effort level for the ACTIVE exact
//! model, on the fly. Options come from the shared engine derivation
//! (`settings::thinking_options_for_model`), so only levels the exact
//! provider-qualified model supports are ever shown. Application goes through
//! the existing checked mutation path (`Runtime::set_reasoning_level_checked`)
//! plus the existing `persist_to_config("thinking", …)` + session sync — the
//! lightbox never invents a new mutation/persistence route.
//!
//! Streaming safety: the open path is refused while a stream is active (both
//! by the generic non-streaming-command refusal and a defensive dispatch
//! guard), and [`apply_guard`] re-checks at apply time so a stream that
//! started *after* the lightbox opened can never race a mutation through.

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Padding, Paragraph},
    Frame,
};

use agent_core::reasoning::ReasoningLevel;

use super::lightbox::lightbox_safe_area;
use super::theme::THEME;

/// State of the `/effort` lightbox.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EffortModalState {
    /// Exact provider-qualified model the options were derived for.
    pub model: String,
    /// Valid effort levels for `model`, in derivation order.
    pub options: Vec<String>,
    pub cursor: usize,
}

/// What the event loop should do after a key in the effort lightbox.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum InputOutcome {
    /// Close without applying (Esc / q).
    Close,
    /// Apply the selected level string.
    Apply(String),
    None,
}

impl EffortModalState {
    /// Build for the active exact model; cursor starts on `current_level`
    /// when it is one of the valid options.
    pub fn new(model: &str, current_level: &str) -> Self {
        let options = super::settings::thinking_options_for_model(model);
        let cursor = options.iter().position(|o| o == current_level).unwrap_or(0);
        Self {
            model: model.to_string(),
            options,
            cursor,
        }
    }
}

/// Key handling: ↑/↓ move (clamped), Enter applies, Esc/q cancels.
pub(crate) fn handle_event(
    state: &mut EffortModalState,
    key: crossterm::event::KeyEvent,
) -> InputOutcome {
    use crossterm::event::KeyCode;
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => InputOutcome::Close,
        KeyCode::Up | KeyCode::Char('k') => {
            state.cursor = state.cursor.saturating_sub(1);
            InputOutcome::None
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if state.cursor + 1 < state.options.len() {
                state.cursor += 1;
            }
            InputOutcome::None
        }
        KeyCode::Enter => match state.options.get(state.cursor) {
            Some(level) => InputOutcome::Apply(level.clone()),
            None => InputOutcome::None,
        },
        _ => InputOutcome::None,
    }
}

/// Race-safe apply gate, checked at DISPATCH time (not open time): rejects
/// while streaming and re-validates the level against the exact model via
/// the shared mutation-time validation. `Err` = do not mutate, do not
/// persist. The runtime's own `set_reasoning_level_checked` remains the
/// authoritative (fail-closed) mutation gate after this.
pub(crate) fn apply_guard(
    streaming: bool,
    level_str: &str,
    model: &str,
) -> Result<ReasoningLevel, String> {
    if streaming {
        return Err(
            "/effort can't change levels while streaming — press Esc to cancel first".to_string(),
        );
    }
    let level = ReasoningLevel::parse(level_str)
        .ok_or_else(|| format!("unknown effort level '{level_str}'"))?;
    agent_engine::runtime::openai::catalog::validation::validate_reasoning_mutation(model, level)?;
    Ok(level)
}

/// Render the effort lightbox centered over the chat.
pub(crate) fn render(frame: &mut Frame, area: Rect, state: &EffortModalState) {
    let safe = lightbox_safe_area(area);
    let height = (state.options.len() as u16 + 4).min(safe.height);
    let width = 44u16.min(safe.width);
    let rect = Rect {
        x: safe.x + safe.width.saturating_sub(width) / 2,
        y: safe.y + safe.height.saturating_sub(height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, rect);
    let theme = THEME.load();
    let mut lines: Vec<Line> = Vec::with_capacity(state.options.len() + 1);
    for (i, opt) in state.options.iter().enumerate() {
        let selected = i == state.cursor;
        let style = if selected {
            Style::default()
                .fg(theme.claude_label)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.claude_text)
        };
        let marker = if selected { "▸ " } else { "  " };
        let label = if opt == "ultracode" {
            "ultracode — xhigh + workflows"
        } else {
            opt
        };
        lines.push(Line::from(Span::styled(format!("{marker}{label}"), style)));
    }
    lines.push(Line::from(Span::styled(
        "  ↑/↓ move · Enter apply · Esc cancel",
        Style::default().fg(theme.help_fg),
    )));
    let block = Block::default()
        .borders(Borders::ALL)
        .padding(Padding::horizontal(1))
        .title(format!(" Effort — {} ", state.model))
        .border_style(Style::default().fg(theme.claude_label));
    frame.render_widget(Paragraph::new(lines).block(block), rect);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    // ── Options: only valid levels for the exact model ─────────────────────

    #[test]
    fn options_are_exactly_the_valid_levels_for_exact_model() {
        // Codex sol supports the extended named set (incl. ultra).
        let sol = EffortModalState::new("openai-codex/gpt-5.6-sol", "medium");
        assert!(sol.options.contains(&"ultra".to_string()));
        assert_eq!(sol.options[0], "off");

        // xAI intrinsic-reasoning model: only the provider default.
        let grok = EffortModalState::new("xai-auth/grok-4.3", "adaptive");
        assert_eq!(grok.options, vec!["adaptive"]);

        // Fable exposes the exact Anthropic logical modes, without Codex ultra.
        let fable = EffortModalState::new("anthropic/claude-fable-5", "medium");
        assert_eq!(
            fable.options,
            [
                "off",
                "adaptive",
                "low",
                "medium",
                "high",
                "xhigh",
                "max",
                "ultracode"
            ]
        );
        assert!(!fable.options.contains(&"ultra".to_string()));
    }

    #[test]
    fn cursor_starts_on_current_level() {
        let state = EffortModalState::new("anthropic/claude-opus-4-7", "high");
        assert_eq!(state.options[state.cursor], "high");
        // Unknown current level falls back to the first option.
        let state = EffortModalState::new("anthropic/claude-opus-4-7", "bogus");
        assert_eq!(state.cursor, 0);
    }

    // ── Keys: Enter applies, Esc cancels, movement clamps ──────────────────

    #[test]
    fn enter_applies_selected_level_and_esc_cancels() {
        let mut state = EffortModalState::new("anthropic/claude-opus-4-7", "medium");
        assert_eq!(
            handle_event(&mut state, key(KeyCode::Enter)),
            InputOutcome::Apply("medium".to_string())
        );
        assert_eq!(
            handle_event(&mut state, key(KeyCode::Esc)),
            InputOutcome::Close
        );
    }

    #[test]
    fn movement_clamps_at_both_ends() {
        let mut state = EffortModalState::new("anthropic/claude-opus-4-7", "off");
        assert_eq!(state.cursor, 0);
        handle_event(&mut state, key(KeyCode::Up));
        assert_eq!(state.cursor, 0, "Up at top clamps");
        for _ in 0..100 {
            handle_event(&mut state, key(KeyCode::Down));
        }
        assert_eq!(
            state.cursor,
            state.options.len() - 1,
            "Down at bottom clamps"
        );
    }

    // ── Race-safe apply guard ───────────────────────────────────────────────

    #[test]
    fn apply_guard_rejects_while_streaming_even_for_valid_level() {
        let err = apply_guard(true, "medium", "anthropic/claude-opus-4-7").unwrap_err();
        assert!(err.contains("streaming"), "{err}");
    }

    #[test]
    fn apply_guard_rejects_unsupported_level_for_exact_model() {
        // luna does not support ultra — must fail closed, no mutation.
        assert!(apply_guard(false, "ultra", "openai-codex/gpt-5.6-luna").is_err());
        // Unknown strings are rejected, never coerced.
        assert!(apply_guard(false, "hyper", "anthropic/claude-opus-4-7").is_err());
    }

    #[test]
    fn apply_guard_accepts_valid_level_when_idle() {
        assert_eq!(
            apply_guard(false, "high", "anthropic/claude-opus-4-7"),
            Ok(agent_core::reasoning::ReasoningLevel::High)
        );
        assert_eq!(
            apply_guard(false, "ultra", "openai-codex/gpt-5.6-sol"),
            Ok(agent_core::reasoning::ReasoningLevel::Ultra)
        );
    }
}
