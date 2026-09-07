//! Per-session in-process save ordering. The guard lives in the blocking
//! writer, not its awaiting frontend: dropping a timed-out save future must
//! never let that old writer publish over a subsequent durable checkpoint.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

type SaveLocks = HashMap<(PathBuf, String), Weak<AsyncMutex<()>>>;

pub(crate) async fn acquire(dir: &Path, id: &str) -> OwnedMutexGuard<()> {
    static LOCKS: OnceLock<Mutex<SaveLocks>> = OnceLock::new();
    let lock = {
        let mut locks = LOCKS
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        locks.retain(|_, lock| lock.strong_count() != 0);
        let key = (dir.to_path_buf(), id.to_owned());
        if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
            lock
        } else {
            let lock = Arc::new(AsyncMutex::new(()));
            locks.insert(key, Arc::downgrade(&lock));
            lock
        }
    };
    lock.lock_owned().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn detached_writer_keeps_save_order_until_blocking_io_finishes() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let guard = acquire(&dir, "session").await;
        let (release, wait) = std::sync::mpsc::channel();
        let (started, ready) = tokio::sync::oneshot::channel();
        let writer = tokio::task::spawn_blocking(move || {
            let _guard = guard;
            started.send(()).unwrap();
            wait.recv().unwrap();
        });
        ready.await.unwrap();
        drop(writer); // same detached-I/O behavior as a timed-out Session::save
        let next = tokio::spawn(async move { acquire(&dir, "session").await });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(!next.is_finished());
        release.send(()).unwrap();
        let _guard = tokio::time::timeout(std::time::Duration::from_secs(2), next)
            .await
            .unwrap()
            .unwrap();
    }
}
