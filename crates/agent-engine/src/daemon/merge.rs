//! One connection's view of a session's two channels (child spend §7.4).
//!
//! A session publishes ordinary events on its main broadcast and child_spend
//! frames on its optional one; both take `seq` from the actor's one counter.
//! A connection that accepted "child_spend" reads both and must get one
//! stream, strictly increasing in seq, without ever holding an envelope back
//! to wait for a later one.
//!
//! The rule that makes that possible: the actor publishes from one task,
//! synchronously, so when an envelope with seq `s` has been received from one
//! channel, every envelope below `s` on the other channel is already in that
//! ring. [`Merge::pop_ready`] therefore fills each empty head with
//! `try_recv` and forwards the lower head; a lone head is forwarded once the
//! other channel answered `Empty` to a poll made AFTER the head was received
//! (nothing below it can still come). A poll made before does not count: the
//! actor runs on another thread, so main can answer `Empty`, then main `s` and
//! optional `s+1` go out, then the optional poll returns `s+1` alone. So the
//! fill polls the other channel once more whenever one head is lone.
//!
//! Lag is per channel. A main lag gives the connection the main lag notice
//! (a SystemNotice) at once, as before. An optional lag gives it
//! `child_spend_lagged {dropped}` (frames only) immediately before the first
//! frame that survived the gap. Both notices are made here, for this
//! connection only, so their envelopes carry seq `u64::MAX` and no actor seq.

use tokio::sync::broadcast::{
    self,
    error::{RecvError, TryRecvError},
};

use crate::session::{Envelope, SessionEventWire, SessionId};

/// Where a popped envelope came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    /// The session's main channel (the connection's skip rules apply).
    Main,
    /// The optional channel (child_spend frames).
    Optional,
    /// Made here for this connection: a lag notice.
    Notice,
}

/// The main channel's lag notice. It is made per connection, so it carries
/// no actor seq (`u64::MAX`).
pub(crate) fn lag_notice(session: &SessionId, dropped: u64) -> Envelope {
    Envelope {
        session_id: session.clone(),
        seq: u64::MAX,
        ts: chrono::Utc::now(),
        event: SessionEventWire::SystemNotice(format!("event stream lagged; {dropped} dropped")),
    }
}

/// The optional channel's lag notice (`child_spend_lagged`).
pub(crate) fn child_spend_lagged(session: &SessionId, dropped: u64) -> Envelope {
    Envelope {
        session_id: session.clone(),
        seq: u64::MAX,
        ts: chrono::Utc::now(),
        event: SessionEventWire::ChildSpendLagged { dropped },
    }
}

/// The merge state of one connection. The receivers stay with the caller
/// (the forwarder task owns them); this holds what was peeked.
#[derive(Debug)]
pub(crate) struct Merge {
    session: SessionId,
    /// Optional envelopes below this seq were published before the
    /// connection's attach snapshot (whose totals include them): dropped.
    floor: u64,
    head_main: Option<Envelope>,
    head_opt: Option<Envelope>,
    main_closed: bool,
    opt_closed: bool,
    /// A main lag not yet reported.
    main_lagged: Option<u64>,
    /// child_spend frames lost since the last forwarded frame.
    opt_dropped: u64,
    /// Test seam: runs once inside `fill`, between the two channels' first
    /// polls, to stage a publish there.
    #[cfg(test)]
    pub(crate) between_polls: BetweenPolls,
}

/// A one-shot hook for [`Merge::fill`] (tests only).
#[cfg(test)]
#[derive(Default)]
pub(crate) struct BetweenPolls(pub(crate) Option<Box<dyn FnOnce() + Send + Sync>>);

#[cfg(test)]
impl std::fmt::Debug for BetweenPolls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() { "BetweenPolls(set)" } else { "BetweenPolls(none)" })
    }
}

impl Merge {
    pub(crate) fn new(session: SessionId, floor: u64) -> Self {
        Self {
            session,
            floor,
            head_main: None,
            head_opt: None,
            main_closed: false,
            opt_closed: false,
            main_lagged: None,
            opt_dropped: 0,
            #[cfg(test)]
            between_polls: BetweenPolls::default(),
        }
    }

    /// The main channel closed and nothing peeked is left to forward.
    fn finished(&self) -> bool {
        self.main_closed && self.head_main.is_none() && self.main_lagged.is_none()
    }

    fn take_main(&mut self, r: Result<Envelope, RecvError>) {
        match r {
            Ok(env) => self.head_main = Some(env),
            Err(RecvError::Lagged(n)) => self.note_main_lag(n),
            Err(RecvError::Closed) => self.main_closed = true,
        }
    }

    fn take_opt(&mut self, r: Result<Envelope, RecvError>) {
        match r {
            Ok(env) => self.got_opt(env),
            Err(RecvError::Lagged(n)) => self.opt_dropped += n,
            Err(RecvError::Closed) => self.opt_closed = true,
        }
    }

    /// An optional envelope came in. One below the floor was published
    /// before the attach snapshot, whose totals include it: dropped. Every
    /// frame a lag lost ahead of it is lower still, so that lag is not
    /// reported either (the client lost nothing the snapshot doesn't hold).
    fn got_opt(&mut self, env: Envelope) {
        if env.seq < self.floor {
            self.opt_dropped = 0;
        } else {
            self.head_opt = Some(env);
        }
    }

    fn note_main_lag(&mut self, n: u64) {
        self.main_lagged = Some(self.main_lagged.unwrap_or(0) + n);
    }

    /// Peek, without waiting, into every empty head. A head that is lone
    /// after both polls may have come in after the other channel's poll
    /// (see the module doc), so the other channel is polled once more: by
    /// then everything below that head is in its ring. Both ways, so the
    /// order of the first two polls doesn't matter.
    fn fill(
        &mut self,
        main: &mut broadcast::Receiver<Envelope>,
        mut opt: Option<&mut broadcast::Receiver<Envelope>>,
    ) {
        self.fill_main(main);
        self.run_between_polls();
        if let Some(opt) = opt.as_deref_mut() {
            self.fill_opt(opt);
        }
        match (self.head_main.is_some(), self.head_opt.is_some()) {
            (false, true) => self.fill_main(main),
            (true, false) => {
                if let Some(opt) = opt {
                    self.fill_opt(opt);
                }
            }
            _ => {}
        }
    }

    fn fill_main(&mut self, main: &mut broadcast::Receiver<Envelope>) {
        while self.head_main.is_none() && !self.main_closed {
            match main.try_recv() {
                Ok(env) => self.head_main = Some(env),
                Err(TryRecvError::Lagged(n)) => self.note_main_lag(n),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Closed) => self.main_closed = true,
            }
        }
    }

    fn fill_opt(&mut self, opt: &mut broadcast::Receiver<Envelope>) {
        while self.head_opt.is_none() && !self.opt_closed {
            match opt.try_recv() {
                Ok(env) => self.got_opt(env),
                Err(TryRecvError::Lagged(n)) => self.opt_dropped += n,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Closed) => self.opt_closed = true,
            }
        }
    }

    #[cfg(test)]
    fn run_between_polls(&mut self) {
        if let Some(hook) = self.between_polls.0.take() {
            hook();
        }
    }

    #[cfg(not(test))]
    fn run_between_polls(&mut self) {}

    /// The next envelope that may go out now, without waiting: a pending
    /// main lag notice first, then the lower of the two heads; a lone head
    /// once the other channel had nothing. Before an optional frame, the
    /// `child_spend_lagged` notice for the frames lost ahead of it.
    pub(crate) fn pop_ready(
        &mut self,
        main: &mut broadcast::Receiver<Envelope>,
        opt: Option<&mut broadcast::Receiver<Envelope>>,
    ) -> Option<(Envelope, Source)> {
        self.pop(main, opt, false)
    }

    /// [`Merge::pop_ready`]; `in_drain` only picks the main lag's log text.
    fn pop(
        &mut self,
        main: &mut broadcast::Receiver<Envelope>,
        opt: Option<&mut broadcast::Receiver<Envelope>>,
        in_drain: bool,
    ) -> Option<(Envelope, Source)> {
        self.fill(main, opt);
        if let Some(n) = self.main_lagged.take() {
            if in_drain {
                tracing::warn!(session = %self.session, dropped = n, "daemon: client lagged (reload drain)");
            } else {
                tracing::warn!(session = %self.session, dropped = n, "daemon: client lagged");
            }
            return Some((lag_notice(&self.session, n), Source::Notice));
        }
        let optional_first = match (&self.head_main, &self.head_opt) {
            (None, None) => return None,
            (Some(_), None) => false,
            (None, Some(_)) => true,
            (Some(m), Some(o)) => o.seq < m.seq,
        };
        if !optional_first {
            return self.head_main.take().map(|env| (env, Source::Main));
        }
        if self.opt_dropped > 0 {
            let n = std::mem::take(&mut self.opt_dropped);
            tracing::warn!(session = %self.session, dropped = n, "daemon: client lagged on child_spend frames");
            return Some((child_spend_lagged(&self.session, n), Source::Notice));
        }
        self.head_opt.take().map(|env| (env, Source::Optional))
    }

    /// The next envelope, waiting for one when nothing is ready. `None`
    /// once the main channel closed and everything peeked went out.
    /// Cancel-safe: what a cancelled wait received is kept in the heads.
    pub(crate) async fn next(
        &mut self,
        main: &mut broadcast::Receiver<Envelope>,
        mut opt: Option<&mut broadcast::Receiver<Envelope>>,
    ) -> Option<(Envelope, Source)> {
        loop {
            if let Some(out) = self.pop_ready(main, opt.as_deref_mut()) {
                return Some(out);
            }
            if self.finished() {
                return None;
            }
            let opt_live = opt.is_some() && !self.opt_closed;
            tokio::select! {
                r = main.recv() => self.take_main(r),
                r = recv_opt(opt.as_deref_mut()), if opt_live => self.take_opt(r),
            }
        }
    }

    /// Forward everything already in both rings (a reload drain), skipping
    /// main envelopes `skip` matches. Returns false once the client is gone.
    pub(crate) async fn drain(
        &mut self,
        main: &mut broadcast::Receiver<Envelope>,
        mut opt: Option<&mut broadcast::Receiver<Envelope>>,
        skip: &impl Fn(&Envelope) -> bool,
        out: &tokio::sync::mpsc::Sender<crate::session::wire::DaemonFrame>,
    ) -> bool {
        while let Some((env, source)) = self.pop(main, opt.as_deref_mut(), true) {
            if source == Source::Main && skip(&env) {
                continue;
            }
            // A run's request-ledger liability is host-internal (§6): no
            // client form, nothing sent.
            let Some(wire) = crate::session::wire::WireEnvelope::for_client(env) else {
                continue;
            };
            if out.send(crate::session::wire::DaemonFrame::Event(wire)).await.is_err() {
                return false;
            }
        }
        true
    }
}

async fn recv_opt(opt: Option<&mut broadcast::Receiver<Envelope>>) -> Result<Envelope, RecvError> {
    match opt {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
#[path = "merge_tests.rs"]
mod tests;
