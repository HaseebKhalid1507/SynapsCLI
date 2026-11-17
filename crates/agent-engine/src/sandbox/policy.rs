//! The pure half of the write sandbox: what is protected, how a policy
//! provider's answer is validated, and which allow-list entries are kept.
//!
//! Nothing here starts a process or touches Landlock, so every rule can be
//! tested without a kernel.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use serde_json::Value;

/// At most this many `write` entries and this many `read` entries are taken
/// from a provider's answer; more is a bad answer.
pub const MAX_ENTRIES: usize = 256;
/// Longest path (bytes) accepted from a provider.
pub const MAX_PATH_BYTES: usize = 4096;
/// Longest refusal reason (characters) kept from a provider.
pub const MAX_REASON_CHARS: usize = 200;

/// Why a session is refused. [`Cause::word`] is the stable word shown by
/// `synaps sandbox check`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cause {
    AbiTooOld,
    NoPolicy,
    ProviderMissing,
    ProviderTimeout,
    ProviderError,
    ProviderBadAnswer,
    ProviderRefused,
    /// A protected path sits inside a write grant and the grant cannot be
    /// split around it (the folder can't be listed, or has too many entries).
    ProtectedInsideGrant,
}

impl Cause {
    pub fn word(self) -> &'static str {
        match self {
            Cause::AbiTooOld => "abi-too-old",
            Cause::NoPolicy => "no-policy",
            Cause::ProviderMissing => "provider-missing",
            Cause::ProviderTimeout => "provider-timeout",
            Cause::ProviderError => "provider-error",
            Cause::ProviderBadAnswer => "provider-bad-answer",
            Cause::ProviderRefused => "provider-refused",
            Cause::ProtectedInsideGrant => "protected-inside-grant",
        }
    }
}

/// A refusal: the cause plus the text a session start reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub cause: Cause,
    /// The cause a refused session carries: `refused: <reason>`, `error`,
    /// `timeout`, `bad-answer`, or the [`Cause::word`] for the others.
    pub session_cause: String,
}

impl Refusal {
    /// What `synaps sandbox check` prints as `refusal`: the cause word, or
    /// for a protected path inside a grant the full `protected-inside-grant:
    /// <path>` text.
    pub fn check_text(&self) -> String {
        match self.cause {
            Cause::ProtectedInsideGrant => self.session_cause.clone(),
            other => other.word().to_string(),
        }
    }
}

/// What the provider call came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderState {
    Off,
    Loaded,
    Missing,
    Timeout,
    Error,
    BadAnswer,
    Refused,
}

impl ProviderState {
    pub fn word(self) -> &'static str {
        match self {
            ProviderState::Off => "off",
            ProviderState::Loaded => "loaded",
            ProviderState::Missing => "missing",
            ProviderState::Timeout => "timeout",
            ProviderState::Error => "error",
            ProviderState::BadAnswer => "bad-answer",
            ProviderState::Refused => "refused",
        }
    }
}

/// A provider's answer, parsed but not yet screened against the protected set.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Answer {
    pub write: Vec<String>,
    pub read: Vec<String>,
}

/// The outcome of asking the provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderOutcome {
    /// No provider is configured.
    NotConfigured,
    /// The configured plugin is not loaded, or does not declare the
    /// `sandbox_policy` hook and the `sandbox.policy` permission.
    Missing,
    Timeout,
    /// The call failed (a JSON-RPC error, a dead process).
    Error,
    BadAnswer,
    Refused(String),
    Answer(Answer),
}

/// A control, bidirectional-formatting or zero-width character: never part
/// of a path or a reason the engine acts on or shows.
pub fn is_bad_char(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{061C}'
                | '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{2069}'
                | '\u{FEFF}'
        )
}

/// Strip control and bidirectional-formatting characters from a provider's
/// refusal reason and cap it at [`MAX_REASON_CHARS`].
pub fn sanitise_reason(raw: &str) -> String {
    let kept: String = raw.chars().filter(|c| !is_bad_char(*c)).take(MAX_REASON_CHARS).collect();
    let kept = kept.trim().to_string();
    if kept.is_empty() {
        "no reason given".to_string()
    } else {
        kept
    }
}

/// Parse a provider's `hook.handle` result: `{"write": [...], "read": [...]}`
/// or `{"refuse": "..."}`. Anything else is a bad answer.
pub fn parse_answer(value: &Value) -> ProviderOutcome {
    let Some(object) = value.as_object() else {
        return ProviderOutcome::BadAnswer;
    };
    // The answer is exactly `{"refuse": reason}` or `{"write": [...],
    // "read": [...]}` (either list may be left out): a key beyond those is
    // garbage, not something to guess about.
    if object.contains_key("refuse") && object.len() != 1 {
        return ProviderOutcome::BadAnswer;
    }
    if !object.contains_key("refuse") && object.keys().any(|k| k != "write" && k != "read") {
        return ProviderOutcome::BadAnswer;
    }
    if let Some(reason) = object.get("refuse") {
        return match reason.as_str() {
            Some(reason) => ProviderOutcome::Refused(sanitise_reason(reason)),
            None => ProviderOutcome::BadAnswer,
        };
    }
    if !object.contains_key("write") && !object.contains_key("read") {
        return ProviderOutcome::BadAnswer;
    }
    let list = |key: &str| -> Option<Vec<String>> {
        let Some(value) = object.get(key) else {
            return Some(Vec::new());
        };
        let items = value.as_array()?;
        if items.len() > MAX_ENTRIES {
            return None;
        }
        items
            .iter()
            .map(|item| {
                let s = item.as_str()?;
                // Absolute, no NUL, bounded: a relative entry has no
                // meaning for a policy, so the whole answer is garbage.
                (s.len() <= MAX_PATH_BYTES && !s.contains('\0') && Path::new(s).is_absolute())
                    .then(|| s.to_string())
            })
            .collect()
    };
    match (list("write"), list("read")) {
        (Some(write), Some(read)) => ProviderOutcome::Answer(Answer { write, read }),
        _ => ProviderOutcome::BadAnswer,
    }
}

// ── paths ────────────────────────────────────────────────────────────────────

/// `path` with `.` and `..` resolved lexically. `None` for a relative path.
pub fn lexical_normalize(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let mut out = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir | Component::Prefix(_) | Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(name) => out.push(name),
        }
    }
    Some(out)
}

/// The canonical form of `path` as far as it exists: the longest existing
/// ancestor is canonicalised (symlinks resolved) and the rest appended
/// lexically. A relative path comes back unchanged.
pub fn canon_lossy(path: &Path) -> PathBuf {
    let Some(norm) = lexical_normalize(path) else {
        return path.to_path_buf();
    };
    let mut existing = norm.clone();
    let mut tail: Vec<OsString> = Vec::new();
    loop {
        match std::fs::canonicalize(&existing) {
            Ok(real) => {
                let mut out = real;
                for part in tail.iter().rev() {
                    out.push(part);
                }
                return out;
            }
            Err(_) => match existing.file_name().map(|n| n.to_os_string()) {
                Some(name) => {
                    tail.push(name);
                    existing.pop();
                }
                None => return norm,
            },
        }
    }
}

/// How a candidate path relates to a protected one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relation {
    Equal,
    Under,
    Above,
}

fn relate(candidate: &Path, protected: &Path) -> Option<Relation> {
    if candidate == protected {
        Some(Relation::Equal)
    } else if candidate.starts_with(protected) {
        Some(Relation::Under)
    } else if protected.starts_with(candidate) {
        Some(Relation::Above)
    } else {
        None
    }
}

/// The two spellings of a path that matter: as written (normalised) and with
/// symlinks resolved.
fn forms(path: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(norm) = lexical_normalize(path) {
        out.push(norm);
    }
    let canon = canon_lossy(path);
    if canon.is_absolute() && !out.contains(&canon) {
        out.push(canon);
    }
    out
}

/// Where this process's home directory is, according to the password
/// database (not `$HOME`, which a process can set to anything).
#[cfg(unix)]
pub fn passwd_home() -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let mut buf = vec![0u8; 16 * 1024];
    // SAFETY: zeroed passwd is a valid out-parameter for getpwuid_r.
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: `pwd`, `buf` and `result` are valid for the call; the strings
    // it fills in point into `buf`, which outlives their use below.
    let rc = unsafe {
        libc::getpwuid_r(
            libc::geteuid(),
            &mut pwd,
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            &mut result,
        )
    };
    if rc != 0 || result.is_null() || pwd.pw_dir.is_null() {
        return None;
    }
    // SAFETY: pw_dir is a NUL-terminated string inside `buf`.
    let dir = unsafe { std::ffi::CStr::from_ptr(pwd.pw_dir) };
    let dir = PathBuf::from(std::ffi::OsString::from_vec(dir.to_bytes().to_vec()));
    dir.is_absolute().then_some(dir)
}

#[cfg(not(unix))]
pub fn passwd_home() -> Option<PathBuf> {
    None
}

/// The places the policy is computed from.
#[derive(Debug, Clone)]
pub struct PolicyEnv {
    /// The password-database home.
    pub home: Option<PathBuf>,
    /// `$HOME`, when it is set and differs from `home`.
    pub env_home: Option<PathBuf>,
    /// The engine base directory (`~/.synaps-cli` or `SYNAPS_BASE_DIR`).
    pub base_dir: PathBuf,
    /// Where session and daemon sockets live (`SYNAPS_RUNTIME_DIR` or
    /// `<base>/run`).
    pub runtime_dir: PathBuf,
    /// `$XDG_RUNTIME_DIR` (the sudo prompt broker's socket lives there).
    pub xdg_runtime_dir: Option<PathBuf>,
    /// Every absolute `$PATH` entry at session start: a program found there
    /// runs the next time any command names it.
    pub path_dirs: Vec<PathBuf>,
    /// The absolute values of `$XDG_CONFIG_HOME`, `$XDG_DATA_HOME` and
    /// `$XDG_STATE_HOME` when set.
    pub xdg_dirs: Vec<PathBuf>,
}

impl PolicyEnv {
    /// An environment with a fixed home and base dir and nothing else: the
    /// starting point for tests.
    pub fn fixed(home: &Path, base_dir: &Path) -> Self {
        PolicyEnv {
            home: Some(home.to_path_buf()),
            env_home: None,
            base_dir: base_dir.to_path_buf(),
            runtime_dir: base_dir.join("run"),
            xdg_runtime_dir: None,
            path_dirs: Vec::new(),
            xdg_dirs: Vec::new(),
        }
    }

    pub fn from_process() -> Self {
        let base_dir = agent_core::config::base_dir();
        let runtime_dir = std::env::var_os("SYNAPS_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| base_dir.join("run"));
        let home = passwd_home();
        let env_home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|h| h.is_absolute() && Some(h) != home.as_ref());
        let xdg_runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|d| d.is_absolute());
        let path_dirs = std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).filter(|d| d.is_absolute()).collect())
            .unwrap_or_default();
        let xdg_dirs = ["XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME"]
            .iter()
            .filter_map(|name| std::env::var_os(name).map(PathBuf::from))
            .filter(|d| d.is_absolute())
            .collect();
        PolicyEnv { home, env_home, base_dir, runtime_dir, xdg_runtime_dir, path_dirs, xdg_dirs }
    }
}

/// Credential stores under a home directory.
pub const HOME_CREDENTIALS: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".aws",
    ".config/gcloud",
    ".kube",
    ".docker",
    ".local/share/keyrings",
    ".password-store",
    ".netrc",
    ".git-credentials",
];

/// User autostart locations under a home directory.
pub const HOME_AUTOSTART: &[&str] = &[".config/systemd", ".local/share/systemd", ".config/autostart"];

/// Whole trees under a home directory that hold configuration, data and
/// toolchains the owner's own commands load code from.
pub const HOME_DATA: &[&str] = &[".config", ".local", ".cargo", ".rustup"];

/// Per-user tools and shell environments (loaded by the owner's shell or
/// editor). Names starting `.vscode` are added by [`Protected::new`].
pub const HOME_SHELL_ENV: &[&str] = &[
    ".bashrc.d",
    ".oh-my-zsh",
    ".vim",
    ".vscode",
    "go/bin",
    ".deno",
    ".bun",
    ".thunderbird",
    ".claude",
];

/// Shell start-up files under a home directory.
pub const HOME_RC_FILES: &[&str] = &[".bashrc", ".profile", ".bash_profile", ".zshrc", ".zprofile"];

/// Entries of the home directory whose names start with this are protected
/// too (`.vscode-server`, `.vscode-insiders`, …).
pub const HOME_PREFIX_PROTECTED: &str = ".vscode";

/// The fixed set of paths no write ever reaches, whatever a provider says.
#[derive(Debug, Clone)]
pub struct Protected {
    /// Whole trees: the engine base dir (config, profiles, plugins,
    /// `sandbox/`), the socket dirs, credential stores, autostart dirs.
    tree: Vec<(PathBuf, String)>,
    /// A home directory: protected against being granted whole (an entry
    /// that is, or contains, it), but not against entries beneath it.
    homes: Vec<(PathBuf, String)>,
}

impl Protected {
    pub fn new(env: &PolicyEnv) -> Self {
        let mut tree: Vec<(PathBuf, String)> = Vec::new();
        let mut homes: Vec<(PathBuf, String)> = Vec::new();
        let mut add_tree = |path: &Path, label: String| {
            for form in forms(path) {
                if !tree.iter().any(|(p, _)| p == &form) {
                    tree.push((form, label.clone()));
                }
            }
        };
        add_tree(&env.base_dir, "the engine base directory".to_string());
        add_tree(&env.runtime_dir, "the socket directory".to_string());
        if let Some(dir) = &env.xdg_runtime_dir {
            add_tree(dir, "the runtime directory".to_string());
        }
        for dir in &env.path_dirs {
            add_tree(dir, "a $PATH entry".to_string());
        }
        for dir in &env.xdg_dirs {
            add_tree(dir, "an XDG base directory".to_string());
        }
        for home in env.home.iter().chain(env.env_home.iter()) {
            let classes: [(&[&str], &str); 5] = [
                (HOME_CREDENTIALS, "credentials"),
                (HOME_AUTOSTART, "autostart"),
                (HOME_DATA, "user data"),
                (HOME_SHELL_ENV, "shell environment"),
                (HOME_RC_FILES, "shell start-up file"),
            ];
            for (names, class) in classes {
                for rel in names {
                    add_tree(&home.join(rel), format!("~/{rel} ({class})"));
                }
            }
            if let Ok(entries) = std::fs::read_dir(home) {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    if name.to_string_lossy().starts_with(HOME_PREFIX_PROTECTED) {
                        add_tree(&home.join(&name), format!("~/{} (shell environment)", name.to_string_lossy()));
                    }
                }
            }
            for form in forms(home) {
                if !homes.iter().any(|(p, _)| p == &form) {
                    homes.push((form, "the home directory".to_string()));
                }
            }
        }
        // Every ancestor of the engine base dir (its parent chain up to `/`):
        // an entry that is one of them contains the base dir.
        for form in forms(&env.base_dir) {
            for ancestor in form.ancestors().skip(1) {
                if !homes.iter().any(|(p, _)| p == ancestor) {
                    homes.push((ancestor.to_path_buf(), "a parent of the engine base directory".to_string()));
                }
            }
        }
        Protected { tree, homes }
    }

    /// Why `candidate` can't be a write root at all, or `None` when it can:
    /// it is equal to or under a protected tree, or equal to or above the
    /// home directory or a parent of the engine base dir (those hold too
    /// much to enumerate, so they are never granted, not even split). A root
    /// that merely *contains* a protected tree is kept: the grant is split
    /// around the protected path (see `Protected::inside`).
    pub fn drop_reason(&self, candidate: &Path) -> Option<String> {
        if !candidate.is_absolute() {
            return Some("not an absolute path".to_string());
        }
        for form in forms(candidate) {
            for (protected, label) in &self.tree {
                match relate(&form, protected) {
                    Some(Relation::Equal) => {
                        return Some(format!("is {label} ({})", protected.display()))
                    }
                    Some(Relation::Under) => {
                        return Some(format!("is inside {label} ({})", protected.display()))
                    }
                    Some(Relation::Above) | None => {}
                }
            }
            for (home, label) in &self.homes {
                match relate(&form, home) {
                    Some(Relation::Equal) => {
                        return Some(format!("is {label} ({})", home.display()))
                    }
                    Some(Relation::Above) => {
                        return Some(format!("contains {label} ({})", home.display()))
                    }
                    _ => {}
                }
            }
        }
        None
    }

    /// Whether a write to `path` (a file or directory entry) lands on a
    /// protected tree: equal to or under one of them. Used for in-process
    /// writes, where the target is a specific path rather than a root.
    pub fn covers(&self, path: &Path) -> bool {
        forms(path).iter().any(|form| {
            self.tree
                .iter()
                .any(|(protected, _)| matches!(relate(form, protected), Some(Relation::Equal | Relation::Under)))
        })
    }

    /// The protected trees, for display and tests.
    pub fn trees(&self) -> Vec<PathBuf> {
        self.tree.iter().map(|(p, _)| p.clone()).collect()
    }

    /// The protected trees that lie strictly inside `root`, each with the
    /// reason it is protected. A grant of `root` must be split around them.
    pub fn inside(&self, root: &Path) -> Vec<(PathBuf, String)> {
        let mut out: Vec<(PathBuf, String)> = Vec::new();
        for form in forms(root) {
            for (protected, label) in &self.tree {
                if protected != &form
                    && protected.starts_with(&form)
                    && !out.iter().any(|(p, _)| p == protected)
                {
                    out.push((protected.clone(), label.clone()));
                }
            }
        }
        out.sort();
        out
    }

    /// Why `path` is protected (equal to or under a protected tree), when it is.
    pub fn why_covered(&self, path: &Path) -> Option<String> {
        for form in forms(path) {
            for (protected, label) in &self.tree {
                if matches!(relate(&form, protected), Some(Relation::Equal | Relation::Under)) {
                    return Some(format!("{label} ({})", protected.display()));
                }
            }
        }
        None
    }
}

// ── the always-on baseline ───────────────────────────────────────────────────

/// A path every confined session may write: scratch space and the devices a
/// shell redirects to.
pub struct Baseline {
    pub path: &'static str,
    /// Files beneath it can be written, never created, removed or renamed:
    /// procfs, where `> /dev/stdout` resolves to `/proc/self/fd/1`.
    pub existing_only: bool,
}

/// Scratch dirs and the character devices a command opens for writing.
pub const BASELINE: &[Baseline] = &[
    Baseline { path: "/tmp", existing_only: false },
    Baseline { path: "/dev/shm", existing_only: false },
    Baseline { path: "/dev/null", existing_only: false },
    Baseline { path: "/dev/zero", existing_only: false },
    Baseline { path: "/dev/tty", existing_only: false },
    Baseline { path: "/dev/ptmx", existing_only: false },
    Baseline { path: "/dev/pts", existing_only: false },
    Baseline { path: PROC, existing_only: true },
];

/// procfs. Only the baseline grants it, and only write-existing: a provider
/// can't widen it.
pub const PROC: &str = "/proc";

/// The baseline paths that get write-existing rights only.
pub fn existing_only_paths() -> Vec<PathBuf> {
    BASELINE.iter().filter(|b| b.existing_only).map(|b| canon_lossy(Path::new(b.path))).collect()
}

// ── resolution ───────────────────────────────────────────────────────────────

/// An allow-list entry that was not kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dropped {
    pub path: PathBuf,
    pub reason: String,
}

/// Everything `synaps sandbox check` reports, and what a session start
/// decides on.
#[derive(Debug, Clone)]
pub struct Resolution {
    pub confine_on: bool,
    pub abi: Option<u32>,
    pub provider: Option<String>,
    pub provider_state: ProviderState,
    pub refusal: Option<Refusal>,
    /// The effective write roots (canonical): the workspace, the provider's
    /// kept entries and the baseline. Empty when the session is refused for
    /// want of an answer.
    pub write: Vec<PathBuf>,
    /// The provider's `read` list as accepted. v1 does not enforce it.
    pub read: Vec<PathBuf>,
    pub dropped: Vec<Dropped>,
}

/// The inputs of [`resolve`].
pub struct ResolveInput<'a> {
    pub confine_on: bool,
    pub provider: Option<&'a str>,
    /// The kernel's Landlock ABI, or why there is none.
    pub abi: Result<u32, ()>,
    pub outcome: ProviderOutcome,
    pub workspace: &'a Path,
    pub env: &'a PolicyEnv,
}

/// Decide a session's policy from the settings, the kernel and the
/// provider's answer. Pure: the baseline and the workspace are screened the
/// same way as the provider's entries.
pub fn resolve(input: ResolveInput<'_>) -> Resolution {
    let ResolveInput { confine_on, provider, abi, outcome, workspace, env } = input;
    let mut res = Resolution {
        confine_on,
        abi: abi.ok(),
        provider: provider.map(str::to_string),
        provider_state: ProviderState::Off,
        refusal: None,
        write: Vec::new(),
        read: Vec::new(),
        dropped: Vec::new(),
    };
    if !confine_on {
        return res;
    }
    let refuse = |cause: Cause, session_cause: String| Refusal { cause, session_cause };
    let mut refusal: Option<Refusal> = None;
    if abi.is_err() {
        refusal = Some(refuse(Cause::AbiTooOld, Cause::AbiTooOld.word().to_string()));
    }
    let mut answer: Option<Answer> = None;
    match outcome {
        ProviderOutcome::NotConfigured => {
            res.provider_state = ProviderState::Missing;
            refusal.get_or_insert_with(|| refuse(Cause::NoPolicy, Cause::NoPolicy.word().to_string()));
        }
        ProviderOutcome::Missing => {
            res.provider_state = ProviderState::Missing;
            refusal.get_or_insert_with(|| {
                refuse(Cause::ProviderMissing, Cause::ProviderMissing.word().to_string())
            });
        }
        ProviderOutcome::Timeout => {
            res.provider_state = ProviderState::Timeout;
            refusal.get_or_insert_with(|| refuse(Cause::ProviderTimeout, "timeout".to_string()));
        }
        ProviderOutcome::Error => {
            res.provider_state = ProviderState::Error;
            refusal.get_or_insert_with(|| refuse(Cause::ProviderError, "error".to_string()));
        }
        ProviderOutcome::BadAnswer => {
            res.provider_state = ProviderState::BadAnswer;
            refusal.get_or_insert_with(|| refuse(Cause::ProviderBadAnswer, "bad-answer".to_string()));
        }
        ProviderOutcome::Refused(reason) => {
            res.provider_state = ProviderState::Refused;
            refusal.get_or_insert_with(|| {
                refuse(Cause::ProviderRefused, format!("refused: {reason}"))
            });
        }
        ProviderOutcome::Answer(a) => {
            res.provider_state = ProviderState::Loaded;
            answer = Some(a);
        }
    }
    res.refusal = refusal;
    // A refused session has no policy: nothing is resolved (the write list
    // stays empty), whatever the provider said.
    let (Some(answer), None) = (answer, res.refusal.as_ref()) else {
        return res;
    };

    let protected = Protected::new(env);
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    let mut keep = |res: &mut Resolution, candidate: PathBuf, from_provider: bool| {
        let screened = if from_provider { screen_provider_entry(&candidate) } else { None };
        if let Some(reason) = screened.or_else(|| protected.drop_reason(&candidate)) {
            res.dropped.push(Dropped { path: candidate, reason });
            return;
        }
        // The canonical form was screened with the as-written one above; a
        // path that does not exist yet is kept (it may be created later).
        let canon = canon_lossy(&candidate);
        if seen.insert(canon.clone()) {
            res.write.push(canon);
        }
    };
    keep(&mut res, workspace.to_path_buf(), false);
    for entry in &answer.write {
        keep(&mut res, PathBuf::from(entry), true);
    }
    for b in BASELINE {
        keep(&mut res, PathBuf::from(b.path), false);
    }
    for entry in answer.read {
        let path = PathBuf::from(&entry);
        if entry.chars().any(is_bad_char) {
            res.dropped.push(Dropped { path, reason: REASON_BAD_CHARS.to_string() });
            continue;
        }
        match lexical_normalize(&path) {
            Some(norm) => res.read.push(norm),
            None => res.dropped.push(Dropped { path, reason: "not an absolute path".to_string() }),
        }
    }
    res
}

/// Why a provider's `write` entry was dropped for its characters.
pub const REASON_BAD_CHARS: &str = "bad characters";
/// Why a provider's `write` entry was dropped for naming a symlink (or a
/// path that is not its own canonical form).
pub const REASON_SYMLINK: &str = "a symlink";
/// Shared scratch directories: an entry must be at least this many
/// components below one.
pub const SCRATCH_DIRS: &[&str] = &["/tmp", "/dev/shm"];
pub const SCRATCH_MIN_DEPTH: usize = 2;

/// The checks that apply to a provider's own entries only (the workspace and
/// the baseline are the engine's): characters, a symlink or other
/// non-canonical spelling, and a shared scratch directory that is too broad.
pub fn screen_provider_entry(path: &Path) -> Option<String> {
    if path.to_string_lossy().chars().any(is_bad_char) || path.to_str().is_none() {
        return Some(REASON_BAD_CHARS.to_string());
    }
    if canon_lossy(path) != path {
        return Some(REASON_SYMLINK.to_string());
    }
    if path.starts_with(PROC) {
        return Some("procfs is granted by the baseline only, write-existing".to_string());
    }
    for scratch in SCRATCH_DIRS {
        if let Ok(rest) = path.strip_prefix(scratch) {
            if rest.components().count() < SCRATCH_MIN_DEPTH {
                return Some(format!(
                    "too broad: {scratch} is shared, so an entry must be at least {SCRATCH_MIN_DEPTH} levels below it"
                ));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn env(base: &str, home: &str) -> PolicyEnv {
        PolicyEnv::fixed(Path::new(home), Path::new(base))
    }

    #[test]
    fn answers_parse_strictly() {
        assert_eq!(
            parse_answer(&json!({"write": ["/a"], "read": ["/b"]})),
            ProviderOutcome::Answer(Answer { write: vec!["/a".into()], read: vec!["/b".into()] })
        );
        assert_eq!(
            parse_answer(&json!({"write": ["/a"]})),
            ProviderOutcome::Answer(Answer { write: vec!["/a".into()], read: vec![] })
        );
        for bad in [
            json!("nope"),
            json!([]),
            json!({}),
            json!({"write": "/a"}),
            json!({"write": [1]}),
            json!({"write": ["/a\u{0}b"]}),
            json!({"write": ["relative/dir"]}),
            json!({"write": [""]}),
            json!({"read": ["./x"]}),
            json!({"refuse": 5}),
            json!({"continue": true}),
            json!({"write": ["/a"], "extra": 1}),
            json!({"write": ["/a"], "refuse": "no"}),
            json!({"refuse": "no", "read": []}),
        ] {
            assert_eq!(parse_answer(&bad), ProviderOutcome::BadAnswer, "{bad}");
        }
        let many: Vec<String> = (0..=MAX_ENTRIES).map(|i| format!("/p{i}")).collect();
        assert_eq!(parse_answer(&json!({ "write": many })), ProviderOutcome::BadAnswer);
    }

    #[test]
    fn refusal_reasons_are_sanitised_and_capped() {
        let raw = format!("no\u{1b}[31m\u{202e}way\u{0}{}", "x".repeat(500));
        let ProviderOutcome::Refused(reason) = parse_answer(&json!({ "refuse": raw })) else {
            panic!("expected a refusal");
        };
        assert!(!reason.chars().any(|c| c.is_control() || c == '\u{202e}'));
        assert!(reason.starts_with("no[31mway"));
        assert_eq!(reason.chars().count(), MAX_REASON_CHARS);
        assert_eq!(sanitise_reason("\u{202e}\u{1b}"), "no reason given");
    }

    #[test]
    fn every_invisible_or_reordering_character_is_a_bad_character() {
        for c in [
            '\u{0}', '\u{1b}', '\u{7f}', '\u{85}', '\u{061C}', '\u{200B}', '\u{200C}', '\u{200D}', '\u{200E}', '\u{200F}',
            '\u{2028}', '\u{2029}', '\u{202A}', '\u{202B}', '\u{202C}', '\u{202D}', '\u{202E}', '\u{2060}', '\u{2066}',
            '\u{2067}', '\u{2068}', '\u{2069}', '\u{FEFF}',
        ] {
            assert!(is_bad_char(c), "{:?} must be a bad character", c);
            assert_eq!(
                screen_provider_entry(Path::new(&format!("/data/a{c}b"))).as_deref(),
                Some(REASON_BAD_CHARS),
                "{c:?} in a path"
            );
        }
        for c in ['a', '/', ' ', 'é', '日'] {
            assert!(!is_bad_char(c), "{c:?} is an ordinary character");
        }
    }

    #[test]
    fn every_parent_of_the_base_dir_is_dropped_even_outside_the_home() {
        let e = env("/srv/engine/.synaps-cli", "/home/u");
        let p = Protected::new(&e);
        for parent in ["/srv/engine", "/srv"] {
            let reason = p.drop_reason(Path::new(parent)).unwrap_or_else(|| panic!("{parent} must drop"));
            assert!(reason.contains("a parent of the engine base directory"), "{parent}: {reason}");
        }
        // The root contains the home as well, so that is the reason it gives.
        assert!(p.drop_reason(Path::new("/")).is_some(), "/ must drop");
        assert_eq!(p.drop_reason(Path::new("/srv/other")), None);
        assert_eq!(p.drop_reason(Path::new("/srv/engine/work")), None);
    }

    #[test]
    fn drops_entries_equal_under_or_above_a_protected_path() {
        let e = env("/home/u/.synaps-cli", "/home/u");
        let p = Protected::new(&e);
        for (path, needle) in [
            ("/home/u/.synaps-cli", "is the engine base"),
            ("/home/u/.synaps-cli/plugins/x", "inside the engine base"),
            ("/home/u/.synaps-cli/sandbox", "inside the engine base"),
            ("/home/u/.synaps-cli/run", "inside the engine base"),
            ("/home/u/.ssh", "~/.ssh"),
            ("/home/u/.ssh/keys", "inside ~/.ssh"),
            ("/home/u/.config", "is ~/.config"),
            ("/home/u", "is the home directory"),
            ("/home", "contains"),
            ("/", "contains"),
            ("/home/u/.netrc", "~/.netrc"),
            ("/home/u/.git-credentials", "~/.git-credentials"),
            ("/home/u/.config/autostart", "~/.config/autostart"),
            ("/home/u/.local/share/keyrings/x", "inside ~/.local/share/keyrings"),
            ("relative/dir", "not an absolute path"),
        ] {
            let reason = p.drop_reason(Path::new(path)).unwrap_or_else(|| panic!("{path} must drop"));
            assert!(reason.contains(needle), "{path}: {reason}");
        }
        for ok in ["/home/u/proj", "/home/u/.cache/x", "/work/x", "/tmp", "/home/other"] {
            assert_eq!(p.drop_reason(Path::new(ok)), None, "{ok} must stay");
        }
    }

    /// An environment with every protected class present, and the same paths
    /// judged by a set built from an environment with none of them.
    struct Classes {
        dir: tempfile::TempDir,
        home: PathBuf,
        protected: Protected,
        bare: Protected,
    }

    fn classes() -> Classes {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join(".vscode-server")).unwrap();
        let mut e = env(home.join(".synaps-cli").to_str().unwrap(), home.to_str().unwrap());
        e.runtime_dir = dir.path().join("sockets");
        e.xdg_runtime_dir = Some(dir.path().join("xdg-run"));
        e.path_dirs = vec![dir.path().join("pathdir/bin")];
        e.xdg_dirs = vec![dir.path().join("xdgconf"), dir.path().join("xdgdata"), dir.path().join("xdgstate")];
        let protected = Protected::new(&e);
        let other = dir.path().join("elsewhere");
        let bare = Protected::new(&env(other.join(".synaps-cli").to_str().unwrap(), other.to_str().unwrap()));
        Classes { dir, home, protected, bare }
    }

    /// One pin per protected class: a path in the class is protected and a
    /// provider entry on it is dropped. The undo control is the same path
    /// against a set built without the class: it is writable there, so the
    /// class is what holds it.
    macro_rules! class_pin {
        ($name:ident, $c:ident => $path:expr) => {
            #[test]
            fn $name() {
                let $c = classes();
                let path: PathBuf = $path;
                assert!($c.protected.covers(&path), "{} is not protected", path.display());
                assert!(
                    $c.protected.drop_reason(&path).is_some(),
                    "a provider entry on {} is not dropped",
                    path.display()
                );
                assert!(!$c.bare.covers(&path), "{} is covered without its class", path.display());
            }
        };
    }

    class_pin!(class_engine_base_dir, c => c.home.join(".synaps-cli/profiles/x"));
    class_pin!(class_engine_plugins, c => c.home.join(".synaps-cli/plugins/guard"));
    class_pin!(class_engine_sandbox_dir, c => c.home.join(".synaps-cli/sandbox/policy"));
    // The config file that holds the owner's settings (and so any guard
    // list), the same file under a profile, the plugin folder itself (a new
    // plugin folder is a new guard or the loss of one) and one plugin's folder.
    class_pin!(class_engine_config_file, c => c.home.join(".synaps-cli/config"));
    class_pin!(class_engine_profile_config, c => c.home.join(".synaps-cli/work/config"));
    class_pin!(class_engine_plugins_dir_itself, c => c.home.join(".synaps-cli/plugins"));
    class_pin!(class_engine_plugin_manifest, c => c.home.join(".synaps-cli/plugins/example-guard/.synaps-plugin/plugin.json"));
    class_pin!(class_socket_dir, c => c.dir.path().join("sockets/s.sock"));
    class_pin!(class_runtime_dir, c => c.dir.path().join("xdg-run/askpass"));
    class_pin!(class_credential_store, c => c.home.join(".gnupg/pubring.kbx"));
    class_pin!(class_credential_file, c => c.home.join(".netrc"));
    class_pin!(class_autostart, c => c.home.join(".config/systemd/user/x.service"));
    class_pin!(class_user_data_config, c => c.home.join(".config/anything"));
    class_pin!(class_user_data_local, c => c.home.join(".local/bin/tool"));
    class_pin!(class_user_data_cargo, c => c.home.join(".cargo/bin/cargo"));
    class_pin!(class_user_data_rustup, c => c.home.join(".rustup/toolchains"));
    class_pin!(class_path_entry, c => c.dir.path().join("pathdir/bin/tool"));
    class_pin!(class_xdg_config_home, c => c.dir.path().join("xdgconf/app/c"));
    class_pin!(class_xdg_data_home, c => c.dir.path().join("xdgdata/app/d"));
    class_pin!(class_xdg_state_home, c => c.dir.path().join("xdgstate/app/s"));
    class_pin!(class_shell_env_bashrc_d, c => c.home.join(".bashrc.d/x.sh"));
    class_pin!(class_shell_env_oh_my_zsh, c => c.home.join(".oh-my-zsh/plugins"));
    class_pin!(class_shell_env_vim, c => c.home.join(".vim/plugin/x.vim"));
    class_pin!(class_shell_env_vscode_prefix, c => c.home.join(".vscode-server/bin/x"));
    class_pin!(class_shell_env_go_bin, c => c.home.join("go/bin/tool"));
    class_pin!(class_shell_env_deno, c => c.home.join(".deno/bin/deno"));
    class_pin!(class_shell_env_bun, c => c.home.join(".bun/bin/bun"));
    class_pin!(class_shell_env_thunderbird, c => c.home.join(".thunderbird/profile"));
    class_pin!(class_shell_env_claude, c => c.home.join(".claude/settings.json"));
    class_pin!(class_rc_file_bashrc, c => c.home.join(".bashrc"));
    class_pin!(class_rc_file_profile, c => c.home.join(".profile"));
    class_pin!(class_rc_file_bash_profile, c => c.home.join(".bash_profile"));
    class_pin!(class_rc_file_zshrc, c => c.home.join(".zshrc"));
    class_pin!(class_rc_file_zprofile, c => c.home.join(".zprofile"));

    /// The credential stores and autostart folders, spelled out one by one
    /// (not read back from the constants), each protected whole and each
    /// writable in a set built without them.
    #[test]
    fn every_credential_store_and_autostart_folder_is_protected_by_name() {
        let c = classes();
        for rel in [
            ".ssh",
            ".gnupg",
            ".aws",
            ".config/gcloud",
            ".kube",
            ".docker",
            ".local/share/keyrings",
            ".password-store",
            ".netrc",
            ".git-credentials",
            ".config/systemd",
            ".local/share/systemd",
            ".config/autostart",
        ] {
            let top = c.home.join(rel);
            let inner = top.join("inner/file");
            for path in [&top, &inner] {
                assert!(c.protected.covers(path), "{} is not protected", path.display());
                assert!(c.protected.drop_reason(path).is_some(), "{} is not dropped", path.display());
            }
        }
        // `~/.config` and `~/.local` are protected whole, so a store beneath
        // them is covered by its parent as well; the stores directly under
        // the home directory have no such parent and show the name list holds.
        for rel in [".ssh", ".gnupg", ".aws", ".kube", ".docker", ".password-store", ".netrc", ".git-credentials"] {
            assert!(!c.bare.covers(&c.home.join(rel)), "{rel} is covered without its class");
        }
    }

    /// The fixed name lists, spelled out: a name added or dropped is a
    /// decision someone has to make on purpose (and the by-name pins above
    /// would otherwise be satisfied by a parent such as `~/.config`).
    #[test]
    fn the_protected_name_lists_are_exactly_the_documented_ones() {
        assert_eq!(
            HOME_CREDENTIALS,
            [
                ".ssh",
                ".gnupg",
                ".aws",
                ".config/gcloud",
                ".kube",
                ".docker",
                ".local/share/keyrings",
                ".password-store",
                ".netrc",
                ".git-credentials",
            ]
        );
        assert_eq!(HOME_AUTOSTART, [".config/systemd", ".local/share/systemd", ".config/autostart"]);
        assert_eq!(HOME_DATA, [".config", ".local", ".cargo", ".rustup"]);
        assert_eq!(
            HOME_SHELL_ENV,
            [".bashrc.d", ".oh-my-zsh", ".vim", ".vscode", "go/bin", ".deno", ".bun", ".thunderbird", ".claude"]
        );
        assert_eq!(HOME_RC_FILES, [".bashrc", ".profile", ".bash_profile", ".zshrc", ".zprofile"]);
        assert_eq!(HOME_PREFIX_PROTECTED, ".vscode");
    }

    #[test]
    fn paths_outside_every_class_stay_writable() {
        let c = classes();
        for ok in [c.home.join("proj"), c.home.join(".cache/x"), c.dir.path().join("pathdir/other")] {
            assert!(!c.protected.covers(&ok), "{} must not be protected", ok.display());
            assert_eq!(c.protected.drop_reason(&ok), None, "{} must not be dropped", ok.display());
        }
    }

    #[test]
    fn the_scratch_dirs_need_two_levels_and_the_rest_of_a_provider_entry_is_screened() {
        for (path, reason) in [
            ("/tmp", "too broad"),
            ("/tmp/x", "too broad"),
            ("/dev/shm", "too broad"),
            ("/dev/shm/x", "too broad"),
            ("/data/ok\u{200b}", REASON_BAD_CHARS),
            ("/data/\u{202e}ok", REASON_BAD_CHARS),
            ("/data/o\u{1b}k", REASON_BAD_CHARS),
        ] {
            let got = screen_provider_entry(Path::new(path)).unwrap_or_else(|| panic!("{path:?} must drop"));
            assert!(got.contains(reason), "{path:?}: {got}");
        }
        for ok in ["/tmp/x/y", "/dev/shm/a/b/c", "/data/out", "/srv"] {
            assert_eq!(screen_provider_entry(Path::new(ok)), None, "{ok}");
        }
    }

    #[test]
    fn a_provider_entry_that_is_a_symlink_or_not_canonical_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real/sub");
        std::fs::create_dir_all(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(dir.path().join("real"), &link).unwrap();
        assert_eq!(screen_provider_entry(&link), Some(REASON_SYMLINK.to_string()));
        assert_eq!(screen_provider_entry(&link.join("sub")), Some(REASON_SYMLINK.to_string()));
        assert_eq!(
            screen_provider_entry(&dir.path().join("real/../real/sub")),
            Some(REASON_SYMLINK.to_string())
        );
        assert_eq!(screen_provider_entry(&canon_lossy(&real)), None);
    }

    #[test]
    fn an_answer_with_a_swapped_link_reports_it_in_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = dir.path().join("elsewhere/inner");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let link = dir.path().join("grant/inner");
        std::fs::create_dir_all(dir.path().join("grant")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &link).unwrap();
        let e = env("/home/u/.synaps-cli", "/home/u");
        let r = resolve(ResolveInput {
            confine_on: true,
            provider: Some("example-guard"),
            abi: Ok(7),
            outcome: ProviderOutcome::Answer(Answer { write: vec![link.to_string_lossy().into_owned()], read: vec![] }),
            workspace: Path::new("/home/u/proj"),
            env: &e,
        });
        assert!(r.refusal.is_none());
        assert!(!r.write.contains(&link) && !r.write.contains(&elsewhere));
        let d = r.dropped.iter().find(|d| d.path == link).expect("the link is reported");
        assert_eq!(d.reason, REASON_SYMLINK);
    }

    #[test]
    fn an_env_home_that_differs_is_protected_too() {
        let mut e = env("/home/u/.synaps-cli", "/home/u");
        e.env_home = Some(PathBuf::from("/srv/fakehome"));
        let p = Protected::new(&e);
        assert!(p.drop_reason(Path::new("/srv/fakehome")).is_some());
        assert!(p.drop_reason(Path::new("/srv/fakehome/.aws/x")).is_some());
        assert!(p.drop_reason(Path::new("/srv/fakehome/proj")).is_none());
    }

    #[test]
    fn a_symlink_into_a_protected_tree_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let ssh = home.join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();
        std::fs::create_dir_all(home.join(".synaps-cli")).unwrap();
        let link = dir.path().join("looks-fine");
        std::os::unix::fs::symlink(&ssh, &link).unwrap();
        let e = env(home.join(".synaps-cli").to_str().unwrap(), home.to_str().unwrap());
        let p = Protected::new(&e);
        let reason = p.drop_reason(&link).expect("a link to ~/.ssh must drop");
        assert!(reason.contains("~/.ssh"), "{reason}");
        assert!(p.covers(&link.join("id_rsa")));
    }

    fn resolved(outcome: ProviderOutcome, abi: Result<u32, ()>) -> Resolution {
        let e = env("/home/u/.synaps-cli", "/home/u");
        resolve(ResolveInput {
            confine_on: true,
            provider: Some("example-guard"),
            abi,
            outcome,
            workspace: Path::new("/home/u/proj"),
            env: &e,
        })
    }

    #[test]
    fn off_resolves_to_nothing() {
        let e = env("/home/u/.synaps-cli", "/home/u");
        let r = resolve(ResolveInput {
            confine_on: false,
            provider: None,
            abi: Ok(7),
            outcome: ProviderOutcome::NotConfigured,
            workspace: Path::new("/home/u/proj"),
            env: &e,
        });
        assert!(!r.confine_on);
        assert_eq!(r.provider_state, ProviderState::Off);
        assert!(r.refusal.is_none() && r.write.is_empty());
    }

    #[test]
    fn every_failure_refuses_with_its_cause() {
        let cases = [
            (ProviderOutcome::NotConfigured, Cause::NoPolicy, "no-policy"),
            (ProviderOutcome::Missing, Cause::ProviderMissing, "provider-missing"),
            (ProviderOutcome::Timeout, Cause::ProviderTimeout, "timeout"),
            (ProviderOutcome::Error, Cause::ProviderError, "error"),
            (ProviderOutcome::BadAnswer, Cause::ProviderBadAnswer, "bad-answer"),
            (ProviderOutcome::Refused("nope".into()), Cause::ProviderRefused, "refused: nope"),
        ];
        for (outcome, cause, session_cause) in cases {
            let r = resolved(outcome, Ok(7));
            let refusal = r.refusal.expect("must refuse");
            assert_eq!(refusal.cause, cause);
            assert_eq!(refusal.session_cause, session_cause);
            assert!(r.write.is_empty());
        }
    }

    #[test]
    fn a_missing_abi_refuses_even_with_a_good_answer() {
        let r = resolved(ProviderOutcome::Answer(Answer::default()), Err(()));
        assert_eq!(r.refusal.unwrap().cause, Cause::AbiTooOld);
        assert_eq!(r.abi, None);
        assert_eq!(r.provider_state, ProviderState::Loaded);
        assert!(r.write.is_empty() && r.read.is_empty() && r.dropped.is_empty());
    }

    #[test]
    fn an_answer_keeps_good_entries_and_reports_each_drop() {
        let answer = Answer {
            write: vec!["/home/u".into(), "/data/out".into(), "/home/u/.ssh".into(), "rel".into()],
            read: vec!["/etc".into(), "nope".into()],
        };
        let r = resolved(ProviderOutcome::Answer(answer), Ok(7));
        assert!(r.refusal.is_none());
        assert_eq!(r.provider_state, ProviderState::Loaded);
        assert!(r.write.contains(&PathBuf::from("/home/u/proj")));
        assert!(r.write.contains(&PathBuf::from("/data/out")));
        assert!(r.write.contains(&PathBuf::from("/tmp")));
        let dropped: Vec<_> = r.dropped.iter().map(|d| d.path.to_string_lossy().into_owned()).collect();
        for gone in ["/home/u", "/home/u/.ssh", "rel", "nope"] {
            assert!(dropped.contains(&gone.to_string()), "{gone} reported: {dropped:?}");
            assert!(!r.write.contains(&PathBuf::from(gone)));
        }
        assert_eq!(r.read, vec![PathBuf::from("/etc")]);
        assert!(r.dropped.iter().all(|d| !d.reason.is_empty()));
    }
}
