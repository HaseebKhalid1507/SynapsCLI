//! Local-first memory store.
//!
//! Append-only JSONL records keyed by namespace, stored under
//! `$SYNAPS_BASE_DIR/memory/<namespace>.jsonl`. No vector layer; queries
//! are prefix/substring filters over tags and content with optional
//! time-range and limit. Extensions access this via the `memory.append`
//! and `memory.query` protocol methods (added in a follow-up step).

pub mod store;
