use crate::{Result, RuntimeError};

pub type VoiceEventSender = tokio::sync::mpsc::Sender<VoiceEvent>;
pub type VoiceEventReceiver = tokio::sync::mpsc::Receiver<VoiceEvent>;

pub const DEFAULT_VOICE_EVENT_BUFFER: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceEvent {
    ListeningStarted,
    ListeningStopped,
    PartialTranscript(String),
    FinalTranscript(String),
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceProviderState {
    Stopped,
    Running,
    Stopping,
}

pub trait SpeechToTextProvider: Send {
    fn start(&mut self, events: VoiceEventSender) -> Result<()>;
    fn stop(&mut self) -> Result<()>;
    fn is_running(&self) -> bool;
}

#[derive(Debug)]
pub struct VoiceRuntimeHandle {
    events: VoiceEventSender,
}

impl VoiceRuntimeHandle {
    pub fn events(&self) -> VoiceEventSender {
        self.events.clone()
    }

    pub fn try_emit(&self, event: VoiceEvent) -> Result<()> {
        self.events
            .try_send(event)
            .map_err(|err| RuntimeError::Tool(format!("voice event channel unavailable: {err}")))
    }
}

pub struct VoiceRuntime {
    events_tx: VoiceEventSender,
    events_rx: VoiceEventReceiver,
    stt: Option<Box<dyn SpeechToTextProvider>>,
    state: VoiceProviderState,
    worker_handles: Vec<tokio::task::JoinHandle<()>>,
}

impl VoiceRuntime {
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_VOICE_EVENT_BUFFER)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        let (events_tx, events_rx) = tokio::sync::mpsc::channel(capacity.max(1));
        Self {
            events_tx,
            events_rx,
            stt: None,
            state: VoiceProviderState::Stopped,
            worker_handles: Vec::new(),
        }
    }

    pub fn with_provider(stt: Option<Box<dyn SpeechToTextProvider>>) -> Self {
        let mut runtime = Self::new();
        runtime.stt = stt;
        runtime
    }

    pub fn handle(&self) -> VoiceRuntimeHandle {
        VoiceRuntimeHandle {
            events: self.events_tx.clone(),
        }
    }

    pub fn event_sender(&self) -> VoiceEventSender {
        self.events_tx.clone()
    }

    pub fn try_emit(&self, event: VoiceEvent) -> Result<()> {
        self.events_tx
            .try_send(event)
            .map_err(|err| RuntimeError::Tool(format!("voice event channel unavailable: {err}")))
    }

    pub async fn join_worker(&self, handle: tokio::task::JoinHandle<()>) -> Result<()> {
        handle
            .await
            .map_err(|err| RuntimeError::Tool(format!("voice worker join failed: {err}")))
    }

    pub fn track_worker(&mut self, handle: tokio::task::JoinHandle<()>) {
        self.worker_handles.push(handle);
    }

    pub async fn join_workers(&mut self) -> Result<()> {
        while let Some(handle) = self.worker_handles.pop() {
            handle
                .await
                .map_err(|err| RuntimeError::Tool(format!("voice worker join failed: {err}")))?;
        }
        Ok(())
    }

    pub async fn recv(&mut self) -> Option<VoiceEvent> {
        self.events_rx.recv().await
    }

    pub fn try_recv(&mut self) -> Option<VoiceEvent> {
        self.events_rx.try_recv().ok()
    }

    pub fn event_receiver_mut(&mut self) -> &mut VoiceEventReceiver {
        &mut self.events_rx
    }

    pub fn close_events(&mut self) {
        self.events_rx.close();
    }

    pub fn state(&self) -> VoiceProviderState {
        self.state
    }

    pub fn set_stt_provider(&mut self, provider: Box<dyn SpeechToTextProvider>) {
        self.stt = Some(provider);
    }

    pub fn start_listening(&mut self) -> Result<()> {
        if let Some(stt) = self.stt.as_mut() {
            stt.start(self.events_tx.clone())?;
            self.state = VoiceProviderState::Running;
        }
        Ok(())
    }

    pub fn stop_listening(&mut self) -> Result<()> {
        if let Some(stt) = self.stt.as_mut() {
            self.state = VoiceProviderState::Stopping;
            stt.stop()?;
        }
        self.state = VoiceProviderState::Stopped;
        Ok(())
    }

    pub fn handle_barge_in(&mut self, cancel_generation: bool) -> Result<bool> {
        Ok(cancel_generation)
    }

    pub fn shutdown(&mut self) -> Result<()> {
        self.close_events();
        self.stop_listening()?;
        Ok(())
    }
}

impl Default for VoiceRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for VoiceRuntime {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct StubStt {
        starts: usize,
        stops: usize,
        running: bool,
        events: Arc<Mutex<Vec<VoiceEvent>>>,
    }

    impl SpeechToTextProvider for StubStt {
        fn start(&mut self, events: VoiceEventSender) -> Result<()> {
            self.starts += 1;
            self.running = true;
            let event = VoiceEvent::ListeningStarted;
            self.events.lock().unwrap().push(event.clone());
            events
                .try_send(event)
                .map_err(|err| RuntimeError::Tool(format!("voice event channel unavailable: {err}")))?;
            Ok(())
        }

        fn stop(&mut self) -> Result<()> {
            self.stops += 1;
            self.running = false;
            Ok(())
        }

        fn is_running(&self) -> bool {
            self.running
        }
    }

    #[test]
    fn voice_event_covers_stt_and_errors() {
        assert_eq!(VoiceEvent::ListeningStarted, VoiceEvent::ListeningStarted);
        assert_eq!(VoiceEvent::ListeningStopped, VoiceEvent::ListeningStopped);
        assert_eq!(VoiceEvent::PartialTranscript("hel".into()), VoiceEvent::PartialTranscript("hel".into()));
        assert_eq!(VoiceEvent::FinalTranscript("hello".into()), VoiceEvent::FinalTranscript("hello".into()));
        assert_eq!(VoiceEvent::Error("mic unavailable".into()), VoiceEvent::Error("mic unavailable".into()));
    }

    #[tokio::test]
    async fn runtime_starts_stt_and_forwards_events() {
        let observed = Arc::new(Mutex::new(Vec::new()));
        let stt = StubStt {
            events: observed.clone(),
            ..Default::default()
        };
        let mut runtime = VoiceRuntime::new();
        runtime.set_stt_provider(Box::new(stt));

        runtime.start_listening().unwrap();

        assert_eq!(runtime.recv().await, Some(VoiceEvent::ListeningStarted));
        assert_eq!(observed.lock().unwrap().as_slice(), &[VoiceEvent::ListeningStarted]);
    }

    #[tokio::test]
    async fn runtime_state_transitions_on_start_stop() {
        let mut runtime = VoiceRuntime::new();
        runtime.set_stt_provider(Box::new(StubStt::default()));

        runtime.start_listening().unwrap();
        assert_eq!(runtime.state(), VoiceProviderState::Running);

        runtime.stop_listening().unwrap();
        assert_eq!(runtime.state(), VoiceProviderState::Stopped);
    }

    #[tokio::test]
    async fn runtime_buffer_capacity_is_at_least_one() {
        let runtime = VoiceRuntime::with_capacity(0);
        runtime.try_emit(VoiceEvent::ListeningStarted).unwrap();
    }
}
