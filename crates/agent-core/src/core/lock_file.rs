//! Exclusive lock files whose body names the holder.
//!
//! The session journal lock and the keeper locks write the holder's identity
//! into the locked file, so a process that is refused can say who has it. On
//! Unix the lock is an advisory `flock`, which never blocks a read. Windows
//! byte-range locks are mandatory: fs4 (like `File::try_lock`) locks the whole
//! file from offset 0, so the refused process could not read the body and
//! every refusal lost its holder ("held by another process"). There the lock
//! covers one byte far past any body instead. That byte lies inside the
//! whole-file range, so it still excludes, and is excluded by, a binary that
//! locks the whole file.
//!
//! A lock is released by dropping its [`ExclusiveLock`], which unlocks
//! explicitly before the file closes. Closing alone is not enough: a `flock`
//! belongs to the open file description, and a child forked by any thread of
//! this process holds a copy of every descriptor until it execs (`O_CLOEXEC`
//! applies only at exec). Until then the lock would outlive its owner, and an
//! immediate re-lock be refused in the name of this very process. Windows
//! releases a closed handle's locks only "depending on available system
//! resources", so there an explicit unlock is what makes the release prompt.

use std::fs::File;
use std::io;

/// An exclusive lock on a file, released when dropped.
#[derive(Debug)]
pub struct ExclusiveLock {
    file: File,
}

impl ExclusiveLock {
    /// The locked file, e.g. to write the holder's body.
    pub fn file(&self) -> &File {
        &self.file
    }
}

impl Drop for ExclusiveLock {
    fn drop(&mut self) {
        // Unlock first: another descriptor for the same open file description
        // (a forked child's, until it execs) would keep the lock past close.
        let _ = imp::unlock(&self.file);
    }
}

/// Take the exclusive lock on `file` without waiting: `Ok(None)` if another
/// handle holds it.
pub fn try_lock(file: File) -> io::Result<Option<ExclusiveLock>> {
    Ok(try_lock_exclusive(&file)?.then_some(ExclusiveLock { file }))
}

fn try_lock_exclusive(file: &File) -> io::Result<bool> {
    imp::try_lock_exclusive(file)
}

#[cfg(not(windows))]
mod imp {
    pub(super) fn try_lock_exclusive(file: &std::fs::File) -> std::io::Result<bool> {
        fs4::fs_std::FileExt::try_lock_exclusive(file)
    }

    pub(super) fn unlock(file: &std::fs::File) -> std::io::Result<()> {
        fs4::fs_std::FileExt::unlock(file)
    }
}

#[cfg(windows)]
mod imp {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{ERROR_IO_PENDING, ERROR_LOCK_VIOLATION};
    use windows_sys::Win32::Storage::FileSystem::{
        LockFileEx, UnlockFileEx, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY,
    };
    use windows_sys::Win32::System::IO::{OVERLAPPED, OVERLAPPED_0, OVERLAPPED_0_0};

    /// High dword of the locked byte's offset (0xFFFF_FFFF_0000_0000): past
    /// any body, yet inside a whole-file lock's range, which ends at 2^64 - 1.
    const LOCK_OFFSET_HIGH: u32 = u32::MAX;

    /// The locked byte's offset, for both the lock and its unlock.
    fn locked_byte() -> OVERLAPPED {
        OVERLAPPED {
            Anonymous: OVERLAPPED_0 { Anonymous: OVERLAPPED_0_0 { Offset: 0, OffsetHigh: LOCK_OFFSET_HIGH } },
            ..Default::default()
        }
    }

    pub(super) fn try_lock_exclusive(file: &std::fs::File) -> std::io::Result<bool> {
        let mut overlapped = locked_byte();
        // SAFETY: `file` is an open handle and `overlapped` outlives the call.
        // The handle is synchronous and the flags never wait, so the call has
        // finished with `overlapped` when it returns.
        let ok = unsafe {
            LockFileEx(
                file.as_raw_handle(),
                LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
                0,
                1,
                0,
                &mut overlapped,
            )
        };
        if ok != 0 {
            return Ok(true);
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(code) if code == ERROR_LOCK_VIOLATION as i32 || code == ERROR_IO_PENDING as i32 => Ok(false),
            _ => Err(err),
        }
    }

    /// Unlock exactly the byte `try_lock_exclusive` locked (Windows only
    /// unlocks a range that matches a lock).
    pub(super) fn unlock(file: &std::fs::File) -> std::io::Result<()> {
        let mut overlapped = locked_byte();
        // SAFETY: as in `try_lock_exclusive`; unlocking never waits.
        let ok = unsafe { UnlockFileEx(file.as_raw_handle(), 0, 1, 0, &mut overlapped) };
        if ok != 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
}

// The whole-file fs4 lock below is released by closing it, which a
// concurrently spawned test process can delay until it executes (see
// `ParkedChild`).
#[cfg(test)]
#[serial_test::parallel(process_spawn)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::path::Path;
    use std::time::{Duration, Instant};

    fn open(path: &Path) -> File {
        OpenOptions::new().create(true).truncate(false).read(true).write(true).open(path).unwrap()
    }

    /// Windows releases a closed handle's locks "depending on available
    /// system resources", so a re-lock after a close may need a moment.
    fn relock_within(file: &File, lock: impl Fn(&File) -> io::Result<bool>) -> bool {
        let t0 = Instant::now();
        loop {
            if lock(file).unwrap() {
                return true;
            }
            if t0.elapsed() > Duration::from_secs(5) {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_refused_locker_can_read_the_holders_body() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.lock");
        let held = try_lock(open(&path)).unwrap().expect("uncontended");
        held.file().write_all(b"4242\ntui\n").unwrap();

        let other = open(&path);
        assert!(!try_lock_exclusive(&other).unwrap(), "held");
        assert!(try_lock(open(&path)).unwrap().is_none(), "held");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "4242\ntui\n");

        drop(held);
        assert!(relock_within(&other, try_lock_exclusive), "released on drop");
    }

    /// A binary that locks the whole file (fs4, before this helper) and one
    /// that uses it exclude each other both ways.
    #[test]
    fn excludes_and_is_excluded_by_a_whole_file_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.lock");
        let whole = open(&path);
        assert!(fs4::fs_std::FileExt::try_lock_exclusive(&whole).unwrap());
        let ours = open(&path);
        assert!(!try_lock_exclusive(&ours).unwrap(), "a whole-file lock excludes ours");

        drop(whole);
        assert!(relock_within(&ours, try_lock_exclusive));
        let late = open(&path);
        assert!(!fs4::fs_std::FileExt::try_lock_exclusive(&late).unwrap(), "ours excludes a whole-file lock");
    }
}

/// A child forked from this process and parked between fork and exec. It
/// holds a copy of every descriptor open at the fork (`O_CLOEXEC` closes them
/// only at exec) until released, which is what any thread spawning a process
/// does for a moment. Use it from `#[serial(process_spawn)]` tests: while
/// parked it also holds other tests' descriptors.
#[cfg(all(test, unix))]
pub(crate) struct ParkedChild {
    release: Option<io::PipeWriter>,
    spawner: Option<std::thread::JoinHandle<io::Result<std::process::ExitStatus>>>,
}

#[cfg(all(test, unix))]
impl ParkedChild {
    /// Fork a child and return once it is parked before exec.
    pub(crate) fn park() -> Self {
        use std::io::Read;
        use std::os::fd::AsRawFd;
        use std::os::unix::process::CommandExt;
        use std::time::{Duration, Instant};
        // Close-on-exec, like every descriptor std opens.
        let (mut forked, signal) = io::pipe().unwrap();
        let (hold, release) = io::pipe().unwrap();
        let (signal_fd, hold_fd) = (signal.as_raw_fd(), hold.as_raw_fd());
        let mut child = Self {
            release: Some(release),
            spawner: Some(std::thread::spawn(move || {
                let mut command = std::process::Command::new("true");
                // SAFETY: only async-signal-safe calls (write, read, _exit)
                // between fork and exec.
                unsafe {
                    command.pre_exec(move || {
                        let interrupted = || io::Error::last_os_error().raw_os_error() == Some(libc::EINTR);
                        loop {
                            match libc::write(signal_fd, b"f".as_ptr().cast(), 1) {
                                1 => break,
                                -1 if interrupted() => {}
                                // EOF on the parent's side fails the test.
                                _ => libc::_exit(1),
                            }
                        }
                        let mut byte = 0u8;
                        while libc::read(hold_fd, (&mut byte as *mut u8).cast(), 1) < 0 && interrupted() {}
                        Ok(())
                    });
                }
                let status = command.status();
                drop((signal, hold));
                status
            })),
        };
        // Bounded: a child that never signals fails the test, and `Drop`
        // releases and reaps it.
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let mut ready = libc::pollfd { fd: forked.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            let left = deadline.saturating_duration_since(Instant::now());
            let timeout = left.as_millis().try_into().unwrap_or(i32::MAX);
            // SAFETY: `ready` is one valid pollfd that outlives the call.
            match unsafe { libc::poll(&mut ready, 1, timeout) } {
                1 => break,
                0 => panic!("the forked child never parked"),
                _ => assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EINTR)),
            }
        }
        // EOF instead of the byte: the spawn failed before the child parked.
        if let Err(e) = forked.read_exact(&mut [0; 1]) {
            let spawn = child.release_and_reap();
            panic!("the forked child never parked: {e}; spawn: {spawn:?}");
        }
        child
    }

    /// Let the child exec and exit; returns once it is reaped.
    pub(crate) fn release(mut self) {
        let status = self.release_and_reap().expect("not yet released");
        assert!(status.expect("spawner panicked").expect("spawn failed").success());
    }

    fn release_and_reap(&mut self) -> Option<std::thread::Result<io::Result<std::process::ExitStatus>>> {
        // The byte, or failing that EOF, ends the child's wait.
        if let Some(mut release) = self.release.take() {
            use std::io::Write;
            let _ = release.write_all(b"r");
        }
        self.spawner.take().map(std::thread::JoinHandle::join)
    }
}

#[cfg(all(test, unix))]
impl Drop for ParkedChild {
    fn drop(&mut self) {
        let _ = self.release_and_reap();
    }
}

#[cfg(all(test, unix))]
mod fork_tests {
    use super::*;
    use std::fs::OpenOptions;

    /// Dropping the lock releases it while a forked child still holds a
    /// copy of its descriptor: the next locker, in this process or another,
    /// gets it at once instead of being refused in this process's name.
    #[test]
    #[serial_test::serial(process_spawn)]
    fn a_descriptor_inherited_by_a_forked_child_does_not_outlive_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.lock");
        let open = || OpenOptions::new().create(true).truncate(false).read(true).write(true).open(&path).unwrap();
        let held = try_lock(open()).unwrap().expect("uncontended");
        let child = ParkedChild::park();
        drop(held);
        // The child is parked before exec, holding an inherited copy.
        let relocked = try_lock(open()).unwrap();
        child.release();
        assert!(relocked.is_some(), "a forked child's inherited descriptor kept the lock");
    }
}
