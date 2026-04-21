use ratatui::style::Color;

// ── CRT Retro Doom Palette ──────────────────────────────────────────────
// Dark, gritty, amber/green phosphor with blood red accents.

/// Deep black background — the void
pub const BG: Color = Color::Rgb(8, 8, 12);

/// Amber phosphor — primary text, borders, warmth
pub const AMBER: Color = Color::Rgb(255, 176, 0);

/// Dim amber — secondary text, muted elements
pub const AMBER_DIM: Color = Color::Rgb(140, 100, 0);

/// Toxic green — accents, wins, positive
pub const GREEN: Color = Color::Rgb(0, 255, 65);

/// Dim green — subtle highlights
pub const GREEN_DIM: Color = Color::Rgb(0, 120, 30);

/// Cyan — cool accents, info, selection
pub const CYAN: Color = Color::Rgb(0, 255, 255);

/// Dim cyan
pub const CYAN_DIM: Color = Color::Rgb(0, 120, 140);

/// Blood red — danger, losses, Doom energy
pub const RED: Color = Color::Rgb(255, 0, 0);

/// Dim red — warnings, low health
pub const RED_DIM: Color = Color::Rgb(140, 0, 0);

/// Hot magenta — jackpots, rare events, neon signs
pub const MAGENTA: Color = Color::Rgb(255, 0, 200);

/// White — high contrast text
pub const WHITE: Color = Color::Rgb(220, 220, 220);

/// Muted gray — disabled, ghosts
pub const GRAY: Color = Color::Rgb(80, 80, 90);

/// Dark gray — subtle borders, scan lines
pub const DARK_GRAY: Color = Color::Rgb(30, 30, 35);

/// HUD background — slightly lighter than void
pub const HUD_BG: Color = Color::Rgb(15, 15, 20);

/// Card red (suits)
pub const CARD_RED: Color = Color::Rgb(255, 50, 50);

/// Card black (suits)  
pub const CARD_BLACK: Color = Color::Rgb(200, 200, 210);
