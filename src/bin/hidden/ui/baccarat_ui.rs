use ratatui::prelude::*;
use ratatui::buffer::Buffer;
use crate::app::App;
use crate::games::baccarat::*;
use crate::games::blackjack::Card;
use crate::ui::theme;

// ── Card Rendering ──────────────────────────────────────────────────

const CARD_W: usize = 7;
const CARD_H: usize = 5;

/// Draw a single card at position
fn draw_card(buf: &mut Buffer, area: Rect, x: usize, y: usize, card: &Card) {
    let ax = area.left() + x as u16;
    let ay = area.top() + y as u16;

    let rank = card.rank.label();
    let suit = card.suit.symbol();
    let suit_color = if card.suit.is_red() { theme::CARD_RED } else { theme::CARD_BLACK };

    let r_pad = if rank.len() == 1 { " " } else { "" };
    let lines = [
        format!("┌─────┐"),
        format!("│{}{}{} │", rank, r_pad, suit),
        format!("│  {}  │", suit),
        format!("│ {}{} │", r_pad, rank),
        format!("└─────┘"),
    ];

    for (dy, line) in lines.iter().enumerate() {
        let cy = ay + dy as u16;
        if cy >= area.bottom() { continue; }
        let mut cx = ax;
        for ch in line.chars() {
            if cx >= area.right() { break; }
            let fg = if ch == '┌' || ch == '┐' || ch == '└' || ch == '┘' || ch == '─' || ch == '│' {
                theme::AMBER_DIM
            } else if ch == '♠' || ch == '♥' || ch == '♦' || ch == '♣' {
                suit_color
            } else {
                theme::WHITE
            };
            buf[(cx, cy)].set_char(ch).set_fg(fg).set_bg(theme::BG);
            cx += 1;
        }
    }
}

/// Draw a hand of cards, overlapping
fn draw_hand(buf: &mut Buffer, area: Rect, x: usize, y: usize, cards: &[Card]) {
    let overlap = 4;
    for (i, card) in cards.iter().enumerate() {
        let cx = x + i * (CARD_W - overlap + 1);
        draw_card(buf, area, cx, y, card);
    }
}

// ── Baccarat Screen ─────────────────────────────────────────────────

pub fn draw_baccarat(f: &mut Frame, app: &App, game: &BaccaratGame, area: Rect) {
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
        BaccaratPhase::Betting => draw_betting(buf, area, game, app, w, h),
        BaccaratPhase::Dealing => draw_hands(buf, area, game, app, w, h),
        BaccaratPhase::Result => {
            draw_hands(buf, area, game, app, w, h);
            draw_result(buf, area, game, app, w, h);
        }
    }
}

fn draw_betting(buf: &mut Buffer, area: Rect, game: &BaccaratGame, app: &App, w: usize, h: usize) {
    let cy = h / 2 - 4;

    draw_str(buf, area, (w - 20) / 2, cy, "╔══════════════════╗", theme::AMBER);
    draw_str(buf, area, (w - 20) / 2, cy + 1, "║   BACCARAT BET    ║", theme::AMBER);
    draw_str(buf, area, (w - 20) / 2, cy + 2, "╚══════════════════╝", theme::AMBER);

    let bet_display = if game.bet_input.is_empty() {
        "_ ".to_string()
    } else {
        format!("{}_", game.bet_input)
    };

    let bet_line = format!("◈ {} TOKENS", bet_display);
    draw_str(buf, area, (w - bet_line.len()) / 2, cy + 4, &bet_line, theme::GREEN);

    // Bet type selection
    let bet_options = ["PLAYER", "BANKER", "TIE"];
    let odds = ["1:1", "0.95:1", "8:1"];
    
    for (i, (option, odd)) in bet_options.iter().zip(odds.iter()).enumerate() {
        let selected = game.cursor == i;
        let fg = if selected { theme::CYAN } else { theme::GRAY };
        let marker = if selected { "►" } else { " " };
        let line = format!("{} {} ({})", marker, option, odd);
        draw_str(buf, area, (w - 20) / 2, cy + 6 + i, &line, fg);
    }

    let balance = format!("Balance: {} tokens", app.tokens);
    draw_str(buf, area, (w - balance.len()) / 2, cy + 10, &balance, theme::GRAY);

    let hint = "[↑↓] Select  ·  [0-9] Amount  ·  [ENTER] Deal  ·  [A] All-in  ·  [ESC] Back";
    let hx = (w.saturating_sub(hint.len())) / 2;
    draw_str(buf, area, hx, h - 2, hint, theme::DARK_GRAY);
}

fn draw_hands(buf: &mut Buffer, area: Rect, game: &BaccaratGame, _app: &App, w: usize, h: usize) {
    let cards_x = (w.saturating_sub(35)) / 2;

    // Banker hand (top)
    let banker_y = 4;
    draw_str(buf, area, cards_x, banker_y - 1, "BANKER", theme::RED_DIM);
    if !game.banker_hand.is_empty() {
        draw_hand(buf, area, cards_x, banker_y, &game.banker_hand);
    }

    // Banker total
    let bt = game.banker_total();
    let bt_str = format!(" {}", bt);
    let bt_color = if bt >= 8 { theme::AMBER } else { theme::GRAY };
    let bt_x = cards_x + game.banker_hand.len() * 4 + 4;
    draw_str(buf, area, bt_x, banker_y + 2, &bt_str, bt_color);

    // Player hand (bottom)
    let player_y = h.saturating_sub(10);
    draw_str(buf, area, cards_x, player_y - 1, "PLAYER", theme::CYAN);
    if !game.player_hand.is_empty() {
        draw_hand(buf, area, cards_x, player_y, &game.player_hand);
    }

    // Player total
    let pt = game.player_total();
    let pt_str = format!(" {}", pt);
    let pt_color = if pt >= 8 { theme::AMBER } else { theme::GRAY };
    let pt_x = cards_x + game.player_hand.len() * 4 + 4;
    draw_str(buf, area, pt_x, player_y + 2, &pt_str, pt_color);

    // Bet display
    let bet_type_str = match game.bet_type {
        BaccaratBet::Player => "PLAYER",
        BaccaratBet::Banker => "BANKER", 
        BaccaratBet::Tie => "TIE",
    };
    let bet_str = format!("BET: {} on {}", game.bet, bet_type_str);
    draw_str(buf, area, (w - bet_str.len()) / 2, h / 2, &bet_str, theme::AMBER_DIM);
}

fn draw_result(buf: &mut Buffer, area: Rect, game: &BaccaratGame, _app: &App, w: usize, h: usize) {
    if let Some(ref outcome) = game.outcome {
        let label = match outcome {
            BaccaratOutcome::PlayerWins => "PLAYER WINS",
            BaccaratOutcome::BankerWins => "BANKER WINS",
            BaccaratOutcome::Tie => "TIE",
        };
        let color = if game.last_payout > 0 { theme::GREEN } else if game.last_payout < 0 { theme::RED } else { theme::AMBER };

        // Result banner
        let banner_w = label.len() + 6;
        let bx = (w.saturating_sub(banner_w)) / 2;
        let by = h / 2 + 3;

        draw_str(buf, area, bx, by, &format!("╔{}╗", "═".repeat(banner_w - 2)), color);
        let pad = (banner_w - 2 - label.len()) / 2;
        draw_str(buf, area, bx, by + 1, &format!("║{}{}{}║", " ".repeat(pad), label, " ".repeat(banner_w - 2 - pad - label.len())), color);
        draw_str(buf, area, bx, by + 2, &format!("╚{}╝", "═".repeat(banner_w - 2)), color);

        // Payout
        let payout_str = if game.last_payout > 0 {
            format!("+{} TOKENS", game.last_payout)
        } else if game.last_payout < 0 {
            format!("{} TOKENS", game.last_payout)
        } else {
            "BET RETURNED".to_string()
        };
        let payout_color = if game.last_payout > 0 { theme::GREEN } else if game.last_payout < 0 { theme::RED } else { theme::GRAY };
        draw_str(buf, area, (w - payout_str.len()) / 2, by + 4, &payout_str, payout_color);

        draw_str(buf, area, (w - 28) / 2, h - 2, "[ENTER] New Hand  ·  [ESC] Back", theme::DARK_GRAY);
    }
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