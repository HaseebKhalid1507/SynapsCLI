//! Thread views: the main transcript plus per-subagent ("worker") threads.
//!
//! Groundwork for watching a running subagent in its own view and steering
//! it from there. The main thread stays `App::transcript`; each worker thread
//! is a separate [`TranscriptStore`] with its own scroll anchor, selection,
//! line cache and response-preview state — so nothing a worker does can move
//! or repaint the main view, and switching views never loses either side's
//! scroll position.
//!
//! Every transcript read/write that should follow the focused thread goes
//! through [`active`] / [`active_mut`]. They take the main store and the
//! thread set as *separate* borrows, so call sites keep borrowing other
//! `App` fields at the same time (e.g. a `RenderCtx` over `app.agent_name`
//! while the selection code mutates the focused store).

use std::collections::BTreeMap;

use super::clock::TuiClock;
use super::transcript::{ChatMessage, TranscriptStore};

/// Which transcript the body, scroll keys, selection and Ctrl-O act on.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum ThreadFocus {
    /// The session's own conversation (`App::transcript`).
    #[default]
    Main,
    /// A subagent's thread, keyed by its registry handle id (`sa_N`).
    Worker(String),
}

/// A subagent's own transcript, filled by paging its watch journal
/// (`super::watch`).
pub(crate) struct WorkerThread {
    pub(crate) subagent_id: u64,
    pub(crate) agent_name: String,
    pub(crate) store: TranscriptStore,
    /// `ResponseStart` snapshot for `ResponseReset` rollback — per thread,
    /// exactly like `App::response_preview` is for the main thread.
    pub(crate) response_preview: Option<(usize, Option<ChatMessage>)>,
    /// Journal cursor: the `since` of the next watch page.
    pub(crate) next_seq: u64,
    /// The worker's status from its last page (`running`, `completed`, …);
    /// empty until the first page arrives.
    pub(crate) status: String,
    /// The worker accepts steering (running, with a steer channel).
    pub(crate) steerable: bool,
    /// Everything the finished worker showed has been read: stop polling.
    pub(crate) drained: bool,
    /// The engine no longer knows this worker (past its watch window).
    pub(crate) gone: bool,
    /// When this view first saw the worker finished (thread pruning).
    pub(crate) finished_at: Option<std::time::Instant>,
}

/// The set of open worker threads plus the current focus.
///
/// Invariant: `focus == Worker(id)` only while `workers` holds `id`
/// ([`Threads::focus_worker`] refuses unknown ids and
/// [`Threads::close_worker`] falls back to main). [`active`] still degrades
/// to the main store if the invariant were ever broken.
#[derive(Default)]
pub(crate) struct Threads {
    focus: ThreadFocus,
    workers: BTreeMap<String, WorkerThread>,
}

impl Threads {
    pub(crate) fn focus(&self) -> &ThreadFocus {
        &self.focus
    }

    pub(crate) fn focus_main(&mut self) {
        self.focus = ThreadFocus::Main;
    }

    /// Focus an open worker thread. Returns `false` (focus unchanged) when
    /// no thread is open for `handle_id`.
    pub(crate) fn focus_worker(&mut self, handle_id: &str) -> bool {
        if self.workers.contains_key(handle_id) {
            self.focus = ThreadFocus::Worker(handle_id.to_string());
            true
        } else {
            false
        }
    }

    /// Open (or return the already-open) thread for a worker. A new
    /// thread's store gets the same scrollback cap as the main transcript.
    pub(crate) fn open_worker(
        &mut self,
        handle_id: &str,
        subagent_id: u64,
        agent_name: &str,
        clock: TuiClock,
        scrollback: (usize, usize),
    ) -> &mut WorkerThread {
        self.workers
            .entry(handle_id.to_string())
            .or_insert_with(|| {
                let mut store = TranscriptStore::new(clock);
                store.set_scrollback(scrollback.0, scrollback.1);
                store.set_worker_view();
                WorkerThread {
                    subagent_id,
                    agent_name: agent_name.to_string(),
                    store,
                    response_preview: None,
                    next_seq: 0,
                    status: String::new(),
                    steerable: false,
                    drained: false,
                    gone: false,
                    finished_at: None,
                }
            })
    }

    pub(crate) fn worker(&self, handle_id: &str) -> Option<&WorkerThread> {
        self.workers.get(handle_id)
    }

    pub(crate) fn worker_mut(&mut self, handle_id: &str) -> Option<&mut WorkerThread> {
        self.workers.get_mut(handle_id)
    }

    /// Close a worker thread. A view focused on it falls back to main.
    pub(crate) fn close_worker(&mut self, handle_id: &str) -> Option<WorkerThread> {
        if self.focus == ThreadFocus::Worker(handle_id.to_string()) {
            self.focus = ThreadFocus::Main;
        }
        self.workers.remove(handle_id)
    }

    /// Open worker threads in handle-id order.
    pub(crate) fn workers(&self) -> impl Iterator<Item = (&str, &WorkerThread)> {
        self.workers.iter().map(|(id, w)| (id.as_str(), w))
    }
}

/// The focused thread's transcript (read).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn active<'a>(main: &'a TranscriptStore, threads: &'a Threads) -> &'a TranscriptStore {
    match &threads.focus {
        ThreadFocus::Main => main,
        ThreadFocus::Worker(id) => threads.workers.get(id).map_or(main, |w| &w.store),
    }
}

/// The focused thread's transcript (write): scroll, selection, Ctrl-O, and
/// the per-frame `visible_window` bookkeeping.
pub(crate) fn active_mut<'a>(
    main: &'a mut TranscriptStore,
    threads: &'a mut Threads,
) -> &'a mut TranscriptStore {
    match &threads.focus {
        ThreadFocus::Main => main,
        ThreadFocus::Worker(id) => match threads.workers.get_mut(id) {
            Some(w) => &mut w.store,
            None => main,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 04-tui M6: a worker view's trim note pointed at `/resync`, which only
    /// reloads the main thread.
    #[test]
    fn a_worker_views_trim_note_does_not_point_at_resync() {
        let mut threads = Threads::default();
        let w = threads.open_worker("sa_1", 1, "scout", TuiClock::test(), (10, 0));
        for i in 0..100 {
            w.store.push_msg(ChatMessage::Text(format!("line {i}")));
        }
        let note = match &w.store.messages()[0].msg {
            ChatMessage::System(t) => t.clone(),
            _ => panic!("expected the trim note first"),
        };
        assert!(note.contains("earlier message(s) hidden"), "{note}");
        assert!(!note.contains("/resync"), "{note}");
        assert!(note.contains("subagent view"), "{note}");
    }

    fn store_with(text: &str) -> TranscriptStore {
        let mut s = TranscriptStore::new(TuiClock::test());
        s.push_msg(ChatMessage::Text(text.to_string()));
        s
    }

    fn first_text(s: &TranscriptStore) -> String {
        match &s.messages()[0].msg {
            ChatMessage::Text(t) => t.clone(),
            _ => panic!("expected text"),
        }
    }

    fn open(threads: &mut Threads, id: &str, text: &str) {
        let w = threads.open_worker(id, 7, "scout", TuiClock::test(), (0, 0));
        w.store.push_msg(ChatMessage::Text(text.to_string()));
    }

    #[test]
    fn main_is_the_default_focus() {
        let mut main = store_with("main");
        let mut threads = Threads::default();
        assert_eq!(threads.focus(), &ThreadFocus::Main);
        assert_eq!(first_text(active(&main, &threads)), "main");
        assert_eq!(first_text(active_mut(&mut main, &mut threads)), "main");
    }

    #[test]
    fn focusing_a_worker_routes_reads_and_writes_to_its_store() {
        let mut main = store_with("main");
        let mut threads = Threads::default();
        open(&mut threads, "sa_7", "worker");
        assert!(threads.focus_worker("sa_7"));
        assert_eq!(first_text(active(&main, &threads)), "worker");

        // Scrolling the focused worker never touches the main store.
        active_mut(&mut main, &mut threads).scroll_up(4);
        assert!(main.is_pinned(), "main view untouched");
        assert!(!threads.worker("sa_7").unwrap().store.is_pinned());

        threads.focus_main();
        assert_eq!(first_text(active(&main, &threads)), "main");
    }

    #[test]
    fn unknown_worker_cannot_take_focus() {
        let mut threads = Threads::default();
        assert!(!threads.focus_worker("sa_404"));
        assert_eq!(threads.focus(), &ThreadFocus::Main);
    }

    #[test]
    fn closing_the_focused_worker_falls_back_to_main() {
        let main = store_with("main");
        let mut threads = Threads::default();
        open(&mut threads, "sa_1", "one");
        open(&mut threads, "sa_2", "two");
        assert!(threads.focus_worker("sa_2"));
        assert!(
            threads.close_worker("sa_1").is_some(),
            "closing another keeps focus"
        );
        assert_eq!(threads.focus(), &ThreadFocus::Worker("sa_2".into()));
        assert!(threads.close_worker("sa_2").is_some());
        assert_eq!(threads.focus(), &ThreadFocus::Main);
        assert_eq!(first_text(active(&main, &threads)), "main");
    }

    #[test]
    fn reopening_returns_the_same_thread() {
        let mut threads = Threads::default();
        open(&mut threads, "sa_3", "first");
        let again = threads.open_worker("sa_3", 3, "scout", TuiClock::test(), (0, 0));
        assert_eq!(again.store.messages().len(), 1, "existing thread kept");
        assert_eq!(threads.workers().count(), 1);
    }
}
