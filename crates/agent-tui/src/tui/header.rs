//! The Synaps header: the neon prompt's slab, hung from the top of the
//! screen, ending in a solid block whose colour is the state.
//!
//! ```text
//!   SynapsCLI ❯ ~/Projects/SynapsCLI · fix-cache   voice  3 agents 4m12s  ⠋ thinking 12s  
//! ▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀
//! ```
//!
//! * Row 0 is the prompt's body, edge to edge (the full width, square ends):
//!   same body colour, same sweep in the same phase (one
//!   [`Slab`] built from the same [`PromptFx`]). A failed turn tints it 7%
//!   toward the error colour, a busy one toward the warning colour.
//! * Row 1 is the slab's bottom rim (`▀`, full width). It is the message pane's top
//!   edge row, so the layout and every coordinate derived from it stay as
//!   they were; the header paints it after the pane.
//! * Left: `Synaps`+`CLI` `❯` folder `·` session (`❯ worker` in a worker view).
//! * Right end: the state block, flush to the right edge. Its cells and the
//!   rim under it take the state's fill; dark ink holds 5:1 on it.
//! * Wells, left of the block: square-ended 1½-row tiles for active/error
//!   sidecar pills, `observing`, and the agent chip (count + longest clock).
//!
//! Nothing moves at rest: the sweep and the spinner run only while a turn or
//! a worker runs. Every colour is a theme token or a mix of two.
//!
//! Design source: `synaps-header/r3/opt_slab_inlay.py` ("Slab, inlaid block").

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    symbols::border,
    widgets::Block,
    Frame,
};

use super::app::SPINNER_FRAMES;
use super::neon_prompt::{color, contrast, mix, rgb, PromptFx, Rgb, Slab};
use super::render_model::{RenderModel, SidecarPillSnap, SubagentSnap};
use super::sidecar::SidecarUiStatus;
use super::subagent_tray::legible;
use super::text_metrics::{char_width, width as display_width};
use super::theme::{background_is_opaque, Theme, THEME};

// ─── state ──────────────────────────────────────────────────────────────────

/// What the current turn is doing. Tracked on `App` by the stream handler;
/// shipped to the render side in [`HeaderSnap`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum Activity {
    #[default]
    Idle,
    /// Request sent (or tool results returned); no first token yet.
    Waiting,
    Thinking,
    /// Text or tool-call input streaming out.
    Writing,
    /// Tool calls dispatched, not all results back.
    Tool,
    /// The provider pushed back (429/5xx/refusal); a retry is pending.
    Retry,
    /// The last turn ended in an error. Cleared when the next turn starts.
    Failed(String),
}

/// The header's slice of the per-frame snapshot.
#[derive(Clone, Debug, Default)]
pub(crate) struct HeaderSnap {
    pub(crate) activity: Activity,
    /// Session name, else its auto title, else its id. Already sanitised.
    pub(crate) session_label: String,
    /// Seconds since the current turn started (`None` when no turn runs).
    pub(crate) turn_elapsed_secs: Option<u64>,
    /// Monotonic animation clock in milliseconds (wall clock, so the
    /// block's sweep keeps its speed however busy the event loop is).
    pub(crate) anim_ms: u64,
    /// The working folder, `~`-abbreviated ([`folder_label`]).
    pub(crate) folder: String,
    /// This client is attached read-only (`synaps --attach --observe`).
    pub(crate) observing: bool,
}

/// What the header shows, derived per frame from the snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HeaderState {
    Idle,
    Connecting,
    Thinking,
    Streaming,
    Tool,
    /// The main turn is idle and workers are running.
    Agents {
        done: usize,
        total: usize,
    },
    Retry,
    /// A transient status the header can only name: compacting,
    /// reconnecting, aborting.
    Busy(String),
    Failed(String),
}

impl HeaderState {
    /// A turn or a worker runs: the only time anything in the header moves.
    fn is_live(&self) -> bool {
        !matches!(self, HeaderState::Idle | HeaderState::Failed(_))
    }
}

/// Derive the header state. `status_text` wins (it is the explicit
/// connecting/compacting/reconnecting/aborting signal); then the streaming
/// turn's activity; then running workers; then the last turn's failure.
pub(crate) fn header_state(
    streaming: bool,
    status_text: Option<&str>,
    activity: &Activity,
    subagents: &[SubagentSnap],
) -> HeaderState {
    if let Some(text) = status_text {
        let word = text.split('\u{2026}').next().unwrap_or(text).trim();
        if word.eq_ignore_ascii_case("connecting") {
            return HeaderState::Connecting;
        }
        if !word.is_empty() {
            return HeaderState::Busy(word.to_string());
        }
    }
    if streaming {
        return match activity {
            Activity::Thinking => HeaderState::Thinking,
            Activity::Writing => HeaderState::Streaming,
            Activity::Tool => HeaderState::Tool,
            Activity::Retry => HeaderState::Retry,
            _ => HeaderState::Connecting,
        };
    }
    let total = subagents.len();
    let done = subagents.iter().filter(|s| s.done).count();
    if done < total {
        return HeaderState::Agents { done, total };
    }
    match activity {
        Activity::Failed(label) => HeaderState::Failed(label.clone()),
        _ => HeaderState::Idle,
    }
}

// ─── text ───────────────────────────────────────────────────────────────────

/// `v<version> · <hash>`, or `v<version>` alone when the build has no
/// hash. `build.rs` bakes `release` in place of the hash for packaged
/// builds (AUR, Homebrew, crates.io build from a source tarball, not a
/// synaps checkout), and a released version already names its commit, so
/// `· release` would only read as a missing hash.
fn build_label(version: &str, hash: &str) -> String {
    if hash.is_empty() || hash == "release" {
        format!("v{version}")
    } else {
        format!("v{version} \u{b7} {hash}")
    }
}

#[cfg(any(test, feature = "testing"))]
thread_local! {
    /// Test seam: the build label headers rendered on this thread show,
    /// whatever id the binary has (`TestHarness::set_build_id`). Headless
    /// tests render on their own thread, so this never leaks between them.
    static BUILD_OVERRIDE: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
}

/// Pin (or with `None`, clear) the build id this thread's headers show.
#[cfg(any(test, feature = "testing"))]
pub(crate) fn set_build_id_for_thread(id: Option<&str>) {
    let label =
        id.map(|id| &*Box::leak(build_label(env!("CARGO_PKG_VERSION"), id).into_boxed_str()));
    BUILD_OVERRIDE.with(|o| o.set(label));
}

/// This build, as the resting block shows it (see [`build_label`]).
pub(crate) fn build() -> &'static str {
    #[cfg(any(test, feature = "testing"))]
    if let Some(label) = BUILD_OVERRIDE.with(std::cell::Cell::get) {
        return label;
    }
    static BUILD: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    // The same identity the version-skew check compares (04-tui M8): the
    // engine's `SYNAPS_BUILD_ID`, which packagers can set.
    BUILD.get_or_init(|| {
        let this = agent_engine::session::transport::BinaryId::this();
        build_label(&this.version, &this.build)
    })
}

/// This build, as `/status` reports it.
pub(crate) fn build_line() -> String {
    format!("synaps {}", build())
}

/// Elapsed time, compact: `12s`, `4m12s`, `1h02m`.
pub(crate) fn elapsed(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, secs % 3600 / 60)
    }
}

/// Session name: name, else auto title, else id. Control characters and
/// runs of whitespace collapse to one space (titles are the first user
/// message and may hold newlines).
pub(crate) fn session_label(name: Option<&str>, title: &str, id: &str) -> String {
    let raw = name
        .filter(|n| !n.trim().is_empty())
        .unwrap_or(if title.trim().is_empty() { id } else { title });
    let mut out = String::with_capacity(raw.len());
    let mut space = false;
    for ch in raw.chars() {
        if ch.is_control() || ch.is_whitespace() {
            space = !out.is_empty();
        } else {
            if space {
                out.push(' ');
                space = false;
            }
            out.push(ch);
        }
    }
    out
}

/// `path` with the home directory written `~`, for the header's folder.
pub(crate) fn folder_label(path: &std::path::Path, home: Option<&std::path::Path>) -> String {
    if let Some(rest) = home.and_then(|h| path.strip_prefix(h).ok()) {
        if rest.as_os_str().is_empty() {
            return "~".to_string();
        }
        return format!("~/{}", rest.display());
    }
    path.display().to_string()
}

/// The process's working folder as the header shows it.
pub(crate) fn current_folder() -> String {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    std::env::current_dir()
        .map(|cwd| folder_label(&cwd, home.as_deref()))
        .unwrap_or_default()
}

/// `s` cut to at most `max` cells, with `…` when cut.
fn truncate(s: &str, max: usize) -> String {
    if display_width(s) <= max {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for ch in s.chars() {
        let cw = char_width(ch);
        if w + cw + 1 > max {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('\u{2026}');
    out
}

/// The detail after a status word, first token only: `reconnecting… (4s
/// left, attempt 2)` gives `4s`.
fn busy_detail(status_text: Option<&str>) -> Option<String> {
    let rest = status_text?.split_once('\u{2026}')?.1.trim();
    let rest = rest.trim_start_matches('(').trim_end_matches(')');
    let word = rest.split_whitespace().next()?.trim_end_matches(',');
    (!word.is_empty()).then(|| word.to_string())
}

// ─── what one frame shows ─────────────────────────────────────────────────

/// Everything the header paints, resolved from the [`RenderModel`].
pub(crate) struct HeaderView<'a> {
    pub(crate) state: HeaderState,
    /// `connecting` came from the run loop (a daemon reconnect), not from a
    /// turn waiting on its first token.
    pub(crate) status_connecting: bool,
    pub(crate) busy_detail: Option<String>,
    pub(crate) turn_secs: Option<u64>,
    pub(crate) anim_ms: u64,
    pub(crate) spinner_frame: usize,
    pub(crate) session: &'a str,
    pub(crate) folder: &'a str,
    /// The worker whose view is focused.
    pub(crate) watching: Option<&'a str>,
    pub(crate) observing: bool,
    /// Active or failed sidecar pills: `(label, failed)`. Idle ones are
    /// never listed.
    pub(crate) pills: Vec<(String, bool)>,
    /// Running workers and the longest one's elapsed seconds.
    pub(crate) workers: usize,
    pub(crate) longest_worker_secs: u64,
    pub(crate) fx: PromptFx,
    /// The prompt's streaming sweep colour (`None`: no sweep).
    pub(crate) glow: Option<Color>,
    pub(crate) build: &'a str,
    pub(crate) scroll_back: u16,
}

impl<'a> HeaderView<'a> {
    pub(crate) fn from_model(model: &'a RenderModel, glow: Option<Color>) -> Self {
        let snap = &model.header;
        let state = header_state(
            model.streaming,
            model.status_text.as_deref(),
            &snap.activity,
            &model.subagents,
        );
        let pills = header_pills(&model.sidecar_pills);
        let running = model.subagents.iter().filter(|s| !s.done);
        Self {
            status_connecting: model.status_text.is_some(),
            busy_detail: busy_detail(model.status_text.as_deref()),
            turn_secs: snap.turn_elapsed_secs,
            anim_ms: snap.anim_ms,
            spinner_frame: model.spinner_frame,
            session: &snap.session_label,
            folder: &snap.folder,
            watching: model.watch_banner.as_ref().map(|b| b.agent_name.as_str()),
            observing: snap.observing,
            pills,
            workers: running.clone().count(),
            longest_worker_secs: running
                .map(|s| s.elapsed_secs.max(0.0) as u64)
                .max()
                .unwrap_or(0),
            fx: model.prompt_fx,
            glow,
            build: build(),
            scroll_back: model.scroll_back,
            state,
        }
    }

    fn spinner(&self) -> &'static str {
        SPINNER_FRAMES[(self.spinner_frame / 3) % SPINNER_FRAMES.len()]
    }
}

/// The sidecar pills worth a well: active (or armed) and failed ones, as
/// `(label, failed)`. Idle and loading pills are hidden.
fn header_pills(pills: &[SidecarPillSnap]) -> Vec<(String, bool)> {
    pills
        .iter()
        .filter_map(|p| {
            let label = p.display_name.clone().unwrap_or_else(|| "sidecar".into());
            match &p.status {
                SidecarUiStatus::Error(_) => Some((label, true)),
                SidecarUiStatus::Active { .. } => Some((label, false)),
                SidecarUiStatus::Idle if p.armed => Some((label, false)),
                _ => None,
            }
        })
        .collect()
}

// ─── colour ─────────────────────────────────────────────────────────────────

/// Which extreme text is lifted toward: white on a dark chrome, black on a
/// light one.
fn toward(t: &Theme) -> Rgb {
    let bg = rgb(t.bg, Theme::default().bg);
    if contrast((255, 255, 255), bg) >= contrast((0, 0, 0), bg) {
        (255, 255, 255)
    } else {
        (0, 0, 0)
    }
}

/// The text roles on the slab and in the wells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    Mark,
    Mark2,
    Chev,
    Path,
    Word,
    Sep,
    Session,
    Agent,
    AgentClock,
    Ready,
    Err,
    Warn,
}

fn tone(t: &Theme, role: Role) -> Rgb {
    let d = Theme::default();
    let agent = rgb(t.subagent_name, d.subagent_name);
    match role {
        Role::Mark => rgb(t.header_fg, d.header_fg),
        Role::Mark2 | Role::Word | Role::Sep => rgb(t.muted, d.muted),
        Role::Chev => rgb(t.prompt_fg, d.prompt_fg),
        Role::Path => rgb(t.claude_text, d.claude_text),
        Role::Session => rgb(t.claude_label, d.claude_label),
        Role::Agent => agent,
        Role::AgentClock => mix(agent, rgb(t.input_fg, d.input_fg), 0.35),
        Role::Ready => rgb(t.status_ready, d.status_ready),
        Role::Err => rgb(t.error_color, d.error_color),
        Role::Warn => rgb(t.warning_color, d.warning_color),
    }
}

/// The contrast a role holds on the slab: separators are glyphs, not words.
fn target(role: Role, need: f32) -> f32 {
    match role {
        Role::Chev | Role::Sep => 1.6,
        Role::Mark2 => 4.5,
        _ => need,
    }
}

/// The colour of letter `i` of `n` in `Synaps`. At rest (idle) it is the
/// header colour; after a failed turn, the error colour (still, nothing moves
/// at rest). While anything runs it glows: a gradient through the state's
/// colour, the prompt colour and the session colour flows across the letters,
/// one lap every 2.5 s.
fn mark_color(t: &Theme, state: &HeaderState, i: usize, n: usize, tick: f32) -> Rgb {
    let d = Theme::default();
    match state {
        HeaderState::Idle => return tone(t, Role::Mark),
        HeaderState::Failed(_) => return rgb(t.error_color, d.error_color),
        _ => {}
    }
    let accent = match fill_kind(state) {
        Fill::Tool => rgb(t.tool_label, d.tool_label),
        Fill::Agents => rgb(t.subagent_name, d.subagent_name),
        Fill::Busy => rgb(t.warning_color, d.warning_color),
        _ => rgb(t.status_streaming, d.status_streaming),
    };
    let palette = [
        accent,
        rgb(t.prompt_fg, d.prompt_fg),
        rgb(t.claude_label, d.claude_label),
        accent,
    ];
    let phase = (i as f32 / n.max(1) as f32 * 0.6 - tick / 50.0).rem_euclid(1.0);
    let seg = phase * (palette.len() - 1) as f32;
    let k = (seg as usize).min(palette.len() - 2);
    let f = seg - k as f32;
    let f = f * f * (3.0 - 2.0 * f);
    mix(palette[k], palette[k + 1], f)
}

/// The block's state family.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fill {
    Rest,
    Stream,
    Tool,
    Agents,
    Busy,
    Failed,
}

fn fill_kind(state: &HeaderState) -> Fill {
    match state {
        HeaderState::Idle => Fill::Rest,
        HeaderState::Connecting | HeaderState::Thinking | HeaderState::Streaming => Fill::Stream,
        HeaderState::Tool => Fill::Tool,
        HeaderState::Agents { .. } => Fill::Agents,
        HeaderState::Retry | HeaderState::Busy(_) => Fill::Busy,
        HeaderState::Failed(_) => Fill::Failed,
    }
}

/// The block's fill and ink: the state colour softened toward the chrome,
/// pushed just far enough that the chrome itself (the ink) reads at 5:1.
fn block_colors(t: &Theme, state: &HeaderState) -> (Rgb, Rgb, Rgb) {
    let d = Theme::default();
    let bg = rgb(t.bg, d.bg);
    let warn = rgb(t.warning_color, d.warning_color);
    let err = rgb(t.error_color, d.error_color);
    let base = match fill_kind(state) {
        Fill::Failed => err,
        Fill::Busy => mix(warn, err, 0.35),
        Fill::Tool => mix(bg, rgb(t.tool_label, d.tool_label), 0.9),
        Fill::Stream => mix(bg, rgb(t.status_streaming, d.status_streaming), 0.9),
        Fill::Agents => mix(bg, rgb(t.subagent_name, d.subagent_name), 0.9),
        Fill::Rest => mix(bg, rgb(t.status_ready, d.status_ready), 0.82),
    };
    let ink = bg;
    let fill = legible(base, toward(t), ink, 5.0);
    (fill, ink, ink2_on(ink, fill))
}

/// Secondary ink: the ink eased toward the fill, still at 4.6:1 or more.
fn ink2_on(ink: Rgb, fill: Rgb) -> Rgb {
    let mut best = ink;
    for i in 1..15 {
        let m = mix(ink, fill, i as f32 / 40.0);
        if contrast(m, fill) < 4.6 {
            break;
        }
        best = m;
    }
    best
}

/// 0..1: a soft band sweeping left to right over `w` columns.
fn sweep_at(x: f32, w: f32, tick: f32, spread: f32) -> f32 {
    let sweep = (tick * 0.9) % (w + 40.0) - 20.0;
    let d = (x - sweep) / spread;
    (-d * d).exp()
}

// ─── content ────────────────────────────────────────────────────────────────

/// Ink roles inside the block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ink {
    Bold,
    Plain,
    Second,
}

type Spans = Vec<(String, Role, bool)>;

fn spans_w(spans: &Spans) -> usize {
    spans.iter().map(|(s, _, _)| display_width(s)).sum()
}

fn block_spans(v: &HeaderView<'_>, hash: bool) -> Vec<(String, Ink)> {
    let spin = format!("{} ", v.spinner());
    let clock = |secs: Option<u64>| secs.map(|s| (format!(" {}", elapsed(s)), Ink::Second));
    let live = |word: &str| {
        let mut out = vec![(spin.clone(), Ink::Plain), (word.to_string(), Ink::Bold)];
        out.extend(clock(v.turn_secs));
        out
    };
    match &v.state {
        HeaderState::Failed(label) => {
            vec![("\u{2717} ".into(), Ink::Bold), (label.clone(), Ink::Bold)]
        }
        HeaderState::Busy(word) => {
            let mut out = vec![(spin.clone(), Ink::Plain), (word.clone(), Ink::Bold)];
            out.extend(
                v.busy_detail
                    .as_ref()
                    .map(|d| (format!(" {d}"), Ink::Second)),
            );
            out
        }
        HeaderState::Connecting if v.status_connecting => live("connecting"),
        HeaderState::Connecting => live("waiting"),
        HeaderState::Thinking => live("thinking"),
        HeaderState::Streaming => live("writing"),
        HeaderState::Tool => live("tools"),
        HeaderState::Retry => live("retrying"),
        HeaderState::Agents { .. } => {
            let n = v.workers;
            vec![
                (spin.clone(), Ink::Plain),
                (
                    format!("{n} agent{}", if n == 1 { "" } else { "s" }),
                    Ink::Bold,
                ),
                (format!(" {}", elapsed(v.longest_worker_secs)), Ink::Second),
            ]
        }
        HeaderState::Idle => {
            let (ver, hash_part) = match v.build.split_once(" \u{b7} ") {
                Some((ver, h)) => (ver, Some(h)),
                None => (v.build, None),
            };
            let mut out = vec![(ver.to_string(), Ink::Bold)];
            if let (Some(h), true) = (hash_part, hash) {
                out.push((format!(" \u{b7} {h}"), Ink::Second));
            }
            out
        }
    }
}

/// The agent chip, only while the main turn owns the block (when only
/// workers run, the block itself shows the count and clock).
fn chip_spans(v: &HeaderView<'_>, compact: bool) -> Spans {
    if v.workers == 0 || matches!(v.state, HeaderState::Agents { .. }) {
        return Vec::new();
    }
    let n = v.workers;
    let word = if compact {
        " "
    } else if n == 1 {
        " agent "
    } else {
        " agents "
    };
    vec![
        (n.to_string(), Role::Agent, true),
        (word.into(), Role::Word, false),
        (elapsed(v.longest_worker_secs), Role::AgentClock, false),
    ]
}

/// The pill and attach-mode wells: `(spans, tone)`.
fn well_items(v: &HeaderView<'_>) -> Vec<(Spans, Role)> {
    let mut out: Vec<(Spans, Role)> = v
        .pills
        .iter()
        .map(|(label, failed)| {
            let role = if *failed { Role::Err } else { Role::Ready };
            (vec![(label.clone(), role, false)], role)
        })
        .collect();
    if v.observing {
        out.push((vec![("observing".into(), Role::Warn, false)], Role::Warn));
    }
    out
}

/// One rung of the fitting ladder.
#[derive(Clone, Copy, Debug)]
struct Cfg {
    /// 0 full path, 1 `…/base`, n>1 the base cut to n cells.
    folder: usize,
    /// Most cells for the session label (0 drops it).
    session: usize,
    chip_compact: bool,
    pad: usize,
    bmin: usize,
    bpad: usize,
    hash: bool,
}

const BASE: Cfg = Cfg {
    folder: 0,
    session: 40,
    chip_compact: false,
    pad: 1,
    bmin: 20,
    bpad: 2,
    hash: true,
};

/// Applied cumulatively until the row fits (the mock's `LADDER`). The
/// wordmark, the block, problems (error pills, `observing`) and the clocks
/// are never dropped.
const LADDER: &[fn(&mut Cfg)] = &[
    |_| {},
    |c| c.folder = 1,
    |c| c.session = 18,
    |c| c.session = 12,
    |c| c.pad = 0,
    |c| {
        c.bmin = 16;
        c.bpad = 1;
    },
    |c| c.session = 8,
    |c| c.chip_compact = true,
    |c| c.hash = false,
    |c| c.folder = 12,
    |c| c.session = 0,
    |c| c.folder = 9,
];

fn configs(w: usize) -> impl Iterator<Item = Cfg> {
    LADDER.iter().scan(BASE, move |cfg, step| {
        step(cfg);
        if w >= 100 {
            // At 100 columns and up the session always keeps 10 cells + `…`.
            cfg.session = cfg.session.max(11);
        }
        Some(*cfg)
    })
}

fn left_spans(v: &HeaderView<'_>, cfg: &Cfg) -> Spans {
    let mut out: Spans = vec![
        ("Synaps".into(), Role::Mark, true),
        ("CLI".into(), Role::Mark2, false),
        (" \u{276f} ".into(), Role::Chev, true),
    ];
    let folder = v.folder.trim_end_matches('/');
    let folder = if folder.is_empty() { v.folder } else { folder };
    if !folder.is_empty() {
        let (prefix, base) = match folder.rsplit_once('/') {
            Some((p, b)) if !b.is_empty() => (Some(p), b),
            _ => (None, folder),
        };
        match (cfg.folder, prefix) {
            (0, Some(p)) => {
                out.push((format!("{p}/"), Role::Word, false));
                out.push((base.into(), Role::Path, false));
            }
            (1, Some(_)) => {
                out.push(("\u{2026}/".into(), Role::Word, false));
                out.push((base.into(), Role::Path, false));
            }
            (n, _) if n > 1 => out.push((truncate(base, n), Role::Path, false)),
            _ => out.push((base.into(), Role::Path, false)),
        }
    }
    if cfg.session > 0 && !v.session.is_empty() {
        out.push((" \u{b7} ".into(), Role::Sep, false));
        out.push((truncate(v.session, cfg.session), Role::Session, false));
    }
    if let Some(name) = v.watching {
        out.push((" \u{276f} ".into(), Role::Chev, true));
        out.push((name.into(), Role::Agent, true));
    }
    out
}

/// `spans` cut to `room` cells, the last one kept ending in `…`.
fn fit_spans(spans: &Spans, room: usize) -> Spans {
    if spans_w(spans) <= room {
        return spans.clone();
    }
    let mut out = Spans::new();
    let mut left = room;
    for (text, role, bold) in spans {
        let w = display_width(text);
        if w < left {
            out.push((text.clone(), *role, *bold));
            left -= w;
        } else {
            if left > 0 {
                out.push((truncate(text, left), *role, *bold));
            }
            break;
        }
    }
    out
}

fn well_w(spans: &Spans, pad: usize) -> usize {
    spans_w(spans) + 2 + 2 * pad
}

/// Where everything goes on a row `w` cells wide.
struct Plan {
    cfg: Cfg,
    left: Spans,
    /// Pill/attach wells then the chip, left to right, each `(spans, tone)`.
    wells: Vec<(Spans, Role)>,
    block: Vec<(String, Ink)>,
    /// Block cells `[bx, w)`: flush to the right edge.
    bx: usize,
    /// Where the first well starts (`bx` with none).
    wells_x: usize,
}

fn plan(v: &HeaderView<'_>, w: usize) -> Plan {
    let items = well_items(v);
    let mut last = None;
    for cfg in configs(w) {
        let left = left_spans(v, &cfg);
        let chip = chip_spans(v, cfg.chip_compact);
        let block = block_spans(v, cfg.hash);
        let bw = cfg.bmin.max(display_width_ink(&block) + 2 * cfg.bpad);
        let mut wells = items.clone();
        if !chip.is_empty() {
            wells.push((chip, Role::Agent));
        }
        let right: usize = wells.iter().map(|(s, _)| well_w(s, cfg.pad) + 1).sum();
        // pad2 [left] gap2 [wells, one gap after each] [block: bw + 1 cells,
        // to the edge]
        let fits = 2 + spans_w(&left) + 2 + right + bw < w;
        last = Some((cfg, left, wells, block, bw));
        if fits {
            break;
        }
    }
    let (cfg, left, mut wells, block, bw) = last.expect("the ladder is never empty");
    // Too narrow even for the last rung: the block shrinks to the room
    // there is, then wells give way from the left. Nothing overlaps.
    let r = w.saturating_sub(1);
    let bw = bw.min(w.saturating_sub(4));
    let bx = r - bw;
    let right = |wells: &Vec<(Spans, Role)>| -> usize {
        wells.iter().map(|(s, _)| well_w(s, cfg.pad) + 1).sum()
    };
    while !wells.is_empty() && bx < right(&wells) + 3 {
        wells.remove(0);
    }
    let wells_x = bx - right(&wells);
    Plan {
        cfg,
        left,
        wells,
        block,
        bx,
        wells_x,
    }
}

fn display_width_ink(spans: &[(String, Ink)]) -> usize {
    spans.iter().map(|(s, _)| display_width(s)).sum()
}

// ─── painting ───────────────────────────────────────────────────────────────

/// One cell: symbol, foreground, background.
type CellSpec = (&'static str, Color, Color);

fn put(buf: &mut Buffer, x: u16, y: u16, sym: &str, fg: Color, bg: Color, bold: bool) {
    if let Some(cell) = buf.cell_mut((x, y)) {
        cell.reset();
        cell.set_symbol(sym).set_fg(fg).set_bg(bg);
        if bold {
            cell.modifier = Modifier::BOLD;
        }
    }
}

/// The colour of a cell already painted (the slab under a well's text).
fn bg_rgb(buf: &Buffer, x: u16, y: u16, fallback: Rgb) -> Rgb {
    buf.cell((x, y))
        .map_or(fallback, |c| rgb(c.bg, color(fallback)))
}

/// Text on whatever row 0 already holds, each cell's colour lifted until it
/// reads on that cell. Stops before `max_x`. Returns the column after it.
#[allow(clippy::too_many_arguments)]
fn write(
    buf: &mut Buffer,
    mut x: u16,
    y: u16,
    s: &str,
    fg: Rgb,
    lift: Rgb,
    target: f32,
    bold: bool,
    max_x: u16,
    fallback: Rgb,
) -> u16 {
    let mut tmp = [0u8; 4];
    for ch in s.chars() {
        let cw = char_width(ch) as u16;
        if x + cw > max_x {
            break;
        }
        let bg = buf.cell((x, y)).map_or(Color::Reset, |c| c.bg);
        let ink = legible(fg, lift, bg_rgb(buf, x, y, fallback), target);
        put(buf, x, y, ch.encode_utf8(&mut tmp), color(ink), bg, bold);
        x += cw;
    }
    x
}

/// Paint the header over `area`: row 0 is the header row, row 1 (if any)
/// the message pane's top edge. Coordinates are absolute; nothing outside
/// `area` is touched.
pub(crate) fn paint(buf: &mut Buffer, area: Rect, t: &Theme, v: &HeaderView<'_>) {
    let w = area.width as usize;
    let y0 = area.y;
    let edge = (area.height >= 2).then_some(area.y + 1);
    let chrome = t.bg;
    if w < 8 {
        for x in area.left()..area.right() {
            put(buf, x, y0, " ", chrome, chrome, false);
        }
        return;
    }
    let d = Theme::default();
    let bg = rgb(t.bg, d.bg);
    // A real chrome colour: the slab. `Reset` (or a named colour) can't
    // carry half-blocks, so only the block and the wells are drawn solid
    // and the pane keeps its plain top rule.
    let solid = matches!(t.bg, Color::Rgb(..));
    let lift = toward(t);
    let need = if v.glow.is_some() { 3.0 } else { 4.5 };
    let tick = v.anim_ms as f32 * 20.0 / 1000.0;

    let p = plan(v, w);
    // Edge to edge: the slab spans the full width with square ends.
    let (l, r) = (0usize, w - 1);
    let bw = r - p.bx;
    let ax = |x: usize| area.x + x as u16;

    // Colours per column.
    let slab = Slab::new(t, v.fx, area.width, v.glow);
    let tint = match fill_kind(&v.state) {
        Fill::Failed => Some(rgb(t.error_color, d.error_color)),
        Fill::Busy => Some(rgb(t.warning_color, d.warning_color)),
        _ => None,
    };
    let (fill, ink, ink2) = block_colors(t, &v.state);
    let moving = v.state.is_live();
    let block_at = |x: usize| {
        if moving {
            let s = sweep_at((x - p.bx) as f32, (bw + 1) as f32, tick, 4.0);
            mix(fill, lift, 0.16 * s)
        } else {
            fill
        }
    };
    let body_at = |x: usize| {
        let f = slab.fill_at(x as u16);
        tint.map_or(f, |c| mix(f, c, 0.07))
    };
    let fill_at = |x: usize| if x >= p.bx { block_at(x) } else { body_at(x) };
    // Under the rim: the transcript itself (its canvas, or the terminal's
    // own background when the canvas is see-through), so the header is the
    // body row plus half a row and nothing more.
    let below = if background_is_opaque() {
        t.message_background()
    } else {
        Color::Reset
    };

    // The slab: row 0 body, row 1 rim, the full width. Without a colour
    // chrome only the block shows.
    for x in 0..w {
        let (row0, row1): (CellSpec, Option<CellSpec>) = if solid || x >= p.bx {
            let f = color(fill_at(x));
            ((" ", f, f), Some(("\u{2580}", f, below)))
        } else {
            ((" ", chrome, chrome), None)
        };
        put(buf, ax(x), y0, row0.0, row0.1, row0.2, false);
        if let (Some(y1), Some((sym, f, b))) = (edge, row1) {
            put(buf, ax(x), y1, sym, f, b, false);
        }
    }

    // Left run, cut with `…` if even the last rung left it too wide.
    let max_left = ax(p.wells_x.saturating_sub(2).max(l + 2));
    let mut x = ax(l + 2);
    for (text, role, bold) in &fit_spans(&p.left, usize::from(max_left - x)) {
        let tg = target(*role, need);
        if *role == Role::Mark {
            // `Synaps` glows while anything runs: one colour per letter.
            let n = text.chars().count();
            let mut tmp = [0u8; 4];
            for (i, ch) in text.chars().enumerate() {
                let fg = mark_color(t, &v.state, i, n, tick);
                x = write(
                    buf,
                    x,
                    y0,
                    ch.encode_utf8(&mut tmp),
                    fg,
                    lift,
                    tg,
                    *bold,
                    max_left,
                    bg,
                );
            }
            continue;
        }
        x = write(
            buf,
            x,
            y0,
            text,
            tone(t, *role),
            lift,
            tg,
            *bold,
            max_left,
            bg,
        );
    }

    // Wells: square-ended 1½-row tiles sunk into the slab.
    let mut wx = p.wells_x;
    for (spans, role) in &p.wells {
        let n = well_w(spans, p.cfg.pad);
        let wtone = tone(t, *role);
        for j in 0..n {
            let cx = ax(wx + j);
            let under = bg_rgb(buf, cx, y0, bg);
            let sunk = color(mix(mix(under, bg, 0.55), wtone, 0.16));
            put(buf, cx, y0, " ", sunk, sunk, false);
            if let Some(y1) = edge {
                let below = buf.cell((cx, y1)).map_or(chrome, |c| c.bg);
                put(buf, cx, y1, "\u{2580}", sunk, below, false);
            }
        }
        let mut tx = ax(wx + 1 + p.cfg.pad);
        for (text, role, bold) in spans {
            tx = write(
                buf,
                tx,
                y0,
                text,
                tone(t, *role),
                lift,
                need,
                *bold,
                ax(wx + n),
                bg,
            );
        }
        wx += n + 1;
    }

    // The block's text.
    let mut tx = ax(p.bx + p.cfg.bpad);
    let mut tmp = [0u8; 4];
    for (text, role) in &p.block {
        let fg = color(if *role == Ink::Second { ink2 } else { ink });
        for ch in text.chars() {
            let cw = char_width(ch) as u16;
            if tx + cw > ax(r) {
                break;
            }
            let b = color(block_at((tx - area.x) as usize));
            put(
                buf,
                tx,
                y0,
                ch.encode_utf8(&mut tmp),
                fg,
                b,
                *role == Ink::Bold,
            );
            tx += cw;
        }
    }

    // Scrolled back: a tab hanging off the rim, just left of the wells.
    if let (Some(y1), true) = (edge, v.scroll_back > 0) {
        let label = format!(" \u{2191}{} ", v.scroll_back);
        let n = display_width(&label);
        let end = p.wells_x.saturating_sub(1);
        if end >= n + l + 2 {
            let start = end - n;
            for (cx, ch) in (ax(start)..).zip(label.chars()) {
                let (fg, b) = if solid {
                    (slab.dim_fg(), color(body_at((cx - area.x) as usize)))
                } else {
                    (t.muted, chrome)
                };
                put(buf, cx, y1, ch.encode_utf8(&mut tmp), fg, b, false);
            }
        }
    }
}

/// Draw the header over `area` (the header row plus the message pane's top
/// edge row under it). Call after the message pane is drawn: the edge row
/// is the pane's border, and the header owns its colours.
pub(crate) fn render(frame: &mut Frame<'_>, area: Rect, model: &RenderModel, glow: Option<Color>) {
    let t = THEME.load();
    let view = HeaderView::from_model(model, glow);
    paint(frame.buffer_mut(), area, &t, &view);
}

/// True when the header paints the message pane's top edge as the slab's
/// rim (`▀`). Off when the chrome is the terminal default (or a named
/// colour): half-blocks there would paint the default foreground.
pub(crate) fn top_edge_is_half_block() -> bool {
    matches!(THEME.load().bg, Color::Rgb(..))
}

/// Style the message pane's top border: the slab's rim where the header
/// paints it (it repaints the row per cell), or the plain rule where it
/// can't. Geometry is unchanged either way.
pub(crate) fn style_top_edge(block: Block<'_>) -> Block<'_> {
    let t = THEME.load();
    if !top_edge_is_half_block() {
        return block.border_style(Style::default().fg(t.border));
    }
    let mut style = Style::default().fg(t.bg);
    if background_is_opaque() {
        style = style.bg(t.message_background());
    }
    block
        .border_set(border::Set {
            horizontal_top: "\u{2580}",
            ..border::PLAIN
        })
        .border_style(style)
}

#[cfg(test)]
mod tests {
    use super::super::testing::TestHarness;
    use super::*;
    use serial_test::serial;

    const DEV: &str = "v0.11.0 \u{b7} 71502686";

    fn view() -> HeaderView<'static> {
        HeaderView {
            state: HeaderState::Idle,
            status_connecting: false,
            busy_detail: None,
            turn_secs: None,
            anim_ms: 0,
            spinner_frame: 0,
            session: "header-redesign",
            folder: "~/Projects/SynapsCLI",
            watching: None,
            observing: false,
            pills: Vec::new(),
            workers: 0,
            longest_worker_secs: 0,
            fx: PromptFx::default(),
            glow: None,
            build: DEV,
            scroll_back: 0,
        }
    }

    fn draw_with(t: &Theme, v: &HeaderView<'_>, w: u16) -> Buffer {
        let area = Rect::new(0, 0, w, 2);
        let mut buf = Buffer::empty(area);
        paint(&mut buf, area, t, v);
        buf
    }

    fn draw(v: &HeaderView<'_>, w: u16) -> Buffer {
        draw_with(&Theme::builtin_for_test("default"), v, w)
    }

    fn row(buf: &Buffer, y: u16) -> String {
        (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
    }

    /// Column (in cells) of `needle` on row `y`.
    fn col_of(buf: &Buffer, y: u16, needle: &str) -> Option<u16> {
        let line = row(buf, y);
        let i = line.find(needle)?;
        Some(display_width(&line[..i]) as u16)
    }

    /// The live states, with a 12 s turn and 3 workers (longest 4m12s).
    fn thinking() -> HeaderView<'static> {
        HeaderView {
            state: HeaderState::Thinking,
            turn_secs: Some(12),
            workers: 3,
            longest_worker_secs: 252,
            pills: vec![("voice".into(), false)],
            ..view()
        }
    }

    #[test]
    fn the_wordmark_is_synaps_cli_then_the_folder() {
        let buf = draw(&view(), 120);
        let line = row(&buf, 0);
        assert!(
            line.contains("SynapsCLI \u{276f} ~/Projects/SynapsCLI \u{b7} header-redesign"),
            "{line:?}"
        );
        let x = col_of(&buf, 0, "Synaps").unwrap();
        assert_eq!(x, 2, "two cells of padding in from the left edge");
        assert!(
            buf[(x, 0)].modifier.contains(Modifier::BOLD),
            "Synaps is bold"
        );
        assert!(
            !buf[(x + 6, 0)].modifier.contains(Modifier::BOLD),
            "CLI is the second tone"
        );
        assert_ne!(buf[(x, 0)].fg, buf[(x + 6, 0)].fg);
        // Edge to edge: square ends, the body from column 0 to the last,
        // and the rim under all of it.
        assert_eq!(buf[(0, 0)].symbol(), " ");
        assert_eq!(buf[(0, 0)].bg, buf[(10, 0)].bg, "the body reaches column 0");
        assert_eq!(buf[(119, 0)].symbol(), " ");
        assert_eq!(row(&buf, 1), "\u{2580}".repeat(120));
    }

    #[test]
    fn the_folder_shortens_from_the_left() {
        let v = HeaderView {
            folder: "~/Projects/SynapsCLI-wt-cache-keepwarm",
            session: "axon-overnight-cache-fix-followups-round-two",
            ..view()
        };
        let line = row(&draw(&v, 100), 0);
        assert!(
            line.contains("\u{276f} \u{2026}/SynapsCLI-wt-cache-keepwarm \u{b7} axon-overnig"),
            "{line:?}"
        );
        assert!(line.contains('\u{2026}'), "{line:?}");
    }

    #[test]
    fn the_hash_shows_on_dev_builds_only() {
        let line = row(&draw(&view(), 100), 0);
        assert!(line.contains("v0.11.0 \u{b7} 71502686"), "{line:?}");
        let release = HeaderView {
            build: "v0.11.0",
            ..view()
        };
        let line = row(&draw(&release, 100), 0);
        assert!(line.contains("v0.11.0"), "{line:?}");
        assert!(!line.contains("71502686"), "{line:?}");
        assert_eq!(
            line.matches('\u{b7}').count(),
            1,
            "only the session separator: {line:?}"
        );
    }

    /// The block's fill (and the cap and rim under it) is the state colour.
    #[test]
    fn the_block_fill_follows_the_state() {
        let t = Theme::builtin_for_test("default");
        let w = 120u16;
        let cases: Vec<(HeaderView<'static>, &str)> = vec![
            (view(), "v0.11.0"),
            (thinking(), "\u{280b} thinking 12s"),
            (
                HeaderView {
                    state: HeaderState::Agents { done: 0, total: 1 },
                    workers: 1,
                    longest_worker_secs: 3725,
                    ..view()
                },
                "\u{280b} 1 agent 1h02m",
            ),
            (
                HeaderView {
                    state: HeaderState::Busy("reconnecting".into()),
                    busy_detail: Some("4s".into()),
                    ..view()
                },
                "\u{280b} reconnecting 4s",
            ),
            (
                HeaderView {
                    state: HeaderState::Failed("429 rate limited".into()),
                    ..view()
                },
                "\u{2717} 429 rate limited",
            ),
        ];
        let mut fills = Vec::new();
        for (v, text) in &cases {
            let buf = draw_with(&t, v, w);
            let line = row(&buf, 0);
            assert!(line.contains(text), "{text}: {line:?}");
            let (fill, ink, _) = block_colors(&t, &v.state);
            let x = col_of(&buf, 0, text).unwrap();
            // anim_ms 0: the block's sweep is off screen, the fill exact.
            assert_eq!(buf[(x, 0)].bg, color(fill), "{text}");
            assert_eq!(buf[(x, 0)].fg, color(ink), "{text}");
            assert_eq!(
                buf[(w - 1, 0)].bg,
                color(fill),
                "{text}: the block is flush to the right edge"
            );
            assert_eq!(buf[(w - 1, 1)].symbol(), "\u{2580}");
            assert_eq!(buf[(w - 1, 1)].fg, color(fill), "{text}: so is the rim");
            assert_eq!(buf[(x, 1)].fg, color(fill), "{text}");
            // The slab left of the block is not.
            assert_ne!(buf[(10, 0)].bg, color(fill), "{text}");
            fills.push(fill);
        }
        for i in 0..fills.len() {
            for j in i + 1..fills.len() {
                assert_ne!(fills[i], fills[j], "states {i} and {j} share a fill");
            }
        }
    }

    #[test]
    fn dark_ink_reads_on_every_fill_on_every_builtin() {
        let states = [
            HeaderState::Idle,
            HeaderState::Thinking,
            HeaderState::Tool,
            HeaderState::Agents { done: 0, total: 1 },
            HeaderState::Busy("compacting".into()),
            HeaderState::Failed("401".into()),
        ];
        for name in [
            "default",
            "night-city",
            "neon-rain",
            "amber",
            "phosphor",
            "solarized-dark",
            "blood",
            "ocean",
            "rose-pine",
            "nord",
            "dracula",
            "monokai",
            "myx",
            "gruvbox",
            "catppuccin",
            "tokyo-night",
            "sunset",
            "ice",
            "forest",
            "lavender",
            "paper",
        ] {
            let t = Theme::builtin_for_test(name);
            for s in &states {
                let (fill, ink, ink2) = block_colors(&t, s);
                assert!(
                    contrast(ink, fill) >= 5.0 - 0.01,
                    "{name} {s:?}: {:.2}",
                    contrast(ink, fill)
                );
                assert!(contrast(ink2, fill) >= 4.6 - 0.01, "{name} {s:?}");
            }
        }
    }

    #[test]
    fn the_agent_chip_shows_the_count_and_the_longest_clock() {
        let line = row(&draw(&thinking(), 120), 0);
        assert!(line.contains("3 agents 4m12s"), "{line:?}");
        assert!(line.contains("thinking 12s"), "{line:?}");
        let chip = line.find("3 agents").unwrap();
        assert!(
            chip < line.find("thinking").unwrap(),
            "the chip sits left of the block"
        );
        // Only workers run: the block itself says so, no chip.
        let v = HeaderView {
            state: HeaderState::Agents { done: 2, total: 5 },
            turn_secs: None,
            ..thinking()
        };
        let line = row(&draw(&v, 120), 0);
        assert_eq!(line.matches("agents").count(), 1, "{line:?}");
        assert!(line.contains("\u{280b} 3 agents 4m12s"), "{line:?}");
    }

    #[test]
    fn wells_draw_on_both_rows() {
        let v = HeaderView {
            observing: true,
            ..thinking()
        };
        let buf = draw(&v, 140);
        for word in ["voice", "observing", "3 agents"] {
            let x = col_of(&buf, 0, word).unwrap_or_else(|| panic!("{word}: {:?}", row(&buf, 0)));
            // One pad cell, then the well's own end cell.
            for cx in [x - 2, x - 1, x] {
                let well = buf[(cx, 0)].bg;
                assert_ne!(well, buf[(4, 0)].bg, "{word}: the well is not the slab");
                assert_eq!(buf[(cx, 1)].symbol(), "\u{2580}", "{word}");
                assert_eq!(buf[(cx, 1)].fg, well, "{word}: its lower half is the well");
            }
            // Square ends: no half-block caps on row 0.
            assert_eq!(buf[(x - 2, 0)].symbol(), " ", "{word}");
        }
    }

    #[test]
    fn idle_pills_are_hidden() {
        let pill = |name: &str, status: SidecarUiStatus, armed: bool| SidecarPillSnap {
            plugin_id: name.into(),
            display_name: Some(name.into()),
            status,
            armed,
        };
        let pills = [
            pill("idle", SidecarUiStatus::Idle, false),
            pill("loading", SidecarUiStatus::Loading, false),
            pill("armed", SidecarUiStatus::Idle, true),
            pill(
                "active",
                SidecarUiStatus::Active {
                    label: "listening".into(),
                },
                false,
            ),
            pill("broken", SidecarUiStatus::Error("x".into()), false),
        ];
        assert_eq!(
            header_pills(&pills),
            vec![
                ("armed".into(), false),
                ("active".into(), false),
                ("broken".into(), true)
            ]
        );
        let line = row(&draw(&view(), 120), 0);
        assert!(!line.contains("idle"), "{line:?}");
    }

    /// At rest the header is one fixed frame: no sweep, no spinner.
    #[test]
    fn nothing_moves_at_rest() {
        let rest = [
            view(),
            HeaderView {
                state: HeaderState::Failed("401".into()),
                ..view()
            },
            HeaderView {
                pills: vec![("voice".into(), false)],
                observing: true,
                ..view()
            },
        ];
        for v in rest {
            let a = draw(&v, 120);
            let later = HeaderView {
                anim_ms: 7_350,
                spinner_frame: 17,
                ..v
            };
            assert_eq!(a, draw(&later, 120), "{:?}", later.state);
        }
        // A live one does move.
        let a = draw(&thinking(), 120);
        let later = HeaderView {
            anim_ms: 7_350,
            spinner_frame: 17,
            ..thinking()
        };
        assert_ne!(a, draw(&later, 120));
    }

    /// `Synaps` glows while anything runs (a gradient flowing across the
    /// letters), sits still in the header colour at rest, and takes the
    /// error colour after a failed turn.
    #[test]
    fn synaps_glows_while_live_and_rests_otherwise() {
        let letters = |v: &HeaderView<'_>| -> Vec<Color> {
            let buf = draw(v, 120);
            let x = col_of(&buf, 0, "Synaps").unwrap();
            (x..x + 6).map(|cx| buf[(cx, 0)].fg).collect()
        };
        let idle = letters(&view());
        assert!(
            idle.windows(2).all(|p| p[0] == p[1]),
            "one colour at rest: {idle:?}"
        );

        for state in [
            HeaderState::Thinking,
            HeaderState::Tool,
            HeaderState::Agents { done: 0, total: 1 },
            HeaderState::Busy("reconnecting".into()),
        ] {
            let v = HeaderView {
                state: state.clone(),
                anim_ms: 400,
                ..view()
            };
            let now = letters(&v);
            assert!(
                now.windows(2).any(|p| p[0] != p[1]),
                "{state:?}: a gradient: {now:?}"
            );
            assert_ne!(now, idle, "{state:?}");
            let later = letters(&HeaderView {
                anim_ms: 1_650,
                ..v
            });
            assert_ne!(now, later, "{state:?}: it flows");
        }

        let failed = letters(&HeaderView {
            state: HeaderState::Failed("401".into()),
            ..view()
        });
        assert!(failed.windows(2).all(|p| p[0] == p[1]), "still: {failed:?}");
        assert_ne!(failed, idle, "the error colour");
    }

    #[test]
    fn no_overlap_or_panic_at_any_width() {
        let long = HeaderView {
            session: "axon-overnight-cache-fix-followups-round-two-and-then-some",
            folder: "~/Projects/SynapsCLI-wt-cache-keepwarm",
            observing: true,
            pills: vec![("voice".into(), false), ("dictate".into(), true)],
            watching: Some("spike"),
            ..thinking()
        };
        let states = [
            long,
            HeaderView {
                session: "x".repeat(300).leak(),
                ..view()
            },
            HeaderView {
                state: HeaderState::Failed("429 rate limited".into()),
                ..view()
            },
        ];
        for v in &states {
            for w in [80u16, 100, 120, 160] {
                let buf = draw(v, w);
                let line = row(&buf, 0);
                let mut order = vec!["SynapsCLI"];
                if v.observing {
                    order.extend([
                        "voice",
                        "dictate",
                        "observing",
                        "3 ",
                        "4m12s",
                        "thinking 12s",
                    ]);
                }
                let mut at = 0;
                for piece in order {
                    let i = line[at..].find(piece).unwrap_or_else(|| {
                        panic!("w={w}: {piece:?} missing or out of order: {line:?}")
                    });
                    at += i + piece.len();
                }
                if let HeaderState::Failed(_) = v.state {
                    assert!(
                        line.contains("\u{2717} 429 rate limited"),
                        "w={w}: {line:?}"
                    );
                }
                if matches!(v.state, HeaderState::Idle) {
                    assert!(line.contains("v0.11.0"), "w={w}: {line:?}");
                }
                // Two clear cells between the left run and whatever follows.
                let first_well = col_of(&buf, 0, "voice").map(|mut x| {
                    let well = buf[(x, 0)].bg;
                    while buf[(x - 1, 0)].bg == well {
                        x -= 1;
                    }
                    x
                });
                if let Some(wx) = first_well {
                    assert_eq!(buf[(wx - 1, 0)].symbol(), " ", "w={w}: {line:?}");
                    assert_eq!(buf[(wx - 2, 0)].symbol(), " ", "w={w}: {line:?}");
                }
                assert_eq!(row(&buf, 1).chars().count(), w as usize);
            }
            for w in [0u16, 1, 5, 8, 12, 20, 30, 40, 60] {
                let _ = draw(v, w);
            }
        }
    }

    #[test]
    fn a_chrome_without_a_colour_degrades_to_a_flat_row() {
        let mut t = Theme::builtin_for_test("default");
        t.bg = Color::Reset;
        let area = Rect::new(0, 0, 100, 2);
        let mut buf = Buffer::empty(area);
        for x in 0..100 {
            buf[(x, 1)].set_symbol("\u{2500}");
        }
        paint(&mut buf, area, &t, &thinking());
        let line = row(&buf, 0);
        assert!(
            line.contains("SynapsCLI") && line.contains("thinking 12s"),
            "{line:?}"
        );
        assert!(
            !line.contains('\u{2590}'),
            "no half-blocks on the default colour: {line:?}"
        );
        assert_eq!(
            buf[(10, 1)].symbol(),
            "\u{2500}",
            "the pane keeps its plain rule"
        );
        assert_eq!(buf[(10, 0)].bg, Color::Reset);
    }

    #[test]
    fn the_scroll_tab_hangs_off_the_rim_left_of_the_block() {
        let v = HeaderView {
            scroll_back: 12,
            ..view()
        };
        let buf = draw(&v, 100);
        let edge = row(&buf, 1);
        let x = col_of(&buf, 1, "\u{2191}12").expect("tab");
        assert!(x + 4 < col_of(&buf, 0, "v0.11.0").unwrap(), "{edge:?}");
        assert_eq!(buf[(x, 1)].bg, buf[(x, 0)].bg, "the tab is the slab's body");
    }

    /// Row 1 is the message pane's top edge; the header paints it as the
    /// slab's rim over the chrome, whatever the background toggle says.
    #[test]
    #[serial]
    fn the_message_top_edge_is_the_slabs_rim() {
        let prior = background_is_opaque();
        for opaque in [true, false] {
            super::super::theme::set_background_opaque(opaque);
            let mut h = TestHarness::boot();
            let buf = h.render().clone();
            let t = THEME.load();
            assert!(top_edge_is_half_block());
            assert_eq!(buf[(0, 1)].symbol(), "\u{2580}", "opaque={opaque}");
            assert_eq!(
                buf[(0, 0)].bg,
                buf[(10, 0)].bg,
                "full width, opaque={opaque}"
            );
            let edge = &buf[(10, 1)];
            assert_eq!(edge.symbol(), "\u{2580}", "opaque={opaque}");
            assert_eq!(
                edge.fg,
                buf[(10, 0)].bg,
                "the rim is the body, opaque={opaque}"
            );
            // Under the rim is the transcript, not more header.
            let under = if opaque {
                t.message_background()
            } else {
                Color::Reset
            };
            assert_eq!(
                edge.bg, under,
                "the transcript under the rim, opaque={opaque}"
            );
            assert_ne!(
                buf[(0, 0)].bg,
                t.bg,
                "the slab covers column 0, opaque={opaque}"
            );
        }
        super::super::theme::set_background_opaque(prior);
    }

    fn worker(done: bool) -> SubagentSnap {
        SubagentSnap {
            name: "w".into(),
            status: String::new(),
            elapsed_secs: 0.0,
            done,
            tools: 0,
            result: None,
            watched: false,
        }
    }

    #[test]
    fn status_text_wins_and_names_the_state() {
        assert_eq!(
            header_state(true, Some("connecting\u{2026}"), &Activity::Thinking, &[]),
            HeaderState::Connecting
        );
        assert_eq!(
            header_state(false, Some("compacting\u{2026}"), &Activity::Idle, &[]),
            HeaderState::Busy("compacting".into())
        );
        assert_eq!(
            header_state(
                false,
                Some("reconnecting\u{2026} (4s left, attempt 2)"),
                &Activity::Idle,
                &[]
            ),
            HeaderState::Busy("reconnecting".into())
        );
        assert_eq!(
            busy_detail(Some("reconnecting\u{2026} (4s left, attempt 2)")).as_deref(),
            Some("4s")
        );
        assert_eq!(busy_detail(Some("compacting\u{2026}")), None);
    }

    #[test]
    fn streaming_turn_follows_the_activity() {
        let cases = [
            (Activity::Idle, HeaderState::Connecting),
            (Activity::Waiting, HeaderState::Connecting),
            (Activity::Thinking, HeaderState::Thinking),
            (Activity::Writing, HeaderState::Streaming),
            (Activity::Tool, HeaderState::Tool),
            (Activity::Retry, HeaderState::Retry),
        ];
        for (activity, want) in cases {
            assert_eq!(
                header_state(true, None, &activity, &[]),
                want,
                "{activity:?}"
            );
        }
    }

    #[test]
    fn workers_show_when_the_main_turn_is_idle() {
        let ws = [worker(true), worker(false), worker(false)];
        assert_eq!(
            header_state(false, None, &Activity::Idle, &ws),
            HeaderState::Agents { done: 1, total: 3 }
        );
        let ws = [worker(true), worker(true)];
        assert_eq!(
            header_state(false, None, &Activity::Idle, &ws),
            HeaderState::Idle
        );
    }

    #[test]
    fn a_failed_turn_stays_visible_until_the_next_one() {
        let s = header_state(false, None, &Activity::Failed("401".into()), &[]);
        assert_eq!(s, HeaderState::Failed("401".into()));
        assert_eq!(
            header_state(true, None, &Activity::Failed("401".into()), &[]),
            HeaderState::Connecting
        );
    }

    #[test]
    fn build_line_names_version_and_hash() {
        assert_eq!(
            build_line(),
            format!("synaps {}", build()),
            "header and /status agree"
        );
        assert!(build().starts_with(concat!("v", env!("CARGO_PKG_VERSION"))));
    }

    /// 04-tui M8: one build identity for the header and the skew check.
    #[test]
    fn the_header_shows_the_build_the_skew_check_compares() {
        let this = agent_engine::session::transport::BinaryId::this();
        assert_eq!(build(), build_label(&this.version, &this.build));
    }

    #[test]
    fn build_label_drops_the_placeholder_hash() {
        assert_eq!(build_label("0.10.2", "release"), "v0.10.2");
        assert_eq!(build_label("0.10.2", ""), "v0.10.2");
        assert_eq!(build_label("0.10.2", "4a47bc5f"), "v0.10.2 \u{b7} 4a47bc5f");
        assert_eq!(
            build_label("0.10.2", "4a47bc5f-dirty"),
            "v0.10.2 \u{b7} 4a47bc5f-dirty"
        );
    }

    #[test]
    fn elapsed_is_compact() {
        assert_eq!(elapsed(0), "0s");
        assert_eq!(elapsed(42), "42s");
        assert_eq!(elapsed(252), "4m12s");
        assert_eq!(elapsed(3725), "1h02m");
    }

    #[test]
    fn folder_label_abbreviates_home() {
        use std::path::Path;
        let home = Some(Path::new("/home/me"));
        assert_eq!(
            folder_label(Path::new("/home/me/Projects/X"), home),
            "~/Projects/X"
        );
        assert_eq!(folder_label(Path::new("/home/me"), home), "~");
        assert_eq!(folder_label(Path::new("/srv/x"), home), "/srv/x");
        assert_eq!(folder_label(Path::new("/home/meow"), home), "/home/meow");
    }

    #[test]
    fn session_label_prefers_name_then_title_then_id_and_flattens_whitespace() {
        assert_eq!(
            session_label(Some("axon-overnight"), "t", "id"),
            "axon-overnight"
        );
        assert_eq!(
            session_label(Some("  "), "fix\n the\tcache ", "id"),
            "fix the cache"
        );
        assert_eq!(
            session_label(None, "", "20261014-011500-79cc"),
            "20261014-011500-79cc"
        );
    }
}
