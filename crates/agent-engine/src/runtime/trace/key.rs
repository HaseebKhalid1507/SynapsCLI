//! Installation-scoped digest key + keyed component digests.
//!
//! The HMAC key is random per installation, 32 bytes, stored `0600` (parent
//! `0700`), symlink-refusing and regular-file-only on both the read and the
//! create path. Neither the key nor digest preimages are ever logged.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

// --- Keyed component digest ---

/// Lowercase-hex HMAC-SHA256 output (exactly 64 hex chars). The validated
/// newtype makes it impossible to smuggle raw content through a digest field:
/// construction is only via [`keyed_digest`] or by parsing a well-formed hex
/// string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ComponentDigest(String);

impl ComponentDigest {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ComponentDigest {
    type Error = String;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.len() == 64
            && value
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        {
            Ok(ComponentDigest(value))
        } else {
            Err("component digest must be exactly 64 lowercase hex chars".to_string())
        }
    }
}

impl From<ComponentDigest> for String {
    fn from(value: ComponentDigest) -> Self {
        value.0
    }
}

/// Domain-separation label for keyed digests, so a digest of e.g. a system
/// segment can never be cross-matched against a tool schema digest of the
/// same bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DigestDomain {
    Wire,
    SystemSegment,
    MessageBlock,
    ToolSchema,
    ToolsPrefix,
    SystemPrefix,
    HistoryTail,
}

impl DigestDomain {
    fn label(self) -> &'static [u8] {
        match self {
            DigestDomain::Wire => b"synaps-trace:wire\0",
            DigestDomain::SystemSegment => b"synaps-trace:system-segment\0",
            DigestDomain::MessageBlock => b"synaps-trace:message-block\0",
            DigestDomain::ToolSchema => b"synaps-trace:tool-schema\0",
            DigestDomain::ToolsPrefix => b"synaps-trace:tools-prefix\0",
            DigestDomain::SystemPrefix => b"synaps-trace:system-prefix\0",
            DigestDomain::HistoryTail => b"synaps-trace:history-tail\0",
        }
    }
}

// --- Installation-scoped digest key ---

/// Random 32-byte per-installation HMAC key. Zeroized on drop; `Debug` is
/// redacted so the key never reaches a log line by accident.
pub struct TraceDigestKey(zeroize::Zeroizing<[u8; 32]>);

impl std::fmt::Debug for TraceDigestKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TraceDigestKey(..)")
    }
}

/// Typed failure for digest-key loading.
#[derive(Debug)]
pub enum TraceKeyError {
    /// A symlink is planted at the key path — refusing to read or create
    /// through it.
    SymlinkRefused(PathBuf),
    /// The key path exists but is not a regular file (FIFO, device,
    /// directory, socket, …) — refusing to open or read it.
    NotRegularFile(PathBuf),
    /// The key file exists but does not contain exactly 32 bytes.
    Corrupt(PathBuf),
    Io(std::io::Error),
}

impl std::fmt::Display for TraceKeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TraceKeyError::SymlinkRefused(p) => {
                write!(f, "refusing symlink at trace key path {}", p.display())
            }
            TraceKeyError::NotRegularFile(p) => {
                write!(f, "trace key path {} is not a regular file", p.display())
            }
            TraceKeyError::Corrupt(p) => {
                write!(f, "trace digest key at {} is corrupt", p.display())
            }
            TraceKeyError::Io(e) => write!(f, "trace digest key io error: {e}"),
        }
    }
}

impl std::error::Error for TraceKeyError {}

impl From<std::io::Error> for TraceKeyError {
    fn from(e: std::io::Error) -> Self {
        TraceKeyError::Io(e)
    }
}

/// Default installation-scoped key path: `<synaps base dir>/trace/digest.key`.
pub fn default_digest_key_path() -> PathBuf {
    agent_core::core::config::base_dir()
        .join("trace")
        .join("digest.key")
}

/// Load the installation digest key, creating it on first use.
pub fn load_or_create_digest_key() -> Result<TraceDigestKey, TraceKeyError> {
    load_or_create_digest_key_at(&default_digest_key_path())
}

/// Load (or atomically create) the digest key at `path`.
///
/// Guarantees:
/// - the parent directory is created `0700` via the Phase 1 private-fs
///   helper; the key file is created `0600` with no broader interval
///   (mode set at `open`, never create-then-chmod);
/// - a symlink planted at the key path is refused, never followed;
/// - a non-regular file (FIFO, device, directory) at the key path is refused
///   with [`TraceKeyError::NotRegularFile`], promptly and without blocking;
/// - a pre-existing key file with a broader mode is repaired to exactly
///   `0600` via the already-open handle (no path re-resolution);
/// - at most 33 bytes are read; any length other than exactly 32 is
///   [`TraceKeyError::Corrupt`];
/// - concurrent first-time creation converges: the key is fully written to a
///   `0600` temp file and published with an atomic `link(2)`; losers read the
///   winner's key, so every caller observes the same key bytes.
pub fn load_or_create_digest_key_at(path: &Path) -> Result<TraceDigestKey, TraceKeyError> {
    use std::io::Read as _;
    use std::io::Write as _;

    let parent = path.parent().ok_or_else(|| {
        TraceKeyError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "trace key path has no parent directory",
        ))
    })?;
    agent_core::core::private_fs::ensure_private_dir(parent).map_err(std::io::Error::from)?;

    // Bounded retry: each iteration either reads an existing complete key or
    // attempts to publish a fresh one.
    for _ in 0..8 {
        match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(TraceKeyError::SymlinkRefused(path.to_path_buf()));
            }
            // Pre-open gate: anything that is not a regular file (FIFO,
            // device, directory, socket) is refused before we ever open it,
            // so a planted FIFO can never block us.
            Ok(meta) if !meta.file_type().is_file() => {
                return Err(TraceKeyError::NotRegularFile(path.to_path_buf()));
            }
            Ok(_) => {
                let file = open_nofollow_read(path)?;
                // Post-open gate on the open handle (no path re-resolution):
                // the fd itself must refer to a regular file, closing the
                // check-then-open race.
                let meta = file.metadata()?;
                if !meta.file_type().is_file() {
                    return Err(TraceKeyError::NotRegularFile(path.to_path_buf()));
                }
                // Repair a pre-existing broader mode to exactly 0600 via
                // fchmod on the already-open handle.
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    let mode = meta.permissions().mode() & 0o777;
                    if mode != agent_core::core::private_fs::FILE_MODE {
                        file.set_permissions(std::fs::Permissions::from_mode(
                            agent_core::core::private_fs::FILE_MODE,
                        ))?;
                    }
                }
                // Bounded read: at most 33 bytes, so an oversized file can
                // never allocate unboundedly. Anything other than exactly 32
                // bytes is corrupt.
                let mut buf = zeroize::Zeroizing::new(Vec::with_capacity(33));
                file.take(33).read_to_end(&mut buf)?;
                if buf.len() != 32 {
                    return Err(TraceKeyError::Corrupt(path.to_path_buf()));
                }
                let mut key = zeroize::Zeroizing::new([0u8; 32]);
                key.copy_from_slice(&buf);
                return Ok(TraceDigestKey(key));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }

        // Generate fresh key material from the OS CSPRNG.
        let mut key = zeroize::Zeroizing::new([0u8; 32]);
        {
            use rand::TryRngCore as _;
            rand::rngs::OsRng
                .try_fill_bytes(key.as_mut())
                .map_err(|e| {
                    TraceKeyError::Io(std::io::Error::new(std::io::ErrorKind::Other, e))
                })?;
        }

        // Write the full key to a private temp file, then publish atomically
        // with link(2): the key path only ever exists complete, so a
        // concurrent reader can never observe a partial key. The temp name is
        // unique per attempt (pid + counter) so concurrent creators — threads
        // or processes — never clobber each other's temp file.
        static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = path.with_file_name(format!("digest.key.tmp.{}.{seq}", std::process::id()));
        let publish = (|| -> Result<bool, TraceKeyError> {
            let mut file = open_create_new_private(&tmp)?;
            file.write_all(key.as_ref())?;
            file.sync_all()?;
            drop(file);
            match std::fs::hard_link(&tmp, path) {
                Ok(()) => Ok(true),
                // Lost the race (or a symlink was planted): loop and re-read.
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
                Err(e) => Err(e.into()),
            }
        })();
        let _ = std::fs::remove_file(&tmp);
        match publish {
            Ok(true) => return Ok(TraceDigestKey(key)),
            Ok(false) => continue,
            Err(e) => return Err(e),
        }
    }
    Err(TraceKeyError::Io(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "trace digest key creation did not converge",
    )))
}

/// Open an existing file for reading without following a symlink at the
/// leaf. On Unix, `O_NOFOLLOW | O_NONBLOCK`: the former refuses a planted
/// symlink, the latter guarantees the open itself can never block on a FIFO
/// or device (regular-file reads are unaffected by `O_NONBLOCK`); the fd is
/// then `fstat`-verified by the caller before any read.
fn open_nofollow_read(path: &Path) -> Result<std::fs::File, TraceKeyError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .map_err(|e| {
                if e.raw_os_error() == Some(libc::ELOOP) {
                    TraceKeyError::SymlinkRefused(path.to_path_buf())
                } else {
                    TraceKeyError::Io(e)
                }
            })
    }
    #[cfg(not(unix))]
    {
        Ok(std::fs::OpenOptions::new().read(true).open(path)?)
    }
}

/// Create a brand-new private (`0600`) file; fails if the path exists.
fn open_create_new_private(path: &Path) -> Result<std::fs::File, TraceKeyError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        Ok(std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(agent_core::core::private_fs::FILE_MODE)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?)
    }
    #[cfg(not(unix))]
    {
        Ok(std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?)
    }
}

/// Keyed HMAC-SHA256 component digest with domain separation. The preimage
/// (`bytes`) is never logged or stored; only the hex MAC output escapes.
pub fn keyed_digest(key: &TraceDigestKey, domain: DigestDomain, bytes: &[u8]) -> ComponentDigest {
    use hmac::Mac as _;
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(key.0.as_ref())
        .expect("HMAC-SHA256 accepts any key length");
    mac.update(domain.label());
    mac.update(bytes);
    let out = mac.finalize().into_bytes();
    let mut hex = String::with_capacity(64);
    for b in out {
        use std::fmt::Write as _;
        let _ = write!(hex, "{b:02x}");
    }
    ComponentDigest(hex)
}
