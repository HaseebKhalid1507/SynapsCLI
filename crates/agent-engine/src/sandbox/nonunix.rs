//! Off Unix (Windows): stand-ins for the Unix file facts the policy, carry
//! and owner rule 27c code reads, so that code compiles there unchanged.
//!
//! Nothing here enforces anything, and nothing here is relied on: off Unix
//! a policy file is refused unread (`super::policy::read_policy_file_meta`,
//! so its session's spawns are refused), a carry fails before it writes
//! (`super::carry`), and a 27c-checked write fails closed
//! (`super::protected::ToolGuard::open_target`). If one of these facts were
//! ever read, it would refuse: every object has the same identity `(0, 0)`
//! (each identity check that refuses on a match refuses), no hard links, no
//! device type and no permission bits.

/// `std::os::unix::fs::MetadataExt`'s identity, link count and mode.
pub(crate) trait MetadataExt {
    fn dev(&self) -> u64;
    fn ino(&self) -> u64;
    fn nlink(&self) -> u64;
    fn mode(&self) -> u32;
}

impl MetadataExt for std::fs::Metadata {
    fn dev(&self) -> u64 {
        0
    }
    fn ino(&self) -> u64 {
        0
    }
    fn nlink(&self) -> u64 {
        0
    }
    fn mode(&self) -> u32 {
        0
    }
}

/// `std::os::unix::fs::FileTypeExt::is_char_device`.
pub(crate) trait FileTypeExt {
    fn is_char_device(&self) -> bool;
}

impl FileTypeExt for std::fs::FileType {
    fn is_char_device(&self) -> bool {
        false
    }
}

/// `std::os::unix::ffi::OsStrExt::as_bytes`, as the OS string's encoded
/// bytes (only compared with ASCII text here).
pub(crate) trait OsStrExt {
    fn as_bytes(&self) -> &[u8];
}

impl OsStrExt for std::ffi::OsStr {
    fn as_bytes(&self) -> &[u8] {
        self.as_encoded_bytes()
    }
}
