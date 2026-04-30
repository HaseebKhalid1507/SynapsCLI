use crossterm::event::Event;
use synaps_cli::{VoiceEvent, VoiceEventReceiver};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum AppInputEvent {
    Terminal(Event),
    Voice(VoiceEvent),
}

pub(super) async fn next_app_input_event(
    event_reader: &mut crossterm::event::EventStream,
    mut voice_events: Option<&mut VoiceEventReceiver>,
) -> Option<AppInputEvent> {
    let mut voice_open = voice_events
        .as_ref()
        .is_some_and(|receiver| !receiver.is_closed());

    loop {
        if voice_open {
            let receiver = voice_events.as_mut().expect("voice_open requires receiver");
            tokio::select! {
                maybe_terminal = futures::StreamExt::next(event_reader) => {
                    return match maybe_terminal {
                        Some(Ok(event)) => Some(AppInputEvent::Terminal(event)),
                        Some(Err(_)) | None => None,
                    };
                }
                maybe_voice = receiver.recv() => {
                    match maybe_voice {
                        Some(event) => return Some(AppInputEvent::Voice(event)),
                        None => voice_open = false,
                    }
                }
            }
        } else {
            return match futures::StreamExt::next(event_reader).await {
                Some(Ok(event)) => Some(AppInputEvent::Terminal(event)),
                Some(Err(_)) | None => None,
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn wraps_terminal_events_without_changing_them() {
        let terminal_event = Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
        let app_event = AppInputEvent::Terminal(terminal_event.clone());

        assert_eq!(app_event, AppInputEvent::Terminal(terminal_event));
    }

    #[test]
    fn wraps_voice_events_as_app_input() {
        let voice_event = VoiceEvent::ListeningStarted;
        let app_event = AppInputEvent::Voice(voice_event.clone());

        assert_eq!(app_event, AppInputEvent::Voice(voice_event));
    }

    #[tokio::test]
    async fn voice_events_do_not_wait_for_terminal_input() {
        let (_tx, mut rx) = tokio::sync::mpsc::channel(1);
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            let event = rx.recv().await.map(AppInputEvent::Voice);
            let _ = result_tx.send(event);
        });

        _tx.try_send(VoiceEvent::ListeningStarted).unwrap();

        let app_event = tokio::time::timeout(std::time::Duration::from_millis(50), result_rx)
            .await
            .expect("voice receive should not block on terminal")
            .expect("task should send result");

        assert_eq!(app_event, Some(AppInputEvent::Voice(VoiceEvent::ListeningStarted)));
    }
}
