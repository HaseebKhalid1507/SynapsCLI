use ratatui::prelude::*;
use ratatui::buffer::Buffer;
use crate::app::App;
use super::theme;

/// The hub selection index
const TABLES: &[&str] = &["BLACKJACK", "SLOTS", "ROULETTE", "WAR", "BACCARAT", "V.POKER", "KENO", "SIC BO", "CRAPS"];
const TABLE_ICONS: &[&str] = &["♠ ♥ ♣ ♦", "⟐ ⟐ ⟐", "◎ ◎ ◎", "⚔ ⚔", "9 9 9", "♠ HOLD", "● ● ●", "⚄ ⚄ ⚄", "⚃ ⚁"];

/// Draw the casino floor with 3D perspective
pub fn draw_hub(f: &mut Frame, app: &App, area: Rect) {
    let buf = f.buffer_mut();

    // Fill background
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            buf[(x, y)].set_char(' ').set_bg(theme::BG);
        }
    }

    let w = area.width as usize;
    let h = area.height as usize;

    if h < 10 || w < 40 {
        return;
    }

    // ── 3D CEILING & FLOOR ──────────────────────────────────────────
    // Perspective lines converging to center
    let cx = w / 2;
    let _vanish_y = 2; // vanishing point near top

    // Draw ceiling perspective lines
    for y in 0..h.min(5) {
        let spread = ((5 - y) as f32 / 5.0 * (w as f32 / 2.0 - 4.0)) as usize;
        let left = cx.saturating_sub(spread);
        let right = (cx + spread).min(w - 1);

        let ax = area.left() + left as u16;
        let ay = area.top() + y as u16;
        if ay < area.bottom() && ax < area.right() {
            buf[(ax, ay)].set_char('╲').set_fg(theme::AMBER_DIM);
        }
        let ax2 = area.left() + right as u16;
        if ay < area.bottom() && ax2 < area.right() {
            buf[(ax2, ay)].set_char('╱').set_fg(theme::AMBER_DIM);
        }

        // Ceiling pattern
        for x in (left + 1)..right {
            let ax = area.left() + x as u16;
            if ay < area.bottom() && ax < area.right() {
                let ch = if y == 0 { '░' } else if y < 2 { '·' } else { ' ' };
                let brightness = if y == 0 { theme::DARK_GRAY } else { theme::BG };
                buf[(ax, ay)].set_char(ch).set_fg(theme::DARK_GRAY).set_bg(brightness);
            }
        }
    }

    // Draw floor with checker pattern
    let floor_start = h.saturating_sub(h / 3);
    for y in floor_start..h {
        let depth = y - floor_start;
        let spread = ((depth + 3) as f32 / (h as f32 / 3.0) * (w as f32 / 2.0)) as usize;
        let left = cx.saturating_sub(spread).max(1);
        let right = (cx + spread).min(w - 1);

        for x in left..right {
            let ax = area.left() + x as u16;
            let ay = area.top() + y as u16;
            if ay < area.bottom() && ax < area.right() {
                let checker = ((x / 3) + (y / 2)) % 2 == 0;
                let ch = if checker { '▓' } else { '░' };
                let fg = if checker { Color::Rgb(25, 25, 30) } else { Color::Rgb(15, 15, 20) };
                buf[(ax, ay)].set_char(ch).set_fg(fg).set_bg(theme::BG);
            }
        }
    }

    // ── NEON SIGN (top) ─────────────────────────────────────────────
    let sign_text = " ◇ G A M B L E R S   D E N ◇ ";
    let sign_w = sign_text.len();
    let sign_x = (w.saturating_sub(sign_w)) / 2;
    let sign_y = 1;

    // Neon flicker effect
    let flicker = (app.frame / 8) % 20 != 0; // occasional flicker off
    let sign_color = if flicker { theme::MAGENTA } else { theme::DARK_GRAY };

    // Sign border
    let border_w = sign_w + 4;
    let border_x = sign_x.saturating_sub(2);
    let top_border = format!("╔{}╗", "═".repeat(border_w - 2));
    let bot_border = format!("╚{}╝", "═".repeat(border_w - 2));

    draw_str(buf, area, border_x, sign_y, &top_border, sign_color);
    draw_str(buf, area, border_x, sign_y + 1, &format!("║ {} ║", sign_text), sign_color);
    draw_str(buf, area, border_x, sign_y + 2, &bot_border, sign_color);

    // Neon glow on surrounding cells
    if flicker {
        for dx in 0..border_w {
            let gx = (border_x + dx) as u16 + area.left();
            let gy_top = sign_y as u16 + area.top();
            let gy_bot = (sign_y + 2) as u16 + area.top();
            if gx < area.right() {
                if gy_top > area.top() {
                    buf[(gx, gy_top - 1)].set_fg(Color::Rgb(60, 0, 50));
                }
                if gy_bot + 1 < area.bottom() {
                    buf[(gx, gy_bot + 1)].set_fg(Color::Rgb(60, 0, 50));
                }
            }
        }
    }

    // ── GAME TABLES (3x3 grid) ────────────────────────────────────
    let table_w = 14;
    let table_h = 5;
    let gap_x = 2;
    let gap_y = 1;
    let cols = 3;
    let total_grid_w = table_w * cols + gap_x * (cols - 1);
    let start_x = (w.saturating_sub(total_grid_w)) / 2;
    let start_y = (h / 2).saturating_sub(4);

    for (i, &name) in TABLES.iter().enumerate() {
        let col = i % cols;
        let row = i / cols;
        let tx = start_x + col * (table_w + gap_x);
        let ty = start_y + row * (table_h + gap_y);
        let selected = app.hub_selection == i;

        let border_color = if selected { theme::CYAN } else { theme::AMBER_DIM };
        let name_color = if selected { theme::WHITE } else { theme::GRAY };
        let icon_color = if selected {
            match i % 6 {
                0 => theme::CYAN,
                1 => theme::MAGENTA,
                2 => theme::RED,
                3 => theme::GREEN,
                4 => theme::AMBER,
                _ => theme::WHITE,
            }
        } else {
            theme::DARK_GRAY
        };

        // Compact table box
        let top = if selected { format!("╔{}╗", "═".repeat(table_w - 2)) } else { format!("┌{}┐", "─".repeat(table_w - 2)) };
        let bot = if selected { format!("╚{}╝", "═".repeat(table_w - 2)) } else { format!("└{}┘", "─".repeat(table_w - 2)) };
        let sl = if selected { "║" } else { "│" };
        let sr = sl;

        draw_str(buf, area, tx, ty, &top, border_color);

        // Icon row
        let icon = TABLE_ICONS[i];
        let ipad = (table_w - 2).saturating_sub(icon.chars().count()) / 2;
        let icon_line = format!("{}{}{}{}{}", sl, " ".repeat(ipad), icon, " ".repeat((table_w - 2).saturating_sub(ipad + icon.chars().count())), sr);
        draw_str_two(buf, area, tx, ty + 1, &icon_line, border_color, icon_color, icon);

        // Name row
        let npad = (table_w - 2).saturating_sub(name.len()) / 2;
        let name_line = format!("{}{}{}{}{}", sl, " ".repeat(npad), name, " ".repeat((table_w - 2).saturating_sub(npad + name.len())), sr);
        draw_str_two(buf, area, tx, ty + 2, &name_line, border_color, name_color, name);

        // Key row
        let key_label = format!("[{}]", i + 1);
        let kpad = (table_w - 2).saturating_sub(key_label.len()) / 2;
        let key_line = format!("{}{}{}{}{}", sl, " ".repeat(kpad), key_label, " ".repeat((table_w - 2).saturating_sub(kpad + key_label.len())), sr);
        let key_color = if selected { theme::GREEN } else { theme::DARK_GRAY };
        draw_str_two(buf, area, tx, ty + 3, &key_line, border_color, key_color, &key_label);

        draw_str(buf, area, tx, ty + 4, &bot, border_color);
    }

    // ── AMBIENT PARTICLES (smoke/sparks) ────────────────────────────
    let particles = [
        ("·", theme::DARK_GRAY),
        ("∘", Color::Rgb(40, 40, 50)),
        ("°", Color::Rgb(35, 30, 25)),
    ];

    // Pseudo-random particle positions based on frame
    for i in 0..12 {
        let seed = (app.frame as usize).wrapping_add(i * 7919);
        let px = (seed * 31 + i * 17) % w;
        let py = (seed * 13 + i * 23) % h;
        let (ch, color) = particles[i % particles.len()];

        let ax = area.left() + px as u16;
        let ay = area.top() + py as u16;
        if ax < area.right() && ay < area.bottom() {
            let cell = &buf[(ax, ay)];
            // Only draw on empty/background cells
            if cell.symbol() == " " || cell.symbol() == "░" || cell.symbol() == "·" {
                buf[(ax, ay)].set_char(ch.chars().next().unwrap()).set_fg(color);
            }
        }
    }

    // ── LIFETIME STATS (left side) ──────────────────────────────────
    let stats_x = 3;
    let stats_y = h.saturating_sub(8);
    draw_str(buf, area, stats_x, stats_y, "┌─────────────────────┐", theme::DARK_GRAY);
    draw_str(buf, area, stats_x, stats_y + 1, "│  LIFETIME STATS     │", theme::GRAY);
    draw_str(buf, area, stats_x, stats_y + 2, "├─────────────────────┤", theme::DARK_GRAY);
    let games = format!("│  Games: {:>10}  │", app.save.games_played);
    draw_str(buf, area, stats_x, stats_y + 3, &games, theme::GRAY);
    let earned = format!("│  Won:   {:>10}  │", app.save.total_earned);
    draw_str(buf, area, stats_x, stats_y + 4, &earned, theme::GREEN_DIM);
    let lost = format!("│  Lost:  {:>10}  │", app.save.total_lost);
    draw_str(buf, area, stats_x, stats_y + 5, &lost, theme::RED_DIM);
    let resets = format!("│  Resets:{:>10}  │", app.save.resets);
    draw_str(buf, area, stats_x, stats_y + 6, &resets, theme::AMBER_DIM);
    draw_str(buf, area, stats_x, stats_y + 7, "└─────────────────────┘", theme::DARK_GRAY);

    // ── BOTTOM INFO ─────────────────────────────────────────────────
    let info_y = h.saturating_sub(2);
    let info = "← → SELECT TABLE  ·  ENTER TO SIT  ·  Q QUIT";
    let info_x = (w.saturating_sub(info.len())) / 2;
    draw_str(buf, area, info_x, info_y, info, theme::GRAY);
}

// ── Helper: draw a string at position ────────────────────────────────
fn draw_str(buf: &mut Buffer, area: Rect, x: usize, y: usize, s: &str, fg: Color) {
    let mut cx = area.left() + x as u16;
    let cy = area.top() + y as u16;
    if cy >= area.bottom() { return; }
    for ch in s.chars() {
        if cx >= area.right() { break; }
        buf[(cx, cy)].set_char(ch).set_fg(fg);
        cx += 1;
    }
}

// ── Helper: draw a string with two colors (border + content) ────────
fn draw_str_two(buf: &mut Buffer, area: Rect, x: usize, y: usize, s: &str, border_fg: Color, content_fg: Color, content: &str) {
    let mut cx = area.left() + x as u16;
    let cy = area.top() + y as u16;
    if cy >= area.bottom() { return; }

    let content_start = s.find(content).unwrap_or(s.len());
    let content_end = content_start + content.len();

    for (i, ch) in s.chars().enumerate() {
        if cx >= area.right() { break; }
        let fg = if i >= content_start && i < content_end { content_fg } else { border_fg };
        buf[(cx, cy)].set_char(ch).set_fg(fg);
        cx += 1;
    }
}
