//! Task 27 — bounded, correlated tool-execution lifecycle metadata.
//!
//! This module bridges the execution gate/ledger and the T7 request trace
//! envelope. It contains no result body or tool input: previews are represented
//! only by byte counts. All IDs and wire names are validated bounded trace
//! types, so hostile model-provided identifiers fail closed to omission.

use std::time::Instant;

use super::{
    ActivationGrantRef, ExecutionCommitStatus, ExecutionEffect, ExecutionPhase, ToolExecutionEvent,
    TraceContext, TraceId, WireName,
};
use crate::tools::activation::ActivationBasis;
use crate::tools::catalog::{ToolEffect, ToolId};

#[derive(Clone)]
pub struct ExecutionCorrelation {
    trace: TraceContext,
    session_id: TraceId,
    turn_id: TraceId,
    request_id: TraceId,
}

/// How a call's history entry differs from its raw result, for
/// [`ExecutionCorrelation::record_virtualised`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HistoryShaping {
    /// The result was virtualised: history carries its preview (or, when
    /// also deduped, the stub that replaced an identical preview).
    pub virtualised: bool,
    /// History carries a read-dedupe stub; the value is the bytes it kept
    /// out of history.
    pub deduped_bytes: Option<usize>,
}

impl ExecutionCorrelation {
    pub fn from_request(trace: &TraceContext, request: &super::RequestCorrelation) -> Self {
        Self {
            trace: trace.clone(),
            session_id: request.session_id.clone(),
            turn_id: request.turn_id.clone(),
            request_id: request.request_id.clone(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record(
        &self,
        tool_call_id: &str,
        stable_tool_id: &ToolId,
        wire_name: &str,
        phase: ExecutionPhase,
        started: Instant,
        result_bytes: usize,
        retained_bytes: usize,
        activation: ActivationBasis,
        effect: ToolEffect,
        commit_status: ExecutionCommitStatus,
        model_order: usize,
    ) {
        self.record_virtualised(
            tool_call_id,
            stable_tool_id,
            wire_name,
            phase,
            started,
            result_bytes,
            retained_bytes,
            activation,
            effect,
            commit_status,
            model_order,
            HistoryShaping::default(),
        );
    }

    /// [`Self::record`] for a call whose history entry may be shaped: a tool
    /// output virtualisation preview (then `result_bytes` is the whole
    /// output and `retained_bytes` the preview, and the event says
    /// `virtualised`) or a read-dedupe stub (`retained_bytes` is the stub;
    /// the event says `deduped` with the bytes kept out of history).
    #[allow(clippy::too_many_arguments)]
    pub fn record_virtualised(
        &self,
        tool_call_id: &str,
        stable_tool_id: &ToolId,
        wire_name: &str,
        phase: ExecutionPhase,
        started: Instant,
        result_bytes: usize,
        retained_bytes: usize,
        activation: ActivationBasis,
        effect: ToolEffect,
        commit_status: ExecutionCommitStatus,
        model_order: usize,
        shaping: HistoryShaping,
    ) {
        let Ok(tool_call_id) = TraceId::new(tool_call_id) else {
            return;
        };
        let Ok(stable_tool_id) = TraceId::new(stable_tool_id.as_str()) else {
            return;
        };
        let Ok(wire_name) = WireName::new(wire_name) else {
            return;
        };
        let activation = match activation {
            ActivationBasis::Core => ActivationGrantRef::Core,
            ActivationBasis::Exact { catalog_generation } => ActivationGrantRef::Exact {
                catalog_generation: catalog_generation.value(),
            },
        };
        let effect = match effect {
            ToolEffect::ReadOnly => ExecutionEffect::ReadOnly,
            ToolEffect::IdempotentWrite => ExecutionEffect::IdempotentWrite,
            ToolEffect::NonIdempotent => ExecutionEffect::NonIdempotent,
        };
        let event = ToolExecutionEvent {
            session_id: self.session_id.clone(),
            turn_id: self.turn_id.clone(),
            request_id: self.request_id.clone(),
            tool_call_id,
            stable_tool_id,
            wire_name,
            phase,
            elapsed_ms: started.elapsed().as_millis() as u64,
            result_bytes: result_bytes as u64,
            truncated: retained_bytes < result_bytes,
            preview_bytes: retained_bytes as u64,
            activation,
            effect,
            commit_status,
            model_order: model_order as u32,
            virtualised: shaping.virtualised,
            deduped: shaping.deduped_bytes.is_some(),
            deduped_bytes: shaping.deduped_bytes.unwrap_or(0) as u64,
        };
        self.trace.record_execution_event(event);
        self.trace.emit_execution_enriched(&self.request_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::trace::{
        AttemptClock, CollectingTraceSink, EndpointMeta, RequestStructure, RequestTracer,
        TransportKind,
    };
    use crate::TurnOutcome;

    #[test]
    fn records_only_bounded_metadata_with_consistent_ids() {
        let sink = CollectingTraceSink::new();
        let trace = TraceContext::with_sink(sink);
        let request = trace.reserve_request_correlation().unwrap();
        let correlation = ExecutionCorrelation::from_request(&trace, &request);
        correlation.record(
            "toolu_1",
            &ToolId::builtin("bash"),
            "bash",
            ExecutionPhase::ResultRecorded,
            Instant::now(),
            9000,
            32,
            ActivationBasis::Core,
            ToolEffect::NonIdempotent,
            ExecutionCommitStatus::ResultRecorded,
            0,
        );
        let events = trace.execution_events(&request.request_id);
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.session_id, request.session_id);
        assert_eq!(event.turn_id, request.turn_id);
        assert_eq!(event.request_id, request.request_id);
        assert_eq!(event.preview_bytes, 32);
        assert_eq!(event.result_bytes, 9000);
        assert!(event.truncated);
        let serialized = serde_json::to_string(event).unwrap();
        assert!(!serialized.contains("raw result"));
    }

    /// A virtualised result: `result_bytes` is the whole output,
    /// `preview_bytes` what history carries; the difference is what was
    /// kept out of history. Legacy events omit the flag on the wire.
    #[test]
    fn virtualised_results_are_flagged_and_sized() {
        let sink = CollectingTraceSink::new();
        let trace = TraceContext::with_sink(sink);
        let request = trace.reserve_request_correlation().unwrap();
        let correlation = ExecutionCorrelation::from_request(&trace, &request);
        for (id, virtualised) in [("toolu_v", true), ("toolu_l", false)] {
            let shaping = HistoryShaping {
                virtualised,
                deduped_bytes: None,
            };
            correlation.record_virtualised(
                id,
                &ToolId::builtin("bash"),
                "bash",
                ExecutionPhase::ResultRecorded,
                Instant::now(),
                48_213,
                7_400,
                ActivationBasis::Core,
                ToolEffect::NonIdempotent,
                ExecutionCommitStatus::ResultRecorded,
                0,
                shaping,
            );
        }
        let events = trace.execution_events(&request.request_id);
        assert!(events[0].virtualised && !events[1].virtualised);
        assert_eq!(events[0].result_bytes - events[0].preview_bytes, 48_213 - 7_400);
        let wire = serde_json::to_value(&events[0]).unwrap();
        assert_eq!(wire["virtualised"], true);
        let legacy = serde_json::to_value(&events[1]).unwrap();
        assert!(legacy.get("virtualised").is_none(), "absent unless set: {legacy}");
        let back: ToolExecutionEvent = serde_json::from_value(legacy).unwrap();
        assert!(!back.virtualised, "old records still read");
    }

    /// A read-dedupe stub: flagged with the bytes it kept out of history;
    /// events without it serialize exactly as before.
    #[test]
    fn deduped_results_are_flagged_with_the_bytes_kept_out() {
        let sink = CollectingTraceSink::new();
        let trace = TraceContext::with_sink(sink);
        let request = trace.reserve_request_correlation().unwrap();
        let correlation = ExecutionCorrelation::from_request(&trace, &request);
        for (id, deduped_bytes) in [("toolu_d", Some(7_900)), ("toolu_l", None)] {
            correlation.record_virtualised(
                id,
                &ToolId::builtin("read"),
                "read",
                ExecutionPhase::ResultRecorded,
                Instant::now(),
                8_040,
                140,
                ActivationBasis::Core,
                ToolEffect::ReadOnly,
                ExecutionCommitStatus::ResultRecorded,
                0,
                HistoryShaping { virtualised: false, deduped_bytes },
            );
        }
        let events = trace.execution_events(&request.request_id);
        assert!(events[0].deduped && events[0].deduped_bytes == 7_900);
        let wire = serde_json::to_value(&events[0]).unwrap();
        assert_eq!((wire["deduped"].clone(), wire["deduped_bytes"].clone()), (true.into(), 7_900.into()));
        let legacy = serde_json::to_value(&events[1]).unwrap();
        assert!(legacy.get("deduped").is_none() && legacy.get("deduped_bytes").is_none(), "{legacy}");
        let back: ToolExecutionEvent = serde_json::from_value(legacy).unwrap();
        assert!(!back.deduped && back.deduped_bytes == 0, "old records still read");
    }

    #[test]
    fn execution_event_is_fed_into_same_request_trace_envelope() {
        let sink = CollectingTraceSink::new();
        let ctx = TraceContext::with_sink(sink.clone());
        let request = ctx.reserve_request_correlation().unwrap();
        let tracer = RequestTracer::begin(
            &ctx,
            Some(request.clone()),
            agent_core::prompt::QualifiedModelId::parse("anthropic/claude-test").unwrap(),
            TransportKind::AnthropicMessages,
            EndpointMeta::new("api.anthropic.com", "/v1/messages").unwrap(),
            RequestStructure::default(),
        )
        .unwrap();
        tracer.finish(
            AttemptClock::start(),
            Some(200),
            None,
            None,
            None,
            TurnOutcome::Completed,
        );
        let correlation = ExecutionCorrelation::from_request(&ctx, &request);
        correlation.record(
            "toolu_1",
            &crate::tools::catalog::ToolId::builtin("bash"),
            "bash",
            ExecutionPhase::ResultRecorded,
            std::time::Instant::now(),
            42,
            8,
            crate::tools::activation::ActivationBasis::Core,
            crate::tools::catalog::ToolEffect::NonIdempotent,
            ExecutionCommitStatus::ResultRecorded,
            0,
        );
        let records = sink.records();
        let enriched = records.last().expect("enriched trace");
        assert_eq!(enriched.request_id, request.request_id);
        assert_eq!(enriched.execution_events.len(), 1);
        assert_eq!(enriched.execution_events[0].turn_id, enriched.turn_id);
        assert_eq!(enriched.execution_events[0].session_id, enriched.session_id);
    }

    #[test]
    fn metadata_serialization_never_contains_result_secret_sentinel() {
        const SECRET: &str = "PH4-EXECUTION-SECRET-SENTINEL-c14d";
        let sink = CollectingTraceSink::new();
        let trace = TraceContext::with_sink(sink);
        let request = trace.reserve_request_correlation().unwrap();
        let correlation = ExecutionCorrelation::from_request(&trace, &request);
        correlation.record(
            "toolu_1",
            &ToolId::builtin("bash"),
            "bash",
            ExecutionPhase::ResultRecorded,
            Instant::now(),
            SECRET.len(),
            8,
            ActivationBasis::Core,
            ToolEffect::NonIdempotent,
            ExecutionCommitStatus::ResultRecorded,
            0,
        );
        let encoded = serde_json::to_string(&trace.execution_events(&request.request_id)).unwrap();
        assert!(!encoded.contains(SECRET));
    }

    #[test]
    fn hostile_unbounded_call_id_is_omitted() {
        let sink = CollectingTraceSink::new();
        let trace = TraceContext::with_sink(sink);
        let request = trace.reserve_request_correlation().unwrap();
        let correlation = ExecutionCorrelation::from_request(&trace, &request);
        correlation.record(
            &"x".repeat(super::super::TRACE_ID_MAX_BYTES + 1),
            &ToolId::builtin("bash"),
            "bash",
            ExecutionPhase::Planned,
            Instant::now(),
            0,
            0,
            ActivationBasis::Core,
            ToolEffect::NonIdempotent,
            ExecutionCommitStatus::NotStarted,
            0,
        );
        assert!(trace.execution_events(&request.request_id).is_empty());
    }
}
