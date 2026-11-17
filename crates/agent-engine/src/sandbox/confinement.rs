//! A resolved, enforceable write policy for one session: the write roots,
//! the protected set, and the paths inside a root that stay read-only.
//!
//! Landlock only *allows*: a rule grants everything beneath a directory and
//! can't carve a hole out of it. So a root that holds a protected path (a
//! `$PATH` entry, the repository's `.git/hooks`, the project's
//! `.synaps/plugins`, …) is never granted whole. It is *split*: each entry of
//! the root is granted by itself, recursing only along the way to each
//! protected inner path, which is left out. The cost is stated plainly: a
//! directory on a split path can't get new entries created directly in it by
//! a shell, because creating an entry needs a grant on the directory itself.
//! Where a root can't be split (a directory that can't be listed, or has too
//! many entries) the session is refused: `protected-inside-grant: <path>`.
//!
//! A git checkout is the common case. In a main checkout the root is split
//! around `.git`, and only `.git/objects`, `.git/refs` and `.git/logs` are
//! granted inside it, so `hooks/` and `config` can't be planted. A linked
//! worktree's `.git` file stays read-only; the common dir it points at gets
//! the same three grants plus the worktree's own admin directory.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::policy::{canon_lossy, existing_only_paths, Dropped, Protected, Refusal, Resolution, REASON_SYMLINK};

/// The subdirectories of a git common dir a confined session may write.
/// Everything else in it (`hooks/`, `config`, `info/`, `packed-refs`,
/// `index`, `HEAD`, `worktrees/`, …) stays read-only.
pub const GIT_COMMON_DIRS: &[&str] = &["objects", "refs", "logs"];

/// A directory with more entries than this is not split: the session is
/// refused rather than building an unbounded ruleset.
pub const MAX_SPLIT_ENTRIES: usize = 2048;

/// A project's own plugin folder, relative to a workspace: the engine loads
/// plugins from it, so a plugin written there would run on the next start.
pub const PROJECT_PLUGINS: [&str; 2] = [".synaps", "plugins"];

/// Which kind of checkout a root's `.git` belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitKind {
    /// `.git` is a directory: the root is a main checkout.
    Main,
    /// `.git` is a file pointing at `<common>/worktrees/<name>`.
    Linked,
    /// A `.git` that is neither, or whose layout doesn't check out. Nothing
    /// of it is granted.
    Unusable,
}

/// The git layout of a write root that has a `.git` entry.
#[derive(Debug, Clone)]
pub struct GitLayout {
    pub kind: GitKind,
    /// `<root>/.git`: a directory or a file.
    pub dot_git: PathBuf,
    /// The common dir (`<root>/.git` for a main checkout).
    pub common: Option<PathBuf>,
    /// Directories granted writable (all rights beneath).
    pub grants: Vec<PathBuf>,
}

fn is_real_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).map(|m| m.is_dir()).unwrap_or(false)
}

fn is_real_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).map(|m| m.is_file()).unwrap_or(false)
}

/// Whether `dir` looks like a git common dir.
fn is_common_dir(dir: &Path) -> bool {
    is_real_dir(&dir.join("objects")) && is_real_dir(&dir.join("refs")) && is_real_file(&dir.join("HEAD"))
}

fn read_small(path: &Path) -> Option<String> {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    let mut text = String::new();
    file.take(4096).read_to_string(&mut text).ok()?;
    Some(text)
}

fn real_dir_list(common: &Path) -> Vec<PathBuf> {
    GIT_COMMON_DIRS.iter().map(|d| common.join(d)).filter(|d| is_real_dir(d)).collect()
}

/// The git layout of `root`, `None` when it has no `.git` entry.
pub fn detect_git(root: &Path) -> Option<GitLayout> {
    let dot_git = root.join(".git");
    let meta = std::fs::symlink_metadata(&dot_git).ok()?;
    let unusable = |common: Option<PathBuf>| GitLayout {
        kind: GitKind::Unusable,
        dot_git: dot_git.clone(),
        common,
        grants: Vec::new(),
    };
    if meta.is_dir() {
        if !is_common_dir(&dot_git) {
            return Some(unusable(None));
        }
        return Some(GitLayout {
            kind: GitKind::Main,
            dot_git: dot_git.clone(),
            common: Some(dot_git.clone()),
            grants: real_dir_list(&dot_git),
        });
    }
    if !meta.is_file() {
        return Some(unusable(None));
    }
    // A linked worktree: `.git` holds `gitdir: <admin dir>`.
    let Some(text) = read_small(&dot_git) else { return Some(unusable(None)) };
    let Some(line) = text.lines().next().and_then(|l| l.strip_prefix("gitdir:")) else {
        return Some(unusable(None));
    };
    let admin = root.join(line.trim());
    let Ok(admin) = std::fs::canonicalize(admin) else { return Some(unusable(None)) };
    let common = match read_small(&admin.join("commondir")) {
        Some(text) => admin.join(text.trim()),
        None => match admin.parent().and_then(Path::parent) {
            Some(p) => p.to_path_buf(),
            None => return Some(unusable(None)),
        },
    };
    let Ok(common) = std::fs::canonicalize(common) else { return Some(unusable(None)) };
    // The admin dir must be `<common>/worktrees/<name>`, the common dir
    // must look like one, and the admin dir must point back at this `.git`.
    let well_formed = admin.parent() == Some(common.join("worktrees").as_path())
        && is_common_dir(&common)
        && read_small(&admin.join("gitdir"))
            .and_then(|back| std::fs::canonicalize(back.trim()).ok())
            == std::fs::canonicalize(&dot_git).ok();
    if !well_formed {
        return Some(unusable(Some(common)));
    }
    let mut grants = real_dir_list(&common);
    grants.push(admin);
    Some(GitLayout { kind: GitKind::Linked, dot_git, grants, common: Some(common) })
}

impl GitLayout {
    /// The paths of this layout that a grant of a containing directory must
    /// leave out, each with the reason.
    fn holes(&self) -> Vec<Hole> {
        let mut out = Vec::new();
        if let Some(common) = &self.common {
            out.push(Hole::new(common.join("hooks"), "git hooks: a planted hook runs on the owner's next git command"));
            out.push(Hole::new(common.join("config"), "git config: it can name a command git then runs"));
        }
        if self.kind != GitKind::Main {
            out.push(Hole::new(self.dot_git.clone(), "git metadata: the .git entry can't be repointed"));
        }
        out
    }
}

/// A path inside a write root that stays read-only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hole {
    pub path: PathBuf,
    pub why: String,
}

impl Hole {
    fn new(path: PathBuf, why: &str) -> Self {
        Hole { path, why: why.to_string() }
    }
}

/// One path to grant to Landlock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantPath {
    pub path: PathBuf,
    /// A root or a layout's directory, not an entry of a split root: when it
    /// can't be granted (a symlink, a protected path) that is reported.
    pub explicit: bool,
    /// Write-existing rights only (procfs): files can be written, never
    /// created, removed or renamed.
    pub existing_only: bool,
}

/// What a session's roots turn into right now.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub grants: Vec<GrantPath>,
    /// Inner paths left out of a grant (protected, so read-only).
    pub left_out: Vec<Hole>,
    /// Directories on a split path: nothing can be created directly in them.
    pub no_new_entries: Vec<PathBuf>,
}

/// A root can't be split around a protected path inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitImpossible {
    pub path: PathBuf,
}

impl SplitImpossible {
    /// The cause a refused session carries.
    pub fn cause(&self) -> String {
        format!("protected-inside-grant: {}", self.path.display())
    }

    pub fn refusal(&self) -> Refusal {
        Refusal { cause: super::policy::Cause::ProtectedInsideGrant, session_cause: self.cause() }
    }
}

/// A session's resolved confinement.
#[derive(Debug)]
pub struct Confinement {
    pub resolution: Resolution,
    roots: Vec<PathBuf>,
    protected: Protected,
    /// Folders whose `.synaps/plugins` is protected: the workspace, the
    /// session's working folder and the process's.
    project_dirs: Vec<PathBuf>,
    /// Roots that get write-existing rights only, whoever listed them.
    existing_only: Vec<PathBuf>,
    /// Grants dropped while the last ruleset was built.
    late_dropped: Mutex<Vec<Dropped>>,
}

impl Clone for Confinement {
    fn clone(&self) -> Self {
        Confinement {
            resolution: self.resolution.clone(),
            roots: self.roots.clone(),
            protected: self.protected.clone(),
            project_dirs: self.project_dirs.clone(),
            existing_only: self.existing_only.clone(),
            late_dropped: Mutex::new(self.late_dropped.lock().map(|d| d.clone()).unwrap_or_default()),
        }
    }
}

impl Confinement {
    pub fn new(resolution: Resolution, protected: Protected, workspace: &Path, cwd: Option<&Path>) -> Self {
        let roots = resolution.write.clone();
        let mut project_dirs: Vec<PathBuf> = vec![workspace.to_path_buf()];
        project_dirs.extend(cwd.map(canon_lossy));
        project_dirs.extend(std::env::current_dir().ok().map(|d| canon_lossy(&d)));
        project_dirs.sort();
        project_dirs.dedup();
        Confinement {
            resolution,
            roots,
            protected,
            project_dirs,
            existing_only: existing_only_paths(),
            late_dropped: Mutex::new(Vec::new()),
        }
    }

    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    pub fn protected(&self) -> &Protected {
        &self.protected
    }

    /// Grants dropped by the most recent ruleset build (a symlink swapped in
    /// after the policy was resolved, a path that turned protected).
    pub fn late_dropped(&self) -> Vec<Dropped> {
        self.late_dropped.lock().map(|d| d.clone()).unwrap_or_default()
    }

    fn layouts(&self) -> Vec<GitLayout> {
        let mut out: Vec<GitLayout> = Vec::new();
        for root in &self.roots {
            if let Some(layout) = detect_git(root) {
                if !out.iter().any(|l| l.dot_git == layout.dot_git) {
                    out.push(layout);
                }
            }
        }
        out
    }

    fn project_holes(&self) -> Vec<Hole> {
        self.project_dirs
            .iter()
            .map(|dir| {
                Hole::new(
                    dir.join(PROJECT_PLUGINS[0]).join(PROJECT_PLUGINS[1]),
                    "the project's plugin folder: a plugin written there loads on the next run",
                )
            })
            .collect()
    }

    /// Every path that must stay read-only, wherever it is: the protected
    /// trees, the git metadata of the roots' checkouts, the project plugin
    /// folders.
    fn all_holes(&self, layouts: &[GitLayout]) -> Vec<Hole> {
        let mut out: Vec<Hole> = Vec::new();
        let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
        let mut push = |h: Hole| {
            if seen.insert(h.path.clone()) {
                out.push(h);
            }
        };
        for root in &self.roots {
            for (path, why) in self.protected.inside(root) {
                push(Hole { path, why });
            }
        }
        for layout in layouts {
            for h in layout.holes() {
                push(h);
            }
        }
        for h in self.project_holes() {
            push(h);
        }
        out
    }

    /// Work out what to grant to Landlock: whole roots where nothing
    /// protected lies inside, split roots otherwise. Lists directories afresh
    /// each time, so entries made since the last spawn are covered.
    pub fn plan(&self) -> Result<Plan, SplitImpossible> {
        let layouts = self.layouts();
        let holes = self.all_holes(&layouts);
        let stop: BTreeSet<PathBuf> = layouts.iter().filter_map(|l| l.common.clone()).collect();
        let mut plan = Plan::default();
        for root in &self.roots {
            let inner: Vec<&Hole> =
                holes.iter().filter(|h| h.path != *root && h.path.starts_with(root)).collect();
            if inner.is_empty() {
                let existing_only = self.existing_only.contains(root);
                plan.grants.push(GrantPath { path: root.clone(), explicit: true, existing_only });
            } else {
                for h in &inner {
                    if !plan.left_out.iter().any(|l| l.path == h.path) {
                        plan.left_out.push((*h).clone());
                    }
                }
                split(root, &inner, &stop, &mut plan)?;
            }
        }
        // A layout's own directories (a linked worktree's common dir may lie
        // outside every root).
        for layout in &layouts {
            for dir in &layout.grants {
                plan.grants.push(GrantPath { path: dir.clone(), explicit: true, existing_only: false });
            }
        }
        plan.left_out.sort_by(|a, b| a.path.cmp(&b.path));
        plan.no_new_entries.sort();
        plan.no_new_entries.dedup();
        Ok(plan)
    }

    /// Whether an in-process write to `path` (an absolute, canonical-parent
    /// path) is allowed. The protected set always wins; git metadata and the
    /// project plugin folder are read-only; then the path must lie in a root
    /// or in one of a checkout's writable git directories.
    pub fn allows_write(&self, path: &Path) -> Result<(), String> {
        if let Some(why) = self.protected.why_covered(path) {
            return Err(format!("sandbox: write refused: {} is protected ({why})", path.display()));
        }
        let layouts = self.layouts();
        for layout in &layouts {
            if let Some(common) = &layout.common {
                if path.starts_with(common) && !layout.grants.iter().any(|dir| path.starts_with(dir)) {
                    return Err(format!(
                        "sandbox: write refused: {} is git metadata, read-only while confined (hooks and config can run code)",
                        path.display()
                    ));
                }
            }
            if layout.kind != GitKind::Main && path.starts_with(&layout.dot_git) {
                return Err(format!(
                    "sandbox: write refused: {} is git metadata, read-only while confined",
                    path.display()
                ));
            }
        }
        for hole in self.project_holes() {
            if path.starts_with(&hole.path) {
                return Err(format!("sandbox: write refused: {} is {}", path.display(), hole.why));
            }
        }
        // Write-existing roots (procfs) are not a place the tools write to.
        let granted = self.roots.iter().any(|root| path.starts_with(root) && !self.existing_only.contains(root))
            || layouts.iter().any(|l| l.grants.iter().any(|dir| path.starts_with(dir)));
        if granted {
            return Ok(());
        }
        Err(format!(
            "sandbox: write refused: {} is outside the directories this session may write",
            path.display()
        ))
    }
}

/// Grant `dir` entry by entry, recursing only where a hole lies beneath.
fn split(dir: &Path, holes: &[&Hole], stop: &BTreeSet<PathBuf>, plan: &mut Plan) -> Result<(), SplitImpossible> {
    plan.no_new_entries.push(dir.to_path_buf());
    // A git common dir is granted by the layout (objects, refs, logs), not
    // by listing it.
    if stop.contains(dir) {
        return Ok(());
    }
    let first = || SplitImpossible { path: holes.first().map(|h| h.path.clone()).unwrap_or_else(|| dir.to_path_buf()) };
    let entries = std::fs::read_dir(dir).map_err(|_| first())?;
    let mut names: Vec<OsString> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|_| first())?;
        names.push(entry.file_name());
        if names.len() > MAX_SPLIT_ENTRIES {
            return Err(first());
        }
    }
    names.sort();
    for name in names {
        let child = dir.join(&name);
        if holes.iter().any(|h| h.path == child) {
            continue;
        }
        let inner: Vec<&Hole> = holes.iter().copied().filter(|h| h.path.starts_with(&child)).collect();
        if inner.is_empty() {
            plan.grants.push(GrantPath { path: child, explicit: false, existing_only: false });
            continue;
        }
        // A hole lies beneath this entry: only a real directory can be
        // split; anything else on the way to a protected path is left out.
        if is_real_dir(&child) {
            split(&child, &inner, stop, plan)?;
        }
    }
    Ok(())
}

// ── opening the grants ───────────────────────────────────────────────────────

/// Why a grant path could not be opened as itself.
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub enum OpenFailure {
    /// The path is gone: skipped.
    Missing,
    /// A component is (now) a symlink, or the opened object is not where the
    /// path says.
    Swapped,
    Io(std::io::Error),
}

/// Open `path` for a Landlock rule without following a symlink in any
/// component: `O_PATH | O_NOFOLLOW` one component at a time from `/`, then
/// check that the kernel's own name for the opened object is `path`.
#[cfg(target_os = "linux")]
pub fn open_no_symlinks(path: &Path) -> Result<std::os::fd::OwnedFd, OpenFailure> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;

    if !path.is_absolute() {
        return Err(OpenFailure::Swapped);
    }
    let open_at = |dir: libc::c_int, name: &std::ffi::CStr, flags: libc::c_int| -> Result<OwnedFd, OpenFailure> {
        // SAFETY: `dir` is an open fd (or AT_FDCWD) and `name` a C string.
        let fd = unsafe { libc::openat(dir, name.as_ptr(), flags | libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
        if fd < 0 {
            let e = std::io::Error::last_os_error();
            return Err(match e.raw_os_error() {
                Some(libc::ENOENT) => OpenFailure::Missing,
                Some(libc::ELOOP) | Some(libc::ENOTDIR) => OpenFailure::Swapped,
                _ => OpenFailure::Io(e),
            });
        }
        // SAFETY: openat returned a new fd that we own.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    };
    let kind_of = |fd: &OwnedFd| -> Result<libc::mode_t, OpenFailure> {
        // SAFETY: a zeroed stat is a valid out-parameter; the fd is open.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::fstat(fd.as_raw_fd(), &mut st) };
        if rc != 0 {
            return Err(OpenFailure::Io(std::io::Error::last_os_error()));
        }
        Ok(st.st_mode & libc::S_IFMT)
    };

    let root = std::ffi::CString::new("/").map_err(|_| OpenFailure::Swapped)?;
    let mut cur = open_at(libc::AT_FDCWD, &root, libc::O_DIRECTORY)?;
    let mut names: Vec<&std::ffi::OsStr> = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::RootDir => {}
            std::path::Component::Normal(name) => names.push(name),
            // `.`, `..` and prefixes have no place in a canonical path.
            _ => return Err(OpenFailure::Swapped),
        }
    }
    let last = names.len();
    for (i, name) in names.iter().enumerate() {
        let c = std::ffi::CString::new(name.as_bytes()).map_err(|_| OpenFailure::Swapped)?;
        let next = open_at(cur.as_raw_fd(), &c, 0)?;
        match kind_of(&next)? {
            libc::S_IFLNK => return Err(OpenFailure::Swapped),
            libc::S_IFDIR => {}
            _ if i + 1 < last => return Err(OpenFailure::Swapped),
            _ => {}
        }
        cur = next;
    }
    // The kernel's name for what we hold must be the path we meant.
    let seen = std::fs::read_link(format!("/proc/self/fd/{}", cur.as_raw_fd()))
        .map_err(OpenFailure::Io)?;
    if seen != path {
        return Err(OpenFailure::Swapped);
    }
    Ok(cur)
}

/// One grant, opened: the object the rule will name.
#[cfg(target_os = "linux")]
pub struct OpenedGrant {
    pub path: PathBuf,
    pub fd: std::os::fd::OwnedFd,
    pub existing_only: bool,
}

/// The grants of a plan, opened and re-checked. A grant that is gone is
/// skipped; one that was swapped for a symlink or turned protected is dropped
/// and reported.
#[cfg(target_os = "linux")]
pub struct OpenedGrants {
    pub grants: Vec<OpenedGrant>,
    pub dropped: Vec<Dropped>,
}

#[cfg(target_os = "linux")]
impl Confinement {
    /// Open every grant of `plan` without following symlinks. `between` runs
    /// after the plan was made and before the first open: a test uses it to
    /// swap a component for a link.
    pub fn open_grants(&self, plan: &Plan, between: &mut dyn FnMut()) -> Result<OpenedGrants, String> {
        between();
        let mut out = OpenedGrants { grants: Vec::new(), dropped: Vec::new() };
        for grant in &plan.grants {
            let fd = match open_no_symlinks(&grant.path) {
                Ok(fd) => fd,
                Err(OpenFailure::Missing) => continue,
                Err(OpenFailure::Swapped) => {
                    // An entry of a split root that is a symlink is simply
                    // not granted (a link is not a place to write); a root
                    // or a layout directory that is one is reported.
                    if grant.explicit {
                        out.dropped.push(Dropped { path: grant.path.clone(), reason: REASON_SYMLINK.to_string() });
                    }
                    continue;
                }
                Err(OpenFailure::Io(e)) => {
                    return Err(format!("sandbox: cannot open {}: {e}", grant.path.display()));
                }
            };
            if let Some(why) = self.protected.drop_reason(&grant.path) {
                out.dropped.push(Dropped { path: grant.path.clone(), reason: why });
                continue;
            }
            out.grants.push(OpenedGrant { path: grant.path.clone(), fd, existing_only: grant.existing_only });
        }
        if let Ok(mut late) = self.late_dropped.lock() {
            *late = out.dropped.clone();
        }
        for d in &out.dropped {
            tracing::warn!(path = %d.path.display(), reason = %d.reason, "sandbox: grant dropped");
        }
        Ok(out)
    }

    /// A Landlock ruleset for one spawn, with the plan it was built from and
    /// what was dropped on the way. A root that can't be split, or a rule the
    /// kernel rejects, fails the build: the spawn is refused, never run with
    /// less than the policy.
    pub fn build_ruleset_with(
        &self,
        between: &mut dyn FnMut(),
    ) -> Result<(super::landlock::Ruleset, Plan, Vec<Dropped>), String> {
        use std::os::fd::AsFd;
        let plan = self.plan().map_err(|e| format!("sandbox: {}", e.cause()))?;
        let opened = self.open_grants(&plan, between)?;
        let mut ruleset = super::landlock::Ruleset::new().map_err(|e| format!("sandbox: {e}"))?;
        for grant in &opened.grants {
            let rights = if grant.existing_only {
                super::landlock::FILE_WRITE_RIGHTS
            } else {
                super::landlock::WRITE_RIGHTS
            };
            ruleset
                .allow_fd(grant.fd.as_fd(), rights)
                .map_err(|e| format!("sandbox: cannot grant {}: {e}", grant.path.display()))?;
        }
        Ok((ruleset, plan, opened.dropped))
    }

    pub fn build_ruleset(&self) -> Result<super::landlock::Ruleset, String> {
        self.build_ruleset_with(&mut || {}).map(|(ruleset, _, _)| ruleset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::policy::{resolve, Answer, PolicyEnv, ProviderOutcome, ResolveInput};

    /// A scratch tree with a home, a base dir and a workspace, and a
    /// resolved confinement around the workspace.
    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
        home: PathBuf,
        ws: PathBuf,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = canon_lossy(dir.path());
        let home = root.join("home");
        let ws = home.join("proj");
        std::fs::create_dir_all(home.join(".synaps-cli")).unwrap();
        std::fs::create_dir_all(&ws).unwrap();
        Fixture { _dir: dir, root, home, ws }
    }

    fn confine(f: &Fixture, path_dirs: Vec<PathBuf>, provider_write: Vec<String>) -> Confinement {
        let mut env = PolicyEnv::fixed(&f.home, &f.home.join(".synaps-cli"));
        env.path_dirs = path_dirs;
        let res = resolve(ResolveInput {
            confine_on: true,
            provider: Some("example-guard"),
            abi: Ok(7),
            outcome: ProviderOutcome::Answer(Answer { write: provider_write, read: vec![] }),
            workspace: &f.ws,
            env: &env,
        });
        assert!(res.refusal.is_none(), "{:?}", res.refusal);
        let protected = Protected::new(&env);
        Confinement::new(res, protected, &f.ws, Some(&f.ws))
    }

    fn granted(plan: &Plan) -> Vec<PathBuf> {
        plan.grants.iter().map(|g| g.path.clone()).collect()
    }

    fn ws_plan(c: &Confinement, ws: &Path) -> (Vec<PathBuf>, Plan) {
        let plan = c.plan().expect("splittable");
        let grants = granted(&plan).into_iter().filter(|g| g.starts_with(ws)).collect();
        (grants, plan)
    }

    #[test]
    fn a_provider_root_with_nothing_protected_inside_is_granted_whole() {
        let f = fixture();
        let cache = f.root.join("data/cache");
        std::fs::create_dir_all(cache.join("sub")).unwrap();
        std::fs::create_dir_all(f.ws.join("src")).unwrap();
        let c = confine(&f, vec![], vec![cache.to_string_lossy().into_owned()]);
        let plan = c.plan().unwrap();
        assert_eq!(granted(&plan).iter().filter(|g| g.starts_with(&cache)).collect::<Vec<_>>(), vec![&cache]);
        assert!(plan.left_out.iter().all(|h| !h.path.starts_with(&cache)));
    }

    #[test]
    fn the_workspace_is_split_even_when_the_project_plugin_folder_is_absent() {
        let f = fixture();
        std::fs::create_dir_all(f.ws.join("src")).unwrap();
        let c = confine(&f, vec![], vec![]);
        let (grants, plan) = ws_plan(&c, &f.ws);
        // A shell could create `.synaps/plugins` in a whole-granted workspace.
        assert_eq!(grants, vec![f.ws.join("src")]);
        assert!(plan.left_out.iter().any(|h| h.path == f.ws.join(".synaps/plugins")));
        assert!(plan.no_new_entries.contains(&f.ws));
    }

    #[test]
    fn a_path_entry_inside_the_workspace_is_split_out_and_listed() {
        let f = fixture();
        for d in ["bin", "src", "docs"] {
            std::fs::create_dir_all(f.ws.join(d)).unwrap();
        }
        std::fs::write(f.ws.join("README.md"), "x").unwrap();
        let c = confine(&f, vec![f.ws.join("bin")], vec![]);
        let (grants, plan) = ws_plan(&c, &f.ws);
        assert!(!grants.contains(&f.ws), "the workspace must not be granted whole: {grants:?}");
        assert!(!grants.contains(&f.ws.join("bin")));
        for kept in ["src", "docs", "README.md"] {
            assert!(grants.contains(&f.ws.join(kept)), "{kept} stays writable: {grants:?}");
        }
        assert!(plan.left_out.iter().any(|h| h.path == f.ws.join("bin") && h.why.contains("PATH")));
        assert!(plan.no_new_entries.contains(&f.ws), "the split dir can't get new entries");
        assert!(c.allows_write(&f.ws.join("bin/tool")).is_err());
        assert!(c.allows_write(&f.ws.join("src/lib.rs")).is_ok());
    }

    #[test]
    fn node_modules_bin_on_the_path_is_split_two_levels_deep() {
        let f = fixture();
        std::fs::create_dir_all(f.ws.join("node_modules/.bin")).unwrap();
        std::fs::create_dir_all(f.ws.join("node_modules/left-pad")).unwrap();
        std::fs::create_dir_all(f.ws.join("src")).unwrap();
        let c = confine(&f, vec![f.ws.join("node_modules/.bin")], vec![]);
        let (grants, plan) = ws_plan(&c, &f.ws);
        assert!(!grants.contains(&f.ws) && !grants.contains(&f.ws.join("node_modules")));
        assert!(!grants.contains(&f.ws.join("node_modules/.bin")));
        assert!(grants.contains(&f.ws.join("node_modules/left-pad")));
        assert!(grants.contains(&f.ws.join("src")));
        assert!(plan.left_out.iter().any(|h| h.path == f.ws.join("node_modules/.bin")));
        assert!(plan.no_new_entries.contains(&f.ws.join("node_modules")));
    }

    fn make_main_checkout(dir: &Path) {
        for d in ["objects", "refs/heads", "logs", "hooks", "info", "worktrees"] {
            std::fs::create_dir_all(dir.join(".git").join(d)).unwrap();
        }
        std::fs::write(dir.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(dir.join(".git/config"), "[core]\n").unwrap();
    }

    #[test]
    fn a_main_checkouts_hooks_and_config_are_left_out() {
        let f = fixture();
        make_main_checkout(&f.ws);
        std::fs::create_dir_all(f.ws.join("src")).unwrap();
        let c = confine(&f, vec![], vec![]);
        let (grants, plan) = ws_plan(&c, &f.ws);
        assert!(!grants.contains(&f.ws) && !grants.contains(&f.ws.join(".git")));
        assert!(grants.contains(&f.ws.join("src")));
        for kept in ["objects", "refs", "logs"] {
            assert!(grants.contains(&f.ws.join(".git").join(kept)), "{kept}: {grants:?}");
        }
        for gone in ["hooks", "config", "info", "worktrees", "HEAD"] {
            assert!(!grants.iter().any(|g| g.starts_with(f.ws.join(".git").join(gone))), "{gone}: {grants:?}");
        }
        assert!(plan.left_out.iter().any(|h| h.path == f.ws.join(".git/hooks")));
        assert!(plan.left_out.iter().any(|h| h.path == f.ws.join(".git/config")));
        assert!(c.allows_write(&f.ws.join(".git/hooks/pre-commit")).is_err());
        assert!(c.allows_write(&f.ws.join(".git/config")).is_err());
        assert!(c.allows_write(&f.ws.join(".git/refs/heads/topic")).is_ok());
        assert!(c.allows_write(&f.ws.join(".git/objects/ab/cdef")).is_ok());
    }

    /// A main repository at `<root>/repo` and a linked worktree of it at
    /// `<root>/repo-wt`; returns (repo, worktree).
    fn make_linked(root: &Path) -> (PathBuf, PathBuf) {
        let repo = root.join("repo");
        let wt = root.join("repo-wt");
        make_main_checkout(&repo);
        let admin = repo.join(".git/worktrees/wt");
        std::fs::create_dir_all(&admin).unwrap();
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(admin.join("commondir"), "../..\n").unwrap();
        std::fs::write(admin.join("gitdir"), format!("{}\n", wt.join(".git").display())).unwrap();
        std::fs::write(admin.join("HEAD"), "ref: refs/heads/wt\n").unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();
        (repo, wt)
    }

    #[test]
    fn a_linked_worktrees_common_dir_gets_only_its_grants() {
        let f = fixture();
        let (repo, wt) = make_linked(&f.root);
        std::fs::create_dir_all(wt.join("src")).unwrap();
        let mut c = confine(&f, vec![], vec![]);
        // The session's workspace is the linked worktree.
        c.roots = vec![wt.clone()];
        c.project_dirs = vec![wt.clone()];
        let plan = c.plan().unwrap();
        let grants = granted(&plan);
        assert!(grants.contains(&wt.join("src")));
        assert!(!grants.contains(&wt) && !grants.contains(&wt.join(".git")), "{grants:?}");
        for kept in ["objects", "refs", "logs"] {
            assert!(grants.contains(&repo.join(".git").join(kept)), "{kept}: {grants:?}");
        }
        assert!(grants.contains(&repo.join(".git/worktrees/wt")));
        assert!(!grants.iter().any(|g| g.starts_with(repo.join(".git/hooks")) || *g == repo.join(".git/config")));
        assert!(c.allows_write(&repo.join(".git/hooks/pre-commit")).is_err());
        assert!(c.allows_write(&repo.join(".git/config")).is_err());
        assert!(c.allows_write(&wt.join(".git")).is_err(), "the .git file can't be repointed");
        assert!(c.allows_write(&repo.join(".git/worktrees/wt/index")).is_ok());
        assert!(c.allows_write(&repo.join(".git/refs/heads/wt")).is_ok());
    }

    #[test]
    fn a_common_dir_inside_another_granted_root_is_split_too() {
        let f = fixture();
        let (repo, wt) = make_linked(&f.root);
        let mut c = confine(&f, vec![], vec![]);
        // The worktree is the workspace and the main repo is granted whole.
        c.roots = vec![wt.clone(), repo.clone()];
        c.project_dirs = vec![wt.clone()];
        let plan = c.plan().unwrap();
        let grants = granted(&plan);
        assert!(!grants.contains(&repo), "{grants:?}");
        assert!(!grants.iter().any(|g| *g == repo.join(".git") || g.starts_with(repo.join(".git/hooks"))));
        assert!(plan.left_out.iter().any(|h| h.path == repo.join(".git/hooks")));
        assert!(plan.left_out.iter().any(|h| h.path == repo.join(".git/config")));
    }

    #[test]
    fn a_project_plugin_folder_is_left_out() {
        let f = fixture();
        std::fs::create_dir_all(f.ws.join(".synaps/plugins/p")).unwrap();
        std::fs::create_dir_all(f.ws.join(".synaps/notes")).unwrap();
        std::fs::create_dir_all(f.ws.join("src")).unwrap();
        let c = confine(&f, vec![], vec![]);
        let (grants, plan) = ws_plan(&c, &f.ws);
        assert!(!grants.contains(&f.ws) && !grants.contains(&f.ws.join(".synaps")));
        assert!(!grants.iter().any(|g| g.starts_with(f.ws.join(".synaps/plugins"))));
        assert!(grants.contains(&f.ws.join(".synaps/notes")));
        assert!(grants.contains(&f.ws.join("src")));
        assert!(plan.left_out.iter().any(|h| h.path == f.ws.join(".synaps/plugins")));
        assert!(c.allows_write(&f.ws.join(".synaps/plugins/p/run.sh")).is_err());
        assert!(c.allows_write(&f.ws.join(".synaps/notes/a.md")).is_ok());
    }

    #[test]
    fn a_root_that_cannot_be_split_refuses_with_the_inner_path() {
        let f = fixture();
        std::fs::create_dir_all(f.ws.join("bin")).unwrap();
        for i in 0..=MAX_SPLIT_ENTRIES {
            std::fs::write(f.ws.join(format!("f{i}")), "").unwrap();
        }
        let c = confine(&f, vec![f.ws.join("bin")], vec![]);
        let err = c.plan().expect_err("too many entries to split");
        assert_eq!(err.path, f.ws.join("bin"));
        assert_eq!(err.cause(), format!("protected-inside-grant: {}", f.ws.join("bin").display()));
        assert_eq!(err.refusal().cause.word(), "protected-inside-grant");
    }

    #[test]
    fn an_entry_that_is_a_symlink_on_a_split_path_is_not_granted() {
        let f = fixture();
        std::fs::create_dir_all(f.ws.join("bin")).unwrap();
        std::fs::create_dir_all(f.root.join("elsewhere")).unwrap();
        std::os::unix::fs::symlink(f.root.join("elsewhere"), f.ws.join("link")).unwrap();
        std::fs::create_dir_all(f.ws.join("src")).unwrap();
        let c = confine(&f, vec![f.ws.join("bin")], vec![]);
        let plan = c.plan().unwrap();
        // The plan lists the link; opening it refuses (see the open pins).
        let grants = granted(&plan);
        assert!(grants.contains(&f.ws.join("link")) && grants.contains(&f.ws.join("src")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_grant_swapped_for_a_symlink_after_validation_is_dropped_and_reported() {
        let f = fixture();
        let granted_dir = f.root.join("data/out");
        let victim = f.root.join("victim");
        std::fs::create_dir_all(&granted_dir).unwrap();
        std::fs::create_dir_all(&victim).unwrap();
        let c = confine(&f, vec![], vec![granted_dir.to_string_lossy().into_owned()]);
        assert!(c.resolution.write.contains(&granted_dir), "valid when the policy was resolved");
        let plan = c.plan().unwrap();
        let swapped = granted_dir.clone();
        let victim2 = victim.clone();
        let mut swap = move || {
            std::fs::remove_dir(&swapped).unwrap();
            std::os::unix::fs::symlink(&victim2, &swapped).unwrap();
        };
        let opened = c.open_grants(&plan, &mut swap).unwrap();
        assert!(opened.grants.iter().all(|g| g.path != granted_dir), "the swapped path must not be granted");
        let d = opened.dropped.iter().find(|d| d.path == granted_dir).expect("reported");
        assert_eq!(d.reason, REASON_SYMLINK);
        assert_eq!(c.late_dropped().len(), opened.dropped.len());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_swapped_parent_component_is_caught_too() {
        let f = fixture();
        let parent = f.root.join("data");
        let granted_dir = parent.join("out");
        let elsewhere = f.root.join("elsewhere");
        std::fs::create_dir_all(&granted_dir).unwrap();
        std::fs::create_dir_all(elsewhere.join("out")).unwrap();
        let c = confine(&f, vec![], vec![granted_dir.to_string_lossy().into_owned()]);
        let plan = c.plan().unwrap();
        let mut swap = {
            let parent = parent.clone();
            let elsewhere = elsewhere.clone();
            move || {
                std::fs::rename(&parent, f.root.join("data-old")).unwrap();
                std::os::unix::fs::symlink(&elsewhere, &parent).unwrap();
            }
        };
        let opened = c.open_grants(&plan, &mut swap).unwrap();
        assert!(opened.grants.iter().all(|g| g.path != granted_dir));
        assert!(opened.dropped.iter().any(|d| d.path == granted_dir && d.reason == REASON_SYMLINK));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn open_no_symlinks_refuses_links_and_relative_paths() {
        let f = fixture();
        std::fs::create_dir_all(f.root.join("real")).unwrap();
        std::os::unix::fs::symlink(f.root.join("real"), f.root.join("link")).unwrap();
        assert!(open_no_symlinks(&f.root.join("real")).is_ok());
        assert!(matches!(open_no_symlinks(&f.root.join("link")), Err(OpenFailure::Swapped)));
        assert!(matches!(open_no_symlinks(&f.root.join("link/x")), Err(OpenFailure::Swapped)));
        assert!(matches!(open_no_symlinks(&f.root.join("nope")), Err(OpenFailure::Missing)));
        assert!(matches!(open_no_symlinks(Path::new("real")), Err(OpenFailure::Swapped)));
        assert!(matches!(open_no_symlinks(&f.root.join("real/../real")), Err(OpenFailure::Swapped)));
    }
}
