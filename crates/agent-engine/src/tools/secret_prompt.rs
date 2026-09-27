use std::sync::{Arc, Mutex};

pub use crate::session::PromptKind;

/// UI-only secret prompt plumbing for interactive tools.
///
/// Secrets sent through this channel are never part of tool parameters, tool
/// results, chat messages, or API messages. The TUI owns the input UI and sends
/// only the final secret bytes back to the waiting tool.
#[derive(Clone)]
pub struct SecretPromptHandle {
    tx: tokio::sync::mpsc::UnboundedSender<SecretPromptRequest>,
}

impl SecretPromptHandle {
    pub fn new(tx: tokio::sync::mpsc::UnboundedSender<SecretPromptRequest>) -> Self {
        Self { tx }
    }

    pub async fn prompt(&self, title: String, prompt: String) -> Option<String> {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        let request = SecretPromptRequest {
            kind: PromptKind::from_title(&title),
            title,
            prompt,
            response_tx,
        };
        self.tx.send(request).ok()?;
        response_rx.await.ok().flatten()
    }
}

pub struct SecretPromptRequest {
    /// `Confirm` renders as a y/n dialog (body visible), `Secret` as a masked
    /// field. Derived from the title by `SecretPromptHandle::prompt`; carried
    /// verbatim from the wire `PromptRequest` by the daemon PromptBridge.
    pub kind: PromptKind,
    pub title: String,
    pub prompt: String,
    pub response_tx: tokio::sync::oneshot::Sender<Option<String>>,
}

pub struct PendingSecretPrompt {
    pub kind: PromptKind,
    pub title: String,
    pub prompt: String,
    pub buffer: String,
    pub response_tx: tokio::sync::oneshot::Sender<Option<String>>,
}

pub struct SecretPromptQueue {
    active: Option<PendingSecretPrompt>,
    pending: std::collections::VecDeque<SecretPromptRequest>,
}

impl Default for SecretPromptQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl SecretPromptQueue {
    pub fn new() -> Self {
        Self {
            active: None,
            pending: std::collections::VecDeque::new(),
        }
    }

    pub fn poll_requests(
        &mut self,
        rx: &Arc<Mutex<tokio::sync::mpsc::UnboundedReceiver<SecretPromptRequest>>>,
    ) {
        if let Ok(mut rx) = rx.lock() {
            while let Ok(req) = rx.try_recv() {
                self.pending.push_back(req);
            }
        }
        self.activate_next();
    }

    fn activate_next(&mut self) {
        if self.active.is_some() {
            return;
        }
        if let Some(req) = self.pending.pop_front() {
            self.active = Some(PendingSecretPrompt {
                kind: req.kind,
                title: req.title,
                prompt: req.prompt,
                buffer: String::new(),
                response_tx: req.response_tx,
            });
        }
    }

    pub fn is_active(&self) -> bool {
        self.active.is_some()
    }

    pub fn active(&self) -> Option<&PendingSecretPrompt> {
        self.active.as_ref()
    }

    pub fn push_char(&mut self, ch: char) {
        if let Some(active) = self.active.as_mut() {
            active.buffer.push(ch);
        }
    }

    pub fn backspace(&mut self) {
        if let Some(active) = self.active.as_mut() {
            active.buffer.pop();
        }
    }

    pub fn submit(&mut self) {
        if let Some(mut active) = self.active.take() {
            let secret = std::mem::take(&mut active.buffer);
            let _ = active.response_tx.send(Some(secret));
        }
        self.activate_next();
    }

    pub fn cancel(&mut self) {
        if let Some(mut active) = self.active.take() {
            active.buffer.clear();
            let _ = active.response_tx.send(None);
        }
        self.activate_next();
    }

    /// Drop the active prompt WITHOUT answering (the oneshot is dropped, not
    /// sent): another client resolved it (`PromptResolved` for a prompt this
    /// client did not answer). The next pending prompt activates.
    pub fn dismiss(&mut self) {
        if let Some(mut active) = self.active.take() {
            active.buffer.clear();
            drop(active.response_tx);
        }
        self.activate_next();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dismiss_drops_without_answering_and_activates_next() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let rx = Arc::new(Mutex::new(rx));
        let (tx1, mut rx1) = tokio::sync::oneshot::channel();
        let (tx2, mut rx2) = tokio::sync::oneshot::channel();
        tx.send(SecretPromptRequest { kind: PromptKind::Secret, title: "a".into(), prompt: "p".into(), response_tx: tx1 }).unwrap();
        tx.send(SecretPromptRequest { kind: PromptKind::Secret, title: "b".into(), prompt: "p".into(), response_tx: tx2 }).unwrap();
        let mut q = SecretPromptQueue::new();
        q.poll_requests(&rx);
        assert_eq!(q.active().unwrap().title, "a");
        q.push_char('z');
        q.dismiss();
        // Dropped, never sent: the waiter sees a closed channel (== cancelled).
        assert!(matches!(rx1.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Closed)));
        // The next prompt is active with a fresh buffer.
        assert_eq!(q.active().unwrap().title, "b");
        assert_eq!(q.active().unwrap().buffer, "");
        q.push_char('x');
        q.submit();
        assert_eq!(rx2.try_recv().unwrap(), Some("x".to_string()));
        assert!(!q.is_active());
    }
}
