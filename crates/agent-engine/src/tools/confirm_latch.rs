//! Per-session, per-extension "Allow all" latch for extension `Confirm`
//! results on `before_tool_call`.
//!
//! The latch is keyed by the extension id that returned `Confirm` (stamped by
//! `HookBus::emit`, never by the extension itself). Answering "Allow all" on
//! a prompt from extension `A` auto-approves later confirms from `A` only;
//! confirms from any other extension still prompt. A confirm with no known
//! source is never auto-approved by the latch.
//!
//! Workers share the parent's `Arc` so the scope (and a revoke) applies to
//! them live. It never affects `Block`/`Modify` and grants no
//! `activate_tools` authority.

use std::collections::BTreeSet;
use std::sync::Mutex;

#[derive(Debug, Default)]
pub struct SessionConfirmLatch {
    allowed: Mutex<BTreeSet<String>>,
}

impl SessionConfirmLatch {
    pub fn new() -> Self {
        Self::default()
    }

    fn set(&self) -> std::sync::MutexGuard<'_, BTreeSet<String>> {
        self.allowed.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// True iff `sources` is non-empty and every source extension is latched.
    pub fn allows_all(&self, sources: &[String]) -> bool {
        if sources.is_empty() {
            return false;
        }
        let set = self.set();
        sources.iter().all(|s| set.contains(s))
    }

    /// Latch every extension in `sources`. Returns false (and latches nothing)
    /// when `sources` is empty — an unattributed confirm cannot be scoped.
    pub fn allow(&self, sources: &[String]) -> bool {
        if sources.is_empty() {
            return false;
        }
        let mut set = self.set();
        for s in sources {
            set.insert(s.clone());
        }
        true
    }

    /// Revoke every latched extension; returns what was revoked (sorted).
    pub fn clear(&self) -> Vec<String> {
        std::mem::take(&mut *self.set()).into_iter().collect()
    }

    /// Latched extension ids (sorted).
    pub fn extensions(&self) -> Vec<String> {
        self.set().iter().cloned().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.set().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn latch_is_scoped_to_the_answering_extension() {
        let latch = SessionConfirmLatch::new();
        assert!(!latch.allows_all(&s(&["jev"])));
        assert!(latch.allow(&s(&["jev"])));
        assert!(latch.allows_all(&s(&["jev"])));
        assert!(!latch.allows_all(&s(&["other"])), "other extensions still prompt");
        assert!(
            !latch.allows_all(&s(&["jev", "other"])),
            "a multi-source confirm needs every source latched"
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
        latch.allow(&s(&["b", "a"]));
        assert_eq!(latch.extensions(), s(&["a", "b"]));
        assert_eq!(latch.clear(), s(&["a", "b"]));
        assert!(latch.is_empty());
        assert!(!latch.allows_all(&s(&["a"])));
    }
}
