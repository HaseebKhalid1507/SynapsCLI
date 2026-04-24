use std::sync::Arc;
use tokio::sync::{oneshot, Mutex};

use super::{CallbackResult, CALLBACK_HOST};

pub(crate) const SUCCESS_HTML: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Login Successful</title>
<style>
body { background: #09090b; color: #fafafa; font-family: system-ui; display: flex;
       align-items: center; justify-content: center; min-height: 100vh; margin: 0; }
main { text-align: center; max-width: 480px; }
h1 { font-size: 24px; margin-bottom: 8px; }
p { color: #a1a1aa; }
</style></head>
<body><main>
<h1>✓ Authentication successful</h1>
<p>You can close this window and return to your terminal.</p>
</main></body></html>"#;

pub(crate) const ERROR_HTML: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Login Failed</title>
<style>
body { background: #09090b; color: #fafafa; font-family: system-ui; display: flex;
       align-items: center; justify-content: center; min-height: 100vh; margin: 0; }
main { text-align: center; max-width: 480px; }
h1 { font-size: 24px; margin-bottom: 8px; color: #ef4444; }
p { color: #a1a1aa; }
</style></head>
<body><main>
<h1>✗ Authentication failed</h1>
<p>Something went wrong. Please try again.</p>
</main></body></html>"#;

/// Handle to shut down the callback server.
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

/// Start a temporary HTTP server on localhost that captures the OAuth callback.
/// Returns a oneshot receiver that resolves with the auth code + state.
pub async fn start_callback_server(
    expected_state: String,
    port: u16,
    callback_path: &str,
) -> std::result::Result<(oneshot::Receiver<CallbackResult>, CallbackServerHandle), String> {
    let (tx, rx) = oneshot::channel::<CallbackResult>();
    let tx = Arc::new(Mutex::new(Some(tx)));

    let expected = expected_state.clone();
    let tx_clone = tx.clone();

    let path = callback_path.to_string();
    let app = axum::Router::new().route(
        &path,
        axum::routing::get(move |query: axum::extract::Query<std::collections::HashMap<String, String>>| {
            let tx = tx_clone.clone();
            let expected = expected.clone();
            async move {
                let code = query.get("code").cloned();
                let state = query.get("state").cloned();
                let error = query.get("error").cloned();

                if let Some(err) = error {
                    eprintln!("OAuth error from provider: {}", err);
                    return axum::response::Html(ERROR_HTML.to_string());
                }

                let (code, state) = match (code, state) {
                    (Some(c), Some(s)) => (c, s),
                    _ => {
                        return axum::response::Html(ERROR_HTML.to_string());
                    }
                };

                if state != expected {
                    eprintln!("State mismatch: expected {}, got {}", expected, state);
                    return axum::response::Html(ERROR_HTML.to_string());
                }

                // Send the result
                if let Some(sender) = tx.lock().await.take() {
                    let _ = sender.send(CallbackResult {
                        code,
                        state,
                    });
                }

                axum::response::Html(SUCCESS_HTML.to_string())
            }
        }),
    );

    let addr = format!("{}:{}", CALLBACK_HOST, port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| format!("Failed to bind callback server on {}: {}", addr, e))?;

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    let server_handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
            .ok();
    });

    Ok((
        rx,
        CallbackServerHandle {
            shutdown: Some(shutdown_tx),
            task: Some(server_handle),
        },
    ))
}
