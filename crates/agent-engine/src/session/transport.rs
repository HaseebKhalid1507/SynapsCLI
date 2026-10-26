//! `ClientTransport` + `LocalTransport` (PLAN-phase2 §2.7).
//!
//! `LocalTransport` is the in-process client: same `Runtime`, same
//! `StreamEvent` values moved through a channel, never serialised. The
//! socket flavour (`SocketTransport`, B2) implements the same trait.

use std::sync::Arc;

use tokio::sync::broadcast;

use super::handle::SessionHandle;
use super::types::{
    AttachMode, AttachSnapshot, ClientId, ClientInput, ClientMeta, Envelope, Roster, SessionCommand,
    SessionEventWire, SessionId, SessionMeta,
};
use super::view::RuntimeView;

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("session closed")]
    Closed,
    #[error("backpressure")]
    Backpressure,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("version: client {client} daemon {daemon}")]
    Version { client: u32, daemon: u32 },
    /// The operation does not exist on this transport (e.g. `reconnect` on
    /// `LocalTransport`).
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
    /// The daemon announced a reload and closed; `reconnect` applies.
    #[error("daemon reloading (generation {generation})")]
    Reloading { generation: u64 },
    /// The daemon/actor refused the attach or command (`AttachRefused`,
    /// `DaemonFrame::Error` during attach).
    #[error("refused: {0}")]
    Refused(String),
    /// The daemon has no live session under that id
    /// (`Refused{UnknownSession}` to a control request such as `Close`).
    #[error("unknown session: {0}")]
    UnknownSession(String),
    /// The daemon predates this control request: it answered the frame
    /// with `Error{"malformed frame: unknown variant …"}`. A
    /// `synaps daemon reload` brings it up to this binary.
    #[error("daemon {daemon_version} predates `{request}`; run `synaps daemon reload`")]
    DaemonTooOld { daemon_version: String, request: &'static str },
}

#[async_trait::async_trait]
pub trait ClientTransport: Send {
    fn session_id(&self) -> &SessionId;
    fn meta(&self) -> &SessionMeta;
    /// Host-originated send (`from: None`; bypasses input ownership).
    async fn send(&self, cmd: SessionCommand) -> Result<(), TransportError>;
    /// Send stamped with this client's id (the TUI uses this; ownership is
    /// enforced by the actor, B1).
    async fn send_from_self(&self, cmd: SessionCommand) -> Result<(), TransportError>;
    /// Next envelope, or `None` when the session ended / socket closed.
    async fn next_event(&mut self) -> Option<Envelope>;
    /// Sync, cheap, never stale by more than one `SettingChanged`: what the
    /// TUI's getters read on day 2.
    fn view(&self) -> Arc<RuntimeView>;
    /// Client id assigned at Attach (needed for Detach/Resync).
    fn client_id(&self) -> ClientId;
    /// The mode this transport attached with.
    fn mode(&self) -> AttachMode;
    /// Current input owner — updated from `Attached` / `InputOwnerChanged`.
    fn input_owner(&self) -> Option<ClientId>;
    /// Attached clients, named — updated from `Attached` / `ClientJoined` /
    /// `ClientLeft`. Empty for a transport that doesn't track them.
    fn roster(&self) -> Roster {
        Roster::default()
    }
    /// Who may drive (from the attach snapshot). `Exclusive` for a
    /// transport that doesn't know.
    fn client_input(&self) -> ClientInput {
        ClientInput::wire_default()
    }
    /// Socket only (Local returns `Err(Unsupported)`): after `Reloading`/EOF,
    /// reconnect with backoff and re-attach with `mode` (C3/A4).
    async fn reconnect(&mut self, mode: AttachMode) -> Result<AttachSnapshot, TransportError>;
    /// F9: single reconnect attempt (no retry loop). Socket only.
    async fn reconnect_once(&mut self, mode: AttachMode) -> Result<AttachSnapshot, TransportError> {
        self.reconnect(mode).await
    }
    /// `true` if the daemon announced a reload before EOF (graceful). Always
    /// `false` on `LocalTransport`.
    fn is_reload_pending(&self) -> bool {
        false
    }
    /// The daemon pid from `Welcome`. Returns `0` on `LocalTransport`.
    fn daemon_pid(&self) -> u32 {
        0
    }
    /// The daemon's binary from the latest `Welcome` (it changes across
    /// `daemon reload`). `None` on `LocalTransport`: same binary.
    fn daemon_binary(&self) -> Option<BinaryId> {
        None
    }
    /// The daemon control socket this transport talks to. `None` on
    /// `LocalTransport` (no daemon). Lets a client open its own control
    /// connections (session list, pin, close) to the same daemon.
    fn socket_path(&self) -> Option<&std::path::Path> {
        None
    }
}

/// Which binary a process runs: its version and build
/// ([`binary_version`](super::wire::binary_version),
/// [`build_id`](super::wire::build_id)); the build is empty when unknown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryId {
    pub version: String,
    pub build: String,
}

impl BinaryId {
    /// This process's own binary.
    pub fn this() -> Self {
        Self { version: super::wire::binary_version(), build: super::wire::build_id().to_string() }
    }

    /// The daemon's, from its handshake.
    pub fn of(welcome: &super::wire::Welcome) -> Self {
        Self { version: welcome.daemon_version.clone(), build: welcome.daemon_build.clone() }
    }

    fn label(&self, show_build: bool) -> String {
        if show_build && !self.build.is_empty() {
            format!("{} ({})", self.version, self.build)
        } else {
            self.version.clone()
        }
    }
}

/// When a client binary differs from the daemon it talks to: what to tell
/// the user, else `None`. An attached TUI keeps its own binary across
/// `daemon reload` (only the daemon re-execs), so after an upgrade it lacks
/// every client-side change — new slash commands, tray fixes, new events —
/// until it is restarted. Builds count, not just versions: a reload onto a
/// newer build of the same release is the common case. An unknown build
/// (empty: older daemon, packaged build) compares by version only.
///
/// The remedy depends on which side is behind. A client older than the
/// daemon restarts to match it. A client newer than the daemon (a new
/// install with the old daemon still running, the usual upgrade path) must
/// not be told to restart, which relaunches the same binary: the daemon is
/// moved onto this build with `synaps daemon reload`. Same version on
/// another build: which one is newer is unknown, so both remedies.
pub fn version_skew_notice(client: &BinaryId, daemon: &BinaryId, session: &str) -> Option<String> {
    let builds_differ = !client.build.is_empty() && !daemon.build.is_empty() && client.build != daemon.build;
    if client.version == daemon.version && !builds_differ {
        return None;
    }
    let show_build = client.version == daemon.version;
    let (c, d) = (client.label(show_build), daemon.label(show_build));
    Some(match compare_versions(&client.version, &daemon.version) {
        Some(std::cmp::Ordering::Less) => format!(
            "this client is synaps {c} but the daemon is {d} — this client is older, so some \
             commands and fixes are unavailable here. Restart the client to match the daemon: \
             synaps --attach {session}"
        ),
        Some(std::cmp::Ordering::Greater) => format!(
            "this client is synaps {c} but the daemon is {d} — the daemon is older than this \
             client, so some commands and fixes are unavailable. To move it onto this build, \
             run `synaps daemon reload`"
        ),
        _ => format!(
            "this client is synaps {c} but the daemon is {d} — different builds, so some commands \
             and fixes may be unavailable. If this client is the newer one, run `synaps daemon \
             reload`; otherwise restart the client to match the daemon: synaps --attach {session}"
        ),
    })
}

/// Compare dotted numeric versions (`0.10.2`, a `-pre`/`+build` suffix
/// ignored); `None` when either doesn't parse.
fn compare_versions(a: &str, b: &str) -> Option<std::cmp::Ordering> {
    fn parts(v: &str) -> Option<Vec<u64>> {
        let core = v.trim().trim_start_matches('v').split(['-', '+']).next()?;
        core.split('.').map(|p| p.parse().ok()).collect()
    }
    Some(parts(a)?.cmp(&parts(b)?))
}

/// Attach handshake budget.
pub const ATTACH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// Attach budget when the listed lifecycle is `Parked` (unpark ≤ 20 s, B3).
pub const ATTACH_TIMEOUT_PARKED: std::time::Duration = std::time::Duration::from_secs(25);

pub struct LocalTransport {
    handle: SessionHandle,
    rx: broadcast::Receiver<Envelope>,
    client: ClientId,
    meta: SessionMeta,
    mode: AttachMode,
    input_owner: Option<ClientId>,
    roster: Roster,
    client_input: ClientInput,
    /// Set after `Ended` (or actor gone): `next_event` returns `None` from then on.
    ended: bool,
}

impl LocalTransport {
    /// Attach in-process as a `Mirror` (= `attach_with(.., Mirror)`).
    pub async fn attach(
        handle: SessionHandle,
        meta: ClientMeta,
    ) -> Result<(Self, AttachSnapshot), TransportError> {
        Self::attach_with(handle, meta, AttachMode::Mirror).await
    }

    /// Attach in-process: sends Attach, awaits the Attached envelope
    /// (bounded 5 s), returns the transport + snapshot.
    pub async fn attach_with(
        handle: SessionHandle,
        meta: ClientMeta,
        mode: AttachMode,
    ) -> Result<(Self, AttachSnapshot), TransportError> {
        // Subscribe BEFORE sending so the Attached reply cannot be missed.
        // `Attached` is broadcast; with one attach in flight per handle at a
        // time (every host today) the first one seen is ours.
        let mut rx = handle.subscribe();
        handle
            .send(SessionCommand::Attach {
                client: meta,
                mode,
            })
            .await?;

        let wait = async {
            loop {
                match rx.recv().await {
                    Ok(env) => match env.event {
                        SessionEventWire::Attached { client, snapshot } => {
                            return Ok((client, snapshot));
                        }
                        SessionEventWire::AttachRefused { message } => {
                            return Err(TransportError::Refused(message));
                        }
                        _ => {}
                    },
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => {
                        return Err(TransportError::Closed)
                    }
                }
            }
        };
        let timeout = if handle.lifecycle() == super::types::SessionLifecycle::Parked {
            ATTACH_TIMEOUT_PARKED
        } else {
            ATTACH_TIMEOUT
        };
        let (client, snapshot) = match tokio::time::timeout(timeout, wait).await {
            Ok(r) => r?,
            Err(_) => return Err(TransportError::Protocol("attach timed out".into())),
        };
        let meta = snapshot.meta.clone();
        Ok((
            Self {
                handle,
                rx,
                client,
                meta,
                mode,
                input_owner: snapshot.input_owner,
                roster: Roster::from_snapshot(&snapshot),
                client_input: snapshot.client_input,
                ended: false,
            },
            snapshot,
        ))
    }

    pub fn handle(&self) -> &SessionHandle {
        &self.handle
    }
}

#[async_trait::async_trait]
impl ClientTransport for LocalTransport {
    fn session_id(&self) -> &SessionId {
        &self.handle.id
    }

    fn meta(&self) -> &SessionMeta {
        &self.meta
    }

    async fn send(&self, cmd: SessionCommand) -> Result<(), TransportError> {
        self.handle.send(cmd).await
    }

    async fn send_from_self(&self, cmd: SessionCommand) -> Result<(), TransportError> {
        self.handle.send_from(self.client, cmd).await
    }

    async fn next_event(&mut self) -> Option<Envelope> {
        if self.ended {
            return None;
        }
        // The handle itself holds a broadcast sender, so `Closed` never
        // fires on its own: end-of-stream is `Ended` or the actor going away
        // (drain what it left behind first).
        let recv = tokio::select! {
            biased;
            r = self.rx.recv() => r,
            _ = self.handle.closed() => match self.rx.try_recv() {
                Ok(env) => Ok(env),
                Err(broadcast::error::TryRecvError::Lagged(n)) => {
                    Err(broadcast::error::RecvError::Lagged(n))
                }
                Err(_) => Err(broadcast::error::RecvError::Closed),
            },
        };
        match recv {
            Ok(env) => {
                match &env.event {
                    SessionEventWire::Ended { .. } => self.ended = true,
                    SessionEventWire::InputOwnerChanged { to, .. } => self.input_owner = *to,
                    other => self.roster.apply(other),
                }
                Some(env)
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                // server.rs Lagged pattern: warn, tell the client once, keep going.
                tracing::warn!(session = %self.handle.id, dropped = n, "session event stream lagged");
                Some(Envelope {
                    session_id: self.handle.id.clone(),
                    seq: u64::MAX,
                    ts: chrono::Utc::now(),
                    event: SessionEventWire::SystemNotice(format!(
                        "event stream lagged; {n} dropped"
                    )),
                })
            }
            Err(broadcast::error::RecvError::Closed) => {
                self.ended = true;
                None
            }
        }
    }

    fn view(&self) -> Arc<RuntimeView> {
        Arc::clone(&self.handle.view())
    }

    fn client_id(&self) -> ClientId {
        self.client
    }

    fn mode(&self) -> AttachMode {
        self.mode
    }

    fn input_owner(&self) -> Option<ClientId> {
        self.input_owner
    }

    fn roster(&self) -> Roster {
        self.roster.clone()
    }

    fn client_input(&self) -> ClientInput {
        self.client_input
    }

    async fn reconnect(&mut self, _mode: AttachMode) -> Result<AttachSnapshot, TransportError> {
        Err(TransportError::Unsupported("reconnect: in-process transport"))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn skew_notice_only_when_the_binaries_differ() {
        use super::{version_skew_notice, BinaryId};
        let bin = |version: &str, build: &str| BinaryId { version: version.into(), build: build.into() };
        assert_eq!(version_skew_notice(&bin("0.10.2", "cd1fbf50"), &bin("0.10.2", "cd1fbf50"), "s"), None);
        // An older release.
        let n = version_skew_notice(&bin("0.10.1", "be670c09"), &bin("0.10.2", "cd1fbf50"), "20261011-041552-79cc")
            .unwrap();
        assert!(n.starts_with("this client is synaps 0.10.1 but the daemon is 0.10.2 —"), "{n}");
        assert!(n.ends_with("synaps --attach 20261011-041552-79cc"), "{n}");
        // Same release, another build (a reload onto a newer merge batch).
        let n = version_skew_notice(&bin("0.10.2", "ae490b32"), &bin("0.10.2", "cd1fbf50"), "s").unwrap();
        assert!(n.starts_with("this client is synaps 0.10.2 (ae490b32) but the daemon is 0.10.2 (cd1fbf50)"), "{n}");
        // Unknown build on either side (an older daemon, a packaged build):
        // versions only.
        assert_eq!(version_skew_notice(&bin("0.10.2", "ae490b32"), &bin("0.10.2", ""), "s"), None);
        assert_eq!(version_skew_notice(&bin("0.10.2", ""), &bin("0.10.2", "cd1fbf50"), "s"), None);
    }

    /// 04-tui I3: a client NEWER than the daemon was told to restart itself,
    /// which relaunches the same new binary. It must say to reload the daemon.
    #[test]
    fn skew_notice_names_the_side_that_is_behind() {
        use super::{version_skew_notice, BinaryId};
        let bin = |version: &str, build: &str| BinaryId { version: version.into(), build: build.into() };
        // Client newer (the usual upgrade path): reload the daemon.
        let n = version_skew_notice(&bin("0.10.3", "be670c09"), &bin("0.10.2", "cd1fbf50"), "s").unwrap();
        assert!(n.starts_with("this client is synaps 0.10.3 but the daemon is 0.10.2 —"), "{n}");
        assert!(n.contains("run `synaps daemon reload`"), "{n}");
        assert!(!n.to_lowercase().contains("restart the client"), "{n}");
        assert!(!n.contains("synaps --attach"), "{n}");
        // Numeric, not lexical: 0.10.0 is newer than 0.9.9.
        let n = version_skew_notice(&bin("0.10.0", ""), &bin("0.9.9", ""), "s").unwrap();
        assert!(n.contains("run `synaps daemon reload`"), "{n}");
        // Client older: restart the client, as before.
        let n = version_skew_notice(&bin("0.9.9", ""), &bin("0.10.0", ""), "s").unwrap();
        assert!(n.contains("Restart the client"), "{n}");
        assert!(!n.contains("daemon reload"), "{n}");
        // Same version, another build: which is newer is unknown; both remedies.
        let n = version_skew_notice(&bin("0.10.2", "ae490b32"), &bin("0.10.2", "cd1fbf50"), "s").unwrap();
        assert!(n.contains("run `synaps daemon reload`"), "{n}");
        assert!(n.contains("restart the client"), "{n}");
        assert!(n.contains("synaps --attach s"), "{n}");
    }

    /// An older daemon's `Welcome` has no build: it parses, build unknown.
    #[test]
    fn welcome_without_a_build_parses() {
        let w: super::super::wire::Welcome = serde_json::from_value(serde_json::json!({
            "protocol_version": 3, "daemon_version": "0.10.1", "pid": 7, "profile": null,
            "sessions": [], "progressive_tool_disclosure": true, "generation": 2
        }))
        .unwrap();
        assert_eq!(super::BinaryId::of(&w), super::BinaryId { version: "0.10.1".into(), build: String::new() });
    }

    use super::*;
    use crate::session::types::*;
    use crate::{LlmEvent, SessionEvent, StreamEvent};

    async fn next(t: &mut LocalTransport) -> Envelope {
        tokio::time::timeout(std::time::Duration::from_secs(2), t.next_event())
            .await
            .expect("timely")
            .expect("open")
    }

    /// Debug strings of every `Stream(_)` up to and including the next `Conversation`.
    async fn collect(t: &mut LocalTransport) -> Vec<String> {
        let mut out = Vec::new();
        loop {
            let env = next(t).await;
            let done = matches!(env.event, SessionEventWire::Conversation(_));
            if matches!(env.event, SessionEventWire::Stream(_)) {
                out.push(format!("{:?}", env.event));
            }
            if done {
                break;
            }
        }
        out
    }

    #[tokio::test]
    async fn local_transport_round_trip_over_echo() {
        let (handle, task) = SessionHandle::echo_for_test(SessionId::from("lt-1"));
        let (mut t, snap) = LocalTransport::attach(handle, ClientMeta::new(ClientKind::Test))
            .await
            .unwrap();
        assert_eq!(t.session_id().as_str(), "lt-1");
        assert_eq!(t.client_id(), ClientId(1));
        assert_eq!(t.mode(), AttachMode::Mirror);
        assert!(t.input_owner().is_none());
        assert!(matches!(t.reconnect(AttachMode::Mirror).await, Err(TransportError::Unsupported(_))));
        assert_eq!(snap.meta.model, "echo");
        assert_eq!(t.view().model, "echo");
        assert!(snap.conversation.api_messages.is_empty());

        // ClientJoined follows Attached on the shared channel.
        let joined = next(&mut t).await;
        assert!(matches!(
            joined.event,
            SessionEventWire::ClientJoined {
                client: ClientId(1),
                kind: ClientKind::Test,
                ..
            }
        ));

        t.send(SessionCommand::Submit {
            text: "ping".into(),
            attachments: vec![],
        })
        .await
        .unwrap();

        let mut seen = Vec::new();
        loop {
            let env = next(&mut t).await;
            let done = matches!(env.event, SessionEventWire::Conversation(_));
            seen.push(env);
            if done {
                break;
            }
        }
        assert!(matches!(seen[0].event, SessionEventWire::TurnStarted { .. }));
        match &seen[1].event {
            SessionEventWire::Stream(StreamEvent::Llm(LlmEvent::Text(t))) => assert_eq!(t, "ping"),
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(
            seen[3].event,
            SessionEventWire::Stream(StreamEvent::Session(SessionEvent::Done))
        ));
        let seqs: Vec<u64> = seen.iter().map(|e| e.seq).collect();
        assert!(seqs.windows(2).all(|w| w[1] == w[0] + 1), "gapless: {seqs:?}");

        // Query{View} answered with the published view.
        t.send(SessionCommand::Query {
            id: 7,
            query: SessionQuery::View,
        })
        .await
        .unwrap();
        let q = next(&mut t).await;
        match q.event {
            SessionEventWire::QueryResult { id, value } => {
                assert_eq!(id, 7);
                assert_eq!(value["model"], "echo");
            }
            other => panic!("unexpected {other:?}"),
        }

        // Detach does not end the session; End does.
        t.send(SessionCommand::Detach { client: t.client_id() })
            .await
            .unwrap();
        let left = next(&mut t).await;
        assert!(matches!(left.event, SessionEventWire::ClientLeft { client: ClientId(1) }));
        assert!(t.handle().is_alive());

        t.send(SessionCommand::End {
            reason: EndReason::HostShutdown,
        })
        .await
        .unwrap();
        let ended = next(&mut t).await;
        assert!(matches!(ended.event, SessionEventWire::Ended { .. }));
        task.await.unwrap();
        assert!(t.next_event().await.is_none());
        assert!(matches!(
            t.send(SessionCommand::Save).await,
            Err(TransportError::Closed)
        ));
    }

    #[tokio::test]
    async fn two_local_clients_see_the_same_stream() {
        let (handle, task) = SessionHandle::echo_for_test(SessionId::from("lt-2"));
        let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
            .await
            .unwrap();
        let (mut b, snap_b) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Attach))
            .await
            .unwrap();
        assert_ne!(a.client_id(), b.client_id());
        assert_eq!(snap_b.clients.len(), 2);

        a.send(SessionCommand::Submit {
            text: "x".into(),
            attachments: vec![],
        })
        .await
        .unwrap();

        let sa = collect(&mut a).await;
        let sb = collect(&mut b).await;
        assert_eq!(sa, sb);
        assert_eq!(sa.len(), 3);

        drop((a, b, handle));
        task.await.unwrap();
    }
}
