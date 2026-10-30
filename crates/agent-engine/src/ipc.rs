//! Daemon IPC endpoint — one API, two transports.
//!
//! * **Unix**: a Unix domain socket at the endpoint path (the daemon binds
//!   it inside the 0700 run dir and chmods it 0600, see
//!   `daemon::listener::bind`).
//! * **Windows**: a named pipe whose name is derived from the endpoint path
//!   ([`pipe_name`]), so `daemon.json`, [`crate::daemon::registry::DaemonPaths`]
//!   and every caller keep speaking in paths; nothing is created on disk at
//!   the path. The pipe carries what the socket's file mode carries on Unix:
//!   - the server instances get an owner-only DACL (this user and SYSTEM)
//!     and refuse remote clients — the 0600 analogue; and
//!   - a client refuses a server process that does not run as this user —
//!     the 0700-dir analogue: no other account can squat the name while no
//!     daemon is serving it.
//!
//! Read halves report a peer that went away as EOF on both platforms (named
//! pipes surface it as `BrokenPipe`), so frame readers keep one EOF path.

use std::io;
use std::path::Path;

pub use imp::*;

#[cfg(unix)]
mod imp {
    use super::*;
    use tokio::net::{UnixListener, UnixStream};

    /// A connection the daemon accepted.
    pub type ServerStream = UnixStream;
    pub type ServerReadHalf = tokio::net::unix::OwnedReadHalf;
    pub type ServerWriteHalf = tokio::net::unix::OwnedWriteHalf;
    /// A client's connection to the daemon.
    pub type ClientStream = UnixStream;
    pub type ClientReadHalf = tokio::net::unix::OwnedReadHalf;
    pub type ClientWriteHalf = tokio::net::unix::OwnedWriteHalf;

    pub fn split_server(s: ServerStream) -> (ServerReadHalf, ServerWriteHalf) {
        s.into_split()
    }

    pub fn split_client(s: ClientStream) -> (ClientReadHalf, ClientWriteHalf) {
        s.into_split()
    }

    pub async fn connect(path: &Path) -> io::Result<ClientStream> {
        UnixStream::connect(path).await
    }

    /// The daemon's accept side.
    pub struct Listener(UnixListener);

    impl Listener {
        /// Plain bind (tests, tools). The daemon binds through
        /// `daemon::listener::bind`, which adds the 0600 / CLOEXEC handling.
        pub fn bind(path: &Path) -> io::Result<Self> {
            UnixListener::bind(path).map(Self)
        }

        pub async fn accept(&mut self) -> io::Result<ServerStream> {
            self.0.accept().await.map(|(s, _)| s)
        }

        pub fn as_unix(&self) -> &UnixListener {
            &self.0
        }
    }

    impl From<UnixListener> for Listener {
        fn from(l: UnixListener) -> Self {
            Self(l)
        }
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use std::time::{Duration, Instant};

    use tokio::io::{AsyncRead, ReadBuf};
    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions};

    /// A connection the daemon accepted.
    pub type ServerStream = NamedPipeServer;
    pub type ServerReadHalf = EofOnBrokenPipe<tokio::io::ReadHalf<NamedPipeServer>>;
    pub type ServerWriteHalf = tokio::io::WriteHalf<NamedPipeServer>;
    /// A client's connection to the daemon.
    pub type ClientStream = NamedPipeClient;
    pub type ClientReadHalf = EofOnBrokenPipe<tokio::io::ReadHalf<NamedPipeClient>>;
    pub type ClientWriteHalf = tokio::io::WriteHalf<NamedPipeClient>;

    pub fn split_server(s: ServerStream) -> (ServerReadHalf, ServerWriteHalf) {
        let (r, w) = tokio::io::split(s);
        (EofOnBrokenPipe(r), w)
    }

    pub fn split_client(s: ClientStream) -> (ClientReadHalf, ClientWriteHalf) {
        let (r, w) = tokio::io::split(s);
        (EofOnBrokenPipe(r), w)
    }

    /// A named pipe reports the other end closing as `BrokenPipe`; a Unix
    /// socket reports it as a zero-byte read. Read halves map the former to
    /// the latter so every frame reader has one EOF path.
    pub struct EofOnBrokenPipe<R>(R);

    impl<R: AsyncRead + Unpin> AsyncRead for EofOnBrokenPipe<R> {
        fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
            match Pin::new(&mut self.0).poll_read(cx, buf) {
                Poll::Ready(Err(e)) if e.kind() == io::ErrorKind::BrokenPipe => Poll::Ready(Ok(())),
                other => other,
            }
        }
    }

    /// How long `connect` keeps retrying `ERROR_PIPE_BUSY` (every instance
    /// is between an accept and the next instance's creation).
    const BUSY_RETRY: Duration = Duration::from_secs(2);

    /// Open the daemon's pipe and check that its server runs as this user.
    /// `NotFound` when nobody serves the endpoint (the `ECONNREFUSED` /
    /// `ENOENT` of the socket world).
    pub async fn connect(path: &Path) -> io::Result<ClientStream> {
        use std::os::windows::io::AsRawHandle;
        let name = pipe_name(path);
        let deadline = Instant::now() + BUSY_RETRY;
        let client = loop {
            match ClientOptions::new().open(&name) {
                Ok(c) => break c,
                Err(e)
                    if e.raw_os_error() == Some(windows_sys::Win32::Foundation::ERROR_PIPE_BUSY as i32)
                        && Instant::now() < deadline =>
                {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                Err(e) => return Err(e),
            }
        };
        win::verify_server_is_current_user(client.as_raw_handle())?;
        Ok(client)
    }

    /// The daemon's accept side: always one listening pipe instance, so the
    /// name is never unserved (and never up for grabs) while the daemon runs.
    pub struct Listener {
        name: String,
        security: win::PipeSecurity,
        next: NamedPipeServer,
    }

    impl Listener {
        /// Create the first instance. Refuses a name that is already served
        /// (`first_pipe_instance`) — never share an endpoint with another
        /// process, the rule `bind` keeps on Unix by never unlinking first.
        pub fn bind(path: &Path) -> io::Result<Self> {
            let name = pipe_name(path);
            let security = win::PipeSecurity::current_user_only()?;
            let next = security.create(ServerOptions::new().first_pipe_instance(true).reject_remote_clients(true), &name)?;
            Ok(Self { name, security, next })
        }

        fn instance(&self) -> io::Result<NamedPipeServer> {
            self.security.create(ServerOptions::new().reject_remote_clients(true), &self.name)
        }

        /// Wait for a client on the listening instance and hand it out; a
        /// fresh instance takes its place first. Cancel-safe (tokio's
        /// `connect` is): a dropped accept resumes on the same instance.
        pub async fn accept(&mut self) -> io::Result<ServerStream> {
            if let Err(e) = self.next.connect().await {
                // A client that came and went before the connect leaves the
                // instance unusable: replace it (new one first).
                if let Ok(fresh) = self.instance() {
                    self.next = fresh;
                }
                return Err(e);
            }
            // On failure `next` stays the connected instance; the next accept
            // returns at once (already connected) and retries the creation.
            let fresh = self.instance()?;
            Ok(std::mem::replace(&mut self.next, fresh))
        }
    }

    /// The pipe name for an endpoint path. Hashed: pipe names live in one
    /// flat, machine-wide namespace and may not contain `\`.
    ///
    /// On Unix every spelling of a socket path reaches the same inode; a
    /// hash only matches the same string. So one spelling is hashed: the
    /// absolute path with its directory resolved (`canonicalize`: 8.3 short
    /// names, junctions, symlinks, `..`) when it exists, without the `\\?\`
    /// prefix `canonicalize` adds, `\` separators, lowercase (paths are
    /// case-insensitive) — the daemon's run dir is canonical while a
    /// caller's may not be.
    pub fn pipe_name(path: &Path) -> String {
        use sha2::{Digest, Sha256};
        let key = endpoint_key(path);
        let digest = Sha256::digest(key.as_bytes());
        let hex: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
        format!(r"\\.\pipe\synaps-daemon-{hex}")
    }

    fn endpoint_key(path: &Path) -> String {
        let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
        let resolved = match (abs.parent(), abs.file_name()) {
            (Some(dir), Some(name)) => dir.canonicalize().map(|d| d.join(name)).unwrap_or_else(|_| abs.clone()),
            _ => abs,
        };
        let s = resolved.to_string_lossy().replace('/', "\\");
        let s = if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            format!(r"\\{rest}")
        } else if let Some(rest) = s.strip_prefix(r"\\?\") {
            rest.to_string()
        } else {
            s
        };
        s.to_lowercase()
    }

    mod win {
        use std::ffi::c_void;
        use std::io;
        use std::ptr::null_mut;

        use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
        use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE};
        use windows_sys::Win32::Security::Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };
        use windows_sys::Win32::Security::{
            EqualSid, GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, TOKEN_QUERY,
            TOKEN_USER,
        };
        use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
        use windows_sys::Win32::System::Threading::{
            GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
        };

        struct Handle(HANDLE);
        impl Drop for Handle {
            fn drop(&mut self) {
                // SAFETY: a handle we opened and own.
                unsafe { CloseHandle(self.0) };
            }
        }

        /// A process token's `TOKEN_USER` (8-aligned: it holds pointers).
        struct TokenUserBuf(Vec<u64>);

        impl TokenUserBuf {
            fn of(process: HANDLE) -> io::Result<Self> {
                let mut token: HANDLE = null_mut();
                // SAFETY: valid process handle and out-pointer.
                if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
                    return Err(io::Error::last_os_error());
                }
                let token = Handle(token);
                let mut len = 0u32;
                // SAFETY: size query (fails with ERROR_INSUFFICIENT_BUFFER, sets len).
                unsafe { GetTokenInformation(token.0, TokenUser, null_mut(), 0, &mut len) };
                if len == 0 {
                    return Err(io::Error::last_os_error());
                }
                let mut buf = vec![0u64; (len as usize).div_ceil(8)];
                // SAFETY: buf holds at least `len` bytes.
                if unsafe { GetTokenInformation(token.0, TokenUser, buf.as_mut_ptr().cast(), len, &mut len) } == 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(Self(buf))
            }

            fn current() -> io::Result<Self> {
                // SAFETY: the pseudo-handle needs no close.
                Self::of(unsafe { GetCurrentProcess() })
            }

            fn sid(&self) -> PSID {
                // SAFETY: filled by GetTokenInformation(TokenUser).
                unsafe { (*(self.0.as_ptr() as *const TOKEN_USER)).User.Sid }
            }
        }

        fn sid_string(sid: PSID) -> io::Result<String> {
            let mut s: *mut u16 = null_mut();
            // SAFETY: valid SID; s receives a LocalAlloc'd string.
            if unsafe { ConvertSidToStringSidW(sid, &mut s) } == 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: NUL-terminated wide string from the call above.
            let out = unsafe {
                let len = (0..).take_while(|&i| *s.add(i) != 0).count();
                String::from_utf16_lossy(std::slice::from_raw_parts(s, len))
            };
            // SAFETY: allocated by ConvertSidToStringSidW.
            unsafe { LocalFree(s.cast()) };
            Ok(out)
        }

        /// Security descriptor for daemon pipe instances: a protected DACL
        /// granting full access to this user and SYSTEM only (the default
        /// pipe DACL also grants Everyone read).
        pub struct PipeSecurity(PSECURITY_DESCRIPTOR);

        // SAFETY: an immutable, self-contained LocalAlloc'd descriptor.
        unsafe impl Send for PipeSecurity {}
        unsafe impl Sync for PipeSecurity {}

        impl PipeSecurity {
            pub fn current_user_only() -> io::Result<Self> {
                let user = sid_string(TokenUserBuf::current()?.sid())?;
                let sddl: Vec<u16> = format!("D:P(A;;GA;;;SY)(A;;GA;;;{user})").encode_utf16().chain([0]).collect();
                let mut sd: PSECURITY_DESCRIPTOR = null_mut();
                // SAFETY: NUL-terminated SDDL; sd receives a LocalAlloc'd descriptor.
                if unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut sd, null_mut()) } == 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(Self(sd))
            }

            pub fn create(&self, opts: &ServerOptions, name: &str) -> io::Result<NamedPipeServer> {
                let mut sa = SECURITY_ATTRIBUTES {
                    nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                    lpSecurityDescriptor: self.0,
                    bInheritHandle: 0,
                };
                // SAFETY: sa and the descriptor outlive the call.
                unsafe { opts.create_with_security_attributes_raw(name, &mut sa as *mut SECURITY_ATTRIBUTES as *mut c_void) }
            }
        }

        impl Drop for PipeSecurity {
            fn drop(&mut self) {
                // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
                unsafe { LocalFree(self.0) };
            }
        }

        /// Refuse a pipe whose server process runs as another account.
        pub fn verify_server_is_current_user(pipe: std::os::windows::io::RawHandle) -> io::Result<()> {
            let mut pid = 0u32;
            // SAFETY: a connected client pipe handle.
            if unsafe { GetNamedPipeServerProcessId(pipe as HANDLE, &mut pid) } == 0 {
                return Err(io::Error::last_os_error());
            }
            let refuse = |why: String| {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("daemon pipe is served by pid {pid}, {why}; refusing to connect"),
                )
            };
            // SAFETY: plain OpenProcess; null on failure.
            let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
            if process.is_null() {
                return Err(refuse(format!("whose owner cannot be read ({})", io::Error::last_os_error())));
            }
            let process = Handle(process);
            let theirs = TokenUserBuf::of(process.0).map_err(|e| refuse(format!("whose owner cannot be read ({e})")))?;
            let ours = TokenUserBuf::current()?;
            // SAFETY: two valid SIDs.
            if unsafe { EqualSid(ours.sid(), theirs.sid()) } == 0 {
                return Err(refuse("which runs as another user".into()));
            }
            Ok(())
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn endpoint() -> (tempfile::TempDir, std::path::PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("daemon.sock");
        (d, p)
    }

    #[test]
    fn pipe_name_is_stable_and_case_and_separator_insensitive() {
        let a = pipe_name(Path::new(r"C:\Users\Me\.synaps-cli\run\daemon.sock"));
        let b = pipe_name(Path::new("c:/users/me/.synaps-cli/run/DAEMON.sock"));
        assert_eq!(a, b);
        assert!(a.starts_with(r"\\.\pipe\synaps-daemon-"));
        assert_ne!(a, pipe_name(Path::new(r"C:\Users\Me\.synaps-cli\run\daemon-work.sock")));
    }

    #[test]
    fn pipe_name_resolves_the_directory_spelling() {
        let (d, p) = endpoint();
        let canon = d.path().canonicalize().unwrap().join("daemon.sock");
        assert!(canon.to_string_lossy().starts_with(r"\\?\"));
        assert_eq!(pipe_name(&p), pipe_name(&canon));
        let dotted = d.path().join("sub").join("..").join("daemon.sock");
        assert_eq!(pipe_name(&p), pipe_name(&dotted));
    }

    #[tokio::test]
    async fn round_trip_and_peer_close_reads_as_eof() {
        let (_d, path) = endpoint();
        let mut l = Listener::bind(&path).unwrap();
        let srv = tokio::spawn(async move {
            let s = l.accept().await.unwrap();
            let (mut r, mut w) = split_server(s);
            let mut buf = [0u8; 5];
            r.read_exact(&mut buf).await.unwrap();
            w.write_all(&buf).await.unwrap();
            // The client closes: EOF, not BrokenPipe.
            let mut rest = Vec::new();
            r.read_to_end(&mut rest).await.unwrap();
            assert!(rest.is_empty());
        });
        let c = connect(&path).await.unwrap();
        let (mut r, mut w) = split_client(c);
        w.write_all(b"hello").await.unwrap();
        let mut buf = [0u8; 5];
        r.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello");
        drop((r, w));
        srv.await.unwrap();
    }

    #[tokio::test]
    async fn serves_several_clients_and_refuses_a_second_server() {
        let (_d, path) = endpoint();
        let mut l = Listener::bind(&path).unwrap();
        assert!(Listener::bind(&path).is_err(), "a served name is never shared");
        let srv = tokio::spawn(async move {
            for _ in 0..3 {
                let s = l.accept().await.unwrap();
                let (_r, mut w) = split_server(s);
                w.write_all(b"x").await.unwrap();
            }
        });
        for _ in 0..3 {
            let (mut r, _w) = split_client(connect(&path).await.unwrap());
            let mut b = [0u8; 1];
            r.read_exact(&mut b).await.unwrap();
        }
        srv.await.unwrap();
    }

    #[tokio::test]
    async fn connect_without_a_server_is_not_found() {
        let (_d, path) = endpoint();
        let e = connect(&path).await.unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
    }
}
