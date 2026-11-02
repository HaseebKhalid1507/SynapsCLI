use super::*;
use crate::events::{Event, EventQueue};
use crate::runtime::subagent::SubagentRegistry;
use crate::tools::test_helpers::create_tool_context;
use std::time::Instant;

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("synaps-wait-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn ctx_with(queue: Option<&Arc<EventQueue>>) -> ToolContext {
    let mut ctx = create_tool_context();
    ctx.capabilities.event_queue = queue.cloned();
    ctx
}

/// Run a wait; returns the JSON result (the line after the summary).
async fn wait_json(params: Value, ctx: ToolContext) -> Value {
    let text = WaitTool::default()
        .execute(params, ctx)
        .await
        .expect("a result");
    let (_, json) = text.split_once('\n').expect("summary line, then JSON");
    serde_json::from_str(json).expect("JSON result")
}

fn event(source: &str, severity: Option<Severity>) -> Event {
    Event::simple(source, "body text", severity)
}

// ── parsing ──

fn parse_err(params: Value) -> String {
    parse(&params, None, 3600).expect_err("refused")
}

#[test]
fn defaults_are_any_of_the_conditions_600_s_and_a_medium_interrupt() {
    let spec = parse(&json!({"any": [{"gone": "/x"}]}), None, 3600).unwrap();
    assert_eq!(spec.mode, Mode::Any);
    assert_eq!(spec.timeout, Duration::from_secs(600));
    assert_eq!(spec.interrupt, Some(Severity::Medium));
    let capped = parse(&json!({"all": [{"gone": "/x"}]}), None, 120).unwrap();
    assert_eq!(
        capped.timeout,
        Duration::from_secs(120),
        "the default never exceeds the max"
    );
    assert_eq!(capped.mode, Mode::All);
    let quiet = parse(
        &json!({"any": [{"gone": "/x"}], "interrupt": "none"}),
        None,
        3600,
    )
    .unwrap();
    assert_eq!(quiet.interrupt, None);
}

#[test]
fn relative_paths_resolve_against_the_session_cwd() {
    let spec = parse(
        &json!({"any": [{"file": "out.log"}, {"gone": "lock"}]}),
        Some(Path::new("/work/dir")),
        3600,
    )
    .unwrap();
    assert!(
        matches!(&spec.conds[0], Cond::File { path, .. } if path == Path::new("/work/dir/out.log"))
    );
    assert!(
        matches!(&spec.conds[1], Cond::Gone { path, .. } if path == Path::new("/work/dir/lock"))
    );
}

#[test]
fn malformed_waits_are_refused_with_the_reason() {
    for (params, needle) in [
        (
            json!({"any": [{"gone": "/x"}], "detach": true}),
            "detach is not supported",
        ),
        (
            json!({"any": [{"gone": "/x"}], "poll": 1}),
            "unknown field 'poll'",
        ),
        (
            json!({"any": [{"gone": "/x"}], "all": [{"gone": "/y"}]}),
            "not both",
        ),
        (json!({"timeout_secs": 5}), "any: [...] or all: [...]"),
        (json!({"any": []}), "1 to 8"),
        (json!({"any": vec![json!({"gone": "/x"}); 9]}), "1 to 8"),
        (
            json!({"any": [{"gone": "/x"}], "timeout_secs": 0}),
            "from 1 to 3600",
        ),
        (
            json!({"any": [{"gone": "/x"}], "timeout_secs": 3601}),
            "from 1 to 3600",
        ),
        (
            json!({"any": [{"gone": "/x"}], "interrupt": "loud"}),
            "interrupt must be",
        ),
        (
            json!({"any": [{"job": "job_1"}]}),
            "job conditions are not available",
        ),
        (
            json!({"any": [{"file": "/x", "gone": "/y"}]}),
            "exactly one of",
        ),
        (
            json!({"any": [{"gone": "/x", "contains": "a"}]}),
            "'contains' does not belong",
        ),
        (json!({"any": [{"file": ""}]}), "non-empty path"),
        (
            json!({"any": [{"file": "/x", "tail_lines": 201}]}),
            "tail_lines must be 1 to 200",
        ),
        (
            json!({"any": [{"file": "/x", "contains": ""}]}),
            "contains must be",
        ),
        (
            json!({"any": [{"pid": 0}]}),
            "pid must be a positive integer",
        ),
        (
            json!({"any": [{"event": {"data": {"k": "v"}}}]}),
            "only type, source and min_severity",
        ),
        (
            json!({"any": [{"event": {"min_severity": "urgent"}}]}),
            "unknown min_severity",
        ),
        (json!({"any": ["file"]}), "a condition is an object"),
    ] {
        let error = parse_err(params.clone());
        assert!(error.contains(needle), "{params}: {error}");
    }
}

// ── file, gone ──

#[tokio::test]
async fn a_file_that_appears_meets_the_wait() {
    let dir = scratch("appears");
    let path = dir.join("done");
    let writer = {
        let path = path.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            std::fs::write(path, "ok").unwrap();
        })
    };
    let result = wait_json(
        json!({"any": [{"file": path.to_str().unwrap()}], "timeout_secs": 10}),
        ctx_with(None),
    )
    .await;
    writer.await.unwrap();
    assert_eq!(result["outcome"], "met", "{result}");
    assert_eq!(result["met"], json!([0]));
    assert_eq!(result["conditions"][0]["detail"]["exists"], true);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn nothing_changing_times_out() {
    let dir = scratch("timeout");
    let started = Instant::now();
    let result = wait_json(
        json!({"any": [{"file": dir.join("never").to_str().unwrap()}], "timeout_secs": 1}),
        ctx_with(None),
    )
    .await;
    assert_eq!(result["outcome"], "timeout", "{result}");
    assert_eq!(result["conditions"][0]["state"], "waiting");
    assert!(started.elapsed() >= Duration::from_secs(1));
    assert!(started.elapsed() < Duration::from_secs(5));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn gone_means_no_such_path_and_a_denied_path_is_an_error() {
    let dir = scratch("gone");
    let missing = dir.join("missing");
    let result = wait_json(
        json!({"any": [{"gone": missing.to_str().unwrap()}]}),
        ctx_with(None),
    )
    .await;
    assert_eq!(result["outcome"], "met", "{result}");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let locked = dir.join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::write(locked.join("inside"), "x").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let denied = std::fs::symlink_metadata(locked.join("inside")).is_err();
        let result = wait_json(
            json!({"any": [{"gone": locked.join("inside").to_str().unwrap()}], "timeout_secs": 2}),
            ctx_with(None),
        )
        .await;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
        if denied {
            assert_eq!(
                result["outcome"], "error",
                "permission denied is not absence: {result}"
            );
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn contains_scans_forward_across_appends_and_rescans_a_new_file() {
    let dir = scratch("scan");
    let path = dir.join("log");
    let mut scan = FileScan::default();
    std::fs::write(&path, "build abc").unwrap();
    assert!(!scan_for(&path, "cde", &mut scan).unwrap());
    // The needle spans the previous scan's end.
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    std::io::Write::write_all(&mut file, b"de").unwrap();
    assert!(
        scan_for(&path, "cde", &mut scan).unwrap(),
        "found across the boundary"
    );
    assert!(scan_for(&path, "cde", &mut scan).unwrap(), "stays seen");
    // A rewrite with a new inode rescans from the start.
    let fresh = dir.join("fresh");
    std::fs::write(&fresh, "nothing here").unwrap();
    std::fs::rename(&fresh, &path).unwrap();
    assert!(
        !scan_for(&path, "cde", &mut scan).unwrap(),
        "a new file is scanned anew"
    );
    // A shrink of the same file rescans too.
    std::fs::write(&path, "x").unwrap();
    assert!(!scan_for(&path, "cde", &mut scan).unwrap());
    std::fs::write(&path, "xcde").unwrap();
    assert!(scan_for(&path, "cde", &mut scan).unwrap());
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[tokio::test]
async fn a_fifo_is_refused_for_contains_without_blocking() {
    let dir = scratch("fifo");
    let fifo = dir.join("pipe");
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap();
    assert!(made.success());
    let started = Instant::now();
    let result = wait_json(
        json!({"any": [{"file": fifo.to_str().unwrap(), "contains": "x"}], "timeout_secs": 5}),
        ctx_with(None),
    )
    .await;
    assert_eq!(result["outcome"], "error", "{result}");
    assert!(result["conditions"][0]["detail"]
        .as_str()
        .unwrap()
        .contains("not a regular file"));
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "never blocked on the FIFO"
    );
    // Existence alone does not open it.
    let exists = wait_json(
        json!({"any": [{"file": fifo.to_str().unwrap()}]}),
        ctx_with(None),
    )
    .await;
    assert_eq!(exists["outcome"], "met", "{exists}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The wait refuses a FIFO from its path metadata before it opens anything
/// (the test above), so the check on the OPENED file, which covers a regular
/// file swapped for a FIFO between that stat and the open, is tested here
/// directly.
#[cfg(unix)]
#[test]
fn open_regular_refuses_a_fifo_it_opened_without_blocking() {
    let dir = scratch("fifo-open");
    let fifo = dir.join("pipe");
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap();
    assert!(made.success());
    let started = Instant::now();
    let err = open_regular(&fifo).expect_err("a FIFO is not a regular file");
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{err}");
    assert!(err.to_string().contains("not a regular file"), "{err}");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the open never blocked on the FIFO"
    );
    let regular = dir.join("log");
    std::fs::write(&regular, b"x\n").unwrap();
    let (_, meta) = open_regular(&regular).expect("a regular file opens");
    assert!(meta.is_file());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_tail_returns_the_last_lines_within_its_bounds() {
    let dir = scratch("tail");
    let path = dir.join("out");
    let text: String = (1..=300).map(|i| format!("line {i}\n")).collect();
    std::fs::write(&path, text).unwrap();
    let result = wait_json(
        json!({"any": [{"file": path.to_str().unwrap(), "contains": "line 300", "tail_lines": 3}]}),
        ctx_with(None),
    )
    .await;
    assert_eq!(result["outcome"], "met", "{result}");
    assert_eq!(
        result["tail"][0]["lines"],
        json!(["line 298", "line 299", "line 300"])
    );
    assert_eq!(result["tail"][0]["truncated"], true);
    let (lines, truncated) = read_tail(&path, 200, 64).unwrap();
    assert!(
        lines.iter().map(String::len).sum::<usize>() <= 64,
        "the byte bound holds"
    );
    assert!(truncated);
    assert!(
        lines.iter().all(|l| l.starts_with("line ")),
        "a partial first line is skipped: {lines:?}"
    );
    assert_eq!(lines.last().map(String::as_str), Some("line 300"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn tail_invalid_utf8_obeys_output_budget() {
    let dir = scratch("tail-utf8");
    // Each 0xFF byte decodes to U+FFFD, three bytes of UTF-8: a budget on
    // the bytes read alone lets the returned text grow threefold.
    let line: Vec<u8> = [0xFF_u8; 40].into_iter().chain([b'\n']).collect();
    let data: Vec<u8> = (0..400).flat_map(|_| line.iter().copied()).collect();
    let (a, b) = (dir.join("a"), dir.join("b"));
    std::fs::write(&a, &data).unwrap();
    std::fs::write(&b, &data).unwrap();
    let (lines, truncated) = read_tail(&a, 200, 64).unwrap();
    let emitted: usize = lines.iter().map(String::len).sum();
    assert!(emitted <= 64, "UTF-8 bytes returned: {emitted}");
    assert!(
        !lines.is_empty() && lines.iter().all(|l| l.chars().all(|c| c == '\u{FFFD}')),
        "the newest line, cut on a char boundary: {lines:?}"
    );
    assert!(truncated);
    let result = wait_json(
        json!({"all": [
            {"file": a.to_str().unwrap(), "tail_lines": 200},
            {"file": b.to_str().unwrap(), "tail_lines": 200},
        ]}),
        ctx_with(None),
    )
    .await;
    assert_eq!(result["outcome"], "met", "{result}");
    let tails = result["tail"].as_array().expect("tails");
    assert_eq!(tails.len(), 2, "{result}");
    let total: usize = tails
        .iter()
        .flat_map(|t| t["lines"].as_array().unwrap())
        .map(|l| l.as_str().unwrap().len())
        .sum();
    assert!(total <= MAX_TAIL_BYTES, "all tails share one bound: {total}");
    assert!(tails.iter().all(|t| t["truncated"] == true), "{result}");
    let _ = std::fs::remove_dir_all(&dir);
}

// ── pid ──

#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_pid_exit_meets_the_wait_and_echoes_the_instance() {
    let mut child = std::process::Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let pid = child.id();
    let ticks = ProcessIdentity::identify(pid).unwrap().start_ticks().unwrap();
    let killer = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        child.kill().unwrap();
        child
    });
    let result = wait_json(
        json!({"any": [{"pid": pid}], "timeout_secs": 10}),
        ctx_with(None),
    )
    .await;
    let mut child = killer.await.unwrap();
    assert_eq!(result["outcome"], "met", "{result}");
    let detail = &result["conditions"][0]["detail"];
    assert_eq!(detail["exit"], "zombie", "not reaped yet: {detail}");
    assert_eq!(detail["start_ticks"], ticks, "captured at entry");
    child.wait().unwrap();
    let gone = wait_json(
        json!({"any": [{"pid": pid, "start_ticks": ticks}]}),
        ctx_with(None),
    )
    .await;
    assert!(
        matches!(
            gone["conditions"][0]["detail"]["exit"].as_str(),
            Some("gone" | "reused")
        ),
        "{gone}"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn another_start_time_reads_as_a_reused_pid() {
    let me = std::process::id();
    let ticks = ProcessIdentity::identify(me).unwrap().start_ticks().unwrap();
    let result = wait_json(
        json!({"any": [{"pid": me, "start_ticks": ticks + 1}]}),
        ctx_with(None),
    )
    .await;
    assert_eq!(result["outcome"], "met");
    assert_eq!(result["conditions"][0]["detail"]["exit"], "reused");
}

// ── subagent ──

#[tokio::test]
async fn a_subagent_that_finishes_meets_the_wait() {
    use crate::runtime::subagent::{SubagentHandle, SubagentState, SubagentStatus};
    let state = Arc::new(std::sync::RwLock::new(SubagentState::new()));
    let handle = SubagentHandle::new(
        "sa_5".into(),
        5,
        "w".into(),
        "task".into(),
        "muse/muse-wonder-1.0".into(),
        String::new(),
        60,
        Arc::clone(&state),
        None,
        None,
        None,
    );
    let mut registry = SubagentRegistry::new();
    let id = registry.register(handle);
    let registry = Arc::new(std::sync::Mutex::new(registry));
    let finisher = {
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            state.write().unwrap().status = SubagentStatus::Completed;
        })
    };
    let mut ctx = ctx_with(None);
    ctx.capabilities.subagent_registry = Some(registry);
    let result = wait_json(json!({"any": [{"subagent": id}], "timeout_secs": 10}), ctx).await;
    finisher.await.unwrap();
    assert_eq!(result["outcome"], "met", "{result}");
    assert_eq!(result["conditions"][0]["detail"]["status"], "completed");
}

#[tokio::test]
async fn an_unknown_handle_or_no_registry_is_an_error() {
    let result = wait_json(json!({"any": [{"subagent": "sa_9"}]}), ctx_with(None)).await;
    assert_eq!(result["outcome"], "error", "{result}");
    let mut ctx = ctx_with(None);
    ctx.capabilities.subagent_registry =
        Some(Arc::new(std::sync::Mutex::new(SubagentRegistry::new())));
    let result = wait_json(json!({"any": [{"subagent": "sa_9"}]}), ctx).await;
    assert_eq!(result["outcome"], "error");
    assert!(result["conditions"][0]["detail"]
        .as_str()
        .unwrap()
        .contains("no subagent with handle_id 'sa_9'"));
}

/// A worker the daemon resumed after a reload: waiting on its old id waits
/// on the run that continues it (the stopped run's end never meets it),
/// and while the launch is under way it is still waiting (mutant: drop the
/// alias in `check_subagent`).
#[tokio::test]
async fn a_wait_on_a_daemon_resumed_worker_follows_its_new_run() {
    use crate::runtime::subagent::{SubagentHandle, SubagentState, SubagentStatus};
    let state = Arc::new(std::sync::RwLock::new(SubagentState::new()));
    let mut registry = SubagentRegistry::new();
    registry.register(SubagentHandle::new(
        "sa_31".into(),
        31,
        "w".into(),
        "task".into(),
        "muse/muse-wonder-1.0".into(),
        String::new(),
        60,
        Arc::clone(&state),
        None,
        None,
        None,
    ));
    // sa_30's stopped run is archived; the daemon resumed it as sa_31.
    registry.adopt_carried(&crate::runtime::subagent::CarriedWorker {
        id: "sa_30".into(),
        status: "cancelled".into(),
        archive_path: Some("/a/sa_30.json".into()),
        stopped_by_reload: true,
        wake: true,
        ..Default::default()
    });
    registry.begin_daemon_resume("sa_30");
    registry.finish_daemon_resume("sa_30", Some("sa_31"));
    registry.begin_daemon_resume("sa_32");
    let registry = Arc::new(std::sync::Mutex::new(registry));

    let mut ctx = ctx_with(None);
    ctx.capabilities.subagent_registry = Some(Arc::clone(&registry));
    let result = wait_json(json!({"any": [{"subagent": "sa_30"}], "timeout_secs": 1}), ctx).await;
    assert_eq!(result["outcome"], "timeout", "sa_31 still runs: {result}");

    let finisher = {
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            state.write().unwrap().status = SubagentStatus::Completed;
        })
    };
    let mut ctx = ctx_with(None);
    ctx.capabilities.subagent_registry = Some(Arc::clone(&registry));
    let result = wait_json(json!({"any": [{"subagent": "sa_30"}], "timeout_secs": 10}), ctx).await;
    finisher.await.unwrap();
    assert_eq!(result["outcome"], "met", "{result}");
    assert_eq!(result["conditions"][0]["detail"]["status"], "completed");
    assert_eq!(result["conditions"][0]["detail"]["resumed_as"], "sa_31");

    let mut ctx = ctx_with(None);
    ctx.capabilities.subagent_registry = Some(registry);
    let result = wait_json(json!({"any": [{"subagent": "sa_32"}], "timeout_secs": 1}), ctx).await;
    assert_eq!(result["outcome"], "timeout", "being resumed: still waiting: {result}");
}

// ── events ──

#[tokio::test]
async fn a_matching_event_meets_the_wait_with_its_header_only() {
    let queue = Arc::new(EventQueue::new(16));
    queue.push(event("worker-s", Some(Severity::High))).unwrap(); // before: never counts
    let pusher = {
        let queue = Arc::clone(&queue);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            queue.push(event("worker-x", Some(Severity::Low))).unwrap();
            queue.push(event("worker-s", Some(Severity::Low))).unwrap();
        })
    };
    let result = wait_json(
        json!({"any": [{"event": {"source": "worker-s"}}], "timeout_secs": 10}),
        ctx_with(Some(&queue)),
    )
    .await;
    pusher.await.unwrap();
    assert_eq!(result["outcome"], "met", "{result}");
    assert_eq!(result["events"][0]["source"], "worker-s");
    assert_eq!(result["events"][0]["severity"], "low");
    assert!(!result.to_string().contains("body text"), "headers only");
}

#[tokio::test]
async fn an_event_at_the_interrupt_level_ends_the_wait_and_a_lower_one_does_not() {
    let queue = Arc::new(EventQueue::new(16));
    let dir = scratch("interrupt");
    let never = dir.join("never");
    let pusher = {
        let queue = Arc::clone(&queue);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            queue.push(event("fyi", Some(Severity::Low))).unwrap();
            tokio::time::sleep(Duration::from_millis(300)).await;
            queue.push(event("coordinator", None)).unwrap(); // missing = medium
        })
    };
    let result = wait_json(
        json!({"any": [{"file": never.to_str().unwrap()}], "timeout_secs": 10}),
        ctx_with(Some(&queue)),
    )
    .await;
    pusher.await.unwrap();
    assert_eq!(result["outcome"], "event", "{result}");
    assert_eq!(
        result["events"],
        json!([{"id": result["events"][0]["id"], "type": "message", "source": "coordinator", "severity": "medium"}])
    );
    // With interrupt none, only conditions end it.
    queue
        .push(event("coordinator", Some(Severity::Critical)))
        .unwrap();
    let quiet = wait_json(
        json!({"any": [{"file": never.to_str().unwrap()}], "timeout_secs": 1, "interrupt": "none"}),
        ctx_with(Some(&queue)),
    )
    .await;
    assert_eq!(quiet["outcome"], "timeout", "{quiet}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_display_only_event_neither_matches_nor_interrupts() {
    let queue = Arc::new(EventQueue::new(16));
    let pusher = {
        let queue = Arc::clone(&queue);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let mut hidden = event("worker-s", Some(Severity::Critical));
            hidden.content.disclosure = Some(agent_core::disclosure::DisclosureClass::NeverPersist);
            queue.push(hidden).unwrap();
        })
    };
    let result = wait_json(
        json!({"any": [{"event": {"source": "worker-s"}}], "timeout_secs": 1}),
        ctx_with(Some(&queue)),
    )
    .await;
    pusher.await.unwrap();
    assert_eq!(result["outcome"], "timeout", "{result}");
    assert!(result.get("events").is_none());
}

#[test]
fn overflow_ends_the_wait_as_arrival_information_unavailable() {
    let spec = parse(
        &json!({"all": [{"event": {"source": "a"}}, {"gone": "/x"}]}),
        None,
        3600,
    )
    .unwrap();
    let mut watch = EventWatch {
        matched: vec![None; 2],
        ..EventWatch::default()
    };
    watch.absorb(
        &spec,
        crate::events::queue::ArrivalsSince {
            arrivals: Vec::new(),
            overflow: true,
            last_seq: 400,
        },
    );
    assert!(watch.overflow);
    assert_eq!(watch.cursor, 400);
    assert!(
        watch.matched.iter().all(Option::is_none),
        "overflow is no match"
    );
    let text = render(
        &spec,
        "event",
        None,
        Duration::from_secs(1),
        &[],
        &watch,
        Vec::new(),
        false,
    );
    assert!(text.contains("arrival information unavailable"), "{text}");
    assert!(text.contains("\"overflow\":true"), "{text}");
}

#[tokio::test]
async fn all_needs_every_condition_and_an_event_stays_seen() {
    let queue = Arc::new(EventQueue::new(16));
    let dir = scratch("all");
    let path = dir.join("flag");
    let steps = {
        let queue = Arc::clone(&queue);
        let path = path.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            queue.push(event("ci", Some(Severity::Low))).unwrap();
            tokio::time::sleep(Duration::from_millis(300)).await;
            std::fs::write(path, "x").unwrap();
        })
    };
    let result = wait_json(
        json!({"all": [{"event": {"source": "ci"}}, {"file": path.to_str().unwrap()}], "timeout_secs": 10}),
        ctx_with(Some(&queue)),
    )
    .await;
    steps.await.unwrap();
    assert_eq!(result["outcome"], "met", "{result}");
    assert_eq!(result["met"], json!([0, 1]));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_worker_cannot_wait_on_events() {
    let queue = Arc::new(EventQueue::new(16));
    let mut ctx = ctx_with(Some(&queue));
    ctx.capabilities.delegation_parent = Some("sa_1".into());
    let error = WaitTool::default()
        .execute(json!({"any": [{"event": {}}]}), ctx)
        .await
        .expect_err("refused");
    assert!(
        error
            .to_string()
            .contains("event conditions need a foreground session"),
        "{error}"
    );
}

// ── quiesce (reload, host checkpoint) ──

/// Run a wait; returns the summary line and the JSON result.
async fn wait_text(params: Value, ctx: ToolContext) -> (String, Value) {
    let text = WaitTool::default()
        .execute(params, ctx)
        .await
        .expect("a result");
    let (line, json) = text.split_once('\n').expect("summary line, then JSON");
    (line.to_string(), serde_json::from_str(json).expect("JSON result"))
}

#[tokio::test]
async fn a_quiesce_set_before_the_call_interrupts_it_at_once() {
    let queue = Arc::new(EventQueue::new(16));
    let dir = scratch("quiesce-before");
    queue.set_quiesce(Some(Quiesce::Reload));
    let started = Instant::now();
    let (line, result) = wait_text(
        json!({"any": [{"file": dir.join("never").to_str().unwrap()}], "timeout_secs": 30}),
        ctx_with(Some(&queue)),
    )
    .await;
    assert_eq!(result["outcome"], "interrupted", "{result}");
    assert_eq!(result["reason"], "reload", "{result}");
    assert!(started.elapsed() < Duration::from_secs(2), "at once");
    assert!(line.contains("the daemon is reloading"), "{line}");
    assert!(
        line.contains("Nothing will wake this session for this wait"),
        "{line}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_quiesce_during_the_wait_ends_it_before_the_next_tick() {
    let queue = Arc::new(EventQueue::new(16));
    let dir = scratch("quiesce-during");
    let slot = queue.quiesce_slot();
    let setter = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        slot.send_replace(Some(Quiesce::Checkpoint));
    });
    let started = Instant::now();
    let (line, result) = wait_text(
        json!({"any": [{"file": dir.join("never").to_str().unwrap()}], "timeout_secs": 30, "interrupt": "none"}),
        ctx_with(Some(&queue)),
    )
    .await;
    setter.await.unwrap();
    assert_eq!(result["outcome"], "interrupted", "{result}");
    assert_eq!(result["reason"], "checkpoint", "{result}");
    // Woken by the flag itself, not by the 1 s tick.
    assert!(
        started.elapsed() < Duration::from_millis(900),
        "{:?}",
        started.elapsed()
    );
    assert!(line.contains("checkpointing this session"), "{line}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn met_beats_interrupted_and_interrupted_beats_an_event() {
    let queue = Arc::new(EventQueue::new(16));
    let dir = scratch("quiesce-order");
    let path = dir.join("here");
    std::fs::write(&path, "x").unwrap();
    queue.set_quiesce(Some(Quiesce::Reload));
    let met = wait_json(
        json!({"any": [{"file": path.to_str().unwrap()}], "timeout_secs": 10}),
        ctx_with(Some(&queue)),
    )
    .await;
    assert_eq!(met["outcome"], "met", "{met}");
    assert!(met.get("reason").is_none(), "{met}");
    // An interrupting event and the flag on one pass: interrupted.
    queue.set_quiesce(None);
    let both = {
        let queue = Arc::clone(&queue);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            queue.push(event("coordinator", Some(Severity::High))).unwrap();
            queue.set_quiesce(Some(Quiesce::Checkpoint));
        })
    };
    let result = wait_json(
        json!({"any": [{"file": dir.join("never").to_str().unwrap()}], "timeout_secs": 10}),
        ctx_with(Some(&queue)),
    )
    .await;
    both.await.unwrap();
    assert_eq!(result["outcome"], "interrupted", "{result}");
    assert_eq!(result["reason"], "checkpoint");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_cleared_flag_or_a_spurious_wake_lets_the_wait_run_on() {
    let queue = Arc::new(EventQueue::new(16));
    let dir = scratch("quiesce-cleared");
    queue.set_quiesce(Some(Quiesce::Reload));
    queue.set_quiesce(None);
    let slot = queue.quiesce_slot();
    let poke = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        slot.send_replace(None); // marks changed, still not quiesced
    });
    let result = wait_json(
        json!({"any": [{"file": dir.join("never").to_str().unwrap()}], "timeout_secs": 1}),
        ctx_with(Some(&queue)),
    )
    .await;
    poke.await.unwrap();
    assert_eq!(result["outcome"], "timeout", "{result}");
    assert!(result.get("reason").is_none(), "{result}");
    let _ = std::fs::remove_dir_all(&dir);
}

// ── admission and cancel ──

#[tokio::test]
async fn a_second_concurrent_wait_in_one_session_is_refused_and_cancel_frees_the_slot() {
    let queue = Arc::new(EventQueue::new(16));
    let dir = scratch("admission");
    let never = json!({"any": [{"file": dir.join("never").to_str().unwrap()}], "timeout_secs": 30, "interrupt": "none"});
    let tool = Arc::new(WaitTool::default());
    let first = {
        let tool = Arc::clone(&tool);
        let (never, ctx) = (never.clone(), ctx_with(Some(&queue)));
        tokio::spawn(async move { tool.execute(never, ctx).await })
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while tool.running.lock().unwrap().is_empty() {
        assert!(Instant::now() < deadline, "the first wait never started");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let second = tool.execute(never.clone(), ctx_with(Some(&queue))).await;
    assert_eq!(
        second.expect_err("refused").to_string(),
        RuntimeError::Tool(SECOND_WAIT_REFUSED.into()).to_string()
    );
    // Another session is not affected.
    let other = Arc::new(EventQueue::new(16));
    let gone = json!({"any": [{"gone": dir.join("never").to_str().unwrap()}]});
    assert!(tool
        .execute(gone.clone(), ctx_with(Some(&other)))
        .await
        .is_ok());
    // Esc drops the first call: its slot goes with it.
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    assert!(
        tool.running.lock().unwrap().is_empty(),
        "nothing left behind"
    );
    assert!(tool.execute(gone, ctx_with(Some(&queue))).await.is_ok());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn waits_without_an_event_queue_are_admitted_one_at_a_time_too() {
    let dir = scratch("admission-unqueued");
    let never = json!({"any": [{"file": dir.join("never").to_str().unwrap()}], "timeout_secs": 30});
    let tool = Arc::new(WaitTool::default());
    let first = {
        let (tool, never) = (Arc::clone(&tool), never.clone());
        tokio::spawn(async move { tool.execute(never, ctx_with(None)).await })
    };
    eventually("the first wait to start", Duration::from_secs(5), || {
        !tool.running.lock().unwrap().is_empty()
    })
    .await;
    let second = tool.execute(never, ctx_with(None)).await;
    assert_eq!(
        second.expect_err("refused").to_string(),
        RuntimeError::Tool(SECOND_WAIT_REFUSED.into()).to_string()
    );
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    assert!(tool.running.lock().unwrap().is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

// ── checks stuck in the kernel (an injected hold, never a real path) ──

async fn eventually(what: &str, budget: Duration, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + budget;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// One wait, bounded by the test: a wait that hangs on its blocked check
/// fails here instead of hanging the suite (the hold is released when its
/// guard drops, on a failure too).
async fn bounded(tool: &WaitTool, params: Value, ctx: ToolContext) -> Result<String> {
    tokio::time::timeout(Duration::from_secs(10), tool.execute(params, ctx))
        .await
        .expect("the wait did not end while its check was blocked")
}

fn split(text: &str) -> (String, Value) {
    let (line, json) = text.split_once('\n').expect("summary line, then JSON");
    (line.to_string(), serde_json::from_str(json).expect("JSON result"))
}

#[tokio::test]
async fn a_probe_blocked_past_the_deadline_ends_the_wait_as_a_timeout() {
    let queue = Arc::new(EventQueue::new(16));
    let key = RuntimeKey::Queue(Arc::as_ptr(&queue) as usize);
    let dir = scratch("held-probe");
    let path = dir.join("held");
    let guard = hold::hold(&path, hold::Site::Probe);
    let tool = WaitTool::default();
    let started = Instant::now();
    let text = bounded(
        &tool,
        json!({"any": [{"file": path.to_str().unwrap()}], "timeout_secs": 1, "interrupt": "none"}),
        ctx_with(Some(&queue)),
    )
    .await
    .expect("a result");
    let elapsed = started.elapsed();
    let (line, result) = split(&text);
    assert_eq!(result["outcome"], "timeout", "{result}");
    assert_eq!(result["check_unfinished"], true, "{result}");
    assert_eq!(result["conditions"][0]["state"], "waiting", "{result}");
    assert!(line.contains("had not returned"), "{line}");
    assert!(elapsed < Duration::from_secs(3), "the deadline plus a grace: {elapsed:?}");
    // The check outlives its wait, holding one of the runtime's slots.
    assert_eq!(guard.entered(), 1);
    assert_eq!(tool.blocking.outstanding(key), 1);
    drop(guard);
    eventually("the released check to free its slot", Duration::from_secs(5), || {
        tool.blocking.outstanding(key) == 0
    })
    .await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_quiesce_ends_a_wait_whose_probe_is_blocked() {
    let queue = Arc::new(EventQueue::new(16));
    let dir = scratch("held-quiesce");
    let path = dir.join("held");
    let guard = hold::hold(&path, hold::Site::Probe);
    let slot = queue.quiesce_slot();
    let setter = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        slot.send_replace(Some(Quiesce::Checkpoint));
    });
    let tool = WaitTool::default();
    let started = Instant::now();
    let text = bounded(
        &tool,
        json!({"any": [{"file": path.to_str().unwrap()}], "timeout_secs": 30, "interrupt": "none"}),
        ctx_with(Some(&queue)),
    )
    .await
    .expect("a result");
    setter.await.unwrap();
    let (_, result) = split(&text);
    assert_eq!(result["outcome"], "interrupted", "{result}");
    assert_eq!(result["reason"], "checkpoint", "{result}");
    assert_eq!(result["check_unfinished"], true, "{result}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the flag plus a grace, not the 30 s timeout: {:?}",
        started.elapsed()
    );
    assert_eq!(guard.entered(), 1);
    drop(guard);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn checks_that_never_return_fill_the_slots_and_new_waits_are_refused_until_they_do() {
    let queue = Arc::new(EventQueue::new(16));
    let key = RuntimeKey::Queue(Arc::as_ptr(&queue) as usize);
    let dir = scratch("held-cap");
    let path = dir.join("held");
    let here = dir.join("here");
    std::fs::write(&here, "x").unwrap();
    let guard = hold::hold(&path, hold::Site::Probe);
    let tool = WaitTool::default();
    let held = json!({"any": [{"file": path.to_str().unwrap()}], "timeout_secs": 1, "interrupt": "none"});
    for n in 1..=MAX_BLOCKING_PER_RUNTIME {
        let text = bounded(&tool, held.clone(), ctx_with(Some(&queue)))
            .await
            .expect("a result");
        let (_, result) = split(&text);
        assert_eq!(result["outcome"], "timeout", "{result}");
        assert_eq!(tool.blocking.outstanding(key), n);
    }
    assert_eq!(guard.entered(), MAX_BLOCKING_PER_RUNTIME);
    let met = json!({"any": [{"file": here.to_str().unwrap()}]});
    let refused = bounded(&tool, met.clone(), ctx_with(Some(&queue))).await;
    assert_eq!(
        refused.expect_err("refused").to_string(),
        RuntimeError::Tool(CHECKS_STILL_BLOCKED.into()).to_string()
    );
    // Another runtime has its own slots.
    let other = Arc::new(EventQueue::new(16));
    assert!(bounded(&tool, met.clone(), ctx_with(Some(&other))).await.is_ok());
    drop(guard);
    eventually("the released checks to free their slots", Duration::from_secs(5), || {
        tool.blocking.outstanding(key) == 0
    })
    .await;
    let text = bounded(&tool, met, ctx_with(Some(&queue)))
        .await
        .expect("admitted again");
    assert_eq!(split(&text).1["outcome"], "met");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_tail_not_read_in_time_is_reported_unavailable() {
    let dir = scratch("held-tail");
    let path = dir.join("out");
    std::fs::write(&path, "a\nb\n").unwrap();
    let guard = hold::hold(&path, hold::Site::Tail);
    let tool = WaitTool::default();
    let started = Instant::now();
    let text = bounded(
        &tool,
        json!({"any": [{"file": path.to_str().unwrap(), "tail_lines": 2}]}),
        ctx_with(None),
    )
    .await
    .expect("a result");
    let (_, result) = split(&text);
    assert_eq!(result["outcome"], "met", "{result}");
    assert_eq!(result["tail"][0]["unavailable"], "timeout", "{result}");
    assert!(result["tail"][0].get("lines").is_none(), "{result}");
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "the tail bound: {:?}",
        started.elapsed()
    );
    assert_eq!(guard.entered(), 1);
    drop(guard);
    eventually("the released tail to free its slot", Duration::from_secs(5), || {
        tool.blocking.outstanding(RuntimeKey::Unqueued) == 0
    })
    .await;
    let _ = std::fs::remove_dir_all(&dir);
}

// ── the daemon log ──

/// The "wait ended" line reaches the daemon log: its filter (RUST_LOG unset)
/// keeps it at INFO under [`LOG_TARGET`] and drops the same level at the
/// earlier `synaps::wait` target and at an unrelated one (the controls, so
/// a filter that kept everything couldn't pass).
#[tokio::test]
async fn the_wait_line_reaches_the_daemon_log() {
    let dir = scratch("daemon-log");
    let path = dir.join("here");
    std::fs::write(&path, "ok").unwrap();
    let (log, guard) = crate::tools::test_helpers::DaemonLog::capture();
    let result = wait_json(
        json!({"any": [{"file": path.to_str().unwrap()}], "timeout_secs": 5}),
        ctx_with(None),
    )
    .await;
    tracing::info!(target: "synaps::wait", "control: the earlier target");
    tracing::info!(target: "some_dependency", "control: an unrelated target");
    drop(guard);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(result["outcome"], "met", "{result}");
    let ended = log.find(LOG_TARGET, tracing::Level::INFO, "wait ended");
    assert_eq!(ended.len(), 1, "{:?}", log.lines());
    assert_eq!(ended[0].fields["outcome"], "met", "{:?}", ended[0]);
    assert!(
        !log.passed("control:"),
        "the controls are dropped: {:?}",
        log.lines()
    );
}
