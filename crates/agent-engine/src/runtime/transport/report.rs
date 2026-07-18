//! `TranslationReport` (Task 9, spec §6.3): the typed record of everything a
//! provider adapter did to a normalized request that was not a faithful 1:1
//! mapping — dropped, merged, renamed, synthesized, downgraded, or
//! unsupported elements. Silent semantic loss is not acceptable: an adapter
//! must either represent an element on the wire or report it here.
//!
//! Entries reuse the trace vocabulary ([`TranslationAction`],
//! [`TranslationElement`], [`TranslationLoss`], bounded [`TraceId`]) so the
//! report populates `RequestTrace.translation_losses` directly, without a
//! schema change (`synaps-request-trace/1` already carries the field).
//! Entries identify elements by **structural position only** — positional
//! paths into the normalized IR such as `messages[3].blocks[1]`, or, for
//! `Synthesized` elements (which have no pre-translation position), a
//! symbolic ID in the `system.synthetic[N]` namespace. Entries never carry
//! content or previews.

use crate::runtime::trace::{TraceId, TranslationAction, TranslationElement, TranslationLoss};

/// The complete, deterministic list of translation losses/rewrites one
/// provider adapter performed for one request. Entry order is structural
/// (system segments first, then messages in order) and stable across runs.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TranslationReport {
    pub entries: Vec<TranslationLoss>,
}

impl TranslationReport {
    /// A report with no entries: the translation was fully faithful.
    pub fn lossless() -> Self {
        Self::default()
    }

    /// True when the translation was fully faithful (no entries).
    /// (Used by tests today; Task 10 provider adapters gate on it.)
    #[allow(dead_code)]
    pub fn is_lossless(&self) -> bool {
        self.entries.is_empty()
    }

    /// Append one entry. `element_id` must already be a bounded safe
    /// [`TraceId`] — construction sites use [`block_path`] / [`message_path`]
    /// / [`synthetic_system_id`], which produce only positional/symbolic IDs.
    pub fn push(
        &mut self,
        action: TranslationAction,
        element: TranslationElement,
        element_id: Option<TraceId>,
    ) {
        self.entries.push(TranslationLoss {
            action,
            element,
            element_id,
        });
    }

    /// Consume into the trace-envelope entry list.
    pub fn into_losses(self) -> Vec<TranslationLoss> {
        self.entries
    }
}

/// Positional path of one block in the normalized IR:
/// `messages[<msg>].blocks[<block>]`. Infallible by construction — the path
/// alphabet is a subset of the [`TraceId`] grammar and indices are bounded
/// integers.
pub fn block_path(message_index: usize, block_index: usize) -> TraceId {
    TraceId::new(format!("messages[{message_index}].blocks[{block_index}]"))
        .expect("positional block path is always a valid TraceId")
}

/// Positional path of one message in the normalized IR: `messages[<msg>]`.
pub fn message_path(message_index: usize) -> TraceId {
    TraceId::new(format!("messages[{message_index}]"))
        .expect("positional message path is always a valid TraceId")
}

/// Symbolic ID for an adapter-synthesized system segment (no
/// pre-translation position exists): `system.synthetic[<n>]`.
pub fn synthetic_system_id(index: usize) -> TraceId {
    TraceId::new(format!("system.synthetic[{index}]"))
        .expect("synthetic system id is always a valid TraceId")
}
