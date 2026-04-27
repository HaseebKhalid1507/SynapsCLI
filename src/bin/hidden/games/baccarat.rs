use crate::games::blackjack::{Card, Rank, Deck};

#[derive(Debug, Clone, PartialEq)]
pub enum BaccaratPhase {
    Betting,
    Dealing,
    Result,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BaccaratBet {
    Player,
    Banker,
    Tie,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BaccaratOutcome {
    PlayerWins,
    BankerWins,
    Tie,
}

fn card_points(card: &Card) -> u8 {
    match card.rank {
        Rank::Ace => 1,
        Rank::Two => 2,
        Rank::Three => 3,
        Rank::Four => 4,
        Rank::Five => 5,
        Rank::Six => 6,
        Rank::Seven => 7,
        Rank::Eight => 8,
        Rank::Nine => 9,
        _ => 0, // 10, J, Q, K = 0
    }
}

pub fn hand_total(cards: &[Card]) -> u8 {
    cards.iter().map(|c| card_points(c)).sum::<u8>() % 10
}

pub struct BaccaratGame {
    pub phase: BaccaratPhase,
    pub deck: Deck,
    pub player_hand: Vec<Card>,
    pub banker_hand: Vec<Card>,
    pub bet_type: BaccaratBet,
    pub bet: u64,
    pub bet_input: String,
    pub cursor: usize, // 0=player, 1=banker, 2=tie
    pub outcome: Option<BaccaratOutcome>,
    pub last_payout: i64,
    pub phase_timer: u64,
}

impl BaccaratGame {
    pub fn new() -> Self {
        Self {
            phase: BaccaratPhase::Betting,
            deck: Deck::new_shuffled(),
            player_hand: Vec::new(),
            banker_hand: Vec::new(),
            bet_type: BaccaratBet::Player,
            bet: 0,
            bet_input: String::new(),
            cursor: 0,
            outcome: None,
            last_payout: 0,
            phase_timer: 0,
        }
    }

    pub fn deal(&mut self, bet: u64, bet_type: BaccaratBet) {
        self.bet = bet;
        self.bet_type = bet_type;
        self.player_hand.clear();
        self.banker_hand.clear();

        // Initial 2 cards each
        self.player_hand.push(self.deck.draw());
        self.banker_hand.push(self.deck.draw());
        self.player_hand.push(self.deck.draw());
        self.banker_hand.push(self.deck.draw());

        // Third card rules (simplified)
        let player_total = hand_total(&self.player_hand);
        let banker_total = hand_total(&self.banker_hand);

        // Natural — no more cards
        if player_total >= 8 || banker_total >= 8 {
            // done
        } else {
            // Player draws on 0-5
            let player_drew = if player_total <= 5 {
                let card = self.deck.draw();
                self.player_hand.push(card);
                Some(card_points(&card))
            } else {
                None
            };

            // Banker rules depend on player's third card
            let banker_draws = match player_drew {
                None => banker_total <= 5,
                Some(p3) => match banker_total {
                    0..=2 => true,
                    3 => p3 != 8,
                    4 => p3 >= 2 && p3 <= 7,
                    5 => p3 >= 4 && p3 <= 7,
                    6 => p3 == 6 || p3 == 7,
                    _ => false,
                },
            };

            if banker_draws {
                self.banker_hand.push(self.deck.draw());
            }
        }

        self.phase = BaccaratPhase::Dealing;
        self.phase_timer = 0;
    }

    pub fn resolve(&mut self) {
        let pt = hand_total(&self.player_hand);
        let bt = hand_total(&self.banker_hand);

        self.outcome = Some(if pt > bt {
            BaccaratOutcome::PlayerWins
        } else if bt > pt {
            BaccaratOutcome::BankerWins
        } else {
            BaccaratOutcome::Tie
        });

        self.last_payout = match (&self.bet_type, self.outcome.as_ref().unwrap()) {
            (BaccaratBet::Player, BaccaratOutcome::PlayerWins) => self.bet as i64,
            (BaccaratBet::Banker, BaccaratOutcome::BankerWins) => {
                // Banker pays 0.95:1 (5% commission)
                ((self.bet as f64 * 0.95) as i64).max(1)
            }
            (BaccaratBet::Tie, BaccaratOutcome::Tie) => (self.bet * 8) as i64, // 8:1
            // Tie returns bet for player/banker bets
            (BaccaratBet::Player, BaccaratOutcome::Tie) |
            (BaccaratBet::Banker, BaccaratOutcome::Tie) => 0,
            _ => -(self.bet as i64),
        };

        self.phase = BaccaratPhase::Result;
    }

    pub fn player_total(&self) -> u8 { hand_total(&self.player_hand) }
    pub fn banker_total(&self) -> u8 { hand_total(&self.banker_hand) }

    pub fn new_hand(&mut self) {
        self.phase = BaccaratPhase::Betting;
        self.player_hand.clear();
        self.banker_hand.clear();
        self.bet = 0;
        self.bet_input.clear();
        self.outcome = None;
        self.last_payout = 0;
        self.phase_timer = 0;
    }
}
