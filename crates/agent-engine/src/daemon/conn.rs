//! Per-connection pump: handshake → control loop → attach → pump.
//!
//! Tracing here never logs frame bodies (`ClientFrame`'s `Debug` redacts
//! `Answer`/`Submit`/`Steer`); `Answer` values are forwarded to the actor
//! and nowhere else.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use super::DaemonState;
use crate::session::handle::CMD_CHAN_CAP;
use crate::session::transport::{TransportError, ATTACH_TIMEOUT, ATTACH_TIMEOUT_PARKED};
use crate::session::wire::*;
use crate::session::*;

const HELLO_TIMEOUT: Duration = Duration::from_secs(2);

type Reader = BufReader<OwnedReadHalf>;

/// Which `SessionCommand`s a socket client may originate (§4 MED). The UDS
/// is 0600 same-uid, so this is a footgun guard, not an auth boundary:
/// anything that drives the actor's *own* bookkeeping (`Attach`, `Detach`
/// for another client, `Resync`, `HostEvent`) or ends the session for every
/// other client (`End`) is refused with an `Error` frame. Ending a session
/// is `daemon stop`'s job today.
fn client_may_send(cmd: &SessionCommand, own: ClientId) -> Result<(), &'static str> {
    use SessionCommand as C;
    match cmd {
        C::Submit { .. }
        | C::Steer { .. }
        | C::Cancel
        | C::Answer { .. }
        | C::Set { .. }
        | C::Compact { .. }
        | C::NewSession
        | C::Save
        | C::Query { .. }
        | C::EngineCommand { .. }
        | C::SubmitPrepared { .. }
        | C::PluginCommand { .. }
        | C::Resume { .. }
        | C::Checkpoint { .. }
        | C::KeepWarm { .. } => Ok(()),
        C::Detach { client } if *client == own => Ok(()),
        C::Detach { .. } => Err("detach: not your client id"),
        C::Attach { .. } => Err("attach: already attached (one session per connection)"),
        // C4: an owner may end its session (`/end`); the actor enforces
        // ownership (B1 `is_input_command`), the conn only screens reasons.
        C::End { reason: EndReason::ClientQuit } => Ok(()),
        C::End { .. } => Err("end: only ClientQuit may come from a client (host reasons are the daemon's)"),
        C::Resync { .. } => Err("resync: not a client command"),
        C::Park => Err("park: not a client command"),
        C::HostEvent(_) => Err("host_event: not a client command"),
    }
}

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
            let bye = matches!(frame, DaemonFrame::Bye { .. } | DaemonFrame::Refused { .. });
            match encode_line(&frame) {
                Ok(line) => {
                    if w.write_all(line.as_bytes()).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    // Symmetric cap: an outbound frame we cannot legally send
                    // becomes an Error frame and the connection closes.
                    tracing::warn!("daemon: encode failed: {e}");
                    let err = DaemonFrame::Error { session_id: None, message: format!("daemon could not encode a frame: {e}") };
                    if let Ok(line) = encode_line(&err) {
                        let _ = w.write_all(line.as_bytes()).await;
                    }
                    let _ = w.shutdown().await;
                    break;
                }
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
            let _ = tx.send(DaemonFrame::Error { session_id: None, message: frame_limit_msg() }).await;
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
        generation: state.generation,
    };
    if tx.send(DaemonFrame::Welcome(welcome)).await.is_err() {
        return;
    }
    tracing::debug!(kind = ?hello.client.kind, "daemon: client connected");

    // ── control loop (no session allocated) ──
    let attach = loop {
        let announce = state.announce();
        let frame = tokio::select! {
            _ = shutdown.cancelled() => { let _ = tx.send(DaemonFrame::Bye { reason: None }).await; let _ = writer.await; return; }
            _ = announce.cancelled() => {
                let _ = tx.send(DaemonFrame::Bye { reason: Some(reload_bye(&state)) }).await;
                let _ = writer.await;
                return;
            }
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
                let _ = tx.send(DaemonFrame::Bye { reason: None }).await;
                let _ = writer.await;
                state.request_shutdown(force);
                return;
            }
            Ok(Read::Frame(ClientFrame::Attach(a))) => {
                if matches!(a, Attach::Create { .. }) && state.reloading.load(Ordering::SeqCst) {
                    let _ = tx
                        .send(DaemonFrame::Refused { reason: RefuseReason::Busy, message: "daemon reloading; retry in a moment".into() })
                        .await;
                    continue;
                }
                break a;
            }
            Ok(Read::Frame(ClientFrame::Cmd { session_id, .. })) => {
                let _ = tx.send(DaemonFrame::Error { session_id: Some(session_id), message: "not attached".into() }).await;
            }
            // C3: the whole §2.8 sequence runs on this connection's task;
            // on success the process is replaced and this never returns.
            Ok(Read::Frame(ref f @ ClientFrame::Reload { .. })) => {
                let req = super::reload::ReloadRequest::from_frame(f).expect("reload frame");
                match super::reload::prepare(&state, &state.paths, req).await {
                    Err(super::reload::ReloadError::Refused(why)) => {
                        let _ = tx
                            .send(DaemonFrame::Refused { reason: RefuseReason::ReloadRefused { why: why.clone() }, message: why })
                            .await;
                    }
                    Err(super::reload::ReloadError::ExecFailed(e)) => {
                        let _ = tx.send(DaemonFrame::Error { session_id: None, message: format!("reload: {e}") }).await;
                    }
                    Ok(prepared) => {
                        // Tell the requester, flush, close — then exec.
                        let _ = tx.send(DaemonFrame::Bye { reason: Some(reload_bye(&state)) }).await;
                        drop(tx);
                        let _ = tokio::time::timeout(Duration::from_secs(1), writer).await;
                        let e = super::reload::exec(&state, &state.paths, prepared).await;
                        tracing::error!(error = %e, "daemon: reload did not exec");
                        return;
                    }
                }
            }
            // C2: jemalloc purge in the daemon (bench hygiene); reply Pong.
            Ok(Read::Frame(ClientFrame::Purge)) => {
                agent_core::core::memstat::purge_arenas();
                let _ = tx
                    .send(DaemonFrame::Pong { pid: std::process::id(), uptime_s: state.uptime_s(), sessions: state.live_sessions().len() })
                    .await;
            }
            Ok(Read::Frame(ClientFrame::Hello(_))) => {
                let _ = tx.send(DaemonFrame::Error { session_id: None, message: "duplicate hello".into() }).await;
            }
            Ok(Read::Frame(ClientFrame::Bye)) | Ok(Read::Eof) | Err(_) => {
                let _ = tx.send(DaemonFrame::Bye { reason: None }).await;
                let _ = writer.await;
                return;
            }
            Ok(Read::Oversize) => {
                let _ = tx.send(DaemonFrame::Error { session_id: None, message: frame_limit_msg() }).await;
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
                let _ = tx.send(DaemonFrame::Bye { reason: None }).await;
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
                    let _ = tx.send(DaemonFrame::Bye { reason: None }).await;
                    let _ = writer.await;
                    return;
                }
            }
            match state.create(config).await {
                Ok(h) => (h, mode),
                Err(e) => {
                    let _ = tx.send(DaemonFrame::Error { session_id: None, message: format!("create session: {e}") }).await;
                    let _ = tx.send(DaemonFrame::Bye { reason: None }).await;
                    let _ = writer.await;
                    return;
                }
            }
        }
    };

    let mut rx = handle.subscribe();
    if let Err(e) = handle.send(SessionCommand::Attach { client: hello.client.clone(), mode }).await {
        let _ = tx.send(DaemonFrame::Error { session_id: Some(handle.id.clone()), message: format!("attach: {e}") }).await;
        let _ = tx.send(DaemonFrame::Bye { reason: None }).await;
        let _ = writer.await;
        return;
    }
    let attach_budget = if handle.lifecycle() == SessionLifecycle::Parked { ATTACH_TIMEOUT_PARKED } else { ATTACH_TIMEOUT };
    let attached = tokio::time::timeout(attach_budget, async {
        loop {
            match rx.recv().await {
                Ok(env) => match env.event {
                    SessionEventWire::Attached { client, snapshot } => return Some(Ok((client, snapshot))),
                    SessionEventWire::AttachRefused { message } => return Some(Err(message)),
                    _ => {}
                },
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    })
    .await;
    let (client, snapshot) = match attached {
        Ok(Some(Ok(x))) => x,
        Ok(Some(Err(message))) => {
            let _ = tx.send(DaemonFrame::Error { session_id: Some(handle.id.clone()), message }).await;
            let _ = tx.send(DaemonFrame::Bye { reason: None }).await;
            let _ = writer.await;
            return;
        }
        _ => {
            let _ = tx.send(DaemonFrame::Error { session_id: Some(handle.id.clone()), message: "attach timed out".into() }).await;
            let _ = tx.send(DaemonFrame::Bye { reason: None }).await;
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
    let fwd_announce = state.announce();
    // Not tied to `shutdown`: on daemon stop the client must still see `Ended`.
    let mut forward = tokio::spawn(async move {
        loop {
            let next = tokio::select! {
                biased;
                r = rx.recv() => r,
                // C3: reload announced — the checkpoint's events (abort
                // notice, Conversation with abort_context) are already in
                // the broadcast; drain what is there, then stop.
                _ = fwd_announce.cancelled() => {
                    while let Ok(env) = rx.try_recv() {
                        if matches!(env.event, SessionEventWire::Attached { .. }) {
                            continue;
                        }
                        if fwd_tx.send(DaemonFrame::Event(env.into())).await.is_err() {
                            break;
                        }
                    }
                    break;
                }
            };
            match next {
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
    let mut reloading = false;
    let announce = state.announce();
    loop {
        let frame = tokio::select! {
            _ = shutdown.cancelled() => { stopping = true; break; }
            _ = announce.cancelled() => { reloading = true; break; }
            _ = handle.closed() => { ended = true; break; }
            r = read_frame(&mut reader, &mut line) => r,
        };
        match frame {
            Ok(Read::Frame(ClientFrame::Cmd { session_id, cmd })) => {
                if session_id != sid {
                    let _ = tx.send(DaemonFrame::Error { session_id: Some(session_id), message: "not attached to that session".into() }).await;
                    continue;
                }
                if let Err(why) = client_may_send(&cmd, client) {
                    let _ = tx.send(DaemonFrame::Error { session_id: Some(sid.clone()), message: format!("refused: {why}") }).await;
                    continue;
                }
                if state.reloading.load(Ordering::SeqCst)
                    && matches!(cmd, SessionCommand::Submit { .. } | SessionCommand::SubmitPrepared { .. } | SessionCommand::Compact { .. })
                {
                    let _ = tx.send(DaemonFrame::Error { session_id: Some(sid.clone()), message: "daemon reloading; retry in a moment".into() }).await;
                    continue;
                }
                match handle.send_from(client, cmd).await {
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
                let _ = tx.send(DaemonFrame::Bye { reason: None }).await;
                state.request_shutdown(force);
                break;
            }
            Ok(Read::Frame(ClientFrame::Attach(_))) | Ok(Read::Frame(ClientFrame::Hello(_))) => {
                let _ = tx.send(DaemonFrame::Error { session_id: Some(sid.clone()), message: "already attached".into() }).await;
            }
            Ok(Read::Frame(ClientFrame::Reload { .. })) | Ok(Read::Frame(ClientFrame::Purge)) => {
                let _ = tx.send(DaemonFrame::Error { session_id: Some(sid.clone()), message: "control frames are not accepted on an attached connection".into() }).await;
            }
            Ok(Read::Frame(ClientFrame::Bye)) | Ok(Read::Eof) | Err(_) => break,
            Ok(Read::Oversize) => {
                let _ = tx.send(DaemonFrame::Error { session_id: Some(sid.clone()), message: frame_limit_msg() }).await;
                break;
            }
            Ok(Read::Bad(e)) => {
                let _ = tx.send(DaemonFrame::Error { session_id: Some(sid.clone()), message: format!("malformed frame: {e}") }).await;
            }
        }
    }

    // C3: reload — the session is checkpointed and will be rehydrated by
    // the next image; tell the client to reconnect and stop forwarding.
    if reloading {
        // The forwarder drains and exits on the announce; bound it anyway.
        if tokio::time::timeout(Duration::from_secs(1), &mut forward).await.is_err() {
            forward.abort();
            let _ = forward.await;
        }
        let (generation, retry_after_ms) = match reload_bye(&state) {
            ByeReason::Reloading { generation, retry_after_ms } => (generation, retry_after_ms),
            _ => unreachable!(),
        };
        let env = crate::session::Envelope {
            session_id: sid.clone(),
            seq: u64::MAX,
            ts: chrono::Utc::now(),
            event: SessionEventWire::Reloading { generation, retry_after_ms },
        };
        let _ = tx.send(DaemonFrame::Event(env.into())).await;
        let _ = tx.send(DaemonFrame::Bye { reason: Some(ByeReason::Reloading { generation, retry_after_ms }) }).await;
        let _ = tokio::time::timeout(Duration::from_secs(1), writer).await;
        return;
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
    let _ = tx.send(DaemonFrame::Bye { reason: None }).await;
    drop(tx);
    let _ = tokio::time::timeout(Duration::from_millis(500), writer).await;
    tracing::debug!(session = %sid, client = client.0, "daemon: client detached");
}

fn reload_bye(state: &DaemonState) -> ByeReason {
    ByeReason::Reloading {
        generation: state.reload_generation.load(Ordering::SeqCst),
        retry_after_ms: super::reload::RETRY_AFTER_MS,
    }
}
