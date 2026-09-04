//! `SocketTransport` — the client side of the daemon UDS (PLAN-phase2 §2.10).
//!
//! Line-JSON frames (`wire.rs`). A writer task owns the write half; the
//! reader lives on the transport. Backpressure mirrors `LocalTransport`:
//! `send` fails with `Backpressure` when the bounded writer queue is full and
//! `Closed` once the socket is gone — never a silent drop.
//!
//! Tracing here logs frame *types* only (`ClientFrame`'s redacting `Debug`).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use super::handle::CMD_CHAN_CAP;
use super::transport::{ClientTransport, TransportError, ATTACH_TIMEOUT, ATTACH_TIMEOUT_PARKED};
use super::types::*;
use super::view::RuntimeView;
use super::wire::*;

pub const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);

/// A connection past `Hello`/`Welcome`, before `Attach`.
pub struct Connected {
    reader: BufReader<OwnedReadHalf>,
    writer: mpsc::Sender<ClientFrame>,
    writer_task: tokio::task::JoinHandle<()>,
    pub welcome: Welcome,
    /// Remembered for `reconnect`.
    path: PathBuf,
    hello: Hello,
}

pub struct Pong {
    pub pid: u32,
    pub uptime_s: u64,
    pub sessions: usize,
}

async fn read_frame(reader: &mut BufReader<OwnedReadHalf>) -> Result<Option<DaemonFrame>, TransportError> {
    let mut line = String::new();
    loop {
        line.clear();
        // `take` bounds the read so an oversize frame cannot grow memory.
        let n = tokio::io::AsyncReadExt::take(&mut *reader, MAX_FRAME_BYTES as u64 + 1)
            .read_line(&mut line)
            .await?;
        if n == 0 {
            return Ok(None);
        }
        if n > MAX_FRAME_BYTES {
            return Err(TransportError::Protocol(frame_limit_msg()));
        }
        let t = line.trim_end();
        if t.is_empty() {
            continue;
        }
        return decode_line::<DaemonFrame>(t)
            .map(Some)
            .map_err(TransportError::Protocol);
    }
}

fn spawn_writer(mut w: OwnedWriteHalf) -> (mpsc::Sender<ClientFrame>, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = mpsc::channel::<ClientFrame>(CMD_CHAN_CAP);
    let task = tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            let line = match encode_line(&frame) {
                Ok(l) => l,
                Err(e) => {
                    tracing::warn!(frame = ?frame, "socket_transport: encode failed: {e}");
                    continue;
                }
            };
            if let Err(e) = w.write_all(line.as_bytes()).await {
                tracing::debug!("socket_transport: write failed: {e}");
                break;
            }
            if matches!(frame, ClientFrame::Bye) {
                let _ = w.shutdown().await;
                break;
            }
        }
    });
    (tx, task)
}

impl Connected {
    /// Hello/Welcome only. `Refused{Version}` → `TransportError::Version`;
    /// any other refusal → `Protocol`.
    pub async fn connect(path: &Path, hello: Hello) -> Result<Self, TransportError> {
        let stream = tokio::time::timeout(CONNECT_TIMEOUT, UnixStream::connect(path))
            .await
            .map_err(|_| TransportError::Protocol("connect timed out".into()))??;
        let (r, w) = stream.into_split();
        let mut reader = BufReader::new(r);
        let (writer, writer_task) = spawn_writer(w);
        let client_version = hello.protocol_version;
        let hello_copy = hello.clone();
        writer
            .send(ClientFrame::Hello(hello))
            .await
            .map_err(|_| TransportError::Closed)?;
        let frame = tokio::time::timeout(HANDSHAKE_TIMEOUT, read_frame(&mut reader))
            .await
            .map_err(|_| TransportError::Protocol("handshake timed out".into()))??;
        match frame {
            Some(DaemonFrame::Welcome(welcome)) => {
                if welcome.protocol_version != client_version {
                    return Err(TransportError::Version { client: client_version, daemon: welcome.protocol_version });
                }
                Ok(Self { reader, writer, writer_task, welcome, path: path.to_path_buf(), hello: hello_copy })
            }
            Some(DaemonFrame::Refused { reason: RefuseReason::Version { daemon_version, .. }, .. }) => {
                Err(TransportError::Version { client: client_version, daemon: daemon_version })
            }
            Some(DaemonFrame::Refused { reason, message }) => {
                Err(TransportError::Protocol(format!("refused ({reason:?}): {message}")))
            }
            Some(other) => Err(TransportError::Protocol(format!("expected welcome, got {other:?}"))),
            None => Err(TransportError::Closed),
        }
    }

    async fn request(&mut self, frame: ClientFrame) -> Result<DaemonFrame, TransportError> {
        self.writer.send(frame).await.map_err(|_| TransportError::Closed)?;
        tokio::time::timeout(HANDSHAKE_TIMEOUT, read_frame(&mut self.reader))
            .await
            .map_err(|_| TransportError::Protocol("reply timed out".into()))??
            .ok_or(TransportError::Closed)
    }

    pub async fn ping(&mut self) -> Result<Pong, TransportError> {
        match self.request(ClientFrame::Ping).await? {
            DaemonFrame::Pong { pid, uptime_s, sessions } => Ok(Pong { pid, uptime_s, sessions }),
            other => Err(TransportError::Protocol(format!("expected pong, got {other:?}"))),
        }
    }

    pub async fn sessions(&mut self) -> Result<Vec<SessionMeta>, TransportError> {
        match self.request(ClientFrame::Sessions).await? {
            DaemonFrame::SessionList { sessions } => Ok(sessions),
            other => Err(TransportError::Protocol(format!("expected session list, got {other:?}"))),
        }
    }

    /// `Purge` → the daemon runs `memstat::purge_arenas()`; replies `Pong`.
    pub async fn purge(&mut self) -> Result<Pong, TransportError> {
        match self.request(ClientFrame::Purge).await? {
            DaemonFrame::Pong { pid, uptime_s, sessions } => Ok(Pong { pid, uptime_s, sessions }),
            DaemonFrame::Error { message, .. } => Err(TransportError::Protocol(message)),
            other => Err(TransportError::Protocol(format!("expected pong, got {other:?}"))),
        }
    }

    pub async fn shutdown(&mut self, force: bool) -> Result<(), TransportError> {
        match self.request(ClientFrame::Shutdown { force }).await? {
            DaemonFrame::Bye { .. } => Ok(()),
            DaemonFrame::Error { message, .. } => Err(TransportError::Protocol(message)),
            other => Err(TransportError::Protocol(format!("expected bye, got {other:?}"))),
        }
    }

    /// Say goodbye and drop the connection.
    pub async fn bye(self) {
        let _ = self.writer.send(ClientFrame::Bye).await;
        let _ = tokio::time::timeout(Duration::from_millis(200), self.writer_task).await;
    }
}

pub struct SocketTransport {
    reader: BufReader<OwnedReadHalf>,
    writer: mpsc::Sender<ClientFrame>,
    _writer_task: tokio::task::JoinHandle<()>,
    session: SessionId,
    meta: SessionMeta,
    client: ClientId,
    view: Arc<arc_swap::ArcSwap<RuntimeView>>,
    pub welcome: Welcome,
    mode: AttachMode,
    input_owner: Option<ClientId>,
    ended: bool,
    path: PathBuf,
    hello: Hello,
    /// `Reloading` seen; the next EOF is a reload, not a death.
    reload_pending: Option<u64>,
    /// Why `next_event` returned `None` (`Reloading{generation}` after a
    /// reload announcement) — the TUI branches to `reconnect` on it.
    last_error: Option<TransportError>,
    /// Local `api_messages` mirror: seeded by `Attached`, updated by
    /// `Stream(MessageHistory)` and `QueryResult{DIGEST_RESYNC_QUERY_ID}`.
    /// `Conversation` arrives as a digest; the mirror fills it in.
    messages: Vec<crate::SharedMessage>,
    /// Digest awaiting a `Messages` re-query (hash miss).
    pending_digest: Option<ConversationDigest>,
}

impl SocketTransport {
    /// Hello/Welcome only (50 ms connect + 2 s handshake budgets).
    pub async fn connect(path: &Path, hello: Hello) -> Result<Connected, TransportError> {
        Connected::connect(path, hello).await
    }

    /// Attach (existing or create) on a handshaken connection. `AttachRefused`
    /// and `DaemonFrame::Error` alike → `TransportError::Refused`. Waits
    /// `ATTACH_TIMEOUT_PARKED` when the daemon lists the session as `Parked`.
    pub async fn attach(mut conn: Connected, attach: Attach) -> Result<(Self, AttachSnapshot), TransportError> {
        let (mode, parked) = match &attach {
            Attach::Existing { session_id, mode } => (
                *mode,
                conn.welcome
                    .sessions
                    .iter()
                    .any(|m| &m.id == session_id && m.lifecycle == SessionLifecycle::Parked),
            ),
            Attach::Create { mode, .. } => (*mode, false),
        };
        conn.writer
            .send(ClientFrame::Attach(attach))
            .await
            .map_err(|_| TransportError::Closed)?;
        let budget = if parked { ATTACH_TIMEOUT_PARKED } else { ATTACH_TIMEOUT };
        let deadline = tokio::time::Instant::now() + budget;
        let attached = loop {
            let frame = tokio::time::timeout_at(deadline, read_frame(&mut conn.reader))
                .await
                .map_err(|_| TransportError::Protocol("attach timed out".into()))??;
            match frame {
                Some(DaemonFrame::Attached(a)) => break a,
                Some(DaemonFrame::Error { message, .. }) => return Err(TransportError::Refused(message)),
                Some(DaemonFrame::Event(w)) => match w.event {
                    WireSessionEvent::AttachRefused { message } => return Err(TransportError::Refused(message)),
                    // Notices/lifecycle chatter may precede `Attached` (unpark).
                    _ => continue,
                },
                Some(DaemonFrame::Bye { .. }) | None => return Err(TransportError::Closed),
                Some(other) => return Err(TransportError::Protocol(format!("expected attached, got {other:?}"))),
            }
        };
        let (client, snapshot) = attached.into_snapshot();
        let meta = snapshot.meta.clone();
        let view = Arc::new(arc_swap::ArcSwap::from_pointee(snapshot.view.clone()));
        Ok((
            Self {
                reader: conn.reader,
                writer: conn.writer,
                _writer_task: conn.writer_task,
                session: meta.id.clone(),
                meta,
                client,
                view,
                welcome: conn.welcome,
                mode,
                input_owner: snapshot.input_owner,
                ended: false,
                path: conn.path,
                hello: conn.hello,
                reload_pending: None,
                last_error: None,
                messages: snapshot.conversation.api_messages.clone(),
                pending_digest: None,
            },
            snapshot,
        ))
    }

    /// Why the last `next_event` returned `None` (if it was not a plain end).
    pub fn last_error(&self) -> Option<&TransportError> {
        self.last_error.as_ref()
    }

    // ── control fast path helpers (fresh connection each) ──

    pub async fn ping(path: &Path) -> Result<Pong, TransportError> {
        let mut c = Connected::connect(path, Hello::new(ClientKind::Attach)).await?;
        let p = c.ping().await;
        c.bye().await;
        p
    }

    pub async fn sessions(path: &Path) -> Result<Vec<SessionMeta>, TransportError> {
        let mut c = Connected::connect(path, Hello::new(ClientKind::Attach)).await?;
        let s = c.sessions().await;
        c.bye().await;
        s
    }

    pub async fn shutdown(path: &Path, force: bool) -> Result<(), TransportError> {
        let mut c = Connected::connect(path, Hello::new(ClientKind::Attach)).await?;
        c.shutdown(force).await
    }

    pub async fn purge(path: &Path) -> Result<Pong, TransportError> {
        let mut c = Connected::connect(path, Hello::new(ClientKind::Attach)).await?;
        let p = c.purge().await;
        c.bye().await;
        p
    }

    /// `Reload{..}` on a control connection. `Ok(generation)` once the daemon
    /// answered `Bye{Reloading{generation}}` (it is about to exec);
    /// `Refused{ReloadRefused}` / `Error` → `Err`.
    pub async fn reload(path: &Path, now: bool, drain_secs: Option<u64>, exe: Option<PathBuf>) -> Result<u64, TransportError> {
        let mut c = Connected::connect(path, Hello::new(ClientKind::Attach)).await?;
        c.writer
            .send(ClientFrame::Reload { now, drain_secs, exe })
            .await
            .map_err(|_| TransportError::Closed)?;
        // Drain + checkpoint can take a while: wait up to drain + 60 s.
        let budget = Duration::from_secs(drain_secs.unwrap_or(30) + 60);
        let frame = tokio::time::timeout(budget, read_frame(&mut c.reader))
            .await
            .map_err(|_| TransportError::Protocol("reload reply timed out".into()))??;
        match frame {
            Some(DaemonFrame::Bye { reason: Some(ByeReason::Reloading { generation, .. }) }) => Ok(generation),
            Some(DaemonFrame::Bye { .. }) => Err(TransportError::Protocol("daemon said bye without a reload reason".into())),
            Some(DaemonFrame::Refused { reason: RefuseReason::ReloadRefused { why }, .. }) => Err(TransportError::Refused(why)),
            Some(DaemonFrame::Refused { message, .. }) | Some(DaemonFrame::Error { message, .. }) => Err(TransportError::Refused(message)),
            Some(other) => Err(TransportError::Protocol(format!("expected bye, got {other:?}"))),
            None => Err(TransportError::Closed),
        }
    }

    /// Detach cleanly: `Detach` + `Bye`. The turn keeps running in the daemon.
    pub async fn detach(&self) {
        let _ = self
            .writer
            .send(ClientFrame::Cmd { session_id: self.session.clone(), cmd: SessionCommand::Detach { client: self.client } })
            .await;
        let _ = self.writer.send(ClientFrame::Bye).await;
    }

    fn notice(&self, text: String) -> Envelope {
        Envelope { session_id: self.session.clone(), seq: u64::MAX, ts: chrono::Utc::now(), event: SessionEventWire::SystemNotice(text) }
    }

    /// The client's view of `api_messages` (kept current by the digest protocol).
    pub fn messages(&self) -> &[crate::SharedMessage] {
        &self.messages
    }

    /// Turn a wire envelope into the in-process one, keeping the message
    /// mirror honest. `None` = swallowed (reserved query id, or a digest that
    /// needs a re-query first — the `Conversation` is re-emitted once the
    /// `Messages` result lands).
    fn absorb(&mut self, w: WireEnvelope) -> Option<Envelope> {
        match w.event {
            WireSessionEvent::Conversation { digest } => {
                if digest.matches(&self.messages) {
                    let event = SessionEventWire::Conversation(digest.into_snapshot(self.messages.clone()));
                    return Some(Envelope { session_id: w.session_id, seq: w.seq, ts: w.ts, event });
                }
                if digest.messages_len == 0 {
                    self.messages.clear();
                    let event = SessionEventWire::Conversation(digest.into_snapshot(Vec::new()));
                    return Some(Envelope { session_id: w.session_id, seq: w.seq, ts: w.ts, event });
                }
                // Mirror drifted (compaction, abort repair, flushed events): re-fetch.
                let first = self.pending_digest.is_none();
                self.pending_digest = Some(digest);
                if first {
                    let _ = self.writer.try_send(ClientFrame::Cmd {
                        session_id: self.session.clone(),
                        cmd: SessionCommand::Query { id: DIGEST_RESYNC_QUERY_ID, query: SessionQuery::Messages },
                    });
                }
                None
            }
            WireSessionEvent::QueryResult { id, value } if id == DIGEST_RESYNC_QUERY_ID => {
                if let Ok(msgs) = serde_json::from_value::<Vec<serde_json::Value>>(value) {
                    self.messages = msgs.into_iter().map(Arc::new).collect();
                }
                let digest = self.pending_digest.take()?;
                if !digest.matches(&self.messages) {
                    tracing::debug!("socket_transport: messages re-query still disagrees with digest; using fetched history");
                }
                let event = SessionEventWire::Conversation(digest.into_snapshot(self.messages.clone()));
                Some(Envelope { session_id: w.session_id, seq: w.seq, ts: w.ts, event })
            }
            WireSessionEvent::QueryResult { id, .. } if id >= RESERVED_QUERY_ID_BASE => None,
            event => {
                let env = Envelope { session_id: w.session_id, seq: w.seq, ts: w.ts, event: event.into() };
                match &env.event {
                    SessionEventWire::Stream(crate::StreamEvent::Session(crate::SessionEvent::MessageHistory(m))) => {
                        self.messages = m.clone();
                    }
                    SessionEventWire::Attached { snapshot, .. } => {
                        self.messages = snapshot.conversation.api_messages.clone();
                    }
                    _ => {}
                }
                Some(env)
            }
        }
    }
}

#[async_trait::async_trait]
impl ClientTransport for SocketTransport {
    fn session_id(&self) -> &SessionId {
        &self.session
    }

    fn meta(&self) -> &SessionMeta {
        &self.meta
    }

    async fn send(&self, cmd: SessionCommand) -> Result<(), TransportError> {
        match self.writer.try_send(ClientFrame::Cmd { session_id: self.session.clone(), cmd }) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => Err(TransportError::Backpressure),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(TransportError::Closed),
        }
    }

    /// The daemon's conn stamps `from` with this connection's client id, so
    /// on the wire `send` and `send_from_self` are the same frame.
    async fn send_from_self(&self, cmd: SessionCommand) -> Result<(), TransportError> {
        self.send(cmd).await
    }

    async fn next_event(&mut self) -> Option<Envelope> {
        if self.ended {
            return None;
        }
        loop {
            match read_frame(&mut self.reader).await {
                Ok(Some(DaemonFrame::Event(w))) => {
                    let Some(env) = self.absorb(w) else { continue };
                    match &env.event {
                        SessionEventWire::SettingChanged(applied) => {
                            self.view.store(Arc::new(applied.view.clone()));
                        }
                        SessionEventWire::Ended { .. } => self.ended = true,
                        SessionEventWire::InputOwnerChanged { to, .. } => self.input_owner = *to,
                        SessionEventWire::Reloading { generation, .. } => {
                            self.reload_pending = Some(*generation);
                        }
                        _ => {}
                    }
                    return Some(env);
                }
                Ok(Some(DaemonFrame::Error { message, .. })) => return Some(self.notice(message)),
                Ok(Some(DaemonFrame::Bye { reason })) => {
                    if let Some(ByeReason::Reloading { generation, .. }) = reason {
                        self.reload_pending = Some(generation);
                    }
                    self.ended = true;
                    if let Some(generation) = self.reload_pending {
                        self.last_error = Some(TransportError::Reloading { generation });
                    }
                    return None;
                }
                Ok(None) => {
                    self.ended = true;
                    if let Some(generation) = self.reload_pending {
                        self.last_error = Some(TransportError::Reloading { generation });
                    }
                    return None;
                }
                Ok(Some(other)) => {
                    tracing::debug!("socket_transport: unexpected frame after attach: {other:?}");
                    continue;
                }
                Err(e) => {
                    tracing::debug!("socket_transport: read error: {e}");
                    self.ended = true;
                    return None;
                }
            }
        }
    }

    fn view(&self) -> Arc<RuntimeView> {
        Arc::clone(&self.view.load())
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

    /// C3: after `Reloading`/EOF — backoff 100 ms ×2 → cap 5 s, total ≤
    /// `SYNAPS_TUI_ATTACH_RECONNECT_SECS` (60); `Hello{reconnect_of}` →
    /// `Attach::Existing{alias-resolved id, mode}` where `mode = Takeover`
    /// iff this client was the input owner (§2.7: two reconnecting mirrors
    /// cannot both take over). On success `self` IS the new connection; the
    /// returned snapshot is a full re-mirror.
    async fn reconnect(&mut self, mode: AttachMode) -> Result<AttachSnapshot, TransportError> {
        let was_owner = self.input_owner == Some(self.client);
        let generation = self.reload_pending.unwrap_or(self.welcome.generation);
        let mut hello = self.hello.clone();
        hello.reconnect_of = Some(ClientReconnect {
            previous_client: self.client,
            session_id: self.session.clone(),
            was_owner,
            generation,
        });
        let mode = if was_owner { AttachMode::Takeover } else { mode };
        let total = std::env::var("SYNAPS_TUI_ATTACH_RECONNECT_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(60));
        let deadline = tokio::time::Instant::now() + total;
        let mut backoff = Duration::from_millis(100);
        let mut last;
        loop {
            match Connected::connect(&self.path, hello.clone()).await {
                Ok(conn) => {
                    match Self::attach(conn, Attach::Existing { session_id: self.session.clone(), mode }).await {
                        Ok((t, snap)) => {
                            *self = t;
                            return Ok(snap);
                        }
                        Err(TransportError::Refused(m)) => return Err(TransportError::Refused(m)),
                        Err(e) => last = e,
                    }
                }
                Err(TransportError::Version { client, daemon }) => {
                    return Err(TransportError::Version { client, daemon })
                }
                Err(e) => last = e,
            }
            if tokio::time::Instant::now() + backoff > deadline {
                return Err(last);
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(5));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LlmEvent, StreamEvent};
    use tokio::net::UnixListener;

    /// A hand-rolled daemon: Welcome, answer Ping, Attach → Attached, then
    /// echo every Cmd::Submit as an Event stream.
    async fn fake_daemon(listener: UnixListener, protocol_version: u32) {
        let (stream, _) = listener.accept().await.unwrap();
        let (r, mut w) = stream.into_split();
        let mut reader = BufReader::new(r);
        let mut line = String::new();
        let mut seq = 0u64;
        let sid = SessionId::from("fake-1");
        loop {
            line.clear();
            if reader.read_line(&mut line).await.unwrap() == 0 {
                break;
            }
            let frame: ClientFrame = decode_line(line.trim_end()).unwrap();
            let reply = match frame {
                ClientFrame::Hello(h) => {
                    if h.protocol_version != protocol_version {
                        let f = DaemonFrame::Refused {
                            reason: RefuseReason::Version { daemon_version: protocol_version, min: protocol_version, max: protocol_version },
                            message: "version".into(),
                        };
                        w.write_all(encode_line(&f).unwrap().as_bytes()).await.unwrap();
                        break;
                    }
                    DaemonFrame::Welcome(Welcome {
                        protocol_version,
                        daemon_version: "t".into(),
                        pid: 1,
                        profile: None,
                        sessions: vec![],
                        progressive_tool_disclosure: true,
                        generation: 1,
                    })
                }
                ClientFrame::Ping => DaemonFrame::Pong { pid: 1, uptime_s: 0, sessions: 0 },
                ClientFrame::Attach(_) => DaemonFrame::Attached(AttachedWire::new(
                    ClientId(7),
                    AttachSnapshot {
                        meta: crate::session::handle::echo::meta_for(&sid),
                        view: crate::session::handle::echo::view_for(),
                        conversation: ConversationSnapshot::default(),
                        streaming: false,
                        replay: vec![],
                        pending_prompts: vec![],
                        clients: vec![],
                        input_owner: Some(ClientId(7)),
                        display_tail: None,
                    },
                )),
                ClientFrame::Cmd { cmd: SessionCommand::Submit { text, .. }, .. } => {
                    seq += 1;
                    DaemonFrame::Event(WireEnvelope {
                        session_id: sid.clone(),
                        seq,
                        ts: chrono::Utc::now(),
                        event: SessionEventWire::Stream(StreamEvent::Llm(LlmEvent::Text(text))).into(),
                    })
                }
                ClientFrame::Cmd { cmd: SessionCommand::Set { id, .. }, .. } => {
                    seq += 1;
                    let mut view = crate::session::handle::echo::view_for();
                    view.model = "changed".into();
                    DaemonFrame::Event(WireEnvelope {
                        session_id: sid.clone(),
                        seq,
                        ts: chrono::Utc::now(),
                        event: SessionEventWire::SettingChanged(SettingApplied {
                            id,
                            setting: "model".into(),
                            ok: true,
                            message: None,
                            view,
                            clamp: None,
                        })
                        .into(),
                    })
                }
                // Checkpoint = "the daemon is about to reload": Reloading event, then Bye{Reloading}.
                ClientFrame::Cmd { cmd: SessionCommand::Checkpoint { .. }, .. } => {
                    seq += 1;
                    let ev = DaemonFrame::Event(WireEnvelope {
                        session_id: sid.clone(),
                        seq,
                        ts: chrono::Utc::now(),
                        event: WireSessionEvent::Reloading { generation: 2, retry_after_ms: 500 },
                    });
                    w.write_all(encode_line(&ev).unwrap().as_bytes()).await.unwrap();
                    let bye = DaemonFrame::Bye { reason: Some(ByeReason::Reloading { generation: 2, retry_after_ms: 500 }) };
                    w.write_all(encode_line(&bye).unwrap().as_bytes()).await.unwrap();
                    break;
                }
                // KeepWarm = ownership change notice.
                ClientFrame::Cmd { cmd: SessionCommand::KeepWarm { .. }, .. } => {
                    seq += 1;
                    DaemonFrame::Event(WireEnvelope {
                        session_id: sid.clone(),
                        seq,
                        ts: chrono::Utc::now(),
                        event: WireSessionEvent::InputOwnerChanged {
                            from: Some(ClientId(7)),
                            to: Some(ClientId(9)),
                            reason: OwnerChangeReason::Takeover,
                        },
                    })
                }
                // Steer = "the daemon's history drifted": digest for [user:text] without a MessageHistory.
                ClientFrame::Cmd { cmd: SessionCommand::Steer { text }, .. } => {
                    seq += 1;
                    let snap = ConversationSnapshot {
                        api_messages: vec![Arc::new(serde_json::json!({"role":"user","content":text}))],
                        cost: 1.5,
                        ..Default::default()
                    };
                    DaemonFrame::Event(WireEnvelope {
                        session_id: sid.clone(),
                        seq,
                        ts: chrono::Utc::now(),
                        event: SessionEventWire::Conversation(snap).into(),
                    })
                }
                ClientFrame::Cmd { cmd: SessionCommand::Query { id, query: SessionQuery::Messages }, .. } => {
                    seq += 1;
                    DaemonFrame::Event(WireEnvelope {
                        session_id: sid.clone(),
                        seq,
                        ts: chrono::Utc::now(),
                        event: WireSessionEvent::QueryResult {
                            id,
                            value: serde_json::json!([{"role":"user","content":"drifted"}]),
                        },
                    })
                }
                ClientFrame::Bye => {
                    let _ = w.write_all(encode_line(&DaemonFrame::Bye { reason: None }).unwrap().as_bytes()).await;
                    break;
                }
                ClientFrame::Cmd { cmd: SessionCommand::Detach { .. }, .. } => continue,
                _ => DaemonFrame::Error { session_id: None, message: "nope".into() },
            };
            w.write_all(encode_line(&reply).unwrap().as_bytes()).await.unwrap();
        }
    }

    fn sock() -> (tempfile::TempDir, std::path::PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("d.sock");
        (d, p)
    }

    #[tokio::test]
    async fn socket_transport_handshake_attach_pump() {
        let (_d, path) = sock();
        let l = UnixListener::bind(&path).unwrap();
        let srv = tokio::spawn(fake_daemon(l, PROTOCOL_VERSION));

        let mut conn = SocketTransport::connect(&path, Hello::new(ClientKind::Test)).await.unwrap();
        assert_eq!(conn.welcome.protocol_version, PROTOCOL_VERSION);
        assert_eq!(conn.ping().await.unwrap().pid, 1);
        let (mut t, snap) = SocketTransport::attach(conn, Attach::Existing { session_id: "fake-1".into(), mode: AttachMode::Mirror })
            .await
            .unwrap();
        assert_eq!(t.client_id(), ClientId(7));
        assert_eq!(t.mode(), AttachMode::Mirror);
        assert_eq!(t.input_owner(), Some(ClientId(7)));
        assert_eq!(snap.meta.id.as_str(), "fake-1");
        assert_eq!(t.session_id().as_str(), "fake-1");
        assert_eq!(t.view().model, "echo");

        t.send(SessionCommand::Submit { text: "hello".into(), attachments: vec![] }).await.unwrap();
        let e = t.next_event().await.unwrap();
        match e.event {
            SessionEventWire::Stream(StreamEvent::Llm(LlmEvent::Text(s))) => assert_eq!(s, "hello"),
            other => panic!("{other:?}"),
        }
        t.send_from_self(SessionCommand::Set { id: 11, setting: SessionSetting::Model { model: "changed".into() } })
            .await
            .unwrap();
        let e = t.next_event().await.unwrap();
        assert!(matches!(e.event, SessionEventWire::SettingChanged(SettingApplied { id: 11, .. })));
        assert_eq!(t.view().model, "changed");
        t.send(SessionCommand::KeepWarm { on: true }).await.unwrap();
        let e = t.next_event().await.unwrap();
        assert!(matches!(e.event, SessionEventWire::InputOwnerChanged { .. }));
        assert_eq!(t.input_owner(), Some(ClientId(9)));

        // Error → SystemNotice; Bye → None.
        t.send(SessionCommand::Save).await.unwrap();
        assert!(matches!(t.next_event().await.unwrap().event, SessionEventWire::SystemNotice(_)));
        t.detach().await;
        assert!(t.next_event().await.is_none());
        assert!(t.next_event().await.is_none());
        srv.await.unwrap();
    }

    #[tokio::test]
    async fn conversation_digest_miss_requeries_messages() {
        let (_d, path) = sock();
        let l = UnixListener::bind(&path).unwrap();
        let srv = tokio::spawn(fake_daemon(l, PROTOCOL_VERSION));
        let conn = SocketTransport::connect(&path, Hello::new(ClientKind::Test)).await.unwrap();
        let (mut t, _) = SocketTransport::attach(conn, Attach::Existing { session_id: "fake-1".into(), mode: AttachMode::Mirror })
            .await
            .unwrap();
        assert!(t.messages().is_empty());
        t.send(SessionCommand::Steer { text: "drifted".into() }).await.unwrap();
        // The digest misses the (empty) mirror → transport re-queries → one
        // Conversation with the fetched history and the digest's cost.
        let e = t.next_event().await.unwrap();
        match e.event {
            SessionEventWire::Conversation(c) => {
                assert_eq!(c.api_messages.len(), 1);
                assert_eq!(c.api_messages[0]["content"], "drifted");
                assert_eq!(c.cost, 1.5);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(t.messages().len(), 1);
        // Same digest again now matches the mirror: no re-query, served locally.
        t.send(SessionCommand::Steer { text: "drifted".into() }).await.unwrap();
        let e = t.next_event().await.unwrap();
        assert!(matches!(e.event, SessionEventWire::Conversation(ref c) if c.api_messages.len() == 1));
        t.detach().await;
        assert!(t.next_event().await.is_none());
        srv.await.unwrap();
    }

    #[tokio::test]
    async fn reloading_then_bye_sets_last_error_and_reconnect_gives_up_on_a_dead_socket() {
        let (_d, path) = sock();
        let l = UnixListener::bind(&path).unwrap();
        let srv = tokio::spawn(fake_daemon(l, PROTOCOL_VERSION));
        let conn = SocketTransport::connect(&path, Hello::new(ClientKind::Test)).await.unwrap();
        let (mut t, _) = SocketTransport::attach(conn, Attach::Existing { session_id: "fake-1".into(), mode: AttachMode::Observe })
            .await
            .unwrap();
        assert_eq!(t.mode(), AttachMode::Observe);
        assert!(t.last_error().is_none());
        t.send(SessionCommand::Checkpoint { reason: CheckpointReason::Reload }).await.unwrap();
        let e = t.next_event().await.unwrap();
        assert!(matches!(e.event, SessionEventWire::Reloading { generation: 2, .. }));
        assert!(t.next_event().await.is_none());
        assert!(matches!(t.last_error(), Some(TransportError::Reloading { generation: 2 })));
        srv.await.unwrap();
        // Nobody listens any more: the backoff loop gives up at the budget.
        std::env::set_var("SYNAPS_TUI_ATTACH_RECONNECT_SECS", "1");
        let r = t.reconnect(AttachMode::Observe).await;
        std::env::remove_var("SYNAPS_TUI_ATTACH_RECONNECT_SECS");
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn attach_error_frame_is_refused() {
        let (_d, path) = sock();
        let l = UnixListener::bind(&path).unwrap();
        // A daemon that welcomes, then answers the Attach with an Error frame.
        let srv = tokio::spawn(async move {
            let (stream, _) = l.accept().await.unwrap();
            let (r, mut w) = stream.into_split();
            let mut reader = BufReader::new(r);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let welcome = DaemonFrame::Welcome(Welcome {
                protocol_version: PROTOCOL_VERSION,
                daemon_version: "t".into(),
                pid: 1,
                profile: None,
                sessions: vec![],
                progressive_tool_disclosure: true,
                generation: 1,
            });
            w.write_all(encode_line(&welcome).unwrap().as_bytes()).await.unwrap();
            line.clear();
            reader.read_line(&mut line).await.unwrap();
            let err = DaemonFrame::Error { session_id: None, message: "unknown session".into() };
            w.write_all(encode_line(&err).unwrap().as_bytes()).await.unwrap();
        });
        let conn = SocketTransport::connect(&path, Hello::new(ClientKind::Test)).await.unwrap();
        let err = SocketTransport::attach(conn, Attach::Existing { session_id: "nope".into(), mode: AttachMode::Mirror })
            .await
            .err()
            .unwrap();
        assert!(matches!(err, TransportError::Refused(ref m) if m == "unknown session"), "{err}");
        srv.await.unwrap();
    }

    #[tokio::test]
    async fn socket_transport_refuses_version_mismatch() {
        let (_d, path) = sock();
        let l = UnixListener::bind(&path).unwrap();
        let srv = tokio::spawn(fake_daemon(l, 99));
        let err = SocketTransport::connect(&path, Hello::new(ClientKind::Test)).await.err().unwrap();
        assert!(matches!(err, TransportError::Version { client: PROTOCOL_VERSION, daemon: 99 }), "{err}");
        srv.await.unwrap();
    }

    #[tokio::test]
    async fn socket_transport_connect_refused_when_nobody_listens() {
        let (_d, path) = sock();
        let err = SocketTransport::connect(&path, Hello::new(ClientKind::Test)).await.err().unwrap();
        assert!(matches!(err, TransportError::Io(_)));
    }
}
