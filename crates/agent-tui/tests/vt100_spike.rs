//! P5 de-risking spike — escape-level render verification via vt100.
//!
//! Proves ONE capture path end-to-end: frame content rendered through
//! `CrosstermBackend<Vec<u8>>` (the production backend, in-memory `Write`
//! sink), parsed by `vt100::Parser`, asserted on the *parsed* screen grid.
//! This is the textual-rs lesson applied: `TestBackend` buffer tests can
//! pass while the live ANSI stream is broken; this path exercises the real
//! escape emission.
//!
//! # Scoping note for the full P5 rig
//!
//! ## What the easy path (this file) CAN verify
//!
//! - Everything crossterm emits while flushing a `terminal.draw()` diff:
//!   cursor positioning (CUP), SGR styling, content bytes, cursor
//!   hide/show. Any bug where the backend emits wrong/mis-ordered
//!   positioning or styling escapes for frame content is catchable here.
//! - Diff behavior across successive draws (render twice into the same
//!   backend, assert the second flush only repaints changed regions).
//! - **Edge scrub is capturable with modest effort**: contrary to the
//!   working assumption, `viewport::scrub_crossterm_terminal_edges` is
//!   generic over `W: Write` and queues `MoveTo`/`Print` *through the
//!   backend*, not raw stdout. A test that builds
//!   `Terminal<CrosstermBackend<Vec<u8>>>` can call it directly and assert
//!   the scrub sequences at the vt100 level. Only `render_frame` (the
//!   composed scrub+draw) is hardcoded to `Stdout`.
//!
//! ## What it CANNOT verify
//!
//! - **Lifecycle sequences**: `lifecycle.rs` writes alt-screen
//!   enter/leave, kitty keyboard-enhancement push/pop, and the emergency
//!   teardown sequence via `execute!(io::stdout(), …)` — bypassing any
//!   injectable backend. Same for the render thread's teardown path
//!   (`do_teardown` → `lifecycle::emergency_teardown_terminal`).
//! - **P1 sync brackets (2026)**: as of this branch no
//!   `BeginSynchronizedUpdate`/`EndSynchronizedUpdate` exists in the crate
//!   yet. When P1 lands, whether it's capturable depends on where it's
//!   written: through the backend (capturable) or raw stdout (not).
//! - **Real-TTY behaviors**: anything conditional on `is_tty`, terminal
//!   size queries, or the actual fd (nothing observed in the render path,
//!   but lifecycle probes keyboard-enhancement support).
//!
//! ## Recommendation for capturing the rest
//!
//! Prefer **(a) an injectable `Write` sink** over (b) fd-level stdout
//! capture. Concretely: make `lifecycle.rs` functions and
//! `render_frame`/`do_teardown` generic over `W: Write + ?Sized` (or take
//! `&mut dyn Write`), with production passing `io::stdout()`. The
//! type-plumbing is mechanical — `render_thread.rs` becomes
//! `Terminal<CrosstermBackend<W>>` end-to-end, which it already almost is
//! (`spawn_render_thread` is the only place `Stdout` is named as a
//! concrete type besides `render_frame`/`do_teardown` signatures).
//! fd-level capture (dup2 tricks) is process-global, races with test
//! parallelism, breaks under `cargo test`'s output capture, and can't
//! isolate which subsystem emitted what. Reject it.
//!
//! ## Full-rig estimate (5 sequence categories from the plan)
//!
//! 1. **Edge scrub** — small: capturable today via the generic
//!    `scrub_crossterm_terminal_edges` (see above). ~½ day incl. asserts.
//! 2. **2026 sync brackets** — blocked on P1 landing; if P1 writes through
//!    the backend, ~½ day. If through raw stdout, needs the W-injection
//!    refactor first.
//! 3. **Alt-screen enter/leave** + 4. **kitty push/pop** + 5. **teardown
//!    completeness** — all live in `lifecycle.rs` behind hardcoded
//!    `io::stdout()`; all three unlock together with one `W: Write`
//!    injection refactor (~1 day incl. not regressing the panic-hook /
//!    emergency-teardown paths, which must keep a real stdout fallback),
//!    then ~½–1 day of vt100 assertions across the three categories.
//!
//! Total: **~2–3 days**, matching the plan's estimate — provided P1 has
//! landed. The refactor is the bulk; the assertions are cheap once bytes
//! are capturable. The `render_ansi()` seam added to `TestHarness` in this
//! spike is the pattern the rig should extend, not replace.

use agent_tui::tui::testing::TestHarness;

/// End-to-end proof: harness frame → real CrosstermBackend ANSI bytes →
/// vt100 parse → assertions on the parsed screen grid.
#[test]
fn vt100_parses_captured_frame_content() {
    let mut h = TestHarness::boot(); // 80x24
    h.type_str("hello vt100");

    let bytes = h.render_ansi();
    assert!(
        !bytes.is_empty(),
        "CrosstermBackend<Vec<u8>> captured no bytes"
    );
    // Sanity: the stream contains real escape sequences, not just text.
    assert!(
        bytes.contains(&0x1b),
        "captured stream contains no ESC bytes — not an ANSI stream"
    );

    let mut parser = vt100::Parser::new(24, 80, 0);
    parser.process(&bytes);
    let screen = parser.screen();

    // Header chrome lands on row 0 of the *parsed* terminal.
    let row0: String = (0..80)
        .filter_map(|col| screen.cell(0, col))
        .map(|c| c.contents())
        .collect();
    assert!(
        row0.contains("Synaps"),
        "header not on parsed row 0: {row0:?}"
    );

    // Typed input appears in the lower quarter of the parsed screen
    // (input box sits above the status bar; exact row depends on layout).
    let lower: String = (18..24)
        .map(|row| {
            (0..80)
                .filter_map(|col| screen.cell(row, col))
                .map(|c| c.contents())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        lower.contains("hello vt100"),
        "typed input not in parsed bottom rows:\n{lower}"
    );

    // Whole-screen check mirrors the P4 smoke test at the escape level.
    let contents = screen.contents();
    assert!(
        contents.contains("ready"),
        "ready status missing from parsed screen:\n{contents}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// P16.2 — DA1-fenced boot query burst, at the byte/escape level.
//
// Same lesson as the render spike: assert on the REAL bytes the production
// writer emits (write_query_burst is exactly what run_setup flushes to the
// tty), and on the parsed result of a realistic synthetic reply stream —
// not on an abstraction of either.
// ─────────────────────────────────────────────────────────────────────────────

use agent_tui::tui::testing::termcaps::{
    parse_burst_replies, write_query_burst, TermCaps, QUERY_BURST,
};

/// The burst as emitted: all five capability queries present, DA1 strictly
/// LAST (the fence — in-band ordering is what makes "no reply by fence time"
/// mean "unsupported"), one flushed write.
#[test]
fn termcaps_burst_emits_da1_fenced_query_bytes() {
    let mut sink: Vec<u8> = Vec::new();
    write_query_burst(&mut sink).expect("write into Vec cannot fail");
    assert_eq!(
        sink.as_slice(),
        QUERY_BURST,
        "production writer must emit the canonical burst verbatim"
    );

    let s = String::from_utf8(sink).expect("burst is pure ASCII");
    // Every query present…
    assert!(s.contains("\x1b[>0q"), "XTVERSION query missing: {s:?}");
    assert!(
        s.contains("\x1b[?2026$p"),
        "DECRQM 2026 (sync output) query missing: {s:?}"
    );
    assert!(
        s.contains("\x1b[?2027$p"),
        "DECRQM 2027 (unicode width) query missing: {s:?}"
    );
    assert!(s.contains("\x1b[?u"), "kitty keyboard query missing: {s:?}");
    assert!(s.contains("\x1b[>c"), "DA2 query missing: {s:?}");
    // …and the fence is last, exactly once.
    assert!(
        s.ends_with("\x1b[c"),
        "DA1 must be the FINAL query (the fence): {s:?}"
    );
    assert_eq!(
        s.matches("\x1b[c").count(),
        1,
        "exactly one DA1 in the burst"
    );
}

/// The burst must be invisible: feeding the query bytes through a real vt100
/// terminal leaves the screen blank — queries render nothing and move nothing.
#[test]
fn termcaps_burst_is_invisible_on_a_vt100_screen() {
    let mut parser = vt100::Parser::new(24, 80, 0);
    parser.process(QUERY_BURST);
    let contents = parser.screen().contents();
    assert!(
        contents.trim().is_empty(),
        "query burst painted visible cells: {contents:?}"
    );
    assert_eq!(
        parser.screen().cursor_position(),
        (0, 0),
        "burst moved the cursor"
    );
}

/// A realistic kitty-style answer stream (XTVERSION DCS, kitty flags,
/// DECRPM 2026/2027, DA2, DA1 fence) parses into fact-based TermCaps.
#[test]
fn termcaps_synthetic_da1_reply_parses_into_caps() {
    let replies: &[u8] =
        b"\x1bP>|kitty(0.32.2)\x1b\\\x1b[?1u\x1b[?2026;2$y\x1b[?2027;1$y\x1b[>1;4000;13c\x1b[?62;22c";

    let parsed = parse_burst_replies(replies);
    assert!(parsed.da1, "DA1 fence not detected");
    assert!(parsed.kitty, "kitty flags reply not detected");
    assert_eq!(parsed.mode_2026, Some(2));
    assert_eq!(parsed.mode_2027, Some(1));

    let mut caps = TermCaps::default();
    caps.merge_burst(&parsed);
    assert!(caps.da1_answered);
    assert!(
        caps.kitty_keyboard,
        "kitty reply must flip kitty_keyboard to fact-true"
    );
    assert!(
        caps.sync_output,
        "DECRPM 2 (reset, settable) counts as supported"
    );
    assert!(caps.mode_2027, "DECRPM 1 (set) counts as supported");
}

/// The safety property behind the boot-hang gate: WITHOUT the DA1 fence
/// (timeout / dumb terminal / partial replies) the merge is a strict no-op —
/// caps stay at the env-detected defaults and boot behaves exactly as today.
#[test]
fn termcaps_unfenced_partial_replies_leave_defaults_untouched() {
    let parsed = parse_burst_replies(b"\x1b[?1u\x1b[?2026;1$y"); // no DA1 reply
    assert!(!parsed.da1);

    let mut caps = TermCaps::default();
    let before = caps.clone();
    caps.merge_burst(&parsed);
    assert_eq!(caps, before, "unfenced replies must be discarded wholesale");
    assert!(!caps.da1_answered);
}

// ═════════════════════════════════════════════════════════════════════════════
// P16.4 — Negotiation matrix (end-to-end, synthetic reply streams only).
//
// Drives the PURE parser (`parse_burst_replies`) + the pure merge
// (`TermCaps::merge_burst`) across every arm of the negotiation matrix the
// memo enumerates: full DA1-fenced reply / timeout (no reply) / partial
// replies / tmux-wrapped (DCS passthrough) answers / garbage-interleaved. Never
// touches real stdin — the async fd-0 `negotiate()` is deliberately NOT on the
// test facade (single-consumer rule). All facts are asserted through the public
// surface: `TermCaps::{default, merge_burst, summary}` + its `pub` fields.
//
// Every failure carries a human-readable artifact (`caps_artifact`) so a red
// run reads like the `--verbose` boot line, not a bare bool mismatch.
// ═════════════════════════════════════════════════════════════════════════════

use agent_tui::tui::testing::termcaps::{BurstReplies, BURST_TIMEOUT};

/// Human-readable failure artifact: mirrors the production `--verbose` boot
/// line (`TermCaps::summary`) so a failed matrix case is self-describing.
fn caps_artifact(label: &str, caps: &TermCaps) -> String {
    format!("[{label}] negotiated caps: {}", caps.summary())
}

/// Negotiate: start from env-detected caps (modeled here via the public fields,
/// since `detect_from_env` is crate-private) and fold in the parsed burst — the
/// exact `caps.merge_burst(&parse_burst_replies(bytes))` production performs.
fn negotiate_synthetic(env: TermCaps, replies: &[u8]) -> TermCaps {
    let mut caps = env;
    caps.merge_burst(&parse_burst_replies(replies));
    caps
}

// ── Case 1: FULL DA1-fenced reply — every capability answered ─────────────────

/// All five queries answered (kitty flags, 2026 set, 2027 set, DA2, DA1 fence
/// last). Every fact must flip to its negotiated value and `da1_answered` set.
#[test]
fn termcaps_negotiation_full_reply_all_caps_answered() {
    // XTVERSION DCS, kitty flags, DECRPM 2026=1 (set), 2027=1 (set), DA2, DA1.
    let stream: &[u8] =
        b"\x1bP>|WezTerm(20240203)\x1b\\\x1b[?1u\x1b[?2026;1$y\x1b[?2027;1$y\x1b[>1;4000;13c\x1b[?65;4c";

    let parsed = parse_burst_replies(stream);
    assert!(parsed.da1, "DA1 fence must be detected in a full reply");
    assert!(parsed.kitty, "kitty flags reply must be detected");
    assert_eq!(parsed.mode_2026, Some(1));
    assert_eq!(parsed.mode_2027, Some(1));

    let caps = negotiate_synthetic(TermCaps::default(), stream);
    let art = caps_artifact("full", &caps);
    assert!(caps.da1_answered, "{art}");
    assert!(caps.kitty_keyboard, "kitty must be fact-true — {art}");
    assert!(caps.sync_output, "2026 set ⇒ supported — {art}");
    assert!(caps.mode_2027, "2027 set ⇒ supported — {art}");
}

/// Full reply where the terminal answers DECRQM with 2026 UNSUPPORTED (value 4
/// = permanently reset) — the one arm that suppresses the sync bracket. DA1
/// still fences, so the fact is trusted.
#[test]
fn termcaps_negotiation_full_reply_2026_unsupported() {
    let stream: &[u8] = b"\x1b[?1u\x1b[?2026;4$y\x1b[?2027;0$y\x1b[?62;9c";
    let caps = negotiate_synthetic(TermCaps::default(), stream);
    let art = caps_artifact("2026-unsupported", &caps);
    assert!(caps.da1_answered, "{art}");
    assert!(caps.kitty_keyboard, "kitty answered ⇒ true — {art}");
    assert!(
        !caps.sync_output,
        "DECRPM 4 = permanently reset = unsupported — {art}"
    );
    assert!(!caps.mode_2027, "DECRPM 0 = not recognized — {art}");
}

// ── Case 2: TIMEOUT / no reply — defaults preserved (today's behavior) ────────

/// The deadline fired before any byte arrived (dumb terminal / pipe / slow
/// link). An empty buffer parses to the default `BurstReplies`; the merge is a
/// strict no-op ⇒ caps stay byte-identical to the env-detected input.
#[test]
fn termcaps_negotiation_timeout_no_reply_yields_defaults() {
    assert_eq!(parse_burst_replies(b""), BurstReplies::default());

    let before = TermCaps::default();
    let after = negotiate_synthetic(before.clone(), b"");
    assert_eq!(after, before, "{}", caps_artifact("timeout-empty", &after));
    assert!(!after.da1_answered, "no reply ⇒ never fenced");
    // Sanity: the documented deadline is a real, positive bound.
    assert!(BURST_TIMEOUT >= std::time::Duration::from_millis(100));
    assert!(BURST_TIMEOUT <= std::time::Duration::from_millis(250));
}

/// Timeout variant: replies arrived but the DA1 fence never did (deadline cut
/// the stream mid-negotiation). Unfenced ⇒ the whole batch is discarded.
#[test]
fn termcaps_negotiation_unfenced_replies_discarded_wholesale() {
    // kitty + both DECRPM answers, but NO DA1 — the fence never landed.
    let stream: &[u8] = b"\x1b[?1u\x1b[?2026;1$y\x1b[?2027;1$y";
    let parsed = parse_burst_replies(stream);
    assert!(!parsed.da1, "no DA1 in this stream");
    assert!(
        parsed.kitty && parsed.mode_2026.is_some(),
        "parser still saw them"
    );

    let before = TermCaps::default();
    let after = negotiate_synthetic(before.clone(), stream);
    assert_eq!(
        after,
        before,
        "unfenced replies must NOT mutate caps — {}",
        caps_artifact("unfenced", &after)
    );
}

// ── Case 3: PARTIAL reply — some caps answered, DA1 arrives ───────────────────

/// DA1 fences, kitty + 2027 answered, but the terminal ignored the 2026 query.
/// The answered facts merge; the un-answered 2026 keeps its harmless default-on
/// (log-honesty: terminals that don't support 2026 ignore the bracket anyway).
#[test]
fn termcaps_negotiation_partial_da1_arrives_unanswered_stay_default() {
    let stream: &[u8] = b"\x1b[?1u\x1b[?2027;1$y\x1b[?62;9c"; // no 2026 reply
    let parsed = parse_burst_replies(stream);
    assert!(parsed.da1 && parsed.kitty);
    assert_eq!(parsed.mode_2026, None, "2026 was never answered");
    assert_eq!(parsed.mode_2027, Some(1));

    let caps = negotiate_synthetic(TermCaps::default(), stream);
    let art = caps_artifact("partial", &caps);
    assert!(caps.da1_answered, "{art}");
    assert!(caps.kitty_keyboard, "{art}");
    assert!(caps.mode_2027, "answered 2027 merges — {art}");
    assert!(
        caps.sync_output,
        "un-answered 2026 keeps default-on — {art}"
    );
}

/// Partial the other way: DA1 fences but the terminal answered NOTHING else
/// (bare-bones VT that only knows DA1). kitty flips to fact-false (in-band
/// ordering: no reply by fence == unsupported); DECRQM-driven fields hold their
/// defaults.
#[test]
fn termcaps_negotiation_partial_da1_only_turns_kitty_off() {
    let caps = negotiate_synthetic(TermCaps::default(), b"\x1b[?6c");
    let art = caps_artifact("da1-only", &caps);
    assert!(caps.da1_answered, "{art}");
    assert!(
        !caps.kitty_keyboard,
        "no kitty reply by fence ⇒ off — {art}"
    );
    assert!(
        caps.sync_output,
        "no DECRQM answer ⇒ harmless default-on — {art}"
    );
    assert!(!caps.mode_2027, "no DECRQM answer ⇒ default-off — {art}");
}

// ── Case 4: tmux-wrapped answers (DCS passthrough) ────────────────────────────

/// REALITY CHECK — the parser treats a DCS block (`ESC P … ST`) as opaque and
/// skips it to the `ST` terminator; it does NOT unwrap tmux passthrough. So a
/// capability reply that a terminal buries inside a tmux passthrough DCS is
/// invisible to the parser — but any UNWRAPPED reply that follows (tmux
/// forwards real terminal responses raw, incl. the DA1 fence) is still parsed.
/// This documents the true behavior rather than pretending to unwrap.
#[test]
fn termcaps_negotiation_tmux_wrapped_dcs_is_skipped_trailing_csi_parsed() {
    // A tmux passthrough DCS (inner ESCs doubled) carrying a 2026 reply, then a
    // real (unwrapped) kitty reply + DA1 fence forwarded by tmux.
    let stream: &[u8] = b"\x1bPtmux;\x1b\x1b[?2026;1$y\x1b\\\x1b[?1u\x1b[?62;9;15c";
    let parsed = parse_burst_replies(stream);
    assert!(parsed.da1, "unwrapped DA1 after the DCS must still fence");
    assert!(
        parsed.kitty,
        "unwrapped kitty reply after the DCS must parse"
    );
    assert_eq!(
        parsed.mode_2026, None,
        "the 2026 reply buried inside the tmux DCS is NOT unwrapped — honest behavior"
    );

    let caps = negotiate_synthetic(TermCaps::default(), stream);
    let art = caps_artifact("tmux-wrapped", &caps);
    assert!(caps.da1_answered, "{art}");
    assert!(caps.kitty_keyboard, "{art}");
    assert!(
        caps.sync_output,
        "wrapped 2026 unseen ⇒ default-on holds — {art}"
    );
}

/// A well-formed XTVERSION DCS reply (`DCS > | … ST`) is skipped cleanly and
/// does not swallow the DA1 fence that follows — the DCS-skip terminates on ST,
/// not greedily.
#[test]
fn termcaps_negotiation_dcs_xtversion_does_not_eat_the_fence() {
    let stream: &[u8] = b"\x1bP>|tmux 3.4\x1b\\\x1b[?62;9c";
    assert!(
        parse_burst_replies(stream).da1,
        "DA1 after an XTVERSION DCS must still be seen"
    );
}

// ── Case 5: garbage / interleaved bytes — robust, no panic ────────────────────

/// A racing user keystroke, an unknown SGR CSI, and stray control bytes
/// interleaved with the real replies. The parser must skip the noise and still
/// recover every genuine reply — no panic, sensible facts.
#[test]
fn termcaps_negotiation_garbage_interleaved_recovers_real_replies() {
    // 'q' keystroke, a truecolor SGR, a BEL, then real 2026 + kitty + DA1.
    let stream: &[u8] = b"q\x1b[38;2;10;20;30m\x07\x1b[?2026;1$y\x1b[?0u\x1b[?6c";
    let parsed = parse_burst_replies(stream);
    assert!(
        parsed.da1 && parsed.kitty,
        "real replies recovered past the noise"
    );
    assert_eq!(parsed.mode_2026, Some(1));

    let caps = negotiate_synthetic(TermCaps::default(), stream);
    assert!(caps.da1_answered, "{}", caps_artifact("garbage", &caps));
}

/// Adversarial fuzz-style corpus: lone ESCs, truncated CSI/DCS, high bytes,
/// NULs, an all-0xFF slab. None may panic; none may spuriously fence; a
/// buffer with no DA1 must leave caps at defaults.
#[test]
fn termcaps_negotiation_adversarial_bytes_never_panic() {
    let corpus: &[&[u8]] = &[
        b"",
        b"\x1b",                    // lone ESC
        b"\x1b[",                   // ESC + CSI introducer, nothing else
        b"\x1b[?2026",              // truncated mid-CSI (no final byte)
        b"\x1bP>|never terminated", // truncated DCS (no ST)
        b"\x00\x00\x00\x00",        // NULs
        b"\xff\xfe\xfd\xfc\xfb",    // high bytes / invalid UTF-8
        b"\x1b[999999999999;0$y",   // absurd DECRQM mode (overflow-safe parse)
        b"\x1b]0;window title\x07", // OSC (not a burst reply)
        &[0xffu8; 256],             // large invalid slab
    ];
    for (idx, bytes) in corpus.iter().enumerate() {
        let parsed = parse_burst_replies(bytes); // must not panic
        assert!(!parsed.da1, "corpus[{idx}] must not spuriously fence");
        let caps = negotiate_synthetic(TermCaps::default(), bytes);
        assert_eq!(
            caps,
            TermCaps::default(),
            "corpus[{idx}] must leave defaults — {}",
            caps_artifact("adversarial", &caps)
        );
    }
}

// ── Composition: env provenance ∪ burst facts ─────────────────────────────────

/// The real negotiation is env-detection UNION burst facts. Provenance fields
/// (term_program, tmux) come from the env layer and MUST survive the burst
/// merge, while the DA1-fenced burst supplies the capability facts. Asserts the
/// two layers compose without clobbering each other.
#[test]
fn termcaps_negotiation_env_provenance_survives_burst_merge() {
    // Model env-detected caps under tmux on WezTerm (public fields stand in for
    // the crate-private detect_from_env).
    let env = TermCaps {
        term_program: Some("WezTerm".to_string()),
        tmux: Some("3.4".to_string()),
        ..TermCaps::default()
    };
    let stream: &[u8] = b"\x1b[?1u\x1b[?2026;4$y\x1b[?62;9c"; // DA1 + kitty, 2026 off
    let caps = negotiate_synthetic(env, stream);
    let art = caps_artifact("env+burst", &caps);
    // Provenance preserved …
    assert_eq!(caps.term_program.as_deref(), Some("WezTerm"), "{art}");
    assert_eq!(caps.tmux.as_deref(), Some("3.4"), "{art}");
    // … and burst facts merged.
    assert!(caps.da1_answered, "{art}");
    assert!(caps.kitty_keyboard, "{art}");
    assert!(!caps.sync_output, "2026 negotiated off — {art}");
}

// ── Verbose diagnostics: the `--verbose` boot line criterion ──────────────────

/// The memo's done-criterion: `--verbose` prints ONE `TermCaps` summary line
/// dumping all fields. `mod.rs` logs `caps.summary()` after `negotiate`; this
/// asserts that summary is complete (all six field keys) and reflects the
/// negotiated values — the same string a red matrix case emits as its artifact.
#[test]
fn termcaps_negotiation_verbose_summary_dumps_all_fields() {
    let env = TermCaps {
        term_program: Some("iTerm.app".to_string()),
        tmux: Some("3.3a".to_string()),
        ..TermCaps::default()
    };
    let stream: &[u8] = b"\x1b[?1u\x1b[?2026;1$y\x1b[?2027;4$y\x1b[?62;9c";
    let caps = negotiate_synthetic(env, stream);
    let line = caps.summary();

    // All six fields present in the one line …
    for key in [
        "sync_output=",
        "kitty_keyboard=",
        "mode_2027=",
        "term_program=",
        "tmux=",
        "da1_answered=",
    ] {
        assert!(line.contains(key), "verbose line missing {key}: {line:?}");
    }
    // … carrying the negotiated values.
    assert!(line.contains("da1_answered=true"), "{line:?}");
    assert!(line.contains("kitty_keyboard=true"), "{line:?}");
    assert!(line.contains("sync_output=true"), "{line:?}"); // 2026;1 = set
    assert!(line.contains("mode_2027=false"), "{line:?}"); // 2027;4 = perm-reset
    assert!(line.contains("term_program=iTerm.app"), "{line:?}");
    assert!(line.contains("tmux=3.3a"), "{line:?}");
}

/// The timeout path's diagnostic honesty: an un-negotiated boot still logs a
/// complete line, and it reports `da1_answered=false` (the "we behaved exactly
/// as today" signal an operator greps for).
#[test]
fn termcaps_negotiation_verbose_summary_reports_timeout_as_unfenced() {
    let caps = negotiate_synthetic(TermCaps::default(), b"");
    let line = caps.summary();
    assert!(
        line.contains("da1_answered=false"),
        "timeout must log unfenced: {line:?}"
    );
    assert!(
        line.contains("term_program=-"),
        "unset provenance renders as '-': {line:?}"
    );
    assert!(
        line.contains("tmux=-"),
        "unset tmux renders as '-': {line:?}"
    );
}

// ── Emit⇄parse round-trip: the production writer feeds the production parser ──

/// Close the loop: `write_query_burst` emits the canonical burst; a matching
/// synthetic reply for that exact burst parses + merges into a fully-negotiated
/// TermCaps. Proves the emitted queries and the parsed replies line up.
#[test]
fn termcaps_negotiation_emit_then_reply_roundtrip() {
    let mut burst: Vec<u8> = Vec::new();
    write_query_burst(&mut burst).expect("write into Vec is infallible");
    assert_eq!(
        burst.as_slice(),
        QUERY_BURST,
        "writer emits the canonical burst"
    );

    // A terminal that supports everything answers each query in the burst.
    let reply: &[u8] =
        b"\x1bP>|xterm(390)\x1b\\\x1b[?1u\x1b[?2026;1$y\x1b[?2027;1$y\x1b[>41;390;0c\x1b[?64;1;9c";
    let caps = negotiate_synthetic(TermCaps::default(), reply);
    let art = caps_artifact("roundtrip", &caps);
    assert!(
        caps.da1_answered && caps.kitty_keyboard && caps.sync_output && caps.mode_2027,
        "{art}"
    );
}
