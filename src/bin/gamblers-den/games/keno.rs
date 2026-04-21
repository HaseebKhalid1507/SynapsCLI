use rand::Rng;

#[derive(Debug, Clone, PartialEq)]
pub enum KenoPhase {
    Picking,   // Player selects numbers
    Drawing,   // Numbers being drawn
    Result,
}

pub struct KenoGame {
    pub phase: KenoPhase,
    pub picks: Vec<u8>,        // Player's selected numbers (1-80)
    pub drawn: Vec<u8>,        // Drawn numbers (up to 20)
    pub bet: u64,
    pub bet_input: String,
    pub cursor: u8,            // Grid cursor (1-80)
    pub last_payout: i64,
    pub phase_timer: u64,
    pub hits: usize,
}

const MAX_PICKS: usize = 10;
const TOTAL_DRAW: usize = 20;

/// Keno payout table: [picks][hits] = multiplier
fn payout_multiplier(picks: usize, hits: usize) -> u64 {
    match (picks, hits) {
        (1, 1) => 3,
        (2, 2) => 9,
        (3, 2) => 2, (3, 3) => 25,
        (4, 2) => 1, (4, 3) => 5, (4, 4) => 75,
        (5, 3) => 2, (5, 4) => 20, (5, 5) => 300,
        (6, 3) => 1, (6, 4) => 8, (6, 5) => 60, (6, 6) => 1500,
        (7, 3) => 1, (7, 4) => 4, (7, 5) => 20, (7, 6) => 100, (7, 7) => 5000,
        (8, 4) => 2, (8, 5) => 10, (8, 6) => 50, (8, 7) => 500, (8, 8) => 10000,
        (9, 4) => 1, (9, 5) => 5, (9, 6) => 25, (9, 7) => 200, (9, 8) => 3000, (9, 9) => 25000,
        (10, 5) => 3, (10, 6) => 15, (10, 7) => 100, (10, 8) => 1000, (10, 9) => 5000, (10, 10) => 100000,
        _ => 0,
    }
}

impl KenoGame {
    pub fn new() -> Self {
        Self {
            phase: KenoPhase::Picking,
            picks: Vec::new(),
            drawn: Vec::new(),
            bet: 0,
            bet_input: String::new(),
            cursor: 1,
            last_payout: 0,
            phase_timer: 0,
            hits: 0,
        }
    }

    pub fn toggle_pick(&mut self, num: u8) {
        if num < 1 || num > 80 { return; }
        if let Some(idx) = self.picks.iter().position(|&n| n == num) {
            self.picks.remove(idx);
        } else if self.picks.len() < MAX_PICKS {
            self.picks.push(num);
        }
    }

    pub fn start_draw(&mut self, bet: u64) {
        if self.picks.is_empty() { return; }
        self.bet = bet;
        self.drawn.clear();
        self.phase = KenoPhase::Drawing;
        self.phase_timer = 0;
    }

    /// Draw one number. Returns true when all 20 drawn.
    pub fn draw_one(&mut self) -> bool {
        let mut rng = rand::rng();
        loop {
            let n = rng.random_range(1..=80);
            if !self.drawn.contains(&n) {
                self.drawn.push(n);
                break;
            }
        }
        self.drawn.len() >= TOTAL_DRAW
    }

    pub fn resolve(&mut self) {
        self.hits = self.picks.iter().filter(|p| self.drawn.contains(p)).count();
        let mult = payout_multiplier(self.picks.len(), self.hits);
        self.last_payout = if mult > 0 {
            (self.bet * mult) as i64
        } else {
            -(self.bet as i64)
        };
        self.phase = KenoPhase::Result;
    }

    pub fn is_hit(&self, num: u8) -> bool {
        self.picks.contains(&num) && self.drawn.contains(&num)
    }

    pub fn is_miss(&self, num: u8) -> bool {
        self.picks.contains(&num) && !self.drawn.contains(&num) && self.phase == KenoPhase::Result
    }

    pub fn new_round(&mut self) {
        self.phase = KenoPhase::Picking;
        self.picks.clear();
        self.drawn.clear();
        self.bet = 0;
        self.bet_input.clear();
        self.cursor = 1;
        self.last_payout = 0;
        self.phase_timer = 0;
        self.hits = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_pick_one_hit_pays_3x() {
        assert_eq!(payout_multiplier(1, 1), 3);
    }

    #[test]
    fn ten_picks_ten_hits_pays_100000x() {
        assert_eq!(payout_multiplier(10, 10), 100000);
    }

    #[test]
    fn no_hits_pays_nothing() {
        assert_eq!(payout_multiplier(5, 0), 0);
        assert_eq!(payout_multiplier(5, 1), 0);
        assert_eq!(payout_multiplier(5, 2), 0);
    }

    #[test]
    fn toggle_pick() {
        let mut game = KenoGame::new();
        game.toggle_pick(42);
        assert!(game.picks.contains(&42));
        game.toggle_pick(42);
        assert!(!game.picks.contains(&42));
    }

    #[test]
    fn max_10_picks() {
        let mut game = KenoGame::new();
        for i in 1..=11 {
            game.toggle_pick(i);
        }
        assert_eq!(game.picks.len(), 10);
    }

    #[test]
    fn draw_20_unique() {
        let mut game = KenoGame::new();
        game.picks.push(1);
        game.bet = 10;
        for _ in 0..20 {
            game.draw_one();
        }
        assert_eq!(game.drawn.len(), 20);
        // All unique
        let mut sorted = game.drawn.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 20);
    }
}
