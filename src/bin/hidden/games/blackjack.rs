use rand::seq::SliceRandom;


// ── Card Types ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Suit {
    Spades,
    Hearts,
    Diamonds,
    Clubs,
}

impl Suit {
    pub fn symbol(&self) -> &'static str {
        match self {
            Suit::Spades => "♠",
            Suit::Hearts => "♥",
            Suit::Diamonds => "♦",
            Suit::Clubs => "♣",
        }
    }

    pub fn is_red(&self) -> bool {
        matches!(self, Suit::Hearts | Suit::Diamonds)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Rank {
    Ace, Two, Three, Four, Five, Six, Seven,
    Eight, Nine, Ten, Jack, Queen, King,
}

impl Rank {
    pub fn label(&self) -> &'static str {
        match self {
            Rank::Ace => "A",
            Rank::Two => "2",
            Rank::Three => "3",
            Rank::Four => "4",
            Rank::Five => "5",
            Rank::Six => "6",
            Rank::Seven => "7",
            Rank::Eight => "8",
            Rank::Nine => "9",
            Rank::Ten => "10",
            Rank::Jack => "J",
            Rank::Queen => "Q",
            Rank::King => "K",
        }
    }

    /// Base value (ace = 11, face = 10)
    pub fn value(&self) -> u8 {
        match self {
            Rank::Ace => 11,
            Rank::Two => 2,
            Rank::Three => 3,
            Rank::Four => 4,
            Rank::Five => 5,
            Rank::Six => 6,
            Rank::Seven => 7,
            Rank::Eight => 8,
            Rank::Nine => 9,
            Rank::Ten | Rank::Jack | Rank::Queen | Rank::King => 10,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Card {
    pub rank: Rank,
    pub suit: Suit,
}

impl Card {
    pub fn new(rank: Rank, suit: Suit) -> Self {
        Self { rank, suit }
    }
}

// ── Deck ────────────────────────────────────────────────────────────

const ALL_SUITS: [Suit; 4] = [Suit::Spades, Suit::Hearts, Suit::Diamonds, Suit::Clubs];
const ALL_RANKS: [Rank; 13] = [
    Rank::Ace, Rank::Two, Rank::Three, Rank::Four, Rank::Five,
    Rank::Six, Rank::Seven, Rank::Eight, Rank::Nine, Rank::Ten,
    Rank::Jack, Rank::Queen, Rank::King,
];

pub struct Deck {
    cards: Vec<Card>,
}

impl Deck {
    pub fn new_shuffled() -> Self {
        let mut cards = Vec::with_capacity(52);
        for &suit in &ALL_SUITS {
            for &rank in &ALL_RANKS {
                cards.push(Card::new(rank, suit));
            }
        }
        let mut rng = rand::rng();
        cards.shuffle(&mut rng);
        Self { cards }
    }

    pub fn draw(&mut self) -> Card {
        if self.cards.is_empty() {
            // Reshuffle infinite deck
            *self = Deck::new_shuffled();
        }
        self.cards.pop().unwrap()
    }
}

// ── Hand Scoring ────────────────────────────────────────────────────

/// Calculate best hand value, auto-optimizing aces.
pub fn hand_value(cards: &[Card]) -> u8 {
    let mut total: u16 = 0;
    let mut aces: u8 = 0;

    for card in cards {
        total += card.rank.value() as u16;
        if card.rank == Rank::Ace {
            aces += 1;
        }
    }

    // Downgrade aces from 11 to 1 while bust
    while total > 21 && aces > 0 {
        total -= 10;
        aces -= 1;
    }

    total.min(255) as u8
}

pub fn is_bust(cards: &[Card]) -> bool {
    hand_value(cards) > 21
}

pub fn is_blackjack(cards: &[Card]) -> bool {
    cards.len() == 2 && hand_value(cards) == 21
}

// ── Game State ──────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum GamePhase {
    Betting,
    Dealing,        // Animation phase: cards being dealt
    PlayerTurn,
    DealerTurn,
    DealerRevealing, // Animation phase: dealer drawing
    Resolving,      // Animation phase: showing result
    Result(Outcome),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    PlayerBlackjack,
    PlayerWin,
    DealerWin,
    Push,
    PlayerBust,
    DealerBust,
}

impl Outcome {
    pub fn label(&self) -> &'static str {
        match self {
            Outcome::PlayerBlackjack => "BLACKJACK!",
            Outcome::PlayerWin => "YOU WIN",
            Outcome::DealerWin => "DEALER WINS",
            Outcome::Push => "PUSH",
            Outcome::PlayerBust => "BUST!",
            Outcome::DealerBust => "DEALER BUSTS",
        }
    }

    pub fn is_win(&self) -> bool {
        matches!(self, Outcome::PlayerBlackjack | Outcome::PlayerWin | Outcome::DealerBust)
    }
}

pub struct BlackjackGame {
    pub deck: Deck,
    pub player_hand: Vec<Card>,
    pub dealer_hand: Vec<Card>,
    pub phase: GamePhase,
    pub bet: u64,
    pub bet_input: String,
    pub doubled: bool,
    /// Animation: how many cards are "visible" (for deal animation)
    pub visible_cards: usize,
    /// Animation timer (frames since phase change)
    pub phase_timer: u64,
    /// Last outcome message for display
    pub last_payout: i64,
}

impl BlackjackGame {
    pub fn new() -> Self {
        Self {
            deck: Deck::new_shuffled(),
            player_hand: Vec::new(),
            dealer_hand: Vec::new(),
            phase: GamePhase::Betting,
            bet: 0,
            bet_input: String::new(),
            doubled: false,
            visible_cards: 0,
            phase_timer: 0,
            last_payout: 0,
        }
    }

    /// Start a new hand with given bet.
    pub fn deal(&mut self, bet: u64) {
        self.player_hand.clear();
        self.dealer_hand.clear();
        self.bet = bet;
        self.doubled = false;
        self.visible_cards = 0;
        self.phase_timer = 0;
        self.last_payout = 0;

        // Deal: player, dealer, player, dealer
        self.player_hand.push(self.deck.draw());
        self.dealer_hand.push(self.deck.draw());
        self.player_hand.push(self.deck.draw());
        self.dealer_hand.push(self.deck.draw());

        self.phase = GamePhase::Dealing;
    }

    /// Called after deal animation completes.
    pub fn after_deal(&mut self) {
        // Check for player blackjack
        if is_blackjack(&self.player_hand) {
            if is_blackjack(&self.dealer_hand) {
                self.phase = GamePhase::Result(Outcome::Push);
            } else {
                self.phase = GamePhase::Result(Outcome::PlayerBlackjack);
            }
        } else {
            self.phase = GamePhase::PlayerTurn;
        }
    }

    /// Player hits (takes another card).
    pub fn hit(&mut self) {
        if self.phase != GamePhase::PlayerTurn { return; }
        self.player_hand.push(self.deck.draw());

        if is_bust(&self.player_hand) {
            self.phase = GamePhase::Result(Outcome::PlayerBust);
        }
    }

    /// Player stands.
    pub fn stand(&mut self) {
        if self.phase != GamePhase::PlayerTurn { return; }
        self.phase = GamePhase::DealerTurn;
        self.phase_timer = 0;
    }

    /// Player doubles down (double bet, one card, then stand).
    pub fn double_down(&mut self) {
        if self.phase != GamePhase::PlayerTurn { return; }
        if self.player_hand.len() != 2 { return; }

        self.doubled = true;
        self.bet *= 2;
        self.player_hand.push(self.deck.draw());

        if is_bust(&self.player_hand) {
            self.phase = GamePhase::Result(Outcome::PlayerBust);
        } else {
            self.phase = GamePhase::DealerTurn;
            self.phase_timer = 0;
        }
    }

    /// Execute one step of dealer's turn. Returns true if dealer is done.
    pub fn dealer_step(&mut self) -> bool {
        if hand_value(&self.dealer_hand) < 17 {
            self.dealer_hand.push(self.deck.draw());
            false
        } else {
            true
        }
    }

    /// Resolve the hand. Returns payout (positive = player wins, negative = player loses).
    pub fn resolve(&mut self) -> i64 {
        let outcome = self.determine_outcome();
        let payout = self.calculate_payout(&outcome);
        self.last_payout = payout;
        self.phase = GamePhase::Result(outcome);
        payout
    }

    fn determine_outcome(&self) -> Outcome {
        let player = hand_value(&self.player_hand);
        let dealer = hand_value(&self.dealer_hand);

        if is_bust(&self.player_hand) {
            Outcome::PlayerBust
        } else if is_bust(&self.dealer_hand) {
            Outcome::DealerBust
        } else if is_blackjack(&self.player_hand) && !is_blackjack(&self.dealer_hand) {
            Outcome::PlayerBlackjack
        } else if player > dealer {
            Outcome::PlayerWin
        } else if dealer > player {
            Outcome::DealerWin
        } else {
            Outcome::Push
        }
    }

    /// Calculate payout amount.
    /// Blackjack pays 3:2 (bet * 1.5), normal win pays 1:1, push returns bet.
    pub fn calculate_payout(&self, outcome: &Outcome) -> i64 {
        match outcome {
            Outcome::PlayerBlackjack => (self.bet as f64 * 1.5) as i64,
            Outcome::PlayerWin | Outcome::DealerBust => self.bet as i64,
            Outcome::Push => 0,
            Outcome::PlayerBust | Outcome::DealerWin => -(self.bet as i64),
        }
    }

    pub fn player_value(&self) -> u8 {
        hand_value(&self.player_hand)
    }

    pub fn dealer_value(&self) -> u8 {
        hand_value(&self.dealer_hand)
    }

    /// Dealer's visible value (only first card during player turn).
    pub fn dealer_showing(&self) -> u8 {
        if self.dealer_hand.is_empty() { return 0; }
        match self.phase {
            GamePhase::PlayerTurn | GamePhase::Dealing => {
                self.dealer_hand[0].rank.value()
            }
            _ => hand_value(&self.dealer_hand),
        }
    }

    /// Reset for a new hand (keep deck).
    pub fn new_hand(&mut self) {
        self.player_hand.clear();
        self.dealer_hand.clear();
        self.phase = GamePhase::Betting;
        self.bet = 0;
        self.bet_input.clear();
        self.doubled = false;
        self.visible_cards = 0;
        self.phase_timer = 0;
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hand_value_simple() {
        let cards = vec![
            Card::new(Rank::Ten, Suit::Spades),
            Card::new(Rank::Seven, Suit::Hearts),
        ];
        assert_eq!(hand_value(&cards), 17);
    }

    #[test]
    fn hand_value_ace_as_11() {
        let cards = vec![
            Card::new(Rank::Ace, Suit::Spades),
            Card::new(Rank::Nine, Suit::Hearts),
        ];
        assert_eq!(hand_value(&cards), 20);
    }

    #[test]
    fn hand_value_ace_as_1_when_bust() {
        let cards = vec![
            Card::new(Rank::Ace, Suit::Spades),
            Card::new(Rank::Eight, Suit::Hearts),
            Card::new(Rank::Seven, Suit::Diamonds),
        ];
        // 11 + 8 + 7 = 26, ace downgrades to 1 → 16
        assert_eq!(hand_value(&cards), 16);
    }

    #[test]
    fn hand_value_two_aces() {
        let cards = vec![
            Card::new(Rank::Ace, Suit::Spades),
            Card::new(Rank::Ace, Suit::Hearts),
        ];
        // 11 + 11 = 22 → one ace becomes 1 → 12
        assert_eq!(hand_value(&cards), 12);
    }

    #[test]
    fn hand_value_three_aces() {
        let cards = vec![
            Card::new(Rank::Ace, Suit::Spades),
            Card::new(Rank::Ace, Suit::Hearts),
            Card::new(Rank::Ace, Suit::Diamonds),
        ];
        // 11+11+11=33 → 1+1+11=13
        assert_eq!(hand_value(&cards), 13);
    }

    #[test]
    fn blackjack_detection() {
        let bj = vec![
            Card::new(Rank::Ace, Suit::Spades),
            Card::new(Rank::King, Suit::Hearts),
        ];
        assert!(is_blackjack(&bj));
        assert_eq!(hand_value(&bj), 21);

        let not_bj = vec![
            Card::new(Rank::Ten, Suit::Spades),
            Card::new(Rank::Five, Suit::Hearts),
            Card::new(Rank::Six, Suit::Diamonds),
        ];
        assert!(!is_blackjack(&not_bj)); // 21 but 3 cards
    }

    #[test]
    fn bust_detection() {
        let bust = vec![
            Card::new(Rank::Ten, Suit::Spades),
            Card::new(Rank::Eight, Suit::Hearts),
            Card::new(Rank::Seven, Suit::Diamonds),
        ];
        assert!(is_bust(&bust));
        assert_eq!(hand_value(&bust), 25);
    }

    #[test]
    fn payout_blackjack_3_to_2() {
        let mut game = BlackjackGame::new();
        game.bet = 100;
        let payout = game.calculate_payout(&Outcome::PlayerBlackjack);
        assert_eq!(payout, 150); // 3:2
    }

    #[test]
    fn payout_normal_win() {
        let mut game = BlackjackGame::new();
        game.bet = 100;
        assert_eq!(game.calculate_payout(&Outcome::PlayerWin), 100);
        assert_eq!(game.calculate_payout(&Outcome::DealerBust), 100);
    }

    #[test]
    fn payout_push_returns_zero() {
        let mut game = BlackjackGame::new();
        game.bet = 100;
        assert_eq!(game.calculate_payout(&Outcome::Push), 0);
    }

    #[test]
    fn payout_loss() {
        let mut game = BlackjackGame::new();
        game.bet = 100;
        assert_eq!(game.calculate_payout(&Outcome::PlayerBust), -100);
        assert_eq!(game.calculate_payout(&Outcome::DealerWin), -100);
    }

    #[test]
    fn payout_double_down() {
        let mut game = BlackjackGame::new();
        game.bet = 200; // doubled from 100
        assert_eq!(game.calculate_payout(&Outcome::PlayerWin), 200);
        assert_eq!(game.calculate_payout(&Outcome::PlayerBust), -200);
    }

    #[test]
    fn dealer_stands_on_17() {
        let mut game = BlackjackGame::new();
        game.dealer_hand = vec![
            Card::new(Rank::Ten, Suit::Spades),
            Card::new(Rank::Seven, Suit::Hearts),
        ];
        assert!(game.dealer_step()); // 17 → done
    }

    #[test]
    fn dealer_hits_on_16() {
        let mut game = BlackjackGame::new();
        game.dealer_hand = vec![
            Card::new(Rank::Ten, Suit::Spades),
            Card::new(Rank::Six, Suit::Hearts),
        ];
        assert!(!game.dealer_step()); // 16 → hits
        assert_eq!(game.dealer_hand.len(), 3);
    }

    #[test]
    fn deck_deals_52_unique_cards() {
        let mut deck = Deck::new_shuffled();
        let mut cards = Vec::new();
        for _ in 0..52 {
            cards.push(deck.draw());
        }
        // All 52 should be unique
        for i in 0..cards.len() {
            for j in (i + 1)..cards.len() {
                assert_ne!((cards[i].rank, cards[i].suit), (cards[j].rank, cards[j].suit));
            }
        }
    }

    #[test]
    fn deck_reshuffles_when_empty() {
        let mut deck = Deck::new_shuffled();
        // Draw all 52
        for _ in 0..52 { deck.draw(); }
        // Should reshuffle and keep going
        let card = deck.draw();
        assert!(card.rank.value() > 0);
    }
}
