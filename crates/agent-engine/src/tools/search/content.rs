//! The grep engine: walk the tree, search each regular file with
//! `grep-searcher`, and print what GNU `grep -rn` printed:
//!
//! - `path:line:text` for a match, `path-line-text` for a context line;
//! - no `path` prefix when the search path is a file;
//! - with `context` (even 0), `--` between groups of lines that aren't
//!   adjacent, within a file and between files;
//! - each line ends in `\n`; a `\r` before it is dropped.
//!
//! Lines longer than [`MAX_COLUMNS`] characters are cut with a marker, and a
//! line over [`LINE_HEAP_LIMIT`] bytes makes that file an error. A file with a
//! NUL byte is binary: its lines are not shown, and a note lists binary files
//! that matched. Output stops at the byte cap, on a whole line.

use std::fmt::Write as _;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

use grep_matcher::LineTerminator;
use grep_regex::RegexMatcher;
use grep_searcher::{
    BinaryDetection, MmapChoice, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch,
};
use ignore::types::Types;

use super::{
    append_note, stop_note, truncation_footer, CancelReader, Control, Stop, Tally, WalkOptions,
};
use crate::RuntimeError;

/// Most memory one line may take; a longer line is a per-file error.
pub(crate) const LINE_HEAP_LIMIT: usize = 8 << 20;

/// Characters shown of one output line.
pub(crate) const MAX_COLUMNS: usize = 500;

/// Binary files named in the note; the rest are counted.
const BINARY_LISTED: usize = 3;

/// One grep call, resolved.
pub(crate) struct ContentRequest {
    pub(crate) root: PathBuf,
    pub(crate) matcher: RegexMatcher,
    pub(crate) include: Option<Types>,
    /// `Some` turns on `--` separators, as `-C` did.
    pub(crate) context: Option<usize>,
    pub(crate) no_ignore: bool,
    pub(crate) cwd: Option<PathBuf>,
    /// Output byte cap (`max_tool_buffer`).
    pub(crate) cap: usize,
}

/// What a search found.
#[derive(Debug, Default)]
pub(crate) struct ContentOutcome {
    /// Output lines, each ending in `\n`.
    pub(crate) text: String,
    /// The cap was reached and the search stopped.
    pub(crate) truncated: bool,
    pub(crate) stop: Option<Stop>,
    /// Files read to the end (or to a binary match).
    pub(crate) searched: usize,
    pub(crate) binary: Vec<String>,
    pub(crate) binary_count: usize,
    pub(crate) errors: Tally,
}

/// Searches `req.root`, opening each regular file with `open`. Errors only
/// when the root can't be read at all; per-file problems go in the tally.
pub(crate) fn search<R: Read>(
    req: &ContentRequest,
    control: &Control,
    mut open: impl FnMut(&Path) -> io::Result<R>,
) -> Result<ContentOutcome, String> {
    if let Err(err) = std::fs::metadata(&req.root) {
        return Err(match err.kind() {
            io::ErrorKind::NotFound => format!("Path not found: {}", req.root.display()),
            _ => format!("{}: {err}", req.root.display()),
        });
    }
    let mut searcher = build_searcher(req.context.unwrap_or(0));
    let separators = req.context.is_some();
    let opts = WalkOptions {
        no_ignore: req.no_ignore,
        cwd: req.cwd.clone(),
        types: req.include.clone(),
    };
    let mut out = ContentOutcome::default();
    for item in super::walk(&req.root, &opts) {
        if let Some(stop) = control.stop() {
            out.stop = Some(stop);
            break;
        }
        let entry = match item {
            Ok(entry) => entry,
            Err(err) => {
                out.errors.add(err);
                continue;
            }
        };
        if let Some(err) = entry.error() {
            out.errors.add(err);
        }
        let Some(file_type) = entry.file_type() else {
            continue;
        };
        // Regular files only: links in the tree aren't followed (as with
        // `grep -r`), and FIFOs, sockets and devices are never opened. An
        // explicit path that is none of these is an error. (A root link to
        // a directory reports itself as a link but is walked.)
        if !file_type.is_file() {
            if entry.depth() == 0 && !file_type.is_dir() && !entry.path().is_dir() {
                out.errors
                    .add(format!("{}: not a regular file", entry.path().display()));
            }
            continue;
        }
        let shown = entry.path().to_string_lossy();
        let reader = match open(entry.path()) {
            Ok(reader) => reader,
            Err(err) => {
                out.errors.add(format!("{shown}: {err}"));
                continue;
            }
        };
        let mut sink = FileSink {
            prefix: (entry.depth() > 0).then_some(&*shown),
            separators,
            budget: req.cap.saturating_sub(out.text.len()),
            lines: String::new(),
            last_line: None,
            binary: false,
            matched: false,
        };
        let result =
            searcher.search_reader(&req.matcher, CancelReader::new(reader, control), &mut sink);
        match result {
            Ok(()) => out.searched += 1,
            Err(err) => match control.stop() {
                Some(stop) => out.stop = Some(stop),
                None => out.errors.add(read_error(&shown, &err)),
            },
        }
        // A file is binary once a NUL turns up, which may be after some of
        // its lines matched: only now is it known whether they can be shown.
        if sink.binary {
            if sink.matched {
                out.binary_count += 1;
                if out.binary.len() < BINARY_LISTED {
                    out.binary.push(shown.to_string());
                }
            }
        } else if !sink.lines.is_empty() {
            commit(&mut out, &sink.lines, separators, req.cap);
        }
        if out.truncated || out.stop.is_some() {
            break;
        }
    }
    Ok(out)
}

fn build_searcher(context: usize) -> Searcher {
    SearcherBuilder::new()
        .line_terminator(LineTerminator::crlf())
        .line_number(true)
        .before_context(context)
        .after_context(context)
        .binary_detection(BinaryDetection::convert(b'\x00'))
        .heap_limit(Some(LINE_HEAP_LIMIT))
        .memory_map(MmapChoice::never())
        .build()
}

fn read_error(shown: &str, err: &io::Error) -> String {
    let msg = err.to_string();
    if msg.contains("allocation limit") {
        format!(
            "{shown}: a line is longer than {} MiB",
            LINE_HEAP_LIMIT >> 20
        )
    } else {
        format!("{shown}: {msg}")
    }
}

/// Adds one file's lines to the output, or as many whole lines as fit
/// under `cap`.
fn commit(out: &mut ContentOutcome, lines: &str, separators: bool, cap: usize) {
    let sep = if separators && !out.text.is_empty() {
        "--\n"
    } else {
        ""
    };
    let room = cap.saturating_sub(out.text.len());
    if sep.len() + lines.len() <= room {
        out.text.push_str(sep);
        out.text.push_str(lines);
        return;
    }
    out.truncated = true;
    let chunk = format!("{sep}{lines}");
    let keep = chunk.as_bytes()[..room]
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(0, |i| i + 1);
    if keep > sep.len() {
        out.text.push_str(&chunk[..keep]);
    }
}

/// Collects one file's output lines.
struct FileSink<'p> {
    prefix: Option<&'p str>,
    separators: bool,
    /// Output room left; the file stops once its lines pass it.
    budget: usize,
    lines: String,
    last_line: Option<u64>,
    binary: bool,
    matched: bool,
}

impl FileSink<'_> {
    /// Adds `bytes` (one or more lines from `first`) with `kind` (`:` or
    /// `-`) after the path and line number. False once over budget.
    fn push(&mut self, bytes: &[u8], first: Option<u64>, kind: char) -> bool {
        let numbered = (first.unwrap_or(0)..).zip(bytes.split_inclusive(|&b| b == b'\n'));
        for (number, line) in numbered {
            if self.separators && self.last_line.is_some_and(|last| number != last + 1) {
                self.lines.push_str("--\n");
            }
            self.last_line = Some(number);
            if let Some(prefix) = self.prefix {
                self.lines.push_str(prefix);
                self.lines.push(kind);
            }
            let _ = write!(self.lines, "{number}{kind}");
            push_text(&mut self.lines, line);
            self.lines.push('\n');
        }
        self.lines.len() <= self.budget
    }
}

impl Sink for FileSink<'_> {
    type Error = io::Error;

    fn matched(&mut self, _: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, io::Error> {
        self.matched = true;
        if self.binary {
            // A binary match is all the note needs.
            return Ok(false);
        }
        Ok(self.push(mat.bytes(), mat.line_number(), ':'))
    }

    fn context(&mut self, _: &Searcher, ctx: &SinkContext<'_>) -> Result<bool, io::Error> {
        if self.binary {
            return Ok(true);
        }
        Ok(self.push(ctx.bytes(), ctx.line_number(), '-'))
    }

    fn binary_data(&mut self, _: &Searcher, _offset: u64) -> Result<bool, io::Error> {
        self.binary = true;
        // Keep looking for a match unless one was already seen.
        Ok(!self.matched)
    }
}

/// One line's text without its line ending, cut to [`MAX_COLUMNS`]
/// characters. Invalid UTF-8 is replaced.
fn push_text(out: &mut String, line: &[u8]) {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    if line.len() <= MAX_COLUMNS {
        out.push_str(&String::from_utf8_lossy(line));
        return;
    }
    // MAX_COLUMNS characters take at most 4 bytes each.
    let head = &line[..line.len().min(MAX_COLUMNS * 4)];
    let text = String::from_utf8_lossy(head);
    let cut = text.char_indices().nth(MAX_COLUMNS).map(|(i, _)| i);
    if cut.is_none() && head.len() == line.len() {
        out.push_str(&text);
        return;
    }
    out.push_str(&text[..cut.unwrap_or(text.len())]);
    let _ = write!(out, " [... line truncated, {} bytes]", line.len());
}

/// The tool result. No output and no binary match is an error when the
/// search stopped early or nothing could be read, else "No matches found.".
pub(crate) fn render(out: ContentOutcome, cap: usize, timeout: Duration) -> crate::Result<String> {
    if out.text.is_empty() && out.binary_count == 0 && !out.truncated {
        match out.stop {
            Some(Stop::TimedOut) => {
                return Err(RuntimeError::Tool(format!(
                    "Grep timed out after {}s before finding a match. Narrow the path or add \
                     an include filter.",
                    timeout.as_secs()
                )))
            }
            Some(Stop::Cancelled) => {
                return Err(RuntimeError::Tool(
                    "Grep was cancelled before finding a match.".to_string(),
                ))
            }
            None => {}
        }
        if out.searched == 0 && out.errors.count > 0 {
            return Err(RuntimeError::Tool(out.errors.failure()));
        }
        let mut text = "No matches found.".to_string();
        if let Some(note) = out.errors.note() {
            append_note(&mut text, &note);
        }
        return Ok(text);
    }
    let mut text = out.text;
    if out.truncated {
        append_note(&mut text, &truncation_footer(cap));
    }
    if let Some(stop) = out.stop {
        append_note(&mut text, &stop_note("grep", stop, timeout));
    }
    if let Some(note) = binary_note(&out.binary, out.binary_count) {
        append_note(&mut text, &note);
    }
    if let Some(note) = out.errors.note() {
        append_note(&mut text, &note);
    }
    Ok(text)
}

fn binary_note(listed: &[String], count: usize) -> Option<String> {
    if count == 0 {
        return None;
    }
    let mut list = listed.join(", ");
    let more = count.saturating_sub(listed.len());
    if more > 0 {
        let _ = write!(list, " and {more} more");
    }
    Some(if count == 1 {
        format!("[binary file matches, lines not shown: {list}]")
    } else {
        format!("[{count} binary files match, lines not shown: {list}]")
    })
}

#[cfg(test)]
mod tests {
    use super::super::{open_regular, pattern};
    use super::*;
    use std::fs;
    use std::time::Instant;

    const LONG: Duration = Duration::from_secs(60);

    fn write(root: &Path, rel: &str, body: impl AsRef<[u8]>) {
        let path = native(root, rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    /// `root` joined with a `/`-separated relative path, component by
    /// component, as the walk builds paths (native separators on Windows).
    fn native(root: &Path, rel: &str) -> PathBuf {
        rel.split('/').fold(root.to_path_buf(), |p, c| p.join(c))
    }

    fn shown(root: &Path, rel: &str) -> String {
        native(root, rel).display().to_string()
    }

    fn request(root: &Path, pattern: &str) -> ContentRequest {
        ContentRequest {
            root: root.to_path_buf(),
            matcher: pattern::content_matcher(pattern).unwrap(),
            include: None,
            context: None,
            no_ignore: false,
            cwd: None,
            cap: 1 << 20,
        }
    }

    fn run(req: &ContentRequest) -> ContentOutcome {
        search(req, &Control::new(LONG), open_regular).unwrap()
    }

    /// The files (relative, `/`-separated) with a matching line, in output
    /// order.
    fn files(out: &ContentOutcome, root: &Path) -> Vec<String> {
        let root = root.display().to_string();
        let mut seen: Vec<String> = Vec::new();
        for line in out.text.lines() {
            let Some(rest) = line.strip_prefix(root.as_str()) else {
                continue;
            };
            let Some((file, _)) = rest.split_once(':') else {
                continue;
            };
            let file = file.trim_start_matches(['/', '\\']).replace('\\', "/");
            if seen.last() != Some(&file) {
                seen.push(file);
            }
        }
        seen
    }

    /// Runs `f` on a thread and fails (instead of hanging) if it takes more
    /// than 20 s.
    #[cfg(unix)]
    fn within<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(Duration::from_secs(20))
            .expect("search hung")
    }

    /// Byte-identical to GNU `grep -rn -C1` on the same tree (GNU's order is
    /// the file system's; ours is sorted). Matches and context lines, `--`
    /// between groups and between files.
    #[test]
    fn context_output_matches_gnu_grep() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "src/a.txt",
            "a1\na2\nhit one\na4\na5\na6\na7\nhit two\na9\n",
        );
        write(root, "src/b.txt", "b1\nhit b\nb3\n");
        write(root, "src/sub/c.rs", "hit sub\n");
        write(root, ".hidden/h.txt", "hit hidden\n");
        let mut req = request(root, "hit");
        req.context = Some(1);
        let (h, a, b, c) = (
            shown(root, ".hidden/h.txt"),
            shown(root, "src/a.txt"),
            shown(root, "src/b.txt"),
            shown(root, "src/sub/c.rs"),
        );
        let expected = format!(
            "{h}:1:hit hidden\n--\n\
             {a}-2-a2\n{a}:3:hit one\n{a}-4-a4\n--\n\
             {a}-7-a7\n{a}:8:hit two\n{a}-9-a9\n--\n\
             {b}-1-b1\n{b}:2:hit b\n{b}-3-b3\n--\n\
             {c}:1:hit sub\n"
        );
        assert_eq!(run(&req).text, expected);

        // No context: no separators.
        req.context = None;
        let expected =
            format!("{h}:1:hit hidden\n{a}:3:hit one\n{a}:8:hit two\n{b}:2:hit b\n{c}:1:hit sub\n");
        assert_eq!(run(&req).text, expected);

        // -C0 as GNU: separators between groups, adjacent lines are one group.
        write(root, "src/b.txt", "hit\nhit\nx\nhit\n");
        let mut req = request(&native(root, "src/b.txt"), "hit");
        req.context = Some(0);
        assert_eq!(run(&req).text, "1:hit\n2:hit\n--\n4:hit\n");
    }

    /// A file as the path prints `N:text` without the file name, as `grep -rn`
    /// did; context lines read `N-text`.
    #[test]
    fn file_root_has_no_path_prefix() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "a.txt",
            "a1\na2\nhit one\na4\na5\na6\na7\nhit two\na9\n",
        );
        let mut req = request(&dir.path().join("a.txt"), "hit");
        req.context = Some(1);
        assert_eq!(
            run(&req).text,
            "2-a2\n3:hit one\n4-a4\n--\n7-a7\n8:hit two\n9-a9\n"
        );
    }

    #[test]
    fn dialect_through_the_engine() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("d.txt");
        fs::write(
            &file,
            "alpha\nbeta\ngamma1\nxx\nx\nfoo bar\nfoobar\nColour\ncolor\n",
        )
        .unwrap();
        let lines = |pattern: &str| -> Vec<u64> {
            run(&request(&file, pattern))
                .text
                .lines()
                .map(|l| l.split(':').next().unwrap().parse().unwrap())
                .collect()
        };
        assert_eq!(lines("alpha|beta"), [1, 2]);
        assert_eq!(lines(r"\d"), [3]);
        assert_eq!(lines("^x+$"), [4, 5]);
        assert_eq!(lines("^x{2}$"), [4]);
        assert_eq!(lines(r"\bfoo\b"), [6]);
        assert_eq!(lines("fo+bar"), [7]);
        assert_eq!(lines("colou?r"), [9]);
        assert_eq!(lines("(?i)colou?r"), [8, 9]);
    }

    /// `.gitignore` (in a repo), `.ignore` and `.git/info/exclude` apply;
    /// `no_ignore` brings the files back.
    #[test]
    fn ignore_files_apply_and_no_ignore_lifts_them() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".git").join("info")).unwrap();
        write(root, ".git/info/exclude", "excluded.txt\n");
        write(root, ".gitignore", "ignored/\n*.log\n");
        write(root, ".ignore", "skipme.txt\n");
        for rel in [
            "ignored/a.txt",
            "x.log",
            "skipme.txt",
            "excluded.txt",
            "keep.txt",
        ] {
            write(root, rel, "hit\n");
        }
        let mut req = request(root, "hit");
        assert_eq!(files(&run(&req), root), ["keep.txt"]);
        req.no_ignore = true;
        assert_eq!(
            files(&run(&req), root),
            [
                "excluded.txt",
                "ignored/a.txt",
                "keep.txt",
                "skipme.txt",
                "x.log"
            ]
        );
    }

    /// node_modules and target are skipped (no repo needed) unless
    /// `no_ignore`; VCS directories always are.
    #[test]
    fn hard_excludes_and_vcs_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for rel in [
            "node_modules/m.txt",
            "target/t.txt",
            "deep/target/d.txt",
            "src/a.txt",
            ".git/config",
            ".hg/x",
            ".svn/x",
            ".bzr/x",
            ".jj/x",
            ".sl/x",
        ] {
            write(root, rel, "hit\n");
        }
        let mut req = request(root, "hit");
        assert_eq!(files(&run(&req), root), ["src/a.txt"]);
        req.no_ignore = true;
        assert_eq!(
            files(&run(&req), root),
            [
                "deep/target/d.txt",
                "node_modules/m.txt",
                "src/a.txt",
                "target/t.txt"
            ]
        );
        // An explicit path into an excluded directory is searched.
        let req = request(&root.join("node_modules"), "hit");
        assert_eq!(files(&run(&req), &root.join("node_modules")), ["m.txt"]);
    }

    #[test]
    fn hidden_files_are_searched() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, ".github/ci.yml", "hit\n");
        write(root, ".hidden.txt", "hit\n");
        write(root, "plain.txt", "hit\n");
        assert_eq!(
            files(&run(&request(root, "hit")), root),
            [".github/ci.yml", ".hidden.txt", "plain.txt"]
        );
    }

    /// Binary files aren't shown, even when the NUL comes after matching
    /// lines; the ones that match are counted in a note.
    #[test]
    fn binary_files_are_skipped_and_counted() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "a.bin", b"hit\x00\x01\x02 hit\n");
        let mut late = b"hit late\n".to_vec();
        late.extend(b"filler\n".repeat(20_000)); // past the first 64 KiB read
        late.extend(b"\x00tail\n");
        write(root, "b.bin", late);
        write(root, "c.bin", b"no match\x00here\n");
        write(root, "d.txt", "hit text\n");
        let out = run(&request(root, "hit"));
        assert_eq!(out.text, format!("{}:1:hit text\n", shown(root, "d.txt")));
        assert_eq!(out.binary_count, 2);
        assert_eq!(out.binary, [shown(root, "a.bin"), shown(root, "b.bin")]);
        let text = render(out, 1 << 20, LONG).unwrap();
        assert!(
            text.ends_with(&format!(
                "\n\n[2 binary files match, lines not shown: {}, {}]",
                shown(root, "a.bin"),
                shown(root, "b.bin")
            )),
            "{text}"
        );
        // Binary-only matches are a result, not "No matches found.".
        let out = run(&request(&root.join("a.bin"), "hit"));
        assert_eq!(
            render(out, 1 << 20, LONG).unwrap(),
            format!(
                "[binary file matches, lines not shown: {}]",
                shown(root, "a.bin")
            )
        );
    }

    /// An unreadable file is counted in a note; the other files' matches are
    /// still returned.
    #[cfg(unix)]
    #[test]
    fn unreadable_file_is_a_note() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for rel in ["a.txt", "b.txt", "c.txt"] {
            write(root, rel, "hit\n");
        }
        let locked = root.join("b.txt");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::File::open(&locked).is_ok() {
            eprintln!("skipped: mode 000 is still readable here (root?)");
            return;
        }
        let out = run(&request(root, "hit"));
        assert_eq!(files(&out, root), ["a.txt", "c.txt"]);
        assert_eq!(out.errors.count, 1);
        let text = render(out, 1 << 20, LONG).unwrap();
        let note = format!(
            "[1 error while searching, results may be incomplete. First: {}: ",
            shown(root, "b.txt")
        );
        assert!(text.contains(&note), "{text}");
        // Nothing readable at all is a tool error.
        let out = run(&request(&locked, "hit"));
        assert!(render(out, 1 << 20, LONG).is_err());
    }

    #[test]
    fn missing_path_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        let err = search(&request(&missing, "x"), &Control::new(LONG), open_regular).unwrap_err();
        assert_eq!(err, format!("Path not found: {}", missing.display()));
    }

    #[test]
    fn no_match_is_exactly_no_matches_found() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.txt", "nothing\n");
        let out = run(&request(dir.path(), "hit"));
        assert_eq!(render(out, 1 << 20, LONG).unwrap(), "No matches found.");
    }

    /// `include` is a file-name glob (braces work); an explicit file is
    /// searched whatever its name.
    #[test]
    fn include_filters_file_names() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for rel in ["a.rs", "b.toml", "c.txt", "sub/d.rs"] {
            write(root, rel, "hit\n");
        }
        let mut req = request(root, "hit");
        req.include = Some(pattern::include_filter("*.rs").unwrap());
        assert_eq!(files(&run(&req), root), ["a.rs", "sub/d.rs"]);
        req.include = Some(pattern::include_filter("*.{rs,toml}").unwrap());
        assert_eq!(files(&run(&req), root), ["a.rs", "b.toml", "sub/d.rs"]);
        let mut req = request(&root.join("c.txt"), "hit");
        req.include = Some(pattern::include_filter("*.rs").unwrap());
        assert_eq!(run(&req).text, "1:hit\n");
    }

    /// Output order is by path, whatever the creation order.
    #[test]
    fn order_is_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for rel in ["z.txt", "m/b.txt", "a.txt", "B.txt", "m/a.txt"] {
            write(root, rel, "hit\n");
        }
        let req = request(root, "hit");
        let first = run(&req).text;
        assert_eq!(run(&req).text, first);
        assert_eq!(
            files(&run(&req), root),
            ["B.txt", "a.txt", "m/a.txt", "m/b.txt", "z.txt"]
        );
    }

    /// At the cap the search stops: whole lines, a prefix of the full output,
    /// and the truncation footer.
    #[test]
    fn output_cap_stops_early() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for i in 0..50 {
            write(root, &format!("f{i:02}.txt"), "hit\nhit\n");
        }
        let mut req = request(root, "hit");
        let full = run(&req).text;
        req.cap = 200;
        let out = run(&req);
        assert!(out.truncated);
        assert!(out.searched < 50, "searched {} files", out.searched);
        assert!(out.text.len() <= 200 && out.text.ends_with('\n'));
        assert!(full.starts_with(&out.text));
        let text = render(out, 200, LONG).unwrap();
        assert!(
            text.ends_with("\n\n... (output truncated, more than 200 total bytes)"),
            "{text}"
        );
    }

    /// One file bigger than the cap stops mid-file.
    #[test]
    fn one_large_file_stops_at_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("big.txt");
        fs::write(&file, "hit\n".repeat(100_000)).unwrap();
        let mut req = request(&file, "hit");
        req.cap = 1000;
        let out = run(&req);
        assert!(out.truncated);
        assert!(out.text.len() <= 1000 && out.text.ends_with('\n'));
    }

    fn numbered_tree(root: &Path) {
        for i in 0..5 {
            write(root, &format!("f{i}.txt"), "hit\n");
        }
    }

    /// Cancelling mid-search keeps what was found and says so.
    #[test]
    fn cancel_returns_partial_results() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        numbered_tree(root);
        let control = Control::new(LONG);
        let canceller = control.clone();
        let open = |path: &Path| {
            if path.ends_with("f2.txt") {
                canceller.cancel();
            }
            open_regular(path)
        };
        let out = search(&request(root, "hit"), &control, open).unwrap();
        assert_eq!(files(&out, root), ["f0.txt", "f1.txt"]);
        assert_eq!(out.stop, Some(Stop::Cancelled));
        let text = render(out, 1 << 20, LONG).unwrap();
        assert!(
            text.ends_with("\n\n... (grep was cancelled; results are partial)"),
            "{text}"
        );
    }

    /// Passing the deadline mid-search keeps what was found and says so.
    #[test]
    fn deadline_returns_partial_results() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        numbered_tree(root);
        let timeout = Duration::from_millis(500);
        let control = Control::new(timeout);
        let deadline = Instant::now() + timeout;
        let open = |path: &Path| {
            if path.ends_with("f2.txt") {
                // A slow file: still opening when the deadline passes.
                std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
                std::thread::sleep(Duration::from_millis(50));
            }
            open_regular(path)
        };
        let out = search(&request(root, "hit"), &control, open).unwrap();
        assert_eq!(files(&out, root), ["f0.txt", "f1.txt"]);
        assert_eq!(out.stop, Some(Stop::TimedOut));
        let text = render(out, 1 << 20, timeout).unwrap();
        assert!(
            text.contains("\n\n... (grep timed out after 0s; results are partial"),
            "{text}"
        );
    }

    /// Timing out before any match is an error, not "No matches found.".
    #[test]
    fn deadline_without_matches_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        numbered_tree(dir.path());
        let out = search(
            &request(dir.path(), "hit"),
            &Control::new(Duration::ZERO),
            open_regular,
        )
        .unwrap();
        let err = render(out, 1 << 20, Duration::from_secs(15))
            .unwrap_err()
            .to_string();
        assert!(err.contains("Grep timed out after 15s"), "{err}");
    }

    /// A FIFO in the tree is skipped and a FIFO as the path is an error;
    /// neither blocks.
    #[cfg(unix)]
    #[test]
    fn fifo_is_never_read() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        write(&root, "a.txt", "hit\n");
        let fifo = root.join("pipe");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: `c` is a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
        let tree = root.clone();
        let out = within(move || run(&request(&tree, "hit")));
        assert_eq!(files(&out, &root), ["a.txt"]);
        assert_eq!(out.errors.count, 0);
        let out = within(move || run(&request(&fifo, "hit")));
        let err = render(out, 1 << 20, LONG).unwrap_err().to_string();
        assert!(err.contains("not a regular file"), "{err}");
    }

    /// Links in the tree aren't followed, so a loop can't hang the walk; a
    /// symlinked root is followed, as with `grep -r`.
    #[cfg(unix)]
    #[test]
    fn symlinks_and_loops() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        write(&root, "real/a.txt", "hit\n");
        symlink(&root, root.join("real").join("loop")).unwrap();
        symlink(
            root.join("real").join("a.txt"),
            root.join("real").join("link.txt"),
        )
        .unwrap();
        symlink(root.join("real"), root.join("alias")).unwrap();
        let tree = root.clone();
        let out = within(move || run(&request(&tree, "hit")));
        assert_eq!(files(&out, &root), ["real/a.txt"]);
        let alias = root.join("alias");
        let tree = alias.clone();
        let out = within(move || run(&request(&tree, "hit")));
        assert_eq!(files(&out, &alias), ["a.txt"]);
        assert_eq!(out.errors.count, 0, "{:?}", out.errors.first);
    }

    /// `$` matches before `\r\n`, and the `\r` isn't printed. A UTF-8 BOM
    /// doesn't stop `^` matching line 1.
    #[test]
    fn crlf_and_bom() {
        let dir = tempfile::tempdir().unwrap();
        let crlf = dir.path().join("crlf.txt");
        fs::write(&crlf, "foo hit\r\nbar\r\n").unwrap();
        assert_eq!(run(&request(&crlf, "hit$")).text, "1:foo hit\n");
        // `^`/`$` anchor every line, and `$` matches before `\r\n`.
        let anchored = dir.path().join("anchored.txt");
        fs::write(&anchored, "x\r\nxx\r\nx\r\n").unwrap();
        assert_eq!(run(&request(&anchored, "^x$")).text, "1:x\n3:x\n");
        let bom = dir.path().join("bom.txt");
        fs::write(&bom, "\u{feff}hit bom\nhit two\n").unwrap();
        assert_eq!(run(&request(&bom, "^hit")).text, "1:hit bom\n2:hit two\n");
    }

    /// An empty pattern matches every line; a leading `-` is just a pattern;
    /// POSIX classes work inside brackets.
    #[test]
    fn empty_dash_and_posix_class_patterns() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("p.txt");
        fs::write(&file, "-v flag\nabc\nx42\n").unwrap();
        assert_eq!(run(&request(&file, "")).text, "1:-v flag\n2:abc\n3:x42\n");
        assert_eq!(run(&request(&file, "-v")).text, "1:-v flag\n");
        assert_eq!(run(&request(&file, "[[:digit:]]+")).text, "3:x42\n");
    }

    /// Latin-1 is searched (shown lossily); UTF-16LE with a BOM is decoded,
    /// not treated as binary.
    #[test]
    fn latin1_and_utf16_bom() {
        let dir = tempfile::tempdir().unwrap();
        let latin1 = dir.path().join("l1.txt");
        fs::write(&latin1, b"caf\xe9 hit\n").unwrap();
        assert_eq!(run(&request(&latin1, "hit")).text, "1:caf\u{fffd} hit\n");
        let utf16 = dir.path().join("u16.txt");
        let mut bytes = vec![0xff, 0xfe];
        bytes.extend("x\nhit utf16\n".encode_utf16().flat_map(u16::to_le_bytes));
        fs::write(&utf16, bytes).unwrap();
        let out = run(&request(&utf16, "hit"));
        assert_eq!(out.text, "2:hit utf16\n");
        assert_eq!(out.binary_count, 0);
    }

    /// Long lines are cut to MAX_COLUMNS characters with a marker.
    #[test]
    fn long_lines_are_cut() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("min.js");
        let line = format!("hit{}", "é".repeat(5_000));
        fs::write(&file, format!("{line}\n")).unwrap();
        let text = run(&request(&file, "hit")).text;
        let expected = format!(
            "1:hit{} [... line truncated, {} bytes]\n",
            "é".repeat(MAX_COLUMNS - 3),
            line.len()
        );
        assert_eq!(text, expected);
    }

    /// A line over the heap limit makes that file an error; other files are
    /// still searched.
    #[test]
    fn line_over_heap_limit_is_a_file_error() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut huge = b"hit".to_vec();
        huge.resize(LINE_HEAP_LIMIT + (1 << 20), b'x');
        write(root, "a.txt", huge);
        write(root, "b.txt", "hit\n");
        let out = run(&request(root, "hit"));
        assert_eq!(files(&out, root), ["b.txt"]);
        assert_eq!(out.errors.count, 1);
        assert!(
            out.errors
                .first
                .as_deref()
                .unwrap()
                .ends_with("a line is longer than 8 MiB"),
            "{:?}",
            out.errors.first
        );
    }
}
