//! Diagnostics on stderr that can never kill the process.
//!
//! `eprintln!` panics when the write fails, and a daemon's stderr fails
//! for reasons it cannot control: the process that started it (and held
//! the read end of its stderr pipe) has exited, so the write gets EPIPE;
//! the terminal it was started from is gone (EIO); the disk is full. In a
//! release build (`panic = "abort"`) that panic ends the whole daemon with
//! no log line. Everything in Synaps' own code that the daemon can reach
//! writes through [`stderr_line`] (or [`eprintln_lossy!`](crate::eprintln_lossy))
//! instead: a failed write drops the line and nothing else happens.
//! Dependencies are not covered: `tracing-appender` still uses `eprintln!`
//! / `println!` on its own error paths (see docs/daemon-mode.md).

use std::fmt;
use std::io::{self, Write};

/// Write `args` and a newline to `w`, reporting the first error.
pub fn write_line<W: Write + ?Sized>(w: &mut W, args: fmt::Arguments<'_>) -> io::Result<()> {
    w.write_fmt(args)?;
    w.write_all(b"\n")?;
    w.flush()
}

/// [`write_line`] that drops the line on any error. Never panics.
pub fn write_line_lossy<W: Write + ?Sized>(w: &mut W, args: fmt::Arguments<'_>) {
    let _ = write_line(w, args);
}

/// One line on the process's stderr; a failed write is dropped silently.
pub fn stderr_line(args: fmt::Arguments<'_>) {
    write_line_lossy(&mut io::stderr().lock(), args);
}

/// `eprintln!` without the panic: a closed, broken or full stderr drops the
/// line. Use it in every path a daemon can reach.
#[macro_export]
macro_rules! eprintln_lossy {
    ($($arg:tt)*) => {
        $crate::core::stdio::stderr_line(::std::format_args!($($arg)*))
    };
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::fd::FromRawFd;

    /// A pipe whose read end is already closed: what a daemon's stderr is
    /// once the client that spawned it has exited.
    fn dead_pipe_writer() -> std::fs::File {
        let mut fds = [0i32; 2];
        // SAFETY: pipe(2) on a valid 2-slot array.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe: {}", io::Error::last_os_error());
        // SAFETY: we own the read end and close it exactly once.
        unsafe { libc::close(fds[0]) };
        // SAFETY: we own the write end; the File closes it on drop.
        unsafe { std::fs::File::from_raw_fd(fds[1]) }
    }

    #[test]
    fn a_write_to_a_pipe_without_reader_fails_with_epipe() {
        // The premise: Rust ignores SIGPIPE, so the write returns EPIPE
        // (which `eprintln!` turns into a panic).
        let mut w = dead_pipe_writer();
        let err = write_line(&mut w, format_args!("config warning: x")).expect_err("a dead pipe must fail");
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe, "{err}");
    }

    #[test]
    fn the_lossy_writer_ignores_epipe() {
        let mut w = dead_pipe_writer();
        // Must return, not panic; twice, as a daemon would keep writing.
        write_line_lossy(&mut w, format_args!("config warning: {}", "first"));
        write_line_lossy(&mut w, format_args!("config warning: {}", "second"));
    }

    #[test]
    fn the_lossy_writer_still_writes_when_it_can() {
        let mut buf: Vec<u8> = Vec::new();
        write_line_lossy(&mut buf, format_args!("a {} c", "b"));
        assert_eq!(buf, b"a b c\n");
    }
}
