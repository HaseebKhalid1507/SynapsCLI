use ratatui::prelude::*;
use ratatui::buffer::Buffer;
use crate::app::App;
use crate::games::keno::*;
use crate::ui::theme;

// ── Keno Screen ─────────────────────────────────────────────────────

pub fn draw_keno(f: &mut Frame, app: &App, game: &KenoGame, area: Rect) {
    let buf = f.buffer_mut();
    let w = area.width as usize;
    let h = area.height as usize;

    // Fill background
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            buf[(x, y)].set_char(' ').set_bg(theme::BG);
        }
    }

    // Keno board background
    let board_top = 2;
    let board_bot = h.saturating_sub(4);
    for y in board_top..board_bot {
        let ay = area.top() + y as u16;
        if ay >= area.bottom() { continue; }
        for x in 2..(w.saturating_sub(2)) {
            let ax = area.left() + x as u16;
            if ax >= area.right() { continue; }
            buf[(ax, ay)].set_bg(Color::Rgb(10, 5, 15));
        }
    }

    // Board border
    for x in 1..(w.saturating_sub(1)) {
        let ax = area.left() + x as u16;
        let at = area.top() + board_top as u16;
        let ab = area.top() + board_bot as u16;
        if ax < area.right() {
            if at < area.bottom() { buf[(ax, at)].set_char('▓').set_fg(theme::MAGENTA); }
            if ab < area.bottom() { buf[(ax, ab)].set_char('▓').set_fg(theme::MAGENTA); }
        }
    }

    match &game.phase {
        KenoPhase::Picking => draw_picking(buf, area, game, app, w, h),
        KenoPhase::Drawing => draw_drawing(buf, area, game, app, w, h),
        KenoPhase::Result => draw_result_grid(buf, area, game, app, w, h),
    }
}

fn draw_picking(buf: &mut Buffer, area: Rect, game: &KenoGame, app: &App, w: usize, h: usize) {
    // Title
    let title = "K E N O";
    draw_str(buf, area, (w - title.len()) / 2, 1, title, theme::MAGENTA);

    // Number grid (8x10 = 80 numbers)
    draw_number_grid(buf, area, game, w, h);

    // Picked numbers display
    if !game.picks.is_empty() {
        let picks_str = format!("PICKS: {}", game.picks.len());
        draw_str(buf, area, 4, h - 6, &picks_str, theme::CYAN);

        let mut picks_line = String::new();
        for (i, &pick) in game.picks.iter().enumerate() {
            if i > 0 { picks_line.push_str(", "); }
            picks_line.push_str(&pick.to_string());
        }
        draw_str(buf, area, 4, h - 5, &picks_line, theme::WHITE);
    }

    // Bet input
    let bet_display = if game.bet_input.is_empty() {
        "_ ".to_string()
    } else {
        format!("{}_", game.bet_input)
    };

    let bet_line = format!("BET: {} TOKENS", bet_display);
    draw_str(buf, area, 4, h - 3, &bet_line, theme::GREEN);

    let balance = format!("Balance: {} tokens", app.tokens);
    draw_str(buf, area, 4, h - 2, &balance, theme::GRAY);

    let hint = "[HJKL/WASD] Move  ·  [SPACE] Pick  ·  [0-9] Bet  ·  [ENTER] Draw  ·  [ESC] Back";
    let hx = (w.saturating_sub(hint.len())) / 2;
    draw_str(buf, area, hx, h - 1, hint, theme::DARK_GRAY);
}

fn draw_drawing(buf: &mut Buffer, area: Rect, game: &KenoGame, _app: &App, w: usize, h: usize) {
    // Title
    let title = "K E N O - DRAWING...";
    draw_str(buf, area, (w - title.len()) / 2, 1, title, theme::MAGENTA);

    // Number grid with drawn numbers highlighted
    draw_number_grid(buf, area, game, w, h);

    // Drawn numbers so far
    let drawn_str = format!("DRAWN: {}/20", game.drawn.len());
    draw_str(buf, area, 4, h - 4, &drawn_str, theme::AMBER);

    if !game.drawn.is_empty() {
        let mut drawn_line = String::new();
        for (i, &num) in game.drawn.iter().enumerate() {
            if i > 0 { drawn_line.push_str(", "); }
            drawn_line.push_str(&num.to_string());
        }
        draw_str(buf, area, 4, h - 3, &drawn_line, theme::WHITE);
    }
}

fn draw_result_grid(buf: &mut Buffer, area: Rect, game: &KenoGame, _app: &App, w: usize, h: usize) {
    // Title
    let title = "K E N O - RESULTS";
    draw_str(buf, area, (w - title.len()) / 2, 1, title, theme::MAGENTA);

    // Number grid with hits glowing
    draw_number_grid(buf, area, game, w, h);

    // Results
    let hits_str = format!("HITS: {}/{}", game.hits, game.picks.len());
    draw_str(buf, area, 4, h - 5, &hits_str, theme::CYAN);

    let payout_str = if game.last_payout > 0 {
        format!("+{} TOKENS", game.last_payout)
    } else {
        format!("-{} TOKENS", game.bet)
    };
    let payout_color = if game.last_payout > 0 { theme::GREEN } else { theme::RED };
    draw_str(buf, area, 4, h - 4, &payout_str, payout_color);

    draw_str(buf, area, (w - 28) / 2, h - 1, "[ENTER] New Round  ·  [ESC] Back", theme::DARK_GRAY);
}

fn draw_number_grid(buf: &mut Buffer, area: Rect, game: &KenoGame, w: usize, _h: usize) {
    let grid_start_x = (w.saturating_sub(50)) / 2;
    let grid_start_y = 4;

    // 8 rows x 10 columns = 80 numbers
    for row in 0..8 {
        for col in 0..10 {
            let num = (row * 10 + col + 1) as u8;
            let x = grid_start_x + col * 5;
            let y = grid_start_y + row * 2;

            // Determine color based on state
            let (fg, bg) = if game.is_hit(num) {
                (theme::BG, theme::GREEN) // Hit - glow green
            } else if game.phase == KenoPhase::Result && game.is_miss(num) {
                (theme::WHITE, theme::RED_DIM) // Miss - dim red
            } else if game.picks.contains(&num) {
                (theme::BG, theme::CYAN) // Picked - cyan
            } else if game.drawn.contains(&num) {
                (theme::BG, theme::AMBER) // Drawn but not picked - amber
            } else if game.cursor == num {
                (theme::CYAN, theme::DARK_GRAY) // Cursor
            } else {
                (theme::GRAY, theme::BG) // Default
            };

            // Draw number
            let num_str = format!("{:2}", num);
            let mut cx = area.left() + x as u16;
            let cy = area.top() + y as u16;
            if cy < area.bottom() {
                for ch in num_str.chars() {
                    if cx >= area.right() { break; }
                    buf[(cx, cy)].set_char(ch).set_fg(fg).set_bg(bg);
                    cx += 1;
                }
            }
        }
    }
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