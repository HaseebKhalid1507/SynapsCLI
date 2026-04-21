//! Inbox watcher — polls `~/.synaps-cli/inbox/` for dropped event JSON files
//! and pushes parsed events into the EventQueue.

use std::path::PathBuf;
use std::sync::Arc;

use super::queue::EventQueue;
use super::types::Event;

/// Watch the inbox directory for new .json files. When one appears,
/// parse it as an Event, push to the queue, and delete the file.
/// Runs as a background tokio task.
pub async fn watch_inbox(inbox_dir: PathBuf, queue: Arc<EventQueue>) {
    let _ = tokio::fs::create_dir_all(&inbox_dir).await;

    loop {
        if let Ok(mut entries) = tokio::fs::read_dir(&inbox_dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "json") {
                    match tokio::fs::read_to_string(&path).await {
                        Ok(content) => match serde_json::from_str::<Event>(&content) {
                            Ok(event) => {
                                tracing::info!(
                                    "Inbox event: {} from {}",
                                    event.id,
                                    event.source.source_type
                                );
                                match queue.push(event) {
                                    Ok(()) => {
                                        let _ = tokio::fs::remove_file(&path).await;
                                    }
                                    Err(e) => {
                                        tracing::warn!("Event queue full, leaving file for retry: {}", e);
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "Invalid event file {}: {}",
                                    path.display(),
                                    e
                                );
                                let error_path = path.with_extension("json.error");
                                let _ = tokio::fs::rename(&path, &error_path).await;
                            }
                        },
                        Err(e) => {
                            tracing::warn!("Failed to read {}: {}", path.display(), e);
                        }
                    }
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{Event, Severity};

    #[tokio::test]
    async fn picks_up_dropped_event() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = dir.path().to_path_buf();
        let queue = Arc::new(EventQueue::new(10));

        let q = queue.clone();
        let ibx = inbox.clone();
        let handle = tokio::spawn(async move { watch_inbox(ibx, q).await });

        let event = Event::simple("test", "hello inbox", Some(Severity::High));
        let path = inbox.join("1.json");
        tokio::fs::write(&path, serde_json::to_string(&event).unwrap())
            .await
            .unwrap();

        // Wait up to 2s for pickup
        for _ in 0..20 {
            if queue.len() > 0 { break; }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        handle.abort();

        let popped = queue.pop().expect("event should have been ingested");
        assert_eq!(popped.content.text, "hello inbox");
        assert!(!path.exists(), "file should have been deleted after ingest");
    }

    #[tokio::test]
    async fn invalid_json_moved_to_error() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = dir.path().to_path_buf();
        let queue = Arc::new(EventQueue::new(10));

        let q = queue.clone();
        let ibx = inbox.clone();
        let handle = tokio::spawn(async move { watch_inbox(ibx, q).await });

        let path = inbox.join("bad.json");
        tokio::fs::write(&path, "not valid json").await.unwrap();

        let err_path = inbox.join("bad.json.error");
        for _ in 0..20 {
            if err_path.exists() { break; }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        handle.abort();

        assert!(err_path.exists(), "bad file should be renamed to .error");
        assert_eq!(queue.len(), 0);
    }
}
