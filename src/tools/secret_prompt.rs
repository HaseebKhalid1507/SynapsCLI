use std::sync::{Arc, Mutex};

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
            title,
            prompt,
            response_tx,
        };
        self.tx.send(request).ok()?;
        response_rx.await.ok().flatten()
    }
}

pub struct SecretPromptRequest {
    pub title: String,
    pub prompt: String,
    pub response_tx: tokio::sync::oneshot::Sender<Option<String>>,
}

pub struct PendingSecretPrompt {
    pub title: String,
    pub prompt: String,
    pub buffer: String,
    pub response_tx: tokio::sync::oneshot::Sender<Option<String>>,
}

pub struct SecretPromptQueue {
    active: Option<PendingSecretPrompt>,
    pending: std::collections::VecDeque<SecretPromptRequest>,
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
}
