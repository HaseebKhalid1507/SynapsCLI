//! MXC (Myx Color Protocol v1) subscriber — the live layer behind the
//! "myx" builtin theme.
//!
//! Myx (the music player) derives a 16-token semantic palette from album art
//! on every track change and publishes it as NDJSON over a Unix socket
//! (`$XDG_RUNTIME_DIR/myx/theme.sock`). While the "myx" theme is active,
//! [`run_subscriber`] holds that socket open and forwards each palette —
//! already mapped onto a full [`Theme`] — through an mpsc channel to the main
//! loop, which applies it via the exact same `set_theme` + `invalidate` path
//! the `/theme` command uses. The subscriber task NEVER mutates theme state
//! itself.
//!
//! Protocol facts this module relies on (MXC spec v0.1.0):
//! - `AF_UNIX`/`SOCK_STREAM`, one compact JSON object per `\n`-terminated line.
//! - Snapshot-on-connect: the first line is always a complete `theme` message.
//! - Full state, always: every `theme` message carries all 16 tokens, so a
//!   reconnect (or an unexpected `seq` restart) needs no resume logic — the
//!   next message is the whole truth. The subscriber is deliberately
//!   stateless about `seq`.
//! - Unknown `t` values and unknown fields are skipped, never fatal.
//!
//! Resilience contract (the subscriber must never hurt synaps):
//! - Socket absent → static palette stands, quiet retry with capped backoff.
//! - Disconnect / EOF / `bye reason:"reload"` → KEEP the last-good palette,
//!   resume retrying (a publisher restart re-snapshots on reconnect).
//! - `bye reason:"shutdown"` → revert to the static myx default. Spec §3.4
//!   SHOULD; deliberately split by reason: keep-last-good across a reload
//!   avoids a revert-then-restore flicker, but after a real shutdown Myx
//!   may be gone for days and yesterday's album colors must not persist.
//!   Unknown/absent reasons keep last-good (same posture as plain EOF).
//! - Malformed JSON line → skip it, keep reading.
//! - Non-UTF-8 bytes or a line exceeding [`MAX_LINE_BYTES`] → drop the
//!   CONNECTION (not just the line) and reconnect on backoff; the next
//!   snapshot-on-connect restores state. The length cap bounds heap growth
//!   against a wedged or hostile publisher; a spec-valid frame is < ~1 KB.
//! - Protocol version newer than ours → drop the connection (we cannot trust
//!   the payload) and retry on backoff; a downgraded Myx heals it. The skew
//!   warning fires once per subscriber generation, not once per reconnect.
//! - Backoff resets on the first VALID theme line, not on connect success —
//!   an accept-then-die endpoint must not pin retries at 1/s forever.
//! - The socket's parent directory must exist and be owned by our uid
//!   before we connect (squat defense for the world-writable `/tmp`
//!   fallback; `$XDG_RUNTIME_DIR` passes the same check trivially).

use std::path::PathBuf;
use std::time::Duration;

use ratatui::style::Color;

use super::Theme;

/// An sRGB triple as decoded off the wire. Kept as bare bytes (not
/// [`Color`]) so the default palette can be a `const` and so the mapping
/// layer owns all `Color` construction.
pub(crate) type Rgb8 = (u8, u8, u8);

/// The 16 MXC palette tokens, decoded. Field names match the wire keys 1:1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MxcColors {
    // Roles
    pub(crate) primary: Rgb8,
    pub(crate) secondary: Rgb8,
    pub(crate) accent: Rgb8,
    // Status
    pub(crate) error: Rgb8,
    pub(crate) warning: Rgb8,
    pub(crate) success: Rgb8,
    pub(crate) info: Rgb8,
    // Text
    pub(crate) text: Rgb8,
    pub(crate) text_muted: Rgb8,
    // Surfaces — three elevation layers, background < panel < element.
    pub(crate) background: Rgb8,
    pub(crate) background_panel: Rgb8,
    pub(crate) background_element: Rgb8,
    // Borders — four hierarchy shades.
    pub(crate) border: Rgb8,
    pub(crate) border_active: Rgb8,
    pub(crate) border_subtle: Rgb8,
    pub(crate) border_dimmest: Rgb8,
}

/// The outcome of parsing one NDJSON line off the socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MxcLine {
    /// A complete `theme` message: apply this palette. `fade_ms` is the
    /// publisher's advisory cross-fade duration (spec §3.3): `Some(0)` means
    /// "snap, no transition intended"; `None` (absent) leaves the choice to
    /// the consumer (we fall back to the default transition duration).
    Theme {
        colors: MxcColors,
        fade_ms: Option<u64>,
    },
    /// Clean publisher goodbye. `revert` is the §3.4 ruling: `true` for
    /// `reason:"shutdown"` (revert to the static myx default — the
    /// publisher is gone for good), `false` for `"reload"` or any
    /// unknown/absent reason (keep last-good, same posture as EOF).
    Bye { revert: bool },
    /// Blank, malformed, unknown tag, or unusable payload — skip, keep reading.
    Skip,
    /// The publisher speaks a newer protocol major than we do. The payload
    /// can no longer be trusted; drop the connection rather than guess.
    VersionSkew,
}

/// Highest MXC protocol major version this subscriber understands.
const PROTOCOL_VERSION: u64 = 1;

/// First reconnect delay. One second: a Myx start is picked up promptly
/// without ever busy-looping while Myx is not running.
const BACKOFF_START: Duration = Duration::from_secs(1);

/// Reconnect delay ceiling. Bounds idle cost when Myx is simply not
/// installed — one failed `connect()` every 30s is free.
const BACKOFF_CAP: Duration = Duration::from_secs(30);

/// Hard per-line byte budget (joestar #1). A spec-valid `theme` frame is
/// < ~1 KB; a publisher streaming an unterminated line would otherwise grow
/// the TUI's heap without bound. Exceeding the cap drops the connection and
/// reuses the normal backoff/reconnect path.
const MAX_LINE_BYTES: usize = 64 * 1024;

// ---------------------------------------------------------------------------
// Socket path
// ---------------------------------------------------------------------------

/// The MXC socket path: `$XDG_RUNTIME_DIR/myx/theme.sock`, falling back to
/// `/tmp/myx-$UID/theme.sock` when `XDG_RUNTIME_DIR` is unset (spec §2.1).
pub(crate) fn socket_path() -> PathBuf {
    socket_path_in(std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from))
}

/// Pure core of [`socket_path`], testable without touching process env.
fn socket_path_in(runtime_dir: Option<PathBuf>) -> PathBuf {
    // XDG basedir spec: a set-but-EMPTY or relative XDG_RUNTIME_DIR must be
    // treated as unset (joestar #3) — otherwise "" yields the CWD-relative
    // path `myx/theme.sock` and silently skips the uid-scoped fallback.
    let dir = runtime_dir
        .filter(|d| d.is_absolute())
        .unwrap_or_else(|| PathBuf::from(format!("/tmp/myx-{}", uid())));
    dir.join("myx").join("theme.sock")
}

/// Trust classification of the socket's parent directory, checked before
/// every connect attempt (joestar #2). `/tmp` is world-writable: another
/// local user can pre-create `/tmp/myx-<uid>/` and squat the socket to feed
/// us palettes (and oversized lines). Refuse to connect unless the directory
/// exists and is owned by our uid. `$XDG_RUNTIME_DIR/myx` passes trivially
/// (the runtime dir is per-user by contract).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SocketDirTrust {
    /// Directory absent: Myx not installed / not yet run. Quiet retry.
    Missing,
    /// Exists and owned by us — safe to connect.
    Trusted,
    /// Exists but NOT ours (or not a directory): likely a squat. Never
    /// connect; warn once.
    Untrusted,
}

#[cfg(unix)]
fn socket_dir_trust(sock: &std::path::Path) -> SocketDirTrust {
    use std::os::unix::fs::MetadataExt;
    let Some(dir) = sock.parent() else {
        return SocketDirTrust::Untrusted;
    };
    match std::fs::metadata(dir) {
        Err(_) => SocketDirTrust::Missing,
        Ok(m) if m.is_dir() && m.uid() == uid() => SocketDirTrust::Trusted,
        Ok(_) => SocketDirTrust::Untrusted,
    }
}

/// Current uid without a libc dependency: owner of `/proc/self`, falling
/// back to the owner of `$HOME`. Only reached when `XDG_RUNTIME_DIR` is
/// unset, which is already an unusual session.
#[cfg(unix)]
fn uid() -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self")
        .or_else(|_| std::fs::metadata(std::env::var_os("HOME").unwrap_or_else(|| "/".into())))
        .map(|m| m.uid())
        .unwrap_or(0)
}

/// Windows: Myx (a Linux compositor companion) is never present — treat the
/// socket dir as missing so the client loop idles quietly.
#[cfg(windows)]
fn socket_dir_trust(_sock: &std::path::Path) -> SocketDirTrust {
    SocketDirTrust::Missing
}

/// Soft detection for the theme listing: is Myx plausibly present? True when
/// the MXC socket exists (Myx running or recently crashed) or a `myx` binary
/// is on `$PATH`. Purely cosmetic — the theme works either way.
pub(crate) fn myx_detected() -> bool {
    if socket_path().exists() {
        return true;
    }
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join("myx").is_file()))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Wire parsing
// ---------------------------------------------------------------------------

/// Strict wire-format hex: `#rrggbb`, exactly. The theme-file loader's
/// forgiving parser (`#rgb`, missing `#`) is right for a config typo but
/// wrong for a protocol — here garbage is a reject, not a coercion.
pub(crate) fn parse_hex(s: &str) -> Option<Rgb8> {
    let body = s.strip_prefix('#')?;
    if body.len() != 6 || !body.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let n = u32::from_str_radix(body, 16).ok()?;
    Some((
        ((n >> 16) & 0xff) as u8,
        ((n >> 8) & 0xff) as u8,
        (n & 0xff) as u8,
    ))
}

/// Classify and decode one NDJSON line (spec §3, §5.3).
///
/// Never returns an error: every failure mode maps to [`MxcLine::Skip`]
/// except version skew, which poisons the connection by design.
pub(crate) fn parse_line(line: &str) -> MxcLine {
    let raw = line.trim();
    if raw.is_empty() {
        return MxcLine::Skip;
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) else {
        return MxcLine::Skip; // not even JSON — a truncated write, perhaps.
    };
    let tag = v.get("t").and_then(|t| t.as_str());
    // Version discipline (spec §3.2 marks `v` REQUIRED): absent → treat as
    // 0 and accept (Postel — a missing field is sloppy, not hostile). But a
    // PRESENT non-u64 value (float, negative, 1e300, string) is NOT a
    // version we can compare — that is skew, never "version 0" (joestar #5).
    let version = match v.get("v") {
        None => 0,
        Some(n) => n.as_u64().unwrap_or(u64::MAX),
    };
    match tag {
        Some("theme") | Some("bye") if version > PROTOCOL_VERSION => MxcLine::VersionSkew,
        Some("theme") => match colors_from_json(v.get("colors")) {
            Some(colors) => MxcLine::Theme {
                colors,
                // Advisory cross-fade duration; clamped later at the apply
                // site. A non-integer value is treated as absent, not fatal.
                fade_ms: v.get("fade_ms").and_then(|n| n.as_u64()),
            },
            None => MxcLine::Skip, // known tag, unusable payload: publisher bug.
        },
        Some("bye") => MxcLine::Bye {
            // §3.4 ruling (yoru F3): only an explicit "shutdown" reverts to
            // the static default; "reload" and unknown/absent reasons keep
            // last-good (identical to a plain EOF).
            revert: v.get("reason").and_then(|r| r.as_str()) == Some("shutdown"),
        },
        _ => MxcLine::Skip, // unknown `t` (or none): invented after we shipped.
    }
}

/// Decode the `colors` object. All 16 tokens are REQUIRED (spec §3.3);
/// any missing or malformed token rejects the whole message.
fn colors_from_json(colors: Option<&serde_json::Value>) -> Option<MxcColors> {
    let obj = colors?.as_object()?;
    let tok = |key: &str| -> Option<Rgb8> { parse_hex(obj.get(key)?.as_str()?) };
    Some(MxcColors {
        primary: tok("primary")?,
        secondary: tok("secondary")?,
        accent: tok("accent")?,
        error: tok("error")?,
        warning: tok("warning")?,
        success: tok("success")?,
        info: tok("info")?,
        text: tok("text")?,
        text_muted: tok("text_muted")?,
        background: tok("background")?,
        background_panel: tok("background_panel")?,
        background_element: tok("background_element")?,
        border: tok("border")?,
        border_active: tok("border_active")?,
        border_subtle: tok("border_subtle")?,
        border_dimmest: tok("border_dimmest")?,
    })
}

// ---------------------------------------------------------------------------
// MXC → Theme mapping
// ---------------------------------------------------------------------------

fn c(rgb: Rgb8) -> Color {
    Color::Rgb(rgb.0, rgb.1, rgb.2)
}

/// Map the 16 MXC tokens onto the synaps [`Theme`].
///
/// This single function is BOTH the live path (every socket message) and the
/// static path (the builtin "myx" palette is this function applied to Myx's
/// default tokyonight tokens), so the two can never drift.
///
/// ## Mapping table
///
/// | MXC token            | synaps `Theme` fields                                              |
/// |----------------------|--------------------------------------------------------------------|
/// | `background`         | `message_bg` — the transcript canvas IS Myx's background           |
/// | `background_panel`   | `bg` (chrome: header/footer), `user_bg`, `code_bg`, `tool_input_bg` — chrome and user turns sit on Myx's library-panel surface |
/// | `background_element` | `tool_output_bg`                                                   |
/// | `text`               | `input_fg`, `claude_text`, `code_fg`, `table_cell_color`, `event_text` |
/// | `text_muted`         | `muted`, `thinking_color`, `quote_color`, `help_fg`, `header_fg`, `tool_param`, `subagent_status`, `subagent_time` |
/// | `primary`            | `claude_label`, `heading_color`, `table_header_color`, `prompt_fg`, `event_source` |
/// | `secondary`          | `subagent_name`, `list_bullet_color`                               |
/// | `accent`             | `user_color` (the user pill), `cost_color`, `event_icon`           |
/// | `error`              | `error_color`, `event_critical`                                    |
/// | `warning`            | `warning_color`, `status_streaming`                                |
/// | `success`            | `status_ready`, `tool_result_ok`, `subagent_done`                  |
/// | `info`               | `tool_label`, `tool_result_color`                                  |
/// | `border`             | `border`, `subagent_border`                                        |
/// | `border_active`      | `border_active`                                                    |
/// | `border_subtle`      | `table_border_color`                                               |
/// | `border_dimmest`     | `separator`                                                        |
///
/// Tool accent colors stay `Color::Reset` (auto-derived from the palette, as
/// every non-night-city builtin does), and per-part overrides stay `None`.
///
/// ## Trust boundary — deliberate deviation from MXC spec §7.2
///
/// The spec says the adapter CLAMPS wire colors (AA-contrast fallbacks for
/// status colors, canvas-invariant enforcement) rather than trusting the
/// wire. This adapter imports all 16 tokens VERBATIM, on purpose: Myx is a
/// trusted, same-uid, local publisher whose own derivation pipeline already
/// orders the surfaces and tunes contrast, and the subscriber refuses to
/// connect through a socket directory it doesn't own. Consequence to keep
/// in mind: the repo's palette invariants (canvas/chrome band, saturation
/// gates) are only ever TESTED against the static [`super::palettes::myx`]
/// snapshot — live palettes bypass them by design. Clamping is deferred
/// until a real-world palette proves illegible; if it lands, write its
/// tests against hostile palettes (all-white, all-grey album art), not the
/// defaults.
pub(crate) fn theme_from_mxc(x: &MxcColors) -> Theme {
    Theme {
        // Markdown
        code_fg: c(x.text),
        code_bg: c(x.background_panel),
        heading_color: c(x.primary),
        quote_color: c(x.text_muted),
        list_bullet_color: c(x.secondary),
        table_border_color: c(x.border_subtle),
        table_header_color: c(x.primary),
        table_cell_color: c(x.text),

        // Base
        bg: c(x.background_panel),
        message_bg: c(x.background),
        border: c(x.border),
        border_active: c(x.border_active),
        muted: c(x.text_muted),

        // Messages
        user_color: c(x.accent),
        user_bg: c(x.background_panel),
        claude_label: c(x.primary),
        claude_text: c(x.text),
        thinking_color: c(x.text_muted),
        tool_label: c(x.info),
        tool_param: c(x.text_muted),
        tool_result_color: c(x.info),
        tool_result_ok: c(x.success),
        error_color: c(x.error),
        warning_color: c(x.warning),

        // UI chrome
        header_fg: c(x.text_muted),
        status_streaming: c(x.warning),
        status_ready: c(x.success),
        help_fg: c(x.text_muted),
        input_fg: c(x.text),
        prompt_fg: c(x.primary),
        separator: c(x.border_dimmest),
        cost_color: c(x.accent),

        // Subagent panel
        subagent_border: c(x.border),
        subagent_name: c(x.secondary),
        subagent_status: c(x.text_muted),
        subagent_done: c(x.success),
        subagent_time: c(x.text_muted),

        // Event bus
        event_icon: c(x.accent),
        event_source: c(x.primary),
        event_text: c(x.text),
        event_critical: c(x.error),

        // Panel backgrounds from the two elevated MXC surfaces; tool accents
        // and per-part overrides keep their defaults (Reset / None).
        tool_input_bg: c(x.background_panel),
        tool_output_bg: c(x.background_element),
        ..Theme::default()
    }
}

// ---------------------------------------------------------------------------
// The subscriber task
// ---------------------------------------------------------------------------

/// Connect to the MXC socket and forward mapped palettes to the UI, forever.
///
/// Spawned when the "myx" theme becomes active; aborted (via its
/// `JoinHandle`) when the theme switches away or the app shuts down. All it
/// ever does outward is `tx.send` — the receiving arm in the main loop is
/// the only place theme state changes (and it re-checks that myx is still
/// active, because queued messages survive an abort).
///
/// Lifecycle:
/// - socket dir missing → Myx not around: sleep on capped backoff, retry.
/// - socket dir exists but isn't ours → possible squat: NEVER connect
///   (warn once).
/// - connected → [`run_session`] until EOF/bye/skew/error, then back off.
/// - backoff resets on the first VALID theme line of a session (not on
///   connect success), so an accept-then-die endpoint climbs to the 30s
///   cap like any other failure.
pub(crate) async fn run_subscriber(tx: tokio::sync::mpsc::UnboundedSender<(Theme, Option<u64>)>) {
    let path = socket_path();
    let mut backoff = BACKOFF_START;
    let mut skew_warned = false;
    let mut squat_warned = false;
    loop {
        match socket_dir_trust(&path) {
            SocketDirTrust::Missing => {} // Myx not installed / not run yet.
            SocketDirTrust::Untrusted => {
                if !squat_warned {
                    squat_warned = true;
                    tracing::warn!(
                        "MXC socket directory {:?} exists but is not owned by this uid; \
                         refusing to connect (possible squat)",
                        path.parent()
                    );
                }
            }
            #[cfg(unix)]
            SocketDirTrust::Trusted => {
                if let Ok(stream) = tokio::net::UnixStream::connect(&path).await {
                    match run_session(stream, &tx, &mut backoff, &mut skew_warned).await {
                        SessionEnd::Teardown => return,
                        SessionEnd::Disconnect => {}
                    }
                }
            }
            #[cfg(windows)]
            SocketDirTrust::Trusted => unreachable!("socket_dir_trust never returns Trusted on Windows"),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(BACKOFF_CAP);
    }
}

/// Why one connected session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionEnd {
    /// The channel receiver is gone — the app is tearing down; exit the task.
    Teardown,
    /// EOF / bye / skew / oversized or non-UTF-8 line / io error — drop the
    /// connection and let the reconnect loop take over.
    Disconnect,
}

/// One connected session: read capped lines, forward palettes, honor `bye`.
///
/// Generic over the reader so tests drive it with in-memory bytes — no
/// socket, no Myx. Framing uses buffered line reads (never chunk-reads), so
/// a message split across socket writes still parses (spec §5.1).
///
/// Side effects on the shared reconnect state:
/// - first VALID theme line → `backoff` resets to [`BACKOFF_START`] and the
///   version-skew warn latch re-arms (a healthy publisher was seen).
async fn run_session<R>(
    stream: R,
    tx: &tokio::sync::mpsc::UnboundedSender<(Theme, Option<u64>)>,
    backoff: &mut Duration,
    skew_warned: &mut bool,
) -> SessionEnd
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut reader = tokio::io::BufReader::new(stream);
    let mut buf = Vec::new();
    let mut got_valid_line = false;
    loop {
        let line = match read_line_capped(&mut reader, &mut buf).await {
            Ok(Some(line)) => line,
            // EOF, oversized line, non-UTF-8, or socket error: the module
            // contract drops the connection for all of these — the next
            // snapshot-on-connect restores state.
            Ok(None) | Err(_) => return SessionEnd::Disconnect,
        };
        match parse_line(&line) {
            MxcLine::Theme { colors, fade_ms } => {
                if !got_valid_line {
                    got_valid_line = true;
                    // Reset on the first VALID line, not on connect success:
                    // an endpoint that accepts and dies must not pin the
                    // retry rate at 1/s forever (okarin F5).
                    *backoff = BACKOFF_START;
                    *skew_warned = false;
                }
                if tx.send((theme_from_mxc(&colors), fade_ms)).is_err() {
                    return SessionEnd::Teardown; // receiver dropped — teardown.
                }
            }
            MxcLine::Skip => {}
            MxcLine::Bye { revert } => {
                if revert {
                    // reason:"shutdown" — Myx is gone, possibly for days;
                    // yesterday's album colors must not persist. Revert to
                    // the static myx default through the normal apply path
                    // (fade per the configured knob).
                    if tx.send((Theme::myx(), None)).is_err() {
                        return SessionEnd::Teardown;
                    }
                }
                return SessionEnd::Disconnect; // reload/unknown: keep last-good.
            }
            MxcLine::VersionSkew => {
                if !*skew_warned {
                    *skew_warned = true;
                    tracing::warn!(
                        "MXC publisher speaks a newer protocol than v{PROTOCOL_VERSION}; \
                         holding last-good palette"
                    );
                }
                return SessionEnd::Disconnect;
            }
        }
    }
}

/// Read one `\n`-terminated line with a hard [`MAX_LINE_BYTES`] budget.
///
/// - `Ok(Some(line))` — a complete line (without the `\n`).
/// - `Ok(None)` — EOF. A trailing unterminated fragment is discarded: the
///   connection died mid-write, and the frame is by definition incomplete.
/// - `Err(_)` — over-budget line or non-UTF-8 bytes (both drop the
///   connection per the module contract) or an underlying io error.
///
/// `tokio`'s own `Lines`/`read_until` accumulate without any maximum, which
/// is a heap-growth DoS from a wedged or hostile publisher (joestar #1).
async fn read_line_capped<R>(reader: &mut R, buf: &mut Vec<u8>) -> std::io::Result<Option<String>>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    use tokio::io::AsyncBufReadExt;
    buf.clear();
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            return Ok(None); // EOF (any partial fragment in `buf` is moot).
        }
        let newline = chunk.iter().position(|&b| b == b'\n');
        let take = newline.unwrap_or(chunk.len());
        if buf.len() + take > MAX_LINE_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "MXC line exceeds the 64 KiB budget",
            ));
        }
        buf.extend_from_slice(&chunk[..take]);
        match newline {
            Some(pos) => {
                reader.consume(pos + 1);
                return String::from_utf8(std::mem::take(buf))
                    .map(Some)
                    .map_err(|_| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "MXC line is not valid UTF-8",
                        )
                    });
            }
            None => reader.consume(take),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests — deterministic, hermetic, no live Myx anywhere.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixture with 16 distinct sentinel values so the mapping test can
    /// prove every token lands exactly where the table says.
    fn sentinels() -> MxcColors {
        MxcColors {
            primary: (1, 1, 1),
            secondary: (2, 2, 2),
            accent: (3, 3, 3),
            error: (4, 4, 4),
            warning: (5, 5, 5),
            success: (6, 6, 6),
            info: (7, 7, 7),
            text: (8, 8, 8),
            text_muted: (9, 9, 9),
            background: (10, 10, 10),
            background_panel: (11, 11, 11),
            background_element: (12, 12, 12),
            border: (13, 13, 13),
            border_active: (14, 14, 14),
            border_subtle: (15, 15, 15),
            border_dimmest: (16, 16, 16),
        }
    }

    /// A spec-valid `theme` line (the §3.3 example, abridged metadata).
    fn theme_line() -> String {
        r##"{"t":"theme","v":1,"seq":0,"ts":1785616484123,
            "origin":{"kind":"album_art","name":"Blue Monday"},
            "fade_ms":600,"is_dark":true,
            "colors":{
              "primary":"#64e0d0","secondary":"#4a9fd8","accent":"#f4aa48",
              "error":"#e05561","warning":"#d9a441","success":"#61c766","info":"#64e0d0",
              "text":"#d8efff","text_muted":"#7a90a4",
              "background":"#081018","background_panel":"#101d2a","background_element":"#18293a",
              "border":"#22374a","border_active":"#42d9d0","border_subtle":"#182838","border_dimmest":"#101c28"},
            "contrast":{"on_primary":"#0b0b0b","on_secondary":"#0b0b0b",
                        "on_accent":"#0b0b0b","on_background":"#d8efff"}}"##
            .replace('\n', "")
            .replace("            ", "")
    }

    // ---- hex ----

    #[test]
    fn parse_hex_accepts_lowercase_rrggbb() {
        assert_eq!(parse_hex("#64e0d0"), Some((0x64, 0xe0, 0xd0)));
        assert_eq!(parse_hex("#000000"), Some((0, 0, 0)));
        assert_eq!(parse_hex("#ffffff"), Some((0xff, 0xff, 0xff)));
        // Uppercase hex digits are still hex digits.
        assert_eq!(parse_hex("#ABCDEF"), Some((0xab, 0xcd, 0xef)));
    }

    #[test]
    fn parse_hex_rejects_garbage_instead_of_coercing() {
        for bad in ["64e0d0", "#64e0d", "#64e0d0ff", "#gggggg", "", "#", "#fff"] {
            assert_eq!(parse_hex(bad), None, "{bad:?} must be rejected");
        }
    }

    // ---- NDJSON frames ----

    #[test]
    fn valid_theme_frame_parses_with_all_tokens() {
        let MxcLine::Theme { colors: x, fade_ms } = parse_line(&theme_line()) else {
            panic!("theme frame must parse");
        };
        assert_eq!(x.primary, (0x64, 0xe0, 0xd0));
        assert_eq!(x.background, (0x08, 0x10, 0x18));
        assert_eq!(x.border_dimmest, (0x10, 0x1c, 0x28));
        assert_eq!(x.text_muted, (0x7a, 0x90, 0xa4));
        // fade_ms rides along verbatim — clamping happens at the apply site.
        assert_eq!(fade_ms, Some(600));
    }

    #[test]
    fn fade_ms_absent_parses_as_none() {
        let line = theme_line().replace("\"fade_ms\":600,", "");
        let MxcLine::Theme { fade_ms, .. } = parse_line(&line) else {
            panic!("theme frame without fade_ms must still parse");
        };
        assert_eq!(fade_ms, None);
    }

    #[test]
    fn fade_ms_zero_means_snap_and_garbage_means_absent() {
        let zero = theme_line().replace("\"fade_ms\":600", "\"fade_ms\":0");
        assert!(matches!(
            parse_line(&zero),
            MxcLine::Theme {
                fade_ms: Some(0),
                ..
            }
        ));
        // Non-integer fade_ms: advisory field, so degrade to absent (spec
        // posture: unknown/unusable metadata never rejects a valid palette).
        let junk = theme_line().replace("\"fade_ms\":600", "\"fade_ms\":\"fast\"");
        assert!(matches!(
            parse_line(&junk),
            MxcLine::Theme { fade_ms: None, .. }
        ));
    }

    #[test]
    fn bye_reason_splits_revert_from_keep_last_good() {
        // §3.4 ruling: only an explicit "shutdown" reverts to the static
        // default; "reload" and unknown/absent reasons keep last-good.
        let shutdown = r#"{"t":"bye","v":1,"seq":12,"ts":1785616999000,"reason":"shutdown"}"#;
        assert_eq!(parse_line(shutdown), MxcLine::Bye { revert: true });
        let reload = r#"{"t":"bye","v":1,"seq":12,"ts":1785616999000,"reason":"reload"}"#;
        assert_eq!(parse_line(reload), MxcLine::Bye { revert: false });
        let unknown = r#"{"t":"bye","v":1,"seq":12,"ts":1,"reason":"cosmic_rays"}"#;
        assert_eq!(parse_line(unknown), MxcLine::Bye { revert: false });
        let absent = r#"{"t":"bye","v":1,"seq":12,"ts":1}"#;
        assert_eq!(parse_line(absent), MxcLine::Bye { revert: false });
    }

    #[test]
    fn garbage_and_blank_lines_are_skipped() {
        for junk in [
            "",
            "   ",
            "not json at all",
            "{\"half\":",
            "[1,2,3]",
            "{\"no_tag\":true}",
        ] {
            assert_eq!(parse_line(junk), MxcLine::Skip, "{junk:?} must be skipped");
        }
    }

    #[test]
    fn unknown_message_types_are_skipped_not_fatal() {
        // Forward compatibility (spec §5.3): a future `t` invented after we
        // shipped must not break the stream.
        let line = r#"{"t":"nowplaying","v":1,"seq":3,"ts":1,"track":"Blue Monday"}"#;
        assert_eq!(parse_line(line), MxcLine::Skip);
    }

    #[test]
    fn theme_frame_missing_a_token_is_skipped() {
        // All 16 tokens are REQUIRED; a partial palette is a publisher bug
        // and must not half-apply.
        let line = theme_line().replace("\"border_dimmest\":\"#101c28\"", "\"x\":\"#101c28\"");
        assert_eq!(parse_line(&line), MxcLine::Skip);
    }

    #[test]
    fn theme_frame_with_malformed_color_is_skipped() {
        let line = theme_line().replace("#64e0d0", "#64e0");
        assert_eq!(parse_line(&line), MxcLine::Skip);
    }

    #[test]
    fn newer_protocol_major_is_version_skew() {
        let line = r#"{"t":"theme","v":2,"seq":0,"ts":1,"colors":{}}"#;
        assert_eq!(parse_line(line), MxcLine::VersionSkew);
        let bye = r#"{"t":"bye","v":2,"seq":9,"ts":1,"reason":"reload"}"#;
        assert_eq!(parse_line(bye), MxcLine::VersionSkew);
    }

    #[test]
    fn non_u64_version_is_skew_not_version_zero() {
        // joestar #5: a PRESENT `v` that isn't a u64 (float, negative,
        // overflow, string) is not a version we can compare — treating it
        // as v0 would accept a payload we cannot trust.
        for v in ["2.5", "-1", "1e300", "\"1\"", "null", "true"] {
            let line = theme_line().replace("\"v\":1", &format!("\"v\":{v}"));
            assert_eq!(
                parse_line(&line),
                MxcLine::VersionSkew,
                "v:{v} must be treated as version skew"
            );
        }
        // Absent `v` stays Postel-lenient: decode as v0, accept the frame.
        let absent = theme_line().replace("\"v\":1,", "");
        assert!(matches!(parse_line(&absent), MxcLine::Theme { .. }));
    }

    #[test]
    fn unknown_envelope_fields_are_ignored() {
        let with_extra = "\"fade_ms\":600,\"future_field\":{\"a\":1}";
        let line = theme_line().replace("\"fade_ms\":600", with_extra);
        assert!(matches!(parse_line(&line), MxcLine::Theme { .. }));
    }

    #[test]
    fn seq_restart_is_just_a_snapshot() {
        // Every theme message is full state, so a seq-0 snapshot after a
        // reconnect parses identically to any other frame — there is no seq
        // state to confuse. This pins that the parser ignores `seq` entirely.
        let snapshot = parse_line(&theme_line());
        let high_seq = theme_line().replace("\"seq\":0", "\"seq\":98765");
        assert_eq!(parse_line(&high_seq), snapshot, "seq is ignored by design");
    }

    // ---- mapping ----

    #[test]
    fn mapping_places_every_token_per_the_table() {
        let t = theme_from_mxc(&sentinels());
        let s = |n: u8| Color::Rgb(n, n, n);

        // background → the transcript canvas
        assert_eq!(t.message_bg, s(10));
        // background_panel → chrome (header/footer), user turns, code, tool input
        assert_eq!(t.bg, s(11));
        assert_eq!(t.user_bg, s(11));
        assert_eq!(t.code_bg, s(11));
        assert_eq!(t.tool_input_bg, s(11));
        // background_element → tool_output_bg
        assert_eq!(t.tool_output_bg, s(12));
        // text → fg family
        for got in [
            t.input_fg,
            t.claude_text,
            t.code_fg,
            t.table_cell_color,
            t.event_text,
        ] {
            assert_eq!(got, s(8));
        }
        // text_muted → dim family
        for got in [
            t.muted,
            t.thinking_color,
            t.quote_color,
            t.help_fg,
            t.header_fg,
            t.tool_param,
            t.subagent_status,
            t.subagent_time,
        ] {
            assert_eq!(got, s(9));
        }
        // primary
        for got in [
            t.claude_label,
            t.heading_color,
            t.table_header_color,
            t.prompt_fg,
            t.event_source,
        ] {
            assert_eq!(got, s(1));
        }
        // secondary
        assert_eq!(t.subagent_name, s(2));
        assert_eq!(t.list_bullet_color, s(2));
        // accent → the user pill + cost/event highlights
        assert_eq!(t.user_color, s(3));
        assert_eq!(t.cost_color, s(3));
        assert_eq!(t.event_icon, s(3));
        // status colors
        assert_eq!(t.error_color, s(4));
        assert_eq!(t.event_critical, s(4));
        assert_eq!(t.warning_color, s(5));
        assert_eq!(t.status_streaming, s(5));
        assert_eq!(t.status_ready, s(6));
        assert_eq!(t.tool_result_ok, s(6));
        assert_eq!(t.subagent_done, s(6));
        assert_eq!(t.tool_label, s(7));
        assert_eq!(t.tool_result_color, s(7));
        // border hierarchy
        assert_eq!(t.border, s(13));
        assert_eq!(t.subagent_border, s(13));
        assert_eq!(t.border_active, s(14));
        assert_eq!(t.table_border_color, s(15));
        assert_eq!(t.separator, s(16));
    }

    #[test]
    fn mapping_keeps_tool_accents_and_overrides_at_defaults() {
        let t = theme_from_mxc(&sentinels());
        assert_eq!(t.tool_bash, Color::Reset);
        assert_eq!(t.tool_generic, Color::Reset);
        assert_eq!(t.settings_border, None);
        assert_eq!(t.sidecar_pill, None);
    }

    #[test]
    fn socket_path_honours_xdg_runtime_dir() {
        let p = socket_path_in(Some(PathBuf::from("/run/user/1000")));
        assert_eq!(p, PathBuf::from("/run/user/1000/myx/theme.sock"));
    }

    #[test]
    fn socket_path_falls_back_to_uid_scoped_tmp() {
        let p = socket_path_in(None);
        let s = p.to_string_lossy();
        assert!(
            s.starts_with("/tmp/myx-") && s.ends_with("/myx/theme.sock"),
            "fallback must be uid-scoped under /tmp, got {s}"
        );
    }

    #[test]
    fn empty_or_relative_xdg_runtime_dir_is_treated_as_unset() {
        // XDG basedir spec: non-absolute values must be ignored (joestar #3).
        for bogus in ["", "relative/dir", "./x"] {
            let p = socket_path_in(Some(PathBuf::from(bogus)));
            assert!(
                p.to_string_lossy().starts_with("/tmp/myx-"),
                "XDG_RUNTIME_DIR={bogus:?} must fall back to the uid-scoped path, got {p:?}"
            );
        }
    }

    #[test]
    fn socket_dir_trust_classifies_missing_and_trusted() {
        // Missing: parent dir does not exist → quiet retry, no connect.
        let ghost = PathBuf::from("/nonexistent-mxc-test-dir/myx/theme.sock");
        assert_eq!(socket_dir_trust(&ghost), SocketDirTrust::Missing);

        // Trusted: a directory we just created is owned by our uid.
        let dir = std::env::temp_dir().join(format!("mxc-trust-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        assert_eq!(
            socket_dir_trust(&dir.join("theme.sock")),
            SocketDirTrust::Trusted
        );
        let _ = std::fs::remove_dir_all(&dir);

        // Untrusted: `/` is root-owned (skip when the test itself runs as
        // root, where every directory is trivially ours).
        if uid() != 0 {
            assert_eq!(
                socket_dir_trust(&PathBuf::from("/theme.sock")),
                SocketDirTrust::Untrusted
            );
        }
    }

    // ---- capped line reader (heap-growth DoS defense) ----

    #[tokio::test]
    async fn read_line_capped_reads_normal_lines_and_eof() {
        let data = b"first\nsecond\ntrailing-fragment";
        let mut r = tokio::io::BufReader::new(&data[..]);
        let mut buf = Vec::new();
        assert_eq!(
            read_line_capped(&mut r, &mut buf).await.unwrap().as_deref(),
            Some("first")
        );
        assert_eq!(
            read_line_capped(&mut r, &mut buf).await.unwrap().as_deref(),
            Some("second")
        );
        // Unterminated trailing fragment = incomplete frame → EOF, discarded.
        assert_eq!(read_line_capped(&mut r, &mut buf).await.unwrap(), None);
    }

    #[tokio::test]
    async fn read_line_capped_rejects_oversized_lines() {
        // One byte over budget, never newline-terminated: must error out
        // (dropping the connection) instead of buffering without bound.
        let data = vec![b'x'; MAX_LINE_BYTES + 1];
        let mut r = tokio::io::BufReader::new(&data[..]);
        let mut buf = Vec::new();
        let err = read_line_capped(&mut r, &mut buf).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        // And the budget itself never ballooned.
        assert!(buf.len() <= MAX_LINE_BYTES);
    }

    #[tokio::test]
    async fn read_line_capped_rejects_non_utf8() {
        // Module contract: non-UTF-8 drops the CONNECTION (doc-aligned,
        // joestar #4) — surfaced as an io error here.
        let data = b"\xff\xfe garbage \xff\n";
        let mut r = tokio::io::BufReader::new(&data[..]);
        let mut buf = Vec::new();
        let err = read_line_capped(&mut r, &mut buf).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    // ---- session core (backoff reset, bye split, skew latch) ----

    type PaletteTx = tokio::sync::mpsc::UnboundedSender<(Theme, Option<u64>)>;
    type PaletteRx = tokio::sync::mpsc::UnboundedReceiver<(Theme, Option<u64>)>;

    fn test_channel() -> (PaletteTx, PaletteRx) {
        tokio::sync::mpsc::unbounded_channel()
    }

    #[tokio::test]
    async fn backoff_resets_on_first_valid_theme_line_not_on_connect() {
        let (tx, mut rx) = test_channel();
        let mut backoff = BACKOFF_CAP;
        let mut skew_warned = true; // healthy line must re-arm the latch too.

        // Session A: connects, delivers garbage only, dies. Backoff must
        // NOT reset — this is the accept-then-die endpoint (okarin F5).
        let garbage = b"not json\n";
        let end = run_session(&garbage[..], &tx, &mut backoff, &mut skew_warned).await;
        assert_eq!(end, SessionEnd::Disconnect);
        assert_eq!(backoff, BACKOFF_CAP, "no valid line → no backoff reset");
        assert!(skew_warned, "no valid line → skew latch stays armed");

        // Session B: a real theme line resets backoff and the skew latch.
        let data = format!("junk\n{}\n", theme_line());
        let end = run_session(data.as_bytes(), &tx, &mut backoff, &mut skew_warned).await;
        assert_eq!(end, SessionEnd::Disconnect); // EOF after the palette.
        assert_eq!(backoff, BACKOFF_START, "first VALID line resets backoff");
        assert!(!skew_warned, "healthy publisher re-arms the skew warn");
        assert!(
            rx.try_recv().is_ok(),
            "the palette must have been forwarded"
        );
    }

    #[tokio::test]
    async fn bye_shutdown_reverts_to_static_default_reload_keeps_last_good() {
        let (tx, mut rx) = test_channel();
        let mut backoff = BACKOFF_START;
        let mut warned = false;

        // reason:"reload" → nothing sent; last-good stands.
        let reload = b"{\"t\":\"bye\",\"v\":1,\"seq\":1,\"ts\":1,\"reason\":\"reload\"}\n";
        let end = run_session(&reload[..], &tx, &mut backoff, &mut warned).await;
        assert_eq!(end, SessionEnd::Disconnect);
        assert!(
            rx.try_recv().is_err(),
            "reload must keep last-good (no send)"
        );

        // reason:"shutdown" → the static myx default goes down the same
        // channel (knob-default fade), so yesterday's album colors die.
        let shutdown = b"{\"t\":\"bye\",\"v\":1,\"seq\":2,\"ts\":1,\"reason\":\"shutdown\"}\n";
        let end = run_session(&shutdown[..], &tx, &mut backoff, &mut warned).await;
        assert_eq!(end, SessionEnd::Disconnect);
        let (palette, fade) = rx.try_recv().expect("shutdown must send the revert");
        assert_eq!(palette, Theme::myx());
        assert_eq!(fade, None, "revert fades per the configured knob");
    }

    #[tokio::test]
    async fn version_skew_warns_once_per_generation() {
        let (tx, _rx) = test_channel();
        let mut backoff = BACKOFF_START;
        let mut warned = false;
        let skew = b"{\"t\":\"theme\",\"v\":9,\"seq\":0,\"ts\":1,\"colors\":{}}\n";

        let end = run_session(&skew[..], &tx, &mut backoff, &mut warned).await;
        assert_eq!(end, SessionEnd::Disconnect);
        assert!(warned, "first skew arms the once-per-generation latch");
        // Subsequent skewed sessions see the latch already set (the warn
        // branch is skipped); only a valid theme line re-arms it.
        let end = run_session(&skew[..], &tx, &mut backoff, &mut warned).await;
        assert_eq!(end, SessionEnd::Disconnect);
        assert!(warned);
    }

    #[tokio::test]
    async fn oversized_line_ends_the_session() {
        let (tx, _rx) = test_channel();
        let mut backoff = BACKOFF_CAP;
        let mut warned = false;
        let mut data = vec![b'{'; MAX_LINE_BYTES + 2];
        data.push(b'\n');
        let end = run_session(&data[..], &tx, &mut backoff, &mut warned).await;
        assert_eq!(end, SessionEnd::Disconnect);
        assert_eq!(backoff, BACKOFF_CAP, "a hostile line is not a valid line");
    }
}
