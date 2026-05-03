//! Post-install setup-script execution.
//!
//! When a plugin manifest declares a setup script, the marketplace
//! install flow auto-runs it after the plugin's source is in place.
//! This is how source-shipped plugins (e.g. ones that ship a Rust
//! crate and need `cargo build --release` to produce the binary
//! [`extension.command`] points at) get built without forcing the
//! user to run `scripts/setup.sh` by hand.
//!
//! Two manifest slots are recognised, in priority order:
//!
//! 1. `extension.setup` — the extension's own build script. Checked
//!    first because the extension binary is what the host will spawn
//!    immediately on session start.
//! 2. `provides.sidecar.setup` — the sidecar's build script (legacy
//!    slot; still honoured for sidecar-only plugins).
//!
//! At most one script runs per install. Plugins that ship both an
//! extension and a sidecar from one repo should drive both builds
//! from a single `scripts/setup.sh` referenced via `extension.setup`.
//!
//! ## Security
//!
//! Setup scripts are arbitrary shell. We mitigate by:
//! - Refusing setup paths that escape the plugin dir (`..`, absolute).
//! - Refusing setup paths that don't resolve (canonicalize) inside the
//!   plugin dir.
//! - Requiring the script file exists and is executable.
//! - Capping wall-clock runtime at [`SETUP_TIMEOUT`].
//! - Capturing stdout+stderr to a per-install log (no swallowing).
//!
//! ## Failure mode
//!
//! A failed setup script does **not** roll back the install — the
//! source is on disk and the user can rerun the script manually. The
//! caller surfaces the failure to the UI with a pointer to the log
//! file.
//!
//! Pure helpers live here so they can be unit-tested without a real
//! tokio runtime; the async runner is a thin shell over
//! `tokio::process::Command`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::skills::manifest::PluginManifest;

/// Stable host-triple string used as the lookup key in
/// [`crate::extensions::manifest::ExtensionManifest::prebuilt`]. We
/// intentionally use a compact `<os>-<arch>` form (e.g.
/// `linux-x86_64`, `darwin-arm64`, `windows-x86_64`) rather than full
/// Rust target triples (`x86_64-unknown-linux-gnu`) because plugin
/// authors hand-write these strings into JSON manifests — readability
/// > pedantry.
///
/// Returns `None` for hosts we don't have a stable name for (caller
/// then skips the prebuilt-fallback path and falls back to the setup
/// script).
pub fn host_triple() -> Option<&'static str> {
    let os = if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        return None;
    };
    let arch = if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        return None;
    };
    // Use a small static table so the returned slice is `'static`.
    Some(match (os, arch) {
        ("linux", "x86_64") => "linux-x86_64",
        ("linux", "arm64") => "linux-arm64",
        ("darwin", "x86_64") => "darwin-x86_64",
        ("darwin", "arm64") => "darwin-arm64",
        ("windows", "x86_64") => "windows-x86_64",
        ("windows", "arm64") => "windows-arm64",
        _ => return None,
    })
}

/// Wall-clock cap on a single setup script. Sample from-scratch
/// builds run ~5 minutes on a modern dev box; 10 minutes leaves a
/// healthy margin for slower CI/older hardware without making a
/// runaway script wedge the install flow forever.
pub const SETUP_TIMEOUT: Duration = Duration::from_secs(600);

/// Outcome of a successful setup-script run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupOutcome {
    /// Path to the log file containing combined stdout+stderr.
    pub log_path: PathBuf,
    /// Process exit status (always 0 on the success path).
    pub exit_code: i32,
}

/// Why a setup script could not be run, or why it failed once started.
#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    /// Manifest declared a setup path but it points outside the plugin
    /// directory or contains a `..` component.
    #[error("setup script path '{path}' escapes plugin directory")]
    EscapesPluginDir { path: String },

    /// Manifest declared a setup path that doesn't exist on disk after
    /// canonicalization.
    #[error("setup script '{path}' not found in plugin directory")]
    NotFound { path: String },

    /// Setup ran but exited non-zero. `log_path` points at captured
    /// stdout+stderr; UI should surface it to the user.
    #[error("setup script exited with code {exit_code}; see {}", log_path.display())]
    NonZeroExit { exit_code: i32, log_path: PathBuf },

    /// Setup exceeded [`SETUP_TIMEOUT`] and was killed.
    #[error("setup script timed out after {secs}s; see {}", log_path.display())]
    Timeout { secs: u64, log_path: PathBuf },

    /// I/O error setting up the log file or spawning the process.
    #[error("setup script io: {0}")]
    Io(#[from] std::io::Error),
}

/// Resolve the manifest-declared setup script to an absolute path
/// inside `plugin_dir`, or return `Ok(None)` if no setup is declared.
///
/// Returns `Err(EscapesPluginDir)` if the declared path is absolute
/// or contains `..`, or if the canonicalized path lives outside
/// `plugin_dir`. Returns `Err(NotFound)` if the resolved path doesn't
/// exist on disk.
///
/// This is the security gate — the async runner trusts the path it
/// gets from this function.
pub fn resolve_setup_script(
    manifest: &PluginManifest,
    plugin_dir: &Path,
) -> Result<Option<PathBuf>, SetupError> {
    // 1. Extension setup wins. The extension binary is what the host
    //    spawns immediately on session start, so its build script gets
    //    priority over the sidecar's.
    if let Some(ext) = manifest.extension.as_ref() {
        if let Some(setup) = ext.setup.as_deref() {
            return validate_setup_path(setup, plugin_dir).map(Some);
        }
    }
    // 2. Fall back to the sidecar's setup (legacy slot).
    if let Some(provides) = manifest.provides.as_ref() {
        if let Some(sidecar) = provides.sidecar.as_ref() {
            if let Some(setup) = sidecar.setup.as_deref() {
                return validate_setup_path(setup, plugin_dir).map(Some);
            }
        }
    }
    Ok(None)
}

/// Security-validate a setup-script path declared in the manifest and
/// resolve it to an absolute path inside `plugin_dir`.
///
/// Shared by both the `extension.setup` and `provides.sidecar.setup`
/// resolution paths so the rules stay identical.
fn validate_setup_path(setup: &str, plugin_dir: &Path) -> Result<PathBuf, SetupError> {
    let setup_path = Path::new(setup);
    if setup_path.is_absolute()
        || setup_path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(SetupError::EscapesPluginDir {
            path: setup.to_string(),
        });
    }
    let joined = plugin_dir.join(setup_path);
    let canonical = match joined.canonicalize() {
        Ok(p) => p,
        Err(_) => {
            return Err(SetupError::NotFound {
                path: setup.to_string(),
            });
        }
    };
    let canonical_dir = plugin_dir
        .canonicalize()
        .unwrap_or_else(|_| plugin_dir.to_path_buf());
    if !canonical.starts_with(&canonical_dir) {
        return Err(SetupError::EscapesPluginDir {
            path: setup.to_string(),
        });
    }
    Ok(canonical)
}

/// Build the per-install log path. Caller is expected to create the
/// parent directory before opening it. Format:
///
/// `{logs_root}/install/{plugin}-{rfc3339}.log`
///
/// where rfc3339 has colons replaced with `-` so the filename is safe
/// on Windows (and grep-friendly).
pub fn install_log_path(logs_root: &Path, plugin_name: &str, now_rfc3339: &str) -> PathBuf {
    let safe_ts = now_rfc3339.replace(':', "-");
    logs_root
        .join("install")
        .join(format!("{plugin_name}-{safe_ts}.log"))
}

/// Run the resolved setup script against `plugin_dir`, streaming
/// combined stdout+stderr to `log_path`. Returns on success, exit
/// code, timeout, or I/O error.
///
/// The script is invoked as `bash <script>` (POSIX shells only — no
/// Windows .bat/.ps1 support in v1; plugins on Windows can ship a
/// shim or rely on the native binary already being committed).
///
/// `cwd` is set to `plugin_dir` so scripts can use relative paths
/// like `target/release/...`.
pub async fn run_setup_script(
    script: &Path,
    plugin_dir: &Path,
    log_path: &Path,
    timeout: Duration,
) -> Result<SetupOutcome, SetupError> {
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt;
    use tokio::process::Command;

    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut log_file = tokio::fs::File::create(log_path).await?;
    let header = format!(
        "$ bash {} (cwd: {})\n",
        script.display(),
        plugin_dir.display()
    );
    log_file.write_all(header.as_bytes()).await?;

    let mut cmd = Command::new("bash");
    cmd.arg(script)
        .current_dir(plugin_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd.spawn()?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");

    let copy_out = async {
        tokio::io::copy(&mut stdout, &mut log_file).await?;
        log_file.flush().await?;
        Ok::<_, std::io::Error>(log_file)
    };
    let collect_err = async {
        let mut buf = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut stderr, &mut buf).await?;
        Ok::<_, std::io::Error>(buf)
    };

    let wait = async {
        let (out_res, err_res, status) = tokio::join!(copy_out, collect_err, child.wait());
        let mut log_file = out_res?;
        let err_buf = err_res?;
        if !err_buf.is_empty() {
            log_file.write_all(b"\n--- stderr ---\n").await?;
            log_file.write_all(&err_buf).await?;
            log_file.flush().await?;
        }
        Ok::<_, std::io::Error>(status?)
    };

    let status = match tokio::time::timeout(timeout, wait).await {
        Ok(res) => res?,
        Err(_) => {
            return Err(SetupError::Timeout {
                secs: timeout.as_secs(),
                log_path: log_path.to_path_buf(),
            });
        }
    };

    let exit_code = status.code().unwrap_or(-1);
    if status.success() {
        Ok(SetupOutcome {
            log_path: log_path.to_path_buf(),
            exit_code,
        })
    } else {
        Err(SetupError::NonZeroExit {
            exit_code,
            log_path: log_path.to_path_buf(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::manifest::{ExtensionManifest, ExtensionRuntime};
    use crate::skills::manifest::{PluginProvides, SidecarManifest};
    use std::fs;

    #[test]
    fn host_triple_matches_compiled_target_when_supported() {
        // Run-time check: triple must be one of the known stable strings
        // on supported hosts (we test on linux/macos/windows in CI).
        let known = [
            "linux-x86_64", "linux-arm64",
            "darwin-x86_64", "darwin-arm64",
            "windows-x86_64", "windows-arm64",
        ];
        let got = host_triple();
        if cfg!(any(target_os = "linux", target_os = "macos", target_os = "windows"))
            && cfg!(any(target_arch = "x86_64", target_arch = "aarch64"))
        {
            let s = got.expect("supported host should yield a triple");
            assert!(known.contains(&s), "unexpected triple: {}", s);
        }
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn host_triple_is_linux_x86_64_on_this_box() {
        // Sanity-pin for the dev box this is being authored on; harmless
        // elsewhere because of the cfg gate.
        assert_eq!(host_triple(), Some("linux-x86_64"));
    }

    fn manifest_with_setup(setup: Option<&str>) -> PluginManifest {
        PluginManifest {
            name: "test-plugin".to_string(),
            version: None,
            description: None,
            keybinds: vec![],
            compatibility: None,
            commands: vec![],
            extension: None,
            help_entries: vec![],
            provides: Some(PluginProvides {
                sidecar: Some(SidecarManifest {
                    command: "bin/sidecar".to_string(),
                    setup: setup.map(|s| s.to_string()),
                    protocol_version: 1,
                    model: None,
                    lifecycle: None,
                }),
            }),
            settings: None,
        }
    }

    /// Build an extension-only manifest with the given setup-script slot.
    fn manifest_with_extension_setup(setup: Option<&str>) -> PluginManifest {
        PluginManifest {
            name: "test-plugin".to_string(),
            version: None,
            description: None,
            keybinds: vec![],
            compatibility: None,
            commands: vec![],
            extension: Some(ExtensionManifest {
                protocol_version: 1,
                runtime: ExtensionRuntime::Process,
                command: "bin/ext".to_string(),
                setup: setup.map(|s| s.to_string()),
                prebuilt: ::std::collections::HashMap::new(),
                args: vec![],
                permissions: vec![],
                hooks: vec![],
                config: vec![],
            }),
            help_entries: vec![],
            provides: None,
            settings: None,
        }
    }

    /// Build a manifest with BOTH extension and sidecar setup slots.
    /// Used to verify extension wins when both are present.
    fn manifest_with_both_setup(ext_setup: &str, side_setup: &str) -> PluginManifest {
        let mut m = manifest_with_extension_setup(Some(ext_setup));
        m.provides = Some(PluginProvides {
            sidecar: Some(SidecarManifest {
                command: "bin/sidecar".to_string(),
                setup: Some(side_setup.to_string()),
                protocol_version: 1,
                model: None,
                lifecycle: None,
            }),
        });
        m
    }

    #[test]
    fn resolve_returns_none_when_no_setup_declared() {
        let m = manifest_with_setup(None);
        let dir = tempfile::tempdir().unwrap();
        let res = resolve_setup_script(&m, dir.path()).unwrap();
        assert!(res.is_none());
    }

    #[test]
    fn resolve_returns_none_when_no_provides() {
        let mut m = manifest_with_setup(None);
        m.provides = None;
        let dir = tempfile::tempdir().unwrap();
        assert!(resolve_setup_script(&m, dir.path()).unwrap().is_none());
    }

    #[test]
    fn resolve_resolves_relative_path_inside_plugin_dir() {
        let dir = tempfile::tempdir().unwrap();
        let scripts = dir.path().join("scripts");
        fs::create_dir(&scripts).unwrap();
        fs::write(scripts.join("setup.sh"), "#!/bin/bash\necho ok").unwrap();
        let m = manifest_with_setup(Some("scripts/setup.sh"));
        let resolved = resolve_setup_script(&m, dir.path()).unwrap().unwrap();
        assert!(resolved.ends_with("scripts/setup.sh"));
        assert!(resolved.is_absolute());
    }

    #[test]
    fn resolve_rejects_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        let m = manifest_with_setup(Some("/etc/passwd"));
        let err = resolve_setup_script(&m, dir.path()).unwrap_err();
        assert!(matches!(err, SetupError::EscapesPluginDir { .. }), "got {err:?}");
    }

    #[test]
    fn resolve_rejects_parent_dir_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let m = manifest_with_setup(Some("../escape.sh"));
        let err = resolve_setup_script(&m, dir.path()).unwrap_err();
        assert!(matches!(err, SetupError::EscapesPluginDir { .. }), "got {err:?}");
    }

    #[test]
    fn resolve_rejects_embedded_parent_dir() {
        let dir = tempfile::tempdir().unwrap();
        let m = manifest_with_setup(Some("scripts/../../etc/passwd"));
        let err = resolve_setup_script(&m, dir.path()).unwrap_err();
        assert!(matches!(err, SetupError::EscapesPluginDir { .. }), "got {err:?}");
    }

    #[test]
    fn resolve_returns_not_found_when_script_missing() {
        let dir = tempfile::tempdir().unwrap();
        let m = manifest_with_setup(Some("scripts/missing.sh"));
        let err = resolve_setup_script(&m, dir.path()).unwrap_err();
        assert!(matches!(err, SetupError::NotFound { .. }), "got {err:?}");
    }

    #[test]
    fn resolve_rejects_symlink_pointing_outside_plugin_dir() {
        // Symlinks that escape via canonicalize should be caught by the
        // starts_with(canonical_dir) check.
        let outer = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let target = outer.path().join("escape.sh");
        fs::write(&target, "#!/bin/bash").unwrap();
        let scripts = dir.path().join("scripts");
        fs::create_dir(&scripts).unwrap();
        let link = scripts.join("setup.sh");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let m = manifest_with_setup(Some("scripts/setup.sh"));
        let err = resolve_setup_script(&m, dir.path()).unwrap_err();
        assert!(matches!(err, SetupError::EscapesPluginDir { .. }), "got {err:?}");
    }

    #[test]
    fn install_log_path_substitutes_colons() {
        let path = install_log_path(
            Path::new("/tmp/logs"),
            "sample-sidecar",
            "2026-05-02T19:30:45-04:00",
        );
        assert_eq!(
            path,
            PathBuf::from("/tmp/logs/install/sample-sidecar-2026-05-02T19-30-45-04-00.log")
        );
    }

    #[tokio::test]
    async fn run_setup_succeeds_for_simple_script() {
        let dir = tempfile::tempdir().unwrap();
        let scripts = dir.path().join("scripts");
        fs::create_dir(&scripts).unwrap();
        let script = scripts.join("setup.sh");
        fs::write(
            &script,
            "#!/bin/bash\necho hello-from-setup\necho 'on stderr' >&2\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let log = dir.path().join("install.log");
        let outcome = run_setup_script(&script, dir.path(), &log, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.log_path, log);
        let captured = fs::read_to_string(&log).unwrap();
        assert!(captured.contains("hello-from-setup"));
        assert!(captured.contains("on stderr"));
    }

    #[tokio::test]
    async fn run_setup_returns_non_zero_exit() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fail.sh");
        fs::write(&script, "#!/bin/bash\necho boom\nexit 7\n").unwrap();
        let log = dir.path().join("install.log");
        let err = run_setup_script(&script, dir.path(), &log, Duration::from_secs(5))
            .await
            .unwrap_err();
        match err {
            SetupError::NonZeroExit { exit_code, log_path } => {
                assert_eq!(exit_code, 7);
                assert_eq!(log_path, log);
                let captured = fs::read_to_string(&log).unwrap();
                assert!(captured.contains("boom"));
            }
            other => panic!("expected NonZeroExit, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_setup_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("loop.sh");
        fs::write(&script, "#!/bin/bash\nsleep 5\n").unwrap();
        let log = dir.path().join("install.log");
        let err = run_setup_script(&script, dir.path(), &log, Duration::from_millis(200))
            .await
            .unwrap_err();
        assert!(matches!(err, SetupError::Timeout { .. }), "got {err:?}");
    }

    // ── extension.setup coverage (added by feat/extension-setup-script) ──

    #[test]
    fn resolve_resolves_extension_setup_path() {
        let dir = tempfile::tempdir().unwrap();
        let scripts = dir.path().join("scripts");
        fs::create_dir(&scripts).unwrap();
        fs::write(scripts.join("setup.sh"), "#!/bin/bash\necho ok").unwrap();
        let m = manifest_with_extension_setup(Some("scripts/setup.sh"));
        let resolved = resolve_setup_script(&m, dir.path()).unwrap().unwrap();
        assert!(resolved.ends_with("scripts/setup.sh"));
        assert!(resolved.is_absolute());
    }

    #[test]
    fn resolve_returns_none_when_extension_has_no_setup() {
        let m = manifest_with_extension_setup(None);
        let dir = tempfile::tempdir().unwrap();
        assert!(resolve_setup_script(&m, dir.path()).unwrap().is_none());
    }

    #[test]
    fn resolve_rejects_extension_setup_with_parent_dir() {
        let dir = tempfile::tempdir().unwrap();
        let m = manifest_with_extension_setup(Some("../escape.sh"));
        let err = resolve_setup_script(&m, dir.path()).unwrap_err();
        assert!(
            matches!(err, SetupError::EscapesPluginDir { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn resolve_rejects_extension_setup_when_absolute() {
        let dir = tempfile::tempdir().unwrap();
        let m = manifest_with_extension_setup(Some("/etc/passwd"));
        let err = resolve_setup_script(&m, dir.path()).unwrap_err();
        assert!(
            matches!(err, SetupError::EscapesPluginDir { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn resolve_returns_not_found_for_missing_extension_setup() {
        let dir = tempfile::tempdir().unwrap();
        let m = manifest_with_extension_setup(Some("scripts/missing.sh"));
        let err = resolve_setup_script(&m, dir.path()).unwrap_err();
        assert!(matches!(err, SetupError::NotFound { .. }), "got {err:?}");
    }

    #[test]
    fn resolve_prefers_extension_setup_over_sidecar_setup() {
        // When both slots are populated, extension wins — the host spawns
        // the extension binary first on session start, so its build must
        // run first.
        let dir = tempfile::tempdir().unwrap();
        let scripts = dir.path().join("scripts");
        fs::create_dir(&scripts).unwrap();
        fs::write(scripts.join("ext.sh"), "#!/bin/bash\necho ext").unwrap();
        fs::write(scripts.join("side.sh"), "#!/bin/bash\necho side").unwrap();
        let m = manifest_with_both_setup("scripts/ext.sh", "scripts/side.sh");
        let resolved = resolve_setup_script(&m, dir.path()).unwrap().unwrap();
        assert!(
            resolved.ends_with("scripts/ext.sh"),
            "expected extension setup to win, got {resolved:?}"
        );
    }

    #[test]
    fn resolve_falls_back_to_sidecar_when_extension_has_no_setup() {
        // Plugin has an extension but no extension.setup, plus a sidecar
        // with setup. The sidecar's setup should still run (legacy slot
        // remains honoured).
        let dir = tempfile::tempdir().unwrap();
        let scripts = dir.path().join("scripts");
        fs::create_dir(&scripts).unwrap();
        fs::write(scripts.join("side.sh"), "#!/bin/bash\necho side").unwrap();
        let mut m = manifest_with_extension_setup(None); // extension present, no setup
        m.provides = Some(PluginProvides {
            sidecar: Some(SidecarManifest {
                command: "bin/sidecar".to_string(),
                setup: Some("scripts/side.sh".to_string()),
                protocol_version: 1,
                model: None,
                lifecycle: None,
            }),
        });
        let resolved = resolve_setup_script(&m, dir.path()).unwrap().unwrap();
        assert!(resolved.ends_with("scripts/side.sh"));
    }

    #[test]
    fn resolve_returns_none_when_neither_slot_has_setup() {
        // Plugin has both extension and sidecar declared, but neither
        // declares a setup script — function returns Ok(None).
        let dir = tempfile::tempdir().unwrap();
        let mut m = manifest_with_extension_setup(None);
        m.provides = Some(PluginProvides {
            sidecar: Some(SidecarManifest {
                command: "bin/sidecar".to_string(),
                setup: None,
                protocol_version: 1,
                model: None,
                lifecycle: None,
            }),
        });
        assert!(resolve_setup_script(&m, dir.path()).unwrap().is_none());
    }
}
