//! Per-connection pump: handshake → control loop → attach → pump.
//!
//! Tracing here never logs frame bodies (`ClientFrame`'s `Debug` redacts
//! `Answer`/`Submit`/`Steer`); `Answer` values are forwarded to the actor
//! and nowhere else.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use super::merge::{Merge, Source};
use super::DaemonState;
use crate::session::handle::CMD_CHAN_CAP;
use crate::session::transport::{TransportError, ATTACH_TIMEOUT, ATTACH_TIMEOUT_PARKED};
use crate::session::wire::*;
use crate::session::*;
use crate::ipc::{ServerReadHalf, ServerStream, ServerWriteHalf};
use crate::{SessionEvent, StreamEvent};

const HELLO_TIMEOUT: Duration = Duration::from_secs(2);

/// How many distinct differing client versions one daemon image notes at
/// info; later ones (and repeats) go to debug.
const CLIENT_VERSION_NOTES_MAX: usize = 32;

/// Client versions this daemon image has already noted at info. A reload
/// execs a fresh image, which starts empty. Kept as hashes, so the memory
/// is bounded whatever a client sends.
static CLIENT_VERSION_NOTES: std::sync::Mutex<ClientVersionNotes> =
    std::sync::Mutex::new(ClientVersionNotes { seen: Vec::new() });

struct ClientVersionNotes {
    seen: Vec<u64>,
}

impl ClientVersionNotes {
    /// True the first time `client` is seen, while the set has room.
    fn first_sight(&mut self, client: &str) -> bool {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        client.hash(&mut hasher);
        let key = hasher.finish();
        if self.seen.contains(&key) || self.seen.len() >= CLIENT_VERSION_NOTES_MAX {
            return false;
        }
        self.seen.push(key);
        true
    }
}

/// A client on the same protocol but another binary version is allowed.
/// Say so once per distinct version (clients may reconnect many times a
/// minute), then at debug.
fn note_client_version(notes: &std::sync::Mutex<ClientVersionNotes>, client: &str, daemon: &str) {
    if client == daemon {
        return;
    }
    let first = notes.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).first_sight(client);
    if first {
        tracing::info!(client = %client, daemon = %daemon, "daemon: client binary version differs (same protocol; allowed)");
    } else {
        tracing::debug!(client = %client, daemon = %daemon, "daemon: client binary version differs (same protocol; allowed)");
    }
}

type Reader = BufReader<ServerReadHalf>;

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
        | C::EphemeralLayer { .. }
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
        | C::TakeInput
        // S1 Focus hold mode: the app's commands.
        | C::Delivery { .. }
        | C::HeldRelease { .. }
        | C::HeldDismiss { .. }
        | C::HeldMark { .. }
        // Client tools v1: the actor checks owner and installer.
        | C::RegisterClientTools { .. }
        | C::ToolReturn { .. } => Ok(()),
        // A reload checkpoint stops every worker and fences the session
        // for the exec that follows: only the daemon's own reload sends it.
        C::Checkpoint { reason: crate::session::CheckpointReason::Reload, .. } => {
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
        C::ArmCapacityResume { .. } => Err("arm_capacity_resume: not a client command"),
        C::ReloadRefused { .. } => Err("reload_refused: not a client command"),
        C::ResumeAfterReload { .. } => Err("resume_after_reload: not a client command"),
        C::ResumeAfterCrash { .. } => Err("resume_after_crash: not a client command"),
        C::HostEvent(_) => Err("host_event: not a client command"),
        C::DriverStart { .. } => Ok(()),
        // Any attached client may fork (the answer goes to it alone).
        C::Fork { .. } => Ok(()),
        C::ForkDone { .. } => Err("fork_done: not a client command"),
    }
}

/// Whether `event` is a reply the actor addressed to another client than
/// this connection's `client` (`forked` goes to the client that asked). The
/// four client tool frames go to the client they name and no other; one
/// naming none goes nowhere.
fn addressed_to_another(event: &SessionEventWire, client: ClientId) -> bool {
    use SessionEventWire as E;
    match event {
        E::Forked { client: Some(to), .. } => *to != client,
        E::ClientTools { client: to, .. }
        | E::ToolInvoke { client: to, .. }
        | E::ToolCancel { client: to, .. }
        | E::ToolReturnAck { client: to, .. } => *to != Some(client),
        _ => false,
    }
}

/// Whether the pump keeps the main-channel envelope `env` from the
/// connection of `client`, which accepted `accepted` and reads history as
/// `history`.
fn pump_skips(env: &Envelope, accepted: &AcceptedEvents, history: HistoryMode, client: ClientId) -> bool {
    !accepted.allows(&env.event)
        || matches!(env.event, SessionEventWire::Attached { .. })
        // Child spend frames go out on the session's optional channel,
        // which this connection reads only after the opt-in
        // (`Hello.accepted_events`), through the merge. A guard: a frame
        // on the main channel is never forwarded.
        || matches!(env.event, SessionEventWire::ChildSpend(_))
        // A fork's reply goes to the client that asked for it only.
        || addressed_to_another(&env.event, client)
        || (history == HistoryMode::Digest
            && matches!(
                env.event,
                SessionEventWire::Stream(StreamEvent::Session(SessionEvent::MessageHistory(_)))
            ))
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

/// Capacity a connection's frame buffer is reset to after a large frame.
const FRAME_BUF_KEEP: usize = 64 * 1024;
/// A frame buffer that grew past this is dropped once the frame is read.
const FRAME_BUF_SHRINK_ABOVE: usize = 1024 * 1024;

/// Read one frame, then release the buffer if that frame made it large.
///
/// `line` lives as long as the connection, and `clear()` keeps capacity: one
/// `Submit` carrying a 15 MiB attachment (~20 MiB of base64) or a refused
/// oversize frame (up to 64 MiB) would otherwise stay allocated until the
/// client detaches, which for a daemon-attached TUI is days. Released right
/// after the read, on every outcome, not before the next one: an idle client
/// can go hours between frames. Same policy as the client's `FrameBuf`.
async fn read_frame(reader: &mut Reader, line: &mut String) -> std::io::Result<Read> {
    let read = read_frame_into(reader, line).await;
    release_large_frame_buf(line);
    read
}

fn release_large_frame_buf(line: &mut String) {
    if line.capacity() > FRAME_BUF_SHRINK_ABOVE {
        *line = String::with_capacity(FRAME_BUF_KEEP);
    }
}

async fn read_frame_into(reader: &mut Reader, line: &mut String) -> std::io::Result<Read> {
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

/// The frame for `env`, or `None` when its event has no client form
/// ([`WireEnvelope::for_client`]).
fn event_frame(env: Envelope) -> Option<DaemonFrame> {
    WireEnvelope::for_client(env).map(DaemonFrame::Event)
}

/// Writer task: owns the write half; bounded queue = per-client backpressure.
fn spawn_writer(mut w: ServerWriteHalf) -> (mpsc::Sender<DaemonFrame>, tokio::task::JoinHandle<()>) {
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

pub async fn serve(state: Arc<DaemonState>, stream: ServerStream, shutdown: CancellationToken) {
    let (r, w) = crate::ipc::split_server(stream);
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
    note_client_version(&CLIENT_VERSION_NOTES, &hello.client_version, &daemon_version);
    // Per connection, from this Hello only (a reconnect after a reload
    // brings its own).
    let accepted = AcceptedEvents::negotiate(&hello.accepted_events);
    let welcome = Welcome {
        protocol_version: PROTOCOL_VERSION,
        daemon_version,
        pid: std::process::id(),
        profile: state.profile.clone(),
        sessions: state.session_metas(),
        progressive_tool_disclosure: state.host.parts().progressive_tool_disclosure,
        generation: state.generation,
        daemon_build: crate::session::wire::build_id().to_string(),
        features: super::control_queries::daemon_features(),
        accepted_events: accepted.names(),
        config_path: state.host.started_config_path().map(str::to_owned),
        memory_backend: Some(state.host.started_memory_backend().to_owned()),
    };
    if tx.send(DaemonFrame::Welcome(welcome)).await.is_err() {
        return;
    }
    tracing::debug!(kind = ?hello.client.kind, "daemon: client connected");
    // This connection's `model_favorites` answer. Every way this connection
    // ends cancels it first, before any teardown that waits; a successful
    // attach keeps it.
    let mut favorites = super::control_queries::OneAtATime::default();

    // ── control loop (no session allocated) ──
    let attach = loop {
        let announce = state.announce();
        let frame = tokio::select! {
            _ = shutdown.cancelled() => {
                favorites.cancel().await;
                let _ = tx.send(DaemonFrame::Bye { reason: None }).await;
                let _ = writer.await;
                return;
            }
            _ = announce.cancelled() => {
                favorites.cancel().await;
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
                favorites.cancel().await;
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
                        // Tell the requester (with the workers a forced
                        // reload went ahead without), flush, close — then exec.
                        favorites.cancel().await;
                        let mut bye = reload_bye(&state);
                        if let ByeReason::Reloading { waived, .. } = &mut bye {
                            waived.clone_from(&prepared.waived);
                        }
                        let _ = tx.send(DaemonFrame::Bye { reason: Some(bye) }).await;
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
            Ok(Read::Frame(ClientFrame::Children { id, session_id, sid, members })) => {
                let _ = tx.send(super::control_queries::children_reply(id, session_id, sid, members).await).await;
            }
            Ok(Read::Frame(ClientFrame::ThinkingOptions { id, model, warm })) => {
                thinking_options(&state, &tx, id, model, warm).await;
            }
            // `synaps auth state`: the host broker's in-memory account state.
            Ok(Read::Frame(ClientFrame::AuthState(query))) => {
                let _ = tx.send(auth_state_reply(&state, &query)).await;
            }
            Ok(Read::Frame(ClientFrame::ModelFavorites { id })) => model_favorites(&state, &tx, id, &mut favorites).await,
            Ok(Read::Frame(ClientFrame::SandboxStatus { id, session })) => {
                let _ = tx.send(super::control_queries::sandbox_status_answer(id, session).await).await;
            }
            Ok(Read::Frame(ClientFrame::Hello(_))) => {
                let _ = tx.send(DaemonFrame::Error { session_id: None, message: "duplicate hello".into() }).await;
            }
            Ok(Read::Frame(ClientFrame::Bye)) | Ok(Read::Eof) | Err(_) => {
                favorites.cancel().await;
                let _ = tx.send(DaemonFrame::Bye { reason: None }).await;
                let _ = writer.await;
                return;
            }
            Ok(Read::Oversize) => {
                // First, so the answer's copy of the sender cannot keep the
                // writer open below.
                favorites.cancel().await;
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
        Attach::Existing { session_id, mode } => {
            // A session a crash restore still holds: wait for it (bounded).
            let gate = state.restoring.wait(session_id.as_str(), super::crash_record::GATE_WAIT).await;
            match state.attach(&session_id) {
                Some(h) if h.meta().locked_by.is_some() => (state.retry_locked_placeholder(h).await, mode),
                Some(h) => {
                    // Live, but given up (the cap ran out after its
                    // create): its one resume decision, before the attach.
                    super::crash_record::loaded_on_attach(&state, &h).await;
                    (h, mode)
                }
                None => {
                    let message = match gate {
                        Err(e) => e.message(session_id.as_str()),
                        Ok(()) => "unknown session".into(),
                    };
                    let _ = tx.send(DaemonFrame::Error { session_id: Some(session_id), message }).await;
                    favorites.cancel().await;
                    let _ = tx.send(DaemonFrame::Bye { reason: None }).await;
                    let _ = writer.await;
                    return;
                }
            }
        }
        Attach::Create { mut config, mode } => {
            // `--continue <id>` naming a session a crash restore still
            // holds: wait for it; once it is back the host hands back the
            // running actor below. One the restore gave up loads from disk.
            if let Some(Some(target)) = &config.continue_session {
                match state.restoring.wait(target, super::crash_record::GATE_WAIT).await {
                    Err(e @ super::crash_record::GateError::StillRestoring) => {
                        let _ = tx
                            .send(DaemonFrame::Error {
                                session_id: Some(SessionId::from(target.as_str())),
                                message: e.message(target),
                            })
                            .await;
                        favorites.cancel().await;
                        let _ = tx.send(DaemonFrame::Bye { reason: None }).await;
                        let _ = writer.await;
                        return;
                    }
                    // Loaded below from disk; its one resume follows.
                    Err(super::crash_record::GateError::GivenUp(_)) | Ok(()) => {}
                }
            }
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
                    favorites.cancel().await;
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
            let created = match state.create(config).await {
                Ok(h) if before.contains(&h.id) => {
                    tracing::info!(session = %h.id, "daemon: continue target is live — attaching, not creating");
                    attached_to_live = true;
                    (h, mode)
                }
                Ok(h) => (h, mode),
                Err(e) => {
                    let _ = tx.send(DaemonFrame::Error { session_id: None, message: format!("create session: {e}") }).await;
                    favorites.cancel().await;
                    let _ = tx.send(DaemonFrame::Bye { reason: None }).await;
                    let _ = writer.await;
                    return;
                }
            };
            // A session the crash restore gave up, now loaded (however the
            // client named it: id, name, chain, `--continue` latest) or
            // found live: the one resume decision the restore owed it
            // (before the attach, as a restore would have).
            super::crash_record::loaded_on_attach(&state, &created.0).await;
            created
        }
    };

    let mut rx = handle.subscribe();
    // §7.4: a connection that accepted "child_spend" also reads the session's
    // optional channel. It subscribes before Attach, so no frame published
    // after the attach snapshot is missed; the frames before it (seq below the
    // Attached envelope's) are in the snapshot's totals, and the merge's floor
    // drops them.
    let mut orx = accepted.accepts(EVENT_CHILD_SPEND).then(|| handle.subscribe_optional());
    if let Err(e) = handle.send(SessionCommand::Attach { client: hello.client.clone(), mode }).await {
        let _ = tx.send(DaemonFrame::Error { session_id: Some(handle.id.clone()), message: format!("attach: {e}") }).await;
        favorites.cancel().await;
        let _ = tx.send(DaemonFrame::Bye { reason: None }).await;
        let _ = writer.await;
        return;
    }
    let attach_budget = if handle.lifecycle() == SessionLifecycle::Parked { ATTACH_TIMEOUT_PARKED } else { ATTACH_TIMEOUT };
    let attached = tokio::time::timeout(attach_budget, async {
        loop {
            match rx.recv().await {
                Ok(env) => match env.event {
                    SessionEventWire::Attached { client, snapshot } => return Some(Ok((client, snapshot, env.seq))),
                    SessionEventWire::AttachRefused { message } => return Some(Err(message)),
                    _ => {}
                },
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    })
    .await;
    let (client, snapshot, floor) = match attached {
        Ok(Some(Ok(x))) => x,
        Ok(Some(Err(message))) => {
            let _ = tx.send(DaemonFrame::Error { session_id: Some(handle.id.clone()), message }).await;
            favorites.cancel().await;
            let _ = tx.send(DaemonFrame::Bye { reason: None }).await;
            let _ = writer.await;
            return;
        }
        _ => {
            let _ = tx.send(DaemonFrame::Error { session_id: Some(handle.id.clone()), message: "attach timed out".into() }).await;
            favorites.cancel().await;
            let _ = tx.send(DaemonFrame::Bye { reason: None }).await;
            let _ = writer.await;
            return;
        }
    };
    let legacy = legacy_shared(hello.client.capabilities, snapshot.client_input);
    let snapshot = replay_for(hello.client.capabilities, snapshot);
    // Our per-connection gates hold for the turn replay as on the pump
    // (790d R4): nothing this hello did not accept, no reply addressed to
    // another client.
    let mut snapshot = if legacy { as_owner(client, snapshot) } else { snapshot };
    snapshot.replay.retain(|env| !pump_skips(env, &accepted, hello.client.history, client));
    if tx.send(DaemonFrame::Attached(AttachedWire::new(client, snapshot))).await.is_err() {
        favorites.cancel().await;
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
        if let Some(frame) = event_frame(env) {
            let _ = tx.send(frame).await;
        }
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
    let accepted = accepted.clone();
    // Both opt-in gates apply, and neither implies the other (790d R4): our
    // `Hello.accepted_events` and per-client addressing (`pump_skips`), and
    // upstream's `ClientMeta.capabilities` (`forwarded`). A legacy client on
    // a shared session never gets owner changes.
    let skip = move |env: &Envelope| -> bool {
        pump_skips(env, &accepted, history, client)
            || !forwarded(history, caps, env)
            || (legacy && matches!(env.event, SessionEventWire::InputOwnerChanged { .. }))
    };
    // Not tied to `shutdown`: on daemon stop the client must still see `Ended`.
    let mut forward = tokio::spawn(async move {
        // §7.4: one stream in seq order from both channels, never holding an
        // envelope back for a later one; `skip` applies to main envelopes only.
        let mut merge = Merge::new(fwd_handle.id.clone(), floor);
        loop {
            let next = tokio::select! {
                biased;
                r = merge.next(&mut rx, orx.as_mut()) => r,
                // C3: reload announced — the checkpoint's events (abort
                // notice, Conversation with the kept partial history) are already in
                // the broadcast; drain what is there, then stop.
                _ = fwd_announce.cancelled() => {
                    merge.drain(&mut rx, orx.as_mut(), &skip, &fwd_tx).await;
                    break;
                }
            };
            // The main channel closed and everything peeked went out.
            let Some((env, source)) = next else { break };
            // Attached is addressed to one client; ours already went out as a frame.
            if source == Source::Main && skip(&env) {
                continue;
            }
            let ended = matches!(env.event, SessionEventWire::Ended { .. });
            let Some(frame) = event_frame(env) else {
                continue;
            };
            if fwd_tx.send(frame).await.is_err() {
                break;
            }
            if ended {
                break;
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
                // A client's own Detach ends its feed: nothing after it is
                // forwarded (its `ClientLeft` included), whether its `Bye`
                // shares the read (one socket segment) or comes in a later
                // one (a named pipe often delivers the two writes apart).
                if matches!(&cmd, SessionCommand::Detach { client: c } if *c == client) {
                    forward.abort();
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
            Ok(Read::Frame(ClientFrame::Children { id, session_id, sid, members })) => {
                let _ = tx.send(super::control_queries::children_reply(id, session_id, sid, members).await).await;
            }
            Ok(Read::Frame(ClientFrame::ThinkingOptions { id, model, warm })) => {
                thinking_options(&state, &tx, id, model, warm).await;
            }
            Ok(Read::Frame(ClientFrame::ModelFavorites { id })) => model_favorites(&state, &tx, id, &mut favorites).await,
            Ok(Read::Frame(ClientFrame::SandboxStatus { id, session })) => {
                let _ = tx.send(super::control_queries::sandbox_status_answer(id, session).await).await;
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

    // The connection is ending: its favorites answer goes before any
    // teardown below waits on the forwarder or the writer.
    favorites.cancel().await;

    // C3: reload — the session is checkpointed and will be rehydrated by
    // the next image; tell the client to reconnect and stop forwarding.
    if reloading {
        // The forwarder drains and exits on the announce; bound it anyway.
        if tokio::time::timeout(Duration::from_secs(1), &mut forward).await.is_err() {
            forward.abort();
            let _ = forward.await;
        }
        let (generation, retry_after_ms) = match reload_bye(&state) {
            ByeReason::Reloading { generation, retry_after_ms, .. } => (generation, retry_after_ms),
            _ => unreachable!(),
        };
        let env = crate::session::Envelope {
            session_id: sid.clone(),
            seq: u64::MAX,
            ts: chrono::Utc::now(),
            event: SessionEventWire::Reloading { generation, retry_after_ms },
        };
        if let Some(frame) = event_frame(env) {
            let _ = tx.send(frame).await;
        }
        let _ = tx
            .send(DaemonFrame::Bye { reason: Some(ByeReason::Reloading { generation, retry_after_ms, waived: Vec::new() }) })
            .await;
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

/// `thinking_options`: no model = what a new session gets (config as of the
/// last boot or reload). With `warm`, the reply comes from a task (match it
/// by `id`).
async fn thinking_options(
    state: &Arc<DaemonState>,
    tx: &mpsc::Sender<DaemonFrame>,
    id: Option<u64>,
    model: Option<String>,
    warm: bool,
) {
    let model = model.unwrap_or_else(|| state.host.new_session_model());
    if !warm {
        let _ = tx.send(super::control_queries::thinking_options_reply(id, model)).await;
        return;
    }
    let (host, tx) = (Arc::clone(&state.host), tx.clone());
    tokio::spawn(async move {
        let frame =
            super::control_queries::thinking_options_answer(id, model, true, || super::control_queries::host_broker(&host)).await;
        let _ = tx.send(frame).await;
    });
}

/// `model_favorites`: answered from a task the connection owns (`in_flight`),
/// so its file reads and the bounded extension-manager read never hold up
/// this connection's loop; match the reply by `id`. One at a time per
/// connection: a request while the last one runs is refused at once.
async fn model_favorites(
    state: &Arc<DaemonState>,
    tx: &mpsc::Sender<DaemonFrame>,
    id: Option<u64>,
    in_flight: &mut super::control_queries::OneAtATime,
) {
    let (host, reply) = (Arc::clone(&state.host), tx.clone());
    let started = in_flight.start(async move {
        let frame = super::control_queries::model_favorites_answer(id, &host).await;
        let _ = reply.send(frame).await;
    });
    if !started {
        let message = "model favorites: the last query on this connection is still running".to_string();
        let _ = tx.send(DaemonFrame::Error { session_id: None, message }).await;
    }
}

/// A reload was announced: forward what is still in the ring, then return.
/// A receiver that fell behind gets `Lagged` first. The drain logs it,
/// sends the lag notice, and goes on with the envelopes that survived:
/// they are the newest, the checkpoint's own notice and Conversation among
/// them, so stopping at the lag would drop exactly those. Envelopes `skip`
/// matches are not forwarded, as on the pump. Returns false once the client
/// is gone. The pump drains both channels through its own [`Merge`]; this is
/// the main channel alone.
#[cfg(any(test, feature = "testing"))]
async fn drain_on_reload(
    rx: &mut broadcast::Receiver<Envelope>,
    skip: &impl Fn(&Envelope) -> bool,
    session: &SessionId,
    out: &mpsc::Sender<DaemonFrame>,
) -> bool {
    Merge::new(session.clone(), 0).drain(rx, None, skip, out).await
}

fn reload_bye(state: &DaemonState) -> ByeReason {
    ByeReason::Reloading {
        generation: state.reload_generation.load(Ordering::SeqCst),
        retry_after_ms: super::reload::RETRY_AFTER_MS,
        waived: Vec::new(),
    }
}

/// Test seams.
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use super::*;

    /// The reload drain, for an integration test's receiver.
    pub async fn drain_on_reload(
        rx: &mut broadcast::Receiver<Envelope>,
        skip: &impl Fn(&Envelope) -> bool,
        session: &SessionId,
        out: &mpsc::Sender<DaemonFrame>,
    ) -> bool {
        super::drain_on_reload(rx, skip, session, out).await
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
        let reload = SessionCommand::Checkpoint {
            reason: CheckpointReason::Reload,
            deadline: None,
            skip_quiesce_grace: false,
        };
        assert!(client_may_send(&reload, own).is_err());
        let host = SessionCommand::Checkpoint {
            reason: CheckpointReason::HostRequest,
            deadline: None,
            skip_quiesce_grace: false,
        };
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
            memory_only: false,
            client_tool: None,
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
            client_tools: Default::default(),
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

    /// A large frame doesn't pin its buffer for the life of the connection:
    /// the buffer is back to [`FRAME_BUF_KEEP`] straight after the read (not
    /// at the next frame, which for an idle TUI may be hours away), and the
    /// connection keeps reading frames normally afterwards.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_large_frame_releases_its_buffer_after_the_read() {
        let (ours, theirs) = tokio::net::UnixStream::pair().unwrap();
        let (read_half, _keep) = ours.into_split();
        let mut reader = BufReader::new(read_half);
        let mut line = String::new();

        // 2 MiB of leading whitespace is still a valid `ping` frame; the
        // second big frame is garbage; the last is a normal small frame.
        let pad = " ".repeat(2 * 1024 * 1024);
        let input = format!("{pad}{{\"type\":\"ping\"}}\n{pad}not json\n{{\"type\":\"ping\"}}\n");
        let writer = tokio::spawn(async move {
            let mut theirs = theirs;
            theirs.write_all(input.as_bytes()).await.unwrap();
            theirs
        });

        let first = read_frame(&mut reader, &mut line).await.unwrap();
        assert!(matches!(first, Read::Frame(ClientFrame::Ping)));
        assert!(line.capacity() <= FRAME_BUF_SHRINK_ABOVE, "kept {} bytes", line.capacity());

        let second = read_frame(&mut reader, &mut line).await.unwrap();
        assert!(matches!(second, Read::Bad(_)), "a malformed big frame is reported");
        assert!(line.capacity() <= FRAME_BUF_SHRINK_ABOVE, "kept {} bytes", line.capacity());

        let third = read_frame(&mut reader, &mut line).await.unwrap();
        assert!(matches!(third, Read::Frame(ClientFrame::Ping)));
        drop(writer.await.unwrap());
        assert!(matches!(read_frame(&mut reader, &mut line).await.unwrap(), Read::Eof));
    }

    /// Ordinary frames reuse the buffer: no reallocation per frame.
    #[test]
    fn a_small_frame_buffer_is_reused() {
        let mut line = String::with_capacity(FRAME_BUF_KEEP);
        line.push_str("{\"type\":\"ping\"}");
        let before = line.as_ptr();
        release_large_frame_buf(&mut line);
        assert_eq!(line.as_ptr(), before, "a small buffer is not reallocated");

        let mut big = String::with_capacity(FRAME_BUF_SHRINK_ABOVE + 1);
        release_large_frame_buf(&mut big);
        assert!((FRAME_BUF_KEEP..=FRAME_BUF_SHRINK_ABOVE).contains(&big.capacity()));
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

    /// `fork` is a client command that waits out a reload; its reply goes to
    /// the client that asked only. Mutant: drop `addressed_to_another` from
    /// the pump's skip and a second client gets the first one's `forked`.
    #[test]
    fn a_fork_is_a_client_command_and_its_reply_goes_to_the_asker_only() {
        let own = ClientId(3);
        let fork = SessionCommand::Fork { label: "Side: x".into() };
        assert!(client_may_send(&fork, own).is_ok());
        assert!(refused_while_draining(&fork));
        let done = SessionCommand::ForkDone {
            client: Some(own),
            label: "Side: x".into(),
            message_count: 0,
            result: Ok("s".into()),
        };
        assert!(client_may_send(&done, own).is_err());
        let forked = |to: Option<ClientId>| SessionEventWire::Forked {
            client: to,
            session_id: "s".into(),
            label: "Side: x".into(),
            message_count: 2,
        };
        assert!(!addressed_to_another(&forked(Some(own)), own));
        assert!(addressed_to_another(&forked(Some(ClientId(4))), own));
        assert!(!addressed_to_another(&forked(None), own), "unaddressed: every client");
        assert!(!addressed_to_another(&SessionEventWire::Idle, own));
    }

    /// Client tools v1: both commands are client commands, and a frame the
    /// actor addressed goes to that client only; an unaddressed one to
    /// nobody (a `tool_invoke` never reaches a second client). Mutant: the
    /// `Forked` arm's `None` = everyone rule for these frames.
    #[test]
    fn client_tool_frames_go_to_the_client_they_name_only() {
        use crate::session::client_tools::ToolCancelFrame;
        let own = ClientId(3);
        let fields = serde_json::Map::new();
        let register = SessionCommand::RegisterClientTools { id: 1, fields: fields.clone() };
        assert!(client_may_send(&register, own).is_ok());
        assert!(client_may_send(&SessionCommand::ToolReturn { id: 2, fields }, own).is_ok());
        let cancel = |to: Option<ClientId>| SessionEventWire::ToolCancel {
            client: to,
            cancel: ToolCancelFrame {
                invocation_id: "ci_1".into(),
                registration_id: "cr_1".into(),
                reason: "turn_cancel".into(),
            },
        };
        assert!(!addressed_to_another(&cancel(Some(own)), own));
        assert!(addressed_to_another(&cancel(Some(ClientId(4))), own));
        assert!(addressed_to_another(&cancel(None), own), "unaddressed: nobody");
    }

    /// The pump's skip for two clients attached to one session: the
    /// `forked` the actor addressed to client 3 goes out on client 3's
    /// connection only; an unaddressed one (a host sender) on both. Mutant:
    /// drop `addressed_to_another` from `pump_skips` and client 4 gets
    /// client 3's reply.
    #[test]
    fn the_pump_sends_a_forked_only_on_the_asking_clients_connection() {
        let accepted = AcceptedEvents::negotiate(&[]);
        let (asker, other) = (ClientId(3), ClientId(4));
        let forked = |to: Option<ClientId>| Envelope {
            session_id: SessionId("s".into()),
            seq: 7,
            ts: chrono::Utc::now(),
            event: SessionEventWire::Forked {
                client: to,
                session_id: "s-fork".into(),
                label: "Side: x".into(),
                message_count: 2,
            },
        };
        for history in [HistoryMode::Full, HistoryMode::Digest] {
            assert!(!pump_skips(&forked(Some(asker)), &accepted, history, asker), "the asker gets it");
            assert!(pump_skips(&forked(Some(asker)), &accepted, history, other), "the other client does not");
            assert!(!pump_skips(&forked(None), &accepted, history, asker));
            assert!(!pump_skips(&forked(None), &accepted, history, other));
        }
    }

    #[derive(Clone, Default)]
    struct Sink(Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A reconnecting client on another binary version is noted at info once
    /// per version, then at debug; the set of noted versions is bounded.
    #[test]
    fn a_differing_client_version_is_noted_once_per_version_and_the_notes_are_bounded() {
        let notes = std::sync::Mutex::new(ClientVersionNotes { seen: Vec::new() });
        let sink = Sink::default();
        let writer = sink.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .without_time()
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            for _ in 0..3 {
                note_client_version(&notes, "0.10.1", "0.10.2");
            }
            note_client_version(&notes, "0.10.2", "0.10.2");
            for n in 0..CLIENT_VERSION_NOTES_MAX {
                note_client_version(&notes, &format!("9.9.{n}"), "0.10.2");
            }
        });
        let out = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
        let level = |l: &str| out.lines().filter(|line| line.contains(l) && line.contains("client binary version differs")).count();
        // 0.10.1 once, then 31 of the 32 new ones: the cap is 32 versions.
        assert_eq!(level("INFO"), CLIENT_VERSION_NOTES_MAX, "{out}");
        assert_eq!(level("DEBUG"), 2 + 1, "{out}");
        assert!(out.lines().filter(|l| l.contains("INFO")).all(|l| l.contains("client=0.10.1") || l.contains("client=9.9.")), "{out}");
        assert!(out.lines().any(|l| l.contains("DEBUG") && l.contains("client=9.9.31")), "the version past the cap goes to debug: {out}");
        assert!(!out.contains("client=0.10.2"), "the same version is never noted: {out}");
        assert_eq!(notes.lock().unwrap().seen.len(), CLIENT_VERSION_NOTES_MAX);
    }
}

#[cfg(test)]
#[path = "conn_tests.rs"]
mod drain_tests;
