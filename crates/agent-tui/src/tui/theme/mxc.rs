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
//! - Disconnect / `bye` / EOF → KEEP the last-good palette, resume retrying.
//! - Malformed line → skip it, keep reading.
//! - Protocol version newer than ours → drop the connection (we cannot trust
//!   the payload) and retry on backoff; a downgraded Myx heals it.

use std::path::PathBuf;
use std::time::Duration;

use ratatui::style::Color;
use tokio::io::AsyncBufReadExt;

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
    /// A complete `theme` message: apply this palette.
    Theme(MxcColors),
    /// Clean publisher goodbye — keep the last-good palette, reconnect later.
    Bye,
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

/// Canvas recession factor: `message_bg` is derived by scaling the MXC
/// `background` toward black. Uniform channel scaling preserves hue and
/// saturation exactly; 0.605 puts the canvas/chrome contrast of the default
/// (tokyonight-based) palette at ~1.11, inside the 1.05–1.30 band the
/// builtin-palette tests enforce.
const CANVAS_RECESS: f32 = 0.605;

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
    let dir = runtime_dir.unwrap_or_else(|| PathBuf::from(format!("/tmp/myx-{}", uid())));
    dir.join("myx").join("theme.sock")
}

/// Current uid without a libc dependency: owner of `/proc/self`, falling
/// back to the owner of `$HOME`. Only reached when `XDG_RUNTIME_DIR` is
/// unset, which is already an unusual session.
fn uid() -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self")
        .or_else(|_| std::fs::metadata(std::env::var_os("HOME").unwrap_or_else(|| "/".into())))
        .map(|m| m.uid())
        .unwrap_or(0)
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
    let version = v.get("v").and_then(|n| n.as_u64()).unwrap_or(0);
    match tag {
        Some("theme") | Some("bye") if version > PROTOCOL_VERSION => MxcLine::VersionSkew,
        Some("theme") => match colors_from_json(v.get("colors")) {
            Some(colors) => MxcLine::Theme(colors),
            None => MxcLine::Skip, // known tag, unusable payload: publisher bug.
        },
        Some("bye") => MxcLine::Bye,
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

/// Recess a surface toward black for the transcript canvas. Uniform channel
/// scaling keeps hue/saturation intact (unlike CIELAB lightening, which
/// greys out saturated bases — see the `elevated_themes_keep_their_colour`
/// palette test).
fn recess(rgb: Rgb8) -> Rgb8 {
    let s = |v: u8| ((v as f32) * CANVAS_RECESS).round() as u8;
    (s(rgb.0), s(rgb.1), s(rgb.2))
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
/// | `background`         | `bg`, `user_bg`                                                    |
/// | `background_panel`   | `code_bg`, `tool_input_bg`                                         |
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
/// | *(derived)*          | `message_bg` = [`recess`]`(background)` — MXC has no surface darker than `background`, and the transcript canvas must sit below chrome |
///
/// Tool accent colors stay `Color::Reset` (auto-derived from the palette, as
/// every non-night-city builtin does), and per-part overrides stay `None`.
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
        bg: c(x.background),
        message_bg: c(recess(x.background)),
        border: c(x.border),
        border_active: c(x.border_active),
        muted: c(x.text_muted),

        // Messages
        user_color: c(x.accent),
        user_bg: c(x.background),
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
/// the only place theme state changes.
///
/// Lifecycle:
/// - connect fails (socket absent / stale) → sleep on capped backoff, retry.
/// - line parses to `Theme` → map, send. If the receiver is gone the app is
///   tearing down: exit.
/// - `Bye` / EOF / read error / version skew → drop the connection, keep the
///   last-good palette on screen, retry on backoff.
///
/// Framing uses a buffered line reader (never chunk-reads), so a message
/// split across socket writes still parses (spec §5.1).
pub(crate) async fn run_subscriber(tx: tokio::sync::mpsc::UnboundedSender<Theme>) {
    let path = socket_path();
    let mut backoff = BACKOFF_START;
    loop {
        if let Ok(stream) = tokio::net::UnixStream::connect(&path).await {
            // Reset only on successful connect, so an absent socket keeps
            // climbing toward the cap instead of oscillating.
            backoff = BACKOFF_START;
            let mut lines = tokio::io::BufReader::new(stream).lines();
            // EOF and socket errors both end the session → reconnect loop.
            while let Ok(Some(line)) = lines.next_line().await {
                match parse_line(&line) {
                    MxcLine::Theme(colors) => {
                        if tx.send(theme_from_mxc(&colors)).is_err() {
                            return; // receiver dropped — app teardown.
                        }
                    }
                    MxcLine::Skip => {}
                    MxcLine::Bye => break, // keep last-good, reconnect.
                    MxcLine::VersionSkew => {
                        tracing::warn!(
                            "MXC publisher speaks a newer protocol than v{PROTOCOL_VERSION}; \
                             holding last-good palette"
                        );
                        break;
                    }
                }
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(BACKOFF_CAP);
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
        let MxcLine::Theme(x) = parse_line(&theme_line()) else {
            panic!("theme frame must parse");
        };
        assert_eq!(x.primary, (0x64, 0xe0, 0xd0));
        assert_eq!(x.background, (0x08, 0x10, 0x18));
        assert_eq!(x.border_dimmest, (0x10, 0x1c, 0x28));
        assert_eq!(x.text_muted, (0x7a, 0x90, 0xa4));
    }

    #[test]
    fn bye_frame_parses() {
        let line = r#"{"t":"bye","v":1,"seq":12,"ts":1785616999000,"reason":"shutdown"}"#;
        assert_eq!(parse_line(line), MxcLine::Bye);
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
    fn unknown_envelope_fields_are_ignored() {
        let with_extra = "\"fade_ms\":600,\"future_field\":{\"a\":1}";
        let line = theme_line().replace("\"fade_ms\":600", with_extra);
        assert!(matches!(parse_line(&line), MxcLine::Theme(_)));
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

        // background → bg, user_bg
        assert_eq!(t.bg, s(10));
        assert_eq!(t.user_bg, s(10));
        // background_panel → code_bg, tool_input_bg
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
        // derived canvas: recess(background), never a raw token.
        assert_eq!(t.message_bg, Color::Rgb(6, 6, 6)); // 10 * 0.605 ≈ 6
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
    fn recess_preserves_saturation_exactly() {
        // Uniform scaling: (max-min)/max is invariant up to rounding.
        let (r, g, b) = recess((0x1a, 0x1b, 0x26));
        assert_eq!((r, g, b), (16, 16, 23));
    }

    // ---- socket path ----

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
}
