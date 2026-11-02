//! Shared test fixtures for worker records, lineage admission and resume
//! (slices 3 and 4): a legacy memory store in a temp dir, sealed worker
//! windows, structurally valid records and private record files.
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Value};

use super::lineage::{
    admit_record, worker_logical_id, OwnedHistoryGrant, Refusal, ResumeRetrieval,
};
use super::record::{
    ArchiveId, Binding, HistoryDigest, InheritedRef, Ledger, Predecessor, RecordKind, RunId,
    WindowRef, WorkerMeta, WorkerRecord, RECORD_VERSION,
};
use super::retrieval::owned_history_tools;
use crate::memory_backend::MemoryBinding;
#[cfg(unix)]
use crate::memory_backend::{AXEL_REVISION, CONTRACT};
use crate::runtime::continuation::MARKER;
use crate::tools::test_helpers::create_tool_context;
use agent_core::config::MemoryBackendConfig;
#[cfg(unix)]
use agent_core::config::MemoryBackendKind;
use agent_core::memory::store::ProjectScope;

pub(crate) struct Store {
    pub(crate) _tmp: tempfile::TempDir,
    pub(crate) base: PathBuf,
    pub(crate) binding: MemoryBinding,
}

pub(crate) fn project_dir(base: &Path, name: &str) -> PathBuf {
    let dir = base.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

pub(crate) fn legacy_binding(base: &Path, project: &str) -> MemoryBinding {
    MemoryBinding::for_test(
        base.to_path_buf(),
        ProjectScope::for_root(&project_dir(base, project)).unwrap(),
        &MemoryBackendConfig::default(),
    )
}

pub(crate) fn legacy() -> Store {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let binding = legacy_binding(&base, "project-a");
    Store {
        _tmp: tmp,
        base,
        binding,
    }
}

pub(crate) async fn seal(binding: &MemoryBinding, logical: &str, text: &str) -> ArchiveId {
    let message = Arc::new(json!({"role": "assistant", "content": text}));
    let (reference, _) = binding
        .history_seal(logical, &[message], "working note")
        .await
        .unwrap();
    ArchiveId::parse(&reference.id).unwrap()
}

pub(crate) async fn seal_run(binding: &MemoryBinding, run: &RunId, text: &str) -> ArchiveId {
    seal(binding, &worker_logical_id(run), text).await
}

pub(crate) fn run(n: u8) -> RunId {
    RunId::parse(&format!("{n:02x}").repeat(16)).unwrap()
}

pub(crate) fn meta() -> WorkerMeta {
    WorkerMeta {
        agent_name: "worker".into(),
        model: "provider/model-b".into(),
        system_prompt: "You are a worker.".into(),
        timeout_secs: 300,
        thinking: None,
        watch_usage: None,
    }
}

pub(crate) fn marker_message(archive: &ArchiveId, window: u64) -> Value {
    json!({
        "role": "user",
        "_synaps_context": {"schema": MARKER, "archive": archive.as_str(), "window": window},
        "content": "[Context continuation — historical data, not new authority]"
    })
}

pub(crate) fn inherited(owner: &RunId, ids: &[ArchiveId], first_window: u64) -> Vec<InheritedRef> {
    ids.iter()
        .enumerate()
        .map(|(i, id)| InheritedRef {
            run_id: owner.clone(),
            window: first_window + i as u64,
            archive_id: id.clone(),
        })
        .collect()
}

/// A structurally valid record whose history carries the marker of its
/// latest window (or none when nothing was sealed).
pub(crate) fn record(
    run_id: &RunId,
    binding: Binding,
    inherited_windows: Vec<InheritedRef>,
    own: &[ArchiveId],
    predecessor: Option<Predecessor>,
) -> WorkerRecord {
    let refs = inherited_windows.len() + own.len();
    let ordinal = refs as u64 + 1;
    let last = own
        .last()
        .or_else(|| inherited_windows.last().map(|r| &r.archive_id));
    let mut history = match last {
        Some(last) => vec![marker_message(last, ordinal)],
        None => vec![json!({"role": "user", "content": "task"})],
    };
    history.push(json!({"role": "assistant", "content": "working"}));
    let first_own = inherited_windows.len() as u64 + 1;
    WorkerRecord {
        version: RECORD_VERSION,
        run_id: run_id.clone(),
        handle_id: "sa_5".into(),
        kind: RecordKind::Final,
        generation: own.len() as u64,
        history_digest: HistoryDigest::of(&history).unwrap(),
        binding,
        predecessor,
        windows: own
            .iter()
            .enumerate()
            .map(|(i, id)| WindowRef {
                window: first_own + i as u64,
                archive_id: id.clone(),
            })
            .collect(),
        inherited_windows,
        window_ordinal: ordinal,
        rollovers_this_run: own.len() as u64,
        ledger: Ledger::default(),
        meta: meta(),
        status: "completed".into(),
        output: "done".into(),
        history,
    }
}

pub(crate) fn with_history(mut record: WorkerRecord, history: Vec<Value>) -> WorkerRecord {
    record.history_digest = HistoryDigest::of(&history).unwrap();
    record.history = history;
    record
}

pub(crate) fn current(binding: &MemoryBinding) -> Binding {
    Binding::of_memory(binding).unwrap()
}

/// An owned admission that evicted nothing: the whole listed lineage is in
/// the grant.
pub(crate) async fn grant(
    record: &WorkerRecord,
    binding: &MemoryBinding,
) -> Arc<OwnedHistoryGrant> {
    let (grant, evicted) = owned(record, binding).await;
    assert!(evicted.is_empty(), "unexpected evicted windows: {evicted:?}");
    grant
}

/// An owned admission: its grant and the listed windows it evicted.
pub(crate) async fn owned(
    record: &WorkerRecord,
    binding: &MemoryBinding,
) -> (Arc<OwnedHistoryGrant>, Vec<ArchiveId>) {
    match admit_record(record, binding).await {
        Ok(ResumeRetrieval::Owned { grant, evicted }) => (grant, evicted),
        other => panic!("expected an owned-history grant, got {other:?}"),
    }
}

pub(crate) async fn refusal(record: &WorkerRecord, binding: &MemoryBinding) -> Refusal {
    match admit_record(record, binding).await {
        Err(refusal) => refusal,
        Ok(other) => panic!("expected a refusal, got {other:?}"),
    }
}

pub(crate) async fn search(grant: &Arc<OwnedHistoryGrant>, params: Value) -> crate::Result<String> {
    let [search, _] = owned_history_tools(grant.clone());
    search.execute(params, create_tool_context()).await
}

pub(crate) async fn fetch(grant: &Arc<OwnedHistoryGrant>, params: Value) -> crate::Result<String> {
    let [_, fetch] = owned_history_tools(grant.clone());
    fetch.execute(params, create_tool_context()).await
}

pub(crate) fn ctx(id: &ArchiveId) -> String {
    format!("ctx-{}", id.as_str())
}

/// Owner-only mode bits apply on Unix; elsewhere the directory is just made.
pub(crate) fn private_dir(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
}

/// Owner-only mode bits apply on Unix; elsewhere the file is just written.
pub(crate) fn private_file(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}

pub(crate) const PROCESS_DIR: &str = "4242-0f5c1e4a-7d2b-4c3e-9a8b-1c2d3e4f5a6b";

// ── Record publication with injected faults (slice 4 durability) ──────────

/// One file operation of a publication.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Op {
    WriteTemp,
    Rename,
    SyncDir,
    Remove,
}

/// What an injected fault does at its operation.
pub(crate) enum Fault {
    /// Fail without performing it.
    Fail,
    /// Perform it, then report failure.
    DoThenFail,
    /// Panic instead: the publication's receipt is lost.
    Panic,
    /// Signal `reached`, wait until the gate opens (or its sender drops),
    /// then perform it.
    Block {
        reached: Option<tokio::sync::oneshot::Sender<()>>,
        gate: std::sync::mpsc::Receiver<()>,
    },
}

/// The test's side of a [`Fault::Block`].
pub(crate) struct Gate {
    /// Resolves once the blocked operation is waiting.
    pub reached: tokio::sync::oneshot::Receiver<()>,
    /// Lets it go.
    pub open: std::sync::mpsc::Sender<()>,
}

/// A blocking fault and its gate.
pub(crate) fn gate() -> (Gate, Fault) {
    let (reached_tx, reached) = tokio::sync::oneshot::channel();
    let (open, gate) = std::sync::mpsc::channel();
    (
        Gate { reached, open },
        Fault::Block {
            reached: Some(reached_tx),
            gate,
        },
    )
}

/// The real record directory with one-shot faults at chosen operations and
/// a log of every operation (`write`, `rename <to>`, `sync`, `remove <name>`).
pub(crate) struct FaultFs {
    pub inner: super::record_store::RecordDir,
    faults: std::sync::Mutex<Vec<(Op, String, Fault)>>,
    log: std::sync::Mutex<Vec<String>>,
}

impl FaultFs {
    pub(crate) fn new(inner: super::record_store::RecordDir) -> Arc<Self> {
        Arc::new(Self {
            inner,
            faults: Default::default(),
            log: Default::default(),
        })
    }

    /// Inject `fault` at the next `op` whose target is `target` (any
    /// target when empty; `write` and `sync` have none).
    pub(crate) fn inject(&self, op: Op, target: &str, fault: Fault) {
        self.faults
            .lock()
            .unwrap()
            .push((op, target.to_owned(), fault));
    }

    pub(crate) fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    fn take(&self, op: Op, target: &str) -> Option<Fault> {
        let mut faults = self.faults.lock().unwrap();
        let i = faults
            .iter()
            .position(|(o, t, _)| *o == op && (t.is_empty() || t == target))?;
        Some(faults.remove(i).2)
    }

    fn run<T>(
        &self,
        op: Op,
        target: &str,
        entry: String,
        perform: impl FnOnce() -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        let injected = || std::io::Error::other(format!("injected {op:?} fault"));
        match self.take(op, target) {
            None => {
                self.log.lock().unwrap().push(entry);
                perform()
            }
            Some(Fault::Fail) => {
                self.log.lock().unwrap().push(format!("{entry} (failed)"));
                Err(injected())
            }
            Some(Fault::DoThenFail) => {
                self.log
                    .lock()
                    .unwrap()
                    .push(format!("{entry} (done, failed)"));
                perform()?;
                Err(injected())
            }
            Some(Fault::Panic) => panic!("injected {op:?} panic"),
            Some(Fault::Block { reached, gate }) => {
                if let Some(reached) = reached {
                    let _ = reached.send(());
                }
                let _ = gate.recv();
                self.log.lock().unwrap().push(format!("{entry} (late)"));
                perform()
            }
        }
    }
}

impl super::record_store::RecordFs for FaultFs {
    fn write_temp(&self, bytes: &[u8]) -> std::io::Result<String> {
        self.run(Op::WriteTemp, "", "write".into(), || {
            super::record_store::RecordFs::write_temp(&self.inner, bytes)
        })
    }
    fn rename(&self, from: &str, to: &str) -> std::io::Result<()> {
        self.run(Op::Rename, to, format!("rename {to}"), || {
            super::record_store::RecordFs::rename(&self.inner, from, to)
        })
    }
    fn sync_dir(&self) -> std::io::Result<()> {
        self.run(Op::SyncDir, "", "sync".into(), || {
            super::record_store::RecordFs::sync_dir(&self.inner)
        })
    }
    fn remove(&self, name: &str) -> std::io::Result<()> {
        self.run(Op::Remove, name, format!("remove {name}"), || {
            super::record_store::RecordFs::remove(&self.inner, name)
        })
    }
    fn location(
        &self,
        handle: &str,
        kind: RecordKind,
    ) -> Result<super::record_file::RecordLocation, super::record_file::RecordFileError> {
        super::record_store::RecordFs::location(&self.inner, handle, kind)
    }
    fn dir(&self) -> &super::record_store::RecordDir {
        &self.inner
    }
}

/// A private trusted root with this fixture's process directory open for
/// writing.
pub(crate) struct WriteRoot {
    pub _tmp: tempfile::TempDir,
    pub root: PathBuf,
    pub dir: PathBuf,
}

pub(crate) fn write_root() -> WriteRoot {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap().join("subagent-history");
    private_dir(&root);
    let dir = root.join(PROCESS_DIR);
    WriteRoot {
        _tmp: tmp,
        root,
        dir,
    }
}

impl WriteRoot {
    pub(crate) fn open(&self) -> super::record_store::RecordDir {
        super::record_store::RecordDir::open_or_create(&self.root, PROCESS_DIR).unwrap()
    }

    pub(crate) fn location(&self, kind: RecordKind) -> super::record_file::RecordLocation {
        super::record_file::location_of(&self.root, PROCESS_DIR, "sa_5", kind).unwrap()
    }
}

/// A legacy-backend binding for records that never meet a store.
pub(crate) fn offline_binding() -> Binding {
    Binding {
        backend: super::record::BackendKind::Legacy,
        scope_digest: super::lineage::scope_digest("p0123456789abcdef"),
    }
}

/// Head `generation` of `run` (handle `sa_5`) whose history ends with
/// `last`.
pub(crate) fn head(run: &RunId, generation: u64, last: &str) -> WorkerRecord {
    let mut rec = record(run, offline_binding(), vec![], &[], None);
    rec.kind = RecordKind::Head;
    rec.generation = generation;
    let mut history = rec.history.clone();
    history.push(json!({"role": "assistant", "content": last}));
    with_history(rec, history)
}

/// A child process forked (by a spawner thread) while every descriptor this
/// process has open at that moment is inherited, held between fork and exec
/// until released: a flock taken through an inherited open file description
/// stays held through the child's copy. Released and reaped on every path,
/// unwinding included (the PR #242 pattern).
#[cfg(unix)]
pub(crate) struct ForkedChild {
    release: Option<std::io::PipeWriter>,
    spawner: Option<std::thread::JoinHandle<std::io::Result<std::process::ExitStatus>>>,
}

#[cfg(unix)]
impl ForkedChild {
    /// Fork the child and wait (bounded) until it is blocked before exec.
    pub(crate) fn spawn() -> Self {
        use std::io::Read as _;
        use std::os::fd::AsRawFd as _;
        use std::os::unix::process::CommandExt as _;
        // Close-on-exec, like every descriptor the record store opens.
        let (mut forked, signal) = std::io::pipe().unwrap();
        let (hold, release) = std::io::pipe().unwrap();
        let (signal_fd, hold_fd) = (signal.as_raw_fd(), hold.as_raw_fd());
        let child = Self {
            release: Some(release),
            spawner: Some(std::thread::spawn(move || {
                let mut command = std::process::Command::new("true");
                // SAFETY: only async-signal-safe calls between fork and exec.
                unsafe {
                    command.pre_exec(move || {
                        let interrupted =
                            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR);
                        loop {
                            match libc::write(signal_fd, b"f".as_ptr().cast(), 1) {
                                1 => break,
                                -1 if interrupted() => {}
                                // EOF on the parent's side fails the test.
                                _ => libc::_exit(1),
                            }
                        }
                        let mut byte = 0u8;
                        while libc::read(hold_fd, (&mut byte as *mut u8).cast(), 1) < 0
                            && interrupted()
                        {}
                        Ok(())
                    });
                }
                let status = command.status();
                drop((signal, hold));
                status
            })),
        };
        // Bounded: a child that never signals fails the test, and Drop
        // releases and reaps it.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let mut ready = libc::pollfd {
                fd: forked.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            let timeout = left.as_millis().try_into().unwrap_or(i32::MAX);
            // SAFETY: one valid pollfd.
            match unsafe { libc::poll(&mut ready, 1, timeout) } {
                1 => break,
                0 => panic!("the forked child never signaled"),
                _ => assert_eq!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(libc::EINTR)
                ),
            }
        }
        forked.read_exact(&mut [0; 1]).unwrap();
        child
    }

    /// Let the child exec and exit; true when it exited successfully.
    pub(crate) fn release_and_reap(&mut self) -> bool {
        use std::io::Write as _;
        // The byte, or failing that EOF, ends the child's wait.
        if let Some(mut release) = self.release.take() {
            let _ = release.write_all(b"r");
        }
        // The spawner returns once the child has exited and is reaped.
        self.spawner
            .take()
            .map(std::thread::JoinHandle::join)
            .is_some_and(|r| r.is_ok_and(|s| s.is_ok_and(|s| s.success())))
    }
}

#[cfg(unix)]
impl Drop for ForkedChild {
    fn drop(&mut self) {
        let _ = self.release_and_reap();
    }
}

// ── Fake Axel service ───────────────────────────────────────────────────────

/// A synthetic Axel service: answers the pinned hello, then one operation
/// from `state.json` beside the brain, appending each request to
/// `requests.log`. Mirrors the real service's refusal of unknown operations.
/// `inspect_reply` / `search_reply` in the state are returned verbatim, to
/// play a misbehaving service.
#[cfg(unix)]
pub(crate) const FAKE_AXEL: &str = r#"#!/usr/bin/python3
import sys, json, os
d = os.path.dirname(sys.argv[2])
state = json.load(open(os.path.join(d, 'state.json')))
def reply(r, ok, value):
    out = dict(schema=r['schema'], project=r['project'], ok=ok)
    if ok:
        out['result'] = value
    else:
        out['error'] = dict(code=value, message='synthetic')
    print(json.dumps(out), flush=True)
r = json.loads(sys.stdin.readline())
reply(r, True, dict(backend='axel', revision='@REVISION@', contract='@CONTRACT@'))
r = json.loads(sys.stdin.readline())
op, p = r['operation'], r['payload']
with open(os.path.join(d, 'requests.log'), 'a') as f:
    f.write(json.dumps(dict(op=op, payload=p)) + '\n')
caps = state['capabilities']
owned = 'history_owned' in caps
if op == 'capabilities':
    reply(r, True, caps)
elif op == 'history_inspect' and owned and 'inspect_reply' in state:
    reply(r, True, state['inspect_reply'])
elif op == 'history_search' and owned and 'search_reply' in state:
    reply(r, True, state['search_reply'])
elif op == 'history_inspect' and owned:
    rows = state['entries']
    if not state.get('ignore_ids'):
        rows = [e for e in rows if e['id'] in p['ids']]
    if 'version' not in p:
        reply(r, True, rows)
    elif p['version'] == 2 and 2 in caps['history_owned'].get('versions', []):
        # history_owned/2: the rows and the brain's eviction count (absent
        # from the state means a brain that predates the count: null).
        reply(r, True, dict(entries=rows, evictions=state.get('evictions')))
    else:
        reply(r, False, 'invalid_request')
elif op == 'history_search' and ('ids' not in p or owned):
    rows = state['rows']
    if 'ids' in p and not state.get('ignore_ids'):
        rows = [x for x in rows if x['id'] in p['ids']]
    reply(r, True, rows[:p.get('limit', 8)])
elif op == 'history_fetch':
    rows = state['fetch'].get(p['id'], [])
    reply(r, True, rows[p.get('start', 0):][:p.get('limit', 8)])
else:
    reply(r, False, 'invalid_request')
"#;

#[cfg(unix)]
pub(crate) struct FakeAxel {
    pub(crate) _tmp: tempfile::TempDir,
    pub(crate) dir: PathBuf,
    pub(crate) binding: MemoryBinding,
    /// Held from the script write for the fake's life (it is executed on
    /// every call); declared last, so it drops last.
    _exec: crate::test_env::ScriptExecGuard,
}

#[cfg(unix)]
impl FakeAxel {
    pub(crate) fn new(state: Value) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let exec = crate::test_env::SCRIPT_EXEC_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        let exe = dir.join("service");
        std::fs::write(
            &exe,
            FAKE_AXEL
                .replace("@REVISION@", AXEL_REVISION)
                .replace("@CONTRACT@", CONTRACT),
        )
        .unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).unwrap();
        let binding = MemoryBinding::for_test(
            dir.clone(),
            ProjectScope::for_root(&project_dir(&dir, "repo")).unwrap(),
            &MemoryBackendConfig {
                kind: MemoryBackendKind::Axel,
                executable: Some(exe),
                brain: Some(dir.join("brain.r8")),
                user_scope: false,
            },
        );
        let fake = Self {
            _tmp: tmp,
            dir,
            binding,
            _exec: exec,
        };
        fake.set(state);
        fake
    }

    pub(crate) fn set(&self, state: Value) {
        std::fs::write(self.dir.join("state.json"), state.to_string()).unwrap();
    }

    pub(crate) fn requests(&self) -> Vec<Value> {
        std::fs::read_to_string(self.dir.join("requests.log"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    pub(crate) fn ops(&self) -> Vec<String> {
        self.requests()
            .iter()
            .map(|r| r["op"].as_str().unwrap().to_owned())
            .collect()
    }
}

pub(crate) fn owned_capabilities() -> Value {
    json!({
        "operations": ["history_seal", "history_search", "history_fetch", "history_note",
                       "history_forget", "history_inspect", "capabilities"],
        "history_logical_id": "sha256_length_prefixed",
        "history_fetch": "archived_message_array",
        "history_owned": {"versions": [1], "max_ids": 128}
    })
}

/// [`owned_capabilities`] of a service whose `history_inspect` also speaks
/// version 2 (the brain's eviction count, `evictions` in the fake's state).
pub(crate) fn owned_capabilities_v2() -> Value {
    let mut caps = owned_capabilities();
    caps["history_owned"]["versions"] = json!([1, 2]);
    caps
}

#[cfg(unix)] // its users (lineage and notice tests) are Unix only
pub(crate) fn older_capabilities() -> Value {
    json!({
        "operations": ["history_seal", "history_search", "history_fetch", "history_note",
                       "history_forget", "capabilities"],
        "history_logical_id": "sha256_length_prefixed",
        "history_fetch": "archived_message_array"
    })
}
