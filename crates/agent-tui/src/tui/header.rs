//! The Ghost header.
//!
//! One quiet row:
//!
//! ```text
//!   synaps  ○ voice           fix the cache thing                  0:42  ⎼⎺⎺⎺⎼⎽
//! ```
//!
//! * The wordmark sleeps (dim) while the session is idle, shimmers while it
//!   works, and takes the error colour after a failed turn.
//! * Sidecar pills follow the wordmark, as before.
//! * The session's name (or its auto title, or its id) sits in the centre.
//! * Idle, the right side shows the build (`v0.10.2 · ae490b32`), dim; it
//!   gives way to the session label when the row is tight. Otherwise it is
//!   a short label (turn clock, `done/total` workers, `retrying`, …)
//!   followed by an indicator whose *shape* names the state (see
//!   [`Indicator::for_state`]), in a fixed-width slot so the label never
//!   moves when the indicator changes.
//!
//! The message pane's top border is drawn as an upper half-block in the
//! header colour ([`style_top_edge`]), so the bar reads 1½ rows tall while
//! the layout, and every coordinate derived from it, stays as it was.

use ratatui::{
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    symbols::border,
    text::{Line, Span},
    widgets::{Block, Paragraph},
    Frame,
};

use super::render_model::{RenderModel, SubagentSnap};
use super::text_metrics::{char_width, width as display_width};
use super::theme::{background_is_opaque, Theme, THEME};

// ─── state ──────────────────────────────────────────────────────────────────

/// What the current turn is doing. Tracked on `App` by the stream handler;
/// shipped to the render side in [`HeaderSnap`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum Activity {
    #[default]
    Idle,
    /// Request sent (or tool results returned); no first token yet.
    Waiting,
    Thinking,
    /// Text or tool-call input streaming out.
    Writing,
    /// Tool calls dispatched, not all results back.
    Tool,
    /// The provider pushed back (429/5xx/refusal); a retry is pending.
    Retry,
    /// The last turn ended in an error. Cleared when the next turn starts.
    Failed(String),
}

/// The header's slice of the per-frame snapshot.
#[derive(Clone, Debug, Default)]
pub(crate) struct HeaderSnap {
    pub(crate) activity: Activity,
    /// Session name, else its auto title, else its id. Already sanitised.
    pub(crate) session_label: String,
    /// Seconds since the current turn started (`None` when no turn runs).
    pub(crate) turn_elapsed_secs: Option<u64>,
    /// Monotonic animation clock in milliseconds (wall clock, so the
    /// indicators keep their speed however busy the event loop is).
    pub(crate) anim_ms: u64,
}

/// What the header shows, derived per frame from the snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HeaderState {
    Idle,
    Connecting,
    Thinking,
    Streaming,
    Tool,
    /// The main turn is idle and workers are running. `bars[i]` is true
    /// while that worker runs (at most [`MAX_BARS`]).
    Agents {
        done: usize,
        total: usize,
        bars: Vec<bool>,
    },
    Retry,
    /// A transient status the header can only name: compacting,
    /// reconnecting, aborting.
    Busy(String),
    Failed(String),
}

impl HeaderState {
    fn is_live(&self) -> bool {
        !matches!(self, HeaderState::Idle | HeaderState::Failed(_))
    }
}

/// Most bars the worker equalizer draws.
pub(crate) const MAX_BARS: usize = 6;

/// Columns reserved for the indicator: the widest one (scanner, scope, a
/// full equalizer). Indicators are left-aligned in it, so the label to
/// their left (turn clock, `1/3`, `retrying`, `✗ 401`) keeps one right
/// edge whichever indicator, or none, is drawing.
pub(crate) const INDICATOR_SLOT: usize = 6;

/// Derive the header state. `status_text` wins (it is the explicit
/// connecting/compacting/reconnecting/aborting signal); then the streaming
/// turn's activity; then running workers; then the last turn's failure.
pub(crate) fn header_state(
    streaming: bool,
    status_text: Option<&str>,
    activity: &Activity,
    subagents: &[SubagentSnap],
) -> HeaderState {
    if let Some(text) = status_text {
        let word = text.split('\u{2026}').next().unwrap_or(text).trim();
        if word.eq_ignore_ascii_case("connecting") {
            return HeaderState::Connecting;
        }
        if !word.is_empty() {
            return HeaderState::Busy(word.to_string());
        }
    }
    if streaming {
        return match activity {
            Activity::Thinking => HeaderState::Thinking,
            Activity::Writing => HeaderState::Streaming,
            Activity::Tool => HeaderState::Tool,
            Activity::Retry => HeaderState::Retry,
            _ => HeaderState::Connecting,
        };
    }
    let total = subagents.len();
    let done = subagents.iter().filter(|s| s.done).count();
    if done < total {
        let bars = if total <= MAX_BARS {
            subagents.iter().map(|s| !s.done).collect()
        } else {
            let running = (total - done).min(MAX_BARS);
            let mut bars = vec![true; running];
            bars.resize(MAX_BARS, false);
            bars
        };
        return HeaderState::Agents { done, total, bars };
    }
    match activity {
        Activity::Failed(label) => HeaderState::Failed(label.clone()),
        _ => HeaderState::Idle,
    }
}

// ─── indicators ─────────────────────────────────────────────────────────────

/// The right-hand activity indicators.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Indicator {
    /// One dot; its colour breathes.
    Breathe,
    /// A dot grows into a ring and fades: `· ∘ ○ ◯`.
    Ripple,
    /// Three dots with a light wave rolling through.
    Dots,
    /// A short bounce with a trail on a tiny rail.
    Scanner,
    /// A scrolling sine: `⎽⎼⎻⎺`.
    Scope,
    /// One bouncing bar per worker; finished workers lie flat.
    Equalizer,
}

impl Indicator {
    /// The auto map: the indicator's shape names the state.
    pub(crate) fn for_state(state: &HeaderState) -> Option<Self> {
        match state {
            HeaderState::Connecting => Some(Self::Dots),
            HeaderState::Thinking | HeaderState::Busy(_) => Some(Self::Breathe),
            HeaderState::Streaming => Some(Self::Scope),
            HeaderState::Tool => Some(Self::Scanner),
            HeaderState::Agents { .. } => Some(Self::Equalizer),
            HeaderState::Retry => Some(Self::Ripple),
            HeaderState::Idle | HeaderState::Failed(_) => None,
        }
    }
}

/// Animation ticks per second (the prototype's frame rate).
const TICKS_PER_SEC: u64 = 30;

fn mix(a: Color, b: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    match (a, b) {
        (Color::Rgb(r0, g0, b0), Color::Rgb(r1, g1, b1)) => {
            let f = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t).round() as u8;
            Color::Rgb(f(r0, r1), f(g0, g1), f(b0, b1))
        }
        _ if t >= 0.5 => b,
        _ => a,
    }
}

fn fg(c: Color) -> Style {
    Style::default().fg(c)
}

/// Render one indicator frame. `bars` only matters for the equalizer.
pub(crate) fn indicator_spans(
    kind: Indicator,
    tick: u64,
    color: Color,
    bg: Color,
    bars: &[bool],
) -> Vec<Span<'static>> {
    let t = tick as f32;
    let dim = mix(bg, color, 0.18);
    match kind {
        Indicator::Breathe => {
            let p = (t / 11.0).sin() * 0.5 + 0.5;
            vec![Span::styled("\u{25cf}", fg(mix(bg, color, 0.2 + 0.8 * p)))]
        }
        Indicator::Ripple => {
            const F: [&str; 5] = ["\u{00b7}", "\u{2218}", "\u{25cb}", "\u{25ef}", " "];
            let i = ((tick / 5) % 5) as usize;
            vec![Span::styled(
                F[i],
                fg(mix(color, bg, i as f32 / 4.0 * 0.85)),
            )]
        }
        Indicator::Dots => (0..3)
            .map(|i| {
                let v = ((t * 0.16 - i as f32 * 0.9).sin() * 0.5 + 0.5).powi(2);
                Span::styled("\u{2022}", fg(mix(dim, color, v)))
            })
            .collect(),
        Indicator::Scanner => {
            let w = 6i32;
            let period = 2 * (w - 1);
            let p = ((tick / 2) as i32) % period;
            let x = if p < w { p } else { period - p };
            (0..w)
                .map(|i| {
                    let b = (1.0 - (i - x).abs() as f32 / 2.2).max(0.0);
                    if b > 0.05 {
                        Span::styled("\u{2501}", fg(mix(dim, color, b)))
                    } else {
                        Span::styled("\u{2500}", fg(dim))
                    }
                })
                .collect()
        }
        Indicator::Scope => {
            const L: [&str; 4] = ["\u{23bd}", "\u{23bc}", "\u{23bb}", "\u{23ba}"];
            (0..6)
                .map(|i| {
                    let v = (i as f32 * 0.95 - t * 0.22).sin() * 0.5 + 0.5;
                    let level = ((v * 3.99) as usize).min(3);
                    Span::styled(L[level], fg(mix(dim, color, 0.45 + 0.55 * v)))
                })
                .collect()
        }
        Indicator::Equalizer => {
            const B: [&str; 7] = [
                "\u{2581}", "\u{2582}", "\u{2583}", "\u{2584}", "\u{2585}", "\u{2586}", "\u{2587}",
            ];
            let default = [true; 3];
            let bars = if bars.is_empty() { &default[..] } else { bars };
            bars.iter()
                .enumerate()
                .map(|(i, running)| {
                    if !running {
                        return Span::styled(B[0], fg(mix(bg, color, 0.35)));
                    }
                    let f = i as f32;
                    let v = (t * 0.17 * (1.0 + 0.37 * f) + f * 1.7).sin() * 0.5 + 0.5;
                    Span::styled(B[((v * 6.99) as usize).min(6)], fg(color))
                })
                .collect()
        }
    }
}

fn state_color(t: &Theme, state: &HeaderState) -> Color {
    match state {
        HeaderState::Streaming => t.status_streaming,
        HeaderState::Tool => t.tool_label,
        HeaderState::Agents { .. } => t.subagent_name,
        HeaderState::Retry => t.warning_color,
        HeaderState::Failed(_) => t.error_color,
        HeaderState::Idle => t.muted,
        _ => t.header_fg,
    }
}

// ─── text ───────────────────────────────────────────────────────────────────

/// `v<version> · <hash>`, or `v<version>` alone when the build has no
/// hash. `build.rs` bakes `release` in place of the hash for packaged
/// builds (AUR, Homebrew, crates.io build from a source tarball, not a
/// synaps checkout), and a released version already names its commit, so
/// `· release` would only read as a missing hash.
fn build_label(version: &str, hash: &str) -> String {
    if hash.is_empty() || hash == "release" {
        format!("v{version}")
    } else {
        format!("v{version} \u{b7} {hash}")
    }
}

/// This build, as the idle header shows it (see [`build_label`]).
pub(crate) fn build() -> &'static str {
    static BUILD: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    BUILD.get_or_init(|| build_label(env!("CARGO_PKG_VERSION"), env!("GIT_HASH")))
}

/// This build, as `/status` reports it (the header drops it when the row
/// is too narrow for it and the session label).
pub(crate) fn build_line() -> String {
    format!("synaps {}", build())
}

pub(crate) fn clock(secs: u64) -> String {
    if secs >= 3600 {
        format!("{}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60)
    } else {
        format!("{}:{:02}", secs / 60, secs % 60)
    }
}

/// The label left of the indicator.
fn right_label(state: &HeaderState, turn_secs: Option<u64>) -> Option<String> {
    match state {
        HeaderState::Idle => None,
        HeaderState::Agents { done, total, .. } => Some(format!("{done}/{total}")),
        HeaderState::Retry => Some("retrying".to_string()),
        HeaderState::Busy(word) => Some(word.clone()),
        HeaderState::Failed(label) => Some(format!("\u{2717} {label}")),
        _ => turn_secs.map(clock),
    }
}

/// Session name for the centre: name, else auto title, else id. Control
/// characters and runs of whitespace collapse to one space (titles are the
/// first user message and may hold newlines).
pub(crate) fn session_label(name: Option<&str>, title: &str, id: &str) -> String {
    let raw = name
        .filter(|n| !n.trim().is_empty())
        .unwrap_or(if title.trim().is_empty() { id } else { title });
    let mut out = String::with_capacity(raw.len());
    let mut space = false;
    for ch in raw.chars() {
        if ch.is_control() || ch.is_whitespace() {
            space = !out.is_empty();
        } else {
            if space {
                out.push(' ');
                space = false;
            }
            out.push(ch);
        }
    }
    out
}

/// `s` cut to at most `max` cells, with `…` when cut.
fn truncate(s: &str, max: usize) -> String {
    if display_width(s) <= max {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for ch in s.chars() {
        let cw = char_width(ch);
        if w + cw + 1 > max {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('\u{2026}');
    out
}

/// Where the centre label goes: true centre when it clears both sides,
/// otherwise slid (and cut) into the free span; `None` when too tight.
fn place_centre(
    label: &str,
    width: usize,
    left_w: usize,
    right_w: usize,
) -> Option<(usize, String)> {
    const GAP: usize = 3;
    const MIN_ROOM: usize = 6;
    if label.is_empty() {
        return None;
    }
    let lo = left_w + GAP;
    let hi = width.checked_sub(right_w + GAP)?;
    if hi <= lo || hi - lo < MIN_ROOM {
        return None;
    }
    let text = truncate(label, hi - lo);
    let tw = display_width(&text);
    let x = (width.saturating_sub(tw) / 2).clamp(lo, hi - tw);
    Some((x, text))
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|s| display_width(&s.content)).sum()
}

// ─── drawing ────────────────────────────────────────────────────────────────

const WORDMARK: &str = "synaps";

/// Smooth highlight sweeping across `len` positions.
fn glow(i: f32, len: f32, tick: u64) -> f32 {
    const W: f32 = 3.0;
    let period = len + 2.0 * W + 8.0;
    let pos = (tick as f32 * 0.4) % period - W;
    let t = (1.0 - (i - pos).abs() / W).max(0.0);
    t * t * (3.0 - 2.0 * t)
}

fn wordmark(t: &Theme, state: &HeaderState, tick: u64) -> Vec<Span<'static>> {
    let base = match state {
        HeaderState::Idle => mix(t.muted, t.bg, 0.25),
        HeaderState::Failed(_) => t.error_color,
        _ => t.header_fg,
    };
    let hi = mix(t.header_fg, Color::Rgb(255, 255, 255), 0.65);
    let n = WORDMARK.chars().count() as f32;
    WORDMARK
        .chars()
        .enumerate()
        .map(|(i, ch)| {
            let c = if state.is_live() {
                mix(base, hi, glow(i as f32, n, tick))
            } else {
                base
            };
            Span::styled(ch.to_string(), fg(c).add_modifier(Modifier::BOLD))
        })
        .collect()
}

/// The right-hand cluster: `label␣␣[indicator, padded to the slot]␣␣`, or
/// nothing when idle. The indicator slot has a fixed width, so the label
/// never moves when the indicator changes shape.
fn right_cluster(
    t: &Theme,
    state: &HeaderState,
    turn_secs: Option<u64>,
    tick: u64,
) -> Vec<Span<'static>> {
    let label = right_label(state, turn_secs);
    let kind = Indicator::for_state(state);
    if label.is_none() && kind.is_none() {
        return Vec::new();
    }
    let mut right = Vec::new();
    if let Some(label) = label {
        let c = if matches!(state, HeaderState::Failed(_)) {
            t.error_color
        } else {
            t.muted
        };
        right.push(Span::styled(label, fg(c)));
        right.push(Span::raw("  "));
    }
    let bars: &[bool] = match state {
        HeaderState::Agents { bars, .. } => bars,
        _ => &[],
    };
    let ind = kind
        .map(|k| indicator_spans(k, tick, state_color(t, state), t.bg, bars))
        .unwrap_or_default();
    let pad = INDICATOR_SLOT.saturating_sub(spans_width(&ind));
    right.extend(ind);
    right.push(Span::raw(" ".repeat(pad + 2)));
    right
}

/// The idle right side: the build, dim, unless showing it would cut or
/// hide the session label (the label matters more).
fn idle_right(t: &Theme, label: &str, width: usize, left_w: usize) -> Vec<Span<'static>> {
    let build = vec![
        Span::styled(build(), fg(mix(t.muted, t.bg, 0.3))),
        Span::raw("  "),
    ];
    let label_whole = match place_centre(label, width, left_w, spans_width(&build)) {
        Some((_, text)) => text == label,
        None => label.is_empty() && width >= left_w + spans_width(&build) + 3,
    };
    if label_whole {
        build
    } else {
        Vec::new()
    }
}

/// Draw the header row.
pub(crate) fn render(frame: &mut Frame<'_>, area: Rect, model: &RenderModel) {
    let t = THEME.load();
    let snap = &model.header;
    let state = header_state(
        model.streaming,
        model.status_text.as_deref(),
        &snap.activity,
        &model.subagents,
    );
    let tick = snap.anim_ms * TICKS_PER_SEC / 1000;

    frame.render_widget(Block::default().style(Style::default().bg(t.bg)), area);

    let mut left = vec![Span::raw("  ")];
    left.extend(wordmark(&t, &state, tick));
    for pill in &model.sidecar_pills {
        left.push(Span::raw(" "));
        left.push(super::draw::sidecar_pill_span(pill, model.spinner_frame));
    }
    // Worker view: say whose thread this is and how to get back.
    if let Some(b) = &model.watch_banner {
        left.push(Span::raw(" "));
        left.extend(super::draw::watch_banner_spans(b));
    }

    let mut right = right_cluster(&t, &state, snap.turn_elapsed_secs, tick);
    if right.is_empty() {
        right = idle_right(
            &t,
            &snap.session_label,
            area.width as usize,
            spans_width(&left),
        );
    }

    let (left_w, right_w) = (spans_width(&left), spans_width(&right));
    frame.render_widget(Paragraph::new(Line::from(left)), area);
    if !right.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(right)).alignment(Alignment::Right),
            area,
        );
    }
    if let Some((x, text)) = place_centre(&snap.session_label, area.width as usize, left_w, right_w)
    {
        let w = display_width(&text) as u16;
        frame.render_widget(
            Paragraph::new(Span::styled(text, fg(mix(t.muted, t.claude_text, 0.45)))),
            Rect {
                x: area.x + x as u16,
                width: w,
                ..area
            },
        );
    }
}

/// True when the message pane's top edge is drawn as the header's lower
/// half (`▀`). Off when the canvas is opaque and the same colour as the
/// header (there is no edge to draw), or when the header colour is the
/// terminal default (`▀` would paint the default foreground).
pub(crate) fn top_edge_is_half_block() -> bool {
    let t = THEME.load();
    if matches!(t.bg, Color::Reset) {
        return false;
    }
    !(background_is_opaque() && t.bg == t.message_background())
}

/// Style the message pane's top border: an upper half-block in the header
/// colour (so the header reads 1½ rows), or the plain rule where that can't
/// show. Geometry is unchanged either way.
pub(crate) fn style_top_edge(block: Block<'_>) -> Block<'_> {
    let t = THEME.load();
    if !top_edge_is_half_block() {
        return block.border_style(fg(t.border));
    }
    let mut style = fg(t.bg);
    if background_is_opaque() {
        style = style.bg(t.message_background());
    }
    block
        .border_set(border::Set {
            horizontal_top: "\u{2580}",
            ..border::PLAIN
        })
        .border_style(style)
}

#[cfg(test)]
mod tests {
    use super::super::testing::TestHarness;
    use super::super::theme::set_background_opaque;
    use super::*;
    use serial_test::serial;

    /// The row under the header is the message pane's top edge drawn as the
    /// header's lower half: `▀`, header colour on top, canvas colour below.
    /// Invisible mode must not paint the canvas colour (no opaque strip).
    #[test]
    #[serial]
    fn message_top_edge_is_the_headers_lower_half() {
        let prior = background_is_opaque();
        for opaque in [true, false] {
            set_background_opaque(opaque);
            let mut h = TestHarness::boot();
            let buf = h.render().clone();
            let t = THEME.load();
            let edge = &buf[(10, 1)];
            if top_edge_is_half_block() {
                assert_eq!(edge.symbol(), "\u{2580}", "opaque={opaque}");
                assert_eq!(edge.fg, t.bg, "opaque={opaque}");
                assert_eq!(buf[(10, 0)].bg, t.bg, "header row colour, opaque={opaque}");
                let want_bg = if opaque {
                    t.message_background()
                } else {
                    Color::Reset
                };
                assert_eq!(edge.bg, want_bg, "opaque={opaque}");
            } else {
                assert_eq!(edge.symbol(), "\u{2500}", "opaque={opaque}");
            }
        }
        set_background_opaque(prior);
    }

    fn worker(done: bool) -> SubagentSnap {
        SubagentSnap {
            name: "w".into(),
            status: String::new(),
            elapsed_secs: 0.0,
            done,
            tools: 0,
            result: None,
            watched: false,
        }
    }

    #[test]
    fn idle_session_shows_nothing_on_the_right() {
        let s = header_state(false, None, &Activity::Idle, &[]);
        assert_eq!(s, HeaderState::Idle);
        assert_eq!(right_label(&s, Some(42)), None);
        assert_eq!(Indicator::for_state(&s), None);
    }

    #[test]
    fn status_text_wins_and_names_the_state() {
        assert_eq!(
            header_state(true, Some("connecting\u{2026}"), &Activity::Thinking, &[]),
            HeaderState::Connecting
        );
        assert_eq!(
            header_state(false, Some("compacting\u{2026}"), &Activity::Idle, &[]),
            HeaderState::Busy("compacting".into())
        );
        assert_eq!(
            header_state(
                false,
                Some("reconnecting\u{2026} (4s left, attempt 2)"),
                &Activity::Idle,
                &[]
            ),
            HeaderState::Busy("reconnecting".into())
        );
    }

    #[test]
    fn streaming_turn_follows_the_activity() {
        let cases = [
            (Activity::Idle, HeaderState::Connecting),
            (Activity::Waiting, HeaderState::Connecting),
            (Activity::Thinking, HeaderState::Thinking),
            (Activity::Writing, HeaderState::Streaming),
            (Activity::Tool, HeaderState::Tool),
            (Activity::Retry, HeaderState::Retry),
        ];
        for (activity, want) in cases {
            assert_eq!(
                header_state(true, None, &activity, &[]),
                want,
                "{activity:?}"
            );
        }
    }

    #[test]
    fn workers_show_when_the_main_turn_is_idle() {
        let ws = [worker(true), worker(false), worker(false)];
        let s = header_state(false, None, &Activity::Idle, &ws);
        assert_eq!(
            s,
            HeaderState::Agents {
                done: 1,
                total: 3,
                bars: vec![false, true, true]
            }
        );
        assert_eq!(right_label(&s, None).as_deref(), Some("1/3"));
        assert_eq!(Indicator::for_state(&s), Some(Indicator::Equalizer));
        // all finished: back to idle
        let ws = [worker(true), worker(true)];
        assert_eq!(
            header_state(false, None, &Activity::Idle, &ws),
            HeaderState::Idle
        );
    }

    #[test]
    fn many_workers_cap_the_bars_and_keep_running_ones_visible() {
        let mut ws: Vec<_> = (0..10).map(|_| worker(true)).collect();
        ws.push(worker(false));
        ws.push(worker(false));
        match header_state(false, None, &Activity::Idle, &ws) {
            HeaderState::Agents { done, total, bars } => {
                assert_eq!((done, total), (10, 12));
                assert_eq!(bars.len(), MAX_BARS);
                assert_eq!(bars.iter().filter(|b| **b).count(), 2);
            }
            other => panic!("expected Agents, got {other:?}"),
        }
    }

    #[test]
    fn a_failed_turn_stays_visible_until_the_next_one() {
        let s = header_state(false, None, &Activity::Failed("401".into()), &[]);
        assert_eq!(s, HeaderState::Failed("401".into()));
        assert_eq!(right_label(&s, None).as_deref(), Some("\u{2717} 401"));
        assert_eq!(Indicator::for_state(&s), None);
        // a new turn supersedes it
        assert_eq!(
            header_state(true, None, &Activity::Failed("401".into()), &[]),
            HeaderState::Connecting
        );
    }

    #[test]
    fn auto_map_gives_each_live_state_its_own_shape() {
        let map = [
            (HeaderState::Connecting, Indicator::Dots),
            (HeaderState::Thinking, Indicator::Breathe),
            (HeaderState::Streaming, Indicator::Scope),
            (HeaderState::Tool, Indicator::Scanner),
            (HeaderState::Retry, Indicator::Ripple),
        ];
        for (state, want) in map {
            assert_eq!(Indicator::for_state(&state), Some(want), "{state:?}");
        }
    }

    #[test]
    fn indicators_keep_a_constant_width_across_frames() {
        let kinds = [
            (Indicator::Breathe, 1),
            (Indicator::Ripple, 1),
            (Indicator::Dots, 3),
            (Indicator::Scanner, 6),
            (Indicator::Scope, 6),
            (Indicator::Equalizer, 3),
        ];
        let (c, bg) = (Color::Rgb(200, 100, 50), Color::Rgb(10, 10, 20));
        for (kind, w) in kinds {
            for tick in 0..120 {
                let spans = indicator_spans(kind, tick, c, bg, &[]);
                assert_eq!(spans_width(&spans), w, "{kind:?} tick {tick}");
            }
        }
        let bars = [false, true, true, false];
        assert_eq!(
            spans_width(&indicator_spans(Indicator::Equalizer, 7, c, bg, &bars)),
            4
        );
    }

    #[test]
    fn every_indicator_fits_the_slot() {
        let (c, bg) = (Color::Rgb(200, 100, 50), Color::Rgb(10, 10, 20));
        let full = [true; MAX_BARS];
        for kind in [
            Indicator::Breathe,
            Indicator::Ripple,
            Indicator::Dots,
            Indicator::Scanner,
            Indicator::Scope,
            Indicator::Equalizer,
        ] {
            for tick in 0..120 {
                let w = spans_width(&indicator_spans(kind, tick, c, bg, &full));
                assert!(w <= INDICATOR_SLOT, "{kind:?} is {w} wide at tick {tick}");
            }
        }
    }

    /// The label (turn clock, `1/3`, `retrying`, `✗ 401`) ends at the same
    /// column in every state and on every frame: only the indicator after
    /// it changes, inside its fixed slot.
    #[test]
    fn the_label_never_moves_when_the_indicator_changes() {
        let t = THEME.load();
        let states = [
            HeaderState::Connecting,
            HeaderState::Thinking,
            HeaderState::Streaming,
            HeaderState::Tool,
            HeaderState::Agents {
                done: 1,
                total: 3,
                bars: vec![false, true, true],
            },
            HeaderState::Agents {
                done: 0,
                total: 6,
                bars: vec![true; 6],
            },
            HeaderState::Retry,
            HeaderState::Busy("compacting".into()),
            HeaderState::Failed("401".into()),
        ];
        for state in &states {
            for tick in 0..120 {
                let right = right_cluster(&t, state, Some(42), tick);
                assert!(!right.is_empty(), "{state:?}");
                let after_label = spans_width(&right[1..]);
                assert_eq!(after_label, 2 + INDICATOR_SLOT + 2, "{state:?} tick {tick}");
            }
        }
        assert!(right_cluster(&t, &HeaderState::Idle, None, 0).is_empty());
    }

    #[test]
    fn idle_shows_the_build_unless_it_would_cut_the_label() {
        let t = THEME.load();
        let left_w = 8; // "  synaps"
        let shown = |label: &str, w: usize| !idle_right(&t, label, w, left_w).is_empty();
        let build_w = build().chars().count() + 2;

        assert!(shown("axon-overnight", 100));
        assert_eq!(
            spans_width(&idle_right(&t, "axon-overnight", 100, left_w)),
            build_w
        );
        // Just room for the whole label next to the build: shown.
        let label = "axon-overnight";
        let exact = left_w + 3 + label.len() + 3 + build_w;
        assert!(shown(label, exact), "width {exact}");
        // One column less would cut the label: the build gives way.
        assert!(!shown(label, exact - 1), "width {}", exact - 1);
        // A long title that won't fit whole anyway: no build either.
        assert!(!shown(&"x".repeat(200), 120));
    }

    #[test]
    fn build_line_names_version_and_hash() {
        let line = build_line();
        assert_eq!(
            line,
            format!("synaps {}", build()),
            "header and /status agree"
        );
        assert!(build().starts_with(concat!("v", env!("CARGO_PKG_VERSION"))));
    }

    /// Packaged builds (`GIT_HASH=release`, see build.rs) show the version
    /// alone; checkouts show the hash, `-dirty` included.
    #[test]
    fn build_label_drops_the_placeholder_hash() {
        assert_eq!(build_label("0.10.2", "release"), "v0.10.2");
        assert_eq!(build_label("0.10.2", ""), "v0.10.2");
        assert_eq!(build_label("0.10.2", "4a47bc5f"), "v0.10.2 \u{b7} 4a47bc5f");
        assert_eq!(
            build_label("0.10.2", "4a47bc5f-dirty"),
            "v0.10.2 \u{b7} 4a47bc5f-dirty"
        );
    }

    #[test]
    fn clock_formats_minutes_and_hours() {
        assert_eq!(clock(0), "0:00");
        assert_eq!(clock(42), "0:42");
        assert_eq!(clock(26 * 60 + 12), "26:12");
        assert_eq!(clock(3600 + 61), "1:01:01");
    }

    #[test]
    fn session_label_prefers_name_then_title_then_id_and_flattens_whitespace() {
        assert_eq!(
            session_label(Some("axon-overnight"), "t", "id"),
            "axon-overnight"
        );
        assert_eq!(
            session_label(Some("  "), "fix\n the\tcache ", "id"),
            "fix the cache"
        );
        assert_eq!(
            session_label(None, "", "20261014-011500-79cc"),
            "20261014-011500-79cc"
        );
    }

    #[test]
    fn centre_label_centres_slides_cuts_or_hides() {
        // fits: true centre
        let (x, text) = place_centre("abcd", 40, 8, 8).unwrap();
        assert_eq!((x, text.as_str()), (18, "abcd"));
        // a long right side pushes it left and cuts it
        let (x, text) = place_centre("a-rather-long-session-name", 40, 8, 20).unwrap();
        assert!(
            x >= 11 && x + display_width(&text) <= 17,
            "x={x} text={text}"
        );
        assert!(text.ends_with('\u{2026}'));
        // no room at all
        assert_eq!(place_centre("abcd", 20, 8, 8), None);
    }
}
