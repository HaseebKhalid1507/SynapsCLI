//! The find engine: the same walk as grep ([`super::walk`]: ignore files,
//! hidden entries, VCS directories and `node_modules`/`target` pruned) and a
//! name glob, printed as GNU `find PATH -name GLOB` printed it:
//!
//! - one path per line, the search path joined with the entry's path below
//!   it (`<cwd>/./src/a.rs` for the path `.`), native separators;
//! - the search path itself first, when its name matches (as GNU's `-name`
//!   takes it: `.` for `<cwd>/.`);
//! - files, directories, links and anything else (FIFOs, sockets) are
//!   listed; nothing is ever opened, so a FIFO can't block.
//!
//! The order is the walk's: a directory, then its entries sorted by name,
//! depth first. Output stops at the byte cap on a whole line, so a cut is
//! the same on every run.

use std::fs::FileType;
use std::path::{Path, PathBuf};
use std::time::Duration;

use globset::GlobMatcher;
use ignore::DirEntry;

use super::{append_note, stop_note, truncation_footer, Control, Stop, Tally, WalkOptions};
use crate::RuntimeError;

/// find's `type`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EntryType {
    /// `f`: a regular file.
    File,
    /// `d`: a directory.
    Dir,
    /// `l`: a symbolic link (never followed).
    Link,
}

impl EntryType {
    /// `f`, `d` or `l`, as `find -type`; anything else is an error.
    pub(crate) fn parse(t: &str) -> Result<Self, String> {
        match t {
            "f" => Ok(Self::File),
            "d" => Ok(Self::Dir),
            "l" => Ok(Self::Link),
            _ => Err(format!("Invalid type {t:?}: use \"f\", \"d\" or \"l\"")),
        }
    }

    fn matches(self, file_type: FileType) -> bool {
        match self {
            Self::File => file_type.is_file(),
            Self::Dir => file_type.is_dir(),
            Self::Link => file_type.is_symlink(),
        }
    }
}

/// One find call, resolved.
pub(crate) struct FilesRequest {
    pub(crate) root: PathBuf,
    /// The glob on entry names.
    pub(crate) name: GlobMatcher,
    pub(crate) entry_type: Option<EntryType>,
    pub(crate) no_ignore: bool,
    pub(crate) cwd: Option<PathBuf>,
    /// Output byte cap (`max_tool_buffer`).
    pub(crate) cap: usize,
}

/// What a find found.
#[derive(Debug, Default)]
pub(crate) struct FilesOutcome {
    /// Matching paths, each ending in `\n`.
    pub(crate) text: String,
    /// The cap was reached and the walk stopped.
    pub(crate) truncated: bool,
    pub(crate) stop: Option<Stop>,
    /// Entries seen, not counting a directory root (so an unreadable root
    /// directory has seen nothing).
    pub(crate) visited: usize,
    pub(crate) errors: Tally,
    /// Ignore rules applied under a directory root (not `no_ignore`, not one
    /// file), so an empty result gets [`super::IGNORED_NOTE`].
    pub(crate) ignore_rules: bool,
}

/// Lists what under `req.root` matches. Errors only when the root doesn't
/// exist or can't be read at all; per-entry problems go in the tally.
pub(crate) fn search(req: &FilesRequest, control: &Control) -> Result<FilesOutcome, String> {
    super::check_root(&req.root)?;
    let opts = WalkOptions {
        no_ignore: req.no_ignore,
        cwd: req.cwd.clone(),
        types: None,
    };
    let mut out = collect(req, control, super::walk(&req.root, &opts));
    out.ignore_rules = !req.no_ignore && req.root.is_dir();
    Ok(out)
}

/// The listing for walk `entries` (split out so tests can stop a walk at a
/// chosen entry).
fn collect(
    req: &FilesRequest,
    control: &Control,
    entries: impl IntoIterator<Item = Result<DirEntry, ignore::Error>>,
) -> FilesOutcome {
    let mut out = FilesOutcome::default();
    for item in entries {
        if let Some(stop) = control.stop() {
            out.stop = Some(stop);
            break;
        }
        let entry = match item {
            Ok(entry) => entry,
            Err(err) => {
                out.errors.add(walk_error(&err));
                continue;
            }
        };
        // A bad ignore file in this directory.
        if let Some(err) = entry.error() {
            out.errors.add(walk_error(err));
        }
        let root = entry.depth() == 0;
        if !root || !entry.path().is_dir() {
            out.visited += 1;
        }
        if !wanted(req, &entry, root) {
            continue;
        }
        let path = entry.path().to_string_lossy();
        if out.text.len() + path.len() + 1 > req.cap {
            out.truncated = true;
            break;
        }
        out.text.push_str(&path);
        out.text.push('\n');
    }
    out
}

/// A walk error as `path: reason`. ignore's own text for an unreadable
/// directory names the path twice (`p: IO error for operation on p: …`).
fn walk_error(err: &ignore::Error) -> String {
    if let ignore::Error::WithPath { path, err: inner } = err {
        if let Some(io) = inner.io_error() {
            // walkdir wraps the OS error with its path; `source` is the OS
            // error itself.
            let reason =
                std::error::Error::source(io).map_or_else(|| io.to_string(), |os| os.to_string());
            return format!("{}: {reason}", path.display());
        }
    }
    err.to_string()
}

/// Whether `entry` matches the name glob and the type.
fn wanted(req: &FilesRequest, entry: &DirEntry, root: bool) -> bool {
    let named = if root {
        req.name.is_match(root_name(entry.path()))
    } else {
        req.name.is_match(entry.file_name())
    };
    if !named {
        return false;
    }
    let Some(want) = req.entry_type else {
        return true;
    };
    // Entries below the root have their own type: links aren't followed.
    // The root is followed (the walk enters a linked root directory), so its
    // type is its target's.
    let file_type = if root {
        std::fs::metadata(entry.path())
            .ok()
            .map(|md| md.file_type())
            .or_else(|| entry.file_type())
    } else {
        entry.file_type()
    };
    file_type.is_some_and(|t| want.matches(t))
}

/// The name GNU find's `-name` matches the search path by: its last
/// component as written, so `<cwd>/.` is `.` (where `Path::file_name` gives
/// the cwd's name).
fn root_name(root: &Path) -> String {
    let text = root.to_string_lossy();
    let trimmed = text.trim_end_matches(std::path::is_separator);
    if trimmed.is_empty() {
        return text.into_owned();
    }
    trimmed
        .rsplit(std::path::is_separator)
        .next()
        .unwrap_or(trimmed)
        .to_string()
}

/// The tool result. Nothing listed is an error when the walk stopped early
/// or nothing could be read, else exactly "No files found." (with a note if
/// there were errors).
pub(crate) fn render(out: FilesOutcome, cap: usize, timeout: Duration) -> crate::Result<String> {
    if out.text.is_empty() && !out.truncated {
        match out.stop {
            Some(Stop::TimedOut) => {
                return Err(RuntimeError::Tool(format!(
                    "Find timed out after {}s before finding a match. Narrow the path.",
                    timeout.as_secs()
                )))
            }
            Some(Stop::Cancelled) => {
                return Err(RuntimeError::Tool(
                    "Find was cancelled before finding a match.".to_string(),
                ))
            }
            None => {}
        }
        if out.visited == 0 && out.errors.count > 0 {
            return Err(RuntimeError::Tool(out.errors.failure()));
        }
        let mut text = "No files found.".to_string();
        if let Some(note) = out.errors.note() {
            append_note(&mut text, &note);
        }
        if out.ignore_rules {
            append_note(&mut text, super::IGNORED_NOTE);
        }
        return Ok(text);
    }
    let mut text = out.text;
    text.truncate(text.trim_end_matches('\n').len());
    if out.truncated {
        append_note(&mut text, &truncation_footer(cap));
    }
    if let Some(stop) = out.stop {
        append_note(&mut text, &stop_note("find", stop, timeout));
    }
    if let Some(note) = out.errors.note() {
        append_note(&mut text, &note);
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::super::pattern;
    use super::*;
    use std::fs;

    const LONG: Duration = Duration::from_secs(60);

    /// `root` joined with a `/`-separated relative path, component by
    /// component, as the walk builds paths (native separators on Windows).
    fn native(root: &Path, rel: &str) -> PathBuf {
        rel.split('/').fold(root.to_path_buf(), |p, c| p.join(c))
    }

    fn touch(root: &Path, rel: &str) {
        let path = native(root, rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "").unwrap();
    }

    fn mkdir(root: &Path, rel: &str) {
        fs::create_dir_all(native(root, rel)).unwrap();
    }

    fn request(root: &Path, glob: &str) -> FilesRequest {
        FilesRequest {
            root: root.to_path_buf(),
            name: pattern::name_glob(glob).unwrap(),
            entry_type: None,
            no_ignore: false,
            cwd: None,
            cap: 1 << 20,
        }
    }

    fn run(req: &FilesRequest) -> FilesOutcome {
        search(req, &Control::new(LONG)).unwrap()
    }

    /// The listed paths relative to `root`, `/`-separated, in output order.
    /// The root itself is `""`.
    fn listed(out: &FilesOutcome, root: &Path) -> Vec<String> {
        let root = root.display().to_string();
        out.text
            .lines()
            .map(|line| {
                let rest = line
                    .strip_prefix(root.as_str())
                    .unwrap_or_else(|| panic!("{line:?} is outside {root:?}"));
                rest.trim_start_matches(['/', '\\']).replace('\\', "/")
            })
            .collect()
    }

    /// Runs `f` on a thread and fails (instead of hanging) if it takes more
    /// than 20 s.
    #[cfg(unix)]
    fn within<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(Duration::from_secs(20)).expect("find hung")
    }

    /// The name glob: `*.rs` at any depth, `Cargo.*`, braces. The root's
    /// own name (`.` here, as GNU) doesn't match these.
    #[test]
    fn name_globs_match_at_any_depth() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for rel in [
            "Cargo.toml",
            "Cargo.lock",
            "cargo.txt",
            "src/main.rs",
            "src/lib.rs.bak",
            "src/deep/er/x.rs",
            "a.toml",
        ] {
            touch(root, rel);
        }
        let dot = root.join(".");
        let names = |glob: &str| listed(&run(&request(&dot, glob)), &dot);
        assert_eq!(names("*.rs"), ["src/deep/er/x.rs", "src/main.rs"]);
        assert_eq!(names("Cargo.*"), ["Cargo.lock", "Cargo.toml"]);
        assert_eq!(
            names("*.{rs,toml}"),
            ["Cargo.toml", "a.toml", "src/deep/er/x.rs", "src/main.rs"]
        );
        assert!(names("*.py").is_empty());
    }

    /// GNU find prints the search path as given, then paths below it joined
    /// onto it: `<cwd>/.` and `<cwd>/./src`. The path matches `*` by its
    /// own name, `.`.
    #[test]
    fn paths_keep_the_search_path_as_given() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "src/a.rs");
        let dot = dir.path().join(".");
        let text = run(&request(&dot, "*")).text;
        let expected = format!(
            "{}\n{}\n{}\n",
            dot.display(),
            dot.join("src").display(),
            dot.join("src").join("a.rs").display()
        );
        assert_eq!(text, expected);
        // `.` is the root's name, not the directory's.
        let name = dir.path().file_name().unwrap().to_string_lossy();
        assert!(run(&request(&dot, &name)).text.is_empty());
        assert_eq!(root_name(&dot), ".");
        assert_eq!(root_name(&dir.path().join("src")), "src");
        assert_eq!(
            root_name(&PathBuf::from(format!("src{}", std::path::MAIN_SEPARATOR))),
            "src"
        );
    }

    /// A file as the path lists the file when its name matches.
    #[test]
    fn a_file_path_lists_itself() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "a.rs");
        let file = dir.path().join("a.rs");
        assert_eq!(
            run(&request(&file, "*.rs")).text,
            format!("{}\n", file.display())
        );
        assert!(run(&request(&file, "*.py")).text.is_empty());
        let mut req = request(&file, "*");
        req.entry_type = Some(EntryType::Dir);
        assert!(run(&req).text.is_empty());
    }

    #[test]
    fn type_filters_files_and_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(root, "src/a.rs");
        mkdir(root, "src/empty");
        let mut req = request(root, "*");
        req.entry_type = Some(EntryType::File);
        assert_eq!(listed(&run(&req), root), ["src/a.rs"]);
        // The root is a directory too, when its name matches.
        req.entry_type = Some(EntryType::Dir);
        assert_eq!(listed(&run(&req), root), ["", "src", "src/empty"]);
        req.entry_type = Some(EntryType::Link);
        assert!(run(&req).text.is_empty());
    }

    #[test]
    fn type_parses_f_d_l_only() {
        assert_eq!(EntryType::parse("f"), Ok(EntryType::File));
        assert_eq!(EntryType::parse("d"), Ok(EntryType::Dir));
        assert_eq!(EntryType::parse("l"), Ok(EntryType::Link));
        for bad in ["", "x", "fd", "F", "-delete"] {
            let err = EntryType::parse(bad).unwrap_err();
            assert!(err.starts_with("Invalid type"), "{bad:?}: {err}");
        }
    }

    /// Links are `l`, never followed: a link to a directory is listed as a
    /// link, and what's behind it isn't walked. A loop can't hang the walk.
    #[cfg(unix)]
    #[test]
    fn links_are_l_and_never_entered() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        touch(&root, "real/a.rs");
        symlink(root.join("real"), root.join("alias")).unwrap();
        symlink(root.join("real").join("a.rs"), root.join("link.rs")).unwrap();
        symlink(&root, root.join("real").join("loop")).unwrap();
        let tree = root.clone();
        let out = within(move || {
            let mut req = request(&tree, "*");
            req.entry_type = Some(EntryType::Link);
            run(&req)
        });
        assert_eq!(listed(&out, &root), ["alias", "link.rs", "real/loop"]);
        let tree = root.clone();
        let out = within(move || run(&request(&tree, "*.rs")));
        assert_eq!(listed(&out, &root), ["link.rs", "real/a.rs"]);
        let tree = root.clone();
        let out = within(move || {
            let mut req = request(&tree, "*");
            req.entry_type = Some(EntryType::Dir);
            run(&req)
        });
        assert_eq!(listed(&out, &root), ["", "real"]);
        assert_eq!(out.errors.count, 0, "{:?}", out.errors.first);
    }

    /// A linked search path is followed (its entries are listed) and is a
    /// directory for `type`; links below it still aren't followed.
    #[cfg(unix)]
    #[test]
    fn a_linked_root_is_followed() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        touch(&root, "real/a.rs");
        symlink(&root, root.join("real").join("loop")).unwrap();
        symlink(root.join("real"), root.join("alias")).unwrap();
        let alias = root.join("alias");
        let tree = alias.clone();
        let out = within(move || run(&request(&tree, "*")));
        assert_eq!(listed(&out, &alias), ["", "a.rs", "loop"]);
        let tree = alias.clone();
        let out = within(move || {
            let mut req = request(&tree, "*");
            req.entry_type = Some(EntryType::Dir);
            run(&req)
        });
        assert_eq!(listed(&out, &alias), [""]);
    }

    /// `.gitignore` (in a repo), `.ignore` and `.git/info/exclude` apply to
    /// files and directories (an ignored directory isn't listed or walked);
    /// `no_ignore` brings them back.
    #[test]
    fn ignore_files_apply_and_no_ignore_lifts_them() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        mkdir(root, ".git/info");
        fs::write(native(root, ".git/info/exclude"), "excluded.txt\n").unwrap();
        fs::write(root.join(".gitignore"), "dist/\n*.log\n").unwrap();
        fs::write(root.join(".ignore"), "skipme.txt\n").unwrap();
        for rel in [
            "dist/x.js",
            "x.log",
            "skipme.txt",
            "excluded.txt",
            "keep.txt",
        ] {
            touch(root, rel);
        }
        let mut req = request(root, "*");
        assert_eq!(
            listed(&run(&req), root),
            ["", ".gitignore", ".ignore", "keep.txt"]
        );
        req.no_ignore = true;
        assert_eq!(
            listed(&run(&req), root),
            [
                "",
                ".gitignore",
                ".ignore",
                "dist",
                "dist/x.js",
                "excluded.txt",
                "keep.txt",
                "skipme.txt",
                "x.log"
            ]
        );
    }

    /// Outside a git repo `.gitignore` isn't read (ripgrep's `require_git`,
    /// as for grep); `.ignore` still is.
    #[test]
    fn gitignore_outside_a_repo_is_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join(".gitignore"), "*.log\n").unwrap();
        fs::write(root.join(".ignore"), "*.tmp\n").unwrap();
        touch(root, "x.log");
        touch(root, "y.tmp");
        assert_eq!(listed(&run(&request(root, "?.*")), root), ["x.log"]);
    }

    /// Row 25: `.git` (a directory or a worktree's `.git` file) and the other
    /// VCS directories are never listed or entered, even with `no_ignore`;
    /// `node_modules` and `target` are neither listed nor walked unless
    /// `no_ignore`. A file named `target` is just a file.
    #[test]
    fn vcs_and_hard_excludes_are_pruned() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for rel in [
            ".git/config",
            ".hg/x",
            ".svn/x",
            ".bzr/x",
            ".jj/x",
            ".sl/x",
            "node_modules/pkg/index.js",
            "target/debug/build.rs",
            "deep/target/d.rs",
            "sub/.git",
            "sub/a.rs",
            "src/target",
        ] {
            touch(root, rel);
        }
        let mut req = request(root, "*");
        assert_eq!(
            listed(&run(&req), root),
            ["", "deep", "src", "src/target", "sub", "sub/a.rs"]
        );
        req.no_ignore = true;
        assert_eq!(
            listed(&run(&req), root),
            [
                "",
                "deep",
                "deep/target",
                "deep/target/d.rs",
                "node_modules",
                "node_modules/pkg",
                "node_modules/pkg/index.js",
                "src",
                "src/target",
                "sub",
                "sub/a.rs",
                "target",
                "target/debug",
                "target/debug/build.rs"
            ]
        );
        // By name too: `.git` is never a result.
        let mut req = request(root, ".git");
        req.no_ignore = true;
        assert!(run(&req).text.is_empty());
    }

    /// Pruned means not walked: an unreadable directory deep inside
    /// `node_modules` would be an error if the walk went there.
    #[cfg(unix)]
    #[test]
    fn excluded_dirs_are_not_walked() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(root, "src/a.rs");
        for locked in ["node_modules/pkg/deep", "target/x", ".git/objects"] {
            let locked = native(root, locked);
            fs::create_dir_all(&locked).unwrap();
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        }
        let out = run(&request(root, "*"));
        assert_eq!(listed(&out, root), ["", "src", "src/a.rs"]);
        assert_eq!(out.errors.count, 0, "{:?}", out.errors.first);
        for locked in ["node_modules/pkg/deep", "target/x", ".git/objects"] {
            let _ = fs::set_permissions(native(root, locked), fs::Permissions::from_mode(0o755));
        }
    }

    /// An explicit path into an excluded directory is walked (the root is
    /// never filtered); excluded directories below it still are.
    #[test]
    fn an_explicit_path_into_an_excluded_dir_is_walked() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(root, "node_modules/pkg/index.js");
        touch(root, "node_modules/pkg/node_modules/inner/x.js");
        touch(root, "target/app/src/a.rs");
        let nm = root.join("node_modules");
        assert_eq!(listed(&run(&request(&nm, "*.js")), &nm), ["pkg/index.js"]);
        let app = native(root, "target/app");
        assert_eq!(listed(&run(&request(&app, "*.rs")), &app), ["src/a.rs"]);
    }

    #[test]
    fn hidden_entries_are_listed() {
        let dir = tempfile::tempdir().unwrap();
        // Not the temp dir itself: its own name (`.tmp…`) matches `.*`.
        let root = &dir.path().join("p");
        touch(root, ".github/workflows/ci.yml");
        touch(root, ".env.example");
        touch(root, "plain.yml");
        assert_eq!(
            listed(&run(&request(root, "*.yml")), root),
            [".github/workflows/ci.yml", "plain.yml"]
        );
        assert_eq!(
            listed(&run(&request(root, ".*")), root),
            [".env.example", ".github"]
        );
    }

    /// Byte order by name within each directory, a directory before its
    /// entries, whatever the creation order; the same on every run.
    #[test]
    fn order_is_sorted_and_stable() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for rel in ["z.rs", "m/b.rs", "a.rs", "B.rs", "m/a.rs", "m-x.rs"] {
            touch(root, rel);
        }
        let req = request(root, "*.rs");
        let first = run(&req).text;
        for _ in 0..3 {
            assert_eq!(run(&req).text, first);
        }
        assert_eq!(
            listed(&run(&req), root),
            ["B.rs", "a.rs", "m/a.rs", "m/b.rs", "m-x.rs", "z.rs"]
        );
    }

    /// At the cap the walk stops: whole lines, a prefix of the full listing,
    /// and the footer; the result ends on a whole path.
    #[test]
    fn output_cap_stops_the_walk() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for i in 0..200 {
            touch(root, &format!("f{i:03}.txt"));
        }
        let mut req = request(root, "*.txt");
        let full = run(&req).text;
        req.cap = 500;
        let out = run(&req);
        assert!(out.truncated);
        assert!(out.visited < 200, "walked {} entries", out.visited);
        assert!(out.text.len() <= 500 && out.text.ends_with('\n'));
        assert!(out.text.len() > 500 - root.join("f000.txt").as_os_str().len() - 1);
        assert!(full.starts_with(&out.text));
        let text = render(out, 500, LONG).unwrap();
        let (body, footer) = text.split_once("\n\n").unwrap();
        assert!(body.ends_with(".txt"), "{body}");
        assert_eq!(footer, "... (output truncated, more than 500 total bytes)");
    }

    /// Stops the walk (as a cancel or deadline would) when `at` comes up.
    fn stop_at(req: &FilesRequest, control: &Control, at: &str) -> FilesOutcome {
        let opts = WalkOptions::default();
        let trigger = control.clone();
        let at = at.to_string();
        let walk = super::super::walk(&req.root, &opts).inspect(move |item| {
            if item.as_ref().is_ok_and(|e| e.file_name() == at.as_str()) {
                trigger.cancel();
            }
        });
        collect(req, control, walk)
    }

    fn numbered_tree(root: &Path) {
        for i in 0..5 {
            touch(root, &format!("f{i}.rs"));
        }
    }

    /// Cancelling mid-walk keeps what was listed and says so.
    #[test]
    fn cancel_returns_partial_results() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        numbered_tree(root);
        let out = stop_at(&request(root, "*.rs"), &Control::new(LONG), "f2.rs");
        assert_eq!(listed(&out, root), ["f0.rs", "f1.rs"]);
        assert_eq!(out.stop, Some(Stop::Cancelled));
        let text = render(out, 1 << 20, LONG).unwrap();
        assert!(
            text.ends_with("f1.rs\n\n... (find was cancelled; results are partial)"),
            "{text}"
        );
        // Cancelled before anything matched: an error.
        let out = stop_at(&request(root, "*.rs"), &Control::new(LONG), "f0.rs");
        let err = render(out, 1 << 20, LONG).unwrap_err().to_string();
        assert!(
            err.contains("Find was cancelled before finding a match"),
            "{err}"
        );
    }

    /// Past the deadline: partial results and a note; nothing yet is an
    /// error, not "No files found.".
    #[test]
    fn deadline_returns_partial_results_or_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        numbered_tree(root);
        let partial = FilesOutcome {
            text: format!("{}\n", root.join("f0.rs").display()),
            stop: Some(Stop::TimedOut),
            visited: 1,
            ..Default::default()
        };
        let timeout = Duration::from_secs(10);
        let text = render(partial, 1 << 20, timeout).unwrap();
        assert!(
            text.ends_with(
                "f0.rs\n\n... (find timed out after 10s; results are partial, more may be missing)"
            ),
            "{text}"
        );
        let out = search(&request(root, "*.rs"), &Control::new(Duration::ZERO)).unwrap();
        assert_eq!(out.stop, Some(Stop::TimedOut));
        assert!(out.text.is_empty());
        let err = render(out, 1 << 20, timeout).unwrap_err().to_string();
        assert!(
            err.contains("Find timed out after 10s before finding a match"),
            "{err}"
        );
    }

    #[test]
    fn missing_path_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        let err = search(&request(&missing, "*"), &Control::new(LONG)).unwrap_err();
        assert_eq!(err, format!("Path not found: {}", missing.display()));
    }

    /// A miss under a directory is "No files found." and the ignored-files
    /// note; with `no_ignore` it's exactly "No files found.".
    #[test]
    fn no_match_is_exactly_no_files_found() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "a.rs");
        let out = run(&request(dir.path(), "*.py"));
        assert_eq!(
            render(out, 1 << 20, LONG).unwrap(),
            format!("No files found.\n\n{}", super::super::IGNORED_NOTE)
        );
        let mut req = request(dir.path(), "*.py");
        req.no_ignore = true;
        assert_eq!(render(run(&req), 1 << 20, LONG).unwrap(), "No files found.");
    }

    /// An unreadable directory is listed (it matched) and counted in a note;
    /// the rest of the walk goes on. Nothing readable at all is an error.
    #[cfg(unix)]
    #[test]
    fn unreadable_dir_is_a_note() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(root, "a/x.rs");
        touch(root, "b/y.rs");
        touch(root, "c/z.rs");
        let locked = root.join("b");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(&locked).is_ok() {
            eprintln!("skipped: mode 000 is still readable here (root?)");
            return;
        }
        let out = run(&request(root, "*.rs"));
        assert_eq!(listed(&out, root), ["a/x.rs", "c/z.rs"]);
        assert_eq!(out.errors.count, 1);
        let text = render(out, 1 << 20, LONG).unwrap();
        let note = format!(
            "\n\n[1 error while searching, results may be incomplete. First: {}: ",
            locked.display()
        );
        assert!(text.contains(&note), "{text}");
        assert!(text.contains("ermission denied"), "{text}");
        assert!(!text.contains("IO error for operation"), "{text}");
        // No match but something was read: "No files found." and the note.
        let text = render(run(&request(root, "*.py")), 1 << 20, LONG).unwrap();
        assert!(text.starts_with("No files found.\n\n[1 error"), "{text}");
        // The unreadable directory as the path: nothing read, an error.
        let err = render(run(&request(&locked, "*.rs")), 1 << 20, LONG)
            .unwrap_err()
            .to_string();
        assert!(err.contains("ermission denied"), "{err}");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A FIFO is listed like any entry (not as `f`), in the tree or as the
    /// path, and never opened, so nothing blocks.
    #[cfg(unix)]
    #[test]
    fn fifo_is_listed_and_never_opened() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        touch(&root, "a.txt");
        let fifo = root.join("pipe");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: `c` is a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
        let tree = root.clone();
        let out = within(move || run(&request(&tree, "*")));
        assert_eq!(listed(&out, &root), ["", "a.txt", "pipe"]);
        let tree = root.clone();
        let out = within(move || {
            let mut req = request(&tree, "*");
            req.entry_type = Some(EntryType::File);
            run(&req)
        });
        assert_eq!(listed(&out, &root), ["a.txt"]);
        let path = fifo.clone();
        let out = within(move || run(&request(&path, "pipe")));
        assert_eq!(out.text, format!("{}\n", fifo.display()));
    }
}
