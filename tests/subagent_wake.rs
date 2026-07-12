//! Integration tests: reactive subagent completion-wake (Phase 1, §5 I1-I6 + L1).
//!
//! These tests exercise the finalizer + registry + EventQueue end-to-end
//! **without a live model**. Each test spawns real OS threads that write
//! terminal state and push a `subagent_completion` event, exercising the REAL
//! `finalize_subagent` from finalize.rs (no mock replica).
//!
//! KEY TEST: I1 `parent_wakes_after_turn_end_multi_subagent` — encodes the
//! live failure where `cleanup_finished()` at turn-end reaped handles before
//! events arrived, causing "No subagent found" on the next collect.

use std::collections::HashMap;
use std::sync::{Arc, Barrier, Mutex, RwLock};
use std::time::{Duration, Instant};

use agent_engine::events::EventQueue;
use agent_engine::runtime::subagent::{
    SubagentHandle, SubagentRegistry, SubagentState, SubagentStatus,
};
use agent_engine::tools::finalize_subagent;

// ── helpers ───────────────────────────────────────────────────────────────────

/// Build a minimal SubagentHandle backed by a shared state Arc.
fn make_handle(id: &str, state: Arc<RwLock<SubagentState>>) -> SubagentHandle {
    SubagentHandle::new(
        id.to_string(),
        "mock-agent".to_string(),
        "mock task".to_string(),
        "claude-test".to_string(),
        "".to_string(),
        300,
        state,
        None, // no steer_tx
        None, // no shutdown_tx
        None, // no result_rx
    )
}

// ── I1: parent_wakes_after_turn_end_multi_subagent ───────────────────────────
//
// THE KEY TEST — encodes the live failure.
//
// Three mock subagents complete at 10/50/100 ms. The parent simulates
// end-of-turn cleanup (cleanup_finished) while threads may still be running.
// After all threads finish:
//   - exactly 3 subagent_completion events must be in the queue
//   - every data.handle_id must still resolve in the registry
//   - collect-equivalent (get + state read + mark_collected) must succeed
//   - post-collect cleanup_finished reaps all 3 (zero "No subagent found")
#[test]
fn parent_wakes_after_turn_end_multi_subagent() {
    let queue = Arc::new(EventQueue::new(1000));
    let registry = Arc::new(Mutex::new(SubagentRegistry::new()));

    let handles_data: Vec<(&str, u64, u64)> = vec![
        ("sa_101", 101, 10),
        ("sa_102", 102, 50),
        ("sa_103", 103, 100),
    ];

    let mut join_handles = vec![];

    for (handle_id, subagent_id, delay_ms) in handles_data.iter().copied() {
        let state = Arc::new(RwLock::new(SubagentState::new()));
        let handle = make_handle(handle_id, Arc::clone(&state));
        registry.lock().unwrap().register(handle);

        let queue_c = Arc::clone(&queue);
        let state_c = Arc::clone(&state);
        let id = handle_id.to_string();
        let jh = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(delay_ms));
            state_c.write().unwrap().status = SubagentStatus::Completed;
            state_c.write().unwrap().partial_text = format!("result from {id}");
            let started = Instant::now() - Duration::from_millis(delay_ms);
            finalize_subagent(&state_c, Some(&queue_c), &id, subagent_id, "mock-agent", started, None);
        });

        // Register the thread handle so cleanup_finished can join it
        {
            let mut reg = registry.lock().unwrap();
            if let Some(h) = reg.get_mut(handle_id) {
                h.set_thread_handle(jh);
            }
        }
        // We track the thread separately via registry join — no separate vec needed
        join_handles.push(handle_id);
    }

    // Simulate end-of-parent-turn: cleanup_finished fires while threads may still run.
    // With the new TTL-aware reaper, uncollected finished handles are RETAINED.
    {
        let mut reg = registry.lock().unwrap();
        reg.cleanup_finished(); // fires early — some threads still running
    }

    // Wait for all completion events (up to 1s).
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        if queue.len() >= 3 { break; }
        assert!(Instant::now() < deadline, "timed out waiting for 3 completion events");
        std::thread::sleep(Duration::from_millis(5));
    }

    // Drain and validate events
    let events = queue.drain();
    let completion_events: Vec<_> = events
        .iter()
        .filter(|e| e.content.content_type == "subagent_completion")
        .collect();

    assert_eq!(
        completion_events.len(), 3,
        "expected exactly 3 completion events, got {}",
        completion_events.len()
    );

    // Every event's handle_id must resolve in the registry and be collectable.
    let mut reg = registry.lock().unwrap();
    for ev in &completion_events {
        let data = ev.content.data.as_ref().unwrap();
        let hid = data["handle_id"].as_str().unwrap();
        let handle = reg.get_mut(hid).unwrap_or_else(|| {
            panic!("No subagent found with handle_id '{}' — live-failure regression!", hid)
        });
        assert!(handle.is_finished(), "handle {hid} must be finished");
        handle.mark_collected();
        assert!(handle.is_collected(), "handle {hid} must be collected after mark");
    }

    // Post-collect cleanup must reap all 3 (all collected → immediate removal).
    reg.cleanup_finished_with_ttl(Duration::from_secs(900));
    assert_eq!(
        reg.list_active().len(), 0,
        "all 3 collected handles should be reaped after cleanup"
    );
}

// ── I2: panic_publishes_failed_completion ────────────────────────────────────

#[test]
fn panic_publishes_failed_completion() {
    let queue = Arc::new(EventQueue::new(100));
    let state = Arc::new(RwLock::new(SubagentState::new()));
    let queue_c = Arc::clone(&queue);
    let state_c = Arc::clone(&state);

    let jh = std::thread::spawn(move || {
        let state_for_finalizer = Arc::clone(&state_c);

        let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            panic!("simulated subagent panic");
        }));

        if let Err(panic_info) = panic_result {
            let msg = if let Some(s) = panic_info.downcast_ref::<&str>() {
                s.to_string()
            } else if let Some(s) = panic_info.downcast_ref::<String>() {
                s.clone()
            } else {
                "unknown panic".to_string()
            };
            state_c.write().unwrap().status =
                SubagentStatus::Failed(format!("panic: {}", msg));
        }

        finalize_subagent(
            &state_for_finalizer,
            Some(&queue_c),
            "sa_panic",
            99,
            "panic-agent",
            Instant::now(),
            None,
        );
    });
    jh.join().unwrap();

    let ev = queue.pop().expect("expected one completion event");
    assert_eq!(ev.content.content_type, "subagent_completion");
    let data = ev.content.data.as_ref().unwrap();
    assert_eq!(data["status"], "failed", "panic must produce failed status");
    assert!(
        ev.content.text.contains("panic"),
        "event text must mention panic: {}",
        ev.content.text
    );
}

// ── I3: timeout_publishes_timed_out_completion ───────────────────────────────

#[test]
fn timeout_publishes_timed_out_completion() {
    let queue = Arc::new(EventQueue::new(100));
    let state = Arc::new(RwLock::new(SubagentState::new()));
    let queue_c = Arc::clone(&queue);
    let state_c = Arc::clone(&state);

    let jh = std::thread::spawn(move || {
        // Simulate timeout: thread sets TimedOut then exits
        state_c.write().unwrap().status = SubagentStatus::TimedOut;
        finalize_subagent(&state_c, Some(&queue_c), "sa_timeout", 50, "timeout-agent", Instant::now(), None);
    });
    jh.join().unwrap();

    let ev = queue.pop().expect("expected one completion event");
    let data = ev.content.data.as_ref().unwrap();
    assert_eq!(data["status"], "timed_out");
}

// ── I4: exactly_once_per_subagent ────────────────────────────────────────────

#[test]
fn exactly_once_per_subagent() {
    let queue = Arc::new(EventQueue::new(2000));
    let mut join_handles = vec![];

    for i in 0u64..20 {
        let queue_c = Arc::clone(&queue);
        let handle_id = format!("sa_once_{i}");
        let jh = std::thread::spawn(move || {
            let state = Arc::new(RwLock::new(SubagentState::new()));
            state.write().unwrap().status = SubagentStatus::Completed;
            finalize_subagent(&state, Some(&queue_c), &handle_id, i, "once-agent", Instant::now(), None);
        });
        join_handles.push(jh);
    }

    for jh in join_handles {
        jh.join().unwrap();
    }

    // Drain and count events per handle_id
    let events = queue.drain();
    let mut counts: HashMap<String, usize> = HashMap::new();
    for ev in &events {
        if ev.content.content_type == "subagent_completion" {
            let hid = ev.content.data.as_ref().unwrap()["handle_id"]
                .as_str()
                .unwrap()
                .to_string();
            *counts.entry(hid).or_insert(0) += 1;
        }
    }

    assert_eq!(counts.len(), 20, "expected 20 distinct handle_ids");
    for (hid, count) in &counts {
        assert_eq!(*count, 1, "handle {hid} published {count} events, expected exactly 1");
    }
}

// ── I5: reaper_race_finish_during_done_handling ───────────────────────────────
//
// Thread finishes *between* cleanup_finished() and queue drain.
// Orchestrated with a Barrier: main thread calls cleanup_finished, then
// signals thread to complete, then drains queue.
// Handle must be retained and event delivered.

#[test]
fn reaper_race_finish_during_done_handling() {
    let queue = Arc::new(EventQueue::new(100));
    let registry = Arc::new(Mutex::new(SubagentRegistry::new()));

    // Barrier: main waits for thread to be "about to finalize", thread waits
    // for main to call cleanup_finished first.
    let barrier = Arc::new(Barrier::new(2));

    let state = Arc::new(RwLock::new(SubagentState::new()));
    let handle = make_handle("sa_race", Arc::clone(&state));
    registry.lock().unwrap().register(handle);

    let queue_c = Arc::clone(&queue);
    let state_c = Arc::clone(&state);
    let barrier_c = Arc::clone(&barrier);

    let jh = std::thread::spawn(move || {
        // Wait for main thread to run cleanup_finished
        barrier_c.wait();
        // Now complete (races with cleanup_finished in main)
        state_c.write().unwrap().status = SubagentStatus::Completed;
        state_c.write().unwrap().partial_text = "race result".to_string();
        finalize_subagent(&state_c, Some(&queue_c), "sa_race", 200, "race-agent", Instant::now(), None);
    });

    // Main: run cleanup_finished first (thread still Running at this point)
    registry.lock().unwrap().cleanup_finished();

    // Signal thread to proceed
    barrier.wait();

    // Wait for thread to finish
    jh.join().unwrap();

    // Event must have been delivered
    let ev = queue.pop().expect("completion event must be delivered even with reaper race");
    let data = ev.content.data.as_ref().unwrap();
    assert_eq!(data["handle_id"], "sa_race");
    assert_eq!(data["status"], "completed");

    // Handle must still be in registry (was Running when cleanup fired → retained)
    // or was just inserted. Either way, collect-equivalent must succeed.
    let mut reg = registry.lock().unwrap();
    // The handle may have been reaped if it finished before cleanup — but the
    // TTL-aware reaper only removes uncollected handles after TTL. Since it was
    // Running at cleanup time, it was NOT removed. Verify:
    let handle = reg.get_mut("sa_race")
        .expect("sa_race handle must be retained after reaper race");
    handle.mark_collected();

    // Now cleanup removes it
    reg.cleanup_finished_with_ttl(Duration::from_secs(900));
    assert!(reg.get("sa_race").is_none(), "collected handle must be reaped");
}

// ── I6: steer_then_complete_still_publishes ───────────────────────────────────
//
// Thread consumes a steer message mid-run then completes.
// Completion event must still be published exactly once.

#[test]
fn steer_then_complete_still_publishes() {
    use tokio::sync::mpsc;

    let queue = Arc::new(EventQueue::new(100));
    let state = Arc::new(RwLock::new(SubagentState::new()));
    let queue_c = Arc::clone(&queue);
    let state_c = Arc::clone(&state);

    let (steer_tx, mut steer_rx) = mpsc::unbounded_channel::<String>();

    let jh = std::thread::spawn(move || {
        // Simulate consuming a steer message mid-run
        // (use try_recv since we're in a sync thread)
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            // Send a steer message and consume it
            let _ = steer_rx.recv().await;
        });

        // Then complete normally
        state_c.write().unwrap().status = SubagentStatus::Completed;
        state_c.write().unwrap().partial_text = "steered then completed".to_string();
        finalize_subagent(&state_c, Some(&queue_c), "sa_steer", 300, "steer-agent", Instant::now(), None);
    });

    // Send a steer message
    steer_tx.send("adjust your approach".to_string()).unwrap();

    jh.join().unwrap();

    // Exactly one completion event
    assert_eq!(queue.len(), 1, "must be exactly one completion event after steer+complete");
    let ev = queue.pop().unwrap();
    let data = ev.content.data.as_ref().unwrap();
    assert_eq!(data["status"], "completed");
    assert_eq!(data["handle_id"], "sa_steer");
    // No second event
    assert!(queue.is_empty(), "no duplicate completion events");
}

// ── L1: live_reactive_subagent_end_to_end ────────────────────────────────────
//
// Requires live credentials + network. Run with:
//   cargo test --test subagent_wake -- --ignored live_reactive_subagent_end_to_end

#[test]
#[ignore = "requires live API credentials and network access"]
fn live_reactive_subagent_end_to_end() {
    use agent_engine::tools::{SubagentCollectTool, SubagentStartTool, SubagentRegistry, Tool};
    use agent_engine::tools::{ToolCapabilities, ToolChannels, ToolContext, ToolLimits};
    use std::sync::{Arc, Mutex};
    use serde_json::json;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    rt.block_on(async move {
        let queue = Arc::new(EventQueue::new(1000));
        let registry = Arc::new(Mutex::new(SubagentRegistry::new()));

        let ctx = ToolContext {
            channels: ToolChannels {
                tx_delta: None,
                tx_events: None,
            },
            capabilities: ToolCapabilities {
                watcher_exit_path: None,
                tool_register_tx: None,
                session_manager: None,
                subagent_registry: Some(Arc::clone(&registry)),
                event_queue: Some(Arc::clone(&queue)),
                secret_prompt: None,
            },
            limits: ToolLimits {
                max_tool_output: 30000,
                max_tool_buffer: 256 * 1024,
                bash_timeout: 30,
                bash_max_timeout: 300,
                subagent_timeout: 120,
            },
        };

        let result = SubagentStartTool.execute(json!({
            "system_prompt": "You are a test subagent. Reply with exactly: done",
            "task": "Say done",
            "timeout": 120
        }), ctx).await.expect("subagent_start must succeed with live credentials");

        let body: serde_json::Value = serde_json::from_str(&result).unwrap();
        let handle_id = body["handle_id"].as_str().unwrap().to_string();
        assert!(handle_id.starts_with("sa_"), "handle_id must be sa_N: {handle_id}");

        // Simulate end-of-parent-turn cleanup — must retain uncollected handle.
        {
            let mut reg = registry.lock().unwrap();
            reg.cleanup_finished();
        }

        // Wait for the completion event (up to 120s).
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            if !queue.is_empty() { break; }
            assert!(Instant::now() < deadline, "timed out waiting for completion event");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        // Pop and validate the event.
        let ev = queue.pop().expect("completion event must be present");
        assert_eq!(ev.content.content_type, "subagent_completion");
        let data = ev.content.data.as_ref().unwrap();
        assert_eq!(data["handle_id"].as_str().unwrap(), handle_id,
            "event handle_id must match the spawned subagent");

        // subagent_collect must succeed.
        let collect_ctx = ToolContext {
            channels: ToolChannels { tx_delta: None, tx_events: None },
            capabilities: ToolCapabilities {
                watcher_exit_path: None,
                tool_register_tx: None,
                session_manager: None,
                subagent_registry: Some(Arc::clone(&registry)),
                event_queue: Some(Arc::clone(&queue)),
                secret_prompt: None,
            },
            limits: ToolLimits {
                max_tool_output: 30000,
                max_tool_buffer: 256 * 1024,
                bash_timeout: 30,
                bash_max_timeout: 300,
                subagent_timeout: 120,
            },
        };

        let collect_result = SubagentCollectTool.execute(json!({ "handle_id": handle_id }), collect_ctx).await;
        assert!(collect_result.is_ok(), "subagent_collect must succeed: {collect_result:?}");
    });
}
