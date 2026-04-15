use tokio::sync::{broadcast, mpsc};
use super::{AgentEvent, Inbound, SyncState, Transport};

/// Clone-able handle to an AgentBus. Supports subscribing to events,
/// sending inbound messages, and connecting transports — without
/// owning the inbound receiver (the driver owns that).
#[derive(Clone)]
pub struct BusHandle {
    event_tx: broadcast::Sender<AgentEvent>,
    inbound_tx: mpsc::UnboundedSender<Inbound>,
}

impl BusHandle {
    pub fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.event_tx.subscribe()
    }

    pub fn inbound(&self) -> mpsc::UnboundedSender<Inbound> {
        self.inbound_tx.clone()
    }

    pub fn broadcast(&self, event: AgentEvent) {
        let _ = self.event_tx.send(event);
    }

    pub fn subscriber_count(&self) -> usize {
        self.event_tx.receiver_count()
    }

    pub fn connect<T: Transport>(&self, mut transport: T, sync: SyncState) {
        let mut event_rx = self.subscribe();
        let inbound_tx = self.inbound();

        tokio::spawn(async move {
            transport.on_sync(sync).await;
            loop {
                tokio::select! {
                    event = event_rx.recv() => {
                        match event {
                            Ok(e) => { if !transport.send(e).await { break; } }
                            Err(_) => break,
                        }
                    }
                    inbound = transport.recv() => {
                        match inbound {
                            Some(msg) => { if inbound_tx.send(msg).is_err() { break; } }
                            None => break,
                        }
                    }
                }
            }
        });
    }
}

pub struct AgentBus {
    event_tx: broadcast::Sender<AgentEvent>,
    inbound_tx: mpsc::UnboundedSender<Inbound>,
    inbound_rx: Option<mpsc::UnboundedReceiver<Inbound>>,
}

impl Default for AgentBus {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentBus {
    pub fn new() -> Self {
        let (event_tx, _) = broadcast::channel(256);
        let (inbound_tx, inbound_rx) = mpsc::unbounded_channel();
        
        Self {
            event_tx,
            inbound_tx,
            inbound_rx: Some(inbound_rx),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.event_tx.subscribe()
    }

    pub fn inbound(&self) -> mpsc::UnboundedSender<Inbound> {
        self.inbound_tx.clone()
    }

    pub fn broadcast(&self, event: AgentEvent) {
        let _ = self.event_tx.send(event);
    }

    pub fn take_inbound_rx(&mut self) -> Option<mpsc::UnboundedReceiver<Inbound>> {
        self.inbound_rx.take()
    }

    pub fn subscriber_count(&self) -> usize {
        self.event_tx.receiver_count()
    }

    pub fn handle(&self) -> BusHandle {
        BusHandle {
            event_tx: self.event_tx.clone(),
            inbound_tx: self.inbound_tx.clone(),
        }
    }

    pub fn connect<T: Transport>(&self, transport: T, sync: SyncState) {
        self.handle().connect(transport, sync);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::time::{sleep, Duration};

    #[derive(Debug)]
    struct MockTransport {
        sent_events: Arc<Mutex<Vec<AgentEvent>>>,
        recv_queue: Arc<Mutex<Vec<Inbound>>>,
        disconnected: Arc<Mutex<bool>>,
    }

    impl MockTransport {
        fn new() -> Self {
            Self {
                sent_events: Arc::new(Mutex::new(Vec::new())),
                recv_queue: Arc::new(Mutex::new(Vec::new())),
                disconnected: Arc::new(Mutex::new(false)),
            }
        }

        fn add_inbound(&self, inbound: Inbound) {
            self.recv_queue.lock().unwrap().push(inbound);
        }

        fn get_sent_events(&self) -> Vec<AgentEvent> {
            self.sent_events.lock().unwrap().clone()
        }

        fn disconnect(&self) {
            *self.disconnected.lock().unwrap() = true;
        }
    }

    #[async_trait::async_trait]
    impl Transport for MockTransport {
        async fn recv(&mut self) -> Option<Inbound> {
            if *self.disconnected.lock().unwrap() {
                return None;
            }

            let item = {
                let mut queue = self.recv_queue.lock().unwrap();
                if queue.is_empty() {
                    None
                } else {
                    Some(queue.remove(0))
                }
            };
            
            match item {
                Some(inbound) => Some(inbound),
                None => {
                    // Just wait a bit but don't disconnect
                    sleep(Duration::from_millis(50)).await;
                    // Keep the connection alive by returning a dummy message periodically
                    Some(Inbound::Message { content: "heartbeat".to_string() })
                }
            }
        }

        async fn send(&mut self, event: AgentEvent) -> bool {
            if *self.disconnected.lock().unwrap() {
                return false;
            }
            self.sent_events.lock().unwrap().push(event);
            true
        }

        fn name(&self) -> &str { "mock" }
    }

    #[tokio::test]
    async fn test_bus_subscribe_broadcast() {
        let bus = AgentBus::new();
        let mut rx = bus.subscribe();
        
        let event = AgentEvent::Text("test".to_string());
        bus.broadcast(event.clone());
        
        let received = rx.recv().await.unwrap();
        match (&event, &received) {
            (AgentEvent::Text(s1), AgentEvent::Text(s2)) => assert_eq!(s1, s2),
            _ => panic!("event mismatch"),
        }
    }

    #[tokio::test]
    async fn test_bus_inbound() {
        let mut bus = AgentBus::new();
        let inbound_tx = bus.inbound();
        let mut inbound_rx = bus.take_inbound_rx().unwrap();
        
        let msg = Inbound::Message { content: "test".to_string() };
        inbound_tx.send(msg).unwrap();
        
        let received = inbound_rx.recv().await.unwrap();
        match received {
            Inbound::Message { content } => assert_eq!(content, "test"),
            _ => panic!("inbound mismatch"),
        }
    }

    #[tokio::test]
    async fn test_subscriber_count() {
        let bus = AgentBus::new();
        assert_eq!(bus.subscriber_count(), 0);
        
        let _rx1 = bus.subscribe();
        assert_eq!(bus.subscriber_count(), 1);
        
        let _rx2 = bus.subscribe();
        assert_eq!(bus.subscriber_count(), 2);
    }

    #[tokio::test]
    async fn test_connect_transport() {
        let bus = AgentBus::new();
        let transport = MockTransport::new();
        let events_ref = transport.sent_events.clone();
        
        let sync = SyncState {
            agent_name: None,
            model: "test".to_string(),
            thinking_level: "low".to_string(),
            session_id: "test".to_string(),
            is_streaming: false,
            turn_count: 0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_cost_usd: 0.0,
            partial_text: None,
            partial_thinking: None,
            active_tool: None,
            recent_events: Vec::new(),
        };

        bus.connect(transport, sync);
        
        // Give connect time to spawn and process sync
        sleep(Duration::from_millis(10)).await;
        
        let event = AgentEvent::Text("hello".to_string());
        bus.broadcast(event.clone());
        
        // Give bridge time to process
        sleep(Duration::from_millis(20)).await;
        
        let sent = events_ref.lock().unwrap().clone();
        assert_eq!(sent.len(), 1);
        match &sent[0] {
            AgentEvent::Text(s) => assert_eq!(s, "hello"),
            _ => panic!("wrong event type: {:?}", sent[0]),
        }
    }
}