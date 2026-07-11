//! P12.4: line-count ratchet for the run() loop spine (test-side mirror of
//! `scripts/loc-ratchet.sh`, which is the CI-wired source of truth).
//!
//! After the P12.1–P12.4 split, `src/tui/mod.rs` is the setup call + the
//! select! routing table + bounded teardown (431 lines at extraction time).
//! CEILING = 460 = that actual count + ~30 lines of margin for comments and
//! small glue. If this fails, do NOT bump the ceiling to make it pass —
//! move the new logic into `run_setup.rs`, `dispatch.rs`, `loop_arms.rs`,
//! or `stream_handler.rs`. Keep CEILING in sync with the script.

#[test]
fn tui_mod_rs_stays_within_line_ceiling() {
    const CEILING: usize = 460;
    let src = include_str!("../src/tui/mod.rs");
    let count = src.lines().count();
    assert!(
        count <= CEILING,
        "src/tui/mod.rs is {count} lines (ceiling {CEILING}) — run() must stay a \
         routing table; move loop logic into run_setup.rs / dispatch.rs / \
         loop_arms.rs / stream_handler.rs (P12.4 ratchet; bump only with review, \
         and keep scripts/loc-ratchet.sh in sync)"
    );
}
