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
