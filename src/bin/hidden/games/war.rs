use crate::games::blackjack::{Card, Rank, Deck};

#[derive(Debug, Clone, PartialEq)]
pub enum WarPhase {
    Betting,
    Reveal,      // Show both cards
    War,         // Tie — go to war
    WarReveal,   // War cards revealed
    Result,
}

#[derive(Debug, Clone, PartialEq)]
pub enum WarOutcome {
    PlayerWin,
    DealerWin,
    TieGoToWar,
    PlayerWinsWar,
    DealerWinsWar,
}

pub struct WarGame {
    pub phase: WarPhase,
    pub deck: Deck,
    pub player_card: Option<Card>,
    pub dealer_card: Option<Card>,
    pub war_player: Option<Card>,
    pub war_dealer: Option<Card>,
    pub bet: u64,
    pub bet_input: String,
    pub outcome: Option<WarOutcome>,
    pub last_payout: i64,
    pub phase_timer: u64,
}

fn card_value(card: &Card) -> u8 {
    match card.rank {
        Rank::Ace => 14,
        Rank::King => 13,
        Rank::Queen => 12,
        Rank::Jack => 11,
        _ => card.rank.value(),
    }
}

impl WarGame {
    pub fn new() -> Self {
        Self {
            phase: WarPhase::Betting,
            deck: Deck::new_shuffled(),
            player_card: None,
            dealer_card: None,
            war_player: None,
            war_dealer: None,
            bet: 0,
            bet_input: String::new(),
            outcome: None,
            last_payout: 0,
            phase_timer: 0,
        }
    }

    pub fn deal(&mut self, bet: u64) {
        self.bet = bet;
        self.player_card = Some(self.deck.draw());
        self.dealer_card = Some(self.deck.draw());
        self.phase = WarPhase::Reveal;
        self.phase_timer = 0;
    }

    pub fn resolve_reveal(&mut self) {
        let pv = card_value(self.player_card.as_ref().unwrap());
        let dv = card_value(self.dealer_card.as_ref().unwrap());

        if pv > dv {
            self.outcome = Some(WarOutcome::PlayerWin);
            self.last_payout = self.bet as i64;
            self.phase = WarPhase::Result;
        } else if dv > pv {
            self.outcome = Some(WarOutcome::DealerWin);
            self.last_payout = -(self.bet as i64);
            self.phase = WarPhase::Result;
        } else {
            self.outcome = Some(WarOutcome::TieGoToWar);
            self.phase = WarPhase::War;
            self.phase_timer = 0;
        }
    }

    pub fn go_to_war(&mut self) {
        // Burn 3 cards, deal 1 each
        for _ in 0..3 { self.deck.draw(); }
        self.war_player = Some(self.deck.draw());
        for _ in 0..3 { self.deck.draw(); }
        self.war_dealer = Some(self.deck.draw());
        self.phase = WarPhase::WarReveal;
        self.phase_timer = 0;
    }

    pub fn resolve_war(&mut self) {
        let pv = card_value(self.war_player.as_ref().unwrap());
        let dv = card_value(self.war_dealer.as_ref().unwrap());

        if pv >= dv {
            // Player wins war — pays 1:1 on original bet
            self.outcome = Some(WarOutcome::PlayerWinsWar);
            self.last_payout = self.bet as i64;
        } else {
            self.outcome = Some(WarOutcome::DealerWinsWar);
            self.last_payout = -(self.bet as i64);
        }
        self.phase = WarPhase::Result;
    }

    pub fn new_hand(&mut self) {
        self.phase = WarPhase::Betting;
        self.player_card = None;
        self.dealer_card = None;
        self.war_player = None;
        self.war_dealer = None;
        self.bet = 0;
        self.bet_input.clear();
        self.outcome = None;
        self.last_payout = 0;
        self.phase_timer = 0;
    }
}
