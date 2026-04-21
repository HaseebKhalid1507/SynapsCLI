use rand::Rng;

// ── Roulette Numbers ────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouletteColor {
    Red,
    Black,
    Green,
}

/// European roulette: 0-36
pub fn number_color(n: u8) -> RouletteColor {
    if n == 0 { return RouletteColor::Green; }
    // Standard European roulette color distribution
    const RED_NUMBERS: [u8; 18] = [1, 3, 5, 7, 9, 12, 14, 16, 18, 19, 21, 23, 25, 27, 30, 32, 34, 36];
    if RED_NUMBERS.contains(&n) { RouletteColor::Red } else { RouletteColor::Black }
}

// ── Bet Types ───────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum BetType {
    Straight(u8),   // Single number (0-36), pays 35:1
    Red,            // All red numbers, pays 1:1
    Black,          // All black numbers, pays 1:1
    Odd,            // All odd (1-35), pays 1:1
    Even,           // All even (2-36), pays 1:1
    Low,            // 1-18, pays 1:1
    High,           // 19-36, pays 1:1
    Dozen(u8),      // 1st(1-12), 2nd(13-24), 3rd(25-36), pays 2:1
}

impl BetType {
    pub fn label(&self) -> String {
        match self {
            BetType::Straight(n) => format!("{}", n),
            BetType::Red => "RED".to_string(),
            BetType::Black => "BLACK".to_string(),
            BetType::Odd => "ODD".to_string(),
            BetType::Even => "EVEN".to_string(),
            BetType::Low => "1-18".to_string(),
            BetType::High => "19-36".to_string(),
            BetType::Dozen(d) => match d {
                1 => "1st 12".to_string(),
                2 => "2nd 12".to_string(),
                3 => "3rd 12".to_string(),
                _ => "???".to_string(),
            },
        }
    }

    pub fn payout_ratio(&self) -> u64 {
        match self {
            BetType::Straight(_) => 35,
            BetType::Red | BetType::Black => 1,
            BetType::Odd | BetType::Even => 1,
            BetType::Low | BetType::High => 1,
            BetType::Dozen(_) => 2,
        }
    }

    pub fn wins(&self, result: u8) -> bool {
        match self {
            BetType::Straight(n) => result == *n,
            BetType::Red => result > 0 && number_color(result) == RouletteColor::Red,
            BetType::Black => result > 0 && number_color(result) == RouletteColor::Black,
            BetType::Odd => result > 0 && result % 2 == 1,
            BetType::Even => result > 0 && result % 2 == 0,
            BetType::Low => result >= 1 && result <= 18,
            BetType::High => result >= 19 && result <= 36,
            BetType::Dozen(d) => match d {
                1 => result >= 1 && result <= 12,
                2 => result >= 13 && result <= 24,
                3 => result >= 25 && result <= 36,
                _ => false,
            },
        }
    }
}

#[derive(Debug, Clone)]
pub struct Bet {
    pub bet_type: BetType,
    pub amount: u64,
}

// ── Game State ──────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum RoulettePhase {
    Betting,
    Spinning,
    Result,
}

/// The available bet options for the UI cursor
pub const BET_OPTIONS: &[BetType] = &[
    BetType::Red, BetType::Black,
    BetType::Odd, BetType::Even,
    BetType::Low, BetType::High,
    BetType::Dozen(1), BetType::Dozen(2), BetType::Dozen(3),
];

pub struct RouletteGame {
    pub phase: RoulettePhase,
    pub bets: Vec<Bet>,
    pub result: Option<u8>,
    pub bet_input: String,
    /// Cursor position in the bet menu
    pub cursor: usize,
    /// Animation: spinning number display
    pub spin_display: u8,
    pub phase_timer: u64,
    pub last_payout: i64,
    pub total_bet: u64,
}

impl RouletteGame {
    pub fn new() -> Self {
        Self {
            phase: RoulettePhase::Betting,
            bets: Vec::new(),
            result: None,
            bet_input: String::new(),
            cursor: 0,
            spin_display: 0,
            phase_timer: 0,
            last_payout: 0,
            total_bet: 0,
        }
    }

    pub fn place_bet(&mut self, bet_type: BetType, amount: u64) {
        self.total_bet += amount;
        self.bets.push(Bet { bet_type, amount });
    }

    pub fn spin(&mut self) {
        let mut rng = rand::rng();
        self.result = Some(rng.random_range(0..=36));
        self.phase = RoulettePhase::Spinning;
        self.phase_timer = 0;
    }

    /// Tick the spin animation. Returns true when done.
    pub fn tick_spin(&mut self) -> bool {
        self.phase_timer += 1;

        // Spin animation: rapidly cycle numbers, then decelerate
        let speed = if self.phase_timer < 30 {
            2 // fast
        } else if self.phase_timer < 60 {
            4 // medium
        } else if self.phase_timer < 80 {
            8 // slow
        } else if self.phase_timer < 95 {
            12 // very slow
        } else {
            0 // stopped
        };

        if speed > 0 && self.phase_timer % speed == 0 {
            let mut rng = rand::rng();
            self.spin_display = rng.random_range(0..=36);
        }

        if self.phase_timer >= 95 {
            self.spin_display = self.result.unwrap_or(0);
            self.phase = RoulettePhase::Result;
            return true;
        }

        false
    }

    /// Calculate total payout across all bets.
    /// Returns net gain/loss. Positive = profit, negative = loss.
    pub fn calculate_payout(&self) -> i64 {
        let result = self.result.unwrap_or(0);
        let mut total_return: i64 = 0;

        for bet in &self.bets {
            if bet.bet_type.wins(result) {
                // Winner: get bet back + profit (bet * payout_ratio)
                total_return += bet.amount as i64 + (bet.amount * bet.bet_type.payout_ratio()) as i64;
            }
            // Losers: nothing returned
        }

        // Net = what you got back - what you put in
        total_return - self.total_bet as i64
    }

    pub fn new_round(&mut self) {
        self.phase = RoulettePhase::Betting;
        self.bets.clear();
        self.result = None;
        self.bet_input.clear();
        self.cursor = 0;
        self.phase_timer = 0;
        self.last_payout = 0;
        self.total_bet = 0;
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_is_green() {
        assert_eq!(number_color(0), RouletteColor::Green);
    }

    #[test]
    fn one_is_red() {
        assert_eq!(number_color(1), RouletteColor::Red);
    }

    #[test]
    fn two_is_black() {
        assert_eq!(number_color(2), RouletteColor::Black);
    }

    #[test]
    fn straight_bet_wins_on_exact() {
        assert!(BetType::Straight(17).wins(17));
        assert!(!BetType::Straight(17).wins(18));
        assert!(!BetType::Straight(17).wins(0));
    }

    #[test]
    fn straight_pays_35_to_1() {
        assert_eq!(BetType::Straight(7).payout_ratio(), 35);
    }

    #[test]
    fn red_bet_wins_on_red_numbers() {
        assert!(BetType::Red.wins(1));   // red
        assert!(!BetType::Red.wins(2));  // black
        assert!(!BetType::Red.wins(0));  // green
    }

    #[test]
    fn black_bet_wins_on_black_numbers() {
        assert!(BetType::Black.wins(2));  // black
        assert!(!BetType::Black.wins(1)); // red
        assert!(!BetType::Black.wins(0)); // green
    }

    #[test]
    fn odd_even_bets() {
        assert!(BetType::Odd.wins(1));
        assert!(!BetType::Odd.wins(2));
        assert!(!BetType::Odd.wins(0));

        assert!(BetType::Even.wins(2));
        assert!(!BetType::Even.wins(1));
        assert!(!BetType::Even.wins(0));
    }

    #[test]
    fn high_low_bets() {
        assert!(BetType::Low.wins(1));
        assert!(BetType::Low.wins(18));
        assert!(!BetType::Low.wins(19));
        assert!(!BetType::Low.wins(0));

        assert!(BetType::High.wins(19));
        assert!(BetType::High.wins(36));
        assert!(!BetType::High.wins(18));
    }

    #[test]
    fn dozen_bets() {
        assert!(BetType::Dozen(1).wins(1));
        assert!(BetType::Dozen(1).wins(12));
        assert!(!BetType::Dozen(1).wins(13));

        assert!(BetType::Dozen(2).wins(13));
        assert!(BetType::Dozen(2).wins(24));
        assert!(!BetType::Dozen(2).wins(25));

        assert!(BetType::Dozen(3).wins(25));
        assert!(BetType::Dozen(3).wins(36));
        assert!(!BetType::Dozen(3).wins(24));
    }

    #[test]
    fn payout_straight_win() {
        let mut game = RouletteGame::new();
        game.place_bet(BetType::Straight(17), 10);
        game.result = Some(17);
        // Win: get 10 back + 35*10 = 360 total return, minus 10 bet = 350 profit
        assert_eq!(game.calculate_payout(), 350);
    }

    #[test]
    fn payout_red_win() {
        let mut game = RouletteGame::new();
        game.place_bet(BetType::Red, 100);
        game.result = Some(1); // red
        // Win: get 100 back + 1*100 = 200 total, minus 100 = 100 profit
        assert_eq!(game.calculate_payout(), 100);
    }

    #[test]
    fn payout_total_loss() {
        let mut game = RouletteGame::new();
        game.place_bet(BetType::Red, 50);
        game.result = Some(2); // black
        // Lose entire bet
        assert_eq!(game.calculate_payout(), -50);
    }

    #[test]
    fn multiple_bets_partial_win() {
        let mut game = RouletteGame::new();
        game.place_bet(BetType::Red, 50);
        game.place_bet(BetType::Odd, 50);
        game.result = Some(1); // red AND odd
        // Both win: (50+50) + (50+50) = 200 return, minus 100 bet = 100 profit
        assert_eq!(game.calculate_payout(), 100);
    }

    #[test]
    fn multiple_bets_one_wins() {
        let mut game = RouletteGame::new();
        game.place_bet(BetType::Red, 50);   // wins
        game.place_bet(BetType::Even, 50);  // loses (1 is odd)
        game.result = Some(1); // red, odd
        // Red wins: 50+50 = 100 return, Even loses: 0
        // Total return 100, total bet 100, net = 0
        assert_eq!(game.calculate_payout(), 0);
    }

    #[test]
    fn zero_loses_all_outside_bets() {
        let mut game = RouletteGame::new();
        game.place_bet(BetType::Red, 50);
        game.place_bet(BetType::Black, 50);
        game.place_bet(BetType::Odd, 50);
        game.result = Some(0); // green — all lose
        assert_eq!(game.calculate_payout(), -150);
    }
}
