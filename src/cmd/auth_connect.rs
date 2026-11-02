//! `synaps auth connect --json` — the app's machine-driven browser sign-in
//! (accounts contract §5). All protocol logic lives in
//! `synaps_cli::auth::connect`; this is only the process shell: the
//! `--json` gate, SIGTERM/SIGINT as a cancel request, and the exit code.

use std::future::Future;

use synaps_cli::auth::connect::{self, ConnectArgs};

/// Run `synaps auth connect` and return the process exit code.
///
/// Without `--json` this refuses with a hint on stderr and prints nothing
/// on stdout: there is no interactive mode (use `synaps login`).
pub async fn run(args: ConnectArgs, json: bool) -> i32 {
    if !json {
        eprintln!(
            "synaps auth connect is machine-only and requires --json; \
             to sign in interactively use: synaps login --provider <provider>"
        );
        return 1;
    }
    // Handlers go in before anything is printed, so a SIGTERM sent after
    // `started` is always a cancel request, never the default kill.
    let interrupt = interrupt_signal();
    connect::run(&args, std::io::stdout(), tokio::io::stdin(), interrupt).await
}

/// Resolves on the first SIGTERM or SIGINT. A handler that can't be
/// installed just never fires (the default disposition then applies).
#[cfg(unix)]
fn interrupt_signal() -> impl Future<Output = ()> {
    use tokio::signal::unix::{signal, SignalKind};
    let term = signal(SignalKind::terminate()).ok();
    let int = signal(SignalKind::interrupt()).ok();
    async move {
        let term = async move {
            match term {
                Some(mut s) => {
                    s.recv().await;
                }
                None => std::future::pending::<()>().await,
            }
        };
        let int = async move {
            match int {
                Some(mut s) => {
                    s.recv().await;
                }
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            () = term => {}
            () = int => {}
        }
    }
}

#[cfg(not(unix))]
fn interrupt_signal() -> impl Future<Output = ()> {
    async {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}
