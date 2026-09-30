//! Subagent tray: running subagents sit on a recessed tray resting on top
//! of the neon prompt (the synaps-dash web tray, in the terminal). The input
//! box is exactly as it is without agents: same corners, same padding. No
//! title row: the header already counts the agents.
//!
//! The tray is half a cell narrower than the input on each side, and its
//! half-cell bottom padding fills the upper half of the input's top rim row.
//! That keeps every cell to two colours: the input's rim corners (▗ ▖) sit
//! outside the tray, so the only cells the two share are the flat rim between
//! them (tray above, input below). A full-width tray can't do this: its
//! bottom corners would land on the input's corner cells, which would then
//! need three colours (chrome, tray, input).
//!
//! One row per agent, lined up with the prompt: glyph under `❯`, name, what
//! it is doing, tool count, elapsed time. A finished agent dims and shows its
//! result until the HUD's 5 s flash expires; failures and time-outs take the
//! theme's error and warning colours. Only the running spinner moves.
//!
//! Every colour is a theme token or a mix of two, resolved against the tray's
//! own surface so secondary text holds 4.5:1 on every palette.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};

use super::app::SPINNER_FRAMES;
use super::neon_prompt::{color, contrast, mix, rgb, Rgb, Slab, INSET_X};
use super::render_model::SubagentSnap;
use super::text_metrics::char_width;
use super::theme::Theme;

/// Most agent rows the tray shows; the rest collapse into one "+N more" row.
const MAX_ROWS: usize = 6;

/// How far the tray sits from the chrome toward the slab body: recessed, so
/// the input line stays the brightest surface in the band.
const TRAY_DEPTH: f32 = 0.55;

/// Rows the tray adds above the prompt for `n` agents: its top rim, then one
/// row per agent (capped, plus "+N more"). Its bottom padding is the upper
/// half of the input's own rim row.
pub(crate) fn tray_height(n: usize) -> u16 {
    if n == 0 {
        return 0;
    }
    n.min(MAX_ROWS) as u16 + u16::from(n > MAX_ROWS) + 1
}

// ───────────────────────────── colour ──────────────────────────────────────

/// `c` pushed toward `toward` just until it reads at `target` on `bg`.
fn legible(c: Rgb, toward: Rgb, bg: Rgb, target: f32) -> Rgb {
    let mut t = 0.0;
    loop {
        let out = mix(c, toward, t);
        if contrast(out, bg) >= target || t >= 1.0 {
            return out;
        }
        t += 0.04;
    }
}

/// The tray's colours, resolved against its surface.
struct Ink {
    name: Rgb,
    step: Rgb,
    dim: Rgb,
    count: Rgb,
    run: Rgb,
    ok: Rgb,
    err: Rgb,
    warn: Rgb,
}

impl Ink {
    fn on(t: &Theme, bg: Rgb) -> Self {
        let d = Theme::default();
        let text = rgb(t.claude_text, d.claude_text);
        let muted = rgb(t.muted, d.muted);
        let dim = legible(muted, text, bg, 4.5);
        Self {
            name: legible(rgb(t.subagent_name, d.subagent_name), text, bg, 4.5),
            step: mix(dim, text, 0.45),
            dim,
            count: legible(mix(bg, muted, 0.55), dim, bg, 3.0),
            run: legible(rgb(t.status_streaming, d.status_streaming), text, bg, 3.0),
            ok: legible(rgb(t.subagent_done, d.subagent_done), text, bg, 3.0),
            err: legible(rgb(t.error_color, d.error_color), text, bg, 3.0),
            warn: legible(rgb(t.warning_color, d.warning_color), text, bg, 3.0),
        }
    }
}

// ───────────────────────────── one agent ───────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq)]
enum State {
    Running,
    Cancelling,
    Ok,
    Failed,
    TimedOut,
}

/// The HUD keeps state in the status text's leading glyph (stream_handler).
fn state(s: &SubagentSnap) -> State {
    let st = s.status.as_str();
    if !s.done {
        if st.contains("cancelling") {
            State::Cancelling
        } else {
            State::Running
        }
    } else if st.starts_with('\u{2718}') {
        State::Failed
    } else if st.starts_with('\u{26a0}') {
        State::TimedOut
    } else {
        State::Ok
    }
}

/// What the agent is doing (or ended with), without the engine's prefixes.
fn step(s: &SubagentSnap) -> String {
    let st = s.status.trim();
    match state(s) {
        State::Ok | State::Failed => s
            .result
            .clone()
            .filter(|r| !r.is_empty())
            .unwrap_or_else(|| st.chars().skip(2).collect()),
        State::TimedOut => st.chars().skip(2).collect::<String>().trim().to_string(),
        State::Cancelling => "cancelling\u{2026}".into(),
        State::Running => {
            if st.starts_with("starting") || st == "running" {
                "starting\u{2026}".into()
            } else if let Some(rest) = st.strip_prefix("\u{2699} ") {
                // "⚙ <tool> (tool #N)" until the tool's detail arrives.
                rest.rsplit_once(" (tool #")
                    .map(|(tool, _)| tool.to_string())
                    .unwrap_or_else(|| rest.to_string())
            } else if st.contains("thinking") {
                "thinking\u{2026}".into()
            } else {
                st.to_string()
            }
        }
    }
}

fn glyph(s: &SubagentSnap, spinner_frame: usize, ink: &Ink) -> (&'static str, Rgb) {
    match state(s) {
        State::Running => (
            SPINNER_FRAMES[(spinner_frame / 3) % SPINNER_FRAMES.len()],
            ink.run,
        ),
        State::Cancelling => ("\u{25cc}", ink.warn), // ◌
        State::Ok => ("\u{2713}", ink.ok),           // ✓
        State::Failed => ("\u{2717}", ink.err),      // ✗
        State::TimedOut => ("!", ink.warn),
    }
}

fn step_ink(s: &SubagentSnap, ink: &Ink) -> Rgb {
    match state(s) {
        State::Running => ink.step,
        State::Cancelling | State::TimedOut => mix(ink.dim, ink.warn, 0.6),
        State::Failed => mix(ink.dim, ink.err, 0.7),
        State::Ok => ink.dim,
    }
}

fn clock(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    if s < 60 {
        format!("{s}s")
    } else {
        format!("{}m{:02}s", s / 60, s % 60)
    }
}

fn tools_label(n: u32) -> String {
    match n {
        0 => String::new(),
        1 => "1 tool".into(),
        n => format!("{n} tools"),
    }
}

// ───────────────────────────── painting ────────────────────────────────────

fn width(s: &str) -> u16 {
    s.chars().map(|c| char_width(c) as u16).sum()
}

fn put(buf: &mut Buffer, x: u16, y: u16, sym: &str, fg: Rgb, bg: Rgb) {
    if let Some(cell) = buf.cell_mut((x, y)) {
        cell.reset();
        cell.set_symbol(sym).set_fg(color(fg)).set_bg(color(bg));
    }
}

/// Write `s` from `x`, never reaching `max_x`; an ellipsis marks a cut.
/// Returns the column after the last cell written.
#[allow(clippy::too_many_arguments)]
fn text(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    s: &str,
    fg: Rgb,
    bg: Color,
    bold: bool,
    max_x: u16,
) -> u16 {
    let cut = x + width(s) > max_x;
    let limit = if cut { max_x.saturating_sub(1) } else { max_x };
    let mut cx = x;
    let mut tmp = [0u8; 4];
    for ch in s.chars() {
        let w = char_width(ch) as u16;
        if cx + w > limit {
            break;
        }
        if let Some(cell) = buf.cell_mut((cx, y)) {
            cell.set_symbol(ch.encode_utf8(&mut tmp))
                .set_fg(color(fg))
                .set_bg(bg);
            cell.modifier = if bold {
                Modifier::BOLD
            } else {
                Modifier::empty()
            };
        }
        cx += w;
    }
    if cut && cx < max_x {
        if let Some(cell) = buf.cell_mut((cx, y)) {
            cell.set_symbol("\u{2026}").set_fg(color(fg)).set_bg(bg);
            cell.modifier = Modifier::empty();
        }
        cx += 1;
    }
    cx
}

/// Right-align `s` to end at `right` (exclusive); returns where it starts.
fn text_right(buf: &mut Buffer, right: u16, y: u16, s: &str, fg: Rgb, bg: Color) -> u16 {
    let x = right.saturating_sub(width(s));
    text(buf, x, y, s, fg, bg, false, right);
    x
}

/// glyph · name · step ········ tools  time, from `x` to `right`.
#[allow(clippy::too_many_arguments)]
fn row(
    buf: &mut Buffer,
    x: u16,
    right: u16,
    y: u16,
    s: &SubagentSnap,
    name_w: u16,
    spinner_frame: usize,
    ink: &Ink,
    bg: Color,
) {
    let (g, gc) = glyph(s, spinner_frame, ink);
    text(buf, x, y, g, gc, bg, true, right);
    let name_x = x + 2;
    let name_ink = if s.done { ink.dim } else { ink.name };
    text(
        buf,
        name_x,
        y,
        &s.name,
        name_ink,
        bg,
        !s.done,
        name_x + name_w,
    );
    let step_x = name_x + name_w + 2;

    let time_x = text_right(buf, right, y, &clock(s.elapsed_secs), ink.dim, bg);
    let mut end = time_x.saturating_sub(2);
    // The tool count goes first when the row is narrow.
    if right.saturating_sub(x) >= 64 {
        let tools = tools_label(s.tools);
        if !tools.is_empty() {
            end = text_right(buf, end, y, &tools, ink.count, bg).saturating_sub(2);
        }
    }
    if step_x + 4 < end {
        text(buf, step_x, y, &step(s), step_ink(s, ink), bg, false, end);
    }
}

/// Paint the tray over `tray` (the strip directly above `input`,
/// [`tray_height`] rows tall) after the prompt's slab is painted. The tray
/// spans the input's inner columns (half a cell in from its rounded sides);
/// its bottom padding fills the upper half of the input's rim row.
pub(crate) fn paint_tray(
    buf: &mut Buffer,
    tray: Rect,
    input: Rect,
    slab: &Slab,
    theme: &Theme,
    snaps: &[SubagentSnap],
    spinner_frame: usize,
) {
    if snaps.is_empty()
        || tray.height < 2
        || tray.width < 12
        || input.height < 3
        || tray.bottom() != input.y
    {
        return;
    }
    let backdrop = slab.backdrop();
    // The input's rounded sides are at columns 1 and width-2; the tray fills
    // the columns between them.
    let (l, r) = (2u16, tray.width - 3);
    let tray_at = |rx: u16| mix(backdrop, slab.fill_at(rx), TRAY_DEPTH);
    let top = tray.y;

    for y in tray.top()..tray.bottom() {
        put(buf, tray.x, y, " ", backdrop, backdrop);
        put(buf, tray.right() - 1, y, " ", backdrop, backdrop);
        put(buf, tray.x + 1, y, " ", slab.halo_at(1), slab.halo_at(1));
        let rr = tray.width - 2;
        put(buf, tray.x + rr, y, " ", slab.halo_at(rr), slab.halo_at(rr));
    }
    // Top rim: the tray's upper half-cell of padding.
    for rx in l..=r {
        put(
            buf,
            tray.x + rx,
            top,
            "\u{2584}",
            tray_at(rx),
            slab.halo_at(rx),
        ); // ▄
    }
    for y in top + 1..tray.bottom() {
        for rx in l..=r {
            put(buf, tray.x + rx, y, " ", tray_at(rx), tray_at(rx));
        }
    }
    // Bottom padding: the upper half of the input's rim row, between its
    // corners (which stay as the slab drew them).
    for rx in l..=r {
        put(
            buf,
            input.x + rx,
            input.y,
            "\u{2584}",
            slab.fill_at(rx),
            tray_at(rx),
        );
    }

    // Rows, lined up with the prompt: glyph under ❯, text to the input's end.
    let surface = tray_at(tray.width / 2);
    let ink = Ink::on(theme, surface);
    let bg = color(surface);
    let x = tray.x + INSET_X;
    let right = tray.right().saturating_sub(INSET_X + 1);
    let name_w = snaps
        .iter()
        .map(|s| width(&s.name))
        .max()
        .unwrap_or(4)
        .clamp(4, 16);
    let mut y = top + 1;
    for s in snaps.iter().take(MAX_ROWS) {
        if y >= tray.bottom() {
            return;
        }
        row(buf, x, right, y, s, name_w, spinner_frame, &ink, bg);
        y += 1;
    }
    if snaps.len() > MAX_ROWS && y < tray.bottom() {
        let more = format!("+{} more", snaps.len() - MAX_ROWS);
        text(buf, x + 2, y, &more, ink.dim, bg, false, right);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::neon_prompt::PromptFx;

    fn snap(name: &str, status: &str, done: bool) -> SubagentSnap {
        SubagentSnap {
            name: name.into(),
            status: status.into(),
            elapsed_secs: 12.0,
            done,
            tools: 0,
            result: None,
        }
    }

    #[test]
    fn height_is_rim_plus_rows_capped() {
        assert_eq!(tray_height(0), 0);
        assert_eq!(tray_height(1), 2, "top rim, one row");
        assert_eq!(tray_height(6), 7);
        assert_eq!(tray_height(9), 8, "top rim, six rows, +N more");
    }

    #[test]
    fn step_drops_the_engine_prefixes() {
        assert_eq!(
            step(&snap("a", "starting: do it", false)),
            "starting\u{2026}"
        );
        assert_eq!(step(&snap("a", "\u{2699} bash (tool #3)", false)), "bash");
        assert_eq!(step(&snap("a", "$ cargo test", false)), "$ cargo test");
        assert_eq!(
            step(&snap("a", "\u{1F4AD} thinking...", false)),
            "thinking\u{2026}"
        );
        assert_eq!(step(&snap("a", "\u{26a0} timed out", true)), "timed out");
        let mut done = snap("a", "\u{2714} short preview", true);
        assert_eq!(step(&done), "short preview");
        done.result = Some("the full first line".into());
        assert_eq!(step(&done), "the full first line");
    }

    #[test]
    fn clock_reads_like_the_web_tray() {
        assert_eq!(clock(8.4), "8s");
        assert_eq!(clock(64.0), "1m04s");
    }

    /// The tray is recessed (between the chrome and the ready body) and every
    /// word on it reads at 4.5:1 (names, steps, times) or 3:1 (glyphs, the
    /// tool count), on every builtin palette.
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

    #[test]
    fn tray_is_recessed_and_legible_on_every_builtin() {
        for &name in BUILTINS {
            let theme = Theme::builtin_for_test(name);
            let slab = Slab::new(&theme, PromptFx::default(), 80, None);
            let backdrop = slab.backdrop();
            let body = slab.fill_at(40);
            let tray = mix(backdrop, body, TRAY_DEPTH);
            let (lb, lt, lo) = (
                crate::tui::neon_prompt::luminance(backdrop),
                crate::tui::neon_prompt::luminance(tray),
                crate::tui::neon_prompt::luminance(body),
            );
            assert!(
                lb.min(lo) <= lt && lt <= lb.max(lo),
                "{name}: tray between chrome and body"
            );
            let ink = Ink::on(&theme, tray);
            for (what, c, min) in [
                ("name", ink.name, 4.5),
                ("step", ink.step, 4.5),
                ("dim", ink.dim, 4.5),
                ("count", ink.count, 3.0),
                ("run", ink.run, 3.0),
                ("ok", ink.ok, 3.0),
                ("err", ink.err, 3.0),
                ("warn", ink.warn, 3.0),
            ] {
                let got = contrast(c, tray);
                assert!(got >= min - 0.01, "{name}: {what} {got:.2} < {min}");
            }
        }
    }
}

/// End to end: events through the production stream arm, then a real frame.
#[cfg(test)]
mod render_tests {
    use crate::tui::app::SPINNER_FRAMES;
    use crate::tui::testing::TestHarness;
    use agent_engine::session::SessionEventWire;
    use ratatui::buffer::Buffer;
    use synaps_cli::{AgentEvent, StreamEvent};

    const W: u16 = 80;
    const H: u16 = 24;

    fn sym(buf: &Buffer, x: u16, y: u16) -> &str {
        buf[(x, y)].symbol()
    }

    fn row(buf: &Buffer, y: u16) -> String {
        (0..buf.area().width).map(|x| sym(buf, x, y)).collect()
    }

    fn agent(h: &mut TestHarness, ev: AgentEvent) {
        h.feed_event(SessionEventWire::Stream(StreamEvent::Agent(ev)));
    }

    fn start(h: &mut TestHarness, id: u64, name: &str) {
        agent(
            h,
            AgentEvent::SubagentStart {
                subagent_id: id,
                agent_name: name.into(),
                task_preview: format!("task for {name}"),
            },
        );
    }

    fn update(h: &mut TestHarness, id: u64, status: &str) {
        agent(
            h,
            AgentEvent::SubagentUpdate {
                subagent_id: id,
                agent_name: String::new(),
                status: status.into(),
            },
        );
    }

    fn done(h: &mut TestHarness, id: u64, preview: &str) {
        agent(
            h,
            AgentEvent::SubagentDone {
                subagent_id: id,
                agent_name: String::new(),
                result_preview: preview.into(),
                duration_secs: 10.5,
            },
        );
    }

    /// The prompt's text row (column 3 holds ❯).
    fn prompt_row(buf: &Buffer) -> u16 {
        (0..buf.area().height)
            .rev()
            .find(|&y| sym(buf, 3, y) == "\u{276f}")
            .expect("prompt ❯")
    }

    /// (top rim, last agent row) of the tray, or None. The tray shows as
    /// the colour above the input's rim between its corners; its top rim is
    /// the ▄ row above the agent rows.
    fn tray_rows(buf: &Buffer) -> Option<(u16, u16)> {
        let rim = prompt_row(buf) - 1;
        assert_eq!(sym(buf, 1, rim), "\u{2597}", "the input's own corner");
        let tray_bg = buf[(2, rim)].bg;
        if tray_bg == buf[(0, rim)].bg {
            return None;
        }
        let mut top = rim - 1;
        while buf[(2, top)].symbol() == " " && buf[(2, top)].bg == tray_bg {
            top -= 1;
        }
        assert_eq!(sym(buf, 2, top), "\u{2584}", "the tray's top rim");
        assert_eq!(buf[(2, top)].fg, tray_bg, "the top rim is tray-coloured");
        Some((top, rim - 1))
    }

    fn cell(
        buf: &Buffer,
        x: u16,
        y: u16,
    ) -> (String, ratatui::style::Color, ratatui::style::Color) {
        let c = &buf[(x, y)];
        (c.symbol().to_string(), c.fg, c.bg)
    }

    #[test]
    fn agents_ride_on_a_tray_resting_on_the_prompt() {
        let mut h = TestHarness::boot_with_size(W, H);
        start(&mut h, 1, "chrollo");
        update(&mut h, 1, "\u{2699} read (tool #4)");
        update(&mut h, 1, "reading oneshot.rs");
        start(&mut h, 2, "spike");
        update(&mut h, 2, "$ cargo test -p synaps-tui");
        let buf = h.render().clone();
        let (top, last) = tray_rows(&buf).expect("a tray");
        assert_eq!(last, top + 2, "top rim, two agent rows");
        assert_eq!(prompt_row(&buf), last + 2, "the input's rim, then ❯");

        for (y, name, what) in [
            (top + 1, "chrollo", "reading oneshot.rs"),
            (top + 2, "spike", "$ cargo test -p synaps-tui"),
        ] {
            let line = row(&buf, y);
            assert!(
                SPINNER_FRAMES.contains(&sym(&buf, 3, y)),
                "running glyph under ❯: {line:?}"
            );
            assert!(line.contains(name) && line.contains(what), "{line:?}");
        }
        assert!(row(&buf, top + 1).contains("4 tools"));

        // No box-drawing anywhere in the band.
        for y in top..=prompt_row(&buf) {
            let line = row(&buf, y);
            for glyph in ['\u{256d}', '\u{256e}', '\u{2570}', '\u{256f}', '\u{2502}'] {
                assert!(!line.contains(glyph), "box glyph {glyph:?} in {line:?}");
            }
        }
    }

    /// The input box is exactly as without agents (corners, padding, every
    /// row) except that the chrome above its flat top edge is now the tray;
    /// the tray's own edges are straight, half a cell in from the input's.
    #[test]
    fn the_input_is_unchanged_and_the_tray_is_a_clean_rectangle() {
        let mut h = TestHarness::boot_with_size(W, H);
        start(&mut h, 1, "sleeper-1");
        start(&mut h, 2, "sleeper-2");
        let buf = h.render().clone();
        let mut plain = TestHarness::boot_with_size(W, H);
        let plain = plain.render().clone();
        assert!(tray_rows(&plain).is_none());

        let prompt = prompt_row(&buf);
        assert_eq!(prompt_row(&plain), prompt, "the input doesn't move");
        let rim = prompt - 1;
        let bottom = (prompt..H)
            .find(|&y| sym(&buf, 1, y) == "\u{259D}")
            .expect("bottom rim");
        // Every input row below its rim: identical.
        for y in prompt..=bottom {
            for x in 0..W {
                assert_eq!(cell(&buf, x, y), cell(&plain, x, y), "input ({x},{y})");
            }
        }
        // The rim: identical corners and outside; between them the same ▄ in
        // the body colour, only the upper half (the tray's padding) differs.
        let (top, _) = tray_rows(&buf).expect("a tray");
        let tray_bg = buf[(2, top + 1)].bg;
        for x in 0..W {
            let (got, want) = (cell(&buf, x, rim), cell(&plain, x, rim));
            if (2..=W - 3).contains(&x) {
                assert_eq!((&got.0, got.1), (&want.0, want.1), "rim ({x})");
                assert_eq!(got.2, tray_bg, "tray padding above the rim ({x})");
            } else {
                assert_eq!(got, want, "rim corner/outside ({x})");
            }
        }
        // The tray: straight edges at columns 2 and W-3, chrome outside.
        let chrome = buf[(0, top + 1)].bg;
        for y in top + 1..rim {
            assert_eq!(buf[(2, y)].bg, tray_bg, "left edge at {y}");
            assert_eq!(buf[(W - 3, y)].bg, tray_bg, "right edge at {y}");
            assert_eq!(buf[(1, y)].bg, chrome, "chrome outside at {y}");
            assert_eq!(buf[(W - 2, y)].bg, chrome, "chrome outside at {y}");
        }
        for x in 2..=W - 3 {
            assert_eq!(
                cell(&buf, x, top),
                ("\u{2584}".into(), tray_bg, chrome),
                "top rim ({x})"
            );
        }
        assert_eq!(buf[(1, top)].bg, chrome);
        assert_eq!(buf[(1, top)].symbol(), " ", "no tray outside its columns");
    }

    #[test]
    fn finished_agents_show_their_result_and_state() {
        let mut h = TestHarness::boot_with_size(W, H);
        start(&mut h, 1, "shady");
        done(&mut h, 1, "ERROR: provider request failed [rate_limit]");
        start(&mut h, 2, "inline");
        done(&mut h, 2, "## Summary\n\nRelease checklist found.\nmore");
        start(&mut h, 3, "gif-recorder");
        done(&mut h, 3, "[TIMED OUT after 30s — partial results below]");
        let buf = h.render().clone();
        let (rim, _) = tray_rows(&buf).expect("a tray");
        let failed = row(&buf, rim + 1);
        assert_eq!(sym(&buf, 3, rim + 1), "\u{2717}", "{failed:?}");
        assert!(
            failed.contains("provider request failed [rate_limit]"),
            "{failed:?}"
        );
        assert!(!failed.contains("ERROR"), "{failed:?}");
        let ok = row(&buf, rim + 2);
        assert_eq!(sym(&buf, 3, rim + 2), "\u{2713}", "{ok:?}");
        assert!(
            ok.contains("Release checklist found."),
            "first real line: {ok:?}"
        );
        let timed = row(&buf, rim + 3);
        assert_eq!(sym(&buf, 3, rim + 3), "!", "{timed:?}");
        assert!(timed.contains("timed out"), "{timed:?}");
        assert!(ok.contains("10s"), "duration: {ok:?}");
    }

    #[test]
    fn more_than_six_agents_collapse_into_a_count() {
        let mut h = TestHarness::boot_with_size(W, H);
        for id in 1..=8 {
            start(&mut h, id, &format!("agent-{id}"));
        }
        let buf = h.render().clone();
        let (rim, last) = tray_rows(&buf).expect("a tray");
        assert_eq!(last, rim + 7, "top rim, six rows, +N more");
        assert_eq!(prompt_row(&buf), last + 2);
        assert!(row(&buf, rim + 6).contains("agent-6"));
        assert!(row(&buf, rim + 7).contains("+2 more"));
        assert!(!row(&buf, rim + 7).contains("agent-7"));
    }

    #[test]
    fn no_agents_no_tray() {
        let mut h = TestHarness::boot_with_size(W, H);
        let buf = h.render().clone();
        assert!(tray_rows(&buf).is_none(), "the plain slab only");
    }

    #[test]
    fn the_tray_meets_the_prompt_even_with_a_download_row() {
        let areas = crate::tui::draw::AppAreas::from_heights(
            ratatui::layout::Rect::new(0, 0, W, H),
            3,
            1,
            3,
        );
        assert_eq!(
            areas.subagent.bottom(),
            areas.input.y,
            "tray right on the prompt"
        );
        assert_eq!(
            areas.download.bottom(),
            areas.subagent.y,
            "download above it"
        );
    }
}
