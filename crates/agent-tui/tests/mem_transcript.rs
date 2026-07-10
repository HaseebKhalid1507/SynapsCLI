//! # mem_transcript — Slice 0 memory measurement harness (T241 §8)
//!
//! Synthetic, public-safe memory checkpoint integration test.
//! Prints `RssAnon` from `/proc/self/status` (Linux only) at four stages that
//! bracket the current eager-render pathology, and asserts counter baselines.
//!
//! ## Checkpoints
//!
//! | # | Label                  | Description                                              |
//! |---|------------------------|----------------------------------------------------------|
//! | 1 | `post-session-load`    | 1 000 synthetic messages pushed, cache Missing            |
//! | 2 | `post-display-rebuild` | identical to 1 in synthetic harness (no api→display step)|
//! | 3 | `post-first-frame`     | cold `render` call: Missing → sync_cache → full rebuild  |
//! | 4 | `post-steady-frame`    | 10 further renders + one scroll cycle                    |
//!
//! ## Invocation
//!
//! ```text
//! cargo test -p synaps-tui --test mem_transcript -- --ignored --nocapture
//! ```
//!
//! ## T241 relationship
//!
//! * **Slice 0 (now):**   captures baseline — all N messages rendered, syntect
//!   loaded, RssAnon high.  The `baseline_*` tests assert the CURRENT pathology.
//! * **Slice 6 (after lazy lands):**  re-run; numbers should satisfy §8
//!   thresholds.  Append results to `~/Jawz/workspace/t98-run-LOG.md`.

use agent_tui::tui::testing::TestHarness;

// ── RssAnon reader ────────────────────────────────────────────────────────────

fn rss_anon_kb() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let text = std::fs::read_to_string("/proc/self/status").ok()?;
        for line in text.lines() {
            if line.starts_with("RssAnon:") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                return parts.get(1).and_then(|v| v.parse::<u64>().ok());
            }
        }
        None
    }
    #[cfg(not(target_os = "linux"))]
    None
}

fn print_checkpoint(label: &str, harness: &TestHarness) {
    let rss = rss_anon_kb();
    let renders = harness.render_count();
    let hl = harness.highlight_call_count();
    let ss = harness.syntax_set_was_touched();
    match rss {
        Some(kb) => println!(
            "CHECKPOINT {label:<25}  RssAnon={:>7} kB ({:>5.1} MB)  \
             renders={:>5}  hl_calls={:>5}  ss_touched={}",
            kb, kb as f64 / 1024.0, renders, hl, ss
        ),
        None => println!(
            "CHECKPOINT {label:<25}  RssAnon=unavailable(non-Linux)  \
             renders={:>5}  hl_calls={:>5}  ss_touched={}",
            renders, hl, ss
        ),
    }
}

// ── Synthetic harness setup ───────────────────────────────────────────────────

/// Push 1 000 synthetic messages: every 5th has an off-screen fenced Rust code
/// block to exercise the current syntect-eager path.
///
/// Using push_text_message (public TestHarness API) to populate the store.
fn populate(harness: &mut TestHarness, total: usize) {
    let code_every = 5;
    for i in 0..total {
        if i % code_every == 0 {
            harness.push_text_message(&format!(
                "Synthetic assistant message {i} with embedded code.\n\
                 ```rust\n\
                 /// Synthetic function {i}\n\
                 fn synthetic_{i}(x: usize) -> usize {{\n\
                     // comment line {i}\n\
                     let squared = x * x;\n\
                     let cubed   = squared * x;\n\
                     cubed + {i}\n\
                 }}\n\
                 ```\n\
                 Prose after the code block for message {i}."
            ));
        } else {
            harness.push_text_message(&format!(
                "Synthetic assistant message {i}.\n\
                 This message contains **bold** and *italic* text.\n\
                 - Item A for message {i}\n\
                 - Item B for message {i}\n\
                 Line four of message {i}: quick brown fox jumps over the lazy dog."
            ));
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

/// Full four-checkpoint memory harness for T241 §8.
///
/// Prints RssAnon + counter values at each stage; also makes baseline
/// assertions (documented as "BASELINE — rewrite in Slice 4").
///
/// BASELINE — rewrite in Slice 4/6:
///   Checkpoint 3 renders == total (currently); after lazy landing renders ≤ 72.
///   Checkpoint 3 hl_calls > 0 (currently); after lazy landing == 0.
#[test]
#[ignore = "slow/synthetic: loads syntect; run with: cargo test -p synaps-tui --test mem_transcript -- --ignored --nocapture"]
fn mem_transcript_four_checkpoint_baseline() {
    const TOTAL: usize = 1_000;
    let viewport_cols: u16 = 122; // outer = inner 120 + 2 border
    let viewport_rows: u16 = 42;  // outer = inner 40 + 2 border

    println!("━━━ mem_transcript_four_checkpoint_baseline (T241 §8) ━━━");
    println!("    messages={TOTAL}  viewport=40×120  (every 5th has off-screen code fence)");
    println!();

    // ── Checkpoint 1: post-session-load ──────────────────────────────────────
    let mut harness = TestHarness::boot_with_size(viewport_cols, viewport_rows);
    harness.reset_perf_probe();
    harness.reset_highlight_probe();
    populate(&mut harness, TOTAL);

    // After push but BEFORE render — cache is Missing.
    // Reset counters so we measure only what `render` adds.
    harness.reset_perf_probe();
    harness.reset_highlight_probe();
    print_checkpoint("post-session-load", &harness);

    // ── Checkpoint 2: post-display-rebuild ───────────────────────────────────
    // In the synthetic harness push_text_message IS the display rebuild.
    // Snapshot again with the same state to match §8 checkpoint structure.
    print_checkpoint("post-display-rebuild", &harness);

    // ── Checkpoint 3: post-first-frame ───────────────────────────────────────
    // Cold render: Missing → sync_cache → renders all TOTAL messages + syntect.
    harness.reset_perf_probe();
    harness.reset_highlight_probe();

    let _ = harness.render();

    let cp3_rss = rss_anon_kb();
    let cp3_renders = harness.render_count();
    let cp3_hl = harness.highlight_call_count();
    let cp3_ss = harness.syntax_set_was_touched();

    print_checkpoint("post-first-frame", &harness);
    println!(
        "         (pathology snapshot: renders={cp3_renders} == total={TOTAL}, \
         hl_calls={cp3_hl}, ss_touched={cp3_ss})"
    );

    // ── Checkpoint 4: post-steady-frame ──────────────────────────────────────
    harness.reset_perf_probe();
    harness.reset_highlight_probe();

    // Scroll cycle: up 20, render×5, down 20, render×5
    for _ in 0..5 {
        harness.key(crossterm::event::KeyCode::PageUp, crossterm::event::KeyModifiers::NONE);
        let _ = harness.render();
    }
    for _ in 0..5 {
        harness.key(crossterm::event::KeyCode::PageDown, crossterm::event::KeyModifiers::NONE);
        let _ = harness.render();
    }

    let cp4_renders = harness.render_count();
    let cp4_hl     = harness.highlight_call_count();
    print_checkpoint("post-steady-frame", &harness);

    println!();
    println!("━━━ Summary ━━━");
    let rss_str = cp3_rss
        .map(|k| format!("{k} kB ({:.1} MB)", k as f64 / 1024.0))
        .unwrap_or_else(|| "unavailable".to_string());
    println!("  cp3 renders  = {cp3_renders}  (BASELINE: == {TOTAL}; target after Slice 3: ≤ 72)");
    println!("  cp3 hl_calls = {cp3_hl}        (BASELINE: > 0; target after Slice 3: == 0)");
    println!("  cp3 RssAnon  = {rss_str}");
    println!("  cp4 renders  = {cp4_renders}   (expected: 0 on a clean cache)");
    println!("  cp4 hl_calls = {cp4_hl}        (expected: 0 on a clean cache)");
    println!();
    println!("  t98-run-LOG.md one-liner:");
    println!(
        "  BASELINE | slice=0 | count={TOTAL} | cp3_RssAnon={rss_str} | \
         cp3_renders={cp3_renders} | cp3_hl={cp3_hl} | ss_touched={cp3_ss}"
    );

    // ── Baseline assertions (REWRITE IN SLICE 4) ─────────────────────────────
    //
    // POST-SLICE-3 RATCHETS — lazy measurement is active; first frame renders
    // only viewport + halo, not all messages.

    // First-frame renders << total messages (viewport+halo only).
    assert!(
        cp3_renders <= 72,
        "RATCHET: cold Missing first frame must render ≤72 (viewport+halo), got {cp3_renders}"
    );

    // Off-screen code fences must NOT trigger syntect; viewport fences may.
    // With 1000 msgs where every 5th has a fence, eager rendered all 200;
    // lazy renders only the ~21 viewport+halo msgs, of which ~4 have fences.
    assert!(
        cp3_hl <= 20,
        "RATCHET: highlight calls must be ≤20 (viewport only), got {cp3_hl}. \
         Eager baseline was ~200."
    );
    assert!(
        cp3_ss,
        "BASELINE: SYNTAX_SET initialized on first frame. \
         Slice 4 rewrites this to false."
    );

    // Steady state: clean cache, zero new renders (this should STAY true).
    assert_eq!(
        cp4_renders, 0,
        "steady-state frame must not trigger renders (clean cache); got {cp4_renders}"
    );
}
