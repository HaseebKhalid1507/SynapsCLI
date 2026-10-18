//! Zero-config machine token for a loopback `synaps auth-broker` (#147 C1).
//!
//! A loopback broker started without `--machine-token-file` /
//! `SYNAPS_BROKER_TOKEN` generates a random token into
//! `<base_dir>/auth-broker.token` (0600, next to `auth.json`) and requires
//! it like any other machine token. A client whose broker endpoint is a
//! loopback URL and that has no `auth.machine_token` /
//! `SYNAPS_MACHINE_TOKEN` reads the same file, so `synaps` → local broker
//! keeps working with no extra configuration for the same user/base dir.

use std::io::Write;
use std::path::{Path, PathBuf};

/// File name of the auto-generated loopback broker token.
pub const LOCAL_BROKER_TOKEN_FILE: &str = "auth-broker.token";

/// `<base_dir>/auth-broker.token`.
pub fn local_broker_token_path() -> PathBuf {
    crate::config::base_dir().join(LOCAL_BROKER_TOKEN_FILE)
}

/// True when `endpoint` is an http(s) URL whose host is loopback
/// (`localhost`, `127.0.0.0/8`, `::1`).
pub fn is_loopback_endpoint(endpoint: &str) -> bool {
    let Ok(url) = url::Url::parse(endpoint.trim()) else {
        return false;
    };
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    match url.host() {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// Client side: the local broker token, if the file exists and is non-empty.
pub fn read_local_broker_token() -> Option<String> {
    read_token_at(&local_broker_token_path()).ok().flatten()
}

/// Largest token file a client will read (the auto token is 65 bytes).
const MAX_CLIENT_TOKEN_FILE: u64 = 4096;

/// Client read of the token file: never follows a symlink, refuses anything
/// but a regular file, and caps the size, so whatever `auth-broker.token`
/// points at is never sent as a bearer.
fn read_token_at(path: &Path) -> std::io::Result<Option<String>> {
    use std::io::Read;
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if meta.file_type().is_symlink() {
        return Err(invalid(path, "is a symlink; refusing to follow it"));
    }
    if !meta.is_file() {
        return Err(invalid(path, "is not a regular file"));
    }
    if meta.len() > MAX_CLIENT_TOKEN_FILE {
        return Err(invalid(path, "is larger than 4 KiB; not a machine token"));
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    let mut s = String::new();
    opts.open(path)?
        .take(MAX_CLIENT_TOKEN_FILE)
        .read_to_string(&mut s)?;
    Ok(Some(s.trim().to_string()).filter(|t| !t.is_empty()))
}

/// How [`load_or_create_broker_token`] obtained the token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrokerTokenOrigin {
    /// Reused the existing private file.
    Loaded,
    /// No file: generated a new token.
    Created,
    /// The existing file was readable by group/other, so its token may have
    /// leaked: a NEW token replaced it (clients must pick it up).
    Rotated,
}

fn invalid(path: &Path, why: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("broker token file {}: {why}", path.display()),
    )
}

/// Broker side: read the token at `path`, or create it when absent.
///
/// Hardening (prA I3):
/// - a symlink is refused (never followed), as is anything but a regular
///   file, or a file owned by another user;
/// - a file readable by group/other is treated as leaked: a NEW token is
///   written (atomically, 0600) instead of chmod-ing and reusing the old one;
/// - creation writes a 0600 temp file and hard-links it into place, so a
///   concurrent starter never sees a half-written file, and the loser reads
///   the winner's token;
/// - an empty existing file is an error (delete it to regenerate).
pub fn load_or_create_broker_token(path: &Path) -> std::io::Result<(String, BrokerTokenOrigin)> {
    if let Some(parent) = path.parent() {
        create_private_dir(parent)?;
    }
    for _ in 0..3 {
        match std::fs::symlink_metadata(path) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    return Err(invalid(path, "is a symlink; refusing to follow it"));
                }
                if !meta.is_file() {
                    return Err(invalid(path, "is not a regular file"));
                }
                check_owner(path, &meta)?;
                if loose_mode(&meta) {
                    let token = generate_token();
                    write_atomic_replace(path, &token)?;
                    return Ok((token, BrokerTokenOrigin::Rotated));
                }
                let token = read_no_follow(path)?;
                if token.is_empty() {
                    return Err(invalid(path, "is empty; delete it to generate a new one"));
                }
                return Ok((token, BrokerTokenOrigin::Loaded));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let token = generate_token();
                match write_new_exclusive(path, &token) {
                    Ok(()) => return Ok((token, BrokerTokenOrigin::Created)),
                    // A concurrent broker won the race: read its token.
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(e) => return Err(e),
                }
            }
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other(format!(
        "could not create or read broker token file {}",
        path.display()
    )))
}

#[cfg(unix)]
fn check_owner(path: &Path, meta: &std::fs::Metadata) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: geteuid has no preconditions and cannot fail.
    if meta.uid() != unsafe { libc::geteuid() } {
        return Err(invalid(path, "is owned by another user; refusing to use it"));
    }
    Ok(())
}
#[cfg(not(unix))]
fn check_owner(_: &Path, _: &std::fs::Metadata) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn loose_mode(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o077 != 0
}
#[cfg(not(unix))]
fn loose_mode(_: &std::fs::Metadata) -> bool {
    false
}

fn read_no_follow(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    let mut s = String::new();
    opts.open(path)?.read_to_string(&mut s)?;
    Ok(s.trim().to_string())
}

/// Write `token` to a fresh 0600 temp file beside `path`.
fn write_temp(path: &Path, token: &str) -> std::io::Result<PathBuf> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| LOCAL_BROKER_TOKEN_FILE.to_string());
    let tmp = path.with_file_name(format!(
        ".{name}.tmp.{}.{:08x}",
        std::process::id(),
        rand::random::<u32>()
    ));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp)?;
    let written = f
        .write_all(token.as_bytes())
        .and_then(|_| f.write_all(b"\n"))
        .and_then(|_| f.sync_all());
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(tmp)
}

/// Create `path` only if absent (hard link of a complete temp file).
fn write_new_exclusive(path: &Path, token: &str) -> std::io::Result<()> {
    let tmp = write_temp(path, token)?;
    let linked = std::fs::hard_link(&tmp, path);
    let _ = std::fs::remove_file(&tmp);
    linked
}

/// Atomically replace `path` (rename of a complete 0600 temp file).
fn write_atomic_replace(path: &Path, token: &str) -> std::io::Result<()> {
    let tmp = write_temp(path, token)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

fn generate_token() -> String {
    let bytes: [u8; 32] = rand::random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    if dir.as_os_str().is_empty() || dir.exists() {
        return Ok(());
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

/// The machine token a client presents to a broker, with its provenance.
///
/// - Explicit (`auth.machine_token` / `SYNAPS_MACHINE_TOKEN`, i.e. a
///   non-empty configured token): fixed. Never replaced by, and never sent
///   alongside, the local file token (prA N1) — even on a loopback endpoint,
///   which may be an `ssh -L` tunnel to a remote broker.
/// - None configured + loopback endpoint: the local broker's
///   `<base_dir>/auth-broker.token`, read now and re-read after a 401
///   (prA I2), so a long-lived client started before the broker's first run,
///   or across a rotation, recovers without a restart.
/// - None configured + remote endpoint: empty, never reads the file.
///
/// Clones share the current value.
#[derive(Clone)]
pub struct MachineToken {
    current: std::sync::Arc<std::sync::RwLock<String>>,
    reload_from: Option<PathBuf>,
}

impl MachineToken {
    /// `token` is the EXPLICITLY configured token ("" = none configured).
    pub fn for_endpoint(endpoint: &str, token: impl Into<String>) -> Self {
        let token = token.into().trim().to_string();
        if !token.is_empty() || !is_loopback_endpoint(endpoint) {
            return Self::fixed(token);
        }
        let path = local_broker_token_path();
        let initial = read_token_at(&path).ok().flatten().unwrap_or_default();
        Self {
            current: std::sync::Arc::new(std::sync::RwLock::new(initial)),
            reload_from: Some(path),
        }
    }

    /// Whether this token comes from the local broker token file.
    pub fn is_from_local_file(&self) -> bool {
        self.reload_from.is_some()
    }

    /// Fixed token, never reloaded.
    pub fn fixed(token: impl Into<String>) -> Self {
        Self {
            current: std::sync::Arc::new(std::sync::RwLock::new(token.into())),
            reload_from: None,
        }
    }

    pub fn get(&self) -> String {
        self.current
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// After a 401: re-read the local broker token file. Returns true when
    /// a different non-empty token was found (and is now current).
    pub fn reload_after_unauthorized(&self) -> bool {
        let Some(path) = &self.reload_from else {
            return false;
        };
        let Ok(Some(fresh)) = read_token_at(path) else {
            return false;
        };
        let mut cur = self.current.write().unwrap_or_else(|e| e.into_inner());
        if *cur == fresh {
            return false;
        }
        *cur = fresh;
        true
    }

    /// Send a request built with the current token; on a 401 from a
    /// loopback broker whose token file has changed, retry once with it.
    pub async fn send(
        &self,
        make: impl Fn(&str) -> reqwest::RequestBuilder,
    ) -> reqwest::Result<reqwest::Response> {
        let resp = make(&self.get()).send().await?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED && self.reload_after_unauthorized() {
            drop(resp);
            return make(&self.get()).send().await;
        }
        Ok(resp)
    }
}

/// Actionable text for a broker 401 (client side).
pub fn unauthorized_hint() -> String {
    format!(
        "broker rejected machine auth (401). Set auth.machine_token or \
         SYNAPS_MACHINE_TOKEN to the broker's token; a loopback broker started \
         without one writes it to {} on the broker host (same user and base dir \
         are picked up automatically). If the broker regenerated it, restart or \
         reload this client.",
        local_broker_token_path().display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_endpoints_are_recognised() {
        for e in [
            "http://127.0.0.1:8181",
            "http://localhost:8181/",
            "https://LOCALHOST:9",
            "http://[::1]:8181",
            "http://127.1.2.3:1",
        ] {
            assert!(is_loopback_endpoint(e), "{e}");
        }
        for e in [
            "https://jade.jade:8181",
            "http://10.0.0.1:8181",
            "http://localhost.evil.com:8181",
            "ftp://127.0.0.1",
            "not a url",
            "",
        ] {
            assert!(!is_loopback_endpoint(e), "{e}");
        }
    }

    #[test]
    fn creates_a_private_random_token_once_and_reuses_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join(LOCAL_BROKER_TOKEN_FILE);
        let (token, origin) = load_or_create_broker_token(&path).unwrap();
        assert_eq!(origin, BrokerTokenOrigin::Created);
        assert_eq!(token.len(), 64, "256-bit hex");
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "token file must be 0600");
            let dir_mode = std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(dir_mode & 0o777, 0o700, "created dir must be 0700");
        }
        let (again, origin) = load_or_create_broker_token(&path).unwrap();
        assert_eq!(origin, BrokerTokenOrigin::Loaded);
        assert_eq!(again, token, "restart keeps the same token");
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "no temp files left behind");
        let (other, _) = load_or_create_broker_token(&dir.path().join("other.token")).unwrap();
        assert_ne!(other, token, "tokens are random");
    }

    /// prA I3: a group/other-readable token may already have leaked; it is
    /// replaced by a NEW token (0600), not chmod-ed and reused.
    #[cfg(unix)]
    #[test]
    fn loose_token_file_is_rotated_not_reused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOCAL_BROKER_TOKEN_FILE);
        std::fs::write(&path, "leaked-token\n").unwrap();
        for mode in [0o644, 0o640, 0o604] {
            std::fs::write(&path, "leaked-token\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            let (token, origin) = load_or_create_broker_token(&path).unwrap();
            assert_eq!(origin, BrokerTokenOrigin::Rotated, "{mode:o}");
            assert_ne!(token, "leaked-token", "{mode:o}");
            assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), token);
            let now = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(now & 0o777, 0o600, "{mode:o}");
        }
        std::fs::write(&path, "  \n").unwrap();
        assert!(load_or_create_broker_token(&path).is_err(), "empty file refused");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_token_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("attacker-controlled");
        std::fs::write(&target, "attacker-token\n").unwrap();
        let path = dir.path().join(LOCAL_BROKER_TOKEN_FILE);
        std::os::unix::fs::symlink(&target, &path).unwrap();
        let err = load_or_create_broker_token(&path).unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err}");
        // A dangling symlink is refused too (never created through).
        std::fs::remove_file(&target).unwrap();
        assert!(load_or_create_broker_token(&path).is_err());
        assert!(!target.exists(), "never written through the link");
    }

    #[test]
    fn non_regular_token_path_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOCAL_BROKER_TOKEN_FILE);
        std::fs::create_dir(&path).unwrap();
        assert!(load_or_create_broker_token(&path).is_err());
    }

    /// Owner check: a file owned by another uid is refused. Needs root to
    /// chown, so it is exercised only when running as root.
    #[cfg(unix)]
    #[test]
    fn foreign_owned_token_file_is_refused_when_testable() {
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } != 0 {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOCAL_BROKER_TOKEN_FILE);
        std::fs::write(&path, "t\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::chown(&path, Some(65534), None).unwrap();
        let err = load_or_create_broker_token(&path).unwrap_err();
        assert!(err.to_string().contains("another user"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn client_read_refuses_symlinks_and_oversized_files() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("elsewhere");
        std::fs::write(&target, "secret-elsewhere\n").unwrap();
        let link = dir.path().join(LOCAL_BROKER_TOKEN_FILE);
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(read_token_at(&link).is_err(), "symlink never followed");
        let big = dir.path().join("big.token");
        std::fs::write(&big, "a".repeat(5000)).unwrap();
        assert!(read_token_at(&big).is_err(), "over 4 KiB refused");
        let ok = dir.path().join("ok.token");
        std::fs::write(&ok, "tok\n").unwrap();
        assert_eq!(read_token_at(&ok).unwrap().as_deref(), Some("tok"));
        assert_eq!(read_token_at(&dir.path().join("missing")).unwrap(), None);
    }

    /// prA I2: a loopback client re-reads the token file after a 401.
    #[test]
    fn machine_token_reloads_only_for_loopback_and_only_when_changed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOCAL_BROKER_TOKEN_FILE);
        let tok = MachineToken {
            current: std::sync::Arc::new(std::sync::RwLock::new(String::new())),
            reload_from: Some(path.clone()),
        };
        assert!(!tok.reload_after_unauthorized(), "no file yet");
        std::fs::write(&path, "fresh\n").unwrap();
        let shared = tok.clone();
        assert!(tok.reload_after_unauthorized());
        assert_eq!(shared.get(), "fresh", "clones share the reloaded token");
        assert!(!tok.reload_after_unauthorized(), "unchanged → no retry");
        let remote = MachineToken::for_endpoint("https://jade.jade:8181", "");
        assert!(!remote.reload_after_unauthorized());
        // prA N1: an explicit token is fixed, even on loopback.
        let explicit = MachineToken::for_endpoint("http://127.0.0.1:8181", "x");
        assert!(!explicit.is_from_local_file());
        assert!(!explicit.reload_after_unauthorized());
        assert_eq!(explicit.get(), "x");
        assert!(MachineToken::for_endpoint("http://127.0.0.1:8181", "").is_from_local_file());
    }
}
