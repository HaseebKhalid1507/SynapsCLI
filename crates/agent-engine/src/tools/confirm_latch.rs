//! Per-session, per-extension "Allow all" latch for extension `Confirm`
//! results on `before_tool_call`.
//!
//! The latch is keyed by (extension id, load generation). Both are stamped
//! by `HookBus::emit`, never by the extension itself. Answering "Allow all"
//! on a prompt from extension `A` auto-approves later confirms from that
//! same loaded instance of `A` only: confirms from any other extension still
//! prompt, and so does `A` after it is unloaded, reloaded or reinstalled
//! (the generation changes). A confirm with no known source is never
//! auto-approved by the latch.
//!
//! Clones of the session runtime share the `Arc`, so a revoke applies to
//! them live. Once [`SessionConfirmLatch::revoke`] has run (conversation
//! boundary) the latch can never be armed again, so a stale clone of an old
//! conversation cannot re-arm an orphaned set nobody can see or reset. It
//! never affects `Block`/`Modify` and grants no `activate_tools` authority.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// (extension id, load generation).
pub type LatchKey = (String, u64);

/// Pair `sources` with `generations` (missing generations are 0).
pub fn latch_keys(sources: &[String], generations: &[u64]) -> Vec<LatchKey> {
    sources
        .iter()
        .enumerate()
        .map(|(i, s)| (s.clone(), generations.get(i).copied().unwrap_or(0)))
        .collect()
}

#[derive(Debug, Default)]
pub struct SessionConfirmLatch {
    allowed: Mutex<BTreeSet<LatchKey>>,
    revoked: AtomicBool,
}

impl SessionConfirmLatch {
    pub fn new() -> Self {
        Self::default()
    }

    fn set(&self) -> std::sync::MutexGuard<'_, BTreeSet<LatchKey>> {
        self.allowed.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// True iff `keys` is non-empty and every key is latched.
    pub fn allows_all(&self, keys: &[LatchKey]) -> bool {
        if keys.is_empty() || self.revoked.load(Ordering::Acquire) {
            return false;
        }
        let set = self.set();
        keys.iter().all(|k| set.contains(k))
    }

    /// Latch every key. Returns false (and latches nothing) when `keys` is
    /// empty — an unattributed confirm cannot be scoped — or the latch has
    /// been revoked for good.
    pub fn allow(&self, keys: &[LatchKey]) -> bool {
        if keys.is_empty() || self.revoked.load(Ordering::Acquire) {
            return false;
        }
        let mut set = self.set();
        for k in keys {
            set.insert(k.clone());
        }
        true
    }

    /// Revoke every latched extension; returns the revoked ids (sorted,
    /// de-duplicated). The latch stays usable (`/confirms reset`).
    pub fn clear(&self) -> Vec<String> {
        let taken = std::mem::take(&mut *self.set());
        let mut ids: Vec<String> = taken.into_iter().map(|(id, _)| id).collect();
        ids.dedup();
        ids
    }

    /// Clear and permanently disable (conversation boundary).
    pub fn revoke(&self) {
        self.revoked.store(true, Ordering::Release);
        self.clear();
    }

    /// Latched extension ids (sorted, de-duplicated).
    pub fn extensions(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.set().iter().map(|(id, _)| id.clone()).collect();
        ids.dedup();
        ids
    }

    pub fn is_empty(&self) -> bool {
        self.set().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(v: &[&str]) -> Vec<LatchKey> {
        v.iter().map(|x| (x.to_string(), 0)).collect()
    }

    #[test]
    fn latch_is_scoped_to_the_answering_extension() {
        let latch = SessionConfirmLatch::new();
        assert!(!latch.allows_all(&k(&["jev"])));
        assert!(latch.allow(&k(&["jev"])));
        assert!(latch.allows_all(&k(&["jev"])));
        assert!(
            !latch.allows_all(&k(&["other"])),
            "other extensions still prompt"
        );
        assert!(
            !latch.allows_all(&k(&["jev", "other"])),
            "a multi-source confirm needs every source latched"
        );
    }

    #[test]
    fn reinstalled_extension_under_same_id_is_not_latched() {
        let latch = SessionConfirmLatch::new();
        latch.allow(&[("jev".to_string(), 0)]);
        assert!(latch.allows_all(&[("jev".to_string(), 0)]));
        assert!(
            !latch.allows_all(&[("jev".to_string(), 1)]),
            "a new load generation of the same id must prompt again"
        );
        assert_eq!(
            latch_keys(&["a".into(), "b".into()], &[4]),
            vec![("a".to_string(), 4), ("b".to_string(), 0)]
        );
    }

    #[test]
    fn unattributed_confirm_is_never_latched() {
        let latch = SessionConfirmLatch::new();
        assert!(!latch.allow(&[]));
        assert!(latch.is_empty());
        assert!(!latch.allows_all(&[]));
    }

    #[test]
    fn clear_revokes_everything() {
        let latch = SessionConfirmLatch::new();
        latch.allow(&k(&["b", "a"]));
        assert_eq!(latch.extensions(), vec!["a".to_string(), "b".to_string()]);
        assert_eq!(latch.clear(), vec!["a".to_string(), "b".to_string()]);
        assert!(latch.is_empty());
        assert!(!latch.allows_all(&k(&["a"])));
        assert!(latch.allow(&k(&["a"])), "reset latch can be armed again");
    }

    #[test]
    fn revoked_latch_can_never_be_rearmed() {
        let latch = SessionConfirmLatch::new();
        latch.allow(&k(&["a"]));
        latch.revoke();
        assert!(latch.is_empty());
        assert!(!latch.allow(&k(&["a"])), "stale clone cannot re-arm");
        assert!(!latch.allows_all(&k(&["a"])));
    }
}
