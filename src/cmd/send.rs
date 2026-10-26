//! `synaps send` — deliver an event via Unix socket, falling back to inbox file-drop.
//!
//! ## Driver-aware remote steering (P9, C2)
//! `send` is a dumb pipe: it delivers an [`Event`] to the target session's
//! per-session event socket, which lands in the runtime `event_queue`. It does
//! NOT speak `SessionCommand` (no `Submit`/`Steer`), and it never inspects
//! driver state. ALL Submit-vs-Steer routing — including remote steering of a
//! *driver-armed* session — is owned by the actor's event-queue wake path
//! (`session/actor.rs::on_queue_wake` → `engine/reactor.rs::drain_event_queue`
//! + `wake_action`):
//!
//! * session **busy** (a driver-owned turn is streaming): the event is
//!   **Steered** into the live stream via the steer channel — this is exactly
//!   the "route to Steer while driver-armed" behavior, achieved server-side.
//! * session **idle + driver armed**: the event is injected as context but the
//!   actor **inhibits the competing turn** (`on_queue_wake`: "RunTurn inhibited
//!   — driver armed") so the driver's own tick owns scheduling — no rogue
//!   Submit-turn races the driver.
//! * session **idle + unarmed**: injected + a normal auto-turn (the historical
//!   `Submit` behavior).
//!
//! Because armed-state routing is the actor's job, `send` needs no armed-state
//! discovery and stays transport-simple. See `tests/send_remote_steering.rs`.
//!
//! ## The shared inbox
//! `~/.synaps-cli/inbox/` is untargeted mail: a daemon session takes a file
//! only while a client owns its input, and exactly one session takes it. So
//! an event addressed to a session never falls back to it: a `--session`
//! that matches nothing, or whose socket still refuses the event after
//! [`SOCKET_ATTEMPTS`] tries, is an error (exit 1, nothing delivered), not
//! a drop that some other session would take. `--inbox` opts back in to the
//! drop. `--broadcast` reports each session it could not reach and exits 1.
//! Only the no-flag form, which names no session, still uses the inbox.

use chrono::Utc;
use synaps_cli::events::registry::{
    find_session_registration, list_active_sessions, SessionRegistration,
};
use synaps_cli::events::{Event, EventChannel, EventContent, EventSource, Severity};
use tokio::io::AsyncWriteExt;
#[cfg(unix)]
use tokio::net::UnixStream;
use uuid::Uuid;

#[allow(clippy::too_many_arguments)]
pub async fn run(
    message: String,
    source: String,
    severity: String,
    channel: Option<String>,
    content_type: String,
    session: Option<String>,
    broadcast: bool,
    inbox: bool,
) -> anyhow::Result<()> {
    let event = build_event(message, source, severity, channel, content_type);
    let json = serde_json::to_string(&event)?;

    if broadcast {
        let sessions = list_active_sessions();
        if sessions.is_empty() {
            eprintln!("No active sessions found for broadcast.");
        }
        let mut missed = Vec::new();
        for reg in sessions {
            match send_with_retry(reg.clone(), &json, None).await {
                Ok(id) => eprintln!("Event sent to session {id} via socket."),
                Err(e) => {
                    eprintln!("Not delivered to session {}: {e}", reg.session_id);
                    missed.push(reg.session_id);
                }
            }
        }
        if !missed.is_empty() {
            anyhow::bail!("broadcast missed {} session(s): {}", missed.len(), missed.join(", "));
        }
        return Ok(());
    }

    if let Some(query) = session {
        match find_session_registration(&query) {
            Some(reg) => match send_with_retry(reg, &json, Some(&query)).await {
                Ok(id) => eprintln!("Event sent to session {id} via socket."),
                Err(e) if inbox => {
                    eprintln!("{e} — dropping to inbox (--inbox).");
                    write_inbox(&event)?;
                    eprintln!("{INBOX_NOTE}");
                }
                Err(e) => anyhow::bail!(
                    "{e}; nothing was delivered.\nAdd --inbox to leave it in the shared inbox instead."
                ),
            },
            None if inbox => {
                eprintln!("No session matching {:?} found — dropping to inbox (--inbox).", query);
                write_inbox(&event)?;
                eprintln!("{INBOX_NOTE}");
            }
            None => anyhow::bail!("{}", no_match_message(&query, &list_active_sessions())),
        }
        return Ok(());
    }

    // No flags: try to auto-resolve to exactly one active session.
    let sessions = list_active_sessions();
    match sessions.len() {
        // Nothing was addressed: if the one running session refuses, the
        // event is still untargeted mail.
        1 => match send_with_retry(sessions[0].clone(), &json, None).await {
            Ok(id) => eprintln!("Event sent to session {id} via socket."),
            Err(e) => {
                eprintln!("{e} — writing to inbox.");
                write_inbox(&event)?;
                eprintln!("{INBOX_NOTE}");
            }
        },
        _ => {
            if sessions.is_empty() {
                eprintln!("No active sessions — writing to inbox.");
            } else {
                eprintln!(
                    "{} active sessions, ambiguous — writing to inbox.",
                    sessions.len()
                );
            }
            write_inbox(&event)?;
            eprintln!("{INBOX_NOTE}");
        }
    }

    Ok(())
}

/// Printed after every deliberate inbox drop: the file is not a delivery to
/// any particular session.
const INBOX_NOTE: &str = "Note: it went to the shared inbox; only a session with an attached \
input owner takes it (exactly once). Use --session <id> to reach a detached session.";

/// The error for a `--session` that matched nothing, naming what is running
/// so a typo (or a short id suffix, which never matches) is easy to fix.
fn no_match_message(query: &str, live: &[SessionRegistration]) -> String {
    let mut msg = format!("no running session matches {query:?}; nothing was delivered.\n");
    if live.is_empty() {
        msg.push_str("No sessions are running.\n");
    } else {
        msg.push_str("Running sessions (match by full id, name, or id prefix):\n");
        for reg in live {
            match reg.name.as_deref() {
                Some(name) => msg.push_str(&format!("  {}  ({name})\n", reg.session_id)),
                None => msg.push_str(&format!("  {}\n", reg.session_id)),
            }
        }
    }
    msg.push_str("Add --inbox to leave it in the shared inbox instead.");
    msg
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn build_event(
    message: String,
    source: String,
    severity: String,
    channel: Option<String>,
    content_type: String,
) -> Event {
    Event {
        id: Uuid::new_v4().to_string(),
        timestamp: Utc::now(),
        source: EventSource {
            source_type: source.clone(),
            name: source,
            callback: None,
        },
        channel: channel.map(|c| EventChannel {
            id: c.clone(),
            name: c,
        }),
        sender: None,
        content: EventContent {
            text: message,
            content_type,
            severity: Some(Severity::from_str(&severity)),
            data: None,
            disclosure: None,
        },
        expects_response: false,
        reply_to: None,
    }
}

/// Deliver an event to exactly one running session (no inbox fallback, no
/// broadcast). Used by the watcher for targeted completion notices so that a
/// supervisor never injects turns into arbitrary interactive sessions.
pub(crate) async fn send_event_to_session(query: &str, event: &Event) -> anyhow::Result<String> {
    let reg = find_session_registration(query)
        .ok_or_else(|| anyhow::anyhow!("no running session matches {:?}", query))?;
    let json = serde_json::to_string(event)?;
    send_with_retry(reg, &json, Some(query)).await
}

/// Socket attempts for an event addressed to one session, and the waits
/// between them. A session that is streaming, or a daemon mid-reload, can
/// refuse a connection for a moment; the event must still reach that
/// session and no other.
const SOCKET_ATTEMPTS: u32 = 3;
const SOCKET_RETRY_WAITS_MS: [u64; 2] = [150, 500];
/// One attempt may not hang `send` (and the script that called it).
const SOCKET_ATTEMPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Deliver to `reg`'s socket, retrying. With `requery`, the registration is
/// looked up again before each retry: a reload gives the session a new
/// socket. Returns the session id it reached, or the last error.
async fn send_with_retry(
    mut reg: SessionRegistration,
    json: &str,
    requery: Option<&str>,
) -> anyhow::Result<String> {
    let mut last = None;
    for attempt in 0..SOCKET_ATTEMPTS {
        if attempt > 0 {
            let wait = SOCKET_RETRY_WAITS_MS[(attempt as usize - 1).min(SOCKET_RETRY_WAITS_MS.len() - 1)];
            tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
            if let Some(found) = requery.and_then(find_session_registration) {
                reg = found;
            }
        }
        let sent = tokio::time::timeout(SOCKET_ATTEMPT_TIMEOUT, send_via_socket(&reg.socket_path, json)).await;
        match sent {
            Ok(Ok(())) => return Ok(reg.session_id),
            Ok(Err(e)) => last = Some(e.to_string()),
            Err(_) => last = Some(format!("timed out after {} s", SOCKET_ATTEMPT_TIMEOUT.as_secs())),
        }
    }
    anyhow::bail!(
        "socket send to session {} failed {} times (last: {})",
        reg.session_id,
        SOCKET_ATTEMPTS,
        last.unwrap_or_default()
    )
}

async fn send_via_socket(socket_path: &str, json: &str) -> anyhow::Result<()> {
    // Validate socket_path is inside the registry dir — prevents a crafted
    // registration JSON from redirecting sends to an attacker-controlled socket.
    let sock = std::path::Path::new(socket_path);
    let run_dir = synaps_cli::events::registry::registry_dir();
    if !sock.starts_with(&run_dir) {
        anyhow::bail!(
            "socket path {:?} is outside registry dir — refusing to connect",
            socket_path
        );
    }
    let mut stream = connect_session_transport(socket_path).await?;
    stream.write_all(json.as_bytes()).await?;
    stream.shutdown().await?;
    Ok(())
}

#[cfg(unix)]
async fn connect_session_transport(socket_path: &str) -> anyhow::Result<UnixStream> {
    Ok(UnixStream::connect(socket_path).await?)
}

/// Windows: the socket_path is only an identifier — connect to the named pipe
/// derived from it (must match the engine's `pipe_name_for` derivation).
#[cfg(windows)]
async fn connect_session_transport(
    socket_path: &str,
) -> anyhow::Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    use tokio::net::windows::named_pipe::ClientOptions;
    let pipe_name = synaps_cli::events::socket::pipe_name_for(socket_path);
    Ok(ClientOptions::new().open(&pipe_name)?)
}

fn write_inbox(event: &Event) -> anyhow::Result<()> {
    let inbox_dir = synaps_cli::config::base_dir().join("inbox");
    std::fs::create_dir_all(&inbox_dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(&inbox_dir) {
            let mut perms = meta.permissions();
            perms.set_mode(0o700);
            let _ = std::fs::set_permissions(&inbox_dir, perms);
        }
    }
    let filename = format!(
        "{}-{}.json",
        Utc::now().timestamp_nanos_opt().unwrap_or(0),
        Uuid::new_v4().simple()
    );
    let path = inbox_dir.join(&filename);
    let tmp_path = inbox_dir.join(format!("{}.tmp", filename));
    std::fs::write(&tmp_path, serde_json::to_string_pretty(event)?)?;
    std::fs::rename(&tmp_path, &path)?;
    eprintln!("Event written to inbox: {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg(id: &str, name: Option<&str>) -> SessionRegistration {
        SessionRegistration {
            kind: "session".into(),
            session_id: id.into(),
            name: name.map(str::to_string),
            socket_path: format!("/tmp/{id}.sock"),
            pid: 1,
            started_at: Utc::now(),
        }
    }

    #[test]
    fn a_session_miss_names_the_running_sessions_and_the_inbox_opt_in() {
        let msg = no_match_message(
            "49f6",
            &[reg("20261014-042151-49f6", Some("ion")), reg("20261014-183459-3f61", None)],
        );
        assert!(msg.contains("\"49f6\""), "{msg}");
        assert!(msg.contains("20261014-042151-49f6  (ion)"), "{msg}");
        assert!(msg.contains("20261014-183459-3f61"), "{msg}");
        assert!(msg.contains("--inbox"), "{msg}");
    }

    #[test]
    fn a_session_miss_with_nothing_running_says_so() {
        let msg = no_match_message("x", &[]);
        assert!(msg.contains("No sessions are running."), "{msg}");
    }
}
