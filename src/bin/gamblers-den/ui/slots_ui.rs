use ratatui::prelude::*;
use ratatui::buffer::Buffer;
use crate::app::App;
use crate::games::slots::*;
use crate::ui::theme;

pub fn draw_slots(f: &mut Frame, app: &App, game: &SlotsGame, area: Rect) {
    let buf = f.buffer_mut();
    let w = area.width as usize;
    let h = area.height as usize;

    // Fill background
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            buf[(x, y)].set_char(' ').set_bg(theme::BG);
        }
    }

    match &game.phase {
        SlotsPhase::Betting => draw_betting(buf, area, game, app, w, h),
        SlotsPhase::Spinning | SlotsPhase::Revealing(_) => draw_reels(buf, area, game, app, w, h, false),
        SlotsPhase::Result => draw_reels(buf, area, game, app, w, h, true),
    }
}

fn draw_betting(buf: &mut Buffer, area: Rect, game: &SlotsGame, app: &App, w: usize, h: usize) {
    // Machine frame
    draw_machine_frame(buf, area, w, h);

    let cy = h / 2 - 3;
    draw_str(buf, area, (w - 16) / 2, cy, "╔══════════════╗", theme::MAGENTA);
    draw_str(buf, area, (w - 16) / 2, cy + 1, "║  INSERT BET  ║", theme::MAGENTA);
    draw_str(buf, area, (w - 16) / 2, cy + 2, "╚══════════════╝", theme::MAGENTA);

    let bet_display = if game.bet_input.is_empty() { "_ ".to_string() } else { format!("{}_", game.bet_input) };
    let bet_line = format!("◈ {} TOKENS", bet_display);
    draw_str(buf, area, (w - bet_line.len()) / 2, cy + 4, &bet_line, theme::GREEN);

    let balance = format!("Balance: {} tokens", app.tokens);
    draw_str(buf, area, (w - balance.len()) / 2, cy + 6, &balance, theme::GRAY);

    // Payout table on the right
    draw_payout_table(buf, area, w, h);

    let hint = "[0-9] Amount  ·  [ENTER] Spin  ·  [A] All-in  ·  [ESC] Back";
    draw_str(buf, area, (w.saturating_sub(hint.len())) / 2, h - 2, hint, theme::DARK_GRAY);
}

fn draw_reels(buf: &mut Buffer, area: Rect, game: &SlotsGame, app: &App, w: usize, h: usize, show_result: bool) {
    draw_machine_frame(buf, area, w, h);

    let reel_w = 9;
    let gap = 2;
    let total_w = reel_w * 3 + gap * 2;
    let start_x = (w.saturating_sub(total_w)) / 2;
    let reel_y = h / 2 - 3;

    // Draw each reel
    for i in 0..3 {
        let rx = start_x + i * (reel_w + gap);
        let window = game.reel_window(i);

        let is_stopped = match game.phase {
            SlotsPhase::Revealing(s) => i < s as usize,
            SlotsPhase::Result => true,
            _ => false,
        };
        let is_stopping = match game.phase {
            SlotsPhase::Revealing(s) => i == s as usize,
            _ => false,
        };

        // Reel border
        let border_color = if is_stopped && show_result && game.multiplier > 0.0 {
            theme::GREEN
        } else if is_stopping {
            theme::AMBER
        } else {
            theme::MAGENTA
        };

        draw_str(buf, area, rx, reel_y, "╔═══════╗", border_color);

        for dy in 0..3 {
            let sym = window[dy];
            let y = reel_y + 1 + dy;
            let is_center = dy == 1;

            draw_str(buf, area, rx, y, "║", border_color);

            let sym_color = if is_center && is_stopped {
                match sym {
                    Symbol::Skull => theme::RED,
                    Symbol::Diamond => theme::CYAN,
                    Symbol::Seven => theme::AMBER,
                    Symbol::Lightning => theme::CYAN,
                    Symbol::Fire => theme::RED,
                    Symbol::Wild => theme::MAGENTA,
                    _ => theme::WHITE,
                }
            } else if is_center {
                theme::WHITE
            } else {
                theme::DARK_GRAY
            };

            // Center row indicator
            let prefix = if is_center { "▸" } else { " " };
            let suffix = if is_center { "◂" } else { " " };

            let label = sym.label();
            draw_str(buf, area, rx + 1, y, &format!("{}{}{}", prefix, label, suffix), sym_color);
            draw_str(buf, area, rx + 8, y, "║", border_color);
        }

        draw_str(buf, area, rx, reel_y + 4, "╚═══════╝", border_color);
    }

    // Bet display
    let bet_str = format!("BET: {}", game.bet);
    draw_str(buf, area, (w - bet_str.len()) / 2, reel_y + 6, &bet_str, theme::AMBER_DIM);

    // Result
    if show_result {
        if game.multiplier >= 50.0 {
            // JACKPOT — FULL SCREEN TAKEOVER
            draw_jackpot_takeover(buf, area, app, w, h, game);
        } else {
            let y = reel_y + 8;
            if game.multiplier > 0.0 {
                let msg = format!("WIN! {}x — +{} TOKENS", game.multiplier, game.last_payout);
                draw_str(buf, area, (w - msg.len()) / 2, y, &msg, theme::GREEN);
            } else {
                draw_str(buf, area, (w - 10) / 2, y, "NO MATCH", theme::RED_DIM);
            }

            let payout_color = if game.last_payout > 0 { theme::GREEN } else { theme::RED };
            let payout_str = if game.last_payout > 0 {
                format!("+{}", game.last_payout)
            } else {
                format!("{}", game.last_payout)
            };
            draw_str(buf, area, (w - payout_str.len()) / 2, y + 1, &payout_str, payout_color);

            draw_str(buf, area, (w - 30) / 2, h - 2, "[ENTER] Spin Again  ·  [ESC] Back", theme::DARK_GRAY);
        }
    }

    // Payout table
    draw_payout_table(buf, area, w, h);
}

fn draw_machine_frame(buf: &mut Buffer, area: Rect, w: usize, h: usize) {
    // Side decorations — neon strips
    for y in 2..(h.saturating_sub(2)) {
        let ay = area.top() + y as u16;
        if ay >= area.bottom() { continue; }

        let left = area.left() + 1;
        let right = area.left() + (w - 2) as u16;
        if left < area.right() {
            let ch = if y % 3 == 0 { '│' } else { '┊' };
            buf[(left, ay)].set_char(ch).set_fg(theme::MAGENTA);
        }
        if right < area.right() {
            let ch = if y % 3 == 0 { '│' } else { '┊' };
            buf[(right, ay)].set_char(ch).set_fg(theme::MAGENTA);
        }
    }

    // Title
    let title = "═══ S L O T S ═══";
    draw_str(buf, area, (w - title.len()) / 2, 1, title, theme::MAGENTA);
}

fn draw_payout_table(buf: &mut Buffer, area: Rect, w: usize, h: usize) {
    let table = payout_table();
    let tx = w.saturating_sub(22).max(2);
    let ty = 3;

    draw_str(buf, area, tx, ty, "┌──────────────────┐", theme::DARK_GRAY);
    draw_str(buf, area, tx, ty + 1, "│   PAYOUT TABLE   │", theme::GRAY);
    draw_str(buf, area, tx, ty + 2, "├──────────────────┤", theme::DARK_GRAY);

    for (i, (name, mult, note)) in table.iter().enumerate() {
        let y = ty + 3 + i;
        if y >= h.saturating_sub(2) { break; }

        let color = if *note == "JACKPOT" { theme::RED } else { theme::GRAY };
        let line = format!("│ {:8} {:>4}   │", name, mult);
        draw_str(buf, area, tx, y, &line, color);
    }

    let bot_y = ty + 3 + table.len().min(h.saturating_sub(ty + 5));
    draw_str(buf, area, tx, bot_y, "└──────────────────┘", theme::DARK_GRAY);
}

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

fn draw_jackpot_takeover(buf: &mut Buffer, area: Rect, app: &App, w: usize, h: usize, game: &SlotsGame) {
    // Strobe background
    let phase = (app.frame / 4) % 6;
    let bg_color = match phase {
        0 => Color::Rgb(40, 0, 40),
        1 => Color::Rgb(0, 0, 40),
        2 => Color::Rgb(40, 40, 0),
        3 => Color::Rgb(40, 0, 0),
        4 => Color::Rgb(0, 40, 0),
        _ => Color::Rgb(20, 0, 30),
    };

    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            buf[(x, y)].set_bg(bg_color);
        }
    }

    // Explosion particles
    let spark_chars = ['★', '✦', '✧', '◆', '◇', '⬥', '·', '✶'];
    let spark_colors = [theme::MAGENTA, theme::AMBER, theme::CYAN, theme::GREEN, theme::RED, theme::WHITE];
    for i in 0..30 {
        let seed = (app.frame as usize).wrapping_add(i * 997);
        let px = (seed * 31 + i * 17) % w;
        let py = (seed * 13 + i * 23) % h;
        let ch = spark_chars[seed % spark_chars.len()];
        let color = spark_colors[(seed / 3) % spark_colors.len()];
        let ax = area.left() + px as u16;
        let ay = area.top() + py as u16;
        if ax < area.right() && ay < area.bottom() {
            buf[(ax, ay)].set_char(ch).set_fg(color);
        }
    }

    // Big JACKPOT text
    let cy = h / 2 - 3;
    let blink1 = (app.frame / 6) % 2 == 0;
    let blink2 = (app.frame / 4) % 2 == 0;

    let j1 = "╔══════════════════════════════╗";
    let j2 = "║  ★  J  A  C  K  P  O  T  ★  ║";
    let j3 = "╚══════════════════════════════╝";

    let color1 = if blink1 { theme::MAGENTA } else { theme::AMBER };
    let color2 = if blink2 { theme::AMBER } else { theme::MAGENTA };

    draw_str(buf, area, (w - j1.len()) / 2, cy, j1, color1);
    draw_str(buf, area, (w - j2.len()) / 2, cy + 1, j2, color2);
    draw_str(buf, area, (w - j3.len()) / 2, cy + 2, j3, color1);

    // Payout
    let payout_str = format!("+{} TOKENS!", game.last_payout);
    let payout_color = if blink1 { theme::GREEN } else { theme::CYAN };
    draw_str(buf, area, (w - payout_str.len()) / 2, cy + 4, &payout_str, payout_color);

    let mult = format!("{}x MULTIPLIER", game.multiplier);
    draw_str(buf, area, (w - mult.len()) / 2, cy + 5, &mult, theme::WHITE);

    draw_str(buf, area, (w - 30) / 2, h - 2, "[ENTER] Spin Again  ·  [ESC] Back", theme::GRAY);
}
