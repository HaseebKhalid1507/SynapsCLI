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

/// Why an extension command verification failed. Distinct from
/// [`SetupError`] because the failure mode and remediation are
/// different — here, the build "succeeded" but the artifact the
/// manifest promised isn't there.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CommandVerifyError {
    /// `extension.command` resolves to a relative path that escapes the
    /// plugin directory (`..` traversal or symlink that points outside).
    #[error("extension command path '{path}' escapes plugin directory")]
    EscapesPluginDir { path: String },

    /// The resolved path doesn't exist on disk. Most common cause:
    /// setup script ran, exited 0, but didn't actually produce the
    /// declared binary.
    #[error("extension command '{path}' does not exist (resolved to {})", resolved.display())]
    Missing { path: String, resolved: PathBuf },

    /// The path exists but isn't executable (Unix only — Windows skips
    /// this check). Common cause: build artifact missing the +x bit
    /// after extraction from a source archive.
    #[cfg(unix)]
    #[error("extension command '{path}' exists but is not executable (mode {mode:o})")]
    NotExecutable { path: String, mode: u32 },

    /// The path resolves to a directory, not a file.
    #[error("extension command '{path}' is a directory, not a file")]
    NotAFile { path: String },
}

/// Verify that the extension binary declared by
/// [`crate::extensions::manifest::ExtensionManifest::command`] actually
/// exists and is executable inside `plugin_dir`. Used as the
/// post-condition check after [`run_setup_script`] succeeds, so a
/// build script that exits 0 but doesn't produce the promised binary
/// surfaces a clear error instead of silently breaking spawn at
/// runtime.
///
/// Mirrors the host-side resolution rules in
/// [`crate::extensions::manager`]:
/// - `command` is **absolute**: verify the absolute path
/// - `command` is **bare** (no path separator): skip — it's a PATH
///   lookup, not a plugin-shipped artifact
/// - `command` is **relative with separators**: join with `plugin_dir`,
///   canonicalize, ensure it stays inside `plugin_dir`, then verify
///
/// Returns `Ok(None)` when the manifest declares no extension or the
/// command is a bare PATH lookup (nothing to verify).
/// Returns `Ok(Some(resolved_path))` on successful verification.
pub fn verify_extension_command(
    manifest: &PluginManifest,
    plugin_dir: &Path,
) -> Result<Option<PathBuf>, CommandVerifyError> {
    let Some(ext) = manifest.extension.as_ref() else {
        return Ok(None);
    };
    let cmd = &ext.command;
    let cmd_path = Path::new(cmd);

    // Bare command name (e.g. "python3") — defer to PATH at spawn time.
    if !cmd.contains(std::path::MAIN_SEPARATOR) && !cmd.contains('/') {
        return Ok(None);
    }

    let resolved = if cmd_path.is_absolute() {
        cmd_path.to_path_buf()
    } else {
        // Reject `..` traversal up front (don't rely on canonicalize).
        if cmd_path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(CommandVerifyError::EscapesPluginDir { path: cmd.clone() });
        }
        let joined = plugin_dir.join(cmd_path);
        match joined.canonicalize() {
            Ok(p) => {
                let canonical_dir = plugin_dir
                    .canonicalize()
                    .unwrap_or_else(|_| plugin_dir.to_path_buf());
                if !p.starts_with(&canonical_dir) {
                    return Err(CommandVerifyError::EscapesPluginDir { path: cmd.clone() });
                }
                p
            }
            Err(_) => {
                return Err(CommandVerifyError::Missing {
                    path: cmd.clone(),
                    resolved: joined,
                });
            }
        }
    };

    if !resolved.exists() {
        return Err(CommandVerifyError::Missing {
            path: cmd.clone(),
            resolved,
        });
    }
    let meta = std::fs::metadata(&resolved).map_err(|_| CommandVerifyError::Missing {
        path: cmd.clone(),
        resolved: resolved.clone(),
    })?;
    if meta.is_dir() {
        return Err(CommandVerifyError::NotAFile { path: cmd.clone() });
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode();
        // Any execute bit is enough — owner/group/other.
        if mode & 0o111 == 0 {
            return Err(CommandVerifyError::NotExecutable {
                path: cmd.clone(),
                mode,
            });
        }
    }
    Ok(Some(resolved))
}

/// Why a prebuilt-binary install attempt failed. Variants distinguish
/// "no asset for this host" (caller falls back to the setup script)
/// from "asset matched but couldn't be installed" (caller surfaces
/// the error — security failures and network issues should not
/// silently trigger a build).
#[derive(Debug, thiserror::Error)]
pub enum PrebuiltError {
    /// Host triple has no entry in `extension.prebuilt`. Caller should
    /// fall back to the setup script. Not really an error — just a
    /// signal that there's nothing to try.
    #[error("no prebuilt asset declared for this host")]
    NoMatchingAsset,

    /// Network / HTTP problem fetching the URL.
    #[error("download failed: {0}")]
    Download(String),

    /// Downloaded bytes don't match the declared SHA-256. Treated as a
    /// hard failure (don't fall back to setup) since this could
    /// indicate tampering, mirror corruption, or a stale manifest.
    #[error("checksum mismatch: expected {expected}, got {actual}")]
    ChecksumMismatch { expected: String, actual: String },

    /// `tar` / `unzip` exited non-zero or wasn't on PATH.
    #[error("archive extraction failed: {0}")]
    Extract(String),

    /// The archive doesn't end in a recognized suffix (we support
    /// `.tar.gz` / `.tgz` / `.tar.xz` / `.tar.bz2` / `.zip`).
    #[error("unsupported archive type for url '{url}'")]
    UnsupportedArchive { url: String },

    /// Asset URL must be `https://` (or `file://` in tests).
    #[error("refusing non-https prebuilt url '{url}'")]
    UnsafeUrl { url: String },

    /// I/O setting up the temp file or moving extracted artifacts.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// Asset extracted but the manifest's `extension.command` still
    /// doesn't resolve. The archive layout is wrong.
    #[error("prebuilt extracted but extension command not found: {0}")]
    Verify(#[from] CommandVerifyError),
}

/// Lower-case hex encode of arbitrary bytes. Inlined to avoid pulling
/// in a `hex` crate just for this one site.
fn hex_encode_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from_digit((*b >> 4) as u32, 16).unwrap());
        out.push(char::from_digit((*b & 0x0f) as u32, 16).unwrap());
    }
    out
}

/// Try to install the extension binary from
/// [`crate::extensions::manifest::ExtensionManifest::prebuilt`] for
/// the current host. Lookup is by [`host_triple`].
///
/// On success: downloads the URL, verifies the SHA-256, extracts
/// the archive into `plugin_dir`, then runs
/// [`verify_extension_command`] to confirm the layout was correct.
/// Returns `Ok(Some(path))` pointing at the resolved binary.
///
/// On `Err(PrebuiltError::NoMatchingAsset)`: no entry for this host
/// — caller should fall back to the setup script.
///
/// On any other `Err`: surface to the user; do **not** silently fall
/// back to the setup script (a checksum failure could mean tampering;
/// a network failure means the user wanted prebuilt and should know).
pub async fn try_install_from_prebuilt(
    manifest: &PluginManifest,
    plugin_dir: &Path,
) -> Result<PathBuf, PrebuiltError> {
    let Some(ext) = manifest.extension.as_ref() else {
        return Err(PrebuiltError::NoMatchingAsset);
    };
    let Some(triple) = host_triple() else {
        return Err(PrebuiltError::NoMatchingAsset);
    };
    let Some(asset) = ext.prebuilt.get(triple) else {
        return Err(PrebuiltError::NoMatchingAsset);
    };

    // URL safety: https only, with an env-var carve-out
    // (`SYNAPS_ALLOW_FILE_PREBUILT=1`) so tests and local-dev mirrors
    // can use `file://` without weakening production behavior.
    let allow_file_scheme = std::env::var("SYNAPS_ALLOW_FILE_PREBUILT")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if !asset.url.starts_with("https://")
        && !(allow_file_scheme && asset.url.starts_with("file://"))
    {
        return Err(PrebuiltError::UnsafeUrl {
            url: asset.url.clone(),
        });
    }

    // Download into a temp file inside plugin_dir so cleanup is
    // automatic if extraction fails.
    let tmp_archive = plugin_dir.join(format!(
        ".prebuilt-{}-download",
        triple,
    ));
    let _ = std::fs::remove_file(&tmp_archive);

    let bytes = if let Some(path) = asset.url.strip_prefix("file://") {
        // Test path: read directly from disk.
        std::fs::read(path).map_err(|e| PrebuiltError::Download(format!("file read {path}: {e}")))?
    } else {
        // Real path: HTTP GET.
        let response = reqwest::get(&asset.url)
            .await
            .map_err(|e| PrebuiltError::Download(e.to_string()))?;
        if !response.status().is_success() {
            return Err(PrebuiltError::Download(format!(
                "HTTP {}",
                response.status()
            )));
        }
        response
            .bytes()
            .await
            .map_err(|e| PrebuiltError::Download(e.to_string()))?
            .to_vec()
    };

    // Checksum the bytes before touching disk.
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let actual = hex_encode_lower(&hasher.finalize());
    if !actual.eq_ignore_ascii_case(&asset.sha256) {
        return Err(PrebuiltError::ChecksumMismatch {
            expected: asset.sha256.clone(),
            actual,
        });
    }

    std::fs::write(&tmp_archive, &bytes)?;
    let extract_res = extract_archive(&tmp_archive, plugin_dir, &asset.url);
    let _ = std::fs::remove_file(&tmp_archive);
    extract_res?;

    // Post-condition: the binary the manifest promised must now resolve.
    let resolved = verify_extension_command(manifest, plugin_dir)?
        .ok_or_else(|| {
            PrebuiltError::Verify(CommandVerifyError::Missing {
                path: ext.command.clone(),
                resolved: plugin_dir.join(&ext.command),
            })
        })?;
    Ok(resolved)
}

/// Shell out to `tar` or `unzip` to extract `archive` into `dest_dir`.
/// Suffix detection from the URL (which is more reliable than the
/// temp-file name we picked).
fn extract_archive(
    archive: &Path,
    dest_dir: &Path,
    url_for_suffix: &str,
) -> Result<(), PrebuiltError> {
    use std::process::Command;
    // Strip query / fragment for suffix detection.
    let url_clean = url_for_suffix
        .split(['?', '#'])
        .next()
        .unwrap_or(url_for_suffix)
        .to_ascii_lowercase();

    let (program, args): (&str, Vec<String>) = if url_clean.ends_with(".tar.gz")
        || url_clean.ends_with(".tgz")
    {
        (
            "tar",
            vec![
                "-xzf".into(),
                archive.to_string_lossy().into_owned(),
                "-C".into(),
                dest_dir.to_string_lossy().into_owned(),
            ],
        )
    } else if url_clean.ends_with(".tar.xz") {
        (
            "tar",
            vec![
                "-xJf".into(),
                archive.to_string_lossy().into_owned(),
                "-C".into(),
                dest_dir.to_string_lossy().into_owned(),
            ],
        )
    } else if url_clean.ends_with(".tar.bz2") {
        (
            "tar",
            vec![
                "-xjf".into(),
                archive.to_string_lossy().into_owned(),
                "-C".into(),
                dest_dir.to_string_lossy().into_owned(),
            ],
        )
    } else if url_clean.ends_with(".zip") {
        (
            "unzip",
            vec![
                "-q".into(),
                "-o".into(),
                archive.to_string_lossy().into_owned(),
                "-d".into(),
                dest_dir.to_string_lossy().into_owned(),
            ],
        )
    } else {
        return Err(PrebuiltError::UnsupportedArchive {
            url: url_for_suffix.to_string(),
        });
    };

    let out = Command::new(program).args(&args).output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            PrebuiltError::Extract(format!("'{program}' not found on PATH"))
        } else {
            PrebuiltError::Extract(format!("spawn {program}: {e}"))
        }
    })?;
    if !out.status.success() {
        return Err(PrebuiltError::Extract(format!(
            "{program} exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
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

    // ---- verify_extension_command tests (slice C) ----

    /// Helper: build a manifest whose extension declares `command`.
    fn manifest_with_extension_command(command: &str) -> PluginManifest {
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
                command: command.to_string(),
                setup: None,
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

    #[test]
    fn verify_returns_ok_none_when_no_extension() {
        let dir = tempfile::tempdir().unwrap();
        let m = manifest_with_setup(None); // sidecar-only manifest
        assert_eq!(verify_extension_command(&m, dir.path()).unwrap(), None);
    }

    #[test]
    fn verify_returns_ok_none_for_bare_command_name() {
        // Bare names defer to PATH lookup at spawn time, not our concern.
        let dir = tempfile::tempdir().unwrap();
        let m = manifest_with_extension_command("python3");
        assert_eq!(verify_extension_command(&m, dir.path()).unwrap(), None);
    }

    #[test]
    fn verify_succeeds_when_relative_binary_exists_and_is_executable() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin/ext");
        fs::create_dir_all(bin.parent().unwrap()).unwrap();
        fs::write(&bin, "#!/bin/sh\necho ok").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let m = manifest_with_extension_command("bin/ext");
        let resolved = verify_extension_command(&m, dir.path()).unwrap();
        assert!(resolved.is_some(), "should return resolved path");
    }

    #[test]
    fn verify_returns_missing_when_binary_absent() {
        let dir = tempfile::tempdir().unwrap();
        let m = manifest_with_extension_command("bin/ext");
        let err = verify_extension_command(&m, dir.path()).unwrap_err();
        assert!(matches!(err, CommandVerifyError::Missing { .. }), "got: {err:?}");
    }

    #[cfg(unix)]
    #[test]
    fn verify_returns_not_executable_when_bit_missing() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin/ext");
        fs::create_dir_all(bin.parent().unwrap()).unwrap();
        fs::write(&bin, "data").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o644)).unwrap();
        let m = manifest_with_extension_command("bin/ext");
        let err = verify_extension_command(&m, dir.path()).unwrap_err();
        assert!(matches!(err, CommandVerifyError::NotExecutable { .. }), "got: {err:?}");
    }

    #[test]
    fn verify_returns_not_a_file_when_path_is_directory() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin/ext");
        fs::create_dir_all(&bin).unwrap();
        let m = manifest_with_extension_command("bin/ext");
        let err = verify_extension_command(&m, dir.path()).unwrap_err();
        assert!(matches!(err, CommandVerifyError::NotAFile { .. }), "got: {err:?}");
    }

    #[test]
    fn verify_rejects_parent_dir_traversal_in_command() {
        let dir = tempfile::tempdir().unwrap();
        let m = manifest_with_extension_command("../escape/bin");
        let err = verify_extension_command(&m, dir.path()).unwrap_err();
        assert!(matches!(err, CommandVerifyError::EscapesPluginDir { .. }), "got: {err:?}");
    }

    #[cfg(unix)]
    #[test]
    fn verify_rejects_symlink_pointing_outside_plugin_dir() {
        let outer = tempfile::tempdir().unwrap();
        let plugin = tempfile::tempdir().unwrap();
        // Create a target binary outside the plugin dir.
        let target = outer.path().join("real-bin");
        fs::write(&target, "x").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        // Symlink inside the plugin dir to that outside binary.
        let link = plugin.path().join("bin/ext");
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let m = manifest_with_extension_command("bin/ext");
        let err = verify_extension_command(&m, plugin.path()).unwrap_err();
        assert!(
            matches!(err, CommandVerifyError::EscapesPluginDir { .. }),
            "got: {err:?}"
        );
    }

    // ---- try_install_from_prebuilt tests (slice E) ----

    /// All prebuilt tests mutate the `SYNAPS_ALLOW_FILE_PREBUILT` env
    /// var, so they must serialize on this lock to avoid stomping on
    /// each other when run in parallel.
    static PREBUILT_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Set `SYNAPS_ALLOW_FILE_PREBUILT=1` for this test scope so we can
    /// exercise the file:// path. Reset on drop. Holds
    /// `PREBUILT_ENV_LOCK` for the duration so parallel tests don't
    /// race on the env var.
    struct AllowFileGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
    }
    impl AllowFileGuard {
        fn enable() -> Self {
            let lock = PREBUILT_ENV_LOCK
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            std::env::set_var("SYNAPS_ALLOW_FILE_PREBUILT", "1");
            Self { _lock: lock }
        }
    }
    impl Drop for AllowFileGuard {
        fn drop(&mut self) {
            std::env::remove_var("SYNAPS_ALLOW_FILE_PREBUILT");
        }
    }

    fn manifest_with_prebuilt(
        command: &str,
        triple: &str,
        url: &str,
        sha256: &str,
    ) -> PluginManifest {
        let mut prebuilt = std::collections::HashMap::new();
        prebuilt.insert(
            triple.to_string(),
            crate::extensions::manifest::PrebuiltAsset {
                url: url.to_string(),
                sha256: sha256.to_string(),
            },
        );
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
                command: command.to_string(),
                setup: None,
                prebuilt,
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

    /// Helper: create a tar.gz archive containing one executable file at
    /// the given relative path inside the archive. Returns the archive
    /// path and its SHA-256.
    fn mk_tarball(staging: &Path, archive_name: &str, inner_path: &str) -> (PathBuf, String) {
        let work = staging.join("staging");
        fs::create_dir_all(&work).unwrap();
        let payload = work.join(inner_path);
        fs::create_dir_all(payload.parent().unwrap()).unwrap();
        fs::write(&payload, "#!/bin/sh\necho prebuilt-bin\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&payload, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let archive = staging.join(archive_name);
        let out = std::process::Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(&work)
            .arg(inner_path)
            .output()
            .expect("system tar must be present");
        assert!(out.status.success(), "tar failed: {:?}", out);
        let bytes = fs::read(&archive).unwrap();
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(&bytes);
        let sha = hex_encode_lower(&h.finalize());
        (archive, sha)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn prebuilt_returns_no_matching_asset_when_triple_missing() {
        let _lock = PREBUILT_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("SYNAPS_ALLOW_FILE_PREBUILT");
        let dir = tempfile::tempdir().unwrap();
        // Asset under a deliberately wrong host triple.
        let m = manifest_with_prebuilt("bin/ext", "fake-triple-9999", "https://x", "00");
        let err = try_install_from_prebuilt(&m, dir.path()).await.unwrap_err();
        assert!(matches!(err, PrebuiltError::NoMatchingAsset), "got: {err:?}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn prebuilt_rejects_non_https_url_in_production_builds() {
        // file:// is allow-listed only via env var; http:// is blocked
        // even when the env var is on.
        let _lock = PREBUILT_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SYNAPS_ALLOW_FILE_PREBUILT", "1");
        struct Cleanup;
        impl Drop for Cleanup { fn drop(&mut self) { std::env::remove_var("SYNAPS_ALLOW_FILE_PREBUILT"); } }
        let _c = Cleanup;
        let dir = tempfile::tempdir().unwrap();
        let triple = host_triple().expect("supported host");
        let m = manifest_with_prebuilt("bin/ext", triple, "http://example.com/x.tar.gz", "00");
        let err = try_install_from_prebuilt(&m, dir.path()).await.unwrap_err();
        assert!(matches!(err, PrebuiltError::UnsafeUrl { .. }), "got: {err:?}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn prebuilt_succeeds_with_valid_tarball_and_checksum() {
        let _allow = AllowFileGuard::enable();
        let staging = tempfile::tempdir().unwrap();
        let plugin = tempfile::tempdir().unwrap();
        let (archive, sha) = mk_tarball(staging.path(), "ext.tar.gz", "bin/ext");
        let url = format!("file://{}", archive.display());
        let triple = host_triple().expect("supported host");
        let m = manifest_with_prebuilt("bin/ext", triple, &url, &sha);
        let resolved = try_install_from_prebuilt(&m, plugin.path()).await.unwrap();
        assert!(resolved.exists(), "extracted binary should exist at {}", resolved.display());
        // Also confirm the temp download file was cleaned up.
        let leftover = plugin.path().join(format!(".prebuilt-{}-download", triple));
        assert!(!leftover.exists(), "temp archive should be removed");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn prebuilt_aborts_on_checksum_mismatch_without_extracting() {
        let _allow = AllowFileGuard::enable();
        let staging = tempfile::tempdir().unwrap();
        let plugin = tempfile::tempdir().unwrap();
        let (archive, _real_sha) = mk_tarball(staging.path(), "ext.tar.gz", "bin/ext");
        let url = format!("file://{}", archive.display());
        let triple = host_triple().expect("supported host");
        let bad_sha = "0".repeat(64);
        let m = manifest_with_prebuilt("bin/ext", triple, &url, &bad_sha);
        let err = try_install_from_prebuilt(&m, plugin.path()).await.unwrap_err();
        match err {
            PrebuiltError::ChecksumMismatch { expected, actual } => {
                assert_eq!(expected, bad_sha);
                assert_eq!(actual.len(), 64, "actual sha should be lowercase hex");
            }
            other => panic!("expected ChecksumMismatch, got {other:?}"),
        }
        // No artifact should have been written.
        assert!(!plugin.path().join("bin/ext").exists());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn prebuilt_rejects_unsupported_archive_suffix() {
        let _allow = AllowFileGuard::enable();
        let staging = tempfile::tempdir().unwrap();
        let plugin = tempfile::tempdir().unwrap();
        // Make a .rar-named file (we only support tar/zip variants).
        let archive = staging.path().join("ext.rar");
        fs::write(&archive, b"not really a rar").unwrap();
        let bytes = fs::read(&archive).unwrap();
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(&bytes);
        let sha = hex_encode_lower(&h.finalize());
        let url = format!("file://{}", archive.display());
        let triple = host_triple().expect("supported host");
        let m = manifest_with_prebuilt("bin/ext", triple, &url, &sha);
        let err = try_install_from_prebuilt(&m, plugin.path()).await.unwrap_err();
        assert!(matches!(err, PrebuiltError::UnsupportedArchive { .. }), "got: {err:?}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn prebuilt_fails_verify_when_archive_does_not_contain_declared_command() {
        let _allow = AllowFileGuard::enable();
        let staging = tempfile::tempdir().unwrap();
        let plugin = tempfile::tempdir().unwrap();
        // Archive ships at bin/wrong-name but manifest declares bin/ext.
        let (archive, sha) = mk_tarball(staging.path(), "ext.tar.gz", "bin/wrong-name");
        let url = format!("file://{}", archive.display());
        let triple = host_triple().expect("supported host");
        let m = manifest_with_prebuilt("bin/ext", triple, &url, &sha);
        let err = try_install_from_prebuilt(&m, plugin.path()).await.unwrap_err();
        assert!(matches!(err, PrebuiltError::Verify(_)), "got: {err:?}");
    }
}
