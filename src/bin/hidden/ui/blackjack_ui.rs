use ratatui::prelude::*;
use ratatui::buffer::Buffer;
use crate::app::App;
use crate::games::blackjack::*;
use crate::ui::theme;

// ── Card Rendering ──────────────────────────────────────────────────

const CARD_W: usize = 7;
const CARD_H: usize = 5;

/// Draw a single card at position
fn draw_card(buf: &mut Buffer, area: Rect, x: usize, y: usize, card: &Card, face_up: bool) {
    let ax = area.left() + x as u16;
    let ay = area.top() + y as u16;

    if !face_up {
        // Face down card
        let lines = [
            "┌─────┐",
            "│░░░░░│",
            "│░░░░░│",
            "│░░░░░│",
            "└─────┘",
        ];
        for (dy, line) in lines.iter().enumerate() {
            let cy = ay + dy as u16;
            if cy >= area.bottom() { continue; }
            let mut cx = ax;
            for ch in line.chars() {
                if cx >= area.right() { break; }
                let fg = if ch == '░' { Color::Rgb(60, 20, 20) } else { theme::AMBER_DIM };
                buf[(cx, cy)].set_char(ch).set_fg(fg).set_bg(theme::BG);
                cx += 1;
            }
        }
        return;
    }

    let rank = card.rank.label();
    let suit = card.suit.symbol();
    let suit_color = if card.suit.is_red() { theme::CARD_RED } else { theme::CARD_BLACK };

    // Top-left rank+suit, center suit, bottom-right rank+suit
    let r_pad = if rank.len() == 1 { " " } else { "" };
    let lines = [
        format!("┌─────┐"),
        format!("│{}{}{} │", rank, r_pad, suit),
        format!("│  {}  │", suit),
        format!("│ {}{} │", r_pad, rank),  // intentionally mirrored without suit for space
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
fn draw_hand(buf: &mut Buffer, area: Rect, x: usize, y: usize, cards: &[Card], face_up_count: usize) {
    let overlap = 4; // how many chars each card overlaps
    for (i, card) in cards.iter().enumerate() {
        let cx = x + i * (CARD_W - overlap + 1);
        let up = i < face_up_count;
        draw_card(buf, area, cx, y, card, up);
    }
}

// ── Blackjack Screen ────────────────────────────────────────────────

pub fn draw_blackjack(f: &mut Frame, app: &App, game: &BlackjackGame, area: Rect) {
    let buf = f.buffer_mut();
    let w = area.width as usize;
    let h = area.height as usize;

    // Fill background
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            buf[(x, y)].set_char(' ').set_bg(theme::BG);
        }
    }

    // Table felt — subtle green tint in the play area
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
        GamePhase::Betting => draw_betting(buf, area, game, app, w, h),
        GamePhase::Dealing | GamePhase::PlayerTurn => {
            draw_hands(buf, area, game, app, w, h, false);
            if game.phase == GamePhase::PlayerTurn {
                draw_actions(buf, area, game, w, h);
            }
        }
        GamePhase::DealerTurn | GamePhase::DealerRevealing => {
            draw_hands(buf, area, game, app, w, h, true);
        }
        GamePhase::Resolving | GamePhase::Result(_) => {
            draw_hands(buf, area, game, app, w, h, true);
            draw_result(buf, area, game, app, w, h);
        }
    }
}

fn draw_betting(buf: &mut Buffer, area: Rect, game: &BlackjackGame, app: &App, w: usize, h: usize) {
    let cy = h / 2 - 2;

    draw_str(buf, area, (w - 20) / 2, cy, "╔══════════════════╗", theme::AMBER);
    draw_str(buf, area, (w - 20) / 2, cy + 1, "║   PLACE YOUR BET  ║", theme::AMBER);
    draw_str(buf, area, (w - 20) / 2, cy + 2, "╚══════════════════╝", theme::AMBER);

    let bet_display = if game.bet_input.is_empty() {
        "_ ".to_string()
    } else {
        format!("{}_", game.bet_input)
    };

    let bet_line = format!("◈ {} TOKENS", bet_display);
    draw_str(buf, area, (w - bet_line.len()) / 2, cy + 4, &bet_line, theme::GREEN);

    let balance = format!("Balance: {} tokens", app.tokens);
    draw_str(buf, area, (w - balance.len()) / 2, cy + 6, &balance, theme::GRAY);

    let hint = "[0-9] Enter amount  ·  [ENTER] Deal  ·  [A] All-in  ·  [ESC] Back";
    let hx = (w.saturating_sub(hint.len())) / 2;
    draw_str(buf, area, hx, h - 2, hint, theme::DARK_GRAY);
}

fn draw_hands(buf: &mut Buffer, area: Rect, game: &BlackjackGame, _app: &App, w: usize, h: usize, show_dealer: bool) {
    let cards_x = (w.saturating_sub(30)) / 2;

    // Dealer hand (top)
    let dealer_y = 4;
    draw_str(buf, area, cards_x, dealer_y - 1, "DEALER", theme::RED_DIM);

    let dealer_face_up = if show_dealer {
        game.dealer_hand.len()
    } else {
        1 // Only first card visible during player turn
    };
    draw_hand(buf, area, cards_x, dealer_y, &game.dealer_hand, dealer_face_up);

    // Dealer value
    let dval = if show_dealer {
        format!(" {}", game.dealer_value())
    } else if !game.dealer_hand.is_empty() {
        format!(" {}", game.dealer_hand[0].rank.value())
    } else {
        String::new()
    };
    let dval_x = cards_x + game.dealer_hand.len() * 4 + 4;
    draw_str(buf, area, dval_x, dealer_y + 2, &dval, theme::GRAY);

    // Player hand (bottom)
    let player_y = h.saturating_sub(10);
    draw_str(buf, area, cards_x, player_y - 1, "YOU", theme::CYAN);
    draw_hand(buf, area, cards_x, player_y, &game.player_hand, game.player_hand.len());

    // Player value
    let pval = format!(" {}", game.player_value());
    let pval_color = if game.player_value() > 21 { theme::RED } else if game.player_value() == 21 { theme::GREEN } else { theme::WHITE };
    let pval_x = cards_x + game.player_hand.len() * 4 + 4;
    draw_str(buf, area, pval_x, player_y + 2, &pval, pval_color);

    // Bet display
    let bet_str = format!("BET: {} {}", game.bet, if game.doubled { "(DOUBLED)" } else { "" });
    draw_str(buf, area, (w - bet_str.len()) / 2, h / 2, &bet_str, theme::AMBER_DIM);
}

fn draw_actions(buf: &mut Buffer, area: Rect, game: &BlackjackGame, w: usize, h: usize) {
    let y = h - 2;
    let can_double = game.player_hand.len() == 2;

    let mut hint = String::from("[H] Hit  ·  [S] Stand");
    if can_double {
        hint.push_str("  ·  [D] Double Down");
    }

    draw_str(buf, area, (w.saturating_sub(hint.len())) / 2, y, &hint, theme::GREEN_DIM);
}

fn draw_result(buf: &mut Buffer, area: Rect, game: &BlackjackGame, _app: &App, w: usize, h: usize) {
    if let GamePhase::Result(ref outcome) = game.phase {
        let label = outcome.label();
        let color = if outcome.is_win() { theme::GREEN } else if matches!(outcome, Outcome::Push) { theme::AMBER } else { theme::RED };

        // Big result banner
        let banner_w = label.len() + 6;
        let bx = (w.saturating_sub(banner_w)) / 2;
        let by = h / 2 - 1;

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
