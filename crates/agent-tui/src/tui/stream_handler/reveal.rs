//! Smooth streaming: reveal a burst of streamed text over a few frames.
//!
//! A provider does not always stream at an even pace. With summarised
//! thinking, the Anthropic API sends the thought's summary late and in
//! clumps, then flushes the answer it generated meanwhile in one go: a
//! kilobyte of agent text lands in one instant, and the transcript painted
//! it in one frame. The [`Reveal`] queue sits between the session arm and
//! the transcript and hands the text on at an adaptive pace:
//!
//! - **Rate** `BASE_CPS + backlog / CATCH_UP` chars a second: about the
//!   normal streaming pace when little is queued, faster the more is (the
//!   backlog decays exponentially), so a burst eases in over ~0.3–0.4 s and
//!   steady streaming lags by well under one `CATCH_UP`.
//! - **Hard bound**: text queued [`MAX_LAG`] ago is released whole, so the
//!   view never trails the stream by more than that.
//! - **Flush**: everything else the session sends (turn end, abort, tool
//!   calls, a reset, a new turn, a snapshot) releases the queue first, in
//!   order, so the transcript is never truncated, reordered or delayed past
//!   such an event; so do scroll/expand/abort input, a session switch, and
//!   turning the setting off.
//!
//! The transcript only ever sees ordinary (smaller) deltas through
//! `append_or_update_text` / `append_or_update_thinking`, so its measure-is-
//! render guarantees, slot cache and live set are untouched; releases happen
//! once per main-loop iteration, just before a frame is built (no extra
//! renders beyond the frame cadence). Thinking text goes through the same
//! queue ahead of the answer that followed it, so a thought that arrives
//! with the answer reads in first instead of popping in finished beside it.
//!
//! Settings → Appearance → "Smooth streaming" (`tui_smooth_streaming`, on
//! by default); off = every delta paints as it arrives, as before.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use synaps_cli::{LlmEvent, StreamEvent};

use super::super::app::App;

/// Reveal rate with nothing queued: about a model's normal streaming pace
/// (Opus streams ~80 tokens/s, ~320 chars/s).
const BASE_CPS: f64 = 320.0;
/// The backlog's decay time: each second the rate adds `backlog / CATCH_UP`.
/// 0.12 s drains a 1 KB burst in ~0.4 s (`CATCH_UP · ln(1 + B/(BASE·CATCH_UP))`).
const CATCH_UP: f64 = 0.12;
/// Nothing stays queued longer than this.
pub(crate) const MAX_LAG: Duration = Duration::from_millis(500);
/// What the first release after an idle queue may reveal: one frame's worth
/// (the queue has no previous release to measure from).
const FIRST_STEP: Duration = Duration::from_millis(16);

/// Which transcript call a queued chunk feeds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Thinking,
    Text,
}

struct Chunk {
    kind: Kind,
    text: String,
    /// When it arrived (the [`MAX_LAG`] bound).
    at: Instant,
}

/// The queue of streamed text not yet shown, oldest first.
#[derive(Default)]
pub(crate) struct Reveal {
    queue: VecDeque<Chunk>,
    /// Chars queued (the rate's backlog).
    chars: usize,
    /// The previous release while the queue was non-empty; `None` when idle.
    last: Option<Instant>,
}

impl Reveal {
    /// Whether anything is waiting to be shown.
    pub(crate) fn pending(&self) -> bool {
        !self.queue.is_empty()
    }

    /// Chars waiting to be shown.
    #[cfg_attr(not(any(test, feature = "testing")), allow(dead_code))]
    pub(crate) fn backlog(&self) -> usize {
        self.chars
    }

    /// Queue `text` (arrived at `now`) behind what is already waiting.
    pub(crate) fn push(&mut self, kind: Kind, text: &str, now: Instant) {
        if text.is_empty() {
            return;
        }
        self.chars += text.chars().count();
        match self.queue.back_mut() {
            // Same kind, same instant (a burst): one chunk.
            Some(c) if c.kind == kind && c.at == now => c.text.push_str(text),
            _ => self.queue.push_back(Chunk {
                kind,
                text: text.to_string(),
                at: now,
            }),
        }
    }

    /// What is due by `now`, in order, adjacent same-kind runs merged (one
    /// transcript call each). Idempotent at a fixed instant: a second call
    /// at the same `now` releases only what the bound forces.
    pub(crate) fn take(&mut self, now: Instant) -> Vec<(Kind, String)> {
        let mut out: Vec<(Kind, String)> = Vec::new();
        if self.queue.is_empty() {
            self.last = None;
            return out;
        }
        let step = match self.last {
            Some(last) => now.saturating_duration_since(last),
            None => FIRST_STEP,
        };
        let rate = BASE_CPS + self.chars as f64 / CATCH_UP;
        let mut budget = (step.as_secs_f64() * rate).ceil() as usize;
        self.last = Some(now);
        while let Some(front) = self.queue.front_mut() {
            let overdue = now.saturating_duration_since(front.at) >= MAX_LAG;
            let n = front.text.chars().count();
            let (kind, text) = if overdue || n <= budget {
                budget = budget.saturating_sub(n);
                self.chars -= n;
                let c = self.queue.pop_front().expect("front exists");
                (c.kind, c.text)
            } else if budget > 0 {
                let cut = front
                    .text
                    .char_indices()
                    .nth(budget)
                    .map_or(front.text.len(), |(i, _)| i);
                let rest = front.text.split_off(cut);
                let head = std::mem::replace(&mut front.text, rest);
                self.chars -= budget;
                budget = 0;
                (front.kind, head)
            } else {
                break;
            };
            match out.last_mut() {
                Some((k, s)) if *k == kind => s.push_str(&text),
                _ => out.push((kind, text)),
            }
        }
        if self.queue.is_empty() {
            self.last = None;
        }
        out
    }

    /// Everything queued, in order (a flush).
    pub(crate) fn drain(&mut self) -> Vec<(Kind, String)> {
        let mut out: Vec<(Kind, String)> = Vec::new();
        for c in self.queue.drain(..) {
            match out.last_mut() {
                Some((k, s)) if *k == c.kind => s.push_str(&c.text),
                _ => out.push((c.kind, c.text)),
            }
        }
        self.chars = 0;
        self.last = None;
        out
    }
}

/// The text a stream event carries, if it is one the queue paces.
pub(crate) fn paced(event: &StreamEvent) -> Option<(Kind, &str)> {
    match event {
        StreamEvent::Llm(LlmEvent::Text(t)) => Some((Kind::Text, t.as_str())),
        StreamEvent::Llm(LlmEvent::Thinking(t)) => Some((Kind::Thinking, t.as_str())),
        _ => None,
    }
}

/// Whether an input event must see the transcript whole first: anything but
/// typing into the editor (and bare pointer motion). Scrolling, Ctrl+O,
/// Esc/abort, a click or drag (selection), Enter (a steer), a resize — the
/// view they act on is the final one.
pub(crate) fn input_flushes(event: &crossterm::event::Event) -> bool {
    use crossterm::event::{Event, KeyCode, KeyModifiers, MouseEventKind};
    match event {
        Event::Key(k) => {
            let editing = matches!(
                k.code,
                KeyCode::Char(_)
                    | KeyCode::Backspace
                    | KeyCode::Delete
                    | KeyCode::Left
                    | KeyCode::Right
                    | KeyCode::Home
                    | KeyCode::End
                    | KeyCode::Tab
            );
            !(editing && (k.modifiers - KeyModifiers::SHIFT).is_empty())
        }
        Event::Mouse(m) => !matches!(m.kind, MouseEventKind::Moved),
        Event::Paste(_) | Event::FocusGained | Event::FocusLost => false,
        Event::Resize(..) => true,
    }
}

impl App {
    /// Settings → "Smooth streaming". Turning it off shows what is queued.
    pub(crate) fn set_smooth_streaming(&mut self, on: bool) {
        self.smooth_streaming = on;
        if !on {
            self.flush_reveal();
        }
    }

    /// Queue a paced stream event's text instead of applying it. `false`:
    /// not paced (the setting is off, or not a text/thinking delta) — the
    /// caller applies the event as before.
    pub(crate) fn defer_stream_text(&mut self, event: &StreamEvent) -> bool {
        if !self.smooth_streaming {
            return false;
        }
        // An empty delta (an omitted-display thought's only one) carries no
        // text to pace; it is applied in order, after the queue.
        let Some((kind, text)) = paced(event).filter(|(_, t)| !t.is_empty()) else {
            return false;
        };
        let now = self.clock.now();
        self.reveal.push(kind, text, now);
        self.needs_redraw = true;
        true
    }

    /// Release what is due now into the main transcript. The run loop calls
    /// it once per iteration, just before it decides whether to draw.
    pub(crate) fn advance_reveal(&mut self) {
        if self.reveal.pending() {
            let now = self.clock.now();
            let due = self.reveal.take(now);
            self.apply_revealed(due);
        }
    }

    /// Show everything queued now (turn end, abort, tool call, scroll,
    /// switch, …). Cheap when nothing is queued.
    pub(crate) fn flush_reveal(&mut self) {
        if self.reveal.pending() {
            let all = self.reveal.drain();
            self.apply_revealed(all);
        }
    }

    fn apply_revealed(&mut self, runs: Vec<(Kind, String)>) {
        if runs.is_empty() {
            return;
        }
        for (kind, text) in runs {
            match kind {
                Kind::Text => self.transcript.append_or_update_text(&text),
                Kind::Thinking => self.transcript.append_or_update_thinking(&text),
            }
        }
        self.needs_redraw = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn joined(runs: &[(Kind, String)]) -> String {
        runs.iter().map(|(_, s)| s.as_str()).collect()
    }

    /// A 1 KB burst comes out over several 16 ms frames, front first, the
    /// first frame already showing some of it, and all of it within the
    /// catch-up window — never past `MAX_LAG`.
    #[test]
    fn a_burst_is_paced_over_frames_and_catches_up() {
        let t0 = Instant::now();
        let mut r = Reveal::default();
        let burst: String = (0..60).map(|i| format!("word{i:02} ab ")).collect();
        assert_eq!(burst.len(), 600);
        r.push(Kind::Text, &burst, t0);
        r.push(Kind::Text, &burst, t0);
        let mut shown = String::new();
        let mut frames = 0;
        let mut t = t0;
        let first = r.take(t);
        assert!(!first.is_empty(), "the first frame shows some of it");
        assert!(joined(&first).len() < 300, "but not all: {}", joined(&first).len());
        shown.push_str(&joined(&first));
        while r.pending() {
            t += Duration::from_millis(16);
            frames += 1;
            shown.push_str(&joined(&r.take(t)));
            assert!(frames < 100, "never stalls");
        }
        assert_eq!(shown, format!("{burst}{burst}"), "whole, in order");
        let took = t - t0;
        assert!(frames >= 8, "several frames, not one: {frames}");
        assert!(took <= Duration::from_millis(450), "caught up in {took:?}");
        assert_eq!(r.backlog(), 0);
    }

    /// Steady streaming (a small delta every 50 ms) is shown within a frame
    /// or two of arriving: the base rate keeps up with the stream.
    #[test]
    fn steady_streaming_does_not_lag() {
        let t0 = Instant::now();
        let mut r = Reveal::default();
        let mut t = t0;
        for _ in 0..40 {
            r.push(Kind::Text, "seventeen bytes. ", t);
            for _ in 0..3 {
                r.take(t);
                t += Duration::from_millis(16);
            }
            t += Duration::from_millis(2);
            assert_eq!(r.backlog(), 0, "shown within three frames");
        }
        r.take(t);
        assert!(!r.pending(), "drained between deltas");
    }

    /// The bound: whatever arrived `MAX_LAG` ago is out, even with no frame
    /// in between.
    #[test]
    fn nothing_waits_longer_than_the_bound() {
        let t0 = Instant::now();
        let mut r = Reveal::default();
        r.push(Kind::Text, &"x".repeat(50_000), t0);
        r.take(t0);
        let out = r.take(t0 + MAX_LAG);
        assert!(!r.pending(), "released at the bound");
        assert!(joined(&out).len() > 40_000);
    }

    /// Thinking queued ahead of text comes out first, the runs merged per
    /// kind; a fixed instant releases nothing new; a cut never splits a char.
    #[test]
    fn order_kinds_and_char_boundaries() {
        let t0 = Instant::now();
        let mut r = Reveal::default();
        r.push(Kind::Thinking, "ponder ", t0);
        r.push(Kind::Thinking, "more", t0);
        r.push(Kind::Text, &"é".repeat(400), t0);
        let first = r.take(t0);
        assert_eq!(first[0], (Kind::Thinking, "ponder more".to_string()));
        assert_eq!(first[1].0, Kind::Text);
        assert!(first[1].1.chars().all(|c| c == 'é'));
        assert!(r.take(t0).is_empty(), "idempotent at a fixed instant");
        let rest = r.drain();
        assert_eq!(rest.len(), 1);
        assert_eq!(first[1].1.chars().count() + rest[0].1.chars().count(), 400);
        assert!(!r.pending() && r.backlog() == 0);
    }

    #[test]
    fn input_that_flushes() {
        use crossterm::event::{
            Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind,
        };
        let key = |c, m| Event::Key(KeyEvent::new(c, m));
        assert!(!input_flushes(&key(KeyCode::Char('a'), KeyModifiers::NONE)));
        assert!(!input_flushes(&key(KeyCode::Char('A'), KeyModifiers::SHIFT)));
        assert!(!input_flushes(&key(KeyCode::Backspace, KeyModifiers::NONE)));
        assert!(input_flushes(&key(KeyCode::Char('o'), KeyModifiers::CONTROL)));
        assert!(input_flushes(&key(KeyCode::Esc, KeyModifiers::NONE)));
        assert!(input_flushes(&key(KeyCode::Up, KeyModifiers::SHIFT)));
        assert!(input_flushes(&key(KeyCode::PageUp, KeyModifiers::NONE)));
        assert!(input_flushes(&key(KeyCode::Enter, KeyModifiers::NONE)));
        let mouse = |kind| {
            Event::Mouse(MouseEvent {
                kind,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            })
        };
        assert!(input_flushes(&mouse(MouseEventKind::ScrollUp)));
        assert!(!input_flushes(&mouse(MouseEventKind::Moved)));
    }
}
