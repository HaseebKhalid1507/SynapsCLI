use super::types::{Event, Severity};
use std::collections::VecDeque;
use std::sync::Mutex;

pub struct EventQueue {
    inner: Mutex<VecDeque<Event>>,
    capacity: usize,
    notify: tokio::sync::Notify,
}

impl EventQueue {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(VecDeque::with_capacity(capacity)),
            capacity,
            notify: tokio::sync::Notify::new(),
        }
    }

    /// Push an event. Critical goes to front, High goes after existing Criticals,
    /// everything else goes to the back. Returns Err if queue is full.
    pub fn push(&self, event: Event) -> Result<(), String> {
        let mut q = self.inner.lock().unwrap();
        if q.len() >= self.capacity {
            return Err(format!("event queue full (capacity {})", self.capacity));
        }
        let sev = event.content.severity.clone();
        match sev {
            Some(Severity::Critical) => q.push_front(event),
            Some(Severity::High) => {
                // insert after trailing Critical events at the front
                let mut idx = 0;
                while idx < q.len()
                    && matches!(
                        q[idx].content.severity,
                        Some(Severity::Critical) | Some(Severity::High)
                    )
                {
                    idx += 1;
                }
                q.insert(idx, event);
            }
            _ => q.push_back(event),
        }
        drop(q);
        self.notify.notify_one();
        Ok(())
    }

    /// Force push to front regardless of severity. Evicts the oldest (back) if full.
    pub fn push_priority(&self, event: Event) {
        let mut q = self.inner.lock().unwrap();
        if q.len() >= self.capacity {
            if let Some(evicted) = q.back() {
                tracing::warn!("event queue full — evicting event id={}", evicted.id);
            }
            q.pop_back();
        }
        q.push_front(event);
        drop(q);
        self.notify.notify_one();
    }

    pub fn pop(&self) -> Option<Event> {
        self.inner.lock().unwrap().pop_front()
    }

    pub fn peek(&self) -> Option<Event> {
        self.inner.lock().unwrap().front().cloned()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    /// Wait until an event is pushed. Use in tokio::select! for instant wake.
    pub fn notified(&self) -> impl std::future::Future<Output = ()> + '_ {
        self.notify.notified()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().unwrap().is_empty()
    }

    pub fn drain(&self) -> Vec<Event> {
        let mut q = self.inner.lock().unwrap();
        q.drain(..).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(text: &str, sev: Option<Severity>) -> Event {
        Event::simple("test", text, sev)
    }

    #[test]
    fn push_pop_fifo_for_medium() {
        let q = EventQueue::new(10);
        q.push(ev("a", Some(Severity::Medium))).unwrap();
        q.push(ev("b", Some(Severity::Low))).unwrap();
        q.push(ev("c", None)).unwrap();
        assert_eq!(q.pop().unwrap().content.text, "a");
        assert_eq!(q.pop().unwrap().content.text, "b");
        assert_eq!(q.pop().unwrap().content.text, "c");
        assert!(q.is_empty());
    }

    #[test]
    fn critical_jumps_to_front() {
        let q = EventQueue::new(10);
        q.push(ev("a", Some(Severity::Medium))).unwrap();
        q.push(ev("b", Some(Severity::Medium))).unwrap();
        q.push(ev("CRIT", Some(Severity::Critical))).unwrap();
        assert_eq!(q.pop().unwrap().content.text, "CRIT");
        assert_eq!(q.pop().unwrap().content.text, "a");
    }

    #[test]
    fn high_sits_after_critical() {
        let q = EventQueue::new(10);
        q.push(ev("med", Some(Severity::Medium))).unwrap();
        q.push(ev("c1", Some(Severity::Critical))).unwrap();
        q.push(ev("c2", Some(Severity::Critical))).unwrap();
        q.push(ev("high", Some(Severity::High))).unwrap();
        // Order should be: c2, c1, high, med
        assert_eq!(q.pop().unwrap().content.text, "c2");
        assert_eq!(q.pop().unwrap().content.text, "c1");
        assert_eq!(q.pop().unwrap().content.text, "high");
        assert_eq!(q.pop().unwrap().content.text, "med");
    }

    #[test]
    fn capacity_limit() {
        let q = EventQueue::new(2);
        q.push(ev("a", None)).unwrap();
        q.push(ev("b", None)).unwrap();
        assert!(q.push(ev("c", None)).is_err());
    }

    #[test]
    fn drain_takes_all() {
        let q = EventQueue::new(10);
        q.push(ev("a", None)).unwrap();
        q.push(ev("b", None)).unwrap();
        let all = q.drain();
        assert_eq!(all.len(), 2);
        assert!(q.is_empty());
    }

    #[test]
    fn peek_does_not_remove() {
        let q = EventQueue::new(10);
        q.push(ev("a", None)).unwrap();
        assert_eq!(q.peek().unwrap().content.text, "a");
        assert_eq!(q.len(), 1);
    }
}
