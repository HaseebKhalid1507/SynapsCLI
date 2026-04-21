//! `synaps send` — drop an event into the inbox for a running session to pick up.

use synaps_cli::events::{Event, EventChannel, EventContent, EventSource, Severity};
use chrono::Utc;
use uuid::Uuid;

pub async fn run(
    message: String,
    source: String,
    severity: String,
    channel: Option<String>,
    content_type: String,
) -> anyhow::Result<()> {
    let event = Event {
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
    };

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
    let filename = format!("{}-{}.json", Utc::now().timestamp_nanos_opt().unwrap_or(0), Uuid::new_v4().simple());
    let path = inbox_dir.join(&filename);
    let tmp_path = inbox_dir.join(format!("{}.tmp", filename));
    std::fs::write(&tmp_path, serde_json::to_string_pretty(&event)?)?;
    std::fs::rename(&tmp_path, &path)?;
    eprintln!("Event sent to inbox: {}", path.display());
    Ok(())
}
