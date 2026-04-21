use ratatui::prelude::*;
use ratatui::buffer::Buffer;
use crate::app::App;
use crate::games::roulette::*;
use crate::ui::theme;

pub fn draw_roulette(f: &mut Frame, app: &App, game: &RouletteGame, area: Rect) {
    let buf = f.buffer_mut();
    let w = area.width as usize;
    let h = area.height as usize;

    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            buf[(x, y)].set_char(' ').set_bg(theme::BG);
        }
    }

    // Title
    let title = "═══ R O U L E T T E ═══";
    draw_str(buf, area, (w - title.len()) / 2, 1, title, theme::RED);

    match &game.phase {
        RoulettePhase::Betting => draw_betting(buf, area, game, app, w, h),
        RoulettePhase::Spinning => draw_spinning(buf, area, game, app, w, h),
        RoulettePhase::Result => draw_result(buf, area, game, app, w, h),
    }
}

fn draw_betting(buf: &mut Buffer, area: Rect, game: &RouletteGame, app: &App, w: usize, h: usize) {
    // Bet options grid
    let options_y = 4;
    draw_str(buf, area, 4, options_y, "SELECT BET TYPE:", theme::AMBER);

    for (i, bet_type) in BET_OPTIONS.iter().enumerate() {
        let y = options_y + 2 + i;
        let selected = game.cursor == i;

        let prefix = if selected { "▸ " } else { "  " };
        let label = bet_type.label();
        let ratio = format!("{}:1", bet_type.payout_ratio());

        let name_color = if selected { theme::WHITE } else { theme::GRAY };
        let _ratio_color = if selected { theme::GREEN } else { theme::DARK_GRAY };

        // Color indicator for red/black
        let color_indicator = match bet_type {
            BetType::Red => " ■",
            BetType::Black => " ■",
            _ => "",
        };
        let ind_color = match bet_type {
            BetType::Red => theme::RED,
            BetType::Black => theme::CARD_BLACK,
            _ => theme::BG,
        };

        draw_str(buf, area, 4, y, &format!("{}{:10} {:>5}", prefix, label, ratio), name_color);
        if !color_indicator.is_empty() {
            draw_str(buf, area, 4 + prefix.len() + label.len(), y, color_indicator, ind_color);
        }

        if selected {
            // Highlight bar
            for x in 3..(w.min(25)) {
                let ax = area.left() + x as u16;
                let ay = area.top() + y as u16;
                if ax < area.right() && ay < area.bottom() {
                    buf[(ax, ay)].set_bg(Color::Rgb(15, 15, 25));
                }
            }
        }
    }

    // Bet amount input (right side)
    let input_x = w / 2 + 2;
    let input_y = options_y + 2;
    draw_str(buf, area, input_x, input_y, "BET AMOUNT:", theme::AMBER);

    let bet_display = if game.bet_input.is_empty() { "_ ".to_string() } else { format!("{}_", game.bet_input) };
    draw_str(buf, area, input_x, input_y + 2, &format!("◈ {} TOKENS", bet_display), theme::GREEN);

    draw_str(buf, area, input_x, input_y + 4, &format!("Balance: {}", app.tokens), theme::GRAY);

    // Active bets
    if !game.bets.is_empty() {
        draw_str(buf, area, input_x, input_y + 7, "ACTIVE BETS:", theme::AMBER);
        for (i, bet) in game.bets.iter().enumerate() {
            let y = input_y + 8 + i;
            if y >= h.saturating_sub(3) { break; }
            let line = format!("  {} — {} tokens", bet.bet_type.label(), bet.amount);
            draw_str(buf, area, input_x, y, &line, theme::CYAN_DIM);
        }
        let total = format!("  TOTAL: {} tokens", game.total_bet);
        draw_str(buf, area, input_x, input_y + 8 + game.bets.len(), &total, theme::AMBER_DIM);
    }

    // Number grid (bottom)
    draw_number_grid(buf, area, w, h, None);

    // Controls
    let hint = "↑↓ Select  ·  [0-9] Amount  ·  [ENTER] Place Bet  ·  [SPACE] Spin  ·  [ESC] Back";
    draw_str(buf, area, (w.saturating_sub(hint.len())) / 2, h - 2, hint, theme::DARK_GRAY);
}

fn draw_spinning(buf: &mut Buffer, area: Rect, game: &RouletteGame, app: &App, w: usize, h: usize) {
    let cy = h / 2 - 2;

    // Big spinning number
    let num = game.spin_display;
    let color = match number_color(num) {
        RouletteColor::Red => theme::RED,
        RouletteColor::Black => theme::WHITE,
        RouletteColor::Green => theme::GREEN,
    };

    let num_str = format!("{:02}", num);
    let display = format!("╔════════╗\n║   {}   ║\n╚════════╝", num_str);
    let lines: Vec<&str> = display.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        draw_str(buf, area, (w - 10) / 2, cy + i, line, color);
    }

    // Spinning indicator
    let spinner = ["◐", "◓", "◑", "◒"];
    let s = spinner[(app.frame as usize / 4) % spinner.len()];
    draw_str(buf, area, (w - 1) / 2, cy + 4, s, theme::AMBER);

    draw_number_grid(buf, area, w, h, None);
}

fn draw_result(buf: &mut Buffer, area: Rect, game: &RouletteGame, _app: &App, w: usize, h: usize) {
    let cy = h / 2 - 4;
    let result = game.result.unwrap_or(0);

    let color = match number_color(result) {
        RouletteColor::Red => theme::RED,
        RouletteColor::Black => theme::WHITE,
        RouletteColor::Green => theme::GREEN,
    };

    let color_name = match number_color(result) {
        RouletteColor::Red => "RED",
        RouletteColor::Black => "BLACK",
        RouletteColor::Green => "GREEN",
    };

    // Result display
    let num_str = format!("{:02}", result);
    draw_str(buf, area, (w - 10) / 2, cy, "╔════════╗", color);
    draw_str(buf, area, (w - 10) / 2, cy + 1, &format!("║   {}   ║", num_str), color);
    draw_str(buf, area, (w - 10) / 2, cy + 2, "╚════════╝", color);
    draw_str(buf, area, (w - color_name.len()) / 2, cy + 3, color_name, color);

    // Payout
    let payout = game.last_payout;
    let payout_str = if payout > 0 {
        format!("+{} TOKENS", payout)
    } else if payout < 0 {
        format!("{} TOKENS", payout)
    } else {
        "BREAK EVEN".to_string()
    };
    let payout_color = if payout > 0 { theme::GREEN } else if payout < 0 { theme::RED } else { theme::GRAY };
    draw_str(buf, area, (w - payout_str.len()) / 2, cy + 5, &payout_str, payout_color);

    // Show which bets won/lost
    let by = cy + 7;
    for (i, bet) in game.bets.iter().enumerate() {
        if by + i >= h.saturating_sub(4) { break; }
        let won = bet.bet_type.wins(result);
        let icon = if won { "✓" } else { "✗" };
        let c = if won { theme::GREEN } else { theme::RED_DIM };
        let line = format!("  {} {} — {}", icon, bet.bet_type.label(), bet.amount);
        draw_str(buf, area, (w - line.len()) / 2, by + i, &line, c);
    }

    draw_number_grid(buf, area, w, h, Some(result));

    draw_str(buf, area, (w - 30) / 2, h - 2, "[ENTER] New Round  ·  [ESC] Back", theme::DARK_GRAY);
}

fn draw_number_grid(buf: &mut Buffer, area: Rect, w: usize, h: usize, highlight: Option<u8>) {
    // Compact number grid at the bottom
    let grid_y = h.saturating_sub(6);
    let grid_x = (w.saturating_sub(48)) / 2;

    // Zero
    let zero_color = if highlight == Some(0) { theme::GREEN } else { Color::Rgb(0, 80, 0) };
    draw_str(buf, area, grid_x, grid_y, " 0 ", zero_color);

    // Numbers 1-36 in 3 rows of 12
    for row in 0..3 {
        let y = grid_y + 1 + row;
        for col in 0..12 {
            let num = (col * 3 + (3 - row)) as u8; // standard roulette layout
            if num > 36 { continue; }
            let x = grid_x + 3 + col * 4;

            let is_highlighted = highlight == Some(num);
            let base_color = match number_color(num) {
                RouletteColor::Red => if is_highlighted { theme::RED } else { Color::Rgb(100, 0, 0) },
                RouletteColor::Black => if is_highlighted { theme::WHITE } else { Color::Rgb(60, 60, 70) },
                RouletteColor::Green => theme::GREEN,
            };

            let bg = if is_highlighted { Color::Rgb(40, 40, 0) } else { theme::BG };
            let s = format!("{:>2} ", num);
            let mut cx = area.left() + x as u16;
            let cy = area.top() + y as u16;
            if cy < area.bottom() {
                for ch in s.chars() {
                    if cx >= area.right() { break; }
                    buf[(cx, cy)].set_char(ch).set_fg(base_color).set_bg(bg);
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
