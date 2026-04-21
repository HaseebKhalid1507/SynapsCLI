use ratatui::prelude::*;
use crate::app::App;
use super::theme;

const POST_LINES: &[&str] = &[
    "",
    "  ARASAKA SYSTEMS BIOS v2.077",
    "  ────────────────────────────",
    "",
    "  Memory Test: 404K ......... OK",
    "  Neural Interface .......... OK",
    "  Credit Chip Reader ........ OK",
    "  RNG Entropy Pool .......... OK",
    "  House Edge Calibration .... OK",
    "",
    "  ╔══════════════════════════════════════╗",
    "  ║                                      ║",
    "  ║     ██████╗  █████╗ ███╗   ███╗      ║",
    "  ║    ██╔════╝ ██╔══██╗████╗ ████║      ║",
    "  ║    ██║  ███╗███████║██╔████╔██║      ║",
    "  ║    ██║   ██║██╔══██║██║╚██╔╝██║      ║",
    "  ║    ╚██████╔╝██║  ██║██║ ╚═╝ ██║      ║",
    "  ║     ╚═════╝ ╚═╝  ╚═╝╚═╝     ╚═╝      ║",
    "  ║        B  L  E  R  S    D  E  N       ║",
    "  ║                                      ║",
    "  ╚══════════════════════════════════════╝",
    "",
    "  Establishing uplink .............",
    "  CONNECTION ESTABLISHED ■",
    "",
    "  > Press any key to enter the den_",
];

pub fn draw_boot(f: &mut Frame, app: &App, area: Rect) {
    let buf = f.buffer_mut();
    let w = area.width as usize;
    let h = area.height as usize;
    let elapsed_ms = app.elapsed.as_millis() as usize;

    // Fill background with black
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            buf[(x, y)].set_char(' ').set_bg(theme::BG);
        }
    }

    // ── SCAN LINE EFFECT ────────────────────────────────────────────
    // A bright scan line sweeps down the screen during boot
    let scan_y = ((elapsed_ms / 30) % (h * 2)) as u16;
    if scan_y < h as u16 {
        let sy = area.top() + scan_y;
        if sy < area.bottom() {
            for x in area.left()..area.right() {
                buf[(x, sy)].set_bg(Color::Rgb(15, 20, 15));
            }
            // Trailing glow
            if scan_y > 0 {
                let sy2 = area.top() + scan_y - 1;
                for x in area.left()..area.right() {
                    buf[(x, sy2)].set_bg(Color::Rgb(8, 12, 8));
                }
            }
        }
    }

    // ── POST TEXT (progressive reveal) ──────────────────────────────
    let lines_to_show = (elapsed_ms / 90).min(POST_LINES.len());

    for (i, &line) in POST_LINES.iter().enumerate().take(lines_to_show) {
        let ay = area.top() + i as u16 + 1;
        if ay >= area.bottom() { break; }

        let color = if line.contains("OK") {
            theme::GREEN
        } else if line.contains("CONNECTION") {
            theme::CYAN
        } else if line.contains("██") || line.contains("╔╝") || line.contains("═╝") {
            theme::AMBER
        } else if line.contains("B  L  E  R  S") {
            theme::AMBER
        } else if line.contains("═") || line.contains("║") || line.contains("╔") || line.contains("╚") {
            theme::AMBER_DIM
        } else if line.contains("────") {
            theme::DARK_GRAY
        } else if line.contains("Press any") {
            // Blink
            if (app.frame / 30) % 2 == 0 { theme::GREEN } else { theme::BG }
        } else if line.contains("uplink") {
            // Animate the dots
            let dots = (elapsed_ms / 200) % 4;
            let dot_str: String = ".".repeat(dots).chars().chain(std::iter::repeat(' ').take(3 - dots)).collect();
            let animated = line.replace("...", &format!("{}", dot_str));
            // Draw manually with animation
            let mut cx = area.left();
            for ch in animated.chars() {
                if cx >= area.right() { break; }
                buf[(cx, ay)].set_char(ch).set_fg(theme::GRAY);
                cx += 1;
            }
            continue;
        } else {
            theme::GRAY
        };

        // Draw the line
        let mut cx = area.left();
        for ch in line.chars() {
            if cx >= area.right() { break; }
            buf[(cx, ay)].set_char(ch).set_fg(color);
            cx += 1;
        }

        // Glitch effect — randomly corrupt a few chars in recently revealed lines
        if i + 2 >= lines_to_show && elapsed_ms % 7 < 3 {
            let glitch_x = ((app.frame as usize * 31 + i * 17) % w.max(1)) as u16 + area.left();
            if glitch_x < area.right() && ay < area.bottom() {
                let glitch_chars = ['█', '▓', '░', '╳', '▒', '■', '¤'];
                let gch = glitch_chars[(app.frame as usize + i) % glitch_chars.len()];
                buf[(glitch_x, ay)].set_char(gch).set_fg(theme::CYAN_DIM);
            }
        }
    }

    // ── Blinking cursor at current line ─────────────────────────────
    if lines_to_show < POST_LINES.len() {
        let cursor_y = area.top() + lines_to_show as u16 + 1;
        if cursor_y < area.bottom() {
            let cursor_x = area.left() + 2;
            if (app.frame / 15) % 2 == 0 {
                buf[(cursor_x, cursor_y)].set_char('█').set_fg(theme::AMBER);
            }
        }
    }

    // ── STATIC NOISE at bottom ──────────────────────────────────────
    // Random noise in the last few rows for atmosphere
    let noise_chars = ['░', '▒', '▓', '·', ':', '.', ' ', ' ', ' ', ' '];
    for y in (h.saturating_sub(3))..h {
        let ay = area.top() + y as u16;
        if ay >= area.bottom() { continue; }
        for x in 0..w {
            let ax = area.left() + x as u16;
            if ax >= area.right() { continue; }
            let cell = &buf[(ax, ay)];
            if cell.symbol() == " " {
                let seed = (app.frame as usize).wrapping_mul(x + 1).wrapping_add(y * 37);
                let nch = noise_chars[seed % noise_chars.len()];
                if nch != ' ' {
                    buf[(ax, ay)].set_char(nch).set_fg(Color::Rgb(20, 20, 25));
                }
            }
        }
    }
}
