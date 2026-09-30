//! Subagent tray: running subagents live inside the neon prompt's slab, on a
//! recessed strip above the input line (the synaps-dash web tray, in the
//! terminal). No box and no title row: the header already counts the agents.
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

/// Rows the tray adds above the prompt for `n` agents: the slab's top rim,
/// then one row per agent (capped, plus "+N more").
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

/// Extend the prompt slab (already painted over `input`) upward over `tray`
/// and put the agents on it. `tray` is the strip directly above `input`,
/// [`tray_height`] rows tall.
pub(crate) fn paint_tray(
    buf: &mut Buffer,
    tray: Rect,
    input: Rect,
    slab: &Slab,
    theme: &Theme,
    snaps: &[SubagentSnap],
    spinner_frame: usize,
) {
    if snaps.is_empty() || tray.height < 2 || tray.width < 12 || input.height < 3 {
        return;
    }
    let backdrop = slab.backdrop();
    let (l, r) = (1u16, tray.width - 2); // the slab's rounded sides
    let tray_at = |rx: u16| mix(backdrop, slab.fill_at(rx), TRAY_DEPTH);

    for y in tray.top()..tray.bottom() {
        put(buf, tray.x, y, " ", backdrop, backdrop);
        put(buf, tray.right() - 1, y, " ", backdrop, backdrop);
    }
    // The slab's top rim now crowns the tray.
    for rx in l..=r {
        let sym = if rx == l {
            "\u{2597}" // ▗
        } else if rx == r {
            "\u{2596}" // ▖
        } else {
            "\u{2584}" // ▄
        };
        put(buf, tray.x + rx, tray.y, sym, tray_at(rx), slab.halo_at(rx));
    }
    for y in tray.y + 1..tray.bottom() {
        put(buf, tray.x + l, y, "\u{2590}", tray_at(l), slab.halo_at(l)); // ▐
        put(buf, tray.x + r, y, "\u{258C}", tray_at(r), slab.halo_at(r)); // ▌
        for rx in l + 1..r {
            put(buf, tray.x + rx, y, " ", tray_at(rx), tray_at(rx));
        }
    }
    // The input's own top rim becomes the half-cell step up from the tray
    // to the brighter body.
    put(
        buf,
        input.x + l,
        input.y,
        "\u{2590}",
        tray_at(l),
        slab.halo_at(l),
    );
    put(
        buf,
        input.x + r,
        input.y,
        "\u{258C}",
        tray_at(r),
        slab.halo_at(r),
    );
    for rx in l + 1..r {
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
    let mut y = tray.y + 1;
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
        assert_eq!(tray_height(1), 2);
        assert_eq!(tray_height(6), 7);
        assert_eq!(tray_height(9), 8, "six rows, +N more, rim");
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

    /// Row of the prompt's text line (column 3 holds ❯) and the slab's top
    /// rim (column 1 holds ▗) above it.
    fn prompt_and_rim(buf: &Buffer) -> (u16, u16) {
        let prompt = (0..buf.area().height)
            .rev()
            .find(|&y| sym(buf, 3, y) == "\u{276f}")
            .expect("prompt ❯");
        let rim = (0..prompt)
            .rev()
            .find(|&y| sym(buf, 1, y) == "\u{2597}")
            .expect("top rim ▗");
        (prompt, rim)
    }

    #[test]
    fn agents_ride_in_a_tray_on_the_prompt_slab() {
        let mut h = TestHarness::boot_with_size(W, H);
        start(&mut h, 1, "chrollo");
        update(&mut h, 1, "\u{2699} read (tool #4)");
        update(&mut h, 1, "reading oneshot.rs");
        start(&mut h, 2, "spike");
        update(&mut h, 2, "$ cargo test -p synaps-tui");
        let buf = h.render().clone();
        let (prompt, rim) = prompt_and_rim(&buf);

        assert_eq!(prompt, rim + 4, "rim, two agent rows, the step, the input");
        let step = prompt - 1;
        assert_eq!(sym(&buf, 1, step), "\u{2590}");
        assert_eq!(
            sym(&buf, 40, step),
            "\u{2584}",
            "half-cell step up to the body"
        );
        assert_eq!(sym(&buf, W - 2, step), "\u{258C}");

        for (y, name, what) in [
            (rim + 1, "chrollo", "reading oneshot.rs"),
            (rim + 2, "spike", "$ cargo test -p synaps-tui"),
        ] {
            let line = row(&buf, y);
            assert!(
                SPINNER_FRAMES.contains(&sym(&buf, 3, y)),
                "running glyph under ❯: {line:?}"
            );
            assert_eq!(sym(&buf, 1, y), "\u{2590}", "slab side: {line:?}");
            assert_eq!(sym(&buf, W - 2, y), "\u{258C}", "slab side: {line:?}");
            assert!(line[..].contains(name) && line.contains(what), "{line:?}");
        }
        assert!(row(&buf, rim + 1).contains("4 tools"));

        // No box anywhere in the band.
        for y in rim..=prompt {
            let line = row(&buf, y);
            for glyph in ['\u{256d}', '\u{256e}', '\u{2570}', '\u{256f}', '\u{2502}'] {
                assert!(!line.contains(glyph), "box glyph {glyph:?} in {line:?}");
            }
        }
    }

    /// A background worker outlives the turn that started it: its progress
    /// reaches the tray through the registry rows, not the ended stream.
    #[test]
    fn a_background_agent_keeps_moving_after_the_turn() {
        use synaps_cli::runtime::subagent::SubagentStatus;
        use synaps_cli::tools::SubagentDisplayRow;
        // The agent's tray row, found by name (independent of tray geometry).
        let agent_row = |buf: &Buffer| {
            (0..buf.area().height)
                .map(|y| row(buf, y))
                .find(|l| l.contains("sleeper-1"))
                .expect("the agent's tray row")
        };
        let mut h = TestHarness::boot_with_size(W, H);
        start(&mut h, 37, "sleeper-1");
        let line = agent_row(&h.render().clone());
        assert!(line.contains("starting"), "{line:?}");

        // The turn is over; only the 1 Hz rows arrive now.
        let rows = |step: &str, tools: u32| {
            SessionEventWire::SubagentRows(vec![SubagentDisplayRow {
                subagent_id: 37,
                agent_name: "sleeper-1".into(),
                status: SubagentStatus::Running,
                cancel_requested: false,
                elapsed_secs: 3.0,
                finished_elapsed: None,
                step: step.into(),
                tools,
            }])
        };
        h.feed_event(rows("$ date +%T; sleep 20; date +%T", 1));
        let line = agent_row(&h.render().clone());
        assert!(line.contains("$ date +%T; sleep 20"), "{line:?}");
        assert!(line.contains("1 tool"), "{line:?}");
        assert!(!line.contains("starting"), "{line:?}");
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
        let (_, rim) = prompt_and_rim(&buf);
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
        let (prompt, rim) = prompt_and_rim(&buf);
        assert_eq!(
            prompt,
            rim + 9,
            "rim, six rows, +N more, the step, the input"
        );
        assert!(row(&buf, rim + 6).contains("agent-6"));
        assert!(row(&buf, rim + 7).contains("+2 more"));
        assert!(!row(&buf, rim + 7).contains("agent-7"));
    }

    #[test]
    fn no_agents_no_tray() {
        let mut h = TestHarness::boot_with_size(W, H);
        let buf = h.render().clone();
        let (prompt, rim) = prompt_and_rim(&buf);
        assert_eq!(prompt, rim + 1, "the plain slab: rim, then the input");
    }

    #[test]
    fn the_tray_meets_the_prompt_even_with_a_download_row() {
        let areas = crate::tui::draw::AppAreas::from_heights(
            ratatui::layout::Rect::new(0, 0, W, H),
            3,
            1,
            3,
        );
        assert_eq!(areas.subagent.bottom(), areas.input.y, "tray on the slab");
        assert_eq!(
            areas.download.bottom(),
            areas.subagent.y,
            "download above it"
        );
    }
}
