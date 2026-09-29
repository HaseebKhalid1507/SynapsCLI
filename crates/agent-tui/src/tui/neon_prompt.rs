//! Neon prompt: the input drawn as a soft-edged slab of light instead of a
//! bordered box.
//!
//! The shape is made only of half-cell blocks (`▗▄▖ ▐ ▌ ▝▀▘`), so there is no
//! line art. Its fill is one flat, faint tint of the theme's prompt colour,
//! floating on the chrome band it shares with the footer.
//!
//! At rest it is completely still (no frames at all). Motion only answers
//! something happening: the cursor is a lit block whose glow trails behind
//! typing, sending flashes the slab, and while a turn streams the colour calms
//! and a shimmer sweeps across with a glow under it.
//!
//! Split in two:
//! - [`PromptClock`] lives on `App` (main task) and turns input/stream events
//!   into a pure per-frame [`PromptFx`] snapshot carried by the `RenderModel`.
//! - [`Slab`] + the `paint_*` functions draw from that snapshot on the render
//!   side and never read `App`.
//!
//! `SYNAPS_NO_BOOT_FX=1` (the existing "no animations" switch) freezes the
//! prompt at its resting look.

use std::time::{Duration, Instant};

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier},
};

use super::theme::Theme;

type Rgb = (u8, u8, u8);

const WHITE: Rgb = (255, 255, 255);

/// Columns between the input area's edge and the text column's prompt glyph:
/// canvas margin, half-block side, padding.
pub(crate) const INSET_X: u16 = 3;

const SHIMMER_PERIOD: f32 = 1.9;
const TRAIL_DECAY: f32 = 0.7;
const PULSE_DECAY: f32 = 0.45;
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
}

/// Input/stream event times → per-frame [`PromptFx`]. Lives on `App`.
#[derive(Debug)]
pub(crate) struct PromptClock {
    enabled: bool,
    last_key: Instant,
    last_send: Instant,
    last_frame: Instant,
    stream_since: Instant,
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
        self.enabled && (streaming || self.trail(now) > 0.0 || self.pulse(now) > 0.0)
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
            if streaming {
                self.stream_since = now;
            }
        }
        let streamed = Self::secs(now, self.stream_since);
        let stream_secs = if streaming { streamed } else { 0.0 };
        if !self.enabled {
            return PromptFx {
                stream_secs,
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
        }
    }
}

// ───────────────────────────── colours (render side) ───────────────────────

/// One frame's colours for an input area of a given width.
pub(crate) struct Slab {
    /// What the slab floats on: the chrome (`bg`), full width, continuous
    /// with the footer row below.
    backdrop: Rgb,
    canvas: Rgb,
    text: Rgb,
    prompt: Rgb,
    muted: Rgb,
    stream: Rgb,
    streaming: bool,
    fx: PromptFx,
    /// Fill of the slab body, per column.
    fill: Vec<Rgb>,
    /// Halo under the half-cell edges, per column.
    halo: Vec<Rgb>,
    /// The colour the fill is tinted toward, per column: the prompt colour,
    /// or the shimmer while streaming (cursor + glow source).
    tint: Vec<Rgb>,
}

impl Slab {
    pub(crate) fn new(theme: &Theme, fx: PromptFx, streaming: bool, width: u16) -> Self {
        let d = Theme::default();
        let backdrop = rgb(theme.bg, d.bg);
        let canvas = rgb(theme.message_background(), d.message_bg);
        let text = rgb(theme.input_fg, d.input_fg);
        let prompt = rgb(theme.prompt_fg, d.prompt_fg);
        let muted = rgb(theme.muted, d.muted);
        let stream = rgb(theme.status_streaming, d.status_streaming);

        let base = mix(backdrop, text, 0.025);
        let w = usize::from(width.max(1));
        let wf = w as f32;
        let sweep = fx.shimmer * (wf + 40.0) - 20.0;
        let (mut fill, mut halo, mut tint) = (
            Vec::with_capacity(w),
            Vec::with_capacity(w),
            Vec::with_capacity(w),
        );
        for x in 0..w {
            let mut t = prompt;
            // `glow` is light spilling onto the band around the slab: only
            // under the moving sweep or a send flash. At rest it is 0, so the
            // band is exactly the footer's colour.
            let (amount, glow) = if streaming {
                let dist = (x as f32 - sweep) / 9.0;
                let s = (-dist * dist).exp();
                t = mix(mix(t, muted, 0.45), stream, s * 0.8);
                (0.075 + 0.085 * s, 0.12 * s)
            } else {
                (0.13, 0.0)
            };
            tint.push(t);
            fill.push(mix(base, t, amount + 0.14 * fx.pulse));
            halo.push(mix(backdrop, t, glow + 0.10 * fx.pulse));
        }
        Self {
            backdrop,
            canvas,
            text,
            prompt,
            muted,
            stream,
            streaming,
            fx,
            fill,
            halo,
            tint,
        }
    }

    fn at(v: &[Rgb], x: u16) -> Rgb {
        v[usize::from(x).min(v.len() - 1)]
    }

    /// Body fill at column `x` (relative to the input area).
    pub(crate) fn fill_at(&self, x: u16) -> Rgb {
        Self::at(&self.fill, x)
    }

    /// Nudge `c` toward the text colour until it reads at `target` on `bg`.
    fn legible(&self, c: Rgb, bg: Rgb, target: f32) -> Rgb {
        let toward = mix(self.text, WHITE, 0.3);
        let mut t = 0.0;
        loop {
            let out = mix(c, toward, t);
            if contrast(out, bg) >= target || t >= 1.0 {
                return out;
            }
            t += 0.05;
        }
    }

    /// Cursor block colour at column `x`.
    fn cursor(&self, x: u16) -> Rgb {
        if self.streaming {
            mix(self.prompt, self.text, 0.35)
        } else {
            mix(Self::at(&self.tint, x), WHITE, 0.25)
        }
    }

    /// How much of the cursor colour bleeds into the cell `dx` columns away.
    fn glow(&self, dx: i32) -> f32 {
        let trail = self.fx.trail;
        let k = 0.35 + 0.65 * trail;
        match dx {
            -1 => 0.26 * k,
            -2 => 0.14 * k,
            -3 => 0.07 * k * trail.max(0.3),
            -4 => 0.035 * trail,
            1 => 0.09 * k,
            _ => 0.0,
        }
    }

    /// The body colour text is hardest to read on: the brightest column of
    /// the fill. Foreground colours are made legible against it, so they
    /// hold everywhere, including under the shimmer.
    fn brightest(&self) -> Rgb {
        self.fill
            .iter()
            .copied()
            .max_by(|a, b| luminance(*a).total_cmp(&luminance(*b)))
            .unwrap_or(self.canvas)
    }

    /// Prompt glyph colour (ready).
    pub(crate) fn prompt_fg(&self) -> Color {
        color(self.legible(self.prompt, self.brightest(), 4.5))
    }

    /// Spinner colour while streaming.
    pub(crate) fn spinner_fg(&self) -> Color {
        color(self.legible(self.stream, self.brightest(), 4.5))
    }

    /// Placeholder text colour (italic).
    pub(crate) fn placeholder_fg(&self) -> Color {
        let bg = self.brightest();
        color(self.legible(mix(bg, self.text, 0.55), bg, 4.5))
    }

    /// Ghost-completion colour: deliberately dimmer than typed text, never
    /// below 3.6:1.
    pub(crate) fn ghost_fg(&self) -> Color {
        let bg = self.brightest();
        color(self.legible(mix(bg, self.text, 0.45), bg, 3.6))
    }

    /// Scroll-arrow colour in the prompt column.
    pub(crate) fn arrow_fg(&self) -> Color {
        color(self.legible(self.muted, self.brightest(), 4.5))
    }

    /// Typed input: the theme's input colour, lifted only if a bright tint
    /// would otherwise wash it out.
    pub(crate) fn text_fg(&self) -> Color {
        color(self.legible(self.text, self.brightest(), 4.5))
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
        let lit = cursor
            .filter(|c| c.y == y)
            .map(|c| (c.x, slab.cursor(c.x - area.x)));
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
    let rel = at.x - area.x;
    let (fill, cur) = (slab.fill_at(rel), slab.cursor(rel));
    if let Some(cell) = buf.cell_mut((at.x, at.y)) {
        let sym = if cell.symbol().trim().is_empty() {
            " ".to_string()
        } else {
            cell.symbol().to_string()
        };
        cell.set_symbol(&sym).set_fg(color(fill)).set_bg(color(cur));
    }
}

/// A status tab hanging off the bottom rim, right-aligned inside the slab.
/// `accent_streaming` picks the stream colour, otherwise the prompt colour.
/// Skipped when it doesn't fit.
pub(crate) fn paint_tab(
    buf: &mut Buffer,
    area: Rect,
    slab: &Slab,
    text: &str,
    accent_streaming: bool,
) {
    let len = super::text_metrics::width(text) as u16;
    if area.height < 3 || area.width < len + 8 {
        return;
    }
    let accent = if accent_streaming {
        slab.stream
    } else {
        slab.prompt
    };
    let y = area.bottom() - 1;
    let r = area.width - 2;
    let mut rx = r - 2 - len;
    for ch in text.chars() {
        let bg = mix(slab.fill_at(rx), accent, 0.16);
        let fg = slab.legible(mix(accent, WHITE, 0.1), bg, 4.5);
        let mut tmp = [0u8; 4];
        put(
            buf,
            area.x + rx,
            y,
            ch.encode_utf8(&mut tmp),
            color(fg),
            color(bg),
        );
        rx += super::text_metrics::char_width(ch) as u16;
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
        ];
        for i in 0..10 {
            out.push((
                PromptFx {
                    shimmer: i as f32 / 10.0,
                    ..PromptFx::default()
                },
                true,
            ));
        }
        out
    }

    #[test]
    fn typed_text_placeholder_and_prompt_stay_legible_on_every_palette() {
        for name in BUILTINS {
            let theme = super::super::theme::Theme::builtin_for_test(name);
            for (fx, streaming) in frames() {
                let slab = Slab::new(&theme, fx, streaming, 100);
                for x in [3u16, 5, 30, 60, 94] {
                    let bg = slab.fill_at(x);
                    let text = contrast(rgb_of(slab.text_fg()), bg);
                    assert!(
                        text >= 4.5,
                        "{name}: text {text:.2} at x={x} streaming={streaming}"
                    );
                    for (what, fg, min) in [
                        ("placeholder", slab.placeholder_fg(), 4.5),
                        ("prompt", slab.prompt_fg(), 4.5),
                        ("spinner", slab.spinner_fg(), 4.5),
                        ("ghost", slab.ghost_fg(), 3.6),
                        ("arrow", slab.arrow_fg(), 4.5),
                    ] {
                        let c = contrast(rgb_of(fg), bg);
                        assert!(c >= min - 0.01, "{name}: {what} {c:.2} at x={x}");
                    }
                }
            }
        }
    }

    #[test]
    fn ghost_reads_dimmer_than_typed_text() {
        for name in BUILTINS {
            let theme = super::super::theme::Theme::builtin_for_test(name);
            let slab = Slab::new(&theme, PromptFx::default(), false, 80);
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
            let slab = Slab::new(&theme, PromptFx::default(), false, 80);
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
                ..PromptFx::default()
            },
            "no motion when disabled; only the elapsed clock"
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
            let slab = Slab::new(&theme, PromptFx::default(), false, 100);
            let first = slab.fill_at(0);
            assert!(
                (0..100).all(|x| slab.fill_at(x) == first),
                "{name}: fill varies across the slab"
            );
        }
    }
}
