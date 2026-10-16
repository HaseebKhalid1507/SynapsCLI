//! Watching a subagent in its own thread view, and steering it from there.
//!
//! - **Switching.** Ctrl-↑ from the main thread focuses the newest worker
//!   (the tray's bottom row); more Ctrl-↑ walk to older ones. Ctrl-↓ walks
//!   back down, and past the newest returns to main. Esc also returns to
//!   main. `/watch [name|sa_N|N|main]` does the same by name. The workers on
//!   offer are the running ones plus any whose view is already open.
//! - **Feeding.** While a worker view is focused, its watch journal is paged
//!   with `SessionQuery::SubagentWatch` (one query in flight, every
//!   [`POLL_INTERVAL`], straight away while a page reports `more`). Each
//!   entry replays through `stream_handler::apply_transcript_event` on the
//!   worker's own store, so a worker renders exactly like the main
//!   conversation and can never touch it (or its scroll).
//! - **Steering.** Enter in a worker view sends the text to that worker
//!   (`SessionCommand::SubagentSteer`, input owner only); the engine also
//!   notes it in the main conversation so the main agent knows. A finished
//!   worker's view is read-only.

use std::time::{Duration, Instant};

use agent_engine::runtime::subagent_watch::{WatchEvent, WatchPage};
use agent_engine::session::SessionQuery;

use super::app::App;
use super::threads::{ThreadFocus, WorkerThread};
use super::transcript::ChatMessage;

/// Poll cadence while a worker view is focused.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// A poll with no reply after this is abandoned; the next one re-asks from
/// the same cursor, so nothing is lost.
pub(crate) const POLL_TIMEOUT: Duration = Duration::from_secs(5);
/// After Esc leaves a worker view, a second Esc this soon does not also
/// abort the main thread's turn (a double tap).
pub(crate) const ESC_GUARD: Duration = Duration::from_millis(300);
/// A finished worker's view, while not focused, is dropped this long after
/// it finished (the engine keeps its journal as long).
pub(crate) const FINISHED_THREAD_TTL: Duration = Duration::from_secs(900);
/// Worker transcripts are capped like a socket client's main transcript.
const WORKER_SCROLLBACK: (usize, usize) = (400, 2 * 1024 * 1024);

/// Watch bookkeeping on [`App`].
#[derive(Default)]
pub(crate) struct WatchState {
    /// The watch query in flight, if any.
    pending: Option<PendingPoll>,
    /// When the next poll is due (`None`: nothing to poll).
    next_poll: Option<Instant>,
    /// Esc presses before this do not abort the main turn ([`ESC_GUARD`]).
    esc_guard_until: Option<Instant>,
    /// The last steer sent, until the engine accepts or refuses it:
    /// `(handle_id, text)`.
    pending_steer: Option<(String, String)>,
    /// The attached daemon predates watch/steer: it answered with a
    /// `malformed frame` error ([`absorb_notice`]). Cleared on reconnect.
    unsupported: bool,
}

/// Shown when the attached daemon is an older binary without watch/steer.
pub(crate) const UNSUPPORTED_HINT: &str = "the attached daemon predates subagent watch \u{2014} \
     run `synaps daemon reload` (after installing this build) to enable it";

struct PendingPoll {
    id: u64,
    handle_id: String,
}

/// A worker that can be watched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub(crate) handle_id: String,
    pub(crate) subagent_id: u64,
    pub(crate) agent_name: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// Ctrl-↑: toward older workers (up the tray).
    Up,
    /// Ctrl-↓: toward newer workers, then main.
    Down,
}

/// The header banner of a focused worker view.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Banner {
    pub(crate) agent_name: String,
    pub(crate) handle_id: String,
    /// `running`, `completed`, … (`connecting` before the first page).
    pub(crate) status: String,
    pub(crate) steerable: bool,
}

// ───────────────────────────── focus ───────────────────────────────────────

/// The focused worker's handle id, if a worker view is focused.
pub(crate) fn focused(app: &App) -> Option<&str> {
    match app.threads.focus() {
        ThreadFocus::Main => None,
        ThreadFocus::Worker(id) => Some(id.as_str()),
    }
}

fn focused_thread_mut(app: &mut App) -> Option<&mut WorkerThread> {
    let id = focused(app)?.to_string();
    app.threads.worker_mut(&id)
}

/// Workers on offer, in start order (the tray's order, newest last): the
/// running ones (registry rows, plus HUD entries whose row has not arrived
/// yet) and every open view.
pub(crate) fn candidates(app: &App) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    let mut add = |c: Candidate| {
        if !out.iter().any(|o| o.handle_id == c.handle_id) {
            out.push(c);
        }
    };
    for row in &app.subagent_rows {
        if row.status == synaps_cli::tools::SubagentStatus::Running && !row.handle_id.is_empty() {
            add(Candidate {
                handle_id: row.handle_id.clone(),
                subagent_id: row.subagent_id,
                agent_name: row.agent_name.clone(),
            });
        }
    }
    // The 1 Hz rows lag a just-started worker; the HUD has it at once.
    // Handle ids are `sa_<subagent_id>` (tools/subagent/start.rs).
    for sa in app.subagents.iter().filter(|s| !s.done) {
        add(Candidate {
            handle_id: format!("sa_{}", sa.id),
            subagent_id: sa.id,
            agent_name: sa.name.clone(),
        });
    }
    for (id, w) in app.threads.workers() {
        if !w.gone {
            add(Candidate {
                handle_id: id.to_string(),
                subagent_id: w.subagent_id,
                agent_name: w.agent_name.clone(),
            });
        }
    }
    out.sort_by_key(|c| c.subagent_id);
    out
}

/// Ctrl-↑ / Ctrl-↓. Returns `false` when there was nowhere to go.
pub(crate) fn cycle(app: &mut App, step: Step, now: Instant) -> bool {
    let list = candidates(app);
    let at = focused(app).and_then(|id| list.iter().position(|c| c.handle_id == id));
    let target = match (step, at) {
        (Step::Up, None) => list.len().checked_sub(1),
        (Step::Up, Some(i)) => Some(i.saturating_sub(1)),
        (Step::Down, None) => None,
        (Step::Down, Some(i)) if i + 1 < list.len() => Some(i + 1),
        (Step::Down, Some(_)) => {
            focus_main(app);
            return true;
        }
    };
    match target {
        Some(i) => {
            focus_worker(app, &list[i], now);
            true
        }
        None => false,
    }
}

/// Open (if needed) and focus a worker's view; its journal is polled at once.
pub(crate) fn focus_worker(app: &mut App, c: &Candidate, now: Instant) {
    let clock = app.clock.clone();
    let thread = app.threads.open_worker(
        &c.handle_id,
        c.subagent_id,
        &c.agent_name,
        clock,
        WORKER_SCROLLBACK,
    );
    if thread.store.is_empty() {
        thread.store.push_msg(ChatMessage::System(format!(
            "subagent {} ({}) \u{2014} Enter steers it \u{00b7} Esc or Ctrl-\u{2193} returns to the main thread",
            c.agent_name, c.handle_id
        )));
        if app.watch.unsupported {
            thread.store.push_msg(ChatMessage::System(UNSUPPORTED_HINT.to_string()));
        }
    }
    app.threads.focus_worker(&c.handle_id);
    app.watch.next_poll = (!app.watch.unsupported).then_some(now);
    app.request_immediate_redraw();
}

/// Back to the main thread (its scroll position is exactly as it was left).
pub(crate) fn focus_main(app: &mut App) {
    app.threads.focus_main();
    app.watch.next_poll = None;
    app.request_immediate_redraw();
}

/// Esc in a worker view: back to main, and arm the [`ESC_GUARD`].
pub(crate) fn leave_on_esc(app: &mut App, now: Instant) {
    focus_main(app);
    app.watch.esc_guard_until = Some(now + ESC_GUARD);
}

/// An Esc that should not abort the main turn (it follows a leave-on-Esc).
pub(crate) fn esc_guarded(app: &App, now: Instant) -> bool {
    app.watch.esc_guard_until.is_some_and(|t| now < t)
}

/// The focused worker view's header banner.
pub(crate) fn banner(app: &App) -> Option<Banner> {
    let id = focused(app)?;
    let w = app.threads.worker(id)?;
    Some(Banner {
        agent_name: w.agent_name.clone(),
        handle_id: id.to_string(),
        status: if w.gone {
            "gone".to_string()
        } else if w.status.is_empty() {
            "connecting".to_string()
        } else {
            w.status.clone()
        },
        steerable: w.steerable,
    })
}

/// The focused worker's numeric id (the tray marks its row).
pub(crate) fn focused_subagent_id(app: &App) -> Option<u64> {
    focused(app)
        .and_then(|id| app.threads.worker(id))
        .map(|w| w.subagent_id)
}

/// Forget every worker view (the session was cleared or replaced).
pub(crate) fn reset(app: &mut App) {
    app.threads = Default::default();
    app.watch = WatchState::default();
}

/// Drop views nobody is looking at whose worker is gone, or finished more
/// than [`FINISHED_THREAD_TTL`] ago.
pub(crate) fn prune(app: &mut App, now: Instant) {
    let focus = focused(app).map(str::to_string);
    let stale: Vec<String> = app
        .threads
        .workers()
        .filter(|(id, w)| {
            focus.as_deref() != Some(*id)
                && (w.gone
                    || w.finished_at
                        .is_some_and(|t| now.saturating_duration_since(t) >= FINISHED_THREAD_TTL))
        })
        .map(|(id, _)| id.to_string())
        .collect();
    for id in stale {
        app.threads.close_worker(&id);
    }
}

// ───────────────────────────── /watch ──────────────────────────────────────

/// A notice in whichever thread is focused (a `/watch` typed in a worker
/// view must answer there, not in the hidden main transcript).
pub(crate) fn notice(app: &mut App, text: String) {
    match focused_thread_mut(app) {
        Some(w) => w.store.push_msg(ChatMessage::System(text)),
        None => app.transcript.push_msg(ChatMessage::System(text)),
    }
    app.request_redraw();
}

/// Every worker `/watch` can name: the candidates plus finished workers
/// still in the registry.
fn nameable(app: &App) -> Vec<Candidate> {
    let mut list = candidates(app);
    for row in &app.subagent_rows {
        if !row.handle_id.is_empty() && !list.iter().any(|c| c.handle_id == row.handle_id) {
            list.push(Candidate {
                handle_id: row.handle_id.clone(),
                subagent_id: row.subagent_id,
                agent_name: row.agent_name.clone(),
            });
        }
    }
    list.sort_by_key(|c| c.subagent_id);
    list
}

/// Resolve a `/watch` argument: `sa_N`, `N`, or an agent name (exact, then
/// a unique prefix; case-insensitive). An `sa_N` / `N` the client has not
/// seen is still tried: the engine keeps a reaped worker's journal for a
/// while, and the first page fills in its name.
fn resolve(app: &App, arg: &str) -> Result<Candidate, String> {
    let list = nameable(app);
    let handle = if let Some(n) = arg.strip_prefix("sa_") {
        n.parse::<u64>().ok().map(|n| (format!("sa_{n}"), n))
    } else {
        arg.parse::<u64>().ok().map(|n| (format!("sa_{n}"), n))
    };
    if let Some((handle_id, n)) = handle {
        return Ok(list
            .into_iter()
            .find(|c| c.handle_id == handle_id)
            .unwrap_or(Candidate {
                handle_id,
                subagent_id: n,
                agent_name: "subagent".to_string(),
            }));
    }
    let lower = arg.to_lowercase();
    let exact: Vec<&Candidate> = list
        .iter()
        .filter(|c| c.agent_name.to_lowercase() == lower)
        .collect();
    let hits: Vec<&Candidate> = if exact.is_empty() {
        list.iter()
            .filter(|c| c.agent_name.to_lowercase().starts_with(&lower))
            .collect()
    } else {
        exact
    };
    match hits.as_slice() {
        [one] => Ok((*one).clone()),
        // Several workers share the name: the newest is the one in view.
        [.., newest] if hits.iter().all(|c| c.agent_name == newest.agent_name) => {
            Ok((*newest).clone())
        }
        [] => Err(format!(
            "no subagent matches {arg:?} \u{2014} /watch lists them"
        )),
        _ => Err(format!(
            "{arg:?} matches {} \u{2014} use the sa_N id",
            hits.iter()
                .map(|c| format!("{} ({})", c.agent_name, c.handle_id))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

fn status_of(app: &App, handle_id: &str) -> String {
    if let Some(w) = app
        .threads
        .worker(handle_id)
        .filter(|w| !w.status.is_empty())
    {
        return w.status.clone();
    }
    app.subagent_rows
        .iter()
        .find(|r| r.handle_id == handle_id)
        .map(|r| match &r.status {
            synaps_cli::tools::SubagentStatus::Running => "running".to_string(),
            synaps_cli::tools::SubagentStatus::Completed => "completed".to_string(),
            synaps_cli::tools::SubagentStatus::Cancelled => "cancelled".to_string(),
            synaps_cli::tools::SubagentStatus::TimedOut => "timed out".to_string(),
            synaps_cli::tools::SubagentStatus::Failed(_) => "failed".to_string(),
        })
        .unwrap_or_else(|| "running".to_string())
}

/// `/watch [name|sa_N|N|main]`.
pub(crate) fn command(app: &mut App, arg: &str, now: Instant) {
    let arg = arg.trim();
    match arg {
        "main" | "off" | "back" | "exit" => {
            if focused(app).is_some() {
                focus_main(app);
            } else {
                notice(app, "already on the main thread".to_string());
            }
        }
        "" => {
            let list = nameable(app);
            if list.is_empty() {
                notice(app, "no subagents to watch".to_string());
                return;
            }
            if list.len() == 1 && focused(app).is_none() {
                focus_worker(app, &list[0], now);
                return;
            }
            let here = focused(app).map(str::to_string);
            let mut text = String::from(
                "subagents \u{2014} Ctrl-\u{2191}/\u{2193} to switch, /watch <name|sa_N>, /watch main:",
            );
            for c in &list {
                let mark = if here.as_deref() == Some(c.handle_id.as_str()) {
                    "\u{25b8}"
                } else {
                    " "
                };
                text.push_str(&format!(
                    "\n {mark} {}  {}  {}",
                    c.handle_id,
                    c.agent_name,
                    status_of(app, &c.handle_id)
                ));
            }
            notice(app, text);
        }
        _ => match resolve(app, arg) {
            Ok(c) => focus_worker(app, &c, now),
            Err(e) => notice(app, e),
        },
    }
}

// ───────────────────────────── polling ─────────────────────────────────────

/// When the watch poll arm should fire (`None`: idle).
pub(crate) fn poll_due_at(app: &App) -> Option<Instant> {
    focused(app)?;
    app.watch.next_poll
}

/// The query to send now, if any. The caller sends it and reports the id
/// with [`poll_sent`].
fn poll_query(app: &mut App, now: Instant) -> Option<(String, SessionQuery)> {
    let id = focused(app)?.to_string();
    if app.watch.unsupported {
        app.watch.next_poll = None;
        return None;
    }
    let Some(w) = app.threads.worker(&id) else {
        app.watch.next_poll = None;
        return None;
    };
    if w.gone || w.drained {
        app.watch.next_poll = None;
        return None;
    }
    // An unanswered poll past its timeout is abandoned (same cursor again).
    app.watch.pending = None;
    app.watch.next_poll = Some(now + POLL_TIMEOUT);
    Some((
        id.clone(),
        SessionQuery::SubagentWatch {
            handle_id: id,
            since: w.next_seq,
        },
    ))
}

/// The watch poll arm: send the focused worker's next page query, if due.
pub(crate) async fn poll(app: &mut App, link: &mut super::session_link::SessionLink, now: Instant) {
    if let Some((handle_id, query)) = poll_query(app, now) {
        let sent = link.send_query(query).await;
        poll_sent(app, handle_id, sent, now);
    }
}

/// The poll in flight: `(query id, handle id)`.
#[cfg(any(test, feature = "testing"))]
pub(crate) fn pending_poll(app: &App) -> Option<(u64, &str)> {
    app.watch
        .pending
        .as_ref()
        .map(|p| (p.id, p.handle_id.as_str()))
}

/// The poll went out as query `id` (or failed to send: retry later).
fn poll_sent(app: &mut App, handle_id: String, sent: Result<u64, String>, now: Instant) {
    match sent {
        Ok(id) => app.watch.pending = Some(PendingPoll { id, handle_id }),
        Err(_) => app.watch.next_poll = Some(now + POLL_INTERVAL * 4),
    }
}

/// A daemon notice. An older daemon cannot decode a watch query or a steer
/// and answers with `malformed frame: unknown variant `subagent_watch``
/// (the connection survives). Such a notice is taken here — shown once in
/// the worker view instead of every poll in the main transcript — and
/// polling stops until the next reconnect. Returns `true` when taken.
pub(crate) fn absorb_notice(app: &mut App, text: &str) -> bool {
    if !text.starts_with("malformed frame") {
        return false;
    }
    let watch = text.contains("`subagent_watch`");
    let steer = text.contains("`subagent_steer`");
    if !watch && !steer {
        return false;
    }
    let first = !app.watch.unsupported;
    app.watch.unsupported = true;
    app.watch.pending = None;
    app.watch.next_poll = None;
    if steer && app.watch.pending_steer.is_some() {
        steer_refused(app, UNSUPPORTED_HINT);
    } else if first {
        notice(app, UNSUPPORTED_HINT.to_string());
    }
    true
}

/// The daemon connection was re-established (possibly a newer image):
/// try watching again.
pub(crate) fn on_reconnect(app: &mut App, now: Instant) {
    app.watch.unsupported = false;
    app.watch.pending = None;
    if focused(app).is_some() {
        app.watch.next_poll = Some(now);
    }
}

/// A `QueryResult`. Returns `true` when it answered the watch poll.
pub(crate) fn on_query_result(
    app: &mut App,
    id: u64,
    value: &serde_json::Value,
    now: Instant,
) -> bool {
    let Some(pending) = app.watch.pending.as_ref().filter(|p| p.id == id) else {
        return false;
    };
    let handle_id = pending.handle_id.clone();
    // Query ids are per client; make sure this reply is about our worker.
    let reply = match value.get("error").and_then(|e| e.as_str()) {
        Some(err) if err.contains(handle_id.as_str()) => Err(err.to_string()),
        Some(_) => return false,
        None => match serde_json::from_value::<WatchPage>(value.clone()) {
            Ok(page) if page.handle_id == handle_id => Ok(page),
            _ => return false,
        },
    };
    app.watch.pending = None;
    let focused_here = focused(app) == Some(handle_id.as_str());
    let Some(w) = app.threads.worker_mut(&handle_id) else {
        return true;
    };
    let next = match reply {
        Ok(page) => {
            let more = page.more;
            if apply_page(w, page, now) {
                w.store.invalidate();
            }
            if more {
                Some(now)
            } else if w.drained {
                None
            } else {
                Some(now + POLL_INTERVAL)
            }
        }
        Err(err) => {
            w.gone = true;
            w.steerable = false;
            w.store.push_msg(ChatMessage::System(format!(
                "{err} \u{2014} a finished subagent stays watchable for 15 minutes"
            )));
            None
        }
    };
    if focused_here {
        app.watch.next_poll = next;
    }
    app.request_redraw();
    true
}

/// Replay one page into a worker's store. Returns `true` when a response
/// rollback needs the line cache rebuilt.
pub(crate) fn apply_page(w: &mut WorkerThread, page: WatchPage, now: Instant) -> bool {
    let was = std::mem::take(&mut w.status);
    w.agent_name = page.agent_name;
    w.subagent_id = page.subagent_id;
    if page.truncated {
        w.store.push_msg(ChatMessage::System(
            "\u{2026} earlier output was dropped (a worker keeps its last 2 MiB)".to_string(),
        ));
    }
    let mut rolled_back = false;
    for ev in &page.events {
        match ev.to_stream() {
            Some(stream) => {
                let applied = super::stream_handler::apply_transcript_event(
                    &mut w.store,
                    &mut w.response_preview,
                    &stream,
                );
                rolled_back |= matches!(applied, super::stream_handler::Applied::RolledBack);
            }
            None => {
                if let WatchEvent::Failed { category } = ev {
                    w.store.end_response();
                    w.store.drop_empty_thinking();
                    w.store
                        .push_msg(ChatMessage::Error(format!("subagent failed [{category}]")));
                }
            }
        }
    }
    w.next_seq = page.next_seq;
    w.steerable = page.steerable;
    let finished = page.status != "running";
    if finished && (was.is_empty() || was == "running") {
        w.store.end_response();
        w.store.drop_empty_thinking();
        w.store.push_msg(ChatMessage::System(format!(
            "{} {} \u{2014} this view is read-only",
            w.agent_name,
            page.status.replace('_', " ")
        )));
        w.finished_at = Some(now);
    }
    w.drained = finished && !page.more;
    w.status = page.status;
    rolled_back
}

// ───────────────────────────── steering ────────────────────────────────────

/// Enter in a worker view: the worker to steer, or why it can't be.
pub(crate) fn steer_target(app: &App) -> Option<Result<String, String>> {
    let id = focused(app)?;
    let w = app.threads.worker(id)?;
    Some(if app.watch.unsupported {
        Err(UNSUPPORTED_HINT.to_string())
    } else if w.steerable {
        Ok(id.to_string())
    } else if w.status.is_empty() && !w.gone {
        Err(format!(
            "{} is still connecting \u{2014} try again in a moment",
            w.agent_name
        ))
    } else {
        Err(format!(
            "{} is not running \u{2014} its view is read-only",
            w.agent_name
        ))
    })
}

/// Explain, in the focused worker view, why Enter did not steer.
pub(crate) fn steer_unavailable(app: &mut App, reason: String) {
    notice(app, reason);
}

/// The steer is on its way: note it in the worker's view. The worker shows
/// it as a user message once it takes it in (at its next step).
pub(crate) fn steer_sent(app: &mut App, handle_id: &str, text: &str) {
    app.watch.pending_steer = Some((handle_id.to_string(), text.to_string()));
    if let Some(w) = app.threads.worker_mut(handle_id) {
        w.store.push_msg(ChatMessage::System(format!(
            "\u{2192} steering {} at its next step: {text}",
            w.agent_name
        )));
    }
    app.request_redraw();
}

/// The engine noted the steer in the main conversation: it was accepted.
pub(crate) fn steer_accepted(app: &mut App) {
    app.watch.pending_steer = None;
}

/// `Refused{command: "subagent_steer"}` for this client: say so in the
/// worker's view and give the text back if the box is still empty there.
pub(crate) fn steer_refused(app: &mut App, reason: &str) {
    let Some((handle_id, text)) = app.watch.pending_steer.take() else {
        app.transcript.push_msg(ChatMessage::Error(format!(
            "subagent steer refused: {reason}"
        )));
        app.request_redraw();
        return;
    };
    match app.threads.worker_mut(&handle_id) {
        Some(w) => w
            .store
            .push_msg(ChatMessage::Error(format!("steer refused: {reason}"))),
        None => app.transcript.push_msg(ChatMessage::Error(format!(
            "subagent steer refused: {reason}"
        ))),
    }
    if focused(app) == Some(handle_id.as_str()) && app.input_is_empty() {
        app.set_input_text(&text);
    }
    app.request_redraw();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::SubagentState;
    use synaps_cli::tools::{SubagentDisplayRow, SubagentStatus};
    use synaps_cli::Session;

    fn app() -> App {
        App::new(Session::new("test-model", "low", None))
    }

    fn row(id: u64, name: &str, status: SubagentStatus) -> SubagentDisplayRow {
        SubagentDisplayRow {
            subagent_id: id,
            agent_name: name.into(),
            status,
            cancel_requested: false,
            elapsed_secs: 1.0,
            finished_elapsed: None,
            step: String::new(),
            tools: 0,
            handle_id: format!("sa_{id}"),
            model: String::new(),
        }
    }

    fn hud(id: u64, name: &str, done: bool) -> SubagentState {
        SubagentState {
            id,
            name: name.into(),
            status: "running".into(),
            start_time: Instant::now(),
            done,
            duration_secs: None,
            done_at: None,
            tools: 0,
            result: None,
        }
    }

    fn page(handle: &str, status: &str, events: Vec<WatchEvent>, next_seq: u64) -> WatchPage {
        WatchPage {
            handle_id: handle.into(),
            subagent_id: handle[3..].parse().unwrap(),
            agent_name: "scout".into(),
            model: String::new(),
            status: status.into(),
            steerable: status == "running",
            next_seq,
            truncated: false,
            more: false,
            events,
        }
    }

    fn text(t: &str) -> WatchEvent {
        WatchEvent::Text { text: t.into() }
    }

    fn texts(app: &App, handle: &str) -> Vec<String> {
        app.threads
            .worker(handle)
            .unwrap()
            .store
            .messages()
            .iter()
            .map(|m| m.msg.source_text().into_owned())
            .collect()
    }

    /// Focus `handle` and answer its first poll with `p`.
    fn focus_and_answer(app: &mut App, handle: &str, p: WatchPage) {
        let now = Instant::now();
        command(app, handle, now);
        let (_, query) = poll_query(app, now).expect("a poll is due");
        assert!(matches!(query, SessionQuery::SubagentWatch { .. }));
        poll_sent(app, handle.to_string(), Ok(41), now);
        assert!(on_query_result(
            app,
            41,
            &serde_json::to_value(p).unwrap(),
            now
        ));
    }

    #[test]
    fn candidates_are_running_workers_and_open_views_in_start_order() {
        let mut app = app();
        app.subagent_rows = vec![
            row(3, "late", SubagentStatus::Running),
            row(1, "early", SubagentStatus::Running),
            row(2, "finished", SubagentStatus::Completed),
        ];
        // A worker whose 1 Hz row has not arrived yet, and a finished HUD entry.
        app.subagents = vec![hud(4, "fresh", false), hud(2, "finished", true)];
        let ids: Vec<String> = candidates(&app).into_iter().map(|c| c.handle_id).collect();
        assert_eq!(ids, ["sa_1", "sa_3", "sa_4"]);

        // An open view stays on offer after its worker finished.
        command(&mut app, "sa_2", Instant::now());
        let ids: Vec<String> = candidates(&app).into_iter().map(|c| c.handle_id).collect();
        assert_eq!(ids, ["sa_1", "sa_2", "sa_3", "sa_4"]);
    }

    #[test]
    fn ctrl_up_opens_the_newest_and_walks_older_ctrl_down_walks_back_to_main() {
        let mut app = app();
        let now = Instant::now();
        assert!(!cycle(&mut app, Step::Up, now), "nothing to watch");
        assert!(!cycle(&mut app, Step::Down, now), "already on main");

        app.subagent_rows = vec![
            row(1, "a", SubagentStatus::Running),
            row(2, "b", SubagentStatus::Running),
        ];
        assert!(cycle(&mut app, Step::Up, now));
        assert_eq!(focused(&app), Some("sa_2"), "newest first (tray bottom)");
        assert!(cycle(&mut app, Step::Up, now));
        assert_eq!(focused(&app), Some("sa_1"));
        assert!(cycle(&mut app, Step::Up, now));
        assert_eq!(focused(&app), Some("sa_1"), "stops at the oldest");
        assert!(cycle(&mut app, Step::Down, now));
        assert_eq!(focused(&app), Some("sa_2"));
        assert!(cycle(&mut app, Step::Down, now));
        assert_eq!(focused(&app), None, "past the newest is main");
        assert!(
            app.transcript.is_empty(),
            "the main transcript is untouched"
        );
    }

    #[test]
    fn pages_replay_into_the_worker_view_only_and_advance_the_cursor() {
        let mut app = app();
        app.transcript
            .push_msg(ChatMessage::Text("main thread".into()));
        app.subagent_rows = vec![row(7, "scout", SubagentStatus::Running)];
        focus_and_answer(
            &mut app,
            "sa_7",
            page(
                "sa_7",
                "running",
                vec![WatchEvent::ResponseStart, text("Reading"), text(" files.")],
                3,
            ),
        );
        assert!(texts(&app, "sa_7").iter().any(|t| t == "Reading files."));
        assert_eq!(app.transcript.messages().len(), 1, "main untouched");
        let w = app.threads.worker("sa_7").unwrap();
        assert_eq!((w.next_seq, w.steerable, w.drained), (3, true, false));

        // The next poll asks from the cursor, after the poll interval.
        let due = poll_due_at(&app).unwrap();
        assert!(due > Instant::now() && due <= Instant::now() + POLL_INTERVAL);
        let (_, q) = poll_query(&mut app, Instant::now()).unwrap();
        assert!(
            matches!(&q, SessionQuery::SubagentWatch { handle_id, since: 3 } if handle_id == "sa_7"),
            "{q:?}"
        );
    }

    #[test]
    fn replies_to_other_queries_or_workers_are_not_taken() {
        let mut app = app();
        let now = Instant::now();
        command(&mut app, "sa_7", now);
        poll_query(&mut app, now).unwrap();
        poll_sent(&mut app, "sa_7".into(), Ok(5), now);

        let other = serde_json::to_value(page("sa_8", "running", vec![text("x")], 1)).unwrap();
        assert!(!on_query_result(&mut app, 4, &other, now), "another id");
        assert!(!on_query_result(&mut app, 5, &other, now), "another worker");
        assert!(!on_query_result(
            &mut app,
            5,
            &serde_json::json!({ "unsupported": "x" }),
            now
        ));
        assert!(pending_poll(&app).is_some(), "still waiting for ours");
    }

    #[test]
    fn a_finished_worker_is_read_only_and_polling_stops_once_drained() {
        let mut app = app();
        let mut p = page("sa_7", "completed", vec![text("done"), WatchEvent::Done], 2);
        p.more = true;
        focus_and_answer(&mut app, "sa_7", p);
        assert!(
            poll_due_at(&app).unwrap() <= Instant::now(),
            "more: ask again at once"
        );
        let w = app.threads.worker("sa_7").unwrap();
        assert!(!w.steerable && !w.drained);
        assert!(texts(&app, "sa_7").iter().any(|t| t.contains("read-only")));
        assert!(matches!(steer_target(&app), Some(Err(e)) if e.contains("not running")));

        let now = Instant::now();
        poll_query(&mut app, now).unwrap();
        poll_sent(&mut app, "sa_7".into(), Ok(42), now);
        let last = serde_json::to_value(page("sa_7", "completed", vec![], 2)).unwrap();
        assert!(on_query_result(&mut app, 42, &last, now));
        assert!(app.threads.worker("sa_7").unwrap().drained);
        assert_eq!(poll_due_at(&app), None, "nothing more to read");
        assert_eq!(
            texts(&app, "sa_7")
                .iter()
                .filter(|t| t.contains("read-only"))
                .count(),
            1,
            "the finish is announced once"
        );
    }

    #[test]
    fn failures_truncation_and_a_vanished_worker_are_shown() {
        let mut app = app();
        let mut p = page(
            "sa_7",
            "failed",
            vec![
                text("partial"),
                WatchEvent::Failed {
                    category: "network".into(),
                },
            ],
            9,
        );
        p.truncated = true;
        focus_and_answer(&mut app, "sa_7", p);
        let t = texts(&app, "sa_7");
        assert!(
            t.iter().any(|t| t.contains("earlier output was dropped")),
            "{t:?}"
        );
        assert!(t.iter().any(|t| t == "subagent failed [network]"), "{t:?}");

        let now = Instant::now();
        command(&mut app, "sa_9", now);
        poll_query(&mut app, now).unwrap();
        poll_sent(&mut app, "sa_9".into(), Ok(50), now);
        let gone = serde_json::json!({ "error": "no subagent sa_9 in this session" });
        assert!(on_query_result(&mut app, 50, &gone, now));
        assert!(app.threads.worker("sa_9").unwrap().gone);
        assert_eq!(poll_due_at(&app), None);
        assert_eq!(banner(&app).unwrap().status, "gone");
    }

    #[test]
    fn watch_command_resolves_names_ids_and_main() {
        let mut app = app();
        let now = Instant::now();
        app.subagent_rows = vec![
            row(1, "scout", SubagentStatus::Running),
            row(2, "fixer", SubagentStatus::Running),
            row(3, "fixer-two", SubagentStatus::Completed),
        ];
        command(&mut app, "SCOUT", now);
        assert_eq!(focused(&app), Some("sa_1"), "case-insensitive name");
        command(&mut app, "2", now);
        assert_eq!(focused(&app), Some("sa_2"), "bare number");
        command(&mut app, "sa_3", now);
        assert_eq!(
            focused(&app),
            Some("sa_3"),
            "finished workers can be opened"
        );
        command(&mut app, "fix", now);
        assert_eq!(
            focused(&app),
            Some("sa_3"),
            "ambiguous prefix: focus unchanged"
        );
        assert!(texts(&app, "sa_3")
            .iter()
            .any(|t| t.contains("use the sa_N id")));
        command(&mut app, "fixer", now);
        assert_eq!(focused(&app), Some("sa_2"), "exact name beats prefix");
        command(&mut app, "", now);
        let listing = texts(&app, "sa_2").last().cloned().unwrap();
        assert!(
            listing.contains("\u{25b8} sa_2") && listing.contains("sa_3"),
            "{listing}"
        );
        command(&mut app, "main", now);
        assert_eq!(focused(&app), None);
        command(&mut app, "nobody", now);
        assert!(matches!(
            &app.transcript.messages().last().unwrap().msg,
            ChatMessage::System(t) if t.contains("no subagent matches")
        ));
    }

    #[test]
    fn a_lone_worker_opens_directly_and_unknown_ids_are_still_tried() {
        let mut app = app();
        app.subagent_rows = vec![row(4, "solo", SubagentStatus::Running)];
        command(&mut app, "", Instant::now());
        assert_eq!(focused(&app), Some("sa_4"));
        command(&mut app, "sa_99", Instant::now());
        assert_eq!(
            focused(&app),
            Some("sa_99"),
            "the engine may still hold its journal"
        );
    }

    #[test]
    fn esc_leaves_a_worker_view_and_guards_a_double_tap() {
        let mut app = app();
        let now = Instant::now();
        command(&mut app, "sa_1", now);
        leave_on_esc(&mut app, now);
        assert_eq!(focused(&app), None);
        assert_eq!(poll_due_at(&app), None, "no polling on main");
        assert!(esc_guarded(&app, now + ESC_GUARD / 2));
        assert!(!esc_guarded(&app, now + ESC_GUARD));
    }

    #[test]
    fn an_older_daemon_is_told_once_in_the_worker_view_and_polling_stops() {
        let mut app = app();
        focus_and_answer(&mut app, "sa_7", page("sa_7", "running", vec![], 0));
        let now = Instant::now();
        assert!(poll_query(&mut app, now).is_some());
        poll_sent(&mut app, "sa_7".to_string(), Ok(42), now);

        let bad = "malformed frame: unknown variant `subagent_watch`, expected one of `context_report`";
        assert!(absorb_notice(&mut app, bad));
        assert!(absorb_notice(&mut app, bad), "repeats are taken too");
        let hints = texts(&app, "sa_7")
            .iter()
            .filter(|t| t.contains("predates subagent watch"))
            .count();
        assert_eq!(hints, 1, "the hint is shown once");
        assert!(app.transcript.is_empty(), "nothing in main");
        assert_eq!(poll_due_at(&app), None);
        assert!(poll_query(&mut app, now).is_none());
        assert_eq!(steer_target(&app), Some(Err(UNSUPPORTED_HINT.to_string())));

        // Other notices are not ours.
        assert!(!absorb_notice(&mut app, "malformed frame: expected value at line 1"));
        assert!(!absorb_notice(&mut app, "unknown variant `subagent_watch`"));

        // A reconnect (maybe to a reloaded, newer daemon) tries again.
        on_reconnect(&mut app, now);
        assert_eq!(poll_due_at(&app), Some(now));
        assert!(poll_query(&mut app, now).is_some());
        assert_eq!(steer_target(&app), Some(Ok("sa_7".to_string())));

        // A steer the daemon cannot decode comes back like a refusal.
        steer_sent(&mut app, "sa_7", "check the tests");
        assert!(absorb_notice(
            &mut app,
            "malformed frame: unknown variant `subagent_steer`, expected one of `submit`"
        ));
        assert!(texts(&app, "sa_7")
            .last()
            .unwrap()
            .contains("steer refused: the attached daemon predates"));
        assert_eq!(app.input_text(), "check the tests");
        assert!(app.transcript.is_empty(), "nothing in main");
    }

    #[test]
    fn a_refused_steer_is_shown_in_the_worker_view_and_the_text_comes_back() {
        let mut app = app();
        focus_and_answer(&mut app, "sa_7", page("sa_7", "running", vec![], 0));
        assert_eq!(steer_target(&app), Some(Ok("sa_7".to_string())));
        steer_sent(&mut app, "sa_7", "check the tests");
        assert!(texts(&app, "sa_7")
            .last()
            .unwrap()
            .contains("steering scout"));

        steer_refused(&mut app, "input owned by client #2");
        assert!(texts(&app, "sa_7")
            .last()
            .unwrap()
            .contains("steer refused: input owned by client #2"));
        assert_eq!(app.input_text(), "check the tests");
        assert!(app.transcript.is_empty(), "nothing in main");

        // Accepted steers clear the pending text: a later stray refusal
        // lands in main and gives nothing back.
        app.clear_input();
        steer_sent(&mut app, "sa_7", "again");
        steer_accepted(&mut app);
        steer_refused(&mut app, "late");
        assert!(app.input_is_empty());
    }

    #[test]
    fn stale_unfocused_views_are_pruned() {
        let mut app = app();
        let now = Instant::now();
        focus_and_answer(&mut app, "sa_1", page("sa_1", "completed", vec![], 0));
        command(&mut app, "sa_2", now);
        app.threads.worker_mut("sa_2").unwrap().gone = true;
        focus_main(&mut app);
        command(&mut app, "sa_3", now); // focused: kept whatever its state
        app.threads.worker_mut("sa_3").unwrap().gone = true;

        prune(&mut app, now);
        assert!(
            app.threads.worker("sa_1").is_some(),
            "finished, within its window"
        );
        assert!(app.threads.worker("sa_2").is_none(), "gone");
        assert!(app.threads.worker("sa_3").is_some(), "focused");
        prune(&mut app, now + FINISHED_THREAD_TTL + Duration::from_secs(1));
        assert!(app.threads.worker("sa_1").is_none(), "past its window");
    }
}
