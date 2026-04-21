use ratatui::prelude::*;
use ratatui::buffer::Buffer;
use crate::app::App;
use crate::games::craps::*;
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

// ── Craps Screen ────────────────────────────────────────────────────

pub fn draw_craps(f: &mut Frame, app: &App, game: &CrapsGame, area: Rect) {
    let buf = f.buffer_mut();
    let w = area.width as usize;
    let h = area.height as usize;

    // Fill background
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            buf[(x, y)].set_char(' ').set_bg(theme::BG);
        }
    }

    // Craps table felt
    let table_top = 2;
    let table_bot = h.saturating_sub(4);
    for y in table_top..table_bot {
        let ay = area.top() + y as u16;
        if ay >= area.bottom() { continue; }
        for x in 2..(w.saturating_sub(2)) {
            let ax = area.left() + x as u16;
            if ax >= area.right() { continue; }
            buf[(ax, ay)].set_bg(Color::Rgb(8, 20, 8));
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
        CrapsPhase::Betting => draw_betting(buf, area, game, app, w, h),
        CrapsPhase::ComeOut | CrapsPhase::Point => draw_playing(buf, area, game, app, w, h),
        CrapsPhase::Result => {
            draw_dice_display(buf, area, game, w, h);
            draw_result(buf, area, game, app, w, h);
        }
    }

    // Always show roll history
    draw_roll_history(buf, area, game, w, h);
}

fn draw_betting(buf: &mut Buffer, area: Rect, game: &CrapsGame, app: &App, w: usize, h: usize) {
    // Title
    let title = "C R A P S";
    draw_str(buf, area, (w - title.len()) / 2, 3, title, theme::GREEN);

    // Bet type selection
    let bet_y = h / 2 - 3;
    draw_str(buf, area, 4, bet_y, "SELECT BET TYPE:", theme::AMBER);

    let bet_options = ["PASS LINE", "DON'T PASS", "FIELD"];
    let bet_odds = ["1:1", "1:1", "1:1 (2:1 on 2,12)"];

    for (i, (option, odds)) in bet_options.iter().zip(bet_odds.iter()).enumerate() {
        let selected = game.cursor == i;
        let fg = if selected { theme::CYAN } else { theme::GRAY };
        let marker = if selected { "►" } else { " " };
        let line = format!("{} {} ({})", marker, option, odds);
        draw_str(buf, area, 4, bet_y + 2 + i, &line, fg);
    }

    // Bet amount input
    let bet_display = if game.bet_input.is_empty() {
        "_ ".to_string()
    } else {
        format!("{}_", game.bet_input)
    };

    let bet_line = format!("AMOUNT: {}", bet_display);
    draw_str(buf, area, 4, h - 5, &bet_line, theme::GREEN);

    let balance = format!("Balance: {} tokens", app.tokens);
    draw_str(buf, area, 4, h - 4, &balance, theme::GRAY);

    let hint = "[↑↓] Select Bet  ·  [0-9] Enter Amount  ·  [ENTER] Roll  ·  [A] All-in  ·  [ESC] Back";
    let hx = (w.saturating_sub(hint.len())) / 2;
    draw_str(buf, area, hx, h - 1, hint, theme::DARK_GRAY);
}

fn draw_playing(buf: &mut Buffer, area: Rect, game: &CrapsGame, _app: &App, w: usize, h: usize) {
    // Title with phase
    let title = match game.phase {
        CrapsPhase::ComeOut => "COME OUT ROLL",
        CrapsPhase::Point => "POINT PHASE",
        _ => "CRAPS",
    };
    draw_str(buf, area, (w - title.len()) / 2, 3, title, theme::GREEN);

    // Dice display
    let dice_x = (w.saturating_sub(16)) / 2;
    let dice_y = h / 2 - 3;

    draw_dice(buf, area, dice_x, dice_y, game.rolling_display[0]);
    draw_dice(buf, area, dice_x + 8, dice_y, game.rolling_display[1]);

    // Total
    let total: u8 = game.rolling_display.iter().sum();
    let total_str = format!("TOTAL: {}", total);
    draw_str(buf, area, (w - total_str.len()) / 2, dice_y + DICE_H + 1, &total_str, theme::AMBER);

    // Point marker
    if let Some(point) = game.point {
        let point_str = format!("POINT: {}", point);
        draw_str(buf, area, (w - point_str.len()) / 2, dice_y + DICE_H + 3, &point_str, theme::MAGENTA);
    }

    // Bet info
    let bet_str = format!("{}: {} TOKENS", game.bet_type.label(), game.bet);
    draw_str(buf, area, 4, h - 3, &bet_str, theme::CYAN);

    let hint = if game.phase == CrapsPhase::ComeOut {
        "[SPACE] Come Out Roll"
    } else {
        "[SPACE] Roll Dice"
    };
    draw_str(buf, area, (w.saturating_sub(hint.len())) / 2, h - 1, hint, theme::GREEN_DIM);
}

fn draw_dice_display(buf: &mut Buffer, area: Rect, game: &CrapsGame, w: usize, h: usize) {
    // Final dice
    let dice_x = (w.saturating_sub(16)) / 2;
    let dice_y = h / 2 - 5;

    draw_dice(buf, area, dice_x, dice_y, game.dice[0]);
    draw_dice(buf, area, dice_x + 8, dice_y, game.dice[1]);

    // Total
    let total = game.total();
    let total_str = format!("TOTAL: {}", total);
    let total_color = match total {
        7 => theme::GREEN,
        11 => theme::CYAN,
        2 | 3 | 12 => theme::RED,
        _ => theme::AMBER,
    };
    draw_str(buf, area, (w - total_str.len()) / 2, dice_y + DICE_H + 1, &total_str, total_color);

    // Point if established
    if let Some(point) = game.point {
        let point_str = format!("POINT: {}", point);
        draw_str(buf, area, (w - point_str.len()) / 2, dice_y + DICE_H + 3, &point_str, theme::MAGENTA);
    }
}

fn draw_roll_history(buf: &mut Buffer, area: Rect, game: &CrapsGame, w: usize, h: usize) {
    if !game.roll_history.is_empty() {
        let history_y = h - 7;
        draw_str(buf, area, w - 20, history_y, "ROLL HISTORY:", theme::GRAY);

        let history_str = game.roll_history.iter()
            .map(|&r| r.to_string())
            .collect::<Vec<_>>()
            .join(", ");

        // Show last 10 characters to fit
        let display_history = if history_str.len() > 15 {
            format!("...{}", &history_str[history_str.len()-12..])
        } else {
            history_str
        };

        draw_str(buf, area, w - 20, history_y + 1, &display_history, theme::WHITE);
    }
}

fn draw_result(buf: &mut Buffer, area: Rect, game: &CrapsGame, _app: &App, w: usize, h: usize) {
    let result_y = h / 2 + 3;

    let (label, color) = if game.last_payout > 0 {
        ("WINNER!", theme::GREEN)
    } else if game.last_payout == 0 {
        ("PUSH", theme::AMBER)
    } else {
        ("SEVEN OUT", theme::RED)
    };

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
        "BET RETURNED".to_string()
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