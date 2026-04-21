use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};
use crate::app::App;
use super::theme;

pub fn draw_hud(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(theme::AMBER_DIM))
        .style(Style::default().bg(theme::HUD_BG));

    // Doom-style status bar: tokens left, screen name, stats
    let screen_name = match &app.screen {
        crate::app::Screen::Boot => "BOOT",
        crate::app::Screen::Hub => "LOBBY",
        crate::app::Screen::Blackjack => "BLACKJACK",
        crate::app::Screen::Slots => "SLOTS",
        crate::app::Screen::Roulette => "ROULETTE",
        crate::app::Screen::War => "WAR",
        crate::app::Screen::Baccarat => "BACCARAT",
        crate::app::Screen::VideoPoker => "VIDEO POKER",
        crate::app::Screen::Keno => "KENO",
        crate::app::Screen::SicBo => "SIC BO",
        crate::app::Screen::Craps => "CRAPS",
        crate::app::Screen::GameOver => "FLATLINE",
    };

    let hud_line = Line::from(vec![
        Span::styled("  ◈ ", Style::default().fg(theme::AMBER)),
        Span::styled(
            format!("{}", app.tokens),
            Style::default()
                .fg(if app.tokens > 200 { theme::GREEN } else if app.tokens > 50 { theme::AMBER } else { theme::RED })
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" TOKENS", Style::default().fg(theme::GRAY)),
        Span::styled("  │  ", Style::default().fg(theme::AMBER_DIM)),
        Span::styled(screen_name, Style::default().fg(theme::CYAN).add_modifier(Modifier::BOLD)),
        Span::styled("  │  ", Style::default().fg(theme::AMBER_DIM)),
        Span::styled("[Q]", Style::default().fg(theme::GRAY)),
        Span::styled(" QUIT", Style::default().fg(theme::GRAY)),
    ]);

    let hud = Paragraph::new(hud_line)
        .block(block)
        .alignment(Alignment::Left);

    f.render_widget(hud, area);
}
