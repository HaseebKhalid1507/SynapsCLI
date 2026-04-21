use ratatui::prelude::*;
use ratatui::buffer::Buffer;
use std::time::Instant;

/// A screen transition effect — CRT channel-switch glitch
pub struct Transition {
    pub active: bool,
    pub start: Instant,
    pub duration_ms: u64,
}

impl Transition {
    pub fn new() -> Self {
        Self {
            active: false,
            start: Instant::now(),
            duration_ms: 250,
        }
    }

    pub fn trigger(&mut self) {
        self.active = true;
        self.start = Instant::now();
    }

    pub fn progress(&self) -> f64 {
        if !self.active { return 1.0; }
        let elapsed = self.start.elapsed().as_millis() as f64;
        (elapsed / self.duration_ms as f64).min(1.0)
    }

    pub fn is_done(&self) -> bool {
        !self.active || self.progress() >= 1.0
    }

    pub fn tick(&mut self) {
        if self.active && self.is_done() {
            self.active = false;
        }
    }
}

/// Draw glitch transition overlay
pub fn draw_transition(buf: &mut Buffer, area: Rect, progress: f64, frame: u64) {
    let w = area.width as usize;
    let h = area.height as usize;

    // Phase 1 (0-0.4): corruption spreads from center
    // Phase 2 (0.4-0.7): full static
    // Phase 3 (0.7-1.0): new screen fades in

    let noise_chars = ['█', '▓', '▒', '░', '╳', '▪', '─', '│', '┼', ' '];

    if progress < 0.4 {
        // Corruption bands spreading from center
        let spread = (progress / 0.4 * h as f64 / 2.0) as usize;
        let center = h / 2;
        let top = center.saturating_sub(spread);
        let bot = (center + spread).min(h);

        for y in top..bot {
            let ay = area.top() + y as u16;
            if ay >= area.bottom() { continue; }
            for x in 0..w {
                let ax = area.left() + x as u16;
                if ax >= area.right() { continue; }
                let seed = (frame as usize).wrapping_mul(x + 1).wrapping_add(y * 31);
                let nch = noise_chars[seed % noise_chars.len()];
                let intensity = ((seed * 7) % 60 + 10) as u8;
                buf[(ax, ay)].set_char(nch)
                    .set_fg(Color::Rgb(intensity, intensity / 2, intensity))
                    .set_bg(Color::Rgb(5, 5, 10));
            }
        }

        // Horizontal displacement on nearby lines
        for offset in 0..3 {
            let lines = [center.saturating_sub(spread + offset), (center + spread + offset).min(h - 1)];
            for &y in &lines {
                let ay = area.top() + y as u16;
                if ay >= area.bottom() || ay < area.top() { continue; }
                let shift = ((frame as usize + offset) * 3) % 8;
                for x in 0..w.saturating_sub(shift) {
                    let ax_src = area.left() + (x + shift) as u16;
                    let ax_dst = area.left() + x as u16;
                    if ax_src < area.right() && ax_dst < area.right() {
                        let cell = buf[(ax_src, ay)].clone();
                        buf[(ax_dst, ay)] = cell;
                    }
                }
            }
        }
    } else if progress < 0.7 {
        // Full static
        for y in 0..h {
            let ay = area.top() + y as u16;
            if ay >= area.bottom() { continue; }
            for x in 0..w {
                let ax = area.left() + x as u16;
                if ax >= area.right() { continue; }
                let seed = (frame as usize).wrapping_mul(x + 1).wrapping_add(y * 37);
                let nch = noise_chars[seed % noise_chars.len()];
                let intensity = ((seed * 13) % 40 + 5) as u8;
                buf[(ax, ay)].set_char(nch)
                    .set_fg(Color::Rgb(intensity, intensity, intensity + 10))
                    .set_bg(Color::Rgb(3, 3, 5));
            }
        }
    }
    // Phase 3: let the new screen show through (no overlay)
}
