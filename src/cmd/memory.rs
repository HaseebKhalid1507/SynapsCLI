//! `synaps memory`: bring archives and notes into the plain-file stores from
//! JSON Lines, and say where a folder's memory lives.
//!
//! This file is the loop, the routing and the report. Every decision about a
//! record is the store's: `ArchiveStore::import` for an archive record,
//! `import_line_in` for a note line. Nothing here reads `memory.backend`, the
//! config or a profile, and nothing here talks to Axel: the command writes
//! plain files whatever the config says.
//!
//! Each action prints one JSON document on standard output. No field of it
//! holds content of an archive or a note: a refusal reason is the store's
//! name for the check that failed, or one of the fixed texts below.
//!
//! Exit codes: 0 nothing was refused, 1 something was (or a forget did not
//! reach every copy), 2 the run stopped (or the command line could not be
//! used). There is no other code.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use agent_core::context_archive::{self as archive, ArchiveStore, ImportSeen};
use agent_core::core::lock_file::{self, ExclusiveLock};
use agent_core::core::private_fs;
use agent_core::memory::store::{self as notes, ImportRun, ImportTarget, ProjectScope};
use serde::Serialize;
use sha2::{Digest, Sha256};

/// Nothing was refused.
const EXIT_OK: i32 = 0;
/// At least one line was refused, or a forget may not have reached every
/// copy; every other line was handled.
const EXIT_REFUSED: i32 = 1;
/// The run stopped, or the command line could not be used.
const EXIT_STOPPED: i32 = 2;

/// The longest line that is read into memory: the largest archive record and
/// some room. A longer line is refused, and skipped without being kept.
const MAX_LINE_BYTES: usize = archive::MAX_SEGMENT_BYTES + 4096;

/// The lock a real `import-notes` run holds from before its first line to
/// after its last sync, under `<base>/memory/`.
const NOTES_IMPORT_LOCK: &str = ".import.lock";
const LOCK_HELD: &str = "another import is running";

/// Notes that are for every project have this key, which is not a project.
const USER_WIDE_KEY: &str = "p0000000000000000";

#[derive(clap::Subcommand, Debug)]
pub enum MemoryAction {
    /// Import archive records (JSON Lines: one archive file's object per
    /// line) into the plain-file archive store.
    ImportArchives(ImportArgs),
    /// Import notes (JSON Lines: note lines and tombstone lines) into the
    /// plain-file notes store. One import at a time.
    ImportNotes(ImportArgs),
    /// Print the project a folder belongs to and where its memory is kept.
    /// Creates nothing.
    Project {
        /// The folder to ask about. Default: the current folder.
        #[arg(long, value_name = "DIR")]
        cwd: Option<PathBuf>,
    },
}

#[derive(clap::Args, Debug)]
#[command(group(
    clap::ArgGroup::new("destination")
        .required(true)
        .multiple(false)
        .args(["as_recorded", "cwd", "project_key"])
))]
pub struct ImportArgs {
    /// JSON Lines files, imported in the order given. `-` is standard input.
    /// Blank lines are skipped.
    #[arg(required = true, value_name = "FILE")]
    files: Vec<PathBuf>,
    /// File each line under the project it names itself. Nothing in it changes.
    #[arg(long)]
    as_recorded: bool,
    /// File every line under the project of this folder, as a chat started
    /// there would. Refused while `SYNAPS_PROJECT_ROOT` is set.
    #[arg(long, value_name = "DIR")]
    cwd: Option<PathBuf>,
    /// File every line under this project key (`p` and 16 hex characters).
    #[arg(long, value_name = "KEY")]
    project_key: Option<String>,
    /// Write nothing and print what the real run would print.
    #[arg(long)]
    dry_run: bool,
}

// ─── The report ─────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct Report {
    command: &'static str,
    dry_run: bool,
    base: String,
    destination: Destination,
    inputs: Vec<Input>,
    total: Counts,
    balanced: bool,
    stopped: Option<Stopped>,
    /// `import-notes` only: tombstone lines that were refused, so the notes
    /// they name were not forgotten by this run.
    #[serde(skip_serializing_if = "Option::is_none")]
    tombstones_not_applied: Option<NotApplied>,
    /// `import-archives` only: tombstone records whose forget may not have
    /// reached every copy of the content (the entries that have `unread`).
    /// Their outcome is `forgotten`, so `total` does not show them.
    #[serde(skip_serializing_if = "Option::is_none")]
    forgets_not_complete: Option<NotApplied>,
    projects: Vec<Project>,
    records: Vec<Entry>,
}

#[derive(Serialize)]
struct Destination {
    mode: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    project_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    project_root_override: Option<bool>,
}

#[derive(Serialize)]
struct Input {
    path: String,
    /// The lines of this input that were read and are not blank.
    lines: u64,
    /// Of every byte of the input. `null` when it was not read to its end.
    sha256: Option<String>,
}

#[derive(Serialize, Default)]
struct Counts {
    read: u64,
    stored: u64,
    already_present: u64,
    forgotten: u64,
    kept_forgotten: u64,
    refused: u64,
}

impl Counts {
    fn count(&mut self, outcome: &str) {
        match outcome {
            "stored" => self.stored += 1,
            "already_present" => self.already_present += 1,
            "forgotten" => self.forgotten += 1,
            "kept_forgotten" => self.kept_forgotten += 1,
            _ => self.refused += 1,
        }
    }

    fn balanced(&self) -> bool {
        self.read
            == self.stored
                + self.already_present
                + self.forgotten
                + self.kept_forgotten
                + self.refused
    }
}

#[derive(Serialize)]
struct Stopped {
    input: Option<usize>,
    line: Option<u64>,
    id: Option<String>,
    error: String,
}

#[derive(Serialize)]
struct NotApplied {
    count: u64,
    ids: Vec<String>,
}

/// One project of the run: a directory of archives, or a notes file.
#[derive(Serialize)]
struct Project {
    #[serde(skip_serializing_if = "Option::is_none")]
    dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    file: Option<String>,
    project_key: Option<String>,
    read: u64,
    /// Lines the store read as a live record, and as a tombstone. A refused
    /// line is in neither unless the store got far enough to tell.
    live: u64,
    tombstones: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    messages: Option<u64>,
    stored: u64,
    already_present: u64,
    forgotten: u64,
    kept_forgotten: u64,
    refused: u64,
    /// Archives only: the records this run stored, by shelf.
    #[serde(skip_serializing_if = "Option::is_none")]
    shelves: Option<BTreeMap<String, u64>>,
}

#[derive(Serialize)]
struct Entry {
    input: usize,
    line: u64,
    id: Option<String>,
    outcome: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    shelf: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    #[serde(skip_serializing_if = "is_false")]
    note_differs: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    same_content_as: Option<String>,
    /// A forget that may not have reached every copy: the store's words.
    #[serde(skip_serializing_if = "Option::is_none")]
    unread: Option<String>,
    /// `import-notes`: the line is a tombstone line.
    #[serde(skip_serializing_if = "is_false")]
    tombstone: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// What a store said about one line.
#[derive(Default)]
struct Judged {
    id: Option<String>,
    outcome: &'static str,
    reason: Option<String>,
    /// The project block the line counts in: a directory name (archives) or
    /// a project key (notes). `None`: the line never got as far as a project.
    project: Option<String>,
    /// Whether the line is a live record, when the store got far enough.
    live: Option<bool>,
    messages: u64,
    shelf: Option<String>,
    note_differs: bool,
    same_content_as: Option<String>,
    unread: Option<String>,
    tombstone_line: bool,
}

impl Judged {
    fn refused(id: Option<String>, reason: String, project: Option<String>) -> Self {
        Self {
            id,
            outcome: "refused",
            reason: Some(reason),
            project,
            ..Self::default()
        }
    }
}

// ─── Where the lines go ─────────────────────────────────────────────────────

/// The destination of a run. `None` as the key: as recorded.
struct Target {
    key: Option<String>,
    shown: Destination,
}

/// The project key syntax both stores file under: `p` and 16 lowercase hex
/// characters, and not the user-wide key. (The notes store checks it again
/// on every line; its own check is not public.)
fn is_project_key(key: &str) -> bool {
    key.len() == 17
        && key.starts_with('p')
        && key.as_bytes()[1..]
            .iter()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
        && key != USER_WIDE_KEY
}

fn root_override() -> bool {
    std::env::var_os("SYNAPS_PROJECT_ROOT").is_some()
}

fn shown(path: &Path) -> String {
    std::path::absolute(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}

fn destination(args: &ImportArgs) -> Result<Target, String> {
    if let Some(key) = &args.project_key {
        if !is_project_key(key) {
            return Err(
                "--project-key takes a project key: p and 16 lowercase hex characters, \
                 and not the user-wide key"
                    .to_owned(),
            );
        }
        return Ok(Target {
            key: Some(key.clone()),
            shown: Destination {
                mode: "project-key",
                cwd: None,
                project_key: Some(key.clone()),
                project_root_override: None,
            },
        });
    }
    if let Some(cwd) = &args.cwd {
        if root_override() {
            // The override wins before the folder is looked at: every
            // folder, also one that does not exist, would be its project.
            return Err(
                "--cwd cannot be used while SYNAPS_PROJECT_ROOT is set: every folder is then \
                 that root's project. Unset it, or pass --project-key; nothing was imported"
                    .to_owned(),
            );
        }
        let scope = ProjectScope::discover(cwd).map_err(|e| format!("--cwd: {e}"))?;
        return Ok(Target {
            key: Some(scope.key().to_owned()),
            shown: Destination {
                mode: "cwd",
                cwd: Some(shown(cwd)),
                project_key: Some(scope.key().to_owned()),
                // Always false: see above. Kept, so that a reader of the
                // document need not know that.
                project_root_override: Some(false),
            },
        });
    }
    Ok(Target {
        key: None,
        shown: Destination {
            mode: "as-recorded",
            cwd: None,
            project_key: None,
            project_root_override: None,
        },
    })
}

// ─── Reading lines ──────────────────────────────────────────────────────────

enum Line {
    Text(Vec<u8>),
    /// Longer than [`MAX_LINE_BYTES`]; its bytes were hashed and not kept.
    TooLong,
}

/// The lines of one input, one at a time, with the hash of every byte read.
struct Lines {
    reader: Box<dyn BufRead>,
    hash: Sha256,
    /// The number of the last line returned, from 1.
    number: u64,
}

impl Lines {
    fn new(reader: Box<dyn BufRead>) -> Self {
        Self {
            reader,
            hash: Sha256::new(),
            number: 0,
        }
    }

    fn next(&mut self) -> io::Result<Option<Line>> {
        let mut line = Vec::new();
        let (mut any, mut too_long) = (false, false);
        loop {
            let buffer = match self.reader.fill_buf() {
                Ok(buffer) => buffer,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            };
            if buffer.is_empty() {
                break;
            }
            any = true;
            let (take, ended) = match memchr::memchr(b'\n', buffer) {
                Some(at) => (at + 1, true),
                None => (buffer.len(), false),
            };
            self.hash.update(&buffer[..take]);
            if !too_long {
                let body = &buffer[..take - usize::from(ended)];
                if line.len() + body.len() > MAX_LINE_BYTES {
                    too_long = true;
                    line = Vec::new();
                } else {
                    line.extend_from_slice(body);
                }
            }
            self.reader.consume(take);
            if ended {
                break;
            }
        }
        if !any {
            return Ok(None);
        }
        self.number += 1;
        Ok(Some(if too_long {
            Line::TooLong
        } else {
            Line::Text(line)
        }))
    }

    fn sha256(self) -> String {
        format!("{:x}", self.hash.finalize())
    }
}

fn open_input(path: &Path) -> io::Result<Box<dyn BufRead>> {
    if path.as_os_str() == "-" {
        return Ok(Box::new(BufReader::new(io::stdin())));
    }
    Ok(Box::new(BufReader::new(File::open(path)?)))
}

// ─── The two stores ─────────────────────────────────────────────────────────

/// One store's side of a run. `Err` from `line` is the machine's fault and
/// stops the run; a refusal is an `Ok`.
trait Importer {
    fn line(&mut self, line: &[u8]) -> Result<Judged, String>;
    /// An empty block for a project of this store.
    fn block(&self, project: &str) -> Project;
    /// End the run, on an early stop too.
    fn finish(self) -> Result<(), String>;
}

struct Archives {
    base: PathBuf,
    dry_run: bool,
    /// The project key and directory every record goes to, or `None`: each
    /// record goes to the directory its own `scope` names.
    target: Option<(String, String)>,
    seen: ImportSeen,
    /// The project directories of the run. `None`: it does not exist (a dry
    /// run), or does not exist yet (a real run, until a record is stored).
    stores: HashMap<String, Option<ArchiveStore>>,
}

impl Archives {
    fn new(base: &Path, dry_run: bool, key: Option<&str>) -> Result<Self, String> {
        let target = match key {
            Some(key) => Some((
                key.to_owned(),
                archive::project_scope(key).map_err(|e| e.to_string())?,
            )),
            None => None,
        };
        Ok(Self {
            base: base.to_path_buf(),
            dry_run,
            target,
            seen: ImportSeen::default(),
            stores: HashMap::new(),
        })
    }
}

impl Importer for Archives {
    fn line(&mut self, line: &[u8]) -> Result<Judged, String> {
        let scope = match &self.target {
            Some((_, scope)) => scope.clone(),
            None => match archive::record_scope(line) {
                Ok(scope) => scope,
                Err(refused) => return Ok(Judged::refused(refused.id, refused.reason, None)),
            },
        };
        let rehome = self.target.is_some();
        if !self.stores.contains_key(&scope) {
            let existing =
                ArchiveStore::existing_scope(&self.base, &scope).map_err(|e| e.to_string())?;
            self.stores.insert(scope.clone(), existing);
        }
        let answer = match self.stores.get(&scope).and_then(Option::as_ref) {
            Some(store) => store.import(line, rehome, self.dry_run, &mut self.seen),
            None if self.dry_run => {
                ArchiveStore::import_dry_absent(&scope, line, rehome, &mut self.seen)
            }
            None => {
                // The project has no directory. The store's own dry run of
                // an empty project judges the record first, so that a
                // refused record does not leave an empty directory behind.
                let mut alone = ImportSeen::default();
                match ArchiveStore::import_dry_absent(&scope, line, rehome, &mut alone) {
                    Ok(Ok(_)) => {
                        let store = ArchiveStore::for_scope(&self.base, &scope)
                            .map_err(|e| e.to_string())?;
                        let answer = store.import(line, rehome, false, &mut self.seen);
                        self.stores.insert(scope.clone(), Some(store));
                        answer
                    }
                    other => other,
                }
            }
        };
        Ok(match answer.map_err(|e| e.to_string())? {
            Err(refused) => Judged::refused(refused.id, refused.reason, Some(scope)),
            Ok(imported) => {
                let (outcome, note_differs) = match imported.outcome {
                    archive::ImportOutcome::Stored => ("stored", false),
                    archive::ImportOutcome::Present { note_differs } => {
                        ("already_present", note_differs)
                    }
                    archive::ImportOutcome::Forgotten => ("forgotten", false),
                    archive::ImportOutcome::KeptForgotten => ("kept_forgotten", false),
                };
                Judged {
                    id: Some(imported.id),
                    outcome,
                    reason: None,
                    project: Some(scope),
                    live: Some(imported.live),
                    messages: imported.message_count as u64,
                    shelf: imported.shelf,
                    note_differs,
                    same_content_as: imported.same_content_as,
                    unread: imported.unread,
                    tombstone_line: false,
                }
            }
        })
    }

    fn block(&self, project: &str) -> Project {
        Project {
            dir: Some(project.to_owned()),
            file: None,
            project_key: self.target.as_ref().map(|(key, _)| key.clone()),
            read: 0,
            live: 0,
            tombstones: 0,
            messages: Some(0),
            stored: 0,
            already_present: 0,
            forgotten: 0,
            kept_forgotten: 0,
            refused: 0,
            shelves: Some(BTreeMap::new()),
        }
    }

    fn finish(self) -> Result<(), String> {
        // The store publishes each record durably by itself.
        Ok(())
    }
}

struct Notes {
    base: PathBuf,
    target: Option<String>,
    run: ImportRun,
    /// Held to the end of a real run.
    _lock: Option<ExclusiveLock>,
}

impl Notes {
    /// A real run takes the import lock here, before a line is read; a dry
    /// run takes none and creates nothing.
    fn new(base: &Path, dry_run: bool, key: Option<&str>) -> Result<Self, String> {
        let lock = if dry_run {
            None
        } else {
            Some(lock_notes_import(base)?)
        };
        Ok(Self {
            base: base.to_path_buf(),
            target: key.map(str::to_owned),
            // A run of its own for each pass: one that has seen a dry run
            // would take every line of the real run for stored.
            run: ImportRun::new(dry_run),
            _lock: lock,
        })
    }
}

/// Take `<base>/memory/.import.lock` without waiting. When another import
/// holds it nothing is created or changed: the file is there already.
fn lock_notes_import(base: &Path) -> Result<ExclusiveLock, String> {
    let dir = notes::memory_dir_in(base);
    let path = dir.join(NOTES_IMPORT_LOCK);
    let cannot = |e: &dyn std::fmt::Display| {
        format!("cannot open the import lock {}: {e}; nothing was written", path.display())
    };
    let open = |create: bool| -> io::Result<File> {
        if path.symlink_metadata().is_ok_and(|meta| !meta.is_file()) {
            return Err(io::Error::other("it is not a regular file"));
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(create).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(private_fs::FILE_MODE);
        }
        options.open(&path)
    };
    let file = match open(false) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            private_fs::ensure_private_dir(&dir).map_err(|e| cannot(&e))?;
            open(true).map_err(|e| cannot(&e))?
        }
        Err(error) => return Err(cannot(&error)),
    };
    match lock_file::try_lock(file) {
        Ok(Some(lock)) => Ok(lock),
        Ok(None) => Err(LOCK_HELD.to_owned()),
        Err(error) => Err(cannot(&error)),
    }
}

impl Importer for Notes {
    fn line(&mut self, line: &[u8]) -> Result<Judged, String> {
        let Ok(text) = std::str::from_utf8(line) else {
            return Ok(Judged::refused(None, "not UTF-8".to_owned(), None));
        };
        let target = match &self.target {
            Some(key) => ImportTarget::Project(key),
            None => ImportTarget::AsRecorded,
        };
        let imported = notes::import_line_in(&self.base, text, target, &mut self.run)
            .map_err(|e| e.to_string())?;
        let reason = match &imported.outcome {
            notes::ImportOutcome::Refused(reason) => Some(reason.clone()),
            _ => None,
        };
        Ok(Judged {
            id: imported.id,
            outcome: imported.outcome.name(),
            reason,
            // A block is a file the line was judged against, so by then the
            // store knows which kind of line it is.
            live: imported.project.as_ref().map(|_| !imported.tombstone),
            project: imported.project,
            tombstone_line: imported.tombstone,
            ..Judged::default()
        })
    }

    fn block(&self, project: &str) -> Project {
        Project {
            dir: None,
            file: Some(
                notes::project_notes_file(&self.base, project)
                    .display()
                    .to_string(),
            ),
            project_key: Some(project.to_owned()),
            read: 0,
            live: 0,
            tombstones: 0,
            messages: None,
            stored: 0,
            already_present: 0,
            forgotten: 0,
            kept_forgotten: 0,
            refused: 0,
            shelves: None,
        }
    }

    fn finish(self) -> Result<(), String> {
        // Nothing the run stored is durable before this returns. The lock
        // is released after it, when `self` goes.
        self.run.finish().map(|_| ()).map_err(|e| e.to_string())
    }
}

// ─── The run ────────────────────────────────────────────────────────────────

pub fn run(action: MemoryAction) -> i32 {
    match action {
        MemoryAction::Project { cwd } => project(cwd),
        MemoryAction::ImportArchives(args) => import("import-archives", args, Archives::new),
        MemoryAction::ImportNotes(args) => import("import-notes", args, Notes::new),
    }
}

fn usage(message: &str) -> i32 {
    eprintln!("synaps memory: {message}");
    EXIT_STOPPED
}

fn print<T: Serialize>(document: &T) -> bool {
    let Ok(mut text) = serde_json::to_string_pretty(document) else {
        return false;
    };
    text.push('\n');
    let mut out = io::stdout().lock();
    out.write_all(text.as_bytes()).and_then(|()| out.flush()).is_ok()
}

/// The base an import files under: `SYNAPS_BASE_DIR`, or `.synaps-cli` in
/// the home folder. It must be an absolute path: an empty variable, or no
/// home folder, would otherwise file notes under the folder the command
/// happens to run in. A real run creates no base: a mistyped one is not
/// made and filled. A dry run goes on without one and says so.
fn import_base(dry_run: bool) -> Result<PathBuf, String> {
    if std::env::var_os("SYNAPS_BASE_DIR").is_some_and(|value| value.to_str().is_none()) {
        return Err("SYNAPS_BASE_DIR is not UTF-8; nothing was imported".to_owned());
    }
    let base = agent_core::config::base_dir();
    if !base.is_absolute() {
        return Err(format!(
            "the base directory \"{}\" is not an absolute path (is SYNAPS_BASE_DIR empty or \
             relative, or HOME not set?); nothing was imported",
            base.display()
        ));
    }
    if !base.is_dir() {
        if !dry_run {
            return Err(format!(
                "the base directory {} does not exist or is not a directory, and an import \
                 does not create it; nothing was imported",
                base.display()
            ));
        }
        eprintln!(
            "synaps memory: the base directory {} does not exist or is not a directory; \
             the real run would stop",
            base.display()
        );
    }
    Ok(base)
}

fn import<I: Importer>(
    command: &'static str,
    args: ImportArgs,
    open: impl FnOnce(&Path, bool, Option<&str>) -> Result<I, String>,
) -> i32 {
    let target = match destination(&args) {
        Ok(target) => target,
        Err(message) => return usage(&message),
    };
    if args.files.iter().filter(|path| path.as_os_str() == "-").count() > 1 {
        return usage("standard input (-) can be named once");
    }
    let base = match import_base(args.dry_run) {
        Ok(base) => base,
        Err(message) => return usage(&message),
    };
    let mut report = Report {
        command,
        dry_run: args.dry_run,
        base: base.display().to_string(),
        destination: target.shown,
        inputs: args
            .files
            .iter()
            .map(|path| Input {
                path: if path.as_os_str() == "-" {
                    "-".to_owned()
                } else {
                    shown(path)
                },
                lines: 0,
                sha256: None,
            })
            .collect(),
        total: Counts::default(),
        balanced: true,
        stopped: None,
        tombstones_not_applied: (command == "import-notes").then_some(NotApplied {
            count: 0,
            ids: Vec::new(),
        }),
        forgets_not_complete: (command == "import-archives").then_some(NotApplied {
            count: 0,
            ids: Vec::new(),
        }),
        projects: Vec::new(),
        records: Vec::new(),
    };

    // Every input is opened before a line is imported: a file that is not
    // there stops the run with nothing written, not half way through.
    let mut inputs = Vec::new();
    for (index, path) in args.files.iter().enumerate() {
        match open_input(path) {
            Ok(reader) => inputs.push(reader),
            Err(error) => {
                report.stopped = Some(Stopped {
                    input: Some(index),
                    line: None,
                    id: None,
                    error: format!("cannot open the input: {error}; nothing was imported"),
                });
                break;
            }
        }
    }
    if report.stopped.is_none() {
        match open(&base, args.dry_run, target.key.as_deref()) {
            Ok(mut importer) => {
                report.stopped = feed(&mut importer, inputs, &mut report);
                if let Err(error) = importer.finish() {
                    // The first stop stays; a failed sync is said as well.
                    match &mut report.stopped {
                        Some(stopped) => {
                            stopped.error = format!("{}; and at the end: {error}", stopped.error)
                        }
                        None => {
                            report.stopped = Some(Stopped {
                                input: None,
                                line: None,
                                id: None,
                                error,
                            })
                        }
                    }
                }
            }
            Err(error) => {
                report.stopped = Some(Stopped {
                    input: None,
                    line: None,
                    id: None,
                    error,
                })
            }
        }
    }
    report.balanced = report.total.balanced();

    if let Some(stopped) = &report.stopped {
        eprintln!("synaps memory {command}: stopped: {}", stopped.error);
    }
    if let Some(unapplied) = report
        .tombstones_not_applied
        .as_ref()
        .filter(|unapplied| unapplied.count > 0)
    {
        eprintln!(
            "synaps memory {command}: {} forgotten id(s) not applied; \
             re-run with --project-key or --cwd: {}",
            unapplied.count,
            unapplied.ids.join(" ")
        );
    }
    let incomplete = report.records.iter().any(|entry| entry.unread.is_some());
    for entry in report.records.iter().filter(|entry| entry.unread.is_some()) {
        eprintln!(
            "synaps memory {command}: the forget of {} may not have reached every copy",
            entry.id.as_deref().unwrap_or("?")
        );
    }
    if !print(&report) || report.stopped.is_some() {
        EXIT_STOPPED
    } else if report.total.refused > 0 || incomplete {
        EXIT_REFUSED
    } else {
        EXIT_OK
    }
}

/// Give every line of every input to the store, in order. `Some` when the
/// run had to stop; everything before that line is done.
fn feed<I: Importer>(
    importer: &mut I,
    inputs: Vec<Box<dyn BufRead>>,
    report: &mut Report,
) -> Option<Stopped> {
    let mut blocks: HashMap<String, usize> = HashMap::new();
    for (input, reader) in inputs.into_iter().enumerate() {
        let mut lines = Lines::new(reader);
        loop {
            let line = match lines.next() {
                Ok(Some(line)) => line,
                Ok(None) => break,
                Err(error) => {
                    return Some(Stopped {
                        input: Some(input),
                        line: Some(lines.number + 1),
                        id: None,
                        error: format!("cannot read the input: {error}"),
                    })
                }
            };
            let trimmed: &[u8] = match &line {
                Line::Text(bytes) => bytes.trim_ascii(),
                Line::TooLong => &[],
            };
            if matches!(line, Line::Text(_)) && trimmed.is_empty() {
                // A blank line is counted nowhere.
                continue;
            }
            report.inputs[input].lines += 1;
            report.total.read += 1;
            let judged = match &line {
                Line::TooLong => Judged::refused(
                    None,
                    format!("the line is longer than {MAX_LINE_BYTES} bytes"),
                    None,
                ),
                Line::Text(_) => match importer.line(trimmed) {
                    Ok(judged) => judged,
                    Err(error) => {
                        return Some(Stopped {
                            input: Some(input),
                            line: Some(lines.number),
                            id: None,
                            error,
                        })
                    }
                },
            };
            report.total.count(judged.outcome);
            if let Some(project) = &judged.project {
                let at = *blocks.entry(project.clone()).or_insert_with(|| {
                    report.projects.push(importer.block(project));
                    report.projects.len() - 1
                });
                let block = &mut report.projects[at];
                block.read += 1;
                match judged.outcome {
                    "stored" => block.stored += 1,
                    "already_present" => block.already_present += 1,
                    "forgotten" => block.forgotten += 1,
                    "kept_forgotten" => block.kept_forgotten += 1,
                    _ => block.refused += 1,
                }
                match judged.live {
                    Some(true) => block.live += 1,
                    Some(false) => block.tombstones += 1,
                    None => {}
                }
                if let Some(messages) = &mut block.messages {
                    *messages += judged.messages;
                }
                if let (Some(shelves), Some(shelf), "stored") =
                    (&mut block.shelves, &judged.shelf, judged.outcome)
                {
                    *shelves.entry(shelf.clone()).or_insert(0) += 1;
                }
            }
            if judged.tombstone_line && judged.outcome == "refused" {
                if let Some(unapplied) = &mut report.tombstones_not_applied {
                    unapplied.count += 1;
                    unapplied.ids.extend(judged.id.clone());
                }
            }
            if judged.unread.is_some() {
                if let Some(incomplete) = &mut report.forgets_not_complete {
                    incomplete.count += 1;
                    incomplete.ids.extend(judged.id.clone());
                }
            }
            report.records.push(Entry {
                input,
                line: lines.number,
                id: judged.id,
                outcome: judged.outcome,
                shelf: judged.shelf,
                reason: judged.reason,
                note_differs: judged.note_differs,
                same_content_as: judged.same_content_as,
                unread: judged.unread,
                tombstone: judged.tombstone_line,
            });
        }
        report.inputs[input].sha256 = Some(lines.sha256());
    }
    None
}

// ─── `synaps memory project` ────────────────────────────────────────────────

#[derive(Serialize)]
struct ProjectDocument {
    cwd: String,
    root: String,
    project_key: String,
    archives_dir: String,
    notes_file: String,
    project_root_override: bool,
}

fn project(cwd: Option<PathBuf>) -> i32 {
    let cwd = match cwd {
        Some(cwd) => cwd,
        None => match std::env::current_dir() {
            Ok(cwd) => cwd,
            Err(error) => return usage(&format!("the current folder: {error}")),
        },
    };
    let scope = match ProjectScope::discover(&cwd) {
        Ok(scope) => scope,
        Err(error) => return usage(&format!("--cwd: {error}")),
    };
    let dir = match archive::project_scope(scope.key()) {
        Ok(dir) => dir,
        Err(error) => return usage(&error.to_string()),
    };
    let base = agent_core::config::base_dir();
    let document = ProjectDocument {
        cwd: shown(&cwd),
        root: scope.root().display().to_string(),
        project_key: scope.key().to_owned(),
        archives_dir: base.join("context-archives").join(dir).display().to_string(),
        notes_file: notes::project_notes_file(&base, scope.key())
            .display()
            .to_string(),
        project_root_override: root_override(),
    };
    if print(&document) {
        EXIT_OK
    } else {
        EXIT_STOPPED
    }
}
