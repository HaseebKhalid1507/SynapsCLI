//! `SocketTransport` — the client side of the daemon UDS (PLAN-phase2 §2.10).
//!
//! Line-JSON frames (`wire.rs`). A writer task owns the write half; the
//! reader lives on the transport. Backpressure mirrors `LocalTransport`:
//! `send` fails with `Backpressure` when the bounded writer queue is full and
//! `Closed` once the socket is gone — never a silent drop.
//!
//! Tracing here logs frame *types* only (`ClientFrame`'s redacting `Debug`).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use super::handle::CMD_CHAN_CAP;
use super::transport::{ClientTransport, TransportError, ATTACH_TIMEOUT};
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
        let n = (&mut *reader)
            .take(MAX_FRAME_BYTES as u64 + 1)
            .read_line(&mut line)
            .await?;
        if n == 0 {
            return Ok(None);
        }
        if n > MAX_FRAME_BYTES {
            return Err(TransportError::Protocol("frame exceeds 1 MiB limit".into()));
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
                Ok(Self { reader, writer, writer_task, welcome })
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

    pub async fn shutdown(&mut self, force: bool) -> Result<(), TransportError> {
        match self.request(ClientFrame::Shutdown { force }).await? {
            DaemonFrame::Bye => Ok(()),
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
    ended: bool,
}

impl SocketTransport {
    /// Hello/Welcome only (50 ms connect + 2 s handshake budgets).
    pub async fn connect(path: &Path, hello: Hello) -> Result<Connected, TransportError> {
        Connected::connect(path, hello).await
    }

    /// Attach (existing or create) on a handshaken connection.
    pub async fn attach(mut conn: Connected, attach: Attach) -> Result<(Self, AttachSnapshot), TransportError> {
        conn.writer
            .send(ClientFrame::Attach(attach))
            .await
            .map_err(|_| TransportError::Closed)?;
        let frame = tokio::time::timeout(ATTACH_TIMEOUT, read_frame(&mut conn.reader))
            .await
            .map_err(|_| TransportError::Protocol("attach timed out".into()))??;
        let attached = match frame {
            Some(DaemonFrame::Attached(a)) => a,
            Some(DaemonFrame::Error { message, .. }) => return Err(TransportError::Protocol(message)),
            Some(other) => return Err(TransportError::Protocol(format!("expected attached, got {other:?}"))),
            None => return Err(TransportError::Closed),
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
                ended: false,
            },
            snapshot,
        ))
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

    async fn next_event(&mut self) -> Option<Envelope> {
        if self.ended {
            return None;
        }
        loop {
            match read_frame(&mut self.reader).await {
                Ok(Some(DaemonFrame::Event(w))) => {
                    let env: Envelope = w.into();
                    match &env.event {
                        SessionEventWire::SettingChanged(applied) => {
                            self.view.store(Arc::new(applied.view.clone()));
                        }
                        SessionEventWire::Ended { .. } => self.ended = true,
                        _ => {}
                    }
                    return Some(env);
                }
                Ok(Some(DaemonFrame::Error { message, .. })) => return Some(self.notice(message)),
                Ok(Some(DaemonFrame::Bye)) | Ok(None) => {
                    self.ended = true;
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
                ClientFrame::Cmd { cmd: SessionCommand::Set(_), .. } => {
                    seq += 1;
                    let mut view = crate::session::handle::echo::view_for();
                    view.model = "changed".into();
                    DaemonFrame::Event(WireEnvelope {
                        session_id: sid.clone(),
                        seq,
                        ts: chrono::Utc::now(),
                        event: SessionEventWire::SettingChanged(SettingApplied { setting: "model".into(), ok: true, message: None, view }).into(),
                    })
                }
                ClientFrame::Bye => {
                    let _ = w.write_all(encode_line(&DaemonFrame::Bye).unwrap().as_bytes()).await;
                    break;
                }
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
        assert_eq!(snap.meta.id.as_str(), "fake-1");
        assert_eq!(t.session_id().as_str(), "fake-1");
        assert_eq!(t.view().model, "echo");

        t.send(SessionCommand::Submit { text: "hello".into(), attachments: vec![] }).await.unwrap();
        let e = t.next_event().await.unwrap();
        match e.event {
            SessionEventWire::Stream(StreamEvent::Llm(LlmEvent::Text(s))) => assert_eq!(s, "hello"),
            other => panic!("{other:?}"),
        }
        t.send(SessionCommand::Set(SessionSetting::Model { model: "changed".into() })).await.unwrap();
        let e = t.next_event().await.unwrap();
        assert!(matches!(e.event, SessionEventWire::SettingChanged(_)));
        assert_eq!(t.view().model, "changed");

        // Error → SystemNotice; Bye → None.
        t.send(SessionCommand::Save).await.unwrap();
        assert!(matches!(t.next_event().await.unwrap().event, SessionEventWire::SystemNotice(_)));
        t.detach().await;
        assert!(t.next_event().await.is_none());
        assert!(t.next_event().await.is_none());
        srv.await.unwrap();
    }

    #[tokio::test]
    async fn socket_transport_refuses_version_mismatch() {
        let (_d, path) = sock();
        let l = UnixListener::bind(&path).unwrap();
        let srv = tokio::spawn(fake_daemon(l, 99));
        let err = SocketTransport::connect(&path, Hello::new(ClientKind::Test)).await.err().unwrap();
        assert!(matches!(err, TransportError::Version { client: 1, daemon: 99 }), "{err}");
        srv.await.unwrap();
    }

    #[tokio::test]
    async fn socket_transport_connect_refused_when_nobody_listens() {
        let (_d, path) = sock();
        let err = SocketTransport::connect(&path, Hello::new(ClientKind::Test)).await.err().unwrap();
        assert!(matches!(err, TransportError::Io(_)));
    }
}
