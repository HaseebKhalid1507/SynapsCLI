//! Terminal capability facts (`TermCaps`) — the P16 migration seam.
//!
//! This module is the designated abstraction boundary between the concrete
//! terminal substrate (crossterm today; termina later) and the rest of
//! `agent-tui`. It answers one question: *what do we actually know about the
//! terminal we're attached to?*
//!
//! ## Isolation contract (P16.1)
//!
//! Right now this struct is **inert**. Nothing in the render/input/lifecycle
//! path reads it to decide behavior — the only wiring is a single `--verbose`
//! (`tracing::debug!`) boot line in `mod.rs` that dumps the detected caps.
//!
//! The conservative defaults are chosen so that when the *future* gates land
//! (P16.3: edge-scrub, synchronized-output, kitty-push), gating on a
//! **default** `TermCaps` reproduces today's *unconditional* behavior exactly.
//! In other words: `default()` is a faithful description of "what the code does
//! today when it assumes nothing and just sends the bytes." Every default below
//! is annotated with the concrete call site it mirrors.
//!
//! ## Two detection layers
//!
//! 1. **Env-only** (P16.1): `$TERM_PROGRAM`, `$TMUX` presence, and the
//!    `tmux -V` provenance string. Can't hang, can't race — safe anywhere.
//! 2. **DA1-fenced query burst** (P16.2): [`negotiate`] emits the batched
//!    query burst (XTVERSION, DECRQM 2026/2027, kitty-keyboard query, DA2,
//!    then DA1 **last** as the fence — the libvaxis pattern) and reads the
//!    replies directly from fd 0 with a hard deadline ([`BURST_TIMEOUT`]).
//!    Timeout or partial (unfenced) replies ⇒ the env-detected caps are
//!    returned **unchanged**, i.e. today's blind-optimism behavior, and boot
//!    proceeds normally.
//!
//! ## The single-consumer rule (load-bearing — crossterm #963 / #993)
//!
//! The burst reads raw bytes from fd 0 itself. That is only safe because it
//! runs in `run_setup()` AFTER raw-mode enable and BEFORE the crossterm
//! `EventStream` (the process's one long-lived stdin consumer) is created —
//! and because nothing else in this crate calls `crossterm::event::{poll,
//! read}` or `supports_keyboard_enhancement()` (which would lazily spawn
//! crossterm's internal reader). The bounded read completes (fence or
//! deadline) and releases fd 0 before `EventStream::new()` executes. It also
//! deliberately bypasses `io::Stdin`'s BufReader — a buffered read could slurp
//! user keystrokes typed during boot into a buffer the EventStream never sees.
//! **Never** call [`negotiate`] after the EventStream exists, and never spawn
//! a blocking reader thread for it (an orphaned `read(2)` after timeout would
//! steal bytes from the EventStream — the exact #963 failure mode).

/// Facts we know (or conservatively assume) about the attached terminal.
///
/// Construct via [`TermCaps::detect`] (reads the real process environment) or
/// [`TermCaps::detect_from_env`] (injected env — used by tests). A bare
/// [`TermCaps::default`] is the "we know nothing, behave exactly as today"
/// baseline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TermCaps {
    /// DEC 2026 synchronized output (`BeginSynchronizedUpdate` /
    /// `EndSynchronizedUpdate`).
    ///
    /// **Default `true` = current behavior.** `draw.rs:730/745` brackets every
    /// frame in Begin/EndSynchronizedUpdate *unconditionally* today. Terminals
    /// that don't support 2026 ignore the escape, so emitting it is harmless.
    /// Keeping the default `true` means a future `if caps.sync_output` gate is a
    /// no-op that still emits the bracket.
    pub sync_output: bool,

    /// Kitty keyboard protocol (disambiguate-escape / report-alternate-keys)
    /// enhancement flags.
    ///
    /// **Default `true` = current behavior.** `lifecycle.rs` blind-pushes
    /// `PushKeyboardEnhancementFlags(...)` best-effort at setup and swallows the
    /// error; terminals that don't support it ignore the sequence. Default
    /// `true` means a future `if caps.kitty_keyboard` gate still performs the
    /// push (the blind push becomes the timeout fallback in P16.3).
    pub kitty_keyboard: bool,

    /// Unicode / grapheme width mode 2027.
    ///
    /// **Default `false` = current behavior.** We do no width negotiation today;
    /// width is handled unconditionally by the existing metrics path. `false`
    /// means "not negotiated," so a future `if caps.mode_2027` gate stays off
    /// and leaves today's width behavior untouched.
    pub mode_2027: bool,

    /// Value of `$TERM_PROGRAM` (e.g. `"iTerm.app"`, `"WezTerm"`, `"tmux"`,
    /// `"vscode"`), if present and non-empty. `None` = unset.
    ///
    /// **Default `None`** — informational provenance only; nothing gates on it
    /// today.
    pub term_program: Option<String>,

    /// tmux provenance. `Some(version)` when `$TMUX` is present (running inside
    /// a tmux client); the version string is parsed from `tmux -V`
    /// (e.g. `"3.3a"`, `"next-3.4"`), or `"unknown"` if `tmux -V` couldn't be
    /// read. `None` = not under tmux.
    ///
    /// **Default `None`.** Note the future edge-scrub gate (P16.3) becomes
    /// *provenance-driven* — scrub only under tmux. That gate intentionally
    /// changes behavior for the no-tmux case and is out of scope here; P16.1
    /// only records the fact. Under real tmux, [`detect_from_env`] sets this to
    /// `Some(..)` so the eventual gate matches today's scrub-under-tmux path.
    ///
    /// [`detect_from_env`]: TermCaps::detect_from_env
    pub tmux: Option<String>,

    /// Whether a DA1 (Primary Device Attributes) reply has been observed.
    ///
    /// **Default `false` = current behavior.** We never query the terminal
    /// today, so DA1 is never answered. `false` is the "unknown / no query
    /// performed" state that all P16.3 fallbacks key off of (unknown ⇒ do the
    /// unconditional thing). This flips to `true` only in P16.2 once the
    /// DA1-fenced boot burst lands.
    pub da1_answered: bool,
}

impl Default for TermCaps {
    /// The conservative baseline: **identical to today's unconditional
    /// behavior.** See each field's doc comment for the mirrored call site.
    fn default() -> Self {
        Self {
            sync_output: true,     // = draw.rs unconditionally brackets frames
            kitty_keyboard: true,  // = lifecycle blind-pushes kitty flags
            mode_2027: false,      // = no width negotiation today
            term_program: None,
            tmux: None,
            da1_answered: false,   // = we never query the terminal today
        }
    }
}

impl TermCaps {
    /// Detect caps from the **real** process environment.
    ///
    /// Reads `std::env::var` and shells out to `tmux -V` for the version. This
    /// is the production entry point; tests use [`detect_from_env`] with an
    /// injected env map so they never touch the real process environment.
    ///
    /// [`detect_from_env`]: TermCaps::detect_from_env
    pub(crate) fn detect() -> Self {
        Self::detect_from_env(
            |k| std::env::var(k).ok(),
            // Lazily invoked only when `$TMUX` is present — avoids spawning a
            // process on the common no-tmux path.
            read_tmux_version_from_command,
        )
    }

    /// Detect caps from an **injected** environment — the test seam.
    ///
    /// * `get_env` resolves an environment variable by name
    ///   (e.g. `|k| std::env::var(k).ok()`), returning `None` when unset.
    /// * `tmux_version` yields the raw `tmux -V` output (e.g.
    ///   `"tmux 3.3a\n"`), or `None` if it couldn't be read. It is only invoked
    ///   when `$TMUX` indicates we're under tmux, so tests for the no-tmux path
    ///   can assert it is never called.
    ///
    /// Starts from [`TermCaps::default`] (today's behavior) and only fills in
    /// the env-derived provenance fields. The boolean capability fields are
    /// left at their conservative defaults — they only change once real queries
    /// land in P16.2/P16.3.
    pub(crate) fn detect_from_env<E, V>(get_env: E, tmux_version: V) -> Self
    where
        E: Fn(&str) -> Option<String>,
        V: FnOnce() -> Option<String>,
    {
        // $TERM_PROGRAM — provenance only. Treat empty string as unset.
        let term_program = get_env("TERM_PROGRAM").filter(|s| !s.is_empty());

        // $TMUX — presence (non-empty) means we're inside a tmux client. tmux
        // sets $TMUX to the socket path; its mere presence is the signal.
        let under_tmux = get_env("TMUX").map(|v| !v.is_empty()).unwrap_or(false);
        let tmux = if under_tmux {
            // Parse the version from `tmux -V`; fall back to "unknown" so the
            // provenance is still recorded even if the version read fails.
            Some(
                parse_tmux_version(tmux_version().as_deref())
                    .unwrap_or_else(|| "unknown".to_string()),
            )
        } else {
            None
        };

        // Only the env-derived provenance fields are filled; the boolean
        // capability bits stay at their conservative (= today's behavior)
        // defaults via struct-update syntax.
        TermCaps {
            term_program,
            tmux,
            ..TermCaps::default()
        }
    }

    /// One-line human-readable summary for the `--verbose` boot log.
    pub(crate) fn summary(&self) -> String {
        format!(
            "sync_output={} kitty_keyboard={} mode_2027={} term_program={} tmux={} da1_answered={}",
            self.sync_output,
            self.kitty_keyboard,
            self.mode_2027,
            self.term_program.as_deref().unwrap_or("-"),
            self.tmux.as_deref().unwrap_or("-"),
            self.da1_answered,
        )
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// P16.3 — capability gates (render path consumers)
// ─────────────────────────────────────────────────────────────────────────────
//
// Each gate is a pure decision function over `Option<&TermCaps>`, where **`None`
// means "no negotiated facts available" — the unknown baseline that MUST
// reproduce today's unconditional behavior byte-for-byte.** Production threads
// `Some(&caps)` from `run_setup` (env detection always ran, DA1 burst maybe);
// the harness renders through `render_frame_into`, which never reaches these
// gates, so its snapshots are unaffected regardless.
//
// The gates only CHANGE behavior when caps are *affirmatively* known:
//   * edge-scrub: keyed on tmux provenance (env-detected, always known under
//     `Some`) — affirmative no-tmux ⇒ skip the scrub the artifact needs.
//   * sync-output / kitty: keyed on the DA1 fence — no fence (`da1_answered ==
//     false`) ⇒ fall back to today's unconditional emit/push.

/// Edge-scrub gate (`viewport::scrub_crossterm_terminal_edges`).
///
/// Edge-scrub exists *because of* tmux/pane scroll artifacts, so with negotiated
/// facts we run it only under tmux provenance. Unknown (`None`) reproduces
/// today's **unconditional** scrub.
///
/// | caps                              | scrub? | rationale                       |
/// |-----------------------------------|--------|---------------------------------|
/// | `None` (unknown / not threaded)   | yes    | = today's unconditional scrub   |
/// | `Some` with `tmux: Some(_)`       | yes    | tmux artifacts present          |
/// | `Some` with `tmux: None`          | no     | affirmatively not under tmux    |
pub(crate) fn edge_scrub_enabled(caps: Option<&TermCaps>) -> bool {
    match caps {
        None => true,                   // unknown ⇒ current behavior (scrub)
        Some(c) => c.tmux.is_some(),    // known ⇒ scrub only under tmux
    }
}

/// Synchronized-output gate (`draw.rs` Begin/EndSynchronizedUpdate, mode 2026).
///
/// Default-ON preserves today: terminals that don't support 2026 ignore the
/// bracket, so emitting it blind is harmless. We only *suppress* the bracket
/// when the DA1 fence proved the terminal answered queries AND affirmatively
/// reported 2026 unsupported (`sync_output == false`). No fence ⇒ emit.
///
/// | caps                                        | emit bracket? |
/// |---------------------------------------------|---------------|
/// | `None` (unknown / not threaded)             | yes (= today) |
/// | `Some` with `da1_answered == false`         | yes (= today) |
/// | `Some`, DA1-fenced, `sync_output == true`   | yes           |
/// | `Some`, DA1-fenced, `sync_output == false`  | no            |
pub(crate) fn sync_output_enabled(caps: Option<&TermCaps>) -> bool {
    match caps {
        None => true,                        // unknown ⇒ emit (current)
        Some(c) if !c.da1_answered => true,  // DA1 timed out ⇒ emit (current)
        Some(c) => c.sync_output,            // negotiated fact
    }
}

/// Kitty-keyboard push gate (`lifecycle::setup_terminal`
/// `PushKeyboardEnhancementFlags`).
///
/// Fact-based when the DA1 fence answered; otherwise the blind best-effort push
/// (= today). Note the push site runs during `setup_terminal`, i.e. BEFORE the
/// DA1 burst, so in production caps are not yet fenced there and this correctly
/// degrades to the blind push. The gate is future-proofing + log-honesty and is
/// exercised as a decision function.
///
/// | caps                                          | push? |
/// |-----------------------------------------------|-------|
/// | `None` (unknown / not threaded)               | yes (= today) |
/// | `Some` with `da1_answered == false`           | yes (= today) |
/// | `Some`, DA1-fenced, `kitty_keyboard == true`  | yes   |
/// | `Some`, DA1-fenced, `kitty_keyboard == false` | no    |
pub(crate) fn kitty_push_enabled(caps: Option<&TermCaps>) -> bool {
    match caps {
        None => true,                        // unknown ⇒ push (current)
        Some(c) if !c.da1_answered => true,  // DA1 timed out ⇒ push (current)
        Some(c) => c.kitty_keyboard,         // negotiated fact
    }
}

/// Parse the version token out of a raw `tmux -V` string.
///
/// `tmux -V` prints e.g. `"tmux 3.3a"`, `"tmux next-3.4"`, or on some builds
/// `"tmux master"`. We strip the leading `"tmux "` prefix and return the rest,
/// trimmed. Returns `None` for empty/whitespace-only input.
fn parse_tmux_version(raw: Option<&str>) -> Option<String> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    let version = raw.strip_prefix("tmux").unwrap_or(raw).trim();
    if version.is_empty() {
        None
    } else {
        Some(version.to_string())
    }
}

/// Production `tmux -V` reader: spawn tmux and capture stdout. Any failure
/// (tmux not installed, non-zero exit, non-UTF8) yields `None`, which the
/// caller maps to the `"unknown"` provenance.
fn read_tmux_version_from_command() -> Option<String> {
    let output = std::process::Command::new("tmux").arg("-V").output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Build an injected-env getter from a static list of pairs. Never touches
    /// the real process environment.
    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| map.get(k).cloned()
    }

    // ── Conservative-defaults matrix ───────────────────────────────────────

    #[test]
    fn defaults_are_identical_to_current_unconditional_behavior() {
        let caps = TermCaps::default();
        // sync output + kitty push happen unconditionally today → default on.
        assert!(caps.sync_output, "sync_output default must stay true (= today's unconditional bracket)");
        assert!(caps.kitty_keyboard, "kitty_keyboard default must stay true (= today's blind push)");
        // No negotiation today → these stay off / unknown.
        assert!(!caps.mode_2027, "mode_2027 default must stay false (no width negotiation today)");
        assert!(!caps.da1_answered, "da1_answered default must stay false (we never query today)");
        assert_eq!(caps.term_program, None);
        assert_eq!(caps.tmux, None);
    }

    #[test]
    fn detection_never_flips_capability_bits_only_provenance() {
        // Even a fully-populated env leaves the boolean caps at defaults — env
        // detection records provenance, it does not negotiate capabilities.
        let caps = TermCaps::detect_from_env(
            env_of(&[("TERM_PROGRAM", "WezTerm"), ("TMUX", "/tmp/tmux-1000/default,42,0")]),
            || Some("tmux 3.4".to_string()),
        );
        assert!(caps.sync_output);
        assert!(caps.kitty_keyboard);
        assert!(!caps.mode_2027);
        assert!(!caps.da1_answered);
    }

    // ── tmux present: version parsing ──────────────────────────────────────

    #[test]
    fn tmux_present_parses_version() {
        let caps = TermCaps::detect_from_env(
            env_of(&[("TMUX", "/tmp/tmux-1000/default,4242,0")]),
            || Some("tmux 3.3a\n".to_string()),
        );
        assert_eq!(caps.tmux.as_deref(), Some("3.3a"));
    }

    #[test]
    fn tmux_present_parses_prerelease_version() {
        let caps = TermCaps::detect_from_env(
            env_of(&[("TMUX", "/tmp/tmux-1000/default,1,0")]),
            || Some("tmux next-3.4".to_string()),
        );
        assert_eq!(caps.tmux.as_deref(), Some("next-3.4"));
    }

    #[test]
    fn tmux_present_but_version_read_fails_records_unknown() {
        let caps = TermCaps::detect_from_env(
            env_of(&[("TMUX", "/tmp/tmux-1000/default,1,0")]),
            || None, // `tmux -V` failed / not on PATH / non-UTF8
        );
        assert_eq!(caps.tmux.as_deref(), Some("unknown"));
    }

    #[test]
    fn tmux_present_but_garbage_version_records_unknown() {
        // Non-empty but unparseable → "" after prefix strip → unknown.
        let caps = TermCaps::detect_from_env(
            env_of(&[("TMUX", "/tmp/x,1,0")]),
            || Some("tmux \n".to_string()),
        );
        assert_eq!(caps.tmux.as_deref(), Some("unknown"));
    }

    // ── no tmux ────────────────────────────────────────────────────────────

    #[test]
    fn no_tmux_env_yields_none_and_never_reads_version() {
        let mut called = false;
        let caps = TermCaps::detect_from_env(
            env_of(&[("TERM_PROGRAM", "Apple_Terminal")]),
            || {
                called = true;
                Some("tmux 3.3a".to_string())
            },
        );
        assert_eq!(caps.tmux, None);
        assert!(!called, "tmux -V must not be invoked when $TMUX is absent");
    }

    #[test]
    fn empty_tmux_var_is_treated_as_no_tmux() {
        let caps = TermCaps::detect_from_env(
            env_of(&[("TMUX", "")]),
            || Some("tmux 3.3a".to_string()),
        );
        assert_eq!(caps.tmux, None);
    }

    // ── $TERM_PROGRAM matrix ───────────────────────────────────────────────

    #[test]
    fn term_program_iterm() {
        let caps = TermCaps::detect_from_env(env_of(&[("TERM_PROGRAM", "iTerm.app")]), || None);
        assert_eq!(caps.term_program.as_deref(), Some("iTerm.app"));
    }

    #[test]
    fn term_program_tmux_value() {
        // $TERM_PROGRAM can literally be "tmux" — that's provenance, distinct
        // from the $TMUX presence check.
        let caps = TermCaps::detect_from_env(env_of(&[("TERM_PROGRAM", "tmux")]), || None);
        assert_eq!(caps.term_program.as_deref(), Some("tmux"));
        assert_eq!(caps.tmux, None, "$TERM_PROGRAM=tmux must NOT imply tmux provenance without $TMUX");
    }

    #[test]
    fn term_program_vscode() {
        let caps = TermCaps::detect_from_env(env_of(&[("TERM_PROGRAM", "vscode")]), || None);
        assert_eq!(caps.term_program.as_deref(), Some("vscode"));
    }

    #[test]
    fn term_program_empty_is_none() {
        let caps = TermCaps::detect_from_env(env_of(&[("TERM_PROGRAM", "")]), || None);
        assert_eq!(caps.term_program, None);
    }

    #[test]
    fn term_program_unset_is_none() {
        let caps = TermCaps::detect_from_env(env_of(&[]), || None);
        assert_eq!(caps.term_program, None);
    }

    // ── combined provenance ────────────────────────────────────────────────

    #[test]
    fn tmux_and_term_program_together() {
        let caps = TermCaps::detect_from_env(
            env_of(&[
                ("TERM_PROGRAM", "iTerm.app"),
                ("TMUX", "/tmp/tmux-1000/default,7,1"),
            ]),
            || Some("tmux 3.2a".to_string()),
        );
        assert_eq!(caps.term_program.as_deref(), Some("iTerm.app"));
        assert_eq!(caps.tmux.as_deref(), Some("3.2a"));
    }

    // ── parse_tmux_version unit coverage ───────────────────────────────────

    #[test]
    fn parse_version_edge_cases() {
        assert_eq!(parse_tmux_version(Some("tmux 3.3a")).as_deref(), Some("3.3a"));
        assert_eq!(parse_tmux_version(Some("  tmux 3.3a  ")).as_deref(), Some("3.3a"));
        assert_eq!(parse_tmux_version(Some("tmux master")).as_deref(), Some("master"));
        assert_eq!(parse_tmux_version(Some("3.3a")).as_deref(), Some("3.3a")); // no prefix
        assert_eq!(parse_tmux_version(Some("")), None);
        assert_eq!(parse_tmux_version(Some("   ")), None);
        assert_eq!(parse_tmux_version(None), None);
    }

    // ── summary line for --verbose ─────────────────────────────────────────

    #[test]
    fn summary_renders_all_fields() {
        let caps = TermCaps::detect_from_env(
            env_of(&[("TERM_PROGRAM", "WezTerm"), ("TMUX", "/tmp/x,1,0")]),
            || Some("tmux 3.4".to_string()),
        );
        let s = caps.summary();
        assert!(s.contains("term_program=WezTerm"));
        assert!(s.contains("tmux=3.4"));
        assert!(s.contains("sync_output=true"));
        assert!(s.contains("da1_answered=false"));
    }
}

#[cfg(test)]
mod gate_tests {
    //! P16.3 gate-decision tests. Each gate has an explicit `unknown ⇒ current
    //! behavior` case plus the affirmatively-known cases that change behavior.
    use super::*;

    // Helper: env-detected caps under real tmux (env detection always runs, so
    // tmux provenance is "known" the moment we have a `Some`).
    fn caps_tmux() -> TermCaps {
        TermCaps {
            tmux: Some("3.4".to_string()),
            ..TermCaps::default()
        }
    }
    // Helper: env-detected caps affirmatively NOT under tmux.
    fn caps_no_tmux() -> TermCaps {
        TermCaps { tmux: None, ..TermCaps::default() }
    }

    // ── Gate 1: edge-scrub (tmux provenance) ───────────────────────────────

    #[test]
    fn edge_scrub_unknown_caps_defaults_to_current_behavior() {
        // No negotiated facts ⇒ today's UNCONDITIONAL scrub.
        assert!(edge_scrub_enabled(None), "unknown caps must scrub (= today)");
    }

    #[test]
    fn edge_scrub_runs_under_tmux() {
        assert!(edge_scrub_enabled(Some(&caps_tmux())), "tmux provenance ⇒ scrub");
    }

    #[test]
    fn edge_scrub_skipped_when_affirmatively_no_tmux() {
        // The ONLY behavior change: affirmatively-known no-tmux ⇒ skip scrub.
        assert!(
            !edge_scrub_enabled(Some(&caps_no_tmux())),
            "affirmative no-tmux ⇒ skip scrub"
        );
    }

    #[test]
    fn edge_scrub_default_termcaps_is_no_tmux() {
        // Sanity: a real detect() with no $TMUX yields tmux=None ⇒ skip, but
        // that is `Some(caps)`, distinct from the `None` unknown baseline.
        assert!(!edge_scrub_enabled(Some(&TermCaps::default())));
        assert!(edge_scrub_enabled(None));
    }

    // ── Gate 2: sync-output (mode 2026, default-ON) ────────────────────────

    #[test]
    fn sync_output_unknown_caps_defaults_to_emit() {
        // No caps threaded ⇒ emit the bracket (= today's unconditional Begin).
        assert!(sync_output_enabled(None), "unknown caps must emit (= today)");
    }

    #[test]
    fn sync_output_da1_timeout_defaults_to_emit() {
        // DA1 never answered (default caps) ⇒ still emit — preserves today.
        let caps = TermCaps::default();
        assert!(!caps.da1_answered);
        assert!(sync_output_enabled(Some(&caps)), "no DA1 fence ⇒ emit (= today)");
    }

    #[test]
    fn sync_output_negotiated_supported_emits() {
        let caps = TermCaps { da1_answered: true, sync_output: true, ..TermCaps::default() };
        assert!(sync_output_enabled(Some(&caps)));
    }

    #[test]
    fn sync_output_negotiated_unsupported_suppressed() {
        // Only affirmatively-negotiated 2026-unsupported suppresses the bracket.
        let caps = TermCaps { da1_answered: true, sync_output: false, ..TermCaps::default() };
        assert!(!sync_output_enabled(Some(&caps)), "2026 negotiated off ⇒ no bracket");
    }

    // ── Gate 3: kitty push (DA1-fenced fact, blind fallback) ───────────────

    #[test]
    fn kitty_push_unknown_caps_defaults_to_push() {
        assert!(kitty_push_enabled(None), "unknown caps must push (= today)");
    }

    #[test]
    fn kitty_push_da1_timeout_defaults_to_push() {
        // Default caps (no fence) ⇒ blind best-effort push, exactly like today.
        let caps = TermCaps::default();
        assert!(!caps.da1_answered);
        assert!(kitty_push_enabled(Some(&caps)), "no DA1 fence ⇒ blind push (= today)");
    }

    #[test]
    fn kitty_push_negotiated_supported_pushes() {
        let caps = TermCaps { da1_answered: true, kitty_keyboard: true, ..TermCaps::default() };
        assert!(kitty_push_enabled(Some(&caps)));
    }

    #[test]
    fn kitty_push_negotiated_unsupported_skipped() {
        let caps = TermCaps { da1_answered: true, kitty_keyboard: false, ..TermCaps::default() };
        assert!(!kitty_push_enabled(Some(&caps)), "kitty negotiated off ⇒ no push");
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// P16.2 — DA1-fenced boot query burst
// ─────────────────────────────────────────────────────────────────────────────

/// The batched capability query burst, emitted as ONE write.
///
/// libvaxis ordering: every capability query first, **DA1 last as the fence**.
/// Terminals process in-band queries in order, so by the time the DA1 reply
/// (`CSI ? … c`) arrives, every query the terminal was ever going to answer
/// has been answered. Terminals ignore queries they don't recognize, and
/// (essentially) every terminal answers DA1.
///
/// | bytes           | query                                  | reply shape            |
/// |-----------------|----------------------------------------|-------------------------|
/// | `ESC [ > 0 q`   | XTVERSION (name/version)               | `DCS > \| … ST` (skipped)|
/// | `ESC [ ?2026$p` | DECRQM: synchronized output (2026)     | `CSI ? 2026 ; v $ y`    |
/// | `ESC [ ?2027$p` | DECRQM: grapheme/unicode width (2027)  | `CSI ? 2027 ; v $ y`    |
/// | `ESC [ ? u`     | kitty keyboard protocol flags          | `CSI ? flags u`         |
/// | `ESC [ > c`     | DA2 (secondary device attributes)      | `CSI > … c` (skipped)   |
/// | `ESC [ c`       | **DA1 — the fence**                    | `CSI ? … c`             |
pub const QUERY_BURST: &[u8] = b"\x1b[>0q\x1b[?2026$p\x1b[?2027$p\x1b[?u\x1b[>c\x1b[c";

/// Hard deadline for the whole reply read. Local terminals answer DA1 in
/// single-digit milliseconds; 150ms covers tmux/ssh indirection while keeping
/// the worst-case boot cost (a terminal that never answers DA1) imperceptible.
/// Within the plan's 100–250ms band.
pub const BURST_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(150);

/// Write the query burst to `w` and flush. Split out (and generic) so the
/// vt100 tests can capture the exact bytes production emits.
pub fn write_query_burst<W: std::io::Write>(w: &mut W) -> std::io::Result<()> {
    w.write_all(QUERY_BURST)?;
    w.flush()
}

/// Replies extracted from the raw bytes read back after the burst.
///
/// `da1` is the fence: [`TermCaps::merge_burst`] applies **nothing** unless it
/// is set — partial (unfenced) replies are discarded wholesale, so the timeout
/// path degenerates to "env caps unchanged".
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BurstReplies {
    /// DA1 reply (`CSI ? … c`) observed — the fence.
    pub da1: bool,
    /// Kitty keyboard flags reply (`CSI ? flags u`) observed.
    pub kitty: bool,
    /// DECRPM value for mode 2026, if a `CSI ? 2026 ; v $ y` reply was seen.
    /// DECRPM values: 0 = not recognized, 1 = set, 2 = reset (settable),
    /// 3 = permanently set, 4 = permanently reset.
    pub mode_2026: Option<u8>,
    /// DECRPM value for mode 2027 (same value semantics as `mode_2026`).
    pub mode_2027: Option<u8>,
}

/// DECRPM value ⇒ "the terminal supports this mode". 1/2/3 are supported
/// (set, resettable, permanently set); 0/4 are not.
fn decrpm_supported(v: u8) -> bool {
    matches!(v, 1..=3)
}

/// Parse the accumulated reply bytes. Pure function — the test seam.
///
/// Tolerant by design: unknown CSI sequences and stray bytes (a user
/// keystroke racing the burst) are skipped; DCS payloads (the XTVERSION
/// reply) are skipped to their `ST` terminator; a truncated trailing sequence
/// is ignored (the caller re-parses as more bytes arrive).
pub fn parse_burst_replies(buf: &[u8]) -> BurstReplies {
    let mut out = BurstReplies::default();
    let mut i = 0;
    while i < buf.len() {
        if buf[i] != 0x1b {
            i += 1; // stray byte (e.g. racing keystroke) — skip
            continue;
        }
        match buf.get(i + 1) {
            Some(b'[') => {
                // CSI: parameter/intermediate bytes (0x20..=0x3f) until a
                // final byte (0x40..=0x7e).
                let start = i + 2;
                let mut j = start;
                while j < buf.len() && !(0x40..=0x7e).contains(&buf[j]) {
                    j += 1;
                }
                if j >= buf.len() {
                    break; // truncated — wait for more bytes
                }
                classify_csi(&buf[start..j], buf[j], &mut out);
                i = j + 1;
            }
            Some(b'P') => {
                // DCS (XTVERSION reply) — skip to ST (ESC \).
                let mut j = i + 2;
                while j + 1 < buf.len() && !(buf[j] == 0x1b && buf[j + 1] == b'\\') {
                    j += 1;
                }
                if j + 1 >= buf.len() {
                    break; // truncated DCS — wait for more bytes
                }
                i = j + 2;
            }
            Some(_) => i += 2, // other escape — skip introducer
            None => break,
        }
    }
    out
}

/// Classify one complete CSI sequence (`body` = bytes between `ESC [` and the
/// final byte).
fn classify_csi(body: &[u8], final_byte: u8, out: &mut BurstReplies) {
    match final_byte {
        // DA1 reply: CSI ? … c  (DA2 replies are `CSI > … c` — ignored)
        b'c' if body.first() == Some(&b'?') => out.da1 = true,
        // kitty keyboard flags reply: CSI ? flags u
        b'u' if body.first() == Some(&b'?') => out.kitty = true,
        // DECRPM reply: CSI ? <mode> ; <value> $ y
        b'y' if body.first() == Some(&b'?') && body.last() == Some(&b'$') => {
            let inner = std::str::from_utf8(&body[1..body.len() - 1]).unwrap_or("");
            let mut parts = inner.split(';');
            let mode = parts.next().and_then(|p| p.parse::<u32>().ok());
            let value = parts.next().and_then(|p| p.parse::<u8>().ok());
            match (mode, value) {
                (Some(2026), Some(v)) => out.mode_2026 = Some(v),
                (Some(2027), Some(v)) => out.mode_2027 = Some(v),
                _ => {}
            }
        }
        _ => {}
    }
}

impl TermCaps {
    /// Merge burst replies onto env-detected caps — **only if DA1-fenced**.
    ///
    /// No fence ⇒ no-op: timeout/partial replies leave the caps exactly as
    /// env detection produced them (= today's blind behavior). With the
    /// fence, every earlier query has had its chance to answer (in-band
    /// ordering), so:
    /// * `kitty_keyboard` becomes fact: reply seen ⇔ supported.
    /// * `mode_2027` / `sync_output` flip on an **explicit** DECRPM value;
    ///   a terminal that answers DA1 but ignores DECRQM entirely leaves
    ///   `sync_output` at its harmless default-true (terminals that don't
    ///   support 2026 ignore the bracket anyway — this is log-honesty).
    pub fn merge_burst(&mut self, replies: &BurstReplies) {
        if !replies.da1 {
            return;
        }
        self.da1_answered = true;
        self.kitty_keyboard = replies.kitty;
        if let Some(v) = replies.mode_2026 {
            self.sync_output = decrpm_supported(v);
        }
        if let Some(v) = replies.mode_2027 {
            self.mode_2027 = decrpm_supported(v);
        }
    }
}

/// Emit the query burst and read the DA1-fenced replies, merging facts onto
/// `caps`. **Placement contract:** call ONLY from `run_setup()`, after
/// raw-mode enable, before `EventStream::new()` — see the module docs
/// ("single-consumer rule"). Every failure mode (not a tty, reactor error,
/// timeout, partial replies, EOF) returns `caps` unchanged: boot never hangs
/// and never changes behavior versus today.
pub(crate) async fn negotiate(caps: TermCaps, timeout: std::time::Duration) -> TermCaps {
    #[cfg(unix)]
    {
        negotiate_unix(caps, timeout).await
    }
    #[cfg(not(unix))]
    {
        let _ = timeout;
        caps
    }
}

#[cfg(unix)]
async fn negotiate_unix(mut caps: TermCaps, timeout: std::time::Duration) -> TermCaps {
    use crossterm::tty::IsTty;
    use std::io::Write;

    // Not a real terminal (pipe, CI, tests): no one will answer — don't emit
    // queries and don't pay the timeout.
    if !std::io::stdin().is_tty() {
        return caps;
    }

    // Emit the burst as one flushed write. Failure ⇒ keep env caps.
    {
        let mut out = std::io::stdout().lock();
        if write_query_burst(&mut out).is_err() {
            return caps;
        }
        let _ = out.flush();
    }

    let deadline = std::time::Instant::now() + timeout;
    let replies = read_replies_fd0(deadline).await;
    caps.merge_burst(&replies); // no-op unless DA1-fenced
    tracing::debug!(
        fenced = replies.da1,
        ?replies,
        "P16.2 query burst complete (timeout {:?})",
        timeout
    );
    caps
}

/// Bounded raw read of fd 0 until the DA1 fence or `deadline`.
///
/// Uses the tokio reactor (`AsyncFd`) for readiness so every wait is
/// deadline-bounded — no blocking reader thread exists to orphan (see module
/// docs). Reads go straight to fd 0 via [`stdin_raw::read_fd0`], bypassing
/// `io::Stdin`'s BufReader, so we never buffer bytes away from the future
/// `EventStream`. Readiness is cleared after every read; if reply bytes are
/// split across chunks and no further edge arrives, the deadline fires and
/// the unfenced partial is discarded by `merge_burst` — safe fallback, never
/// a hang.
#[cfg(unix)]
async fn read_replies_fd0(deadline: std::time::Instant) -> BurstReplies {
    let afd = match tokio::io::unix::AsyncFd::with_interest(
        stdin_raw::StdinFd,
        tokio::io::Interest::READABLE,
    ) {
        Ok(afd) => afd,
        Err(e) => {
            tracing::debug!("P16.2: could not register fd 0 with reactor: {e}");
            return BurstReplies::default();
        }
    };
    let mut acc: Vec<u8> = Vec::with_capacity(256);
    loop {
        let now = std::time::Instant::now();
        if now >= deadline {
            return parse_burst_replies(&acc);
        }
        let mut guard = match tokio::time::timeout(deadline - now, afd.readable()).await {
            Ok(Ok(guard)) => guard,
            Ok(Err(_)) | Err(_) => return parse_burst_replies(&acc), // reactor err / deadline
        };
        let mut chunk = [0u8; 512];
        match stdin_raw::read_fd0(&mut chunk) {
            Ok(0) => return parse_burst_replies(&acc), // EOF
            Ok(n) => {
                acc.extend_from_slice(&chunk[..n]);
                let replies = parse_burst_replies(&acc);
                if replies.da1 {
                    return replies; // fence — done, fd 0 is released
                }
                // Mandatory: without this, the next `readable()` returns
                // immediately and a second read on the (blocking) fd with no
                // data would hang boot. Clearing waits for the next edge.
                guard.clear_ready();
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => guard.clear_ready(),
            Err(_) => return parse_burst_replies(&acc),
        }
    }
}

/// Raw, unbuffered fd-0 access for the burst read.
#[cfg(unix)]
mod stdin_raw {
    use std::fs::File;
    use std::io::{self, Read};
    use std::mem::ManuallyDrop;
    use std::os::fd::{AsRawFd, RawFd};

    /// Registration token handing fd 0 to `AsyncFd` (readiness only — the
    /// reactor never reads the fd itself).
    pub(super) struct StdinFd;
    impl AsRawFd for StdinFd {
        fn as_raw_fd(&self) -> RawFd {
            0
        }
    }

    /// One `read(2)` straight off fd 0. `ManuallyDrop` prevents the borrowed
    /// `File` from closing fd 0. Bypasses `io::Stdin`'s BufReader on purpose:
    /// a buffered read could pull user keystrokes into a userspace buffer the
    /// later `EventStream` (which reads the fd directly) would never see.
    /// Only called after the reactor reported fd 0 readable, while this burst
    /// is the process's sole stdin consumer (EventStream not yet created), so
    /// the bytes that raised POLLIN are still there and the read returns
    /// without blocking.
    pub(super) fn read_fd0(buf: &mut [u8]) -> io::Result<usize> {
        use std::os::fd::FromRawFd;
        let mut f = ManuallyDrop::new(unsafe { File::from_raw_fd(0) });
        f.read(buf)
    }
}

#[cfg(test)]
mod burst_tests {
    use super::*;

    #[test]
    fn burst_bytes_contain_every_query_with_da1_fence_last() {
        let mut sink: Vec<u8> = Vec::new();
        write_query_burst(&mut sink).unwrap();
        assert_eq!(sink.as_slice(), QUERY_BURST);
        let s = String::from_utf8(sink).unwrap();
        assert!(s.contains("\x1b[>0q"), "XTVERSION query missing");
        assert!(s.contains("\x1b[?2026$p"), "DECRQM 2026 query missing");
        assert!(s.contains("\x1b[?2027$p"), "DECRQM 2027 query missing");
        assert!(s.contains("\x1b[?u"), "kitty keyboard query missing");
        assert!(s.contains("\x1b[>c"), "DA2 query missing");
        assert!(s.ends_with("\x1b[c"), "DA1 must be the LAST query (the fence)");
        assert_eq!(s.matches("\x1b[c").count(), 1, "exactly one DA1 query");
    }

    #[test]
    fn full_reply_stream_parses_and_merges() {
        // XTVERSION DCS, kitty flags, DECRPM 2026=2 (reset/settable),
        // DECRPM 2027=0 (not recognized), DA2, DA1 fence.
        let bytes = b"\x1bP>|kitty(0.32.2)\x1b\\\x1b[?1u\x1b[?2026;2$y\x1b[?2027;0$y\x1b[>1;4000;13c\x1b[?62;22c";
        let r = parse_burst_replies(bytes);
        assert!(r.da1 && r.kitty);
        assert_eq!(r.mode_2026, Some(2));
        assert_eq!(r.mode_2027, Some(0));
        let mut caps = TermCaps::default();
        caps.merge_burst(&r);
        assert!(caps.da1_answered);
        assert!(caps.kitty_keyboard);
        assert!(caps.sync_output, "DECRPM 2 = reset-but-settable = supported");
        assert!(!caps.mode_2027, "DECRPM 0 = not recognized");
    }

    #[test]
    fn unfenced_partial_replies_are_discarded() {
        // Everything answered EXCEPT the DA1 fence (e.g. deadline fired).
        let r = parse_burst_replies(b"\x1b[?1u\x1b[?2026;1$y\x1b[?2027;1$y");
        assert!(!r.da1);
        let mut caps = TermCaps::default();
        let before = caps.clone();
        caps.merge_burst(&r);
        assert_eq!(caps, before, "no DA1 fence ⇒ merge must be a no-op");
    }

    #[test]
    fn da1_fence_without_kitty_reply_turns_kitty_off() {
        // Terminal answers DA1 but ignored the kitty query (in-band ordering
        // means "no reply by fence time" == "unsupported").
        let mut caps = TermCaps::default();
        caps.merge_burst(&parse_burst_replies(b"\x1b[?62;4c"));
        assert!(caps.da1_answered);
        assert!(!caps.kitty_keyboard);
        assert!(caps.sync_output, "no DECRQM answer ⇒ keep harmless default-on");
    }

    #[test]
    fn interleaved_keystroke_bytes_and_unknown_sequences_are_skipped() {
        // A racing keystroke ('q'), an unknown CSI, then real replies.
        let bytes = b"q\x1b[38;2;1;2;3m\x1b[?2026;1$y\x1b[?0u\x1b[?6c";
        let r = parse_burst_replies(bytes);
        assert!(r.da1 && r.kitty);
        assert_eq!(r.mode_2026, Some(1));
    }

    #[test]
    fn truncated_trailing_sequence_is_tolerated() {
        let r = parse_burst_replies(b"\x1b[?1u\x1b[?2026"); // cut mid-CSI
        assert!(r.kitty);
        assert!(!r.da1);
        assert_eq!(r.mode_2026, None);
        // Truncated DCS likewise.
        let r = parse_burst_replies(b"\x1bP>|WezTerm 2024");
        assert_eq!(r, BurstReplies::default());
    }
}
