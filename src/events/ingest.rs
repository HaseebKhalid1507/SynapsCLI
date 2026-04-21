//! Inbox watcher — uses inotify (via the `notify` crate) to instantly react
//! to dropped event JSON files in the inbox directory. Falls back to polling.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::queue::EventQueue;
use super::types::Event;
use notify::Watcher;

async fn process_file(path: &Path, queue: &EventQueue) {
    #[cfg(unix)]
    {
        if let Ok(meta) = tokio::fs::symlink_metadata(path).await {
            if meta.file_type().is_symlink() {
                tracing::warn!("refusing symlink in inbox: {}", path.display());
                let _ = tokio::fs::remove_file(path).await;
                return;
            }
        }
    }
    if path.extension().is_some_and(|e| e == "json") {
        if let Ok(meta) = tokio::fs::metadata(path).await {
            if meta.len() > 256 * 1024 {
                tracing::warn!("inbox file too large ({}B), skipping: {}", meta.len(), path.display());
                let _ = tokio::fs::rename(path, path.with_extension("json.oversized")).await;
                return;
            }
        }
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
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = tokio::fs::metadata(&inbox_dir).await {
            let mut perms = meta.permissions();
            perms.set_mode(0o700);
            let _ = tokio::fs::set_permissions(&inbox_dir, perms).await;
        }
    }
    scan_inbox(&inbox_dir, &queue).await;

    // Use a tokio channel so the async runtime isn't blocked
    let (async_tx, mut async_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<PathBuf>>();

    // Spawn the blocking notify watcher on a dedicated thread
    let inbox_clone = inbox_dir.clone();
    let watcher_handle = tokio::task::spawn_blocking(move || {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut _watcher: notify::RecommendedWatcher = match notify::RecommendedWatcher::new(
            tx,
            notify::Config::default(),
        ) {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!("inotify unavailable: {}", e);
                return;
            }
        };

        if let Err(e) = _watcher.watch(&inbox_clone, notify::RecursiveMode::NonRecursive) {
            tracing::warn!("watch failed: {}", e);
            return;
        }
        tracing::info!("Inbox watcher (inotify) on {}", inbox_clone.display());

        // Blocking loop on its own thread — doesn't touch tokio
        loop {
            match rx.recv() {
                Ok(Ok(event)) => {
                    if !event.paths.is_empty() {
                        let _ = async_tx.send(event.paths);
                    }
                }
                Ok(Err(e)) => tracing::warn!("notify error: {}", e),
                Err(_) => {
                    tracing::error!("notify disconnected");
                    break;
                }
            }
        }
    });

    // Async loop — receives paths from the blocking watcher thread
    let queue_ref = &queue;
    let dir_ref = &inbox_dir;
    loop {
        tokio::select! {
            Some(paths) = async_rx.recv() => {
                for path in &paths {
                    process_file(path, queue_ref).await;
                }
                // Sweep for any files inotify missed in the batch
                scan_inbox(dir_ref, queue_ref).await;
            }
            // Safety scan every 2s in case inotify misses something
            _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {
                scan_inbox(dir_ref, queue_ref).await;
            }
        }
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
