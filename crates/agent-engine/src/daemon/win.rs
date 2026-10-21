//! Windows pieces of the daemon lifecycle that Unix does with fds, `setsid`
//! and `execv`:
//!
//! * the spawn **ready signal** — a uniquely named event the spawner waits
//!   on together with the child's process handle (EOF-before-`R` on the
//!   Unix ready pipe ⇔ the process handle signalling first);
//! * the **detached spawn** — hidden console (so the daemon's own console
//!   children never pop a window), its own process group (the user's Ctrl+C
//!   never reaches it), outside the spawner's job when the job allows it,
//!   and an explicit inherited-handle list (see [`spawn_detached`]);
//! * the **reload lock hand-off** — Windows has no `execv` and byte-range
//!   locks belong to the process that took them, so the next image queues
//!   an exclusive lock request on `daemon.lock` behind the old image and
//!   tells it so; the old image exits and the kernel grants the queued
//!   request as it releases the lock. The liveness oracle never reads
//!   "free" in between, exactly as with the inherited flock on Unix.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, RawHandle};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, SetHandleInformation, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_IO_PENDING,
    GENERIC_READ, GENERIC_WRITE, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, LockFileEx, FILE_SHARE_READ, FILE_SHARE_WRITE, LOCKFILE_EXCLUSIVE_LOCK, OPEN_EXISTING,
};
use windows_sys::Win32::System::Console::{SetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows_sys::Win32::System::Threading::{
    CreateEventW, CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess, InitializeProcThreadAttributeList,
    OpenEventW, OpenProcess, SetEvent, TerminateProcess, UpdateProcThreadAttribute, WaitForMultipleObjects,
    WaitForSingleObject, CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW,
    CREATE_UNICODE_ENVIRONMENT, EVENT_MODIFY_STATE, EXTENDED_STARTUPINFO_PRESENT, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROCESS_INFORMATION, PROCESS_SYNCHRONIZE, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, STARTF_USESTDHANDLES,
    STARTUPINFOEXW,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain([0]).collect()
}

fn millis(d: Duration) -> u32 {
    // INFINITE is u32::MAX: stay one below it.
    d.as_millis().min(u128::from(u32::MAX - 1)) as u32
}

struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: a handle we opened and own.
        unsafe { CloseHandle(self.0) };
    }
}

/// A uniquely named manual-reset event, created unsignalled.
pub struct Event {
    handle: Handle,
    name: String,
}

// SAFETY: kernel handles are process-wide; the event is only waited on/closed.
unsafe impl Send for Event {}
unsafe impl Sync for Event {}

impl Event {
    /// `Local\synaps-daemon-<purpose>-<uuid>` (this logon session only).
    pub fn create_unique(purpose: &str) -> io::Result<Self> {
        let name = format!(r"Local\synaps-daemon-{purpose}-{}", uuid::Uuid::new_v4().simple());
        let w = wide(name.as_ref());
        // SAFETY: NUL-terminated name; default security; manual reset, unsignalled.
        let h = unsafe { CreateEventW(null(), 1, 0, w.as_ptr()) };
        if h.is_null() {
            return Err(io::Error::last_os_error());
        }
        let handle = Handle(h);
        // SAFETY: reads the thread's last error, set by CreateEventW.
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            // Someone else's event under our fresh name: never trust it.
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("event {name} already exists")));
        }
        Ok(Self { handle, name })
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

/// Child side: signal the event the parent created (by name).
pub fn signal_event(name: &str) -> io::Result<()> {
    let w = wide(name.as_ref());
    // SAFETY: NUL-terminated name.
    let h = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, w.as_ptr()) };
    if h.is_null() {
        return Err(io::Error::last_os_error());
    }
    let h = Handle(h);
    // SAFETY: an event handle opened with EVENT_MODIFY_STATE.
    if unsafe { SetEvent(h.0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// What [`wait_event_or_exit`] saw first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    Signalled,
    Exited,
    TimedOut,
}

/// Wait for `event`, or for `child` to exit, whichever comes first.
/// The event wins a tie (signalled, then exited, is still ready).
pub fn wait_event_or_exit(event: &Event, child: &Spawned, timeout: Duration) -> Wait {
    let handles = [event.handle.0, child.process.0];
    // SAFETY: two valid handles that outlive the call.
    match unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, millis(timeout)) } {
        r if r == WAIT_OBJECT_0 => Wait::Signalled,
        r if r == WAIT_OBJECT_0 + 1 => Wait::Exited,
        _ => Wait::TimedOut,
    }
}

/// A process started by [`spawn_detached`]. Dropping it closes our handles;
/// the process runs on.
pub struct Spawned {
    process: Handle,
    pid: u32,
    stderr: Option<File>,
}

// SAFETY: as for `Event`.
unsafe impl Send for Spawned {}

impl Spawned {
    pub fn id(&self) -> u32 {
        self.pid
    }

    /// The exit code once the process has exited, waiting up to `timeout`.
    pub fn wait(&self, timeout: Duration) -> Option<u32> {
        // SAFETY: a valid process handle.
        unsafe {
            if WaitForSingleObject(self.process.0, millis(timeout)) != WAIT_OBJECT_0 {
                return None;
            }
            let mut code = 0u32;
            GetExitCodeProcess(self.process.0, &mut code);
            Some(code)
        }
    }

    pub fn kill(&self) {
        // SAFETY: a valid process handle.
        unsafe { TerminateProcess(self.process.0, 1) };
    }

    /// The read end of the child's stderr.
    pub fn take_stderr(&mut self) -> Option<File> {
        self.stderr.take()
    }
}

/// What a detached daemon's stderr pipe can hold before it would block —
/// the Linux pipe default (it is read only after ready, or after a death).
const STDERR_PIPE_BYTES: u32 = 64 * 1024;

/// Start `program args` detached from this console, with `env` as its whole
/// environment, stdin/stdout on NUL and stderr on a pipe we read:
///
/// * a hidden console of its own (`CREATE_NO_WINDOW`) — the daemon's
///   console children inherit it instead of each opening a window;
/// * its own process group — the user's Ctrl+C never reaches it;
/// * outside our job object when the job permits breakaway — a terminal
///   that kills its job on close must not take the daemon with it (when the
///   job forbids it, the daemon stays in the job);
/// * **exactly** those three handles inherited
///   (`PROC_THREAD_ATTRIBUTE_HANDLE_LIST`). `std::process::Command` lets a
///   child inherit every inheritable handle we hold — our own stdio among
///   them when it came from a pipe — and a daemon holding its spawner's
///   stdout pipe keeps whoever reads that pipe waiting for EOF until the
///   daemon exits. (On Unix only fds 0–2 cross the exec, and they are
///   replaced.)
///
/// A `.bat`/`.cmd` `program` runs under `cmd.exe /d /s /c`, as
/// `CreateProcess` requires.
pub fn spawn_detached(program: &Path, args: &[OsString], env: &[(OsString, OsString)]) -> io::Result<Spawned> {
    let inheritable = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: null_mut(),
        bInheritHandle: 1,
    };
    let nul_name = wide("NUL".as_ref());
    // SAFETY: NUL-terminated name, valid security attributes.
    let nul = unsafe {
        CreateFileW(
            nul_name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            &inheritable,
            OPEN_EXISTING,
            0,
            null_mut(),
        )
    };
    if nul == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let nul = Handle(nul);
    let (mut r, mut w): (HANDLE, HANDLE) = (null_mut(), null_mut());
    // SAFETY: valid out-pointers and security attributes.
    if unsafe { CreatePipe(&mut r, &mut w, &inheritable, STDERR_PIPE_BYTES) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fresh handles we own; our read end never crosses a spawn.
    let stderr_read = unsafe { File::from_raw_handle(r as RawHandle) };
    let stderr_write = Handle(w);
    unsafe { SetHandleInformation(r, HANDLE_FLAG_INHERIT, 0) };

    let inherit = [nul.0, stderr_write.0];
    let list = AttributeList::with_handles(&inherit)?;
    // SAFETY: STARTUPINFOEXW is plain data.
    let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    si.StartupInfo.hStdInput = nul.0;
    si.StartupInfo.hStdOutput = nul.0;
    si.StartupInfo.hStdError = stderr_write.0;
    si.lpAttributeList = list.ptr();

    let (app, line) = command_line(program, args)?;
    let env = env_block(env);
    let flags = CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT;
    let create = |flags| -> Result<PROCESS_INFORMATION, u32> {
        // CreateProcessW may write to the command line: a fresh copy each time.
        let mut line = line.clone();
        // SAFETY: PROCESS_INFORMATION is plain data.
        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: NUL-terminated strings, a NUL-NUL-terminated UTF-16
        // environment block, and a startup info whose attribute list and
        // handles outlive the call.
        let ok = unsafe {
            CreateProcessW(
                app.as_ptr(),
                line.as_mut_ptr(),
                null(),
                null(),
                1,
                flags,
                env.as_ptr().cast(),
                null(),
                &si.StartupInfo,
                &mut pi,
            )
        };
        // SAFETY: reads the thread's last error.
        if ok == 0 { Err(unsafe { GetLastError() }) } else { Ok(pi) }
    };
    let pi = match create(flags | CREATE_BREAKAWAY_FROM_JOB) {
        Err(e) if e == ERROR_ACCESS_DENIED => create(flags),
        r => r,
    }
    .map_err(|e| io::Error::from_raw_os_error(e as i32))?;
    // SAFETY: the thread handle is ours and unused.
    unsafe { CloseHandle(pi.hThread) };
    // The child holds its own copies now: EOF on our read end means every
    // holder of the write end is gone.
    drop(stderr_write);
    drop(nul);
    Ok(Spawned { process: Handle(pi.hProcess), pid: pi.dwProcessId, stderr: Some(stderr_read) })
}

/// A one-entry `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`. Borrows the handle
/// array: it must outlive the `CreateProcessW` call.
struct AttributeList<'a> {
    buf: Vec<u64>,
    _handles: std::marker::PhantomData<&'a [HANDLE]>,
}

impl<'a> AttributeList<'a> {
    fn with_handles(handles: &'a [HANDLE]) -> io::Result<Self> {
        let mut size = 0usize;
        // SAFETY: size query (fails with ERROR_INSUFFICIENT_BUFFER, sets size).
        unsafe { InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut size) };
        let mut list = Self { buf: vec![0u64; size.div_ceil(8)], _handles: std::marker::PhantomData };
        // SAFETY: buf holds at least `size` bytes, 8-aligned.
        if unsafe { InitializeProcThreadAttributeList(list.ptr(), 1, 0, &mut size) } == 0 {
            list.buf = Vec::new(); // never initialised: Drop must not delete it
            return Err(io::Error::last_os_error());
        }
        // SAFETY: an initialised list; the handle array outlives it ('a).
        let ok = unsafe {
            UpdateProcThreadAttribute(
                list.ptr(),
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                handles.as_ptr().cast(),
                std::mem::size_of_val(handles),
                null_mut(),
                null(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(list)
    }

    fn ptr(&self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.buf.as_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST
    }
}

impl Drop for AttributeList<'_> {
    fn drop(&mut self) {
        if !self.buf.is_empty() {
            // SAFETY: initialised by InitializeProcThreadAttributeList.
            unsafe { DeleteProcThreadAttributeList(self.ptr()) };
        }
    }
}

/// `lpApplicationName` and the command line (both NUL-terminated).
fn command_line(program: &Path, args: &[OsString]) -> io::Result<(Vec<u16>, Vec<u16>)> {
    let mut line = Vec::new();
    let batch = program
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("bat") || e.eq_ignore_ascii_case("cmd"));
    if !batch {
        append_arg(&mut line, program.as_os_str());
        for a in args {
            line.push(b' ' as u16);
            append_arg(&mut line, a);
        }
        line.push(0);
        return Ok((wide(program.as_os_str()), line));
    }
    // `cmd.exe /d /s /c ""<script>" args"`: with /s, cmd strips exactly the
    // outer quotes and runs the rest. cmd does not take `\\?\` paths.
    let cmd = system_dir()?.join("cmd.exe");
    append_arg(&mut line, cmd.as_os_str());
    line.extend(" /d /s /c \"".encode_utf16());
    append_arg(&mut line, strip_verbatim(program).as_os_str());
    for a in args {
        line.push(b' ' as u16);
        append_arg(&mut line, a);
    }
    line.push(b'"' as u16);
    line.push(0);
    Ok((wide(cmd.as_os_str()), line))
}

/// One argument, quoted the way `CommandLineToArgvW` / the MSVC runtime
/// split it back (backslashes are literal unless they precede a quote).
fn append_arg(line: &mut Vec<u16>, arg: &OsStr) {
    let arg: Vec<u16> = arg.encode_wide().collect();
    let quote = arg.is_empty() || arg.iter().any(|&c| c == b' ' as u16 || c == b'\t' as u16 || c == b'"' as u16);
    if quote {
        line.push(b'"' as u16);
    }
    let mut backslashes = 0;
    for &c in &arg {
        if c == b'\\' as u16 {
            backslashes += 1;
        } else {
            if c == b'"' as u16 {
                line.extend(std::iter::repeat(b'\\' as u16).take(backslashes + 1));
            }
            backslashes = 0;
        }
        line.push(c);
    }
    if quote {
        line.extend(std::iter::repeat(b'\\' as u16).take(backslashes));
        line.push(b'"' as u16);
    }
}

/// The environment block: `K=V\0…\0\0`, keys case-insensitively unique
/// (the last one wins) and sorted, as Windows keeps its own.
fn env_block(env: &[(OsString, OsString)]) -> Vec<u16> {
    let mut sorted: BTreeMap<String, (&OsString, &OsString)> = BTreeMap::new();
    for (k, v) in env {
        sorted.insert(k.to_string_lossy().to_uppercase(), (k, v));
    }
    let mut block = Vec::new();
    for (k, v) in sorted.into_values() {
        block.extend(k.encode_wide());
        block.push(b'=' as u16);
        block.extend(v.encode_wide());
        block.push(0);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    block
}

fn system_dir() -> io::Result<PathBuf> {
    let mut buf = vec![0u16; 260];
    // SAFETY: buf is writable for its length.
    let n = unsafe { GetSystemDirectoryW(buf.as_mut_ptr(), buf.len() as u32) } as usize;
    if n == 0 || n > buf.len() {
        return Err(io::Error::last_os_error());
    }
    Ok(PathBuf::from(OsString::from_wide(&buf[..n])))
}

/// `\\?\C:\x` → `C:\x`, `\\?\UNC\h\s` → `\\h\s`; anything else unchanged.
fn strip_verbatim(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        p.to_path_buf()
    }
}

/// Point our own stdout/stderr at NUL. A detached daemon's spawner stops
/// reading its stderr once it is ready (the old image, once the new one has
/// taken over); a write to a pipe nobody reads fails, and `eprintln!`
/// panics on that. (Rust's std looks the handle up on every write.)
pub fn detach_stdio() {
    let nul_name = wide("NUL".as_ref());
    // SAFETY: NUL-terminated name; not inheritable.
    let nul = unsafe {
        CreateFileW(nul_name.as_ptr(), GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE, null(), OPEN_EXISTING, 0, null_mut())
    };
    if nul == INVALID_HANDLE_VALUE {
        return;
    }
    // SAFETY: a valid handle; it stays the std handle for the process's life
    // (never closed). The previous handles are left open: another thread
    // may be mid-write on them.
    unsafe {
        SetStdHandle(STD_OUTPUT_HANDLE, nul);
        SetStdHandle(STD_ERROR_HANDLE, nul);
    }
}

/// Up to `limit` bytes of a dead child's stderr, giving up after `wait`
/// (a descendant may still hold the pipe's write end open).
pub fn stderr_tail(stderr: Option<File>, limit: u64, wait: Duration) -> String {
    let Some(mut err) = stderr else {
        return String::new();
    };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = err.by_ref().take(limit).read_to_end(&mut bytes);
        let _ = tx.send(bytes);
    });
    rx.recv_timeout(wait).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default()
}

/// A handle to wait for another process's exit (`SYNCHRONIZE` only).
pub struct Process(Handle);

// SAFETY: as for `Event`.
unsafe impl Send for Process {}

impl Process {
    /// `None` when `pid` is gone (or not ours to watch).
    pub fn open(pid: u32) -> Option<Self> {
        // SAFETY: plain OpenProcess; null on failure.
        let h = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        (!h.is_null()).then(|| Self(Handle(h)))
    }

    /// Whether the process exited within `timeout`.
    pub fn wait(&self, timeout: Duration) -> bool {
        // SAFETY: a valid process handle with SYNCHRONIZE.
        unsafe { WaitForSingleObject(self.0 .0, millis(timeout)) == WAIT_OBJECT_0 }
    }
}

/// Take an exclusive lock on all of `file` (the range `fs4` locks, so its
/// `try_lock_exclusive` probes in other processes contend with it), queued
/// behind the current holder. `queued` runs once the request is in the
/// kernel's wait queue (or already granted) — from then on, the holder
/// releasing the lock (closing it, or exiting) grants it to us before any
/// other taker can see it free. `file` must be opened with
/// `FILE_FLAG_OVERLAPPED`, or `LockFileEx` would block instead of queueing.
pub fn lock_exclusive_queued(file: &File, timeout: Duration, queued: impl FnOnce()) -> io::Result<()> {
    let h = file.as_raw_handle() as HANDLE;
    // SAFETY: unnamed manual-reset event, unsignalled.
    let ev = unsafe { CreateEventW(null(), 1, 0, null()) };
    if ev.is_null() {
        return Err(io::Error::last_os_error());
    }
    let ev = Handle(ev);
    // SAFETY: OVERLAPPED is plain data; zero = offset 0.
    let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
    ov.hEvent = ev.0;
    // SAFETY: `ov` outlives the request: every path below waits for it to
    // complete (granted, failed or cancelled) before returning.
    if unsafe { LockFileEx(h, LOCKFILE_EXCLUSIVE_LOCK, 0, u32::MAX, u32::MAX, &mut ov) } != 0 {
        queued();
        return Ok(());
    }
    // SAFETY: reads the thread's last error.
    let err = unsafe { GetLastError() };
    if err != ERROR_IO_PENDING {
        return Err(io::Error::from_raw_os_error(err as i32));
    }
    queued();
    let mut n = 0u32;
    // SAFETY: valid event / file handles and the in-flight `ov`.
    unsafe {
        if WaitForSingleObject(ev.0, millis(timeout)) == WAIT_OBJECT_0 {
            return if GetOverlappedResult(h, &ov, &mut n, 0) != 0 { Ok(()) } else { Err(io::Error::last_os_error()) };
        }
        CancelIoEx(h, &ov);
        // Wait for the cancellation to land; a grant that won the race counts.
        if GetOverlappedResult(h, &ov, &mut n, 1) != 0 {
            return Ok(());
        }
    }
    Err(io::Error::new(io::ErrorKind::TimedOut, "the lock holder did not release it in time"))
}

/// `pid` names a live process (or one we may not inspect, which is alive).
pub fn pid_is_alive(pid: u32) -> bool {
    // SAFETY: plain OpenProcess; null on failure.
    let h = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if h.is_null() {
        // SAFETY: reads the thread's last error.
        return unsafe { GetLastError() } == ERROR_ACCESS_DENIED;
    }
    let h = Handle(h);
    // SAFETY: valid handle with SYNCHRONIZE; a zero wait just polls.
    unsafe { WaitForSingleObject(h.0, 0) == WAIT_TIMEOUT }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OVERLAPPED;

    fn split(line: &[u16]) -> Vec<String> {
        // Mirror of CommandLineToArgvW's rules for the cases we produce.
        let s = String::from_utf16_lossy(&line[..line.len() - 1]);
        let mut out = Vec::new();
        let mut cur = String::new();
        let (mut inq, mut any, mut bs) = (false, false, 0usize);
        for c in s.chars() {
            match c {
                '\\' => bs += 1,
                '"' => {
                    cur.extend(std::iter::repeat('\\').take(bs / 2));
                    if bs % 2 == 1 {
                        cur.push('"');
                    } else {
                        inq = !inq;
                        any = true;
                    }
                    bs = 0;
                }
                ' ' | '\t' if !inq => {
                    cur.extend(std::iter::repeat('\\').take(bs));
                    bs = 0;
                    if !cur.is_empty() || any {
                        out.push(std::mem::take(&mut cur));
                    }
                    any = false;
                }
                c => {
                    cur.extend(std::iter::repeat('\\').take(bs));
                    bs = 0;
                    cur.push(c);
                }
            }
        }
        cur.extend(std::iter::repeat('\\').take(bs));
        if !cur.is_empty() || any {
            out.push(cur);
        }
        out
    }

    #[test]
    fn command_line_quotes_round_trip() {
        let args: Vec<OsString> =
            ["daemon", "--profile", "a b", "", r#"say "hi""#, r"C:\dir\", r"C:\dir with space\"].map(OsString::from).to_vec();
        let (_, line) = command_line(Path::new(r"C:\Program Files\synaps\synaps.exe"), &args).unwrap();
        let back = split(&line);
        assert_eq!(back[0], r"C:\Program Files\synaps\synaps.exe");
        assert_eq!(back[1..], args.iter().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>()[..]);
    }

    #[test]
    fn env_block_is_sorted_deduplicated_and_double_terminated() {
        let env = [("b", "2"), ("A", "1"), ("a", "3")].map(|(k, v)| (OsString::from(k), OsString::from(v)));
        let block = env_block(&env);
        assert_eq!(String::from_utf16_lossy(&block), "a=3\0b=2\0\0");
    }

    #[test]
    fn detached_child_inherits_only_its_stdio_and_reports_stderr() {
        let env = crate::process_scope::inherited_env();
        let mut child = spawn_detached(
            Path::new(&std::env::var("ComSpec").unwrap_or_else(|_| r"C:\Windows\System32\cmd.exe".into())),
            &["/d".into(), "/c".into(), "echo boom 1>&2 & exit /b 7".into()],
            &env,
        )
        .unwrap();
        assert_eq!(child.wait(Duration::from_secs(10)), Some(7));
        let tail = stderr_tail(child.take_stderr(), 1024, Duration::from_secs(5));
        assert_eq!(tail.trim(), "boom");
    }

    #[test]
    fn detached_batch_script_runs_under_cmd() {
        let d = tempfile::tempdir().unwrap();
        let script = d.path().join("probe.cmd");
        std::fs::write(&script, "@echo off\r\nif \"%2\"==\"--print-version\" (echo ok 1>&2 & exit /b 0)\r\nexit /b 3\r\n").unwrap();
        let env = crate::process_scope::inherited_env();
        let script = script.canonicalize().unwrap(); // a \\?\ path, as validate_exe hands it over
        let mut ok = spawn_detached(&script, &["daemon".into(), "--print-version".into()], &env).unwrap();
        assert_eq!(ok.wait(Duration::from_secs(10)), Some(0));
        assert_eq!(stderr_tail(ok.take_stderr(), 1024, Duration::from_secs(5)).trim(), "ok");
        let no = spawn_detached(&script, &["daemon".into(), "--foreground".into()], &env).unwrap();
        assert_eq!(no.wait(Duration::from_secs(10)), Some(3));
    }

    #[test]
    fn named_event_round_trip() {
        let ev = Event::create_unique("test").unwrap();
        signal_event(ev.name()).unwrap();
        // SAFETY: valid handle.
        assert_eq!(unsafe { WaitForSingleObject(ev.handle.0, 0) }, WAIT_OBJECT_0);
        assert!(signal_event(r"Local\synaps-daemon-test-missing").is_err());
    }

    #[test]
    fn own_pid_is_alive_and_a_dead_child_is_not() {
        assert!(pid_is_alive(std::process::id()));
        let mut child = std::process::Command::new("cmd").args(["/C", "exit 0"]).spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        drop(child);
        assert!(!pid_is_alive(pid));
    }

    #[test]
    fn queued_lock_is_granted_when_the_holder_lets_go() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("daemon.lock");
        let holder = File::create(&path).unwrap();
        fs4::fs_std::FileExt::try_lock_exclusive(&holder).unwrap();
        let path2 = path.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let f = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(FILE_FLAG_OVERLAPPED)
                .open(&path2)
                .unwrap();
            lock_exclusive_queued(&f, Duration::from_secs(10), || tx.send(()).unwrap()).map(|()| f)
        });
        rx.recv_timeout(Duration::from_secs(5)).expect("queued");
        // Still held: a probe from a third handle fails.
        let probe = File::open(&path).unwrap();
        assert!(!fs4::fs_std::FileExt::try_lock_exclusive(&probe).unwrap());
        drop(holder);
        let granted = waiter.join().unwrap().expect("granted after release");
        assert!(!fs4::fs_std::FileExt::try_lock_exclusive(&probe).unwrap(), "the waiter holds it now");
        drop(granted);
        assert!(fs4::fs_std::FileExt::try_lock_exclusive(&probe).unwrap());
    }

    #[test]
    fn queued_lock_times_out_while_held() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("daemon.lock");
        let holder = File::create(&path).unwrap();
        fs4::fs_std::FileExt::try_lock_exclusive(&holder).unwrap();
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(FILE_FLAG_OVERLAPPED)
            .open(&path)
            .unwrap();
        let e = lock_exclusive_queued(&f, Duration::from_millis(100), || {}).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);
        drop(holder);
    }
}
