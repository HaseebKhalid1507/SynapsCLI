//! Freezes version 1 of the context archive file format, as published in
//! `docs/extensions/context-archives.md`.
//!
//! Extensions read these files directly, so the record layout, the file
//! naming and the markers the store writes are a contract. Each test here
//! fails when the store would write, or stop reading, something that the
//! document and its fixtures do not describe, or when the document, its
//! examples and its reference reader stop agreeing with the fixtures.
//!
//! A failure is never a reason to edit a fixture by hand. The fixtures are
//! real files and they are version 1:
//!
//! - `000.json` was written by the store as it was before shelves existed
//!   (origin/dev `c8ee5623`), for [`legacy_window`]. Its id is random, as
//!   every id was then, so it cannot be written again; it is only read.
//! - the files in `s000001/` are what today's store writes for
//!   [`live_window`], [`forgotten_window`], [`reduced_window`] and
//!   [`json_text_window`]. After a
//!   change that the document allows without a new `version` (growth inside
//!   `message`), print them again with
//!   `cargo test -p synaps-core --test context_archive_format -- --ignored --nocapture print_fixtures`
//!   and describe the change in the document.
//! - one value in them is not what a store writes again: `created_ms`, the
//!   time of the write. The tests check that the store writes one and that
//!   it is the time of the write, and compare everything else. The values in
//!   the fixtures were put in when the field was added to version 1 (before
//!   its release): times on the day of the made-up session, in the place
//!   the store writes the field. Printed again, they are the time of the
//!   printing; the document's example and the reader's listing quote them.
//!
//! Only the store's public API is used. Where a platform has no store
//! (`ErrorKind::Unsupported`) the tests that need one skip themselves, and
//! the ones that only compare the document, the fixtures and the reader
//! still run.

use agent_core::context_archive::{
    archive_file, ArchiveStore, INPUT_JSON_KEY, MAX_INPUT_BYTES, MAX_MESSAGES, MAX_NOTE_BYTES,
    MAX_SEGMENT_BYTES, REDUCED_KEY, SHELF_FILES,
};
use agent_core::memory::store::ProjectScope;
use agent_core::SharedMessage;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The project key and session id of every fixture: the ones in the examples
/// of `docs/extensions/hooks.md`.
const PROJECT_KEY: &str = "p0123456789abcdef";
const SESSION_ID: &str = "20260501-120000-abcd";
/// `<project dir>` for [`PROJECT_KEY`], and `logical` for [`SESSION_ID`].
const PROJECT_DIR: &str = "01a8f6fb3ec12e1d59d07bd6e3943c84c18415e3111dd5cba218b7ce9218b0c7";
const LOGICAL: &str = "900c08a5903292975711f75ce85a7b6e30fab1b1223378ab1111cdbb7bc3d809";
/// The fixtures: four archives on the first shelf and one in a legacy slot.
const SHELF: &str = "s000001";
const LIVE: &str = "b436b6196e6ca0758c70d5faed628f29";
const TOMBSTONE: &str = "693c14d38641fb504a642f0e7cbfaac9";
const REDUCED: &str = "3fe5a51ef0d405efec79538b7717e255";
const JSON_TEXT: &str = "5ad68965a84888ab221823f5b4490804";
const LEGACY_SLOT: &str = "000.json";
const LEGACY: &str = "4c459668c1384294b09674c915b29c2d";

const LIVE_NOTE: &str = "Goal: find where the retry limit is set. Done: MAX_RETRIES is 5, in \
                         src/net/retry.rs. Next: nothing.";
const FORGOTTEN_NOTE: &str = "The user asked for this to be forgotten.";
const REDUCED_NOTE: &str = "The build log was pasted whole.";
const JSON_TEXT_NOTE: &str = "Evaluated a deeply nested expression.";
const LEGACY_NOTE: &str = "Answered a question about the Makefile. Nothing pending.";

const RECORD_DRIFT: &str = "the store no longer writes the record that \
    docs/extensions/context-archives.md describes as version 1. Extensions read these files. \
    A change to a record-level field, to a file name or to a tombstone needs a higher \
    `version`, new fixtures beside the version 1 ones, and a new section in the document. \
    Do not edit the version 1 fixtures";
const MESSAGE_DRIFT: &str = "the store no longer stores this message the way the fixture in \
    docs/extensions/fixtures/context-archives shows. The contents of `message` may grow \
    without a new `version`, but not silently: describe the change in \
    docs/extensions/context-archives.md and print the fixtures again (see the top of this \
    file). Anything a version 1 reader could misread needs a higher `version` instead";

fn message(value: Value) -> SharedMessage {
    Arc::new(value)
}

/// Seven source messages, six stored: a system message and a reasoning block
/// are left out, a checkpoint's input and a string that reads like a
/// credential are withheld.
fn live_window() -> Vec<SharedMessage> {
    vec![
        message(json!({"role":"system","content":"You are a coding assistant."})),
        message(json!({"role":"user","content":"Where is the retry limit set?"})),
        message(json!({"role":"assistant","content":[
            {"type":"thinking","thinking":"Probably the network module."},
            {"type":"text","text":"I will look in the network module."},
            {"type":"tool_use","id":"toolu_01","name":"read","input":{"path":"src/net/retries.rs"}},
            {"type":"tool_use","id":"toolu_02","name":"read","input":{"path":"src/net/retry.rs"}}
        ]})),
        message(json!({"role":"user","content":[
            {"type":"tool_result","tool_use_id":"toolu_01","is_error":true,
             "content":"No such file: src/net/retries.rs"},
            {"type":"tool_result","tool_use_id":"toolu_02","content":[
                {"type":"text","text":"pub const MAX_RETRIES: u32 = 5;\n"}
            ]}
        ]})),
        message(json!({"role":"assistant","content":[
            {"type":"text","text":"MAX_RETRIES is 5, in src/net/retry.rs."},
            {"type":"tool_use","id":"toolu_03","name":"context_checkpoint",
             "input":{"phase":"wrap_up","note":"Retry limit found."}}
        ]})),
        message(json!({"role":"user","content":[
            {"type":"tool_result","tool_use_id":"toolu_03","content":"ok"}
        ]})),
        message(json!({"role":"user",
            "content":"Thanks. The staging password is example-only; keep it out of the logs."})),
    ]
}

fn forgotten_window() -> Vec<SharedMessage> {
    vec![
        message(json!({"role":"user","content":"Remember that the demo is on Friday."})),
        message(json!({"role":"assistant","content":"Noted: the demo is on Friday."})),
    ]
}

/// One message that is over the limits of one archive by itself: a text one
/// line longer than [`MAX_INPUT_BYTES`], and a tool call.
fn reduced_window() -> Vec<SharedMessage> {
    let mut log = String::with_capacity(MAX_INPUT_BYTES + 32);
    let mut line = 0;
    while log.len() <= MAX_INPUT_BYTES {
        line += 1;
        log.push_str(&format!("build log line {line:07}\n"));
    }
    vec![message(json!({"role":"assistant","content":[
        {"type":"text","text":log},
        {"type":"tool_use","id":"toolu_04","name":"write",
         "input":{"path":"build.log","content":"(the log above)"}}
    ]}))]
}

/// What the tool of [`json_text_window`] was called with: twenty groups, one
/// inside the other, around a leaf. Forty levels of JSON, more than an
/// archive stores as a value.
fn nested_input() -> Value {
    let mut nested = json!({"op":"leaf","value":1});
    for _ in 0..20 {
        nested = json!({"op":"group","of":[nested]});
    }
    nested
}

/// One message that is over a limit of one archive by its shape alone: a
/// tool input that is nested too deep, beside one that is not.
fn json_text_window() -> Vec<SharedMessage> {
    vec![message(json!({"role":"assistant","content":[
        {"type":"text","text":"Evaluating the nested expression."},
        {"type":"tool_use","id":"toolu_08","name":"evaluate","input":nested_input()},
        {"type":"tool_use","id":"toolu_09","name":"read","input":{"path":"src/eval.rs"}}
    ]}))]
}

/// What the pre-shelf store was given for `000.json`.
fn legacy_window() -> Vec<SharedMessage> {
    vec![
        message(json!({"role":"user","content":"What does `make check` run?"})),
        message(json!({"role":"assistant",
            "content":"It runs cargo fmt --check, then cargo clippy, then the tests."})),
    ]
}

fn repo_path(parts: &[&str]) -> PathBuf {
    let mut path = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..");
    path.extend(parts);
    path
}

fn document() -> String {
    let path = repo_path(&["docs", "extensions", "context-archives.md"]);
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

fn reader_script() -> PathBuf {
    repo_path(&["examples", "extensions", "context_archive_reader.py"])
}

/// The published project directory: `docs/extensions/fixtures/context-archives/<project dir>`.
fn fixture_project() -> PathBuf {
    repo_path(&["docs", "extensions", "fixtures", "context-archives", PROJECT_DIR])
}

fn project_dir(base: &Path) -> PathBuf {
    base.join("context-archives").join(PROJECT_DIR)
}

/// A temporary directory as the store takes its base: absolute, and on Unix
/// with no link on the way to it (macOS's `/var` is one).
fn base_in(tmp: &tempfile::TempDir) -> PathBuf {
    if cfg!(unix) {
        tmp.path().canonicalize().unwrap()
    } else {
        tmp.path().to_path_buf()
    }
}

/// The store for the fixtures' project and chat, with a redactor that lets
/// everything through (the host installs its own; without one every tool
/// input and result is withheld). `None` where this platform has no store.
fn open(base: &Path) -> Option<ArchiveStore> {
    match ArchiveStore::new(base, PROJECT_KEY, SESSION_ID) {
        Ok(store) => Some(store.with_redactor(Arc::new(str::to_owned))),
        Err(error) if error.kind() == ErrorKind::Unsupported => {
            if cfg!(unix) {
                panic!("the file archive store exists on every Unix: {error}");
            }
            eprintln!("skipped: no file archive store on this platform ({error})");
            None
        }
        Err(error) => panic!("cannot open an archive store in {}: {error}", base.display()),
    }
}

// ─── the rules of the document, written out a second time ───────────────────
//
// These follow docs/extensions/context-archives.md, not the store's code, so
// that a rule the store changes shows up as a difference.

fn is_lower_hex(text: &str, len: usize) -> bool {
    let lower_hex = |b: u8| b.is_ascii_digit() || (b'a'..=b'f').contains(&b);
    text.len() == len && text.bytes().all(lower_hex)
}

/// An archive id: 32 lowercase hex characters.
fn is_id(id: &str) -> bool {
    is_lower_hex(id, 32)
}

/// `<id>.json` in a shelf.
fn is_archive_name(name: &str) -> bool {
    name.strip_suffix(".json").is_some_and(is_id)
}

/// `000.json` to `127.json` in the project directory.
fn is_slot_name(name: &str) -> bool {
    let Some(digits) = name.strip_suffix(".json") else {
        return false;
    };
    digits.len() == 3
        && digits.bytes().all(|b| b.is_ascii_digit())
        && digits.parse::<usize>().is_ok_and(|slot| slot < 128)
}

/// `s` and the shelf number, padded with zeros to six digits.
fn is_shelf_name(name: &str) -> bool {
    let Some(digits) = name.strip_prefix('s') else {
        return false;
    };
    digits.bytes().all(|b| b.is_ascii_digit())
        && (digits.len() == 6 || (digits.len() > 6 && !digits.starts_with('0')))
}

/// SHA-256 over each part, every part preceded by its length as eight
/// big-endian bytes: how the document says a project key becomes
/// `<project dir>` and a session id becomes `logical`.
fn hash(part: &str) -> String {
    let mut hash = Sha256::new();
    hash.update((part.len() as u64).to_be_bytes());
    hash.update(part.as_bytes());
    hex(&hash.finalize())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn names(dir: &Path) -> Vec<String> {
    let entries = std::fs::read_dir(dir).unwrap_or_else(|error| panic!("{}: {error}", dir.display()));
    let mut names: Vec<_> = entries
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// The archive files of a project directory, found the way the document tells
/// a reader to find them: `NNN.json` in the directory itself, `<id>.json` in
/// its shelves, and no other name. Keyed by path below the directory, with `/`.
fn archive_files(project: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    for name in names(project) {
        let path = project.join(&name);
        if is_slot_name(&name) && path.is_file() {
            files.insert(name, std::fs::read(&path).unwrap());
        } else if is_shelf_name(&name) && path.is_dir() {
            for file in names(&path).into_iter().filter(|file| is_archive_name(file)) {
                let bytes = std::fs::read(path.join(&file)).unwrap();
                files.insert(format!("{name}/{file}"), bytes);
            }
        }
    }
    files
}

fn parse(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).expect("an archive file is one JSON value")
}

fn fixture(path: &str) -> Value {
    let files = archive_files(&fixture_project());
    parse(files.get(path).unwrap_or_else(|| panic!("no fixture {path}")))
}

fn shelf_path(id: &str) -> String {
    format!("{SHELF}/{id}.json")
}

/// The keys of a JSON object, sorted.
fn keys(value: &Value) -> Vec<&str> {
    let object = value.as_object().expect("a JSON object");
    let mut keys: Vec<_> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();
    keys
}

/// A record without what its messages hold: everything version 1 freezes.
fn record_level(record: &Value) -> Value {
    let mut record = record.clone();
    if let Some(messages) = record["body"]["messages"].as_array_mut() {
        for archived in messages {
            archived["message"] = Value::Null;
        }
    }
    record
}

/// Every entry below a directory: a file by its bytes, anything else by kind.
fn tree(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(dir: &Path, prefix: &str, entries: &mut BTreeMap<String, Vec<u8>>) {
        for name in names(dir) {
            let path = dir.join(&name);
            let kind = std::fs::symlink_metadata(&path).unwrap().file_type();
            let key = format!("{prefix}{name}");
            if kind.is_dir() {
                entries.insert(key.clone(), b"<directory>".to_vec());
                walk(&path, &format!("{key}/"), entries);
            } else if kind.is_file() {
                entries.insert(key, std::fs::read(&path).unwrap());
            } else {
                entries.insert(key, b"<link or special file>".to_vec());
            }
        }
    }
    let mut entries = BTreeMap::new();
    walk(dir, "", &mut entries);
    entries
}

/// The published fixtures, copied to where a store with this base finds them.
fn copy_fixtures(base: &Path) -> PathBuf {
    let project = project_dir(base);
    for (path, bytes) in archive_files(&fixture_project()) {
        let target = path.split('/').fold(project.clone(), |dir, part| dir.join(part));
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(target, bytes).unwrap();
    }
    project
}

/// The id of a window that was sealed as one archive.
fn one_part(store: &ArchiveStore, messages: &[SharedMessage], note: &str) -> String {
    let window = store.seal_window(messages, note).unwrap();
    assert_eq!(window.parts.len(), 1, "sealed as one archive");
    assert_eq!(window.parts[0].first_source_index, 0);
    window.parts[0].archive.id.clone()
}

fn now_ms() -> u64 {
    let since = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
    since.unwrap().as_millis() as u64
}

/// Seal the four fixed windows and forget the second: what `s000001/` in the
/// fixtures was made by. Returns their ids.
fn seal_fixture_windows(store: &ArchiveStore) -> [String; 4] {
    let live = one_part(store, &live_window(), LIVE_NOTE);
    let forgotten = one_part(store, &forgotten_window(), FORGOTTEN_NOTE);
    store.forget(&forgotten).unwrap();
    let reduced = one_part(store, &reduced_window(), REDUCED_NOTE);
    let json_text = one_part(store, &json_text_window(), JSON_TEXT_NOTE);
    [live, forgotten, reduced, json_text]
}

/// A marker with its numbers replaced by `N`, as the document spells it.
fn template(marker: &str) -> String {
    let mut template = String::new();
    let mut in_number = false;
    for character in marker.chars() {
        if character.is_ascii_digit() {
            if !in_number {
                template.push('N');
            }
            in_number = true;
        } else {
            template.push(character);
            in_number = false;
        }
    }
    template
}

// ─── what the store writes ───────────────────────────────────────────────────

/// Prints the files of `s000001/` in the fixtures. Not a test: run it by name
/// (see the top of this file) and save each line's content, without a
/// trailing newline, under `docs/extensions/fixtures/context-archives/`.
#[test]
#[ignore = "prints the fixtures; run it by name to make them again"]
fn print_fixtures() {
    let tmp = tempfile::tempdir().unwrap();
    let base = base_in(&tmp);
    let Some(store) = open(&base) else {
        return;
    };
    seal_fixture_windows(&store);
    for (path, bytes) in archive_files(&project_dir(&base)) {
        println!("FIXTURE\t{PROJECT_DIR}/{path}\t{}", String::from_utf8(bytes).unwrap());
    }
}

/// The store writes, for the fixed windows, the files the fixtures hold: the
/// same names and, key for key, the same records.
#[test]
fn the_store_writes_the_published_fixtures() {
    let tmp = tempfile::tempdir().unwrap();
    let base = base_in(&tmp);
    let Some(store) = open(&base) else {
        return;
    };
    let before = now_ms();
    let ids = seal_fixture_windows(&store);
    let after = now_ms();
    let written = archive_files(&project_dir(&base));
    assert_eq!(written.len(), 4, "four archives, each in a file named by its id: {RECORD_DRIFT}");

    // A record without `id` and `digest`, which follow from its content.
    let shape = |record: &Value| {
        let mut record = record_level(record);
        record["id"] = Value::Null;
        record["digest"] = Value::Null;
        record
    };
    for (id, published_id) in ids.iter().zip([LIVE, TOMBSTONE, REDUCED, JSON_TEXT]) {
        let bytes = written
            .get(&shelf_path(id))
            .unwrap_or_else(|| panic!("no file named by the id {id}: {RECORD_DRIFT}"));
        let (mut written, mut published) = (parse(bytes), fixture(&shelf_path(published_id)));
        // A live record says when it was written, which is now for the one
        // and was then for the other; a tombstone does not say.
        if published_id == TOMBSTONE {
            assert!(written.get("created_ms").is_none(), "{RECORD_DRIFT}");
        } else {
            let created = written["created_ms"].take().as_u64();
            let created = created.unwrap_or_else(|| panic!("no created_ms: {RECORD_DRIFT}"));
            assert!((before..=after).contains(&created), "created_ms is the time of the write");
            assert!(published["created_ms"].take().is_u64(), "{RECORD_DRIFT}");
        }
        assert_eq!(shape(&written), shape(&published), "{RECORD_DRIFT}");
        assert_eq!(
            written["body"]["messages"], published["body"]["messages"],
            "{MESSAGE_DRIFT}"
        );
        // The same content, so the same digest, id and file name.
        assert_eq!(written, published, "unchanged content got another digest or id: {RECORD_DRIFT}");
    }
    // The entry point rollover uses today gives the same archive.
    let (again, note) = store.seal_with_note(&live_window(), "another note").unwrap();
    assert_eq!((again.id.as_str(), note.as_str()), (LIVE, LIVE_NOTE));
}

/// A reader that takes no lock and opens only the documented names sees
/// whole records and nothing else, while archives are being written and
/// forgotten: a file under a final name is never half written.
#[test]
fn a_reader_without_a_lock_sees_only_whole_records() {
    let tmp = tempfile::tempdir().unwrap();
    let base = base_in(&tmp);
    let Some(store) = open(&base) else {
        return;
    };
    use std::sync::atomic::{AtomicBool, Ordering};
    /// Stops the reader when the writer is done, also when it panicked.
    struct Finish<'a>(&'a AtomicBool);
    impl Drop for Finish<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    let project = project_dir(&base);
    let done = AtomicBool::new(false);
    let (read, live, forgotten) = std::thread::scope(|scope| {
        let reader = scope.spawn(|| {
            let mut read = 0usize;
            // One more pass after the writer has finished.
            let mut last = false;
            while !last {
                last = done.load(Ordering::Acquire);
                for (path, bytes) in archive_files(&project) {
                    let record: Value = serde_json::from_slice(&bytes)
                        .unwrap_or_else(|error| panic!("{path} is not a whole record: {error}"));
                    assert_eq!(record["version"], 1, "{path}");
                    assert_eq!(record["scope"], PROJECT_DIR, "{path}");
                    assert_eq!(path, shelf_path(record["id"].as_str().unwrap()));
                    match record["body"]["state"].as_str() {
                        Some("live") => assert!(record["body"]["messages"].is_array(), "{path}"),
                        Some("tombstone") => assert_eq!(keys(&record["body"]), ["state"], "{path}"),
                        other => panic!("{path}: state {other:?}"),
                    }
                    read += 1;
                }
            }
            read
        });
        let finish = Finish(&done);
        let (mut live, mut forgotten) = (0usize, 0usize);
        for index in 0..60 {
            let text = "words of the conversation ".repeat(200 + index);
            let id = store.seal(&[message(json!({"role":"user","content":text}))], "").unwrap().id;
            if index % 2 == 0 {
                store.forget(&id).unwrap();
                forgotten += 1;
            } else {
                live += 1;
            }
        }
        drop(finish);
        (reader.join().unwrap(), live, forgotten)
    });
    assert!(read >= live + forgotten, "the reader read every archive at least once");
    let states: Vec<_> = archive_files(&project)
        .values()
        .map(|bytes| parse(bytes.as_slice())["body"]["state"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(states.iter().filter(|state| *state == "live").count(), live);
    assert_eq!(states.iter().filter(|state| *state == "tombstone").count(), forgotten);
}

/// Where the files go and what they are called: `<project dir>` from the
/// project key, shelves numbered from `s000001`, 128 archives to a shelf, and
/// every archive in a file named by its id.
#[test]
fn archives_are_named_by_their_id_on_numbered_shelves() {
    let tmp = tempfile::tempdir().unwrap();
    let base = base_in(&tmp);
    let Some(store) = open(&base) else {
        return;
    };
    let seal = |index: usize| {
        let text = format!("archive number {index}");
        store.seal(&[message(json!({"role":"user","content":text}))], "").unwrap().id
    };
    let ids: Vec<String> = (0..=SHELF_FILES).map(seal).collect();

    // One project directory, named by the hash of the project key.
    assert_eq!(names(&base.join("context-archives")), [PROJECT_DIR], "{RECORD_DRIFT}");
    let project = project_dir(&base);
    let listed = names(&project);
    let shelves: Vec<_> = listed.iter().filter(|name| is_shelf_name(name)).collect();
    assert_eq!(shelves, ["s000001", "s000002"], "{RECORD_DRIFT}");
    assert!(!listed.iter().any(|name| is_slot_name(name)), "no legacy slot is written");

    // A full shelf is followed by the next one.
    let files = archive_files(&project);
    let paths: Vec<_> = files.keys().cloned().collect();
    let mut expected: Vec<_> = ids[..SHELF_FILES]
        .iter()
        .map(|id| format!("s000001/{id}.json"))
        .collect();
    expected.sort();
    expected.push(format!("s000002/{}.json", ids[SHELF_FILES]));
    assert_eq!(paths, expected, "{RECORD_DRIFT}");

    for (path, bytes) in &files {
        let record = parse(bytes);
        let id = record["id"].as_str().unwrap();
        assert!(is_id(id), "{path}: {RECORD_DRIFT}");
        assert_eq!(path.rsplit('/').next().unwrap(), format!("{id}.json"), "{RECORD_DRIFT}");
        assert_eq!(record["scope"], PROJECT_DIR, "{path}: {RECORD_DRIFT}");
        assert_eq!(record["logical"], LOGICAL, "{path}: {RECORD_DRIFT}");
        // True of what this version seals; the document tells readers not to
        // rely on it, because a legacy id is random.
        assert_eq!(id, &record["digest"].as_str().unwrap()[..32], "{path}");
        // The path a hook event carries is this file.
        let on_disk = path.split('/').fold(project.clone(), |dir, part| dir.join(part));
        assert_eq!(archive_file(&base, PROJECT_KEY, id).unwrap(), Some(on_disk));
    }
}

/// Forgetting an archive replaces its file, under the same name, with a
/// record that keeps `version`, `scope`, `logical`, `id` and `digest` and
/// holds nothing else. Nothing is deleted and nothing else changes.
#[test]
fn forgetting_leaves_a_tombstone_under_the_same_name() {
    let tmp = tempfile::tempdir().unwrap();
    let base = base_in(&tmp);
    let Some(store) = open(&base) else {
        return;
    };
    let kept = one_part(&store, &live_window(), LIVE_NOTE);
    let gone = one_part(&store, &forgotten_window(), FORGOTTEN_NOTE);
    let project = project_dir(&base);
    let before = archive_files(&project);
    assert_eq!(before.len(), 2);

    store.forget(&gone).unwrap();
    let after = archive_files(&project);
    assert_eq!(after.keys().collect::<Vec<_>>(), before.keys().collect::<Vec<_>>());
    assert_eq!(after[&shelf_path(&kept)], before[&shelf_path(&kept)], "the other is untouched");
    let mut expected = parse(&before[&shelf_path(&gone)]);
    assert_eq!(expected["body"]["state"], "live");
    expected["body"] = json!({"state":"tombstone"});
    // When it was written goes with the content.
    let live = expected.as_object_mut().unwrap();
    assert!(live.remove("created_ms").is_some_and(|created| created.is_u64()), "{RECORD_DRIFT}");
    let tombstone = parse(&after[&shelf_path(&gone)]);
    assert_eq!(tombstone, expected, "{RECORD_DRIFT}");
    let record_keys = ["body", "digest", "id", "logical", "scope", "version"];
    assert_eq!(keys(&tombstone), record_keys, "{RECORD_DRIFT}");

    // Forgetting again changes nothing, and the content cannot come back.
    store.forget(&gone).unwrap();
    let denied = store.seal(&forgotten_window(), "").unwrap_err();
    assert_eq!(denied.kind(), ErrorKind::PermissionDenied);
    assert_eq!(archive_files(&project), after);
    assert_eq!(store.fetch(&gone, 0, 1).unwrap_err().kind(), ErrorKind::NotFound);
}

/// The parts of a window that is too large for one archive are ordinary
/// archives. Nothing in them says they belong together, or in which order:
/// only the list the store returns to its caller does.
#[test]
fn the_parts_of_a_window_do_not_name_each_other() {
    let tmp = tempfile::tempdir().unwrap();
    let base = base_in(&tmp);
    let Some(store) = open(&base) else {
        return;
    };
    // One message more than one archive holds.
    let window: Vec<_> = (0..=MAX_MESSAGES)
        .map(|index| message(json!({"role":"user","content":format!("message {index}")})))
        .collect();
    let sealed = store.seal_window(&window, "the note of the window").unwrap();
    // Where the window is cut is the store's business; the parts follow one
    // another and cover it.
    assert!(sealed.parts.len() > 1);
    let mut next = 0;
    for part in &sealed.parts {
        assert_eq!(part.first_source_index, next);
        next += part.archive.source_message_count;
    }
    assert_eq!(next, window.len());

    let files = archive_files(&project_dir(&base));
    assert_eq!(files.len(), sealed.parts.len());
    let record_keys = ["body", "created_ms", "digest", "id", "logical", "scope", "version"];
    let body_keys = ["messages", "note", "source_message_count", "state"];
    let notes: Vec<_> = sealed
        .parts
        .iter()
        .map(|part| {
            let record = parse(&files[&shelf_path(&part.archive.id)]);
            assert_eq!(keys(&record), record_keys, "{RECORD_DRIFT}");
            assert_eq!(keys(&record["body"]), body_keys, "{RECORD_DRIFT}");
            assert_eq!(record["logical"], LOGICAL);
            // Each part counts its messages from its own first one.
            let messages = record["body"]["messages"].as_array().unwrap();
            assert_eq!(messages[0]["source_index"], 0);
            let last = part.archive.source_message_count - 1;
            assert_eq!(messages[last]["source_index"], last);
            record["body"]["note"].as_str().unwrap().to_owned()
        })
        .collect();
    // The note is stored with the last part.
    let (last, earlier) = notes.split_last().unwrap();
    assert_eq!(last, "the note of the window");
    assert!(earlier.iter().all(String::is_empty));
}

/// The marker strings of a reduced message, as the store writes them and as
/// the document spells them.
#[test]
fn reduced_messages_carry_the_documented_markers() {
    assert_eq!(REDUCED_KEY, "_archive_reduced", "{MESSAGE_DRIFT}");
    assert_eq!(INPUT_JSON_KEY, "_archive_json", "{MESSAGE_DRIFT}");
    let document = document();
    let tmp = tempfile::tempdir().unwrap();
    let base = base_in(&tmp);
    let Some(store) = open(&base) else {
        return;
    };
    let reduced = |messages: &[SharedMessage]| {
        let window = store.seal_window(messages, "").unwrap();
        let part = window.parts.last().unwrap();
        assert_eq!(part.reduced_messages, 1, "the last part is the reduced message");
        let id = &part.archive.id;
        let stored = store.fetch_with_provenance(id, 0, 1).unwrap().remove(0);
        ((*stored.message).clone(), stored.block_indices)
    };
    let documented = |marker: &str| {
        let template = template(marker);
        assert!(document.contains(&format!("`{template}`")), "not in the document: {template}");
    };
    // What a reduced message says of itself: how many things were left out,
    // how many tool inputs are stored as text, or both.
    let says = |stored: &Value, omissions: usize, as_text: usize| {
        let changed = match (omissions, as_text) {
            (omissions, 0) => format!("{omissions} omission(s)"),
            (0, as_text) => format!("{as_text} tool input(s) as JSON text"),
            _ => format!("{omissions} omission(s) and {as_text} tool input(s) as JSON text"),
        };
        let expected = format!(
            "this message alone is over the per-archive limit; it is stored with {changed}, each \
             marked in place"
        );
        assert_eq!(stored[REDUCED_KEY], expected.as_str(), "{MESSAGE_DRIFT}");
        documented(&expected);
    };

    // A tool input that is over a limit by its shape alone (nested too
    // deep) is stored as one string, its JSON text: nothing is left out.
    // The text and the other input stay exact.
    let mut deep = json!("leaf");
    for _ in 0..40 {
        deep = json!([deep]);
    }
    let (stored, indices) = reduced(&[message(json!({"role":"assistant","content":[
        {"type":"text","text":"running both"},
        {"type":"tool_use","id":"toolu_05","name":"nest","input":deep},
        {"type":"tool_use","id":"toolu_06","name":"read","input":{"path":"src/lib.rs"}}
    ]}))]);
    let expected = json!({
        "_archive_reduced": stored[REDUCED_KEY],
        "role": "assistant",
        "content": [
            {"type":"text","text":"running both"},
            {"type":"tool_use","id":"toolu_05","name":"nest",
             "input":{"_archive_json":serde_json::to_string(&deep).unwrap()}},
            {"type":"tool_use","id":"toolu_06","name":"read","input":{"path":"src/lib.rs"}}
        ]
    });
    assert_eq!(stored, expected, "{MESSAGE_DRIFT}");
    assert_eq!(indices, [0, 1, 2]);
    says(&stored, 0, 1);

    // One that is too large as text as well is the only thing left out,
    // beside one that is stored as text.
    let large = json!([["x".repeat(MAX_INPUT_BYTES + 1)]]);
    let input_bytes = serde_json::to_string(&large).unwrap().len();
    let (stored, _) = reduced(&[message(json!({"role":"assistant","content":[
        {"type":"tool_use","id":"toolu_05","name":"nest","input":deep},
        {"type":"tool_use","id":"toolu_06","name":"write","input":large}
    ]}))]);
    let omitted_input = format!(
        "[archive: tool input of {input_bytes} bytes omitted, over the per-archive limit]"
    );
    assert!(stored["content"][0]["input"][INPUT_JSON_KEY].is_string(), "{MESSAGE_DRIFT}");
    let omitted = json!({"_archive_omitted":omitted_input});
    assert_eq!(stored["content"][1]["input"], omitted, "{MESSAGE_DRIFT}");
    says(&stored, 1, 1);
    documented(&omitted_input);

    // A message that is still too large without its tool inputs: long
    // strings are cut, a tool result's block list becomes its first text,
    // and the content blocks after the first 256 become one text block.
    let long = "x".repeat(MAX_INPUT_BYTES + 1);
    let mut blocks = vec![json!({"type":"tool_result","tool_use_id":"toolu_07","content":[
        {"type":"text","text":long},
        {"type":"text","text":"second"},
        {"type":"text","text":"third"}
    ]})];
    blocks.extend((1..300).map(|index| json!({"type":"text","text":format!("block {index}")})));
    let (stored, indices) = reduced(&[message(json!({"role":"user","content":blocks}))]);
    let content = stored["content"].as_array().unwrap();
    let result = content[0]["content"].as_str().unwrap();
    let (kept, markers) = result.split_once('\n').unwrap();
    assert!(!kept.is_empty() && kept.bytes().all(|byte| byte == b'x'));
    let cut = format!(
        "[archive: {} bytes omitted, over the per-archive limit]",
        long.len() - kept.len()
    );
    let followed = "[archive: 2 blocks omitted, over the per-archive limit]";
    assert_eq!(markers, format!("{cut}\n{followed}"), "{MESSAGE_DRIFT}");
    // The first blocks are kept as they are; one text block stands for the
    // rest, under the index of the first block it replaces.
    let kept_blocks = content.len() - 1;
    assert_eq!(content[1], json!({"type":"text","text":"block 1"}));
    assert_eq!(content[kept_blocks - 1]["text"], format!("block {}", kept_blocks - 1));
    let rest = format!(
        "[archive: {} blocks omitted, over the per-archive limit]",
        300 - kept_blocks
    );
    assert_eq!(content[kept_blocks], json!({"type":"text","text":rest}), "{MESSAGE_DRIFT}");
    assert_eq!(indices, (0..=kept_blocks).collect::<Vec<_>>());
    says(&stored, 3, 0);
    for marker in [cut.as_str(), followed, rest.as_str()] {
        documented(marker);
    }

    // A message the store did not reduce never carries the key, whatever its
    // source says.
    let imitation = message(json!({"role":"user","_archive_reduced":"not so","content":"whole"}));
    let window = store.seal_window(&[imitation], "").unwrap();
    assert_eq!(window.parts[0].reduced_messages, 0);
    let stored = store.fetch(&window.parts[0].archive.id, 0, 1).unwrap().remove(0);
    assert_eq!(*stored, json!({"role":"user","content":"whole"}), "{MESSAGE_DRIFT}");
}

// ─── what the store reads ────────────────────────────────────────────────────

/// Today's store reads every published fixture, the legacy slot included,
/// finds each one's file, and treats them as its own: the same content sealed
/// again is the archive already there, and the forgotten one stays forgotten.
#[test]
fn the_store_reads_every_published_fixture() {
    let tmp = tempfile::tempdir().unwrap();
    let base = base_in(&tmp);
    let project = copy_fixtures(&base);
    let published = archive_files(&project);
    assert_eq!(published, archive_files(&fixture_project()));
    let Some(store) = open(&base) else {
        return;
    };

    let live = [
        (LIVE, shelf_path(LIVE), LIVE_NOTE),
        (REDUCED, shelf_path(REDUCED), REDUCED_NOTE),
        (JSON_TEXT, shelf_path(JSON_TEXT), JSON_TEXT_NOTE),
        (LEGACY, LEGACY_SLOT.to_owned(), LEGACY_NOTE),
    ];
    for (id, path, note) in &live {
        let record = parse(&published[path]);
        let messages = store.fetch_with_provenance(id, 0, 16).unwrap_or_else(|error| {
            panic!("today's store cannot read the published {path} ({error}): {RECORD_DRIFT}")
        });
        assert_eq!(
            serde_json::to_value(&messages).unwrap(),
            record["body"]["messages"],
            "{path}: {RECORD_DRIFT}"
        );
        assert_eq!(store.fetch_note(id).unwrap(), *note, "{path}");
        assert_eq!(record["body"]["note"], *note, "{path}");
        let on_disk = path.split('/').fold(project.clone(), |dir, part| dir.join(part));
        assert_eq!(archive_file(&base, PROJECT_KEY, id).unwrap(), Some(on_disk), "{path}");
    }
    // A tombstone is found, and has nothing to fetch.
    assert_eq!(store.fetch(TOMBSTONE, 0, 1).unwrap_err().kind(), ErrorKind::NotFound);
    assert_eq!(store.fetch_note(TOMBSTONE).unwrap_err().kind(), ErrorKind::NotFound);
    assert_eq!(
        archive_file(&base, PROJECT_KEY, TOMBSTONE).unwrap(),
        Some(project.join(SHELF).join(format!("{TOMBSTONE}.json")))
    );
    let mut listed: Vec<_> = store.search("", 8).unwrap().into_iter().map(|row| row.id).collect();
    listed.sort();
    let mut expected = vec![LEGACY, LIVE, REDUCED, JSON_TEXT];
    expected.sort_unstable();
    assert_eq!(listed, expected);

    // Sealing the same windows again finds the fixtures and writes nothing.
    assert_eq!(seal_fixture_windows_again(&store), [LIVE, REDUCED, JSON_TEXT, LEGACY]);
    let denied = store.seal(&forgotten_window(), FORGOTTEN_NOTE).unwrap_err();
    assert_eq!(denied.kind(), ErrorKind::PermissionDenied, "a forgotten archive stays forgotten");
    assert_eq!(archive_files(&project), published, "reading and sealing again rewrote a fixture");
}

fn seal_fixture_windows_again(store: &ArchiveStore) -> [String; 4] {
    [
        one_part(store, &live_window(), "a later note"),
        one_part(store, &reduced_window(), "a later note"),
        one_part(store, &json_text_window(), "a later note"),
        // Found in its slot by its content, under the random id it was given.
        one_part(store, &legacy_window(), "a later note"),
    ]
}

// ─── the fixtures and the document ───────────────────────────────────────────

/// Every fixture has exactly the record-level fields of version 1, with the
/// types and the limits the document gives them.
#[test]
fn the_fixtures_have_exactly_the_documented_fields() {
    let files = archive_files(&fixture_project());
    let paths: Vec<_> = files.keys().map(String::as_str).collect();
    let mut expected: Vec<_> = [LIVE, TOMBSTONE, REDUCED, JSON_TEXT].map(shelf_path).to_vec();
    expected.sort();
    expected.insert(0, LEGACY_SLOT.to_owned());
    assert_eq!(paths, expected, "the fixtures are these five files");
    assert_eq!(PROJECT_DIR, hash(PROJECT_KEY));
    assert_eq!(LOGICAL, hash(SESSION_ID));
    // The sizes the document tells a reader it can count on.
    assert_eq!(
        (MAX_SEGMENT_BYTES, MAX_MESSAGES, MAX_NOTE_BYTES),
        (16 * 1024 * 1024, 4096, 8192),
        "{RECORD_DRIFT}"
    );
    // And one it says not to count on, but quotes.
    assert!(document().contains(&format!("up to {SHELF_FILES} archives")));

    for (path, bytes) in &files {
        assert!(bytes.len() <= MAX_SEGMENT_BYTES);
        let record = parse(bytes);
        // `created_ms` is the one optional field: in the live records of the
        // shelf, not in the tombstone, not in the record from before it.
        let mut record_keys = vec!["body", "digest", "id", "logical", "scope", "version"];
        if path != LEGACY_SLOT && record["id"] != TOMBSTONE {
            record_keys.insert(1, "created_ms");
            assert!(record["created_ms"].is_u64(), "{path}");
        }
        assert_eq!(keys(&record), record_keys, "{path}");
        assert_eq!(record["version"], json!(1), "{path}");
        assert_eq!(record["scope"], PROJECT_DIR, "{path}");
        assert_eq!(record["logical"], LOGICAL, "{path}");
        let id = record["id"].as_str().unwrap();
        assert!(is_id(id) && is_lower_hex(record["digest"].as_str().unwrap(), 64), "{path}");
        // A shelf file is named by its id; a legacy slot is not.
        if path != LEGACY_SLOT {
            assert_eq!(*path, shelf_path(id));
        }
        let body = &record["body"];
        if id == TOMBSTONE {
            assert_eq!(*body, json!({"state":"tombstone"}), "{path}");
            continue;
        }
        assert_eq!(keys(body), ["messages", "note", "source_message_count", "state"], "{path}");
        assert_eq!(body["state"], "live", "{path}");
        let count = body["source_message_count"].as_u64().unwrap();
        assert!(count <= MAX_MESSAGES as u64);
        assert!(body["note"].as_str().unwrap().len() <= MAX_NOTE_BYTES);
        let mut previous = None;
        for archived in body["messages"].as_array().unwrap() {
            assert_eq!(keys(archived), ["block_indices", "message", "source_index"], "{path}");
            let index = archived["source_index"].as_u64().unwrap();
            assert!(index < count && previous < Some(index), "{path}: in order, inside the slice");
            previous = Some(index);
            let message = &archived["message"];
            assert!(matches!(message["role"].as_str(), Some("user" | "assistant")), "{path}");
            let blocks: Vec<_> = archived["block_indices"]
                .as_array()
                .unwrap()
                .iter()
                .map(|index| index.as_u64().unwrap())
                .collect();
            assert!(blocks.windows(2).all(|pair| pair[0] < pair[1]), "{path}");
            match &message["content"] {
                Value::String(_) => assert!(blocks.is_empty(), "{path}"),
                Value::Array(content) => assert_eq!(content.len(), blocks.len(), "{path}"),
                other => panic!("{path}: content is a string or a list, not {other}"),
            }
        }
    }

    // What each fixture is there to show.
    let live = fixture(&shelf_path(LIVE));
    assert_eq!(live["body"]["source_message_count"], 7);
    let stored = live["body"]["messages"].as_array().unwrap();
    let indices: Vec<_> = stored.iter().map(|archived| &archived["source_index"]).collect();
    assert_eq!(indices, [1, 2, 3, 4, 5, 6], "the system message is not stored");
    assert_eq!(stored[1]["block_indices"], json!([1, 2, 3]), "the reasoning block is not stored");
    assert_eq!(stored[3]["message"]["content"][1]["input"], json!({"_archive_withheld":true}));
    assert_eq!(stored[5]["message"]["content"], "[archive: withheld]");
    let reduced = fixture(&shelf_path(REDUCED));
    assert!(reduced["body"]["messages"][0]["message"][REDUCED_KEY].is_string());
    // A tool input stored as its JSON text: one string that is the whole
    // input, in a message that says it is reduced. The other input is exact.
    let json_text = fixture(&shelf_path(JSON_TEXT));
    let stored = &json_text["body"]["messages"][0]["message"];
    assert!(stored[REDUCED_KEY].is_string());
    let input = &stored["content"][1]["input"];
    assert_eq!(keys(input), [INPUT_JSON_KEY]);
    let text = input[INPUT_JSON_KEY].as_str().unwrap();
    assert_eq!(serde_json::from_str::<Value>(text).unwrap(), nested_input());
    assert_eq!(stored["content"][2]["input"], json!({"path":"src/eval.rs"}));
    let legacy = fixture(LEGACY_SLOT);
    assert_eq!(legacy["id"], LEGACY);
    assert_ne!(legacy["digest"].as_str().unwrap()[..32], *LEGACY, "a legacy id is random");
}

/// Every key the fixtures contain, and every word of the format's own
/// vocabulary, is in the document.
#[test]
fn the_document_names_everything_the_fixtures_contain() {
    /// Keys of the format: every key, except that what a tool was called
    /// with is the tool's own business, apart from the store's markers.
    fn collect(value: &Value, tool_input: bool, keys: &mut BTreeSet<String>) {
        match value {
            Value::Object(object) => {
                let tool_use = object.get("type").is_some_and(|kind| kind == "tool_use");
                for (key, child) in object {
                    if !tool_input || key.starts_with("_archive_") {
                        keys.insert(key.clone());
                    }
                    collect(child, tool_input || (tool_use && key == "input"), keys);
                }
            }
            Value::Array(items) => items.iter().for_each(|item| collect(item, tool_input, keys)),
            _ => {}
        }
    }
    let mut found = BTreeSet::new();
    for bytes in archive_files(&fixture_project()).values() {
        collect(&parse(bytes), false, &mut found);
    }
    let expected = [
        "_archive_json",
        "_archive_omitted",
        "_archive_reduced",
        "_archive_withheld",
        "block_indices",
        "body",
        "content",
        "created_ms",
        "digest",
        "id",
        "input",
        "is_error",
        "logical",
        "message",
        "messages",
        "name",
        "note",
        "role",
        "scope",
        "source_index",
        "source_message_count",
        "state",
        "text",
        "tool_use_id",
        "type",
        "version",
    ];
    assert_eq!(
        found.iter().map(String::as_str).collect::<Vec<_>>(),
        expected,
        "the fixtures hold a key this test does not know: document it, then add it here"
    );
    let document = document();
    let vocabulary = [
        "live",
        "tombstone",
        "user",
        "assistant",
        "tool_use",
        "tool_result",
        "[archive: withheld]",
        "s000001",
        "000.json",
        "127.json",
        "<id>.json",
        "on_context_archived",
        "on_memory_forgotten",
    ];
    for word in expected.iter().chain(&vocabulary) {
        assert!(document.contains(&format!("`{word}`")), "not in the document: `{word}`");
    }
    // The markers of the reduced fixture, with their numbers as `N`.
    let reduced = fixture(&shelf_path(REDUCED));
    let stored = &reduced["body"]["messages"][0]["message"];
    let text = stored["content"][0]["text"].as_str().unwrap();
    // And what the fixture with an input as JSON text says of itself.
    let json_text = fixture(&shelf_path(JSON_TEXT));
    let as_text = &json_text["body"]["messages"][0]["message"];
    let markers = [
        stored[REDUCED_KEY].as_str().unwrap(),
        text.rsplit('\n').next().unwrap(),
        stored["content"][1]["input"]["_archive_omitted"].as_str().unwrap(),
        as_text[REDUCED_KEY].as_str().unwrap(),
    ];
    for marker in markers {
        let template = template(marker);
        assert!(document.contains(&format!("`{template}`")), "not in the document: {template}");
        // The excerpt of this fixture that the document shows.
        assert!(document.contains(marker), "the document's excerpt lacks: {marker}");
    }
    // The directory name the document works out for its example key.
    assert!(document.contains(PROJECT_KEY) && document.contains(PROJECT_DIR));
    assert!(document.contains(SESSION_ID) && document.contains(LOGICAL));
}

/// The fenced blocks of the document that start with ` ```<language>`.
fn fenced_blocks(document: &str, language: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut open: Option<Vec<&str>> = None;
    for line in document.lines() {
        let fence = line.trim_end();
        if let Some(mut lines) = open.take() {
            if fence == "```" {
                blocks.push(lines.join("\n"));
            } else {
                lines.push(line);
                open = Some(lines);
            }
        } else if fence == format!("```{language}") {
            open = Some(Vec::new());
        }
    }
    blocks
}

/// The records the document shows are the fixtures, and the reader it prints
/// is the reference reader, so an example cannot drift from what is tested.
#[test]
fn the_examples_in_the_document_are_the_fixtures() {
    let document = document();
    let published: Vec<Value> = archive_files(&fixture_project())
        .values()
        .map(|bytes| parse(bytes.as_slice()))
        .collect();
    let mut shown = BTreeSet::new();
    for block in fenced_blocks(&document, "json") {
        let example: Value = serde_json::from_str(&block)
            .unwrap_or_else(|error| panic!("a json block of the document is not JSON: {error}"));
        if example.get("version").is_none() {
            continue;
        }
        let id = example["id"].as_str().unwrap_or_default().to_owned();
        assert!(
            published.contains(&example),
            "the document shows a record ({id}) that is not one of the fixtures"
        );
        shown.insert(id);
    }
    for id in [LIVE, TOMBSTONE, LEGACY] {
        assert!(shown.contains(id), "the document no longer shows the fixture {id}");
    }

    let reader = std::fs::read_to_string(reader_script()).unwrap();
    let printed = fenced_blocks(&document, "python")
        .into_iter()
        .find(|block| block.starts_with("#!/usr/bin/env python3"))
        .expect("the document prints the reference reader");
    assert_eq!(
        printed.lines().collect::<Vec<_>>(),
        reader.lines().collect::<Vec<_>>(),
        "the reader in the document is not examples/extensions/context_archive_reader.py"
    );

    let index = repo_path(&["docs", "extensions", "README.md"]);
    let index = std::fs::read_to_string(index).unwrap();
    assert!(index.contains("context-archives.md"), "the extension docs index links the document");
}

/// How the host gets from a chat's folder to the project key, as the
/// document describes it: the nearest folder upwards that holds `.git` or
/// `.synaps-project`, made canonical, hashed, `p` and sixteen hex characters.
#[test]
fn the_project_key_is_derived_as_the_document_says() {
    let tmp = tempfile::tempdir().unwrap();
    for marker in [".git", ".synaps-project"] {
        let root = tmp.path().join(format!("project{marker}"));
        let nested = root.join("src").join("net");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(root.join(marker), "").unwrap();
        let canonical = root.canonicalize().unwrap();
        let digest = Sha256::digest(canonical.as_os_str().as_encoded_bytes());
        let expected = format!("p{}", hex(&digest[..8]));
        for folder in [&nested, &root] {
            let scope = ProjectScope::discover_with_override(folder, None).unwrap();
            assert_eq!(scope.key(), expected, "{marker}");
        }
        // An explicit root (`SYNAPS_PROJECT_ROOT`) replaces the walk.
        let scope = ProjectScope::discover_with_override(tmp.path(), Some(&root)).unwrap();
        assert_eq!(scope.key(), expected, "{marker}");
    }
}

// ─── the reference reader ────────────────────────────────────────────────────

/// The interpreter this repository's Python fixtures run under (as in
/// `tests/daemon_reload.rs`), or `None` when there is none to run (as
/// `tests/rpc_e2e.rs` checks before it uses one).
fn python() -> Option<String> {
    let program = if cfg!(windows) {
        // No `python3` there, and a bare `python` may be the Store alias stub.
        let path = std::env::var("PATH").unwrap_or_default();
        path.split(';')
            .map(|dir| Path::new(dir.trim().trim_matches('"')).join("python.exe"))
            .find(|path| path.is_file() && !path.to_string_lossy().contains("WindowsApps"))
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "python".into())
    } else {
        "python3".into()
    };
    let runs = std::process::Command::new(&program)
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success());
    runs.then_some(program)
}

/// The reader's standard output for a project directory, line by line.
fn read_with_reference_reader(python: &str, project: &Path) -> Vec<String> {
    let output = std::process::Command::new(python)
        // No user site, no PYTHON* variables, no bytecode written: the
        // script and the standard library.
        .args(["-I", "-B"])
        .arg(reader_script())
        .arg(project)
        .output()
        .unwrap();
    let errors = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "the reader failed: {errors}");
    assert!(!errors.contains("Traceback"), "the reader crashed on a file: {errors}");
    let listing = String::from_utf8(output.stdout).unwrap();
    listing.lines().map(str::to_owned).collect()
}

/// A reader written to the document, not to the store's code: it lists the
/// fixtures, and in a directory full of other names it lists nothing more,
/// writes nothing and does not fail.
#[test]
fn the_reference_reader_reads_the_fixtures_and_skips_everything_else() {
    let Some(python) = python() else {
        eprintln!("skipped: no Python 3 interpreter to run the reference reader with");
        return;
    };
    let mut rows: Vec<_> = [
        format!("live\t{LIVE}\t{SHELF}/{LIVE}.json\t6\t7\t0\t0\t1777639267512"),
        format!("tombstone\t{TOMBSTONE}\t{SHELF}/{TOMBSTONE}.json"),
        format!("live\t{REDUCED}\t{SHELF}/{REDUCED}.json\t1\t1\t1\t0\t1777640575090"),
        format!("live\t{JSON_TEXT}\t{SHELF}/{JSON_TEXT}.json\t1\t1\t1\t1\t1777642219347"),
    ]
    .into_iter()
    .map(|line| (line.split('\t').nth(1).unwrap().to_owned(), line))
    .collect();
    rows.sort();
    // The last column is `created_ms`, which the legacy record does not have.
    let mut expected = vec![format!("live\t{LEGACY}\t{LEGACY_SLOT}\t2\t2\t0\t0\t-")];
    expected.extend(rows.into_iter().map(|(_, line)| line));

    // The published directory, where it is.
    assert_eq!(read_with_reference_reader(&python, &fixture_project()), expected);

    // A copy with everything in it that a reader must pass over.
    let tmp = tempfile::tempdir().unwrap();
    let project = copy_fixtures(tmp.path());
    let shelf = project.join(SHELF);
    let live = std::fs::read(shelf.join(format!("{LIVE}.json"))).unwrap();
    let legacy = std::fs::read(project.join(LEGACY_SLOT)).unwrap();
    let write = |path: PathBuf, bytes: &[u8]| std::fs::write(path, bytes).unwrap();
    let record = |change: &dyn Fn(&mut Value)| {
        let mut record = parse(&live);
        change(&mut record);
        serde_json::to_vec(&record).unwrap()
    };
    let other = "0123456789abcdef0123456789abcdef";

    // Names that are not archives, some of them holding a whole record.
    write(project.join("lock"), b"");
    write(project.join("notes.txt"), b"not an archive");
    for name in ["128.json", "999.json", "0.json", "00.json", "0000.json", "005.JSON", "001.tmp"] {
        write(project.join(name), &legacy);
    }
    for name in [
        format!("{other}.tmp"),
        format!("{other}.json.tmp"),
        format!("{other}.JSON"),
        format!("{}.json", &other[..31]),
        format!("{other}0.json"),
        format!("{}.json", other.to_uppercase()),
        "unfinished.tmp".to_owned(),
    ] {
        write(shelf.join(name), &live);
    }
    // Directories that are not shelves, and a shelf's name on a file.
    for name in ["s1", "s0000002", "S000003", "shelf", "000001"] {
        std::fs::create_dir(project.join(name)).unwrap();
        write(project.join(name).join(format!("{LIVE}.json")), &live);
    }
    write(project.join("s000004"), &live);
    std::fs::create_dir(project.join("s000005")).unwrap();
    std::fs::create_dir(shelf.join(SHELF)).unwrap();
    write(shelf.join(SHELF).join(format!("{LIVE}.json")), &live);
    std::fs::create_dir(shelf.join("ffffffffffffffffffffffffffffffff.json")).unwrap();

    // Names that are right over content that is not a record a version 1
    // reader may use: each is skipped and the listing goes on.
    let named = |digit: char| format!("{}.json", digit.to_string().repeat(32));
    write(shelf.join(named('1')), &live); // the name is not the record's id
    write(shelf.join(named('2')), &record(&|record| {
        record["version"] = json!(2);
        record["id"] = json!("2".repeat(32));
    }));
    write(shelf.join(named('3')), &record(&|record| {
        record["scope"] = json!(hash("another-project"));
        record["id"] = json!("3".repeat(32));
    }));
    write(shelf.join(named('4')), &record(&|record| {
        record["body"] = json!({"state":"compacted"});
        record["id"] = json!("4".repeat(32));
    }));
    // A `created_ms` that is not a whole number of milliseconds, and one in
    // a tombstone.
    let not_a_time = [
        ('a', json!(null)),
        ('b', json!("1777639267512")),
        ('c', json!(1.5)),
        ('d', json!(-1)),
        ('e', json!(true)),
    ];
    for (digit, created) in not_a_time {
        write(shelf.join(named(digit)), &record(&|record| {
            record["created_ms"] = created.clone();
            record["id"] = json!(digit.to_string().repeat(32));
        }));
    }
    write(shelf.join(named('0')), &record(&|record| {
        assert!(record["created_ms"].is_u64());
        record["body"] = json!({"state":"tombstone"});
        record["id"] = json!("0".repeat(32));
    }));
    write(shelf.join(named('5')), &live[..live.len() / 2]);
    write(shelf.join(named('6')), b"[1, 2, 3]");
    write(shelf.join(named('7')), b"\xff\xfe not UTF-8");
    write(shelf.join(named('8')), b"");
    write(project.join("001.json"), &legacy[..legacy.len() / 2]);
    write(project.join("002.json"), &record(&|record| record["version"] = json!("1")));
    #[cfg(unix)]
    {
        // Synaps never creates links here; a reader does not follow one.
        use std::os::unix::fs::symlink;
        symlink(&shelf, project.join("s000006")).unwrap();
        symlink(shelf.join(format!("{LIVE}.json")), shelf.join(named('9'))).unwrap();
        symlink(project.join(LEGACY_SLOT), project.join("003.json")).unwrap();
    }

    let before = tree(&project);
    assert_eq!(read_with_reference_reader(&python, &project), expected);
    assert_eq!(tree(&project), before, "the reader changed the directory it read");
}
