#[cfg(unix)]
pub async fn run(command: String, args: Vec<String>) {
    crate::watcher::run(command, args).await;
}

/// The watcher daemon relies on Unix process supervision (fork/exec process
/// groups, UDS control sockets). Not yet ported to Windows — fail with a
/// clear message instead of a compile-gated hole.
#[cfg(windows)]
pub async fn run(_command: String, _args: Vec<String>) {
    eprintln!("synaps watch: the watcher daemon is not yet supported on Windows.");
    std::process::exit(1);
}
