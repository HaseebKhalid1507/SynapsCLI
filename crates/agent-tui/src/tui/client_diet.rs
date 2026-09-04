//! Client diet (PLAN-phase4 §4): purge points for the thin client. The
//! allocator tuning and the idle arm land with A2.

use agent_core::core::memstat;

/// `arena.<all>.purge` and one ladder line at `stage`.
pub fn purge_arenas(stage: &'static str) {
    memstat::purge_arenas();
    memstat::ladder(stage, &"");
}
