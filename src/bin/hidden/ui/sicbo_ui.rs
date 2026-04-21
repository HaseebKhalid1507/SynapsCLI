use ratatui::prelude::*;
use ratatui::buffer::Buffer;
use crate::app::App;
use crate::games::sicbo::*;
use crate::ui::theme;

// ── Dice Rendering ──────────────────────────────────────────────────

const DICE_W: usize = 7;
const DICE_H: usize = 5;

/// Draw ASCII dice based on value (1-6)
fn draw_dice(buf: &mut Buffer, area: Rect, x: usize, y: usize, value: u8) {
    let ax = area.left() + x as u16;
    let ay = area.top() + y as u16;

    let pattern = match value {
        1 => [
            "┌─────┐",
            "│     │",
            "│  ●  │",
            "│     │",
            "└─────┘",
        ],
        2 => [
            "┌─────┐",
            "│ ●   │",
            "│     │",
            "│   ● │",
            "└─────┘",
        ],
        3 => [
            "┌─────┐",
            "│ ●   │",
            "│  ●  │",
            "│   ● │",
            "└─────┘",
        ],
        4 => [
            "┌─────┐",
            "│ ● ● │",
            "│     │",
            "│ ● ● │",
            "└─────┘",
        ],
        5 => [
            "┌─────┐",
            "│ ● ● │",
            "│  ●  │",
            "│ ● ● │",
            "└─────┘",
        ],
        6 => [
            "┌─────┐",
            "│ ● ● │",
            "│ ● ● │",
            "│ ● ● │",
            "└─────┘",
        ],
        _ => [
            "┌─────┐",
            "│  ?  │",
            "│  ?  │",
            "│  ?  │",
            "└─────┘",
        ],
    };

    for (dy, line) in pattern.iter().enumerate() {
        let cy = ay + dy as u16;
        if cy >= area.bottom() { continue; }
        let mut cx = ax;
        for ch in line.chars() {
            if cx >= area.right() { break; }
            let fg = if ch == '●' { theme::WHITE } else { theme::AMBER_DIM };
            buf[(cx, cy)].set_char(ch).set_fg(fg).set_bg(theme::BG);
            cx += 1;
        }
    }
}

// ── Sic Bo Screen ───────────────────────────────────────────────────

pub fn draw_sicbo(f: &mut Frame, app: &App, game: &SicBoGame, area: Rect) {
    let buf = f.buffer_mut();
    let w = area.width as usize;
    let h = area.height as usize;

    // Fill background
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            buf[(x, y)].set_char(' ').set_bg(theme::BG);
        }
    }

    // Table felt
    let table_top = 2;
    let table_bot = h.saturating_sub(4);
    for y in table_top..table_bot {
        let ay = area.top() + y as u16;
        if ay >= area.bottom() { continue; }
        for x in 2..(w.saturating_sub(2)) {
            let ax = area.left() + x as u16;
            if ax >= area.right() { continue; }
            buf[(ax, ay)].set_bg(Color::Rgb(5, 15, 8));
        }
    }

    // Table border
    for x in 1..(w.saturating_sub(1)) {
        let ax = area.left() + x as u16;
        let at = area.top() + table_top as u16;
        let ab = area.top() + table_bot as u16;
        if ax < area.right() {
            if at < area.bottom() { buf[(ax, at)].set_char('─').set_fg(theme::AMBER_DIM); }
            if ab < area.bottom() { buf[(ax, ab)].set_char('─').set_fg(theme::AMBER_DIM); }
        }
    }

    match &game.phase {
        SicBoPhase::Betting => draw_betting(buf, area, game, app, w, h),
        SicBoPhase::Rolling => draw_rolling(buf, area, game, app, w, h),
        SicBoPhase::Result => {
            draw_dice_display(buf, area, game, w, h);
            draw_result(buf, area, game, app, w, h);
        }
    }
}

fn draw_betting(buf: &mut Buffer, area: Rect, game: &SicBoGame, app: &App, w: usize, h: usize) {
    // Title
    let title = "S I C   B O";
    draw_str(buf, area, (w - title.len()) / 2, 3, title, theme::MAGENTA);

    // Bet options
    let bet_y = h / 2 - 4;
    draw_str(buf, area, 4, bet_y, "BET OPTIONS:", theme::AMBER);

    let options = [
        ("BIG (11-17)", "1:1"),
        ("SMALL (4-10)", "1:1"),
        ("ODD", "1:1"),
        ("EVEN", "1:1"),
        ("ANY TRIPLE", "24:1"),
    ];

    for (i, (option, odds)) in options.iter().enumerate() {
        let selected = game.cursor == i;
        let fg = if selected { theme::CYAN } else { theme::GRAY };
        let marker = if selected { "►" } else { " " };
        let line = format!("{} {} ({})", marker, option, odds);
        draw_str(buf, area, 4, bet_y + 2 + i, &line, fg);
    }

    // Current bets
    if !game.bets.is_empty() {
        let total_str = format!("TOTAL BET: {} TOKENS", game.total_bet);
        draw_str(buf, area, w - 25, bet_y, &total_str, theme::GREEN);

        for (i, (bet, amount)) in game.bets.iter().enumerate() {
            let bet_str = format!("{}: {}", bet.label(), amount);
            draw_str(buf, area, w - 25, bet_y + 2 + i, &bet_str, theme::AMBER_DIM);
        }
    }

    // Bet input
    let bet_display = if game.bet_input.is_empty() {
        "_ ".to_string()
    } else {
        format!("{}_", game.bet_input)
    };

    let bet_line = format!("AMOUNT: {}", bet_display);
    draw_str(buf, area, 4, h - 4, &bet_line, theme::GREEN);

    let balance = format!("Balance: {} tokens", app.tokens);
    draw_str(buf, area, 4, h - 3, &balance, theme::GRAY);

    let hint = "[↑↓] Select  ·  [0-9] Amount  ·  [SPACE] Place Bet  ·  [ENTER] Roll  ·  [ESC] Back";
    let hx = (w.saturating_sub(hint.len())) / 2;
    draw_str(buf, area, hx, h - 1, hint, theme::DARK_GRAY);
}

fn draw_rolling(buf: &mut Buffer, area: Rect, game: &SicBoGame, _app: &App, w: usize, h: usize) {
    // Title
    let title = "ROLLING DICE...";
    draw_str(buf, area, (w - title.len()) / 2, 3, title, theme::MAGENTA);

    // Dice with rolling animation
    let dice_x = (w.saturating_sub(25)) / 2;
    let dice_y = h / 2 - 3;

    for i in 0..3 {
        let x = dice_x + i * 8;
        draw_dice(buf, area, x, dice_y, game.rolling_display[i]);
    }

    // Total
    let total: u8 = game.rolling_display.iter().sum();
    let total_str = format!("TOTAL: {}", total);
    draw_str(buf, area, (w - total_str.len()) / 2, dice_y + DICE_H + 1, &total_str, theme::AMBER);
}

fn draw_dice_display(buf: &mut Buffer, area: Rect, game: &SicBoGame, w: usize, h: usize) {
    // Title
    let title = "FINAL RESULT";
    draw_str(buf, area, (w - title.len()) / 2, 3, title, theme::AMBER);

    // Final dice
    let dice_x = (w.saturating_sub(25)) / 2;
    let dice_y = h / 2 - 5;

    for i in 0..3 {
        let x = dice_x + i * 8;
        draw_dice(buf, area, x, dice_y, game.dice[i]);
    }

    // Total
    let total: u8 = game.dice.iter().sum();
    let total_str = format!("TOTAL: {}", total);
    let total_color = if total >= 11 { theme::GREEN } else { theme::CYAN };
    draw_str(buf, area, (w - total_str.len()) / 2, dice_y + DICE_H + 1, &total_str, total_color);
}

fn draw_result(buf: &mut Buffer, area: Rect, game: &SicBoGame, _app: &App, w: usize, h: usize) {
    let result_y = h - 6;

    // Calculate total winnings
    let mut _total_won = 0;
    let mut any_wins = false;

    for (bet, amount) in &game.bets {
        if bet.wins(&game.dice) {
            _total_won += amount + (amount * bet.payout_ratio());
            any_wins = true;
        }
    }

    let label = if any_wins { "WINNER!" } else { "NO WIN" };
    let color = if any_wins { theme::GREEN } else { theme::RED };

    // Result banner
    let banner_w = label.len() + 6;
    let bx = (w.saturating_sub(banner_w)) / 2;

    draw_str(buf, area, bx, result_y, &format!("╔{}╗", "═".repeat(banner_w - 2)), color);
    let pad = (banner_w - 2 - label.len()) / 2;
    draw_str(buf, area, bx, result_y + 1, &format!("║{}{}{}║", " ".repeat(pad), label, " ".repeat(banner_w - 2 - pad - label.len())), color);
    draw_str(buf, area, bx, result_y + 2, &format!("╚{}╝", "═".repeat(banner_w - 2)), color);

    // Payout
    let payout_str = if game.last_payout > 0 {
        format!("+{} TOKENS", game.last_payout)
    } else if game.last_payout < 0 {
        format!("{} TOKENS", game.last_payout)
    } else {
        "NO CHANGE".to_string()
    };
    let payout_color = if game.last_payout > 0 { theme::GREEN } else if game.last_payout < 0 { theme::RED } else { theme::GRAY };
    draw_str(buf, area, (w - payout_str.len()) / 2, result_y + 4, &payout_str, payout_color);

    draw_str(buf, area, (w - 28) / 2, h - 1, "[ENTER] New Round  ·  [ESC] Back", theme::DARK_GRAY);
}

// ── Helpers ─────────────────────────────────────────────────────────

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