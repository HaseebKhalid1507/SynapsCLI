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
//! ## What this does NOT do (yet)
//!
//! No terminal queries. We do **not** send DA1 / XTVERSION / mode-2027 /
//! kitty-query bursts here — that is P16.2, and it must be DA1-fenced to avoid
//! the crossterm query-race (issues #963 / #993, see the substrate memo).
//! Detection here is **env-only**: `$TERM_PROGRAM`, `$TMUX` presence, and the
//! `tmux -V` provenance string. Env detection can't hang and can't race, so it
//! is safe to run unconditionally at boot.

/// Facts we know (or conservatively assume) about the attached terminal.
///
/// Construct via [`TermCaps::detect`] (reads the real process environment) or
/// [`TermCaps::detect_from_env`] (injected env — used by tests). A bare
/// [`TermCaps::default`] is the "we know nothing, behave exactly as today"
/// baseline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TermCaps {
    /// DEC 2026 synchronized output (`BeginSynchronizedUpdate` /
    /// `EndSynchronizedUpdate`).
    ///
    /// **Default `true` = current behavior.** `draw.rs:730/745` brackets every
    /// frame in Begin/EndSynchronizedUpdate *unconditionally* today. Terminals
    /// that don't support 2026 ignore the escape, so emitting it is harmless.
    /// Keeping the default `true` means a future `if caps.sync_output` gate is a
    /// no-op that still emits the bracket.
    pub(crate) sync_output: bool,

    /// Kitty keyboard protocol (disambiguate-escape / report-alternate-keys)
    /// enhancement flags.
    ///
    /// **Default `true` = current behavior.** `lifecycle.rs` blind-pushes
    /// `PushKeyboardEnhancementFlags(...)` best-effort at setup and swallows the
    /// error; terminals that don't support it ignore the sequence. Default
    /// `true` means a future `if caps.kitty_keyboard` gate still performs the
    /// push (the blind push becomes the timeout fallback in P16.3).
    pub(crate) kitty_keyboard: bool,

    /// Unicode / grapheme width mode 2027.
    ///
    /// **Default `false` = current behavior.** We do no width negotiation today;
    /// width is handled unconditionally by the existing metrics path. `false`
    /// means "not negotiated," so a future `if caps.mode_2027` gate stays off
    /// and leaves today's width behavior untouched.
    pub(crate) mode_2027: bool,

    /// Value of `$TERM_PROGRAM` (e.g. `"iTerm.app"`, `"WezTerm"`, `"tmux"`,
    /// `"vscode"`), if present and non-empty. `None` = unset.
    ///
    /// **Default `None`** — informational provenance only; nothing gates on it
    /// today.
    pub(crate) term_program: Option<String>,

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
    pub(crate) tmux: Option<String>,

    /// Whether a DA1 (Primary Device Attributes) reply has been observed.
    ///
    /// **Default `false` = current behavior.** We never query the terminal
    /// today, so DA1 is never answered. `false` is the "unknown / no query
    /// performed" state that all P16.3 fallbacks key off of (unknown ⇒ do the
    /// unconditional thing). This flips to `true` only in P16.2 once the
    /// DA1-fenced boot burst lands.
    pub(crate) da1_answered: bool,
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
