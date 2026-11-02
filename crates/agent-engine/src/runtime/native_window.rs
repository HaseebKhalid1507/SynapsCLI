//! The 1M entitlement fallback.
//!
//! `context_window = 1m` makes a 1M-capable Anthropic model send the
//! `context-1m-2025-08-07` beta and budget for a 1M window. Nothing checks
//! that the account is entitled to it: once a request's input passes the
//! native 200k window, an account that isn't gets refused. When a request
//! that carried the beta is refused with a long-context class
//! ([`classify_long_context_refusal`](crate::core::error::classify_long_context_refusal))
//! while its input was over the native window, the stream turn latches the
//! session's model to its native window here, rolls the context over sized
//! for that window (cold summary only) and retries the request once.
//!
//! The latch is runtime state only: it is never written to config, and
//! `context_window_override` keeps the configured value. It is held per
//! (session, model): runtime clones are the same session and share it,
//! subagents (fresh runtimes) start without it, and a switch to another
//! model starts unlatched while switching back keeps the latch. A model is
//! latched at most once, so a later refusal at the native window ends the
//! turn as any other failed request does.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use crate::core::error::LongContextRefusal;

/// A long-context refusal of one request: the HTTP status and the vetted
/// class, never the provider's body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LongContextFailure {
    pub(crate) status: u16,
    pub(crate) class: LongContextRefusal,
}

/// Where the transport reports a long-context refusal for the stream turn
/// that installed it (see `ApiOptions::long_context_refusal`).
#[derive(Debug, Clone, Default)]
pub(crate) struct LongContextSlot(Arc<Mutex<Option<LongContextFailure>>>);

impl LongContextSlot {
    /// Record the refusal of the request that just failed.
    pub(crate) fn report(&self, failure: LongContextFailure) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(failure);
    }

    /// The refusal reported since the last take, clearing it.
    pub(crate) fn take(&self) -> Option<LongContextFailure> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

/// The session's models latched to their native window.
#[derive(Debug, Default)]
pub(crate) struct NativeWindowLatch {
    models: Mutex<HashSet<String>>,
}

impl NativeWindowLatch {
    fn key(model: &str) -> &str {
        model.strip_prefix("anthropic/").unwrap_or(model)
    }

    /// Whether `model` was latched to its native window in this session.
    pub(crate) fn is_latched(&self, model: &str) -> bool {
        self.models
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(Self::key(model))
    }

    /// Latch `model` to its native window. True only the first time, so
    /// one session falls back at most once per model.
    pub(crate) fn latch(&self, model: &str) -> bool {
        self.models
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(Self::key(model).to_string())
    }

    /// `window`, held to `model`'s native window when the model is latched.
    pub(crate) fn clamp(&self, model: &str, window: u64) -> u64 {
        if self.is_latched(model) {
            window.min(crate::core::models::context_window_for_model(model))
        } else {
            window
        }
    }
}

/// The one notice shown when the session falls back to the native window.
pub(crate) fn fallback_notice(native: u64) -> String {
    format!(
        "This account didn't accept the 1M context window, so this session continues at the \
         {}k native window.",
        native / 1_000
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latch_is_once_per_model_and_ignores_the_provider_prefix() {
        let latch = NativeWindowLatch::default();
        assert!(!latch.is_latched("claude-sonnet-4-6"));
        assert_eq!(latch.clamp("claude-sonnet-4-6", 1_000_000), 1_000_000);
        assert!(latch.latch("claude-sonnet-4-6"));
        assert!(!latch.latch("anthropic/claude-sonnet-4-6"));
        assert!(latch.is_latched("anthropic/claude-sonnet-4-6"));
        assert_eq!(latch.clamp("claude-sonnet-4-6", 1_000_000), 200_000);
        assert!(!latch.is_latched("claude-opus-4-6"));
        assert_eq!(latch.clamp("claude-opus-4-6", 1_000_000), 1_000_000);
    }

    #[test]
    fn slot_hands_over_one_refusal() {
        let slot = LongContextSlot::default();
        assert_eq!(slot.take(), None);
        slot.clone().report(LongContextFailure {
            status: 400,
            class: LongContextRefusal::PromptTooLong,
        });
        assert_eq!(
            slot.take(),
            Some(LongContextFailure {
                status: 400,
                class: LongContextRefusal::PromptTooLong,
            })
        );
        assert_eq!(slot.take(), None);
    }
}
