use ratatui::prelude::*;
use ratatui::buffer::Buffer;
use crate::app::App;
use crate::games::video_poker::*;
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

// ── Video Poker Screen ──────────────────────────────────────────────

pub fn draw_video_poker(f: &mut Frame, app: &App, game: &VideoPokerGame, area: Rect) {
    let buf = f.buffer_mut();
    let w = area.width as usize;
    let h = area.height as usize;

    // Fill background
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            buf[(x, y)].set_char(' ').set_bg(theme::BG);
        }
    }

    // Machine background
    let machine_top = 2;
    let machine_bot = h.saturating_sub(2);
    for y in machine_top..machine_bot {
        let ay = area.top() + y as u16;
        if ay >= area.bottom() { continue; }
        for x in 2..(w.saturating_sub(2)) {
            let ax = area.left() + x as u16;
            if ax >= area.right() { continue; }
            buf[(ax, ay)].set_bg(Color::Rgb(8, 8, 15));
        }
    }

    // Machine border
    for x in 1..(w.saturating_sub(1)) {
        let ax = area.left() + x as u16;
        let at = area.top() + machine_top as u16;
        let ab = area.top() + machine_bot as u16;
        if ax < area.right() {
            if at < area.bottom() { buf[(ax, at)].set_char('━').set_fg(theme::CYAN_DIM); }
            if ab < area.bottom() { buf[(ax, ab)].set_char('━').set_fg(theme::CYAN_DIM); }
        }
    }

    match &game.phase {
        VideoPokerPhase::Betting => draw_betting(buf, area, game, app, w, h),
        VideoPokerPhase::Hold | VideoPokerPhase::Drawing => draw_cards(buf, area, game, app, w, h),
        VideoPokerPhase::Result => {
            draw_cards(buf, area, game, app, w, h);
            draw_result(buf, area, game, app, w, h);
        }
        _ => {}
    }

    // Always show pay table
    draw_paytable(buf, area, game, w, h);
}

fn draw_betting(buf: &mut Buffer, area: Rect, game: &VideoPokerGame, app: &App, w: usize, h: usize) {
    let cy = h / 2 - 2;

    draw_str(buf, area, (w - 22) / 2, cy, "┏━━━━━━━━━━━━━━━━━━━━┓", theme::CYAN);
    draw_str(buf, area, (w - 22) / 2, cy + 1, "┃   VIDEO POKER BET   ┃", theme::CYAN);
    draw_str(buf, area, (w - 22) / 2, cy + 2, "┗━━━━━━━━━━━━━━━━━━━━┛", theme::CYAN);

    let bet_display = if game.bet_input.is_empty() {
        "_ ".to_string()
    } else {
        format!("{}_", game.bet_input)
    };

    let bet_line = format!("◈ {} CREDITS", bet_display);
    draw_str(buf, area, (w - bet_line.len()) / 2, cy + 4, &bet_line, theme::GREEN);

    let balance = format!("Credits: {}", app.tokens);
    draw_str(buf, area, (w - balance.len()) / 2, cy + 6, &balance, theme::GRAY);

    let hint = "[0-9] Enter amount  ·  [ENTER] Deal  ·  [A] All-in  ·  [ESC] Back";
    let hx = (w.saturating_sub(hint.len())) / 2;
    draw_str(buf, area, hx, h - 2, hint, theme::DARK_GRAY);
}

fn draw_cards(buf: &mut Buffer, area: Rect, game: &VideoPokerGame, _app: &App, w: usize, h: usize) {
    let cards_x = (w.saturating_sub(40)) / 2;
    let cards_y = h / 2 - 3;

    // Draw 5 cards in a row
    for i in 0..5 {
        let x = cards_x + i * 8;
        draw_card(buf, area, x, cards_y, &game.hand[i]);

        // HOLD label and selection
        let hold_color = if game.held[i] { theme::GREEN } else { theme::DARK_GRAY };
        let hold_text = if game.held[i] { "HOLD" } else { "    " };
        draw_str(buf, area, x + 1, cards_y - 1, hold_text, hold_color);

        // Cursor indicator
        if game.phase == VideoPokerPhase::Hold && game.cursor == i {
            draw_str(buf, area, x, cards_y + CARD_H, "^^^^^", theme::CYAN);
        }
    }

    // Bet display
    let bet_str = format!("BET: {} CREDITS", game.bet);
    draw_str(buf, area, (w - bet_str.len()) / 2, cards_y + CARD_H + 2, &bet_str, theme::AMBER_DIM);

    // Controls
    if game.phase == VideoPokerPhase::Hold {
        let hint = "[←→] Select Card  ·  [SPACE] Toggle Hold  ·  [ENTER] Draw";
        draw_str(buf, area, (w.saturating_sub(hint.len())) / 2, h - 2, hint, theme::GREEN_DIM);
    }
}

fn draw_paytable(buf: &mut Buffer, area: Rect, game: &VideoPokerGame, w: usize, _h: usize) {
    let table_x = w.saturating_sub(20);
    let table_y = 4;

    draw_str(buf, area, table_x, table_y, "PAYTABLE", theme::AMBER);
    
    let hands = [
        ("Royal Flush", 250),
        ("Straight Flush", 50),
        ("4 of a Kind", 25),
        ("Full House", 9),
        ("Flush", 6),
        ("Straight", 4),
        ("3 of a Kind", 3),
        ("Two Pair", 2),
        ("Jacks+", 1),
    ];

    for (i, (hand, mult)) in hands.iter().enumerate() {
        let y = table_y + 2 + i;
        let line = format!("{:<12} {:>3}x", hand, mult);
        let color = if let Some(ref result) = game.result {
            if result.label() == *hand { theme::GREEN } else { theme::GRAY }
        } else {
            theme::GRAY
        };
        draw_str(buf, area, table_x, y, &line, color);
    }
}

fn draw_result(buf: &mut Buffer, area: Rect, game: &VideoPokerGame, _app: &App, w: usize, h: usize) {
    if let Some(ref result) = game.result {
        let label = result.label();
        let color = if game.last_payout > 0 { theme::GREEN } else { theme::RED };

        // Result banner
        let banner_w = label.len() + 6;
        let bx = (w.saturating_sub(banner_w)) / 2;
        let by = h / 2 + 10;

        draw_str(buf, area, bx, by, &format!("┏{}┓", "━".repeat(banner_w - 2)), color);
        let pad = (banner_w - 2 - label.len()) / 2;
        draw_str(buf, area, bx, by + 1, &format!("┃{}{}{}┃", " ".repeat(pad), label, " ".repeat(banner_w - 2 - pad - label.len())), color);
        draw_str(buf, area, bx, by + 2, &format!("┗{}┛", "━".repeat(banner_w - 2)), color);

        // Payout
        let payout_str = if game.last_payout > 0 {
            format!("+{} CREDITS", game.last_payout)
        } else {
            format!("-{} CREDITS", game.bet)
        };
        let payout_color = if game.last_payout > 0 { theme::GREEN } else { theme::RED };
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