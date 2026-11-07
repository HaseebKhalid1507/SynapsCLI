//! uds-relay <socket>: stdin -> Unix socket, socket -> stdout. Static (musl)
//! so it runs in any task container; lets synaps-run.sh speak the synaps
//! daemon's line-JSON protocol.
//!
//! Plain read/write loops, never `std::io::copy`: on Linux that splices a
//! socket into a pipe in the kernel and can hold the last frame back until
//! more data arrives (a turn's final `done`/`idle` went missing that way).
use std::fs::File;
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::fd::FromRawFd;
use std::os::unix::net::UnixStream;

fn pump(mut from: impl Read, mut to: impl Write) {
    let mut buf = [0u8; 65536];
    loop {
        match from.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => {
                if to.write_all(&buf[..n]).and_then(|_| to.flush()).is_err() {
                    return;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        }
    }
}

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: uds-relay <socket>");
        std::process::exit(2)
    });
    let sock = UnixStream::connect(&path).unwrap_or_else(|e| {
        eprintln!("uds-relay: connect {path}: {e}");
        std::process::exit(2)
    });
    let w = sock.try_clone().expect("clone socket");
    std::thread::spawn(move || {
        // SAFETY: fd 0 is this process's stdin for its whole life.
        pump(unsafe { File::from_raw_fd(0) }, &w);
        let _ = w.shutdown(Shutdown::Write);
    });
    // SAFETY: fd 1 is this process's stdout; unbuffered, so every frame
    // reaches the pipe as soon as it is read.
    pump(&sock, unsafe { File::from_raw_fd(1) });
    // The daemon closed the connection: done, even if stdin is still open.
    std::process::exit(0);
}
