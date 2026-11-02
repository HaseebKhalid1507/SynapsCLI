// Slice 8a, visibility (spec §8): a v2 run's window counters and its
// notice ring in subagent_status, subagent_collect and the completion
// event. Included at the end of launch_tests.rs as `mod visibility`. A v1
// run (no plan, or a fallback to rollover off) shows none of it: the
// historical gate (`visibility_gate`) pins its surfaces.

use crate::runtime::subagent::{
    bounded_notice, SubagentResult, WindowCounters, WorkerVisibility, NOTICE_BYTES, NOTICE_RING,
};
use crate::tools::subagent::record::{HistoryDigest, Predecessor};
use crate::tools::Tool;

fn notice(text: &str) -> StreamEvent {
    StreamEvent::Session(SessionEvent::Notice(text.into()))
}

fn counters(window_ordinal: u64, rollovers_this_run: u64, inherited_windows: u64) -> WindowCounters {
    WindowCounters {
        window_ordinal,
        rollovers_this_run,
        inherited_windows,
    }
}

async fn tool_body(tool: &dyn Tool, registry: &Arc<Mutex<SubagentRegistry>>, handle_id: &str) -> Value {
    let mut ctx = crate::tools::test_helpers::create_tool_context();
    ctx.capabilities.subagent_registry = Some(Arc::clone(registry));
    ctx.capabilities.orchestration = None;
    let body = tool
        .execute(json!({ "handle_id": handle_id }), ctx)
        .await
        .unwrap_or_else(|e| panic!("{}: {e}", tool.name()));
    serde_json::from_str(&body).unwrap()
}

/// What the caller sees of a finished worker: its subagent_status body,
/// its completion event's data and its subagent_collect body, after the
/// worker is finished the way start.rs finishes it.
async fn surfaces(state: &Arc<RwLock<SubagentState>>, result: SubagentResult) -> (Value, Value, Value) {
    state.write().unwrap().status = SubagentStatus::Completed;
    let (steer_tx, _steer_rx) = tokio::sync::mpsc::unbounded_channel();
    let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel();
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    let registry = Arc::new(Mutex::new(SubagentRegistry::new()));
    registry.lock().unwrap().register(SubagentHandle::new(
        "sa_5".into(),
        5,
        "worker".into(),
        "task".into(),
        BOOKED_MODEL.into(),
        "s".into(),
        60,
        Arc::clone(state),
        Some(steer_tx),
        Some(shutdown_tx),
        Some(result_rx),
    ));
    let queue = Arc::new(crate::events::EventQueue::new(10));
    let dir = tempfile::tempdir().unwrap();
    crate::tools::subagent::finalize::finalize_subagent_in(
        state,
        Some(&queue),
        "sa_5",
        5,
        "worker",
        std::time::Instant::now(),
        None,
        dir.path(),
    );
    let _ = result_tx.send(result);
    let completion = queue.pop().unwrap().content.data.unwrap();
    let status = tool_body(&crate::tools::subagent::SubagentStatusTool, &registry, "sa_5").await;
    let collect = tool_body(
        &crate::tools::subagent::collect::SubagentCollectTool,
        &registry,
        "sa_5",
    )
    .await;
    (status, completion, collect)
}

/// The live updates' statuses `updates` holds now, in order.
fn statuses(updates: &mut tokio::sync::mpsc::UnboundedReceiver<StreamEvent>) -> Vec<String> {
    let mut seen = Vec::new();
    while let Ok(event) = updates.try_recv() {
        if let StreamEvent::Agent(crate::AgentEvent::SubagentUpdate { status, .. }) = event {
            seen.push(status);
        }
    }
    seen
}

fn notices_of(body: &Value, key: &str) -> Vec<String> {
    body[key]
        .as_array()
        .unwrap_or_else(|| panic!("no {key} in {body}"))
        .iter()
        .map(|n| n.as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn v1_the_notice_ring_keeps_the_latest_notices_each_bounded_at_a_char_boundary() {
    assert_eq!((NOTICE_RING, NOTICE_BYTES), (8, 512));
    let mut v = WorkerVisibility::new(counters(1, 0, 0));
    for i in 0..10 {
        assert_eq!(v.push_notice(&format!("n{i}")), Some(format!("n{i}")));
    }
    assert_eq!(
        v.notices().to_vec(),
        (2..10).map(|i| format!("n{i}")).collect::<Vec<_>>(),
        "oldest first, the latest eight"
    );
    assert_eq!(v.dropped(), 2);
    assert_eq!(v.windows, counters(1, 0, 0));

    // Exactly the bound stays as it is; one byte more is cut.
    let fits = "a".repeat(NOTICE_BYTES);
    assert_eq!(bounded_notice(&fits), fits);
    let over = "a".repeat(NOTICE_BYTES + 1);
    let cut = bounded_notice(&over);
    assert!(cut.len() <= NOTICE_BYTES, "{}", cut.len());
    assert!(cut.ends_with('…'));
    assert!(over.starts_with(cut.trim_end_matches('…')));

    // Multibyte text is cut at a char boundary, never inside a char.
    for text in [
        "é".repeat(400),
        format!("a{}", "😀".repeat(200)),
        format!("ab{}", "€".repeat(300)),
    ] {
        let cut = bounded_notice(&text);
        assert!(cut.len() <= NOTICE_BYTES, "{}", cut.len());
        assert!(cut.len() > NOTICE_BYTES - 8, "kept as much as fits: {}", cut.len());
        assert!(text.starts_with(cut.trim_end_matches('…')));
    }
    // The ring keeps what it was given, bounded.
    let mut v = WorkerVisibility::new(counters(1, 0, 0));
    assert_eq!(v.push_notice(&over), Some(cut.clone()));
    assert_eq!(v.notices().to_vec(), vec![cut]);
}

#[tokio::test]
async fn v2_a_v2_runs_notices_reach_status_completion_and_collect() {
    let store = fx::legacy();
    let r = write_root();
    let long = format!("Rolling summary skipped: {}", "é".repeat(400));
    let mut events = vec![text("looking"), notice("Context is getting large."), notice(&long)];
    for i in 0..9 {
        events.push(notice(&format!("notice {i}")));
    }
    events.extend([text("done"), history_event(msgs(vec![user("task"), assistant("done")])), done()]);
    let (_started, start) = runtime(events, || {});
    let (mut d, state, _cancel) = drive(0, &task());
    let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
    d.tx_events = Some(tx);
    let result = run_worker_with(d, task(), Some(plan(&r, &store.binding, None)), start, &real)
        .await
        .unwrap();
    assert!(state.read().unwrap().v2_records);

    // The notices went out as live updates, bounded, at most one a second
    // ((b)): the first at once, the latest when the drive ended; the ones
    // between, all in the same instant, were coalesced (v9).
    let seen = statuses(&mut updates);
    assert_eq!(seen, ["Context is getting large.", "notice 8"], "{seen:?}");
    assert!(bounded_notice(&long).len() <= NOTICE_BYTES);

    // The ring: the latest eight of the eleven, oldest first.
    let latest: Vec<String> = (1..9).map(|i| format!("notice {i}")).collect();
    {
        let s = state.read().unwrap();
        let v = s.visibility.as_ref().expect("a v2 run shows its visibility");
        assert_eq!(v.notices().to_vec(), latest);
        assert_eq!(v.dropped(), 3);
        assert_eq!(v.windows, counters(1, 0, 0));
    }

    let (status, completion, collect) = surfaces(&state, result).await;
    for (body, notices, dropped, windows) in [
        (&status, "rolloverNotices", "rolloverNoticesDropped", "rolloverWindows"),
        (&collect, "rolloverNotices", "rolloverNoticesDropped", "rolloverWindows"),
        (&completion, "rollover_notices", "rollover_notices_dropped", "rollover_windows"),
    ] {
        assert_eq!(notices_of(body, notices), latest, "{body}");
        assert_eq!(body[dropped], 3, "{body}");
        assert_eq!(
            body[windows],
            json!({"window_ordinal": 1, "rollovers_this_run": 0, "inherited_windows": 0}),
            "{body}"
        );
    }
    assert_eq!(collect["output"], "lookingdone");
}

#[tokio::test]
async fn v3_typed_state_outside_the_ring_survives_its_eviction() {
    let store = fx::legacy();
    let r = write_root();
    let reset = "rollover thresholds reset to this worker's defaults: pressure_tokens must be \
                 below the context window (window 200000 tokens)";
    let mut context = worker_context();
    context.notice = Some(reset.into());
    let mut plan = plan(&r, &store.binding, None);
    plan.context = Ok(context);
    let mut events: Vec<StreamEvent> = (0..20).map(|i| notice(&format!("notice {i}"))).collect();
    events.extend([text("done"), done()]);
    let (_started, start) = runtime(events, || {});
    let (d, state, _cancel) = drive(0, &task());
    let result = run_worker_with(d, task(), Some(plan), start, &real).await.unwrap();
    let (status, completion, collect) = surfaces(&state, result).await;
    for body in [&status, &collect] {
        assert_eq!(body["rolloverNotice"], reset, "{body}");
        assert_eq!(notices_of(body, "rolloverNotices").len(), NOTICE_RING);
        assert_eq!(body["rolloverNoticesDropped"], 12);
        assert_eq!(body["terminal_cause"]["code"], "completed", "{body}");
    }
    assert_eq!(completion["rollover_notice"], reset);
    assert_eq!(notices_of(&completion, "rollover_notices").len(), NOTICE_RING);
    assert_eq!(completion["status"], "completed");
}

#[tokio::test]
async fn v4_window_counters_follow_durable_heads_and_the_lineage() {
    let store = fx::legacy();
    let r = write_root();
    let pred = record(
        &RunId::random(),
        fx::current(&store.binding),
        vec![],
        &[arch(1), arch(2)],
        None,
    );
    let lineage = successor_lineage(&pred, RecordKind::Final).unwrap();
    let opening = resume_messages(pred.history.clone(), "continue");
    let (d, state, _cancel) = drive(0, &opening);
    let at_start: Arc<Mutex<Option<WindowCounters>>> = Arc::default();
    let rx_slot: Arc<Mutex<Vec<tokio::sync::oneshot::Receiver<_>>>> = Arc::default();
    let (seen, slot, live) = (Arc::clone(&at_start), Arc::clone(&rx_slot), Arc::clone(&state));
    let location = r.location(RecordKind::Final);
    let start = move |_opening: Vec<SharedMessage>| {
        *seen.lock().unwrap() = live.read().unwrap().visibility.as_ref().map(|v| v.windows);
        let Candidate::Present(head) = record_file::load(&location).unwrap().head else {
            panic!("no spawn head");
        };
        // One durable rollover into window 4, then a checkpoint that is not
        // this run's (refused: it counts nothing).
        let next = msgs(vec![fx::marker_message(&arch(3), 4), assistant("window four")]);
        let (committed, rx1) = checkpoint(worker_logical_id(&head.run_id), next.clone());
        let (refused, rx2) = checkpoint("worker-run:another".into(), next.clone());
        slot.lock().unwrap().extend([rx1, rx2]);
        let events = vec![committed, notice("Continued automatically in context window 4."), refused, history_event(next), done()];
        std::future::ready(Box::pin(futures::stream::iter(events)) as WorkerStream)
    };
    let result = run_worker_with(
        d,
        opening,
        Some(plan(&r, &store.binding, Some(lineage))),
        start,
        &real,
    )
    .await
    .unwrap();
    assert_eq!(
        *at_start.lock().unwrap(),
        Some(counters(3, 0, 2)),
        "at the first inference: the window after the two inherited ones"
    );
    let mut rxs = std::mem::take(&mut *rx_slot.lock().unwrap());
    let refused = rxs.pop().unwrap();
    assert_eq!(rxs.pop().unwrap().await.unwrap(), Ok(()));
    assert!(refused.await.unwrap().is_err());

    // Distinct counters, the same in the final record and every surface.
    let fin = v2_final(&r);
    assert_eq!(
        (fin.window_ordinal, fin.rollovers_this_run, fin.inherited_windows.len() as u64),
        (4, 1, 2)
    );
    let shown = json!({"window_ordinal": 4, "rollovers_this_run": 1, "inherited_windows": 2});
    let (status, completion, collect) = surfaces(&state, result).await;
    assert_eq!(status["rolloverWindows"], shown);
    assert_eq!(collect["rolloverWindows"], shown);
    assert_eq!(completion["rollover_windows"], shown);
    assert_eq!(
        notices_of(&status, "rolloverNotices"),
        vec!["Continued automatically in context window 4.".to_owned()]
    );
}

#[tokio::test]
async fn v5_a_reaped_v2_runs_collect_shows_the_counters_of_its_final() {
    let r = write_root();
    fx::private_dir(&r.dir);
    let run = RunId::random();
    let owner = RunId::random();
    let mut fin = record(
        &run,
        fx::offline_binding(),
        fx::inherited(&owner, &[arch(1), arch(2)], 1),
        &[arch(3)],
        Some(Predecessor {
            run_id: owner,
            kind: RecordKind::Final,
            generation: 0,
            digest: HistoryDigest::of(&[user("task")]).unwrap(),
        }),
    );
    fin.status = "completed".into();
    private_file(&r.dir.join(FINAL), &serde_json::to_vec(&fin).unwrap());
    let body = collect(reaped("sa_5", r.dir.join(FINAL), Some(&run)), "sa_5", &r.root)
        .await
        .unwrap();
    assert_eq!(body["collected"], true, "{body}");
    assert_eq!(
        body["rolloverWindows"],
        json!({"window_ordinal": 4, "rollovers_this_run": 1, "inherited_windows": 2})
    );
    assert!(body.get("rolloverNotices").is_none(), "notices are not kept in records");

    // A v1 archive shows no counters.
    let r = write_root();
    fx::private_dir(&r.dir);
    let path = v1_archive(&r);
    let body = collect(reaped("sa_5", path, None), "sa_5", &r.root).await.unwrap();
    assert!(body.get("rolloverWindows").is_none(), "{body}");
}

#[tokio::test]
async fn v6_a_fallback_to_rollover_off_shows_no_counters_and_routes_no_notice() {
    let r = write_root();
    let base = r._tmp.path().canonicalize().unwrap();
    let binding = unsupported_binding(&base);
    let (_started, start) = runtime(vec![notice("Context is getting large."), text("done"), done()], || {});
    let (mut d, state, _cancel) = drive(0, &task());
    let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
    d.tx_events = Some(tx);
    let result = run_worker_with(d, task(), Some(plan(&r, &binding, None)), start, &real)
        .await
        .unwrap();
    assert!(!state.read().unwrap().v2_records);
    assert!(state.read().unwrap().visibility.is_none());
    while let Ok(event) = updates.try_recv() {
        if let StreamEvent::Agent(crate::AgentEvent::SubagentUpdate { status, .. }) = event {
            assert!(!status.contains("Context is getting large"), "{status}");
        }
    }
    let (status, completion, collect) = surfaces(&state, result).await;
    for body in [&status, &collect] {
        assert!(body.get("rolloverWindows").is_none(), "{body}");
        assert!(body.get("rolloverNotices").is_none(), "{body}");
        assert!(
            body["rolloverNotice"]
                .as_str()
                .is_some_and(|n| n.starts_with("rollover disabled for this worker: ")),
            "{body}"
        );
    }
    assert!(completion.get("rollover_windows").is_none());
    assert!(completion.get("rollover_notices").is_none());
}

#[tokio::test]
async fn v7_a_notice_is_flattened_to_one_plain_line_before_it_is_bounded() {
    // Controls and line separators become spaces, the ends are trimmed, and
    // only then is the text cut to NOTICE_BYTES: the ring, the live update,
    // the progress line and every surface carry the same plain text.
    let probe = "review\nsecond\r\x1b[31mred\x07";
    let probe_plain = "review second  [31mred";
    let separators = "\u{2028}  left\u{2029}right\t\u{85}end  \n";
    let separators_plain = "left right  end";
    let long = format!("\n\n{}\x1b{}", "a".repeat(300), "b".repeat(300));
    let long_plain = format!("{} {}…", "a".repeat(300), "b".repeat(208));
    assert_eq!(long_plain.len(), NOTICE_BYTES);
    assert_eq!(bounded_notice(probe), probe_plain);
    assert_eq!(bounded_notice(separators), separators_plain);
    assert_eq!(bounded_notice(&long), long_plain);
    assert_eq!(bounded_notice(" \r\n\t "), "");
    let plain = [probe_plain.to_owned(), separators_plain.to_owned(), long_plain.clone()];

    let store = fx::legacy();
    let r = write_root();
    let events = vec![
        text("looking"),
        notice(probe),
        notice(separators),
        notice(&long),
        text("done"),
        history_event(msgs(vec![user("task"), assistant("done")])),
        done(),
    ];
    let (_started, start) = runtime(events, || {});
    let (mut d, state, _cancel) = drive(0, &task());
    let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
    d.tx_events = Some(tx);
    let result = run_worker_with(d, task(), Some(plan(&r, &store.binding, None)), start, &real)
        .await
        .unwrap();
    assert!(state.read().unwrap().v2_records);

    let seen = statuses(&mut updates);
    // Live at most one a second ((b)): the first at once, the latest when
    // the drive ended, each the plain text; the one between was coalesced.
    assert_eq!(seen, [plain[0].clone(), plain[2].clone()], "{seen:?}");
    assert!(
        !seen.iter().any(|s| s.chars().any(char::is_control)),
        "no update carries a control: {seen:?}"
    );
    {
        let s = state.read().unwrap();
        assert_eq!(s.visibility.as_ref().unwrap().notices(), plain.to_vec());
        assert_eq!(s.step, long_plain, "the progress line is the plain text");
    }
    let (status, completion, collect) = surfaces(&state, result).await;
    for (body, key) in [
        (&status, "rolloverNotices"),
        (&collect, "rolloverNotices"),
        (&completion, "rollover_notices"),
    ] {
        assert_eq!(notices_of(body, key), plain.to_vec(), "{body}");
    }
    for text in &plain {
        assert!(
            !text.chars().any(|c| c.is_control() || c == '\u{2028}' || c == '\u{2029}'),
            "{text:?}"
        );
    }
}

#[tokio::test]
async fn v8_a_notice_blank_after_flattening_is_skipped() {
    // A notice with nothing left once flattened takes no ring slot, evicts
    // nothing, sends no update and leaves the progress line as it was.
    let mut v = WorkerVisibility::new(counters(1, 0, 0));
    for i in 0..NOTICE_RING {
        let _ = v.push_notice(&format!("n{i}"));
    }
    assert_eq!(v.push_notice(" \r\n\t\x1b\u{2028} "), None);
    let full: Vec<String> = (0..NOTICE_RING).map(|i| format!("n{i}")).collect();
    assert_eq!(v.notices(), full);
    assert_eq!(v.dropped(), 0);

    let store = fx::legacy();
    let r = write_root();
    let mut events = vec![text("looking")];
    events.extend((0..NOTICE_RING).map(|i| notice(&format!("n{i}"))));
    events.extend([
        notice("\n\n"),
        notice(" \x07 "),
        history_event(msgs(vec![user("task"), assistant("looking")])),
        done(),
    ]);
    let (_started, start) = runtime(events, || {});
    let (mut d, state, _cancel) = drive(0, &task());
    let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
    d.tx_events = Some(tx);
    let result = run_worker_with(d, task(), Some(plan(&r, &store.binding, None)), start, &real)
        .await
        .unwrap();
    let mut seen = Vec::new();
    while let Ok(event) = updates.try_recv() {
        if let StreamEvent::Agent(crate::AgentEvent::SubagentUpdate { status, .. }) = event {
            seen.push(status);
        }
    }
    assert!(!seen.iter().any(|s| s.trim().is_empty()), "no blank update: {seen:?}");
    {
        let s = state.read().unwrap();
        let v = s.visibility.as_ref().unwrap();
        assert_eq!(v.notices(), full);
        assert_eq!(v.dropped(), 0);
        assert_eq!(s.step, format!("n{}", NOTICE_RING - 1), "the progress line is kept");
    }
    let (status, completion, collect) = surfaces(&state, result).await;
    for (body, notices, dropped) in [
        (&status, "rolloverNotices", "rolloverNoticesDropped"),
        (&collect, "rolloverNotices", "rolloverNoticesDropped"),
        (&completion, "rollover_notices", "rollover_notices_dropped"),
    ] {
        assert_eq!(notices_of(body, notices), full, "{body}");
        assert!(body.get(dropped).is_none(), "{body}");
    }
}

// ── Live notice updates, paced ((b)) ──────────────────────────────────────────

/// v9 ((b)): a burst of notices goes out live at most one a second, the
/// latest winning, and nothing waits on the consumer: a thousand notices
/// at once, to a parent that drains nothing while the run lasts, send two
/// updates (the first at once, the latest when the drive ends), while the
/// ring and the progress line keep up with each. On paused time, so the
/// burst is one instant however the runner is loaded.
#[tokio::test(start_paused = true)]
async fn v9_a_burst_of_notices_sends_at_most_one_live_update_a_second() {
    let store = fx::legacy();
    let r = write_root();
    const BURST: usize = 1000;
    let mut events = vec![text("looking")];
    events.extend((0..BURST).map(|i| notice(&format!("burst {i}"))));
    events.extend([
        text("done"),
        history_event(msgs(vec![user("task"), assistant("lookingdone")])),
        done(),
    ]);
    let (_started, start) = runtime(events, || {});
    let (mut d, state, _cancel) = drive(0, &task());
    let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
    d.tx_events = Some(tx);
    let result = run_worker_with(d, task(), Some(plan(&r, &store.binding, None)), start, &real)
        .await
        .unwrap();
    assert_eq!(result.text, "lookingdone");
    let seen = statuses(&mut updates);
    assert_eq!(seen, ["burst 0".to_owned(), format!("burst {}", BURST - 1)], "{} updates", seen.len());
    let s = state.read().unwrap();
    let v = s.visibility.as_ref().expect("a v2 run");
    let latest: Vec<String> = (BURST - NOTICE_RING..BURST).map(|i| format!("burst {i}")).collect();
    assert_eq!(v.notices(), latest, "the ring has every notice as it came");
    assert_eq!(v.dropped(), (BURST - NOTICE_RING) as u64);
    assert_eq!(s.step, format!("burst {}", BURST - 1), "the progress line is the latest");
}

/// v10 ((b)): a held notice goes out once its second is up while the
/// worker runs on (the trailing flush, the drive's timer arm), not only
/// when the run ends, and only once. The worker speaks again 5 s after
/// two notices; by then the parent has both.
#[tokio::test(start_paused = true)]
async fn v10_a_held_notice_goes_out_when_its_second_is_up() {
    let store = fx::legacy();
    let r = write_root();
    let (tx, updates) = tokio::sync::mpsc::unbounded_channel();
    let updates = Arc::new(Mutex::new(updates));
    // What the parent had received when the worker spoke again.
    let before: Arc<Mutex<Option<Vec<String>>>> = Arc::default();
    let later = {
        let (updates, before) = (Arc::clone(&updates), Arc::clone(&before));
        futures::stream::once(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            *before.lock().unwrap() = Some(statuses(&mut updates.lock().unwrap()));
            text("done")
        })
    };
    let stream = futures::stream::iter(vec![text("looking"), notice("first"), notice("second")])
        .chain(later)
        .chain(futures::stream::iter(vec![
            history_event(msgs(vec![user("task"), assistant("lookingdone")])),
            done(),
        ]));
    let start = move |_opening: Vec<SharedMessage>| {
        std::future::ready(Box::pin(stream) as WorkerStream)
    };
    let (mut d, state, _cancel) = drive(0, &task());
    d.tx_events = Some(tx);
    let result = run_worker_with(d, task(), Some(plan(&r, &store.binding, None)), start, &real)
        .await
        .unwrap();
    assert_eq!(result.text, "lookingdone");
    assert_eq!(
        before.lock().unwrap().as_deref(),
        Some(["first".to_owned(), "second".to_owned()].as_slice()),
        "the held notice went out while the worker ran"
    );
    let after = statuses(&mut updates.lock().unwrap());
    assert!(!after.contains(&"second".to_owned()), "once: {after:?}");
    assert_eq!(state.read().unwrap().step, "second");
}

/// B31 (a): a resume's evicted-window count (its launch plan's) reaches
/// status, collect and the completion event, by count only, whether the run
/// goes on with rollover on or falls back to off; a run without one shows
/// no key (mutants: either branch of `run_planned_worker` drops the count; a
/// surface adds the key at 0).
#[tokio::test]
async fn v11_a_resumes_evicted_count_reaches_every_surface_on_and_off() {
    let store = fx::legacy();
    for (on, evicted) in [(true, 2), (false, 2), (true, 0), (false, 0)] {
        let r = write_root();
        let base = r._tmp.path().canonicalize().unwrap();
        let binding = if on {
            store.binding.clone()
        } else {
            unsupported_binding(&base)
        };
        let mut p = plan(&r, &binding, None);
        p.evicted_windows = evicted;
        let events = vec![
            text("done"),
            history_event(msgs(vec![user("task"), assistant("done")])),
            done(),
        ];
        let (_started, start) = runtime(events, || {});
        let (d, state, _cancel) = drive(0, &task());
        let result = run_worker_with(d, task(), Some(p), start, &real).await.unwrap();
        {
            let s = state.read().unwrap();
            assert_eq!(s.v2_records, on, "on={on}");
            assert_eq!(s.evicted_windows, evicted, "on={on}");
        }
        let (status, completion, collect) = surfaces(&state, result).await;
        for (body, key) in [
            (&status, "evictedWindows"),
            (&collect, "evictedWindows"),
            (&completion, "evicted_windows"),
        ] {
            if evicted > 0 {
                assert_eq!(body[key], evicted, "on={on}: {body}");
            } else {
                assert!(body.get(key).is_none(), "on={on}: {body}");
            }
        }
    }

    // A running worker's status and collect show it too.
    let state = Arc::new(RwLock::new(SubagentState::new()));
    state.write().unwrap().evicted_windows = 3;
    let (steer_tx, _steer_rx) = tokio::sync::mpsc::unbounded_channel();
    let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel();
    let (_result_tx, result_rx) = tokio::sync::oneshot::channel();
    let registry = Arc::new(Mutex::new(SubagentRegistry::new()));
    registry.lock().unwrap().register(SubagentHandle::new(
        "sa_6".into(),
        6,
        "worker".into(),
        "task".into(),
        BOOKED_MODEL.into(),
        "s".into(),
        60,
        Arc::clone(&state),
        Some(steer_tx),
        Some(shutdown_tx),
        Some(result_rx),
    ));
    let status = tool_body(&crate::tools::subagent::SubagentStatusTool, &registry, "sa_6").await;
    let collect = tool_body(
        &crate::tools::subagent::collect::SubagentCollectTool,
        &registry,
        "sa_6",
    )
    .await;
    assert_eq!(collect["status"], "running", "{collect}");
    for body in [&status, &collect] {
        assert_eq!(body["evictedWindows"], 3, "{body}");
    }
}
