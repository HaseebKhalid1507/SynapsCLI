//! Inbox watcher — uses inotify (via the `notify` crate) to instantly react
//! to dropped event JSON files in the inbox directory. Falls back to polling.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::queue::EventQueue;
use super::types::Event;
use notify::Watcher;

async fn process_file(path: &Path, queue: &EventQueue) {
    if path.extension().is_some_and(|e| e == "json") {
        match tokio::fs::read_to_string(path).await {
            Ok(content) => match serde_json::from_str::<Event>(&content) {
                Ok(event) => {
                    tracing::info!("Inbox event: {} from {}", event.id, event.source.source_type);
                    match queue.push(event) {
                        Ok(()) => { let _ = tokio::fs::remove_file(path).await; }
                        Err(e) => { tracing::warn!("Queue full, retry later: {}", e); }
                    }
                }
                Err(e) => {
                    tracing::warn!("Invalid event {}: {}", path.display(), e);
                    let _ = tokio::fs::rename(path, path.with_extension("json.error")).await;
                }
            },
            Err(e) => tracing::warn!("Read failed {}: {}", path.display(), e),
        }
    }
}

async fn scan_inbox(dir: &Path, queue: &EventQueue) {
    if let Ok(mut entries) = tokio::fs::read_dir(dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            process_file(&entry.path(), queue).await;
        }
    }
}

pub async fn watch_inbox(inbox_dir: PathBuf, queue: Arc<EventQueue>) {
    let _ = tokio::fs::create_dir_all(&inbox_dir).await;
    scan_inbox(&inbox_dir, &queue).await;

    // Set up inotify watcher (same pattern as watcher/supervisor.rs)
    let (tx, rx) = std::sync::mpsc::channel();
    let mut notify_watcher: notify::RecommendedWatcher = match notify::RecommendedWatcher::new(
        tx,
        notify::Config::default(),
    ) {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!("inotify unavailable ({}), polling fallback", e);
            poll_loop(&inbox_dir, &queue).await;
            return;
        }
    };

    if let Err(e) = notify_watcher.watch(&inbox_dir, notify::RecursiveMode::NonRecursive) {
        tracing::warn!("watch failed ({}), polling fallback", e);
        poll_loop(&inbox_dir, &queue).await;
        return;
    }
    tracing::info!("Inbox watcher (inotify) on {}", inbox_dir.display());

    // Keep watcher alive, process events
    loop {
        match rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(Ok(event)) => {
                for path in &event.paths {
                    process_file(path, &queue).await;
                }
            }
            Ok(Err(e)) => tracing::warn!("notify error: {}", e),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                scan_inbox(&inbox_dir, &queue).await; // safety scan
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                tracing::error!("notify disconnected, switching to polling");
                poll_loop(&inbox_dir, &queue).await;
                return;
            }
        }
    }
}

async fn poll_loop(dir: &Path, queue: &EventQueue) {
    loop {
        scan_inbox(dir, queue).await;
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

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let event = Event::simple("test", "hello inbox", Some(Severity::High));
        let path = inbox.join("1.json");
        tokio::fs::write(&path, serde_json::to_string(&event).unwrap()).await.unwrap();

        for _ in 0..30 {
            if queue.len() > 0 { break; }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        handle.abort();
        let popped = queue.pop().expect("event should have been ingested");
        assert_eq!(popped.content.text, "hello inbox");
    }

    #[tokio::test]
    async fn invalid_json_moved_to_error() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = dir.path().to_path_buf();
        let queue = Arc::new(EventQueue::new(10));
        let q = queue.clone();
        let ibx = inbox.clone();
        let handle = tokio::spawn(async move { watch_inbox(ibx, q).await });

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let path = inbox.join("bad.json");
        tokio::fs::write(&path, "not json").await.unwrap();
        let err_path = inbox.join("bad.json.error");
        for _ in 0..30 {
            if err_path.exists() { break; }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        handle.abort();
        assert!(err_path.exists());
        assert_eq!(queue.len(), 0);
    }
}
