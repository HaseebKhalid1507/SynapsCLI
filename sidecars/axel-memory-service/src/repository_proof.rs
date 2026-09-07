//! Read-only proof verifier for the host's Git-common-directory identity marker.
//! No marker creation, hooks, network, user Git config, or content remapping.
use crate::{contract::*, scope::Alias};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::Read,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt},
        },
    },
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
};

pub(crate) fn path_key(path: &Path) -> String {
    let digest = Sha256::digest(path.as_os_str().as_encoded_bytes());
    let mut key = String::from("p");
    for b in &digest[..8] {
        key.push_str(&format!("{b:02x}"));
    }
    key
}
fn git_path(root: &Path, arg: &str) -> Result<PathBuf> {
    let mut cmd = Command::new("git");
    cmd.env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--path-format=absolute", arg])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().map_err(|_| Error::Scope)?;
    let mut bytes = Vec::new();
    let read = child
        .stdout
        .take()
        .ok_or(Error::Scope)?
        .take(1024 * 1024 + 1)
        .read_to_end(&mut bytes);
    if read.is_err() || bytes.len() > 1024 * 1024 {
        let _ = child.kill();
        let _ = child.wait();
        return Err(Error::Scope);
    }
    if !child.wait()?.success() {
        return Err(Error::Scope);
    }
    let bytes = bytes.strip_suffix(b"\n").ok_or(Error::Scope)?;
    let path = PathBuf::from(std::ffi::OsStr::from_bytes(bytes));
    if !path.is_absolute() {
        return Err(Error::Scope);
    }
    path.canonicalize().map_err(|_| Error::Scope)
}
fn common(root: &Path) -> Result<PathBuf> {
    let entry = fs::symlink_metadata(root.join(".git")).map_err(|_| Error::Scope)?;
    if entry.file_type().is_symlink()
        || !(entry.is_file() || entry.is_dir())
        || git_path(root, "--show-toplevel")? != root
    {
        return Err(Error::Scope);
    }
    let dir = git_path(root, "--absolute-git-dir")?;
    let common = git_path(root, "--git-common-dir")?;
    if dir != common {
        if dir.parent() != Some(common.join("worktrees").as_path()) {
            return Err(Error::Scope);
        }
        let bytes = fs::read(dir.join("gitdir"))?;
        let bytes = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
        let back = PathBuf::from(std::ffi::OsStr::from_bytes(bytes));
        let back = if back.is_absolute() {
            back
        } else {
            dir.join(back)
        };
        if back.canonicalize()? != root.join(".git").canonicalize()? {
            return Err(Error::Scope);
        }
    }
    Ok(common)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Marker {
    version: u32,
    key: String,
    initial_root: PathBuf,
    roots: Vec<PathBuf>,
}
fn canonical_path(p: &Path) -> bool {
    p.is_absolute()
        && p.components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
}
fn private_marker(common: &Path) -> Result<(File, File)> {
    let mut directory = File::open("/")?;
    for part in common.components() {
        match part {
            Component::RootDir => {}
            Component::Normal(name) => {
                let name =
                    std::ffi::CString::new(name.as_bytes()).map_err(|_| Error::UnsafePath)?;
                let fd = unsafe {
                    libc::openat(
                        directory.as_raw_fd(),
                        name.as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                };
                if fd < 0 {
                    return Err(Error::UnsafePath);
                }
                directory = unsafe { File::from_raw_fd(fd) };
                let m = directory.metadata()?;
                if (m.uid() != unsafe { libc::geteuid() } && m.uid() != 0)
                    || (m.mode() & 0o002 != 0 && !(m.uid() == 0 && m.mode() & 0o1000 != 0))
                {
                    return Err(Error::UnsafePath);
                }
            }
            _ => return Err(Error::UnsafePath),
        }
    }
    let m = directory.metadata()?;
    if m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o002 != 0 {
        return Err(Error::UnsafePath);
    }
    let anchor = PathBuf::from(format!(
        "/proc/{}/fd/{}",
        std::process::id(),
        directory.as_raw_fd()
    ));
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(anchor.join("synaps-memory-identity.json"))?;
    let m = file.metadata()?;
    if !m.is_file()
        || m.uid() != unsafe { libc::geteuid() }
        || m.mode() & 0o7777 != 0o600
        || m.nlink() != 1
        || m.len() > 1024 * 1024
    {
        return Err(Error::UnsafePath);
    }
    Ok((directory, file))
}
pub(crate) fn verify(q: &Alias, canonical: &str) -> Result<()> {
    if !valid_project(canonical)
        || canonical == USER_PROJECT
        || q.alias_project == USER_PROJECT
        || !valid_project(&q.alias_project)
        || !canonical_path(&q.alias_root)
        || !canonical_path(&q.canonical_root)
        || path_key(&q.alias_root) != q.alias_project
    {
        return Err(Error::Scope);
    }
    let root = q.canonical_root.canonicalize().map_err(|_| Error::Scope)?;
    // Main/common-dir fallback supports bare and separate-git-dir repository roots.
    let shared = if root.join(".git").symlink_metadata().is_ok() {
        common(&root)?
    } else {
        git_path(&root, "--git-common-dir")?
    };
    let (_directory, mut file) = private_marker(&shared)?;
    let before = file.metadata()?;
    let mut bytes = Vec::new();
    (&mut file).take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 1024 * 1024 {
        return Err(Error::TooLarge);
    }
    let marker: Marker = serde_json::from_slice(&bytes)?;
    if marker.version != 2
        || marker.key != canonical
        || !valid_project(&marker.key)
        || marker.key == USER_PROJECT
        || marker.roots.is_empty()
        || marker.roots.len() > 4096
        || !marker.roots.contains(&marker.initial_root)
        || !marker.roots.contains(&q.alias_root)
        || !marker.roots.iter().all(|p| canonical_path(p))
    {
        return Err(Error::Scope);
    }
    match fs::symlink_metadata(&q.alias_root) {
        Ok(m) => {
            if !m.is_dir()
                || q.alias_root.canonicalize()? != q.alias_root
                || common(&q.alias_root)? != shared
            {
                return Err(Error::Scope);
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {} // durable historical root after move
        Err(_) => return Err(Error::Scope),
    }
    let (dir2, file2) = private_marker(&shared)?;
    let after = file2.metadata()?;
    if before.dev() != after.dev()
        || before.ino() != after.ino()
        || before.len() != after.len()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
    {
        return Err(Error::Scope);
    }
    if dir2.metadata()?.ino() != _directory.metadata()?.ino() {
        return Err(Error::Scope);
    }
    Ok(())
}
