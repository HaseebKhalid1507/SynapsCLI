//! Live `tool_child` / `tool_child_ended` events: how a tool's spawn report
//! reaches its daemon session.
//!
//! - A session actor owns one [`ChildRoute`] (its daemon session id plus an
//!   unbounded channel the actor drains) and sets it on every runtime it
//!   builds.
//! - The runtime tags each tool call's `DeltaSender` with a [`ChildSink`]
//!   (the route plus the call's tool id and name). `bash` and `shell_start`
//!   open a [`ChildWatch`] right after `spawn()` returns and close it at the
//!   reap.
//! - Workers inherit the parent's route with their own handle id
//!   ([`worker_route`]), so a worker's children are reported on the parent
//!   session with `handle_id` set.
//! - **The ledger is written first, then the event is sent**: a `children`
//!   query issued on receipt of the event finds the record.
//! - [`EventOrder`] is the actor's ordering rule: a foreground event waits
//!   until its call's `tool_use` has gone out, so clients see `tool_use`,
//!   then `tool_child`, then the call's output.
//!
//! The events are optional (`Hello.accepted_events`), never replayed on
//! Attach, and not part of the conversation.

use std::collections::HashSet;
use std::sync::Arc;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use super::children::{self, ChildIdentity, ChildSpawn};

/// `SIGKILL`: the signal a timeout, a cancel or the output cap sends.
pub const KILLED_BY: i32 = 9;

/// One report about one spawned child.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChildProcessEvent {
    /// `tool_child`.
    Spawned(ChildSpawn),
    /// `tool_child_ended`: the spawned process itself was reaped.
    Ended {
        tool_id: String,
        handle_id: Option<String>,
        child: ChildIdentity,
        exit_code: Option<i32>,
        signal: Option<i32>,
    },
}

impl ChildProcessEvent {
    pub fn tool_id(&self) -> &str {
        match self {
            Self::Spawned(spawn) => &spawn.tool_id,
            Self::Ended { tool_id, .. } => tool_id,
        }
    }

    pub fn handle_id(&self) -> Option<&str> {
        match self {
            Self::Spawned(spawn) => spawn.handle_id.as_deref(),
            Self::Ended { handle_id, .. } => handle_id.as_deref(),
        }
    }
}

/// Where a runtime's child reports go: one daemon session, and the worker
/// (if any) the runtime runs.
#[derive(Clone)]
pub struct ChildRoute {
    session_id: Arc<str>,
    handle_id: Option<Arc<str>>,
    tx: UnboundedSender<ChildProcessEvent>,
}

impl std::fmt::Debug for ChildRoute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildRoute")
            .field("session_id", &self.session_id)
            .field("handle_id", &self.handle_id)
            .finish_non_exhaustive()
    }
}

impl ChildRoute {
    /// A session's route (foreground: no handle id) and the receiver its
    /// actor drains.
    pub fn channel(session_id: &str) -> (Self, UnboundedReceiver<ChildProcessEvent>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (Self { session_id: Arc::from(session_id), handle_id: None, tx }, rx)
    }

    /// The daemon session id the ledger records under.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn handle_id(&self) -> Option<&str> {
        self.handle_id.as_deref()
    }

    /// The same session and channel, for the worker `handle_id`.
    pub fn for_worker(&self, handle_id: &str) -> Self {
        Self { handle_id: Some(Arc::from(handle_id)), ..self.clone() }
    }

    /// The sink for one tool call.
    pub fn sink(&self, tool_id: &str, tool_name: &str) -> ChildSink {
        ChildSink { route: self.clone(), tool_id: tool_id.to_string(), tool_name: tool_name.to_string() }
    }
}

/// The route a worker started from a tool call inherits: the calling
/// runtime's route with the worker's own handle id. `None` when the caller
/// has no route (no daemon session, e.g. a test runtime).
pub fn worker_route(
    caller: Option<&crate::tools::output::DeltaSender>,
    handle_id: &str,
) -> Option<ChildRoute> {
    caller.and_then(|delta| delta.child_sink()).map(|sink| sink.route().for_worker(handle_id))
}

/// One tool call's way to report the children it spawns.
#[derive(Clone, Debug)]
pub struct ChildSink {
    route: ChildRoute,
    tool_id: String,
    tool_name: String,
}

impl ChildSink {
    pub fn route(&self) -> &ChildRoute {
        &self.route
    }

    /// Record `pid` (read its identity now, while it is unreaped), then send
    /// `tool_child`. The event is sent even when the ledger caps dropped the
    /// record. `None` when the identity can't be read (off Linux, or gone).
    pub fn spawned(&self, pid: u32, shell_session_id: Option<&str>) -> Option<ChildIdentity> {
        let child = children::identify(pid)?;
        let spawn = ChildSpawn {
            tool_id: self.tool_id.clone(),
            tool_name: self.tool_name.clone(),
            handle_id: self.route.handle_id().map(str::to_string),
            shell_session_id: shell_session_id.map(str::to_string),
            child: child.clone(),
        };
        children::record_spawn(self.route.session_id(), spawn.clone());
        let _ = self.route.tx.send(ChildProcessEvent::Spawned(spawn));
        Some(child)
    }

    /// Mark `child` ended in the ledger, then send `tool_child_ended`.
    pub fn ended(&self, child: &ChildIdentity, exit_code: Option<i32>, signal: Option<i32>) {
        children::record_end(child, exit_code, signal);
        let _ = self.route.tx.send(ChildProcessEvent::Ended {
            tool_id: self.tool_id.clone(),
            handle_id: self.route.handle_id().map(str::to_string),
            child: child.clone(),
            exit_code,
            signal,
        });
    }
}

/// One spawned child, from `spawn()` to its reap: reports `ended` exactly
/// once. Dropped without a report (a timeout or a cancel drops the tool's
/// future, and the process group is killed), it reports [`KILLED_BY`].
#[derive(Debug, Default)]
pub struct ChildWatch {
    armed: Option<(ChildSink, ChildIdentity)>,
}

impl ChildWatch {
    /// Report the spawn of `pid` through `sink`. Without a sink or a pid it
    /// watches nothing.
    pub fn start(sink: Option<&ChildSink>, pid: Option<u32>, shell_session_id: Option<&str>) -> Self {
        let armed = sink.zip(pid).and_then(|(sink, pid)| {
            sink.spawned(pid, shell_session_id).map(|child| (sink.clone(), child))
        });
        Self { armed }
    }

    pub fn identity(&self) -> Option<&ChildIdentity> {
        self.armed.as_ref().map(|(_, child)| child)
    }

    /// The child was reaped with `status`.
    pub fn exited(&mut self, status: &std::process::ExitStatus) {
        #[cfg(unix)]
        let signal = std::os::unix::process::ExitStatusExt::signal(status);
        #[cfg(not(unix))]
        let signal = None;
        self.end(status.code(), signal);
    }

    /// The child was killed (and reaped) by the tool.
    pub fn killed(&mut self) {
        self.end(None, Some(KILLED_BY));
    }

    /// Report the end once; later calls do nothing.
    pub fn end(&mut self, exit_code: Option<i32>, signal: Option<i32>) {
        if let Some((sink, child)) = self.armed.take() {
            sink.ended(&child, exit_code, signal);
        }
    }
}

impl Drop for ChildWatch {
    fn drop(&mut self) {
        self.killed();
    }
}

/// The actor's ordering rule for child events on a session's stream: a
/// foreground event goes out after its call's `tool_use` (`ToolUseStart` or
/// `ToolUse`), and before the call's output, which reaches the actor later
/// than the event on the same tool's program order. Worker events (with a
/// handle id) and events outside a turn go out at once.
#[derive(Debug, Default)]
pub struct EventOrder {
    /// Foreground tool ids whose `tool_use` went out this turn.
    announced: HashSet<String>,
    /// Foreground events waiting for their `tool_use`, in arrival order.
    held: Vec<ChildProcessEvent>,
}

impl EventOrder {
    /// An event from the route: `Some` = send it now, `None` = held.
    pub fn offer(&mut self, event: ChildProcessEvent, streaming: bool) -> Option<ChildProcessEvent> {
        let tool_id = event.tool_id();
        let waits = streaming
            && event.handle_id().is_none()
            && !self.announced.contains(tool_id)
            && (matches!(event, ChildProcessEvent::Spawned(_))
                || self.held.iter().any(|held| held.tool_id() == tool_id));
        if waits {
            self.held.push(event);
            None
        } else {
            Some(event)
        }
    }

    /// A call's `tool_use` went out: its held events, in arrival order.
    pub fn announced(&mut self, tool_id: &str) -> Vec<ChildProcessEvent> {
        self.announced.insert(tool_id.to_string());
        if !self.held.iter().any(|held| held.tool_id() == tool_id) {
            return Vec::new();
        }
        let (ready, rest) =
            std::mem::take(&mut self.held).into_iter().partition(|held| held.tool_id() == tool_id);
        self.held = rest;
        ready
    }

    /// The turn is over: everything still held, in arrival order.
    pub fn turn_ended(&mut self) -> Vec<ChildProcessEvent> {
        self.announced.clear();
        std::mem::take(&mut self.held)
    }

    pub fn held_len(&self) -> usize {
        self.held.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ident(pid: u32) -> ChildIdentity {
        ChildIdentity { pid, start_ticks: 7, boot_id: "b".into(), sid: pid, pgid: pid }
    }

    fn spawned(tool_id: &str, handle_id: Option<&str>) -> ChildProcessEvent {
        ChildProcessEvent::Spawned(ChildSpawn {
            tool_id: tool_id.into(),
            tool_name: "bash".into(),
            handle_id: handle_id.map(str::to_string),
            shell_session_id: None,
            child: ident(10),
        })
    }

    fn ended(tool_id: &str, handle_id: Option<&str>) -> ChildProcessEvent {
        ChildProcessEvent::Ended {
            tool_id: tool_id.into(),
            handle_id: handle_id.map(str::to_string),
            child: ident(10),
            exit_code: Some(0),
            signal: None,
        }
    }

    #[test]
    fn a_foreground_event_waits_for_its_tool_use() {
        let mut order = EventOrder::default();
        assert_eq!(order.offer(spawned("t1", None), true), None, "before tool_use: held");
        assert_eq!(order.offer(ended("t1", None), true), None, "its end stays behind it");
        assert_eq!(order.offer(spawned("t2", None), true), None);
        assert_eq!(order.announced("t1"), vec![spawned("t1", None), ended("t1", None)]);
        assert_eq!(order.held_len(), 1, "t2 still waits");
        assert_eq!(order.offer(spawned("t1", None), true), Some(spawned("t1", None)), "announced: at once");
        assert_eq!(order.announced("t3"), vec![]);
        assert_eq!(order.turn_ended(), vec![spawned("t2", None)], "turn end flushes the rest");
        assert_eq!(order.held_len(), 0);
        assert_eq!(order.offer(spawned("t1", None), true), None, "a new turn starts unannounced");
    }

    #[test]
    fn worker_events_and_events_outside_a_turn_go_at_once() {
        let mut order = EventOrder::default();
        assert_eq!(order.offer(spawned("w1", Some("sa_3")), true), Some(spawned("w1", Some("sa_3"))));
        assert_eq!(order.offer(spawned("t1", None), false), Some(spawned("t1", None)));
        // An end whose spawn already went out (a late reap) never waits.
        assert_eq!(order.offer(ended("t9", None), true), Some(ended("t9", None)));
        assert_eq!(order.held_len(), 0);
    }

    #[test]
    fn a_worker_route_keeps_the_session_and_channel_with_its_own_handle() {
        let (route, mut rx) = ChildRoute::channel("sess-1");
        assert_eq!(route.handle_id(), None);
        let worker = route.for_worker("sa_7");
        assert_eq!((worker.session_id(), worker.handle_id()), ("sess-1", Some("sa_7")));
        let nested = worker.for_worker("sa_9");
        assert_eq!(nested.handle_id(), Some("sa_9"));
        let channel = crate::tools::output::delta_channel();
        let mut sender = channel.sender;
        assert!(worker_route(Some(&sender), "sa_1").is_none(), "an untagged call has no route");
        sender.set_child_sink(route.sink("toolu_1", "subagent_start"));
        let inherited = worker_route(Some(&sender), "sa_1").expect("inherited");
        assert_eq!((inherited.session_id(), inherited.handle_id()), ("sess-1", Some("sa_1")));
        assert!(worker_route(None, "sa_1").is_none());
        // Same channel: a worker's report reaches the session's receiver.
        nested.sink("toolu_w", "bash").ended(&ident(0), Some(0), None);
        let ChildProcessEvent::Ended { tool_id, handle_id, .. } = rx.try_recv().expect("delivered") else {
            panic!("ended");
        };
        assert_eq!((tool_id.as_str(), handle_id.as_deref()), ("toolu_w", Some("sa_9")));
    }

    /// The ledger insert happens before the event send: while the ledger is
    /// locked, a spawn report sends nothing; once it is released, the event
    /// arrives and the record is already listed.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_ledger_is_written_before_the_event_is_sent() {
        if !children::supported() {
            return;
        }
        let session = format!("hb-{}", std::process::id());
        for i in 0..50 {
            let (route, mut rx) = ChildRoute::channel(&session);
            let sink = route.sink(&format!("toolu_hb{i}"), "bash");
            let guard = children::ledger().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let reporter = std::thread::spawn(move || sink.spawned(std::process::id(), None));
            std::thread::sleep(std::time::Duration::from_millis(if i == 0 { 100 } else { 5 }));
            assert!(rx.try_recv().is_err(), "spawn {i}: event sent before the ledger insert");
            drop(guard);
            assert!(reporter.join().unwrap().is_some());
            let ChildProcessEvent::Spawned(spawn) = rx.try_recv().expect("sent after the insert") else {
                panic!("spawned");
            };
            let listing = children::list(Some(&session), None);
            assert!(
                listing.sessions.iter().flat_map(|s| &s.children).any(|c| c.tool_id == spawn.tool_id),
                "spawn {i}: the record is listed when the event arrives"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_watch_reports_its_end_once_and_a_dropped_one_reports_a_kill() {
        if !children::supported() {
            return;
        }
        let (route, mut rx) = ChildRoute::channel(&format!("watch-{}", std::process::id()));
        let sink = route.sink("toolu_w1", "bash");
        let mut child = std::process::Command::new("sh").args(["-c", "exit 3"]).spawn().unwrap();
        let mut watch = ChildWatch::start(Some(&sink), Some(child.id()), None);
        let identity = watch.identity().cloned().expect("identity read while unreaped");
        assert_eq!(identity.pid, child.id());
        assert!(matches!(rx.try_recv(), Ok(ChildProcessEvent::Spawned(_))));
        let status = child.wait().unwrap();
        watch.exited(&status);
        watch.killed();
        drop(watch);
        let Ok(ChildProcessEvent::Ended { exit_code, signal, child: ended_child, .. }) = rx.try_recv() else {
            panic!("ended");
        };
        assert_eq!((exit_code, signal, ended_child), (Some(3), None, identity));
        assert!(rx.try_recv().is_err(), "reported once");

        let mut sleeper = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        let watch = ChildWatch::start(Some(&sink), Some(sleeper.id()), None);
        assert!(matches!(rx.try_recv(), Ok(ChildProcessEvent::Spawned(_))));
        drop(watch);
        let _ = sleeper.kill();
        let _ = sleeper.wait();
        assert!(matches!(
            rx.try_recv(),
            Ok(ChildProcessEvent::Ended { exit_code: None, signal: Some(KILLED_BY), .. })
        ));
        assert!(ChildWatch::start(None, Some(1), None).identity().is_none(), "no sink: nothing");
    }
}
