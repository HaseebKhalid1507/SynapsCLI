use rand::Rng;

// ── Symbols ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Symbol {
    Skull,
    Diamond,
    Seven,
    Lightning,
    Fire,
    Bar,
    Cherry,
    Wild,
}

impl Symbol {
    pub fn icon(&self) -> &'static str {
        match self {
            Symbol::Skull => "💀",
            Symbol::Diamond => "◆",
            Symbol::Seven => "7",
            Symbol::Lightning => "⚡",
            Symbol::Fire => "🔥",
            Symbol::Bar => "≡",
            Symbol::Cherry => "●",
            Symbol::Wild => "★",
        }
    }

    /// ASCII-friendly display for reel rendering
    pub fn label(&self) -> &'static str {
        match self {
            Symbol::Skull => "SKULL",
            Symbol::Diamond => " GEM ",
            Symbol::Seven => "  7  ",
            Symbol::Lightning => "BOLT ",
            Symbol::Fire => "FIRE ",
            Symbol::Bar => " BAR ",
            Symbol::Cherry => "CHRRY",
            Symbol::Wild => "WILD ",
        }
    }

    /// Weight for RNG — lower = rarer
    fn weight(&self) -> u32 {
        match self {
            Symbol::Skull => 2,     // jackpot — very rare
            Symbol::Diamond => 4,
            Symbol::Seven => 6,
            Symbol::Lightning => 8,
            Symbol::Fire => 10,
            Symbol::Bar => 12,
            Symbol::Cherry => 14,
            Symbol::Wild => 3,      // wild — rare but not jackpot rare
        }
    }
}

const ALL_SYMBOLS: [Symbol; 8] = [
    Symbol::Skull, Symbol::Diamond, Symbol::Seven, Symbol::Lightning,
    Symbol::Fire, Symbol::Bar, Symbol::Cherry, Symbol::Wild,
];

// ── Payout Table ────────────────────────────────────────────────────

/// Calculate payout multiplier for a 3-reel result.
pub fn calculate_multiplier(reels: &[Symbol; 3]) -> f64 {
    let [a, b, c] = reels;

    // Expand wilds: wild matches anything
    let matches_with_wild = |x: &Symbol, y: &Symbol| -> bool {
        *x == *y || *x == Symbol::Wild || *y == Symbol::Wild
    };

    let all_match = matches_with_wild(a, b) && matches_with_wild(b, c) && matches_with_wild(a, c);

    if all_match {
        // Determine the "real" symbol (non-wild)
        let real = if *a != Symbol::Wild { a }
            else if *b != Symbol::Wild { b }
            else if *c != Symbol::Wild { c }
            else { &Symbol::Wild }; // three wilds

        return match real {
            Symbol::Skull => 50.0,
            Symbol::Diamond => 25.0,
            Symbol::Seven => 15.0,
            Symbol::Lightning => 10.0,
            Symbol::Fire => 5.0,
            Symbol::Bar => 3.0,
            Symbol::Cherry => 2.0,
            Symbol::Wild => 50.0, // three wilds = jackpot
        };
    }

    // Two matching (any position)
    let two_match = matches_with_wild(a, b) || matches_with_wild(b, c) || matches_with_wild(a, c);
    if two_match {
        return 0.5; // half bet back
    }

    0.0 // loss
}

pub fn is_jackpot(reels: &[Symbol; 3]) -> bool {
    calculate_multiplier(reels) >= 50.0
}

// ── Reel Spinning ───────────────────────────────────────────────────

/// Pick a random symbol based on weights.
fn random_symbol() -> Symbol {
    let total_weight: u32 = ALL_SYMBOLS.iter().map(|s| s.weight()).sum();
    let mut rng = rand::rng();
    let mut roll = rng.random_range(0..total_weight);

    for sym in &ALL_SYMBOLS {
        let w = sym.weight();
        if roll < w {
            return *sym;
        }
        roll -= w;
    }
    Symbol::Cherry // fallback
}

/// Generate a reel strip (sequence of symbols for spinning animation).
pub fn generate_reel_strip(len: usize) -> Vec<Symbol> {
    (0..len).map(|_| random_symbol()).collect()
}

// ── Game State ──────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum SlotsPhase {
    Betting,
    Spinning,      // Reels are spinning
    Revealing(u8), // Reels stopping one by one (0, 1, 2)
    Result,
}

pub struct SlotsGame {
    pub phase: SlotsPhase,
    pub bet: u64,
    pub bet_input: String,
    /// Final reel results
    pub reels: [Symbol; 3],
    /// Reel strips for animation (each reel has a sequence)
    pub reel_strips: [Vec<Symbol>; 3],
    /// Current position in each reel strip
    pub reel_pos: [usize; 3],
    /// Frame counter for spin speed
    pub phase_timer: u64,
    /// Speed of each reel (frames per tick) — higher = slower
    pub reel_speed: [u64; 3],
    /// Payout multiplier for current spin
    pub multiplier: f64,
    pub last_payout: i64,
}

impl SlotsGame {
    pub fn new() -> Self {
        Self {
            phase: SlotsPhase::Betting,
            bet: 0,
            bet_input: String::new(),
            reels: [Symbol::Cherry; 3],
            reel_strips: [
                generate_reel_strip(40),
                generate_reel_strip(40),
                generate_reel_strip(40),
            ],
            reel_pos: [0; 3],
            phase_timer: 0,
            reel_speed: [2, 2, 2], // fast initially
            multiplier: 0.0,
            last_payout: 0,
        }
    }

    pub fn spin(&mut self, bet: u64) {
        self.bet = bet;
        self.last_payout = 0;
        self.phase_timer = 0;

        // Generate the final results first
        self.reels = [random_symbol(), random_symbol(), random_symbol()];

        // Generate reel strips with final symbol at the end
        for i in 0..3 {
            let mut strip = generate_reel_strip(30 + i * 8); // staggered lengths
            // Place the final result at the end
            strip.push(self.reels[i]);
            self.reel_strips[i] = strip;
        }

        self.reel_pos = [0; 3];
        self.reel_speed = [2, 2, 2];
        self.phase = SlotsPhase::Spinning;
    }

    /// Tick the spin animation. Returns true when all reels stopped.
    pub fn tick_spin(&mut self) -> bool {
        self.phase_timer += 1;

        match self.phase {
            SlotsPhase::Spinning => {
                // All reels spin fast for 30 frames, then start stopping
                for i in 0..3 {
                    if self.phase_timer % self.reel_speed[i] == 0 {
                        self.reel_pos[i] += 1;
                    }
                }

                // After 40 frames, start revealing
                if self.phase_timer > 40 {
                    self.phase = SlotsPhase::Revealing(0);
                    self.phase_timer = 0;
                }
                false
            }
            SlotsPhase::Revealing(stopped) => {
                // Spin remaining reels, decelerate the current one
                let current = stopped as usize;

                for i in 0..3 {
                    if i <= current {
                        // Decelerate current reel
                        if i == current {
                            self.reel_speed[i] = 2 + (self.phase_timer / 8).min(6);
                            if self.phase_timer % self.reel_speed[i] == 0 {
                                self.reel_pos[i] += 1;
                            }
                            // Stop when we reach the end of the strip
                            if self.reel_pos[i] >= self.reel_strips[i].len() - 1 {
                                self.reel_pos[i] = self.reel_strips[i].len() - 1;
                                if stopped < 2 {
                                    self.phase = SlotsPhase::Revealing(stopped + 1);
                                    self.phase_timer = 0;
                                } else {
                                    // All stopped
                                    self.multiplier = calculate_multiplier(&self.reels);
                                    self.last_payout = (self.bet as f64 * self.multiplier) as i64 - self.bet as i64;
                                    self.phase = SlotsPhase::Result;
                                    return true;
                                }
                            }
                        }
                        // Already stopped reels stay put
                    } else {
                        // Still spinning fast
                        if self.phase_timer % 2 == 0 {
                            self.reel_pos[i] += 1;
                            if self.reel_pos[i] >= self.reel_strips[i].len() {
                                self.reel_pos[i] = 0; // loop
                            }
                        }
                    }
                }
                false
            }
            _ => true,
        }
    }

    /// Get the current display symbol for a reel
    pub fn display_symbol(&self, reel: usize) -> Symbol {
        let strip = &self.reel_strips[reel];
        if strip.is_empty() { return Symbol::Cherry; }
        strip[self.reel_pos[reel] % strip.len()]
    }

    /// Get symbols above and below current for reel display
    pub fn reel_window(&self, reel: usize) -> [Symbol; 3] {
        let strip = &self.reel_strips[reel];
        let len = strip.len();
        if len == 0 { return [Symbol::Cherry; 3]; }
        let pos = self.reel_pos[reel] % len;
        let above = if pos == 0 { len - 1 } else { pos - 1 };
        let below = (pos + 1) % len;
        [strip[above], strip[pos], strip[below]]
    }

    pub fn new_spin(&mut self) {
        self.phase = SlotsPhase::Betting;
        self.bet = 0;
        self.bet_input.clear();
        self.phase_timer = 0;
        self.multiplier = 0.0;
    }
}

// ── Payout Table Display ────────────────────────────────────────────

pub fn payout_table() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        ("SKULL ×3", "50x", "JACKPOT"),
        ("GEM   ×3", "25x", ""),
        ("7     ×3", "15x", ""),
        ("BOLT  ×3", "10x", ""),
        ("FIRE  ×3", " 5x", ""),
        ("BAR   ×3", " 3x", ""),
        ("ANY   ×3", " 2x", ""),
        ("ANY   ×2", "0.5x", ""),
        ("★ WILD", "SUBS", ""),
    ]
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_skulls_is_jackpot() {
        let reels = [Symbol::Skull, Symbol::Skull, Symbol::Skull];
        assert_eq!(calculate_multiplier(&reels), 50.0);
        assert!(is_jackpot(&reels));
    }

    #[test]
    fn three_sevens() {
        let reels = [Symbol::Seven, Symbol::Seven, Symbol::Seven];
        assert_eq!(calculate_multiplier(&reels), 15.0);
    }

    #[test]
    fn three_bars() {
        let reels = [Symbol::Bar, Symbol::Bar, Symbol::Bar];
        assert_eq!(calculate_multiplier(&reels), 3.0);
    }

    #[test]
    fn wild_substitutes_for_match() {
        let reels = [Symbol::Wild, Symbol::Seven, Symbol::Seven];
        assert_eq!(calculate_multiplier(&reels), 15.0);

        let reels2 = [Symbol::Diamond, Symbol::Wild, Symbol::Diamond];
        assert_eq!(calculate_multiplier(&reels2), 25.0);
    }

    #[test]
    fn three_wilds_is_jackpot() {
        let reels = [Symbol::Wild, Symbol::Wild, Symbol::Wild];
        assert_eq!(calculate_multiplier(&reels), 50.0);
    }

    #[test]
    fn two_matching_returns_half() {
        let reels = [Symbol::Seven, Symbol::Seven, Symbol::Bar];
        assert_eq!(calculate_multiplier(&reels), 0.5);
    }

    #[test]
    fn no_match_returns_zero() {
        let reels = [Symbol::Seven, Symbol::Bar, Symbol::Fire];
        assert_eq!(calculate_multiplier(&reels), 0.0);
    }

    #[test]
    fn payout_calculation_jackpot() {
        // Bet 100, hit 50x jackpot = 5000 profit (5000 - 100)
        let reels = [Symbol::Skull, Symbol::Skull, Symbol::Skull];
        let bet = 100u64;
        let mult = calculate_multiplier(&reels);
        let payout = (bet as f64 * mult) as i64 - bet as i64;
        assert_eq!(payout, 4900);
    }

    #[test]
    fn payout_calculation_loss() {
        let reels = [Symbol::Seven, Symbol::Bar, Symbol::Fire];
        let bet = 50u64;
        let mult = calculate_multiplier(&reels);
        let payout = (bet as f64 * mult) as i64 - bet as i64;
        assert_eq!(payout, -50);
    }

    #[test]
    fn payout_two_match_half_back() {
        let reels = [Symbol::Bar, Symbol::Bar, Symbol::Fire];
        let bet = 100u64;
        let mult = calculate_multiplier(&reels);
        let payout = (bet as f64 * mult) as i64 - bet as i64;
        assert_eq!(payout, -50); // get 50 back, net -50
    }

    #[test]
    fn reel_strip_generation() {
        let strip = generate_reel_strip(20);
        assert_eq!(strip.len(), 20);
        // All symbols should be valid
        for s in &strip {
            assert!(ALL_SYMBOLS.contains(s));
        }
    }
}
