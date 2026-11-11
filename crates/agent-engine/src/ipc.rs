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

    /// A pipe instance with the daemon pipe's protections, for other
    /// per-user endpoints (the session event pipes): an owner-only DACL
    /// (this user and SYSTEM) and no remote clients. `first` refuses a name
    /// that is already served.
    pub(crate) fn create_owner_only_pipe(name: &str, first: bool) -> io::Result<NamedPipeServer> {
        win::PipeSecurity::current_user_only()?
            .create(ServerOptions::new().first_pipe_instance(first).reject_remote_clients(true), name)
    }

    /// Refuse a connected pipe whose server process runs as another account.
    pub(crate) fn verify_pipe_server(client: &NamedPipeClient) -> io::Result<()> {
        use std::os::windows::io::AsRawHandle;
        win::verify_server_is_current_user(client.as_raw_handle())
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
        pipe_name_of_key(&endpoint_key(path))
    }

    fn pipe_name_of_key(key: &str) -> String {
        use sha2::{Digest, Sha256};
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
        normalize_key(&resolved.to_string_lossy())
    }

    /// The spelling rules of [`endpoint_key`] once the file system has
    /// resolved the path: `\` separators, no `\\?\` (`\\?\UNC\` → `\\`),
    /// lowercase.
    fn normalize_key(resolved: &str) -> String {
        let s = resolved.replace('/', "\\");
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

        pub(super) fn sid_string(sid: PSID) -> io::Result<String> {
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

        /// This process's user SID as `S-1-5-…`.
        #[cfg(test)]
        pub fn current_user_sid() -> io::Result<String> {
            sid_string(TokenUserBuf::current()?.sid())
        }

        /// Refuse a pipe whose server process runs as another account.
        pub fn verify_server_is_current_user(pipe: std::os::windows::io::RawHandle) -> io::Result<()> {
            let ours = TokenUserBuf::current()?;
            verify_server_user(pipe, ours.sid())
        }

        /// Refuse a pipe whose server process does not run as `ours`. Split
        /// out so a test can play "another account" without a second one.
        pub fn verify_server_user(pipe: std::os::windows::io::RawHandle, ours: PSID) -> io::Result<()> {
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
            // SAFETY: two valid SIDs.
            if unsafe { EqualSid(ours, theirs.sid()) } == 0 {
                return Err(refuse("which runs as another user".into()));
            }
            Ok(())
        }
    }

    #[cfg(test)]
    mod security_tests {
        use super::*;
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use std::ptr::{null, null_mut};

        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use windows_sys::Win32::Foundation::{LocalFree, GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Security::Authorization::{
            ConvertSecurityDescriptorToStringSecurityDescriptorW, ConvertStringSidToSidW, GetSecurityInfo, SDDL_REVISION_1,
            SE_KERNEL_OBJECT,
        };
        use windows_sys::Win32::Security::{
            GetTokenInformation, RevertToSelf, SecurityIdentification, TokenImpersonationLevel, DACL_SECURITY_INFORMATION,
            LABEL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, TOKEN_QUERY,
        };
        use windows_sys::Win32::Storage::FileSystem::{CreateFileW, OPEN_EXISTING, READ_CONTROL};
        use windows_sys::Win32::System::Pipes::ImpersonateNamedPipeClient;
        use windows_sys::Win32::System::Threading::{GetCurrentThread, OpenThreadToken};

        fn endpoint() -> (tempfile::TempDir, std::path::PathBuf) {
            let d = tempfile::tempdir().unwrap();
            let p = d.path().join("daemon.sock");
            (d, p)
        }

        fn wide(s: &str) -> Vec<u16> {
            s.encode_utf16().chain([0]).collect()
        }

        /// A raw, synchronous client open (no tokio), what any local program
        /// can do. `flags` 0 is "no SQOS" (.NET's default, libuv);
        /// `SECURITY_SQOS_PRESENT | …` picks an impersonation level.
        fn open_raw(name: &str, access: u32, flags: u32) -> io::Result<OwnedHandle> {
            let n = wide(name);
            // SAFETY: NUL-terminated name; no security attributes or template.
            let h = unsafe { CreateFileW(n.as_ptr(), access, 0, null(), OPEN_EXISTING, flags, null_mut()) };
            if h == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: a handle we just opened and now own.
            Ok(unsafe { OwnedHandle::from_raw_handle(h) })
        }

        /// A SID from its string form (aliases such as `SY`, `LA`, `HI` too).
        struct Sid(PSID);
        impl Sid {
            fn parse(s: &str) -> Self {
                let w = wide(s);
                let mut p: PSID = null_mut();
                // SAFETY: NUL-terminated string; p receives a LocalAlloc'd SID.
                assert_ne!(unsafe { ConvertStringSidToSidW(w.as_ptr(), &mut p) }, 0, "{s}: {}", io::Error::last_os_error());
                Self(p)
            }
        }
        impl Drop for Sid {
            fn drop(&mut self) {
                // SAFETY: allocated by ConvertStringSidToSidW.
                unsafe { LocalFree(self.0) };
            }
        }

        /// `S-1-…` for a SID string or an SDDL alias.
        fn canon(s: &str) -> String {
            win::sid_string(Sid::parse(s).0).unwrap()
        }

        /// The pipe name's DACL and mandatory label (NPFS keeps one
        /// descriptor per name), read through a client handle, as SDDL.
        fn sddl_of(h: &OwnedHandle) -> String {
            let info = DACL_SECURITY_INFORMATION | LABEL_SECURITY_INFORMATION;
            let mut sd: PSECURITY_DESCRIPTOR = null_mut();
            // SAFETY: a handle opened with READ_CONTROL; sd receives a LocalAlloc'd descriptor.
            let rc = unsafe {
                GetSecurityInfo(h.as_raw_handle() as HANDLE, SE_KERNEL_OBJECT, info, null_mut(), null_mut(), null_mut(), null_mut(), &mut sd)
            };
            assert_eq!(rc, 0, "GetSecurityInfo: {}", io::Error::from_raw_os_error(rc as i32));
            let mut s: *mut u16 = null_mut();
            // SAFETY: a valid descriptor; s receives a LocalAlloc'd string.
            let ok = unsafe { ConvertSecurityDescriptorToStringSecurityDescriptorW(sd, SDDL_REVISION_1, info, &mut s, null_mut()) };
            assert_ne!(ok, 0, "{}", io::Error::last_os_error());
            // SAFETY: the NUL-terminated wide string from the call above.
            let out = unsafe {
                let len = (0..).take_while(|&i| *s.add(i) != 0).count();
                String::from_utf16_lossy(std::slice::from_raw_parts(s, len))
            };
            // SAFETY: both were LocalAlloc'd above.
            unsafe {
                LocalFree(s.cast());
                LocalFree(sd);
            }
            out
        }

        /// `D:P`, exactly two allow ACEs, for SYSTEM and this user.
        fn assert_owner_only_dacl(sddl: &str) {
            let dacl = sddl.split("S:").next().unwrap();
            assert!(dacl.starts_with("D:P"), "protected DACL: {sddl}");
            let aces: Vec<&str> = dacl.split('(').skip(1).map(|a| a.trim_end_matches(')')).collect();
            assert_eq!(aces.len(), 2, "exactly two ACEs: {sddl}");
            assert!(aces.iter().all(|a| a.starts_with("A;")), "allow ACEs only: {sddl}");
            let mut trustees: Vec<String> = aces.iter().map(|a| canon(a.rsplit(';').next().unwrap())).collect();
            trustees.sort();
            let mut want = vec!["S-1-5-18".to_string(), win::current_user_sid().unwrap()];
            want.sort();
            assert_eq!(trustees, want, "{sddl}");
        }

        /// §5 row 1: the name's owner-only DACL, read through the first
        /// instance and again through its replacement after one accept.
        #[tokio::test]
        async fn the_owner_only_dacl_persists_while_instances_are_replaced() {
            let (_d, path) = endpoint();
            let name = pipe_name(&path);
            let mut l = Listener::bind(&path).unwrap();
            let first = open_raw(&name, GENERIC_READ | READ_CONTROL, 0).unwrap();
            assert_owner_only_dacl(&sddl_of(&first));
            // Accepting puts the replacement instance up (`Listener::instance`).
            let _served = l.accept().await.unwrap();
            let second = open_raw(&name, GENERIC_READ | READ_CONTROL, 0).unwrap();
            assert_owner_only_dacl(&sddl_of(&second));
        }

        /// §5 row 2: a client refuses a server that runs as another account.
        /// SYSTEM plays "ours", so this process's own server is the stranger.
        /// The accepted end stays alive during the checks (else ERROR_NO_DATA).
        #[tokio::test]
        async fn a_server_of_another_account_is_refused() {
            let (_d, path) = endpoint();
            let mut l = Listener::bind(&path).unwrap();
            let client = ClientOptions::new().open(pipe_name(&path)).unwrap();
            let _served = l.accept().await.unwrap();
            let system = Sid::parse("S-1-5-18");
            let e = win::verify_server_user(client.as_raw_handle(), system.0).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
            assert!(e.to_string().contains("runs as another user"), "{e}");
            win::verify_server_is_current_user(client.as_raw_handle()).unwrap();
        }

        /// §5 row 4: `reject_remote_clients`. The SMB loopback counts as remote.
        #[tokio::test]
        async fn a_remote_client_is_refused() {
            let remote = |local: &str| local.replacen(r"\\.\", r"\\127.0.0.1\", 1);
            // Positive control: a pipe that accepts remote clients opens through the loopback.
            let (_c, control) = endpoint();
            let control = pipe_name(&control);
            let _open = ServerOptions::new().first_pipe_instance(true).reject_remote_clients(false).create(&control).unwrap();
            if let Err(e) = ClientOptions::new().open(remote(&control)) {
                eprintln!("skipped: no SMB loopback to \\\\127.0.0.1\\pipe ({e}); is the Server service (LanmanServer) running?");
                return;
            }
            let (_d, path) = endpoint();
            let _l = Listener::bind(&path).unwrap();
            let local = pipe_name(&path);
            assert!(ClientOptions::new().open(remote(&local)).is_err(), "a remote open of {local} must fail");
            ClientOptions::new().open(&local).unwrap();
        }

        /// Impersonate the pipe's client on this thread, read the thread
        /// token's impersonation level, revert. Nothing awaits in between.
        fn client_impersonation_level(server: &NamedPipeServer) -> io::Result<i32> {
            // SAFETY: a connected server pipe handle.
            if unsafe { ImpersonateNamedPipeClient(server.as_raw_handle() as HANDLE) } == 0 {
                return Err(io::Error::last_os_error());
            }
            let level = (|| {
                let mut token: HANDLE = null_mut();
                // SAFETY: OpenAsSelf = TRUE: an Identification token can't open itself as the client.
                if unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut token) } == 0 {
                    return Err(io::Error::last_os_error());
                }
                // SAFETY: a token handle we own.
                let token = unsafe { OwnedHandle::from_raw_handle(token) };
                let (mut level, mut len) = (0i32, 0u32);
                // SAFETY: level is a SECURITY_IMPERSONATION_LEVEL (an i32).
                if unsafe { GetTokenInformation(token.as_raw_handle() as HANDLE, TokenImpersonationLevel, (&mut level as *mut i32).cast(), 4, &mut len) } == 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(level)
            })();
            // SAFETY: ends the impersonation started above.
            if unsafe { RevertToSelf() } == 0 {
                std::process::abort();
            }
            level
        }

        /// §5 row 5: tokio's client lets the daemon identify it, never act as
        /// it. Runs on the test's own thread (current-thread runtime), with
        /// no await between impersonating and reverting.
        #[tokio::test]
        async fn the_server_can_identify_but_not_impersonate_a_client() {
            let (_d, path) = endpoint();
            let mut l = Listener::bind(&path).unwrap();
            let client = connect(&path).await.unwrap();
            let mut server = l.accept().await.unwrap();
            let (_cr, mut cw) = split_client(client);
            cw.write_all(b"x").await.unwrap();
            let mut b = [0u8; 1];
            server.read_exact(&mut b).await.unwrap();
            assert_eq!(client_impersonation_level(&server).unwrap(), SecurityIdentification);
        }
    }

    /// Third-party clients derive these names (the pipe has no other
    /// discovery), so they are a contract: a change here breaks them.
    #[cfg(test)]
    mod golden_tests {
        use super::*;

        const DAEMON: &str = r"\\.\pipe\synaps-daemon-149c48c0c65b7de4dce27f2e01cf19eb";

        #[test]
        fn daemon_pipe_names_match_the_published_vectors() {
            for spelling in [
                r"C:\Users\Me\.synaps-cli\run\daemon.sock",
                "c:/users/me/.synaps-cli/run/DAEMON.sock",
                r"\\?\C:\Users\Me\.synaps-cli\run\daemon.sock",
            ] {
                assert_eq!(pipe_name(Path::new(spelling)), DAEMON, "{spelling}");
            }
            assert_eq!(
                pipe_name(Path::new(r"C:\Users\Me\.synaps-cli\run\daemon-work.sock")),
                r"\\.\pipe\synaps-daemon-1738620a231da3350250d3e8e0d36581"
            );
            assert_eq!(
                pipe_name(Path::new("C:\\Users\\\u{c9}LODIE\\.synaps-cli\\run\\daemon.sock")),
                r"\\.\pipe\synaps-daemon-a14db3bda4b8487626a820b0bf42be30"
            );
        }

        /// UNC through the pure half: `canonicalize` on a UNC dir would go to the network.
        #[test]
        fn a_unc_endpoint_is_keyed_without_the_verbatim_prefix() {
            let key = normalize_key(r"\\?\UNC\server\share\run\daemon.sock");
            assert_eq!(key, r"\\server\share\run\daemon.sock");
            assert_eq!(pipe_name_of_key(&key), r"\\.\pipe\synaps-daemon-89cd23ff91040f8e924ae5f009e8945c");
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
