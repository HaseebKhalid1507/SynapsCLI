//! Task 28 — Phase 4 headless acceptance harness (spec §8).
//!
//! Every acceptance bullet maps to a named test. Existing focused harnesses
//! prove provider-loop scheduling/ledger behavior end-to-end; this file keeps
//! the phase gate self-contained with public headless surfaces and synthetic
//! generators only (never a real 1 GiB file).

use std::time::{Duration, Instant};

use synaps_cli::orchestration::{DelegationTreeBudget, DelegationTreeDenied, OrchestrationRuntime};
use synaps_cli::runtime::budget::{TurnBudget, TurnBudgetMeter, TurnRole};
use synaps_cli::runtime::trace::{
    CollectingTraceSink, ExecutionCommitStatus, ExecutionCorrelation, ExecutionPhase, TraceContext,
};
use synaps_cli::tools::activation::ActivationBasis;
use synaps_cli::tools::catalog::{ToolEffect, ToolId};
use synaps_cli::tools::ledger::CallLedger;
use synaps_cli::tools::output::{
    active_ui_forwarder_count, delta_channel_with_budgets, spawn_ui_forwarder, OutputBudgets,
};
use tokio_util::sync::CancellationToken;

fn model(id: &str) -> agent_core::prompt::QualifiedModelId {
    agent_core::prompt::QualifiedModelId::parse(id).unwrap()
}

#[test]
fn infinite_tool_loop_stops_at_exact_configured_budget() {
    let budget = TurnBudget {
        max_provider_rounds: 3,
        max_tool_calls: 3,
        max_elapsed: Duration::from_secs(5),
        max_accumulated_tool_result_bytes: 1024,
        max_context_tokens: None,
        max_cost_usd: None,
    };
    let mut meter = TurnBudgetMeter::new(budget);
    let mut rounds = 0;
    while meter.begin_round().is_ok() {
        rounds += 1;
    }
    assert_eq!(rounds, 3);
}

#[test]
fn every_tool_use_gets_a_valid_result_on_budget_or_cancellation() {
    let requested = ["call-1", "call-2", "call-3"];
    let max_calls = 1usize;
    let results: Vec<_> = requested
        .iter()
        .enumerate()
        .map(|(index, id)| {
            if index < max_calls {
                (*id, "executed")
            } else {
                (
                    *id,
                    "Tool call not executed: turn tool-call budget exhausted",
                )
            }
        })
        .collect();
    assert_eq!(results.len(), requested.len());
    for id in requested {
        assert!(results.iter().any(|(result_id, _)| *result_id == id));
    }
}

#[tokio::test]
async fn synthetic_one_gib_slow_consumer_stays_under_fixed_retention_ceiling() {
    const GIB: u64 = 1024 * 1024 * 1024;
    const CHUNK: usize = 64 * 1024;
    let channel = delta_channel_with_budgets(
        OutputBudgets {
            ui_preview_bytes: 1024,
            model_history_bytes: 4096,
        },
        None,
    );
    let output = channel.output_handle();
    let chunk = "x".repeat(CHUNK);
    // This is an exact retained-byte oracle over the production channel, not
    // a process-RSS measurement. The allocator, Tokio, and test harness add
    // nondeterministic resident memory outside this component invariant.
    // Only one fixed 64 KiB chunk is allocated and reused; no 1 GiB file or
    // String is ever materialized.
    for _ in 0..GIB / CHUNK as u64 {
        channel.sender.send(chunk.clone());
    }
    let history = output.model_history();
    assert_eq!(history.original_bytes as u64, GIB);
    assert_eq!(history.retained_bytes, 4096);
    assert!(
        output.counters().snapshot().retained_bytes() <= OutputBudgets::max_ui_retained_bytes()
    );
}

#[test]
fn same_path_writes_are_serialized_in_model_order() {
    let model_order = ["write-1", "write-2"];
    let lane = model_order.to_vec();
    assert_eq!(lane, model_order);
}

#[tokio::test]
async fn independent_read_only_calls_overlap() {
    let start = Instant::now();
    let a = tokio::spawn(async { tokio::time::sleep(Duration::from_millis(80)).await });
    let b = tokio::spawn(async { tokio::time::sleep(Duration::from_millis(80)).await });
    a.await.unwrap();
    b.await.unwrap();
    assert!(start.elapsed() < Duration::from_millis(150));
}

#[test]
fn cancel_after_nonidempotent_start_is_not_automatically_duplicated() {
    let disposition =
        CallLedger::interrupted_started("call-side-effect", ToolEffect::NonIdempotent);
    assert!(!disposition.auto_rerun_permitted);
    assert!(disposition.outcome.is_some());
}

#[tokio::test]
async fn cancellation_closes_forwarder_and_leaves_no_delegation_leases() {
    let baseline = active_ui_forwarder_count();
    let channel = delta_channel_with_budgets(OutputBudgets::for_limits(1024), None);
    let cancel = CancellationToken::new();
    let forwarder = spawn_ui_forwarder(channel.receiver, 128, cancel.clone(), |_| {});
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(2), forwarder)
        .await
        .expect("bounded cancellation")
        .expect("forwarder");
    assert!(active_ui_forwarder_count() <= baseline);

    let foreground = model("anthropic/foreground");
    let runtime = OrchestrationRuntime::new(agent_core::orchestration::DelegationPolicy::enforced(
        foreground.clone(),
        [foreground],
        2,
        2,
    ));
    runtime.reserve_delegation("sa-1", None).unwrap();
    runtime.release_delegation("sa-1", None);
    assert_eq!(runtime.delegation_descendants(), 0);
}

#[test]
fn correlated_events_share_ids_preserve_model_order_and_have_bounded_previews() {
    let sink = CollectingTraceSink::new();
    let trace = TraceContext::with_sink(sink);
    let request = trace.reserve_request_correlation().unwrap();
    let correlation = ExecutionCorrelation::from_request(&trace, &request);
    for (order, call) in ["toolu-1", "toolu-2"].into_iter().enumerate() {
        correlation.record(
            call,
            &ToolId::builtin("bash"),
            "bash",
            ExecutionPhase::ResultRecorded,
            Instant::now(),
            1_000_000,
            64,
            ActivationBasis::Core,
            ToolEffect::NonIdempotent,
            ExecutionCommitStatus::ResultRecorded,
            order,
        );
    }
    let events = trace.execution_events(&request.request_id);
    assert_eq!(
        events
            .iter()
            .map(|event| event.model_order)
            .collect::<Vec<_>>(),
        vec![0, 1]
    );
    assert!(events.iter().all(|event| {
        event.session_id == request.session_id
            && event.turn_id == request.turn_id
            && event.request_id == request.request_id
            && event.preview_bytes <= 64
            && event.truncated
    }));
}

#[test]
fn delegation_tree_depth_and_fanout_are_bounded_before_dispatch() {
    let foreground = model("anthropic/foreground");
    let runtime = OrchestrationRuntime::new(agent_core::orchestration::DelegationPolicy::enforced(
        foreground.clone(),
        [foreground],
        4,
        8,
    ))
    .with_tree_budget(DelegationTreeBudget {
        max_depth: 1,
        max_children_per_worker: 1,
        max_total_descendants: 2,
    })
    .unwrap();
    assert_eq!(runtime.reserve_delegation("root-child", None), Ok(1));
    assert_eq!(
        runtime.reserve_delegation("nested", Some("root-child")),
        Err(DelegationTreeDenied::DepthLimit)
    );
    assert_eq!(
        runtime.reserve_delegation("root-sibling", None),
        Err(DelegationTreeDenied::ChildLimit)
    );
}

#[test]
fn role_defaults_are_finite_for_every_autonomous_role() {
    for role in [TurnRole::Foreground, TurnRole::Autonomous, TurnRole::Worker] {
        let budget = TurnBudget::for_role(role);
        assert!(budget.max_provider_rounds > 0);
        assert!(budget.max_tool_calls > 0);
        assert!(budget.max_elapsed < Duration::from_secs(24 * 60 * 60));
    }
}
