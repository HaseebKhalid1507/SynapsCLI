//! `synaps send` — deliver an event via Unix socket, falling back to inbox file-drop.

use synaps_cli::events::{Event, EventChannel, EventContent, EventSource, Severity};
use synaps_cli::events::registry::{find_session_registration, list_active_sessions, SessionRegistration};
use chrono::Utc;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use uuid::Uuid;

pub async fn run(
    message: String,
    source: String,
    severity: String,
    channel: Option<String>,
    content_type: String,
    session: Option<String>,
    broadcast: bool,
) -> anyhow::Result<()> {
    let event = build_event(message, source, severity, channel, content_type);
    let json = serde_json::to_string(&event)?;

    if broadcast {
        let sessions = list_active_sessions();
        if sessions.is_empty() {
            eprintln!("No active sessions found for broadcast.");
        }
        for reg in sessions {
            send_to_socket_or_inbox(&reg, &json, &event).await;
        }
        return Ok(());
    }

    if let Some(query) = session {
        match find_session_registration(&query) {
            Some(reg) => send_to_socket_or_inbox(&reg, &json, &event).await,
            None => {
                eprintln!("No session matching {:?} found — dropping to inbox.", query);
                write_inbox(&event)?;
            }
        }
        return Ok(());
    }

    // No flags: try to auto-resolve to exactly one active session.
    let sessions = list_active_sessions();
    match sessions.len() {
        1 => send_to_socket_or_inbox(&sessions[0], &json, &event).await,
        _ => {
            if sessions.is_empty() {
                eprintln!("No active sessions — writing to inbox.");
            } else {
                eprintln!("{} active sessions, ambiguous — writing to inbox.", sessions.len());
            }
            write_inbox(&event)?;
        }
    }

    Ok(())
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
        channel: channel.map(|c| EventChannel { id: c.clone(), name: c }),
        sender: None,
        content: EventContent {
            text: message,
            content_type,
            severity: Some(Severity::from_str(&severity)),
            data: None,
        },
        expects_response: false,
        reply_to: None,
    }
}

async fn send_to_socket_or_inbox(reg: &SessionRegistration, json: &str, event: &Event) {
    match send_via_socket(&reg.socket_path, json).await {
        Ok(()) => eprintln!("Event sent to session {} via socket.", reg.session_id),
        Err(e) => {
            eprintln!(
                "Warning: socket send to session {} failed ({}), falling back to inbox.",
                reg.session_id, e
            );
            if let Err(ie) = write_inbox(event) {
                eprintln!("Inbox fallback also failed: {}", ie);
            }
        }
    }
}

async fn send_via_socket(socket_path: &str, json: &str) -> anyhow::Result<()> {
    let mut stream = UnixStream::connect(socket_path).await?;
    stream.write_all(json.as_bytes()).await?;
    stream.shutdown().await?;
    Ok(())
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
