//! Informational memory baseline (SPEC-daemon-mode §5.3 / docs/memory-budget.md).
//!
//! `cargo test --test memory_baseline -- --ignored --nocapture`
//!
//! Prints the test process' own kernel + allocator view and, when a `synaps`
//! binary is available at `SYNAPS_BIN`, the `status --memory --json` report
//! for the live sessions on this machine. Never gates: the acceptance gates
//! run against release binaries on bella via `scripts/memprof/bench-sessions.sh`.

use synaps_cli::core::memstat;

#[test]
#[ignore = "informational: prints memory numbers, gates live in docs/memory-budget.md"]
fn memory_baseline_report() {
    let me = memstat::self_snapshot();
    println!(
        "self: rss={:.1} MB rss_anon={:.1} MB threads={} jemalloc(alloc={:.1} active={:.1} resident={:.1} retained={:.1} MB)",
        me.rss_kb as f64 / 1024.0,
        me.rss_anon_kb as f64 / 1024.0,
        me.threads,
        me.jemalloc_allocated_kb as f64 / 1024.0,
        me.jemalloc_active_kb as f64 / 1024.0,
        me.jemalloc_resident_kb as f64 / 1024.0,
        me.jemalloc_retained_kb as f64 / 1024.0,
    );

    if let Ok(rows) = memstat::tree(std::process::id()) {
        let t = memstat::MemTotals::of(&rows);
        println!(
            "tree: procs={} rss={:.1} pss={:.1} uss={:.1} anon={:.1} MB",
            t.procs,
            t.rss_kb as f64 / 1024.0,
            t.pss_kb as f64 / 1024.0,
            t.uss_kb as f64 / 1024.0,
            t.anon_kb as f64 / 1024.0
        );
    }

    if let Ok(bin) = std::env::var("SYNAPS_BIN") {
        match std::process::Command::new(bin)
            .args(["status", "--memory", "--json"])
            .output()
        {
            Ok(out) => println!("{}", String::from_utf8_lossy(&out.stdout)),
            Err(e) => println!("status --memory unavailable: {e}"),
        }
    }
    println!("gates: RssAnon(N=1) <= 24.5 MB · PSS(N=3) <= 160 MB · procs/session == 3 · startup <= 80 ms");
}

#[cfg_attr(not(target_os = "linux"), ignore)]
#[test]
fn memstat_tree_reports_self_as_engine_or_other_with_pss() {
    let rows = memstat::tree(std::process::id()).expect("linux /proc walk");
    assert_eq!(rows[0].pid, std::process::id());
    assert!(rows[0].pss_kb > 0);
}
