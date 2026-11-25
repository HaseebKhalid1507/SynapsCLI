//! Request-only prompt material. Never part of Runtime's ordinary prompt or history.
use std::sync::{Arc, Mutex};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

pub const MAX_BYTES: usize = 262_144;
const DEFAULT_WAIT_MS: u64 = 2_000;
pub const MAX_WAIT_MS: u64 = 15_000;

/// Transport-only payload; never retained in replay or persisted state.
#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Replace { text: String },
    Clear,
    Arm { supply_wait_ms: u64 },
}
impl std::fmt::Debug for Operation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self { Self::Replace { .. } => "Replace(<redacted>)", Self::Clear => "Clear", Self::Arm { .. } => "Arm" })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State { NeedsSupply, Ready, Cleared, Missing }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason { Installed, Cleared, Reload, Rollover, Disconnect, Timeout, NewSession, RequestSettled, Armed }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome { Applied, StaleEpoch, StaleGeneration, TooLarge, Empty, Busy, NotOwner, InvalidWait }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub epoch: String,
    pub generation: u64,
    pub state: State,
    pub reason: Reason,
    pub sequence: u64,
    pub in_flight: bool,
}
struct Inner { status: Status, text: Option<String>, active: bool, opted_in: bool, wait_ms: u64 }

/// Explicitly shared with this session's stream, never by Runtime::clone.
#[derive(Clone)]
pub struct Layer {
    inner: Arc<Mutex<Inner>>,
    changed: watch::Sender<Status>,
}
impl std::fmt::Debug for Layer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("EphemeralLayer(<redacted>)") }
}
impl Default for Layer { fn default() -> Self { Self::new(false) } }
impl Layer {
    pub fn new(opted_in: bool) -> Self {
        let status = Status { epoch: uuid::Uuid::new_v4().to_string(), generation: 0,
            state: if opted_in { State::NeedsSupply } else { State::Cleared }, reason: Reason::Reload, sequence: 0, in_flight: false };
        let (changed, _) = watch::channel(status.clone());
        Self { inner: Arc::new(Mutex::new(Inner { status, text: None, active: false, opted_in, wait_ms: DEFAULT_WAIT_MS })), changed }
    }
    pub fn configured(wait_ms: Option<u64>) -> Self {
        let layer = Self::new(wait_ms.is_some());
        if let Some(ms) = wait_ms { layer.inner.lock().unwrap().wait_ms = ms.min(MAX_WAIT_MS); }
        layer
    }
    pub fn wait_ms(&self) -> Option<u64> {
        let s = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        s.opted_in.then_some(s.wait_ms)
    }
    pub fn subscribe(&self) -> watch::Receiver<Status> { self.changed.subscribe() }
    pub fn status(&self) -> Status { self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner).status.clone() }
    pub fn opted_in(&self) -> bool { self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner).opted_in }
    pub fn update(&self, epoch: &str, generation: u64, op: Operation) -> Outcome {
        let mut s = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if epoch != s.status.epoch { return Outcome::StaleEpoch; }
        if generation <= s.status.generation { return Outcome::StaleGeneration; }
        if s.active { return Outcome::Busy; }
        let (text, state, reason) = match op {
            Operation::Arm { supply_wait_ms } if supply_wait_ms > MAX_WAIT_MS => return Outcome::InvalidWait,
            Operation::Arm { supply_wait_ms } => { s.wait_ms = supply_wait_ms; (None, State::NeedsSupply, Reason::Armed) },
            Operation::Replace { text } if text.len() > MAX_BYTES => return Outcome::TooLarge,
            Operation::Replace { text } if text.is_empty() => return Outcome::Empty,
            Operation::Replace { text } => (Some(text), State::Ready, Reason::Installed),
            Operation::Clear => (None, State::Cleared, Reason::Cleared),
        };
        s.text = text;
        s.opted_in = true;
        s.status.generation = generation;
        s.status.state = state;
        s.status.reason = reason;
        s.status.sequence += 1;
        self.changed.send_replace(s.status.clone());
        Outcome::Applied
    }
    pub fn invalidate(&self, reason: Reason) {
        let mut s = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        s.text = None;
        s.status.epoch = uuid::Uuid::new_v4().to_string();
        s.status.generation = 0;
        s.status.state = if s.opted_in { State::NeedsSupply } else { State::Cleared };
        s.status.reason = reason;
        s.status.sequence += 1;
        self.changed.send_replace(s.status.clone());
    }
    /// Admission owns the fence until all attempts of this logical request finish.
    /// Updates during that interval are refused rather than falsely acknowledged.
    pub async fn admit(&self, cancel: &CancellationToken) -> Option<Permit> {
        let wait_ms = self.wait_ms().unwrap_or(0);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(wait_ms);
        let mut changes = self.subscribe();
        loop {
            if cancel.is_cancelled() { return None; }
            {
                let mut s = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                if s.status.state == State::NeedsSupply && tokio::time::Instant::now() >= deadline {
                    s.status.state = State::Missing;
                    s.status.reason = Reason::Timeout;
                    s.status.sequence += 1;
                    self.changed.send_replace(s.status.clone());
                }
                if s.status.state != State::NeedsSupply && !s.active {
                    s.active = true;
                    s.status.in_flight = true;
                    s.status.sequence += 1;
                    self.changed.send_replace(s.status.clone());
                    return Some(Permit { layer: self.clone(), text: s.text.clone() });
                }
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return None,
                _ = changes.changed() => {},
                _ = tokio::time::sleep_until(deadline), if tokio::time::Instant::now() < deadline => {},
            }
        }
    }
}
/// Never Debug/Serialize. The only content reader is the final request composer.
pub struct Permit { layer: Layer, text: Option<String> }
impl Permit {
    pub fn has_text(&self) -> bool { self.text.is_some() }
    pub fn compose(&self, base: &Option<String>) -> Option<String> {
        match &self.text {
            Some(text) => Some(format!("{}\n\n{}", base.as_deref().unwrap_or_default(), text)),
            None => base.clone(),
        }
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        let mut s = self.layer.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        s.active = false;
        s.status.in_flight = false;
        s.status.reason = Reason::RequestSettled;
        s.status.sequence += 1;
        self.layer.changed.send_replace(s.status.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const SENTINEL: &str = "EPHEMERAL_SENTINEL_NOT_FOR_DISK";
    #[tokio::test]
    async fn ephemeral_layer_fence_generation_and_clear() {
        let layer = Layer::new(true);
        let epoch = layer.status().epoch;
        assert_eq!(layer.update(&epoch, 1, Operation::Replace { text: SENTINEL.into() }), Outcome::Applied);
        let permit = layer.admit(&CancellationToken::new()).await.unwrap();
        assert!(permit.compose(&Some("policy".into())).unwrap().ends_with(SENTINEL));
        assert_eq!(layer.update(&epoch, 2, Operation::Clear), Outcome::Busy);
        drop(permit);
        assert_eq!(layer.update(&epoch, 2, Operation::Clear), Outcome::Applied);
        assert_eq!(layer.update(&epoch, 1, Operation::Clear), Outcome::StaleGeneration);
        assert!(!layer.admit(&CancellationToken::new()).await.unwrap().has_text());
        layer.invalidate(Reason::Rollover);
        assert_ne!(layer.status().epoch, epoch);
        assert_eq!(layer.update(&epoch, 3, Operation::Clear), Outcome::StaleEpoch);
    }
    #[tokio::test]
    async fn ephemeral_layer_wait_is_cancellable_and_supply_unblocks() {
        let layer = Layer::new(true);
        let cancel = CancellationToken::new(); cancel.cancel();
        assert!(layer.admit(&cancel).await.is_none());
        let l = layer.clone();
        let task = tokio::spawn(async move { l.admit(&CancellationToken::new()).await });
        tokio::task::yield_now().await;
        assert!(!task.is_finished());
        let epoch = layer.status().epoch;
        assert_eq!(layer.update(&epoch, 1, Operation::Clear), Outcome::Applied);
        assert!(!task.await.unwrap().unwrap().has_text());
    }
    #[test]
    fn ephemeral_layer_bounds_and_clone_isolation() {
        let runtime = crate::Runtime::new_headless();
        let epoch = runtime.ephemeral_layer.status().epoch;
        assert_eq!(runtime.ephemeral_layer.update(&epoch, 1, Operation::Replace { text: "x".repeat(MAX_BYTES) }), Outcome::Applied);
        assert_eq!(runtime.ephemeral_layer.update(&epoch, 2, Operation::Replace { text: "x".repeat(MAX_BYTES+1) }), Outcome::TooLarge);
        assert_eq!(runtime.ephemeral_layer.update(&epoch, 2, Operation::Replace { text: String::new() }), Outcome::Empty);
        assert_eq!(runtime.ephemeral_layer.update(&epoch, 2, Operation::Arm { supply_wait_ms: MAX_WAIT_MS+1 }), Outcome::InvalidWait);
        let cloned = runtime.clone();
        assert!(!cloned.ephemeral_layer.opted_in());
        assert_eq!(cloned.ephemeral_layer.status().state, State::Cleared);
        assert_ne!(cloned.ephemeral_layer.status().epoch, epoch);
    }
}
