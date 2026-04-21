use ratatui::prelude::*;
use ratatui::buffer::Buffer;
use crate::app::App;
use crate::games::war::*;
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

// ── War Screen ──────────────────────────────────────────────────────

pub fn draw_war(f: &mut Frame, app: &App, game: &WarGame, area: Rect) {
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
        WarPhase::Betting => draw_betting(buf, area, game, app, w, h),
        WarPhase::Reveal => draw_cards(buf, area, game, app, w, h, false),
        WarPhase::War => draw_war_state(buf, area, game, app, w, h),
        WarPhase::WarReveal => draw_cards(buf, area, game, app, w, h, true),
        WarPhase::Result => {
            draw_cards(buf, area, game, app, w, h, true);
            draw_result(buf, area, game, app, w, h);
        }
    }
}

fn draw_betting(buf: &mut Buffer, area: Rect, game: &WarGame, app: &App, w: usize, h: usize) {
    let cy = h / 2 - 2;

    draw_str(buf, area, (w - 18) / 2, cy, "╔════════════════╗", theme::AMBER);
    draw_str(buf, area, (w - 18) / 2, cy + 1, "║  WAR CARD BET  ║", theme::AMBER);
    draw_str(buf, area, (w - 18) / 2, cy + 2, "╚════════════════╝", theme::AMBER);

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

fn draw_cards(buf: &mut Buffer, area: Rect, game: &WarGame, _app: &App, w: usize, h: usize, show_war: bool) {
    let cards_x = (w.saturating_sub(20)) / 2;

    // Dealer card
    let dealer_y = 4;
    draw_str(buf, area, cards_x, dealer_y - 1, "DEALER", theme::RED_DIM);
    if let Some(ref card) = game.dealer_card {
        draw_card(buf, area, cards_x, dealer_y, card);
    }

    // Player card
    let player_y = h.saturating_sub(10);
    draw_str(buf, area, cards_x + 10, player_y - 1, "YOU", theme::CYAN);
    if let Some(ref card) = game.player_card {
        draw_card(buf, area, cards_x + 10, player_y, card);
    }

    // War cards if present
    if show_war && game.war_dealer.is_some() && game.war_player.is_some() {
        let war_x = cards_x + 5;
        draw_str(buf, area, war_x, dealer_y - 2, "WAR", theme::MAGENTA);
        
        if let Some(ref card) = game.war_dealer {
            draw_card(buf, area, war_x, dealer_y + 7, card);
        }
        if let Some(ref card) = game.war_player {
            draw_card(buf, area, war_x, player_y - 7, card);
        }
    }

    // Bet display
    let bet_str = format!("BET: {}", game.bet);
    draw_str(buf, area, (w - bet_str.len()) / 2, h / 2, &bet_str, theme::AMBER_DIM);
}

fn draw_war_state(buf: &mut Buffer, area: Rect, game: &WarGame, app: &App, w: usize, h: usize) {
    draw_cards(buf, area, game, app, w, h, false);
    
    let msg = "TIE! GOING TO WAR...";
    draw_str(buf, area, (w - msg.len()) / 2, h / 2 - 2, msg, theme::MAGENTA);
    
    let hint = "[SPACE] Continue to War";
    draw_str(buf, area, (w - hint.len()) / 2, h - 2, hint, theme::GREEN_DIM);
}

fn draw_result(buf: &mut Buffer, area: Rect, game: &WarGame, _app: &App, w: usize, h: usize) {
    if let Some(ref outcome) = game.outcome {
        let label = match outcome {
            WarOutcome::PlayerWin => "YOU WIN!",
            WarOutcome::DealerWin => "DEALER WINS",
            WarOutcome::TieGoToWar => "TIE - WAR",
            WarOutcome::PlayerWinsWar => "YOU WIN WAR!",
            WarOutcome::DealerWinsWar => "DEALER WINS WAR",
        };
        let color = match outcome {
            WarOutcome::PlayerWin | WarOutcome::PlayerWinsWar => theme::GREEN,
            _ => theme::RED,
        };

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
            "NO CHANGE".to_string()
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