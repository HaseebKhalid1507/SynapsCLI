use crate::games::blackjack::{Card, Rank, Suit, Deck};

#[derive(Debug, Clone, PartialEq)]
pub enum VideoPokerPhase {
    Betting,
    Dealing,
    Hold,    // Player selects which cards to hold
    Drawing, // Replace non-held cards
    Result,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PokerHand {
    RoyalFlush,
    StraightFlush,
    FourOfAKind,
    FullHouse,
    Flush,
    Straight,
    ThreeOfAKind,
    TwoPair,
    JacksOrBetter,
    Nothing,
}

impl PokerHand {
    pub fn label(&self) -> &'static str {
        match self {
            PokerHand::RoyalFlush => "ROYAL FLUSH",
            PokerHand::StraightFlush => "STRAIGHT FLUSH",
            PokerHand::FourOfAKind => "FOUR OF A KIND",
            PokerHand::FullHouse => "FULL HOUSE",
            PokerHand::Flush => "FLUSH",
            PokerHand::Straight => "STRAIGHT",
            PokerHand::ThreeOfAKind => "THREE OF A KIND",
            PokerHand::TwoPair => "TWO PAIR",
            PokerHand::JacksOrBetter => "JACKS OR BETTER",
            PokerHand::Nothing => "NOTHING",
        }
    }

    pub fn multiplier(&self) -> u64 {
        match self {
            PokerHand::RoyalFlush => 250,
            PokerHand::StraightFlush => 50,
            PokerHand::FourOfAKind => 25,
            PokerHand::FullHouse => 9,
            PokerHand::Flush => 6,
            PokerHand::Straight => 4,
            PokerHand::ThreeOfAKind => 3,
            PokerHand::TwoPair => 2,
            PokerHand::JacksOrBetter => 1,
            PokerHand::Nothing => 0,
        }
    }
}

fn rank_num(r: &Rank) -> u8 {
    match r {
        Rank::Ace => 14,
        Rank::King => 13,
        Rank::Queen => 12,
        Rank::Jack => 11,
        Rank::Ten => 10,
        Rank::Nine => 9,
        Rank::Eight => 8,
        Rank::Seven => 7,
        Rank::Six => 6,
        Rank::Five => 5,
        Rank::Four => 4,
        Rank::Three => 3,
        Rank::Two => 2,
    }
}

pub fn evaluate_hand(cards: &[Card; 5]) -> PokerHand {
    let mut ranks: Vec<u8> = cards.iter().map(|c| rank_num(&c.rank)).collect();
    ranks.sort();

    let is_flush = cards.iter().all(|c| c.suit == cards[0].suit);
    let is_straight = {
        let mut s = true;
        for i in 1..5 {
            if ranks[i] != ranks[i - 1] + 1 { s = false; break; }
        }
        // Ace-low straight: A,2,3,4,5
        if !s && ranks == [2, 3, 4, 5, 14] { s = true; }
        s
    };

    // Count occurrences
    let mut counts = [0u8; 15];
    for &r in &ranks { counts[r as usize] += 1; }
    let mut groups: Vec<u8> = counts.iter().filter(|&&c| c > 0).cloned().collect();
    groups.sort_unstable_by(|a, b| b.cmp(a));

    if is_flush && is_straight {
        if ranks[0] == 10 { return PokerHand::RoyalFlush; }
        return PokerHand::StraightFlush;
    }
    if groups[0] == 4 { return PokerHand::FourOfAKind; }
    if groups[0] == 3 && groups[1] == 2 { return PokerHand::FullHouse; }
    if is_flush { return PokerHand::Flush; }
    if is_straight { return PokerHand::Straight; }
    if groups[0] == 3 { return PokerHand::ThreeOfAKind; }
    if groups[0] == 2 && groups[1] == 2 { return PokerHand::TwoPair; }
    if groups[0] == 2 {
        // Check if pair is jacks or better
        for r in [11, 12, 13, 14] {
            if counts[r] == 2 { return PokerHand::JacksOrBetter; }
        }
    }
    PokerHand::Nothing
}

pub struct VideoPokerGame {
    pub phase: VideoPokerPhase,
    pub deck: Deck,
    pub hand: [Card; 5],
    pub held: [bool; 5],
    pub bet: u64,
    pub bet_input: String,
    pub result: Option<PokerHand>,
    pub last_payout: i64,
    pub phase_timer: u64,
    pub cursor: usize,
}

impl VideoPokerGame {
    pub fn new() -> Self {
        let blank = Card::new(Rank::Two, Suit::Spades);
        Self {
            phase: VideoPokerPhase::Betting,
            deck: Deck::new_shuffled(),
            hand: [blank; 5],
            held: [false; 5],
            bet: 0,
            bet_input: String::new(),
            result: None,
            last_payout: 0,
            phase_timer: 0,
            cursor: 0,
        }
    }

    pub fn deal(&mut self, bet: u64) {
        self.deck = Deck::new_shuffled();
        self.bet = bet;
        self.held = [false; 5];
        self.cursor = 0;
        for i in 0..5 {
            self.hand[i] = self.deck.draw();
        }
        self.phase = VideoPokerPhase::Hold;
        self.phase_timer = 0;
    }

    pub fn toggle_hold(&mut self, idx: usize) {
        if idx < 5 { self.held[idx] = !self.held[idx]; }
    }

    pub fn draw_cards(&mut self) {
        for i in 0..5 {
            if !self.held[i] {
                self.hand[i] = self.deck.draw();
            }
        }
        let hand_type = evaluate_hand(&self.hand);
        let mult = hand_type.multiplier();
        self.last_payout = if mult > 0 {
            (self.bet * mult) as i64
        } else {
            -(self.bet as i64)
        };
        self.result = Some(hand_type);
        self.phase = VideoPokerPhase::Result;
    }

    pub fn new_hand(&mut self) {
        self.phase = VideoPokerPhase::Betting;
        self.held = [false; 5];
        self.bet = 0;
        self.bet_input.clear();
        self.result = None;
        self.last_payout = 0;
        self.phase_timer = 0;
        self.cursor = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn royal_flush() {
        let hand = [
            Card::new(Rank::Ten, Suit::Spades), Card::new(Rank::Jack, Suit::Spades),
            Card::new(Rank::Queen, Suit::Spades), Card::new(Rank::King, Suit::Spades),
            Card::new(Rank::Ace, Suit::Spades),
        ];
        assert_eq!(evaluate_hand(&hand), PokerHand::RoyalFlush);
    }

    #[test]
    fn straight_flush() {
        let hand = [
            Card::new(Rank::Five, Suit::Hearts), Card::new(Rank::Six, Suit::Hearts),
            Card::new(Rank::Seven, Suit::Hearts), Card::new(Rank::Eight, Suit::Hearts),
            Card::new(Rank::Nine, Suit::Hearts),
        ];
        assert_eq!(evaluate_hand(&hand), PokerHand::StraightFlush);
    }

    #[test]
    fn four_of_a_kind() {
        let hand = [
            Card::new(Rank::Seven, Suit::Spades), Card::new(Rank::Seven, Suit::Hearts),
            Card::new(Rank::Seven, Suit::Diamonds), Card::new(Rank::Seven, Suit::Clubs),
            Card::new(Rank::Ace, Suit::Spades),
        ];
        assert_eq!(evaluate_hand(&hand), PokerHand::FourOfAKind);
    }

    #[test]
    fn full_house() {
        let hand = [
            Card::new(Rank::King, Suit::Spades), Card::new(Rank::King, Suit::Hearts),
            Card::new(Rank::King, Suit::Diamonds), Card::new(Rank::Three, Suit::Clubs),
            Card::new(Rank::Three, Suit::Spades),
        ];
        assert_eq!(evaluate_hand(&hand), PokerHand::FullHouse);
    }

    #[test]
    fn flush() {
        let hand = [
            Card::new(Rank::Two, Suit::Clubs), Card::new(Rank::Five, Suit::Clubs),
            Card::new(Rank::Eight, Suit::Clubs), Card::new(Rank::Jack, Suit::Clubs),
            Card::new(Rank::Ace, Suit::Clubs),
        ];
        assert_eq!(evaluate_hand(&hand), PokerHand::Flush);
    }

    #[test]
    fn jacks_or_better() {
        let hand = [
            Card::new(Rank::Jack, Suit::Spades), Card::new(Rank::Jack, Suit::Hearts),
            Card::new(Rank::Three, Suit::Diamonds), Card::new(Rank::Seven, Suit::Clubs),
            Card::new(Rank::Nine, Suit::Spades),
        ];
        assert_eq!(evaluate_hand(&hand), PokerHand::JacksOrBetter);
    }

    #[test]
    fn pair_of_tens_is_nothing() {
        let hand = [
            Card::new(Rank::Ten, Suit::Spades), Card::new(Rank::Ten, Suit::Hearts),
            Card::new(Rank::Three, Suit::Diamonds), Card::new(Rank::Seven, Suit::Clubs),
            Card::new(Rank::Nine, Suit::Spades),
        ];
        assert_eq!(evaluate_hand(&hand), PokerHand::Nothing);
    }

    #[test]
    fn royal_flush_pays_250x() {
        assert_eq!(PokerHand::RoyalFlush.multiplier(), 250);
    }
}
