//! `synaps memory import-archives | import-notes | project`: the built binary,
//! with `SYNAPS_BASE_DIR` and `HOME` in a temp folder of the test. Nothing
//! here reads or writes `~/.synaps-cli`.
//!
//! T-A11, T-A12, T-A13 and T-A15 are the tests of that name in the cut spec.

#![cfg(unix)]

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use agent_core::context_archive::{archive_file, project_scope, ArchiveStore};
use agent_core::memory::store::ProjectScope;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// In every message and note the tests make. No report may hold it.
const MARKER: &str = "ZEBRA-CONTENT-7731";
/// The project the made-up records were sealed in.
const SOURCE_KEY: &str = "p1111111111111111";
/// A project a run files them under.
const KEY: &str = "p0cc924a8dff92368";
const TOTALS: [&str; 6] = [
    "read",
    "stored",
    "already_present",
    "forgotten",
    "kept_forgotten",
    "refused",
];

struct Env {
    tmp: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        for dir in ["home", "work", "base"] {
            std::fs::create_dir(tmp.path().join(dir)).unwrap();
        }
        Self { tmp }
    }
    /// With no base directory: a run that makes it is seen.
    fn without_base() -> Self {
        let env = Self::new();
        std::fs::remove_dir(env.base()).unwrap();
        env
    }
    /// Empty until a real run: whatever a run makes in it is seen.
    fn base(&self) -> PathBuf {
        self.tmp.path().join("base")
    }
    fn work(&self) -> PathBuf {
        self.tmp.path().join("work")
    }
    fn file(&self, name: &str, lines: &[String]) -> String {
        let path = self.work().join(name);
        let mut text = lines.join("\n");
        text.push('\n');
        std::fs::write(&path, text).unwrap();
        path.to_str().unwrap().to_owned()
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_synaps"));
        command
            .arg("memory")
            .args(args)
            .current_dir(self.work())
            .env("SYNAPS_BASE_DIR", self.base())
            .env("HOME", self.tmp.path().join("home"))
            .env_remove("SYNAPS_PROJECT_ROOT")
            .env_remove("SYNAPS_PROFILE")
            .env_remove("XDG_CACHE_HOME")
            .env_remove("XDG_CONFIG_HOME")
            .stdin(Stdio::null());
        command
    }
    fn run(&self, args: &[&str]) -> Out {
        finish(self.command(args), None)
    }
    /// Everything under the temp folder: path, mode, and a file's bytes.
    fn listing(&self) -> Vec<(String, u32, Option<Vec<u8>>)> {
        let mut out = Vec::new();
        walk(self.tmp.path(), self.tmp.path(), &mut out);
        out.sort();
        out
    }
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, u32, Option<Vec<u8>>)>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let meta = path.symlink_metadata().unwrap();
        let name = path.strip_prefix(root).unwrap().to_str().unwrap().to_owned();
        let mode = meta.permissions().mode();
        if meta.is_dir() {
            out.push((name, mode, None));
            walk(root, &path, out);
        } else if meta.is_file() {
            out.push((name, mode, Some(std::fs::read(&path).unwrap_or_default())));
        } else if meta.is_symlink() {
            // Where it points, not what is there: it is not followed.
            use std::os::unix::ffi::OsStrExt;
            let target = std::fs::read_link(&path).unwrap();
            out.push((name, mode, Some(target.as_os_str().as_bytes().to_vec())));
        } else {
            // A FIFO: reading it would wait for a writer.
            out.push((name, mode, None));
        }
    }
}

struct Out {
    code: i32,
    /// `Null` when standard output is not one JSON document.
    doc: Value,
    stdout: String,
    stderr: String,
}

impl Out {
    fn totals(&self) -> [u64; 6] {
        TOTALS.map(|name| {
            self.doc["total"][name]
                .as_u64()
                .unwrap_or_else(|| panic!("no total.{name} in {}", self.stdout))
        })
    }
    /// The entries of `records` with this outcome.
    fn records(&self, outcome: &str) -> Vec<&Value> {
        self.doc["records"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["outcome"] == outcome)
            .collect()
    }
}

fn finish(mut command: Command, stdin: Option<&[u8]>) -> Out {
    if stdin.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn the synaps binary");
    if let Some(bytes) = stdin {
        child.stdin.take().unwrap().write_all(bytes).unwrap();
    }
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    // Shown when a test fails, or with `-- --nocapture`.
    let args: Vec<_> = command.get_args().map(|arg| arg.to_string_lossy()).collect();
    println!("$ synaps {}\n{stdout}{stderr}exit code {:?}", args.join(" "), output.status.code());
    for text in [&stdout, &stderr] {
        assert!(!text.contains(MARKER), "content in the output: {text}");
    }
    Out {
        code: output.status.code().expect("killed by a signal"),
        doc: serde_json::from_str(&stdout).unwrap_or(Value::Null),
        stdout,
        stderr,
    }
}

/// `count` archive records, as lines, sealed by the store itself in a base
/// of their own, with their ids. `tag` keeps two calls apart.
fn sealed(tag: &str, count: usize) -> Vec<(String, String)> {
    let source = tempfile::tempdir().unwrap();
    let store = ArchiveStore::new(source.path(), SOURCE_KEY, "made-up-chat").unwrap();
    (0..count)
        .map(|n| {
            let messages = [
                Arc::new(json!({"role": "user", "content": format!("{MARKER} {tag} question {n}")})),
                Arc::new(json!({"role": "assistant", "content": format!("{MARKER} {tag} answer {n}")})),
            ];
            let id = store.seal(&messages, &format!("{MARKER} note {n}")).unwrap().id;
            let path = archive_file(source.path(), SOURCE_KEY, &id).unwrap().unwrap();
            let line = std::fs::read_to_string(path).unwrap().trim().to_owned();
            assert!(!line.contains('\n'));
            (id, line)
        })
        .collect()
}

fn lines(records: &[(String, String)]) -> Vec<String> {
    records.iter().map(|(_, line)| line.clone()).collect()
}

/// The tombstone of a record line: what a forget leaves of it.
fn tombstone_of(line: &str) -> String {
    let mut record: Value = serde_json::from_str(line).unwrap();
    let object = record.as_object_mut().unwrap();
    object.remove("created_ms");
    object.insert("body".into(), json!({"state": "tombstone"}));
    record.to_string()
}

fn fixture_lines() -> Vec<String> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(
        "docs/extensions/fixtures/context-archives/\
         01a8f6fb3ec12e1d59d07bd6e3943c84c18415e3111dd5cba218b7ce9218b0c7",
    );
    let mut files = vec![dir.join("000.json")];
    let mut shelf: Vec<PathBuf> = std::fs::read_dir(dir.join("s000001"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    shelf.sort();
    files.extend(shelf);
    files
        .iter()
        .map(|path| std::fs::read_to_string(path).unwrap().trim().to_owned())
        .collect()
}

const FIXTURE_DIR: &str = "01a8f6fb3ec12e1d59d07bd6e3943c84c18415e3111dd5cba218b7ce9218b0c7";

fn note(id: &str, project: &str, content: &str) -> String {
    json!({
        "namespace": format!("project-{project}"),
        "timestamp_ms": 1_777_000_000_000u64,
        "content": format!("{MARKER} {content}"),
        "tags": ["imported"],
        "meta": {},
        "id": id,
        "project": project,
        "provenance": {"source": "model", "session": "sess-old"},
        "sensitivity": "normal",
        "retention": "standard",
    })
    .to_string()
}

fn note_tombstone(id: &str) -> String {
    // In the order the notes store writes it.
    format!(r#"{{"tombstone":"{id}","timestamp_ms":1777000000500}}"#)
}

// ─── Archives ───────────────────────────────────────────────────────────────

/// cut-spec T-A11.
#[test]
fn t_a11_the_command() {
    let fixtures = fixture_lines();
    assert_eq!(fixtures.len(), 5);

    // As recorded: a dry run, the real run, and the real run again.
    let env = Env::new();
    let file = env.file("fixtures.jsonl", &fixtures);
    let before = env.listing();
    let dry = env.run(&["import-archives", &file, "--as-recorded", "--dry-run"]);
    assert_eq!(dry.code, 0, "{}{}", dry.stdout, dry.stderr);
    assert_eq!(env.listing(), before, "a dry run wrote something");
    assert_eq!(dry.doc["dry_run"], true);

    let real = env.run(&["import-archives", &file, "--as-recorded"]);
    assert_eq!(real.code, 0, "{}{}", real.stdout, real.stderr);
    assert_eq!(real.doc["command"], "import-archives");
    assert_eq!(real.doc["dry_run"], false);
    assert_eq!(real.doc["balanced"], true);
    assert_eq!(real.doc["stopped"], Value::Null);
    assert_eq!(real.totals(), [5, 5, 0, 0, 0, 0]);
    assert_eq!(dry.totals(), real.totals());
    assert_eq!(dry.doc["records"], real.doc["records"]);
    assert_eq!(dry.doc["projects"], real.doc["projects"]);
    assert_eq!(real.doc["base"], env.base().to_str().unwrap());
    assert_eq!(real.doc["destination"], json!({"mode": "as-recorded"}));
    let bytes = std::fs::read(&file).unwrap();
    assert_eq!(
        real.doc["inputs"],
        json!([{"path": file, "lines": 5, "sha256": format!("{:x}", Sha256::digest(&bytes))}])
    );
    let project = &real.doc["projects"][0];
    assert_eq!(project["dir"], FIXTURE_DIR);
    assert_eq!(project["project_key"], Value::Null);
    assert_eq!(
        [&project["read"], &project["live"], &project["tombstones"], &project["stored"]],
        [&json!(5), &json!(4), &json!(1), &json!(5)]
    );
    assert_eq!(project["shelves"], json!({"s000001": 5}));
    let dir = env.base().join("context-archives").join(FIXTURE_DIR);
    for entry in real.doc["records"].as_array().unwrap() {
        assert_eq!(entry["outcome"], "stored");
        assert_eq!(entry["input"], 0);
        let id = entry["id"].as_str().unwrap();
        assert!(dir.join("s000001").join(format!("{id}.json")).is_file());
    }

    let stored = env.listing();
    let again = env.run(&["import-archives", &file, "--as-recorded"]);
    assert_eq!(again.code, 0, "{}{}", again.stdout, again.stderr);
    assert_eq!(again.totals(), [5, 0, 5, 0, 0, 0]);
    assert_eq!(env.listing(), stored, "a second run changed a byte");

    // One corrupted line: exit code 1 and one refused entry with its reason.
    let env = Env::new();
    let mut corrupted = fixtures.clone();
    assert!(corrupted[0].contains("What does"));
    corrupted[0] = corrupted[0].replace("What does", "What did");
    let file = env.file("corrupted.jsonl", &corrupted);
    let dry = env.run(&["import-archives", &file, "--as-recorded", "--dry-run"]);
    let real = env.run(&["import-archives", &file, "--as-recorded"]);
    assert_eq!((dry.code, real.code), (1, 1), "{}{}", real.stdout, real.stderr);
    assert_eq!(real.totals(), [5, 4, 0, 0, 0, 1]);
    assert_eq!(dry.totals(), real.totals());
    assert_eq!(real.doc["balanced"], true);
    let refused = real.records("refused");
    assert_eq!(refused.len(), 1);
    assert_eq!(refused[0]["line"], 1);
    let reason = refused[0]["reason"].as_str().unwrap();
    assert!(reason.starts_with("digest does not match content"), "{reason}");
    assert!(!real.stdout.contains("What did"));

    // --cwd: under the key `memory project --cwd` prints for that folder.
    let env = Env::new();
    let repo = env.work().join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::create_dir(repo.join("inner")).unwrap();
    let inner = repo.join("inner");
    let inner = inner.to_str().unwrap();
    let file = env.file("fixtures.jsonl", &fixtures);
    let before = env.listing();
    let asked = env.run(&["project", "--cwd", inner]);
    assert_eq!(asked.code, 0, "{}{}", asked.stdout, asked.stderr);
    assert_eq!(env.listing(), before, "`memory project` wrote something");
    let key = asked.doc["project_key"].as_str().unwrap().to_owned();
    assert_eq!(key, ProjectScope::for_root(&repo).unwrap().key());
    assert_eq!(asked.doc["cwd"], inner);
    assert_eq!(asked.doc["root"], repo.canonicalize().unwrap().to_str().unwrap());
    assert_eq!(asked.doc["project_root_override"], false);
    let dir = project_scope(&key).unwrap();
    assert_eq!(
        asked.doc["archives_dir"],
        env.base().join("context-archives").join(&dir).to_str().unwrap()
    );
    assert_eq!(
        asked.doc["notes_file"],
        env.base()
            .join("memory")
            .join(format!("project-{key}.jsonl"))
            .to_str()
            .unwrap()
    );

    let real = env.run(&["import-archives", &file, "--cwd", inner]);
    assert_eq!(real.code, 0, "{}{}", real.stdout, real.stderr);
    assert_eq!(real.totals(), [5, 5, 0, 0, 0, 0]);
    assert_eq!(
        real.doc["destination"],
        json!({"mode": "cwd", "cwd": inner, "project_key": key, "project_root_override": false})
    );
    assert_eq!(real.doc["projects"][0]["dir"], dir.as_str());
    assert_eq!(real.doc["projects"][0]["project_key"], key.as_str());
    for entry in real.doc["records"].as_array().unwrap() {
        let id = entry["id"].as_str().unwrap();
        let path = archive_file(&env.base(), &key, id).unwrap();
        assert!(path.is_some(), "{id} is not in the folder's project");
    }
    assert!(!env.base().join("context-archives").join(FIXTURE_DIR).exists());
}

/// cut-spec T-A12.
#[test]
fn t_a12_the_gap_as_move_sh_runs_it() {
    const N: usize = 4;
    const K: usize = 3;
    let first = sealed("first", N);
    let new = sealed("new", K);
    let env = Env::new();
    let file_1 = env.file("export-0.jsonl", &lines(&first));
    let mut second = lines(&first);
    second[1] = tombstone_of(&second[1]);
    second.extend(lines(&new));
    let file_2 = env.file("export-1.jsonl", &second);

    let run = env.run(&["import-archives", &file_1, "--project-key", KEY]);
    assert_eq!(run.code, 0, "{}{}", run.stdout, run.stderr);
    assert_eq!(run.totals(), [N as u64, N as u64, 0, 0, 0, 0]);
    assert_eq!(
        run.doc["destination"],
        json!({"mode": "project-key", "project_key": KEY})
    );

    let want = [(2 * N + K) as u64, K as u64, (2 * N - 2) as u64, 1, 1, 0];
    let before = env.listing();
    let dry = env.run(&["import-archives", &file_2, &file_1, "--project-key", KEY, "--dry-run"]);
    assert_eq!(dry.code, 0, "{}{}", dry.stdout, dry.stderr);
    assert_eq!(dry.totals(), want);
    assert_eq!(env.listing(), before, "a dry run wrote something");

    let real = env.run(&["import-archives", &file_2, &file_1, "--project-key", KEY]);
    assert_eq!(real.code, 0, "{}{}", real.stdout, real.stderr);
    assert_eq!(real.totals(), want);
    assert_eq!(real.doc["balanced"], true);
    assert_eq!(dry.doc["records"], real.doc["records"]);
    assert_eq!(dry.doc["projects"], real.doc["projects"]);

    // In the order given: the tombstone is line 2 of the first file named,
    // and the live copy it keeps out is line 2 of the second.
    let forgotten = real.records("forgotten");
    assert_eq!((&forgotten[0]["input"], &forgotten[0]["line"]), (&json!(0), &json!(2)));
    let kept = real.records("kept_forgotten");
    assert_eq!((&kept[0]["input"], &kept[0]["line"]), (&json!(1), &json!(2)));
    assert_eq!(kept[0]["id"], first[1].0.as_str());
    let project = &real.doc["projects"][0];
    assert_eq!(project["project_key"], KEY);
    assert_eq!(project["dir"], project_scope(KEY).unwrap().as_str());
    assert_eq!(project["read"], (2 * N + K) as u64);
    assert_eq!(project["tombstones"], 1);
    assert_eq!(project["messages"], (2 * (2 * N + K - 1)) as u64);

    // The forgotten archive is a tombstone file; nothing went to the
    // project the records were sealed in.
    let path = archive_file(&env.base(), KEY, &first[1].0).unwrap().unwrap();
    let held: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(held["body"], json!({"state": "tombstone"}));
    let source_dir = project_scope(SOURCE_KEY).unwrap();
    assert!(!env.base().join("context-archives").join(source_dir).exists());
}

/// cut-spec T-A13.
#[test]
fn t_a13_beside_a_running_store() {
    let records = sealed("beside", 50);
    let env = Env::new();
    let file = env.file("fifty.jsonl", &lines(&records));
    let base = env.base();

    let stop = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicUsize::new(0));
    let sealer = {
        let (base, stop, done) = (base.clone(), stop.clone(), done.clone());
        std::thread::spawn(move || {
            let store = ArchiveStore::new(&base, KEY, "a-running-chat").unwrap();
            let mut ids = Vec::new();
            while !stop.load(Ordering::SeqCst) && ids.len() < 400 {
                let message = Arc::new(
                    json!({"role": "user", "content": format!("{MARKER} live {}", ids.len())}),
                );
                ids.push(store.seal(&[message], "live").unwrap().id);
                done.store(ids.len(), Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            ids
        })
    };
    while done.load(Ordering::SeqCst) < 2 {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let at_start = done.load(Ordering::SeqCst);
    let run = env.run(&["import-archives", &file, "--project-key", KEY]);
    let at_end = done.load(Ordering::SeqCst);
    stop.store(true, Ordering::SeqCst);
    let live = sealer.join().unwrap();

    assert_eq!(run.code, 0, "{}{}", run.stdout, run.stderr);
    assert_eq!(run.totals(), [50, 50, 0, 0, 0, 0]);
    assert!(at_end > at_start, "no seal happened while the import ran");
    for (id, _) in &records {
        assert!(archive_file(&base, KEY, id).unwrap().is_some(), "import {id} is not on disk");
    }
    for id in &live {
        assert!(archive_file(&base, KEY, id).unwrap().is_some(), "seal {id} is not on disk");
    }
}

/// cut-spec T-A15.
#[test]
fn t_a15_the_destination() {
    let env = Env::new();
    let file = env.file("records.jsonl", &lines(&sealed("destination", 2)));
    let before = env.listing();

    // No destination, or two: clap refuses; nothing is read (the input that
    // is not there is never opened) and nothing is printed or made.
    let missing = env.work().join("not-there.jsonl");
    let missing = missing.to_str().unwrap();
    for command in ["import-archives", "import-notes"] {
        for flags in [&[][..], &["--as-recorded", "--project-key", KEY][..]] {
            let mut args = vec![command, missing];
            args.extend_from_slice(flags);
            let out = env.run(&args);
            assert_eq!(out.code, 2, "{command} {flags:?}: {}", out.stderr);
            assert!(out.stdout.is_empty());
            assert!(out.stderr.contains("--as-recorded"), "{}", out.stderr);
            assert!(!out.stderr.contains("cannot open"), "{}", out.stderr);
        }
        // A key that is not a project key, and the user-wide key.
        for key in ["p0cc924a8dff9236", "P0CC924A8DFF92368", "p0000000000000000"] {
            let out = env.run(&[command, &file, "--project-key", key]);
            assert_eq!(out.code, 2, "{command} {key}: {}", out.stderr);
            assert!(out.stdout.is_empty());
        }
        let out = env.run(&[command, &file, "--cwd", missing]);
        assert_eq!(out.code, 2, "{}", out.stderr);
        assert!(out.stdout.is_empty());
    }
    assert_eq!(env.listing(), before);

    // With SYNAPS_PROJECT_ROOT every folder gives the override's project.
    let root = env.work().join("override-root");
    let other = env.work().join("other");
    for dir in [&root, &other] {
        std::fs::create_dir(dir).unwrap();
    }
    let other = other.to_str().unwrap();
    let key = ProjectScope::for_root(&root).unwrap().key().to_owned();
    assert_ne!(key, ProjectScope::for_root(Path::new(other)).unwrap().key());

    // An import refuses --cwd then (core-a5-review finding 4): the folder
    // would not be looked at, and one that is not there would pass.
    let before = env.listing();
    for command in ["import-archives", "import-notes"] {
        for cwd in [other, missing] {
            for dry_run in [true, false] {
                let mut args = vec![command, file.as_str(), "--cwd", cwd];
                if dry_run {
                    args.push("--dry-run");
                }
                let mut command = env.command(&args);
                command.env("SYNAPS_PROJECT_ROOT", &root);
                let run = finish(command, None);
                assert_eq!(run.code, 2, "{}{}", run.stdout, run.stderr);
                assert!(run.stdout.is_empty(), "{}", run.stdout);
                assert!(run.stderr.contains("SYNAPS_PROJECT_ROOT"), "{}", run.stderr);
            }
        }
    }
    assert_eq!(env.listing(), before);
    // The other two destinations do not look at it.
    let mut command = env.command(&["import-archives", &file, "--project-key", KEY]);
    command.env("SYNAPS_PROJECT_ROOT", &root);
    let run = finish(command, None);
    assert_eq!(run.code, 0, "{}{}", run.stdout, run.stderr);
    assert_eq!(run.doc["projects"][0]["dir"], project_scope(KEY).unwrap().as_str());
    // Without the variable the folder is its own project.
    let run = env.run(&["import-archives", &file, "--cwd", other]);
    assert_eq!(run.code, 0, "{}{}", run.stdout, run.stderr);
    assert_eq!(run.doc["destination"]["project_root_override"], false);
    let own = ProjectScope::for_root(Path::new(other)).unwrap().key().to_owned();
    assert_eq!(run.doc["destination"]["project_key"], own.as_str());

    let mut command = env.command(&["project", "--cwd", other]);
    command.env("SYNAPS_PROJECT_ROOT", &root);
    let asked = finish(command, None);
    assert_eq!(asked.code, 0, "{}{}", asked.stdout, asked.stderr);
    assert_eq!(asked.doc["project_root_override"], true);
    assert_eq!(asked.doc["project_key"], key.as_str());
    assert_eq!(asked.doc["root"], root.canonicalize().unwrap().to_str().unwrap());

    // Without it the folder is its own project, and the report says so.
    let plain = env.run(&["project", "--cwd", other]);
    assert_eq!(plain.doc["project_root_override"], false);
    assert_ne!(plain.doc["project_key"], key.as_str());

    // No --cwd: the folder the command runs in.
    let here = env.run(&["project"]);
    assert_eq!(here.code, 0, "{}", here.stderr);
    assert_eq!(
        here.doc["project_key"],
        ProjectScope::discover_with_override(&env.work(), None)
            .unwrap()
            .key()
    );
}

/// core-a5-review finding 3.
#[test]
fn the_base_is_an_absolute_path_and_a_real_run_does_not_create_it() {
    let env = Env::new();
    let archives = env.file("records.jsonl", &lines(&sealed("base", 1)));
    let notes = env.file("notes.jsonl", &[note("mem-1", KEY, "one")]);
    let before = env.listing();
    // The command runs in `work`: a relative base would land there.
    type Change = fn(&mut Command);
    let cases: [(&str, Change); 4] = [
        ("an empty SYNAPS_BASE_DIR", |command| {
            command.env("SYNAPS_BASE_DIR", "");
        }),
        ("a relative SYNAPS_BASE_DIR", |command| {
            command.env("SYNAPS_BASE_DIR", "base");
        }),
        ("no SYNAPS_BASE_DIR and no HOME", |command| {
            command.env_remove("SYNAPS_BASE_DIR").env_remove("HOME").env_remove("USERPROFILE");
        }),
        ("a SYNAPS_BASE_DIR that is not UTF-8", |command| {
            use std::os::unix::ffi::OsStrExt;
            command.env("SYNAPS_BASE_DIR", std::ffi::OsStr::from_bytes(b"/tmp/\xff"));
        }),
    ];
    for (name, change) in cases {
        for (action, file) in [("import-archives", &archives), ("import-notes", &notes)] {
            for dry_run in [true, false] {
                let mut args = vec![action, file.as_str(), "--project-key", KEY];
                if dry_run {
                    args.push("--dry-run");
                }
                let mut command = env.command(&args);
                change(&mut command);
                let run = finish(command, None);
                assert_eq!(run.code, 2, "{name}, {action}: {}{}", run.stdout, run.stderr);
                assert!(run.stdout.is_empty(), "{name}, {action}: {}", run.stdout);
                assert!(run.stderr.contains("nothing was imported"), "{name}: {}", run.stderr);
                assert_eq!(env.listing(), before, "{name}, {action}: something was written");
            }
        }
    }

    // A base that is not there, and one that is a file: the dry run reports
    // what it would do and says that the real run would stop; the real run
    // stops and makes nothing.
    let no_base = Env::without_base();
    let a_file = Env::without_base();
    std::fs::write(a_file.base(), "").unwrap();
    for env in [&no_base, &a_file] {
        let archives = env.file("records.jsonl", &lines(&sealed("base", 1)));
        let notes = env.file("notes.jsonl", &[note("mem-1", KEY, "one")]);
        let before = env.listing();
        for (action, file) in [("import-archives", &archives), ("import-notes", &notes)] {
            let real = env.run(&[action, file, "--project-key", KEY]);
            assert_eq!(real.code, 2, "{action}: {}{}", real.stdout, real.stderr);
            assert!(real.stdout.is_empty(), "{}", real.stdout);
            assert!(real.stderr.contains("does not create it"), "{}", real.stderr);
            assert_eq!(env.listing(), before, "{action}: a real run made the base");
        }
    }
    for (action, file) in [("import-archives", &archives), ("import-notes", &notes)] {
        let file = no_base.work().join(Path::new(file).file_name().unwrap());
        let before = no_base.listing();
        let dry = no_base.run(&[action, file.to_str().unwrap(), "--project-key", KEY, "--dry-run"]);
        assert_eq!(dry.code, 0, "{action}: {}{}", dry.stdout, dry.stderr);
        assert_eq!(dry.totals(), [1, 1, 0, 0, 0, 0]);
        assert_eq!(dry.doc["base"], no_base.base().to_str().unwrap());
        assert!(dry.stderr.contains("the real run would stop"), "{}", dry.stderr);
        assert_eq!(no_base.listing(), before, "{action}: a dry run made something");
    }
}

#[test]
fn a_line_over_the_size_limit_is_refused_and_the_run_goes_on() {
    let records = sealed("long", 2);
    let env = Env::new();
    let path = env.work().join("long.jsonl");
    let mut bytes = records[0].1.clone().into_bytes();
    bytes.push(b'\n');
    // 16 MiB + 4 KiB is the longest line read; this one is a byte longer.
    bytes.extend(std::iter::repeat_n(b'x', 16 * 1024 * 1024 + 4096 + 1));
    bytes.extend_from_slice(b"\n\n");
    bytes.extend_from_slice(records[1].1.as_bytes());
    std::fs::write(&path, &bytes).unwrap();
    let file = path.to_str().unwrap();

    for dry_run in [true, false] {
        let mut args = vec!["import-archives", file, "--project-key", KEY];
        if dry_run {
            args.push("--dry-run");
        }
        let run = env.run(&args);
        assert_eq!(run.code, 1, "{}", run.stderr);
        assert_eq!(run.totals(), [3, 2, 0, 0, 0, 1]);
        assert_eq!(run.doc["balanced"], true);
        assert_eq!(run.doc["stopped"], Value::Null);
        let refused = run.records("refused");
        assert_eq!(refused.len(), 1);
        assert_eq!(refused[0]["line"], 2);
        assert_eq!(refused[0]["id"], Value::Null);
        assert_eq!(refused[0]["reason"], "the line is longer than 16781312 bytes");
        // Line 3 is blank and counted nowhere; the record after it is line 4.
        assert_eq!(run.records("stored")[1]["line"], 4);
        assert_eq!(run.doc["inputs"][0]["lines"], 3);
        assert_eq!(
            run.doc["inputs"][0]["sha256"],
            format!("{:x}", Sha256::digest(&bytes))
        );
        assert!(run.stdout.len() < 64 * 1024);
    }
}

#[test]
fn an_input_that_cannot_be_opened_stops_the_run_before_anything_is_written() {
    let env = Env::new();
    let file = env.file("records.jsonl", &lines(&sealed("stop", 2)));
    let notes = env.file("notes.jsonl", &[note("mem-1", KEY, "one")]);
    let missing = env.work().join("not-there.jsonl");
    // Not regular files (core-a5-review finding 5). The directory comes
    // first: opening it does not fail, reading it does. A FIFO would block
    // at the open and /dev/zero never ends a line.
    let directory = env.work().join("a-directory");
    std::fs::create_dir(&directory).unwrap();
    let fifo = env.work().join("a-fifo");
    let made = Command::new("mkfifo").arg(&fifo).status().unwrap();
    assert!(made.success());
    let link = env.work().join("a-link-to-a-device");
    std::os::unix::fs::symlink("/dev/zero", &link).unwrap();
    let cases = [
        (missing.as_path(), "No such file"),
        (directory.as_path(), "not a regular file"),
        (fifo.as_path(), "not a regular file"),
        (Path::new("/dev/zero"), "not a regular file"),
        (link.as_path(), "not a regular file"),
    ];
    let before = env.listing();
    for (bad, why) in cases {
        let bad = bad.to_str().unwrap();
        for (command, good) in [("import-archives", &file), ("import-notes", &notes)] {
            let run = env.run(&[command, good, bad, "--project-key", KEY]);
            assert_eq!(run.code, 2, "{bad}: {}{}", run.stdout, run.stderr);
            assert_eq!(run.totals(), [0; 6], "{bad}");
            assert_eq!(run.doc["stopped"]["input"], 1);
            assert_eq!(run.doc["stopped"]["line"], Value::Null);
            assert_eq!(run.doc["stopped"]["id"], Value::Null);
            let error = run.doc["stopped"]["error"].as_str().unwrap();
            assert!(error.starts_with("cannot open the input"), "{bad}: {error}");
            assert!(error.contains(why), "{bad}: {error}");
            assert!(error.ends_with("nothing was imported"), "{bad}: {error}");
            assert_eq!(run.doc["records"], json!([]));
            assert_eq!(env.listing(), before, "{bad}: a stopped run wrote something");
        }
    }
    // A link to a regular file is an input.
    let linked = env.work().join("a-link-to-a-file");
    std::os::unix::fs::symlink(&file, &linked).unwrap();
    let run = env.run(&["import-archives", linked.to_str().unwrap(), "--project-key", KEY]);
    assert_eq!((run.code, run.totals()), (0, [2, 2, 0, 0, 0, 0]), "{}", run.stderr);
}

#[test]
fn a_store_that_cannot_answer_stops_the_run_at_that_record() {
    let records = sealed("unreadable", 3);
    let env = Env::new();
    let file = env.file("records.jsonl", &lines(&records));
    let first = env.file("first.jsonl", &lines(&records[..1]));
    let run = env.run(&["import-archives", &first, "--project-key", KEY]);
    assert_eq!(run.code, 0, "{}{}", run.stdout, run.stderr);

    // Something that is not a record under the second record's name: the
    // store cannot tell whether that id is there.
    let shelf = env
        .base()
        .join("context-archives")
        .join(project_scope(KEY).unwrap())
        .join("s000001");
    let stray = shelf.join(format!("{}.json", records[1].0));
    std::fs::write(&stray, "{\"cut").unwrap();
    std::fs::set_permissions(&stray, std::fs::Permissions::from_mode(0o600)).unwrap();

    for dry_run in [true, false] {
        let mut args = vec!["import-archives", file.as_str(), "--project-key", KEY];
        if dry_run {
            args.push("--dry-run");
        }
        let before = env.listing();
        let run = env.run(&args);
        assert_eq!(run.code, 2, "{}{}", run.stdout, run.stderr);
        assert_eq!(run.doc["stopped"]["input"], 0);
        assert_eq!(run.doc["stopped"]["line"], 2);
        // The store's error does not name the file. The command adds the
        // id and the project directory: the file is <id>.json on a shelf
        // of that directory (core-a5-review finding 7).
        let error = run.doc["stopped"]["error"].as_str().unwrap();
        assert!(error.contains(&format!("{}.json", records[1].0)), "{error}");
        assert!(error.contains(shelf.parent().unwrap().to_str().unwrap()), "{error}");
        assert_eq!(run.doc["stopped"]["id"], records[1].0.as_str());
        // The line the run stopped at was read and has no outcome.
        assert_eq!(run.totals(), [2, 0, 1, 0, 0, 0]);
        assert_eq!(run.doc["balanced"], false);
        assert_eq!(run.doc["inputs"][0]["lines"], 2);
        assert_eq!(run.doc["inputs"][0]["sha256"], Value::Null);
        assert_eq!(run.doc["records"].as_array().unwrap().len(), 1);
        // The record after the stop was not imported.
        assert_eq!(env.listing(), before);
        assert!(!shelf.join(format!("{}.json", records[2].0)).exists());
    }
}

/// core-a5-review finding 2.
#[test]
fn a_forget_that_did_not_reach_every_copy_exits_1_and_is_named() {
    let records = sealed("incomplete", 2);
    let env = Env::new();
    let file = env.file("records.jsonl", &lines(&records));
    let run = env.run(&["import-archives", &file, "--project-key", KEY]);
    assert_eq!(run.code, 0, "{}{}", run.stdout, run.stderr);
    assert_eq!(run.doc["forgets_not_complete"], json!({"count": 0, "ids": []}));

    // A file that names the first record's content and is not a record: it
    // may be a damaged copy, and a forget leaves it as it is.
    let record: Value = serde_json::from_str(&records[0].1).unwrap();
    let shelf = env
        .base()
        .join("context-archives")
        .join(project_scope(KEY).unwrap())
        .join("s000001");
    let stray = shelf.join(format!("{}.json", "f".repeat(32)));
    std::fs::write(&stray, json!({"digest": record["digest"]}).to_string()).unwrap();
    std::fs::set_permissions(&stray, std::fs::Permissions::from_mode(0o600)).unwrap();

    let forget = env.file("forget.jsonl", &[tombstone_of(&records[0].1), records[1].1.clone()]);
    let run = env.run(&["import-archives", &forget, "--project-key", KEY]);
    // Nothing was refused and the run did not stop: the exit code and
    // `forgets_not_complete` are what say that it is not all done.
    assert_eq!(run.totals(), [2, 0, 1, 1, 0, 0], "{}", run.stdout);
    assert_eq!(run.doc["balanced"], true);
    assert_eq!(run.doc["stopped"], Value::Null);
    let entry = &run.doc["records"][0];
    assert_eq!(entry["outcome"], "forgotten");
    let unread = entry["unread"].as_str().unwrap();
    assert!(unread.starts_with("1 place(s) could not be read"), "{unread}");
    assert_eq!(run.doc["records"][1].get("unread"), None);
    assert_eq!(
        run.doc["forgets_not_complete"],
        json!({"count": 1, "ids": [records[0].0]})
    );
    assert!(run.stderr.contains("may not have reached every copy"), "{}", run.stderr);
    assert_eq!(run.code, 1, "{}{}", run.stdout, run.stderr);

    // Notes have no such thing, and no such field.
    let notes = env.file("notes.jsonl", &[note("mem-1", KEY, "one")]);
    let run = env.run(&["import-notes", &notes, "--project-key", KEY, "--dry-run"]);
    assert_eq!(run.doc.get("forgets_not_complete"), None);
}

#[test]
fn a_refused_record_makes_no_project_directory() {
    let env = Env::new();
    let mut line = sealed("refused", 1).remove(0).1;
    line = line.replace("question", "QUESTION");
    let file = env.file("bad.jsonl", &[line, "not json".to_owned()]);
    let before = env.listing();
    for flags in [&["--as-recorded"][..], &["--project-key", KEY][..]] {
        let mut args = vec!["import-archives", file.as_str()];
        args.extend_from_slice(flags);
        let run = env.run(&args);
        assert_eq!(run.code, 1, "{}{}", run.stdout, run.stderr);
        assert_eq!(run.totals(), [2, 0, 0, 0, 0, 2]);
        assert_eq!(run.records("refused").len(), 2);
    }
    assert_eq!(env.listing(), before, "a refused record left something behind");
}

// ─── Notes ──────────────────────────────────────────────────────────────────

#[test]
fn notes_a_dry_run_then_the_real_run_then_the_same_again() {
    let env = Env::new();
    let text = format!(
        "{}\n\n   \n{}\r\n{}\n{}",
        note("mem-1", SOURCE_KEY, "one"),
        note("mem-2", SOURCE_KEY, "two"),
        note_tombstone("mem-9"),
        note("mem-3", "p0000000000000000", "three"),
    );
    let path = env.work().join("notes.jsonl");
    std::fs::write(&path, &text).unwrap();
    let file = path.to_str().unwrap();
    let notes_file = env.base().join("memory").join(format!("project-{KEY}.jsonl"));

    let before = env.listing();
    let dry = env.run(&["import-notes", file, "--project-key", KEY, "--dry-run"]);
    assert_eq!(dry.code, 0, "{}{}", dry.stdout, dry.stderr);
    assert_eq!(env.listing(), before, "a dry run wrote something");

    let real = env.run(&["import-notes", file, "--project-key", KEY]);
    assert_eq!(real.code, 0, "{}{}", real.stdout, real.stderr);
    assert_eq!(real.doc["command"], "import-notes");
    // Two blank lines are counted nowhere.
    assert_eq!(real.totals(), [4, 4, 0, 0, 0, 0]);
    assert_eq!(dry.totals(), real.totals());
    assert_eq!(dry.doc["records"], real.doc["records"]);
    assert_eq!(dry.doc["projects"], real.doc["projects"]);
    assert_eq!(dry.doc["dry_run"], true);
    assert_eq!(real.doc["balanced"], true);
    assert_eq!(real.doc["tombstones_not_applied"], json!({"count": 0, "ids": []}));
    assert_eq!(
        real.doc["inputs"],
        json!([{"path": file, "lines": 4,
                "sha256": format!("{:x}", Sha256::digest(text.as_bytes()))}])
    );
    assert_eq!(
        real.doc["projects"],
        json!([{
            "file": notes_file.to_str().unwrap(), "project_key": KEY,
            "read": 4, "live": 3, "tombstones": 1,
            "stored": 4, "already_present": 0, "forgotten": 0, "kept_forgotten": 0, "refused": 0
        }])
    );
    assert_eq!(real.doc["records"][3]["line"], 6);
    assert_eq!(real.doc["records"][2]["tombstone"], true);
    let held = std::fs::read_to_string(&notes_file).unwrap();
    assert_eq!(held.lines().count(), 4);
    assert!(held.contains(&format!("\"project\":\"{KEY}\"")));
    assert!(!held.contains(SOURCE_KEY));

    let stored = env.listing();
    let again = env.run(&["import-notes", file, "--project-key", KEY]);
    assert_eq!(again.code, 0, "{}{}", again.stdout, again.stderr);
    assert_eq!(again.totals(), [4, 0, 4, 0, 0, 0]);
    assert_eq!(env.listing(), stored, "a second run changed a byte");
}

#[test]
fn standard_input_is_an_input_named_by_a_dash() {
    let env = Env::new();
    let file = env.file("first.jsonl", &[note("mem-1", KEY, "one")]);
    let piped = format!("{}\n{}\n", note("mem-2", KEY, "two"), note("mem-1", KEY, "one"));
    let run = finish(
        env.command(&["import-notes", &file, "-", "--as-recorded"]),
        Some(piped.as_bytes()),
    );
    assert_eq!(run.code, 0, "{}{}", run.stdout, run.stderr);
    assert_eq!(run.totals(), [3, 2, 1, 0, 0, 0]);
    assert_eq!(run.doc["inputs"][1]["path"], "-");
    assert_eq!(run.doc["inputs"][1]["lines"], 2);
    assert_eq!(
        run.doc["inputs"][1]["sha256"],
        format!("{:x}", Sha256::digest(piped.as_bytes()))
    );
    assert_eq!(run.doc["records"][2]["input"], 1);
    assert_eq!(run.doc["records"][2]["outcome"], "already_present");

    let archives = lines(&sealed("piped", 2)).join("\n");
    let run = finish(
        env.command(&["import-archives", "-", "--project-key", KEY]),
        Some(archives.as_bytes()),
    );
    assert_eq!(run.code, 0, "{}{}", run.stdout, run.stderr);
    assert_eq!(run.totals(), [2, 2, 0, 0, 0, 0]);

    let twice = env.run(&["import-notes", "-", "-", "--as-recorded"]);
    assert_eq!(twice.code, 2);
    assert!(twice.stdout.is_empty());
}

#[test]
fn a_real_notes_import_holds_one_lock_and_a_second_one_stops() {
    let env = Env::new();
    let file = env.file("notes.jsonl", &[note("mem-1", KEY, "one")]);
    let memory = env.base().join("memory");
    std::fs::create_dir_all(&memory).unwrap();
    let lock_path = memory.join(".import.lock");
    let held = agent_core::core::lock_file::try_lock(std::fs::File::create(&lock_path).unwrap())
        .unwrap()
        .expect("the test takes the lock first");

    let before = env.listing();
    let run = env.run(&["import-notes", &file, "--project-key", KEY]);
    assert_eq!(run.code, 2, "{}{}", run.stdout, run.stderr);
    assert_eq!(run.doc["stopped"]["error"], "another import is running");
    assert_eq!(run.doc["stopped"]["input"], Value::Null);
    assert_eq!(run.totals(), [0; 6]);
    assert!(run.stderr.contains("another import is running"));
    assert_eq!(env.listing(), before, "a run without the lock wrote something");

    // A dry run takes no lock.
    let dry = env.run(&["import-notes", &file, "--project-key", KEY, "--dry-run"]);
    assert_eq!(dry.code, 0, "{}{}", dry.stdout, dry.stderr);
    assert_eq!(dry.totals(), [1, 1, 0, 0, 0, 0]);
    assert_eq!(env.listing(), before);

    drop(held);
    let run = env.run(&["import-notes", &file, "--project-key", KEY]);
    assert_eq!(run.code, 0, "{}{}", run.stdout, run.stderr);
    assert_eq!(run.totals(), [1, 1, 0, 0, 0, 0]);

    // A base with no memory folder: the real run makes it and the lock file,
    // both private.
    let env = Env::new();
    let file = env.file("notes.jsonl", &[note("mem-1", KEY, "one")]);
    let run = env.run(&["import-notes", &file, "--project-key", KEY]);
    assert_eq!(run.code, 0, "{}{}", run.stdout, run.stderr);
    let memory = env.base().join("memory");
    let mode = |path: &Path| path.metadata().unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&memory), 0o700);
    assert_eq!(mode(&memory.join(".import.lock")), 0o600);
}

/// core-a5-review finding 6.
#[test]
fn the_import_lock_is_never_opened_through_a_link() {
    let env = Env::new();
    let file = env.file("notes.jsonl", &[note("mem-1", KEY, "one")]);
    let memory = env.base().join("memory");
    std::fs::create_dir(&memory).unwrap();
    let lock = memory.join(".import.lock");
    let dangling = env.work().join("made-through-the-link");
    let existing = env.work().join("somebody-elses-file");
    std::fs::write(&existing, "").unwrap();
    for target in [&dangling, &existing] {
        std::os::unix::fs::symlink(target, &lock).unwrap();
        let before = env.listing();
        let run = env.run(&["import-notes", &file, "--project-key", KEY]);
        assert_eq!(run.code, 2, "{}{}", run.stdout, run.stderr);
        let error = run.doc["stopped"]["error"].as_str().unwrap();
        assert!(error.starts_with("cannot open the import lock"), "{error}");
        assert!(error.ends_with("nothing was written"), "{error}");
        assert_eq!(run.totals(), [0; 6]);
        assert_eq!(env.listing(), before, "the link was followed, or a note was written");
        assert!(!dangling.exists());
        std::fs::remove_file(&lock).unwrap();
    }
}

#[test]
fn a_note_tombstone_that_is_not_applied_is_counted_named_and_exits_1() {
    let env = Env::new();
    let input = [
        note_tombstone("mem-gone"),
        note("mem-gone", KEY, "forgotten at the source"),
        note("mem-kept", KEY, "kept"),
        note("has space", KEY, "a bad id"),
    ];
    let file = env.file("notes.jsonl", &input);

    // As recorded the tombstone line names no project and no project's
    // notes hold its id: refused, and so is the note it forgot.
    let before = env.listing();
    let dry = env.run(&["import-notes", &file, "--as-recorded", "--dry-run"]);
    assert_eq!(env.listing(), before);
    let real = env.run(&["import-notes", &file, "--as-recorded"]);
    assert_eq!((dry.code, real.code), (1, 1), "{}{}", real.stdout, real.stderr);
    assert_eq!(real.totals(), [4, 1, 0, 0, 0, 3]);
    assert_eq!(dry.totals(), real.totals());
    assert_eq!(dry.doc["records"], real.doc["records"]);
    assert_eq!(
        real.doc["tombstones_not_applied"],
        json!({"count": 1, "ids": ["mem-gone"]})
    );
    assert!(real.stderr.contains("1 forgotten id(s) not applied"), "{}", real.stderr);
    assert!(real.stderr.contains("mem-gone"));
    let records = real.doc["records"].as_array().unwrap();
    assert_eq!(
        records[0],
        json!({"input": 0, "line": 1, "id": "mem-gone", "outcome": "refused", "tombstone": true,
               "reason": "a tombstone line names no project and no project's notes hold its id; \
                          pass --cwd or --project-key"})
    );
    assert_eq!(records[1]["reason"], "its tombstone in this run could not be filed");
    assert_eq!(records[2]["outcome"], "stored");
    assert_eq!(records[3]["reason"], "invalid id");
    assert_eq!(real.doc["destination"], json!({"mode": "as-recorded"}));
    // A refused line that reached no project is in the total only.
    assert_eq!(real.doc["projects"].as_array().unwrap().len(), 1);
    assert_eq!(real.doc["projects"][0]["read"], 1);

    // With a target the tombstone is filed, and keeps its note out.
    let env = Env::new();
    let file = env.file("notes.jsonl", &input[..3]);
    let real = env.run(&["import-notes", &file, "--project-key", KEY]);
    assert_eq!(real.code, 0, "{}{}", real.stdout, real.stderr);
    assert_eq!(real.totals(), [3, 2, 0, 0, 1, 0]);
    assert_eq!(real.doc["tombstones_not_applied"]["count"], 0);
    assert!(real.stderr.is_empty(), "{}", real.stderr);
}

/// core-a5-review finding 1: the note is live on disk from an earlier
/// command, in a file no earlier line of this command has named.
#[test]
fn a_note_tombstone_as_recorded_finds_the_note_an_earlier_command_stored() {
    const NOT_APPLIED: &str = "a tombstone line names no project and no project's notes hold \
                               its id; pass --cwd or --project-key";
    let notes_of = |env: &Env, key: &str| {
        let path = env.base().join("memory").join(format!("project-{key}.jsonl"));
        std::fs::read_to_string(path).unwrap_or_default()
    };

    let env = Env::new();
    let notes = env.file(
        "notes.jsonl",
        &[note("mem-a", KEY, "one"), note("mem-b", SOURCE_KEY, "two")],
    );
    let run = env.run(&["import-notes", &notes, "--as-recorded"]);
    assert_eq!((run.code, run.totals()), (0, [2, 2, 0, 0, 0, 0]), "{}", run.stderr);

    // The tombstone is the first line of its command. A note for another
    // project follows it; a tombstone of an id no file holds ends it.
    let later = env.file(
        "later.jsonl",
        &[note_tombstone("mem-a"), note("mem-c", SOURCE_KEY, "three"), note_tombstone("mem-zz")],
    );
    let before = env.listing();
    let dry = env.run(&["import-notes", &later, "--as-recorded", "--dry-run"]);
    assert_eq!(env.listing(), before, "a dry run wrote something");
    let real = env.run(&["import-notes", &later, "--as-recorded"]);
    assert_eq!((dry.code, real.code), (1, 1), "{}{}", real.stdout, real.stderr);
    assert_eq!(real.totals(), [3, 1, 0, 1, 0, 1]);
    assert_eq!(dry.totals(), real.totals());
    assert_eq!(dry.doc["records"], real.doc["records"]);
    assert_eq!(dry.doc["projects"], real.doc["projects"]);
    assert_eq!(dry.doc["tombstones_not_applied"], real.doc["tombstones_not_applied"]);
    assert_eq!(
        real.doc["records"][0],
        json!({"input": 0, "line": 1, "id": "mem-a", "outcome": "forgotten", "tombstone": true})
    );
    assert_eq!(real.doc["records"][2]["reason"], NOT_APPLIED);
    assert_eq!(real.doc["tombstones_not_applied"], json!({"count": 1, "ids": ["mem-zz"]}));
    assert_eq!(real.doc["projects"][0]["project_key"], KEY);
    assert_eq!(notes_of(&env, KEY).lines().last(), Some(note_tombstone("mem-a").as_str()));
    assert!(!notes_of(&env, SOURCE_KEY).contains("tombstone"));
    // The note stays forgotten, and the tombstone is there already.
    let again = env.run(&["import-notes", &notes, &later, "--as-recorded"]);
    assert_eq!(again.totals(), [5, 0, 3, 0, 1, 1], "{}", again.stdout);

    // The other order over two commands: the tombstone first, into a base
    // that holds its id nowhere. It is refused and named, exit code 1; no
    // notes file is made. Nothing of that outlives the command, so the note
    // is stored by the next one. The tombstone's file again then forgets
    // it: the two commands settle, in either order, when both are run again.
    let env = Env::new();
    let tombstones = env.file("tombstones.jsonl", &[note_tombstone("mem-a")]);
    let notes = env.file("notes.jsonl", &[note("mem-a", KEY, "one")]);
    let first = env.run(&["import-notes", &tombstones, "--as-recorded"]);
    assert_eq!((first.code, first.totals()), (1, [1, 0, 0, 0, 0, 1]), "{}", first.stderr);
    assert_eq!(first.doc["tombstones_not_applied"], json!({"count": 1, "ids": ["mem-a"]}));
    assert_eq!(first.doc["records"][0]["reason"], NOT_APPLIED);
    assert!(first.stderr.contains("mem-a"), "{}", first.stderr);
    assert_eq!(notes_of(&env, KEY), "");
    let second = env.run(&["import-notes", &notes, "--as-recorded"]);
    assert_eq!((second.code, second.totals()), (0, [1, 1, 0, 0, 0, 0]), "{}", second.stderr);

    let before = env.listing();
    let dry = env.run(&["import-notes", &tombstones, "--as-recorded", "--dry-run"]);
    assert_eq!(env.listing(), before, "a dry run wrote something");
    let first = env.run(&["import-notes", &tombstones, "--as-recorded"]);
    assert_eq!((first.code, first.totals()), (0, [1, 0, 0, 1, 0, 0]), "{}", first.stderr);
    assert_eq!((dry.code, dry.totals()), (first.code, first.totals()));
    assert_eq!(dry.doc["records"], first.doc["records"]);
    assert_eq!(first.doc["tombstones_not_applied"]["count"], 0);
    let second = env.run(&["import-notes", &notes, "--as-recorded"]);
    assert_eq!((second.code, second.totals()), (0, [1, 0, 0, 0, 1, 0]), "{}", second.stderr);
    assert_eq!(notes_of(&env, KEY).lines().count(), 2);
}

#[test]
fn memory_project_creates_nothing_and_names_the_file_an_import_writes() {
    let env = Env::new();
    let work = env.work();
    let before = env.listing();
    let asked = env.run(&["project", "--cwd", work.to_str().unwrap()]);
    assert_eq!(asked.code, 0, "{}", asked.stderr);
    assert_eq!(env.listing(), before);
    let keys: Vec<&str> = asked.doc.as_object().unwrap().keys().map(String::as_str).collect();
    let mut want = [
        "cwd",
        "root",
        "project_key",
        "archives_dir",
        "notes_file",
        "project_root_override",
    ];
    want.sort();
    let mut got = keys.clone();
    got.sort();
    assert_eq!(got, want);
    // In the order of the spec's example.
    assert!(asked.stdout.find("\"cwd\"").unwrap() < asked.stdout.find("\"root\"").unwrap());

    let key = asked.doc["project_key"].as_str().unwrap();
    let file = env.file("notes.jsonl", &[note("mem-1", SOURCE_KEY, "one")]);
    let run = env.run(&["import-notes", &file, "--cwd", work.to_str().unwrap()]);
    assert_eq!(run.code, 0, "{}{}", run.stdout, run.stderr);
    assert_eq!(run.doc["destination"]["project_key"], key);
    assert_eq!(run.doc["projects"][0]["file"], asked.doc["notes_file"]);
    assert!(Path::new(asked.doc["notes_file"].as_str().unwrap()).is_file());

    let missing = env.run(&["project", "--cwd", work.join("not-there").to_str().unwrap()]);
    assert_eq!(missing.code, 2);
    assert!(missing.stdout.is_empty());
}

#[test]
fn totals_come_first_in_the_document() {
    let env = Env::new();
    let file = env.file("records.jsonl", &lines(&sealed("order", 1)));
    let run = env.run(&["import-archives", &file, "--project-key", KEY, "--dry-run"]);
    let at = |key: &str| {
        run.stdout
            .find(&format!("\n  \"{key}\""))
            .unwrap_or_else(|| panic!("no {key} in {}", run.stdout))
    };
    let order = [
        "command",
        "dry_run",
        "base",
        "destination",
        "inputs",
        "total",
        "balanced",
        "stopped",
        "projects",
        "records",
    ];
    for pair in order.windows(2) {
        assert!(at(pair[0]) < at(pair[1]), "{} is after {}", pair[0], pair[1]);
    }
    assert_eq!(run.doc["records"][0]["shelf"], "s000001");
}
