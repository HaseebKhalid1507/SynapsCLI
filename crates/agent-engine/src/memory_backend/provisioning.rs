//! Provision only the configured user-default brain parent, never repair/adopt
//! an explicit path. Descriptor-relative traversal refuses symlink components.
use super::{error, Result};
use std::path::Path;

pub(super) fn ensure_default_parent(base: &Path, brain: &Path) -> Result<()> {
    if brain != base.join("memory/axel/brain.r8") || !base.is_absolute() {
        return Err(error("invalid managed Axel brain path"));
    }
    #[cfg(target_os = "linux")]
    {
        use std::ffi::CString;
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::MetadataExt;
        let mut directory =
            std::fs::File::open("/").map_err(|_| error("Axel parent unavailable"))?;
        let parent = brain.parent().expect("absolute brain");
        let mut traversed = std::path::PathBuf::from("/");
        for component in parent.components() {
            let std::path::Component::Normal(name) = component else {
                if component == std::path::Component::RootDir {
                    continue;
                }
                return Err(error("Axel parent traversal refused"));
            };
            traversed.push(name);
            let name =
                CString::new(name.as_bytes()).map_err(|_| error("invalid parent component"))?;
            let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
            let mut fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
            if fd < 0
                && std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound
                && traversed.starts_with(base)
            {
                let result = unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o700) };
                if result != 0
                    && std::io::Error::last_os_error().kind() != std::io::ErrorKind::AlreadyExists
                {
                    return Err(error("cannot provision private Axel parent"));
                }
                fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
            }
            if fd < 0 {
                return Err(error("Axel parent unavailable or symlink refused"));
            }
            let next = unsafe { std::fs::File::from_raw_fd(fd) };
            if traversed.starts_with(base) {
                let meta = next
                    .metadata()
                    .map_err(|_| error("Axel parent metadata unavailable"))?;
                if meta.uid() != unsafe { libc::geteuid() }
                    || (traversed == parent && meta.mode() & 0o077 != 0)
                    || meta.mode() & 0o002 != 0
                {
                    return Err(error(
                        "Axel managed parent must be private and user-owned; no permission repair",
                    ));
                }
            }
            directory
                .sync_all()
                .map_err(|_| error("Axel parent durability unavailable"))?;
            directory = next;
        }
        directory
            .sync_all()
            .map_err(|_| error("Axel parent durability unavailable"))?;
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    Err(error("shared Axel storage requires Linux/procfs"))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    #[test]
    fn existing_group_writable_legacy_parent_is_not_chmodded_but_leaf_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("host");
        let memory = base.join("memory");
        std::fs::create_dir_all(&memory).unwrap();
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o775)).unwrap();
        std::fs::set_permissions(&memory, std::fs::Permissions::from_mode(0o775)).unwrap();
        let brain = memory.join("axel/brain.r8");
        ensure_default_parent(&base, &brain).unwrap();
        assert_eq!(
            std::fs::metadata(&memory).unwrap().permissions().mode() & 0o777,
            0o775
        );
        assert_eq!(
            std::fs::metadata(brain.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        std::fs::set_permissions(
            brain.parent().unwrap(),
            std::fs::Permissions::from_mode(0o775),
        )
        .unwrap();
        assert!(ensure_default_parent(&base, &brain).is_err());
    }

    #[test]
    fn provision_is_private_idempotent_and_symlink_refusing() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("host");
        let brain = base.join("memory/axel/brain.r8");
        ensure_default_parent(&base, &brain).unwrap();
        ensure_default_parent(&base, &brain).unwrap();
        assert!(!brain.exists());
        assert_eq!(
            std::fs::metadata(brain.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let other = tmp.path().join("other");
        std::fs::create_dir(&other).unwrap();
        let fake = tmp.path().join("fake");
        symlink(&other, &fake).unwrap();
        assert!(ensure_default_parent(&fake, &fake.join("memory/axel/brain.r8")).is_err());
        assert!(!other.join("memory").exists());
    }
}
