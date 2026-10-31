//! Live slots: the messages that animate on their own — a running tool
//! card's clock and sweep, a thinking block's header while it thinks.
//!
//! The animation tick used to dirty only the LAST message, so a live
//! element anywhere else (a running card with a steer or a notice under it,
//! the first of two parallel cards) was served from the line cache and
//! froze. The store now keeps the indices of its live messages in
//! `live` (sorted), updated on every insert/remove and whenever a tool
//! starts or ends, and the tick re-renders exactly those slots
//! ([`TranscriptStore::tick_live`] → [`TranscriptStore::refresh_live_slots`]).
//! Nothing live ⇒ the tick does nothing.
//!
//! Debug builds check the tracked set against a full scan after every cache
//! sync, so a mutation path that forgets to keep it in step fails tests.

use super::{CacheState, ChatMessage, MsgSlot, RenderCtx, TranscriptStore, THINKING_PLACEHOLDER};

impl TranscriptStore {
    /// Whether the tool call `tool_id` is still running. A call without an
    /// id (legacy providers) runs while the shared tool clock does.
    pub(crate) fn tool_running(&self, tool_id: &str) -> bool {
        if tool_id.is_empty() {
            self.tool_start_time.is_some()
        } else {
            self.tool_start_times.contains_key(tool_id)
        }
    }

    /// When the running tool `tool_id` started: its own start, so parallel
    /// calls each count from theirs; the shared clock for a call without an
    /// id.
    pub(crate) fn tool_started_at(&self, tool_id: &str) -> Option<std::time::Instant> {
        self.tool_start_times
            .get(tool_id)
            .copied()
            .or(self.tool_start_time)
    }

    /// Whether the header of the tool card at `idx` shows the running clock
    /// and sweep: while the tool runs, wherever the card is, until a result
    /// row (which then carries them) sits directly beneath it. A result is
    /// inserted right under its call by id; a call without an id gets its
    /// result appended at the end, so that header runs only while last.
    pub(crate) fn tool_header_runs(&self, idx: usize, tool_id: &str) -> bool {
        if !self.tool_running(tool_id) {
            return false;
        }
        if tool_id.is_empty() {
            return idx + 1 == self.messages.len();
        }
        !matches!(
            self.messages.get(idx + 1).map(|m| &m.msg),
            Some(ChatMessage::ToolResult { tool_id: t, .. }) if t == tool_id
        )
    }

    /// The text block the reply's next delta extends (see
    /// `append_or_update_text`): the last message that isn't a system line,
    /// when it is text — and, if notices landed after it, belongs to the
    /// response being streamed. Its agent label pulses while streaming,
    /// wherever it is.
    pub(crate) fn streaming_text_idx(&self) -> Option<usize> {
        if self.text_sealed {
            return None;
        }
        let i = self
            .messages
            .iter()
            .rposition(|m| !matches!(m.msg, ChatMessage::System(_)))?;
        let own = i + 1 == self.messages.len() || self.response_start.is_some_and(|s| i >= s);
        (own && matches!(self.messages[i].msg, ChatMessage::Text(_))).then_some(i)
    }

    /// Whether message `idx` animates by itself (so the tick must re-render
    /// it). Depends only on the message and the tool timers, never on its
    /// position — the renderer decides which row of a running card carries
    /// the clock.
    fn is_live(&self, idx: usize) -> bool {
        let Some(m) = self.messages.get(idx) else {
            return false;
        };
        match &m.msg {
            // The placeholder's header, and a streamed block's until it is
            // sealed (its header shimmers; the clock lives on the message).
            // Not while Ctrl+O shows thoughts whole: the card then renders
            // its full source, so it moves per delta only, like agent text.
            ChatMessage::Thinking(t) => {
                t == THINKING_PLACEHOLDER
                    || !self.show_full_output && m.thought.is_some_and(|c| !c.done)
            }
            ChatMessage::ToolUseStart { tool_id, .. } | ChatMessage::ToolUse { tool_id, .. } => {
                self.tool_running(tool_id)
            }
            ChatMessage::ToolResult { .. } => self.is_active_tool_result(idx),
            _ => false,
        }
    }

    fn scan_live(&self) -> Vec<usize> {
        (0..self.messages.len())
            .filter(|&i| self.is_live(i))
            .collect()
    }

    /// Rebuild the live set from scratch. O(n): for the rare structural
    /// edits (rollback, resume cap) and the shared tool clock starting or
    /// stopping.
    pub(super) fn relive(&mut self) {
        self.live = self.scan_live();
    }

    /// Re-evaluate message `idx` after it changed in place.
    pub(super) fn live_note(&mut self, idx: usize) {
        let live = self.is_live(idx);
        match (self.live.binary_search(&idx), live) {
            (Err(at), true) => self.live.insert(at, idx),
            (Ok(at), false) => {
                self.live.remove(at);
            }
            _ => {}
        }
    }

    /// A message was inserted at `at`: later slots shift down one.
    pub(super) fn live_inserted(&mut self, at: usize) {
        for i in self.live.iter_mut().filter(|i| **i >= at) {
            *i += 1;
        }
        self.live_note(at);
    }

    /// The message at `at` was removed: later slots shift up one.
    pub(super) fn live_removed(&mut self, at: usize) {
        self.live.retain(|&i| i != at);
        for i in self.live.iter_mut().filter(|i| **i > at) {
            *i -= 1;
        }
    }

    /// The first `n` messages were drained.
    pub(super) fn live_drained_front(&mut self, n: usize) {
        self.live.retain(|&i| i >= n);
        for i in self.live.iter_mut() {
            *i -= n;
        }
    }

    /// Set the shared tool clock. Starting or stopping it changes whether
    /// id-less tool cards run, so the set is rebuilt then (once per batch
    /// of tools, not per event).
    pub(super) fn set_tool_clock(&mut self, at: Option<std::time::Instant>) {
        let flipped = self.tool_start_time.is_some() != at.is_some();
        self.tool_start_time = at;
        if flipped {
            self.relive();
        }
    }

    /// True while anything in this transcript animates by itself.
    pub(crate) fn has_live(&self) -> bool {
        !self.live.is_empty()
    }

    /// One animation frame: ask the next cache sync to re-render the live
    /// slots. Returns whether there were any (nothing live: no-op).
    pub(crate) fn tick_live(&mut self) -> bool {
        if self.live.is_empty() {
            return false;
        }
        self.live_refresh = true;
        true
    }

    /// The live indices (tests).
    #[cfg(test)]
    pub(crate) fn live_slots(&self) -> &[usize] {
        &self.live
    }

    /// Part of [`Self::sync_cache`]: re-render each live slot that is
    /// cached as exact rows and that the dirty watermark won't re-render
    /// anyway. Selection and anchors are untouched — only the card's chrome
    /// (clock, sweep, spinner) changes on a tick. A height change re-splices
    /// the cumulative offsets from the first slot that moved.
    pub(super) fn refresh_live_slots(&mut self, width: usize, ctx: &RenderCtx<'_>) {
        if !std::mem::take(&mut self.live_refresh) {
            return;
        }
        let limit = match &self.cache {
            CacheState::Clean(c) if c.per_msg.len() == self.messages.len() => c.per_msg.len(),
            CacheState::Dirty(c, k) => (*k).min(c.per_msg.len()),
            _ => return,
        };
        let Some(cache) = self.cache.line_cache() else {
            return;
        };
        if cache.width != width {
            return;
        }
        let todo: Vec<usize> = self
            .live
            .iter()
            .copied()
            .take_while(|&i| i < limit)
            .filter(|&i| cache.per_msg[i].lines.is_some())
            .collect();
        if todo.is_empty() {
            return;
        }
        let fresh: Vec<(usize, MsgSlot)> = todo
            .into_iter()
            .map(|i| (i, self.render_message_lines(i, width, ctx)))
            .collect();
        let Some(cache) = self.cache.line_cache_mut() else {
            return;
        };
        let mut moved: Option<usize> = None;
        for (i, slot) in fresh {
            if moved.is_none() && slot.height() != cache.per_msg[i].height() {
                moved = Some(i);
            }
            cache.per_msg[i] = slot;
        }
        if let Some(k) = moved {
            let _cum_written = cache.rebuild_cum_from(k);
            #[cfg(any(test, feature = "testing"))]
            self.probe
                .cum_writes
                .fetch_add(_cum_written, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Debug builds: the tracked live set matches a full scan.
    pub(super) fn debug_assert_live(&self) {
        #[cfg(debug_assertions)]
        debug_assert_eq!(
            self.live,
            self.scan_live(),
            "live slots drifted from the transcript"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::super::{ChatMessage, RenderCtx, TranscriptStore, THINKING_PLACEHOLDER};
    use crate::tui::clock::TuiClock;

    fn store() -> TranscriptStore {
        TranscriptStore::new(TuiClock::test())
    }

    fn ctx() -> RenderCtx<'static> {
        RenderCtx {
            spinner_frame: 0,
            streaming: true,
            agent_name: "agent",
        }
    }

    fn running(s: &mut TranscriptStore, id: &str) {
        s.on_tool_use_start(id.into(), "bash".into());
        s.on_tool_use_finalized(id.into(), "bash".into(), r#"{"command":"sleep 9"}"#.into());
    }

    /// A streamed thought is live (its header shimmers and counts) from its
    /// first delta until the model moves on; Ctrl+O, which renders thoughts
    /// whole, takes it off the tick (it then moves per delta, like agent
    /// text) and back.
    #[test]
    fn a_streaming_thought_is_live_until_sealed() {
        let mut s = store();
        s.push_msg(ChatMessage::User("go".into()));
        s.push_msg(ChatMessage::Thinking(THINKING_PLACEHOLDER.into()));
        assert_eq!(s.live_slots(), &[1]);
        s.append_or_update_thinking("weighing it");
        assert_eq!(s.live_slots(), &[1], "real thinking keeps the slot");
        s.set_show_full_output(true);
        assert!(s.live_slots().is_empty(), "expanded: not ticked");
        s.set_show_full_output(false);
        assert_eq!(s.live_slots(), &[1]);
        s.append_or_update_text("answer");
        assert!(s.live_slots().is_empty(), "text sealed the thought");
        assert!(s.messages()[1].thought.is_some_and(|c| c.done));
        s.debug_assert_live();
    }

    #[test]
    fn running_cards_and_the_placeholder_are_live_wherever_they_are() {
        let mut s = store();
        s.push_msg(ChatMessage::User("go".into()));
        running(&mut s, "a");
        running(&mut s, "b");
        s.push_msg(ChatMessage::System("→ steering: x".into()));
        assert_eq!(s.live_slots(), &[1, 2]);
        s.push_msg(ChatMessage::Thinking(THINKING_PLACEHOLDER.into()));
        assert_eq!(s.live_slots(), &[1, 2, 4]);
        s.debug_assert_live();
    }

    #[test]
    fn a_finished_card_stops_being_live_and_inserts_shift_the_rest() {
        let mut s = store();
        running(&mut s, "a");
        running(&mut s, "b");
        // b streams output: its result row lands under b (index 2).
        s.on_tool_result_delta("b".into(), "out\n".into());
        assert_eq!(s.live_slots(), &[0, 1, 2]);
        // a finishes: its result is inserted under a, shifting b's rows.
        s.on_tool_result("a".into(), "done".into());
        assert_eq!(s.live_slots(), &[2, 3]);
        s.on_tool_result("b".into(), "done".into());
        assert!(!s.has_live(), "{:?}", s.live_slots());
        assert!(!s.tick_live(), "nothing live: the tick does nothing");
        s.debug_assert_live();
    }

    #[test]
    fn removals_and_the_scrollback_cap_keep_indices_in_step() {
        let mut s = store();
        s.set_scrollback(4, 0);
        s.push_msg(ChatMessage::User("go".into()));
        s.push_msg(ChatMessage::Thinking(THINKING_PLACEHOLDER.into()));
        s.push_msg(ChatMessage::System("notice".into()));
        // The placeholder is dropped from under the notice.
        s.drop_empty_thinking();
        assert!(!s.has_live());
        running(&mut s, "a");
        for n in 0..200 {
            s.push_msg(ChatMessage::System(format!("notice {n}")));
            s.debug_assert_live();
        }
        assert!(s.scrollback_dropped() > 0, "the cap drained the front");
        running(&mut s, "b");
        for n in 0..2 {
            s.push_msg(ChatMessage::System(format!("later {n}")));
            s.debug_assert_live();
        }
        let b = s
            .messages()
            .iter()
            .position(|m| matches!(&m.msg, ChatMessage::ToolUse { tool_id, .. } if tool_id == "b"))
            .expect("b is on screen");
        assert!(s.live_slots().contains(&b), "{:?} vs {b}", s.live_slots());
    }

    #[test]
    fn a_tick_re_renders_only_the_live_slots_above_the_tail() {
        let mut s = store();
        s.push_msg(ChatMessage::User("go".into()));
        running(&mut s, "a");
        s.push_msg(ChatMessage::Text("long reply".into()));
        s.push_msg(ChatMessage::System("→ steering: x".into()));
        s.sync_cache(80, &ctx());
        s.sync_cache(80, &ctx());
        // promote the slots as a draw would
        for i in 0..s.messages().len() {
            let slot = s.render_message_lines(i, 80, &ctx());
            s.cache.line_cache_mut().unwrap().per_msg[i] = slot;
        }
        s.cache.line_cache_mut().unwrap().rebuild_cum_from(0);
        s.probe_reset();
        assert!(s.tick_live());
        s.sync_cache(80, &ctx());
        assert_eq!(s.probe_render_count(), 1, "only the running card re-renders");
        s.sync_cache(80, &ctx());
        assert_eq!(s.probe_render_count(), 1, "the refresh is one-shot per tick");
    }

    #[test]
    fn the_streaming_text_block_is_found_under_notices() {
        let mut s = store();
        s.begin_response();
        s.append_or_update_text("hello");
        assert_eq!(s.streaming_text_idx(), Some(0));
        s.push_msg(ChatMessage::System("→ steering: x".into()));
        assert_eq!(s.streaming_text_idx(), Some(0), "still the reply's block");
        running(&mut s, "a");
        assert_eq!(s.streaming_text_idx(), None, "a tool card ended the block");
        s.seal_text_block();
        assert_eq!(s.streaming_text_idx(), None);
    }
}
