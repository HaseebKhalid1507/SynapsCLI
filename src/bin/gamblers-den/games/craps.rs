use rand::Rng;

#[derive(Debug, Clone, PartialEq)]
pub enum CrapsPhase {
    Betting,
    ComeOut,     // Come-out roll
    Point,       // Point established, keep rolling
    Result,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CrapsBet {
    Pass,         // Win on 7/11 come-out, lose on 2/3/12, establish point
    DontPass,     // Opposite of pass
    Field,        // One-roll: win on 2,3,4,9,10,11,12. Double on 2/12.
}

impl CrapsBet {
    pub fn label(&self) -> &'static str {
        match self {
            CrapsBet::Pass => "PASS LINE",
            CrapsBet::DontPass => "DON'T PASS",
            CrapsBet::Field => "FIELD",
        }
    }
}

pub struct CrapsGame {
    pub phase: CrapsPhase,
    pub dice: [u8; 2],
    pub point: Option<u8>,
    pub bet_type: CrapsBet,
    pub bet: u64,
    pub bet_input: String,
    pub cursor: usize,
    pub last_payout: i64,
    pub phase_timer: u64,
    pub rolling_display: [u8; 2],
    pub roll_history: Vec<u8>,
}

impl CrapsGame {
    pub fn new() -> Self {
        Self {
            phase: CrapsPhase::Betting,
            dice: [1, 1],
            point: None,
            bet_type: CrapsBet::Pass,
            bet: 0,
            bet_input: String::new(),
            cursor: 0,
            last_payout: 0,
            phase_timer: 0,
            rolling_display: [1, 1],
            roll_history: Vec::new(),
        }
    }

    pub fn roll_dice(&mut self) {
        let mut rng = rand::rng();
        self.dice = [rng.random_range(1..=6), rng.random_range(1..=6)];
        self.phase_timer = 0;
    }

    pub fn tick_roll(&mut self) -> bool {
        self.phase_timer += 1;
        let mut rng = rand::rng();

        let speed = if self.phase_timer < 15 { 2 } else if self.phase_timer < 30 { 4 } else if self.phase_timer < 40 { 8 } else { 0 };
        if speed > 0 && self.phase_timer % speed == 0 {
            self.rolling_display = [rng.random_range(1..=6), rng.random_range(1..=6)];
        }

        if self.phase_timer >= 40 {
            self.rolling_display = self.dice;
            return true;
        }
        false
    }

    pub fn total(&self) -> u8 { self.dice[0] + self.dice[1] }

    pub fn start_round(&mut self, bet: u64, bet_type: CrapsBet) {
        self.bet = bet;
        self.bet_type = bet_type;
        self.point = None;
        self.roll_history.clear();
        self.roll_dice();
        self.phase = CrapsPhase::ComeOut;
    }

    /// Resolve after roll animation. Returns Some(payout) if round is over.
    pub fn resolve_roll(&mut self) -> Option<i64> {
        let total = self.total();
        self.roll_history.push(total);

        match self.bet_type {
            CrapsBet::Field => {
                // One-roll bet
                let payout = match total {
                    2 | 12 => (self.bet * 2) as i64,  // double
                    3 | 4 | 9 | 10 | 11 => self.bet as i64,
                    _ => -(self.bet as i64),
                };
                self.last_payout = payout;
                self.phase = CrapsPhase::Result;
                return Some(payout);
            }
            CrapsBet::Pass => {
                if self.point.is_none() {
                    // Come-out roll
                    match total {
                        7 | 11 => {
                            self.last_payout = self.bet as i64;
                            self.phase = CrapsPhase::Result;
                            return Some(self.bet as i64);
                        }
                        2 | 3 | 12 => {
                            self.last_payout = -(self.bet as i64);
                            self.phase = CrapsPhase::Result;
                            return Some(-(self.bet as i64));
                        }
                        _ => {
                            self.point = Some(total);
                            self.phase = CrapsPhase::Point;
                            return None;
                        }
                    }
                } else {
                    let pt = self.point.unwrap();
                    if total == pt {
                        self.last_payout = self.bet as i64;
                        self.phase = CrapsPhase::Result;
                        return Some(self.bet as i64);
                    } else if total == 7 {
                        self.last_payout = -(self.bet as i64);
                        self.phase = CrapsPhase::Result;
                        return Some(-(self.bet as i64));
                    }
                    return None; // keep rolling
                }
            }
            CrapsBet::DontPass => {
                if self.point.is_none() {
                    match total {
                        2 | 3 => {
                            self.last_payout = self.bet as i64;
                            self.phase = CrapsPhase::Result;
                            return Some(self.bet as i64);
                        }
                        12 => {
                            self.last_payout = 0; // push
                            self.phase = CrapsPhase::Result;
                            return Some(0);
                        }
                        7 | 11 => {
                            self.last_payout = -(self.bet as i64);
                            self.phase = CrapsPhase::Result;
                            return Some(-(self.bet as i64));
                        }
                        _ => {
                            self.point = Some(total);
                            self.phase = CrapsPhase::Point;
                            return None;
                        }
                    }
                } else {
                    let pt = self.point.unwrap();
                    if total == 7 {
                        self.last_payout = self.bet as i64;
                        self.phase = CrapsPhase::Result;
                        return Some(self.bet as i64);
                    } else if total == pt {
                        self.last_payout = -(self.bet as i64);
                        self.phase = CrapsPhase::Result;
                        return Some(-(self.bet as i64));
                    }
                    return None;
                }
            }
        }
    }

    pub fn new_round(&mut self) {
        self.phase = CrapsPhase::Betting;
        self.point = None;
        self.bet = 0;
        self.bet_input.clear();
        self.cursor = 0;
        self.last_payout = 0;
        self.phase_timer = 0;
        self.roll_history.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pass_wins_on_7_come_out() {
        let mut game = CrapsGame::new();
        game.bet = 100;
        game.bet_type = CrapsBet::Pass;
        game.dice = [3, 4]; // 7
        let result = game.resolve_roll();
        assert_eq!(result, Some(100));
    }

    #[test]
    fn pass_loses_on_2_come_out() {
        let mut game = CrapsGame::new();
        game.bet = 100;
        game.bet_type = CrapsBet::Pass;
        game.dice = [1, 1]; // 2 (craps)
        let result = game.resolve_roll();
        assert_eq!(result, Some(-100));
    }

    #[test]
    fn pass_establishes_point() {
        let mut game = CrapsGame::new();
        game.bet = 100;
        game.bet_type = CrapsBet::Pass;
        game.dice = [4, 4]; // 8 — point
        let result = game.resolve_roll();
        assert_eq!(result, None);
        assert_eq!(game.point, Some(8));
    }

    #[test]
    fn pass_wins_on_point() {
        let mut game = CrapsGame::new();
        game.bet = 100;
        game.bet_type = CrapsBet::Pass;
        game.point = Some(8);
        game.phase = CrapsPhase::Point;
        game.dice = [3, 5]; // 8 = point
        let result = game.resolve_roll();
        assert_eq!(result, Some(100));
    }

    #[test]
    fn pass_loses_on_7_after_point() {
        let mut game = CrapsGame::new();
        game.bet = 100;
        game.bet_type = CrapsBet::Pass;
        game.point = Some(8);
        game.phase = CrapsPhase::Point;
        game.dice = [3, 4]; // 7
        let result = game.resolve_roll();
        assert_eq!(result, Some(-100));
    }

    #[test]
    fn field_wins_on_4() {
        let mut game = CrapsGame::new();
        game.bet = 100;
        game.bet_type = CrapsBet::Field;
        game.dice = [2, 2]; // 4
        let result = game.resolve_roll();
        assert_eq!(result, Some(100));
    }

    #[test]
    fn field_double_on_2() {
        let mut game = CrapsGame::new();
        game.bet = 100;
        game.bet_type = CrapsBet::Field;
        game.dice = [1, 1]; // 2
        let result = game.resolve_roll();
        assert_eq!(result, Some(200));
    }

    #[test]
    fn dont_pass_push_on_12() {
        let mut game = CrapsGame::new();
        game.bet = 100;
        game.bet_type = CrapsBet::DontPass;
        game.dice = [6, 6]; // 12
        let result = game.resolve_roll();
        assert_eq!(result, Some(0));
    }
}
