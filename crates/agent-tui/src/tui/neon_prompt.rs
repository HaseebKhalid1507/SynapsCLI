//! Neon prompt: the input drawn as a soft-edged slab of light instead of a
//! bordered box.
//!
//! The shape is made only of half-cell blocks (`▗▄▖ ▐ ▌ ▝▀▘`), so there is no
//! line art. Its body is the theme's chrome lifted toward the theme's own
//! text colour, floating on the chrome band it shares with the footer. Every
//! colour is a theme token or a mix of two.
//!
//! At rest it is completely still (no frames at all). Motion only answers
//! something happening: a faint glow trails behind the cursor while typing,
//! sending brightens the body for a moment, and while a turn streams a soft
//! band of the theme's streaming colour sweeps across.
//!
//! Split in two:
//! - [`PromptClock`] lives on `App` (main task) and turns input/stream events
//!   into a pure per-frame [`PromptFx`] snapshot carried by the `RenderModel`.
//! - [`Slab`] + the `paint_*` functions draw from that snapshot on the render
//!   side and never read `App`.
//!
//! `SYNAPS_NO_BOOT_FX=1` (the existing "no animations" switch) freezes the
//! prompt at its resting look.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier},
};

use super::theme::Theme;

type Rgb = (u8, u8, u8);

/// Columns between the input area's edge and the text column's prompt glyph:
/// canvas margin, half-block side, padding.
pub(crate) const INSET_X: u16 = 3;

const SHIMMER_PERIOD: f32 = 1.9;
const TRAIL_DECAY: f32 = 0.7;
const PULSE_DECAY: f32 = 0.45;
/// Seconds the prompt takes to dim when a turn starts streaming (and to come
/// back when it ends).
const DIM_FADE: f32 = 0.25;
/// How far the dim goes: the body gives back this share of its lift off the
/// chrome, and typed text moves this far toward the body (never below AA).
const DIM_BODY: f32 = 0.6;
const DIM_TEXT: f32 = 0.3;
/// Seconds of the "your turn" cue when a turn ends: a wash of the theme's
/// prompt colour over the slab (and a little around it) that fades out.
const ARRIVE_DECAY: f32 = 0.9;
/// Redraw cadence the prompt asks for while it animates on its own.
const FRAME: Duration = Duration::from_millis(40);

// ───────────────────────────── colour helpers ──────────────────────────────

fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    let f = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t).round() as u8;
    (f(a.0, b.0), f(a.1, b.1), f(a.2, b.2))
}

fn luminance(c: Rgb) -> f32 {
    let ch = |v: u8| {
        let v = f32::from(v) / 255.0;
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * ch(c.0) + 0.7152 * ch(c.1) + 0.0722 * ch(c.2)
}

fn contrast(a: Rgb, b: Rgb) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

fn color(c: Rgb) -> Color {
    Color::Rgb(c.0, c.1, c.2)
}

/// Theme colours are RGB for every builtin, user theme file and MXC feed;
/// anything else falls back to the default palette's value.
fn rgb(c: Color, fallback: Color) -> Rgb {
    match (c, fallback) {
        (Color::Rgb(r, g, b), _) | (_, Color::Rgb(r, g, b)) => (r, g, b),
        _ => (0, 0, 0),
    }
}

// ───────────────────────────── settings ────────────────────────────────────

/// Settings → Appearance → "Streaming glow" (`tui_streaming_glow`, off by
/// default): whether a glow sweeps across the prompt while a turn streams.
static STREAMING_GLOW: LazyLock<AtomicBool> =
    LazyLock::new(|| AtomicBool::new(synaps_cli::config::load_config().tui_streaming_glow));

pub(crate) fn streaming_glow_enabled() -> bool {
    STREAMING_GLOW.load(Ordering::Relaxed)
}

pub(crate) fn set_streaming_glow(on: bool) {
    STREAMING_GLOW.store(on, Ordering::Relaxed);
}

// ───────────────────────────── timing (main task) ──────────────────────────

/// Everything time-dependent the renderer needs for one frame. `Default` is
/// the resting look (no trail, no pulse) — what idle, tests and
/// `SYNAPS_NO_BOOT_FX=1` get.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct PromptFx {
    /// 0..1 position of the streaming sweep.
    pub(crate) shimmer: f32,
    /// 0..1 cursor glow energy after a keystroke.
    pub(crate) trail: f32,
    /// 0..1 flash after sending.
    pub(crate) pulse: f32,
    /// Seconds the current turn has been streaming.
    pub(crate) stream_secs: f32,
    /// 0..1 how dimmed the prompt is: 1 while a turn streams (the agent has
    /// the floor), 0 when idle, eased between over [`DIM_FADE`].
    pub(crate) dim: f32,
    /// 0..1 the "your turn" cue right after a turn ends, fading out over
    /// [`ARRIVE_DECAY`].
    pub(crate) arrive: f32,
}

/// Input/stream event times → per-frame [`PromptFx`]. Lives on `App`.
#[derive(Debug)]
pub(crate) struct PromptClock {
    enabled: bool,
    last_key: Instant,
    last_send: Instant,
    last_frame: Instant,
    stream_since: Instant,
    /// When streaming last started or stopped (drives the dim fade).
    stream_changed: Instant,
    /// When the last turn ended (drives the "your turn" cue).
    ready_at: Instant,
    was_streaming: bool,
}

impl PromptClock {
    pub(crate) fn new(now: Instant) -> Self {
        let enabled = !std::env::var("SYNAPS_NO_BOOT_FX").is_ok_and(|v| v == "1");
        Self::with_enabled(now, enabled)
    }

    fn with_enabled(now: Instant, enabled: bool) -> Self {
        let long_ago = now.checked_sub(Duration::from_secs(60)).unwrap_or(now);
        Self {
            enabled,
            last_key: long_ago,
            last_send: long_ago,
            last_frame: long_ago,
            stream_since: now,
            stream_changed: long_ago,
            ready_at: long_ago,
            was_streaming: false,
        }
    }

    fn secs(later: Instant, earlier: Instant) -> f32 {
        later.saturating_duration_since(earlier).as_secs_f32()
    }

    fn trail(&self, now: Instant) -> f32 {
        (1.0 - Self::secs(now, self.last_key) / TRAIL_DECAY)
            .max(0.0)
            .powi(2)
    }

    fn pulse(&self, now: Instant) -> f32 {
        (1.0 - Self::secs(now, self.last_send) / PULSE_DECAY)
            .max(0.0)
            .powi(2)
    }

    fn arrive(&self, now: Instant, streaming: bool) -> f32 {
        if streaming {
            return 0.0;
        }
        (1.0 - Self::secs(now, self.ready_at) / ARRIVE_DECAY)
            .max(0.0)
            .powi(2)
    }

    fn fading(&self, now: Instant) -> bool {
        Self::secs(now, self.stream_changed) < DIM_FADE
    }

    fn dim(&self, now: Instant, streaming: bool) -> f32 {
        let p = (Self::secs(now, self.stream_changed) / DIM_FADE).clamp(0.0, 1.0);
        let eased = p * p * (3.0 - 2.0 * p);
        if streaming {
            eased
        } else {
            1.0 - eased
        }
    }

    /// A key or paste went into the input: lights the cursor trail.
    pub(crate) fn touch(&mut self, now: Instant) {
        self.last_key = now;
    }

    /// A message was sent: brief brightening of the slab.
    pub(crate) fn sent(&mut self, now: Instant) {
        self.last_send = now;
    }

    /// True while something is moving: a turn is streaming, or a keystroke's
    /// trail / a send's flash is still fading (under a second). At rest it is
    /// false, so the prompt never costs a frame while idle. Feeds the tick
    /// guard.
    pub(crate) fn animating(&self, now: Instant, streaming: bool) -> bool {
        self.enabled
            && (streaming
                || self.fading(now)
                || self.arrive(now, streaming) > 0.0
                || self.trail(now) > 0.0
                || self.pulse(now) > 0.0)
    }

    /// Whether the tick arm should request a redraw for the prompt now.
    /// Throttled to [`FRAME`]; the frame itself is time-based, so a skipped
    /// tick never makes the animation jump.
    pub(crate) fn wants_frame(&mut self, now: Instant, streaming: bool) -> bool {
        if !self.animating(now, streaming) || Self::secs(now, self.last_frame) < FRAME.as_secs_f32()
        {
            return false;
        }
        self.last_frame = now;
        true
    }

    /// This frame's snapshot. Called once per built `RenderModel`.
    pub(crate) fn frame(&mut self, now: Instant, streaming: bool) -> PromptFx {
        if streaming != self.was_streaming {
            self.was_streaming = streaming;
            self.stream_changed = now;
            if streaming {
                self.stream_since = now;
            } else {
                self.ready_at = now;
            }
        }
        let streamed = Self::secs(now, self.stream_since);
        let stream_secs = if streaming { streamed } else { 0.0 };
        if !self.enabled {
            return PromptFx {
                stream_secs,
                dim: if streaming { 1.0 } else { 0.0 },
                ..PromptFx::default()
            };
        }
        PromptFx {
            shimmer: if streaming {
                (streamed / SHIMMER_PERIOD).fract()
            } else {
                0.0
            },
            trail: self.trail(now),
            pulse: self.pulse(now),
            stream_secs,
            dim: self.dim(now, streaming),
            arrive: self.arrive(now, streaming),
        }
    }
}

// ───────────────────────────── colours (render side) ───────────────────────

/// One frame's colours for an input area of a given width.
///
/// Every colour is a theme token or a mix of two theme tokens — nothing
/// invented (no white, no foreign accent). The body is the chrome lifted
/// toward the theme's own text colour, so it keeps the chrome's hue on every
/// palette; the prompt glyph, text and spinner use their tokens unchanged.
pub(crate) struct Slab {
    /// What the slab floats on: the chrome (`bg`), full width, continuous
    /// with the footer row below.
    backdrop: Rgb,
    /// The body colour this frame (dimmed while streaming, before flash or
    /// sweep).
    body: Rgb,
    /// The theme's `input_fg`, full strength (the legibility anchor).
    text: Rgb,
    /// Typed-text colour this frame: `text`, dimmed while streaming.
    typed: Rgb,
    prompt: Rgb,
    muted: Rgb,
    stream: Rgb,
    fx: PromptFx,
    /// Fill of the slab body, per column (varies only under the shimmer).
    fill: Vec<Rgb>,
    /// Band colour under the half-cell edges, per column: the chrome, lit
    /// only under the shimmer.
    halo: Vec<Rgb>,
}

/// How far the body stands off the chrome when the prompt is ready (WCAG
/// contrast): clearly the brightest surface in the bottom band, the "ready for
/// you" state. Streaming gives back [`DIM_BODY`] of it.
const BODY_CONTRAST: f32 = 1.22;
/// Cap on the lift toward the text colour.
const BODY_MAX_LIFT: f32 = 0.25;

impl Slab {
    /// `glow`: the colour of the sweep across the slab while a turn streams
    /// (the context bar's colour), or `None` for no sweep — not streaming, or
    /// the "Streaming glow" setting is off.
    pub(crate) fn new(theme: &Theme, fx: PromptFx, width: u16, glow: Option<Color>) -> Self {
        let d = Theme::default();
        let glow = glow.map(|g| rgb(g, d.border_active));
        let backdrop = rgb(theme.bg, d.bg);
        let text = rgb(theme.input_fg, d.input_fg);
        let prompt = rgb(theme.prompt_fg, d.prompt_fg);
        let muted = rgb(theme.muted, d.muted);
        let stream = rgb(theme.status_streaming, d.status_streaming);

        // Chrome lifted toward the theme's text just far enough to read.
        let mut body = backdrop;
        let mut lift = 0.0;
        while lift < BODY_MAX_LIFT && contrast(body, backdrop) < BODY_CONTRAST {
            lift += 0.01;
            body = mix(backdrop, text, lift);
        }
        // Streaming: the agent has the floor, so the prompt steps back a
        // little — the body gives back part of its lift.
        body = mix(backdrop, text, lift * (1.0 - DIM_BODY * fx.dim));
        // Send flash: the body brightens toward the text colour for a moment.
        // "Your turn": when a turn ends the prompt colour washes over the
        // slab and fades, so the hand-back is felt, not just read.
        let rest = mix(mix(body, text, 0.08 * fx.pulse), prompt, 0.14 * fx.arrive);
        let arrive_halo = mix(backdrop, prompt, 0.06 * fx.arrive);

        let w = usize::from(width.max(1));
        let wf = w as f32;
        let sweep = fx.shimmer * (wf + 40.0) - 20.0;
        let (mut fill, mut halo) = (Vec::with_capacity(w), Vec::with_capacity(w));
        for x in 0..w {
            if let Some(glow) = glow {
                // A soft band of the glow colour sweeps across, spilling a
                // little onto the chrome around the slab.
                let dist = (x as f32 - sweep) / 9.0;
                let s = (-dist * dist).exp();
                fill.push(mix(rest, glow, 0.10 * s));
                halo.push(mix(backdrop, glow, 0.07 * s));
            } else {
                fill.push(rest);
                halo.push(arrive_halo);
            }
        }
        let typed = mix(text, body, DIM_TEXT * fx.dim);
        Self {
            backdrop,
            body,
            text,
            typed,
            prompt,
            muted,
            stream,
            fx,
            fill,
            halo,
        }
    }

    fn at(v: &[Rgb], x: u16) -> Rgb {
        v[usize::from(x).min(v.len() - 1)]
    }

    /// Body fill at column `x` (relative to the input area).
    pub(crate) fn fill_at(&self, x: u16) -> Rgb {
        Self::at(&self.fill, x)
    }

    /// `c` mixed toward the theme's text colour just until it reads at
    /// `target` on `bg` (unchanged if it already does).
    fn legible(&self, c: Rgb, bg: Rgb, target: f32) -> Rgb {
        let mut t = 0.0;
        loop {
            let out = mix(c, self.text, t);
            if contrast(out, bg) >= target || t >= 1.0 {
                return out;
            }
            t += 0.05;
        }
    }

    /// Cursor block: the typed-text colour (the classic inverse cursor).
    fn cursor(&self) -> Rgb {
        self.legible(self.typed, self.brightest(), 4.5)
    }

    /// How much of the cursor colour bleeds into the cell `dx` columns away:
    /// a faint glow that trails behind typing and fades.
    fn glow(&self, dx: i32) -> f32 {
        let trail = self.fx.trail;
        match dx {
            -1 => 0.12 * trail,
            -2 => 0.06 * trail,
            -3 => 0.03 * trail,
            _ => 0.0,
        }
    }

    /// The body colour text is hardest to read on: the brightest column of
    /// the fill. Foreground colours are checked against it, so they hold
    /// everywhere, including under the shimmer.
    fn brightest(&self) -> Rgb {
        self.fill
            .iter()
            .copied()
            .max_by(|a, b| luminance(*a).total_cmp(&luminance(*b)))
            .unwrap_or(self.body)
    }

    /// Prompt glyph: the theme's `prompt_fg`.
    pub(crate) fn prompt_fg(&self) -> Color {
        color(self.legible(self.prompt, self.brightest(), 3.0))
    }

    /// Spinner while streaming: the theme's `status_streaming`.
    pub(crate) fn spinner_fg(&self) -> Color {
        color(self.legible(self.stream, self.brightest(), 3.0))
    }

    /// Secondary text on the body (placeholder, scroll arrows, hint tabs):
    /// `muted` lifted toward the text colour until it reads at 4.5:1.
    pub(crate) fn dim_fg(&self) -> Color {
        color(self.legible(self.muted, self.brightest(), 4.5))
    }

    /// Ghost completion: `muted` at 3.6:1, deliberately dimmer than typed
    /// text and than the placeholder.
    pub(crate) fn ghost_fg(&self) -> Color {
        color(self.legible(self.muted, self.brightest(), 3.6))
    }

    /// Typed input: the theme's `input_fg`, dimmed a little while a turn
    /// streams (never below 4.5:1).
    pub(crate) fn text_fg(&self) -> Color {
        color(self.legible(self.typed, self.brightest(), 4.5))
    }
}

// ───────────────────────────── painting ────────────────────────────────────

fn put(buf: &mut Buffer, x: u16, y: u16, sym: &str, fg: Color, bg: Color) {
    if let Some(cell) = buf.cell_mut((x, y)) {
        cell.set_symbol(sym).set_fg(fg).set_bg(bg).modifier = Modifier::empty();
    }
}

/// Where the cursor sits, for the glow under it (absolute buffer coords).
#[derive(Clone, Copy)]
pub(crate) struct CursorAt {
    pub(crate) x: u16,
    pub(crate) y: u16,
}

/// Paint the slab (backdrop, half-cell rims, sides, body) over `area`.
///
/// The backdrop is the chrome colour across the full width, so the input rows
/// and the footer read as one band with the slab floating on it. Like the
/// header and footer, it is painted whatever the background toggle says —
/// the toggle owns only the conversation canvas.
pub(crate) fn paint_slab(buf: &mut Buffer, area: Rect, slab: &Slab, cursor: Option<CursorAt>) {
    if area.width < 4 || area.height < 3 {
        return;
    }
    let outside = |x: u16, v: &[Rgb]| color(Slab::at(v, x));
    let backdrop = color(slab.backdrop);
    let (top, bottom) = (area.y, area.bottom() - 1);
    let (l, r) = (1u16, area.width - 2); // relative columns of the rounded sides
    for y in area.top()..area.bottom() {
        put(buf, area.x, y, " ", backdrop, backdrop);
        put(buf, area.right() - 1, y, " ", backdrop, backdrop);
    }
    for rx in l..=r {
        let (top_sym, bottom_sym) = if rx == l {
            ("\u{2597}", "\u{259D}") // ▗ ▝
        } else if rx == r {
            ("\u{2596}", "\u{2598}") // ▖ ▘
        } else {
            ("\u{2584}", "\u{2580}") // ▄ ▀
        };
        let fill = color(slab.fill_at(rx));
        let halo = outside(rx, &slab.halo);
        put(buf, area.x + rx, top, top_sym, fill, halo);
        put(buf, area.x + rx, bottom, bottom_sym, fill, halo);
    }
    for y in top + 1..bottom {
        let halo_l = outside(l, &slab.halo);
        let halo_r = outside(r, &slab.halo);
        put(
            buf,
            area.x + l,
            y,
            "\u{2590}",
            color(slab.fill_at(l)),
            halo_l,
        ); // ▐
        put(
            buf,
            area.x + r,
            y,
            "\u{258C}",
            color(slab.fill_at(r)),
            halo_r,
        ); // ▌
        let lit = cursor.filter(|c| c.y == y).map(|c| (c.x, slab.cursor()));
        for rx in l + 1..r {
            let mut bg = slab.fill_at(rx);
            if let Some((cx, cur)) = lit {
                bg = mix(bg, cur, slab.glow(i32::from(area.x + rx) - i32::from(cx)));
            }
            put(buf, area.x + rx, y, " ", color(bg), color(bg));
        }
    }
}

/// Light the cursor cell: a block in the cursor colour with the glyph under
/// it drawn in the body colour. Call after the text is rendered.
pub(crate) fn paint_cursor(buf: &mut Buffer, area: Rect, slab: &Slab, at: CursorAt) {
    if at.x < area.x + 2 || at.x + 2 >= area.right() || at.y <= area.y || at.y + 1 >= area.bottom()
    {
        return;
    }
    let fill = slab.fill_at(at.x - area.x);
    if let Some(cell) = buf.cell_mut((at.x, at.y)) {
        let sym = if cell.symbol().trim().is_empty() {
            " ".to_string()
        } else {
            cell.symbol().to_string()
        };
        cell.set_symbol(&sym)
            .set_fg(color(fill))
            .set_bg(color(slab.cursor()));
    }
}

/// The role of a piece of hint text — Noodle's contrast rule: the thing you
/// press is bright, the word that explains it is dim.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Tone {
    /// A key or command to type (`input_fg`).
    Key,
    /// A key in a passive hint (placeholder, line count): halfway between
    /// [`Tone::Key`] and [`Tone::Word`], so it reads as a key without
    /// shouting.
    SoftKey,
    /// The explaining word, a count, a time (the legible dim).
    Word,
    /// Live activity (`status_streaming`).
    Live,
}

impl Slab {
    /// Foreground for a hint [`Tone`] on the body.
    pub(crate) fn tone_fg(&self, tone: Tone) -> Color {
        match tone {
            Tone::Key => color(self.legible(self.text, self.brightest(), 4.5)),
            Tone::SoftKey => {
                let word = self.legible(self.muted, self.brightest(), 4.5);
                let key = self.legible(self.text, self.brightest(), 4.5);
                color(mix(key, word, 0.5))
            }
            Tone::Word => self.dim_fg(),
            Tone::Live => self.spinner_fg(),
        }
    }
}

/// A status tab hanging off the bottom rim, right-aligned inside the slab:
/// the body colour extends down under the text, padded one cell each side
/// (a Noodle badge). Skipped when it doesn't fit.
pub(crate) fn paint_tab(buf: &mut Buffer, area: Rect, slab: &Slab, parts: &[(String, Tone)]) {
    let len: u16 = parts
        .iter()
        .map(|(t, _)| super::text_metrics::width(t) as u16)
        .sum::<u16>()
        + 2;
    if area.height < 3 || area.width < len + 8 {
        return;
    }
    let y = area.bottom() - 1;
    let r = area.width - 2;
    let mut rx = r - 2 - len;
    let pad = [(" ".to_string(), Tone::Word)];
    for (text, tone) in pad.iter().chain(parts).chain(pad.iter()) {
        let fg = slab.tone_fg(*tone);
        for ch in text.chars() {
            let bg = color(slab.fill_at(rx));
            let mut tmp = [0u8; 4];
            put(buf, area.x + rx, y, ch.encode_utf8(&mut tmp), fg, bg);
            rx += super::text_metrics::char_width(ch) as u16;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUILTINS: &[&str] = &[
        "default",
        "night-city",
        "neon-rain",
        "amber",
        "phosphor",
        "solarized-dark",
        "blood",
        "ocean",
        "rose-pine",
        "nord",
        "dracula",
        "monokai",
        "myx",
        "gruvbox",
        "catppuccin",
        "tokyo-night",
        "sunset",
        "ice",
        "forest",
        "lavender",
    ];

    fn rgb_of(c: Color) -> Rgb {
        match c {
            Color::Rgb(r, g, b) => (r, g, b),
            other => panic!("expected rgb, got {other:?}"),
        }
    }

    /// Worst-case frames: rest, full pulse + trail, and a shimmer sweeping
    /// every position while streaming.
    fn frames() -> Vec<(PromptFx, bool)> {
        let mut out = vec![
            (PromptFx::default(), false),
            (
                PromptFx {
                    pulse: 1.0,
                    trail: 1.0,
                    ..PromptFx::default()
                },
                false,
            ),
            (
                PromptFx {
                    arrive: 1.0,
                    ..PromptFx::default()
                },
                false,
            ),
        ];
        for i in 0..10 {
            out.push((
                PromptFx {
                    shimmer: i as f32 / 10.0,
                    dim: 1.0,
                    ..PromptFx::default()
                },
                true,
            ));
        }
        out
    }

    #[test]
    fn text_and_hints_stay_legible_on_every_palette() {
        for name in BUILTINS {
            let theme = super::super::theme::Theme::builtin_for_test(name);
            // The sweep can wear any of the context bar's three colours.
            let glows = [
                theme.border_active,
                theme.status_streaming,
                theme.error_color,
            ];
            for ((fx, streaming), glow) in frames()
                .into_iter()
                .flat_map(|f| glows.map(move |g| (f, g)))
            {
                let slab = Slab::new(&theme, fx, 100, streaming.then_some(glow));
                let bg = slab.brightest();
                for (what, fg, min) in [
                    ("text", slab.text_fg(), 4.5),
                    ("dim", slab.dim_fg(), 4.5),
                    ("ghost", slab.ghost_fg(), 3.6),
                    ("prompt", slab.prompt_fg(), 3.0),
                    ("spinner", slab.spinner_fg(), 3.0),
                ] {
                    let c = contrast(rgb_of(fg), bg);
                    assert!(
                        c >= min - 0.01,
                        "{name}: {what} {c:.2} streaming={streaming}"
                    );
                }
            }
        }
    }

    /// Matches the theme: at rest the prompt glyph, text and cursor are the
    /// theme's own tokens, and the body stays in the chrome's family — a
    /// quiet step off the chrome, not a new colour.
    #[test]
    fn resting_colours_are_the_themes_own() {
        for name in BUILTINS {
            let theme = super::super::theme::Theme::builtin_for_test(name);
            let slab = Slab::new(&theme, PromptFx::default(), 80, None);
            assert_eq!(slab.text_fg(), theme.input_fg, "{name}: text");
            assert_eq!(slab.cursor(), rgb_of(theme.input_fg), "{name}: cursor");
            let c = contrast(slab.body, rgb_of(theme.bg));
            assert!(
                (1.05..=1.30).contains(&c),
                "{name}: body/chrome step {c:.3} (body {:?})",
                slab.body
            );
        }
    }

    #[test]
    fn ghost_reads_dimmer_than_typed_text() {
        for name in BUILTINS {
            let theme = super::super::theme::Theme::builtin_for_test(name);
            let slab = Slab::new(&theme, PromptFx::default(), 80, None);
            let bg = slab.fill_at(10);
            assert!(
                contrast(rgb_of(slab.ghost_fg()), bg) < contrast(rgb_of(slab.text_fg()), bg),
                "{name}: ghost must be dimmer than typed text"
            );
        }
    }

    #[test]
    fn still_at_rest_moving_only_on_events() {
        let t0 = Instant::now();
        let mut clock = PromptClock::with_enabled(t0, true);
        assert!(!clock.animating(t0, false), "no motion at boot/idle");
        assert!(!clock.wants_frame(t0, false));
        assert_eq!(clock.frame(t0, false), PromptFx::default(), "resting look");

        clock.touch(t0);
        assert!(clock.animating(t0, false), "a key lights the trail");
        assert!(clock.frame(t0, false).trail > 0.9);
        let faded = t0 + Duration::from_secs_f32(TRAIL_DECAY + 0.01);
        assert!(!clock.animating(faded, false), "and it fades back to rest");

        clock.sent(faded);
        assert!(clock.animating(faded, false), "a send flashes");
        let settled = faded + Duration::from_secs_f32(PULSE_DECAY + 0.01);
        assert!(!clock.animating(settled, false));

        assert!(
            clock.animating(settled + Duration::from_secs(600), true),
            "always while streaming"
        );
    }

    /// At rest the band behind the slab (halo) is exactly the chrome, so the
    /// input rows and the footer are one colour.
    #[test]
    fn resting_halo_is_exactly_the_chrome() {
        for name in BUILTINS {
            let theme = super::super::theme::Theme::builtin_for_test(name);
            let slab = Slab::new(&theme, PromptFx::default(), 80, None);
            for x in [1u16, 20, 78] {
                assert_eq!(Slab::at(&slab.halo, x), rgb_of(theme.bg), "{name} x={x}");
            }
        }
    }

    #[test]
    fn frames_are_throttled() {
        let t0 = Instant::now();
        let mut clock = PromptClock::with_enabled(t0, true);
        assert!(clock.wants_frame(t0, true));
        assert!(!clock.wants_frame(t0 + Duration::from_millis(10), true));
        assert!(clock.wants_frame(t0 + FRAME, true));
    }

    #[test]
    fn disabled_clock_is_static() {
        let t0 = Instant::now();
        let mut clock = PromptClock::with_enabled(t0, false);
        clock.touch(t0);
        assert!(!clock.animating(t0, true));
        let fx = clock.frame(t0 + Duration::from_secs(2), true);
        assert_eq!(
            fx,
            PromptFx {
                stream_secs: fx.stream_secs,
                dim: 1.0,
                ..PromptFx::default()
            },
            "no motion when disabled: the elapsed clock, and the dim snaps"
        );
    }

    #[test]
    fn stream_start_resets_the_stream_clock() {
        let t0 = Instant::now();
        let mut clock = PromptClock::with_enabled(t0, true);
        let t1 = t0 + Duration::from_secs(30);
        assert_eq!(clock.frame(t1, true).stream_secs, 0.0);
        let fx = clock.frame(t1 + Duration::from_millis(4200), true);
        assert!((fx.stream_secs - 4.2).abs() < 0.01);
        assert_eq!(
            clock.frame(t1 + Duration::from_secs(5), false).stream_secs,
            0.0
        );
    }
}

#[cfg(test)]
mod flat_fill_tests {
    use super::*;

    /// No gradient: at rest every column of the body is the same colour.
    #[test]
    fn resting_fill_is_flat() {
        for name in ["default", "myx", "night-city", "gruvbox"] {
            let theme = super::super::theme::Theme::builtin_for_test(name);
            let slab = Slab::new(&theme, PromptFx::default(), 100, None);
            let first = slab.fill_at(0);
            assert!(
                (0..100).all(|x| slab.fill_at(x) == first),
                "{name}: fill varies across the slab"
            );
        }
    }
}

#[cfg(test)]
mod glow_tests {
    use super::*;

    /// The streaming sweep takes the caller's glow colour (the context bar's).
    #[test]
    fn streaming_sweep_wears_the_glow_colour() {
        let theme = Theme::default();
        let glow = Color::Rgb(250, 10, 10);
        let fx = PromptFx {
            shimmer: 0.5, // sweep centre at column 50 of 100
            ..PromptFx::default()
        };
        let slab = Slab::new(&theme, fx, 100, Some(glow));
        assert_eq!(slab.fill_at(50), mix(slab.body, (250, 10, 10), 0.10));
        assert_eq!(
            Slab::at(&slab.halo, 50),
            mix(slab.backdrop, (250, 10, 10), 0.07)
        );
        assert_eq!(
            slab.fill_at(0),
            slab.body,
            "outside the sweep the body is untouched"
        );
    }
}

#[cfg(test)]
mod streaming_dim_tests {
    use super::*;

    const PALETTES: &[&str] = &["default", "myx", "night-city", "gruvbox", "forest", "blood"];

    /// Idle and streaming look different: while streaming the body sits
    /// closer to the chrome and typed text is softer — still AA.
    #[test]
    fn streaming_dims_body_and_text() {
        for name in PALETTES {
            let theme = super::super::theme::Theme::builtin_for_test(name);
            let idle = Slab::new(&theme, PromptFx::default(), 80, None);
            let busy = Slab::new(
                &theme,
                PromptFx {
                    dim: 1.0,
                    ..PromptFx::default()
                },
                80,
                None,
            );
            let chrome = rgb_of(theme.bg);
            assert!(
                contrast(busy.body, chrome) < contrast(idle.body, chrome),
                "{name}: body steps back toward the chrome"
            );
            assert!(
                contrast(busy.body, chrome) > 1.02,
                "{name}: but the slab is still there"
            );
            let (ti, tb) = (rgb_of(idle.text_fg()), rgb_of(busy.text_fg()));
            assert_ne!(ti, tb, "{name}: typed text dims");
            assert!(
                contrast(tb, busy.brightest()) >= 4.49,
                "{name}: dimmed text stays AA"
            );
        }
    }

    fn rgb_of(c: Color) -> Rgb {
        match c {
            Color::Rgb(r, g, b) => (r, g, b),
            other => panic!("expected rgb, got {other:?}"),
        }
    }

    #[test]
    fn dim_fades_in_and_out_with_the_stream() {
        let t0 = Instant::now();
        let mut clock = PromptClock::with_enabled(t0, true);
        assert_eq!(clock.frame(t0, false).dim, 0.0, "idle: not dimmed");
        assert_eq!(
            clock.frame(t0, true).dim,
            0.0,
            "stream starts: fade begins at 0"
        );
        let mid = t0 + Duration::from_secs_f32(DIM_FADE / 2.0);
        let d = clock.frame(mid, true).dim;
        assert!(d > 0.2 && d < 0.8, "halfway through the fade: {d}");
        let done = t0 + Duration::from_secs_f32(DIM_FADE + 0.01);
        assert_eq!(clock.frame(done, true).dim, 1.0);

        // Stream ends: fades back, and keeps asking for frames until it has.
        assert_eq!(clock.frame(done, false).dim, 1.0);
        assert!(clock.animating(done, false), "frames for the fade back");
        let back = done + Duration::from_secs_f32(DIM_FADE + 0.01);
        assert_eq!(clock.frame(back, false).dim, 0.0);
        // (The "your turn" cue outlasts the fade; still once both are done.)
        let settled = done + Duration::from_secs_f32(DIM_FADE.max(ARRIVE_DECAY) + 0.01);
        assert!(!clock.animating(settled, false), "still again afterwards");
    }
}

#[cfg(test)]
mod tone_tests {
    use super::*;

    fn rgb_of(c: Color) -> Rgb {
        match c {
            Color::Rgb(r, g, b) => (r, g, b),
            other => panic!("expected rgb, got {other:?}"),
        }
    }

    /// A soft key sits between a key and a word: dimmer than a key, still
    /// brighter than the word beside it, and AA.
    #[test]
    fn soft_key_is_between_key_and_word() {
        for name in ["default", "myx", "night-city", "gruvbox", "nord", "forest"] {
            let theme = super::super::theme::Theme::builtin_for_test(name);
            let slab = Slab::new(&theme, PromptFx::default(), 80, None);
            let bg = slab.brightest();
            let c = |t| contrast(rgb_of(slab.tone_fg(t)), bg);
            let (key, soft, word) = (c(Tone::Key), c(Tone::SoftKey), c(Tone::Word));
            assert!(
                soft < key && soft > word,
                "{name}: {key:.2} > {soft:.2} > {word:.2}"
            );
            assert!(soft >= 4.5, "{name}: soft key {soft:.2}");
        }
    }
}

#[cfg(test)]
mod ready_tests {
    use super::*;

    const PALETTES: &[&str] = &[
        "default",
        "myx",
        "night-city",
        "gruvbox",
        "nord",
        "catppuccin",
        "blood",
        "forest",
    ];

    /// Ready is clearly the more prominent state: the slab stands well off
    /// the chrome when idle and steps back while a turn streams.
    #[test]
    fn ready_slab_stands_out_and_streaming_steps_back() {
        for name in PALETTES {
            let theme = super::super::theme::Theme::builtin_for_test(name);
            let chrome = match theme.bg {
                Color::Rgb(r, g, b) => (r, g, b),
                _ => unreachable!(),
            };
            let ready = Slab::new(&theme, PromptFx::default(), 80, None);
            let busy = Slab::new(
                &theme,
                PromptFx {
                    dim: 1.0,
                    ..PromptFx::default()
                },
                80,
                None,
            );
            let (r, b) = (contrast(ready.body, chrome), contrast(busy.body, chrome));
            assert!(r >= 1.15, "{name}: ready step {r:.3}");
            assert!(r - b >= 0.08, "{name}: ready {r:.3} vs streaming {b:.3}");
        }
    }

    /// When a turn ends the prompt colour washes over the slab, then fades;
    /// the clock asks for frames only while it does.
    #[test]
    fn turn_end_cues_your_turn_then_rests() {
        let t0 = Instant::now();
        let mut clock = PromptClock::with_enabled(t0, true);
        assert_eq!(clock.frame(t0, false).arrive, 0.0, "no cue at launch");
        clock.frame(t0, true);
        let end = t0 + Duration::from_secs(5);
        assert_eq!(clock.frame(end, false).arrive, 1.0, "cue at turn end");
        assert!(clock.animating(end + Duration::from_millis(500), false));
        let later = end + Duration::from_secs_f32(ARRIVE_DECAY + 0.01);
        assert_eq!(clock.frame(later, false).arrive, 0.0);
        assert!(!clock.animating(later, false), "still afterwards");

        let theme = Theme::default();
        let cue = Slab::new(
            &theme,
            PromptFx {
                arrive: 1.0,
                ..PromptFx::default()
            },
            80,
            None,
        );
        let calm = Slab::new(&theme, PromptFx::default(), 80, None);
        assert_ne!(cue.fill_at(10), calm.fill_at(10), "the cue tints the slab");
        assert_ne!(
            Slab::at(&cue.halo, 10),
            calm.backdrop,
            "and glows around it"
        );
    }
}
