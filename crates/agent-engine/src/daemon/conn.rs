//! Per-connection pump: handshake → control loop → attach → pump.
//!
//! Tracing here never logs frame bodies (`ClientFrame`'s `Debug` redacts
//! `Answer`/`Submit`/`Steer`); `Answer` values are forwarded to the actor
//! and nowhere else.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use super::DaemonState;
use crate::session::handle::CMD_CHAN_CAP;
use crate::session::transport::{TransportError, ATTACH_TIMEOUT};
use crate::session::wire::*;
use crate::session::*;

const HELLO_TIMEOUT: Duration = Duration::from_secs(2);

type Reader = BufReader<OwnedReadHalf>;

enum Read {
    Frame(ClientFrame),
    Eof,
    Oversize,
    Bad(String),
}

async fn read_frame(reader: &mut Reader, line: &mut String) -> std::io::Result<Read> {
    loop {
        line.clear();
        let n = tokio::io::AsyncReadExt::take(&mut *reader, MAX_FRAME_BYTES as u64 + 1)
            .read_line(line)
            .await?;
        if n == 0 {
            return Ok(Read::Eof);
        }
        if n > MAX_FRAME_BYTES {
            return Ok(Read::Oversize);
        }
        let t = line.trim_end();
        if t.is_empty() {
            continue;
        }
        return Ok(match decode_line::<ClientFrame>(t) {
            Ok(f) => Read::Frame(f),
            Err(e) => Read::Bad(e),
        });
    }
}

/// Writer task: owns the write half; bounded queue = per-client backpressure.
fn spawn_writer(mut w: OwnedWriteHalf) -> (mpsc::Sender<DaemonFrame>, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = mpsc::channel::<DaemonFrame>(CMD_CHAN_CAP);
    let task = tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            let bye = matches!(frame, DaemonFrame::Bye | DaemonFrame::Refused { .. });
            match encode_line(&frame) {
                Ok(line) => {
                    if w.write_all(line.as_bytes()).await.is_err() {
                        break;
                    }
                }
                Err(e) => tracing::warn!("daemon: encode failed: {e}"),
            }
            if bye {
                let _ = w.shutdown().await;
                break;
            }
        }
    });
    (tx, task)
}

pub async fn serve(state: Arc<DaemonState>, stream: UnixStream, shutdown: CancellationToken) {
    let (r, w) = stream.into_split();
    let mut reader = BufReader::new(r);
    let (tx, writer) = spawn_writer(w);
    let mut line = String::new();

    // ── handshake: first frame MUST be Hello ──
    let hello = match tokio::time::timeout(HELLO_TIMEOUT, read_frame(&mut reader, &mut line)).await {
        Ok(Ok(Read::Frame(ClientFrame::Hello(h)))) => h,
        Ok(Ok(Read::Frame(_))) | Ok(Ok(Read::Bad(_))) => {
            let _ = tx
                .send(DaemonFrame::Refused { reason: RefuseReason::Protocol, message: "first frame must be hello".into() })
                .await;
            let _ = writer.await;
            return;
        }
        Ok(Ok(Read::Oversize)) => {
            let _ = tx.send(DaemonFrame::Error { session_id: None, message: "frame exceeds 1 MiB limit".into() }).await;
            drop(tx);
            let _ = writer.await;
            return;
        }
        _ => {
            drop(tx);
            let _ = writer.await;
            return;
        }
    };
    if hello.protocol_version < PROTOCOL_MIN || hello.protocol_version > PROTOCOL_MAX {
        tracing::warn!(client = hello.protocol_version, daemon = PROTOCOL_VERSION, "daemon: refusing protocol version");
        let _ = tx
            .send(DaemonFrame::Refused {
                reason: RefuseReason::Version { daemon_version: PROTOCOL_VERSION, min: PROTOCOL_MIN, max: PROTOCOL_MAX },
                message: format!(
                    "protocol version {} not supported (daemon speaks {}; daemon binary {})",
                    hello.protocol_version,
                    PROTOCOL_VERSION,
                    binary_version()
                ),
            })
            .await;
        let _ = writer.await;
        return;
    }
    let daemon_version = binary_version();
    if hello.client_version != daemon_version {
        tracing::info!(client = %hello.client_version, daemon = %daemon_version, "daemon: client binary version differs (same protocol; allowed)");
    }
    let welcome = Welcome {
        protocol_version: PROTOCOL_VERSION,
        daemon_version,
        pid: std::process::id(),
        profile: state.profile.clone(),
        sessions: state.session_metas(),
        progressive_tool_disclosure: state.host.parts().progressive_tool_disclosure,
    };
    if tx.send(DaemonFrame::Welcome(welcome)).await.is_err() {
        return;
    }
    tracing::debug!(kind = ?hello.client.kind, "daemon: client connected");

    // ── control loop (no session allocated) ──
    let attach = loop {
        let frame = tokio::select! {
            _ = shutdown.cancelled() => { let _ = tx.send(DaemonFrame::Bye).await; let _ = writer.await; return; }
            r = read_frame(&mut reader, &mut line) => r,
        };
        match frame {
            Ok(Read::Frame(ClientFrame::Ping)) => {
                let _ = tx
                    .send(DaemonFrame::Pong { pid: std::process::id(), uptime_s: state.uptime_s(), sessions: state.live_sessions().len() })
                    .await;
            }
            Ok(Read::Frame(ClientFrame::Sessions)) => {
                let _ = tx.send(DaemonFrame::SessionList { sessions: state.session_metas() }).await;
            }
            Ok(Read::Frame(ClientFrame::Shutdown { force })) => {
                tracing::info!(force, "daemon: shutdown requested over the socket");
                let _ = tx.send(DaemonFrame::Bye).await;
                let _ = writer.await;
                state.request_shutdown(force);
                return;
            }
            Ok(Read::Frame(ClientFrame::Attach(a))) => break a,
            Ok(Read::Frame(ClientFrame::Cmd { session_id, .. })) => {
                let _ = tx.send(DaemonFrame::Error { session_id: Some(session_id), message: "not attached".into() }).await;
            }
            Ok(Read::Frame(ClientFrame::Hello(_))) => {
                let _ = tx.send(DaemonFrame::Error { session_id: None, message: "duplicate hello".into() }).await;
            }
            Ok(Read::Frame(ClientFrame::Bye)) | Ok(Read::Eof) | Err(_) => {
                let _ = tx.send(DaemonFrame::Bye).await;
                let _ = writer.await;
                return;
            }
            Ok(Read::Oversize) => {
                let _ = tx.send(DaemonFrame::Error { session_id: None, message: "frame exceeds 1 MiB limit".into() }).await;
                drop(tx);
                let _ = writer.await;
                return;
            }
            Ok(Read::Bad(e)) => {
                let _ = tx.send(DaemonFrame::Error { session_id: None, message: format!("malformed frame: {e}") }).await;
            }
        }
    };

    // ── attach ──
    let (handle, mode) = match attach {
        Attach::Existing { session_id, mode } => match state.attach(&session_id) {
            Some(h) => (h, mode),
            None => {
                let _ = tx
                    .send(DaemonFrame::Error { session_id: Some(session_id), message: "unknown session".into() })
                    .await;
                let _ = tx.send(DaemonFrame::Bye).await;
                let _ = writer.await;
                return;
            }
        },
        Attach::Create { mut config, mode } => {
            if config.cwd.is_none() {
                config.cwd = Some(hello.cwd.clone());
            }
            if let Some(cwd) = &config.cwd {
                if !cwd.is_absolute() || !cwd.is_dir() {
                    let _ = tx
                        .send(DaemonFrame::Error {
                            session_id: None,
                            message: format!("cwd must be an absolute existing directory: {}", cwd.display()),
                        })
                        .await;
                    let _ = tx.send(DaemonFrame::Bye).await;
                    let _ = writer.await;
                    return;
                }
            }
            match state.create(config).await {
                Ok(h) => (h, mode),
                Err(e) => {
                    let _ = tx.send(DaemonFrame::Error { session_id: None, message: format!("create session: {e}") }).await;
                    let _ = tx.send(DaemonFrame::Bye).await;
                    let _ = writer.await;
                    return;
                }
            }
        }
    };

    let mut rx = handle.subscribe();
    if let Err(e) = handle.send(SessionCommand::Attach { client: hello.client.clone(), mode }).await {
        let _ = tx.send(DaemonFrame::Error { session_id: Some(handle.id.clone()), message: format!("attach: {e}") }).await;
        let _ = tx.send(DaemonFrame::Bye).await;
        let _ = writer.await;
        return;
    }
    let attached = tokio::time::timeout(ATTACH_TIMEOUT, async {
        loop {
            match rx.recv().await {
                Ok(env) => {
                    if let SessionEventWire::Attached { client, snapshot } = env.event {
                        return Some((client, snapshot));
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    })
    .await;
    let (client, snapshot) = match attached {
        Ok(Some(x)) => x,
        _ => {
            let _ = tx.send(DaemonFrame::Error { session_id: Some(handle.id.clone()), message: "attach timed out".into() }).await;
            let _ = tx.send(DaemonFrame::Bye).await;
            let _ = writer.await;
            return;
        }
    };
    if tx.send(DaemonFrame::Attached(AttachedWire::new(client, snapshot))).await.is_err() {
        let _ = handle.send(SessionCommand::Detach { client }).await;
        return;
    }
    tracing::debug!(session = %handle.id, client = client.0, "daemon: client attached");

    // ── pump ──
    let sid = handle.id.clone();
    let fwd_tx = tx.clone();
    let fwd_handle = handle.clone();
    // Not tied to `shutdown`: on daemon stop the client must still see `Ended`.
    let mut forward = tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(env) => {
                    // Attached is addressed to one client; ours already went out as a frame.
                    if matches!(env.event, SessionEventWire::Attached { .. }) {
                        continue;
                    }
                    let ended = matches!(env.event, SessionEventWire::Ended { .. });
                    if fwd_tx.send(DaemonFrame::Event(env.into())).await.is_err() {
                        break;
                    }
                    if ended {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(session = %fwd_handle.id, dropped = n, "daemon: client lagged");
                    let env = Envelope {
                        session_id: fwd_handle.id.clone(),
                        seq: u64::MAX,
                        ts: chrono::Utc::now(),
                        event: SessionEventWire::SystemNotice(format!("event stream lagged; {n} dropped")),
                    };
                    if fwd_tx.send(DaemonFrame::Event(env.into())).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    let mut ended = false;
    let mut stopping = false;
    loop {
        let frame = tokio::select! {
            _ = shutdown.cancelled() => { stopping = true; break; }
            _ = handle.closed() => { ended = true; break; }
            r = read_frame(&mut reader, &mut line) => r,
        };
        match frame {
            Ok(Read::Frame(ClientFrame::Cmd { session_id, cmd })) => {
                if session_id != sid {
                    let _ = tx.send(DaemonFrame::Error { session_id: Some(session_id), message: "not attached to that session".into() }).await;
                    continue;
                }
                if matches!(cmd, SessionCommand::HostEvent(_)) {
                    continue;
                }
                match handle.send(cmd).await {
                    Ok(()) => {}
                    Err(TransportError::Backpressure) => {
                        let _ = tx.send(DaemonFrame::Error { session_id: Some(sid.clone()), message: "backpressure: command queue full".into() }).await;
                    }
                    Err(TransportError::Closed) => {
                        ended = true;
                        break;
                    }
                    Err(e) => {
                        let _ = tx.send(DaemonFrame::Error { session_id: Some(sid.clone()), message: e.to_string() }).await;
                    }
                }
            }
            Ok(Read::Frame(ClientFrame::Ping)) => {
                let _ = tx
                    .send(DaemonFrame::Pong { pid: std::process::id(), uptime_s: state.uptime_s(), sessions: state.live_sessions().len() })
                    .await;
            }
            Ok(Read::Frame(ClientFrame::Sessions)) => {
                let _ = tx.send(DaemonFrame::SessionList { sessions: state.session_metas() }).await;
            }
            Ok(Read::Frame(ClientFrame::Shutdown { force })) => {
                let _ = tx.send(DaemonFrame::Bye).await;
                state.request_shutdown(force);
                break;
            }
            Ok(Read::Frame(ClientFrame::Attach(_))) | Ok(Read::Frame(ClientFrame::Hello(_))) => {
                let _ = tx.send(DaemonFrame::Error { session_id: Some(sid.clone()), message: "already attached".into() }).await;
            }
            Ok(Read::Frame(ClientFrame::Bye)) | Ok(Read::Eof) | Err(_) => break,
            Ok(Read::Oversize) => {
                let _ = tx.send(DaemonFrame::Error { session_id: Some(sid.clone()), message: "frame exceeds 1 MiB limit".into() }).await;
                break;
            }
            Ok(Read::Bad(e)) => {
                let _ = tx.send(DaemonFrame::Error { session_id: Some(sid.clone()), message: format!("malformed frame: {e}") }).await;
            }
        }
    }

    // Socket gone / Bye = Detach. The turn keeps running (§8).
    if stopping {
        // Daemon stop: let the forwarder deliver Ended inside the lifecycle budget.
        match tokio::time::timeout(super::lifecycle::SESSION_END_BUDGET + Duration::from_secs(1), &mut forward).await {
            Ok(_) => {}
            Err(_) => {
                forward.abort();
                let _ = forward.await;
            }
        }
    } else {
        forward.abort();
        let _ = forward.await;
    }
    if !ended && handle.is_alive() {
        let _ = handle.send(SessionCommand::Detach { client }).await;
    }
    if !handle.is_alive() {
        state.remove(&sid);
    }
    let _ = tx.send(DaemonFrame::Bye).await;
    drop(tx);
    let _ = tokio::time::timeout(Duration::from_millis(500), writer).await;
    tracing::debug!(session = %sid, client = client.0, "daemon: client detached");
}
