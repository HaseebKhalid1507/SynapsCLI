//! Which process wrote each frame on a daemon connection.
//!
//! The daemon sets `SO_PASSCRED` on every accepted socket ([`pass_creds`])
//! and reads it with `recvmsg`, taking the `SCM_CREDENTIALS` the kernel
//! attaches: the writer's tgid as of its `write`. With `SO_PASSCRED` on, the
//! kernel never joins two writers' bytes in one read, so each read is one
//! writer's. [`CredRead`] logs each read's byte range with that pid, and
//! [`CredRead::writer_of`] names the one writer of a frame's bytes, or
//! `None` when they came from more than one process (or one it can't tell:
//! bytes written before `SO_PASSCRED` was on, a writer in a pid namespace
//! the daemon can't see). A child of the app writing on an inherited fd is
//! its own tgid, so its frames are not the owner app's own.
//!
//! The pid is a fact about the write only; `owner_claim` pins the process
//! itself (pidfd + start time) and checks it still runs when a frame
//! counts. Off Linux every writer is `None` (and no claim binds).

use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, ReadBuf};

use crate::ipc::{ServerReadHalf, ServerStream};

/// Turn on `SO_PASSCRED` for an accepted connection (Linux). A failure is
/// logged; that connection's writers are then unknown, so its claims fail
/// closed (`not_the_app`).
pub fn pass_creds(stream: &ServerStream) {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let on: libc::c_int = 1;
        // SAFETY: a valid socket fd and a c_int option of the right size.
        let rc = unsafe {
            libc::setsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PASSCRED,
                (&on as *const libc::c_int).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            tracing::warn!(error = %io::Error::last_os_error(), "daemon: SO_PASSCRED failed; this connection's frames have no known writer");
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = stream;
}

/// A connection's read half that remembers who wrote each byte range.
pub struct CredRead {
    inner: ServerReadHalf,
    /// Bytes read so far.
    received: u64,
    /// `(start, end, writer)` per read, oldest first; pruned as frames are
    /// consumed ([`Self::writer_of`]).
    reads: VecDeque<(u64, u64, Option<u32>)>,
}

impl CredRead {
    pub fn new(inner: ServerReadHalf) -> Self {
        Self { inner, received: 0, reads: VecDeque::new() }
    }

    /// Bytes read from the socket so far (the frame reader's buffer
    /// included).
    pub fn received(&self) -> u64 {
        self.received
    }

    /// An offset no byte written to this socket so far ends past: the bytes
    /// read (the frame reader's buffer included) plus those still queued in
    /// the socket (`FIONREAD`). Taken when a claim passes the image check,
    /// it is the link's mark: a frame that starts before it was written
    /// before the check, perhaps by the image the claimant ran before an
    /// `execve`, so it is nobody's. `None` when the queue can't be measured
    /// (off Linux, or the ioctl failed).
    pub fn horizon(&self) -> Option<u64> {
        queued(&self.inner).map(|queued| self.received + queued)
    }

    /// The one writer of the bytes `start..end`, or `None` (more than one,
    /// or unknown). Forgets every read that ends by `end`.
    pub fn writer_of(&mut self, start: u64, end: u64) -> Option<u32> {
        let mut writer: Option<Option<u32>> = None;
        let mut mixed = false;
        for &(from, to, pid) in &self.reads {
            if to <= start || from >= end {
                continue;
            }
            match writer {
                None => writer = Some(pid),
                Some(seen) if seen != pid => mixed = true,
                Some(_) => {}
            }
        }
        while self.reads.front().is_some_and(|&(_, to, _)| to <= end) {
            self.reads.pop_front();
        }
        if mixed {
            return None;
        }
        writer.flatten()
    }

    fn note(&mut self, n: usize, pid: Option<u32>) {
        if n == 0 {
            return;
        }
        let start = self.received;
        self.received += n as u64;
        match self.reads.back_mut() {
            Some((_, end, last)) if *last == pid && *end == start => *end = self.received,
            _ => self.reads.push_back((start, self.received, pid)),
        }
    }
}

impl AsyncRead for CredRead {
    #[cfg(target_os = "linux")]
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        use std::os::fd::AsRawFd;
        let me = self.get_mut();
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        loop {
            let got = {
                let stream: &tokio::net::UnixStream = me.inner.as_ref();
                std::task::ready!(stream.poll_read_ready(cx))?;
                let dst = buf.initialize_unfilled();
                stream.try_io(tokio::io::Interest::READABLE, || recv_with_creds(stream.as_raw_fd(), dst))
            };
            match got {
                Ok((n, pid)) => {
                    buf.advance(n);
                    me.note(n, pid);
                    return Poll::Ready(Ok(()));
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(e) => return Poll::Ready(Err(e)),
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let me = self.get_mut();
        let before = buf.filled().len();
        let polled = Pin::new(&mut me.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &polled {
            let n = buf.filled().len() - before;
            me.note(n, None);
        }
        polled
    }
}

/// The bytes queued unread in the socket (`FIONREAD`; for a Unix stream
/// socket, every byte written to it and not yet read).
#[cfg(target_os = "linux")]
fn queued(inner: &ServerReadHalf) -> Option<u64> {
    use std::os::fd::AsRawFd;
    let stream: &tokio::net::UnixStream = inner.as_ref();
    let mut n: libc::c_int = 0;
    // SAFETY: FIONREAD on a valid socket fd writes one c_int into `n`.
    let rc = unsafe { libc::ioctl(stream.as_raw_fd(), libc::FIONREAD, &mut n) };
    if rc != 0 {
        return None;
    }
    u64::try_from(n).ok()
}

#[cfg(not(target_os = "linux"))]
fn queued(_inner: &ServerReadHalf) -> Option<u64> {
    None
}

/// One `recvmsg` into `dst`: the byte count and the writer's pid from
/// `SCM_CREDENTIALS` (`None` when absent or 0). Any fds passed along are
/// closed (the kernel installs them close-on-exec).
#[cfg(target_os = "linux")]
fn recv_with_creds(fd: std::os::fd::RawFd, dst: &mut [u8]) -> io::Result<(usize, Option<u32>)> {
    let mut iov = libc::iovec { iov_base: dst.as_mut_ptr().cast(), iov_len: dst.len() };
    // Room for one `ucred` and a few fds, u64-aligned for `cmsghdr`.
    let mut control = [0u64; 16];
    // SAFETY: an all-zero msghdr is a valid empty one.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = std::mem::size_of_val(&control) as _;
    // SAFETY: `msg` points at `iov` (into `dst`) and `control`, both live
    // for the call.
    let n = unsafe { libc::recvmsg(fd, &mut msg, libc::MSG_CMSG_CLOEXEC | libc::MSG_DONTWAIT) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut pid = None;
    // SAFETY: the CMSG_* walk stays inside `control` as the kernel filled it
    // (`msg_controllen`); each read is unaligned-safe.
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            let head = &*cmsg;
            let data = libc::CMSG_DATA(cmsg);
            let len = (head.cmsg_len as usize).saturating_sub(libc::CMSG_LEN(0) as usize);
            if head.cmsg_level == libc::SOL_SOCKET && head.cmsg_type == libc::SCM_CREDENTIALS && len >= std::mem::size_of::<libc::ucred>() {
                let cred: libc::ucred = std::ptr::read_unaligned(data.cast());
                pid = u32::try_from(cred.pid).ok().filter(|p| *p > 0);
            } else if head.cmsg_level == libc::SOL_SOCKET && head.cmsg_type == libc::SCM_RIGHTS {
                for i in 0..len / std::mem::size_of::<libc::c_int>() {
                    let passed: libc::c_int = std::ptr::read_unaligned(data.cast::<libc::c_int>().add(i));
                    libc::close(passed);
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }
    Ok((n as usize, pid))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    /// The writer of `line` as the conn reads it: the span the frame
    /// reader consumed.
    async fn read_line(reader: &mut BufReader<CredRead>) -> (String, Option<u32>) {
        let start = reader.get_ref().received() - reader.buffer().len() as u64;
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let end = reader.get_ref().received() - reader.buffer().len() as u64;
        let writer = reader.get_mut().writer_of(start, end);
        (line, writer)
    }

    fn pair() -> (tokio::net::UnixStream, BufReader<CredRead>) {
        let (client, server) = tokio::net::UnixStream::pair().unwrap();
        pass_creds(&server);
        let (r, _w) = crate::ipc::split_server(server);
        (client, BufReader::new(CredRead::new(r)))
    }

    #[tokio::test]
    async fn a_frame_names_its_writer() {
        let (mut client, mut reader) = pair();
        client.write_all(b"{\"type\":\"ping\"}\n").await.unwrap();
        let (line, writer) = read_line(&mut reader).await;
        assert_eq!(line, "{\"type\":\"ping\"}\n");
        assert_eq!(writer, Some(std::process::id()));
    }

    /// A child of this process writing `text` on its stdout, which is a
    /// clone of `client` (the fd it inherits); its pid.
    fn child_writes(client: &std::os::unix::net::UnixStream, text: &str) -> u32 {
        let out = std::os::fd::OwnedFd::from(client.try_clone().unwrap());
        let child = std::process::Command::new("printf")
            .arg(text)
            .stdout(std::process::Stdio::from(out))
            .spawn()
            .unwrap();
        let pid = child.id();
        assert!(child.wait_with_output().unwrap().status.success());
        pid
    }

    /// A child writing on an inherited fd is its own tgid: its frame names
    /// the child, never this process; a frame of both is nobody's.
    #[tokio::test]
    async fn a_child_on_an_inherited_fd_is_not_the_parent() {
        use std::io::Write;
        let (client, mut reader) = pair();
        let client = client.into_std().unwrap();
        client.set_nonblocking(false).unwrap();
        // The child's whole frame.
        let child = child_writes(&client, "{\"type\":\"ping\"}\n");
        let (line, writer) = read_line(&mut reader).await;
        assert_eq!(line, "{\"type\":\"ping\"}\n");
        assert_eq!(writer, Some(child), "the child's tgid");
        assert_ne!(writer, Some(std::process::id()));

        // Half from this process, the rest from a child: nobody's.
        (&client).write_all(b"{\"type\":").unwrap();
        child_writes(&client, "\"ping\"}\n");
        let (line, writer) = read_line(&mut reader).await;
        assert_eq!(line, "{\"type\":\"ping\"}\n");
        assert_eq!(writer, None, "a frame of two writers is nobody's");

        // This process again.
        (&client).write_all(b"{\"type\":\"ping\"}\n").unwrap();
        let (_, writer) = read_line(&mut reader).await;
        assert_eq!(writer, Some(std::process::id()));
    }

    /// Without `SO_PASSCRED` no frame has a writer, so no claim can pass
    /// (fail closed).
    #[tokio::test]
    async fn without_passcred_no_frame_has_a_writer() {
        let (mut client, server) = tokio::net::UnixStream::pair().unwrap();
        let (r, _w) = crate::ipc::split_server(server);
        let mut reader = BufReader::new(CredRead::new(r));
        client.write_all(b"{\"type\":\"ping\"}\n").await.unwrap();
        let (_, writer) = read_line(&mut reader).await;
        assert_eq!(writer, None);
    }

    /// The horizon (a claim's mark) is past every byte written so far:
    /// those read, those in the frame reader's buffer and those still
    /// queued in the socket.
    #[tokio::test]
    async fn the_horizon_counts_the_bytes_queued_unread() {
        let (mut client, mut reader) = pair();
        assert_eq!(reader.get_ref().horizon(), Some(0));
        let ping = b"{\"type\":\"ping\"}\n";
        client.write_all(&[&ping[..], &ping[..]].concat()).await.unwrap();
        assert_eq!(reader.get_ref().received(), 0);
        assert_eq!(reader.get_ref().horizon(), Some(32), "queued, none read");
        let (line, _) = read_line(&mut reader).await;
        assert_eq!(line.as_bytes(), ping);
        assert_eq!(reader.get_ref().horizon(), Some(32), "one frame read, the next buffered");
        client.write_all(b"{").await.unwrap();
        assert_eq!(reader.get_ref().horizon(), Some(33), "a byte written later");
    }

    #[tokio::test]
    async fn writer_of_spans_and_prunes() {
        let (_client, server) = tokio::net::UnixStream::pair().unwrap();
        let (r, _w) = crate::ipc::split_server(server);
        let mut cr = CredRead::new(r);
        cr.note(10, Some(7));
        cr.note(5, Some(7));
        assert_eq!(cr.reads.len(), 1, "one writer's reads join");
        cr.note(5, Some(8));
        assert_eq!(cr.writer_of(0, 15), Some(7));
        cr.note(4, Some(7));
        assert_eq!(cr.writer_of(15, 24), None, "two writers");
        assert!(cr.reads.is_empty(), "consumed reads are forgotten");
        cr.note(10, None);
        assert_eq!(cr.writer_of(24, 30), None, "an unknown writer");
        assert_eq!(cr.reads.len(), 1, "a read with bytes still buffered is kept");
    }
}
