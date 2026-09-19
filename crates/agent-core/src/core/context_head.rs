//! Frontend acknowledgement for a context-head persistence barrier.
//!
//! Cloneable because stream events are cloneable, but completion is single-use.
//! Dropping every copy without acknowledgement closes the receiver: consumers
//! without durable session persistence fail closed rather than permit inference.
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

type HeadResult = Result<(), String>;
type HeadSender = Arc<Mutex<Option<oneshot::Sender<HeadResult>>>>;

#[derive(Clone, Debug)]
pub struct ContextHeadReceipt {
    sender: HeadSender,
}

impl ContextHeadReceipt {
    pub fn channel() -> (Self, oneshot::Receiver<Result<(), String>>) {
        let (sender, receiver) = oneshot::channel();
        (
            Self {
                sender: Arc::new(Mutex::new(Some(sender))),
            },
            receiver,
        )
    }

    /// Acknowledge only after the candidate session head is durably saved.
    /// An I/O error may be post-rename: it stops inference, not promises rollback.
    pub fn complete(&self, result: std::io::Result<()>) {
        if let Some(sender) = self
            .sender
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = sender.send(result.map_err(|error| error.to_string()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn receipt_is_single_use_across_clones() {
        let (receipt, result) = ContextHeadReceipt::channel();
        receipt.clone().complete(Ok(()));
        receipt.complete(Err(std::io::Error::other("late error")));
        assert_eq!(result.await.unwrap(), Ok(()));
    }

    #[tokio::test]
    async fn unhandled_receipt_fails_closed() {
        let (receipt, result) = ContextHeadReceipt::channel();
        drop(receipt);
        assert!(result.await.is_err());
    }
}
