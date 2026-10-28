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
use crate::{SessionEvent, StreamEvent};

const HELLO_TIMEOUT: Duration = Duration::from_secs(2);

type Reader = BufReader<OwnedReadHalf>;

/// Which `SessionCommand`s a socket client may originate (§4 MED). The UDS
/// is 0600 same-uid, so this is a footgun guard, not an auth boundary:
/// anything that drives the actor's *own* bookkeeping (`Attach`, `Detach`
/// for another client, `Resync`, `HostEvent`) or ends the session for every
/// other client with a host reason (`End`) is refused with an `Error` frame.
/// The owner ends a session with `/close` (an engine command, checked by the
/// actor); `daemon close` does it from a control connection
/// (`ClientFrame::Close`), `daemon stop` for every session.
fn client_may_send(cmd: &SessionCommand, own: ClientId) -> Result<(), &'static str> {
    use SessionCommand as C;
    match cmd {
        C::Submit { .. }
        | C::Steer { .. }
        | C::SubagentSteer { .. }
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
        | C::KeepWarm { .. }
        // `/takeover`: the actor checks the attach mode.
        | C::TakeInput => Ok(()),
        // A reload checkpoint stops every worker and fences the session
        // for the exec that follows: only the daemon's own reload sends it.
        C::Checkpoint { reason: crate::session::CheckpointReason::Reload } => {
            Err("checkpoint(reload): host-only (use `synaps daemon reload`)")
        }
        C::Checkpoint { .. } => Ok(()),
        C::Detach { client } if *client == own => Ok(()),
        C::Detach { .. } => Err("detach: not your client id"),
        C::Attach { .. } => Err("attach: already attached (one session per connection)"),
        // C4: an owner may end its session (`/end`); the actor enforces
        // ownership (B1 `is_input_command`), the conn only screens reasons.
        C::End { reason: EndReason::ClientQuit } => Ok(()),
        C::End { .. } => Err("end: only ClientQuit may come from a client (host reasons are the daemon's)"),
        C::Resync { .. } => Err("resync: not a client command"),
        C::Park => Err("park: not a client command"),
        C::AdoptWorkers { .. } => Err("adopt_workers: not a client command"),
        C::RestoreToolActivations { .. } => Err("restore_tool_activations: not a client command"),
        C::ReloadAborted => Err("reload_aborted: not a client command"),
        C::ArmPark => Err("arm_park: not a client command"),
        C::HostEvent(_) => Err("host_event: not a client command"),
        C::DriverStart { .. } => Ok(()),
    }
}

/// Refused at the connection while a reload is in progress — from the drain
/// on, before the session is even checkpointed. `SessionCommand::
/// refused_while_reloading` minus what the drain must still allow: a
/// `Steer`, `Set` or engine command for the turn the drain is waiting to
/// finish. The actor's reload fence refuses those once it has checkpointed
/// (it alone knows whether a `Steer` would start a turn).
fn refused_while_draining(cmd: &SessionCommand) -> bool {
    use SessionCommand as C;
    cmd.refused_while_reloading()
        && !matches!(
            cmd,
            C::Steer { .. } | C::SubagentSteer { .. } | C::Set { .. } | C::EngineCommand { .. } | C::KeepWarm { .. }
        )
}

// One per line read, consumed at once: not worth boxing every frame.
#[allow(clippy::large_enum_variant)]
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
    // Clients validate their name before connecting; this is the backstop.
    if let Some(Err(message)) = hello.client.name.as_deref().map(crate::session::validate_client_name) {
        let _ = tx.send(DaemonFrame::Refused { reason: RefuseReason::Protocol, message }).await;
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
        daemon_build: crate::session::wire::build_id().to_string(),
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
                // `daemon status` and `daemon reload` read daemon.json after a Ping.
                state.heal_daemon_json();
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
                        let _ = tokio::time::timeout(
                            Duration::from_secs(crate::session::budgets::RELOAD_BYE_FLUSH_SECS),
                            writer,
                        )
                        .await;
                        let e = super::reload::exec(&state, prepared).await;
                        tracing::error!(error = %e, "daemon: reload did not exec");
                        return;
                    }
                }
            }
            // `daemon close`: end one session with host authority. A
            // refusal is terminal (the writer closes after `Refused`).
            Ok(Read::Frame(ClientFrame::Close { session_id, force, attached })) => {
                tracing::info!(session = %session_id, force, attached, "daemon: close requested over the socket");
                let reply = super::close::close_session(&state, &session_id, force, attached).await;
                let _ = tx.send(reply).await;
            }
            // `daemon pin`: pin/unpin one session with host authority. A
            // refusal is terminal, like `Close`.
            Ok(Read::Frame(ClientFrame::Pin { session_id, on })) => {
                tracing::info!(session = %session_id, on, "daemon: pin requested over the socket");
                let reply = super::pin::pin_session(&state, &session_id, on).await;
                let _ = tx.send(reply).await;
            }
            // `daemon rename`: name/unname one session with host authority,
            // no attach. A refusal is terminal, like `Close`.
            Ok(Read::Frame(ClientFrame::Rename { session_id, name, expect })) => {
                tracing::info!(session = %session_id, clear = name.is_none(), "daemon: rename requested over the socket");
                let reply = super::rename::rename_session(&state, &session_id, name.as_deref(), expect).await;
                let _ = tx.send(reply).await;
            }
            // `daemon reload-extension`: restart one extension in place,
            // sessions untouched. A refusal is terminal, like `Close`.
            Ok(Read::Frame(ClientFrame::ReloadExtension { extension })) => {
                tracing::info!(extension = %extension, "daemon: extension reload requested over the socket");
                let reply = super::extension_reload::reload_extension(&state, &extension).await;
                let _ = tx.send(reply).await;
            }
            // C2: jemalloc purge in the daemon (bench hygiene); reply Pong.
            Ok(Read::Frame(ClientFrame::Purge)) => {
                agent_core::core::memstat::purge_arenas();
                let _ = tx
                    .send(DaemonFrame::Pong { pid: std::process::id(), uptime_s: state.uptime_s(), sessions: state.live_sessions().len() })
                    .await;
            }
            // `synaps auth state`: the host broker's in-memory account state.
            Ok(Read::Frame(ClientFrame::AuthState(query))) => {
                let _ = tx.send(auth_state_reply(&state, &query)).await;
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
    let mut attached_to_live = false;
    let (handle, mode) = match attach {
        Attach::Existing { session_id, mode } => match state.attach(&session_id) {
            Some(h) if h.meta().locked_by.is_some() => (state.retry_locked_placeholder(h).await, mode),
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
            // Recorded by the actor from `/memory` (and carried by reload's
            // rehydrate, which does not come through here); a client turns
            // memory on or off with `/memory`, not by create config.
            config.memory_mode = None;
            if config.cwd.is_none() {
                config.cwd = Some(hello.cwd.clone());
            }
            if config.env.is_none() {
                config.env = hello.env.clone();
            }
            if config.env_stripped.is_empty() {
                config.env_stripped = hello.env_stripped.clone();
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
            // `--continue X` while X is live here: the host hands back the
            // running actor (attach-if-live). Tell the client — it asked to
            // create and got the existing session instead. Detected by id
            // presence before/after so a client-less Parked target counts too.
            let before: Vec<SessionId> = if config.continue_session.is_some() {
                state.live_sessions().into_iter().map(|h| h.id).collect()
            } else {
                Vec::new()
            };
            match state.create(config).await {
                Ok(h) if before.contains(&h.id) => {
                    tracing::info!(session = %h.id, "daemon: continue target is live — attaching, not creating");
                    attached_to_live = true;
                    (h, mode)
                }
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
    let legacy = legacy_shared(hello.client.capabilities, snapshot.client_input);
    let snapshot = replay_for(hello.client.capabilities, snapshot);
    let snapshot = if legacy { as_owner(client, snapshot) } else { snapshot };
    if tx.send(DaemonFrame::Attached(AttachedWire::new(client, snapshot))).await.is_err() {
        let _ = handle.send(SessionCommand::Detach { client }).await;
        return;
    }
    tracing::debug!(session = %handle.id, client = client.0, "daemon: client attached");
    if attached_to_live {
        let env = Envelope {
            session_id: handle.id.clone(),
            seq: u64::MAX,
            ts: chrono::Utc::now(),
            event: SessionEventWire::SystemNotice(format!(
                "session {} is already running in this daemon — attached to it instead of creating a second copy",
                handle.id
            )),
        };
        let _ = tx.send(DaemonFrame::Event(env.into())).await;
    }

    // ── pump ──
    let sid = handle.id.clone();
    let fwd_tx = tx.clone();
    let fwd_handle = handle.clone();
    let fwd_announce = state.announce();
    // Phase 4 §2.3: Digest clients never receive the per-round full history;
    // the `Conversation` digest the actor emits right after is what they key on.
    let history = hello.client.history;
    let caps = hello.client.capabilities;
    let skip = move |env: &Envelope| -> bool {
        !forwarded(history, caps, env) || (legacy && matches!(env.event, SessionEventWire::InputOwnerChanged { .. }))
    };
    // Not tied to `shutdown`: on daemon stop the client must still see `Ended`.
    let mut forward = tokio::spawn(async move {
        loop {
            let next = tokio::select! {
                biased;
                r = rx.recv() => r,
                // C3: reload announced — the checkpoint's events (abort
                // notice, Conversation with the kept partial history) are already in
                // the broadcast; drain what is there, then stop.
                _ = fwd_announce.cancelled() => {
                    while let Ok(env) = rx.try_recv() {
                        if skip(&env) {
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
                    if skip(&env) {
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
                if state.reloading.load(Ordering::SeqCst) && refused_while_draining(&cmd) {
                    let _ = tx.send(DaemonFrame::Error { session_id: Some(sid.clone()), message: crate::session::RELOADING_REFUSAL.into() }).await;
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
                // `daemon status` and `daemon reload` read daemon.json after a Ping.
                state.heal_daemon_json();
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
            // An attached client closes its session with `/close` and pins
            // it with `/pin`; `synaps auth state` is a control connection.
            Ok(Read::Frame(ClientFrame::Reload { .. }))
            | Ok(Read::Frame(ClientFrame::Purge))
            | Ok(Read::Frame(ClientFrame::Close { .. }))
            | Ok(Read::Frame(ClientFrame::Pin { .. }))
            | Ok(Read::Frame(ClientFrame::Rename { .. }))
            | Ok(Read::Frame(ClientFrame::ReloadExtension { .. }))
            | Ok(Read::Frame(ClientFrame::AuthState(_))) => {
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
    } else if ended {
        // The actor broadcast `Ended` before it dropped its queue, so it is
        // in the forwarder's receiver: let it go out before the `Bye`, or the
        // client sees EOF and never learns the session ended (`/close` from
        // another client, `daemon close`, eviction).
        if tokio::time::timeout(Duration::from_secs(1), &mut forward).await.is_err() {
            forward.abort();
            let _ = forward.await;
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

/// Whether the pump forwards `env` to this connection's client. Never the
/// actor's `Attached` (the conn sends its own). Digest clients never get the
/// per-round full history; the `Conversation` digest the actor emits right
/// after is what they key on (phase 4 §2.3). And never an event the client
/// cannot decode ([`client_decodes`]: an older client fails the whole frame).
fn forwarded(history: HistoryMode, caps: ClientCapabilities, env: &Envelope) -> bool {
    !(matches!(env.event, SessionEventWire::Attached { .. })
        || (history == HistoryMode::Digest
            && matches!(
                env.event,
                SessionEventWire::Stream(StreamEvent::Session(SessionEvent::MessageHistory(_)))
            ))
        || !client_decodes(caps, &env.event))
}

/// The attach snapshot as this client may receive it: its turn replay
/// without events it cannot decode — one would sink the whole `Attached`
/// frame, and with it every reconnect.
fn replay_for(caps: ClientCapabilities, mut snapshot: AttachSnapshot) -> AttachSnapshot {
    snapshot.replay.retain(|env| client_decodes(caps, &env.event));
    snapshot
}

/// [`legacy_shared`]: the snapshot as an older client on a shared session
/// gets it: it owns input unless it observes, and its replay has no owner
/// changes (it never gets live ones either).
fn as_owner(client: ClientId, mut snapshot: AttachSnapshot) -> AttachSnapshot {
    let observes = snapshot.roster.iter().any(|c| c.id == client && c.mode == AttachMode::Observe);
    if !observes {
        snapshot.input_owner = Some(client);
    }
    snapshot.replay.retain(|env| !matches!(env.event, SessionEventWire::InputOwnerChanged { .. }));
    snapshot
}

fn reload_bye(state: &DaemonState) -> ByeReason {
    ByeReason::Reloading {
        generation: state.reload_generation.load(Ordering::SeqCst),
        retry_after_ms: super::reload::RETRY_AFTER_MS,
    }
}

/// `AuthState`: the in-memory account state of the broker on the host's
/// token cache, which every session of this daemon uses. Same trust as the
/// other control frames (the socket is the user's own). No network request,
/// no refresh; a clear never writes the credential store.
fn auth_state_reply(state: &DaemonState, query: &agent_core::auth::AuthStateQuery) -> DaemonFrame {
    match agent_core::auth::local_auth_state(&state.host.parts().token_cache, query, true) {
        Ok(report) => DaemonFrame::AuthState { report },
        Err(e) => DaemonFrame::Error { session_id: None, message: format!("auth state: {e}") },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::CheckpointReason;

    /// A reload checkpoint stops every worker and fences the session until
    /// an exec that only the daemon's own reload performs: no client may
    /// send one. A host-request checkpoint stays a client command.
    #[test]
    fn a_reload_checkpoint_is_host_only() {
        let own = ClientId(1);
        let reload = SessionCommand::Checkpoint { reason: CheckpointReason::Reload };
        assert!(client_may_send(&reload, own).is_err());
        let host = SessionCommand::Checkpoint { reason: CheckpointReason::HostRequest };
        assert!(client_may_send(&host, own).is_ok());
    }

    /// `/takeover` comes from a client by definition (the actor checks the
    /// attach mode), so the conn lets it through.
    #[test]
    fn take_input_is_a_client_command() {
        assert!(client_may_send(&SessionCommand::TakeInput, ClientId(2)).is_ok());
    }

    fn stream_env(ev: StreamEvent) -> Envelope {
        Envelope {
            session_id: SessionId::from("s"),
            seq: 1,
            ts: chrono::Utc::now(),
            event: SessionEventWire::Stream(ev),
        }
    }

    /// An older client (no `tool_queue` capability) is never sent the queue
    /// signals — live or in the attach replay — and still gets every other
    /// event; a current client gets them all.
    #[test]
    fn queue_signals_are_only_forwarded_to_clients_that_opted_in() {
        use crate::LlmEvent;
        let queued = stream_env(StreamEvent::Llm(LlmEvent::ToolQueued { tool_id: "t2".into() }));
        let started = stream_env(StreamEvent::Llm(LlmEvent::ToolStarted { tool_id: "t2".into() }));
        let result = stream_env(StreamEvent::Llm(LlmEvent::ToolResult {
            tool_id: "t1".into(),
            result: "ok".into(),
        }));
        let old = ClientCapabilities::default();
        let new = ClientCapabilities::current();
        for history in [HistoryMode::Full, HistoryMode::Digest] {
            assert!(!forwarded(history, old, &queued));
            assert!(!forwarded(history, old, &started));
            assert!(forwarded(history, old, &result));
            assert!(forwarded(history, new, &queued));
            assert!(forwarded(history, new, &started));
            assert!(forwarded(history, new, &result));
        }

        let session = crate::session::handle::echo::meta_for(&SessionId::from("s"));
        let snapshot = AttachSnapshot {
            meta: session,
            view: crate::session::handle::echo::view_for(),
            conversation: Default::default(),
            streaming: true,
            replay: vec![queued.clone(), result.clone(), started.clone()],
            pending_prompts: vec![],
            clients: vec![],
            roster: Vec::new(),
            client_input: ClientInput::wire_default(),
            input_owner: None,
            display_tail: None,
        };
        let kinds = |s: &AttachSnapshot| -> Vec<String> {
            s.replay
                .iter()
                .map(|e| match &e.event {
                    SessionEventWire::Stream(StreamEvent::Llm(LlmEvent::ToolQueued { .. })) => "queued",
                    SessionEventWire::Stream(StreamEvent::Llm(LlmEvent::ToolStarted { .. })) => "started",
                    _ => "other",
                })
                .map(str::to_string)
                .collect()
        };
        assert_eq!(kinds(&replay_for(old, snapshot.clone())), vec!["other"]);
        assert_eq!(kinds(&replay_for(new, snapshot)), vec!["queued", "other", "started"]);
    }

    /// The drain gate refuses work that would start after the record, and
    /// leaves what the turn being drained still needs.
    #[test]
    fn the_drain_gate_keeps_steering_the_running_turn() {
        let submit = SessionCommand::Submit { text: "x".into(), attachments: vec![] };
        assert!(refused_while_draining(&submit));
        assert!(refused_while_draining(&SessionCommand::NewSession));
        assert!(refused_while_draining(&SessionCommand::Resume { id: 1, query: "q".into() }));
        assert!(!refused_while_draining(&SessionCommand::Steer { text: "x".into() }));
        assert!(!refused_while_draining(&SessionCommand::Cancel));
    }
}
