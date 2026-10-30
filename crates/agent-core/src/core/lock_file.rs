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

use std::fs::File;
use std::io;

/// Take the exclusive lock without waiting: `Ok(false)` if another handle
/// holds it. Released when `file` is closed.
pub fn try_lock_exclusive(file: &File) -> io::Result<bool> {
    imp::try_lock_exclusive(file)
}

#[cfg(not(windows))]
mod imp {
    pub(super) fn try_lock_exclusive(file: &std::fs::File) -> std::io::Result<bool> {
        fs4::fs_std::FileExt::try_lock_exclusive(file)
    }
}

#[cfg(windows)]
mod imp {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{ERROR_IO_PENDING, ERROR_LOCK_VIOLATION};
    use windows_sys::Win32::Storage::FileSystem::{LockFileEx, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY};
    use windows_sys::Win32::System::IO::{OVERLAPPED, OVERLAPPED_0, OVERLAPPED_0_0};

    /// High dword of the locked byte's offset (0xFFFF_FFFF_0000_0000): past
    /// any body, yet inside a whole-file lock's range, which ends at 2^64 - 1.
    const LOCK_OFFSET_HIGH: u32 = u32::MAX;

    pub(super) fn try_lock_exclusive(file: &std::fs::File) -> std::io::Result<bool> {
        let mut overlapped = OVERLAPPED {
            Anonymous: OVERLAPPED_0 { Anonymous: OVERLAPPED_0_0 { Offset: 0, OffsetHigh: LOCK_OFFSET_HIGH } },
            ..Default::default()
        };
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
}

// A concurrently spawned test process can briefly hold an inherited lock
// descriptor until it executes, as in `session_lock`'s tests.
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
        let held = open(&path);
        assert!(try_lock_exclusive(&held).unwrap());
        (&held).write_all(b"4242\ntui\n").unwrap();

        let other = open(&path);
        assert!(!try_lock_exclusive(&other).unwrap(), "held");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "4242\ntui\n");

        drop(held);
        assert!(relock_within(&other, try_lock_exclusive), "released on close");
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
