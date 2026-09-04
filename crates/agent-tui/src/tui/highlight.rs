use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

use super::text_metrics::{char_width, width as display_width};
use super::theme::THEME;

/// Clamp a `Line` to fit within `width` terminal columns.
/// Walks spans left-to-right, truncating/dropping once the budget is exceeded.
/// Avoids rendering artifacts from lines that exceed terminal width.
pub(crate) fn clamp_line(line: Line<'static>, width: usize) -> Line<'static> {
    if width == 0 {
        return Line::from("");
    }
    let mut remaining = width;
    let mut clamped: Vec<Span<'static>> = Vec::new();
    for span in line.spans {
        let span_width = display_width(span.content.as_ref());
        if remaining == 0 {
            break;
        }
        if span_width <= remaining {
            remaining -= span_width;
            clamped.push(span);
        } else {
            // Truncate this span by display width. Multi-column chars must be
            // counted by terminal cells, not scalar values, or a supposedly
            // clamped line can still overrun the viewport and leave artifacts.
            let mut used = 0;
            let mut truncated = String::new();
            for ch in span.content.chars() {
                let ch_width = char_width(ch);
                if used + ch_width > remaining {
                    break;
                }
                used += ch_width;
                truncated.push(ch);
            }
            clamped.push(Span::styled(truncated, span.style));
            remaining = 0;
        }
    }
    Line::from(clamped)
}

/// Curated syntect dump written by `build.rs` (PLAN-phase4 §3 C1): the
/// `CURATED` grammars plus their include closure. Colours are golden-tested
/// identical to the full default set (`tests/highlight_curated.rs`).
static CURATED_DUMP: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/curated_newlines.packdump"));

/// `SYNAPS_TUI_SYNTECT=full` → syntect's full default set (kill-switch for a
/// language missing from the curated list). Anything else → curated dump.
fn load_syntax_set() -> SyntaxSet {
    if std::env::var("SYNAPS_TUI_SYNTECT").is_ok_and(|v| v == "full") {
        return SyntaxSet::load_defaults_newlines();
    }
    // The dump is produced by this same build, so a decode failure is a build
    // bug — fall back to the full set rather than lose highlighting.
    syntect::dumps::from_uncompressed_data(CURATED_DUMP)
        .unwrap_or_else(|_| SyntaxSet::load_defaults_newlines())
}

/// Everything syntect needs to highlight, loaded together and dropped
/// together (PLAN-phase4 §3 C2).
struct Loaded {
    set: SyntaxSet,
    theme: Theme,
}

impl Loaded {
    fn theme(&self) -> &Theme {
        &self.theme
    }
}

/// The one syntect theme the palette maps code colours from. Only this theme
/// is kept (PLAN-phase4 §3 C3); the rest of `ThemeSet::load_defaults()` is
/// dropped on the spot. Falls back to the first default theme if the name
/// ever disappears from syntect's bundle — never panics.
const CODE_THEME: &str = "base16-ocean.dark";

fn load_theme() -> Theme {
    let mut themes = ThemeSet::load_defaults().themes;
    themes
        .remove(CODE_THEME)
        .or_else(|| themes.into_values().next())
        .unwrap_or_default()
}

/// Lazily loaded, idle-evictable syntect state. Rendered `Line<'static>`s own
/// their spans, so dropping this never touches what is on screen — only the
/// next highlight call pays a reload (`hl_first` ladder stage, `load_ms=`).
struct SyntaxCache {
    loaded: Mutex<Option<Arc<Loaded>>>,
    /// Millis since `EPOCH` of the last `syntax_set()` call.
    last_use: AtomicU64,
    /// Number of loads so far (1 = first touch, >1 = reload after eviction).
    loads: AtomicU64,
}

static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);
static CACHE: SyntaxCache = SyntaxCache {
    loaded: Mutex::new(None),
    last_use: AtomicU64::new(0),
    loads: AtomicU64::new(0),
};

fn now_ms() -> u64 {
    EPOCH.elapsed().as_millis() as u64
}

/// The syntect state, loading it on first use and stamping `last_use`.
fn syntax_set() -> Arc<Loaded> {
    CACHE.last_use.store(now_ms(), Ordering::Relaxed);
    let mut guard = CACHE.loaded.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(loaded) = guard.as_ref() {
        return Arc::clone(loaded);
    }
    #[cfg(any(test, feature = "testing"))]
    SYNTAX_SET_TOUCHED.store(true, Ordering::Relaxed);
    let t0 = Instant::now();
    let loaded = Arc::new(Loaded {
        set: load_syntax_set(),
        theme: load_theme(),
    });
    let loads = CACHE.loads.fetch_add(1, Ordering::Relaxed) + 1;
    agent_core::core::memstat::ladder(
        "hl_first",
        &format!("load_ms={} loads={}", t0.elapsed().as_millis(), loads),
    );
    *guard = Some(Arc::clone(&loaded));
    loaded
}

/// `SYNAPS_TUI_SYNTECT_IDLE_SECS` override for the idle-eviction period:
/// `None` = not set (use the caller's default), `Some(None)` = `0` (never),
/// `Some(Some(d))` = evict after `d`.
fn idle_override() -> Option<Option<Duration>> {
    static OVERRIDE: OnceLock<Option<Option<Duration>>> = OnceLock::new();
    *OVERRIDE.get_or_init(|| {
        let secs: u64 = std::env::var("SYNAPS_TUI_SYNTECT_IDLE_SECS")
            .ok()?
            .trim()
            .parse()
            .ok()?;
        Some((secs > 0).then(|| Duration::from_secs(secs)))
    })
}

/// Default idle period before the syntect state is dropped (PLAN-phase4 §8.5).
#[allow(dead_code)]
pub(crate) const SYNTECT_IDLE_DEFAULT: Duration = Duration::from_secs(120);

/// Drop the syntect `SyntaxSet`/`Theme` when no highlight call has used
/// them for `idle` (PLAN-phase4 §3 C2). `SYNAPS_TUI_SYNTECT_IDLE_SECS`
/// overrides `idle` when set (`0` = never evict). Returns `true` only when
/// something was actually dropped; a set that is currently borrowed by a
/// highlight call (another `Arc` alive) is left alone. The next highlight
/// call reloads lazily and emits another `hl_first` ladder line.
#[allow(dead_code)] // A's idle arm is the caller
pub(crate) fn evict_if_idle(idle: Duration) -> bool {
    let idle = match idle_override() {
        Some(None) => return false,
        Some(Some(d)) => d,
        None => idle,
    };
    let mut guard = CACHE.loaded.lock().unwrap_or_else(|e| e.into_inner());
    let Some(loaded) = guard.as_ref() else {
        return false;
    };
    let since = now_ms().saturating_sub(CACHE.last_use.load(Ordering::Relaxed));
    if Duration::from_millis(since) < idle || Arc::strong_count(loaded) > 1 {
        return false;
    }
    *guard = None;
    true
}

/// Whether the syntect state is currently resident.
#[cfg(any(test, feature = "testing"))]
#[allow(dead_code)]
pub(crate) fn is_loaded() -> bool {
    CACHE
        .loaded
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some()
}

// ── Test-only highlight instrumentation (Slice 0 / T241) ─────────────────────
//
// Two counters:
//   HIGHLIGHT_CALLS   — bumped once per syntect highlight_line() call-site entry
//                       (each of highlight_code_block / highlight_tool_code /
//                        highlight_read_output touching SYNTAX_SET increments this)
//   SYNTAX_SET_TOUCHED — latched true the first time SYNTAX_SET is force-initialized
//                        (i.e. the LazyLock closure runs). Loading syntect defaults
//                        is the single largest first-touch memory cost (~10–20 MB);
//                        off-screen code fences must NOT trigger it on the first frame
//                        after Slice 3 lands.
//
// All three items compile to zero bytes and zero cycles in production.
// Reset via `highlight_reset_counters()`; read via the `highlight_*` accessors.
#[cfg(any(test, feature = "testing"))]
pub(crate) static HIGHLIGHT_CALLS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(any(test, feature = "testing"))]
pub(crate) static SYNTAX_SET_TOUCHED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Reset both counters to zero — call before a measurement frame.
/// Also clears `SYNTAX_SET_TOUCHED` (which is a latch, not a rate counter,
/// so it only makes sense to clear between isolated test runs).
#[cfg(any(test, feature = "testing"))]
pub(crate) fn highlight_reset_counters() {
    HIGHLIGHT_CALLS.store(0, std::sync::atomic::Ordering::Relaxed);
    SYNTAX_SET_TOUCHED.store(false, std::sync::atomic::Ordering::Relaxed);
}

/// Read `HIGHLIGHT_CALLS`.
#[cfg(any(test, feature = "testing"))]
pub(crate) fn highlight_call_count() -> usize {
    HIGHLIGHT_CALLS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Read `SYNTAX_SET_TOUCHED`.
#[cfg(any(test, feature = "testing"))]
pub(crate) fn syntax_set_was_touched() -> bool {
    SYNTAX_SET_TOUCHED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Internal helper: bump HIGHLIGHT_CALLS once per syntect highlight session entry.
#[cfg(any(test, feature = "testing"))]
#[inline(always)]
fn note_highlight_call() {
    HIGHLIGHT_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}
#[cfg(not(any(test, feature = "testing")))]
#[inline(always)]
fn note_highlight_call() {}

/// Highlight a code block using syntect
pub(crate) fn highlight_code_block(code: &str, lang: &str, prefix: &str) -> Vec<Line<'static>> {
    note_highlight_call();
    let loaded = syntax_set();
    let ss = &loaded.set;
    let theme = loaded.theme();

    let syntax = ss
        .find_syntax_by_token(lang)
        .unwrap_or_else(|| ss.find_syntax_plain_text());

    let mut h = HighlightLines::new(syntax, theme);
    let mut lines: Vec<Line> = Vec::new();

    for line in LinesWithEndings::from(code) {
        let ranges = h.highlight_line(line, ss).unwrap_or_default();
        let mut spans: Vec<Span> = Vec::new();
        spans.push(Span::styled(
            format!("{}  \u{2502} ", prefix),
            Style::default().fg(THEME.load().muted),
        ));
        for (style, text) in ranges {
            let fg = Color::Rgb(style.foreground.r, style.foreground.g, style.foreground.b);
            let content = text.trim_end_matches('\n').to_string();
            if !content.is_empty() {
                spans.push(Span::styled(
                    content,
                    Style::default().fg(fg).bg(THEME.load().code_bg),
                ));
            }
        }
        lines.push(Line::from(spans));
    }
    lines
}

/// Try to syntax-highlight a single tool output line.
/// Highlight code lines for tool params (write content, edit old/new) — clean style matching read output
pub(crate) fn highlight_tool_code(
    lines: &[&str],
    ext: &str,
    margin: &str,
    marker: &str,
    marker_color: Color,
) -> Vec<Line<'static>> {
    note_highlight_call();
    let loaded = syntax_set();
    let ss = &loaded.set;
    let theme = loaded.theme();

    let syntax = ss
        .find_syntax_by_extension(ext)
        .unwrap_or_else(|| ss.find_syntax_plain_text());

    let mut h = HighlightLines::new(syntax, theme);
    let mut result = Vec::new();

    // Determine tint based on marker (red for old, green for new, neutral for content)
    let tint = match marker {
        "−" => (40i16, -60i16, -60i16), // shift toward red: boost red, crush green/blue
        "+" => (-15i16, 10i16, -15i16), // shift toward green: reduce red/blue
        _ => (0i16, 0i16, 0i16),        // neutral for write content
    };

    for (i, line) in lines.iter().enumerate() {
        let code_with_nl = format!("{}\n", line);
        let ranges = h.highlight_line(&code_with_nl, ss).unwrap_or_default();

        let mut spans = vec![Span::styled(
            format!("{}    {:>3} {} ", margin, i + 1, marker),
            Style::default().fg(marker_color),
        )];
        for (sty, text) in ranges {
            let r = (sty.foreground.r as i16 + tint.0).clamp(0, 255) as u8;
            let g = (sty.foreground.g as i16 + tint.1).clamp(0, 255) as u8;
            let b = (sty.foreground.b as i16 + tint.2).clamp(0, 255) as u8;
            let fg = Color::Rgb(r, g, b);
            let content = text.trim_end_matches('\n').to_string();
            if !content.is_empty() {
                spans.push(Span::styled(content, Style::default().fg(fg)));
            }
        }
        result.push(Line::from(spans));
    }

    result
}

/// Highlight bash tool output with blue tint and pattern detection
pub(crate) fn highlight_bash_output(lines: &[&str], margin: &str) -> Vec<Line<'static>> {
    let mut result = Vec::new();

    for raw_line in lines {
        // Replace tabs with spaces — ratatui doesn't handle \t correctly and causes overlap artifacts
        let line = raw_line.replace('\t', "    ");
        let trimmed = line.trim();
        let mut spans = vec![Span::styled(
            format!("{}       ", margin),
            Style::default().fg(THEME.load().tool_result_color),
        )];

        if trimmed.is_empty() {
            result.push(Line::from(spans));
            continue;
        }

        // Detect patterns and colorize
        let lc = trimmed.to_ascii_lowercase();
        if lc.starts_with("error") || lc.starts_with("fatal") {
            // Errors → red
            spans.push(Span::styled(
                line.to_string(),
                Style::default().fg(THEME.load().error_color),
            ));
        } else if lc.starts_with("warning") || lc.starts_with("warn") {
            // Warnings → yellow
            spans.push(Span::styled(
                line.to_string(),
                Style::default().fg(THEME.load().warning_color),
            ));
        } else if trimmed.starts_with("✅")
            || trimmed.starts_with("ok")
            || trimmed.starts_with("OK")
            || trimmed.starts_with("done")
            || trimmed.starts_with("Done")
            || trimmed.starts_with("success")
        {
            // Success → green with blue tint
            spans.push(Span::styled(
                line.to_string(),
                Style::default().fg(THEME.load().tool_result_ok),
            ));
        } else {
            // Default: blue-tinted with smart coloring
            let mut remaining = line.as_str();
            while !remaining.is_empty() {
                // Find paths (contain /)
                if let Some(slash_pos) = remaining.find('/') {
                    // Output text before the path
                    if slash_pos > 0 {
                        let before = &remaining[..slash_pos];
                        // Find the start of the path (walk back to whitespace)
                        let path_start = before
                            .rfind(|c: char| c.is_whitespace())
                            .map(|p| p + before[p..].chars().next().unwrap().len_utf8())
                            .unwrap_or(0);
                        if path_start > 0 {
                            spans.push(Span::styled(
                                remaining[..path_start].to_string(),
                                Style::default().fg(THEME.load().tool_result_color),
                            ));
                        }
                        // Path portion
                        let after_slash = &remaining[path_start..];
                        let path_end = after_slash
                            .find(|c: char| c.is_whitespace() || c == ':' || c == ')' || c == ']')
                            .unwrap_or(after_slash.len());
                        // Guard: if path_end is 0, we'd loop forever — consume at least 1 char
                        if path_end == 0 {
                            let first_char_len = after_slash
                                .chars()
                                .next()
                                .map(|c| c.len_utf8())
                                .unwrap_or(1);
                            spans.push(Span::styled(
                                after_slash[..first_char_len].to_string(),
                                Style::default().fg(THEME.load().tool_result_color),
                            ));
                            remaining = &after_slash[first_char_len..];
                        } else {
                            spans.push(Span::styled(
                                after_slash[..path_end].to_string(),
                                Style::default().fg(THEME.load().tool_label),
                            ));
                            remaining = &after_slash[path_end..];
                        }
                    } else {
                        let path_end = remaining
                            .find(|c: char| c.is_whitespace() || c == ':' || c == ')' || c == ']')
                            .unwrap_or(remaining.len());
                        // Guard: if path_end is 0, consume at least 1 char to avoid infinite loop
                        if path_end == 0 {
                            let first_char_len =
                                remaining.chars().next().map(|c| c.len_utf8()).unwrap_or(1);
                            spans.push(Span::styled(
                                remaining[..first_char_len].to_string(),
                                Style::default().fg(THEME.load().tool_result_color),
                            ));
                            remaining = &remaining[first_char_len..];
                        } else {
                            spans.push(Span::styled(
                                remaining[..path_end].to_string(),
                                Style::default().fg(THEME.load().tool_label),
                            ));
                            remaining = &remaining[path_end..];
                        }
                    }
                } else {
                    // No more paths — output the rest with blue tint
                    spans.push(Span::styled(
                        remaining.to_string(),
                        Style::default().fg(THEME.load().tool_result_color),
                    ));
                    break;
                }
            }
        }

        result.push(Line::from(spans));
    }

    result
}

/// Try to highlight a grep output line (filepath:linenum:content)
pub(crate) fn try_highlight_grep_line(line: &str, margin: &str) -> Option<Vec<Span<'static>>> {
    // Grep format: filepath:linenum:content  or  filepath-linenum-content (context)
    // Also: filepath:linenum:  (empty match line)
    let first_colon = line.find(':')?;
    let filepath = &line[..first_colon];

    // Filepath should look like a path (contain / or .)
    if !filepath.contains('/') && !filepath.contains('.') {
        return None;
    }

    let rest = &line[first_colon + 1..];
    let second_sep = rest.find([':', '-'])?;
    let linenum = &rest[..second_sep];

    // Line number should be numeric
    if !linenum.chars().all(|c| c.is_ascii_digit()) || linenum.is_empty() {
        return None;
    }

    let sep_char = rest.as_bytes()[second_sep] as char;
    let content = if second_sep + 1 < rest.len() {
        &rest[second_sep + 1..]
    } else {
        ""
    };

    let is_context = sep_char == '-';

    Some(vec![
        Span::styled(
            format!("{}       ", margin),
            Style::default().fg(THEME.load().tool_result_color),
        ),
        Span::styled(
            filepath.to_string(),
            Style::default().fg(THEME.load().tool_label),
        ),
        Span::styled(":", Style::default().fg(THEME.load().muted)),
        Span::styled(
            linenum.to_string(),
            Style::default().fg(THEME.load().list_bullet_color),
        ),
        Span::styled(
            format!("{}", sep_char),
            Style::default().fg(THEME.load().muted),
        ),
        Span::styled(
            content.to_string(),
            if is_context {
                Style::default().fg(THEME.load().muted)
            } else {
                Style::default().fg(THEME.load().tool_result_color)
            },
        ),
    ])
}

/// Check if tool output looks like read tool output (line-numbered with tabs)
pub(crate) fn is_read_tool_output(lines: &[&str]) -> bool {
    if lines.is_empty() {
        return false;
    }
    // Check first few non-empty lines for "number\tcontent" pattern
    let mut checked = 0;
    let mut matches = 0;
    for line in lines.iter().take(10) {
        if line.trim().is_empty() {
            continue;
        }
        checked += 1;
        if let Some(tab_idx) = line.find('\t') {
            if line[..tab_idx].trim().chars().all(|c| c.is_ascii_digit())
                && !line[..tab_idx].trim().is_empty()
            {
                matches += 1;
            }
        }
    }
    checked > 0 && matches * 2 >= checked // At least half the lines match
}

/// Highlight read tool output as a code block using syntect
pub(crate) fn highlight_read_output(
    lines: &[&str],
    ext: &str,
    margin: &str,
) -> Option<Vec<Line<'static>>> {
    note_highlight_call();
    let loaded = syntax_set();
    let ss = &loaded.set;
    let theme = loaded.theme();

    let syntax = if !ext.is_empty() {
        ss.find_syntax_by_extension(ext)
            .unwrap_or_else(|| ss.find_syntax_plain_text())
    } else {
        ss.find_syntax_plain_text()
    };

    // If plain text, don't bother highlighting
    if syntax.name == "Plain Text" && ext.is_empty() {
        return None;
    }

    let mut h = HighlightLines::new(syntax, theme);
    let mut result = Vec::new();

    for line in lines {
        let (line_num, code) = if let Some(tab_idx) = line.find('\t') {
            let num = line[..tab_idx].trim();
            let code = &line[tab_idx + 1..];
            (num.to_string(), code)
        } else {
            (String::new(), *line)
        };

        let code_with_nl = format!("{}\n", code);
        let ranges = h.highlight_line(&code_with_nl, ss).unwrap_or_default();

        let mut spans = vec![Span::styled(
            format!("{}     {:>4} \u{2502} ", margin, line_num),
            Style::default().fg(THEME.load().muted),
        )];
        for (sty, text) in ranges {
            // Slight cool tint for read output to differentiate from edit
            let r = (sty.foreground.r as i16 - 5).clamp(0, 255) as u8;
            let g = (sty.foreground.g as i16).clamp(0, 255) as u8;
            let b = (sty.foreground.b as i16 + 10).clamp(0, 255) as u8;
            let fg = Color::Rgb(r, g, b);
            let content = text.trim_end_matches('\n').to_string();
            if !content.is_empty() {
                spans.push(Span::styled(content, Style::default().fg(fg)));
            }
        }
        result.push(Line::from(spans));
    }

    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{
        style::Style,
        text::{Line, Span},
    };

    #[test]
    fn clamp_line_truncates_by_display_width_not_char_count() {
        let line = Line::from(vec![Span::styled("ab漢字c", Style::default())]);

        let clamped = clamp_line(line, 5);
        let rendered: String = clamped
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();

        assert_eq!(rendered, "ab漢");
        assert_eq!(display_width(rendered.as_str()), 4);
    }

    #[test]
    fn clamp_line_preserves_spans_within_display_width_budget() {
        let line = Line::from(vec![
            Span::styled("ab", Style::default()),
            Span::styled("漢", Style::default()),
            Span::styled("cd", Style::default()),
        ]);

        let clamped = clamp_line(line, 4);
        let rendered: String = clamped
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();

        assert_eq!(rendered, "ab漢");
        assert_eq!(display_width(rendered.as_str()), 4);
    }

    fn evict_now() -> bool {
        // Other tests in this binary may hold the Arc for a moment.
        for _ in 0..200 {
            if evict_if_idle(Duration::ZERO) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    /// PLAN-phase4 §8.4 C1/C2 numbers. Run in release, once per mode:
    /// `cargo test -p synaps-tui --release --lib -- --ignored measure_hl_first --nocapture`
    /// and again with `SYNAPS_TUI_SYNTECT=full`. Prints RssAnon before the
    /// first highlight, after it (`hl_first` step), after eviction, and after
    /// the reload, with `load_ms` for both loads.
    #[test]
    #[ignore]
    #[serial_test::serial(highlight_cache)]
    fn measure_hl_first() {
        use agent_core::core::memstat::self_snapshot;
        let code = include_str!("../../tests/fixtures/highlight/sample.rs.txt");
        let py = include_str!("../../tests/fixtures/highlight/sample.py.txt");
        let _ = evict_now();
        let mode = std::env::var("SYNAPS_TUI_SYNTECT").unwrap_or_else(|_| "curated".into());
        let base = self_snapshot().rss_anon_kb;
        let t0 = Instant::now();
        let _ = highlight_code_block(code, "rust", "");
        let first_ms = t0.elapsed().as_millis();
        let after_first = self_snapshot().rss_anon_kb;
        let _ = highlight_code_block(py, "python", "");
        let _ = highlight_code_block(code, "rust", "");
        let after_more = self_snapshot().rss_anon_kb;
        assert!(evict_now());
        let after_evict = self_snapshot().rss_anon_kb;
        let t1 = Instant::now();
        let _ = highlight_code_block(code, "rust", "");
        let reload_ms = t1.elapsed().as_millis();
        let after_reload = self_snapshot().rss_anon_kb;
        eprintln!(
            "measure_hl_first mode={mode} dump_bytes={} rss_anon_kb: base={base} hl_first={after_first} (+{}) +py={after_more} (+{}) evicted={after_evict} ({:+}) reload={after_reload} (+{}) first_hl_ms={first_ms} reload_hl_ms={reload_ms}",
            CURATED_DUMP.len(),
            after_first as i64 - base as i64,
            after_more as i64 - base as i64,
            after_evict as i64 - after_more as i64,
            after_reload as i64 - after_evict as i64,
        );
    }

    #[test]
    fn single_code_theme_is_the_palette_theme() {
        let full = ThemeSet::load_defaults();
        assert_eq!(load_theme(), full.themes[CODE_THEME]);
    }

    #[test]
    #[serial_test::serial(highlight_cache)]
    fn evict_if_idle_drops_and_reloads_with_identical_output() {
        let code = "fn main() {\n    let x: u32 = 0x1f;\n}\n";
        let before = highlight_code_block(code, "rust", "");
        assert!(is_loaded());
        assert!(evict_now(), "evict_if_idle(0) must drop the idle set");
        assert!(!is_loaded());
        assert!(!evict_if_idle(Duration::ZERO), "nothing to evict twice");
        let after = highlight_code_block(code, "rust", "");
        assert!(is_loaded());
        assert_eq!(before, after);
        assert!(before.iter().flat_map(|l| l.spans.iter()).count() > 3);
    }

    #[test]
    #[serial_test::serial(highlight_cache)]
    fn evict_if_idle_refuses_while_in_use_or_not_idle() {
        let held = syntax_set();
        assert!(
            !evict_if_idle(Duration::ZERO),
            "borrowed set must not be dropped"
        );
        drop(held);
        let _ = syntax_set();
        assert!(!evict_if_idle(Duration::from_secs(3600)), "not idle yet");
        assert!(is_loaded());
    }

    #[test]
    fn highlight_bash_output_handles_nbsp_in_path_context() {
        // Regression: \u{a0} (NBSP) is whitespace but 2 bytes in UTF-8.
        // rfind(whitespace) + 1 landed mid-char → panic on slice.
        // This is the exact crash string from S182.
        let lines = vec![") \\| [395 comments](https://news.ycombinator.com/item?id=47961319) |"];
        let result = highlight_bash_output(&lines, "");
        assert!(!result.is_empty());
    }

    #[test]
    fn highlight_bash_output_handles_nbsp_before_slash() {
        // NBSP (\u{00a0}) right before a slash — rfind finds NBSP as whitespace,
        // +1 byte would land inside the 2-byte char. Must use char boundary.
        let line_with_nbsp = "text\u{00a0}/some/path here";
        let lines = vec![line_with_nbsp];
        let result = highlight_bash_output(&lines, "");
        assert!(!result.is_empty());
    }

    #[test]
    fn highlight_bash_output_handles_multibyte_at_path_boundary() {
        // Multi-byte char (é = 2 bytes) near a slash
        let lines = vec!["café/menu"];
        let result = highlight_bash_output(&lines, "");
        assert!(!result.is_empty());
    }
}
