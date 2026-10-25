//! `/sessions` modal — the shell (tabs, header, footer, confirm popup) and the
//! **Live** tab: every session the daemon holds, by state (streaming /
//! waiting / idle / parked), with pin and close.
//!
//! Data never touches the draw loop: [`open`] spawns a poller that asks the
//! daemon for its list (`SocketTransport::sessions`) about once a second and
//! sends each answer back as a [`SessionsMsg`] to the main loop's
//! `sessions_rx` arm. The poller lives in `App::sessions_poller` and is aborted
//! when the modal closes ([`close`]); every message carries the open's
//! generation, so a late answer from a previous open is dropped.
//!
//! The **Past** tab lives in [`past`] so it can be swapped wholesale for the
//! catalog-backed browser (PR 3).

pub(crate) mod draw;
pub(crate) mod input;
pub(crate) mod past;

pub(crate) use draw::render;
pub(crate) use input::{handle_event, InputOutcome};

use agent_engine::session::{SessionId, SessionLifecycle, SessionMeta};
use chrono::{DateTime, Utc};
use std::path::PathBuf;
use std::time::Duration;

/// How often the open modal re-polls the daemon.
pub(crate) const POLL_EVERY: Duration = Duration::from_secs(1);
/// One poll's budget: a wedged daemon must not stall the poller forever.
const POLL_TIMEOUT: Duration = Duration::from_secs(3);
/// `Close{attached}` sent from the modal. `false`: the daemon refuses to
/// close a session that has clients attached, and the refusal is shown
/// inline — the modal never kicks another terminal's TUI by itself.
pub(crate) const CLOSE_ATTACHED: bool = false;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tab {
    Live,
    Past,
}

/// Where the Live list comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LiveSource {
    /// Daemon mode, first answer not in yet.
    Loading,
    /// Daemon mode, list is current.
    Daemon,
    /// In-process TUI: there is no daemon, only this session.
    InProcess,
    /// Daemon mode but the last poll failed (daemon down / restarting).
    DaemonDown(String),
}

/// A session's state as the Live tab shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum RowState {
    Streaming,
    Waiting,
    Idle,
    /// `Parking` / `Ending`: transient, listed with the live ones.
    Transient,
    Parked,
}

impl RowState {
    pub(crate) fn of(m: &SessionMeta) -> Self {
        match m.lifecycle {
            SessionLifecycle::Parked => Self::Parked,
            SessionLifecycle::Parking | SessionLifecycle::Ending => Self::Transient,
            SessionLifecycle::Live if m.awaiting_input > 0 => Self::Waiting,
            SessionLifecycle::Live if m.busy => Self::Streaming,
            SessionLifecycle::Live => Self::Idle,
        }
    }

    pub(crate) fn is_parked(self) -> bool {
        self == Self::Parked
    }
}

/// The close-confirm popup: which session, and what it will say.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ConfirmClose {
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) clients: usize,
}

/// An inline result line (pin/close answers, refusals).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Notice {
    pub(crate) text: String,
    pub(crate) error: bool,
}

/// A daemon request the modal asks the dispatch loop to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SessionsOp {
    Pin { id: String, on: bool },
    Close { id: String, label: String },
}

/// What the background tasks send back to the main loop.
#[derive(Debug)]
pub(crate) enum SessionsMsg {
    Live { gen: u64, result: Result<Vec<SessionMeta>, String> },
    Past { gen: u64, result: Result<Vec<synaps_cli::SessionInfo>, String> },
    OpDone { gen: u64, op: SessionsOp, result: Result<String, String> },
}

/// Modal state. Pure data (cloned into every render snapshot); the poller
/// handle lives on `App`, not here.
#[derive(Clone, Debug)]
pub(crate) struct SessionsState {
    pub(crate) tab: Tab,
    pub(crate) gen: u64,
    /// This TUI's own session id (never offered for close; shown as `you`).
    pub(crate) own_id: String,
    pub(crate) source: LiveSource,
    /// Live rows, already in display order (LIVE section, then PARKED).
    pub(crate) live: Vec<SessionMeta>,
    /// Selected Live row, tracked by id so a refresh never jumps the cursor.
    pub(crate) selected: Option<String>,
    pub(crate) confirm: Option<ConfirmClose>,
    pub(crate) notice: Option<Notice>,
    /// The op in flight (one at a time).
    pub(crate) pending: Option<SessionsOp>,
    pub(crate) past: past::PastState,
    /// Wall clock the relative times are drawn against (set on every poll
    /// answer, so countdowns tick with the 1 s re-poll; pinned in tests).
    pub(crate) now: DateTime<Utc>,
    /// `$HOME`, so cwd shows as `~/…`.
    pub(crate) home: Option<PathBuf>,
}

impl SessionsState {
    pub(crate) fn new(gen: u64, own: SessionMeta, daemon: bool) -> Self {
        let own_id = own.id.0.clone();
        let mut st = Self {
            tab: Tab::Live,
            gen,
            own_id: own_id.clone(),
            source: if daemon { LiveSource::Loading } else { LiveSource::InProcess },
            live: Vec::new(),
            selected: Some(own_id),
            confirm: None,
            notice: None,
            pending: None,
            past: past::PastState::default(),
            now: Utc::now(),
            home: std::env::var_os("HOME").map(PathBuf::from),
        };
        st.set_live(vec![own]);
        st
    }

    /// Replace the Live rows (sorted for display), keeping the selection on
    /// the same session when it is still listed.
    pub(crate) fn set_live(&mut self, mut rows: Vec<SessionMeta>) {
        rows.sort_by(|a, b| {
            RowState::of(a)
                .cmp(&RowState::of(b))
                .then_with(|| b.created_at.cmp(&a.created_at))
                .then_with(|| a.id.0.cmp(&b.id.0))
        });
        self.live = rows;
        let still = self
            .selected
            .as_ref()
            .is_some_and(|id| self.live.iter().any(|m| &m.id.0 == id));
        if !still {
            self.selected = self
                .live
                .iter()
                .find(|m| m.id.0 == self.own_id)
                .or_else(|| self.live.first())
                .map(|m| m.id.0.clone());
        }
        if let Some(c) = &self.confirm {
            if !self.live.iter().any(|m| m.id.0 == c.id) {
                self.confirm = None; // the session went away under the popup
            }
        }
    }

    pub(crate) fn selected_index(&self) -> Option<usize> {
        let id = self.selected.as_ref()?;
        self.live.iter().position(|m| &m.id.0 == id)
    }

    pub(crate) fn selected_meta(&self) -> Option<&SessionMeta> {
        self.selected_index().map(|i| &self.live[i])
    }

    pub(crate) fn is_own(&self, m: &SessionMeta) -> bool {
        m.id.0 == self.own_id
    }

    pub(crate) fn move_selection(&mut self, delta: isize) {
        if self.live.is_empty() {
            return;
        }
        let cur = self.selected_index().unwrap_or(0) as isize;
        let next = (cur + delta).clamp(0, self.live.len() as isize - 1) as usize;
        self.selected = Some(self.live[next].id.0.clone());
    }

    /// `(streaming, waiting, idle, parked)` counts for the header.
    pub(crate) fn counts(&self) -> (usize, usize, usize, usize) {
        let mut c = (0, 0, 0, 0);
        for m in &self.live {
            match RowState::of(m) {
                RowState::Streaming => c.0 += 1,
                RowState::Waiting => c.1 += 1,
                RowState::Idle | RowState::Transient => c.2 += 1,
                RowState::Parked => c.3 += 1,
            }
        }
        c
    }

    /// Apply one background message. Returns `false` when it was stale.
    pub(crate) fn apply(&mut self, msg: SessionsMsg, own_fallback: impl FnOnce() -> SessionMeta) -> bool {
        match msg {
            SessionsMsg::Live { gen, result } => {
                if gen != self.gen {
                    return false;
                }
                self.now = Utc::now();
                let own = own_fallback();
                match result {
                    Ok(mut rows) => {
                        // This TUI knows its own turn is running even when
                        // the daemon predates `busy` (older daemons omit it).
                        if let Some(m) = rows.iter_mut().find(|m| m.id.0 == self.own_id) {
                            m.busy |= own.busy;
                        }
                        self.source = LiveSource::Daemon;
                        self.set_live(rows);
                    }
                    Err(e) => {
                        self.source = LiveSource::DaemonDown(e);
                        self.set_live(vec![own]);
                    }
                }
            }
            SessionsMsg::Past { gen, result } => {
                if gen != self.gen {
                    return false;
                }
                self.past.set(result);
            }
            SessionsMsg::OpDone { gen, op, result } => {
                if gen != self.gen {
                    return false;
                }
                self.pending = None;
                self.notice = Some(op_notice(&op, result));
            }
        }
        true
    }
}

/// The inline line a finished pin/close leaves.
fn op_notice(op: &SessionsOp, result: Result<String, String>) -> Notice {
    match (op, result) {
        (SessionsOp::Pin { on: true, .. }, Ok(_)) => Notice { text: "pinned — it stays loaded".into(), error: false },
        (SessionsOp::Pin { on: false, .. }, Ok(_)) => Notice { text: "unpinned — it parks when idle".into(), error: false },
        (SessionsOp::Pin { .. }, Err(e)) => Notice { text: format!("pin failed: {e}"), error: true },
        (SessionsOp::Close { label, .. }, Ok(_)) => Notice { text: format!("closed {label}"), error: false },
        (SessionsOp::Close { label, .. }, Err(e)) => Notice { text: format!("{label} not closed: {}", e.trim_start_matches("not closed: ")), error: true },
    }
}

// ── formatting helpers (shared by draw + tests) ─────────────────────────────

/// `42s`, `3m`, `2h`, `5d` — coarse elapsed/remaining time.
pub(crate) fn short_dur(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86_400 {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / 86_400)
    }
}

/// The row's state text: `streaming 2m`, `waiting on you`,
/// `idle 4m · parks in 42s`, `parked 12m · evicts in 48m`.
pub(crate) fn state_text(m: &SessionMeta, now: DateTime<Utc>) -> String {
    let since = m
        .state_since
        .map(|t| format!(" {}", short_dur((now - t).num_seconds())))
        .unwrap_or_default();
    let until = |due: Option<DateTime<Utc>>, verb: &str| {
        due.map(|d| {
            let left = (d - now).num_seconds();
            if left <= 0 {
                format!(" · {verb} now")
            } else {
                format!(" · {verb} in {}", short_dur(left))
            }
        })
        .unwrap_or_default()
    };
    match RowState::of(m) {
        RowState::Streaming => format!("streaming{since}"),
        RowState::Waiting => "waiting on you".to_string(),
        RowState::Idle if m.pinned => format!("idle{since}"),
        RowState::Idle => format!("idle{since}{}", until(m.park_due, "parks")),
        RowState::Transient => match m.lifecycle {
            SessionLifecycle::Ending => "ending…".to_string(),
            _ => "parking…".to_string(),
        },
        RowState::Parked => format!("parked{since}{}", until(m.evict_due, "evicts")),
    }
}

/// Clients column: `you`, `you +1`, `2 clients`, `—`.
pub(crate) fn clients_text(m: &SessionMeta, own: bool) -> String {
    match (own, m.clients) {
        (true, 0 | 1) => "you".to_string(),
        (true, n) => format!("you +{}", n - 1),
        (false, 0) => "\u{2014}".to_string(),
        (false, 1) => "1 client".to_string(),
        (false, n) => format!("{n} clients"),
    }
}

/// `anthropic/claude-opus-5-5` → `opus-5-5`.
pub(crate) fn short_model(model: &str) -> String {
    let tail = model.rsplit('/').next().unwrap_or(model);
    tail.strip_prefix("claude-").unwrap_or(tail).to_string()
}

/// cwd with `$HOME` as `~`.
pub(crate) fn short_cwd(cwd: Option<&std::path::Path>, home: Option<&std::path::Path>) -> String {
    let Some(cwd) = cwd else { return String::new() };
    if let Some(home) = home {
        if cwd == home {
            return "~/".to_string();
        }
        if let Ok(rest) = cwd.strip_prefix(home) {
            return format!("~/{}", rest.display());
        }
    }
    cwd.display().to_string()
}

/// The 4-char tail of an id (more recognisable than a prefix).
pub(crate) fn short_id(id: &str) -> &str {
    let mut start = id.len().saturating_sub(4);
    while !id.is_char_boundary(start) {
        start -= 1;
    }
    &id[start..]
}

/// A session's name, else `…` + its short id.
pub(crate) fn label(m: &SessionMeta) -> String {
    match m.name.as_deref().filter(|n| !n.is_empty()) {
        Some(n) => n.to_string(),
        None => format!("\u{2026}{}", short_id(&m.id.0)),
    }
}

/// `$12.40` (`<$0.01` for a few tokens; empty when nothing was spent).
pub(crate) fn cost_text(usd: f64) -> String {
    if usd <= 0.0 {
        String::new()
    } else if usd < 0.005 {
        "<$0.01".to_string()
    } else if usd >= 100.0 {
        format!("${usd:.0}")
    } else {
        format!("${usd:.2}")
    }
}

// ── open / close / poll ─────────────────────────────────────────────────────

/// This TUI's own session as a Live row, for the in-process / daemon-down
/// fallback (the transport's meta, with the app's live busy flag).
pub(crate) fn own_row(app: &super::app::App, link: &super::session_link::SessionLink) -> SessionMeta {
    own_row_from(link.transport().meta(), app)
}

pub(crate) fn own_row_from(meta: &SessionMeta, app: &super::app::App) -> SessionMeta {
    let mut m = meta.clone();
    m.lifecycle = SessionLifecycle::Live;
    m.busy = app.streaming || app.compacting;
    m.clients = m.clients.max(1);
    if m.name.is_none() {
        m.name = app.session.name.clone();
    }
    m
}

/// Open the modal: state, stack push, and (daemon mode) the poller + the
/// Past list load. Safe to call while streaming.
pub(crate) fn open(app: &mut super::app::App, link: &super::session_link::SessionLink) {
    let socket = link.transport().socket_path().map(|p| p.to_path_buf());
    let own = own_row(app, link);
    open_with(app, own, socket);
}

pub(crate) fn open_with(app: &mut super::app::App, own: SessionMeta, socket: Option<PathBuf>) {
    if app.sessions.is_some() {
        return;
    }
    app.sessions_gen = app.sessions_gen.wrapping_add(1);
    let gen = app.sessions_gen;
    app.sessions = Some(SessionsState::new(gen, own, socket.is_some()));
    app.modal_stack.push(super::focus::PaneId::Sessions);
    #[cfg(debug_assertions)]
    super::focus::debug_assert_stack_sync(app);

    // Background work needs a runtime; the headless harness has none (it
    // feeds messages directly).
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    let tx = app.sessions_tx.clone();
    {
        let tx = tx.clone();
        tokio::task::spawn_blocking(move || {
            let result = synaps_cli::list_recent_sessions(past::PAST_LIMIT).map_err(|e| e.to_string());
            let _ = tx.send(SessionsMsg::Past { gen, result });
        });
    }
    if let Some(path) = socket {
        app.sessions_poller = Some(AbortOnDrop(tokio::spawn(poll_loop(path, gen, tx))));
    }
}

async fn poll_loop(path: PathBuf, gen: u64, tx: tokio::sync::mpsc::UnboundedSender<SessionsMsg>) {
    use agent_engine::session::socket_transport::SocketTransport;
    loop {
        let result = match tokio::time::timeout(POLL_TIMEOUT, SocketTransport::sessions(&path)).await {
            Ok(Ok(rows)) => Ok(rows),
            Ok(Err(e)) => Err(format!("daemon unreachable: {e}")),
            Err(_) => Err("daemon did not answer".to_string()),
        };
        if tx.send(SessionsMsg::Live { gen, result }).is_err() {
            return;
        }
        tokio::time::sleep(POLL_EVERY).await;
    }
}

/// Run a pin/close against the daemon on a task; the answer comes back as
/// `OpDone`. No socket (in-process) → answered at once with the reason.
pub(crate) fn spawn_op(app: &mut super::app::App, socket: Option<PathBuf>, op: SessionsOp) {
    use agent_engine::session::socket_transport::SocketTransport;
    let Some(st) = app.sessions.as_mut() else { return };
    let gen = st.gen;
    let Some(path) = socket else {
        st.pending = None;
        st.notice = Some(Notice {
            text: "no daemon — pin and close need `synaps --attach`".into(),
            error: true,
        });
        return;
    };
    st.pending = Some(op.clone());
    st.notice = None;
    let tx = app.sessions_tx.clone();
    tokio::spawn(async move {
        let result = match &op {
            SessionsOp::Pin { id, on } => SocketTransport::pin(&path, &SessionId(id.clone()), *on)
                .await
                .map(|on| if on { "pinned".into() } else { "unpinned".into() })
                .map_err(|e| e.to_string()),
            SessionsOp::Close { id, .. } => {
                SocketTransport::close(&path, &SessionId(id.clone()), false, CLOSE_ATTACHED)
                    .await
                    .map_err(|e| match e {
                        agent_engine::session::transport::TransportError::Refused(why) => why,
                        other => other.to_string(),
                    })
            }
        };
        let _ = tx.send(SessionsMsg::OpDone { gen, op, result });
    });
}

/// Close the modal: drop state, pop the stack, stop the poller.
pub(crate) fn close(app: &mut super::app::App) {
    app.sessions = None;
    app.sessions_poller = None;
    app.modal_stack.remove(super::focus::PaneId::Sessions);
}

/// A task handle that aborts on drop (the poller stops when the modal closes).
pub(crate) struct AbortOnDrop(pub(crate) tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests;
