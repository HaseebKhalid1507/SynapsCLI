use std::{collections::HashMap, sync::Arc};
use tokio::sync::{oneshot, Mutex};

use super::{CallbackResult, CALLBACK_HOST};

pub(crate) const SUCCESS_HTML: &str = "<!doctype html><title>Login Successful</title><h1>Authentication successful</h1><p>You can close this window.</p>";
pub(crate) const ERROR_HTML: &str = "<!doctype html><title>Login Failed</title><h1>Authentication failed</h1><p>Return to the terminal.</p>";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallbackOutcome {
    Authorized(CallbackResult),
    Denied {
        error: String,
        description: Option<String>,
    },
    Invalid,
}

pub struct CallbackServerHandle {
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}
impl CallbackServerHandle {
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), task).await;
        }
    }
}
impl Drop for CallbackServerHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

pub async fn start_callback_server(
    expected_state: String,
    port: u16,
) -> Result<(oneshot::Receiver<CallbackOutcome>, CallbackServerHandle), String> {
    start_callback_server_at(expected_state, CALLBACK_HOST, port, "/callback").await
}

pub async fn start_callback_server_at(
    expected_state: String,
    host: &str,
    port: u16,
    path: &str,
) -> Result<(oneshot::Receiver<CallbackOutcome>, CallbackServerHandle), String> {
    if !path.starts_with('/') || path.contains("..") {
        return Err("invalid callback path".into());
    }
    let (tx, rx) = oneshot::channel();
    let tx = Arc::new(Mutex::new(Some(tx)));
    let handler = {
        let tx = tx.clone();
        move |axum::extract::Query(query): axum::extract::Query<HashMap<String, String>>| {
            let tx = tx.clone();
            let expected = expected_state.clone();
            async move {
                let outcome = if let Some(error) = query.get("error") {
                    CallbackOutcome::Denied {
                        error: sanitize(error),
                        description: query.get("error_description").map(|s| sanitize(s)),
                    }
                } else if let (Some(code), Some(state)) = (query.get("code"), query.get("state")) {
                    if state == &expected {
                        CallbackOutcome::Authorized(CallbackResult {
                            code: code.clone(),
                            state: state.clone(),
                        })
                    } else {
                        CallbackOutcome::Invalid
                    }
                } else {
                    CallbackOutcome::Invalid
                };
                if let Some(sender) = tx.lock().await.take() {
                    let _ = sender.send(outcome.clone());
                }
                axum::response::Html(if matches!(outcome, CallbackOutcome::Authorized(_)) {
                    SUCCESS_HTML
                } else {
                    ERROR_HTML
                })
            }
        }
    };
    let app = axum::Router::new().route(path, axum::routing::get(handler));
    let addr = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| format!("Failed to bind callback server on {addr}: {e}"))?;
    let (shutdown, stop) = oneshot::channel();
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = stop.await;
            })
            .await;
    });
    Ok((
        rx,
        CallbackServerHandle {
            shutdown: Some(shutdown),
            task: Some(task),
        },
    ))
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '_' | '-' | '.'))
        .take(160)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sanitizes_provider_errors() {
        assert_eq!(sanitize("denied\nsecret=?"), "deniedsecret");
    }
}
