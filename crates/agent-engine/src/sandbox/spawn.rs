//! One spawn under a session's policy, once `super::decide` has decided to
//! restrict it: restricting the child (bash: a `pre_exec` hook; PTY: a confined thread),
//! the sandboxed `sudo` prelude, and the hint added to a result whose output
//! shows a write the sandbox refused.

use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use super::audit::{render_path, safe_class};
use super::landlock::{self, Ruleset, Tag};
use super::policy::{BuildError, Policy, RuleClass};
use super::SandboxBinding;
// The decision moved to `super::decide`; its tests here still name these.
#[cfg(test)]
use super::{landlock::Unavailable, policy, SpawnSandbox};

/// The sandboxed `sudo` function's refusal (stderr, exit 1), followed by the
/// policy's contact sentence.
pub const SUDO_REFUSED: &str =
    "synaps sandbox: sudo is refused in this session (no_new_privs is set, so setuid programs can't raise privileges).";
/// What sudo prints when no_new_privs keeps it from becoming root: sudo-rs
/// and sudo.ws both print the first; sudo-rs's setuid self-check prints the
/// second (under no_new_privs the setuid bit is ignored).
const NNP_SUDO_TEXTS: &[&str] =
    &["\"no new privileges\" flag is set", "sudo must be owned by uid 0 and have the setuid bit set"];
/// Paths listed in a hint before "+N more".
const HINT_MAX_PATHS: usize = 6;

/// Does `command` run `sudo`: the word anywhere in it (`sudo x`,
/// `cd d && sudo x`, `command sudo x`, `/usr/bin/sudo x`), not inside a
/// longer name (`pseudo`, `visudo`, `sudoers`, `sudo_x`)? Only such a
/// command's output can show a refused sudo: any other output that quotes
/// the refusal (a doc, a source file, a log) is just text.
pub fn invokes_sudo(command: &str) -> bool {
    let word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    command.match_indices("sudo").any(|(i, m)| {
        let before = command[..i].chars().next_back();
        let after = command[i + m.len()..].chars().next();
        !before.is_some_and(word) && !after.is_some_and(word)
    })
}
/// The denial class of `<git_common>/packed-refs.lock`: git locks it to
/// delete any ref (a commit deletes a pseudo-ref too), and the common dir's
/// root stays read-only by design.
pub const PACKED_REFS_CLASS: &str = "git_packed_refs";

/// A decided, ready-to-apply domain for one spawn.
pub struct Prepared {
    ruleset: Ruleset,
    ctx: SpawnCtx,
}

/// Everything about one sandboxed spawn except the domain itself: its
/// identity (session, tool, tag) and what hints need (the rules, the
/// contact sentence, where new worktrees can go).
#[derive(Clone, Debug)]
pub struct SpawnCtx {
    pub tag: Tag,
    pub session: String,
    pub tool: &'static str,
    pub contact: String,
    /// Every rule as (path, is a directory), baseline included.
    pub rules: Vec<(PathBuf, bool)>,
    /// The non-baseline rule paths, for hints.
    pub allowed: Vec<PathBuf>,
    /// The first `write` directory: where a new worktree can go.
    pub work_dir: Option<PathBuf>,
    /// The policy's `git_common` directories (their root stays read-only,
    /// so git can't lock `packed-refs` there).
    pub git_common: Vec<PathBuf>,
    pub home: PathBuf,
    pub base: PathBuf,
    /// The spawn's place in the kernel-audit reader's registry (`None`
    /// without a reader). Every clone holds it: the spawn counts as exited
    /// when the last one is dropped.
    pub audit: Option<super::audit::Registration>,
}

/// What a sandboxed result gets: a hint for the agent and log lines.
#[derive(Debug, PartialEq, Eq)]
pub struct Annotation {
    pub hint: String,
    pub lines: Vec<String>,
}

/// The decision itself is portable (`super::decide`); its tests here and
/// in `audit` still call it by this path.
#[cfg(test)]
pub(crate) use super::decide::{prepare, prepare_hooked};

/// The policy's Landlock ruleset, built in the parent before any child
/// exists (a test binding can make the build fail like a refused rule).
pub(super) fn build(b: &SandboxBinding, p: &Policy) -> Result<Ruleset, BuildError> {
    #[cfg(test)]
    if b.build_fails {
        let path = p.rules().last().map(|r| r.path().to_path_buf()).unwrap_or_default();
        return Err(BuildError::Rule { path, error: io::Error::from_raw_os_error(libc::EINVAL) });
    }
    let _ = b;
    p.to_ruleset()
}

impl Prepared {
    pub(super) fn new(ruleset: Ruleset, session: &str, tool: &'static str, p: &Policy, b: &SandboxBinding) -> Self {
        let canonical = |q: &Path| std::fs::canonicalize(q).unwrap_or_else(|_| q.to_path_buf());
        let tag = Tag::next();
        let (home, base) = (canonical(&b.env.home), canonical(&b.env.base_dir));
        let git_common = rule_parents(p, "git_common");
        let audit = b.audit.as_ref().map(|r| {
            r.registry().register(
                &tag,
                super::audit::SpawnInfo {
                    session: session.to_string(),
                    tool,
                    quiet: p.audit_log().is_none(),
                    home: home.clone(),
                    base: base.clone(),
                    git_common: git_common.clone(),
                    cargo_home: rule_parents(p, "cargo_home"),
                },
            )
        });
        let ctx = SpawnCtx {
            tag,
            session: session.to_string(),
            tool,
            contact: p.contact().to_string(),
            rules: p.rules().iter().map(|r| (r.path().to_path_buf(), r.is_dir())).collect(),
            allowed: p.rules().iter().filter(|r| r.class() != RuleClass::Baseline).map(|r| r.path().to_path_buf()).collect(),
            work_dir: p.rules().iter().find(|r| r.class() == RuleClass::Work && r.is_dir()).map(|r| r.path().to_path_buf()),
            git_common,
            home,
            base,
            audit,
        };
        Prepared { ruleset, ctx }
    }

    /// The spawn's identity and hint context.
    pub fn ctx(&self) -> &SpawnCtx {
        &self.ctx
    }

    pub fn tag(&self) -> &Tag {
        &self.ctx.tag
    }

    /// Restrict `cmd`'s child before exec (a `pre_exec` hook after any
    /// already registered). If the restrict fails, the spawn fails and
    /// nothing runs.
    pub fn install(&self, cmd: &mut std::process::Command) -> io::Result<()> {
        landlock::confine_command(cmd, &self.ruleset, self.ctx.tag.clone())
    }

    /// Run `f` (which spawns the child) on a fresh thread restricted by
    /// this domain.
    pub fn run_confined<T: Send, F: FnOnce() -> T + Send>(&self, f: F) -> io::Result<T> {
        landlock::run_confined(&self.ruleset, &self.ctx.tag, f)
    }

    pub fn spawned_line(&self, pid: Option<u32>) -> String {
        self.ctx.spawned_line(pid)
    }

    /// The child exists: give the audit registry its pid (a bash child's
    /// domain names the child) and log the `sandbox.spawn` line.
    pub fn log_spawned(&self, pid: Option<u32>) {
        if let (Some(r), Some(pid)) = (&self.ctx.audit, pid) {
            r.set_pid(pid);
        }
        super::log_line(&self.spawned_line(pid));
    }

    pub fn spawn_failed_line(&self, error: &dyn std::fmt::Display) -> String {
        self.ctx.spawn_failed_line(error)
    }

    pub fn bash_script(&self, command: &str) -> String {
        self.ctx.bash_script(command)
    }

    /// [`SpawnCtx::annotate`].
    pub fn annotate(&self, command: &str, output: &str) -> Option<Annotation> {
        self.ctx.annotate(command, output)
    }

    /// [`SpawnCtx::command_in`].
    pub fn command_in<'a>(&self, script: &'a str) -> &'a str {
        self.ctx.command_in(script)
    }
}

impl SpawnCtx {
    /// `sandbox.spawn …`: which session and tool a tag (and a child pid)
    /// belongs to, for anyone reading the log.
    pub fn spawned_line(&self, pid: Option<u32>) -> String {
        let pid = pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into());
        format!("sandbox.spawn session={} tool={} tag={} pid={pid}", self.session, self.tool, self.tag)
    }

    /// `sandbox.spawn_failed …`: the restricted spawn failed (for example
    /// the restrict itself); nothing ran.
    pub fn spawn_failed_line(&self, error: &dyn std::fmt::Display) -> String {
        format!("sandbox.spawn_failed session={} tool={} tag={} error={error}", self.session, self.tool, self.tag)
    }

    /// The bash script for a sandboxed command: `sudo` is a function that
    /// refuses at once (setuid can't work under no_new_privs), instead of
    /// the unsandboxed prelude's password-prompt function.
    pub fn bash_script(&self, command: &str) -> String {
        let msg = format!("{SUDO_REFUSED} {}", self.contact);
        format!("sudo() {{\n    printf '%s\\n' {} >&2\n    return 1\n}}\n{command}", sh_single_quote(&msg))
    }

    /// The command in a script that [`SpawnCtx::bash_script`] made: the
    /// script without the sudo prelude (the whole script if it has none).
    pub fn command_in<'a>(&self, script: &'a str) -> &'a str {
        script.strip_prefix(self.bash_script("").as_str()).unwrap_or(script)
    }

    /// The hint and log lines for the output of a sandboxed `command` (a
    /// bash command, a PTY shell's command or input), if it shows a refused
    /// sudo or a refused write outside the rules. A refused sudo counts only
    /// when `command` runs sudo ([`invokes_sudo`]): output that merely quotes
    /// a refusal (reading a doc or this crate's source) is not one.
    pub fn annotate(&self, command: &str, output: &str) -> Option<Annotation> {
        let who = format!("session={} tool={} tag={}", self.session, self.tool, self.tag);
        let mut lines = Vec::new();
        let mut hints = Vec::new();
        if invokes_sudo(command) {
            let real_sudo = NNP_SUDO_TEXTS.iter().any(|t| output.contains(t));
            if real_sudo || output.contains(SUDO_REFUSED) {
                lines.push(format!("sandbox.sudo_refused {who}"));
                if real_sudo && !output.contains(SUDO_REFUSED) {
                    hints.push(format!("{SUDO_REFUSED} {}", self.contact));
                }
            }
        }
        let mut outside: Vec<(PathBuf, String)> = Vec::new();
        let mut unattributed = false;
        for text in denial_texts(output) {
            let line = text.as_str();
            let paths = abs_paths(line);
            if paths.is_empty() {
                unattributed |= names_a_write_tool(line);
                continue;
            }
            for path in paths {
                if is_proc_magic_link(&path)
                    || is_proc_dotdot(&path)
                    || self.covers(&path)
                    || !daemon_could_write(&path)
                    || outside.iter().any(|(q, _)| *q == path)
                {
                    continue;
                }
                let class = if self.is_packed_refs_lock(&path) {
                    PACKED_REFS_CLASS.to_string()
                } else {
                    safe_class(classify(&path, line, &self.home, &self.base))
                };
                outside.push((path, class));
            }
        }
        for (path, class) in &outside {
            lines.push(format!("sandbox.denial_suspected {who} class={class} path={} src=output", log_path(path)));
        }
        if outside.iter().any(|(_, c)| c == "worktree_new") {
            let dir = self.work_dir.as_ref().map(|d| d.display().to_string()).unwrap_or_else(|| "<a write dir>".into());
            hints.push(format!(
                "[sandbox] new worktrees go under a directory this session may write, e.g. `git worktree add {dir}/wt-<name> <base>`; \
                 another path must first be added to this session's policy. {}",
                self.contact
            ));
        }
        let packed: Vec<&Path> = outside.iter().filter(|(_, c)| c == PACKED_REFS_CLASS).map(|(p, _)| p.as_path()).collect();
        if !packed.is_empty() {
            hints.push(format!(
                "[sandbox] git may not create {} (by design: the repository's common dir stays read-only). \
                 The exit code tells: a commit or `git worktree add` that prints this still succeeds; \
                 deleting a ref (`branch -D`, `tag -d`, `update-ref -d`, `fetch --prune`) fails. {} Do not work around it.",
                list(&packed),
                self.contact
            ));
        }
        let others: Vec<&Path> = outside
            .iter()
            .filter(|(_, c)| c != "worktree_new" && c != PACKED_REFS_CLASS)
            .map(|(p, _)| p.as_path())
            .collect();
        if !others.is_empty() {
            hints.push(format!(
                "[sandbox] writes outside this session's write set are refused: {}. Allowed: {}. {} Do not work around it.",
                list(&others),
                self.allowed_list(),
                self.contact
            ));
        } else if outside.is_empty() && unattributed {
            lines.push(format!("sandbox.denial_suspected {who} class=unknown path=- src=output"));
            hints.push(format!(
                "[sandbox] this session may write only: {}. A \"Permission denied\" for a write elsewhere comes from the sandbox. {} Do not work around it.",
                self.allowed_list(),
                self.contact
            ));
        }
        if lines.is_empty() && hints.is_empty() {
            None
        } else {
            Some(Annotation { hint: hints.join("\n"), lines })
        }
    }

    /// Whether a rule covers `path` (a directory rule: the path or anything
    /// beneath it; a file rule: that path).
    fn covers(&self, path: &Path) -> bool {
        self.rules.iter().any(|(r, dir)| if *dir { path.starts_with(r) } else { path == r })
    }

    /// `<git_common>/packed-refs.lock` of one of the policy's `git_common`
    /// dirs (as named, or by its directory's canonical path).
    fn is_packed_refs_lock(&self, path: &Path) -> bool {
        if path.file_name() != Some(std::ffi::OsStr::new("packed-refs.lock")) {
            return false;
        }
        let Some(dir) = path.parent() else { return false };
        let canonical = std::fs::canonicalize(dir).ok();
        self.git_common.iter().any(|g| g == dir || canonical.as_deref() == Some(g.as_path()))
    }

    fn allowed_list(&self) -> String {
        let paths: Vec<&Path> = self.allowed.iter().map(PathBuf::as_path).collect();
        let mut s = list(&paths);
        if !s.is_empty() {
            s.push_str(", ");
        }
        s.push_str("/tmp");
        s
    }
}

/// `a, b, c (+N more)`, each path as [`log_path`] shows it.
fn list(paths: &[&Path]) -> String {
    let mut s = paths.iter().take(HINT_MAX_PATHS).map(|p| log_path(p)).collect::<Vec<_>>().join(", ");
    if paths.len() > HINT_MAX_PATHS {
        s.push_str(&format!(" (+{} more)", paths.len() - HINT_MAX_PATHS));
    }
    s
}

/// The most bytes a path takes as a log field or in a hint (finding 12).
const PATH_FIELD_MAX: usize = 1024;

/// `path` as one log field (and as a hint shows it): [`render_path`]'s
/// escaping (as is when every byte is visible ASCII other than `"` and `\`,
/// else quoted with every other byte as `\xNN`), at most [`PATH_FIELD_MAX`]
/// bytes. A longer one keeps the bytes that fit, cut at a whole escape,
/// quoted, then `...` after the closing quote; an uncut field never ends in
/// `"...`.
fn log_path(path: &Path) -> String {
    let p = path.as_os_str().as_bytes();
    let whole = render_path(p);
    if whole.len() <= PATH_FIELD_MAX {
        return whole;
    }
    let width = |b: u8| match b {
        b'"' | b'\\' => 2,
        0x21..=0x7e => 1,
        _ => 4,
    };
    let room = PATH_FIELD_MAX - "\"\"...".len();
    let (mut used, mut n) = (0, 0);
    for &b in p {
        used += width(b);
        if used > room {
            break;
        }
        n += 1;
    }
    let head = render_path(&p[..n]);
    if head.starts_with('"') {
        format!("{head}...")
    } else {
        format!("\"{head}\"...")
    }
}

/// `s` as one single-quoted shell word.
pub fn sh_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The words of a denial message (EACCES, EXDEV), compared case-blind.
const DENIAL_WORDS: &[&str] = &["permission denied", "invalid cross-device link"];

/// A line reporting EACCES or EXDEV (what Landlock returns): `Permission
/// denied` or `Invalid cross-device link` in any case (C's strerror text is
/// capitalised; Go's and other runtimes' errors are lowercase, as in
/// `open <path>: permission denied`), or the errno name itself.
fn is_denial_line(line: &str) -> bool {
    DENIAL_WORDS.iter().any(|t| contains_ignore_ascii_case(line, t)) || ["EACCES", "EXDEV"].iter().any(|t| line.contains(t))
}

/// Whether `hay` contains `needle`, ASCII letters compared case-blind.
fn contains_ignore_ascii_case(hay: &str, needle: &str) -> bool {
    let (h, n) = (hay.as_bytes(), needle.as_bytes());
    h.len() >= n.len() && h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
}

/// Whether `line` ends in an absolute path followed by `:` (`open /a/b:`).
fn ends_in_a_path_and_colon(line: &str) -> bool {
    line.split_whitespace()
        .next_back()
        .and_then(|w| w.strip_suffix(':'))
        .is_some_and(|p| p.len() > 1 && p.starts_with('/'))
}

/// Whether the byte after a phrase (`None`: the end of the text) ends a
/// word there: anything but an ASCII letter, digit or `_`.
fn at_word_end(next: Option<&u8>) -> bool {
    !next.is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
}

/// Whether `text` holds a denial's whole words (any case) ending at a word
/// end (`permission denied.`, not `permission deniedness`).
fn has_denial_words(text: &str) -> bool {
    let t = text.as_bytes();
    DENIAL_WORDS.iter().any(|w| {
        let n = w.len();
        t.windows(n).enumerate().any(|(i, win)| win.eq_ignore_ascii_case(w.as_bytes()) && at_word_end(t.get(i + n)))
    })
}

/// Whether `line` starts, after its indent, with a denial's whole words
/// (any case) ending at a word end.
fn starts_with_denial_words(line: &str) -> bool {
    let l = line.trim_start().as_bytes();
    DENIAL_WORDS
        .iter()
        .any(|w| l.len() >= w.len() && l[..w.len()].eq_ignore_ascii_case(w.as_bytes()) && at_word_end(l.get(w.len())))
}

/// The texts in `output` that report a denial: each line that does, and a
/// line joined to the next (`a.trim_end() + " " + b.trim_start()`) in the
/// two shapes of a soft-wrapped message:
/// - the denial's words straddle the break (`…: permission` then
///   `  denied.`): neither line reports a denial alone, the join holds the
///   whole words at a word end;
/// - the break falls after the path's colon (`… open /a/b:` then
///   `  permission denied.`): the line ends in an absolute path and `:`,
///   and the next starts with the whole words at a word end.
///
/// Otherwise lines are never joined, so a path on one line isn't paired
/// with some other denial on the next. A join consumes the next line.
fn denial_texts(output: &str) -> Vec<String> {
    let lines: Vec<&str> = output.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if is_denial_line(lines[i]) {
            out.push(lines[i].to_string());
        } else if let Some(next) = lines.get(i + 1) {
            let joined = format!("{} {}", lines[i].trim_end(), next.trim_start());
            let straddle = !is_denial_line(next) && has_denial_words(&joined);
            let after_path = ends_in_a_path_and_colon(lines[i]) && starts_with_denial_words(next);
            if straddle || after_path {
                out.push(joined);
                i += 1;
            }
        }
        i += 1;
    }
    out
}

/// Whether `path` is, by its text, one of /proc's magic links: a process's
/// (or a thread's, under `task/<tid>`) `cwd`, `exe`, `root`, `fd/<n>` or
/// `map_files/<x>`, for `<pid>`, `self` or `thread-self`. Following one
/// reaches whatever that process has open, so a refused readlink or open of
/// it says nothing about a write of this session's.
fn is_proc_magic_link(path: &Path) -> bool {
    use std::path::Component;
    let mut parts = path.components();
    if parts.next() != Some(Component::RootDir) || parts.next() != Some(Component::Normal(std::ffi::OsStr::new("proc"))) {
        return false;
    }
    let Some(rest) = parts
        .map(|c| match c {
            Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect::<Option<Vec<&str>>>()
    else {
        return false;
    };
    let tail = match rest.as_slice() {
        [p, "task", t, tail @ ..] if (all_digits(p) || *p == "self") && all_digits(t) => tail,
        [p, tail @ ..] if all_digits(p) || *p == "self" || *p == "thread-self" => tail,
        _ => return false,
    };
    match tail {
        ["cwd"] | ["exe"] | ["root"] | ["map_files", _] => true,
        ["fd", n] => all_digits(n),
        _ => false,
    }
}

/// Whether `path` has a `..` component and is under /proc by its text
/// (`/proc/1/../1/cwd`, `/proc/1/cwd/../x`), or once each `..` drops the
/// part before it (`/../proc/1/cwd`). Such a path can spell a magic link,
/// and a `..` after a link climbs from where the link points, so it is
/// skipped whole: never resolved to find out.
fn is_proc_dotdot(path: &Path) -> bool {
    use std::path::Component;
    if !path.has_root() || !path.components().any(|c| c == Component::ParentDir) {
        return false;
    }
    let proc = Component::Normal(std::ffi::OsStr::new("proc"));
    if path.components().nth(1) == Some(proc) {
        return true;
    }
    let mut kept: Vec<Component> = Vec::new();
    for c in path.components() {
        match c {
            Component::Normal(_) => kept.push(c),
            Component::ParentDir => {
                kept.pop();
            }
            _ => {}
        }
    }
    kept.first() == Some(&proc)
}

/// A nonempty run of ASCII digits.
fn all_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// A denial line from a file tool that names no path (uutils `mkdir`/`ln`).
fn names_a_write_tool(line: &str) -> bool {
    const TOOLS: &[&str] =
        &["mkdir:", "ln:", "rm:", "rmdir:", "touch:", "cp:", "mv:", "install:", "tee:", "truncate:", "mkfifo:", "mknod:"];
    let l = line.trim_start();
    TOOLS.iter().any(|t| l.starts_with(t))
}

/// Whether `c` ends a path taken from output: whitespace, a backtick or a
/// quote. A quoted span of prose that starts with `/` (a printed brief)
/// then gives its first word, never the whole span.
fn ends_a_path(c: char) -> bool {
    c.is_whitespace() || matches!(c, '`' | '\'' | '"' | '‘' | '’')
}

/// Absolute paths named in a line: quoted ('…', "…", ‘…’, `…') or bare
/// words starting with `/`, each cut at the first whitespace, backtick or
/// quote ([`ends_a_path`]), then trailing `:`/`,`/`)`/`.`/`;` removed.
pub fn abs_paths(line: &str) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut push = |s: &str| {
        let s = s.split(ends_a_path).next().unwrap_or_default();
        let s = s.trim_end_matches([':', ',', ')', '.', ';']);
        if s.starts_with('/') && s.len() > 1 && !out.iter().any(|p| p.as_os_str() == s) {
            out.push(PathBuf::from(s));
        }
    };
    for (open, close) in [('\'', '\''), ('"', '"'), ('‘', '’'), ('`', '\'')] {
        let mut rest = line;
        while let Some(i) = rest.find(open) {
            let after = &rest[i + open.len_utf8()..];
            let Some(j) = after.find(close) else { break };
            push(&after[..j]);
            rest = &after[j + close.len_utf8()..];
        }
    }
    for word in line.split_whitespace() {
        if word.starts_with('/') {
            push(word);
        }
    }
    out
}

/// Whether this process could write where `path` points without the
/// sandbox (DAC): `path` itself if it exists, else its nearest existing
/// ancestor. A denial the daemon would get too is not the sandbox's.
fn daemon_could_write(path: &Path) -> bool {
    let writable = |p: &Path| {
        CString::new(p.as_os_str().as_bytes())
            // SAFETY: a valid NUL-terminated path; access(2) only reads it.
            .map(|c| unsafe { libc::access(c.as_ptr(), libc::W_OK) } == 0)
            .unwrap_or(false)
    };
    if path.symlink_metadata().is_ok() && writable(path) {
        return true;
    }
    let mut dir = path.parent();
    while let Some(d) = dir {
        if d.symlink_metadata().is_ok() {
            return writable(d);
        }
        dir = d.parent();
    }
    false
}

/// The parent dirs of the policy's rules from `key` lines (a `git_common` or
/// `cargo_home` line expands to rules inside the dir it names), each once.
fn rule_parents(p: &Policy, key: &str) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for d in p.rules().iter().filter(|r| r.key() == key).filter_map(|r| r.path().parent()) {
        if !dirs.iter().any(|q| q == d) {
            dirs.push(d.to_path_buf());
        }
    }
    dirs
}

/// The denial class of a path outside the rules (the stage log counts
/// `worktree_new` separately).
pub fn classify(path: &Path, line: &str, home: &Path, base: &Path) -> String {
    let s = path.to_string_lossy();
    if s.contains("/.worktrees/") || line.contains("could not create leading directories") {
        return "worktree_new".into();
    }
    if path.starts_with(base) {
        return "synaps_state".into();
    }
    if let Ok(rel) = path.strip_prefix(home) {
        return match rel.components().next() {
            Some(c) if c.as_os_str().as_bytes().starts_with(b".") => {
                format!("home_dot:{}", c.as_os_str().to_string_lossy())
            }
            _ => "home_other".into(),
        };
    }
    if path.starts_with("/dev") || path.starts_with("/proc") {
        return "dev_proc".into();
    }
    if ["/etc", "/usr", "/var", "/opt", "/boot", "/srv", "/bin", "/sbin", "/lib"].iter().any(|d| path.starts_with(d)) {
        return "system".into();
    }
    "other".into()
}

#[cfg(test)]
mod tests;
