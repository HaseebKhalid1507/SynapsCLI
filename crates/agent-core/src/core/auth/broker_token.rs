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

fn read_token_at(path: &Path) -> std::io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(s.trim().to_string()).filter(|t| !t.is_empty())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Broker side: read the token at `path`, or create it (0600, random 256-bit
/// hex) when absent. Returns `(token, created)`. An existing file readable by
/// group/other is tightened to 0600; an existing empty file is an error
/// (delete it to regenerate) rather than silently serving with no auth.
pub fn load_or_create_broker_token(path: &Path) -> std::io::Result<(String, bool)> {
    if let Some(parent) = path.parent() {
        create_private_dir(parent)?;
    }
    for _ in 0..2 {
        match read_token_at(path) {
            Ok(Some(token)) => {
                tighten_permissions(path)?;
                return Ok((token, false));
            }
            Ok(None) if path.exists() => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "broker token file {} is empty; delete it to generate a new one",
                        path.display()
                    ),
                ))
            }
            Ok(None) => {}
            Err(e) => return Err(e),
        }
        let token = generate_token();
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        match opts.open(path) {
            Ok(mut f) => {
                f.write_all(token.as_bytes())?;
                f.write_all(b"\n")?;
                f.sync_all()?;
                return Ok((token, true));
            }
            // A concurrent broker won the race: read its token.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other(format!(
        "could not create or read broker token file {}",
        path.display()
    )))
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

fn tighten_permissions(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)?.permissions().mode();
        if mode & 0o077 != 0 {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
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
        let (token, created) = load_or_create_broker_token(&path).unwrap();
        assert!(created);
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
        let (again, created) = load_or_create_broker_token(&path).unwrap();
        assert!(!created);
        assert_eq!(again, token, "restart keeps the same token");
        assert_eq!(read_token_at(&path).unwrap().as_deref(), Some(token.as_str()));
        let (other, _) =
            load_or_create_broker_token(&dir.path().join("other.token")).unwrap();
        assert_ne!(other, token, "tokens are random");
    }

    #[cfg(unix)]
    #[test]
    fn existing_loose_token_file_is_tightened_and_empty_file_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOCAL_BROKER_TOKEN_FILE);
        std::fs::write(&path, "abc\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let (token, created) = load_or_create_broker_token(&path).unwrap();
        assert_eq!((token.as_str(), created), ("abc", false));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);

        std::fs::write(&path, "  \n").unwrap();
        assert!(load_or_create_broker_token(&path).is_err());
    }
}
