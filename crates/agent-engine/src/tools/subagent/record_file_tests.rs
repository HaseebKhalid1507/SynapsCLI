//! Record files under concurrent replacement and growth, and owner checks
//! (§4 "Paths"; slice 4). Each race is made deterministic by the load's step
//! seam: the test replaces a component exactly between two steps.
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use super::seams;
use super::*;
use crate::tools::subagent::lineage::scope_digest;
use crate::tools::subagent::record::{BackendKind, FinalRecord, RunId};
use crate::tools::subagent::test_fixtures::{private_dir, private_file, record, run, PROCESS_DIR};

struct Root {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    dir: PathBuf,
}

fn binding() -> Binding {
    Binding {
        backend: BackendKind::Legacy,
        scope_digest: scope_digest("p0123456789abcdef"),
    }
}

fn record_bytes(run_id: &RunId, kind: RecordKind) -> Vec<u8> {
    let mut rec = record(run_id, binding(), vec![], &[], None);
    rec.handle_id = "sa_3".into();
    rec.kind = kind;
    serde_json::to_vec(&rec).unwrap()
}

/// A private root with one process directory holding the final `sa_3.json`
/// of run 1.
fn root() -> Root {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap().join("subagent-history");
    let dir = root.join(PROCESS_DIR);
    private_dir(&root);
    private_dir(&dir);
    private_file(
        &dir.join("sa_3.json"),
        &record_bytes(&run(1), RecordKind::Final),
    );
    Root {
        _tmp: tmp,
        root,
        dir,
    }
}

fn location(r: &Root) -> RecordLocation {
    locate(r.dir.join("sa_3.json").to_str().unwrap(), &r.root).unwrap()
}

fn final_run(loaded: &LoadedRecords) -> RunId {
    match &loaded.final_record {
        Candidate::Present(FinalRecord::V2(rec)) => rec.run_id.clone(),
        other => panic!("expected a v2 final, got {other:?}"),
    }
}

/// Atomically replace `path` with a private file holding `bytes` (the
/// writer's temp-then-rename publication).
fn rename_over(path: &Path, bytes: &[u8]) {
    let tmp = path.with_extension("tmp");
    private_file(&tmp, bytes);
    std::fs::rename(&tmp, path).unwrap();
}

#[test]
fn process_dir_swapped_for_a_symlink_after_the_root_opens_fails_closed() {
    let r = root();
    let (dir, moved) = (r.dir.clone(), r.dir.with_extension("moved"));
    let _seam = seams::hook(move |step| {
        if step == Step::RootOpened {
            std::fs::rename(&dir, &moved).unwrap();
            symlink(&moved, &dir).unwrap();
        }
    });
    assert_eq!(load(&location(&r)).unwrap_err(), RecordFileError::Symlink);
}

#[test]
fn process_dir_replaced_after_it_opens_reads_both_siblings_from_the_opened_dir() {
    let r = root();
    let (dir, old) = (r.dir.clone(), r.dir.with_extension("old"));
    let _seam = seams::hook(move |step| {
        if step == Step::DirOpened {
            // A different, fully valid pair appears under the same name.
            std::fs::rename(&dir, &old).unwrap();
            private_dir(&dir);
            private_file(
                &dir.join("sa_3.head.json"),
                &record_bytes(&run(2), RecordKind::Head),
            );
            private_file(
                &dir.join("sa_3.json"),
                &record_bytes(&run(2), RecordKind::Final),
            );
        }
    });
    let loaded = load(&location(&r)).unwrap();
    assert!(matches!(loaded.head, Candidate::Absent), "no mixed pair");
    assert_eq!(final_run(&loaded), run(1));
}

#[test]
fn record_swapped_for_a_symlink_before_it_opens_fails_closed() {
    let r = root();
    let dir = r.dir.clone();
    let _seam = seams::hook(move |step| {
        if step == Step::RecordOpened(RecordKind::Head) {
            // The final is replaced after the head was looked up, before its
            // own open.
            let target = dir.join("elsewhere.json");
            private_file(&target, &record_bytes(&run(2), RecordKind::Final));
            std::fs::remove_file(dir.join("sa_3.json")).unwrap();
            symlink(&target, dir.join("sa_3.json")).unwrap();
        }
    });
    // A head exists, so the hook runs between the two record opens.
    private_file(
        &r.dir.join("sa_3.head.json"),
        &record_bytes(&run(1), RecordKind::Head),
    );
    assert_eq!(load(&location(&r)).unwrap_err(), RecordFileError::Symlink);
}

#[test]
fn record_replaced_after_it_opens_and_before_its_checks_is_refused() {
    let r = root();
    let path = r.dir.join("sa_3.json");
    let _seam = seams::hook(move |step| {
        if step == Step::RecordOpened(RecordKind::Final) {
            rename_over(&path, &record_bytes(&run(2), RecordKind::Final));
        }
    });
    // The opened file has no name left: it is neither the record now on disk
    // nor one that may be trusted to be complete.
    assert_eq!(load(&location(&r)).unwrap_err(), RecordFileError::Replaced);
}

#[test]
fn record_replaced_after_its_checks_yields_the_opened_contents() {
    let r = root();
    let path = r.dir.join("sa_3.json");
    let _seam = seams::hook(move |step| {
        if step == Step::RecordChecked(RecordKind::Final) {
            rename_over(&path, &record_bytes(&run(2), RecordKind::Final));
        }
    });
    let loaded = load(&location(&r)).unwrap();
    assert_eq!(final_run(&loaded), run(1), "the checked file, whole");
}

#[test]
fn growth_past_the_limit_after_the_size_check_is_refused() {
    let r = root();
    let path = r.dir.join("sa_3.json");
    let size = std::fs::metadata(&path).unwrap().len();
    let limit = size + 64;
    let grow = path.clone();
    let _seam = seams::hook(move |step| {
        if step == Step::RecordChecked(RecordKind::Final) {
            use std::io::Write as _;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&grow)
                .unwrap();
            f.write_all(&vec![b' '; 4096]).unwrap();
        }
    });
    // Checked at `size` (within the limit); the read stops at limit + 1.
    assert_eq!(
        load_limited(&location(&r), limit).unwrap_err(),
        RecordFileError::TooLarge(limit + 1)
    );
}

#[test]
fn growth_within_the_limit_after_the_size_check_is_read_whole() {
    let r = root();
    let path = r.dir.join("sa_3.json");
    let size = std::fs::metadata(&path).unwrap().len();
    let grow = path.clone();
    let _seam = seams::hook(move |step| {
        if step == Step::RecordChecked(RecordKind::Final) {
            use std::io::Write as _;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&grow)
                .unwrap();
            f.write_all(&[b' '; 32]).unwrap();
        }
    });
    let loaded = load_limited(&location(&r), size + 64).unwrap();
    assert_eq!(final_run(&loaded), run(1));
}

fn other_uid() -> u32 {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() }.wrapping_add(1)
}

/// Owner checks at each level, through the expected-owner seam: switching
/// the expected owner between steps isolates the root, the process
/// directory and each record file.
#[test]
fn objects_of_another_owner_are_refused_at_every_level() {
    let r = root();
    private_file(
        &r.dir.join("sa_3.head.json"),
        &record_bytes(&run(1), RecordKind::Head),
    );
    // Control: the effective UID is accepted.
    assert!(load(&location(&r)).is_ok());

    // Root.
    {
        let _seam = seams::hook(|_| {});
        seams::expect_owner(Some(other_uid()));
        assert_eq!(
            load(&location(&r)).unwrap_err(),
            RecordFileError::NotPrivate
        );
    }
    // Process directory: the root passed as ours.
    {
        let _seam = seams::hook(|step| {
            if step == Step::RootOpened {
                seams::expect_owner(Some(other_uid()));
            }
        });
        assert_eq!(
            load(&location(&r)).unwrap_err(),
            RecordFileError::NotPrivate
        );
    }
    // Head, then final record.
    for kind in [RecordKind::Head, RecordKind::Final] {
        let _seam = seams::hook(move |step| match step {
            Step::RecordOpened(k) if k == kind => seams::expect_owner(Some(other_uid())),
            Step::RecordChecked(_) => seams::expect_owner(None),
            _ => {}
        });
        assert_eq!(
            load(&location(&r)).unwrap_err(),
            RecordFileError::NotPrivate,
            "{kind:?}"
        );
    }
}

/// The same with files really owned by another user. Needs root to chown;
/// skipped (and says so) otherwise.
#[test]
fn objects_owned_by_another_user_on_disk_are_refused_when_run_as_root() {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("skipped: chown to another user needs root");
        return;
    }
    let nobody = 65534;
    let r = root();
    let record_path = r.dir.join("sa_3.json");
    std::os::unix::fs::chown(&record_path, Some(nobody), None).unwrap();
    assert_eq!(
        load(&location(&r)).unwrap_err(),
        RecordFileError::NotPrivate
    );
    std::os::unix::fs::chown(&record_path, Some(0), None).unwrap();
    std::os::unix::fs::chown(&r.dir, Some(nobody), None).unwrap();
    assert_eq!(
        load(&location(&r)).unwrap_err(),
        RecordFileError::NotPrivate
    );
    std::os::unix::fs::chown(&r.dir, Some(0), None).unwrap();
    std::os::unix::fs::chown(&r.root, Some(nobody), None).unwrap();
    assert_eq!(
        load(&location(&r)).unwrap_err(),
        RecordFileError::NotPrivate
    );
    std::os::unix::fs::chown(&r.root, Some(0), None).unwrap();
    assert!(load(&location(&r)).is_ok());
}

// ── The seams themselves: scoped to one guard on one thread ─────────────

// A seam guard stays on the thread whose seams it clears: this compiles only
// while `seams::Guard` is neither `Send` nor `Sync` (for a type that is, both
// impls apply and the item is ambiguous).
const _: fn() = || {
    trait AmbiguousIfSend<A> {
        fn some_item() {}
    }
    impl<T: ?Sized> AmbiguousIfSend<()> for T {}
    #[allow(dead_code)]
    struct Invalid;
    impl<T: ?Sized + Send> AmbiguousIfSend<Invalid> for T {}
    let _ = <seams::Guard as AmbiguousIfSend<_>>::some_item;

    trait AmbiguousIfSync<A> {
        fn some_item() {}
    }
    impl<T: ?Sized> AmbiguousIfSync<()> for T {}
    impl<T: ?Sized + Sync> AmbiguousIfSync<Invalid> for T {}
    let _ = <seams::Guard as AmbiguousIfSync<_>>::some_item;
};

#[test]
fn a_second_hook_on_the_same_thread_is_refused_and_the_first_stays() {
    let calls = std::rc::Rc::new(std::cell::Cell::new(0u32));
    let counter = calls.clone();
    let _outer = seams::hook(move |_| counter.set(counter.get() + 1));
    let nested = std::panic::catch_unwind(|| seams::hook(|_| {}));
    assert!(nested.is_err(), "a nested hook must be refused");
    let r = root();
    load(&location(&r)).unwrap();
    assert!(calls.get() > 0, "the first hook is still installed");
}

#[test]
fn an_owner_override_needs_a_guard_on_this_thread() {
    let unguarded = std::panic::catch_unwind(|| seams::expect_owner(Some(other_uid())));
    assert!(unguarded.is_err(), "an override without a guard must be refused");
    assert_eq!(seams::expected_owner(), None);
    let r = root();
    assert!(load(&location(&r)).is_ok());
}

#[test]
fn a_panic_under_a_guard_clears_both_seams() {
    let failed = std::panic::catch_unwind(|| {
        let _seam = seams::hook(|_| panic!("a hook outlived its guard"));
        seams::expect_owner(Some(other_uid()));
        panic!("the test body fails");
    });
    assert!(failed.is_err());
    assert_eq!(seams::expected_owner(), None);
    // No hook is left: a load runs without one, and a new one installs.
    let r = root();
    assert!(load(&location(&r)).is_ok());
    let _again = seams::hook(|_| {});
}

#[test]
fn seams_of_one_thread_do_not_reach_another() {
    let r = root();
    let loc = location(&r);
    let called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = called.clone();
    let _seam = seams::hook(move |_| flag.store(true, std::sync::atomic::Ordering::SeqCst));
    seams::expect_owner(Some(other_uid()));
    std::thread::scope(|s| {
        s.spawn(|| {
            assert_eq!(seams::expected_owner(), None);
            assert!(load(&loc).is_ok());
        })
        .join()
        .unwrap();
    });
    assert!(!called.load(std::sync::atomic::Ordering::SeqCst));
}

// ── v1 finals over the record cap (strict r1 A-B1(a)) ─────────────────────

/// A v1 archive as its writer lays it out: `version` first.
#[derive(serde::Serialize)]
struct V1First<'a> {
    version: u32,
    meta: serde_json::Value,
    status: &'a str,
    output: &'a str,
    history: Vec<serde_json::Value>,
}

/// The same archive with `version` after other members (a hand-edited copy).
#[derive(serde::Serialize)]
struct V1Later<'a> {
    meta: serde_json::Value,
    status: &'a str,
    version: u32,
    output: &'a str,
    history: Vec<serde_json::Value>,
}

fn v1_meta(system_prompt: &str) -> serde_json::Value {
    serde_json::json!({
        "agent_name": "archived",
        "model": "anthropic/claude-opus-5-5",
        "system_prompt": system_prompt,
        "timeout_secs": 30,
    })
}

fn v1_history(text: &str) -> Vec<serde_json::Value> {
    vec![serde_json::json!({"role": "user", "content": text})]
}

/// One v1 archive whose history message is `text`, spelled three ways:
/// compact, pretty-printed, and with `version` after a nested member whose
/// strings hold quotes and brackets.
fn v1_spellings(text: &str) -> Vec<(&'static str, Vec<u8>)> {
    let prompt = "a \"}\" {[ prompt";
    let first = V1First {
        version: 1,
        meta: v1_meta(prompt),
        status: "completed",
        output: "done",
        history: v1_history(text),
    };
    let later = V1Later {
        meta: v1_meta(prompt),
        status: "completed",
        version: 1,
        output: "done",
        history: v1_history(text),
    };
    vec![
        ("compact", serde_json::to_vec(&first).unwrap()),
        ("pretty", serde_json::to_vec_pretty(&first).unwrap()),
        ("version later", serde_json::to_vec_pretty(&later).unwrap()),
    ]
}

fn v1_text(loaded: &LoadedRecords) -> String {
    match &loaded.final_record {
        Candidate::Present(FinalRecord::V1(archive)) => {
            archive.history[0]["content"].as_str().unwrap().to_owned()
        }
        other => panic!("expected a v1 final, got {other:?}"),
    }
}

#[test]
fn top_level_version_finds_the_member_in_any_layout() {
    for (form, bytes) in v1_spellings("hello") {
        assert_eq!(top_level_version(&bytes), Some(1), "{form}");
    }
    let found: &[(&[u8], u64)] = &[
        (br#"{"vers\u0069on":1,"#, 1),
        (br#"{"version":10,"#, 10),
        (br#"{ "version" : 2 }"#, 2),
        (br#"{"versions":1,"version":2,"#, 2),
        (br#"{"a":[{"version":5}],"b":"\\","version":1}"#, 1),
    ];
    for (bytes, version) in found {
        let shown = String::from_utf8_lossy(bytes);
        assert_eq!(top_level_version(bytes), Some(*version), "{shown}");
    }
    let none: &[&[u8]] = &[
        b"",
        &[0u8; 64],
        br#"{"version":1"#,
        br#"{"version":1.0,"#,
        br#"{"version":"1","#,
        br#"{"version":-1,"#,
        br#"[{"version":1}]"#,
        br#"{"meta":{"p":"x"#,
        br#"{"meta":{"version":1}}"#,
        br#"{"a" 1,"version":1,"#,
        br#"{"a":1}"#,
    ];
    for bytes in none {
        let shown = String::from_utf8_lossy(bytes);
        assert_eq!(top_level_version(bytes), None, "{shown}");
    }
}

#[test]
fn a_v1_final_over_the_limit_is_read_whole_in_any_layout() {
    let text = "x".repeat(4096);
    for (form, bytes) in v1_spellings(&text) {
        let r = root();
        private_file(&r.dir.join("sa_3.json"), &bytes);
        assert!(bytes.len() > 1024, "{form}: over the limit");
        let loaded = load_limited(&location(&r), 1024).unwrap_or_else(|e| panic!("{form}: {e}"));
        assert_eq!(v1_text(&loaded), text, "{form}: read whole");
    }
}

#[test]
fn a_v2_final_or_any_head_over_the_limit_is_refused() {
    let r = root();
    let size = std::fs::metadata(r.dir.join("sa_3.json")).unwrap().len();
    assert_eq!(
        load_limited(&location(&r), size - 1).unwrap_err(),
        RecordFileError::TooLarge(size)
    );
    // A head is v2 only: even v1-shaped bytes there are not read on.
    let (_, v1) = v1_spellings(&"x".repeat(4096)).remove(0);
    private_file(&r.dir.join("sa_3.head.json"), &v1);
    assert_eq!(
        load_limited(&location(&r), 1024).unwrap_err(),
        RecordFileError::TooLarge(v1.len() as u64)
    );
}

#[test]
fn a_final_whose_version_lies_past_the_sniffed_prefix_is_refused_over_the_limit() {
    let r = root();
    let late = V1Later {
        meta: v1_meta(&"p".repeat(VERSION_SNIFF_BYTES)),
        status: "completed",
        version: 1,
        output: "done",
        history: v1_history("hello"),
    };
    let bytes = serde_json::to_vec(&late).unwrap();
    private_file(&r.dir.join("sa_3.json"), &bytes);
    assert_eq!(
        load_limited(&location(&r), 1024).unwrap_err(),
        RecordFileError::TooLarge(bytes.len() as u64)
    );
    // Within the limit it is parsed whole, as before, wherever `version` is.
    let loaded = load_limited(&location(&r), bytes.len() as u64).unwrap();
    assert_eq!(v1_text(&loaded), "hello");
}

#[test]
fn a_v1_final_that_grows_past_the_limit_after_its_check_is_read_whole() {
    let r = root();
    let path = r.dir.join("sa_3.json");
    let (_, v1) = v1_spellings("hello").remove(0);
    private_file(&path, &v1);
    let limit = v1.len() as u64 + 64;
    let grow = path.clone();
    let _seam = seams::hook(move |step| {
        if step == Step::RecordChecked(RecordKind::Final) {
            use std::io::Write as _;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&grow)
                .unwrap();
            f.write_all(&[b' '; 4096]).unwrap();
        }
    });
    let loaded = load_limited(&location(&r), limit).unwrap();
    assert_eq!(v1_text(&loaded), "hello");
}

#[test]
fn the_record_limit_seam_reaches_load_and_needs_a_guard() {
    let r = root();
    let size = std::fs::metadata(r.dir.join("sa_3.json")).unwrap().len();
    let unguarded = std::panic::catch_unwind(|| seams::limit_records(16));
    assert!(unguarded.is_err(), "a limit without a guard must be refused");
    {
        let _seam = seams::hook(|_| {});
        seams::limit_records(16);
        assert_eq!(
            load(&location(&r)).unwrap_err(),
            RecordFileError::TooLarge(size)
        );
    }
    assert!(load(&location(&r)).is_ok(), "the guard's drop clears the limit");
}

// ── The legacy process-directory layout, v1 only (history admission scan) ──

/// An earlier writer's process directory spelling (`YYYYMMDD-HHMMSS-<pid>`).
const LEGACY_DIR: &str = "20261008-000144-2531188";

/// A private root whose legacy process directory holds `sa_3.json` = `bytes`.
fn legacy_root(bytes: &[u8]) -> Root {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap().join("subagent-history");
    let dir = root.join(LEGACY_DIR);
    private_dir(&root);
    private_dir(&dir);
    private_file(&dir.join("sa_3.json"), bytes);
    Root {
        _tmp: tmp,
        root,
        dir,
    }
}

fn compact_v1() -> Vec<u8> {
    v1_spellings("hello").remove(0).1
}

#[test]
fn locate_admits_the_legacy_process_dir_for_a_final_only() {
    let r = legacy_root(b"{}");
    let at = |rel: &str| locate(r.root.join(rel).to_str().unwrap(), &r.root);
    let loc = at("20261008-000144-2531188/sa_3.json").unwrap();
    assert!(loc.legacy);
    assert_eq!(loc.process_dir(), LEGACY_DIR);
    assert!(!location(&root()).legacy, "the current spelling is not legacy");
    for bad in [
        "20261008-000144-2531188/sa_3.head.json",
        "20261008-000144-2531188/sa_03.json",
        "20261008-000144-2531188/.sa_5.json.tmp",
        "20261308-000144-2531188/sa_3.json",
        "20260230-000144-2531188/sa_3.json",
        "20261008-240000-2531188/sa_3.json",
        "20261008-000144-02531188/sa_3.json",
        "20261008-000144-/sa_3.json",
        "20261008-000144-4294967296/sa_3.json",
        "2026108-000144-2531188/sa_3.json",
        "+2026108-000144-2531188/sa_3.json",
        "20261008_000144-2531188/sa_3.json",
        "20261008-000144_2531188/sa_3.json",
        "20261008-000144-2531188-x/sa_3.json",
    ] {
        assert_eq!(at(bad).unwrap_err(), RecordFileError::BadName, "{bad}");
    }
    // The writer's own names never take the legacy spelling.
    assert_eq!(
        location_of(&r.root, LEGACY_DIR, "sa_3", RecordKind::Final).unwrap_err(),
        RecordFileError::BadName
    );
    assert_eq!(
        location_in(&r.root, LEGACY_DIR, "sa_3", RecordKind::Final).unwrap_err(),
        RecordFileError::BadName
    );
}

#[test]
fn a_legacy_process_dir_yields_its_v1_final_in_any_layout() {
    for (form, bytes) in v1_spellings("hello") {
        let r = legacy_root(&bytes);
        let loaded = load(&location(&r)).unwrap_or_else(|e| panic!("{form}: {e}"));
        assert!(matches!(loaded.head, Candidate::Absent), "{form}");
        assert_eq!(v1_text(&loaded), "hello", "{form}");
        // Over the cap too, as in any process directory.
        let capped = load_limited(&location(&r), 64).unwrap_or_else(|e| panic!("{form}: {e}"));
        assert_eq!(v1_text(&capped), "hello", "{form}: over the cap");
    }
}

#[test]
fn a_legacy_process_dir_refuses_a_v2_final_or_any_head() {
    let r = legacy_root(&record_bytes(&run(1), RecordKind::Final));
    assert_eq!(
        load(&location(&r)).unwrap_err(),
        RecordFileError::BadName,
        "a v2 final"
    );
    private_file(&r.dir.join("sa_3.json"), &compact_v1());
    assert_eq!(v1_text(&load(&location(&r)).unwrap()), "hello");
    private_file(
        &r.dir.join("sa_3.head.json"),
        &record_bytes(&run(1), RecordKind::Head),
    );
    assert_eq!(
        load(&location(&r)).unwrap_err(),
        RecordFileError::BadName,
        "a head beside the v1 final"
    );
}

#[test]
fn a_legacy_process_dir_keeps_every_link_and_owner_check() {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = |path: &Path, mode: u32| {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap()
    };
    // The process directory swapped for a symlink.
    let r = legacy_root(&compact_v1());
    let moved = r.dir.with_extension("moved");
    std::fs::rename(&r.dir, &moved).unwrap();
    symlink(&moved, &r.dir).unwrap();
    assert_eq!(load(&location(&r)).unwrap_err(), RecordFileError::Symlink);
    // The record a symlink.
    let r = legacy_root(&compact_v1());
    let target = r.dir.join("elsewhere.json");
    private_file(&target, &compact_v1());
    std::fs::remove_file(r.dir.join("sa_3.json")).unwrap();
    symlink(&target, r.dir.join("sa_3.json")).unwrap();
    assert_eq!(load(&location(&r)).unwrap_err(), RecordFileError::Symlink);
    // The process directory open to others.
    let r = legacy_root(&compact_v1());
    mode(&r.dir, 0o755);
    assert_eq!(load(&location(&r)).unwrap_err(), RecordFileError::NotPrivate);
    // The record readable by others.
    let r = legacy_root(&compact_v1());
    mode(&r.dir.join("sa_3.json"), 0o644);
    assert_eq!(load(&location(&r)).unwrap_err(), RecordFileError::NotPrivate);
    // The record hard-linked.
    let r = legacy_root(&compact_v1());
    std::fs::hard_link(r.dir.join("sa_3.json"), r.root.join("other.json")).unwrap();
    assert_eq!(load(&location(&r)).unwrap_err(), RecordFileError::NotPrivate);
}
