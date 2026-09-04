//! `ClientTransport` + `LocalTransport` (PLAN-phase2 §2.7).
//!
//! `LocalTransport` is the in-process client: same `Runtime`, same
//! `StreamEvent` values moved through a channel, never serialised. The
//! socket flavour (`SocketTransport`, B2) implements the same trait.

use std::sync::Arc;

use tokio::sync::broadcast;

use super::handle::SessionHandle;
use super::types::{
    AttachMode, AttachSnapshot, ClientId, ClientMeta, Envelope, SessionCommand, SessionEventWire,
    SessionId, SessionMeta,
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
    /// Socket only (Local returns `Err(Unsupported)`): after `Reloading`/EOF,
    /// reconnect with backoff and re-attach with `mode` (C3/A4).
    async fn reconnect(&mut self, mode: AttachMode) -> Result<AttachSnapshot, TransportError>;
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
                    _ => {}
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

    async fn reconnect(&mut self, _mode: AttachMode) -> Result<AttachSnapshot, TransportError> {
        Err(TransportError::Unsupported("reconnect: in-process transport"))
    }
}

#[cfg(test)]
mod tests {
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
                kind: ClientKind::Test
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
