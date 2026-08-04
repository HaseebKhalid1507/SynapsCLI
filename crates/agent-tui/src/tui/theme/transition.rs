//! Animated theme transitions — a cross-fade engine for theme changes.
//!
//! Given `(from, to, duration)`, [`ThemeTransition`] produces interpolated
//! [`Theme`] frames at ease-in-out-cubic progress. It owns NO timer: frames
//! are pulled by the existing animation tick (`loop_arms::handle_animation_tick`),
//! whose select! GUARD in `mod.rs` treats `app.theme_transition.is_some()` as
//! "animation active" — exactly how the boot effect and spinner register
//! activity. When the transition lands, [`advance`] applies the EXACT target
//! theme (no float-drift residue) and clears the slot, so the guard goes back
//! to sleep and idle cost returns to zero (#131: a guard that never sleeps
//! again is a permanent 60fps burn).
//!
//! Interpolation rules:
//! - `Color::Rgb` ↔ `Color::Rgb`: per-channel sRGB lerp.
//! - Any non-Rgb endpoint (named / indexed / `Reset`): snap to the target at
//!   `t >= 0.5` — there is no meaningful midpoint between `Reset` and a color.
//! - Non-color fields (`Option<Color>` overrides, `ext_overrides`): snap at
//!   `t >= 0.5` too.
//!
//! Retarget-mid-flight: when a new target arrives while a transition is
//! active (rapid MXC palette updates, fast `/theme` spam), the new transition
//! starts FROM THE CURRENT INTERPOLATED FRAME toward the new target — never
//! queued, never a visible jump.
//!
//! All timing is injected (`Instant` parameters), so tests drive transitions
//! with synthetic instants and never sleep.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use ratatui::style::Color;
use synaps_cli::config::ThemeTransitionMode;

use super::Theme;

// ---------------------------------------------------------------------------
// Config knob (theme_transition = on | off | <ms>)
// ---------------------------------------------------------------------------

/// Cached `theme_transition` knob as a default duration in ms; `0` = off.
/// Same pattern as `theme::BACKGROUND_OPAQUE`: read config once at first use,
/// mutate live from the settings modal via [`set_transition_mode`].
static TRANSITION_DEFAULT_MS: LazyLock<AtomicU64> = LazyLock::new(|| {
    synaps_cli::config::load_config()
        .theme_transition
        .duration_ms()
});

/// Live-apply a new `theme_transition` mode (settings modal hot path).
pub(crate) fn set_transition_mode(mode: ThemeTransitionMode) {
    TRANSITION_DEFAULT_MS.store(mode.duration_ms(), Ordering::Relaxed);
}

/// Resolve the duration a theme change should animate over. `requested` is a
/// per-change advisory (MXC wire `fade_ms`); `None` means "use the default".
/// The knob's `off` (0 ms) state vetoes everything — accessibility wins.
pub(crate) fn effective_duration(requested: Option<Duration>) -> Duration {
    effective_in(TRANSITION_DEFAULT_MS.load(Ordering::Relaxed), requested)
}

/// Pure core of [`effective_duration`] — hermetically testable, no statics.
fn effective_in(default_ms: u64, requested: Option<Duration>) -> Duration {
    match default_ms {
        0 => Duration::ZERO, // knob is off: every change snaps.
        ms => requested.unwrap_or(Duration::from_millis(ms)),
    }
}

/// Map an MXC wire `fade_ms` onto a requested duration. Present values clamp
/// to `0..=2000` ms; `Some(0)` is the spec's "snap, no transition intended";
/// absent (`None`) falls back to the configured default (350 ms).
pub(crate) fn wire_fade_duration(fade_ms: Option<u64>) -> Option<Duration> {
    fade_ms.map(|ms| Duration::from_millis(ms.min(ThemeTransitionMode::MAX_MS)))
}

// ---------------------------------------------------------------------------
// Interpolation math
// ---------------------------------------------------------------------------

/// Ease-in-out cubic. Monotonic on [0,1] with e(0)=0, e(0.5)=0.5, e(1)=1.
fn ease_in_out_cubic(t: f32) -> f32 {
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
    }
}

fn lerp_channel(a: u8, b: u8, t: f32) -> u8 {
    (f32::from(a) + (f32::from(b) - f32::from(a)) * t).round() as u8
}

/// Per-channel sRGB lerp when BOTH endpoints are `Color::Rgb`; any other
/// pairing snaps to the target at `t >= 0.5` (no midpoint exists between
/// `Reset`/named/indexed and anything else).
fn lerp_color(a: Color, b: Color, t: f32) -> Color {
    match (a, b) {
        (Color::Rgb(r0, g0, b0), Color::Rgb(r1, g1, b1)) => Color::Rgb(
            lerp_channel(r0, r1, t),
            lerp_channel(g0, g1, t),
            lerp_channel(b0, b1, t),
        ),
        _ if t >= 0.5 => b,
        _ => a,
    }
}

/// Interpolate every field of a [`Theme`] at (already eased) progress `t`.
/// Color fields lerp per [`lerp_color`]; non-color fields (per-part
/// `Option<Color>` overrides, `ext_overrides`) snap at `t >= 0.5`.
pub(crate) fn lerp_theme(from: &Theme, to: &Theme, t: f32) -> Theme {
    macro_rules! c {
        ($f:ident) => {
            lerp_color(from.$f, to.$f, t)
        };
    }
    macro_rules! snap {
        ($f:ident) => {
            if t >= 0.5 {
                to.$f.clone()
            } else {
                from.$f.clone()
            }
        };
    }
    Theme {
        code_fg: c!(code_fg),
        code_bg: c!(code_bg),
        heading_color: c!(heading_color),
        quote_color: c!(quote_color),
        list_bullet_color: c!(list_bullet_color),
        table_border_color: c!(table_border_color),
        table_header_color: c!(table_header_color),
        table_cell_color: c!(table_cell_color),
        bg: c!(bg),
        message_bg: c!(message_bg),
        border: c!(border),
        border_active: c!(border_active),
        muted: c!(muted),
        user_color: c!(user_color),
        user_bg: c!(user_bg),
        claude_label: c!(claude_label),
        claude_text: c!(claude_text),
        thinking_color: c!(thinking_color),
        tool_label: c!(tool_label),
        tool_param: c!(tool_param),
        tool_result_color: c!(tool_result_color),
        tool_result_ok: c!(tool_result_ok),
        error_color: c!(error_color),
        warning_color: c!(warning_color),
        header_fg: c!(header_fg),
        status_streaming: c!(status_streaming),
        status_ready: c!(status_ready),
        help_fg: c!(help_fg),
        input_fg: c!(input_fg),
        prompt_fg: c!(prompt_fg),
        separator: c!(separator),
        cost_color: c!(cost_color),
        subagent_border: c!(subagent_border),
        subagent_name: c!(subagent_name),
        subagent_status: c!(subagent_status),
        subagent_done: c!(subagent_done),
        subagent_time: c!(subagent_time),
        event_icon: c!(event_icon),
        event_source: c!(event_source),
        event_text: c!(event_text),
        event_critical: c!(event_critical),
        tool_bash: c!(tool_bash),
        tool_read: c!(tool_read),
        tool_write: c!(tool_write),
        tool_edit: c!(tool_edit),
        tool_grep: c!(tool_grep),
        tool_find: c!(tool_find),
        tool_ls: c!(tool_ls),
        tool_subagent: c!(tool_subagent),
        tool_ext: c!(tool_ext),
        tool_generic: c!(tool_generic),
        tool_input_bg: c!(tool_input_bg),
        tool_output_bg: c!(tool_output_bg),
        settings_border: snap!(settings_border),
        settings_title: snap!(settings_title),
        plugins_border: snap!(plugins_border),
        plugins_title: snap!(plugins_title),
        models_border: snap!(models_border),
        models_title: snap!(models_title),
        sidecar_pill: snap!(sidecar_pill),
        ext_overrides: snap!(ext_overrides),
    }
}

// ---------------------------------------------------------------------------
// The transition
// ---------------------------------------------------------------------------

/// One in-flight cross-fade. Held in `App::theme_transition`; its presence IS
/// the "animation active" signal for the tick guard in `mod.rs`.
pub(crate) struct ThemeTransition {
    from: Theme,
    to: Theme,
    start: Instant,
    duration: Duration,
}

impl ThemeTransition {
    /// `duration` must be non-zero — zero-duration changes go straight
    /// through `set_theme` (see [`apply_animated`]) and never allocate one
    /// of these.
    pub(crate) fn new(from: Theme, to: Theme, duration: Duration, now: Instant) -> Self {
        Self {
            from,
            to,
            start: now,
            duration,
        }
    }

    /// Linear progress in [0,1] at `now`.
    fn progress(&self, now: Instant) -> f32 {
        if self.duration.is_zero() {
            return 1.0;
        }
        (now.saturating_duration_since(self.start).as_secs_f32()
            / self.duration.as_secs_f32())
        .clamp(0.0, 1.0)
    }

    /// The interpolated frame at `now` (eased).
    pub(crate) fn frame(&self, now: Instant) -> Theme {
        lerp_theme(&self.from, &self.to, ease_in_out_cubic(self.progress(now)))
    }

    pub(crate) fn is_complete(&self, now: Instant) -> bool {
        self.progress(now) >= 1.0
    }

    /// Consume the transition, yielding the byte-exact target theme.
    pub(crate) fn into_target(self) -> Theme {
        self.to
    }
}

// ---------------------------------------------------------------------------
// Slot operations (App::theme_transition)
// ---------------------------------------------------------------------------

/// Start (or retarget) a transition toward `target`.
///
/// - Effective duration zero (knob off / fade_ms 0) → clear the slot and
///   apply `target` instantly through the normal `set_theme` path.
/// - Slot already active → restart FROM the current interpolated frame
///   toward `target` (never queue, never jump).
/// - Slot idle → start from the currently applied global theme.
///
/// The caller still owns invalidation (`app.invalidate()`), keeping this the
/// same apply discipline `/theme` has always had.
pub(crate) fn apply_animated(
    slot: &mut Option<ThemeTransition>,
    target: Theme,
    requested: Option<Duration>,
    now: Instant,
) {
    apply_animated_over(slot, target, effective_duration(requested), now);
}

/// [`apply_animated`] with the duration already resolved — the hermetically
/// testable core (no config-knob static involved).
fn apply_animated_over(
    slot: &mut Option<ThemeTransition>,
    target: Theme,
    duration: Duration,
    now: Instant,
) {
    if duration.is_zero() {
        *slot = None;
        super::set_theme(target);
        return;
    }
    let from = match slot.take() {
        Some(active) => active.frame(now), // retarget mid-flight
        None => super::THEME.load().as_ref().clone(),
    };
    *slot = Some(ThemeTransition::new(from, target, duration, now));
}

/// Advance the active transition by one tick. Returns the frame to apply via
/// `set_theme`, or `None` when idle (zero cost — the tick guard should not
/// even be awake for us then). On completion the slot is CLEARED and the
/// byte-exact target is returned, so the guard goes inactive next tick.
pub(crate) fn advance(slot: &mut Option<ThemeTransition>, now: Instant) -> Option<Theme> {
    let active = slot.take()?;
    if active.is_complete(now) {
        return Some(active.into_target()); // exact landing, slot stays None.
    }
    let frame = active.frame(now);
    *slot = Some(active);
    Some(frame)
}

// ---------------------------------------------------------------------------
// Tests — hermetic: synthetic instants, no sleeps, no global theme state
// except where a test explicitly owns the snap path.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Two RGB-only endpoint themes with distinct, lerp-checkable values.
    fn theme_a() -> Theme {
        Theme {
            bg: Color::Rgb(0, 0, 0),
            border: Color::Rgb(10, 20, 30),
            border_active: Color::Rgb(100, 100, 100),
            ..Theme::default()
        }
    }

    fn theme_b() -> Theme {
        Theme {
            bg: Color::Rgb(200, 100, 50),
            border: Color::Rgb(210, 220, 230),
            border_active: Color::Rgb(0, 0, 0),
            ..Theme::default()
        }
    }

    fn at(start: Instant, ms: u64) -> Instant {
        start + Duration::from_millis(ms)
    }

    // ---- lerp math ----

    #[test]
    fn lerp_color_endpoints_are_exact() {
        let a = Color::Rgb(13, 37, 240);
        let b = Color::Rgb(200, 3, 77);
        assert_eq!(lerp_color(a, b, 0.0), a);
        assert_eq!(lerp_color(a, b, 1.0), b);
    }

    #[test]
    fn lerp_color_midpoint_is_channelwise_mean() {
        let a = Color::Rgb(0, 100, 200);
        let b = Color::Rgb(100, 200, 250);
        assert_eq!(lerp_color(a, b, 0.5), Color::Rgb(50, 150, 225));
    }

    #[test]
    fn non_rgb_endpoints_snap_at_half() {
        // Reset, named, and indexed endpoints have no midpoint: hold the
        // source strictly below t=0.5, target at and above.
        for (a, b) in [
            (Color::Reset, Color::Rgb(9, 9, 9)),
            (Color::Rgb(9, 9, 9), Color::Reset),
            (Color::Cyan, Color::Rgb(1, 2, 3)),
            (Color::Indexed(42), Color::Indexed(7)),
        ] {
            assert_eq!(lerp_color(a, b, 0.0), a);
            assert_eq!(lerp_color(a, b, 0.49), a);
            assert_eq!(lerp_color(a, b, 0.5), b);
            assert_eq!(lerp_color(a, b, 1.0), b);
        }
    }

    #[test]
    fn lerp_theme_endpoints_are_byte_exact() {
        let (a, b) = (theme_a(), theme_b());
        assert_eq!(lerp_theme(&a, &b, 0.0), a);
        assert_eq!(lerp_theme(&a, &b, 1.0), b);
    }

    #[test]
    fn lerp_theme_snaps_non_color_fields_at_half() {
        let mut a = theme_a();
        a.settings_border = Some(Color::Rgb(1, 1, 1));
        a.ext_overrides
            .insert("ext.x.accent".into(), Color::Rgb(2, 2, 2));
        let mut b = theme_b();
        b.settings_border = None;
        b.ext_overrides
            .insert("ext.x.accent".into(), Color::Rgb(9, 9, 9));

        let before = lerp_theme(&a, &b, 0.49);
        assert_eq!(before.settings_border, a.settings_border);
        assert_eq!(before.ext_overrides, a.ext_overrides);
        let after = lerp_theme(&a, &b, 0.5);
        assert_eq!(after.settings_border, b.settings_border);
        assert_eq!(after.ext_overrides, b.ext_overrides);
    }

    // ---- easing ----

    #[test]
    fn easing_hits_exact_endpoints_and_midpoint() {
        assert_eq!(ease_in_out_cubic(0.0), 0.0);
        assert_eq!(ease_in_out_cubic(1.0), 1.0);
        assert!((ease_in_out_cubic(0.5) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn easing_is_monotonic_on_unit_interval() {
        let mut prev = ease_in_out_cubic(0.0);
        for i in 1..=1000 {
            let e = ease_in_out_cubic(i as f32 / 1000.0);
            assert!(e >= prev, "easing regressed at t={}", i as f32 / 1000.0);
            prev = e;
        }
    }

    // ---- transition lifecycle (synthetic instants only) ----

    #[test]
    fn frame_starts_at_from_and_lands_on_to() {
        let start = Instant::now();
        let tr = ThemeTransition::new(theme_a(), theme_b(), Duration::from_millis(350), start);
        assert_eq!(tr.frame(start), theme_a());
        assert!(!tr.is_complete(start));
        assert!(tr.is_complete(at(start, 350)));
        assert_eq!(tr.frame(at(start, 350)), theme_b());
        // Way past the end: still clamped, still exact.
        assert_eq!(tr.frame(at(start, 5000)), theme_b());
    }

    #[test]
    fn midflight_frame_is_strictly_between_endpoints() {
        let start = Instant::now();
        let tr = ThemeTransition::new(theme_a(), theme_b(), Duration::from_millis(400), start);
        let mid = tr.frame(at(start, 200)); // eased t = 0.5 exactly
        assert_eq!(mid.bg, Color::Rgb(100, 50, 25));
        assert_ne!(mid, theme_a());
        assert_ne!(mid, theme_b());
    }

    #[test]
    fn advance_clears_slot_and_lands_byte_exact_on_completion() {
        let start = Instant::now();
        let mut slot = Some(ThemeTransition::new(
            theme_a(),
            theme_b(),
            Duration::from_millis(350),
            start,
        ));
        // Mid-flight: yields a frame, slot stays occupied (guard stays awake).
        let frame = advance(&mut slot, at(start, 100)).expect("mid-flight frame");
        assert_ne!(frame, theme_b());
        assert!(slot.is_some(), "guard must stay active mid-flight");
        // Landing: byte-exact target, slot cleared → guard goes inactive.
        let landed = advance(&mut slot, at(start, 350)).expect("landing frame");
        assert_eq!(landed, theme_b(), "no float-drift residue allowed");
        assert!(slot.is_none(), "slot must deregister on completion (#131)");
        // Idle: zero work, no frame — the leak check.
        assert_eq!(advance(&mut slot, at(start, 400)), None);
        assert!(slot.is_none());
    }

    #[test]
    fn retarget_midflight_starts_from_current_frame() {
        let start = Instant::now();
        let mut slot = Some(ThemeTransition::new(
            theme_a(),
            theme_b(),
            Duration::from_millis(400),
            start,
        ));
        let now = at(start, 200);
        let current = slot.as_ref().unwrap().frame(now);
        // New target arrives mid-flight (rapid MXC update / /theme spam).
        let c = Theme {
            bg: Color::Rgb(5, 250, 5),
            ..Theme::default()
        };
        apply_animated_over(&mut slot, c.clone(), Duration::from_millis(300), now);
        let tr = slot.as_ref().expect("restarted, not cleared");
        // t=0 of the new transition == the exact frame that was on screen.
        assert_eq!(tr.frame(now), current, "must restart from current frame");
        assert_eq!(tr.frame(at(start, 500)), c, "and land on the new target");
    }

    #[test]
    fn zero_duration_snaps_and_clears_slot() {
        let start = Instant::now();
        let mut slot = Some(ThemeTransition::new(
            theme_a(),
            theme_b(),
            Duration::from_millis(400),
            start,
        ));
        // fade_ms: 0 → "snap, no transition intended" — even mid-flight.
        apply_animated_over(&mut slot, theme_b(), Duration::ZERO, at(start, 100));
        assert!(slot.is_none(), "snap must deregister the guard source");
    }

    // ---- durations (pure helpers; no static knob mutation) ----

    #[test]
    fn effective_duration_off_vetoes_requests() {
        assert_eq!(effective_in(0, None), Duration::ZERO);
        assert_eq!(
            effective_in(0, Some(Duration::from_millis(600))),
            Duration::ZERO,
            "knob off must veto wire fade_ms"
        );
    }

    #[test]
    fn effective_duration_prefers_request_then_default() {
        assert_eq!(
            effective_in(350, Some(Duration::from_millis(600))),
            Duration::from_millis(600)
        );
        assert_eq!(effective_in(350, None), Duration::from_millis(350));
        assert_eq!(effective_in(1200, None), Duration::from_millis(1200));
    }

    #[test]
    fn wire_fade_ms_clamps_and_passes_through() {
        assert_eq!(wire_fade_duration(None), None, "absent → caller default");
        assert_eq!(wire_fade_duration(Some(0)), Some(Duration::ZERO));
        assert_eq!(
            wire_fade_duration(Some(600)),
            Some(Duration::from_millis(600))
        );
        assert_eq!(
            wire_fade_duration(Some(999_999)),
            Some(Duration::from_millis(2000)),
            "clamped to 0..=2000"
        );
    }
}
