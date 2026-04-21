mod blackjack_ui;
mod boot;
mod hub;
mod hud;
mod roulette_ui;
mod slots_ui;
mod war_ui;
mod baccarat_ui;
mod video_poker_ui;
mod keno_ui;
mod sicbo_ui;
mod craps_ui;
pub mod theme;
pub mod transition;

use ratatui::Frame;
use ratatui::style::Color;
use crate::app::App;

pub fn draw(f: &mut Frame, app: &App) {
    use crate::app::Screen;
    use ratatui::layout::{Layout, Constraint, Direction};

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),     // main area
            Constraint::Length(3),  // HUD
        ])
        .split(f.area());

    // Main content
    match &app.screen {
        Screen::Boot => boot::draw_boot(f, app, chunks[0]),
        Screen::Hub => hub::draw_hub(f, app, chunks[0]),
        Screen::Blackjack => blackjack_ui::draw_blackjack(f, app, &app.blackjack, chunks[0]),
        Screen::Slots => slots_ui::draw_slots(f, app, &app.slots, chunks[0]),
        Screen::Roulette => roulette_ui::draw_roulette(f, app, &app.roulette, chunks[0]),
        Screen::War => war_ui::draw_war(f, app, &app.war, chunks[0]),
        Screen::Baccarat => baccarat_ui::draw_baccarat(f, app, &app.baccarat, chunks[0]),
        Screen::VideoPoker => video_poker_ui::draw_video_poker(f, app, &app.video_poker, chunks[0]),
        Screen::Keno => keno_ui::draw_keno(f, app, &app.keno, chunks[0]),
        Screen::SicBo => sicbo_ui::draw_sicbo(f, app, &app.sicbo, chunks[0]),
        Screen::Craps => craps_ui::draw_craps(f, app, &app.craps, chunks[0]),
        Screen::GameOver => draw_game_over(f, app, chunks[0]),
    }

    // HUD (always visible)
    hud::draw_hud(f, app, chunks[1]);

    // ── CRT OVERLAY (always on) ─────────────────────────────────────
    draw_crt_overlay(f, app);

    // ── TRANSITION OVERLAY ──────────────────────────────────────────
    if app.transition.active {
        let progress = app.transition.progress();
        let area = f.area();
        transition::draw_transition(f.buffer_mut(), area, progress, app.frame);
    }
}

/// CRT scan lines + ambient flicker overlaid on everything
fn draw_crt_overlay(f: &mut Frame, app: &App) {
    let area = f.area();
    let buf = f.buffer_mut();

    for y in (area.top()..area.bottom()).step_by(3) {
        for x in area.left()..area.right() {
            let cell = &mut buf[(x, y)];
            if let Color::Rgb(r, g, b) = cell.bg {
                cell.set_bg(Color::Rgb(r.saturating_sub(3), g.saturating_sub(3), b.saturating_sub(3)));
            }
        }
    }

    if app.frame % 4 == 0 {
        let seed = app.frame as usize;
        let gx = ((seed * 7919) % area.width as usize) as u16 + area.left();
        let gy = ((seed * 1049) % area.height as usize) as u16 + area.top();
        if gx < area.right() && gy < area.bottom() {
            let glitch_chars = ['▒', '░', '▓', '╳', '¤', '▪'];
            let gch = glitch_chars[seed % glitch_chars.len()];
            buf[(gx, gy)].set_char(gch).set_fg(Color::Rgb(40, 40, 50));
        }
    }
}

/// Game over screen with corruption effect
fn draw_game_over(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let buf = f.buffer_mut();
    let w = area.width as usize;
    let h = area.height as usize;

    let noise_chars = ['░', '▒', '▓', '█', '·', ':', ' ', ' '];
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let seed = (app.frame as usize).wrapping_mul(x as usize + 1).wrapping_add(y as usize * 37);
            let nch = noise_chars[seed % noise_chars.len()];
            let intensity = ((seed * 13) % 30) as u8;
            buf[(x, y)].set_char(nch).set_fg(Color::Rgb(intensity, intensity / 2, intensity / 3)).set_bg(Color::Rgb(5, 0, 0));
        }
    }

    let msg = "CONNECTION TERMINATED";
    let mx = (w.saturating_sub(msg.len())) / 2;
    let my = h / 2 - 1;
    let ay = area.top() + my as u16;
    if ay < area.bottom() {
        let blink = (app.frame / 20) % 2 == 0;
        let mut cx = area.left() + mx as u16;
        for ch in msg.chars() {
            if cx >= area.right() { break; }
            let color = if blink { theme::RED } else { theme::RED_DIM };
            buf[(cx, ay)].set_char(ch).set_fg(color).set_bg(Color::Rgb(30, 0, 0));
            cx += 1;
        }
    }

    let sub = "TOKENS DEPLETED — PRESS ANY KEY TO REBOOT";
    let sx = (w.saturating_sub(sub.len())) / 2;
    let sy = h / 2 + 1;
    let ay2 = area.top() + sy as u16;
    if ay2 < area.bottom() {
        let mut cx = area.left() + sx as u16;
        for ch in sub.chars() {
            if cx >= area.right() { break; }
            buf[(cx, ay2)].set_char(ch).set_fg(theme::GRAY);
            cx += 1;
        }
    }
}
