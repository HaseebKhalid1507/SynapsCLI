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
// The Windows security tests' token helpers, for `events::socket`'s tests.
#[cfg(all(test, windows))]
pub(crate) use imp::security_tests;

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

    /// An account and its integrity level: who a daemon or a client runs as.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Identity {
        /// The token user's SID (`S-1-5-…`).
        pub sid: String,
        /// The mandatory label's RID: 0x1000 low, 0x2000 medium, 0x3000
        /// high (elevated), 0x4000 system.
        pub integrity: u32,
        /// A restricted token (restricting SIDs) or an AppContainer token: a
        /// sandboxed process that carries its user's SID and integrity level
        /// but acts with less than that user's rights.
        pub confined: bool,
    }

    /// The raw handle of an accepted pipe, kept by `conn::serve` before it
    /// splits the stream so it can identify the client after `Hello`. Valid
    /// while either half of the stream lives.
    #[derive(Clone, Copy)]
    pub struct PeerHandle(std::os::windows::io::RawHandle);

    // SAFETY: a kernel handle value, usable from any thread; it is only
    // passed to `client_identity` while the stream that owns it is alive.
    unsafe impl Send for PeerHandle {}

    impl PeerHandle {
        pub fn of(server: &NamedPipeServer) -> Self {
            use std::os::windows::io::AsRawHandle;
            Self(server.as_raw_handle())
        }
    }

    /// This process's identity.
    pub fn own_identity() -> io::Result<Identity> {
        win::current_process_identity()
    }

    /// The identity of the client on an accepted pipe: the token it
    /// presented, read by impersonating it (so no PID-reuse race). Call it
    /// only after reading from the pipe: a client opened without SQOS flags
    /// can't be impersonated until it has written, and is then identified as
    /// the thread that wrote the last bytes read.
    pub fn client_identity(peer: PeerHandle) -> io::Result<Identity> {
        win::client_identity(peer.0)
    }

    /// The Windows `admit`, run by `conn::serve` after `Hello` and before
    /// `Welcome`. The client must be the daemon's user, at the daemon's
    /// integrity level or higher, so an elevated daemon serves no medium
    /// process of its user (on Unix, elevation changes the uid and the uid
    /// check covers it), and unconfined (no restricted or AppContainer
    /// token). SYSTEM is refused like root. An Anonymous-level
    /// client can't be read, and anything unreadable refuses the connection.
    pub fn admit_client(peer: PeerHandle, own: &io::Result<Identity>) -> Result<(), String> {
        let own = own.as_ref().map_err(|e| format!("daemon identity unreadable: {e}"))?;
        let who = client_identity(peer).map_err(|e| format!("peer identity unreadable: {e}"))?;
        admit_identity(own, &who)
    }

    fn admit_identity(own: &Identity, peer: &Identity) -> Result<(), String> {
        if peer.sid != own.sid {
            return Err(format!("peer {} is not the daemon's user {}", peer.sid, own.sid));
        }
        if peer.integrity < own.integrity {
            return Err(format!("peer integrity {:#x} is below the daemon's {:#x}", peer.integrity, own.integrity));
        }
        // A sandboxed client of the daemon's user would otherwise have the
        // daemon run its tools with the user's full token.
        if peer.confined {
            return Err("peer runs with a restricted or AppContainer token".to_string());
        }
        Ok(())
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
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
            SE_KERNEL_OBJECT,
        };
        use windows_sys::Win32::Security::{
            EqualSid, GetAce, GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, RevertToSelf,
            TokenIntegrityLevel, TokenIsAppContainer, TokenIsRestricted, TokenUser, ACE_HEADER, ACL, LABEL_SECURITY_INFORMATION,
            PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, TOKEN_INFORMATION_CLASS, TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
            TOKEN_USER,
        };
        use windows_sys::Win32::System::Pipes::{GetNamedPipeServerProcessId, ImpersonateNamedPipeClient};
        use windows_sys::Win32::System::Threading::{
            GetCurrentProcess, GetCurrentThread, OpenProcess, OpenProcessToken, OpenThreadToken,
            PROCESS_QUERY_LIMITED_INFORMATION,
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

        /// Security descriptor for daemon pipes: a protected DACL granting
        /// full access to this user and SYSTEM only (the default pipe DACL
        /// also grants Everyone read), and a no-write-up label at this
        /// process's integrity level. NPFS keeps the first instance's
        /// descriptor for the whole pipe name.
        pub struct PipeSecurity(PSECURITY_DESCRIPTOR);

        // SAFETY: an immutable, self-contained LocalAlloc'd descriptor.
        unsafe impl Send for PipeSecurity {}
        unsafe impl Sync for PipeSecurity {}

        impl PipeSecurity {
            pub fn current_user_only() -> io::Result<Self> {
                let me = current_process_identity()?;
                Self::labelled(&me.sid, me.integrity)
            }

            /// Owner-only DACL for `sid`, plus a no-write-up, no-read-up label at
            /// `integrity`: a lower-integrity process of this user can't open
            /// the pipe for writing (`admit_client` checks the rest), and a
            /// client can tell what integrity the server was created at
            /// (`verify_server_integrity`). Split out so a test can create a
            /// pipe as a lower-integrity squatter would.
            pub fn labelled(sid: &str, integrity: u32) -> io::Result<Self> {
                let sddl = format!("D:P(A;;GA;;;SY)(A;;GA;;;{sid})S:(ML;;NWNR;;;S-1-16-{integrity})");
                let sddl: Vec<u16> = sddl.encode_utf16().chain([0]).collect();
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

        /// Refuse a pipe whose server process runs as another account, or
        /// that was created below this client's integrity (at most Medium: a
        /// High client can't demand a High server, but never talks to a Low
        /// one). A same-user process at lower integrity can otherwise squat
        /// the name before the daemon starts and read what the client sends.
        pub fn verify_server_is_current_user(pipe: std::os::windows::io::RawHandle) -> io::Result<()> {
            let ours = TokenUserBuf::current()?;
            verify_server_user(pipe, ours.sid())?;
            verify_server_integrity(pipe, current_process_identity()?.integrity.min(0x2000))
        }

        /// The mandatory label's RID on the pipe's own descriptor (NPFS keeps
        /// the first instance's for the whole name), read through the client's
        /// handle (`READ_CONTROL`): no process id to race. `None`: no label ACE.
        pub(super) fn pipe_label(pipe: HANDLE) -> io::Result<Option<u32>> {
            let mut sd: PSECURITY_DESCRIPTOR = null_mut();
            let mut sacl: *mut ACL = null_mut();
            // SAFETY: a connected client pipe handle opened with READ_CONTROL; sacl points into sd.
            let rc = unsafe {
                GetSecurityInfo(pipe, SE_KERNEL_OBJECT, LABEL_SECURITY_INFORMATION, null_mut(), null_mut(), null_mut(), &mut sacl, &mut sd)
            };
            if rc != 0 {
                return Err(io::Error::from_raw_os_error(rc as i32));
            }
            let label = (|| -> io::Result<Option<u32>> {
                if sacl.is_null() {
                    return Ok(None);
                }
                // SAFETY: a valid ACL, owned by sd, which lives until after this closure.
                let acl_end = sacl as usize + usize::from(unsafe { (*sacl).AclSize });
                // SAFETY: as above.
                for i in 0..u32::from(unsafe { (*sacl).AceCount }) {
                    let mut ace: *mut c_void = null_mut();
                    // SAFETY: i < AceCount; ace receives a pointer into the ACL.
                    if unsafe { GetAce(sacl, i, &mut ace) } == 0 {
                        return Err(io::Error::last_os_error());
                    }
                    // SAFETY: every ACE starts with an ACE_HEADER.
                    let size = usize::from(unsafe { (*(ace as *const ACE_HEADER)).AceSize });
                    // The ACE's bytes, never past the end of the ACL.
                    let len = size.min(acl_end.saturating_sub(ace as usize));
                    // SAFETY: `len` bytes from an ACE inside the ACL, which sd owns.
                    let bytes = unsafe { std::slice::from_raw_parts(ace as *const u8, len) };
                    if let Some(rid) = label_rid(bytes)? {
                        return Ok(Some(rid));
                    }
                }
                Ok(None)
            })();
            // SAFETY: allocated by GetSecurityInfo.
            unsafe { LocalFree(sd) };
            label
        }

        /// The integrity RID one ACE of a SACL carries. `Ok(None)`: not a
        /// label ACE, or one that is `INHERIT_ONLY` (it labels children, not
        /// the pipe). A label ACE is its 4-byte header, a 4-byte mask and the
        /// label SID `S-1-16-<rid>` (revision 1, one sub-authority, authority 16),
        /// and anything else is an error: the bytes come from the kernel, and
        /// are still checked before they are trusted.
        pub(super) fn label_rid(ace: &[u8]) -> io::Result<Option<u32>> {
            /// `SYSTEM_MANDATORY_LABEL_ACE_TYPE` (winnt.h); its windows-sys
            /// constant needs the `Win32_System_SystemServices` feature.
            const LABEL_ACE: u8 = 17;
            const INHERIT_ONLY_ACE: u8 = 0x08;
            const SID_OFFSET: usize = 8;
            let bad = |why: &str| io::Error::other(format!("malformed integrity label ACE: {why}"));
            if ace.len() < 4 {
                return Err(bad("shorter than its header"));
            }
            if ace[0] != LABEL_ACE || ace[1] & INHERIT_ONLY_ACE != 0 {
                return Ok(None);
            }
            let size = usize::from(u16::from_le_bytes([ace[2], ace[3]]));
            // The header, the mask, and a SID with one sub-authority (8 + 4 bytes).
            if size < SID_OFFSET + 12 || size > ace.len() {
                return Err(bad("its size does not hold the label SID"));
            }
            let sid = &ace[SID_OFFSET..size];
            if sid[0] != 1 || sid[1] != 1 || sid[2..8] != [0, 0, 0, 0, 0, 16] {
                return Err(bad("not an S-1-16-<rid> SID"));
            }
            Ok(Some(u32::from_le_bytes([sid[8], sid[9], sid[10], sid[11]])))
        }

        /// Refuse a pipe created below integrity `min`, or whose label can't
        /// be read, or that has none: our own pipes always carry one, and a
        /// lower-integrity process can't label its pipe above itself.
        pub fn verify_server_integrity(pipe: std::os::windows::io::RawHandle, min: u32) -> io::Result<()> {
            let refuse = |why: String| {
                io::Error::new(io::ErrorKind::PermissionDenied, format!("daemon pipe {why}; refusing to connect"))
            };
            match pipe_label(pipe as HANDLE) {
                Err(e) => Err(refuse(format!("has an integrity label that cannot be read ({e})"))),
                Ok(None) => Err(refuse("carries no integrity label (not a pipe this daemon created)".into())),
                Ok(Some(rid)) if rid < min => {
                    Err(refuse(format!("was created at integrity {rid:#x}, below the {min:#x} this client requires")))
                }
                Ok(Some(_)) => Ok(()),
            }
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
            // The same user is not enough. The pipe's DACL grants this user's
            // SID, which is the restricting SID a sandboxed process of this user
            // carries, and a new instance of a name gets the label of the
            // first: such a process can add an instance to a live daemon's
            // name, or take the name before one starts, and receive what
            // clients send. `GetNamedPipeServerProcessId` names the creator of
            // this very instance.
            let theirs = process_identity(process.0).map_err(|e| refuse(format!("whose token cannot be read ({e})")))?;
            verify_server_unconfined(pid, &theirs)
        }

        /// Refuse a server whose token is restricted or AppContainer
        /// (`admit_client`, the other way round).
        pub(super) fn verify_server_unconfined(pid: u32, theirs: &super::Identity) -> io::Result<()> {
            if theirs.confined {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("daemon pipe is served by pid {pid}, which runs with a restricted or AppContainer token; refusing to connect"),
                ));
            }
            Ok(())
        }

        /// One `GetTokenInformation` result (8-aligned: it holds pointers).
        fn token_info(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> io::Result<Vec<u64>> {
            let mut len = 0u32;
            // SAFETY: size query (fails with ERROR_INSUFFICIENT_BUFFER, sets len).
            unsafe { GetTokenInformation(token, class, null_mut(), 0, &mut len) };
            if len == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut buf = vec![0u64; (len as usize).div_ceil(8)];
            // SAFETY: buf holds at least `len` bytes.
            if unsafe { GetTokenInformation(token, class, buf.as_mut_ptr().cast(), len, &mut len) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(buf)
        }

        /// A token's user SID, integrity RID, and whether it is confined.
        fn token_identity(token: HANDLE) -> io::Result<super::Identity> {
            let user = token_info(token, TokenUser)?;
            // SAFETY: filled by GetTokenInformation(TokenUser).
            let sid = sid_string(unsafe { (*(user.as_ptr() as *const TOKEN_USER)).User.Sid })?;
            let label = token_info(token, TokenIntegrityLevel)?;
            // SAFETY: filled by GetTokenInformation(TokenIntegrityLevel); a
            // label SID (S-1-16-<rid>) ends with its RID.
            let integrity = unsafe {
                let il = (*(label.as_ptr() as *const TOKEN_MANDATORY_LABEL)).Label.Sid;
                let n = *GetSidSubAuthorityCount(il);
                if n == 0 {
                    return Err(io::Error::other("integrity label without a RID"));
                }
                *GetSidSubAuthority(il, u32::from(n) - 1)
            };
            // A failed query fails the identity: closed, never open.
            let confined = token_is_restricted(token)? || token_dword(token, TokenIsAppContainer)? != 0;
            Ok(super::Identity { sid, integrity, confined })
        }

        /// One DWORD of token information. A failed query is an error, never a 0.
        fn token_dword(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> io::Result<u32> {
            let (mut value, mut len) = (0u32, 0u32);
            // SAFETY: the classes asked for (TokenIsRestricted, TokenIsAppContainer) are DWORDs.
            if unsafe { GetTokenInformation(token, class, (&mut value as *mut u32).cast(), 4, &mut len) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(value)
        }

        /// Whether a token has restricting SIDs. `TokenIsRestricted` through
        /// `GetTokenInformation`, not `IsTokenRestricted` (which returns FALSE
        /// on a failed query as well, so an error would read as "unrestricted")
        /// and not `TokenHasRestrictions` (the UAC-filtered token of a
        /// non-elevated administrator has it, and is an ordinary client). A
        /// restricted token passes every access check as the user's SID, so
        /// only this query tells it apart.
        pub(super) fn token_is_restricted(token: HANDLE) -> io::Result<bool> {
            token_dword(token, TokenIsRestricted).map(|v| v != 0)
        }

        /// The identity of a process, from its token (`TOKEN_QUERY`).
        fn process_identity(process: HANDLE) -> io::Result<super::Identity> {
            let mut token: HANDLE = null_mut();
            // SAFETY: a valid process handle and an out-pointer.
            if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
                return Err(io::Error::last_os_error());
            }
            let token = Handle(token);
            token_identity(token.0)
        }

        pub fn current_process_identity() -> io::Result<super::Identity> {
            // SAFETY: the pseudo-handle needs no close.
            process_identity(unsafe { GetCurrentProcess() })
        }

        /// Ends this thread's impersonation when dropped, on every path out.
        /// A failed revert aborts: a worker thread must never keep a
        /// client's identity.
        struct RevertOnDrop;
        impl Drop for RevertOnDrop {
            fn drop(&mut self) {
                // SAFETY: created right after a successful impersonation on this thread.
                if unsafe { RevertToSelf() } == 0 {
                    std::process::abort();
                }
            }
        }

        /// Impersonate the pipe's client on this thread, read its token,
        /// revert. Synchronous: nothing else runs on this thread while it
        /// wears the client's identity.
        pub fn client_identity(pipe: std::os::windows::io::RawHandle) -> io::Result<super::Identity> {
            // SAFETY: a connected server pipe handle.
            if unsafe { ImpersonateNamedPipeClient(pipe as HANDLE) } == 0 {
                return Err(io::Error::last_os_error());
            }
            // Declared before `token`, so it drops (reverts) after the token closes.
            let _revert = RevertOnDrop;
            let mut token: HANDLE = null_mut();
            // SAFETY: OpenAsSelf = TRUE: the client's token is at
            // Identification level and can't open itself. An Anonymous-level
            // client fails here (ERROR_CANT_OPEN_ANONYMOUS).
            if unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut token) } == 0 {
                return Err(io::Error::last_os_error());
            }
            let token = Handle(token);
            token_identity(token.0)
        }
    }

    #[cfg(test)]
    // `pub(crate)`: `events::socket`'s tests wear the same tokens.
    pub(crate) mod security_tests {
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

        use std::io::Write;

        use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, GENERIC_WRITE};
        use windows_sys::Win32::Security::{
            CreateRestrictedToken, ImpersonateAnonymousToken, DuplicateTokenEx, GetLengthSid, SetTokenInformation, SecurityImpersonation, TokenImpersonation,
            TokenIntegrityLevel, SID_AND_ATTRIBUTES, TOKEN_ADJUST_DEFAULT, TOKEN_DUPLICATE, TOKEN_IMPERSONATE,
            TOKEN_MANDATORY_LABEL,
        };
        use windows_sys::Win32::Storage::FileSystem::{SECURITY_ANONYMOUS, SECURITY_SQOS_PRESENT};
        use windows_sys::Win32::System::Threading::{
            CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess, OpenProcessToken, SetThreadToken, TerminateProcess,
            CREATE_NO_WINDOW, PROCESS_INFORMATION, STARTUPINFOW,
        };

        fn endpoint() -> (tempfile::TempDir, std::path::PathBuf) {
            let d = tempfile::tempdir().unwrap();
            let p = d.path().join("daemon.sock");
            (d, p)
        }

        /// A test that can't run on this machine says so. Under CI (`CI=true`,
        /// which GitHub Actions sets) that is a failure: a check that silently
        /// doesn't run protects nothing.
        fn skipped(why: &str) {
            assert!(std::env::var("CI").as_deref() != Ok("true"), "skipped under CI: {why}");
            eprintln!("skipped: {why}");
        }

        fn wide(s: &str) -> Vec<u16> {
            s.encode_utf16().chain([0]).collect()
        }

        /// A raw, synchronous client open (no tokio), what any local program
        /// can do. `flags` 0 is "no SQOS" (.NET's default, libuv);
        /// `SECURITY_SQOS_PRESENT | …` picks an impersonation level.
        pub(crate) fn open_raw(name: &str, access: u32, flags: u32) -> io::Result<OwnedHandle> {
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

        /// The name's owner-only DACL, read through the first
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

        /// A client refuses a server that runs as another account.
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

        /// `reject_remote_clients`. The SMB loopback counts as remote.
        #[tokio::test]
        async fn a_remote_client_is_refused() {
            let remote = |local: &str| local.replacen(r"\\.\", r"\\127.0.0.1\", 1);
            // Positive control: a pipe that accepts remote clients opens through the loopback.
            let (_c, control) = endpoint();
            let control = pipe_name(&control);
            let _open = ServerOptions::new().first_pipe_instance(true).reject_remote_clients(false).create(&control).unwrap();
            if let Err(e) = ClientOptions::new().open(remote(&control)) {
                skipped(&format!("no SMB loopback to \\\\127.0.0.1\\pipe ({e}); is the Server service (LanmanServer) running?"));
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

        /// Tokio's client lets the daemon identify it, never act as
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

        /// `SE_GROUP_INTEGRITY` (winnt.h); its windows-sys constant needs the
        /// `Win32_System_SystemServices` feature, which this crate doesn't enable.
        const SE_GROUP_INTEGRITY: u32 = 0x20;

        /// Stands in for the `Hello` line: `admit_client` doesn't parse it,
        /// `conn::serve` has already done that when it calls it.
        const HELLO: &[u8] = b"{\"hello\":1}\n";

        async fn read_hello(server: &mut NamedPipeServer) {
            let mut b = vec![0u8; HELLO.len()];
            server.read_exact(&mut b).await.unwrap();
            assert_eq!(b, HELLO);
        }

        /// A synchronous write on a raw client handle, from the calling thread.
        pub(crate) fn write_line(client: &OwnedHandle, line: &[u8]) -> io::Result<()> {
            std::fs::File::from(client.try_clone()?).write_all(line)
        }

        /// This process's token, duplicated for impersonation and lowered to
        /// integrity `rid` (lowering needs no privilege).
        fn lowered_token(rid: u32) -> io::Result<OwnedHandle> {
            let mut process_token: HANDLE = null_mut();
            // SAFETY: the pseudo-handle and an out-pointer.
            if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_DUPLICATE | TOKEN_QUERY, &mut process_token) } == 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: a token handle we own.
            let process_token = unsafe { OwnedHandle::from_raw_handle(process_token) };
            let mut dup: HANDLE = null_mut();
            let access = TOKEN_QUERY | TOKEN_IMPERSONATE | TOKEN_ADJUST_DEFAULT;
            // SAFETY: a valid token; dup receives an impersonation token a thread can wear.
            if unsafe { DuplicateTokenEx(process_token.as_raw_handle() as HANDLE, access, null(), SecurityImpersonation, TokenImpersonation, &mut dup) } == 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: a token handle we own.
            let dup = unsafe { OwnedHandle::from_raw_handle(dup) };
            let il = Sid::parse(&format!("S-1-16-{rid}"));
            let label = TOKEN_MANDATORY_LABEL { Label: SID_AND_ATTRIBUTES { Sid: il.0, Attributes: SE_GROUP_INTEGRITY } };
            // SAFETY: label and its SID outlive the call; the length covers the struct and the SID it points to.
            let len = std::mem::size_of::<TOKEN_MANDATORY_LABEL>() as u32 + unsafe { GetLengthSid(il.0) };
            // SAFETY: as above.
            if unsafe { SetTokenInformation(dup.as_raw_handle() as HANDLE, TokenIntegrityLevel, (&label as *const TOKEN_MANDATORY_LABEL).cast(), len) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(dup)
        }

        /// Run `f` on a fresh thread wearing this process's token one
        /// integrity level lower (High → Medium, Medium → Low). Returns the
        /// lowered RID and `f`'s result; `None` when already at Low.
        fn as_lower_integrity<T: Send>(f: impl FnOnce() -> T + Send) -> Option<(u32, T)> {
            let own = own_identity().unwrap().integrity;
            if own < 0x2000 {
                return None;
            }
            let lower = (own & !0xfff) - 0x1000;
            Some(std::thread::scope(|s| {
                s.spawn(move || {
                    let token = lowered_token(lower).unwrap();
                    // SAFETY: null = the calling thread; a token with TOKEN_IMPERSONATE.
                    assert_ne!(unsafe { SetThreadToken(null(), token.as_raw_handle() as HANDLE) }, 0, "{}", io::Error::last_os_error());
                    let out = f();
                    // SAFETY: ends this thread's impersonation.
                    if unsafe { RevertToSelf() } == 0 {
                        std::process::abort();
                    }
                    (lower, out)
                })
                .join()
                .unwrap()
            }))
        }

        /// The mandatory-label ACE, wherever the SACL puts it (`S:(ML;…)`,
        /// or `S:AI(ML;…)` once the label is inherited).
        fn assert_label(sddl: &str, rid: u32) {
            let start = sddl.find("(ML;").unwrap_or_else(|| panic!("no label: {sddl}")) + 1;
            let end = start + sddl[start..].find(')').unwrap_or_else(|| panic!("unclosed label: {sddl}"));
            let ace: Vec<&str> = sddl[start..end].split(';').collect();
            assert_eq!(ace.len(), 6, "{sddl}");
            assert_eq!(ace[..5], ["ML", "", "NWNR", "", ""], "{sddl}");
            assert_eq!(canon(ace[5]), format!("S-1-16-{rid}"), "{sddl}");
        }

        /// The pipe name's descriptor carries the
        /// daemon's integrity label, read through the first instance and
        /// again through its replacement.
        #[tokio::test]
        async fn the_pipe_name_carries_the_daemons_integrity_label() {
            let (_d, path) = endpoint();
            let name = pipe_name(&path);
            let rid = own_identity().unwrap().integrity;
            let mut l = Listener::bind(&path).unwrap();
            let first = open_raw(&name, GENERIC_READ | READ_CONTROL, 0).unwrap();
            assert_label(&sddl_of(&first), rid);
            let _served = l.accept().await.unwrap();
            let second = open_raw(&name, GENERIC_READ | READ_CONTROL, 0).unwrap();
            assert_label(&sddl_of(&second), rid);
            assert_owner_only_dacl(&sddl_of(&second));
        }

        /// Tokio's client (SQOS, Identification) is admitted after `Hello`.
        #[tokio::test]
        async fn an_sqos_client_is_admitted_after_hello() {
            let (_d, path) = endpoint();
            let mut l = Listener::bind(&path).unwrap();
            let client = connect(&path).await.unwrap();
            let mut server = l.accept().await.unwrap();
            let (_cr, mut cw) = split_client(client);
            cw.write_all(HELLO).await.unwrap();
            read_hello(&mut server).await;
            let peer = PeerHandle::of(&server);
            assert_eq!(client_identity(peer).unwrap(), own_identity().unwrap());
            assert_eq!(admit_client(peer, &own_identity()), Ok(()));
        }

        /// A client opened without SQOS flags (flags 0: .NET's
        /// default, libuv) is admitted after `Hello`, as the thread that wrote it.
        #[tokio::test]
        async fn a_client_without_sqos_is_admitted_after_hello() {
            let (_d, path) = endpoint();
            let name = pipe_name(&path);
            let mut l = Listener::bind(&path).unwrap();
            let client = open_raw(&name, GENERIC_READ | GENERIC_WRITE, 0).unwrap();
            let mut server = l.accept().await.unwrap();
            write_line(&client, HELLO).unwrap();
            read_hello(&mut server).await;
            let peer = PeerHandle::of(&server);
            assert_eq!(client_identity(peer).unwrap(), own_identity().unwrap());
            assert_eq!(admit_client(peer, &own_identity()), Ok(()));
        }

        /// A client opened at this process's level whose `Hello`
        /// is written from a lower-integrity thread is identified as that
        /// thread, and refused.
        #[tokio::test]
        async fn a_hello_written_at_lower_integrity_is_refused() {
            let (_d, path) = endpoint();
            let name = pipe_name(&path);
            let mut l = Listener::bind(&path).unwrap();
            let client = open_raw(&name, GENERIC_READ | GENERIC_WRITE, 0).unwrap();
            let mut server = l.accept().await.unwrap();
            let Some((lower, wrote)) = as_lower_integrity(|| write_line(&client, HELLO)) else {
                skipped("this process already runs at Low integrity");
                return;
            };
            wrote.unwrap();
            read_hello(&mut server).await;
            let peer = PeerHandle::of(&server);
            let who = client_identity(peer).unwrap();
            assert_eq!(who.sid, own_identity().unwrap().sid);
            assert_eq!(who.integrity, lower, "the identity of the thread that wrote Hello");
            let err = admit_client(peer, &own_identity()).unwrap_err();
            assert!(err.contains("below the daemon's"), "{err}");
        }

        /// A client that opened at Anonymous level can't be identified, so it is refused.
        #[tokio::test]
        async fn an_anonymous_client_is_refused() {
            let (_d, path) = endpoint();
            let name = pipe_name(&path);
            let mut l = Listener::bind(&path).unwrap();
            let client = open_raw(&name, GENERIC_READ | GENERIC_WRITE, SECURITY_SQOS_PRESENT | SECURITY_ANONYMOUS).unwrap();
            let mut server = l.accept().await.unwrap();
            write_line(&client, HELLO).unwrap();
            read_hello(&mut server).await;
            let err = admit_client(PeerHandle::of(&server), &own_identity()).unwrap_err();
            // Expected: OpenThreadToken's ERROR_CANT_OPEN_ANONYMOUS (os error 1347).
            eprintln!("anonymous client: {err}");
            assert!(err.contains("peer identity unreadable"), "{err}");
        }

        /// Another account is refused, mirroring Unix's `admit`.
        #[test]
        fn a_client_of_another_account_is_refused() {
            let own = own_identity().unwrap();
            let system = Identity { sid: "S-1-5-18".into(), integrity: 0x4000, confined: false };
            let err = admit_identity(&own, &system).unwrap_err();
            assert!(err.contains("is not the daemon's user"), "{err}");
        }

        #[test]
        fn a_lower_integrity_client_is_refused_and_a_higher_one_admitted() {
            let high = Identity { sid: "S-1-5-21-1-2-3-1001".into(), integrity: 0x3000, confined: false };
            let medium = Identity { integrity: 0x2000, ..high.clone() };
            let err = admit_identity(&high, &medium).unwrap_err();
            assert!(err.contains("below the daemon's"), "{err}");
            assert_eq!(admit_identity(&medium, &high), Ok(()));
        }

        #[test]
        fn a_confined_client_is_refused() {
            let own = own_identity().unwrap();
            let sandboxed = Identity { confined: true, ..own.clone() };
            let err = admit_identity(&own, &sandboxed).unwrap_err();
            assert!(err.contains("restricted or AppContainer"), "{err}");
        }

        /// The label stops a lower-integrity writer at open.
        #[tokio::test]
        async fn a_lower_integrity_writer_is_refused_at_open() {
            let (_d, path) = endpoint();
            let _l = Listener::bind(&path).unwrap();
            let name = pipe_name(&path);
            let Some((lower, opened)) = as_lower_integrity(|| open_raw(&name, GENERIC_READ | GENERIC_WRITE, 0)) else {
                skipped("this process already runs at Low integrity");
                return;
            };
            let e = opened.expect_err("a lower-integrity writer must be refused at open");
            assert_eq!(e.raw_os_error(), Some(ERROR_ACCESS_DENIED as i32), "integrity {lower:#x}: {e}");
        }

        /// Run `f` on a fresh thread that wears the impersonation token
        /// `make` builds, and reverts when `f` returns.
        pub(crate) fn wearing<T: Send>(make: impl FnOnce() -> io::Result<OwnedHandle> + Send, f: impl FnOnce() -> T + Send) -> T {
            std::thread::scope(|s| {
                s.spawn(move || {
                    let token = make().unwrap();
                    // SAFETY: null = the calling thread; a token with TOKEN_IMPERSONATE.
                    assert_ne!(unsafe { SetThreadToken(null(), token.as_raw_handle() as HANDLE) }, 0, "{}", io::Error::last_os_error());
                    let out = f();
                    // SAFETY: ends this thread's impersonation.
                    if unsafe { RevertToSelf() } == 0 {
                        std::process::abort();
                    }
                    out
                })
                .join()
                .unwrap()
            })
        }

        /// A primary token made from this process's own, restricted to this
        /// user's SID and `groups` (restricting SIDs): same user, same
        /// integrity, and every access check it faces is also run against
        /// those SIDs, so with the user's SID among them it passes the pipe's
        /// DACL and label. What `CreateRestrictedToken` sandboxes (browsers,
        /// Chrome-style launchers) build, and needs no privilege.
        fn restricted_primary(groups: &[&str]) -> io::Result<OwnedHandle> {
            let mut process_token: HANDLE = null_mut();
            // SAFETY: the pseudo-handle and an out-pointer.
            if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_DUPLICATE | TOKEN_QUERY, &mut process_token) } == 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: a token handle we own.
            let process_token = unsafe { OwnedHandle::from_raw_handle(process_token) };
            let user = win::current_user_sid()?;
            let sids: Vec<Sid> = std::iter::once(user.as_str()).chain(groups.iter().copied()).map(Sid::parse).collect();
            let restrict: Vec<SID_AND_ATTRIBUTES> = sids.iter().map(|s| SID_AND_ATTRIBUTES { Sid: s.0, Attributes: 0 }).collect();
            let mut restricted: HANDLE = null_mut();
            // SAFETY: a valid token with TOKEN_DUPLICATE; the restricting SIDs outlive the call.
            if unsafe {
                CreateRestrictedToken(process_token.as_raw_handle() as HANDLE, 0, 0, null(), 0, null(), restrict.len() as u32, restrict.as_ptr(), &mut restricted)
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: a token handle we own.
            Ok(unsafe { OwnedHandle::from_raw_handle(restricted) })
        }

        /// [`restricted_primary`] with only the user's SID, duplicated for
        /// impersonation: what a thread wears to act as such a client.
        pub(crate) fn restricted_token() -> io::Result<OwnedHandle> {
            let restricted = restricted_primary(&[])?;
            let mut dup: HANDLE = null_mut();
            // SAFETY: a valid token; dup receives an impersonation token a thread can wear.
            if unsafe { DuplicateTokenEx(restricted.as_raw_handle() as HANDLE, TOKEN_QUERY | TOKEN_IMPERSONATE, null(), SecurityImpersonation, TokenImpersonation, &mut dup) } == 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: a token handle we own.
            Ok(unsafe { OwnedHandle::from_raw_handle(dup) })
        }

        /// A client of this user and integrity level whose token carries a
        /// restricting SID passes the pipe's DACL and label, so only the
        /// daemon's `admit` can stop it: the daemon would otherwise run the
        /// client's tools with its own, unrestricted token.
        #[tokio::test]
        async fn a_restricted_token_client_is_refused() {
            let (_d, path) = endpoint();
            let name = pipe_name(&path);
            let mut l = Listener::bind(&path).unwrap();
            let client = open_raw(&name, GENERIC_READ | GENERIC_WRITE, 0).unwrap();
            let mut server = l.accept().await.unwrap();
            wearing(restricted_token, || write_line(&client, HELLO)).unwrap();
            read_hello(&mut server).await;
            let peer = PeerHandle::of(&server);
            let own = own_identity().unwrap();
            assert!(!own.confined, "the test process itself is not restricted");
            let who = client_identity(peer).unwrap();
            assert_eq!((&who.sid, who.integrity), (&own.sid, own.integrity), "only the restriction differs");
            assert!(who.confined, "{who:?}");
            let err = admit_client(peer, &Ok(own)).unwrap_err();
            assert!(err.contains("restricted or AppContainer"), "{err}");
        }

        /// A `Hello` written from a thread wearing the anonymous token is a
        /// different account (S-1-5-7): refused, whichever way it is read.
        #[tokio::test]
        async fn a_hello_written_under_the_anonymous_token_is_refused() {
            let (_d, path) = endpoint();
            let name = pipe_name(&path);
            let mut l = Listener::bind(&path).unwrap();
            let client = open_raw(&name, GENERIC_READ | GENERIC_WRITE, 0).unwrap();
            let mut server = l.accept().await.unwrap();
            std::thread::scope(|s| {
                s.spawn(|| {
                    // SAFETY: the calling thread's pseudo-handle.
                    assert_ne!(unsafe { ImpersonateAnonymousToken(GetCurrentThread()) }, 0, "{}", io::Error::last_os_error());
                    let wrote = write_line(&client, HELLO);
                    // SAFETY: ends this thread's impersonation.
                    if unsafe { RevertToSelf() } == 0 {
                        std::process::abort();
                    }
                    wrote
                })
                .join()
                .unwrap()
            })
            .unwrap();
            read_hello(&mut server).await;
            let err = admit_client(PeerHandle::of(&server), &own_identity()).unwrap_err();
            eprintln!("anonymous-token client: {err}");
            if err.contains("peer identity unreadable") {
                skipped(&format!("the anonymous token can't be read here, so there is no second account to compare: {err}"));
                return;
            }
            assert!(err.contains("is not the daemon's user"), "{err}");
        }

        /// A Low-integrity process of this user can create the pipe name
        /// before the daemon does (it can label the pipe no higher than
        /// itself). The client refuses to talk to it.
        #[tokio::test]
        async fn a_server_created_at_lower_integrity_is_refused() {
            if own_identity().unwrap().integrity < 0x2000 {
                skipped("this process already runs at Low integrity");
                return;
            }
            let (_d, path) = endpoint();
            let name = pipe_name(&path);
            let sid = win::current_user_sid().unwrap();
            let runtime = tokio::runtime::Handle::current();
            let squatter = wearing(|| lowered_token(0x1000), || {
                let _entered = runtime.enter();
                win::PipeSecurity::labelled(&sid, 0x1000)?
                    .create(ServerOptions::new().first_pipe_instance(true).reject_remote_clients(true), &name)
            })
            .unwrap();
            let e = connect(&path).await.expect_err("a pipe created at Low integrity must be refused");
            assert_eq!(e.kind(), io::ErrorKind::PermissionDenied, "{e}");
            assert!(e.to_string().contains("was created at integrity 0x1000"), "{e}");
            drop(squatter);
        }

        /// A pipe with no mandatory label (one a build without the label
        /// created, or any plain `CreateNamedPipe`) is refused too: the
        /// label is how the client knows who created the name.
        #[tokio::test]
        async fn an_unlabelled_server_is_refused() {
            let (_d, path) = endpoint();
            let name = pipe_name(&path);
            let _plain = ServerOptions::new().first_pipe_instance(true).create(&name).unwrap();
            let client = ClientOptions::new().open(&name).unwrap();
            match win::pipe_label(client.as_raw_handle() as HANDLE).unwrap() {
                None => {
                    let e = win::verify_server_integrity(client.as_raw_handle(), 0x2000).unwrap_err();
                    assert!(e.to_string().contains("no integrity label"), "{e}");
                }
                Some(rid) => skipped(&format!("a default pipe of this process carries the label {rid:#x}: nothing to test")),
            }
        }

        /// The label ACE parse trusts nothing it can check: it takes only a
        /// live label ACE (not an inherit-only one, which labels children),
        /// and only a well-formed `S-1-16-<rid>` SID that fits in the ACE.
        #[test]
        fn the_label_ace_parse_checks_what_it_reads() {
            // header (type, flags, size), mask, SID: revision 1, one sub-authority, authority 16, rid.
            let ace = |ty: u8, flags: u8, size: u16, sid: &[u8]| {
                let mut v = vec![ty, flags];
                v.extend(size.to_le_bytes());
                v.extend([0u8; 4]);
                v.extend(sid);
                v
            };
            let label = |rid: u32| {
                let mut sid = vec![1u8, 1, 0, 0, 0, 0, 0, 16];
                sid.extend(rid.to_le_bytes());
                sid
            };
            assert_eq!(win::label_rid(&ace(17, 0, 20, &label(0x2000))).unwrap(), Some(0x2000));
            assert_eq!(win::label_rid(&ace(17, 0x10, 20, &label(0x1000))).unwrap(), Some(0x1000), "other flags don't matter");
            // Not a live label: another ACE type, or inherit-only.
            assert_eq!(win::label_rid(&ace(0, 0, 20, &label(0x2000))).unwrap(), None);
            assert_eq!(win::label_rid(&ace(17, 0x08, 20, &label(0x2000))).unwrap(), None);
            // Malformed: a size that doesn't hold the SID, one that runs past the bytes, a SID of
            // two sub-authorities, another authority, another revision, and a stub.
            assert!(win::label_rid(&ace(17, 0, 16, &label(0x2000))).is_err());
            assert!(win::label_rid(&ace(17, 0, 24, &label(0x2000))).is_err());
            let mut two = vec![1u8, 2, 0, 0, 0, 0, 0, 16];
            two.extend([0u8; 8]);
            assert!(win::label_rid(&ace(17, 0, 24, &two)).is_err());
            let mut nt = label(0x2000);
            nt[7] = 5;
            assert!(win::label_rid(&ace(17, 0, 20, &nt)).is_err());
            let mut rev = label(0x2000);
            rev[0] = 2;
            assert!(win::label_rid(&ace(17, 0, 20, &rev)).is_err());
            assert!(win::label_rid(&[17, 0]).is_err());
        }

        /// `IsTokenRestricted` answers FALSE when it fails, so a query that
        /// failed would read as "unrestricted". A token handle opened without
        /// `TOKEN_QUERY` can't be asked: that must be an error, and the real
        /// answers must still come out right.
        #[test]
        fn a_failed_restriction_query_is_an_error() {
            let mut token: HANDLE = null_mut();
            // SAFETY: the pseudo-handle and an out-pointer; no TOKEN_QUERY, on purpose.
            assert_ne!(unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_DUPLICATE, &mut token) }, 0, "{}", io::Error::last_os_error());
            // SAFETY: a token handle we own.
            let blind = unsafe { OwnedHandle::from_raw_handle(token) };
            let e = win::token_is_restricted(blind.as_raw_handle() as HANDLE);
            assert!(e.is_err(), "a failed query must not read as unrestricted: {e:?}");
            let sandboxed = restricted_token().unwrap();
            assert!(win::token_is_restricted(sandboxed.as_raw_handle() as HANDLE).unwrap());
            let mine = win::current_process_identity().unwrap();
            assert!(!mine.confined);
        }

        /// The client reads the server's token too: a confined server is refused.
        #[test]
        fn a_confined_server_is_refused() {
            let own = own_identity().unwrap();
            win::verify_server_unconfined(7, &own).unwrap();
            let e = win::verify_server_unconfined(7, &Identity { confined: true, ..own }).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::PermissionDenied, "{e}");
            assert!(e.to_string().contains("restricted or AppContainer"), "{e}");
        }

        /// The server half of `a_server_process_with_a_restricted_token_is_refused`.
        /// Does nothing unless that test started this binary, through a
        /// restricted token, with the endpoint, the user's SID and integrity
        /// level as arguments (a restricted process can't always open its own
        /// token, so it is told who it is).
        #[test]
        fn restricted_token_server_child() {
            let arg = |key: &str| std::env::args().find_map(|a| a.strip_prefix(key).map(str::to_owned));
            let (Some(endpoint), Some(sid), Some(il)) = (arg("W1UENDPOINT:"), arg("W1USID:"), arg("W1UIL:")) else {
                return;
            };
            let il: u32 = il.parse().unwrap();
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async {
                let _first = win::PipeSecurity::labelled(&sid, il)
                    .unwrap()
                    .create(ServerOptions::new().first_pipe_instance(true).reject_remote_clients(true), &pipe_name(Path::new(&endpoint)))
                    .unwrap();
                tokio::time::sleep(Duration::from_secs(120)).await;
            });
        }

        /// Ends the process it holds when dropped.
        struct KillOnDrop(OwnedHandle);
        impl Drop for KillOnDrop {
            fn drop(&mut self) {
                // SAFETY: a process handle we own.
                unsafe { TerminateProcess(self.0.as_raw_handle() as HANDLE, 1) };
            }
        }

        /// A same-user process with a restricted token serves the endpoint
        /// (its DACL grants the user's SID, which is its restricting SID, and
        /// it gets the daemon's label: what it could do before a daemon starts,
        /// or as one more instance of a live daemon's name). The client reads
        /// that server process's own token and refuses it.
        #[tokio::test]
        async fn a_server_process_with_a_restricted_token_is_refused() {
            let (_d, path) = endpoint();
            let me = win::current_process_identity().unwrap();
            // The user, and the groups system files are readable by: a
            // restricted process starts only if its restricting SIDs pass the
            // file checks too.
            let token = match restricted_primary(&["S-1-1-0", "S-1-5-32-545", "S-1-5-11", "S-1-5-12"]) {
                Ok(t) => t,
                Err(e) => {
                    skipped(&format!("CreateRestrictedToken: {e}"));
                    return;
                }
            };
            let exe = std::env::current_exe().unwrap();
            let mut cmd: Vec<u16> = format!(
                "\"{}\" --exact ipc::imp::security_tests::restricted_token_server_child --nocapture \"W1UENDPOINT:{}\" W1USID:{} W1UIL:{}",
                exe.display(),
                path.display(),
                me.sid,
                me.integrity
            )
            .encode_utf16()
            .chain([0])
            .collect();
            // SAFETY: plain-data structs, all zero.
            let (mut si, mut pi): (STARTUPINFOW, PROCESS_INFORMATION) = unsafe { (std::mem::zeroed(), std::mem::zeroed()) };
            si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
            // SAFETY: a primary token, a NUL-terminated command line the call may write to, no handles inherited.
            let started = unsafe {
                CreateProcessAsUserW(token.as_raw_handle() as HANDLE, null(), cmd.as_mut_ptr(), null(), null(), 0, CREATE_NO_WINDOW, null(), null(), &si, &mut pi)
            };
            if started == 0 {
                skipped(&format!("cannot start a process with a restricted token: {}", io::Error::last_os_error()));
                return;
            }
            // SAFETY: the handles CreateProcessAsUserW returned, which we own.
            let (child, _thread) = unsafe { (OwnedHandle::from_raw_handle(pi.hProcess), OwnedHandle::from_raw_handle(pi.hThread)) };
            let child = KillOnDrop(child);
            let deadline = Instant::now() + Duration::from_secs(40);
            let result = loop {
                match connect(&path).await {
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        let mut code = 0u32;
                        // SAFETY: a process handle we own.
                        let known = unsafe { GetExitCodeProcess(child.0.as_raw_handle() as HANDLE, &mut code) };
                        // 259 = STILL_ACTIVE.
                        if known != 0 && code != 259 {
                            skipped(&format!("the restricted-token process exited with {code:#x} before it served the pipe"));
                            return;
                        }
                        assert!(Instant::now() < deadline, "the restricted-token process never bound the pipe");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    other => break other,
                }
            };
            let e = result.expect_err("a server with a restricted token must be refused");
            assert_eq!(e.kind(), io::ErrorKind::PermissionDenied, "{e}");
            assert!(e.to_string().contains("restricted or AppContainer"), "{e}");
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
