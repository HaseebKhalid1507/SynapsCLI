use rand::Rng;

#[derive(Debug, Clone, PartialEq)]
pub enum SicBoPhase {
    Betting,
    Rolling,
    Result,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SicBoBet {
    Big,           // Total 11-17 (not triple), pays 1:1
    Small,         // Total 4-10 (not triple), pays 1:1
    Odd,           // Total is odd, pays 1:1
    Even,          // Total is even, pays 1:1
    Triple(u8),    // Specific triple (e.g., 1-1-1), pays 150:1
    AnyTriple,     // Any triple, pays 24:1
    Total(u8),     // Specific total (4-17), variable payout
}

impl SicBoBet {
    pub fn label(&self) -> String {
        match self {
            SicBoBet::Big => "BIG (11-17)".to_string(),
            SicBoBet::Small => "SMALL (4-10)".to_string(),
            SicBoBet::Odd => "ODD".to_string(),
            SicBoBet::Even => "EVEN".to_string(),
            SicBoBet::AnyTriple => "ANY TRIPLE".to_string(),
            SicBoBet::Triple(n) => format!("TRIPLE {}s", n),
            SicBoBet::Total(n) => format!("TOTAL {}", n),
        }
    }

    pub fn payout_ratio(&self) -> u64 {
        match self {
            SicBoBet::Big | SicBoBet::Small => 1,
            SicBoBet::Odd | SicBoBet::Even => 1,
            SicBoBet::AnyTriple => 24,
            SicBoBet::Triple(_) => 150,
            SicBoBet::Total(t) => match t {
                4 | 17 => 50,
                5 | 16 => 18,
                6 | 15 => 14,
                7 | 14 => 12,
                8 | 13 => 8,
                9 | 12 => 6,
                10 | 11 => 6,
                _ => 0,
            },
        }
    }

    pub fn wins(&self, dice: &[u8; 3]) -> bool {
        let total: u8 = dice.iter().sum();
        let is_triple = dice[0] == dice[1] && dice[1] == dice[2];

        match self {
            SicBoBet::Big => total >= 11 && total <= 17 && !is_triple,
            SicBoBet::Small => total >= 4 && total <= 10 && !is_triple,
            SicBoBet::Odd => total % 2 == 1,
            SicBoBet::Even => total % 2 == 0,
            SicBoBet::AnyTriple => is_triple,
            SicBoBet::Triple(n) => is_triple && dice[0] == *n,
            SicBoBet::Total(t) => total == *t,
        }
    }
}

pub const BET_OPTIONS: &[SicBoBet] = &[
    SicBoBet::Big, SicBoBet::Small,
    SicBoBet::Odd, SicBoBet::Even,
    SicBoBet::AnyTriple,
];

pub struct SicBoGame {
    pub phase: SicBoPhase,
    pub dice: [u8; 3],
    pub bets: Vec<(SicBoBet, u64)>,
    pub total_bet: u64,
    pub bet_input: String,
    pub cursor: usize,
    pub last_payout: i64,
    pub phase_timer: u64,
    pub rolling_display: [u8; 3],
}

impl SicBoGame {
    pub fn new() -> Self {
        Self {
            phase: SicBoPhase::Betting,
            dice: [1, 1, 1],
            bets: Vec::new(),
            total_bet: 0,
            bet_input: String::new(),
            cursor: 0,
            last_payout: 0,
            phase_timer: 0,
            rolling_display: [1, 1, 1],
        }
    }

    pub fn place_bet(&mut self, bet: SicBoBet, amount: u64) {
        self.total_bet += amount;
        self.bets.push((bet, amount));
    }

    pub fn roll(&mut self) {
        let mut rng = rand::rng();
        self.dice = [rng.random_range(1..=6), rng.random_range(1..=6), rng.random_range(1..=6)];
        self.phase = SicBoPhase::Rolling;
        self.phase_timer = 0;
    }

    pub fn tick_roll(&mut self) -> bool {
        self.phase_timer += 1;
        let mut rng = rand::rng();

        let speed = if self.phase_timer < 20 { 2 } else if self.phase_timer < 40 { 4 } else if self.phase_timer < 55 { 8 } else { 0 };

        if speed > 0 && self.phase_timer % speed == 0 {
            self.rolling_display = [rng.random_range(1..=6), rng.random_range(1..=6), rng.random_range(1..=6)];
        }

        if self.phase_timer >= 55 {
            self.rolling_display = self.dice;
            return true;
        }
        false
    }

    pub fn resolve(&mut self) {
        let mut winnings: i64 = 0;
        for (bet, amount) in &self.bets {
            if bet.wins(&self.dice) {
                winnings += *amount as i64 + (*amount * bet.payout_ratio()) as i64;
            }
        }
        self.last_payout = winnings - self.total_bet as i64;
        self.phase = SicBoPhase::Result;
    }

    pub fn new_round(&mut self) {
        self.phase = SicBoPhase::Betting;
        self.bets.clear();
        self.total_bet = 0;
        self.bet_input.clear();
        self.cursor = 0;
        self.last_payout = 0;
        self.phase_timer = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn big_wins_on_12() {
        assert!(SicBoBet::Big.wins(&[4, 4, 4]) == false); // triple excluded
        assert!(SicBoBet::Big.wins(&[4, 5, 3])); // 12, not triple
    }

    #[test]
    fn small_wins_on_7() {
        assert!(SicBoBet::Small.wins(&[2, 3, 2])); // 7
        assert!(!SicBoBet::Small.wins(&[5, 5, 5])); // triple excluded
    }

    #[test]
    fn any_triple() {
        assert!(SicBoBet::AnyTriple.wins(&[3, 3, 3]));
        assert!(!SicBoBet::AnyTriple.wins(&[3, 3, 2]));
    }

    #[test]
    fn specific_triple() {
        assert!(SicBoBet::Triple(6).wins(&[6, 6, 6]));
        assert!(!SicBoBet::Triple(6).wins(&[5, 5, 5]));
    }

    #[test]
    fn total_bet() {
        assert!(SicBoBet::Total(9).wins(&[3, 3, 3]));
        assert!(SicBoBet::Total(9).wins(&[2, 3, 4]));
        assert!(!SicBoBet::Total(9).wins(&[2, 3, 5]));
    }

    #[test]
    fn payout_triple_150_to_1() {
        assert_eq!(SicBoBet::Triple(1).payout_ratio(), 150);
    }
}
